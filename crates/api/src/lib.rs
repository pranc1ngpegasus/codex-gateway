use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use codex_gateway_app_server::{AppServer, AppServerError, TurnEvent, TurnStream};
use codex_gateway_config::Config;
use codex_gateway_translate::{
    ChatRequest, ForcedTool, ResponsesRequest, chat_prompt, forced_tool, response_output_schema,
    responses_prompt,
};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub app_server: AppServer,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticate(&state.config, &headers)?;
    Ok(Json(json!({
        "object": "list",
        "data": [{
            "id": state.config.exposed_model,
            "object": "model",
            "created": 0,
            "owned_by": "openai"
        }]
    })))
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<ChatRequest>,
) -> Result<Response, ApiError> {
    authenticate(&state.config, &headers)?;
    let prompt = chat_prompt(&request);
    if prompt.trim().is_empty() {
        return Err(ApiError::bad_request("messages did not contain text"));
    }
    let forced = forced_tool(&request);
    let turn = state
        .app_server
        .start_turn(prompt, forced.as_ref().map(|tool| tool.schema.clone()))
        .await?;

    let id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    if request.stream {
        Ok(chat_stream(state, turn, id, request.model, forced))
    } else {
        let text = collect_turn(&state.app_server, turn, state.config.timeout_secs).await?;
        let message = match forced {
            Some(tool) => json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{}", Uuid::new_v4().simple()),
                    "type": "function",
                    "function": {"name": tool.name, "arguments": text}
                }]
            }),
            None => json!({"role": "assistant", "content": text}),
        };
        let finish_reason = if message.get("tool_calls").is_some() {
            "tool_calls"
        } else {
            "stop"
        };
        Ok(Json(json!({
            "id": id,
            "object": "chat.completion",
            "created": now_seconds(),
            "model": request.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason
            }],
            "usage": empty_usage()
        }))
        .into_response())
    }
}

async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<ResponsesRequest>,
) -> Result<Response, ApiError> {
    authenticate(&state.config, &headers)?;
    let prompt = responses_prompt(&request);
    if prompt.trim().is_empty() {
        return Err(ApiError::bad_request("input did not contain text"));
    }
    let turn = state
        .app_server
        .start_turn(prompt, response_output_schema(&request))
        .await?;
    let id = format!("resp_{}", Uuid::new_v4().simple());
    if request.stream {
        Ok(responses_stream(state, turn, id, request.model))
    } else {
        let text = collect_turn(&state.app_server, turn, state.config.timeout_secs).await?;
        Ok(Json(response_object(
            &id,
            &request.model,
            &text,
            "completed",
            None,
            None,
        ))
        .into_response())
    }
}

fn chat_stream(
    state: Arc<AppState>,
    turn: TurnStream,
    id: String,
    model: String,
    forced: Option<ForcedTool>,
) -> Response {
    let (sender, receiver) = mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(run_chat_stream(sender, state, turn, id, model, forced));
    Sse::new(ReceiverStream::new(receiver))
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn run_chat_stream(
    sender: mpsc::Sender<Result<Event, Infallible>>,
    state: Arc<AppState>,
    mut turn: TurnStream,
    id: String,
    model: String,
    forced: Option<ForcedTool>,
) {
    let created = now_seconds();
    let initial_delta = initial_chat_delta(forced.as_ref());
    if send_sse(
        &sender,
        chat_chunk(&id, created, &model, &initial_delta, &Value::Null),
    )
    .await
    .is_err()
    {
        state
            .app_server
            .interrupt(&turn.thread_id, &turn.turn_id)
            .await;
        return;
    }

    let timeout = tokio::time::sleep(Duration::from_secs(state.config.timeout_secs));
    tokio::pin!(timeout);
    let mut failed = None;
    loop {
        tokio::select! {
            () = &mut timeout => {
                failed = Some("Codex turn timed out".to_owned());
                state.app_server.interrupt(&turn.thread_id, &turn.turn_id).await;
                break;
            }
            event = turn.events.recv() => match event {
                Some(TurnEvent::Delta(delta)) => {
                    let body = match &forced {
                        Some(_) => json!({"tool_calls": [{"index": 0, "function": {"arguments": delta}}]}),
                        None => json!({"content": delta}),
                    };
                    if send_sse(
                        &sender,
                        chat_chunk(&id, created, &model, &body, &Value::Null),
                    ).await.is_err() {
                        state.app_server.interrupt(&turn.thread_id, &turn.turn_id).await;
                        return;
                    }
                }
                Some(TurnEvent::Completed { status, error }) => {
                    if status != "completed" {
                        failed = error.or(Some(format!("Codex turn ended with status {status}")));
                    }
                    break;
                }
                Some(TurnEvent::Error(error)) => {
                    failed = Some(error);
                    break;
                }
                None => {
                    failed = Some("Codex event stream closed".into());
                    break;
                }
            }
        }
    }

    if let Some(error) = failed {
        let _ = send_sse(
                &sender,
                json!({"error": {"message": error, "type": "server_error", "code": "codex_turn_failed"}}),
            )
            .await;
    } else {
        let reason = if forced.is_some() {
            "tool_calls"
        } else {
            "stop"
        };
        let _ = send_sse(
            &sender,
            chat_chunk(&id, created, &model, &json!({}), &json!(reason)),
        )
        .await;
        let _ = send_sse(
            &sender,
            json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [],
                "usage": empty_usage()
            }),
        )
        .await;
    }
    let _ = sender.send(Ok(Event::default().data("[DONE]"))).await;
}

fn initial_chat_delta(forced: Option<&ForcedTool>) -> Value {
    match forced {
        Some(tool) => json!({
            "role": "assistant",
            "tool_calls": [{
                "index": 0,
                "id": format!("call_{}", Uuid::new_v4().simple()),
                "type": "function",
                "function": {"name": tool.name, "arguments": ""}
            }]
        }),
        None => json!({"role": "assistant", "content": ""}),
    }
}

fn responses_stream(
    state: Arc<AppState>,
    turn: TurnStream,
    id: String,
    model: String,
) -> Response {
    let (sender, receiver) = mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(run_responses_stream(sender, state, turn, id, model));
    Sse::new(ReceiverStream::new(receiver))
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn run_responses_stream(
    sender: mpsc::Sender<Result<Event, Infallible>>,
    state: Arc<AppState>,
    mut turn: TurnStream,
    id: String,
    model: String,
) {
    let message_id = format!("msg_{}", Uuid::new_v4().simple());
    let mut sequence = 0_u64;
    let mut text = String::new();
    for (event_type, body) in initial_response_events(&id, &model, &message_id) {
        sequence += 1;
        if send_response_event(&sender, event_type, sequence, body)
            .await
            .is_err()
        {
            state
                .app_server
                .interrupt(&turn.thread_id, &turn.turn_id)
                .await;
            return;
        }
    }

    let timeout = tokio::time::sleep(Duration::from_secs(state.config.timeout_secs));
    tokio::pin!(timeout);
    let mut failure = None;
    loop {
        tokio::select! {
            () = &mut timeout => {
                failure = Some("Codex turn timed out".to_owned());
                state.app_server.interrupt(&turn.thread_id, &turn.turn_id).await;
                break;
            }
            event = turn.events.recv() => match event {
                Some(TurnEvent::Delta(delta)) => {
                    text.push_str(&delta);
                    sequence += 1;
                    if send_response_event(
                        &sender,
                        "response.output_text.delta",
                        sequence,
                        json!({
                            "item_id": message_id,
                            "output_index": 0,
                            "content_index": 0,
                            "delta": delta
                        }),
                    ).await.is_err() {
                        state.app_server.interrupt(&turn.thread_id, &turn.turn_id).await;
                        return;
                    }
                }
                Some(TurnEvent::Completed { status, error }) => {
                    if status != "completed" {
                        failure = error.or(Some(format!("Codex turn ended with status {status}")));
                    }
                    break;
                }
                Some(TurnEvent::Error(error)) => {
                    failure = Some(error);
                    break;
                }
                None => {
                    failure = Some("Codex event stream closed".into());
                    break;
                }
            }
        }
    }

    if let Some(error) = failure {
        sequence += 1;
        let _ = send_response_event(
            &sender,
            "response.failed",
            sequence,
            json!({
                "response": response_object(
                    &id,
                    &model,
                    &text,
                    "failed",
                    Some(error),
                    Some(&message_id)
                )
            }),
        )
        .await;
        return;
    }

    for (event_type, body) in final_response_events(&id, &model, &message_id, &text) {
        sequence += 1;
        let _ = send_response_event(&sender, event_type, sequence, body).await;
    }
}

fn initial_response_events(
    id: &str,
    model: &str,
    message_id: &str,
) -> [(&'static str, Value); 4] {
    let created = response_object(id, model, "", "in_progress", None, Some(message_id));
    [
        ("response.created", json!({"response": created})),
        (
            "response.in_progress",
            json!({"response": response_object(
                id,
                model,
                "",
                "in_progress",
                None,
                Some(message_id)
            )}),
        ),
        (
            "response.output_item.added",
            json!({
                "output_index": 0,
                "item": message_item(message_id, "", "in_progress")
            }),
        ),
        (
            "response.content_part.added",
            json!({
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}
            }),
        ),
    ]
}

fn final_response_events(
    id: &str,
    model: &str,
    message_id: &str,
    text: &str,
) -> [(&'static str, Value); 4] {
    [
        (
            "response.output_text.done",
            json!({
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "text": text
            }),
        ),
        (
            "response.content_part.done",
            json!({
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": text, "annotations": []}
            }),
        ),
        (
            "response.output_item.done",
            json!({
                "output_index": 0,
                "item": message_item(message_id, text, "completed")
            }),
        ),
        (
            "response.completed",
            json!({
                "response": response_object(
                    id,
                    model,
                    text,
                    "completed",
                    None,
                    Some(message_id)
                )
            }),
        ),
    ]
}

async fn collect_turn(
    app_server: &AppServer,
    mut turn: TurnStream,
    timeout_secs: u64,
) -> Result<String, ApiError> {
    let thread_id = turn.thread_id.clone();
    let turn_id = turn.turn_id.clone();
    let future = async {
        let mut text = String::new();
        while let Some(event) = turn.events.recv().await {
            match event {
                TurnEvent::Delta(delta) => text.push_str(&delta),
                TurnEvent::Completed { status, error: _ } if status == "completed" => {
                    return Ok(text);
                },
                TurnEvent::Completed { status, error } => {
                    return Err(ApiError::server(error.unwrap_or_else(|| {
                        format!("Codex turn ended with status {status}")
                    })));
                },
                TurnEvent::Error(error) => return Err(ApiError::server(error)),
            }
        }
        Err(ApiError::server("Codex event stream closed"))
    };
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(timeout_secs), future).await {
        result
    } else {
        app_server.interrupt(&thread_id, &turn_id).await;
        Err(ApiError::server("Codex turn timed out"))
    }
}

fn chat_chunk(
    id: &str,
    created: u64,
    model: &str,
    delta: &Value,
    finish_reason: &Value,
) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason
        }]
    })
}

fn response_object(
    id: &str,
    model: &str,
    text: &str,
    status: &str,
    error: Option<String>,
    message_id: Option<&str>,
) -> Value {
    let generated_message_id;
    let message_id = if let Some(message_id) = message_id {
        message_id
    } else {
        generated_message_id = format!("msg_{}", Uuid::new_v4().simple());
        &generated_message_id
    };
    json!({
        "id": id,
        "object": "response",
        "created_at": now_seconds(),
        "status": status,
        "error": error.map(|message| json!({
            "code": "codex_turn_failed",
            "message": message
        })),
        "incomplete_details": null,
        "instructions": null,
        "model": model,
        "output": if status == "completed" {
            vec![message_item(message_id, text, "completed")]
        } else {
            Vec::new()
        },
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "store": false,
        "temperature": null,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_p": null,
        "truncation": "disabled",
        "usage": {
            "input_tokens": 0,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 0,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 0
        },
        "metadata": {}
    })
}

fn message_item(
    id: &str,
    text: &str,
    status: &str,
) -> Value {
    json!({
        "id": id,
        "type": "message",
        "status": status,
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": text,
            "annotations": []
        }]
    })
}

async fn send_sse(
    sender: &mpsc::Sender<Result<Event, Infallible>>,
    body: Value,
) -> Result<(), ()> {
    sender
        .send(Ok(Event::default().data(body.to_string())))
        .await
        .map_err(|_| ())
}

async fn send_response_event(
    sender: &mpsc::Sender<Result<Event, Infallible>>,
    event_type: &str,
    sequence_number: u64,
    mut body: Value,
) -> Result<(), ()> {
    body["type"] = json!(event_type);
    body["sequence_number"] = json!(sequence_number);
    sender
        .send(Ok(Event::default()
            .event(event_type)
            .data(body.to_string())))
        .await
        .map_err(|_| ())
}

fn authenticate(
    config: &Config,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    if config.no_auth {
        return Ok(());
    }
    let expected = config.api_key.as_deref().unwrap_or_default().as_bytes();
    let supplied = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::as_bytes)
        .unwrap_or_default();
    if constant_time_eq(expected, supplied) {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "invalid or missing bearer token".into(),
            code: "invalid_api_key",
        })
    }
}

fn constant_time_eq(
    left: &[u8],
    right: &[u8],
) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn empty_usage() -> Value {
    json!({
        "prompt_tokens": 0,
        "completion_tokens": 0,
        "total_tokens": 0
    })
}

pub struct ApiError {
    status: StatusCode,
    message: String,
    code: &'static str,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            code: "invalid_request_error",
        }
    }

    fn server(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
            code: "codex_app_server_error",
        }
    }
}

impl From<AppServerError> for ApiError {
    fn from(error: AppServerError) -> Self {
        Self::server(error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "message": self.message,
                    "type": "server_error",
                    "code": self.code
                }
            })),
        )
            .into_response()
    }
}
