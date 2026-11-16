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
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate};
use serde_json::{Map, Value};
use std::sync::Arc;

use super::actions::Timer;
use super::hub::Hub;
use super::page::HubServices;
use crate::backends::meeting::{MeetingClient, MeetingClientError, MeetingResult, SERVER_DOWN};
use crate::backends::prompts::{PromptEntry, PromptQueue, QueueError, WorkPrompt};
use crate::hub_route::{self, HubWindowState};

/// Seconds between page-request polls (`_POLL_SECONDS`).
pub const POLL_SECONDS: f64 = 0.25;

/// The "show a window" action the delegate runs on reopen.
pub type ShowFn = Rc<dyn Fn()>;

define_class!(
    /// `MainAppDelegate`: shows a window when the app is reopened with none
    /// visible. `NSApplication` keeps only a weak reference to its delegate, so
    /// [`run_app`] holds the strong one for the life of the run loop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ShowFn]
    #[name = "LFMainAppDelegate"]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, has_visible: bool) -> bool {
            if !has_visible {
                // A panic must not unwind into AppKit.
                let show = self.ivars().clone();
                let _ = catch_unwind(AssertUnwindSafe(|| show()));
            }
            true
        }
    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker, show: ShowFn) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(show);
        // SAFETY: plain `init` on NSObject.
        unsafe { msg_send![super(this), init] }
    }
}

/// Show the first window with `show`, then serve events until quit.
///
/// `poll` (optional) returns true after serving a page request; it runs on a
/// run-loop timer for the life of the app and the app is activated whenever it
/// returns true. Panics inside `poll` are swallowed.
pub fn run_app(mtm: MainThreadMarker, show: ShowFn, poll: Option<Box<dyn Fn() -> bool>>) {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate = AppDelegate::new(mtm, show.clone());
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

/// A prompt queue that reports itself unavailable instead of pretending.
///
/// There is no file-backed queue in this crate yet (the Python `queue.json`
/// store is not ported), so the Hub's Prompts page shows its real "can't read
/// the queue" state rather than an invented empty list.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailablePromptQueue;

const QUEUE_UNAVAILABLE: &str = "prompt queue not available in this build";

impl PromptQueue for UnavailablePromptQueue {
    fn add(&self, _: &[WorkPrompt], _: &[String], _: &str) -> Result<Vec<PromptEntry>, QueueError> {
        Err(QueueError::Io(QUEUE_UNAVAILABLE.into()))
    }

    fn list(&self, _status: Option<&str>) -> Result<Vec<PromptEntry>, QueueError> {
        Err(QueueError::Io(QUEUE_UNAVAILABLE.into()))
    }

    fn get(&self, _entry_id: &str) -> Result<Option<PromptEntry>, QueueError> {
        Err(QueueError::Io(QUEUE_UNAVAILABLE.into()))
    }

    fn set_status(
        &self,
        _entry_id: &str,
        _status: &str,
        _expect: Option<&[&str]>,
        _fields: &Map<String, Value>,
    ) -> Result<Option<PromptEntry>, QueueError> {
        Err(QueueError::Io(QUEUE_UNAVAILABLE.into()))
    }
}

/// A meeting client that always says the local server is not reachable (there
/// is no HTTP client in this crate); every call fails with [`SERVER_DOWN`].
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableMeetingClient;

fn down<T>() -> MeetingResult<T> {
    Err(MeetingClientError(SERVER_DOWN.to_string()))
}

impl MeetingClient for UnavailableMeetingClient {
    fn status(&self) -> MeetingResult<Value> {
        down()
    }

    fn templates(&self) -> MeetingResult<Value> {
        down()
    }

    fn start(&self, _: &str, _: &str, _: &str, _: &str, _: Option<bool>) -> MeetingResult<Value> {
        down()
    }

    fn save_notes(&self, _notes: &str) -> MeetingResult<Value> {
        down()
    }

    fn stop(&self) -> MeetingResult<Value> {
        down()
    }

    fn chat(&self, _question: &str) -> MeetingResult<Value> {
        down()
    }

    fn dismiss(&self) -> MeetingResult<()> {
        down()
    }
}

/// Build the real services from lf-core: the user's config, history database
/// and dictionary. The prompt queue and meeting client are the honest
/// "unavailable" adapters above.
pub fn real_services() -> Result<Rc<HubServices>> {
    let config = load_config()?;
    let enabled = config.field_bool("history_enabled").unwrap_or(true);
    let history = Rc::new(HistoryStore::open(None, None, enabled)?);
    let dictionary = Rc::new(DictionaryStore::new(None));
    Ok(HubServices::new(
        config,
        history,
        dictionary,
        Arc::new(UnavailablePromptQueue),
        Arc::new(UnavailableMeetingClient),
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

/// `hub_mac.main` + `main_app`: open the Hub on `initial_page` and run until
/// quit. Reopening (Dock click) with no visible window builds a new Hub; page
/// requests left by other processes are served by the same slot.
pub fn run_hub(initial_page: &str) -> Result<()> {
    let mtm =
        MainThreadMarker::new().ok_or_else(|| anyhow!("the Hub must run on the main thread"))?;
    let page = hub_route::canonical_page(initial_page)
        .ok_or_else(|| anyhow!("unknown Hub page: {initial_page:?}"))?;
    // Fail on unreadable data before any window exists, like Python's `main`.
    let first = real_services()?;
    let handed_out = Cell::new(Some(first));
    let slot = HubSlot::new(
        mtm,
        move || match handed_out.take() {
            Some(services) => Ok(services),
            None => real_services(),
        },
        Some(page),
    );
    let show: ShowFn = {
        let slot = slot.clone();
        Rc::new(move || {
            if let Err(error) = slot.open(None) {
                eprintln!("lf-hub: could not open the Hub: {error:#}");
            }
        })
    };
    let poll: Box<dyn Fn() -> bool> = {
        let slot = slot.clone();
        Box::new(move || match hub_route::consume_request() {
            Some(page) => {
                if let Err(error) = slot.open(Some(page)) {
                    eprintln!("lf-hub: could not open the Hub: {error:#}");
                }
                true
            }
            None => false,
        })
    };
    run_app(mtm, show, Some(poll));
    Ok(())
}
