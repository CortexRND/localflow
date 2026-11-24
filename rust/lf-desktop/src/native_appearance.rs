//! Native window chrome (titlebar, traffic lights) follows the chosen
//! [`AppearanceMode`].
//!
//! GPUI 0.2.2 has no appearance setter and paints only the content, so on
//! macOS an explicit Light/Dark choice would otherwise sit under chrome drawn
//! for the OS setting. Setting `NSApplication.appearance` (Aqua / DarkAqua,
//! or nil to inherit the OS) recolours every window of this process and is
//! also what GPUI's `App::window_appearance` reads back (it queries
//! `NSApp.effectiveAppearance`). Uses the typed `objc2-app-kit` bindings; no
//! window handles are touched. Other platforms: a no-op.
//!
//! AppKit may notify the window views synchronously, and GPUI's handler for
//! that borrows the app mutably, so [`set`] must not run inside a GPUI
//! update while a window exists (see `theme::apply_mode`).

use crate::appearance::AppearanceMode;

/// Whether [`set`] changes anything on this platform.
pub const SUPPORTED: bool = cfg!(target_os = "macos");

/// Force (Light/Dark) or release (System) this process's native appearance.
/// Must be called on the main thread; elsewhere it does nothing.
#[cfg(target_os = "macos")]
pub fn set(mode: AppearanceMode) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    };

    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("native appearance: not on the main thread; chrome left unchanged");
        return;
    };
    // SAFETY: reading AppKit's immutable, process-lifetime name constants.
    let name = match mode {
        AppearanceMode::System => None,
        AppearanceMode::Light => Some(unsafe { NSAppearanceNameAqua }),
        AppearanceMode::Dark => Some(unsafe { NSAppearanceNameDarkAqua }),
    };
    let appearance = name.and_then(NSAppearance::appearanceNamed);
    if name.is_some() && appearance.is_none() {
        log::warn!("native appearance: {} unavailable", mode.label());
    }
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
}

#[cfg(not(target_os = "macos"))]
pub fn set(_mode: AppearanceMode) {}
