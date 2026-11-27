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
use crate::controllers::instance::{Instance, Request, Route};
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
    /// `--sandbox-fixture`: the screenshot scene's one action after launch.
    pub fixture: Option<crate::sandbox::Fixture>,
    /// `--page agents`: opened once the service says whether Agents is on
    /// (Home with the unknown-page notice if it isn't). `notice` is `None`.
    pub agents_request: bool,
    /// `--agent-session`: the session a cold-start `--page agents` selects.
    pub agent_session: Option<String>,
    /// `--page agents/settings` (with `agents_request`): Settings → Agents
    /// once the service has answered.
    pub agents_settings: bool,
}

/// A request for the `agents` page (an unknown name: it has no Hub route).
fn is_agents_request(request: &Request) -> bool {
    request.is_unknown_home()
        && request
            .unknown_page
            .as_deref()
            .is_some_and(Page::is_agents_name)
}

/// `agents/settings` (from the launcher or an empty state): Settings →
/// Agents, which needs only a supporting service (Agents may be off).
fn is_agents_settings_request(request: &Request) -> bool {
    request.is_unknown_home()
        && request
            .unknown_page
            .as_deref()
            .is_some_and(Page::is_agents_settings_name)
}

/// What the needs-input watcher does next.
enum AttentionNext {
    Stop,
    Pause(Duration),
    Prime(std::sync::Arc<dyn crate::controllers::agents::AgentsBackend>),
    Poll(
        std::sync::Arc<dyn crate::controllers::agents::AgentsBackend>,
        u64,
        u32,
    ),
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

type HealthCheckDone = Box<dyn FnOnce(&mut App)>;

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
    /// The agents service speaks v1, on or off (written by the shell's
    /// status probe): Settings shows its Agents tab.
    pub agents_supported: Rc<std::cell::Cell<bool>>,
    /// At most one `/changes` call open in this window (the Agents page's
    /// list and the shell's needs-input watcher share it).
    pub agents_changes: pages::agents_poll::ChangesGate,
}

impl PageCx {
    /// Settings → Agents is offered (see [`crate::controllers::agents::supported`]).
    pub fn agents_supported(&self) -> bool {
        self.agents_supported.get()
    }
    /// `[agents]` changed at the service: ask it again, so the gate (and
    /// the sidebar's Agents row) follows without a restart.
    pub fn agents_changed(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| {
            shell.probe_agents(cx);
            cx.notify();
        });
    }
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
    /// [`PageCx::recheck_health`], then `done` once this check's result is
    /// stored (also when health didn't change).
    pub fn recheck_health_then(&self, cx: &mut App, done: impl FnOnce(&mut App) + 'static) {
        self.with_shell(cx, move |shell, cx| {
            shell.check_health_then(Some(Box::new(done)), cx)
        });
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
    /// The Agents page, while shown, feeds the sidebar's needs-input count.
    pub fn set_agents_attention(&self, n: usize, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| {
            if shell.page == Page::Agents && shell.agents_attention != n {
                shell.agents_attention = n;
                cx.notify();
            }
        });
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
    People(Entity<pages::people::PeoplePage>),
    Agents(Entity<pages::agents::AgentsPage>),
}

pub struct Shell {
    pub cx: PageCx,
    pub page: Page,
    views: Vec<(Page, View)>,
    pub health: Health,
    pub pending_prompts: usize,
    /// The service last reported the agents feature on (see
    /// [`Shell::agents_enabled`]).
    agents: bool,
    /// The newest status probe (older replies are dropped).
    agents_probe: u64,
    /// A status probe has answered at least once.
    agents_known: bool,
    /// A request for `agents` waiting for that first answer.
    pending_agents: Option<Request>,
    /// Sessions that need input: the sidebar badge on Agents.
    pub agents_attention: usize,
    /// The needs-input watcher's model (ids only; see
    /// [`Shell::watch_attention`]).
    attention: crate::controllers::agents_attention::Attention,
    /// Providers read with the last prime (the toast names one).
    attention_providers: Vec<crate::controllers::agents::Provider>,
    /// The watcher loop is running.
    attention_watching: bool,
    /// Failed watcher reads in a row (the backoff).
    attention_failures: u32,
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
    queued: Option<Request>,
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
            agents_supported: Rc::new(std::cell::Cell::new(false)),
            agents_changes: Default::default(),
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
            agents: false,
            agents_probe: 0,
            agents_known: false,
            pending_agents: launch.agents_request.then(|| Request {
                route: Route::Page(Page::Home),
                unknown_page: Some(match &launch.agent_session {
                    _ if launch.agents_settings => Page::AGENTS_SETTINGS.into(),
                    Some(id) => format!("agents/{id}"),
                    None => "agents".into(),
                }),
            }),
            agents_attention: 0,
            attention: Default::default(),
            attention_providers: Vec::new(),
            attention_watching: false,
            attention_failures: 0,
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
        if launch.fixture.is_some_and(|f| f.is_agents()) {
            // A scene draws the pinned in-memory seed, never the shared
            // sandbox state another run left behind.
            if let Some(fake) = shell.cx.services.remote.sandbox_agents() {
                match launch.fixture {
                    Some(f) if f.is_agents_settings() => fake.use_settings_fixture(),
                    _ => fake.use_fixture(),
                }
            }
        }
        shell.probe_agents(cx);
        shell.probe_runtime(cx);
        shell.start_timers(cx);
        if let Some(fixture) = launch.fixture {
            crate::sandbox::set_scene(fixture);
            // After the first frames and the page's first reads.
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let _ = this.update(cx, |shell, cx| shell.run_fixture(fixture, cx));
            })
            .detach();
        }
        shell
    }

    /// The fixture scene's one real action, through the page's own methods.
    fn run_fixture(&mut self, fixture: crate::sandbox::Fixture, cx: &mut Context<Self>) {
        use crate::sandbox::Fixture;
        self.navigate(fixture.page(), None, cx);
        match (fixture, self.find(self.page)) {
            (Fixture::SpeakersSaved, Some(View::Meetings(v))) => {
                v.update(cx, |p, cx| p.start_and_stop(cx))
            }
            (Fixture::SpeakersRecent, Some(View::Meetings(v))) => {
                v.update(cx, |p, cx| p.open_first_recent_speakers(cx))
            }
            (Fixture::SpeakersSettings, Some(View::Settings(v))) => {
                v.update(cx, |p, cx| p.open_tab("meetings", cx))
            }
            (Fixture::SpeakersPeople, Some(View::People(v))) => v.update(cx, |p, cx| p.refresh(cx)),
            (Fixture::AgentsSettings, Some(View::Settings(v))) => {
                v.update(cx, |p, cx| p.open_tab("agents", cx))
            }
            (Fixture::AgentsSettingsKey, Some(View::Settings(v))) => v.update(cx, |p, cx| {
                p.open_tab("agents", cx);
                if let Some(agents) = p.agents_settings() {
                    agents.update(cx, |a, cx| a.run_key_fixture(cx));
                }
            }),
            (Fixture::AgentsSettingsTrigger, Some(View::Settings(v))) => v.update(cx, |p, cx| {
                p.open_tab("agents", cx);
                if let Some(agents) = p.agents_settings() {
                    agents.update(cx, |a, cx| a.run_trigger_fixture(cx));
                }
            }),
            (
                Fixture::AgentsPage | Fixture::AgentsStates | Fixture::AgentsDetail,
                Some(View::Agents(v)),
            ) => v.update(cx, |p, cx| p.run_fixture(fixture, cx)),
            _ => {}
        }
    }

    /// Agents is shown (sidebar, routing, page) only while the service
    /// reports the feature on: `GET /api/agents/status` answers v1 with
    /// `enabled` (a 404 from an older or Rust server hides it). This is the
    /// `[features] agents` gate of the design; the switch itself lives in
    /// the service's config (`agents.enabled`), not in this window's.
    pub fn agents_enabled(&self) -> bool {
        self.agents
    }

    /// Ask the service whether Agents is on. The sandbox fake answers in
    /// process (at once); real mode reads it in the background. Only a
    /// decisive answer moves the gate ([`crate::controllers::agents::gate`]):
    /// while the service is stopped or answers with an error the last
    /// answer stands, so the page shows its own banner and Disconnected
    /// rows instead of vanishing.
    fn probe_agents(&mut self, cx: &mut Context<Self>) {
        self.agents_probe = self.agents_probe.wrapping_add(1);
        let generation = self.agents_probe;
        let backend = self.cx.services.remote.agents(&self.cx.services.config());
        if backend.simulated() {
            let status = backend.status();
            self.agents_answered(generation, status, cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { backend.status() })
                .await;
            let _ = this.update(cx, |shell, cx| {
                shell.agents_answered(generation, status, cx)
            });
        })
        .detach();
    }

    /// A status probe answered. A reply older than the newest probe is
    /// dropped, so a slow "off" can't overwrite a newer "on".
    fn agents_answered(
        &mut self,
        generation: u64,
        status: Result<crate::controllers::agents::Status, crate::controllers::agents::AgentsError>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.agents_probe {
            return;
        }
        if let Some(supported) = crate::controllers::agents::supported(&status) {
            if self.cx.agents_supported.replace(supported) != supported {
                cx.notify();
            }
        }
        if let Some(on) = crate::controllers::agents::gate(&status) {
            self.set_agents(on, cx);
        }
        // The first answer of any kind settles a request for `agents` that
        // arrived before the window knew (Agents, or Home with the notice).
        if !self.agents_known {
            self.agents_known = true;
            if let Some(request) = self.pending_agents.take() {
                self.route(request, cx);
            }
        }
    }

    fn set_agents(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.agents != on {
            self.agents = on;
            self.enforce_agents_gate(cx);
            if !on {
                // The badge goes with the page.
                self.attention.reset();
                self.agents_attention = 0;
            }
            cx.notify();
        }
        if on {
            self.watch_attention(cx);
        }
    }

    // ---------------------------------------------------- agents attention ---

    /// Keep the sidebar's needs-input count while Agents is on. One loop,
    /// ids only: it primes with `GET /sessions?state=waiting_input`, then
    /// follows `/changes` (held long-polls against a real service). While
    /// the Agents page is shown it makes no call at all (the page is the
    /// one `/changes` reader and feeds the count); coming back, it primes
    /// again, quietly. A session that newly needs input after a prime
    /// toasts once, while Agents isn't shown. It stops when the gate
    /// closes, the service has no agents API, or the window goes away.
    fn watch_attention(&mut self, cx: &mut Context<Self>) {
        use crate::pages::agents_poll::blocking;
        if self.attention_watching || !self.agents_enabled() {
            return;
        }
        self.attention_watching = true;
        let gate = self.cx.agents_changes.clone();
        cx.spawn(async move |this, cx| loop {
            let next = this
                .update(cx, |shell, cx| shell.next_attention(cx))
                .unwrap_or(AttentionNext::Stop);
            let pause = match next {
                AttentionNext::Stop => break,
                AttentionNext::Pause(pause) => pause,
                AttentionNext::Prime(backend) => {
                    let result = blocking(cx, false, move || {
                        Ok((backend.providers()?, backend.waiting()?))
                    })
                    .await;
                    match this.update(cx, |shell, cx| shell.attention_primed(result, cx)) {
                        Ok(Some(pause)) => pause,
                        _ => break,
                    }
                }
                AttentionNext::Poll(backend, since, wait) => {
                    let Some(ticket) = gate.enter() else {
                        // The page's call from before it was hidden.
                        cx.background_executor()
                            .timer(crate::pages::agents_poll::MIN_PAUSE)
                            .await;
                        continue;
                    };
                    let result = blocking(cx, wait > 0, move || backend.changes(since, wait)).await;
                    drop(ticket);
                    match this.update(cx, |shell, cx| shell.attention_polled(result, cx)) {
                        Ok(Some(pause)) => pause,
                        _ => break,
                    }
                }
            };
            cx.background_executor().timer(pause).await;
        })
        .detach();
    }

    fn next_attention(&mut self, _: &mut Context<Self>) -> AttentionNext {
        use crate::pages::agents_poll::{LONG_POLL, POLL_PAUSE};
        if !self.agents_enabled() || self.stopping {
            self.attention_watching = false;
            self.attention.reset();
            return AttentionNext::Stop;
        }
        if self.page == Page::Agents {
            // The page reads `/changes` and feeds the count; prime again
            // (quietly) once it is left.
            self.attention.reset();
            return AttentionNext::Pause(POLL_PAUSE);
        }
        let backend = self.cx.services.remote.agents(&self.cx.services.config());
        if !self.attention.primed() {
            return AttentionNext::Prime(backend);
        }
        let wait = if backend.simulated() { 0 } else { LONG_POLL };
        AttentionNext::Poll(backend, self.attention.revision(), wait)
    }

    fn attention_failed(
        &mut self,
        error: crate::controllers::agents::AgentsError,
    ) -> Option<Duration> {
        self.attention_failures = self.attention_failures.saturating_add(1);
        if error == crate::controllers::agents::AgentsError::Absent {
            self.attention_watching = false;
            return None;
        }
        Some(crate::pages::agents_poll::backoff(self.attention_failures))
    }

    fn attention_primed(
        &mut self,
        result: Result<
            (
                Vec<crate::controllers::agents::Provider>,
                crate::controllers::agents::SessionPage,
            ),
            crate::controllers::agents::AgentsError,
        >,
        cx: &mut Context<Self>,
    ) -> Option<Duration> {
        if self.page == Page::Agents || !self.agents_enabled() {
            return Some(Duration::ZERO);
        }
        match result {
            Ok((providers, page)) => {
                self.attention_failures = 0;
                self.attention.prime(&page);
                self.attention_providers = providers;
                self.set_attention_count(cx);
                Some(Duration::ZERO)
            }
            Err(error) => self.attention_failed(error),
        }
    }

    fn attention_polled(
        &mut self,
        result: Result<
            crate::controllers::agents::Changes,
            crate::controllers::agents::AgentsError,
        >,
        cx: &mut Context<Self>,
    ) -> Option<Duration> {
        use crate::controllers::agents_attention as model;
        use crate::pages::agents_poll::{MIN_PAUSE, POLL_PAUSE};
        // Shown meanwhile: the page took over (its own full read counts).
        if self.page == Page::Agents || !self.agents_enabled() {
            return Some(Duration::ZERO);
        }
        let changes = match result {
            Ok(changes) => changes,
            Err(error) => return self.attention_failed(error),
        };
        self.attention_failures = 0;
        let newly = self.attention.apply(&changes);
        self.set_attention_count(cx);
        // One toast: the newest session that started waiting.
        if let Some(session) = newly.last() {
            let message = model::toast(session, &self.attention_providers);
            let id = session.id.clone();
            let shell = cx.weak_entity();
            let open: ui::Action = Rc::new(move |_, cx| {
                let (shell, id) = (shell.clone(), id.clone());
                cx.defer(move |cx| {
                    let _ = shell.update(cx, |shell, cx| shell.open_agent_session(id, cx));
                });
            });
            self.toast(message.into(), Some((model::OPEN.into(), open)), cx);
        }
        let simulated = self
            .cx
            .services
            .remote
            .agents(&self.cx.services.config())
            .simulated();
        Some(if changes.waited && !simulated {
            MIN_PAUSE
        } else {
            POLL_PAUSE
        })
    }

    fn set_attention_count(&mut self, cx: &mut Context<Self>) {
        let n = self.attention.count();
        if self.agents_attention != n {
            self.agents_attention = n;
            cx.notify();
        }
    }

    /// The toast's Open: Agents, on that session.
    fn open_agent_session(&mut self, id: String, cx: &mut Context<Self>) {
        self.navigate(Page::Agents, None, cx);
        if let (Page::Agents, Some(View::Agents(v))) = (self.page, self.find(Page::Agents)) {
            v.update(cx, |p, cx| p.open_session(id, cx));
        }
    }

    /// The gate closed: drop the Agents view and leave the page if shown.
    fn enforce_agents_gate(&mut self, cx: &mut Context<Self>) {
        if self.agents {
            return;
        }
        if self.page == Page::Agents {
            self.page = Page::Home;
            *self.cx.current_page.borrow_mut() = Page::Home;
            self.ensure_view(Page::Home, cx);
            cx.notify();
        }
        self.views.retain(|(page, _)| *page != Page::Agents);
    }

    /// People is shown (sidebar, routing, page) only while turned on.
    pub fn people_enabled(&self) -> bool {
        crate::controllers::people::enabled(&self.cx.services.config())
    }

    /// `--page people` arrives as an unknown page (it has no Hub route):
    /// People while it is turned on, else Home with the notice as usual.
    fn resolve_people(&self, request: Request) -> Request {
        let people = request.is_unknown_home()
            && request
                .unknown_page
                .as_deref()
                .is_some_and(Page::is_people_name);
        if people && self.people_enabled() {
            Route::Page(Page::People).into()
        } else if is_agents_request(&request) && self.agents_enabled() {
            Route::Page(Page::Agents).into()
        } else if is_agents_request(&request) {
            // Off: the notice names the page, not a session it carried.
            Request {
                unknown_page: Some("agents".into()),
                ..request
            }
        } else {
            request
        }
    }

    /// The gate closed: drop the People view (its list goes with it) and
    /// leave the page if it is on screen.
    fn enforce_people_gate(&mut self, cx: &mut Context<Self>) {
        if self.people_enabled() {
            return;
        }
        if self.page == Page::People {
            self.page = Page::Home;
            *self.cx.current_page.borrow_mut() = Page::Home;
            self.ensure_view(Page::Home, cx);
            cx.notify();
        }
        self.views.retain(|(page, _)| *page != Page::People);
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
    /// overlay or a dialog is open). A routed Setup shows no unknown-page
    /// notice: it opens no page, so Home isn't shown. An unknown page's Home
    /// fallback closes the overlay instead (see [`Shell::apply_queued`]).
    fn poll_instance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stopping {
            // Leave the request in place (TTL-bounded): it is routed if the
            // close fails, and no setup or page opens meanwhile.
            return;
        }
        let Some(request) = self.instance.take_request() else {
            return;
        };
        window.activate_window();
        self.route(request, cx);
    }

    /// Route a request. One for `agents` that arrives before the first
    /// status probe has answered waits for it (see
    /// [`Shell::agents_answered`]) rather than showing "no page called
    /// agents" for a page that is about to exist.
    fn route(&mut self, request: Request, cx: &mut Context<Self>) {
        let settings = is_agents_settings_request(&request);
        if !self.agents_known && (settings || is_agents_request(&request)) {
            self.pending_agents = Some(request);
            return;
        }
        if settings {
            // Settings → Agents (General when the service doesn't support
            // agents); behind the overlay or a dialog it waits like any page.
            if self.setup_visible || self.confirm.is_some() {
                self.queued = Some(Route::Page(Page::Settings).into());
                self.apply_queued(cx);
            } else {
                self.navigate(Page::Settings, Some("agents"), cx);
            }
            return;
        }
        // `--page agents --agent-session <id>` (from the launcher).
        let session = request
            .unknown_page
            .as_deref()
            .filter(|_| is_agents_request(&request))
            .and_then(Page::agents_session);
        let request = self.resolve_people(request);
        match request.route {
            Route::Setup if self.confirm.is_none() => self.show_setup(cx),
            _ if self.setup_visible || self.confirm.is_some() => {
                self.queued = Some(request);
                self.apply_queued(cx);
            }
            Route::Page(page) => {
                self.open_routed(page, request.notice(), cx);
                if let (Page::Agents, Some(id), Some(View::Agents(v))) =
                    (self.page, session, self.find(Page::Agents))
                {
                    v.update(cx, |p, cx| p.open_session(id, cx));
                }
            }
            Route::Setup => self.queued = Some(request),
        }
    }

    /// Route the newest queued request once nothing covers the page. Behind
    /// setup only an unknown page's Home fallback goes ahead: it hides the
    /// overlay (draft kept, audio released), which routes it.
    fn apply_queued(&mut self, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            return;
        }
        if self.setup_visible {
            if !self.stopping && self.queued.as_ref().is_some_and(Request::is_unknown_home) {
                self.hide_setup(cx);
            }
            return;
        }
        let Some(request) = self.queued.take() else {
            return;
        };
        match request.route {
            Route::Page(page) => {
                let message = request
                    .notice()
                    .unwrap_or_else(|| format!("Opened {}", page.label()));
                self.open_routed(page, Some(message), cx);
            }
            Route::Setup => self.show_setup(cx),
        }
    }

    /// Go to a routed page, then toast `message` only if it really opened
    /// (a page with an unsaved edit can refuse to be left, and says why).
    fn open_routed(&mut self, page: Page, message: Option<String>, cx: &mut Context<Self>) {
        self.navigate(page, None, cx);
        if let Some(message) = message.filter(|_| self.page == page) {
            self.toast(message.into(), None, cx);
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
        self.enforce_people_gate(cx);
        self.probe_agents(cx);
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
                View::People(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Agents(v) => v.update(cx, |p, cx| p.refresh(cx)),
            }
        }
    }

    pub fn check_health(&mut self, cx: &mut Context<Self>) {
        self.check_health_then(None, cx);
    }

    /// [`Shell::check_health`], then `done` once the result is stored
    /// (also when health didn't change), outside the shell's update.
    fn check_health_then(&mut self, done: Option<HealthCheckDone>, cx: &mut Context<Self>) {
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
                } else if shell.health == Health::Online {
                    // The agents switch can change while the service runs.
                    shell.probe_agents(cx);
                }
            });
            if let Some(done) = done {
                let _ = cx.update(done);
            }
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
                self.queued = Some(Route::Page(page).into());
            }
            return;
        }
        // People is only reachable while turned on (and the gate may have
        // closed since its sidebar entry was drawn).
        let page = if (page == Page::People && !self.people_enabled())
            || (page == Page::Agents && !self.agents_enabled())
        {
            Page::Home
        } else {
            page
        };
        self.enforce_people_gate(cx);
        self.enforce_agents_gate(cx);
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
        if leaving == Page::Settings && page != Page::Settings {
            if let Some(View::Settings(v)) = self.find(Page::Settings) {
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
                View::People(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Agents(v) => v.update(cx, |p, cx| p.refresh(cx)),
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

    /// The window lost or regained activation. Losing it releases a setup
    /// microphone test; regaining it while Settings is the current page lets
    /// the page re-check speaker readiness (it asks only on the Meetings tab
    /// when not ready yet).
    pub(crate) fn window_activation_changed(&mut self, active: bool, cx: &mut Context<Self>) {
        if !active {
            if let Some(view) = self.setup_view.clone() {
                view.update(cx, |v, cx| v.release_audio(cx));
            }
            return;
        }
        if *self.cx.current_page.borrow() != Page::Settings {
            return;
        }
        let settings = match self.find(Page::Settings) {
            Some(View::Settings(view)) => view.clone(),
            _ => return,
        };
        settings.update(cx, |v, cx| v.window_activated(cx));
    }

    /// A created page's view (page tests drive the real page through it).
    #[cfg(test)]
    pub(crate) fn view(&self, page: Page) -> Option<&View> {
        self.find(page)
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
            Page::People => View::People(cx.new(|cx| pages::people::PeoplePage::new(pcx, cx))),
            Page::Agents => View::Agents(cx.new(|cx| pages::agents::AgentsPage::new(pcx, cx))),
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
            Some(View::People(v)) => v.clone().into_any_element(),
            Some(View::Agents(v)) => v.clone().into_any_element(),
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
            Some(View::Agents(v)) => v.update(cx, |p, cx| p.focus_search(window, cx)),
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
            Some(View::Agents(v)) => v.update(cx, |p, cx| p.new_task(None, cx)),
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
            // Escape clears the Agents search; otherwise it passes on.
            Some(View::Agents(v)) => {
                if !v.update(cx, |p, cx| p.escape(window, cx)) {
                    cx.propagate();
                }
            }
            // Escape closes Settings → Agents' open editor; else passes on.
            Some(View::Settings(v)) => {
                if !v.update(cx, |p, cx| p.escape(cx)) {
                    cx.propagate();
                }
            }
            // Escape stops a running Ask question; otherwise it passes on.
            Some(View::Ask(v)) => {
                if !v.update(cx, |p, cx| p.escape(cx)) {
                    cx.propagate();
                }
            }
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
        let order = Page::visible_order(self.people_enabled(), self.agents_enabled());
        let i = order.iter().position(|p| *p == self.page).unwrap_or(0) as i32;
        let n = order.len() as i32;
        let next = order[((i + delta).rem_euclid(n)) as usize];
        self.navigate(next, None, cx);
    }

    // ---------------------------------------------------------------- views ---

    fn render_sidebar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let people = self.people_enabled();
        let agents = self.agents_enabled();
        let nav_item = |page: Page, cx: &mut Context<Self>| {
            let selected = self.page == page;
            let (sz, lh) = text::body();
            let badge = match page {
                Page::Prompts if self.pending_prompts > 0 => {
                    Some(ui::badge(t, self.pending_prompts))
                }
                // Needs input only (running never badges).
                Page::Agents if self.agents_attention > 0 => {
                    Some(ui::badge(t, self.agents_attention))
                }
                _ => None,
            };
            // The row's accessible name ("Agents, 2 needs input"); GPUI
            // has no accessibility tree yet, so it is carried as the row's
            // selector for now.
            let name = (page == Page::Agents)
                .then(|| crate::controllers::agents_attention::nav_label(self.agents_attention));
            // Selected: a soft rounded fill, heavier label, accent icon.
            ui::row_button(t, ("nav", page as u64), selected)
                .when_some(name, |d, name| {
                    d.debug_selector(move || format!("nav-label:{name}"))
                })
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
                // People follows Meetings, only while it is turned on.
                if *page == Page::Meetings && people {
                    groups = groups.child(nav_item(Page::People, cx));
                }
                // Agents follows Prompts, only while the service has it on.
                if *page == Page::Prompts && agents {
                    groups = groups.child(nav_item(Page::Agents, cx));
                }
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
        let retry = self.cx.services.retry.clone();
        let engine = self.engine.take();
        cx.spawn(async move |this, cx| {
            let (jobs_stopped, engine_outcome, engine) = cx
                .background_executor()
                .spawn(async move {
                    // Downloads are cancelled; deletions and persistent writes
                    // (keys, provider, setup, settings) are waited for.
                    downloads.cancel_all();
                    if let Some(python) = retry.python() {
                        python.stop();
                    }
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
pub(crate) fn pick_font(cx: &App) -> SharedString {
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
                        // Regaining it re-checks speaker readiness on Settings.
                        cx.observe_window_activation(window, |shell, window, cx| {
                            shell.window_activation_changed(window.is_window_active(), cx);
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
            // A window-manager close reaches this from inside the platform's
            // event handler (X11 still holds its client state), so the quit
            // runs on the next turn of the event loop.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    lf_ui::shutdown::wait_for_pending_writes(Duration::from_secs(5));
                    cx.spawn(async |cx| cx.update(|cx| cx.quit())).detach();
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
pub(crate) mod focus_tests {
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
    pub(crate) fn open_with(
        page: Page,
        prepare: impl FnOnce(&crate::data::DataRoot),
        cx: &mut TestAppContext,
    ) -> (Entity<Shell>, &mut VisualTestContext, tempfile::TempDir) {
        open_launch(page, prepare, false, cx)
    }

    /// [`open_with`]; `agents_request` is a cold-start `--page agents`.
    fn open_launch(
        page: Page,
        prepare: impl FnOnce(&crate::data::DataRoot),
        agents_request: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<Shell>, &mut VisualTestContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let launched = crate::launch::sandbox(
            &dir.path().join("sandbox"),
            false,
            Some(Route::Page(page).into()),
        )
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
            fixture: None,
            agents_request,
            agent_session: None,
            agents_settings: false,
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
    fn ask_page_fills_the_main_pane(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open(Page::Ask, cx);
        for (w, h) in [(1280., 850.), (960., 640.), (1280., 850.)] {
            cx.simulate_resize(gpui::size(gpui::px(w), gpui::px(h)));
            cx.run_until_parked();
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            // Measure the elements themselves, not their wrappers: a block
            // wrapper is full width even when the child inside shrank. The
            // main pane is the window minus the 240 px sidebar, less 2 x 32 px
            // page padding, capped at `size::COLUMN` (760 px).
            let column = (w - 240. - 64.).min(crate::theme::size::COLUMN);
            let banner = cx.debug_bounds("ask-banner").expect("banner").size.width;
            assert_eq!(banner, gpui::px(column), "banner at {w}x{h}");
            let convo = cx.debug_bounds("ask-convo").expect("convo").size.width;
            assert_eq!(convo, gpui::px(column), "conversation column at {w}x{h}");
            // Beside the 96 px Send/Stop slot and an `s4` gap.
            let input = cx
                .debug_bounds("text-input-Ask about your dictations and work prompts")
                .expect("question box")
                .size
                .width;
            assert_eq!(
                input,
                gpui::px(column - 96.) - crate::theme::space::s4(),
                "question box is {input:?} wide at {w}x{h}"
            );
        }

        // Long conversations must overflow the scroll viewport vertically so
        // GPUI computes a positive scroll range instead of shrinking the list.
        let ask = shell.read_with(cx, |shell, _| match shell.find(Page::Ask) {
            Some(View::Ask(view)) => view.clone(),
            _ => panic!("Ask isn't open"),
        });
        cx.update(|_, cx| {
            ask.update(cx, |page, cx| {
                for i in 0..12 {
                    page.push_turn(
                        crate::controllers::ask::AgentTurn {
                            question: format!("Question {i}"),
                            answer: format!("Answer {i} with enough text to take up vertical space in the conversation."),
                            ..Default::default()
                        },
                        cx,
                    );
                }
            });
        });
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let scroll = cx
            .debug_bounds("ask-scroll")
            .expect("ask-scroll")
            .size
            .height;
        let content = cx
            .debug_bounds("ask-scroll-content")
            .expect("ask-scroll-content")
            .size
            .height;
        let convo = cx.debug_bounds("ask-convo").expect("ask-convo").size.height;
        assert!(
            content >= convo && content > scroll,
            "direct scroll child ({content:?}) must wrap convo ({convo:?}) and exceed scroll viewport ({scroll:?})"
        );
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

    /// A second launch hands `request` to the open window, which routes it.
    fn hand_off(
        shell: &Entity<Shell>,
        dir: &tempfile::TempDir,
        request: crate::controllers::instance::Request,
        cx: &mut VisualTestContext,
    ) {
        let launched =
            crate::launch::sandbox(&dir.path().join("sandbox"), false, Some(request)).unwrap();
        assert!(matches!(launched, crate::launch::Launched::HandedOff));
        cx.update(|window, cx| shell.update(cx, |shell, cx| shell.poll_instance(window, cx)));
        cx.run_until_parked();
    }

    fn toast(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<String> {
        shell.read_with(cx, |shell, _| {
            shell.toast.as_ref().map(|t| t.message.to_string())
        })
    }

    #[gpui::test]
    fn an_unknown_page_handed_off_opens_home_with_its_notice(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::History, cx);
        let request = crate::controllers::instance::Request {
            route: Route::Page(Page::Home),
            unknown_page: Some("NoSuchPage".into()),
        };
        hand_off(&shell, &dir, request, cx);
        assert_eq!(page(&shell, cx), Page::Home);
        assert_eq!(
            toast(&shell, cx).as_deref(),
            Some("There's no page called “NoSuchPage”, so Home is shown.")
        );
    }

    #[gpui::test]
    fn a_known_page_handed_off_opens_without_a_notice(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::Home, cx);
        hand_off(&shell, &dir, Route::Page(Page::History).into(), cx);
        assert_eq!(page(&shell, cx), Page::History);
        assert_eq!(toast(&shell, cx), None);
    }

    fn people_request() -> crate::controllers::instance::Request {
        crate::controllers::instance::Request {
            route: Route::Page(Page::Home),
            unknown_page: Some("people".into()),
        }
    }

    fn set_people_flags(root: &crate::data::DataRoot, on: bool) {
        let mut config = lf_core::config::load_from(&root.config_path).unwrap();
        config.set_field("speaker_diarization", serde_json::json!(on));
        config.set_field("speaker_recognition", serde_json::json!(on));
        lf_core::config::save_config(&config, Some(&root.config_path)).unwrap();
    }

    fn has_people_view(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> bool {
        shell.read_with(cx, |shell, _| shell.find(Page::People).is_some())
    }

    /// Flags off (the default sandbox): no People anywhere; `people` is an
    /// unknown page like any other, and direct navigation lands on Home.
    #[gpui::test]
    fn people_is_hidden_and_unroutable_while_off(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::History, cx);
        assert!(!shell.read_with(cx, |shell, _| shell.people_enabled()));
        hand_off(&shell, &dir, people_request(), cx);
        assert_eq!(page(&shell, cx), Page::Home);
        assert_eq!(
            toast(&shell, cx).as_deref(),
            Some("There's no page called “people”, so Home is shown.")
        );
        shell.update(cx, |shell, cx| shell.navigate(Page::People, None, cx));
        assert_eq!(page(&shell, cx), Page::Home);
        assert!(!has_people_view(&shell, cx));
        assert_eq!(Page::visible_order(false, false), Page::sidebar_order());
    }

    /// Both flags on: People is routable (no notice); turning the gate off
    /// while it is open goes Home and drops the page.
    #[gpui::test]
    fn people_follows_its_gate(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open_with(Page::Home, |root| set_people_flags(root, true), cx);
        assert!(shell.read_with(cx, |shell, _| shell.people_enabled()));
        hand_off(&shell, &dir, people_request(), cx);
        assert_eq!(page(&shell, cx), Page::People);
        assert_eq!(toast(&shell, cx), None);
        assert!(has_people_view(&shell, cx));

        let root = shell.read_with(cx, |shell, _| shell.cx.services.root.clone());
        set_people_flags(&root, false);
        shell.update(cx, |shell, cx| {
            shell.cx.services.reload_config();
            shell.refresh_views(cx);
        });
        cx.run_until_parked();
        assert_eq!(page(&shell, cx), Page::Home);
        assert!(!has_people_view(&shell, cx), "the list goes with the gate");
    }

    fn agents_request() -> crate::controllers::instance::Request {
        crate::controllers::instance::Request {
            route: Route::Page(Page::Home),
            unknown_page: Some("agents".into()),
        }
    }

    fn agents_fake(
        shell: &Entity<Shell>,
        cx: &mut VisualTestContext,
    ) -> std::sync::Arc<crate::sandbox::agents::FakeAgents> {
        shell.read_with(cx, |shell, _| {
            shell
                .cx
                .services
                .remote
                .sandbox_agents()
                .expect("sandbox agents")
        })
    }

    /// Turn the fake service's `agents.enabled` on or off, as a real
    /// service's status would report it on the next probe.
    fn set_agents(shell: &Entity<Shell>, on: bool, cx: &mut VisualTestContext) {
        agents_fake(shell, cx).set_enabled(on);
        shell.update(cx, |shell, cx| shell.refresh_views(cx));
        cx.run_until_parked();
    }

    fn agents_view(
        shell: &Entity<Shell>,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<pages::agents::AgentsPage>> {
        shell.read_with(cx, |shell, _| match shell.find(Page::Agents) {
            Some(View::Agents(view)) => Some(view.clone()),
            _ => None,
        })
    }

    /// Opens Agents with the fake service on and the list loaded.
    fn open_agents(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Shell>,
        Entity<pages::agents::AgentsPage>,
        &mut VisualTestContext,
        tempfile::TempDir,
    ) {
        let (shell, cx, dir) = open(Page::Home, cx);
        set_agents(&shell, true, cx);
        shell.update(cx, |shell, cx| shell.navigate(Page::Agents, None, cx));
        cx.run_until_parked();
        let view = agents_view(&shell, cx).expect("Agents is open");
        (shell, view, cx, dir)
    }

    /// Off (the default sandbox, like a service without the feature): no
    /// Agents anywhere; `agents` is an unknown page like any other.
    #[gpui::test]
    fn agents_is_hidden_and_unroutable_while_off(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::History, cx);
        assert!(!shell.read_with(cx, |shell, _| shell.agents_enabled()));
        hand_off(&shell, &dir, agents_request(), cx);
        assert_eq!(page(&shell, cx), Page::Home);
        assert_eq!(
            toast(&shell, cx).as_deref(),
            Some("There's no page called “agents”, so Home is shown.")
        );
        shell.update(cx, |shell, cx| shell.navigate(Page::Agents, None, cx));
        assert_eq!(page(&shell, cx), Page::Home);
        assert!(agents_view(&shell, cx).is_none());
    }

    /// On: routable after Prompts; Ctrl+1..0 keep their ten destinations;
    /// turning the feature off while the page is open goes Home and drops it.
    #[gpui::test]
    fn agents_follows_its_gate_and_leaves_the_shortcuts_alone(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::Home, cx);
        set_agents(&shell, true, cx);
        assert!(shell.read_with(cx, |shell, _| shell.agents_enabled()));
        hand_off(&shell, &dir, agents_request(), cx);
        assert_eq!(page(&shell, cx), Page::Agents);
        assert_eq!(toast(&shell, cx), None);
        for (i, expected) in Page::sidebar_order().into_iter().enumerate() {
            let key = (i + 1) % 10;
            cx.simulate_keystrokes(&format!("secondary-{key}"));
            assert_eq!(page(&shell, cx), expected, "secondary-{key}");
        }
        shell.update(cx, |shell, cx| shell.navigate(Page::Agents, None, cx));
        assert_eq!(page(&shell, cx), Page::Agents);
        set_agents(&shell, false, cx);
        assert_eq!(page(&shell, cx), Page::Home);
        assert!(
            agents_view(&shell, cx).is_none(),
            "the list goes with the gate"
        );
    }

    #[gpui::test]
    fn agents_keys_move_the_selection_search_and_clear(cx: &mut TestAppContext) {
        let (shell, agents, cx, _dir) = open_agents(cx);
        let list = agents.read_with(cx, |p, _| p.list_focus());
        cx.update(|window, _| window.focus(&list));
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        let first = agents.read_with(cx, |p, _| p.selected());
        assert_eq!(first.as_deref(), Some("a1f3a2c80001"), "Needs you is first");
        cx.simulate_keystrokes("down");
        assert_eq!(
            agents.read_with(cx, |p, _| p.selected()).as_deref(),
            Some("b41b7e000002")
        );
        cx.simulate_keystrokes("up");
        assert_eq!(agents.read_with(cx, |p, _| p.selected()), first);
        // The detail read brings the task text.
        cx.run_until_parked();
        assert!(agents.read_with(cx, |p, _| p
            .store()
            .session("a1f3a2c80001")
            .unwrap()
            .prompt
            .is_some()));

        cx.simulate_keystrokes("secondary-k");
        assert!(in_shell(&shell, cx));
        assert!(!focused(&list, cx), "search took focus");
        cx.simulate_input("billing");
        let query = |cx: &mut VisualTestContext| agents.read_with(cx, |p, cx| p.search_text(cx));
        assert_eq!(query(cx), "billing");
        cx.simulate_keystrokes("escape");
        assert_eq!(query(cx), "", "Esc clears the search");
        assert!(focused(&list, cx), "and returns to the list");
        assert_eq!(page(&shell, cx), Page::Agents);
    }

    #[gpui::test]
    fn agents_empty_states_follow_providers_and_sessions(cx: &mut TestAppContext) {
        let (shell, agents, cx, _dir) = open_agents(cx);
        assert_eq!(agents.read_with(cx, |p, _| p.empty_heading()), None);
        let fake = agents_fake(&shell, cx);
        fake.clear(true);
        agents.update(cx, |p, cx| p.refresh(cx));
        cx.run_until_parked();
        assert_eq!(
            agents.read_with(cx, |p, _| p.empty_heading()),
            Some(crate::controllers::agents::NO_SESSIONS_HEADING)
        );
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("agents-empty").is_some(), "no split");
        fake.clear(false);
        agents.update(cx, |p, cx| p.refresh(cx));
        cx.run_until_parked();
        assert_eq!(
            agents.read_with(cx, |p, _| p.empty_heading()),
            Some(crate::controllers::agents::NO_PROVIDER_HEADING)
        );
    }

    fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} isn't on screen"))
    }

    fn assert_inside(
        inner: gpui::Bounds<gpui::Pixels>,
        outer: gpui::Bounds<gpui::Pixels>,
        what: &str,
    ) {
        assert!(
            inner.left() >= outer.left()
                && inner.top() >= outer.top()
                && inner.right() <= outer.right()
                && inner.bottom() <= outer.bottom(),
            "{what}: {inner:?} isn't inside {outer:?}"
        );
    }

    /// The split at the design size and the minimum window, in both
    /// palettes: a 340 px list beside the detail, rows inside the list.
    #[gpui::test]
    fn agents_page_lays_out_at_both_sizes_in_both_palettes(cx: &mut TestAppContext) {
        let (_shell, agents, cx, _dir) = open_agents(cx);
        agents.update(cx, |p, cx| p.select_for_test("a1f3a2c80001", cx));
        cx.run_until_parked();
        for dark in [false, true] {
            cx.update(|_, cx| cx.set_global(if dark { Theme::dark() } else { Theme::light() }));
            for (w, h) in [(1200., 760.), (960., 640.)] {
                cx.simulate_resize(size(px(w), px(h)));
                cx.run_until_parked();
                cx.update(|window, _| window.refresh());
                cx.run_until_parked();
                let list = bounds(cx, "agents-list");
                let detail = bounds(cx, "agents-detail");
                // The 340 px pane less its 1 px hairline.
                assert_eq!(list.size.width, px(crate::theme::size::LIST_PANE - 1.));
                assert_eq!(list.left(), px(crate::theme::size::SIDEBAR));
                assert!(detail.left() >= list.right(), "detail beside the list");
                assert!(detail.right() <= px(w), "detail fits {w}x{h}");
                let row = bounds(cx, "agents-row-a1f3a2c80001");
                assert_inside(row, list, "first row");
                assert_eq!(row.size.height, px(56.));
                let banner = bounds(cx, "agents-state-banner");
                assert_inside(banner, detail, "state banner");
                let receipt = bounds(cx, "agents-receipt");
                assert_inside(receipt, detail, "receipt");
                // Section headings sit on one line across the pane, and the
                // task card holds its receipt (no letter-a-line heading, no
                // clipped card); taller content scrolls instead.
                let card = bounds(cx, "agents-task-card");
                assert_inside(receipt, card, "receipt in the task card");
                let column = bounds(cx, "agents-detail-column");
                for name in ["section-heading:Task", "section-heading:Activity"] {
                    let heading = bounds(cx, name);
                    assert!(heading.size.height < px(40.), "{name}: {heading:?}");
                    assert_eq!(heading.size.width, card.size.width, "{name}");
                }
                assert_eq!(bounds(cx, "agents-activity").size.width, card.size.width);
                assert!(card.size.width > px(300.), "{card:?}");
                if h < 700. {
                    let scroll = bounds(cx, "agents-detail-scroll");
                    assert!(column.size.height > scroll.size.height, "it scrolls");
                }
            }
        }
    }

    /// `--sandbox-fixture agents-page|agents-states`: an agents fixture
    /// sandbox has the fake service on (its clock pinned); each scene's one
    /// action lands its end state.
    #[gpui::test]
    fn agents_fixture_scenes_reach_their_end_state(cx: &mut TestAppContext) {
        use crate::controllers::agents::{AgentsBackend, UiState};
        use crate::sandbox::Fixture;
        for fixture in [
            Fixture::AgentsPage,
            Fixture::AgentsStates,
            Fixture::AgentsDetail,
        ] {
            let (shell, cx, _dir) = open_with(
                Page::Home,
                |root| crate::sandbox::add_agents_fixture(root).unwrap(),
                cx,
            );
            assert!(shell.read_with(cx, |shell, _| shell.agents_enabled()));
            let fake = agents_fake(&shell, cx);
            assert_ne!(
                fake.now(),
                crate::sandbox::agents::FIXTURE_NOW,
                "shared, wall clock"
            );
            // What a `--sandbox-fixture` launch does first (Shell::new).
            fake.use_fixture();
            assert_eq!(fake.now(), crate::sandbox::agents::FIXTURE_NOW);
            assert_eq!(fixture.page(), Page::Agents);
            shell.update(cx, |shell, cx| shell.run_fixture(fixture, cx));
            cx.run_until_parked();
            assert_eq!(page(&shell, cx), Page::Agents);
            let agents = agents_view(&shell, cx).unwrap();
            let (rows, counts) =
                agents.read_with(cx, |p, _| (p.store().rows(fake.now()), p.store().counts));
            match fixture {
                Fixture::AgentsPage => {
                    let selected = agents.read_with(cx, |p, _| p.selected());
                    assert_eq!(selected.as_deref(), Some("a1f3a2c80001"));
                    assert_eq!(
                        crate::controllers::agents::support_line(&rows, counts),
                        "2 running · 1 needs you"
                    );
                }
                Fixture::AgentsDetail => {
                    let selected = agents.read_with(cx, |p, _| p.selected());
                    assert_eq!(
                        selected.as_deref(),
                        Some(crate::sandbox::agents::CLAUDE_RUNNING),
                        "a running session that takes replies"
                    );
                    assert!(agents.read_with(cx, |p, _| p.activity().unwrap().len()) > 0);
                }
                _ => {
                    for state in UiState::ALL {
                        assert!(rows.iter().any(|r| r.state == state), "{state:?}");
                    }
                }
            }
        }
    }

    /// Filters and search narrow the list; an empty filter says so.
    #[gpui::test]
    fn agents_filters_narrow_the_list(cx: &mut TestAppContext) {
        let (_shell, agents, cx, _dir) = open_agents(cx);
        agents.update(cx, |p, cx| {
            p.set_filter(crate::controllers::agents::Filter::Done, cx)
        });
        cx.run_until_parked();
        // (Debug bounds outlive a frame, so absence is read from the page.)
        let shown = |cx: &mut VisualTestContext| agents.read_with(cx, |p, cx| p.visible_ids(cx));
        assert_eq!(shown(cx), ["d2c90de00004", "e5e61ab00005", "f9b3f4200006"]);
        agents.update(cx, |p, cx| {
            p.set_filter(crate::controllers::agents::Filter::NeedsYou, cx)
        });
        assert_eq!(shown(cx), ["a1f3a2c80001"]);
        agents.update(cx, |p, cx| {
            p.set_filter(crate::controllers::agents::Filter::Active, cx)
        });
        assert_eq!(shown(cx).len(), 5, "needs you and running groups");
        // Search narrows within the filter; nothing matching says so.
        cx.update(|window, cx| agents.update(cx, |p, cx| p.focus_search(window, cx)));
        cx.simulate_input("codex");
        assert_eq!(shown(cx), ["c77ad1900003", "a1d08c600007"]);
        cx.simulate_input(" zebra");
        assert!(shown(cx).is_empty());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("button:Clear search").is_some());
    }

    fn probe(shell: &Entity<Shell>, cx: &mut VisualTestContext) {
        shell.update(cx, |shell, cx| shell.probe_agents(cx));
        cx.run_until_parked();
    }

    /// One poll-loop turn: the loop's pause elapses and its read lands.
    fn tick(cx: &mut VisualTestContext, secs: u64) {
        cx.executor().advance_clock(Duration::from_secs(secs));
        cx.run_until_parked();
    }

    /// Review H1: a transient status failure (nothing answering, a 401/5xx,
    /// a malformed body) keeps Agents open with its own banner; only "no
    /// agents API" (404) or a parsed "off" closes it and goes Home.
    #[gpui::test]
    fn a_failing_status_probe_keeps_agents_open(cx: &mut TestAppContext) {
        use crate::controllers::agents::AgentsError;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        for error in [
            AgentsError::Refused("the service didn't accept LocalFlow's API token.".into()),
            AgentsError::Refused("Invalid response from LocalFlow server".into()),
            AgentsError::Offline,
        ] {
            fake.fail(Some(error.clone()));
            probe(&shell, cx);
            assert_eq!(page(&shell, cx), Page::Agents, "{error:?}");
            assert!(shell.read_with(cx, |shell, _| shell.agents_enabled()));
            assert!(agents_view(&shell, cx).is_some(), "{error:?}: view kept");
        }
        // The page says what's wrong itself, and keeps its rows.
        tick(cx, 2);
        let (error, rows) =
            agents.read_with(cx, |p, _| (p.store().error.clone(), p.store().is_empty()));
        assert_eq!(error, Some(AgentsError::Offline));
        assert!(!rows, "rows kept");
        fake.fail(Some(AgentsError::Absent));
        probe(&shell, cx);
        assert_eq!(page(&shell, cx), Page::Home);
        assert!(
            agents_view(&shell, cx).is_none(),
            "the list goes with the gate"
        );
    }

    /// Review L4: probe answers land out of order; only the newest counts.
    #[gpui::test]
    fn a_late_probe_answer_never_overwrites_a_newer_one(cx: &mut TestAppContext) {
        use crate::controllers::agents::Status;
        let (shell, cx, _dir) = open(Page::Home, cx);
        let status = |enabled| {
            Ok(Status {
                api_version: 1,
                enabled,
                ..Default::default()
            })
        };
        shell.update(cx, |shell, cx| {
            shell.agents_probe += 2;
            let newest = shell.agents_probe;
            shell.agents_answered(newest, status(true), cx);
            shell.agents_answered(newest - 1, status(false), cx);
            assert!(shell.agents_enabled(), "the older \"off\" is dropped");
        });
    }

    /// Review L5: a request for `agents` before the first probe answer
    /// waits for it: Agents when on, Home with the notice when off.
    #[gpui::test]
    fn an_agents_request_waits_for_the_first_probe(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::History, cx);
        let fake = agents_fake(&shell, cx);
        fake.set_enabled(true);
        shell.update(cx, |shell, _| shell.agents_known = false);
        hand_off(&shell, &dir, agents_request(), cx);
        assert_eq!(page(&shell, cx), Page::History, "waits");
        assert_eq!(toast(&shell, cx), None, "no premature notice");
        probe(&shell, cx);
        assert_eq!(page(&shell, cx), Page::Agents);
        assert_eq!(toast(&shell, cx), None);

        fake.set_enabled(false);
        probe(&shell, cx);
        assert_eq!(page(&shell, cx), Page::Home);
        shell.update(cx, |shell, _| shell.agents_known = false);
        hand_off(&shell, &dir, agents_request(), cx);
        assert_eq!(toast(&shell, cx), None);
        probe(&shell, cx);
        assert_eq!(page(&shell, cx), Page::Home);
        assert_eq!(
            toast(&shell, cx).as_deref(),
            Some("There's no page called “agents”, so Home is shown.")
        );
    }

    /// Review L5, cold start: `--page agents` opens Agents when the service
    /// has it on, else Home with the notice.
    #[gpui::test]
    fn a_cold_start_agents_request_opens_once_known(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open_launch(
            Page::Home,
            |root| crate::sandbox::add_agents_fixture(root).unwrap(),
            true,
            cx,
        );
        cx.run_until_parked();
        assert_eq!(page(&shell, cx), Page::Agents);
        assert_eq!(toast(&shell, cx), None);
    }

    #[gpui::test]
    fn a_cold_start_agents_request_while_off_shows_the_notice(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open_launch(Page::Home, |_| {}, true, cx);
        cx.run_until_parked();
        assert_eq!(page(&shell, cx), Page::Home);
        assert_eq!(
            toast(&shell, cx).as_deref(),
            Some("There's no page called “agents”, so Home is shown.")
        );
    }

    /// The selected row as the page draws it.
    fn agents_row(
        agents: &Entity<pages::agents::AgentsPage>,
        id: &str,
        cx: &mut VisualTestContext,
    ) -> crate::controllers::agents::Row {
        agents.read_with(cx, |p, _| {
            p.store()
                .rows(crate::controllers::agents::now())
                .into_iter()
                .find(|r| r.session.id == id)
                .unwrap_or_else(|| panic!("{id} is listed"))
        })
    }

    fn select(agents: &Entity<pages::agents::AgentsPage>, id: &str, cx: &mut VisualTestContext) {
        agents.update(cx, |p, cx| p.select_for_test(id, cx));
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    /// The open confirmation's title, and its confirm action.
    fn confirm_of(
        shell: &Entity<Shell>,
        cx: &mut VisualTestContext,
    ) -> Option<(String, ui::Action)> {
        shell.read_with(cx, |shell, _| {
            shell
                .confirm
                .as_ref()
                .map(|c| (c.title.to_string(), c.on_confirm.clone()))
        })
    }

    /// Capability-gated controls (G4): present only with the capability,
    /// and nothing is ever called without it.
    #[gpui::test]
    fn agents_actions_follow_capabilities(cx: &mut TestAppContext) {
        use crate::controllers::agents::actions;
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        select(&agents, seed::CLAUDE_RUNNING, cx);
        assert!(cx.debug_bounds("button:Stop").is_some());
        assert!(cx.debug_bounds("button:Run again").is_some());
        assert!(
            cx.debug_bounds("agents-reply").is_some(),
            "Claude takes messages"
        );
        let claude = actions(&agents_row(&agents, seed::CLAUDE_RUNNING, cx));
        assert_eq!(
            (claude.open, claude.stop, claude.reply),
            (None, Some(true), Some(true))
        );
        select(&agents, seed::NEEDS_INPUT, cx);
        assert!(cx.debug_bounds("button:Open in Devin").is_some());
        let devin = actions(&agents_row(&agents, seed::NEEDS_INPUT, cx));
        assert_eq!(
            (devin.stop, devin.reply),
            (None, None),
            "no cancel, no messages"
        );
        // Asking anyway does nothing and calls nothing.
        agents.update(cx, |p, cx| p.ask_stop_for_test(cx));
        agents.update(cx, |p, cx| p.ask_remove_for_test(cx));
        cx.run_until_parked();
        assert!(confirm_of(&shell, cx).is_none());
        let fake = agents_fake(&shell, cx);
        let calls = fake.calls();
        assert_eq!((calls.cancel, calls.remove, calls.message), (0, 0, 0));
        // The question the agent paused on is quoted (from its last output).
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("agents-state-banner").is_some());
        let question = agents.read_with(cx, |p, _| {
            p.activity().and_then(|a| a.question().map(str::to_owned))
        });
        assert_eq!(
            question.as_deref(),
            Some("Should the v1 handler stay for one release, or be removed now?")
        );
    }

    #[gpui::test]
    fn stop_asks_first_then_cancels_once(cx: &mut TestAppContext) {
        use crate::controllers::agents::{RuntimeState, UiState};
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        select(&agents, seed::CLAUDE_RUNNING, cx);
        let fake = agents_fake(&shell, cx);
        agents.update(cx, |p, cx| p.ask_stop_for_test(cx));
        cx.run_until_parked();
        let (title, confirm) = confirm_of(&shell, cx).expect("a confirmation");
        assert_eq!(title, "Stop this session?");
        assert_eq!(fake.calls().cancel, 0, "nothing until confirmed");
        let cancel = shell.read_with(cx, |shell, _| shell.cancel_focus.clone());
        lose_focus(cx);
        assert!(focused(&cancel, cx), "focus starts on Cancel");
        cx.update(|window, cx| confirm(window, cx));
        cx.run_until_parked();
        assert_eq!(fake.calls().cancel, 1);
        let row = agents_row(&agents, seed::CLAUDE_RUNNING, cx);
        assert_eq!(row.session.runtime, RuntimeState::Cancelling);
        assert_eq!(row.state, UiState::Stopping);
        assert_eq!(
            crate::controllers::agents::actions(&row).stop,
            Some(false),
            "drawn, busy, not again"
        );
        assert_eq!(
            toast(&shell, cx).as_deref(),
            Some("Stopping the session…"),
            "not \"stopped\" while it is still cancelling"
        );
    }

    #[gpui::test]
    fn remove_is_final_only_asks_first_and_clears_the_detail(cx: &mut TestAppContext) {
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        select(&agents, seed::CLAUDE_RUNNING, cx);
        agents.update(cx, |p, cx| p.ask_remove_for_test(cx));
        cx.run_until_parked();
        assert!(
            confirm_of(&shell, cx).is_none(),
            "live sessions can't be removed"
        );
        select(&agents, seed::CLAUDE_DONE, cx);
        agents.update(cx, |p, cx| p.ask_remove_for_test(cx));
        cx.run_until_parked();
        let (title, confirm) = confirm_of(&shell, cx).expect("a confirmation");
        assert_eq!(title, "Remove from LocalFlow?");
        let body = shell.read_with(cx, |s, _| s.confirm.as_ref().unwrap().body.to_string());
        assert!(body.contains("from this list only"), "{body}");
        cx.update(|window, cx| confirm(window, cx));
        cx.run_until_parked();
        assert_eq!(fake.calls().remove, 1);
        assert!(agents.read_with(cx, |p, _| p.store().session(seed::CLAUDE_DONE).is_none()));
        assert_eq!(agents.read_with(cx, |p, _| p.selected()), None);
        assert_eq!(toast(&shell, cx).as_deref(), Some("Removed from LocalFlow"));
    }

    #[gpui::test]
    fn a_reply_sends_only_its_text_and_a_failure_keeps_it(cx: &mut TestAppContext) {
        use crate::controllers::agents::{AgentsError, Stream};
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        select(&agents, seed::CLAUDE_RUNNING, cx);
        let reply = agents.read_with(cx, |p, _| p.reply_input());
        cx.update(|window, cx| reply.read(cx).focus(window));
        fake.fail_writes(Some(AgentsError::rejected(
            502,
            "rate_limited",
            "slow down",
        )));
        cx.simulate_input("Use the fake clock");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            agents.read_with(cx, |p, _| p.reply_error()).as_deref(),
            Some("Couldn't send your reply. It's still in the field. Slow down.")
        );
        let text = |cx: &mut VisualTestContext| reply.read_with(cx, |r, _| r.text().to_string());
        assert_eq!(text(cx), "Use the fake clock", "kept");
        fake.fail_writes(None);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(fake.calls().message, 2);
        assert_eq!(text(cx), "", "sent: the field is cleared");
        assert!(
            cx.update(|window, cx| reply.read(cx).is_focused(window)),
            "focus kept"
        );
        assert_eq!(
            fake.received().last().unwrap(),
            &serde_json::json!({"text": "Use the fake clock"})
        );
        let mine = agents.read_with(cx, |p, _| {
            p.activity()
                .unwrap()
                .newest_first()
                .next()
                .map(|e| (e.stream, e.text.clone()))
        });
        assert_eq!(mine, Some((Stream::User, "Use the fake clock".to_string())));
        assert_eq!(toast(&shell, cx).as_deref(), Some("Reply sent to Claude"));
        // A final session has no reply field.
        let done = agents_row(&agents, seed::CLAUDE_DONE, cx);
        assert_eq!(crate::controllers::agents::actions(&done).reply, None);
    }

    /// Activity is masked, read once and then only when the log grows,
    /// and never while the page is hidden (no hidden polling).
    #[gpui::test]
    fn activity_is_masked_and_read_only_when_it_grows(cx: &mut TestAppContext) {
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        select(&agents, seed::CLAUDE_RUNNING, cx);
        assert!(cx.debug_bounds("agents-activity").is_some());
        let lines = |cx: &mut VisualTestContext| {
            agents.read_with(cx, |p, _| {
                p.activity()
                    .unwrap()
                    .newest_first()
                    .map(|e| e.text.clone())
                    .collect::<Vec<_>>()
            })
        };
        let first = lines(cx);
        assert_eq!(first.len(), 5);
        assert!(
            first.iter().all(|l| !l.contains("abcdefghijklmnop")),
            "masked"
        );
        assert!(
            first[0].contains("https://ci.example.com/runs/4821"),
            "plain text"
        );
        let reads = fake.calls().events;
        tick(cx, 2);
        tick(cx, 2);
        assert_eq!(fake.calls().events, reads, "nothing new, no read");
        fake.emit(
            seed::CLAUDE_RUNNING,
            "Pushed branch fix-retry. key=ghp_zzzzzzzzzzzzzzzzzzzzzz00",
        );
        tick(cx, 2);
        assert_eq!(fake.calls().events, reads + 1, "the log grew: one read");
        let now = lines(cx);
        assert_eq!(now.len(), 6);
        assert!(now[0].starts_with("Pushed branch fix-retry.") && !now[0].contains("zzzzzz"));
        shell.update(cx, |shell, cx| shell.navigate(Page::Home, None, cx));
        fake.emit(seed::CLAUDE_RUNNING, "more");
        tick(cx, 4);
        tick(cx, 4);
        assert_eq!(fake.calls().events, reads + 1, "hidden: nothing is read");
    }

    #[gpui::test]
    fn results_come_from_the_detail_read(cx: &mut TestAppContext) {
        use crate::controllers::agents::{results, Results};
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        // Before the detail: only the summary's preview.
        let row = agents_row(&agents, seed::CLAUDE_DONE, cx);
        assert!(matches!(results(&row), Some(Results::Preview(_))));
        select(&agents, seed::CLAUDE_DONE, cx);
        assert!(cx.debug_bounds("agents-results").is_some());
        let row = agents_row(&agents, seed::CLAUDE_DONE, cx);
        let Some(Results::Text { text, truncated }) = results(&row) else {
            panic!("the full result");
        };
        assert!(text.contains("branch email-dark-tokens") && !truncated);
        // A truncated, credential-bearing result arrives masked with its note.
        fake.set_result(
            seed::CLAUDE_DONE,
            "Done. token=abcdefghijklmnopqrstuv",
            true,
        );
        tick(cx, 2);
        let row = agents_row(&agents, seed::CLAUDE_DONE, cx);
        let Some(Results::Text { text, truncated }) = results(&row) else {
            panic!("a result");
        };
        assert!(truncated && !text.contains("abcdefghijklmnop"), "{text}");
    }

    #[gpui::test]
    fn run_again_opens_the_launcher_with_a_private_prefill(cx: &mut TestAppContext) {
        use crate::controllers::agents_launcher::{launcher_dir, DraftStore};
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        crate::launcher::take_spawned();
        select(&agents, seed::CLAUDE_DONE, cx);
        agents.update(cx, |p, cx| p.run_again_for_test(cx));
        assert_eq!(
            crate::launcher::take_spawned(),
            [crate::launcher::Spawn::Launcher { prefill: true }]
        );
        let root = shell.read_with(cx, |s, _| s.cx.services.root.clone());
        let draft = DraftStore::new(&launcher_dir(&root))
            .load(crate::controllers::agents::now())
            .expect("prefill saved");
        assert!(draft.text.starts_with("Add dark-mode tokens"));
        assert_eq!(draft.provider, Some(("claude".into(), String::new())));
        assert_eq!(draft.workspace.as_deref(), Some("acme/web"));
        // Cmd/Ctrl+N: a new, empty task.
        cx.simulate_keystrokes("secondary-n");
        assert_eq!(
            crate::launcher::take_spawned(),
            [crate::launcher::Spawn::Launcher { prefill: false }]
        );
    }

    /// The launcher's Enter on a session row: `--page agents
    /// --agent-session <id>` hands off as `agents/<id>`.
    #[gpui::test]
    fn the_launcher_opens_agents_on_one_session(cx: &mut TestAppContext) {
        use crate::sandbox::agents as seed;
        let (shell, cx, dir) = open(Page::History, cx);
        set_agents(&shell, true, cx);
        hand_off(
            &shell,
            &dir,
            crate::controllers::instance::Request {
                route: Route::Page(Page::Home),
                unknown_page: Some(format!("agents/{}", seed::CLAUDE_RUNNING)),
            },
            cx,
        );
        assert_eq!(page(&shell, cx), Page::Agents);
        let agents = agents_view(&shell, cx).unwrap();
        assert_eq!(
            agents.read_with(cx, |p, _| p.selected()).as_deref(),
            Some(seed::CLAUDE_RUNNING)
        );
        let reply = agents.read_with(cx, |p, _| p.reply_input());
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.update(|window, cx| reply.read(cx).is_focused(window)),
            "the reply field takes focus"
        );
        // Off: the notice names the page only.
        set_agents(&shell, false, cx);
        hand_off(
            &shell,
            &dir,
            crate::controllers::instance::Request {
                route: Route::Page(Page::Home),
                unknown_page: Some(format!("agents/{}", seed::CLAUDE_RUNNING)),
            },
            cx,
        );
        assert_eq!(
            toast(&shell, cx).as_deref(),
            Some("There's no page called “agents”, so Home is shown.")
        );
    }

    #[gpui::test]
    fn enter_on_a_row_moves_into_the_detail_and_escape_returns(cx: &mut TestAppContext) {
        use crate::sandbox::agents as seed;
        let (_shell, agents, cx, _dir) = open_agents(cx);
        let list = agents.read_with(cx, |p, _| p.list_focus());
        cx.update(|window, _| window.focus(&list));
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(
            agents.read_with(cx, |p, _| p.selected()).as_deref(),
            Some(seed::NEEDS_INPUT)
        );
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        crate::launcher::take_spawned();
        cx.simulate_keystrokes("enter");
        let run_again = agents.read_with(cx, |p, _| p.run_again_focus());
        assert!(focused(&run_again, cx), "no reply field: the first action");
        release(cx, "enter");
        assert!(
            crate::launcher::take_spawned().is_empty(),
            "that Enter's release doesn't click Run again"
        );
        cx.simulate_keystrokes("escape");
        assert!(focused(&list, cx), "Esc returns to the list");
        cx.simulate_keystrokes("down enter");
        cx.run_until_parked();
        let reply = agents.read_with(cx, |p, _| p.reply_input());
        assert!(
            cx.update(|window, cx| reply.read(cx).is_focused(window)),
            "reply field"
        );
    }

    /// A key's release after a frame is drawn (GPUI clicks a focused
    /// element on Enter's or Space's key-up; tests only send key-downs).
    fn release(cx: &mut VisualTestContext, key: &str) {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        cx.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
        });
        cx.run_until_parked();
    }

    fn press(cx: &mut VisualTestContext, key: &str) {
        cx.simulate_keystrokes(key);
        release(cx, key);
    }

    /// Review H4: every action has a key path. Tab reaches Run again and
    /// `…`; Enter opens the menu with focus on its first entry; the arrows
    /// move; Enter on Remove from LocalFlow asks first.
    #[gpui::test]
    fn every_detail_action_has_a_key_path(cx: &mut TestAppContext) {
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        select(&agents, seed::CLAUDE_DONE, cx);
        let run_again = agents.read_with(cx, |p, _| p.run_again_focus());
        cx.update(|window, _| window.focus(&run_again));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        crate::launcher::take_spawned();
        press(cx, "enter");
        assert_eq!(
            crate::launcher::take_spawned(),
            [crate::launcher::Spawn::Launcher { prefill: true }],
            "Run again by Enter"
        );
        cx.simulate_keystrokes("tab");
        let more = agents.read_with(cx, |p, _| p.more_focus());
        assert!(focused(&more, cx), "Tab: `…`");
        press(cx, "enter");
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("agents-menu").is_some(), "open");
        let entries = agents.read_with(cx, |p, _| p.menu_focus());
        assert!(focused(&entries[1], cx), "Copy ID first (no link)");
        cx.simulate_keystrokes("down");
        assert!(focused(&entries[2], cx), "Remove from LocalFlow");
        cx.simulate_keystrokes("up");
        assert!(focused(&entries[1], cx));
        cx.simulate_keystrokes("end");
        press(cx, "enter");
        let (title, _) = confirm_of(&shell, cx).expect("Remove asks first");
        assert_eq!(title, "Remove from LocalFlow?");
        // Esc in the menu closes it and returns to `…`.
        cx.simulate_keystrokes("escape");
        cx.update(|window, _| window.focus(&more));
        press(cx, "enter");
        assert!(focused(&entries[1], cx));
        assert!(agents.read_with(cx, |p, _| p.more_open()));
        cx.simulate_keystrokes("escape");
        assert!(focused(&more, cx), "back on `…`");
        assert!(!agents.read_with(cx, |p, _| p.more_open()), "closed");
    }

    /// Review M1: a session launched after the list was read is selected
    /// once `/changes` brings it; a manual pick in between wins.
    #[gpui::test]
    fn a_session_opened_before_it_is_listed_is_selected_on_arrival(cx: &mut TestAppContext) {
        use crate::controllers::agents::AgentsBackend;
        use crate::controllers::agents_launcher::request;
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        let launch = |cx: &mut VisualTestContext| {
            let claude = agents
                .read_with(cx, |p, _| p.store().provider("claude", "").cloned())
                .unwrap();
            let id = fake
                .launch(&request(None, "A new task", &claude, Some("acme/web")))
                .unwrap()
                .id;
            agents.update(cx, |p, cx| p.open_session(id.clone(), cx));
            id
        };
        let id = launch(cx);
        assert_ne!(agents.read_with(cx, |p, _| p.selected()), Some(id.clone()));
        tick(cx, 2);
        assert_eq!(agents.read_with(cx, |p, _| p.selected()), Some(id));
        // A pick before it arrives: the pick stays, nothing steals focus.
        let id = launch(cx);
        let list = agents.read_with(cx, |p, _| p.list_focus());
        cx.update(|window, _| window.focus(&list));
        agents.update(cx, |p, cx| p.select_by_user_for_test(seed::CLAUDE_DONE, cx));
        tick(cx, 2);
        assert_eq!(
            agents.read_with(cx, |p, _| p.selected()).as_deref(),
            Some(seed::CLAUDE_DONE)
        );
        assert!(agents.read_with(cx, |p, _| p.store().session(&id).is_some()));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(focused(&list, cx), "focus stays in the list");
    }

    /// Review L: the reply field is per session.
    #[gpui::test]
    fn a_reply_draft_stays_with_its_session(cx: &mut TestAppContext) {
        use crate::controllers::agents::AgentsError;
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        select(&agents, seed::CLAUDE_RUNNING, cx);
        let reply = agents.read_with(cx, |p, _| p.reply_input());
        let text = |cx: &mut VisualTestContext| reply.read_with(cx, |r, _| r.text().to_string());
        cx.update(|window, cx| reply.read(cx).focus(window));
        cx.simulate_input("For Claude only");
        select(&agents, seed::CLAUDE_DONE, cx);
        assert_eq!(text(cx), "", "not shown under another session");
        select(&agents, seed::CLAUDE_RUNNING, cx);
        assert_eq!(text(cx), "For Claude only", "back with its session");
        // A send that fails after the selection moved stays with its own
        // session, and the error is a toast.
        fake.fail_writes(Some(AgentsError::rejected(502, "rate_limited", "slow")));
        agents.update(cx, |p, cx| {
            p.send_reply_for_test(cx);
            p.select_for_test(seed::CLAUDE_DONE, cx);
        });
        cx.run_until_parked();
        assert_eq!(fake.calls().message, 1);
        assert_eq!(text(cx), "");
        assert_eq!(agents.read_with(cx, |p, _| p.reply_error()), None);
        assert!(toast(&shell, cx)
            .unwrap()
            .starts_with("Couldn't send your reply"));
        select(&agents, seed::CLAUDE_RUNNING, cx);
        assert_eq!(text(cx), "For Claude only");
    }

    /// Review L: Run again over an unsent launcher task says it replaced it.
    #[gpui::test]
    fn run_again_says_when_it_replaces_an_unsent_task(cx: &mut TestAppContext) {
        use crate::controllers::agents_launcher::{launcher_dir, Draft, DraftStore};
        use crate::sandbox::agents as seed;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let root = shell.read_with(cx, |s, _| s.cx.services.root.clone());
        DraftStore::new(&launcher_dir(&root)).save(&Draft {
            text: "Something half typed".into(),
            provider: None,
            workspace: None,
            saved_at: crate::controllers::agents::now(),
        });
        select(&agents, seed::CLAUDE_DONE, cx);
        agents.update(cx, |p, cx| p.run_again_for_test(cx));
        crate::launcher::take_spawned();
        assert_eq!(
            toast(&shell, cx).as_deref(),
            Some("Replaced the unsent task in the launcher")
        );
    }

    /// Review blocking test 2: the `/changes` loop. Failures keep the rows
    /// (Disconnected after the 20 s grace) and recover; `reset` reloads the
    /// list; the loop stops while hidden and never runs twice.
    #[gpui::test]
    fn the_poll_loop_rides_out_failures_resets_and_hiding(cx: &mut TestAppContext) {
        use crate::controllers::agents::{AgentsBackend, AgentsError, UiState};
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        let state = |cx: &mut VisualTestContext| {
            agents.read_with(cx, |p, _| {
                p.store()
                    .rows(fake.now())
                    .into_iter()
                    .find(|r| r.session.id == "b41b7e000002")
                    .unwrap()
                    .state
            })
        };
        assert_eq!(state(cx), UiState::Running);
        let rows = agents.read_with(cx, |p, cx| p.visible_ids(cx).len());

        fake.fail(Some(AgentsError::Offline));
        let polls = fake.calls().changes;
        tick(cx, 2);
        assert_eq!(fake.calls().changes, polls + 1);
        assert_eq!(state(cx), UiState::Running, "inside the grace period");
        assert_eq!(agents.read_with(cx, |p, cx| p.visible_ids(cx).len()), rows);
        fake.advance(21.0);
        tick(cx, 2);
        assert_eq!(fake.calls().changes, polls + 2);
        assert_eq!(state(cx), UiState::Disconnected);
        assert_eq!(agents.read_with(cx, |p, cx| p.visible_ids(cx).len()), rows);
        // Backing off: the third failure waits 4 s, not 2.
        tick(cx, 2);
        assert_eq!(fake.calls().changes, polls + 2, "backing off");
        fake.fail(None);
        tick(cx, 2);
        assert_eq!(fake.calls().changes, polls + 3);
        assert_eq!(state(cx), UiState::Running, "recovered");
        assert_eq!(agents.read_with(cx, |p, _| p.store().error.clone()), None);

        // `reset: true`: the list is read again.
        let lists = fake.calls().sessions;
        fake.reset_next();
        tick(cx, 2);
        assert!(fake.calls().sessions > lists, "reloaded on reset");
        assert_eq!(state(cx), UiState::Running);

        // Away and straight back (inside one pause): still one loop.
        shell.update(cx, |shell, cx| shell.navigate(Page::Home, None, cx));
        shell.update(cx, |shell, cx| shell.navigate(Page::Agents, None, cx));
        cx.run_until_parked();
        let polls = fake.calls().changes;
        for _ in 0..4 {
            tick(cx, 2);
        }
        assert_eq!(fake.calls().changes, polls + 4, "one read per pause");

        // Hidden: the loop stops.
        shell.update(cx, |shell, cx| shell.navigate(Page::Home, None, cx));
        tick(cx, 2);
        assert!(!agents.read_with(cx, |p, _| p.polling()));
        // The page reads nothing while hidden; the shell's needs-input
        // watcher is the one reader now (Slice C): one read per pause.
        let polls = fake.calls().changes;
        for _ in 0..4 {
            tick(cx, 2);
        }
        assert_eq!(fake.calls().changes, polls + 4, "one reader while hidden");
        assert!(!agents.read_with(cx, |p, _| p.polling()));
        // Back: exactly one loop again.
        shell.update(cx, |shell, cx| shell.navigate(Page::Agents, None, cx));
        cx.run_until_parked();
        tick(cx, 2);
        let polls = fake.calls().changes;
        for _ in 0..3 {
            tick(cx, 2);
        }
        assert_eq!(fake.calls().changes, polls + 3);
    }

    /// Review L3: a 404 from `/changes` (no agents API) ends the loop.
    #[gpui::test]
    fn the_poll_loop_stops_when_the_api_is_absent(cx: &mut TestAppContext) {
        use crate::controllers::agents::AgentsError;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        fake.fail(Some(AgentsError::Absent));
        tick(cx, 2);
        assert!(!agents.read_with(cx, |p, _| p.polling()));
        assert_eq!(
            agents.read_with(cx, |p, _| p.store().error.clone()),
            Some(AgentsError::Absent)
        );
        let polls = fake.calls().changes;
        tick(cx, 10);
        assert_eq!(fake.calls().changes, polls);
    }

    /// Review M4: the service down on the first read shows only its
    /// banner, never "Connect an agent to get started".
    #[gpui::test]
    fn a_service_down_first_read_shows_only_the_banner(cx: &mut TestAppContext) {
        use crate::controllers::agents::AgentsError;
        let (shell, cx, _dir) = open(Page::Home, cx);
        set_agents(&shell, true, cx);
        agents_fake(&shell, cx).fail(Some(AgentsError::Offline));
        shell.update(cx, |shell, cx| shell.navigate(Page::Agents, None, cx));
        cx.run_until_parked();
        let agents = agents_view(&shell, cx).unwrap();
        assert_eq!(agents.read_with(cx, |p, _| p.empty_heading()), None);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(cx.debug_bounds("agents-unread").is_some());
        assert!(cx.debug_bounds("button:Try again").is_some(), "the banner");
        // (Debug bounds outlive a frame, so the empty state's absence is
        // read from the page: `empty_heading` above.)
    }

    /// Review L6: the Task section tells reading, failed and absent apart.
    #[gpui::test]
    fn the_task_text_says_whether_it_is_loading_failed_or_absent(cx: &mut TestAppContext) {
        use crate::controllers::agents::AgentsError;
        let (shell, agents, cx, _dir) = open_agents(cx);
        let fake = agents_fake(&shell, cx);
        agents.update(cx, |p, cx| {
            p.select_for_test("b41b7e000002", cx);
            assert_eq!(p.task_text(), "Reading this task…");
        });
        cx.run_until_parked();
        assert!(agents
            .read_with(cx, |p, _| p.task_text())
            .starts_with("Fix the flaky retry test"));
        fake.fail(Some(AgentsError::Refused("session not found".into())));
        agents.update(cx, |p, cx| p.select_for_test("c77ad1900003", cx));
        cx.run_until_parked();
        assert_eq!(
            agents.read_with(cx, |p, _| p.task_text()),
            "LocalFlow couldn't read this task: session not found"
        );
        fake.fail(Some(AgentsError::Offline));
        agents.update(cx, |p, cx| p.select_for_test("d2c90de00004", cx));
        cx.run_until_parked();
        assert!(agents
            .read_with(cx, |p, _| p.task_text())
            .contains("isn't answering"));
    }

    /// Review M7: arrow keys keep the selected row in view.
    #[gpui::test]
    fn keyboard_selection_scrolls_into_view(cx: &mut TestAppContext) {
        let (shell, agents, cx, _dir) = open_agents(cx);
        agents_fake(&shell, cx).add_history(30);
        agents.update(cx, |p, cx| p.refresh(cx));
        cx.simulate_resize(size(px(960.), px(640.)));
        cx.run_until_parked();
        let list = agents.read_with(cx, |p, _| p.list_focus());
        cx.update(|window, _| window.focus(&list));
        let scroll = agents.read_with(cx, |p, _| p.list_scroll());
        let frame = |cx: &mut VisualTestContext| {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        };
        frame(cx);
        assert_eq!(scroll.offset().y, px(0.));
        cx.simulate_keystrokes("end");
        frame(cx);
        let last = agents.read_with(cx, |p, cx| p.visible_ids(cx).last().cloned());
        assert_eq!(agents.read_with(cx, |p, _| p.selected()), last);
        assert!(scroll.offset().y < px(0.), "scrolled to the last row");
        let viewport = scroll.bounds();
        // The 30th older session (`add_history` ids are 12-hex too).
        let row = bounds(cx, "agents-row-f0000000001d");
        assert_inside(row, viewport, "the selected row");
        cx.simulate_keystrokes("home");
        frame(cx);
        assert_eq!(scroll.offset().y, px(0.), "back to the top");
    }

    /// Review M5: the speaker fixture doesn't turn Agents on.
    #[gpui::test]
    fn the_speaker_fixture_leaves_agents_off(cx: &mut TestAppContext) {
        let (shell, cx, _dir) = open_with(
            Page::Home,
            |root| crate::sandbox::add_fixture(root).unwrap(),
            cx,
        );
        assert!(!shell.read_with(cx, |shell, _| shell.agents_enabled()));
    }

    const NOTICE: &str = "There's no page called “NoSuchPage”, so Home is shown.";
    const SNIPPET_WARNING: &str = "Fix the snippet before leaving; it couldn't be saved.";

    fn unknown(route: Route) -> crate::controllers::instance::Request {
        crate::controllers::instance::Request {
            route,
            unknown_page: Some("NoSuchPage".into()),
        }
    }

    fn set_setup(shell: &Entity<Shell>, visible: bool, cx: &mut VisualTestContext) {
        shell.update(cx, |shell, cx| {
            if visible {
                shell.show_setup(cx)
            } else {
                shell.hide_setup(cx)
            }
        });
        cx.run_until_parked();
    }

    /// Snippets with a new snippet that has a trigger but no text: it
    /// can't be saved, so leaving the page is refused.
    fn open_unsaveable_snippet(
        cx: &mut TestAppContext,
    ) -> (Entity<Shell>, &mut VisualTestContext, tempfile::TempDir) {
        let (shell, cx, dir) = open(Page::Snippets, cx);
        cx.simulate_keystrokes("secondary-n");
        cx.simulate_input("my address");
        cx.run_until_parked();
        (shell, cx, dir)
    }

    fn setup_visible(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> bool {
        shell.read_with(cx, |shell, _| shell.setup_visible)
    }

    #[gpui::test]
    fn an_unknown_page_over_setup_shows_home_and_replaces_a_queued_page(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::History, cx);
        set_setup(&shell, true, cx);
        // A known page still waits behind the overlay.
        hand_off(&shell, &dir, Route::Page(Page::Models).into(), cx);
        assert!(setup_visible(&shell, cx));
        assert_eq!(page(&shell, cx), Page::History);
        // The newer unknown page hides setup (draft kept) and goes Home.
        hand_off(&shell, &dir, unknown(Route::Page(Page::Home)), cx);
        assert!(!setup_visible(&shell, cx));
        assert!(shell.read_with(cx, |shell, _| shell.setup_view.is_some()));
        assert_eq!(page(&shell, cx), Page::Home);
        assert_eq!(toast(&shell, cx).as_deref(), Some(NOTICE));
        assert!(shell.read_with(cx, |shell, _| shell.queued.is_none()));
    }

    #[gpui::test]
    fn an_unknown_page_over_setup_waits_for_an_open_dialog(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::History, cx);
        set_setup(&shell, true, cx);
        shell.update(cx, |shell, cx| {
            shell.confirm = Some(Confirm {
                title: "Discard?".into(),
                body: "Test dialog".into(),
                verb: "Discard".into(),
                destructive: true,
                on_confirm: Rc::new(|_, _| panic!("the dialog was cancelled")),
            });
            cx.notify();
        });
        hand_off(&shell, &dir, unknown(Route::Page(Page::Home)), cx);
        assert!(setup_visible(&shell, cx), "the dialog protects setup");
        assert_eq!(page(&shell, cx), Page::History);
        assert_eq!(toast(&shell, cx), None);
        cx.update(|window, cx| shell.update(cx, |shell, cx| shell.close_dialog(false, window, cx)));
        cx.run_until_parked();
        assert!(!setup_visible(&shell, cx));
        assert_eq!(page(&shell, cx), Page::Home);
        assert_eq!(toast(&shell, cx).as_deref(), Some(NOTICE));
    }

    #[gpui::test]
    fn setup_with_an_unknown_page_opens_setup_without_the_notice(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open(Page::History, cx);
        hand_off(&shell, &dir, unknown(Route::Setup), cx);
        assert!(shell.read_with(cx, |shell, _| shell.setup_visible));
        assert_eq!(page(&shell, cx), Page::History);
        assert_eq!(toast(&shell, cx), None);
    }

    #[gpui::test]
    fn a_refused_unknown_page_hand_off_keeps_the_snippet_warning(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open_unsaveable_snippet(cx);
        hand_off(&shell, &dir, unknown(Route::Page(Page::Home)), cx);
        assert_eq!(page(&shell, cx), Page::Snippets);
        assert_eq!(toast(&shell, cx).as_deref(), Some(SNIPPET_WARNING));
    }

    #[gpui::test]
    fn a_refused_queued_page_is_not_reported_as_opened(cx: &mut TestAppContext) {
        let (shell, cx, dir) = open_unsaveable_snippet(cx);
        set_setup(&shell, true, cx);
        hand_off(&shell, &dir, Route::Page(Page::History).into(), cx);
        set_setup(&shell, false, cx);
        assert_eq!(page(&shell, cx), Page::Snippets);
        assert_eq!(toast(&shell, cx).as_deref(), Some(SNIPPET_WARNING));
    }

    /// The History heading counts what a fresh load would after every
    /// change made from the page: a deleted failed row, a successful retry
    /// (applied by the page's own poll of the shared slot) and a deleted
    /// ok row.
    #[gpui::test]
    fn the_history_count_follows_delete_and_retry(cx: &mut TestAppContext) {
        use lf_core::history::NewEntry;
        let (shell, cx, _dir) = open_with(
            Page::History,
            |root| {
                let store = crate::data::open_history(root).unwrap();
                store.clear().unwrap();
                let mut ok = NewEntry::new("dictate");
                ok.final_text = "Already transcribed.".into();
                store.add(&ok).unwrap();
                let tone: Vec<f32> = (0..24_000)
                    .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / 16_000.0).sin() * 0.1)
                    .collect();
                for clip in [true, false] {
                    let mut failed = NewEntry::new("dictate");
                    failed.status = "failed".into();
                    failed.duration_s = 1.5;
                    if clip {
                        let path = store.write_clip(&tone, 16_000).unwrap();
                        failed.audio_path = Some(path.to_string_lossy().into());
                    }
                    store.add(&failed).unwrap();
                }
            },
            cx,
        );
        let view = shell.read_with(cx, |shell, _| match shell.find(Page::History) {
            Some(View::History(view)) => view.clone(),
            _ => panic!("History isn't open"),
        });
        let jobs = shell.read_with(cx, |shell, _| shell.cx.services.retry_jobs.clone());
        let count = |cx: &mut VisualTestContext| view.read_with(cx, |page, _| page.count_line());
        let rows = |cx: &mut VisualTestContext| view.read_with(cx, |page, _| page.rows());
        let one = Some("1 dictation on this device".to_string());
        assert_eq!(rows(cx).len(), 3);
        assert_eq!(count(cx), one);

        let unretryable = rows(cx)
            .into_iter()
            .find(|e| e.status == "failed" && e.audio_path.is_none())
            .unwrap();
        view.update(cx, |page, cx| page.delete(unretryable.id, cx));
        cx.run_until_parked();
        assert_eq!(rows(cx).len(), 2);
        assert_eq!(count(cx), one, "a failed row was never counted");

        let failed = rows(cx).into_iter().find(|e| e.status == "failed").unwrap();
        view.update(cx, |page, cx| {
            page.start_retry(crate::controllers::retry::RetryKey::of(&failed), cx)
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while jobs.running().is_some() {
            assert!(std::time::Instant::now() < deadline, "retry never finished");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        assert!(rows(cx).iter().all(|e| e.status == "ok"), "{:?}", rows(cx));
        assert_eq!(count(cx), Some("2 dictations on this device".to_string()));

        view.update(cx, |page, cx| page.delete(failed.id, cx));
        cx.run_until_parked();
        assert_eq!(rows(cx).len(), 1);
        assert_eq!(count(cx), one);
    }
}

/// History's Retry failure banner, laid out in a headless window: the
/// message wraps inside the detail column instead of pushing "Try again"
/// past the window edge.
#[cfg(test)]
#[path = "app_agents_settings_tests.rs"]
mod agents_settings_tests;

#[cfg(test)]
#[path = "app_agents_attention_tests.rs"]
mod agents_attention_tests;

#[cfg(test)]
mod history_layout_tests {
    use gpui::{Bounds, Entity, Modifiers, Pixels, TestAppContext, VisualTestContext};

    use super::focus_tests::open_with;
    use super::*;
    use crate::controllers::retry::{RetryError, RetryKey};

    /// A real-world failure: a long sentence with an unspaced Windows path.
    fn long_error() -> RetryError {
        RetryError::Unavailable(
            "The Whisper model file at \
             C:\\Users\\someone\\AppData\\Local\\LocalFlow\\models\\whisper\\ggml-tiny.bin \
             couldn't be opened because it is missing, incomplete or was moved while \
             LocalFlow was running. Download the model again from Models, or pick another \
             speech model in Settings, then try again."
                .into(),
        )
    }

    /// The failure from the bug report screenshot, verbatim.
    const REPORTED_ERROR: &str = "the Whisper tiny model isn't downloaded \
        (ggerganov/whisper.cpp/ggml-tiny.bin), and the 'lf' found isn't a Python LocalFlow \
        app that can retry. Download a Whisper model in Models & keys or install the Python \
        LocalFlow app, then try again.";

    /// The short transcript of the row in the bug report screenshot.
    const SHORT_TEXT: &str = "fixture final before retry";

    /// Adds a retryable dictation that reads [`SHORT_TEXT`].
    fn add_short_retryable(root: &crate::data::DataRoot) {
        let history = crate::data::open_history(root).unwrap();
        let clip = history.write_clip(&[0.1; 16_000], 16_000).unwrap();
        let mut entry = lf_core::history::NewEntry::new("dictate");
        entry.raw_text = SHORT_TEXT.into();
        entry.final_text = SHORT_TEXT.into();
        entry.duration_s = 1.0;
        entry.audio_path = Some(clip.to_string_lossy().into());
        history.add(&entry).unwrap();
    }

    /// Opens History at `width`×`height` with `error` as the last retry
    /// failure of a retryable row (the sandbox's failed one), and selects it.
    fn show_retry_error(
        width: f32,
        height: f32,
        error: RetryError,
        cx: &mut TestAppContext,
    ) -> (Entity<Shell>, &mut VisualTestContext, tempfile::TempDir) {
        show_retry_error_on(width, height, error, false, cx)
    }

    /// [`show_retry_error`]; `short` uses a row reading [`SHORT_TEXT`].
    fn show_retry_error_on(
        width: f32,
        height: f32,
        error: RetryError,
        short: bool,
        cx: &mut TestAppContext,
    ) -> (Entity<Shell>, &mut VisualTestContext, tempfile::TempDir) {
        let (shell, cx, dir) = open_with(
            Page::History,
            |root| {
                if short {
                    add_short_retryable(root)
                }
            },
            cx,
        );
        cx.simulate_resize(size(px(width), px(height)));
        let (entry, jobs) = shell.read_with(cx, |shell, _| {
            let services = &shell.cx.services;
            let entry = services
                .history()
                .unwrap()
                .recent(100, None, 0)
                .unwrap()
                .into_iter()
                .find(|e| {
                    e.kind == "dictate"
                        && e.audio_path.as_deref().is_some_and(|p| !p.is_empty())
                        && (!short || e.final_text == SHORT_TEXT)
                })
                .expect("the sandbox has a retryable dictation");
            (entry, services.retry_jobs.clone())
        });
        jobs.fail_for_test(RetryKey::of(&entry), error);
        let history = shell.read_with(cx, |shell, _| match shell.find(Page::History) {
            Some(View::History(view)) => view.clone(),
            _ => panic!("History isn't open"),
        });
        history.update(cx, |page, cx| page.select_id(entry.id, cx));
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        (shell, cx, dir)
    }

    fn bounds(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} isn't on screen"))
    }

    fn assert_inside(inner: Bounds<Pixels>, outer: Bounds<Pixels>, what: &str) {
        assert!(
            inner.left() >= outer.left()
                && inner.top() >= outer.top()
                && inner.right() <= outer.right()
                && inner.bottom() <= outer.bottom(),
            "{what}: {inner:?} isn't inside {outer:?}"
        );
    }

    fn viewport(cx: &mut VisualTestContext) -> Bounds<Pixels> {
        let size = cx.update(|window, _| window.viewport_size());
        Bounds::new(gpui::point(px(0.), px(0.)), size)
    }

    /// The wrapped banner and its action stay inside the banner, column and
    /// window; the actions row below is one row of buttons.
    fn assert_wraps_inside(cx: &mut VisualTestContext, right_edge: Pixels) {
        let column = bounds(cx, "history-detail-column");
        let banner = bounds(cx, "history-retry-banner");
        let again = bounds(cx, "button:Try again");
        let window = viewport(cx);
        assert!(
            banner.right() <= column.right(),
            "banner {banner:?} overflows column {column:?}"
        );
        assert!(
            banner.right() <= right_edge,
            "banner {banner:?} is past the edge"
        );
        assert!(banner.size.width <= px(640.), "banner {banner:?} too wide");
        assert!(
            banner.size.height > tsize::row(),
            "the message wraps: {banner:?}"
        );
        assert_inside(again, banner, "Try again in the banner");
        assert_inside(again, window, "Try again in the window");
        let message = bounds(cx, "banner-message");
        assert_inside(message, banner, "the message in the banner");
        assert!(
            message.right() <= again.left(),
            "the message {message:?} runs under Try again {again:?}"
        );

        let actions = bounds(cx, "history-actions");
        let note = bounds(cx, "history-retry-note");
        assert!(
            actions.size.height <= tsize::control_md() + px(0.5),
            "the actions are one row: {actions:?}"
        );
        assert!(
            (note.top() - actions.bottom() - space::s6()).abs() <= px(0.5),
            "no gap above the note: actions {actions:?}, note {note:?}"
        );
    }

    /// The detail pane is narrower than the column's `max_w` cap.
    #[gpui::test]
    fn long_retry_error_wraps_inside_the_detail_pane_wide_uncapped(cx: &mut TestAppContext) {
        let (_shell, cx, _dir) = show_retry_error(1280., 850., long_error(), cx);
        assert!(
            cx.debug_bounds("button:Back to list").is_none(),
            "wide layout"
        );
        assert!(
            bounds(cx, "history-detail").size.width < px(tsize::READING + 64.),
            "the column cap is inert"
        );
        assert_wraps_inside(cx, px(1280.));
    }

    /// The bug report: a wide pane (the column cap applies), a four-word
    /// transcript, and the reported message.
    #[gpui::test]
    fn long_retry_error_wraps_inside_the_detail_pane_wide_capped(cx: &mut TestAppContext) {
        let (_shell, cx, _dir) = show_retry_error_on(
            1600.,
            1200.,
            RetryError::Unavailable(REPORTED_ERROR.into()),
            true,
            cx,
        );
        assert!(
            cx.debug_bounds("button:Back to list").is_none(),
            "wide layout"
        );
        assert!(
            bounds(cx, "history-detail").size.width > px(tsize::READING + 64.),
            "the column cap applies"
        );
        assert_wraps_inside(cx, px(1600.));
    }

    #[gpui::test]
    fn long_retry_error_wraps_inside_the_detail_pane_narrow(cx: &mut TestAppContext) {
        let (_shell, cx, _dir) = show_retry_error(960., 640., long_error(), cx);
        assert!(
            cx.debug_bounds("button:Back to list").is_some(),
            "narrow layout"
        );
        assert_wraps_inside(cx, px(960.));
    }

    #[gpui::test]
    fn try_again_is_clickable_when_the_error_wraps(cx: &mut TestAppContext) {
        // Narrow: where an unconstrained banner pushed the button off-screen.
        let (shell, cx, _dir) = show_retry_error(960., 640., long_error(), cx);
        let jobs = shell.read_with(cx, |shell, _| shell.cx.services.retry_jobs.clone());
        let before = jobs.seq();
        assert!(jobs.running().is_none());
        let again = bounds(cx, "button:Try again");
        assert_inside(again, viewport(cx), "Try again in the window");
        cx.simulate_click(again.center(), Modifiers::none());
        // A refused start leaves the slot idle and the sequence unchanged.
        assert!(jobs.wait_idle(Duration::from_secs(10)));
        assert_eq!(jobs.seq(), before + 1, "Try again ran one retry");
        let last = jobs.last().expect("the retry finished");
        assert!(last.outcome.is_ok(), "the retry succeeded: {last:?}");
    }

    #[gpui::test]
    fn single_line_banner_stays_balanced(cx: &mut TestAppContext) {
        let (_shell, cx, _dir) =
            show_retry_error(1280., 850., RetryError::Failed("timed out".into()), cx);
        let banner = bounds(cx, "history-retry-banner");
        assert_eq!(banner.size.height, tsize::row(), "one line: {banner:?}");
        let mid = banner.center().y;
        for selector in ["banner-icon", "banner-message", "button:Try again"] {
            let part = bounds(cx, selector);
            assert!(
                (part.center().y - mid).abs() <= px(1.),
                "{selector} {part:?} is centred in {banner:?}"
            );
        }
    }
}
