use async_channel::Sender;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use super::{icon_pixels, Action, View, ICON_SIZE};

pub(crate) struct Tray {
    _icon: TrayIcon,
    status: MenuItem,
    dictation: MenuItem,
    paste: MenuItem,
}

impl Tray {
    // The native icon and menu both live on GPUI's Windows message-loop thread.
    pub fn new(view: View, actions: Sender<Action>) -> anyhow::Result<Self> {
        let menu = Menu::new();
        let open = MenuItem::with_id("open", "Open LocalFlow", true, None);
        let status = MenuItem::new(status_text(&view), false, None);
        let dictation = MenuItem::with_id(
            "dictation",
            view.model.dictation_item(),
            view.can_toggle,
            None,
        );
        let paste = MenuItem::with_id("paste", "Paste last transcript", view.can_paste, None);
        let quit = MenuItem::with_id("quit", "Quit LocalFlow", true, None);
        menu.append_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &status,
            &dictation,
            &paste,
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;
        let icon = TrayIconBuilder::new()
            .with_icon(Icon::from_rgba(icon_pixels()?, ICON_SIZE, ICON_SIZE)?)
            .with_tooltip("LocalFlow")
            .with_menu(Box::new(menu))
            .build()?;
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = match event.id.0.as_str() {
                "open" => Action::Open,
                "dictation" => Action::ToggleDictation,
                "paste" => Action::PasteLast,
                "quit" => Action::Quit,
                _ => return,
            };
            action.send(&actions);
        }));
        Ok(Self {
            _icon: icon,
            status,
            dictation,
            paste,
        })
    }

    pub fn update(&mut self, view: View) {
        self.status.set_text(status_text(&view));
        self.dictation.set_text(view.model.dictation_item());
        self.dictation.set_enabled(view.can_toggle);
        self.paste.set_enabled(view.can_paste);
    }
}

fn status_text(view: &View) -> String {
    let mut text = view.model.status_line.clone();
    if let Some(warning) = &view.model.input_warning {
        text.push_str(" — ");
        text.push_str(warning);
    }
    text.replace('&', "&&")
}
