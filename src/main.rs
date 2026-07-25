use clap::Parser;
use codex_gateway_api::{AppState, router};
use codex_gateway_app_server::{AppServer, AppServerError};
use codex_gateway_config::{Config, ConfigError};
use std::sync::Arc;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Error)]
enum MainError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    AppServer(#[from] AppServerError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
enum ShutdownError {
    #[error("failed to install Ctrl-C handler")]
    CtrlC(#[source] std::io::Error),
    #[cfg(unix)]
    #[error("failed to install SIGTERM handler")]
    Terminate(#[source] std::io::Error),
}

#[tokio::main]
async fn main() -> Result<(), MainError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::parse().validate()?;
    let app_server = AppServer::spawn(config.clone()).await?;
    let listener = TcpListener::bind(config.listen).await?;
    let cwd = config.cwd.as_deref().ok_or(ConfigError::MissingCwd)?;
    info!(
        listen = %config.listen,
        cwd = %cwd.display(),
        model = %config.exposed_model,
        "Codex gateway is ready"
    );

    axum::serve(listener, router(Arc::new(AppState { config, app_server })))
        .with_graceful_shutdown(async {
            if let Err(error) = shutdown_signal().await {
                error!(%error, "shutdown signal handler failed");
            }
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() -> Result<(), ShutdownError> {
    let ctrl_c = async { tokio::signal::ctrl_c().await.map_err(ShutdownError::CtrlC) };

    #[cfg(unix)]
    let terminate = async {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(ShutdownError::Terminate)?;
        signal.recv().await;
        Ok(())
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<Result<(), ShutdownError>>();

    tokio::select! {
        result = ctrl_c => result,
        result = terminate => result,
    }
}
