use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use async_channel::Sender;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow};

use super::{icon_pixels, Action, View, ICON_SIZE};

pub(crate) struct Tray {
    icon: TrayIcon,
    paste_target: Arc<AtomicPtr<c_void>>,
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
        let paste_target = Arc::new(AtomicPtr::new(ptr::null_mut()));
        let capture = paste_target.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left | MouseButton::Right,
                    button_state: MouseButtonState::Down,
                    ..
                }
            ) {
                // tray-icon emits the click before foregrounding its hidden
                // menu window; remember where the user's paste belongs.
                capture.store(unsafe { GetForegroundWindow() }, Ordering::Relaxed);
            }
        }));
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
            icon,
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

    pub fn restore_paste_target(&self) -> bool {
        let target = self.paste_target.swap(ptr::null_mut(), Ordering::Relaxed);
        let owner = self.icon.window_handle();
        if target.is_null() || target == owner {
            return false;
        }
        unsafe {
            if GetForegroundWindow() == owner {
                SetForegroundWindow(target);
            }
            // Don't send Ctrl+V to the tray or a different app if activation
            // was refused or the user changed focus after opening the menu.
            GetForegroundWindow() == target
        }
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
