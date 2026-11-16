//! The macOS AppKit layer. The module exists on every platform but is empty
//! outside macOS: every submodule is `cfg(target_os = "macos")`.

#[cfg(target_os = "macos")]
pub mod actions;
#[cfg(target_os = "macos")]
pub mod app;
#[cfg(target_os = "macos")]
pub mod hub;
#[cfg(target_os = "macos")]
pub mod page;
#[cfg(target_os = "macos")]
pub mod pages;
#[cfg(target_os = "macos")]
pub mod setup;
#[cfg(target_os = "macos")]
pub mod snapshot;
#[cfg(target_os = "macos")]
pub mod style;
#[cfg(target_os = "macos")]
pub mod widgets;
