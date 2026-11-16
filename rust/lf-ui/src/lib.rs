//! LocalFlow UI logic (Stage 1 Rust port): the platform-independent parts of
//! the Hub, setup, settings, models, Ask and hot-phrases windows. No AppKit,
//! no network and no real config/keychain access in tests; everything that
//! depends on the platform sits behind a trait so a later `cfg(macos)` layer
//! can supply the real implementation.

pub mod ask_layout;
pub mod backends;
pub mod hot_phrases_editor;
pub mod hub_model;
pub mod hub_route;
pub mod hub_settings;
pub mod models_layout;
pub mod pystr;
pub mod setup_flow;
pub mod text_wrap;
pub mod tokens;
