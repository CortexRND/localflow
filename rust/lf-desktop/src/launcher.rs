//! The agent launcher: its own small process and popup window
//! (`lf-desktop --agents-launcher`), never a Hub page (design sections 2, 4,
//! 5, 8; the model is `controllers::agents_launcher`).
//!
//! A 640 px borderless popup (not resizable) in the upper third of the
//! primary display, focused, with the caret in the task field. On macOS and
//! Windows it is a `WindowKind::PopUp`; on Linux a plain window without
//! decorations (see [`window_style`]), because X11 gives a pop-up a
//! notification type many window managers never focus. It has its own single-instance lock beside the Hub's (a second
//! `--agents-launcher` closes it; Run again asks it to show a prefill), so
//! it works with the Hub closed and never holds the Hub's lock. It reads
//! the same agents service as the Agents page and opens the Hub only when
//! asked (`Cmd/Ctrl+Enter`, or Enter on a session row), by starting this
//! executable with `--page agents`, which the Hub's own hand-off routes.
//!
//! Keyboard focus stays in the task field: arrows move a highlight over the
//! list, Enter activates it, Shift+Enter is a new line, Esc steps back one
//! level then closes. Tab moves to the provider and workspace chips (a menu
//! they open hands focus back to the field); inside "Send without
//! redacting?" Tab stays between its two buttons. Rows and menu entries
//! also follow the pointer and take a click. Every launch goes through the local checks and the
//! secret scan first; nothing is sent while one blocks.
//!
//! Losing focus closes the launcher (not in a screenshot scene). Global
//! hotkey registration, tray items and context attachments are not part
//! of this window yet; it is shaped so they only need to start it.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    actions, div, prelude::*, px, size, App, Application, Bounds, Context, Entity, FocusHandle,
    FontWeight, KeyBinding, KeyUpEvent, SharedString, Window, WindowBackgroundAppearance,
    WindowBounds, WindowDecorations, WindowKind, WindowOptions,
};

use crate::controllers::agents::{self as ctl, AgentsBackend, AgentsError, LaunchRequest, Store};
use crate::controllers::agents_launcher::{
    self as model, Choice, Draft, DraftStore, Item, Key, LauncherInstance, SecretChoice, Section,
    Signal,
};
use crate::controllers::agents_secrets::{self as secrets, Finding};
use crate::data::DataRoot;
use crate::sandbox::Fixture;
use crate::theme::{elevation, size as tsize, space, theme, Theme, Tone};
use crate::ui::input::{self, InputEvent, TextInput};
use crate::ui::{self, Button};

pub const WIDTH: f32 = 640.;
pub const HEIGHT: f32 = 540.;
/// Live rows while open: `/changes` every 2 s without holding a request.
const POLL: Duration = Duration::from_secs(2);
/// Signals from a second launch.
const SIGNAL_POLL: Duration = Duration::from_millis(200);

actions!(
    agent_launcher,
    [
        Close,
        LaunchAndOpen,
        ProviderMenu,
        WorkspaceMenu,
        Quit,
        TabNext,
        TabPrev
    ]
);

const CONTEXT: &str = "AgentLauncher";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("escape", Close, c),
        KeyBinding::new("secondary-enter", LaunchAndOpen, c),
        KeyBinding::new("secondary-p", ProviderMenu, c),
        KeyBinding::new("secondary-r", WorkspaceMenu, c),
        KeyBinding::new("secondary-q", Quit, c),
        KeyBinding::new("tab", TabNext, c),
        KeyBinding::new("shift-tab", TabPrev, c),
    ]);
    input::bind_keys(cx);
}

// ---------------------------------------------------------------- spawn ---

/// A window this process starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spawn {
    /// `--agents-launcher` (`prefill`: show the draft Run again saved).
    Launcher { prefill: bool },
    /// `--page agents` (with a session to select).
    Agents { session: Option<String> },
    /// `--page agents/settings`: the Hub on Settings → Agents (works with
    /// Agents off; only a supporting service is needed).
    AgentSettings,
}

impl Spawn {
    /// The arguments after the data-root ones.
    pub fn args(&self) -> Vec<String> {
        match self {
            Spawn::Launcher { prefill } => {
                let mut args = vec!["--agents-launcher".to_string()];
                if *prefill {
                    args.push("--agent-prefill".into());
                }
                args
            }
            Spawn::Agents { session } => {
                let mut args = vec!["--page".to_string(), "agents".into()];
                if let Some(id) = session.as_ref().filter(|id| ctl::is_session_id(id)) {
                    args.push("--agent-session".into());
                    args.push(id.clone());
                }
                args
            }
            Spawn::AgentSettings => {
                vec![
                    "--page".to_string(),
                    crate::page::Page::AGENTS_SETTINGS.into(),
                ]
            }
        }
    }
}

/// The full argument list for `what` over `root` (a sandbox keeps its
/// directory, so the new window opens the same sandbox).
pub fn spawn_args(root: &DataRoot, what: &Spawn) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(dir) = &root.sandbox_dir {
        args.push("--sandbox".into());
        args.push("--data-dir".into());
        args.push(dir.display().to_string());
    }
    args.extend(what.args());
    args
}

#[cfg(test)]
thread_local! {
    /// What this test thread "started" (tests never start a process).
    static SPAWNED: std::cell::RefCell<Vec<Spawn>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// What this test thread "started" since the last call.
#[cfg(test)]
pub(crate) fn take_spawned() -> Vec<Spawn> {
    SPAWNED.with(|s| std::mem::take(&mut *s.borrow_mut()))
}

/// Start this executable for `what`, detached. Its output is discarded:
/// the arguments carry no task text (Run again's prefill goes through the
/// mode-0600 draft file, not the command line other users can read).
pub fn spawn(root: &DataRoot, what: Spawn) -> Result<(), String> {
    #[cfg(test)]
    {
        let _ = root;
        SPAWNED.with(|s| s.borrow_mut().push(what));
        Ok(())
    }
    #[cfg(not(test))]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut child = std::process::Command::new(exe)
            .args(spawn_args(root, &what))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        // Reap it when it exits, so no zombie is left behind.
        let _ = std::thread::Builder::new()
            .name("lf-spawn-reaper".into())
            .spawn(move || {
                let _ = child.wait();
            });
        Ok(())
    }
}

// --------------------------------------------------------------- window ---

/// What `main` hands the launcher.
pub struct LauncherLaunch {
    pub root: DataRoot,
    pub backend: Arc<dyn AgentsBackend>,
    /// `None` in tests.
    pub instance: Option<LauncherInstance>,
    /// `--agent-task` / `--agent-provider` / `--agent-workspace`.
    pub prefill: Option<Draft>,
    pub fixture: Option<Fixture>,
}

/// An open menu over the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Menu {
    Provider,
    Workspace,
}

pub struct Launcher {
    root: DataRoot,
    backend: Arc<dyn AgentsBackend>,
    instance: Option<LauncherInstance>,
    drafts: DraftStore,
    store: Store,
    task: Entity<TextInput>,
    choice: Choice,
    /// A prefill's provider, applied once providers are known.
    prefer: Option<(String, Option<String>)>,
    highlight: Option<Key>,
    menu: Option<(Menu, usize)>,
    /// The workspace was chosen from the menu (sticky across reloads).
    workspace_picked: bool,
    /// The secret check blocked this task (M3).
    findings: Vec<Finding>,
    /// The provider the blocked launch was for (`(id, profile)`): Redact and
    /// Send unchanged go there, never to another row's provider.
    pinned: Option<(String, String)>,
    /// "Send unchanged…" asked for confirmation.
    confirming: bool,
    cancel_focus: FocusHandle,
    send_focus: FocusHandle,
    provider_chip: FocusHandle,
    workspace_chip: FocusHandle,
    in_flight: bool,
    /// The last request tried (a retry of the same body reuses its id).
    last_request: Option<LaunchRequest>,
    error: Option<String>,
    /// The launch succeeded: nothing to keep as a draft.
    launched: bool,
    /// Asked to close while a launch was in flight: close once it lands.
    close_requested: bool,
    /// Enter's key-down moved focus onto a button: its key-up must not
    /// click that button.
    eat_enter_up: bool,
    closed: bool,
    was_active: bool,
    fixture: Option<Fixture>,
}

impl Launcher {
    pub fn new(launch: LauncherLaunch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let task = cx.new(|cx| {
            TextInput::new(cx, model::PLACEHOLDER)
                .multiline(1, 6)
                .submit_on_enter()
                .max_chars(20_000)
                .label("Task for an agent")
        });
        // Enter is caught before the field (see `render`); Submit is unused.
        cx.subscribe(&task, |this: &mut Self, _, event, cx| {
            if *event == InputEvent::Changed {
                this.edited(cx)
            }
        })
        .detach();
        let drafts = DraftStore::new(&model::launcher_dir(&launch.root));
        let mut launcher = Launcher {
            root: launch.root,
            backend: launch.backend,
            instance: launch.instance,
            drafts,
            store: Store::default(),
            task,
            choice: Choice::default(),
            prefer: None,
            highlight: None,
            menu: None,
            workspace_picked: false,
            findings: Vec::new(),
            pinned: None,
            confirming: false,
            cancel_focus: cx.focus_handle(),
            send_focus: cx.focus_handle(),
            provider_chip: cx.focus_handle().tab_stop(true),
            workspace_chip: cx.focus_handle().tab_stop(true),
            in_flight: false,
            last_request: None,
            error: None,
            launched: false,
            close_requested: false,
            eat_enter_up: false,
            closed: false,
            was_active: false,
            fixture: launch.fixture,
        };
        // A screenshot scene never reads or writes the draft file, so one
        // scene's task can't leak into the next.
        let draft = launch.prefill.or_else(|| launcher.load_draft());
        launcher.apply_draft(draft, window, cx);
        launcher.task.read(cx).focus(window);
        launcher.load(cx);
        launcher.watch(cx);
        launcher
    }

    /// Show a draft (restored or prefilled) as a new draft: its text fully
    /// selected (typing replaces it), its provider and workspace preferred.
    fn apply_draft(&mut self, draft: Option<Draft>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = draft else {
            return;
        };
        self.task.update(cx, |t, cx| t.set_text(draft.text, cx));
        if let Some((id, profile)) = draft.provider {
            self.prefer = Some((id, Some(profile)));
        }
        if draft.workspace.is_some() {
            self.choice.workspace = draft.workspace;
        }
        self.findings.clear();
        self.pinned = None;
        self.error = None;
        self.resolve();
        self.refresh_highlight(cx);
        self.task.read(cx).focus(window);
        window.dispatch_action(Box::new(input::SelectAll), cx);
    }

    /// The kept draft, unless this is a screenshot scene.
    fn load_draft(&self) -> Option<Draft> {
        if self.fixture.is_some() {
            return None;
        }
        self.drafts.load(ctl::now())
    }

    /// Keep the field as the draft (unless this is a screenshot scene). A
    /// value the secret check flags is never written: the draft keeps the
    /// task with each such value replaced by `[redacted]`.
    fn keep_draft(&self, cx: &App) {
        if self.fixture.is_some() {
            return;
        }
        let text = self.text(cx);
        let text = if model::scan(&text).is_empty() {
            text
        } else {
            secrets::redact(&text)
        };
        self.drafts.save(&Draft {
            text,
            provider: self.choice.provider.clone(),
            workspace: self.choice.workspace.clone(),
            saved_at: ctl::now(),
        });
    }

    fn text(&self, cx: &App) -> String {
        self.task.read(cx).text().to_string()
    }

    fn typed(&self, cx: &App) -> bool {
        !self.text(cx).trim().is_empty()
    }

    fn simulated(&self) -> bool {
        ctl::simulated(self.backend.simulated(), self.store.status.as_ref())
    }

    fn resolve(&mut self) {
        let prefer = self.prefer.clone();
        let picked = self.workspace_picked.then(|| self.choice.workspace.clone());
        self.choice.resolve(
            &self.store,
            prefer.as_ref().map(|(id, p)| (id.as_str(), p.as_deref())),
        );
        // A workspace picked from the menu (even "No workspace") stays,
        // while the menu still offers it (no "none" once a default is set).
        if let Some(workspace) = picked.filter(|w| {
            model::workspace_options(self.store.status.as_ref())
                .iter()
                .any(|(alias, _)| alias == w)
        }) {
            self.choice.workspace = workspace;
        }
        if self.store.loaded && !self.store.providers.is_empty() {
            self.prefer = None;
        }
    }

    fn sections(&self, cx: &App) -> Vec<Section> {
        let rows = self.store.rows(self.backend.now());
        model::sections(
            &self.store,
            &rows,
            &self.text(cx),
            &self.choice,
            &self.findings,
        )
    }

    fn refresh_highlight(&mut self, cx: &mut Context<Self>) {
        let sections = self.sections(cx);
        self.highlight = model::keep(&sections, self.highlight.as_ref(), self.typed(cx));
        cx.notify();
    }

    /// The field changed: leave a blocking state, reset the highlight to
    /// the first launch row (or none while empty).
    fn edited(&mut self, cx: &mut Context<Self>) {
        self.findings.clear();
        self.pinned = None;
        self.confirming = false;
        self.error = None;
        let sections = self.sections(cx);
        self.highlight = model::initial(&sections, self.typed(cx));
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let generation = self.store.begin();
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ctl::load(backend.as_ref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let now = this.backend.now();
                if this.store.finish(generation, result, now) {
                    this.resolve();
                    // The first frame can already have painted before this
                    // answer lands; force the loaded rows to repaint (a
                    // fixed-size popup may never otherwise get a resize).
                    // Screenshot scenes only (review M7): in normal use
                    // `refresh_highlight` below notifies, which repaints.
                    if this.fixture.is_some() {
                        cx.refresh_windows();
                    }
                    // The M3 scene's one action: Enter on its seeded task.
                    if this.fixture == Some(Fixture::AgentsLauncherSecret)
                        && this.findings.is_empty()
                    {
                        this.pinned = this.choice.provider.clone();
                        this.findings = model::scan(&this.text(cx));
                    }
                    this.refresh_highlight(cx);
                    // Readiness is the scene's state; the capture loop
                    // retries until pixels are actually on screen.
                    this.report_scene(cx);
                }
            });
        })
        .detach();
    }

    /// Rows stay live while the launcher is open, without moving the
    /// highlight; and signals from a second launch are picked up.
    fn watch(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut since_poll = Duration::ZERO;
            loop {
                cx.background_executor().timer(SIGNAL_POLL).await;
                since_poll += SIGNAL_POLL;
                let poll = since_poll >= POLL;
                if poll {
                    since_poll = Duration::ZERO;
                }
                let next = this.update(cx, |this, _| {
                    (!this.closed)
                        .then(|| (this.backend.clone(), this.store.revision, this.store.loaded))
                });
                let Ok(Some((backend, since, loaded))) = next else {
                    break;
                };
                if poll && loaded {
                    let result = cx
                        .background_executor()
                        .spawn(async move { backend.changes(since, 0) })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        match result {
                            Ok(changes) => {
                                if this.store.apply_changes(changes) {
                                    this.load(cx);
                                }
                            }
                            Err(error) => {
                                let now = this.backend.now();
                                this.store.failed(error, now);
                            }
                        }
                        this.refresh_highlight(cx);
                    });
                }
            }
        })
        .detach();
    }

    /// A signal from a second launch (polled with the window).
    pub fn poll_signal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(signal) = self
            .instance
            .as_ref()
            .and_then(LauncherInstance::take_signal)
        else {
            return;
        };
        match signal {
            Signal::Toggle => self.close(window, cx),
            Signal::Show => {
                let draft = self.load_draft();
                self.apply_draft(draft, window, cx);
                window.activate_window();
            }
        }
    }

    // ------------------------------------------------------------ keys ---

    fn step_highlight(&mut self, down: bool, cx: &mut Context<Self>) {
        if let Some((menu, at)) = self.menu {
            let n = self.menu_items(menu).len();
            let at = if down {
                (at + 1).min(n.saturating_sub(1))
            } else {
                at.saturating_sub(1)
            };
            self.menu = Some((menu, at));
        } else {
            let sections = self.sections(cx);
            self.highlight = model::step(&sections, self.highlight.as_ref(), down);
        }
        cx.notify();
    }

    /// Esc: close a menu or the confirmation; leave a blocking state; else
    /// close the launcher (keeping the draft).
    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() || std::mem::take(&mut self.confirming) {
            self.task.read(cx).focus(window);
        } else if !self.findings.is_empty() {
            self.findings.clear();
            self.pinned = None;
            self.refresh_highlight(cx);
            self.task.read(cx).focus(window);
        } else {
            self.close(window, cx);
        }
        cx.notify();
    }

    /// Enter (or `Cmd/Ctrl+Enter` with `open`): activate the highlighted row.
    pub fn enter(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.in_flight || self.confirming {
            return;
        }
        if let Some((menu, at)) = self.menu.take() {
            self.choose(menu, at, cx);
            return;
        }
        // M5: nothing connected (the list is the empty state): Enter opens
        // Settings → Agents; a typed task is kept as the draft.
        if self.no_provider() && self.findings.is_empty() {
            self.open_agent_settings(window, cx);
            return;
        }
        let sections = self.sections(cx);
        let item = model::find(&sections, self.highlight.as_ref()).cloned();
        match item {
            Some(Item::Secret(SecretChoice::Redact)) => {
                let redacted = secrets::redact(&self.text(cx));
                self.launch(redacted, open, window, cx);
            }
            Some(Item::Secret(SecretChoice::SendUnchanged)) => {
                self.confirming = true;
                // This Enter's key-up must not click Cancel.
                self.eat_enter_up = true;
                window.focus(&self.cancel_focus);
                cx.notify();
            }
            Some(Item::Session(row)) => {
                self.open_agents(Some(row.session.id.clone()), window, cx);
            }
            Some(Item::Launch { enabled: true, .. }) => {
                let text = self.text(cx);
                self.check_and_launch(text, open, window, cx);
            }
            // Needs sign-in: the row's own reason says where to go.
            Some(Item::Launch {
                enabled: false,
                provider,
                ..
            }) if provider.readiness == ctl::Readiness::NeedsSignIn => {
                self.open_agent_settings(window, cx)
            }
            _ if open && !self.typed(cx) => self.open_agents(None, window, cx),
            _ if open => {
                // Cmd+Enter with text and no highlighted row: the chosen
                // provider.
                let text = self.text(cx);
                self.check_and_launch(text, open, window, cx);
            }
            // Empty field, or nothing usable highlighted: nothing happens.
            _ => {}
        }
    }

    /// The provider a launch goes to: while the secret check blocks, the
    /// one the blocked launch was for; else the highlighted Launch row's,
    /// else the chosen one.
    fn launch_provider(&self, cx: &App) -> Option<ctl::Provider> {
        if let (false, Some((id, profile))) = (self.findings.is_empty(), &self.pinned) {
            return self
                .store
                .provider(id, profile)
                .filter(|p| p.readiness == ctl::Readiness::Connected)
                .cloned();
        }
        let sections = self.sections(cx);
        model::active_provider(
            &self.store,
            &sections,
            self.highlight.as_ref(),
            &self.choice,
        )
        .filter(|p| p.readiness == ctl::Readiness::Connected)
        .cloned()
    }

    /// The local checks, then the secret scan, then the launch. Either
    /// blocks; the local checks first, so their reason is never hidden
    /// behind the secret choices (review M5).
    fn check_and_launch(
        &mut self,
        text: String,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.trim().is_empty() {
            return;
        }
        let Some(provider) = self.launch_provider(cx) else {
            return;
        };
        if let Some(reason) = self.local_check(&text, &provider) {
            self.error = Some(reason);
            cx.notify();
            return;
        }
        let findings = model::scan(&text);
        if !findings.is_empty() {
            self.findings = findings;
            self.pinned = Some((provider.id.clone(), provider.profile.clone()));
            self.error = None;
            let sections = self.sections(cx);
            self.highlight = model::initial(&sections, true);
            cx.notify();
            return;
        }
        self.launch(text, open, window, cx);
    }

    fn local_check(&self, text: &str, provider: &ctl::Provider) -> Option<String> {
        let max = self.store.status.as_ref().and_then(|s| s.max_prompt_chars);
        model::local_check(
            text,
            provider,
            &self.store.providers,
            self.choice.workspace.as_deref(),
            max,
        )
    }

    /// Send exactly one launch. While it is in flight every further Enter
    /// is ignored; a retry of the same task reuses its id (replay-safe).
    fn launch(&mut self, text: String, open: bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.in_flight || text.trim().is_empty() {
            return;
        }
        let Some(provider) = self.launch_provider(cx) else {
            // The blocked launch's provider went offline meanwhile: say so
            // rather than ignore Enter (it is never swapped for another).
            if let (false, Some((id, profile))) = (self.findings.is_empty(), &self.pinned) {
                let name = self
                    .store
                    .provider(id, profile)
                    .map(|p| model::provider_name(p, &self.store.providers))
                    .unwrap_or_else(|| "That provider".into());
                self.error = Some(format!(
                    "{name} isn't connected right now. Your task is kept here."
                ));
                cx.notify();
            }
            return;
        };
        let workspace = self.choice.workspace.clone();
        if let Some(reason) = self.local_check(&text, &provider) {
            self.error = Some(reason);
            cx.notify();
            return;
        }
        let request = model::request(
            self.last_request.as_ref(),
            &text,
            &provider,
            workspace.as_deref(),
        );
        self.last_request = Some(request.clone());
        self.in_flight = true;
        self.error = None;
        self.task.update(cx, |t, cx| t.set_disabled(true, cx));
        let name = model::provider_name(&provider, &self.store.providers);
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.launch(&request) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.in_flight = false;
                this.task.update(cx, |t, cx| t.set_disabled(false, cx));
                match result {
                    Ok(session) => {
                        this.launched = true;
                        if this.fixture.is_none() {
                            this.drafts.clear();
                        }
                        let id = session.id.clone();
                        this.store.apply_session(session);
                        if open {
                            let _ = spawn(&this.root, Spawn::Agents { session: Some(id) });
                        }
                        this.closed = true;
                        cx.notify();
                    }
                    Err(error) if this.close_requested => {
                        // Closed while in flight (review M3): go now. The
                        // task is kept as a draft only when the service
                        // answered no; after a transport failure it may
                        // have launched, and a restored draft would launch
                        // it a second time under a new id.
                        if error != AgentsError::Offline {
                            this.keep_draft(cx);
                        }
                        this.closed = true;
                        cx.notify();
                    }
                    Err(error) => {
                        if !model::replayable(&error) {
                            this.last_request = None;
                        }
                        this.findings.clear();
                        // Back on the blocked launch's own provider row.
                        if let Some((id, profile)) = this.pinned.take() {
                            this.highlight = Some(Key::Launch(id, profile));
                        }
                        this.error = Some(model::launch_error(&error, &name));
                        this.refresh_highlight(cx);
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn open_agents(
        &mut self,
        session: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if spawn(&self.root, Spawn::Agents { session }).is_err() {
            self.error = Some("LocalFlow couldn't open Agents.".into());
            cx.notify();
            return;
        }
        self.close(window, cx);
    }

    /// M5: the list is read, the service answers, and no provider is
    /// connected or even set up.
    fn no_provider(&self) -> bool {
        let service_error = matches!(
            (&self.store.error, &self.store.status),
            (Some(e), None) if *e != AgentsError::Absent
        );
        self.store.loaded
            && !service_error
            && self
                .store
                .providers
                .iter()
                .all(|p| p.readiness == ctl::Readiness::NotConnected)
    }

    /// Open Settings → Agents in the Hub, then close (keeping a draft).
    fn open_agent_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if spawn(&self.root, Spawn::AgentSettings).is_err() {
            self.error = Some("LocalFlow couldn't open Agent settings.".into());
            cx.notify();
            return;
        }
        self.close(window, cx);
    }

    /// Close: keep an unsent draft (not after a launch), then go away.
    /// While a launch is in flight the window stays until it lands, and
    /// nothing is kept as a draft unless it failed: closing now would quit
    /// mid-request and keep a task that may already be running.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        if self.in_flight {
            self.close_requested = true;
            self.menu = None;
            self.confirming = false;
            cx.notify();
            return;
        }
        if !self.launched {
            self.keep_draft(cx);
        }
        self.closed = true;
        window.remove_window();
        cx.notify();
    }

    /// Tab / Shift+Tab: inside "Send without redacting?" between its two
    /// buttons only; else on to the next control (an open menu closes).
    fn tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming {
            let order = [self.cancel_focus.clone(), self.send_focus.clone()];
            let at = order.iter().position(|h| h.is_focused(window));
            let next = match (at, forward) {
                (None, _) => 0,
                (Some(i), true) => (i + 1) % order.len(),
                (Some(i), false) => (i + order.len() - 1) % order.len(),
            };
            window.focus(&order[next]);
        } else {
            self.menu = None;
            if forward {
                window.focus_next();
            } else {
                window.focus_prev();
            }
        }
        cx.notify();
    }

    // ----------------------------------------------------------- menus ---

    fn menu_items(&self, menu: Menu) -> Vec<(String, bool)> {
        match menu {
            Menu::Provider => self
                .store
                .providers
                .iter()
                .filter(|p| p.readiness != ctl::Readiness::NotConnected)
                .map(|p| {
                    (
                        model::provider_name(p, &self.store.providers),
                        p.readiness == ctl::Readiness::Connected,
                    )
                })
                .collect(),
            Menu::Workspace => {
                let status = self.store.status.as_ref();
                model::workspace_options(status)
                    .into_iter()
                    .map(|(alias, usable)| {
                        (model::workspace_label(alias.as_deref(), status), usable)
                    })
                    .collect()
            }
        }
    }

    /// Open (or close) a menu. Focus goes back to the task field, where
    /// the arrows, Enter and Esc work the menu (review M7).
    fn open_menu(&mut self, menu: Menu, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming || !self.findings.is_empty() || self.in_flight {
            return;
        }
        self.task.read(cx).focus(window);
        let at = match menu {
            Menu::Provider => self
                .store
                .providers
                .iter()
                .filter(|p| p.readiness != ctl::Readiness::NotConnected)
                .position(|p| self.choice.is(p))
                .unwrap_or(0),
            Menu::Workspace => model::workspace_options(self.store.status.as_ref())
                .iter()
                .position(|(alias, _)| *alias == self.choice.workspace)
                .unwrap_or(0),
        };
        self.menu = if self.menu.map(|(m, _)| m) == Some(menu) {
            None
        } else {
            Some((menu, at))
        };
        cx.notify();
    }

    /// Pick a menu entry: sticky for this launcher only (no settings are
    /// written yet).
    fn choose(&mut self, menu: Menu, at: usize, cx: &mut Context<Self>) {
        match menu {
            Menu::Provider => {
                let usable: Vec<&ctl::Provider> = self
                    .store
                    .providers
                    .iter()
                    .filter(|p| p.readiness != ctl::Readiness::NotConnected)
                    .collect();
                if let Some(p) = usable
                    .get(at)
                    .filter(|p| p.readiness == ctl::Readiness::Connected)
                {
                    self.choice.provider = Some((p.id.clone(), p.profile.clone()));
                }
            }
            Menu::Workspace => {
                if let Some((alias, true)) =
                    model::workspace_options(self.store.status.as_ref()).get(at)
                {
                    self.workspace_picked = true;
                    self.choice.workspace = alias.clone();
                }
            }
        }
        self.menu = None;
        let sections = self.sections(cx);
        self.highlight = model::initial(&sections, self.typed(cx));
        cx.notify();
    }

    // ------------------------------------------------------ test hooks ---

    #[cfg(test)]
    pub(crate) fn highlight(&self) -> Option<Key> {
        self.highlight.clone()
    }

    #[cfg(test)]
    pub(crate) fn findings(&self) -> usize {
        self.findings.len()
    }

    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<String> {
        self.error.clone()
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.closed
    }

    #[cfg(test)]
    pub(crate) fn confirming(&self) -> bool {
        self.confirming
    }

    #[cfg(test)]
    pub(crate) fn cancel_focus(&self) -> FocusHandle {
        self.cancel_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn task_input(&self) -> Entity<TextInput> {
        self.task.clone()
    }

    #[cfg(test)]
    pub(crate) fn choice(&self) -> Choice {
        self.choice.clone()
    }

    #[cfg(test)]
    pub(crate) fn sends(&self, cx: &App) -> String {
        self.sends_line(cx)
    }

    #[cfg(test)]
    pub(crate) fn send_focus(&self) -> FocusHandle {
        self.send_focus.clone()
    }

    #[cfg(test)]
    pub(crate) fn provider_chip(&self) -> FocusHandle {
        self.provider_chip.clone()
    }

    #[cfg(test)]
    pub(crate) fn menu_open(&self) -> bool {
        self.menu.is_some()
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> bool {
        self.in_flight
    }

    // ---------------------------------------------------------- render ---

    fn sends_line(&self, cx: &App) -> String {
        let sections = self.sections(cx);
        let provider = model::active_provider(
            &self.store,
            &sections,
            self.highlight.as_ref(),
            &self.choice,
        );
        model::sends_line(
            provider,
            &self.store.providers,
            self.choice.workspace.as_deref(),
            self.simulated(),
            !self.findings.is_empty(),
        )
    }

    /// "Send unchanged" confirmed: launch the task as typed.
    fn confirm_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirming = false;
        let text = self.text(cx);
        self.launch(text, false, window, cx);
    }

    fn report_scene(&self, cx: &App) {
        let Some(fixture) = self.fixture else {
            return;
        };
        let ready = self.store.loaded
            && match fixture {
                Fixture::AgentsLauncher => !self.typed(cx) && !self.store.is_empty(),
                Fixture::AgentsLauncherTyped => self.typed(cx) && self.highlight.is_some(),
                Fixture::AgentsLauncherSecret => !self.findings.is_empty(),
                Fixture::AgentsLauncherEmpty => !self.store.any_connected(),
                _ => false,
            };
        if ready {
            crate::sandbox::scene_ready(fixture);
        }
    }

    fn chip(
        &self,
        t: &Theme,
        id: &'static str,
        label: String,
        focus: &FocusHandle,
        menu: Menu,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let ring = ui::ring(t);
        let hover = t.hover;
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .track_focus(focus)
            .flex()
            .items_center()
            .gap(space::s2())
            .h(px(26.))
            .px(space::s4())
            .rounded(tsize::radius_inner())
            .bg(t.bg_muted)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .focus(move |s| s.shadow(ring.clone()))
            .on_click(cx.listener(move |this, _, window, cx| this.open_menu(menu, window, cx)))
            .child(ui::supporting(t, label).text_color(t.text))
            .child(ui::icon("chevron-down", t.icon_secondary).size(px(12.)))
    }

    /// One row. Rows follow the pointer (the highlight moves under it) and
    /// a click activates the row like Enter; a disabled row does neither.
    fn render_item(
        &self,
        t: &Theme,
        item: &Item,
        highlighted: bool,
        now: f64,
        secret_provider: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let sub = if highlighted {
            t.text_accent
        } else {
            t.text_secondary
        };
        let (tile, title, detail, trailing, enabled): (
            String,
            String,
            gpui::AnyElement,
            String,
            bool,
        ) = match item {
            Item::Launch {
                provider,
                enabled,
                title,
                detail,
            } => (
                provider.monogram(),
                title.clone(),
                ui::supporting(t, detail.clone())
                    .text_color(if *enabled { sub } else { t.text_disabled })
                    .into_any_element(),
                String::new(),
                *enabled,
            ),
            Item::Session(row) => {
                let mut meta = format!(" · {}", row.provider);
                if let Some(repo) = &row.session.repo {
                    meta.push_str(&format!(" · {repo}"));
                }
                let when = row
                    .session
                    .updated_at
                    .or(row.session.created_at)
                    .map(|ts| ctl::relative(ts, now))
                    .unwrap_or_default();
                (
                    row.monogram.clone(),
                    row.title.clone(),
                    div()
                        .flex()
                        .items_center()
                        .min_w_0()
                        .child(ui::status_glyph(
                            t,
                            row.state.tone(),
                            row.state.icon(),
                            row.state.label(),
                        ))
                        .child(
                            ui::supporting(t, meta)
                                .text_color(sub)
                                .line_clamp(1)
                                .text_ellipsis(),
                        )
                        .into_any_element(),
                    when,
                    true,
                )
            }
            Item::Secret(choice) => {
                let provider = secret_provider;
                let (title, detail) = match choice {
                    SecretChoice::Redact => (
                        "Redact and launch".to_string(),
                        secrets::redact_detail(provider, self.findings.len()),
                    ),
                    SecretChoice::SendUnchanged => (
                        "Send unchanged…".to_string(),
                        format!(
                            "Asks you to confirm before anything leaves {}",
                            ctl::this_computer()
                        ),
                    ),
                };
                (
                    String::new(),
                    title,
                    ui::supporting(t, detail).text_color(sub).into_any_element(),
                    String::new(),
                    true,
                )
            }
        };
        // A row that needs sign-in is reachable too (Enter opens Agent
        // settings); its text stays in the disabled colour.
        let interactive = item.actionable();
        let key = item.key();
        let selector = match &key {
            Key::Launch(id, profile) => format!("launcher-launch-{id}-{profile}"),
            Key::Session(id) => format!("launcher-session-{id}"),
            Key::Secret(SecretChoice::Redact) => "launcher-redact".into(),
            Key::Secret(SecretChoice::SendUnchanged) => "launcher-unchanged".into(),
        };
        let hover_key = key.clone();
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .when(interactive, |d| {
                d.cursor_pointer()
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && this.menu.is_none() && !this.confirming && !this.in_flight {
                            this.highlight = Some(hover_key.clone());
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.click_row(key.clone(), window, cx)
                    }))
            })
            .flex()
            .items_center()
            .gap(space::s5())
            .h(px(48.))
            .flex_none()
            .mx(px(6.))
            .px(space::s4())
            .rounded(tsize::radius_element())
            .when(highlighted, |d| d.bg(t.accent_muted))
            .child(if tile.is_empty() {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(26.))
                    .child(ui::icon("shield", t.icon_secondary).size(px(16.)))
            } else {
                ui::monogram_tile(t, tile, px(26.))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        ui::label(t, title)
                            .when(!enabled, |d| d.text_color(t.text_disabled))
                            .line_clamp(1)
                            .text_ellipsis(),
                    )
                    .child(detail),
            )
            .when(!trailing.is_empty(), |d| {
                d.child(ui::supporting(t, trailing).text_color(sub).flex_none())
            })
            .into_any_element()
    }

    /// A click on a row: the same as Enter with it highlighted.
    fn click_row(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming || self.in_flight {
            return;
        }
        self.menu = None;
        self.highlight = Some(key);
        self.task.read(cx).focus(window);
        self.enter(false, window, cx);
        // No Enter key-up follows a click: nothing to swallow.
        self.eat_enter_up = false;
        cx.notify();
    }

    /// The chosen provider's name (the chip).
    fn launch_provider_name(&self) -> Option<String> {
        self.choice
            .provider(&self.store)
            .map(|p| model::provider_name(p, &self.store.providers))
    }

    /// The name the secret rows and the confirmation use: the provider the
    /// blocked launch goes to.
    fn secret_provider_name(&self, cx: &App) -> String {
        self.launch_provider(cx)
            .map(|p| model::provider_name(&p, &self.store.providers))
            .unwrap_or_else(|| "The agent".into())
    }

    fn render_menu(
        &self,
        t: &Theme,
        menu: Menu,
        at: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let items = self.menu_items(menu);
        div()
            .id("launcher-menu")
            .debug_selector(|| "launcher-menu".into())
            .occlude()
            .absolute()
            .top(px(4.))
            .left(space::s6())
            .w(px(280.))
            .p(space::s2())
            .flex()
            .flex_col()
            .rounded(tsize::radius_element())
            .border_1()
            .border_color(t.border)
            .bg(t.bg_popover)
            .shadow(elevation::popover(t))
            .children(items.into_iter().enumerate().map(|(i, (label, enabled))| {
                div()
                    .id(("launcher-menu-item", i))
                    .debug_selector(move || format!("launcher-menu-{i}"))
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                if let (true, Some((m, _))) = (*hovered, this.menu) {
                                    this.menu = Some((m, i));
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.task.read(cx).focus(window);
                                this.choose(menu, i, cx);
                            }))
                    })
                    .px(space::s4())
                    .py(space::s3())
                    .rounded(tsize::radius_inner())
                    .when(i == at, |d| d.bg(t.accent_muted))
                    .child(ui::body(t, label).text_color(if enabled {
                        t.text
                    } else {
                        t.text_disabled
                    }))
            }))
    }

    fn render_confirm(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let provider = self.secret_provider_name(cx);
        let (title, body, verb) =
            secrets::send_unchanged_confirm(&self.findings, "your task", &provider);
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(t.scrim)
            .child(
                div()
                    .debug_selector(|| "launcher-confirm".into())
                    .w(px(400.))
                    .p(space::s8())
                    .flex()
                    .flex_col()
                    .gap(space::s5())
                    .rounded(tsize::radius_container())
                    .bg(t.bg_popover)
                    .border_1()
                    .border_color(t.border)
                    .shadow(elevation::popover(t))
                    .child(ui::heading(t, title))
                    .child(ui::secondary(t, body))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(space::s3())
                            .child(
                                Button::new("launcher-confirm-cancel", "Cancel")
                                    .track_focus(&self.cancel_focus)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirming = false;
                                        this.task.read(cx).focus(window);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("launcher-confirm-send", verb)
                                    .destructive()
                                    .track_focus(&self.send_focus)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.confirm_send(window, cx)
                                    })),
                            ),
                    ),
            )
    }
}

impl Render for Launcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        self.report_scene(cx);
        if !self.was_active && window.is_window_active() {
            self.was_active = true;
        }
        let now = self.backend.now();
        let sections = self.sections(cx);
        let typed = self.typed(cx);
        let blocking = !self.findings.is_empty();
        let sends = self.sends_line(cx);
        let count = self.task.read(cx).text().chars().count();
        let max = self.store.status.as_ref().and_then(|s| s.max_prompt_chars);
        // Nothing was ever read and the service fails (down, refused, or a
        // 401/5xx): say so, rather than guess "connect an agent" (M4).
        let service_error = match (&self.store.error, &self.store.status) {
            (Some(AgentsError::Offline), None) => Some(ctl::SERVICE_DOWN.to_string()),
            (Some(AgentsError::Absent), None) => None,
            (Some(e), None) => Some(e.message()),
            _ => None,
        }
        .filter(|_| self.store.loaded);
        let service_down = service_error.is_some();
        // M5: nothing connected or even set up. Offline or signed-out
        // providers are a warning over their disabled rows instead.
        let no_provider = !service_down && self.no_provider();
        let all_offline = self.store.loaded && !no_provider && !self.store.any_connected();
        // Chips: provider, then workspace (when workspaces are configured).
        let provider_label = self.launch_provider_name();
        let has_repos = self
            .store
            .status
            .as_ref()
            .is_some_and(|s| !s.repos.is_empty());
        let workspace_label = self
            .choice
            .workspace
            .clone()
            .unwrap_or_else(|| "No workspace".into());
        let mut chips = div()
            .flex()
            .items_center()
            .gap(space::s3())
            .px(space::s6())
            .h(px(44.));
        if let Some(label) = provider_label.clone() {
            let focus = self.provider_chip.clone();
            chips =
                chips.child(self.chip(&t, "launcher-provider", label, &focus, Menu::Provider, cx));
        }
        if has_repos && provider_label.is_some() {
            let focus = self.workspace_chip.clone();
            chips = chips.child(self.chip(
                &t,
                "launcher-workspace",
                workspace_label,
                &focus,
                Menu::Workspace,
                cx,
            ));
        }
        // The banner for the state: a failed check or launch, a secret, the
        // service failing, else what blocks every launch right now.
        let secret = secrets::warning(&self.findings, "your task").filter(|_| self.error.is_none());
        let banner: Option<gpui::AnyElement> = if let Some((title, body)) = secret {
            let (fg, bg) = t.tone(Tone::Warning);
            Some(
                div()
                    .debug_selector(|| "launcher-secret".into())
                    .mx(space::s6())
                    .mt(space::s4())
                    .p(space::s5())
                    .flex()
                    .gap(space::s4())
                    .rounded(tsize::radius_group())
                    .bg(bg)
                    .child(ui::icon("triangle-alert", fg))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .flex_1()
                            .min_w_0()
                            .child(ui::label(&t, title).font_weight(FontWeight::SEMIBOLD))
                            .child(ui::body(&t, body)),
                    )
                    .into_any_element(),
            )
        } else if let (None, Some(message)) = (&self.error, service_error) {
            Some(
                div()
                    .debug_selector(|| "launcher-service".into())
                    .mx(space::s6())
                    .mt(space::s4())
                    .child(ui::banner(
                        &t,
                        Tone::Error,
                        message,
                        Some(
                            Button::new("launcher-retry", "Try again")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                                .into_any_element(),
                        ),
                    ))
                    .into_any_element(),
            )
        } else {
            // A failed launch; else what blocks every launch right now.
            let state = match (&self.error, &self.store.error, &self.store.status) {
                (Some(e), _, _) => Some((Tone::Error, e.clone())),
                (None, Some(e @ AgentsError::Absent), _) => Some((Tone::Error, e.message())),
                (None, _, Some(s)) if self.store.loaded && !s.enabled => Some((
                    Tone::Warning,
                    "Agents is turned off in LocalFlow's agent service.".to_string(),
                )),
                _ if all_offline => Some((
                    Tone::Warning,
                    "No agent provider can be reached right now. Your task is kept here."
                        .to_string(),
                )),
                _ => None,
            };
            state.map(|(tone, e)| {
                div()
                    .debug_selector(|| "launcher-error".into())
                    .mx(space::s6())
                    .mt(space::s4())
                    .child(ui::banner(&t, tone, e, None))
                    .into_any_element()
            })
        };
        // The list, or the no-provider state (M5).
        let mut list: Vec<gpui::AnyElement> = Vec::new();
        let secret_provider = self.secret_provider_name(cx);
        if no_provider && !blocking {
            list.push(
                ui::empty_state(
                    &t,
                    "bot",
                    ctl::NO_PROVIDER_HEADING,
                    ctl::no_provider_body(),
                    Some(
                        Button::new("launcher-open-settings", ctl::OPEN_AGENT_SETTINGS)
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_agent_settings(window, cx)
                            }))
                            .into_any_element(),
                    ),
                )
                .debug_selector(|| "launcher-no-provider".into())
                .into_any_element(),
            );
        } else {
            for section in &sections {
                if !section.heading.is_empty() {
                    list.push(
                        ui::supporting(&t, section.heading.clone())
                            .font_weight(FontWeight::SEMIBOLD)
                            .h(px(28.))
                            .flex()
                            .items_end()
                            .pb(space::s2())
                            .px(space::s8())
                            .flex_none()
                            .into_any_element(),
                    );
                }
                for item in &section.items {
                    let highlighted = self.highlight.as_ref() == Some(&item.key());
                    list.push(self.render_item(&t, item, highlighted, now, &secret_provider, cx));
                }
            }
        }
        let hints = model::hints(typed, blocking, !self.store.is_empty());
        let footer = div()
            .flex()
            .items_center()
            .gap(space::s6())
            .h(px(36.))
            .px(space::s6())
            .border_t_1()
            .border_color(t.separator)
            .when(!typed && !blocking, |d| {
                d.child(ui::supporting(&t, model::EMPTY_HINT))
            })
            .child(div().flex_1())
            .children(hints.into_iter().map(|(keys, word)| {
                div()
                    .flex()
                    .items_center()
                    .gap(space::s2())
                    .child(ui::keys(&t, &model::key_label(keys)))
                    .child(ui::supporting(&t, word))
            }));
        let menu = self
            .menu
            .map(|(menu, at)| self.render_menu(&t, menu, at, cx));
        let rounded = window_style().rounded;
        div()
            .id("agent-launcher")
            .debug_selector(|| "agent-launcher".into())
            .key_context(CONTEXT)
            // An Enter whose key-down moved focus onto a button must not
            // click it on key-up (GPUI clicks a focused element then).
            .capture_key_up(cx.listener(|this, event: &KeyUpEvent, window, _| {
                if event.keystroke.key == "enter" && std::mem::take(&mut this.eat_enter_up) {
                    window.prevent_default();
                }
            }))
            .on_action(cx.listener(|this, _: &TabNext, window, cx| this.tab(true, window, cx)))
            .on_action(cx.listener(|this, _: &TabPrev, window, cx| this.tab(false, window, cx)))
            .capture_action(cx.listener(|this, _: &input::Up, _, cx| {
                this.step_highlight(false, cx);
                cx.stop_propagation();
            }))
            .capture_action(cx.listener(|this, _: &input::Down, _, cx| {
                this.step_highlight(true, cx);
                cx.stop_propagation();
            }))
            // Enter activates the highlighted row (Shift+Enter, a separate
            // action, still inserts a new line). Only while the field has
            // focus: a focused button takes Enter as its own click.
            .capture_action(cx.listener(|this, _: &input::Enter, window, cx| {
                if this.task.read(cx).is_focused(window) {
                    this.enter(false, window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &Close, window, cx| this.escape(window, cx)))
            .on_action(
                cx.listener(|this, _: &LaunchAndOpen, window, cx| this.enter(true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &ProviderMenu, window, cx| {
                this.open_menu(Menu::Provider, window, cx)
            }))
            .on_action(cx.listener(|this, _: &WorkspaceMenu, window, cx| {
                this.open_menu(Menu::Workspace, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Quit, window, cx| this.close(window, cx)))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.bg_popover)
            .border_1()
            .border_color(t.border)
            .when(rounded, |d| d.rounded(tsize::radius_container()))
            .overflow_hidden()
            .text_color(t.text)
            .font_family(t.font_family.clone())
            // 1. Task field.
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(space::s4())
                    .px(space::s6())
                    .pt(space::s5())
                    .min_h(px(56.))
                    .child(
                        div()
                            .pt(space::s2())
                            .child(ui::icon("bot", t.icon_secondary)),
                    )
                    .child(div().flex_1().min_w_0().child(self.task.clone()))
                    .when(count > model::COUNT_FROM, |d| {
                        let label = match max {
                            Some(max) => format!("{count} / {max}"),
                            None => count.to_string(),
                        };
                        d.child(ui::supporting(&t, label).pt(space::s3()))
                    }),
            )
            // 2. Launch bar.
            .child(chips)
            // 3. Sends line (polite live region in the design).
            .child(
                div()
                    .debug_selector(|| "launcher-sends".into())
                    .flex()
                    .items_center()
                    .gap(space::s3())
                    .h(px(30.))
                    .px(space::s6())
                    .child(ui::icon("shield", t.icon_secondary).size(px(14.)))
                    .child(ui::supporting(&t, SharedString::from(sends))),
            )
            .child(ui::divider(&t))
            .children(banner)
            // 4. List.
            .child(
                div()
                    .id("launcher-list")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .py(space::s2())
                    .overflow_y_scroll()
                    .children(list)
                    .children(menu),
            )
            // 5. Footer.
            .child(footer)
            .when(self.confirming, |d| d.child(self.render_confirm(&t, cx)))
    }
}

// ------------------------------------------------------------------ run ---

/// How the launcher window is made on this platform (review H5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowStyle {
    pub kind: WindowKind,
    pub decorations: Option<WindowDecorations>,
    pub background: WindowBackgroundAppearance,
    /// The panel draws rounded corners (only where the window behind them
    /// is transparent; elsewhere they would show square artifacts).
    pub rounded: bool,
}

/// macOS: a transparent `PopUp` panel with rounded corners. Linux: GPUI
/// gives an X11 `PopUp` `_NET_WM_WINDOW_TYPE_NOTIFICATION`, which many
/// window managers never focus (typing, close-on-blur and activation would
/// all fail), so it is a normal window asking for no decorations, opaque
/// with square corners (X11 has no transparency without a compositor; on
/// Wayland it is an ordinary toplevel either way). Windows: an opaque
/// `PopUp` with square corners.
pub fn window_style() -> WindowStyle {
    if cfg!(target_os = "macos") {
        WindowStyle {
            kind: WindowKind::PopUp,
            decorations: None,
            background: WindowBackgroundAppearance::Transparent,
            rounded: true,
        }
    } else if cfg!(any(target_os = "linux", target_os = "freebsd")) {
        WindowStyle {
            kind: WindowKind::Normal,
            decorations: Some(WindowDecorations::Client),
            background: WindowBackgroundAppearance::Opaque,
            rounded: false,
        }
    } else {
        WindowStyle {
            kind: WindowKind::PopUp,
            decorations: None,
            background: WindowBackgroundAppearance::Opaque,
            rounded: false,
        }
    }
}

/// Open the launcher window and run until it closes. `Ok` when a window
/// opened (and later closed); the caller turns an error into exit 1.
pub fn run(launch: LauncherLaunch) -> Result<(), String> {
    let failed = Rc::new(std::cell::RefCell::new(None::<String>));
    let seen = failed.clone();
    let mode = crate::appearance::load(&launch.root);
    let sandbox = launch.root.is_sandbox();
    let scene = launch.fixture.is_some();
    Application::new()
        .with_assets(crate::ui::assets::Assets)
        .run(move |cx: &mut App| {
            crate::native_appearance::set(mode);
            let mut base = Theme::for_mode(mode, cx.window_appearance());
            base.font_family = crate::app::pick_font(cx);
            cx.set_global(base);
            bind_keys(cx);
            let display = cx.primary_display();
            let bounds = match &display {
                Some(d) => {
                    let area = d.bounds();
                    let x = area.origin.x + (area.size.width - px(WIDTH)) / 2.;
                    let y = area.origin.y + area.size.height / 6.;
                    Bounds::new(gpui::point(x, y), size(px(WIDTH), px(HEIGHT)))
                }
                None => Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx),
            };
            let title = if sandbox {
                "Agent launcher (Sandbox)"
            } else {
                model::ACCESSIBLE_NAME
            };
            let style = window_style();
            let window = cx.open_window(
                WindowOptions {
                    window_background: style.background,
                    window_decorations: style.decorations,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some(title.into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    focus: true,
                    show: true,
                    kind: style.kind,
                    is_movable: false,
                    is_resizable: false,
                    is_minimizable: false,
                    display_id: display.map(|d| d.id()),
                    app_id: Some("localflow-agent-launcher".into()),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Launcher::new(launch, window, cx)),
            );
            match window {
                Ok(window) => {
                    crate::console::detach();
                    let _ = window.update(cx, |_, window, cx| {
                        cx.activate(true);
                        window.activate_window();
                        // Signals from a second launch, and close once a
                        // launch has landed.
                        cx.spawn_in(window, async move |this, cx| loop {
                            cx.background_executor().timer(SIGNAL_POLL).await;
                            let Ok(()) = this.update_in(cx, |l: &mut Launcher, window, cx| {
                                if l.closed {
                                    window.remove_window();
                                } else {
                                    l.poll_signal(window, cx);
                                }
                            }) else {
                                break;
                            };
                        })
                        .detach();
                        // Click outside or an app switch: close (the
                        // platform tells us through window activation).
                        // Screenshot scenes stay open.
                        if !scene {
                            cx.observe_window_activation(window, |l: &mut Launcher, window, cx| {
                                if window.is_window_active() {
                                    l.was_active = true;
                                } else if l.was_active && !l.closed {
                                    l.close(window, cx);
                                }
                            })
                            .detach();
                        }
                        // Follow the system light/dark setting live, in
                        // System mode only (Light/Dark ignore the OS), as
                        // the Hub does: on Linux the OS appearance arrives
                        // after the window opens, so this is what turns a
                        // dark session's launcher dark.
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
                    *seen.borrow_mut() = Some(format!("couldn't open the agent launcher: {err:#}"));
                    cx.quit();
                }
            }
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.spawn(async |cx| cx.update(|cx| cx.quit())).detach();
                }
            })
            .detach();
        });
    let error = failed.borrow_mut().take();
    match error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The launcher's data root and backend for this launch (sandbox or real).
pub fn backend_for(root: &DataRoot) -> Arc<dyn AgentsBackend> {
    let remote = crate::remote::Remote::for_root(root);
    let config = crate::data::ConfigFile::new(root.clone())
        .load()
        .unwrap_or_else(|_| lf_core::config::Config::defaults_for(cfg!(target_os = "macos")));
    remote.agents(&config)
}

/// [`backend_for`] for a screenshot scene: the pinned in-memory seed, with
/// no provider connected for M5.
pub fn scene_backend(root: &DataRoot, fixture: Fixture) -> Arc<dyn AgentsBackend> {
    let remote = crate::remote::Remote::for_root(root);
    if let Some(fake) = remote.sandbox_agents() {
        fake.use_fixture();
        if fixture == Fixture::AgentsLauncherEmpty {
            fake.disconnect_all();
        }
        return fake;
    }
    backend_for(root)
}

/// The typed scene's task (M2) and the M3 scene's seeded fake credential.
pub fn scene_prefill(fixture: Fixture) -> Option<Draft> {
    let text = match fixture {
        Fixture::AgentsLauncherTyped => "Add retry backoff to the webhook sender and open a PR",
        Fixture::AgentsLauncherSecret => {
            "Rotate the staging webhook secret.\nThe current one is \
             api_key=sk-test-FAKE0sandbox0credential0000 and must stop working."
        }
        _ => return None,
    };
    Some(Draft {
        text: text.into(),
        provider: None,
        workspace: None,
        saved_at: ctl::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::agents::{self as seed, FakeAgents};
    use gpui::{TestAppContext, VisualTestContext};

    struct Open<'a> {
        launcher: Entity<Launcher>,
        fake: Arc<FakeAgents>,
        cx: &'a mut VisualTestContext,
        dir: tempfile::TempDir,
    }

    fn open_with<'a>(
        cx: &'a mut TestAppContext,
        prefill: Option<Draft>,
        prepare: impl FnOnce(&FakeAgents),
    ) -> Open<'a> {
        open_in(cx, tempfile::tempdir().unwrap(), prefill, None, prepare)
    }

    fn open_in<'a>(
        cx: &'a mut TestAppContext,
        dir: tempfile::TempDir,
        prefill: Option<Draft>,
        fixture: Option<Fixture>,
        prepare: impl FnOnce(&FakeAgents),
    ) -> Open<'a> {
        let fake = Arc::new(FakeAgents::fixture());
        prepare(&fake);
        cx.update(|cx| {
            cx.set_global(Theme::light());
            bind_keys(cx);
        });
        let launch = LauncherLaunch {
            root: DataRoot::sandbox(dir.path()),
            backend: fake.clone(),
            instance: None,
            prefill,
            fixture,
        };
        let (launcher, cx) = cx.add_window_view(|window, cx| Launcher::new(launch, window, cx));
        cx.run_until_parked();
        take_spawned();
        Open {
            launcher,
            fake,
            cx,
            dir,
        }
    }

    fn open(cx: &mut TestAppContext) -> Open<'_> {
        open_with(cx, None, |_| {})
    }

    fn read<T>(o: &mut Open, f: impl FnOnce(&Launcher, &App) -> T) -> T {
        o.launcher.read_with(o.cx, f)
    }

    fn drafts(o: &Open) -> DraftStore {
        DraftStore::new(&model::launcher_dir(&DataRoot::sandbox(o.dir.path())))
    }

    fn focused(o: &mut Open, handle: &FocusHandle) -> bool {
        o.cx.update(|window, _| handle.is_focused(window))
    }

    /// A key's release after a frame is drawn: GPUI clicks a focused
    /// element on Enter's or Space's key-up (tests only send key-downs).
    fn release(o: &mut Open, key: &str) {
        o.cx.update(|window, _| window.refresh());
        o.cx.run_until_parked();
        o.cx.simulate_event(KeyUpEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
        });
        o.cx.run_until_parked();
    }

    /// Press and release `key`, as a keyboard does.
    fn press(o: &mut Open, key: &str) {
        o.cx.simulate_keystrokes(key);
        release(o, key);
    }

    fn click(o: &mut Open, selector: &'static str) {
        o.cx.update(|window, _| window.refresh());
        o.cx.run_until_parked();
        let bounds = o.cx.debug_bounds(selector).expect(selector);
        o.cx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::none());
        o.cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        o.cx.run_until_parked();
    }

    fn draft_file(o: &Open) -> Option<String> {
        std::fs::read_to_string(drafts(o).path()).ok()
    }

    #[gpui::test]
    fn an_empty_enter_does_nothing_and_arrows_reach_sessions(cx: &mut TestAppContext) {
        let mut o = open(cx);
        assert_eq!(read(&mut o, |l, _| l.highlight()), None);
        o.cx.simulate_keystrokes("enter");
        assert_eq!(o.fake.calls().launch, 0);
        assert!(!read(&mut o, |l, _| l.is_closed()));
        o.cx.simulate_keystrokes("down");
        assert_eq!(
            read(&mut o, |l, _| l.highlight()),
            Some(Key::Session(seed::NEEDS_INPUT.into())),
            "Needs you first"
        );
        o.cx.simulate_keystrokes("up");
        assert_eq!(
            read(&mut o, |l, _| l.highlight()),
            None,
            "↑ from the first: none"
        );
        o.cx.simulate_keystrokes("down enter");
        assert_eq!(
            take_spawned(),
            [Spawn::Agents {
                session: Some(seed::NEEDS_INPUT.into())
            }]
        );
        assert!(read(&mut o, |l, _| l.is_closed()));
    }

    #[gpui::test]
    fn typed_enter_launches_exactly_once_and_closes(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("Add retry backoff");
        assert_eq!(
            read(&mut o, |l, _| l.highlight()),
            Some(Key::Launch("claude".into(), String::new())),
            "the default provider's row"
        );
        assert_eq!(read(&mut o, |l, cx| l.sends(cx)), ctl::SANDBOX_SENDS);
        let before = o.fake.status().unwrap().counts.unwrap();
        // A double Enter while the first is in flight: one launch.
        let launcher = o.launcher.clone();
        o.cx.update(|window, cx| {
            launcher.update(cx, |l, cx| {
                l.enter(false, window, cx);
                l.enter(false, window, cx);
            })
        });
        o.cx.run_until_parked();
        assert_eq!(o.fake.calls().launch, 1);
        let after = o.fake.status().unwrap().counts.unwrap();
        assert_eq!(after.queued, before.queued + 1, "exactly one session");
        assert!(read(&mut o, |l, _| l.is_closed()));
        assert!(take_spawned().is_empty(), "plain Enter opens nothing");
        let body = o.fake.received().pop().unwrap();
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["prompt", "provider", "id", "repo", "origin"]);
        assert_eq!(body["prompt"], "Add retry backoff");
        assert_eq!(body["repo"], "acme/web");
        assert!(ctl::is_session_id(body["id"].as_str().unwrap()));
        let drafts = DraftStore::new(&model::launcher_dir(&DataRoot::sandbox(o.dir.path())));
        assert_eq!(drafts.load(ctl::now()), None, "no draft after a launch");
    }

    #[gpui::test]
    fn shift_enter_is_a_new_line_and_arrows_switch_provider(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("first");
        o.cx.simulate_keystrokes("shift-enter");
        o.cx.simulate_input("second");
        let input = read(&mut o, |l, _| l.task_input());
        assert_eq!(
            input.read_with(o.cx, |t, _| t.text().to_string()),
            "first\nsecond"
        );
        assert_eq!(o.fake.calls().launch, 0);
        o.cx.simulate_keystrokes("down");
        assert_eq!(
            read(&mut o, |l, _| l.highlight()),
            Some(Key::Launch("devin".into(), String::new()))
        );
        o.cx.simulate_keystrokes("enter");
        let body = o.fake.received().pop().unwrap();
        assert_eq!(body["provider"], "devin");
        assert_eq!(body["prompt"], "first\nsecond", "newlines kept");
    }

    #[gpui::test]
    fn cmd_enter_with_an_empty_field_opens_agents(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_keystrokes("secondary-enter");
        assert_eq!(take_spawned(), [Spawn::Agents { session: None }]);
        assert!(read(&mut o, |l, _| l.is_closed()));
        assert_eq!(o.fake.calls().launch, 0);
    }

    #[gpui::test]
    fn cmd_enter_launches_then_opens_the_session(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("Draft the notes");
        o.cx.simulate_keystrokes("secondary-enter");
        let id = o.fake.received().pop().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(take_spawned(), [Spawn::Agents { session: Some(id) }]);
        assert!(read(&mut o, |l, _| l.is_closed()));
    }

    #[gpui::test]
    fn escape_closes_and_keeps_the_draft(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("Half a task");
        o.cx.simulate_keystrokes("escape");
        assert!(read(&mut o, |l, _| l.is_closed()));
        let drafts = DraftStore::new(&model::launcher_dir(&DataRoot::sandbox(o.dir.path())));
        let draft = drafts.load(ctl::now()).expect("kept");
        assert_eq!(draft.text, "Half a task");
        assert_eq!(draft.provider, Some(("claude".into(), String::new())));
    }

    #[gpui::test]
    fn a_restored_draft_is_selected_so_typing_replaces_it(cx: &mut TestAppContext) {
        let draft = Draft {
            text: "Half a task".into(),
            provider: Some(("devin".into(), String::new())),
            workspace: Some("acme/api".into()),
            saved_at: ctl::now(),
        };
        let mut o = open_with(cx, Some(draft), |_| {});
        assert_eq!(
            read(&mut o, |l, _| l.choice()),
            Choice {
                provider: Some(("devin".into(), String::new())),
                workspace: Some("acme/api".into())
            },
            "Run again's provider and workspace"
        );
        let input = read(&mut o, |l, _| l.task_input());
        assert_eq!(
            input.read_with(o.cx, |t, _| t.text().to_string()),
            "Half a task"
        );
        assert!(read(&mut o, |l, _| l.highlight()).is_some());
        // Typing replaces it (it was restored fully selected).
        o.cx.simulate_input("New");
        assert_eq!(input.read_with(o.cx, |t, _| t.text().to_string()), "New");
    }

    #[gpui::test]
    fn no_connected_provider_offers_nothing_to_launch(cx: &mut TestAppContext) {
        let mut o = open_with(cx, None, |fake| fake.disconnect_all());
        o.cx.simulate_input("Do something");
        assert_eq!(read(&mut o, |l, _| l.highlight()), None);
        o.cx.update(|window, _| window.refresh());
        o.cx.run_until_parked();
        assert!(o.cx.debug_bounds("launcher-no-provider").is_some());
        let _ = take_spawned();
        // M5: Enter opens Settings → Agents (never a launch); the typed
        // task is kept as the draft.
        o.cx.simulate_keystrokes("enter");
        assert_eq!(o.fake.calls().launch, 0);
        assert_eq!(take_spawned(), [Spawn::AgentSettings]);
        assert!(read(&mut o, |l, _| l.is_closed()));
        assert_eq!(
            drafts(&o).load(ctl::now()).map(|d| d.text).as_deref(),
            Some("Do something"),
            "the draft stays"
        );
    }

    /// A provider that needs sign-in is listed disabled; Enter on it opens
    /// Settings → Agents, never a launch.
    #[gpui::test]
    fn enter_on_a_signed_out_provider_opens_agent_settings(cx: &mut TestAppContext) {
        let mut o = open_with(cx, None, |fake| {
            for id in ["claude", "devin", "codex"] {
                fake.set_readiness(id, "auth_failed");
            }
        });
        o.cx.simulate_input("qqzzqq");
        assert_eq!(read(&mut o, |l, _| l.highlight()), None, "never first");
        o.cx.simulate_keystrokes("down");
        assert!(matches!(
            read(&mut o, |l, _| l.highlight()),
            Some(Key::Launch(..))
        ));
        o.cx.simulate_keystrokes("enter");
        assert_eq!(o.fake.calls().launch, 0);
        assert_eq!(take_spawned(), [Spawn::AgentSettings]);
    }

    #[test]
    fn agent_settings_spawns_the_hub_on_its_tab() {
        assert_eq!(
            Spawn::AgentSettings.args(),
            ["--page".to_string(), "agents/settings".to_string()]
        );
        let parsed = crate::args::parse(Spawn::AgentSettings.args(), false).unwrap();
        assert_eq!(parsed.unknown_page.as_deref(), Some("agents/settings"));
        assert!(crate::page::Page::is_agents_settings_name(
            parsed.unknown_page.as_deref().unwrap()
        ));
    }

    #[gpui::test]
    fn an_offline_provider_is_listed_but_never_highlighted(cx: &mut TestAppContext) {
        let mut o = open_with(cx, None, |fake| fake.set_readiness("devin", "unreachable"));
        o.cx.simulate_input("x");
        o.cx.simulate_keystrokes("down");
        assert_eq!(
            read(&mut o, |l, _| l.highlight()),
            Some(Key::Launch("codex".into(), String::new())),
            "Devin is skipped"
        );
        o.cx.update(|window, _| window.refresh());
        o.cx.run_until_parked();
        assert!(
            o.cx.debug_bounds("launcher-launch-devin-").is_some(),
            "but drawn"
        );
    }

    /// Review G2: a seeded credential never reaches the provider payload,
    /// the session store or the screen after Redact.
    #[gpui::test]
    fn a_secret_blocks_and_redact_keeps_it_contained(cx: &mut TestAppContext) {
        const KEY: &str = "sk-test-SEEDEDcredential0000xyz";
        let mut o = open(cx);
        o.cx.simulate_input(&format!("Rotate it.\nOld key: api_key={KEY}"));
        o.cx.simulate_keystrokes("enter");
        assert_eq!(o.fake.calls().launch, 0, "blocked before anything is sent");
        assert_eq!(read(&mut o, |l, _| l.findings()), 1);
        assert_eq!(
            read(&mut o, |l, _| l.highlight()),
            Some(Key::Secret(SecretChoice::Redact)),
            "the safest choice first"
        );
        assert_eq!(read(&mut o, |l, cx| l.sends(cx)), ctl::NOTHING_SENT);
        o.cx.update(|window, _| window.refresh());
        o.cx.run_until_parked();
        assert!(o.cx.debug_bounds("launcher-secret").is_some());
        // Esc steps back to editing, Enter blocks again, then Redact.
        o.cx.simulate_keystrokes("escape");
        assert_eq!(read(&mut o, |l, _| l.findings()), 0);
        assert!(!read(&mut o, |l, _| l.is_closed()));
        o.cx.simulate_keystrokes("enter enter");
        assert_eq!(o.fake.calls().launch, 1);
        let received = o.fake.received().pop().unwrap().to_string();
        assert!(
            !received.contains("SEEDEDcredential"),
            "payload is redacted"
        );
        assert!(received.contains("[redacted]"));
        assert!(
            !o.fake.stored().contains("SEEDEDcredential"),
            "store is clean"
        );
        assert!(read(&mut o, |l, _| l.is_closed()));
    }

    #[gpui::test]
    fn send_unchanged_asks_first_with_focus_on_cancel(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("password=hunter2hunter2 for staging");
        o.cx.simulate_keystrokes("enter down enter");
        assert!(read(&mut o, |l, _| l.confirming()));
        let cancel = read(&mut o, |l, _| l.cancel_focus());
        assert!(
            o.cx.update(|window, _| cancel.is_focused(window)),
            "focus on Cancel"
        );
        assert_eq!(o.fake.calls().launch, 0);
        o.cx.simulate_keystrokes("escape");
        assert!(!read(&mut o, |l, _| l.confirming()));
        assert_eq!(o.fake.calls().launch, 0, "nothing sent without the confirm");
        // Review H4: by keyboard only. The Enter that opened the dialog
        // doesn't click Cancel on its release; Tab stays inside the dialog.
        o.cx.simulate_keystrokes("down");
        press(&mut o, "enter");
        assert!(read(&mut o, |l, _| l.confirming()), "still asking");
        let send = read(&mut o, |l, _| l.send_focus());
        o.cx.simulate_keystrokes("tab");
        assert!(focused(&mut o, &send), "Tab reaches Send unchanged");
        o.cx.simulate_keystrokes("tab");
        assert!(focused(&mut o, &cancel), "and wraps to Cancel");
        o.cx.simulate_keystrokes("shift-tab");
        assert!(focused(&mut o, &send), "Shift+Tab wraps back");
        assert_eq!(o.fake.calls().launch, 0);
        press(&mut o, "enter");
        assert_eq!(o.fake.calls().launch, 1, "sent as typed, once confirmed");
        let body = o.fake.received().pop().unwrap();
        assert_eq!(body["prompt"], "password=hunter2hunter2 for staging");
    }

    /// Review H1: a flagged value never reaches the draft file.
    #[gpui::test]
    fn a_flagged_task_is_kept_only_redacted(cx: &mut TestAppContext) {
        const KEY: &str = "sk-test-SEEDEDcredential0000xyz";
        let mut o = open(cx);
        o.cx.simulate_input(&format!("Rotate it.\nOld key: api_key={KEY}"));
        o.cx.simulate_keystrokes("enter");
        assert_eq!(read(&mut o, |l, _| l.findings()), 1, "blocked");
        o.cx.simulate_keystrokes("secondary-q");
        assert!(read(&mut o, |l, _| l.is_closed()));
        let raw = draft_file(&o).expect("the task is kept");
        assert!(!raw.contains("SEEDEDcredential"), "{raw}");
        assert_eq!(
            drafts(&o).load(ctl::now()).unwrap().text,
            "Rotate it.\nOld key: api_key=[redacted]"
        );
    }

    /// Review H2: a screenshot scene neither restores nor keeps a draft.
    #[gpui::test]
    fn a_scene_never_reads_or_writes_the_draft(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let store = DraftStore::new(&model::launcher_dir(&DataRoot::sandbox(dir.path())));
        let earlier = Draft {
            text: "An earlier task".into(),
            provider: None,
            workspace: None,
            saved_at: ctl::now(),
        };
        store.save(&earlier);
        let before = std::fs::read_to_string(store.path()).unwrap();
        let mut o = open_in(
            cx,
            dir,
            scene_prefill(Fixture::AgentsLauncherSecret),
            Some(Fixture::AgentsLauncherSecret),
            |_| {},
        );
        assert_eq!(read(&mut o, |l, _| l.findings()), 1, "the scene's state");
        o.cx.simulate_keystrokes("secondary-q");
        assert!(read(&mut o, |l, _| l.is_closed()));
        assert_eq!(
            draft_file(&o).as_deref(),
            Some(before.as_str()),
            "untouched"
        );
    }

    #[gpui::test]
    fn an_empty_scene_restores_no_draft(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        DraftStore::new(&model::launcher_dir(&DataRoot::sandbox(dir.path()))).save(&Draft {
            text: "An earlier task".into(),
            provider: None,
            workspace: None,
            saved_at: ctl::now(),
        });
        let mut o = open_in(cx, dir, None, Some(Fixture::AgentsLauncherEmpty), |_| {});
        let input = read(&mut o, |l, _| l.task_input());
        assert_eq!(input.read_with(o.cx, |t, _| t.text().to_string()), "");
    }

    /// Review H3: Redact and Send unchanged go to the provider whose row
    /// was chosen, and say so.
    #[gpui::test]
    fn the_blocked_launch_keeps_its_provider(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("password=hunter2hunter2 for staging");
        o.cx.simulate_keystrokes("down");
        assert_eq!(
            read(&mut o, |l, _| l.highlight()),
            Some(Key::Launch("devin".into(), String::new()))
        );
        o.cx.simulate_keystrokes("enter");
        assert_eq!(read(&mut o, |l, _| l.findings()), 1);
        o.cx.update(|window, _| window.refresh());
        o.cx.run_until_parked();
        // Send unchanged… names Devin in its confirmation.
        o.cx.simulate_keystrokes("down");
        press(&mut o, "enter");
        assert!(read(&mut o, |l, _| l.confirming()));
        let confirm = o.cx.debug_bounds("launcher-confirm");
        assert!(confirm.is_some());
        let launcher = o.launcher.clone();
        let named =
            o.cx.update(|_, cx| launcher.read(cx).secret_provider_name(cx));
        assert_eq!(named, "Devin");
        // If Devin goes offline meanwhile, Redact says so; it never falls
        // back to another provider.
        o.cx.simulate_keystrokes("escape up");
        o.fake.set_readiness("devin", "unreachable");
        let launcher = o.launcher.clone();
        o.cx.update(|_, cx| launcher.update(cx, |l, cx| l.load(cx)));
        o.cx.run_until_parked();
        assert_eq!(read(&mut o, |l, _| l.findings()), 1, "still blocked");
        o.cx.simulate_keystrokes("enter");
        assert_eq!(o.fake.calls().launch, 0);
        assert_eq!(
            read(&mut o, |l, _| l.error()).as_deref(),
            Some("Devin isn't connected right now. Your task is kept here.")
        );
        o.fake.set_readiness("devin", "ok");
        o.cx.update(|_, cx| launcher.update(cx, |l, cx| l.load(cx)));
        o.cx.run_until_parked();
        o.cx.simulate_keystrokes("enter");
        let body = o.fake.received().pop().unwrap();
        assert_eq!(body["provider"], "devin", "Redact went to Devin");
        assert_eq!(body["prompt"], "password=[redacted] for staging");
    }

    /// A click that opens "Send without redacting?" leaves the next Enter
    /// release alone (only a key-down Enter arms the swallow).
    #[gpui::test]
    fn a_clicked_send_unchanged_still_takes_enter(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("password=hunter2hunter2 for staging");
        o.cx.simulate_keystrokes("enter");
        click(&mut o, "launcher-unchanged");
        assert!(read(&mut o, |l, _| l.confirming()));
        o.cx.simulate_keystrokes("tab");
        press(&mut o, "enter");
        assert_eq!(o.fake.calls().launch, 1, "Send unchanged by Enter");
    }

    /// Review H4 and M7: Tab reaches the chips; a chip's menu hands focus
    /// back to the field, where the arrows and Enter work it.
    #[gpui::test]
    fn tab_reaches_the_chips_and_their_menus_work(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("x");
        o.cx.update(|window, _| window.refresh());
        o.cx.run_until_parked();
        let chip = read(&mut o, |l, _| l.provider_chip());
        o.cx.simulate_keystrokes("tab");
        assert!(focused(&mut o, &chip), "Tab: the provider chip");
        press(&mut o, "enter");
        assert!(read(&mut o, |l, _| l.menu_open()), "Enter opens its menu");
        let input = read(&mut o, |l, _| l.task_input());
        assert!(o.cx.update(|window, cx| input.read(cx).is_focused(window)));
        o.cx.simulate_keystrokes("down enter");
        assert_eq!(
            read(&mut o, |l, _| l.choice()).provider,
            Some(("devin".into(), String::new()))
        );
        o.cx.simulate_keystrokes("shift-tab");
        assert!(!o.cx.update(|window, cx| input.read(cx).is_focused(window)));
    }

    /// Review M2: rows and menu entries take the pointer.
    #[gpui::test]
    fn rows_and_menu_entries_take_a_click(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("Click me");
        o.cx.simulate_keystrokes("secondary-p");
        click(&mut o, "launcher-menu-2");
        assert!(!read(&mut o, |l, _| l.menu_open()));
        assert_eq!(
            read(&mut o, |l, _| l.choice()).provider,
            Some(("codex".into(), String::new())),
            "the third entry"
        );
        click(&mut o, "launcher-launch-devin-");
        let body = o.fake.received().pop().expect("launched by click");
        assert_eq!(body["provider"], "devin");
        assert!(read(&mut o, |l, _| l.is_closed()));
    }

    /// Review M3: closing while the launch is in flight waits for it and
    /// keeps no draft of a task that launched.
    #[gpui::test]
    fn closing_in_flight_waits_and_keeps_no_launched_draft(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("Ship it");
        let launcher = o.launcher.clone();
        o.cx.update(|window, cx| {
            launcher.update(cx, |l, cx| {
                l.enter(false, window, cx);
                assert!(l.in_flight());
                l.close(window, cx);
                assert!(!l.is_closed(), "not before the reply");
            })
        });
        o.cx.run_until_parked();
        assert!(read(&mut o, |l, _| l.is_closed()));
        assert_eq!(o.fake.calls().launch, 1);
        assert_eq!(draft_file(&o), None, "launched: nothing kept");
    }

    #[gpui::test]
    fn a_rejected_launch_closed_in_flight_keeps_its_task(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.fake
            .fail_writes(Some(AgentsError::rejected(409, "queue_full", "full")));
        o.cx.simulate_input("Ship it later");
        let launcher = o.launcher.clone();
        o.cx.update(|window, cx| {
            launcher.update(cx, |l, cx| {
                l.enter(false, window, cx);
                l.close(window, cx);
            })
        });
        o.cx.run_until_parked();
        assert!(read(&mut o, |l, _| l.is_closed()));
        assert_eq!(drafts(&o).load(ctl::now()).unwrap().text, "Ship it later");
    }

    /// Review M4: a first read that fails (401/5xx) says so instead of
    /// "Connect an agent".
    #[gpui::test]
    fn a_failed_first_read_is_a_service_error(cx: &mut TestAppContext) {
        let o = open_with(cx, None, |fake| {
            fake.fail(Some(AgentsError::Refused("401 Unauthorized".into())))
        });
        o.cx.update(|window, _| window.refresh());
        o.cx.run_until_parked();
        assert!(o.cx.debug_bounds("launcher-service").is_some());
        assert!(o.cx.debug_bounds("launcher-no-provider").is_none());
    }

    /// Review M5: a local check's reason shows, before any secret choice.
    #[gpui::test]
    fn local_checks_come_before_the_secret_check(cx: &mut TestAppContext) {
        // No default workspace at the service: "No workspace" is real.
        let mut o = open_with(cx, None, |fake| {
            AgentsBackend::set_config(fake, &serde_json::json!({"agents": {"default_repo": ""}}))
                .unwrap()
        });
        o.cx.simulate_input("password=hunter2hunter2 for staging");
        o.cx.simulate_keystrokes("secondary-r up enter");
        assert_eq!(read(&mut o, |l, _| l.choice()).workspace, None);
        o.cx.simulate_keystrokes("enter");
        assert_eq!(read(&mut o, |l, _| l.findings()), 0, "no secret choices");
        assert!(read(&mut o, |l, _| l.error())
            .unwrap()
            .ends_with("Choose a workspace first."));
        assert_eq!(o.fake.calls().launch, 0);
    }

    #[test]
    fn the_window_is_focusable_where_it_runs() {
        let style = window_style();
        if cfg!(target_os = "linux") {
            assert_eq!(style.kind, WindowKind::Normal, "never an X11 notification");
            assert!(!style.rounded, "no corners over an opaque window");
        }
        assert_eq!(
            style.rounded,
            style.background == WindowBackgroundAppearance::Transparent
        );
    }

    #[gpui::test]
    fn a_failed_launch_stays_open_and_retries_with_the_same_id(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.fake.fail_writes(Some(AgentsError::rejected(
            409,
            "provider_not_ready",
            "not ready",
        )));
        o.cx.simulate_input("Try this");
        o.cx.simulate_keystrokes("enter");
        assert!(!read(&mut o, |l, _| l.is_closed()));
        assert_eq!(
            read(&mut o, |l, _| l.error()).as_deref(),
            Some("Claude isn't connected right now. Your task is kept here.")
        );
        o.fake.fail_writes(Some(AgentsError::Offline));
        o.cx.simulate_keystrokes("enter");
        assert_eq!(
            read(&mut o, |l, _| l.error()).as_deref(),
            Some(ctl::SERVICE_DOWN)
        );
        o.fake.fail_writes(None);
        o.cx.simulate_keystrokes("enter");
        assert!(read(&mut o, |l, _| l.is_closed()));
        let ids: Vec<String> = o
            .fake
            .received()
            .iter()
            .map(|b| b["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 1, "failed writes never reached the store");
        assert_eq!(o.fake.calls().launch, 3);
    }

    #[gpui::test]
    fn menus_choose_provider_and_workspace_by_keyboard(cx: &mut TestAppContext) {
        let mut o = open(cx);
        o.cx.simulate_input("x");
        o.cx.simulate_keystrokes("secondary-p down enter");
        assert_eq!(
            read(&mut o, |l, _| l.choice()).provider,
            Some(("devin".into(), String::new()))
        );
        assert_eq!(
            read(&mut o, |l, _| l.highlight()),
            Some(Key::Launch("devin".into(), String::new())),
            "the chosen provider leads"
        );
        // H2: the fixture's service has a default workspace (acme/web), so
        // the menu offers no "No workspace" (v1 would use the default
        // anyway): the first entry is the default, and it is what's sent.
        assert_eq!(
            read(&mut o, |l, _| l.menu_items(Menu::Workspace))[0].0,
            "acme/web \u{00b7} default"
        );
        o.cx.simulate_keystrokes("secondary-r down enter");
        assert_eq!(
            read(&mut o, |l, _| l.choice()).workspace.as_deref(),
            Some("acme/billing")
        );
        o.cx.simulate_keystrokes("secondary-r up enter");
        assert_eq!(
            read(&mut o, |l, _| l.choice()).workspace.as_deref(),
            Some("acme/web"),
            "never none while a default is set"
        );
        o.cx.simulate_keystrokes("secondary-r escape");
        assert!(
            !read(&mut o, |l, _| l.is_closed()),
            "Esc closes the menu first"
        );
        o.cx.simulate_keystrokes("enter");
        let body = o.fake.received().pop().unwrap();
        assert_eq!(body["provider"], "devin");
        assert_eq!(body["repo"], "acme/web", "the workspace used is named");
    }

    #[gpui::test]
    fn a_second_launch_toggles_this_one_closed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(model::LAUNCHER_DIR);
        let model::LauncherClaim::Owner(instance) = model::claim(&path, Signal::Toggle).unwrap()
        else {
            panic!()
        };
        let fake = Arc::new(FakeAgents::fixture());
        cx.update(|cx| {
            cx.set_global(Theme::light());
            bind_keys(cx);
        });
        let launch = LauncherLaunch {
            root: DataRoot::sandbox(dir.path()),
            backend: fake,
            instance: Some(instance),
            prefill: None,
            fixture: None,
        };
        let (launcher, cx) = cx.add_window_view(|window, cx| Launcher::new(launch, window, cx));
        cx.run_until_parked();
        assert!(matches!(
            model::claim(&path, Signal::Toggle).unwrap(),
            model::LauncherClaim::HandedOff
        ));
        let l = launcher.clone();
        cx.update(|window, cx| l.update(cx, |l, cx| l.poll_signal(window, cx)));
        assert!(launcher.read_with(cx, |l, _| l.is_closed()));
    }

    #[test]
    fn spawned_windows_keep_the_sandbox_and_carry_no_task() {
        let root = DataRoot::sandbox(std::path::Path::new("/tmp/sb"));
        assert_eq!(
            spawn_args(&root, &Spawn::Launcher { prefill: true }),
            [
                "--sandbox",
                "--data-dir",
                "/tmp/sb",
                "--agents-launcher",
                "--agent-prefill"
            ]
        );
        assert_eq!(
            spawn_args(
                &root,
                &Spawn::Agents {
                    session: Some("0123456789ab".into())
                }
            ),
            [
                "--sandbox",
                "--data-dir",
                "/tmp/sb",
                "--page",
                "agents",
                "--agent-session",
                "0123456789ab"
            ]
        );
        let bad = Spawn::Agents {
            session: Some("../x".into()),
        };
        assert_eq!(bad.args(), ["--page", "agents"], "an invalid id is dropped");
        // Every spawned argument list parses as a valid launch.
        for what in [
            Spawn::Launcher { prefill: false },
            Spawn::Launcher { prefill: true },
            Spawn::Agents { session: None },
            Spawn::Agents {
                session: Some("0123456789ab".into()),
            },
        ] {
            assert!(
                crate::args::parse(spawn_args(&root, &what), false).is_ok(),
                "{what:?}"
            );
        }
    }
}
