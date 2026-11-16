//! The LocalFlow dictation runtime: controller state machine, worker and
//! text pipeline, live config reload, menu bar and single-instance lock.
//!
//! Audio capture and speech recognition come in through the [`engine`]
//! traits so the parallel `lf-audio`/`lf-stt` crates can be plugged in.

pub mod app;
pub mod context;
pub mod controller;
pub mod engine;
pub mod live;
pub mod menubar;
pub mod pipeline;
pub mod run;
pub mod settings;
pub mod singleinstance;
pub mod status;
pub mod transform;
pub mod worker;

#[cfg(target_os = "macos")]
pub mod macos;

/// The platform's menu bar UI, if it has one in the Rust port.
pub fn platform_menu_bar() -> Option<std::sync::Arc<dyn menubar::MenuBar>> {
    #[cfg(target_os = "macos")]
    {
        Some(macos::menu_bar())
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Tray icons for Linux/Windows are not ported; state still goes to
        // status.json for the Python menu bar/tray to show.
        None
    }
}
