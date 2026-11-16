//! macOS: the NSStatusItem menu bar and frontmost-app lookup.
//!
//! The menu lives on the main thread (AppKit's rule); `MacMenuBar::update`
//! hops there with `dispatch_async` on the main queue, so updates are pushed
//! as they happen and nothing polls.

use std::cell::RefCell;
use std::sync::Arc;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValueOff, NSControlStateValueOn,
    NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWorkspace,
};
use objc2_foundation::NSString;

use crate::context::AppInfo;
use crate::menubar::{MenuActions, MenuBar, MenuModel};

pub fn frontmost_app() -> Option<AppInfo> {
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let name = app
        .localizedName()
        .map(|n| n.to_string())
        .unwrap_or_default();
    let bundle_id = app
        .bundleIdentifier()
        .map(|n| n.to_string())
        .unwrap_or_default();
    Some(AppInfo { name, bundle_id })
}

pub struct TargetIvars {
    actions: MenuActions,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LFMenuTarget"]
    #[ivars = TargetIvars]
    pub struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(toggleDictation:))]
        fn toggle_dictation(&self, _sender: Option<&AnyObject>) {
            (self.ivars().actions.toggle_dictation)();
        }

        #[unsafe(method(pasteLast:))]
        fn paste_last(&self, _sender: Option<&AnyObject>) {
            (self.ivars().actions.paste_last)();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            (self.ivars().actions.quit)();
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, actions: MenuActions) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TargetIvars { actions });
        unsafe { msg_send![super(this), init] }
    }
}

struct Ui {
    item: Retained<NSStatusItem>,
    status: Retained<NSMenuItem>,
    dictation: Retained<NSMenuItem>,
    warning: Retained<NSMenuItem>,
    _menu: Retained<NSMenu>,
    _target: Retained<MenuTarget>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Option<objc2::runtime::Sel>,
    target: &MenuTarget,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &ns(title),
            action,
            &ns(""),
        )
    };
    if action.is_some() {
        unsafe { item.setTarget(Some(target)) };
    } else {
        item.setEnabled(false);
    }
    item
}

fn render(ui: &Ui, model: &MenuModel, mtm: MainThreadMarker) {
    if let Some(button) = ui.item.button(mtm) {
        button.setTitle(&ns(&model.title()));
    }
    ui.status.setTitle(&ns(&model.status_line));
    ui.dictation.setTitle(&ns(model.dictation_item()));
    ui.dictation.setState(if model.dictation_enabled {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    match &model.input_warning {
        Some(w) => {
            ui.warning.setTitle(&ns(w));
            ui.warning.setHidden(false);
        }
        None => ui.warning.setHidden(true),
    }
}

/// Handle for pushing menu updates from any thread.
pub struct MacMenuBar;

impl MenuBar for MacMenuBar {
    fn update(&self, model: &MenuModel) {
        let model = model.clone();
        DispatchQueue::main().exec_async(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            UI.with(|ui| {
                if let Some(ui) = ui.borrow().as_ref() {
                    render(ui, &model, mtm);
                }
            });
        });
    }
}

/// Build the status item on the main thread and run the AppKit loop.
/// Never returns while the app runs.
pub fn run_menu_bar(model: MenuModel, actions: MenuActions) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("the menu bar must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let target = MenuTarget::new(mtm, actions);
    let menu = NSMenu::new(mtm);
    let status = menu_item(mtm, &model.status_line, None, &target);
    let dictation = menu_item(
        mtm,
        model.dictation_item(),
        Some(sel!(toggleDictation:)),
        &target,
    );
    let warning = menu_item(mtm, "", None, &target);
    menu.addItem(&status);
    menu.addItem(&dictation);
    menu.addItem(&warning);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(
        mtm,
        "Paste last transcript",
        Some(sel!(pasteLast:)),
        &target,
    ));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&menu_item(
        mtm,
        "Quit LocalFlow",
        Some(sel!(quit:)),
        &target,
    ));
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    item.setMenu(Some(&menu));
    let ui = Ui {
        item,
        status,
        dictation,
        warning,
        _menu: menu,
        _target: target,
    };
    render(&ui, &model, mtm);
    UI.with(|slot| *slot.borrow_mut() = Some(ui));
    app.run();
    Ok(())
}

pub fn menu_bar() -> Arc<dyn MenuBar> {
    Arc::new(MacMenuBar)
}
