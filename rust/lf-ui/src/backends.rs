//! Backend traits the UI talks to, plus in-memory fakes for tests.
//!
//! Each trait lives in its own file; the real implementations (HTTP client,
//! settings agent, prompt queue) belong to a later platform layer.

pub mod ask;
pub mod http;
pub mod meeting;
pub mod prompts;
pub mod settings_agent;
