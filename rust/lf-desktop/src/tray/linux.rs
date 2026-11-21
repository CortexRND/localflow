use async_channel::Sender;
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};

use super::{icon_pixels, Action, View, ICON_SIZE};

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
            menu.push(item(warning, false, None));
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
