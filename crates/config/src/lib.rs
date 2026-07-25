use clap::{Parser, ValueEnum};
use std::{net::SocketAddr, path::PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("CODEX_BRIDGE_API_KEY is required; use --no-auth only for a loopback-only server")]
    MissingApiKey,
    #[error("--no-auth may only be used with a loopback listen address")]
    NoAuthOnNonLoopback,
    #[error("cannot determine current directory")]
    CurrentDir(#[source] std::io::Error),
    #[error("working directory was not initialized")]
    MissingCwd,
    #[error("CODEX_BRIDGE_CWD must be an existing absolute directory: {}", .0.display())]
    InvalidCwd(PathBuf),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl Sandbox {
    #[must_use]
    pub fn as_app_server_value(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

#[derive(Debug, Clone, Parser)]
#[command(version, about)]
pub struct Config {
    /// Address on which the OpenAI-compatible API listens.
    #[arg(long, env = "CODEX_BRIDGE_LISTEN", default_value = "127.0.0.1:8787")]
    pub listen: SocketAddr,

    /// Bearer token accepted from clients. Required unless --no-auth is set.
    #[arg(long, env = "CODEX_BRIDGE_API_KEY")]
    pub api_key: Option<String>,

    /// Disable HTTP authentication. Safe only on a loopback listener.
    #[arg(long, env = "CODEX_BRIDGE_NO_AUTH", default_value_t = false)]
    pub no_auth: bool,

    /// Working directory exposed to Codex.
    #[arg(long, env = "CODEX_BRIDGE_CWD")]
    pub cwd: Option<PathBuf>,

    /// Path to the Codex CLI executable.
    #[arg(long, env = "CODEX_BRIDGE_CODEX_BIN", default_value = "codex")]
    pub codex_bin: PathBuf,

    /// Codex model override. Omit to use the Codex configuration default.
    #[arg(long, env = "CODEX_BRIDGE_CODEX_MODEL")]
    pub codex_model: Option<String>,

    /// Model ID advertised by /v1/models.
    #[arg(long, env = "CODEX_BRIDGE_MODEL", default_value = "codex")]
    pub exposed_model: String,

    /// Sandbox granted to Codex turns.
    #[arg(
        long,
        env = "CODEX_BRIDGE_SANDBOX",
        value_enum,
        default_value = "workspace-write"
    )]
    pub sandbox: Sandbox,

    /// Maximum time for one Codex turn.
    #[arg(long, env = "CODEX_BRIDGE_TIMEOUT_SECS", default_value_t = 3600)]
    pub timeout_secs: u64,
}

impl Config {
    /// Validates authentication, listener, and working-directory settings.
    ///
    /// # Errors
    ///
    /// Returns an error when authentication is missing, unauthenticated access
    /// is exposed beyond loopback, or the working directory is invalid.
    pub fn validate(mut self) -> Result<Self, ConfigError> {
        if !self.no_auth && self.api_key.as_deref().is_none_or(str::is_empty) {
            return Err(ConfigError::MissingApiKey);
        }
        if self.no_auth && !self.listen.ip().is_loopback() {
            return Err(ConfigError::NoAuthOnNonLoopback);
        }
        let cwd = self
            .cwd
            .take()
            .map_or_else(std::env::current_dir, Ok)
            .map_err(ConfigError::CurrentDir)?;
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err(ConfigError::InvalidCwd(cwd));
        }
        self.cwd = Some(cwd);
        Ok(self)
    }
}
