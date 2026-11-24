//! Window, shell (sidebar + page host), toasts, confirm dialogs and the
//! global keyboard map.

use std::rc::Rc;
use std::time::{Duration, SystemTime};

use gpui::{
    actions, div, prelude::*, px, size, AnyElement, App, Application, Bounds, Context, Entity,
    FocusHandle, Focusable, FontWeight, KeyBinding, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};

use crate::controllers::engine::Engine;
use crate::controllers::instance::{Instance, Route};
use crate::controllers::runtime_state::{self, OwnEngine, RuntimeView, Signal};
use crate::data::Services;
use crate::page::{Page, NAV_FOOTER, NAV_GROUPS};
use crate::pages;
use crate::remote::Health;
use crate::theme::{elevation, size as tsize, space, text, theme, Theme, Tone};
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use crate::tray;
use crate::ui::{self, assets::Assets, Button};

/// Theme tone of a runtime signal.
pub fn signal_tone(signal: Signal) -> Tone {
    match signal {
        Signal::Success => Tone::Success,
        Signal::Info => Tone::Info,
        Signal::Warning => Tone::Warning,
        Signal::Danger | Signal::Recording => Tone::Error,
        Signal::Neutral => Tone::Neutral,
    }
}

actions!(
    localflow,
    [
        GoHome,
        Go1,
        Go2,
        Go3,
        Go4,
        Go5,
        Go6,
        Go7,
        Go8,
        Go9,
        Go10,
        FocusSearch,
        NewItem,
        OpenSettings,
        Quit,
        CloseWindow,
        FocusNext,
        FocusPrev,
        Escape,
        NavUp,
        NavDown,
    ]
);

/// What `main` hands to the app.
pub struct Launch {
    pub services: Services,
    pub page: Page,
    pub setup: bool,
    pub notice: Option<String>,
    /// The window lock for this data root, held for the whole process.
    pub instance: Instance,
}

pub struct Toast {
    pub message: SharedString,
    pub action: Option<(SharedString, ui::Action)>,
    generation: u64,
}

pub struct Confirm {
    pub title: SharedString,
    pub body: SharedString,
    pub verb: SharedString,
    pub destructive: bool,
    pub on_confirm: ui::Action,
}

/// Shared by every page: services plus a handle to the shell.
///
/// Every call that changes the shell is deferred to the end of the current
/// effect cycle, so pages may call these from any callback (including ones the
/// shell itself runs, such as confirm dialogs and toast actions) without
/// re-entering an entity that is already being updated.
#[derive(Clone)]
pub struct PageCx {
    pub services: Rc<Services>,
    pub shell: gpui::WeakEntity<Shell>,
    /// Last known local-service health (written by the shell).
    pub status: Rc<std::cell::RefCell<Health>>,
    /// The page on screen (written by the shell); lets pages check this
    /// without reading the shell entity, which may be mid-update.
    pub current_page: Rc<std::cell::RefCell<Page>>,
    /// Latest runtime view (written by the shell's probe).
    pub runtime: Rc<std::cell::RefCell<RuntimeView>>,
    /// This window's engine state (written by the shell).
    pub engine: Rc<std::cell::RefCell<Option<OwnEngine>>>,
    /// A setup draft is waiting (Home shows Resume setup).
    pub setup_draft: Rc<std::cell::Cell<bool>>,
}

impl PageCx {
    fn with_shell(&self, cx: &mut App, f: impl FnOnce(&mut Shell, &mut Context<Shell>) + 'static) {
        let shell = self.shell.clone();
        cx.defer(move |cx| {
            let _ = shell.update(cx, f);
        });
    }
    pub fn navigate(&self, page: Page, target: Option<&'static str>, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| shell.navigate(page, target, cx));
    }
    pub fn open_history(&self, id: i64, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| shell.open_history_entry(id, cx));
    }
    pub fn toast(&self, message: impl Into<SharedString>, cx: &mut App) {
        let message = message.into();
        self.with_shell(cx, move |shell, cx| shell.toast(message, None, cx));
    }
    pub fn toast_with(
        &self,
        message: impl Into<SharedString>,
        action: impl Into<SharedString>,
        f: impl Fn(&mut Window, &mut App) + 'static,
        cx: &mut App,
    ) {
        let message = message.into();
        let action = Some((action.into(), Rc::new(f) as ui::Action));
        self.with_shell(cx, move |shell, cx| shell.toast(message, action, cx));
    }
    pub fn confirm(&self, confirm: Confirm, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| {
            shell.confirm = Some(confirm);
            shell.dialog_armed = false;
            cx.notify();
        });
    }
    /// Config changed: repaint the shell (status row, badges).
    pub fn config_changed(&self, cx: &mut App) {
        self.with_shell(cx, |_, cx| cx.notify());
    }
    pub fn health(&self, _: &App) -> Health {
        self.status.borrow().clone()
    }
    pub fn recheck_health(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| shell.check_health(cx));
    }
    /// Open (or resume) the setup wizard overlay.
    pub fn open_setup(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| shell.show_setup(cx));
    }
    /// Hide the overlay; the draft stays for the session.
    pub fn close_setup(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| shell.hide_setup(cx));
    }
    /// Apply finished: drop the draft and show Home.
    pub fn setup_applied(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| {
            shell.setup_view = None;
            shell.hide_setup(cx);
            shell.navigate(Page::Home, None, cx);
            shell.probe_runtime(cx);
        });
    }
    pub fn runtime(&self) -> RuntimeView {
        self.runtime.borrow().clone()
    }
    /// Why trial/download/delete must wait (dictation mid-gesture).
    pub fn runtime_busy(&self) -> Option<&'static str> {
        self.runtime.borrow().busy_reason()
    }
    pub fn engine_state(&self) -> Option<OwnEngine> {
        self.engine.borrow().clone()
    }
    pub fn engine_start(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| {
            if let Some(engine) = &shell.engine {
                engine.start();
            }
            shell.probe_runtime(cx);
        });
    }
    pub fn engine_stop(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| {
            if let Some(engine) = &shell.engine {
                engine.stop();
            }
            shell.probe_runtime(cx);
        });
    }
    /// The sandbox's simulated runtime state changed (Settings → Advanced).
    pub fn reprobe(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| shell.probe_runtime(cx));
    }
    pub fn set_pending_prompts(&self, n: usize, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| {
            if shell.pending_prompts != n {
                shell.pending_prompts = n;
                cx.notify();
            }
        });
    }
}

pub enum View {
    Home(Entity<pages::home::HomePage>),
    History(Entity<pages::history::HistoryPage>),
    Dictionary(Entity<pages::dictionary::DictionaryPage>),
    Snippets(Entity<pages::snippets::SnippetsPage>),
    Style(Entity<pages::style::StylePage>),
    Ask(Entity<pages::ask::AskPage>),
    Meetings(Entity<pages::meetings::MeetingsPage>),
    Prompts(Entity<pages::prompts::PromptsPage>),
    Models(Entity<pages::models::ModelsPage>),
    Settings(Entity<pages::settings::SettingsPage>),
}

pub struct Shell {
    pub cx: PageCx,
    pub page: Page,
    views: Vec<(Page, View)>,
    pub health: Health,
    pub pending_prompts: usize,
    toast: Option<Toast>,
    toast_gen: u64,
    pub confirm: Option<Confirm>,
    focus: FocusHandle,
    nav_focus: FocusHandle,
    /// Dialog buttons (focus is moved and trapped between them).
    cancel_focus: FocusHandle,
    ok_focus: FocusHandle,
    /// Where focus was before the dialog opened; restored on close.
    restore_focus: Option<FocusHandle>,
    /// The dialog's initial focus has been scheduled.
    dialog_armed: bool,
    /// The user chose to close or quit despite unsaved changes.
    force_close: bool,
    config_mtime: Option<SystemTime>,
    setup: bool,
    instance: Instance,
    /// The wizard (kept while hidden: the draft survives for the session).
    pub setup_view: Option<Entity<pages::setup::SetupView>>,
    setup_visible: bool,
    /// A routed page that arrived behind the overlay or a dialog.
    queued: Option<Route>,
    pub runtime: RuntimeView,
    probing: bool,
    pub engine: Option<Engine>,
    /// A background stop (close/quit) is running.
    stopping: bool,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    tray: Option<(tray::Tray, tray::View)>,
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    tray_task: Option<gpui::Task<()>>,
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Config and machine facts for the runtime view (no probing here).
pub fn runtime_context(services: &Services) -> runtime_state::Context {
    let config = services.config();
    runtime_state::Context {
        enabled: config.field_bool("dictation_enabled").unwrap_or(true),
        key: lf_ui::hub_model::format_hotkey(config.field_str("hotkey")),
        session: if services.is_sandbox() {
            runtime_state::Session::X11
        } else {
            runtime_state::Session::detect()
        },
        configured_mic: config.field_str("input_device").unwrap_or("").to_string(),
        missing_paste_tool: None,
    }
}

/// Before the first probe: "not checked yet" (never ready).
fn initial_runtime(services: &Services) -> RuntimeView {
    let context = runtime_context(services);
    if services.is_sandbox() {
        return runtime_state::simulated(services.simulated_runtime.get(), &context);
    }
    let mut view = runtime_state::derive(
        &runtime_state::Observation {
            file: runtime_state::FileObs::Absent,
            pid_alive: false,
            lock: lf_runtime::singleinstance::LockProbe::Unknown,
            lock_identity: None,
            own_instance: None,
            now: 0.0,
            own: OwnEngine::None,
        },
        &context,
    );
    view.lead = "Checking LocalFlow…".into();
    view.support = String::new();
    view.sidebar = (Signal::Neutral, "Checking…".into(), String::new());
    view
}

/// The engine exists only for `--features runtime` + `--engine` outside the
/// sandbox; it never starts by itself.
fn spawn_engine(services: &Services) -> Option<Engine> {
    if !services.caps.engine_allowed() {
        return None;
    }
    #[cfg(feature = "runtime")]
    {
        match crate::controllers::engine::EmbeddedBackend::new(&services.root)
            .map_err(std::io::Error::other)
            .and_then(Engine::spawn)
        {
            Ok(engine) => Some(engine),
            Err(error) => {
                eprintln!("lf-desktop: dictation engine unavailable: {error}");
                None
            }
        }
    }
    #[cfg(not(feature = "runtime"))]
    {
        None
    }
}

impl Shell {
    fn new(launch: Launch, cx: &mut Context<Self>) -> Self {
        let services = Rc::new(launch.services);
        let initial = if services.is_sandbox() {
            Health::Simulated
        } else {
            Health::Unknown
        };
        let runtime = initial_runtime(&services);
        let engine = spawn_engine(&services);
        let page_cx = PageCx {
            services: services.clone(),
            shell: cx.weak_entity(),
            status: Rc::new(std::cell::RefCell::new(initial.clone())),
            current_page: Rc::new(std::cell::RefCell::new(launch.page)),
            runtime: Rc::new(std::cell::RefCell::new(runtime.clone())),
            engine: Rc::new(std::cell::RefCell::new(
                engine.as_ref().map(|e: &Engine| e.state()),
            )),
            setup_draft: Rc::new(std::cell::Cell::new(false)),
        };
        let config_mtime = mtime(&services.root.config_path);
        let mut shell = Shell {
            cx: page_cx,
            page: launch.page,
            views: Vec::new(),
            health: if services.is_sandbox() {
                Health::Simulated
            } else {
                Health::Unknown
            },
            pending_prompts: 0,
            toast: None,
            toast_gen: 0,
            confirm: None,
            focus: cx.focus_handle(),
            nav_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle().tab_stop(true),
            ok_focus: cx.focus_handle().tab_stop(true),
            restore_focus: None,
            dialog_armed: false,
            force_close: false,
            config_mtime,
            setup: launch.setup,
            instance: launch.instance,
            setup_view: None,
            setup_visible: false,
            queued: None,
            runtime,
            probing: false,
            engine,
            stopping: false,
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            tray: None,
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            tray_task: None,
        };
        if let Some(notice) = launch.notice {
            shell.toast(notice.into(), None, cx);
        }
        if launch.setup {
            shell.show_setup(cx);
        }
        shell.check_health(cx);
        shell.probe_runtime(cx);
        shell.start_timers(cx);
        shell
    }

    // ------------------------------------------------------------ setup ---

    pub fn show_setup(&mut self, cx: &mut Context<Self>) {
        if self.stopping {
            return;
        }
        match &self.setup_view {
            // Resumed: refresh the read-only facts (choices are kept).
            Some(view) if !self.setup_visible => view.update(cx, |v, cx| v.resume(cx)),
            Some(_) => {}
            None => {
                let pcx = self.cx.clone();
                self.setup_view = Some(cx.new(|cx| pages::setup::SetupView::new(pcx, cx)));
            }
        }
        self.setup_visible = true;
        cx.notify();
    }

    pub fn hide_setup(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.setup_view {
            view.update(cx, |v, cx| v.release_audio(cx));
        }
        self.setup_visible = false;
        self.cx.setup_draft.set(self.setup_dirty(cx));
        self.refresh_views(cx);
        self.apply_queued(cx);
        cx.notify();
    }

    fn setup_dirty(&self, cx: &App) -> bool {
        self.setup_view.as_ref().is_some_and(|v| v.read(cx).dirty())
    }

    // ---------------------------------------------------------- routing ---

    /// Requests from a second launch: raise, then go there (queued while the
    /// overlay or a dialog is open).
    fn poll_instance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stopping {
            // Leave the request in place (TTL-bounded): it is routed if the
            // close fails, and no setup or page opens meanwhile.
            return;
        }
        let Some(route) = self.instance.take_request() else {
            return;
        };
        window.activate_window();
        match route {
            Route::Setup if self.confirm.is_none() => self.show_setup(cx),
            route if self.setup_visible || self.confirm.is_some() => {
                self.queued = Some(route);
            }
            Route::Page(page) => self.navigate(page, None, cx),
            Route::Setup => self.queued = Some(route),
        }
    }

    fn apply_queued(&mut self, cx: &mut Context<Self>) {
        if self.setup_visible || self.confirm.is_some() {
            return;
        }
        match self.queued.take() {
            Some(Route::Page(page)) => {
                self.navigate(page, None, cx);
                self.toast(format!("Opened {}", page.label()).into(), None, cx);
            }
            Some(Route::Setup) => self.show_setup(cx),
            None => {}
        }
    }

    // ---------------------------------------------------------- runtime ---

    pub fn probe_runtime(&mut self, cx: &mut Context<Self>) {
        let services = self.cx.services.clone();
        let own = self.engine.as_ref().map(Engine::state);
        *self.cx.engine.borrow_mut() = own.clone();
        let context = runtime_context(&services);
        if services.is_sandbox() {
            let view = runtime_state::simulated(services.simulated_runtime.get(), &context);
            self.set_runtime(view, cx);
            return;
        }
        if self.probing {
            return;
        }
        self.probing = true;
        let root = services.root.clone();
        let own = own.unwrap_or(OwnEngine::None);
        let own_instance = self.engine.as_ref().and_then(Engine::instance);
        cx.spawn(async move |this, cx| {
            let view = cx
                .background_executor()
                .spawn(async move {
                    let mut context = context;
                    context.missing_paste_tool =
                        runtime_state::missing_paste_tool(context.session, &|tool| {
                            lf_ui::setup_flow::SetupPlatform::which(
                                &lf_ui::setup_flow::HostPlatform,
                                tool,
                            )
                        });
                    runtime_state::derive(
                        &runtime_state::observe(&root, own, own_instance),
                        &context,
                    )
                })
                .await;
            let _ = this.update(cx, |shell, cx| {
                shell.probing = false;
                shell.set_runtime(view, cx);
            });
        })
        .detach();
    }

    fn set_runtime(&mut self, view: RuntimeView, cx: &mut Context<Self>) {
        if self.runtime != view {
            *self.cx.runtime.borrow_mut() = view.clone();
            self.runtime = view;
            if let Some(View::Home(v)) = self.find(Page::Home) {
                v.update(cx, |_, cx| cx.notify());
            }
            cx.notify();
        }
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        self.update_tray();
    }

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    fn tray_view(&self) -> tray::View {
        tray::View::new(
            &self.runtime,
            lf_runtime::settings::dictation_enabled(&self.cx.services.config()),
            self.engine.as_ref().map(Engine::state),
        )
    }

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    fn update_tray(&mut self) {
        if self.tray.is_none() {
            return;
        }
        let view = self.tray_view();
        if let Some((icon, previous)) = &mut self.tray {
            if *previous != view {
                icon.update(view.clone());
                *previous = view;
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    fn start_tray(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Sandbox automation starts nothing in the macOS menu bar.
        #[cfg(target_os = "macos")]
        if self.cx.services.is_sandbox() {
            return;
        }
        let view = self.tray_view();
        let initial = view.clone();
        let (actions, events) = async_channel::unbounded();
        self.tray_task = Some(cx.spawn_in(window, async move |this, cx| {
            #[cfg(target_os = "linux")]
            let result = cx
                .background_executor()
                .spawn(async move { tray::Tray::new(initial, actions) })
                .await;
            // AppKit and Win32 objects live on GPUI's main thread.
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            let result = tray::Tray::new(initial, actions);
            let icon = match result {
                Ok(icon) => icon,
                Err(error) => {
                    log::warn!("LocalFlow tray unavailable: {error:#}");
                    return;
                }
            };
            if this
                .update_in(cx, |shell, _, _| {
                    shell.tray = Some((icon, view));
                    shell.update_tray();
                })
                .is_err()
            {
                return;
            }
            while let Ok(action) = events.recv().await {
                if this
                    .update_in(cx, |shell, window, cx| {
                        shell.tray_action(action, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    fn tray_action(&mut self, action: tray::Action, window: &mut Window, cx: &mut Context<Self>) {
        use crate::controllers::engine::MenuCommand;

        // Every receipt consumes the pending macOS paste request, including
        // ones refused below, so the menu can always take a new one.
        #[cfg(target_os = "macos")]
        let paste_target_ok = action == tray::Action::PasteLast
            && self
                .tray
                .as_ref()
                .is_some_and(|(icon, _)| icon.restore_paste_target());
        if self.stopping {
            return;
        }
        // On macOS a menu bar item doesn't activate LocalFlow, and GPUI's
        // `activate_window` only orders the window front within the app, so
        // anything that should surface the window activates the app too.
        match action {
            tray::Action::Open => {
                #[cfg(target_os = "macos")]
                cx.activate(true);
                window.activate_window();
            }
            tray::Action::Quit => {
                #[cfg(target_os = "macos")]
                cx.activate(true);
                window.activate_window();
                self.request_close(true, window, cx);
            }
            tray::Action::ToggleDictation => match self.engine.as_ref().map(Engine::state) {
                Some(OwnEngine::Running) => {
                    if let Some(engine) = &self.engine {
                        engine.menu_command(MenuCommand::ToggleDictation);
                    }
                }
                Some(OwnEngine::Starting | OwnEngine::Stopping) => {}
                _ => {
                    if let Err(error) = self.cx.services.update(|current| {
                        let mut updated = current.clone();
                        let enabled = lf_runtime::settings::dictation_enabled(current);
                        updated.set_field("dictation_enabled", serde_json::json!(!enabled));
                        Ok(updated)
                    }) {
                        #[cfg(target_os = "macos")]
                        cx.activate(true);
                        window.activate_window();
                        self.toast(
                            format!("Couldn't switch dictation: {error}").into(),
                            None,
                            cx,
                        );
                    }
                    self.refresh_views(cx);
                    self.update_tray();
                    self.probe_runtime(cx);
                }
            },
            tray::Action::PasteLast => {
                if let Some(engine) = self
                    .engine
                    .as_ref()
                    .filter(|e| e.state() == OwnEngine::Running)
                {
                    #[cfg(target_os = "windows")]
                    if !self
                        .tray
                        .as_ref()
                        .is_some_and(|(icon, _)| icon.restore_paste_target())
                    {
                        self.toast(
                            "Couldn't restore the window to paste into.".into(),
                            None,
                            cx,
                        );
                        return;
                    }
                    // Never activates anything: on refusal the user re-picks
                    // their app, and the clipboard is untouched.
                    #[cfg(target_os = "macos")]
                    if !paste_target_ok {
                        self.toast(
                            "Switch to the app you want to paste into, then choose \
                             Paste last transcript again."
                                .into(),
                            None,
                            cx,
                        );
                        return;
                    }
                    engine.menu_command(MenuCommand::PasteLast);
                }
            }
        }
    }

    fn start_timers(&mut self, cx: &mut Context<Self>) {
        // Service health every 5 s; config file changes every 1.5 s.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let Ok(()) = this.update(cx, |shell, cx| shell.watch_config(cx)) else {
                break;
            };
        })
        .detach();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let Ok(()) = this.update(cx, |shell, cx| shell.check_health(cx)) else {
                break;
            };
        })
        .detach();
        // Runtime signals every second (status file, lock holder, engine).
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let Ok(()) = this.update(cx, |shell, cx| shell.probe_runtime(cx)) else {
                break;
            };
        })
        .detach();
    }

    /// Page requests from other launches, every 250 ms (needs the window).
    fn start_instance_poll(window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let Ok(()) = this.update_in(cx, |shell, window, cx| shell.poll_instance(window, cx))
            else {
                break;
            };
        })
        .detach();
    }

    /// When the focused element leaves the frame (a cached page that was
    /// navigated away from, or a button replaced by another), GPUI dispatches
    /// keys from the window root, above the shell's action listeners, so
    /// shortcuts stop working until something is clicked. Move focus back to
    /// the shell, or to Cancel while a dialog is open. GPUI calls this in the
    /// draw that lost focus, after the new frame is in place; it never runs
    /// while focus is on a rendered element.
    fn install_focus_recovery(window: &mut Window, cx: &mut Context<Self>) {
        cx.on_focus_lost(window, |shell, window, cx| {
            let target = if shell.confirm.is_some() && shell.dialog_armed {
                &shell.cancel_focus
            } else {
                &shell.focus
            };
            // Now, so a key handled in this same dispatch reaches the shell.
            window.focus(target);
            // A refresh inside the draw is ignored: repaint once it is over,
            // so the frame shows the target as focused (Cancel's ring) and a
            // later loss is seen as one again.
            cx.defer_in(window, |_, window, _| window.refresh());
        })
        .detach();
    }

    fn watch_config(&mut self, cx: &mut Context<Self>) {
        let now = mtime(&self.cx.services.root.config_path);
        if now != self.config_mtime {
            self.config_mtime = now;
            self.cx.services.reload_config();
            self.refresh_views(cx);
            cx.notify();
        }
    }

    /// Re-read config-derived state on every created page.
    pub fn refresh_views(&mut self, cx: &mut Context<Self>) {
        for (_, view) in &self.views {
            match view {
                View::Home(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::History(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Dictionary(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Snippets(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Style(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Ask(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Meetings(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Prompts(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Models(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Settings(v) => v.update(cx, |p, cx| p.refresh(cx)),
            }
        }
    }

    pub fn check_health(&mut self, cx: &mut Context<Self>) {
        let remote = self.cx.services.remote.clone();
        let config = self.cx.services.config();
        cx.spawn(async move |this, cx| {
            let health = cx
                .background_executor()
                .spawn(async move { remote.health(&config) })
                .await;
            let _ = this.update(cx, |shell, cx| {
                if shell.health != health {
                    *shell.cx.status.borrow_mut() = health.clone();
                    shell.health = health;
                    shell.refresh_views(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn navigate(&mut self, page: Page, target: Option<&'static str>, cx: &mut Context<Self>) {
        if self.stopping {
            return; // quiesced while closing
        }
        if self.confirm.is_some() || self.setup_visible {
            // A dialog or the setup overlay is open: no navigation behind it
            // (routed requests are queued and applied when it closes).
            if self.setup_visible && self.confirm.is_none() {
                self.queued = Some(Route::Page(page));
            }
            return;
        }
        let leaving = self.page;
        if leaving == Page::Snippets && page != Page::Snippets {
            if let Some(View::Snippets(v)) = self.find(Page::Snippets) {
                let ok = v.update(cx, |p, cx| p.leave(cx));
                if !ok {
                    self.toast(
                        "Fix the snippet before leaving; it couldn't be saved.".into(),
                        None,
                        cx,
                    );
                    return;
                }
            }
        }
        if leaving == Page::Models && page != Page::Models {
            if let Some(View::Models(v)) = self.find(Page::Models) {
                v.update(cx, |p, cx| p.clear_secret(cx));
            }
        }
        self.page = page;
        *self.cx.current_page.borrow_mut() = page;
        self.ensure_view(page, cx);
        if let Some(target) = target {
            self.deep_link(page, target, cx);
        }
        if let Some(view) = self.find(page) {
            match view {
                View::Home(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::History(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Dictionary(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Snippets(v) => v.update(cx, |p, cx| p.show(cx)),
                View::Style(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Ask(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Meetings(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Prompts(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Models(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Settings(v) => v.update(cx, |p, cx| p.refresh(cx)),
            }
        }
        cx.notify();
    }

    fn deep_link(&mut self, page: Page, target: &str, cx: &mut Context<Self>) {
        match self.find(page) {
            Some(View::Settings(v)) => v.update(cx, |p, cx| p.open_tab(target, cx)),
            Some(View::Models(v)) => v.update(cx, |p, cx| p.open_tab(target, cx)),
            Some(View::History(v)) => {
                if let Ok(id) = target.parse::<i64>() {
                    v.update(cx, |p, cx| p.select_id(id, cx));
                }
            }
            _ => {}
        }
    }

    /// Open History on a specific entry.
    pub fn open_history_entry(&mut self, id: i64, cx: &mut Context<Self>) {
        self.navigate(Page::History, None, cx);
        if let Some(View::History(v)) = self.find(Page::History) {
            v.update(cx, |p, cx| p.select_id(id, cx));
        }
    }

    fn find(&self, page: Page) -> Option<&View> {
        self.views.iter().find(|(p, _)| *p == page).map(|(_, v)| v)
    }

    fn ensure_view(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.find(page).is_some() {
            return;
        }
        let pcx = self.cx.clone();
        let setup = self.setup;
        let view = match page {
            Page::Home => View::Home(cx.new(|cx| pages::home::HomePage::new(pcx, setup, cx))),
            Page::History => View::History(cx.new(|cx| pages::history::HistoryPage::new(pcx, cx))),
            Page::Dictionary => {
                View::Dictionary(cx.new(|cx| pages::dictionary::DictionaryPage::new(pcx, cx)))
            }
            Page::Snippets => {
                View::Snippets(cx.new(|cx| pages::snippets::SnippetsPage::new(pcx, cx)))
            }
            Page::Style => View::Style(cx.new(|cx| pages::style::StylePage::new(pcx, cx))),
            Page::Ask => View::Ask(cx.new(|cx| pages::ask::AskPage::new(pcx, cx))),
            Page::Meetings => {
                View::Meetings(cx.new(|cx| pages::meetings::MeetingsPage::new(pcx, cx)))
            }
            Page::Prompts => View::Prompts(cx.new(|cx| pages::prompts::PromptsPage::new(pcx, cx))),
            Page::Models => View::Models(cx.new(|cx| pages::models::ModelsPage::new(pcx, cx))),
            Page::Settings => {
                View::Settings(cx.new(|cx| pages::settings::SettingsPage::new(pcx, cx)))
            }
        };
        self.views.push((page, view));
    }

    pub fn toast(
        &mut self,
        message: SharedString,
        action: Option<(SharedString, ui::Action)>,
        cx: &mut Context<Self>,
    ) {
        self.toast_gen += 1;
        let generation = self.toast_gen;
        self.toast = Some(Toast {
            message,
            action,
            generation,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(6)).await;
            let _ = this.update(cx, |shell, cx| {
                if shell
                    .toast
                    .as_ref()
                    .is_some_and(|t| t.generation == generation)
                {
                    shell.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn go_index(&mut self, index: usize, cx: &mut Context<Self>) {
        // Pages behind a dialog or the setup overlay take no shortcuts.
        if self.confirm.is_some() || self.setup_visible || self.stopping {
            return;
        }
        if let Some(page) = Page::sidebar_order().get(index).copied() {
            self.navigate(page, None, cx);
        }
    }

    fn page_element(&self) -> AnyElement {
        match self.find(self.page) {
            Some(View::Home(v)) => v.clone().into_any_element(),
            Some(View::History(v)) => v.clone().into_any_element(),
            Some(View::Dictionary(v)) => v.clone().into_any_element(),
            Some(View::Snippets(v)) => v.clone().into_any_element(),
            Some(View::Style(v)) => v.clone().into_any_element(),
            Some(View::Ask(v)) => v.clone().into_any_element(),
            Some(View::Meetings(v)) => v.clone().into_any_element(),
            Some(View::Prompts(v)) => v.clone().into_any_element(),
            Some(View::Models(v)) => v.clone().into_any_element(),
            Some(View::Settings(v)) => v.clone().into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Pages behind a dialog or the setup overlay take no shortcuts.
        if self.confirm.is_some() || self.setup_visible || self.stopping {
            return;
        }
        match self.find(self.page) {
            Some(View::History(v)) => v.update(cx, |p, cx| p.focus_search(window, cx)),
            Some(View::Snippets(v)) => v.update(cx, |p, cx| p.focus_search(window, cx)),
            Some(View::Dictionary(v)) => v.update(cx, |p, cx| p.focus_search(window, cx)),
            _ => {}
        }
    }

    fn new_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Pages behind a dialog or the setup overlay take no shortcuts.
        if self.confirm.is_some() || self.setup_visible || self.stopping {
            return;
        }
        match self.find(self.page) {
            Some(View::Snippets(v)) => v.update(cx, |p, cx| p.new_snippet(window, cx)),
            Some(View::Dictionary(v)) => v.update(cx, |p, cx| p.focus_new_word(window, cx)),
            Some(View::Ask(v)) => v.update(cx, |p, cx| p.new_question(window, cx)),
            _ => {}
        }
    }

    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stopping {
            return;
        }
        if self.confirm.is_some() {
            // Cancel: the action is never run.
            self.close_dialog(false, window, cx);
            return;
        }
        if self.toast.take().is_some() {
            cx.notify();
            return;
        }
        if self.setup_visible {
            if let Some(view) = self.setup_view.clone() {
                view.update(cx, |v, cx| v.close(cx));
            }
            return;
        }
        match self.find(self.page) {
            Some(View::History(v)) => v.update(cx, |p, cx| p.clear_search(window, cx)),
            _ => cx.propagate(),
        }
    }

    fn nav_step(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        // Pages behind a dialog or the setup overlay take no shortcuts.
        if self.confirm.is_some() || self.setup_visible || self.stopping {
            return;
        }
        if !self.nav_focus.contains_focused(window, cx) {
            cx.propagate();
            return;
        }
        let order = Page::sidebar_order();
        let i = order.iter().position(|p| *p == self.page).unwrap_or(0) as i32;
        let n = order.len() as i32;
        let next = order[((i + delta).rem_euclid(n)) as usize];
        self.navigate(next, None, cx);
    }

    // ---------------------------------------------------------------- views ---

    fn render_sidebar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let nav_item = |page: Page, cx: &mut Context<Self>| {
            let selected = self.page == page;
            let (sz, lh) = text::body();
            let badge = (page == Page::Prompts && self.pending_prompts > 0)
                .then(|| ui::badge(t, self.pending_prompts));
            // Selected: a soft rounded fill, heavier label, accent icon.
            ui::row_button(t, ("nav", page as u64), selected)
                .h(tsize::nav_row())
                .items_center()
                .gap(space::s4())
                .px(space::s4())
                .rounded(tsize::radius_inner())
                .text_size(sz)
                .line_height(lh)
                .text_color(t.text)
                .when(selected, |d| {
                    d.bg(t.bg_sidebar_selected)
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .on_click(cx.listener(move |this, _, _, cx| this.navigate(page, None, cx)))
                .child(ui::icon(
                    page.icon(),
                    if selected {
                        t.icon_accent
                    } else {
                        t.icon_secondary
                    },
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(page.label()),
                )
                .children(badge)
        };
        let mut groups = div()
            .flex()
            .flex_col()
            .gap(space::s1())
            .px(space::s4())
            .pb(space::s4());
        for (label, pages) in NAV_GROUPS {
            if let Some(label) = label {
                groups = groups.child(
                    ui::supporting(t, label)
                        .font_weight(FontWeight::SEMIBOLD)
                        .px(space::s4())
                        .pt(space::s6())
                        .pb(space::s2()),
                );
            }
            for page in pages {
                groups = groups.child(nav_item(*page, cx));
            }
        }
        let mut footer = div()
            .flex()
            .flex_col()
            .gap(space::s1())
            .px(space::s4())
            .py(space::s4());
        for page in NAV_FOOTER {
            footer = footer.child(nav_item(page, cx));
        }
        // Status row: what a runtime itself reports (status file, lock,
        // this window's engine). HTTP reachability never changes it. A
        // quiet footer: dot, line 1, line 2; clicking opens Home.
        let (signal, line1, line2) = self.runtime.sidebar.clone();
        let tone = signal_tone(signal);
        let (sz, lh) = text::supporting();
        let status = div().px(space::s4()).py(space::s4()).child(
            ui::row_button(t, "status-row", false)
                .flex_col()
                .gap(space::s1())
                .px(space::s4())
                .py(space::s3())
                .rounded(tsize::radius_element())
                .on_click(cx.listener(|this, _, _, cx| this.navigate(Page::Home, None, cx)))
                .child(
                    ui::status_dot(t, tone, line1)
                        .text_size(sz)
                        .line_height(lh)
                        .font_weight(FontWeight::MEDIUM),
                )
                .when(!line2.is_empty(), |d| {
                    d.child(ui::supporting(t, line2).pl(px(14.)))
                }),
        );

        div()
            .id("sidebar")
            .track_focus(&self.nav_focus)
            .flex()
            .flex_col()
            .flex_none()
            .w(px(tsize::SIDEBAR))
            .h_full()
            .bg(t.bg_body)
            .border_r_1()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    .gap(space::s4())
                    .h(px(52.))
                    .px(space::s6())
                    .child(ui::brand(t))
                    .child(ui::label(t, "LocalFlow").font_weight(FontWeight::SEMIBOLD)),
            )
            .child(
                div()
                    .id("nav-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(groups),
            )
            .child(ui::divider(t))
            .child(footer)
            .child(ui::divider(t))
            .child(status)
    }

    fn render_toast(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let toast = self.toast.as_ref()?;
        let (sz, lh) = text::body();
        let action = toast.action.clone();
        Some(
            div()
                .absolute()
                .bottom(space::s8())
                .left(px(0.))
                .right(px(0.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("toast")
                        .occlude()
                        .flex()
                        .items_center()
                        .gap(space::s5())
                        .pl(space::s6())
                        .pr(space::s2())
                        .py(space::s2())
                        .min_h(px(40.))
                        .rounded(tsize::radius_group())
                        .border_1()
                        .border_color(t.border)
                        .bg(t.bg_popover)
                        .text_color(t.text)
                        .text_size(sz)
                        .line_height(lh)
                        .shadow(elevation::popover(t))
                        .max_w(px(560.))
                        .child(toast.message.clone())
                        .when_some(action, |d, (label, f)| {
                            let ring = ui::ring_over(t, t.bg_popover);
                            d.child(
                                div()
                                    .id("toast-action")
                                    .tab_index(0)
                                    .cursor_pointer()
                                    .px(space::s2())
                                    .rounded(tsize::radius_inner())
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(t.text_accent)
                                    .focus(move |s| s.shadow(ring.clone()))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.toast = None;
                                        f(window, cx);
                                        cx.notify();
                                    }))
                                    .child(label),
                            )
                        })
                        .child(ui::IconButton::new("toast-close", "x", "Dismiss").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.toast = None;
                                cx.notify();
                            }),
                        )),
                ),
        )
    }

    /// Flush every page's dirty state and wait (bounded) for the writes.
    pub fn prepare_close(
        &mut self,
        cx: &mut Context<Self>,
    ) -> crate::controllers::shutdown::CloseCheck {
        use crate::controllers::shutdown;
        let mut failures = Vec::new();
        let threads = shutdown::failures();
        for (_, view) in &self.views {
            match view {
                View::Snippets(v) => {
                    if let Err(e) = v.update(cx, |p, cx| p.flush_for_close(cx)) {
                        failures.push(e);
                    }
                }
                View::Meetings(v) => {
                    let threads = threads.clone();
                    if let Err(e) = v.update(cx, |p, cx| p.flush_for_close(threads, cx)) {
                        failures.push(e);
                    }
                }
                _ => {}
            }
        }
        shutdown::finish(failures, &threads, shutdown::CLOSE_WAIT)
    }

    /// `true` when the window may close now. Otherwise a dialog explains what
    /// wasn't saved and offers to close anyway.
    pub fn should_close(
        &mut self,
        quit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.force_close {
            return true;
        }
        if self.stopping {
            return false; // a background stop is running; it closes when done
        }
        let check = self.prepare_close(cx);
        let pending = self.pending_work(cx);
        if check.ok() && pending.is_empty() {
            self.force_close = true;
            self.release_setup_audio(cx);
            return true;
        }
        let shell = cx.weak_entity();
        let (title, body) = if check.ok() {
            (
                if quit {
                    "Quit LocalFlow?"
                } else {
                    "Close LocalFlow?"
                },
                pending.join(" "),
            )
        } else {
            (
                "Some changes weren't saved",
                format!(
                    "{} {} Keep the window open to fix this, or close anyway and lose them.",
                    check.message(),
                    pending.join(" ")
                ),
            )
        };
        self.confirm = Some(Confirm {
            title: title.into(),
            body: body.into(),
            verb: if quit { "Quit anyway" } else { "Close anyway" }.into(),
            destructive: true,
            on_confirm: Rc::new(move |window, cx| {
                // Runs inside the shell's own listener: defer the shell update.
                let handle = window.window_handle();
                let shell = shell.clone();
                cx.defer(move |cx| {
                    let _ = shell.update(cx, |s, cx| s.begin_stop(quit, handle, cx));
                });
            }),
        });
        self.dialog_armed = false;
        window.refresh();
        cx.notify();
        false
    }

    /// Work a close would stop, in words (empty when nothing is running).
    fn pending_work(&self, cx: &App) -> Vec<String> {
        let mut out = Vec::new();
        if self.engine.as_ref().is_some_and(Engine::active) {
            out.push("Dictation running in this window will stop.".to_string());
        }
        let downloads = self.cx.services.downloads.running();
        if !downloads.is_empty() {
            out.push(format!(
                "{} model download{} will be cancelled.",
                downloads.len(),
                if downloads.len() == 1 { "" } else { "s" }
            ));
        }
        if self.cx.services.downloads.deleting() {
            out.push("A speech model is being deleted; closing waits for it.".into());
        }
        // A History retry is a registered write too, but it isn't a setting.
        let retrying = self.cx.services.retry_jobs.running().is_some();
        if retrying {
            out.push(
                "A dictation is being retried; closing waits briefly for it, and speech recognition can't be interrupted."
                    .into(),
            );
        }
        // Key, provider, setup or settings writes on workers (registered as
        // pending writes); a zero-length check, no waiting here.
        if !retrying && !lf_ui::shutdown::wait_for_pending_writes(Duration::ZERO) {
            out.push("A settings or key change is still being saved; closing waits for it.".into());
        }
        if self.setup_dirty(cx) {
            out.push("Your setup choices haven't been applied and will be discarded.".into());
        }
        out
    }

    fn release_setup_audio(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.setup_view {
            view.update(cx, |v, cx| v.release_audio(cx));
        }
    }

    /// Stop what this window started — cancel model jobs, stop the engine —
    /// on the background executor with bounded waits, then close. If
    /// stopping isn't confirmed, the window stays open, keeps the engine
    /// (so a later close waits for it again), and says so, offering an
    /// explicit force quit instead of claiming it finished.
    fn begin_stop(&mut self, quit: bool, handle: gpui::AnyWindowHandle, cx: &mut Context<Self>) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.release_setup_audio(cx);
        // Quiesce: pages are covered (render), shell actions and routed
        // pages are refused, and focus leaves any page control, until the
        // stop finishes or fails.
        let focus = self.focus.clone();
        cx.defer(move |cx| {
            let _ = handle.update(cx, |_, window, _| window.focus(&focus));
        });
        let downloads = self.cx.services.downloads.clone();
        let engine = self.engine.take();
        cx.spawn(async move |this, cx| {
            let (jobs_stopped, engine_outcome, engine) = cx
                .background_executor()
                .spawn(async move {
                    // Downloads are cancelled; deletions and persistent writes
                    // (keys, provider, setup, settings) are waited for.
                    downloads.cancel_all();
                    let jobs_stopped = downloads.wait_idle(Duration::from_secs(3))
                        && lf_ui::shutdown::wait_for_pending_writes(Duration::from_secs(3));
                    let (outcome, engine) = match engine {
                        Some(mut engine) => {
                            let outcome = engine.shutdown(crate::controllers::engine::STOP_WAIT);
                            (outcome, Some(engine))
                        }
                        None => (crate::controllers::engine::Stopped::Done, None),
                    };
                    (jobs_stopped, outcome, engine)
                })
                .await;
            let _ = this.update(cx, |shell, cx| {
                use crate::controllers::engine::Stopped;
                shell.stopping = false;
                // Fresh check: anything that started after the drain keeps
                // the window open (never an obsolete snapshot).
                let late = crate::controllers::shutdown::remaining_work(
                    &shell.cx.services.downloads,
                    &shell.cx.services.retry_jobs,
                    lf_ui::shutdown::wait_for_pending_writes(Duration::ZERO),
                );
                if jobs_stopped && engine_outcome == Stopped::Done && !late.is_empty() {
                    shell.show_stop_problem(quit, handle, late.join(" "), cx);
                    return;
                }
                if jobs_stopped && engine_outcome == Stopped::Done {
                    shell.force_close = true;
                    cx.defer(move |cx| {
                        if quit {
                            cx.quit();
                        } else {
                            let _ = handle.update(cx, |_, window, _| window.remove_window());
                        }
                    });
                    return;
                }
                // Keep ownership of whatever didn't confirm it stopped
                // (a crashed engine stays unconfirmed on the next close too).
                if engine_outcome != Stopped::Done {
                    shell.engine = engine;
                }
                let mut problems = Vec::new();
                if !jobs_stopped {
                    if shell.cx.services.retry_jobs.running().is_some() {
                        problems.push(crate::controllers::shutdown::RETRY_RUNNING);
                    }
                    problems.push(
                        "A model download, model deletion or settings/key write hasn't finished.",
                    );
                }
                match engine_outcome {
                    Stopped::Pending => problems.push("Dictation in this window is still stopping."),
                    Stopped::Crashed => problems.push(
                        "The dictation engine failed while stopping, so it isn't confirmed that the microphone and keyboard listener were released.",
                    ),
                    Stopped::Done => {}
                }
                shell.show_stop_problem(quit, handle, problems.join(" "), cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn show_stop_problem(
        &mut self,
        quit: bool,
        handle: gpui::AnyWindowHandle,
        problems: String,
        cx: &mut Context<Self>,
    ) {
        let shell = cx.weak_entity();
        self.confirm = Some(Confirm {
            title: "LocalFlow hasn't finished stopping".into(),
            body: format!(
                "{problems} Cancel keeps the window open; close it again to retry. Quitting anyway ends the app now without waiting."
            )
            .into(),
            verb: if quit { "Quit anyway" } else { "Close anyway" }.into(),
            destructive: true,
            on_confirm: Rc::new(move |_, cx| {
                let shell = shell.clone();
                cx.defer(move |cx| {
                    let _ = shell.update(cx, |s, _| s.force_close = true);
                    if quit {
                        cx.quit();
                    } else {
                        let _ = handle.update(cx, |_, window, _| window.remove_window());
                    }
                });
            }),
        });
        self.dialog_armed = false;
        cx.notify();
    }

    /// Ctrl+Q / Ctrl+W.
    fn request_close(&mut self, quit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.should_close(quit, window, cx) {
            if quit {
                cx.quit();
            } else {
                window.remove_window();
            }
        }
    }

    /// Close the dialog, restore focus, then run the action when confirmed.
    fn close_dialog(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        self.dialog_armed = false;
        match self.restore_focus.take() {
            Some(handle) => window.focus(&handle),
            None => window.focus(&self.focus),
        }
        if run {
            (confirm.on_confirm)(window, cx);
        }
        self.apply_queued(cx);
        cx.notify();
    }

    /// Tab / Shift-Tab: cycle inside an open dialog, else move normally.
    fn tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_none() {
            if forward {
                window.focus_next();
            } else {
                window.focus_prev();
            }
            return;
        }
        let order = [self.cancel_focus.clone(), self.ok_focus.clone()];
        let current = order.iter().position(|h| h.is_focused(window));
        let next = trap_step(current, order.len(), forward);
        window.focus(&order[next]);
        cx.notify();
    }

    fn render_confirm(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let confirm = self.confirm.as_ref()?;
        let verb = Button::new("confirm-ok", confirm.verb.clone())
            .track_focus(&self.ok_focus)
            .variant(if confirm.destructive {
                ui::Variant::Destructive
            } else {
                ui::Variant::Primary
            })
            .on_click(cx.listener(|this, _, window, cx| this.close_dialog(true, window, cx)));
        Some(
            div()
                .id("scrim")
                .occlude()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.scrim)
                .child(
                    div()
                        .id("dialog")
                        .occlude()
                        .flex()
                        .flex_col()
                        .gap(space::s3())
                        .w(px(420.))
                        .p(space::s7())
                        .rounded(tsize::radius_container())
                        .border_1()
                        .border_color(t.border)
                        .bg(t.bg_popover)
                        .shadow(elevation::popover(t))
                        .child(ui::heading(t, confirm.title.clone()))
                        .child(ui::secondary(t, confirm.body.clone()))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(space::s4())
                                .pt(space::s6())
                                .child(
                                    Button::new("confirm-cancel", "Cancel")
                                        .track_focus(&self.cancel_focus)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_dialog(false, window, cx)
                                        })),
                                )
                                .child(verb),
                        ),
                ),
        )
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        if self.confirm.is_some() && !self.dialog_armed {
            // Move focus into the dialog (on Cancel) once it is on screen.
            self.dialog_armed = true;
            cx.on_next_frame(window, |this, window, cx| {
                if this.confirm.is_some() {
                    let current = window.focused(cx);
                    let inside = current
                        .as_ref()
                        .is_some_and(|h| *h == this.cancel_focus || *h == this.ok_focus);
                    if !inside {
                        this.restore_focus = current;
                    }
                    window.focus(&this.cancel_focus);
                    cx.notify();
                }
            });
        }
        self.ensure_view(self.page, cx);
        let sandbox = self.cx.services.is_sandbox();
        let overlay = self
            .setup_view
            .clone()
            .filter(|_| self.setup_visible)
            .map(|v| v.into_any_element());
        let main = match overlay {
            Some(overlay) => div().flex().flex_1().min_h_0().child(overlay),
            None => {
                let sidebar = self.render_sidebar(&t, cx);
                let page = self.page_element();
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(div().flex_1().min_w_0().h_full().child(page))
            }
        };
        let toast = self.render_toast(&t, cx);
        let confirm = self.render_confirm(&t, cx);
        let (sz, lh) = text::body();
        div()
            .id("shell")
            .key_context("Shell")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Go1, _, cx| this.go_index(0, cx)))
            .on_action(cx.listener(|this, _: &Go2, _, cx| this.go_index(1, cx)))
            .on_action(cx.listener(|this, _: &Go3, _, cx| this.go_index(2, cx)))
            .on_action(cx.listener(|this, _: &Go4, _, cx| this.go_index(3, cx)))
            .on_action(cx.listener(|this, _: &Go5, _, cx| this.go_index(4, cx)))
            .on_action(cx.listener(|this, _: &Go6, _, cx| this.go_index(5, cx)))
            .on_action(cx.listener(|this, _: &Go7, _, cx| this.go_index(6, cx)))
            .on_action(cx.listener(|this, _: &Go8, _, cx| this.go_index(7, cx)))
            .on_action(cx.listener(|this, _: &Go9, _, cx| this.go_index(8, cx)))
            .on_action(cx.listener(|this, _: &Go10, _, cx| this.go_index(9, cx)))
            .on_action(cx.listener(|this, _: &GoHome, _, cx| this.go_index(0, cx)))
            .on_action(
                cx.listener(|this, _: &OpenSettings, _, cx| {
                    this.navigate(Page::Settings, None, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &FocusSearch, w, cx| this.focus_search(w, cx)))
            .on_action(cx.listener(|this, _: &NewItem, w, cx| this.new_item(w, cx)))
            .on_action(cx.listener(|this, _: &Escape, w, cx| this.escape(w, cx)))
            .on_action(cx.listener(|this, _: &NavUp, w, cx| this.nav_step(-1, w, cx)))
            .on_action(cx.listener(|this, _: &NavDown, w, cx| this.nav_step(1, w, cx)))
            .on_action(cx.listener(|this, _: &FocusNext, window, cx| this.tab(true, window, cx)))
            .on_action(cx.listener(|this, _: &FocusPrev, window, cx| this.tab(false, window, cx)))
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                this.request_close(false, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &Quit, window, cx| this.request_close(true, window, cx)),
            )
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .font_family(t.font_family.clone())
            .text_size(sz)
            .line_height(lh)
            .text_color(t.text)
            .bg(t.bg_surface)
            .when(sandbox, |d| {
                let dir = self
                    .cx
                    .services
                    .root
                    .sandbox_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                d.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(space::s4())
                        .h(px(28.))
                        .px(space::s6())
                        .bg(t.sandbox_bg)
                        .border_b_1()
                        .border_color(t.border)
                        .text_color(t.sandbox_text)
                        .text_size(text::supporting().0)
                        .line_height(text::supporting().1)
                        .child(ui::icon("triangle-alert", t.sandbox_text).size(px(14.)))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Sandbox — example data."),
                        )
                        .child(format!(
                            "Nothing here touches your real LocalFlow data. Stored in {dir}"
                        )),
                )
            })
            .child(main)
            .children(toast)
            .children(confirm)
            // While closing: everything under the scrim takes no clicks.
            .when(self.stopping, |d| {
                d.child(
                    div()
                        .id("stopping-scrim")
                        .occlude()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(t.scrim)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(space::s4())
                                .px(space::s7())
                                .py(space::s5())
                                .rounded(tsize::radius_container())
                                .border_1()
                                .border_color(t.border)
                                .bg(t.bg_popover)
                                .shadow(elevation::popover(&t))
                                .child(ui::icon("loader-circle", t.icon_secondary))
                                .child(ui::label(&t, "Stopping LocalFlow…")),
                        ),
                )
            })
    }
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-1", Go1, None),
        KeyBinding::new("secondary-2", Go2, None),
        KeyBinding::new("secondary-3", Go3, None),
        KeyBinding::new("secondary-4", Go4, None),
        KeyBinding::new("secondary-5", Go5, None),
        KeyBinding::new("secondary-6", Go6, None),
        KeyBinding::new("secondary-7", Go7, None),
        KeyBinding::new("secondary-8", Go8, None),
        KeyBinding::new("secondary-9", Go9, None),
        KeyBinding::new("secondary-0", Go10, None),
        KeyBinding::new("secondary-k", FocusSearch, None),
        KeyBinding::new("secondary-f", FocusSearch, None),
        KeyBinding::new("secondary-n", NewItem, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrev, None),
        KeyBinding::new("escape", Escape, None),
        KeyBinding::new("up", NavUp, Some("Shell")),
        KeyBinding::new("down", NavDown, Some("Shell")),
    ]);
    ui::input::bind_keys(cx);
    ui::select::bind_keys(cx);
}

/// Prefer Figtree (if installed), then common Linux UI fonts.
fn pick_font(cx: &App) -> SharedString {
    if cfg!(target_os = "macos") || cfg!(windows) {
        return ".SystemUIFont".into();
    }
    let names = cx.text_system().all_font_names();
    for want in [
        "Figtree",
        "Inter",
        "Cantarell",
        "Ubuntu",
        "Noto Sans",
        "Liberation Sans",
        "DejaVu Sans",
    ] {
        if names.iter().any(|n| n == want) {
            return want.into();
        }
    }
    ".SystemUIFont".into()
}

/// How [`run`] ended, for the process exit status. Launchers (`lf --gpui`,
/// `lf-hub` on Linux) read exit 0 as "a window is up or the request was
/// handed to the open one", so a window that never opened must not be 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    /// The window opened (and later closed normally).
    Closed,
    /// No window could be opened.
    WindowFailed,
}

impl RunStatus {
    /// Exit code: 0 for a normal close, 1 when no window could open.
    pub fn exit_code(self) -> u8 {
        match self {
            RunStatus::Closed => 0,
            RunStatus::WindowFailed => 1,
        }
    }
}

/// Record a failed `open_window`: report it and mark the run failed. On macOS
/// GPUI's quit is `NSApp terminate:`, which exits the process with status 0
/// and never returns from [`run`], so there the failure status exits directly.
/// At this point no window or engine has started; services were already opened
/// before [`run`], so this makes no claim about earlier data writes.
fn window_failed(error: &dyn std::fmt::Display, status: &std::cell::Cell<RunStatus>) {
    eprintln!("lf-desktop: couldn't open a window: {error:#}");
    status.set(RunStatus::WindowFailed);
    if cfg!(target_os = "macos") {
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        std::process::exit(i32::from(RunStatus::WindowFailed.exit_code()));
    }
}

pub fn run(launch: Launch) -> RunStatus {
    let status = Rc::new(std::cell::Cell::new(RunStatus::Closed));
    let seen = status.clone();
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            // The stored appearance choice decides the very first frame:
            // native chrome first (no window exists yet, so no appearance
            // handler can re-enter), then the palette, so System resolves
            // against what the chrome will show.
            let mode = crate::appearance::load(&launch.services.root);
            crate::native_appearance::set(mode);
            let mut base = Theme::for_mode(mode, cx.window_appearance());
            base.font_family = pick_font(cx);
            cx.set_global(base);
            bind_keys(cx);
            // Quit with nothing focused in the shell: ask the shell window.
            cx.on_action(|_: &Quit, cx| {
                let shells: Vec<_> = cx
                    .windows()
                    .into_iter()
                    .filter_map(|w| w.downcast::<Shell>())
                    .collect();
                if shells.is_empty() {
                    lf_ui::shutdown::wait_for_pending_writes(Duration::from_secs(5));
                    cx.quit();
                }
                for shell in shells {
                    let _ = shell.update(cx, |s, window, cx| s.request_close(true, window, cx));
                }
            });
            let sandbox = launch.services.is_sandbox();
            let bounds = Bounds::centered(None, size(px(1280.), px(850.)), cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(960.), px(640.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some(
                            if sandbox {
                                "LocalFlow (Sandbox)"
                            } else {
                                "LocalFlow"
                            }
                            .into(),
                        ),
                        ..Default::default()
                    }),
                    app_id: Some("localflow-desktop".into()),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| Shell::new(launch, cx)),
            );
            match window {
                Ok(window) => {
                    crate::console::detach();
                    let _ = window.update(cx, |shell, window, cx| {
                        window.focus(&shell.focus_handle(cx));
                        Shell::install_focus_recovery(window, cx);
                        cx.activate(true);
                        // Titlebar close: flush first; keep the window open
                        // (with an explanation) when something wasn't saved.
                        let weak = cx.weak_entity();
                        window.on_window_should_close(cx, move |window, cx| {
                            weak.update(cx, |s, cx| s.should_close(false, window, cx))
                                .unwrap_or(true)
                        });
                        Shell::start_instance_poll(window, cx);
                        #[cfg(any(
                            target_os = "linux",
                            target_os = "windows",
                            target_os = "macos"
                        ))]
                        shell.start_tray(window, cx);
                        // Losing the window releases a setup microphone test
                        // (also one still opening) and cancels a trial;
                        // nothing captures in the background. After a system
                        // permission prompt the user presses Test again.
                        cx.observe_window_activation(window, |shell, window, cx| {
                            if !window.is_window_active() {
                                if let Some(view) = shell.setup_view.clone() {
                                    view.update(cx, |v, cx| v.release_audio(cx));
                                }
                            }
                        })
                        .detach();
                        // Follow the system light/dark setting live, in
                        // System mode only (Light/Dark ignore the OS).
                        cx.observe_window_appearance(window, |_, window, cx| {
                            if let Some(next) = theme(cx).after_system_change(window.appearance()) {
                                cx.set_global(next);
                                cx.refresh_windows();
                                cx.notify();
                            }
                        })
                        .detach();
                    });
                }
                Err(err) => {
                    window_failed(&err, &seen);
                    cx.quit();
                }
            }
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    lf_ui::shutdown::wait_for_pending_writes(Duration::from_secs(5));
                    cx.quit();
                }
            })
            .detach();
        });
    status.get()
}

/// Next index in a focus trap of `len` stops (wraps; starts at the first
/// stop going forward and the last going backward).
pub fn trap_step(current: Option<usize>, len: usize, forward: bool) -> usize {
    match (current, forward) {
        (None, true) => 0,
        (None, false) => len.saturating_sub(1),
        (Some(i), true) => (i + 1) % len.max(1),
        (Some(i), false) => (i + len.max(1) - 1) % len.max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::{trap_step, RunStatus};

    #[test]
    fn only_a_failed_window_exits_non_zero() {
        // 0 is what launchers read as "window up or handed off".
        assert_eq!(RunStatus::Closed.exit_code(), 0);
        assert_eq!(RunStatus::WindowFailed.exit_code(), 1);
    }

    // On macOS `window_failed` exits the process itself (GPUI's quit there
    // never returns from `run`), so the recording path is checked elsewhere.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_window_that_fails_to_open_is_recorded_for_the_exit_status() {
        let status = std::cell::Cell::new(RunStatus::Closed);
        super::window_failed(&"no display", &status);
        assert_eq!(status.get(), RunStatus::WindowFailed);
        assert_ne!(status.get().exit_code(), 0);
    }

    #[test]
    fn dialog_focus_trap_cycles_within_its_buttons() {
        assert_eq!(
            trap_step(None, 2, true),
            0,
            "Tab from outside lands on Cancel"
        );
        assert_eq!(trap_step(None, 2, false), 1);
        assert_eq!(trap_step(Some(0), 2, true), 1);
        assert_eq!(trap_step(Some(1), 2, true), 0, "wraps instead of leaving");
        assert_eq!(trap_step(Some(0), 2, false), 1);
        assert_eq!(trap_step(Some(1), 2, false), 0);
    }
}

/// Keyboard focus in a headless GPUI window (no display, no app): the real
/// `Shell` over a seeded temporary sandbox, so secrets stay in memory, health
/// and runtime are simulated, and no engine, tray, hotkey, model or network
/// is touched. Shortcuts go through the real keymap and dispatch tree.
#[cfg(test)]
mod focus_tests {
    use std::rc::Rc;

    use gpui::{Entity, FocusHandle, TestAppContext, VisualTestContext};

    use super::*;

    /// Opens the shell on `page` the way [`run`] does. Keep the returned
    /// directory alive for the whole test.
    fn open(
        page: Page,
        cx: &mut TestAppContext,
    ) -> (Entity<Shell>, &mut VisualTestContext, tempfile::TempDir) {
        open_with(page, |_| {}, cx)
    }

    /// [`open`], after `prepare` adjusts the seeded sandbox data.
    fn open_with(
        page: Page,
        prepare: impl FnOnce(&crate::data::DataRoot),
        cx: &mut TestAppContext,
    ) -> (Entity<Shell>, &mut VisualTestContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let launched =
            crate::launch::sandbox(&dir.path().join("sandbox"), false, Some(Route::Page(page)))
                .unwrap();
        let crate::launch::Launched::Window(root, instance) = launched else {
            panic!("expected to own the sandbox window");
        };
        prepare(&root);
        let services = Services::open(*root).unwrap();
        assert!(services.is_sandbox());
        cx.update(|cx| {
            cx.set_global(Theme::for_appearance(cx.window_appearance()));
            bind_keys(cx);
        });
        let launch = Launch {
            services,
            page,
            setup: false,
            notice: None,
            instance,
        };
        let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(launch, cx));
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                window.focus(&shell.focus);
                Shell::install_focus_recovery(window, cx);
            })
        });
        cx.run_until_parked();
        (shell, cx, dir)
    }

    fn page(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Page {
        shell.read_with(cx, |shell, _| shell.page)
    }

    /// Focus is on a rendered element inside the shell, where its shortcut
    /// listeners are.
    fn in_shell(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> bool {
        let root = shell.read_with(cx, |shell, _| shell.focus.clone());
        cx.update(|window, cx| root.contains_focused(window, cx))
    }

    fn focused(handle: &FocusHandle, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, _| handle.is_focused(window))
    }

    #[gpui::test]
    fn shortcuts_keep_working_after_leaving_a_focused_history_list(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open(Page::History, cx);
        let list = shell.read_with(cx, |shell, cx| match shell.find(Page::History) {
            Some(View::History(view)) => view.read(cx).list_focus(),
            _ => panic!("History isn't open"),
        });
        // What a row click does.
        cx.update(|window, _| window.focus(&list));
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(focused(&list, cx), "a repaint leaves valid focus alone");
        assert!(in_shell(&shell, cx));

        cx.simulate_keystrokes("secondary-1");
        assert_eq!(page(&shell, cx), Page::Home);

        // History (and its list handle) is cached but no longer on screen.
        let third = Page::sidebar_order()[2];
        cx.simulate_keystrokes("secondary-3");
        assert_eq!(page(&shell, cx), third, "shortcuts still reach the shell");
        assert!(in_shell(&shell, cx), "focus came back to the shell");
        cx.simulate_keystrokes("secondary-3");
        assert_eq!(page(&shell, cx), third, "the same shortcut again stays");
        cx.simulate_keystrokes("secondary-2");
        assert_eq!(page(&shell, cx), Page::History);
    }

    #[gpui::test]
    fn shortcuts_keep_working_after_the_focused_element_is_removed(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open(Page::Models, cx);
        assert_eq!(page(&shell, cx), Page::Models);
        let root = shell.read_with(cx, |shell, _| shell.focus.clone());
        let action: ui::Action = Rc::new(|_, _| panic!("the toast action ran"));
        shell.update(cx, |shell, cx| {
            shell.toast("Model removed".into(), Some(("Undo".into(), action)), cx)
        });
        cx.run_until_parked();

        // The toast is painted last, so Shift-Tab from the shell reaches its
        // Dismiss button, then its action (both keep their focus handles in
        // element state only).
        let mounted = |handle: &FocusHandle, cx: &mut VisualTestContext| {
            cx.update(|window, _| root.contains(handle, window))
        };
        cx.simulate_keystrokes("shift-tab");
        let dismiss = cx.update(|window, cx| window.focused(cx)).unwrap();
        cx.simulate_keystrokes("shift-tab");
        let toast_action = cx.update(|window, cx| window.focused(cx)).unwrap();
        assert!(toast_action != dismiss && toast_action != root);
        assert!(mounted(&dismiss, cx) && mounted(&toast_action, cx));
        assert!(in_shell(&shell, cx), "valid focus inside the shell");
        let (dismiss, toast_action) = (dismiss.downgrade(), toast_action.downgrade());

        cx.simulate_keystrokes("escape");
        assert!(shell.read_with(cx, |shell, _| shell.toast.is_none()));
        // Both stops belonged to the toast: they went with it, so the
        // focused element is gone rather than merely unfocused.
        for (name, gone) in [("action", &toast_action), ("Dismiss", &dismiss)] {
            assert!(
                gone.upgrade().is_none_or(|h| !mounted(&h, cx)),
                "the toast {name} is no longer on screen"
            );
        }

        cx.simulate_keystrokes("secondary-1");
        assert_eq!(page(&shell, cx), Page::Home);
        assert!(in_shell(&shell, cx));
    }

    #[gpui::test]
    fn an_open_dialog_takes_lost_focus_on_cancel_and_keeps_its_trap(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open(Page::Models, cx);
        shell.update(cx, |shell, cx| {
            shell.confirm = Some(Confirm {
                title: "Delete?".into(),
                body: "Test dialog".into(),
                verb: "Delete".into(),
                destructive: true,
                on_confirm: Rc::new(|_, _| panic!("the dialog was cancelled")),
            });
            shell.dialog_armed = false;
            cx.notify();
        });
        cx.run_until_parked();
        let (cancel, ok) = shell.read_with(cx, |shell, _| {
            (shell.cancel_focus.clone(), shell.ok_focus.clone())
        });
        // The test window runs no next-frame callbacks, so the dialog's own
        // move to Cancel hasn't happened; lose focus instead.
        cx.update(|window, cx| window.focus(&cx.focus_handle()));
        cx.run_until_parked();
        assert!(focused(&cancel, cx), "lost focus lands on Cancel");

        cx.dispatch_action(FocusNext);
        assert!(focused(&ok, cx));
        cx.dispatch_action(FocusNext);
        assert!(focused(&cancel, cx), "Tab stays inside the dialog");
        cx.dispatch_action(Go1);
        assert_eq!(page(&shell, cx), Page::Models, "no navigation behind it");

        cx.dispatch_action(Escape);
        assert!(shell.read_with(cx, |shell, _| shell.confirm.is_none()));
        assert!(in_shell(&shell, cx));
        cx.dispatch_action(Go1);
        assert_eq!(page(&shell, cx), Page::Home);
    }

    /// Focus on a handle that has just been dropped (a removed button).
    fn lose_focus(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.focus(&cx.focus_handle()));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn focus_is_recovered_again_after_a_second_loss(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open(Page::Models, cx);
        lose_focus(cx);
        assert!(in_shell(&shell, cx), "first loss recovered");
        // Nothing else repaints in between (a keyboard-only user).
        lose_focus(cx);
        assert!(in_shell(&shell, cx), "second loss recovered");
        cx.dispatch_action(Go1);
        assert_eq!(page(&shell, cx), Page::Home);
    }

    fn dictionary_filter(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> FocusHandle {
        shell.read_with(cx, |shell, cx| match shell.find(Page::Dictionary) {
            Some(View::Dictionary(view)) => view.read(cx).filter_focus(cx),
            _ => panic!("Dictionary isn't open"),
        })
    }

    #[gpui::test]
    fn find_on_a_short_dictionary_leaves_focus_where_it_is(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open(Page::Dictionary, cx);
        let (words, replacements) = shell.read_with(cx, |shell, _| {
            let store = &shell.cx.services.dictionary;
            (store.words().len(), store.replacements().len())
        });
        assert!(words + replacements <= 12, "the seeded list is short");
        let filter = dictionary_filter(&shell, cx);
        let root = shell.read_with(cx, |shell, _| shell.focus.clone());
        assert!(
            !cx.update(|window, _| root.contains(&filter, window)),
            "a short list shows no filter"
        );
        // Tab onto a real control.
        cx.update(|window, _| window.focus_next());
        cx.run_until_parked();
        let control = cx
            .update(|window, cx| window.focused(cx))
            .expect("Tab focused a control");
        assert!(control != root && in_shell(&shell, cx));

        cx.simulate_keystrokes("secondary-k");
        assert!(focused(&control, cx), "Find with no filter keeps focus");
        cx.simulate_keystrokes("secondary-k");
        assert!(focused(&control, cx), "and again");
        assert!(in_shell(&shell, cx));
        cx.simulate_keystrokes("secondary-1");
        assert_eq!(page(&shell, cx), Page::Home);
    }

    #[gpui::test]
    fn find_on_a_long_dictionary_focuses_its_filter(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open_with(
            Page::Dictionary,
            |root| {
                let store =
                    lf_core::dictionary::DictionaryStore::new(Some(root.dictionary_path.clone()));
                for word in ["Alpha", "Bravo", "Charlie", "Delta", "Echo"] {
                    store.add_word(word).unwrap();
                }
            },
            cx,
        );
        let filter = dictionary_filter(&shell, cx);
        cx.simulate_keystrokes("secondary-k");
        assert!(focused(&filter, cx), "Find focuses the filter");
        assert!(in_shell(&shell, cx), "which is on screen");
    }
}
