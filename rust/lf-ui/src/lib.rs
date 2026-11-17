//! Shared UI primitives and the native LocalFlow Hub shell.
//!
//! Pure state and layout helpers are portable; AppKit code is macOS-only.

pub mod ask_layout;
pub mod backends;
pub mod config_write;
#[cfg(feature = "runtime")]
pub mod embedded;
pub mod hot_phrases_editor;
pub mod hub_model;
pub mod hub_route;
pub mod hub_settings;
pub mod mac;
pub mod models_layout;
pub mod pystr;
pub mod setup_flow;
pub mod shutdown;
pub mod snapshot_plan;
pub mod text_wrap;
pub mod tokens;
