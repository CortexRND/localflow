use std::sync::Arc;

use async_channel::Sender;
use objc2_app_kit::NSWorkspace;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{status_text, template_icon, Action, PasteTarget, View};

/// LocalFlow's menu bar item. Built, updated and dropped on GPUI's main
/// thread; tray-icon creates only the `NSStatusItem` and never touches
/// `NSApplication`, its delegate, activation policy or main menu.
pub(crate) struct Tray {
    _icon: TrayIcon,
    paste_target: Arc<PasteTarget>,
    status: MenuItem,
    dictation: MenuItem,
    paste: MenuItem,
}

impl Tray {
    pub fn new(view: View, actions: Sender<Action>) -> anyhow::Result<Self> {
        let menu = Menu::new();
        let open = MenuItem::with_id("open", "Open LocalFlow", true, None);
        let status = MenuItem::new(status_text(&view), false, None);
        // Plain items: a check item would flip itself before the toggle is
        // known to have happened.
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
        let (pixels, width, height) = template_icon()?;
        let from_pixels = Icon::from_rgba;
        // `set_icon` would drop template mode, so the icon is only set here.
        let icon = TrayIconBuilder::new()
            .with_icon(from_pixels(pixels, width, height)?)
            .with_icon_as_template(true)
            .with_tooltip("LocalFlow")
            .with_menu(Box::new(menu))
            .build()?;
        // Pointer events are unused; without a handler tray-icon queues them
        // in an unbounded channel for the life of the process.
        TrayIconEvent::set_event_handler(Some(|_: TrayIconEvent| {}));
        let paste_target = Arc::new(PasteTarget::default());
        let capture = paste_target.clone();
        // Runs inside AppKit's menu action on the main thread: it only records
        // the target and queues the action, never touching GPUI state.
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let action = match event.id.0.as_str() {
                "open" => Action::Open,
                "dictation" => Action::ToggleDictation,
                "paste" => {
                    // A status menu doesn't activate LocalFlow, so the user's
                    // app is still frontmost, whether opened by mouse or
                    // keyboard. While an earlier request is pending this one
                    // is dropped, so it can't swap that request's target.
                    if !capture.capture(frontmost_pid()) {
                        return;
                    }
                    Action::PasteLast
                }
                "quit" => Action::Quit,
                _ => return,
            };
            action.send(&actions);
        }));
        Ok(Self {
            _icon: icon,
            paste_target,
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

    /// True only if the app frontmost at menu selection is still frontmost
    /// and isn't LocalFlow. Call exactly once per received `PasteLast`: it
    /// consumes the pending request either way. Never activates another app.
    pub fn restore_paste_target(&self) -> bool {
        self.paste_target
            .take_if_still_frontmost(frontmost_pid(), std::process::id() as i32)
    }
}

fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|app| app.processIdentifier())
}
