//! Native tray for the GPUI window, using the shared menubar model.

use async_channel::Sender;
use lf_runtime::menubar::MenuModel;

use crate::controllers::runtime_state::{OwnEngine, RuntimeView};

#[cfg(target_os = "linux")]
#[path = "tray/linux.rs"]
mod platform;
#[cfg(target_os = "windows")]
#[path = "tray/windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "tray/macos.rs"]
mod platform;

pub(crate) use platform::Tray;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Open,
    ToggleDictation,
    PasteLast,
    Quit,
}

impl Action {
    fn send(self, actions: &Sender<Self>) {
        let _ = actions.try_send(self);
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct View {
    pub model: MenuModel,
    pub can_toggle: bool,
    pub can_paste: bool,
}

impl View {
    pub fn new(runtime: &RuntimeView, enabled: bool, engine: Option<OwnEngine>) -> Self {
        Self {
            model: MenuModel {
                dictation_enabled: enabled,
                status_line: runtime.lead.clone(),
                input_warning: runtime.warnings.first().cloned(),
                ..MenuModel::default()
            },
            can_toggle: !matches!(engine, Some(OwnEngine::Starting | OwnEngine::Stopping)),
            can_paste: engine == Some(OwnEngine::Running),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
const ICON_SIZE: u32 = 32;

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn icon_pixels() -> image::ImageResult<Vec<u8>> {
    let icon = image::load_from_memory_with_format(
        include_bytes!("../../../packaging/macos/LocalFlow.png"),
        image::ImageFormat::Png,
    )?;
    Ok(icon
        .resize_exact(ICON_SIZE, ICON_SIZE, image::imageops::FilterType::Lanczos3)
        .into_rgba8()
        .into_raw())
}

/// The menu bar icon: `assets/brand.svg` as a black-on-transparent raster,
/// used as an `NSImage` template so macOS tints it for the menu bar.
#[cfg(target_os = "macos")]
fn template_icon() -> image::ImageResult<(Vec<u8>, u32, u32)> {
    let icon = image::load_from_memory_with_format(
        include_bytes!("../assets/tray-template.png"),
        image::ImageFormat::Png,
    )?
    .into_rgba8();
    let (width, height) = icon.dimensions();
    Ok((icon.into_raw(), width, height))
}

/// One status row: the runtime lead plus the first input warning. Both menu
/// libraries strip single `&` as a mnemonic marker and render `&&` as `&`.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn status_text(view: &View) -> String {
    let mut text = view.model.status_line.clone();
    if let Some(warning) = &view.model.input_warning {
        text.push_str(" — ");
        text.push_str(warning);
    }
    text.replace('&', "&&")
}

/// The app that was frontmost when "Paste last transcript" was chosen.
///
/// At most one request is pending: the menu callback reserves the slot and
/// queues a single action, and every receipt of that action consumes it. A
/// second selection before then is dropped rather than replacing the target
/// the first request will be checked against. Paste goes ahead only when the
/// same foreign app is still frontmost; nothing is activated and no "previous
/// app" is guessed.
#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
pub(crate) struct PasteTarget(std::sync::atomic::AtomicI32);

#[cfg(any(target_os = "macos", test))]
impl PasteTarget {
    /// Nothing pending. `0` never names a frontmost app.
    const EMPTY: i32 = 0;
    /// Pending, but no usable frontmost app was seen. Never passes the check
    /// and still holds the slot until consumed.
    const UNKNOWN: i32 = -1;

    /// Reserves the slot for `pid`. False if a request is already pending, in
    /// which case no action may be sent.
    pub fn capture(&self, pid: Option<i32>) -> bool {
        let pid = pid.filter(|&p| p != Self::EMPTY).unwrap_or(Self::UNKNOWN);
        self.0
            .compare_exchange(
                Self::EMPTY,
                pid,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    /// Consumes the pending request (one-shot) and checks it against `current`.
    pub fn take_if_still_frontmost(&self, current: Option<i32>, own: i32) -> bool {
        let captured = self
            .0
            .swap(Self::EMPTY, std::sync::atomic::Ordering::AcqRel);
        paste_target_ok(captured, current, own)
    }
}

/// Rejects an unknown or invalid pid (AppKit reports `-1` for apps without
/// one), LocalFlow itself, and a frontmost app that changed between reads.
#[cfg(any(target_os = "macos", test))]
fn paste_target_ok(captured: i32, current: Option<i32>, own: i32) -> bool {
    captured > 0 && captured != own && current == Some(captured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::runtime_state::{Signal, State};

    const OWN: i32 = 4242;

    #[test]
    fn paste_target_requires_the_same_foreign_app_at_both_reads() {
        assert!(paste_target_ok(77, Some(77), OWN));
        // No frontmost app now, or nothing pending.
        assert!(!paste_target_ok(77, None, OWN));
        assert!(!paste_target_ok(PasteTarget::EMPTY, Some(77), OWN));
        assert!(!paste_target_ok(PasteTarget::EMPTY, Some(0), OWN));
        // Unknown at selection time (also AppKit's "no pid").
        assert!(!paste_target_ok(PasteTarget::UNKNOWN, Some(-1), OWN));
        assert!(!paste_target_ok(-5, Some(-5), OWN));
        // LocalFlow itself, and focus that moved between the two reads.
        assert!(!paste_target_ok(OWN, Some(OWN), OWN));
        assert!(!paste_target_ok(77, Some(78), OWN));
        assert!(!paste_target_ok(77, Some(OWN), OWN));
    }

    #[test]
    fn paste_target_is_consumed_once() {
        let target = PasteTarget::default();
        // Nothing pending: never paste.
        assert!(!target.take_if_still_frontmost(Some(77), OWN));

        assert!(target.capture(Some(77)));
        assert!(target.take_if_still_frontmost(Some(77), OWN));
        // A second receipt without a new selection is refused.
        assert!(!target.take_if_still_frontmost(Some(77), OWN));

        // A refused check still consumes the request.
        assert!(target.capture(Some(77)));
        assert!(!target.take_if_still_frontmost(Some(78), OWN));
        assert!(!target.take_if_still_frontmost(Some(77), OWN));
    }

    #[test]
    fn a_pending_paste_target_is_never_replaced() {
        let target = PasteTarget::default();
        // Request A at 77 is pending; B at 78 must not replace it.
        assert!(target.capture(Some(77)));
        assert!(!target.capture(Some(78)));
        // A is processed after focus moved to 78: refused, not pasted into 78.
        assert!(!target.take_if_still_frontmost(Some(78), OWN));
        // Once A is consumed, B can reserve and succeed exactly once.
        assert!(target.capture(Some(78)));
        assert!(target.take_if_still_frontmost(Some(78), OWN));
        assert!(!target.take_if_still_frontmost(Some(78), OWN));
    }

    #[test]
    fn an_unknown_pending_target_blocks_replacement_until_consumed() {
        let target = PasteTarget::default();
        for unknown in [None, Some(0), Some(-1)] {
            assert!(target.capture(unknown));
            assert!(!target.capture(Some(77)), "{unknown:?} was replaced");
            assert!(!target.take_if_still_frontmost(Some(77), OWN));
            // Consumed: the next valid request goes through.
            assert!(target.capture(Some(77)));
            assert!(target.take_if_still_frontmost(Some(77), OWN));
        }
        // LocalFlow frontmost at selection still holds the slot, then refuses.
        assert!(target.capture(Some(OWN)));
        assert!(!target.capture(Some(77)));
        assert!(!target.take_if_still_frontmost(Some(OWN), OWN));
    }

    #[test]
    fn renders_runtime_status_and_only_enables_paste_for_an_owned_engine() {
        let runtime = RuntimeView {
            state: State::Listening,
            lead: "Shortcut listening".into(),
            support: String::new(),
            sidebar: (Signal::Success, String::new(), String::new()),
            shortcut: (Signal::Success, String::new()),
            microphone: (Signal::Warning, String::new()),
            warnings: vec!["Paste tool missing".into()],
            listening: true,
            busy: false,
            alive: true,
            pid: Some(42),
            external_owner: Some(42),
        };
        let external = View::new(&runtime, false, Some(OwnEngine::OwnedElsewhere(Some(42))));
        assert_eq!(external.model.dictation_item(), "Dictation: Off");
        assert_eq!(external.model.status_line, runtime.lead);
        assert_eq!(
            external.model.input_warning.as_deref(),
            Some("Paste tool missing")
        );
        assert!(external.can_toggle);
        assert!(!external.can_paste);

        let starting = View::new(&runtime, true, Some(OwnEngine::Starting));
        assert!(!starting.can_toggle);
        assert!(!starting.can_paste);

        let running = View::new(&runtime, true, Some(OwnEngine::Running));
        assert_eq!(running.model.dictation_item(), "Dictation: On");
        assert!(running.can_toggle);
        assert!(running.can_paste);
    }
}
