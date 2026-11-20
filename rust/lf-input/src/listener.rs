//! The OS keyboard event source that feeds a [`KeyboardHub`].
//!
//! macOS: a listen-only CGEventTap on the current thread's run loop, with
//! the Input Monitoring (TCC) preflight and dead-tap watchdog from PR #31.
//! Linux: listen-only XInput2 raw key events on X11 (see [`crate::linux`]);
//! Wayland and display-less sessions get an explicit `Unsupported` error.
//! Other platforms: a clear `Unsupported` error.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::hub::KeyboardHub;

/// kIOHIDRequestTypeListenEvent — the Input Monitoring TCC service.
pub const LISTEN_EVENT: u32 = 1;
/// A live tap sees *something* well within this window of normal use;
/// silence this long almost always means TCC is dropping events.
pub const DEAD_TAP: Duration = Duration::from_secs(15);

pub const INPUT_MONITORING_HELP: &str =
    "hold-to-talk and global hotkeys cannot see keystrokes until Input Monitoring \
is enabled for the app that launched LocalFlow — 'LocalFlow' when started \
from the app, or your terminal app / 'Python' when run from a terminal — in \
System Settings → Privacy & Security → Input Monitoring. Restart LocalFlow \
after granting; macOS does not re-arm a running event tap.";

/// Something the user must be told about (shown as a notification and in
/// the menu bar by the runtime).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputWarning {
    InputMonitoringDenied,
    DeadTap,
}

impl fmt::Display for InputWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputWarning::InputMonitoringDenied => {
                write!(f, "Input Monitoring not granted: {INPUT_MONITORING_HELP}")
            }
            InputWarning::DeadTap => write!(
                f,
                "no keyboard events received {}s after start; if hold-to-talk does not \
                 respond, the event tap is dead: {INPUT_MONITORING_HELP}",
                DEAD_TAP.as_secs()
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputError {
    /// The OS refused to create the tap (Accessibility/Input Monitoring).
    TapCreateFailed,
    Unsupported(&'static str),
    /// The platform event source failed or disconnected (X server gone,
    /// XInput2 missing). The text never contains key data.
    Backend(String),
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputError::TapCreateFailed => {
                write!(
                    f,
                    "could not create the keyboard event tap: {INPUT_MONITORING_HELP}"
                )
            }
            InputError::Unsupported(what) => write!(f, "{what}"),
            InputError::Backend(what) => write!(f, "keyboard listener failed: {what}"),
        }
    }
}

/// Told once the platform accepted the listener (macOS: tap installed with
/// Input Monitoring granted / X11: raw key subscription confirmed by the
/// server). Nothing calls it for a listener
/// that failed to register, so "listening" is never inferred from a thread.
pub type ReadySink = Arc<dyn Fn() + Send + Sync>;

/// What this build and session can do for global shortcuts, without
/// starting anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Support {
    /// macOS event tap (still subject to Input Monitoring).
    MacOs,
    /// A native X11 session: XInput2 raw key events.
    X11,
    /// Can't work here, with the reason to show.
    Unsupported(&'static str),
}

pub fn support() -> Support {
    #[cfg(target_os = "macos")]
    {
        Support::MacOs
    }
    #[cfg(target_os = "linux")]
    {
        match crate::linux::check_session(crate::linux::session()) {
            Ok(()) => Support::X11,
            Err(InputError::Unsupported(why)) => Support::Unsupported(why),
            Err(_) => Support::Unsupported(crate::linux::NO_DISPLAY),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Support::Unsupported(UNSUPPORTED_PLATFORM)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const UNSUPPORTED_PLATFORM: &str = "global hotkeys are only implemented for macOS and Linux (X11) \
in the Rust port; use the Python app on Windows";

impl std::error::Error for InputError {}

pub type WarningSink = Arc<dyn Fn(InputWarning) + Send + Sync>;

/// Only watch for a dead tap when access is denied or unknown: with a
/// confirmed grant, silence is just a user who hasn't typed yet.
pub fn watchdog_armed(granted: bool) -> bool {
    !granted
}

/// Run after [`DEAD_TAP`] of the watchdog: warn when the tap never saw a key.
pub fn check_tap_alive(hub: &KeyboardHub, warn: &WarningSink) -> bool {
    if hub.saw_any_event() {
        return true;
    }
    emit(warn, InputWarning::DeadTap);
    false
}

pub(crate) fn emit(warn: &WarningSink, warning: InputWarning) {
    log::warn!("{warning}");
    warn(warning);
}

/// Block on the platform event loop, feeding `hub` until it exits.
pub fn run_forever(hub: &KeyboardHub, warn: WarningSink) -> Result<(), InputError> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::run_tap(hub, warn)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = warn;
        crate::linux::run_x11(hub, &std::sync::atomic::AtomicBool::new(false), &|| {})
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (hub, warn);
        Err(InputError::Unsupported(UNSUPPORTED_PLATFORM))
    }
}

/// Input Monitoring state: Some(true/false) on macOS, None elsewhere.
pub fn input_monitoring_granted() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        Some(crate::macos::input_monitoring_granted())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

pub fn run_cancellable(
    hub: &KeyboardHub,
    warn: WarningSink,
    stop: Arc<std::sync::atomic::AtomicBool>,
    request_permissions: bool,
) -> Result<(), InputError> {
    run_cancellable_with_ready(hub, warn, stop, request_permissions, Arc::new(|| {}))
}

/// [`run_cancellable`], calling `ready` once the platform registered the
/// listener (before any key event is delivered).
pub fn run_cancellable_with_ready(
    hub: &KeyboardHub,
    warn: WarningSink,
    stop: Arc<std::sync::atomic::AtomicBool>,
    request_permissions: bool,
    ready: ReadySink,
) -> Result<(), InputError> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::run_tap_cancellable_ready(hub, warn, stop, request_permissions, ready)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (warn, request_permissions);
        crate::linux::run_x11(hub, &stop, ready.as_ref())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (stop, request_permissions, ready);
        run_forever(hub, warn)
    }
}
