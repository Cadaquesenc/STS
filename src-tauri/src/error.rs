//! The one error type that crosses the IPC boundary.
//!
//! A command that fails has to say why in a sentence a person can read, so this
//! serialises to `{ kind, message }` rather than a bare string: the UI switches
//! on `kind` to decide what to do, and shows `message` to whoever is watching.

use serde::Serialize;

/// Anything a Tauri command in this crate can fail with.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum EngineError {
    /// `sts.db` could not be opened, or a statement against it failed.
    Database(String),
    /// The engine is on its way down and will not take new work.
    ShuttingDown(String),
    /// Telemetry could not be delivered to the window that asked for it.
    Telemetry(String),
    /// The ingestion layer refused what it was asked for.
    Ingestion(String),
    /// A fixture would not open, or replay refused to start over what is
    /// already running.
    Replay(String),
    /// A clustering or funding-trace request could not be answered as asked.
    ///
    /// Always a refusal to measure rather than a measurement that failed: the
    /// forensic modules return UNKNOWN inside their reports and never raise, so
    /// anything that reaches here is a request that did not describe a launch.
    Forensics(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::Database(message) => write!(f, "database: {message}"),
            EngineError::ShuttingDown(message) => write!(f, "shutting down: {message}"),
            EngineError::Telemetry(message) => write!(f, "telemetry: {message}"),
            EngineError::Ingestion(message) => write!(f, "ingestion: {message}"),
            EngineError::Replay(message) => write!(f, "replay: {message}"),
            EngineError::Forensics(message) => write!(f, "forensics: {message}"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<rusqlite::Error> for EngineError {
    fn from(err: rusqlite::Error) -> Self {
        EngineError::Database(err.to_string())
    }
}

impl From<crate::replay::SessionError> for EngineError {
    fn from(err: crate::replay::SessionError) -> Self {
        EngineError::Replay(err.to_string())
    }
}
