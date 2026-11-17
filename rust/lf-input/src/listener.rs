//! The OS keyboard event source that feeds a [`KeyboardHub`].
//!
//! macOS: a listen-only CGEventTap on the current thread's run loop, with
//! the Input Monitoring (TCC) preflight and dead-tap watchdog from PR #31.
//! Other platforms: a clear `Unsupported` error (global key-up events for a
//! bare hold key need a low-level hook that is not ported yet).

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
        }
    }
}

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
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (hub, warn);
        Err(InputError::Unsupported(
            "global hotkeys are only implemented for macOS in the Rust port; \
             use the Python app on Linux and Windows",
        ))
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
    #[cfg(target_os = "macos")]
    {
        crate::macos::run_tap_cancellable(hub, warn, stop, request_permissions)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (stop, request_permissions);
        run_forever(hub, warn)
    }
}
