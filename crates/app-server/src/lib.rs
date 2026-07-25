use codex_gateway_config::Config;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc, oneshot},
};
use tracing::{debug, error, warn};

#[derive(Debug, Error, Clone)]
pub enum AppServerError {
    #[error("failed to start Codex app-server: {0}")]
    Spawn(String),
    #[error("Codex app-server transport closed")]
    Closed,
    #[error("Codex app-server RPC failed: {0}")]
    Rpc(String),
    #[error("invalid Codex app-server response: {0}")]
    Protocol(String),
}

#[derive(Debug)]
pub enum TurnEvent {
    Delta(String),
    Completed {
        status: String,
        error: Option<String>,
    },
    Error(String),
}

pub struct TurnStream {
    pub thread_id: String,
    pub turn_id: String,
    pub events: mpsc::Receiver<TurnEvent>,
}

struct Inner {
    writer: mpsc::Sender<Value>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, AppServerError>>>>,
    turns: Mutex<HashMap<String, mpsc::Sender<TurnEvent>>>,
    next_id: AtomicU64,
    config: Config,
}

#[derive(Clone)]
pub struct AppServer {
    inner: Arc<Inner>,
    _child: Arc<Mutex<Child>>,
}

impl AppServer {
    /// Starts and initializes a Codex app-server subprocess.
    ///
    /// # Errors
    ///
    /// Returns an error if the subprocess cannot be started or initialized.
    pub async fn spawn(config: Config) -> Result<Self, AppServerError> {
        let mut child = Command::new(&config.codex_bin)
            .args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| AppServerError::Spawn(error.to_string()))?;

        let stdin = child.stdin.take().ok_or_else(|| {
            AppServerError::Spawn("Codex app-server stdin was not available".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AppServerError::Spawn("Codex app-server stdout was not available".into())
        })?;

        let (writer, mut outbound_messages) = mpsc::channel::<Value>(128);
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(message) = outbound_messages.recv().await {
                let mut encoded = match serde_json::to_vec(&message) {
                    Ok(encoded) => encoded,
                    Err(error) => {
                        error!(%error, "failed to encode app-server message");
                        continue;
                    },
                };
                encoded.push(b'\n');
                if let Err(error) = stdin.write_all(&encoded).await {
                    error!(%error, "failed to write to app-server");
                    break;
                }
            }
        });

        let inner = Arc::new(Inner {
            writer,
            pending: Mutex::new(HashMap::new()),
            turns: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            config,
        });

        tokio::spawn(read_loop(stdout, inner.clone()));

        let server = Self {
            inner,
            _child: Arc::new(Mutex::new(child)),
        };
        server
            .rpc(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "codex-responses-bridge",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "experimentalApi": false
                    }
                }),
            )
            .await?;
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    async fn rpc(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, AppServerError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, sender);
        if self
            .inner
            .writer
            .send(json!({"id": id, "method": method, "params": params}))
            .await
            .is_err()
        {
            self.inner.pending.lock().await.remove(&id);
            return Err(AppServerError::Closed);
        }
        receiver.await.map_err(|_| AppServerError::Closed)?
    }

    async fn notify(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(), AppServerError> {
        self.inner
            .writer
            .send(json!({"method": method, "params": params}))
            .await
            .map_err(|_| AppServerError::Closed)
    }

    /// Starts a Codex thread and turn for the supplied prompt.
    ///
    /// # Errors
    ///
    /// Returns an error if either app-server RPC fails or has an invalid response.
    pub async fn start_turn(
        &self,
        prompt: String,
        output_schema: Option<Value>,
    ) -> Result<TurnStream, AppServerError> {
        let config = &self.inner.config;
        let mut thread_params = json!({
            "cwd": config.cwd,
            "ephemeral": true,
            "approvalPolicy": "never",
            "sandbox": config.sandbox.as_app_server_value(),
        });
        if let Some(model) = &config.codex_model {
            thread_params["model"] = json!(model);
        }

        let thread = self.rpc("thread/start", thread_params).await?;
        let thread_id = thread
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| AppServerError::Protocol("thread/start omitted thread.id".into()))?
            .to_owned();

        let (sender, events) = mpsc::channel(256);
        self.inner
            .turns
            .lock()
            .await
            .insert(thread_id.clone(), sender);

        let mut turn_params = json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt}],
        });
        if let Some(schema) = output_schema {
            turn_params["outputSchema"] = schema;
        }
        let turn = match self.rpc("turn/start", turn_params).await {
            Ok(turn) => turn,
            Err(error) => {
                self.inner.turns.lock().await.remove(&thread_id);
                return Err(error);
            },
        };
        let turn_id = turn
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| AppServerError::Protocol("turn/start omitted turn.id".into()))?
            .to_owned();

        Ok(TurnStream {
            thread_id,
            turn_id,
            events,
        })
    }

    pub async fn interrupt(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) {
        let _ = self
            .rpc(
                "turn/interrupt",
                json!({"threadId": thread_id, "turnId": turn_id}),
            )
            .await;
    }
}

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    inner: Arc<Inner>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let message: Value = match serde_json::from_str(&line) {
                    Ok(message) => message,
                    Err(error) => {
                        warn!(%error, %line, "ignored malformed app-server output");
                        continue;
                    },
                };
                debug!(message = %message, "app-server message");
                if message.get("method").is_some()
                    && let Some(id) = message.get("id").cloned()
                {
                    answer_server_request(&inner, id, &message).await;
                    continue;
                }
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    if let Some(sender) = inner.pending.lock().await.remove(&id) {
                        let result = if let Some(error) = message.get("error") {
                            Err(AppServerError::Rpc(error.to_string()))
                        } else {
                            Ok(message.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = sender.send(result);
                    }
                    continue;
                }
                route_notification(&inner, message).await;
            },
            Ok(None) => break,
            Err(error) => {
                error!(%error, "failed reading app-server output");
                break;
            },
        }
    }

    for (_, sender) in inner.pending.lock().await.drain() {
        let _ = sender.send(Err(AppServerError::Closed));
    }
    let turn_senders = inner
        .turns
        .lock()
        .await
        .drain()
        .map(|(_, sender)| sender)
        .collect::<Vec<_>>();
    for sender in turn_senders {
        let _ = sender
            .send(TurnEvent::Error("Codex app-server closed".into()))
            .await;
    }
}

async fn route_notification(
    inner: &Inner,
    message: Value,
) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
        return;
    };
    let sender = inner.turns.lock().await.get(thread_id).cloned();
    let Some(sender) = sender else {
        return;
    };

    let event = match method {
        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| TurnEvent::Delta(delta.to_owned())),
        "error" => {
            let will_retry = params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (!will_retry).then(|| {
                TurnEvent::Error(
                    params
                        .get("error")
                        .map_or_else(|| "Codex turn failed".into(), Value::to_string),
                )
            })
        },
        "turn/completed" => {
            let status = params
                .pointer("/turn/status")
                .and_then(Value::as_str)
                .unwrap_or("failed")
                .to_owned();
            let error = params
                .pointer("/turn/error")
                .filter(|value| !value.is_null())
                .map(Value::to_string);
            Some(TurnEvent::Completed { status, error })
        },
        _ => None,
    };
    if let Some(event) = event {
        let completed = matches!(event, TurnEvent::Completed { .. } | TurnEvent::Error(_));
        let _ = sender.send(event).await;
        if completed {
            inner.turns.lock().await.remove(thread_id);
        }
    }
}

async fn answer_server_request(
    inner: &Inner,
    id: Value,
    message: &Value,
) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({"decision": "decline"})
        },
        "item/tool/requestUserInput" => {
            let answers = message
                .pointer("/params/questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|question| question.get("id").and_then(Value::as_str))
                .map(|id| (id.to_owned(), json!({"answers": []})))
                .collect::<serde_json::Map<_, _>>();
            json!({"answers": answers})
        },
        "currentTime/read" => {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            json!({"currentTimeAt": seconds})
        },
        "applyPatchApproval" | "execCommandApproval" => {
            json!({"decision": {"denied": {"rejection": "bridge runs non-interactively"}}})
        },
        _ => {
            let response = json!({
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("unsupported server request: {method}")
                }
            });
            let _ = inner.writer.send(response).await;
            return;
        },
    };
    let _ = inner.writer.send(json!({"id": id, "result": result})).await;
}
