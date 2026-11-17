//! Application delegate and run loop (port of `localflow/main_app_mac.py`, plus
//! `hub_mac.main`).
//!
//! The app process outlives its windows, so LaunchServices delivers a reopen
//! Apple event (Dock click, double-click) to this process instead of launching
//! it again; the delegate answers it by showing a window. Other processes ask
//! the running Hub for a page through a request file ([`crate::hub_route`]),
//! which a 0.25 s run-loop timer consumes; every failure of that poll is
//! swallowed so a bad request can never stop the event loop.
#![allow(deprecated)]

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use anyhow::{anyhow, Result};
use lf_core::config::load_config;
use lf_core::dictionary::DictionaryStore;
use lf_core::history::HistoryStore;
use lf_runtime::menubar::{MenuActions, MenuModel, MenuState};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate,
    NSApplicationTerminateReply,
};
use objc2_foundation::NSNotification;
use std::sync::Arc;
use std::time::Duration;

use super::actions::Timer;
use super::hub::Hub;
use super::page::HubServices;
use crate::hub_route::{self, HubWindowState};

/// Seconds between page-request polls (`_POLL_SECONDS`).
pub const POLL_SECONDS: f64 = 0.25;

const QUIT_FLUSH_TIMEOUT: Duration = Duration::from_secs(4);

/// The "show a window" action the delegate runs on reopen.
pub type ShowFn = Rc<dyn Fn()>;

/// Install the status menu without removing the Hub's Dock/reopen entry point.
pub fn install_hub_menu_bar(model: MenuModel, actions: MenuActions) -> Result<(), String> {
    lf_runtime::macos::install_menu_bar(model, actions)?;
    let mtm = MainThreadMarker::new().ok_or("the Hub menu must run on the main thread")?;
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Regular);
    Ok(())
}

enum MenuCommand {
    ToggleDictation,
    PasteLast,
    Quit,
}

pub struct AppDelegateState {
    show: ShowFn,
    before_terminate: Rc<dyn Fn()>,
    termination_started: Cell<bool>,
}

define_class!(
    /// `MainAppDelegate`: shows a window when the app is reopened with none
    /// visible. `NSApplication` keeps only a weak reference to its delegate, so
    /// [`run_app`] holds the strong one for the life of the run loop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateState]
    #[name = "LFMainAppDelegate"]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, has_visible: bool) -> bool {
            if !has_visible {
                // A panic must not unwind into AppKit.
                let show = self.ivars().show.clone();
                let _ = catch_unwind(AssertUnwindSafe(|| show()));
            }
            true
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            self.prepare_to_terminate();
            NSApplicationTerminateReply::TerminateNow
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _notification: &NSNotification) {
            self.prepare_to_terminate();
        }
    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker, show: ShowFn) -> Retained<Self> {
        Self::new_with_termination(mtm, show, None)
    }

    pub fn new_with_termination(
        mtm: MainThreadMarker,
        show: ShowFn,
        before_terminate: Option<Rc<dyn Fn()>>,
    ) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(AppDelegateState {
            show,
            before_terminate: before_terminate.unwrap_or_else(|| Rc::new(|| {})),
            termination_started: Cell::new(false),
        });
        // SAFETY: plain `init` on NSObject.
        unsafe { msg_send![super(this), init] }
    }

    fn prepare_to_terminate(&self) {
        if self.ivars().termination_started.replace(true) {
            return;
        }
        let before_terminate = self.ivars().before_terminate.clone();
        let _ = catch_unwind(AssertUnwindSafe(|| before_terminate()));
    }
}

/// Show the first window with `show`, then serve events until quit.
///
/// `poll` (optional) returns true after serving a page request; it runs on a
/// run-loop timer for the life of the app and the app is activated whenever it
/// returns true. Panics inside `poll` are swallowed.
pub fn run_app(mtm: MainThreadMarker, show: ShowFn, poll: Option<Box<dyn Fn() -> bool>>) {
    run_app_with_termination(mtm, show, poll, None);
}

fn run_app_with_termination(
    mtm: MainThreadMarker,
    show: ShowFn,
    poll: Option<Box<dyn Fn() -> bool>>,
    before_terminate: Option<Rc<dyn Fn()>>,
) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate = AppDelegate::new_with_termination(mtm, show.clone(), before_terminate);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    show();
    // Kept alive until the run loop returns; dropping invalidates the timer.
    let _poller = poll.map(|poll| {
        Timer::schedule(POLL_SECONDS, true, move || {
            // A bad request must not stop the run loop.
            if let Ok(true) = catch_unwind(AssertUnwindSafe(&poll)) {
                NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
            }
        })
    });
    app.activateIgnoringOtherApps(true);
    app.run();
    drop(delegate);
}

/// Read the user's config and local stores without starting backend services.
pub fn real_services() -> Result<Rc<HubServices>> {
    let config = load_config()?;
    let enabled = config.field_bool("history_enabled").unwrap_or(true);
    Ok(HubServices::new(
        config,
        Rc::new(HistoryStore::open(None, None, enabled)?),
        Rc::new(DictionaryStore::new(None)),
    ))
}

/// Owns "the" Hub of the app process and decides, on every launch, reopen and
/// page request, whether to reuse it or build a new one (`main_app`'s
/// `front_or_build` / `WindowRouter`).
///
/// A closed Hub is shut down for good (its pages are flushed, its timers
/// invalidated and its window delegate detached), so closing the window and then
/// clicking the Dock icon builds a **new** Hub with freshly loaded services
/// instead of showing a dead one.
pub struct HubSlot {
    mtm: MainThreadMarker,
    /// Builds the services of a new Hub (the real ones, or fakes in tests).
    services: Box<dyn Fn() -> Result<Rc<HubServices>>>,
    hub: RefCell<Option<Rc<Hub>>>,
    /// The page asked for on the command line, served by the first `open`.
    desired: Cell<Option<&'static str>>,
}

impl HubSlot {
    pub fn new(
        mtm: MainThreadMarker,
        services: impl Fn() -> Result<Rc<HubServices>> + 'static,
        desired: Option<&'static str>,
    ) -> Rc<HubSlot> {
        Rc::new(HubSlot {
            mtm,
            services: Box::new(services),
            hub: RefCell::new(None),
            desired: Cell::new(desired),
        })
    }

    /// The Hub currently held (possibly closed).
    pub fn hub(&self) -> Option<Rc<Hub>> {
        self.hub.borrow().clone()
    }

    fn state(hub: &Hub) -> HubWindowState {
        let window = hub.window();
        HubWindowState {
            closed: hub.is_closed(),
            miniaturized: window.isMiniaturized(),
            visible: window.isVisible(),
            page: hub.current_page(),
        }
    }

    /// Show the Hub: bring the live one forward (navigating when `page` is a
    /// different page) or build a new one. `page` is a validated request; with
    /// none, the command-line page is used once, then the default.
    pub fn open(&self, page: Option<&str>) -> Result<()> {
        let requested = page
            .and_then(hub_route::validate_page)
            .or_else(|| self.desired.take());
        let existing = self.hub.borrow().clone();
        let state = existing.as_deref().map(Self::state);
        match hub_route::plan_reopen(state.as_ref(), requested) {
            hub_route::ReopenPlan::Reuse {
                deminiaturize,
                navigate,
            } => {
                if let Some(hub) = existing {
                    if deminiaturize {
                        hub.window().deminiaturize(None);
                    }
                    hub.show();
                    if let Some(page) = navigate {
                        hub.show_page(page);
                    }
                }
            }
            hub_route::ReopenPlan::Build { page } => {
                let hub = Hub::new(self.mtm, (self.services)()?, page);
                hub.show();
                // The closed predecessor is released only now that its
                // replacement exists.
                *self.hub.borrow_mut() = Some(hub);
            }
        }
        Ok(())
    }
}

struct MenuRuntime {
    menu: MenuState,
}

#[derive(Default)]
struct MenuSlot {
    runtime: RefCell<Option<MenuRuntime>>,
}

impl MenuSlot {
    fn ensure(&self, commands: &std::sync::mpsc::Sender<MenuCommand>) -> Result<()> {
        if self.runtime.borrow().is_some() {
            return Ok(());
        }
        let config = load_config()?;
        let menu = MenuState::new(
            lf_runtime::platform_menu_bar(),
            config.field_bool("dictation_enabled").unwrap_or(true),
        );
        let toggle = commands.clone();
        let paste = commands.clone();
        let quit = commands.clone();
        install_hub_menu_bar(
            menu.snapshot(),
            MenuActions {
                toggle_dictation: Arc::new(move || {
                    let _ = toggle.send(MenuCommand::ToggleDictation);
                }),
                paste_last: Arc::new(move || {
                    let _ = paste.send(MenuCommand::PasteLast);
                }),
                quit: Arc::new(move || {
                    let _ = quit.send(MenuCommand::Quit);
                }),
            },
        )
        .map_err(|error| anyhow!(error))?;
        eprintln!("menu bar: enabled");
        *self.runtime.borrow_mut() = Some(MenuRuntime { menu });
        Ok(())
    }

    fn update(&self, enabled: bool) {
        if let Some(runtime) = self.runtime.borrow().as_ref() {
            runtime
                .menu
                .change(|model| model.dictation_enabled = enabled);
        }
    }
}

/// Run the Hub shell until quit; subsequent stack layers add service adapters.
pub fn run_hub(initial_page: Option<&str>) -> Result<()> {
    let mtm =
        MainThreadMarker::new().ok_or_else(|| anyhow!("the Hub must run on the main thread"))?;
    let page = initial_page
        .and_then(hub_route::canonical_page)
        .map(|name| hub_route::ported_page(Some(name)));
    let (commands, menu_commands) = std::sync::mpsc::channel();
    let menu = Rc::new(MenuSlot::default());
    let slot = HubSlot::new(mtm, real_services, page);
    let show: ShowFn = {
        let slot = slot.clone();
        let menu = menu.clone();
        let commands = commands.clone();
        Rc::new(move || {
            if let Err(error) = menu.ensure(&commands) {
                eprintln!("lf-hub: could not install the native menu: {error:#}");
            }
            if let Err(error) = slot.open(None) {
                eprintln!("lf-hub: could not open the Hub: {error:#}");
            }
        })
    };
    let poll: Box<dyn Fn() -> bool> = {
        let slot = slot.clone();
        Box::new(move || {
            for command in menu_commands.try_iter() {
                if let MenuCommand::Quit = command {
                    NSApplication::sharedApplication(mtm).terminate(None);
                    continue;
                }
                if let Some(hub) = slot.hub() {
                    let services = hub.services();
                    match command {
                        MenuCommand::ToggleDictation => {
                            let enabled = services
                                .dictation_config()
                                .field_bool("dictation_enabled")
                                .unwrap_or(true);
                            match services.update_dictation(!enabled) {
                                Ok(_) => services.clear_page_error("Home"),
                                Err(error) => services.set_page_error(
                                    "Home",
                                    &format!("Couldn't change dictation: {error}"),
                                ),
                            }
                            hub.rebuild_current_page();
                        }
                        MenuCommand::PasteLast => {
                            let fallback = lf_runtime::status::read_last_text(
                                &lf_runtime::status::last_text_path(),
                            );
                            let text = services.history.latest_text(&fallback).unwrap_or(fallback);
                            if !text.is_empty() {
                                lf_input::paste::Injector::native().paste_text(
                                    &text,
                                    services
                                        .config()
                                        .field_str("paste_method")
                                        .filter(|value| !value.is_empty())
                                        .unwrap_or("auto"),
                                    None,
                                );
                            }
                        }
                        MenuCommand::Quit => unreachable!(),
                    }
                }
            }
            if let Some(hub) = slot.hub() {
                menu.update(
                    hub.services()
                        .dictation_config()
                        .field_bool("dictation_enabled")
                        .unwrap_or(true),
                );
            }
            if let Some(page) = hub_route::consume_request() {
                match slot.open(Some(page)) {
                    Ok(()) => return true,
                    Err(error) => eprintln!("lf-hub: could not serve page request: {error:#}"),
                }
            }
            false
        })
    };
    let before_terminate: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(hub) = slot.hub() {
            hub.shutdown();
        }
        if !crate::shutdown::wait_for_pending_writes(QUIT_FLUSH_TIMEOUT) {
            log::warn!("timed out waiting for pending writes during quit");
        }
    });
    run_app_with_termination(mtm, show, Some(poll), Some(before_terminate));
    Ok(())
}
