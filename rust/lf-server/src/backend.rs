//! Integration boundary for the runtime crates being ported independently.
//!
//! TODO: implement `Backend` using the meeting, recall, platform, settings-agent
//! and transcription crates. The HTTP server never invokes Python. Failures use
//! FastAPI's HTTPException envelope (`{"detail": ...}`), including 503 while a
//! Rust backend is unavailable, rather than returning fabricated successful data.
use axum::{
    body::Bytes,
    http::{HeaderMap, Method},
};
use lf_core::config::Config;
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct BackendRequest {
    pub method: Method,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HeaderMap,
    /// Original body, including multipart boundaries for `/transcribe`.
    pub body: Bytes,
    /// Coherent snapshot of the current live configuration.
    pub config: Config,
}

pub struct BackendResponse {
    pub status: u16,
    pub body: Value,
}

impl BackendResponse {
    pub fn error(status: u16, detail: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"detail": detail.into()}),
        }
    }
}

/// Runs on Tokio's blocking pool, never on an executor worker. Implementations
/// own validation and lifecycle for their routes. They must not log request
/// bodies/headers (which can contain credentials or private recordings).
pub trait Backend: Send + Sync + 'static {
    fn handle(&self, request: BackendRequest) -> BackendResponse;

    /// Finish owned sessions before stopping the server; runs on a blocking thread.
    fn shutdown(&self, _config: &Config) -> anyhow::Result<()> {
        Ok(())
    }

    fn stt_available(&self, _provider: &str) -> bool {
        false
    }
    fn transcriber_loaded(&self) -> bool {
        false
    }
    fn autostart_status(&self) -> String {
        "unavailable".into()
    }
}

#[derive(Default)]
pub struct UnavailableBackend;

impl Backend for UnavailableBackend {
    fn handle(&self, request: BackendRequest) -> BackendResponse {
        let name = match request.path.as_str() {
            "/transcribe" => "transcription",
            "/api/recall" => "recall",
            "/api/autostart" => "autostart",
            p if p.starts_with("/prompts") => "prompts",
            p if p.starts_with("/meeting") => "meeting",
            _ => "settings agent",
        };
        BackendResponse::error(503, format!("{name} backend unavailable"))
    }
}
