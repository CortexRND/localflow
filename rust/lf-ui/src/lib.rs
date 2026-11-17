//! Shared UI primitives and the native LocalFlow Hub shell.
//!
//! Pure state and layout helpers are portable; AppKit code is macOS-only.

pub mod ask_layout;
pub mod backends;
pub mod hot_phrases_editor;
pub mod hub_model;
pub mod hub_route;
pub mod mac;
pub mod pystr;
pub mod shutdown;
pub mod snapshot_plan;
pub mod text_wrap;
pub mod tokens;
