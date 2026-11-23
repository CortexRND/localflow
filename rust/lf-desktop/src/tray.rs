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

const ICON_SIZE: u32 = 32;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::runtime_state::{Signal, State};

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
