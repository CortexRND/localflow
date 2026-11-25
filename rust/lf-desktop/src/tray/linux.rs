use async_channel::Sender;
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};

use super::{icon_pixels, Action, View, ICON_SIZE};
use crate::controllers::runtime_state::compact_paste_tool_warning;

pub(crate) struct Tray(Handle<Item>);

impl Tray {
    pub fn new(view: View, actions: Sender<Action>) -> anyhow::Result<Self> {
        let mut data = icon_pixels()?;
        for pixel in data.as_chunks_mut::<4>().0 {
            pixel.rotate_right(1); // StatusNotifierItem expects ARGB, not RGBA.
        }
        Ok(Self(
            Item {
                view,
                actions,
                icon: ksni::Icon {
                    width: ICON_SIZE as i32,
                    height: ICON_SIZE as i32,
                    data,
                },
            }
            .spawn()?,
        ))
    }

    pub fn update(&mut self, view: View) {
        self.0.update(|item| item.view = view);
    }
}

struct Item {
    view: View,
    actions: Sender<Action>,
    icon: ksni::Icon,
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "localflow-desktop".into()
    }

    fn title(&self) -> String {
        "LocalFlow".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }

    fn activate(&mut self, _: i32, _: i32) {
        Action::Open.send(&self.actions);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        fn item(label: &str, enabled: bool, action: Option<Action>) -> MenuItem<Item> {
            StandardItem {
                label: label.replace('_', "__"),
                enabled,
                activate: Box::new(move |tray: &mut Item| {
                    if enabled {
                        if let Some(action) = action {
                            action.send(&tray.actions);
                        }
                    }
                }),
                ..Default::default()
            }
            .into()
        }

        let mut menu = vec![
            item("Open LocalFlow", true, Some(Action::Open)),
            MenuItem::Separator,
            item(&self.view.model.status_line, false, None),
        ];
        if let Some(warning) = &self.view.model.input_warning {
            let compact = compact_paste_tool_warning(warning);
            menu.push(item(compact.as_deref().unwrap_or(warning), false, None));
        }
        menu.extend([
            item(
                self.view.model.dictation_item(),
                self.view.can_toggle,
                Some(Action::ToggleDictation),
            ),
            item(
                "Paste last transcript",
                self.view.can_paste,
                Some(Action::PasteLast),
            ),
            MenuItem::Separator,
            item("Quit LocalFlow", true, Some(Action::Quit)),
        ]);
        menu
    }
}

#[cfg(test)]
mod tests {
    use ksni::Tray as _;
    use lf_runtime::singleinstance::LockProbe;
    use lf_runtime::status::StatusFile;

    use super::*;
    use crate::controllers::runtime_state::{
        derive, Context, FileObs, Observation, OwnEngine, Session,
    };

    fn rows(file: FileObs, session: Session, tool: &'static str) -> (Vec<String>, Vec<String>) {
        let obs = Observation {
            file,
            pid_alive: false,
            lock: LockProbe::Free,
            lock_identity: None,
            own_instance: None,
            now: 2_000_000_000.0,
            own: OwnEngine::None,
        };
        let cx = Context {
            enabled: true,
            key: "Right Ctrl".into(),
            session,
            configured_mic: String::new(),
            missing_paste_tool: Some(tool),
        };
        let runtime = derive(&obs, &cx);
        let item = Item {
            view: View::new(&runtime, true, None),
            actions: async_channel::unbounded().0,
            icon: ksni::Icon {
                width: 0,
                height: 0,
                data: Vec::new(),
            },
        };
        let labels = item
            .menu()
            .into_iter()
            .filter_map(|row| match row {
                MenuItem::Standard(StandardItem { label, .. }) => Some(label),
                _ => None,
            })
            .collect();
        (labels, runtime.warnings)
    }

    #[test]
    fn a_missing_paste_tool_is_a_short_tray_row_but_full_guidance_on_home() {
        for (session, tool) in [(Session::Wayland, "wtype"), (Session::X11, "xdotool")] {
            let (labels, warnings) = rows(FileObs::Absent, session, tool);
            assert_eq!(labels[2], format!("Paste needs {tool} (not installed)"));
            assert_eq!(
                warnings,
                [format!(
                    "Pasting dictated text needs `{tool}`, which isn't installed. Install it with your package manager."
                )]
            );
            assert_eq!(labels[3], "Dictation: On");

            // An earlier runtime warning still leads, unchanged.
            let file = StatusFile {
                warnings: vec!["speech-model".into()],
                ..StatusFile::default()
            };
            let (labels, warnings) = rows(FileObs::Present(file), session, tool);
            let speech = "The speech model couldn't be loaded. Check Models & keys.";
            assert_eq!(labels[2..4], [speech, "Dictation: On"]);
            assert_eq!(warnings[0], speech);
            assert!(warnings[1].contains(&format!("`{tool}`")));
        }
    }
}
