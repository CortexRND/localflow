//! Global hotkeys and text insertion for the LocalFlow Rust port.
//!
//! - [`keys`]: pynput-compatible key names and order-insensitive chords.
//! - [`hub`]: the `KeyboardHub` state machine (hold, toggle, chords, command
//!   mode, deferred rebinding), independent of any OS.
//! - [`listener`]: the OS event source feeding the hub (CGEventTap on macOS,
//!   XInput2 raw key events on Linux X11, a low-level keyboard hook on Windows).
//! - [`paste`]: clipboard capture/restore and synthetic paste.

pub mod hub;
pub mod keys;
pub mod linux;
pub mod listener;
pub mod paste;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub use hub::{BindingWarning, Callback, HubActions, KeyboardHub, Shortcuts};
pub use keys::{
    canonical_shortcut, is_hold_key, keys_for_typed, Chord, Key, Modifier, ShortcutError,
};
pub use listener::{InputError, InputWarning};
