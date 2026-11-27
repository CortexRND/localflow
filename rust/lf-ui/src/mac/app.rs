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
use std::os::unix::fs::PermissionsExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::Command;
use std::rc::{Rc, Weak};
use std::sync::mpsc;

use anyhow::{anyhow, Result};
use lf_core::config::load_config;
use lf_core::dictionary::DictionaryStore;
use lf_core::history::HistoryStore;
use lf_runtime::menubar::{MeetingStatusPoller, MenuActions, MenuModel, MenuState};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSRunningApplication,
};
use objc2_foundation::NSNotification;
use std::sync::Arc;
use std::time::Duration;

use super::actions::Timer;
use super::hub::Hub;
use super::page::HubServices;
use super::setup::{
    FlowApplyRunner, FlowChecksRunner, FlowModelBackend, SetupContext, SetupHooks, SetupServices,
    SetupWindow,
};
use super::setup_audio::{NativeSetupPlatform, RecorderMicLevelSource};
use crate::backends::http::{HttpAskBackend, HttpMeetingClient, HttpPromptQueue, HttpTransport};
use crate::hub_route::{self, HubWindowState};
use crate::setup_flow::{FileConfigStore, LaunchAgent, SetupPlatform};

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

#[cfg(test)]
mod runtime_start_tests {
    use super::*;

    #[test]
    fn reentrant_start_does_not_install_another_status_item_or_start_services() {
        let menu = MenuSlot::default();
        menu.starting.set(true);
        let (commands, _) = mpsc::channel();
        let transport = Rc::new(RefCell::new(None));
        menu.ensure_runtime(hub_route::RuntimeStart::Full, &commands, &transport)
            .unwrap();
        assert!(menu.runtime.borrow().is_none());
        assert!(menu.starting.get());
    }

    fn menu_without_dictation(enabled: bool) -> MenuSlot {
        let menu = MenuSlot::default();
        *menu.runtime.borrow_mut() = Some(MenuRuntime {
            menu: MenuState::new(None, enabled),
            lifecycle: hub_route::RuntimeLifecycle::default(),
            poller: None,
            menu_lock: None,
            #[cfg(feature = "runtime")]
            bootstrap: None,
            #[cfg(feature = "runtime")]
            activation: None,
            #[cfg(feature = "runtime")]
            embedded: None,
        });
        menu
    }

    #[test]
    fn without_a_dictation_runtime_the_menu_mirrors_the_hub_and_is_not_authoritative() {
        let empty = MenuSlot::default();
        assert_eq!(empty.runtime_dictation(), None);
        // Nothing to mirror into: the (possibly file-reading) value is not read.
        empty.update(|| panic!("no menu runtime: the Hub value must not be read"));

        // Server-only setup, a failed activation or a non-runtime build.
        let menu = menu_without_dictation(true);
        assert_eq!(menu.runtime_dictation(), None);
        menu.update(|| false);
        let mirrored = menu.runtime.borrow().as_ref().unwrap().menu.snapshot();
        assert!(!mirrored.dictation_enabled);
        // The mirrored menu still is not reported as runtime state.
        assert_eq!(menu.runtime_dictation(), None);
    }
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
    let _signals = crate::termination::SignalHandlers::install()
        .map_err(|error| log::error!("could not install graceful termination handlers: {error}"))
        .ok();
    let _signal_poller = Timer::schedule(POLL_SECONDS, true, move || {
        crate::termination::poll(|| NSApplication::sharedApplication(mtm).terminate(None));
    });
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

const AGENT_LABEL: &str = "com.cortexrnd.localflow";

struct NativeLaunchAgent;

impl NativeLaunchAgent {
    fn plist_path() -> Result<std::path::PathBuf> {
        Ok(lf_core::home::python_home()?
            .join("Library/LaunchAgents")
            .join(format!("{AGENT_LABEL}.plist")))
    }

    fn target() -> String {
        // SAFETY: getuid only reads the process's real user id.
        let uid = unsafe { libc::getuid() };
        format!("gui/{uid}/{AGENT_LABEL}")
    }

    fn xml(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

impl LaunchAgent for NativeLaunchAgent {
    fn status(&self) -> std::result::Result<String, String> {
        let path = Self::plist_path().map_err(|error| error.to_string())?;
        if !path.exists() {
            return Ok("not installed".into());
        }
        let result = Command::new("launchctl")
            .args(["print", &Self::target()])
            .output()
            .map_err(|error| error.to_string())?;
        if !result.status.success() {
            return Ok(format!("not loaded ({AGENT_LABEL})"));
        }
        let output = String::from_utf8_lossy(&result.stdout);
        if output.contains("state = running") {
            Ok(format!("running ({AGENT_LABEL})"))
        } else {
            Ok(format!("loaded, not running ({AGENT_LABEL})"))
        }
    }

    fn install(&self) -> std::result::Result<(), String> {
        let current = std::env::current_exe().map_err(|error| error.to_string())?;
        if let Some(helper) = crate::launch_agent::bundled_helper(&current) {
            // Called from this running app: the helper keeps this process
            // running and defers loading the agent to the next login.
            let result = Command::new("/bin/bash")
                .args(crate::launch_agent::install_rust_args(
                    &helper,
                    &current,
                    std::process::id(),
                ))
                .output()
                .map_err(|error| error.to_string())?;
            return if result.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "Rust LaunchAgent install failed: {}",
                    String::from_utf8_lossy(&result.stderr).trim()
                ))
            };
        }
        let path = Self::plist_path().map_err(|error| error.to_string())?;
        let parent = path
            .parent()
            .ok_or("launch agent has no parent directory")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let logs = lf_core::home::localflow_dir();
        std::fs::create_dir_all(&logs).map_err(|error| error.to_string())?;
        let sibling = std::env::current_exe()
            .map_err(|error| error.to_string())?
            .with_file_name("localflow");
        let exe = std::iter::once(sibling)
            .chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                    .map(|directory| directory.join("localflow")),
            )
            .find(|path| {
                path.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .ok_or(
                "The localflow runtime executable is not installed; cannot enable start on login.",
            )?;
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{AGENT_LABEL}</string><key>ProgramArguments</key><array><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>LimitLoadToSessionType</key><string>Aqua</string><key>StandardOutPath</key><string>{}</string><key>StandardErrorPath</key><string>{}</string></dict></plist>\n",
            Self::xml(&exe.to_string_lossy()),
            Self::xml(&logs.join("agent.out.log").to_string_lossy()),
            Self::xml(&logs.join("agent.err.log").to_string_lossy()),
        );
        std::fs::write(&path, plist).map_err(|error| error.to_string())?;
        let _ = Command::new("launchctl")
            .args(["bootout", &Self::target()])
            .output();
        // Clear a disable override left by `disable_autostart`.
        let _ = Command::new("launchctl")
            .args(["enable", &Self::target()])
            .output();
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let result = Command::new("launchctl")
            .arg("bootstrap")
            .arg(domain)
            .arg(&path)
            .output()
            .map_err(|error| error.to_string())?;
        if result.status.success() {
            return Ok(());
        }
        let bootstrap_error = String::from_utf8_lossy(&result.stderr).trim().to_string();
        let fallback = Command::new("launchctl")
            .arg("load")
            .arg("-w")
            .arg(&path)
            .output()
            .map_err(|error| error.to_string())?;
        if fallback.status.success() {
            Ok(())
        } else {
            let load_error = String::from_utf8_lossy(&fallback.stderr).trim().to_string();
            Err(format!(
                "launchctl bootstrap failed: {bootstrap_error}; launchctl load fallback also failed: {load_error}"
            ))
        }
    }

    fn uninstall(&self) -> std::result::Result<(), String> {
        let path = Self::plist_path().map_err(|error| error.to_string())?;
        crate::launch_agent::uninstall(&path, || {
            let _ = Command::new("launchctl")
                .args(["bootout", &Self::target()])
                .output();
        })
        .map_err(|error| error.to_string())
    }

    fn disable_autostart(&self) -> std::result::Result<(), String> {
        let current = std::env::current_exe().map_err(|error| error.to_string())?;
        if let Some(helper) = crate::launch_agent::bundled_helper(&current) {
            let result = Command::new("/bin/bash")
                .args(crate::launch_agent::disable_autostart_args(&helper))
                .output()
                .map_err(|error| error.to_string())?;
            return if result.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "Disabling start on login failed: {}",
                    String::from_utf8_lossy(&result.stderr).trim()
                ))
            };
        }
        let path = Self::plist_path().map_err(|error| error.to_string())?;
        crate::launch_agent::disable_autostart(&path, || {
            let result = Command::new("launchctl")
                .args(["disable", &Self::target()])
                .output()
                .map_err(|error| error.to_string())?;
            if result.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "launchctl disable failed: {}",
                    String::from_utf8_lossy(&result.stderr).trim()
                ))
            }
        })
    }
}

/// Read local stores and prepare the authenticated runtime adapters.
pub fn real_services() -> Result<(Rc<HubServices>, HttpTransport)> {
    real_services_using(None)
}

fn real_services_using(
    existing_http: Option<HttpTransport>,
) -> Result<(Rc<HubServices>, HttpTransport)> {
    let config = load_config()?;
    let enabled = config.field_bool("history_enabled").unwrap_or(true);
    let history = Rc::new(HistoryStore::open(None, None, enabled)?);
    let dictionary = Rc::new(DictionaryStore::new(None));
    let http = match existing_http {
        Some(http) => http,
        None => HttpTransport::from_config(&config)?,
    };
    let services = HubServices::new(
        config,
        history,
        dictionary,
        Arc::new(HttpPromptQueue(http.clone())),
        Arc::new(HttpMeetingClient(http.clone())),
        Box::new(HttpAskBackend::new(http.clone())),
    );
    *services.settings_agent.borrow_mut() =
        crate::backends::settings_agent_native::native_settings_agent()
            .map_err(|error| anyhow!(error.0))?;
    *services.perf_summary.borrow_mut() = Some(Rc::new(crate::hub_model::load_perf_summary));
    Ok((services, http))
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

/// Apply one [`hub_route::DictationStatusSync`] poll to the Hub: adopt the
/// runtime's dictation state into the in-memory config (that one field only,
/// never written to disk) and re-render Home when it is the visible page.
/// While runtime and Hub agree this is an in-memory comparison only; the
/// Hub's config source is read only to resolve a disagreement.
fn sync_hub_dictation(
    sync: &mut hub_route::DictationStatusSync,
    runtime: Option<bool>,
    hub: Option<&Hub>,
) {
    let state = hub.map(HubSlot::state);
    let cached = hub.and_then(|hub| {
        hub.services()
            .config
            .borrow()
            .field_bool("dictation_enabled")
    });
    let steps = sync.poll(runtime, cached, state.as_ref(), || {
        hub.and_then(|hub| hub.services().persisted_dictation())
    });
    let Some(hub) = hub else {
        return;
    };
    if let Some(enabled) = steps.adopt {
        hub.services()
            .config
            .borrow_mut()
            .set_field("dictation_enabled", serde_json::Value::Bool(enabled));
    }
    if steps.rebuild_home {
        hub.rebuild_current_page();
    }
}

/// The one in-process runtime, driven by [`hub_route::RuntimeLifecycle`]:
/// unconfigured setup gets only the server; the first Full request (configured
/// launch, or Finish) adds the status menu, its poller and dictation, which is
/// activated on that same server with config loaded after setup saved it.
struct MenuRuntime {
    menu: MenuState,
    lifecycle: hub_route::RuntimeLifecycle,
    poller: Option<MeetingStatusPoller>,
    menu_lock: Option<lf_runtime::singleinstance::SingleInstance>,
    #[cfg(feature = "runtime")]
    bootstrap: Option<mpsc::Receiver<crate::embedded::EmbeddedRuntime>>,
    /// Dictation activation in flight; the runtime is moved to its worker.
    #[cfg(feature = "runtime")]
    activation: Option<mpsc::Receiver<(crate::embedded::EmbeddedRuntime, bool)>>,
    #[cfg(feature = "runtime")]
    embedded: Option<crate::embedded::EmbeddedRuntime>,
}

/// Persist the menu's dictation toggle. Only a Full (configured) runtime gets
/// one, so toggling can never write a config during first-run setup.
#[cfg(feature = "runtime")]
fn dictation_toggle() -> lf_runtime::run::ToggleStore {
    Arc::new(|| {
        crate::config_write::update(None, |current| {
            let mut updated = current.clone();
            updated.set_field(
                "dictation_enabled",
                serde_json::json!(!current.field_bool("dictation_enabled").unwrap_or(true)),
            );
            Ok(updated)
        })
        .map(|config| config.field_bool("dictation_enabled").unwrap_or(true))
        .map_err(|e| e.to_string())
    })
}

#[derive(Default)]
struct MenuSlot {
    runtime: RefCell<Option<MenuRuntime>>,
    starting: Cell<bool>,
}

impl MenuSlot {
    fn actions(commands: &mpsc::Sender<MenuCommand>) -> MenuActions {
        let toggle = commands.clone();
        let paste = commands.clone();
        let quit = commands.clone();
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
        }
    }
    /// Idempotently bring the runtime up to `start`: never a second server,
    /// menu or dictation runtime, and never a downgrade.
    fn ensure_runtime(
        &self,
        start: hub_route::RuntimeStart,
        commands: &std::sync::mpsc::Sender<MenuCommand>,
        transport: &Rc<RefCell<Option<HttpTransport>>>,
    ) -> Result<()> {
        if self.starting.replace(true) {
            return Ok(());
        }
        struct Reset<'a>(&'a Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.starting);
        // Collect a finished server start first, so a Finish that arrives
        // after it landed activates now instead of on the next poll.
        #[cfg(feature = "runtime")]
        self.drive();
        let mut slot = self.runtime.borrow_mut();
        if slot.is_none() {
            let config = load_config()?;
            *slot = Some(MenuRuntime {
                menu: MenuState::new(
                    lf_runtime::platform_menu_bar(),
                    config.field_bool("dictation_enabled").unwrap_or(true),
                ),
                lifecycle: hub_route::RuntimeLifecycle::default(),
                poller: None,
                menu_lock: None,
                #[cfg(feature = "runtime")]
                bootstrap: None,
                #[cfg(feature = "runtime")]
                activation: None,
                #[cfg(feature = "runtime")]
                embedded: None,
            });
        }
        let Some(runtime) = slot.as_mut() else {
            return Ok(());
        };
        let steps = runtime.lifecycle.request(start);
        #[cfg(feature = "runtime")]
        {
            if steps.start_server {
                if let Err(error) = Self::spawn_server(runtime) {
                    runtime.lifecycle.server_failed();
                    return Err(error);
                }
            }
            if steps.activate_dictation {
                Self::spawn_activation(runtime);
            }
        }
        if !runtime.lifecycle.full() {
            return Ok(());
        }
        if runtime.poller.is_none() {
            let mut borrowed = transport.borrow_mut();
            let http = match borrowed.as_ref() {
                Some(http) => http.clone(),
                None => {
                    let http = HttpTransport::from_config(&load_config()?)?;
                    *borrowed = Some(http.clone());
                    http
                }
            };
            drop(borrowed);
            let meeting_menu = runtime.menu.clone();
            runtime.poller = Some(MeetingStatusPoller::start(
                HttpMeetingClient(http).status_fetch(),
                Arc::new(move |result| meeting_menu.change(|model| model.apply_status(&result))),
            ));
            eprintln!("menu meeting status poller: enabled");
        }
        if runtime.menu_lock.is_none() {
            if let Some(lock) = lf_runtime::singleinstance::acquire("menubar")? {
                install_hub_menu_bar(runtime.menu.snapshot(), Self::actions(commands))
                    .map_err(|error| anyhow!(error))?;
                runtime.menu_lock = Some(lock);
                eprintln!("menu bar: enabled");
            } else if steps.install_menu {
                eprintln!("menu bar: existing owner");
            }
        }
        Ok(())
    }

    /// Start only the server, off the main thread.
    #[cfg(feature = "runtime")]
    fn spawn_server(runtime: &mut MenuRuntime) -> Result<()> {
        let config = load_config()?;
        let (send, receive) = mpsc::channel();
        let menu = runtime.menu.clone();
        std::thread::Builder::new()
            .name("localflow-startup".into())
            .spawn(move || {
                let _ = send.send(crate::embedded::EmbeddedRuntime::start_server(config, menu));
            })?;
        runtime.bootstrap = Some(receive);
        Ok(())
    }

    /// Activate dictation on the running server off the main thread, with
    /// settings loaded now (after setup saved the microphone and model).
    #[cfg(feature = "runtime")]
    fn spawn_activation(runtime: &mut MenuRuntime) {
        let Some(embedded) = runtime.embedded.take() else {
            runtime.lifecycle.dictation_finished(false);
            return;
        };
        // Handed over through a slot so a failed spawn gives the runtime back.
        let handoff = Arc::new(std::sync::Mutex::new(Some(embedded)));
        let worker = handoff.clone();
        let (send, receive) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("localflow-dictation".into())
            .spawn(move || {
                let Some(mut embedded) = worker.lock().unwrap().take() else {
                    return;
                };
                let ok = match load_config() {
                    Ok(config) => embedded.activate(config, dictation_toggle()),
                    Err(error) => {
                        log::error!("could not load settings to start dictation: {error}");
                        false
                    }
                };
                let _ = send.send((embedded, ok));
            });
        match spawned {
            Ok(_) => runtime.activation = Some(receive),
            Err(error) => {
                log::error!("could not start dictation: {error}");
                runtime.embedded = handoff.lock().unwrap().take();
                runtime.lifecycle.dictation_finished(false);
            }
        }
    }

    /// Collect finished startup/activation workers and run a deferred
    /// activation once the server is up.
    #[cfg(feature = "runtime")]
    fn drive(&self) {
        let mut slot = self.runtime.borrow_mut();
        let Some(runtime) = slot.as_mut() else {
            return;
        };
        if let Some(receiver) = runtime.bootstrap.as_ref() {
            match receiver.try_recv() {
                Ok(embedded) => {
                    runtime.bootstrap = None;
                    runtime.embedded = Some(embedded);
                    if runtime.lifecycle.server_started() {
                        Self::spawn_activation(runtime);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::error!("runtime startup worker stopped");
                    runtime.bootstrap = None;
                    runtime.lifecycle.server_failed();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = runtime.activation.as_ref() {
            match receiver.try_recv() {
                Ok((embedded, ok)) => {
                    runtime.activation = None;
                    runtime.embedded = Some(embedded);
                    runtime.lifecycle.dictation_finished(ok);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    log::error!("dictation startup worker stopped");
                    runtime.activation = None;
                    runtime.lifecycle.server_failed();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    /// Mirror the Hub's setting into the menu while no dictation runtime owns
    /// it. `enabled` is only evaluated then (it may read the config file).
    fn update(&self, enabled: impl FnOnce() -> bool) {
        #[cfg(feature = "runtime")]
        {
            self.drive();
            if self.runtime_dictation().is_some() {
                return;
            }
        }
        if let Some(runtime) = self.runtime.borrow().as_ref() {
            let enabled = enabled();
            runtime
                .menu
                .change(|model| model.dictation_enabled = enabled);
        }
    }

    /// The running dictation runtime's dictation state: the shared menu model
    /// it updates after every toggle (menu, hotkey, or a saved Hub change it
    /// picked up). `None` while no dictation runtime runs in this process —
    /// then the menu only mirrors the Hub and is not authoritative.
    fn runtime_dictation(&self) -> Option<bool> {
        #[cfg(feature = "runtime")]
        {
            let slot = self.runtime.borrow();
            let runtime = slot.as_ref()?;
            if runtime
                .embedded
                .as_ref()
                .is_some_and(|embedded| embedded.runtime.is_some())
            {
                return Some(runtime.menu.snapshot().dictation_enabled);
            }
        }
        None
    }

    #[cfg(feature = "runtime")]
    fn command(&self, command: &MenuCommand) -> bool {
        let runtime = self.runtime.borrow();
        let Some(runtime) = runtime
            .as_ref()
            .and_then(|runtime| runtime.embedded.as_ref())
            .and_then(|embedded| embedded.runtime.as_ref())
        else {
            return false;
        };
        match command {
            MenuCommand::ToggleDictation => runtime.controller.note_dictation_toggle(),
            MenuCommand::PasteLast => runtime.controller.note_paste_last(),
            MenuCommand::Quit => return false,
        }
        true
    }

    #[cfg(feature = "runtime")]
    fn shutdown(&self) {
        if let Some(runtime) = self.runtime.borrow_mut().as_mut() {
            runtime.lifecycle.stop();
            runtime.bootstrap.take();
            // An in-flight activation's runtime shuts down when its worker
            // finds the receiver gone and drops it.
            runtime.activation.take();
            if let Some(embedded) = runtime.embedded.as_mut() {
                embedded.shutdown();
            }
        }
    }
}

/// Run the Hub with authenticated runtime services until quit.
/// Own the one Hub and one setup window, including first-run completion.
pub struct WindowRouter {
    mtm: MainThreadMarker,
    hub_slot: Rc<HubSlot>,
    setup: RefCell<Option<Rc<SetupWindow>>>,
    setup_load: RefCell<Option<mpsc::Receiver<std::result::Result<SetupContext, String>>>>,
    desired: Cell<Option<&'static str>>,
    awaiting_setup: Cell<bool>,
    setup_only: Cell<bool>,
    ensure_runtime: EnsureRuntime,
    me: Weak<WindowRouter>,
}

type EnsureRuntime = Rc<dyn Fn(hub_route::RuntimeStart) -> Result<()>>;

impl WindowRouter {
    fn new(
        mtm: MainThreadMarker,
        hub_slot: Rc<HubSlot>,
        desired: Option<&'static str>,
        setup_only: bool,
        ensure_runtime: EnsureRuntime,
    ) -> Rc<Self> {
        Rc::new_cyclic(|me| Self {
            mtm,
            hub_slot,
            setup: RefCell::new(None),
            setup_load: RefCell::new(None),
            desired: Cell::new(desired),
            awaiting_setup: Cell::new(false),
            setup_only: Cell::new(setup_only),
            ensure_runtime,
            me: me.clone(),
        })
    }

    fn make_setup(&self, context: SetupContext) -> Rc<SetupWindow> {
        let platform = Arc::new(NativeSetupPlatform);
        let launch_agent = Arc::new(NativeLaunchAgent);
        let weak = self.me.clone();
        let hooks = SetupHooks {
            open_settings: None,
            launch_runtime: Some(Rc::new(move || {
                if let Some(router) = weak.upgrade() {
                    router.setup_only.set(false);
                    if let Err(error) = router.show(None) {
                        eprintln!("lf-hub: could not open the Hub after setup: {error:#}");
                    }
                }
            })),
        };
        SetupWindow::new(
            self.mtm,
            context,
            SetupServices {
                checks: Arc::new(FlowChecksRunner::new(platform.clone())),
                apply: Arc::new(FlowApplyRunner::new(
                    Arc::new(FileConfigStore::default_location()),
                    platform,
                    launch_agent,
                )),
                models: Arc::new(FlowModelBackend::default()),
                mic: Some(Arc::new(RecorderMicLevelSource::new())),
                hooks,
            },
        )
    }

    fn open_setup(&self) -> Result<()> {
        let existing = self.setup.borrow().clone();
        if let Some(setup) = existing.filter(|setup| !setup.is_closed()) {
            setup.show();
            return Ok(());
        }
        if self.setup_load.borrow().is_some() {
            return Ok(());
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("setup-detect".into())
            .spawn(move || {
                let context = (|| -> Result<SetupContext> {
                    let config = load_config()?;
                    let platform = NativeSetupPlatform;
                    let hardware = platform.detect_hardware().map_err(anyhow::Error::msg)?;
                    SetupContext::detect(config, hardware, &platform, &NativeLaunchAgent)
                        .map_err(|error| anyhow!(error.0))
                })()
                .map_err(|error| error.to_string());
                let _ = sender.send(context);
            })?;
        *self.setup_load.borrow_mut() = Some(receiver);
        Ok(())
    }

    fn attach_setup_opener(&self) {
        let Some(hub) = self.hub_slot.hub() else {
            return;
        };
        if hub.services().setup_opener.borrow().is_some() {
            return;
        }
        let weak = self.me.clone();
        *hub.services().setup_opener.borrow_mut() = Some(Rc::new(move || {
            if let Some(router) = weak.upgrade() {
                if let Err(error) = router.open_setup() {
                    eprintln!("lf-hub: could not open setup: {error:#}");
                }
            }
        }));
    }

    pub fn hub(&self) -> Option<Rc<Hub>> {
        self.hub_slot.hub()
    }

    pub fn setup_window(&self) -> Option<Rc<SetupWindow>> {
        self.setup.borrow().clone()
    }

    pub fn show(&self, page: Option<&str>) -> Result<()> {
        let setup_page = page == Some("Setup");
        if let Some(page) = page.filter(|_| !setup_page) {
            self.desired.set(Some(
                hub_route::validate_page(page)
                    .ok_or_else(|| anyhow!("unknown Hub page: {page:?}"))?,
            ));
        }
        let requested = self.desired.get();
        let plan = hub_route::plan_window(
            setup_page,
            self.setup_only.get(),
            hub_route::is_configured(),
            requested,
        )
        .map_err(anyhow::Error::msg)?;
        // The same single runtime the Hub uses: unconfigured setup gets only
        // the server; Finish (now configured) upgrades it in place.
        if let Err(error) = (self.ensure_runtime)(plan.runtime) {
            eprintln!("lf-hub: could not ensure the native menu runtime: {error:#}");
        }
        match plan.action {
            hub_route::WindowAction::OpenSetup => self.open_setup(),
            hub_route::WindowAction::SetupOnly => {
                self.awaiting_setup.set(false);
                self.open_setup()
            }
            hub_route::WindowAction::FirstRun { desired } => {
                self.desired.set(desired);
                self.awaiting_setup.set(true);
                self.open_setup()
            }
            hub_route::WindowAction::Hub { page } => {
                self.awaiting_setup.set(false);
                self.desired.set(None);
                if requested.is_some() {
                    self.hub_slot.open(Some(page))?;
                } else {
                    self.hub_slot.open(None)?;
                }
                self.attach_setup_opener();
                Ok(())
            }
        }
    }

    pub fn poll(&self) -> Result<bool> {
        let completion = self
            .setup_load
            .borrow()
            .as_ref()
            .map(|receiver| receiver.try_recv());
        match completion {
            Some(Ok(context)) => {
                self.setup_load.borrow_mut().take();
                let setup = self.make_setup(context.map_err(anyhow::Error::msg)?);
                setup.show();
                *self.setup.borrow_mut() = Some(setup);
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.setup_load.borrow_mut().take();
                return Err(anyhow!("setup detection worker stopped"));
            }
            _ => (),
        }
        if let Some(page) = hub_route::consume_request() {
            self.show(Some(page))?;
            return Ok(true);
        }
        if self.awaiting_setup.get() && hub_route::is_configured() {
            self.show(None)?;
            return Ok(true);
        }
        Ok(false)
    }
}

pub fn run_hub(initial_page: Option<&str>, setup_only: bool) -> Result<()> {
    let page = initial_page.and_then(hub_route::canonical_page);
    let request = if setup_only { Some("Setup") } else { page };
    let Some(_instance) = hub_route::acquire_main_app(request)? else {
        let (pid, _) = lf_runtime::singleinstance::read_holder("main-app");
        if let Some(owner) = pid
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(NSRunningApplication::runningApplicationWithProcessIdentifier)
        {
            owner.activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
        }
        return Ok(());
    };
    let mtm =
        MainThreadMarker::new().ok_or_else(|| anyhow!("the Hub must run on the main thread"))?;
    let transport = Rc::new(RefCell::new(None::<HttpTransport>));
    let (commands, menu_commands) = std::sync::mpsc::channel();
    let menu = Rc::new(MenuSlot::default());
    let service_transport = transport.clone();
    let slot = HubSlot::new(
        mtm,
        move || {
            let existing = service_transport.borrow().clone();
            let (services, http) = real_services_using(existing.clone())?;
            if existing.is_none() {
                *service_transport.borrow_mut() = Some(http);
            }
            Ok(services)
        },
        page.filter(|page| *page != "Setup"),
    );
    let runtime_menu = menu.clone();
    let runtime_commands = commands.clone();
    let ensure_runtime: EnsureRuntime =
        Rc::new(move |start| runtime_menu.ensure_runtime(start, &runtime_commands, &transport));
    let router = WindowRouter::new(
        mtm,
        slot,
        page.filter(|page| *page != "Setup"),
        setup_only || page == Some("Setup"),
        ensure_runtime,
    );
    let _memory_profile = super::memory_profile::start(router.clone());
    let show: ShowFn = {
        let router = router.clone();
        Rc::new(move || {
            if let Err(error) = router.show(None) {
                eprintln!("lf-hub: could not open the native window: {error:#}");
            }
        })
    };
    #[cfg(feature = "runtime")]
    let quit_menu = menu.clone();
    let poll: Box<dyn Fn() -> bool> = {
        let router = router.clone();
        let dictation_sync = RefCell::new(hub_route::DictationStatusSync::default());
        Box::new(move || {
            #[cfg(feature = "runtime")]
            menu.update(|| {
                router
                    .hub()
                    .map(|hub| {
                        hub.services()
                            .config()
                            .field_bool("dictation_enabled")
                            .unwrap_or(true)
                    })
                    .unwrap_or(true)
            });
            for command in menu_commands.try_iter() {
                if let MenuCommand::Quit = command {
                    NSApplication::sharedApplication(mtm).terminate(None);
                    continue;
                }
                #[cfg(feature = "runtime")]
                if menu.command(&command) {
                    continue;
                }
                if let Some(hub) = router.hub() {
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
            let hub = router.hub();
            if let Some(hub) = hub.as_ref() {
                menu.update(|| {
                    hub.services()
                        .dictation_config()
                        .field_bool("dictation_enabled")
                        .unwrap_or(true)
                });
            }
            sync_hub_dictation(
                &mut dictation_sync.borrow_mut(),
                menu.runtime_dictation(),
                hub.as_deref(),
            );
            match router.poll() {
                Ok(served) => served,
                Err(error) => {
                    eprintln!("lf-hub: could not serve the native window request: {error:#}");
                    false
                }
            }
        })
    };
    let before_terminate: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(hub) = router.hub() {
            hub.shutdown();
        }
        if let Some(setup) = router.setup_window() {
            setup.close();
        }
        #[cfg(feature = "runtime")]
        quit_menu.shutdown();
        if !crate::shutdown::wait_for_quit(QUIT_FLUSH_TIMEOUT, Duration::from_secs(60)) {
            log::warn!("timed out waiting for pending writes during quit");
        }
    });
    run_app_with_termination(mtm, show, Some(poll), Some(before_terminate));
    Ok(())
}
