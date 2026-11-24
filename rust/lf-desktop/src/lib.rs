//! LocalFlow native desktop app (GPUI).
//!
//! A parallel front end over the existing portable `lf-core` / `lf-ui` stores
//! and HTTP adapters. It does not replace `lf-hub`, `lf hub` or the Python
//! runtime; see `docs/gpui-migration.md` for the cutover gates.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod app;
pub mod args;
pub mod console;
pub mod controllers;
pub mod data;
pub mod launch;
pub mod page;
pub mod pages;
pub mod remote;
pub mod sandbox;
pub mod theme;
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
mod tray;
pub mod ui;
