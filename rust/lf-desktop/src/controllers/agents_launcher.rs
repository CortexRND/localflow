//! The agent launcher's model (design sections 4, 5, 8, 10, 13), GPUI-free.
//!
//! - **Rows**: the grouped list under the task field, and the highlight
//!   that moves over it. The highlight is kept by identity (a provider, a
//!   session, a choice), so live updates never move it.
//! - **Choice**: the provider and workspace a launch uses, defaulted from
//!   `GET /api/agents/status` (`default_provider`, `default_repo`) and
//!   sticky for the launcher's life only (no settings are written).
//! - **Payload**: the exact [`LaunchRequest`] and the Sends line, both
//!   derived from the same choice.
//! - **Draft**: an unsent task kept for 10 minutes across close/reopen in
//!   one mode-0600 file (the launcher is its own short-lived process, so it
//!   can't stay in memory), deleted on launch and on expiry, never logged,
//!   never written to History.
//! - **Instance**: the launcher's own single-instance lock and hand-off
//!   file, apart from the Hub's: a second `--agents-launcher` closes an
//!   open launcher (toggle) and exits; Run again asks it to show a prefill.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::controllers::agents::{
    self, AgentsError, Kind, LaunchRequest, Payload, Provider, Readiness, Row, Store, UiState,
    Workspace,
};
use crate::controllers::agents_secrets::{self, Finding};

/// Session rows shown with an empty field (design: six at most).
pub const MAX_SESSION_ROWS: usize = 6;
const RUNNING_ROWS: usize = 3;
const RECENT_ROWS: usize = 2;
const MATCH_ROWS: usize = 3;
/// How long an unsent draft is restored (design section 4).
pub const DRAFT_TTL: f64 = 600.0;
/// Over this many characters a count shows at the field's edge.
pub const COUNT_FROM: usize = 1000;

pub const PLACEHOLDER: &str = "Describe a task for an agent…";
pub const ACCESSIBLE_NAME: &str = "Agent launcher";
pub const EMPTY_HINT: &str = "Type a task to launch";

// ------------------------------------------------------------------ rows ---

/// A launcher row. Rows are options, not containers: nothing inside a row
/// is separately clickable.
#[derive(Clone, Debug, PartialEq)]
// A launcher lists ten rows at most; boxing them buys nothing.
#[allow(clippy::large_enum_variant)]
pub enum Item {
    /// "Launch with {P}". `enabled` is false for an offline provider or one
    /// that needs sign-in (drawn, never highlighted).
    Launch {
        provider: Provider,
        enabled: bool,
        title: String,
        detail: String,
    },
    /// A session; Enter opens it in Agents.
    Session(Row),
    /// The secret check's choices (M3).
    Secret(SecretChoice),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretChoice {
    /// The default: the value is replaced by `[redacted]`.
    Redact,
    /// Opens a confirmation first.
    SendUnchanged,
}

/// What a highlight points at (stable across live updates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Launch(String, String),
    Session(String),
    Secret(SecretChoice),
}

impl Item {
    pub fn enabled(&self) -> bool {
        match self {
            Item::Launch { enabled, .. } => *enabled,
            _ => true,
        }
    }

    /// Can be reached with the arrows or a click and activated with Enter:
    /// every usable row, and a provider that needs sign-in (its reason
    /// says "open Agent settings"; Enter does that, never a launch). It is
    /// never the first highlight, so a launch's default row is unchanged.
    pub fn actionable(&self) -> bool {
        match self {
            Item::Launch {
                enabled, provider, ..
            } => *enabled || provider.readiness == Readiness::NeedsSignIn,
            _ => true,
        }
    }

    pub fn key(&self) -> Key {
        match self {
            Item::Launch { provider, .. } => {
                Key::Launch(provider.id.clone(), provider.profile.clone())
            }
            Item::Session(row) => Key::Session(row.session.id.clone()),
            Item::Secret(choice) => Key::Secret(*choice),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub heading: String,
    pub items: Vec<Item>,
}

/// A provider's name as listed: its label, plus the profile when another
/// listed provider has the same label ("Codex · staging").
pub fn provider_name(provider: &Provider, all: &[Provider]) -> String {
    let shared = all.iter().filter(|p| p.label == provider.label).count() > 1;
    if shared && !provider.profile.is_empty() {
        format!("{} \u{00b7} {}", provider.label, provider.profile)
    } else {
        provider.label.clone()
    }
}

/// Providers that get a Launch row: connected (enabled), and offline or
/// needing sign-in (disabled with the reason). Not-connected providers
/// have no row: they can't be used from here.
fn launchable(p: &Provider) -> bool {
    p.readiness != Readiness::NotConnected
}

/// The list for this field (design section 4, "List contents").
pub fn sections(
    store: &Store,
    rows: &[Row],
    query: &str,
    choice: &Choice,
    findings: &[Finding],
) -> Vec<Section> {
    if !findings.is_empty() {
        return vec![Section {
            heading: String::new(),
            items: vec![
                Item::Secret(SecretChoice::Redact),
                Item::Secret(SecretChoice::SendUnchanged),
            ],
        }];
    }
    let query = query.trim();
    if query.is_empty() {
        let mut out = Vec::new();
        let mut left = MAX_SESSION_ROWS;
        let mut take = |heading: &str, limit: usize, f: &dyn Fn(&Row) -> bool| {
            let items: Vec<Item> = rows
                .iter()
                .filter(|r| f(r))
                .take(limit.min(left))
                .cloned()
                .map(Item::Session)
                .collect();
            left -= items.len();
            if !items.is_empty() {
                out.push(Section {
                    heading: heading.into(),
                    items,
                });
            }
        };
        take("Needs you", MAX_SESSION_ROWS, &|r| {
            r.state == UiState::NeedsInput
        });
        take("Running", RUNNING_ROWS, &|r| {
            r.state != UiState::NeedsInput && !r.state.terminal()
        });
        take("Recent", RECENT_ROWS, &|r| r.state.terminal());
        return out;
    }
    let mut out = Vec::new();
    let mut providers: Vec<&Provider> = store.providers.iter().filter(|p| launchable(p)).collect();
    // The chosen provider first, then the rest in the service's order.
    providers.sort_by_key(|p| !choice.is(p));
    let launch: Vec<Item> = providers
        .into_iter()
        .map(|p| {
            let name = provider_name(p, &store.providers);
            let workspace = choice.workspace.clone();
            let detail = match p.readiness {
                Readiness::Connected if choice.is(p) => match &workspace {
                    Some(ws) => format!("Default provider \u{00b7} {ws}"),
                    None => "Default provider".into(),
                },
                Readiness::Connected => workspace.unwrap_or_else(|| "No workspace".into()),
                Readiness::NeedsSignIn => "Sign-in expired \u{2014} open Agent settings".into(),
                _ => "Can't be reached right now \u{00b7} Offline".into(),
            };
            Item::Launch {
                enabled: p.readiness == Readiness::Connected,
                title: format!("Launch with {name}"),
                detail,
                provider: p.clone(),
            }
        })
        .collect();
    if !launch.is_empty() {
        out.push(Section {
            heading: "Launch".into(),
            items: launch,
        });
    }
    let matching: Vec<Item> = rows
        .iter()
        .filter(|r| r.matches(query))
        .take(MATCH_ROWS)
        .cloned()
        .map(Item::Session)
        .collect();
    if !matching.is_empty() {
        out.push(Section {
            heading: format!("Sessions matching \u{201c}{query}\u{201d}"),
            items: matching,
        });
    }
    out
}

/// Every item, in order (what the highlight moves over).
pub fn flat(sections: &[Section]) -> Vec<&Item> {
    sections.iter().flat_map(|s| s.items.iter()).collect()
}

/// Where the highlight lands after the field changed: the first usable
/// row while there is text (or a blocking state), none while it's empty.
pub fn initial(sections: &[Section], typed: bool) -> Option<Key> {
    let items = flat(sections);
    let blocking = items.iter().any(|i| matches!(i, Item::Secret(_)));
    (typed || blocking)
        .then(|| items.into_iter().find(|i| i.enabled()).map(Item::key))
        .flatten()
}

/// The highlight kept across a live update: the same row if it is still
/// listed and usable, else the first usable row (with text), else none.
pub fn keep(sections: &[Section], highlight: Option<&Key>, typed: bool) -> Option<Key> {
    let items = flat(sections);
    match highlight {
        Some(key) if items.iter().any(|i| i.key() == *key && i.actionable()) => Some(key.clone()),
        _ => initial(sections, typed),
    }
}

/// `↓` / `↑`: the next usable row; `↑` from the first returns to none.
/// Disabled rows are skipped (they can't be highlighted), except one that
/// needs sign-in ([`Item::actionable`]).
pub fn step(sections: &[Section], highlight: Option<&Key>, down: bool) -> Option<Key> {
    let usable: Vec<Key> = flat(sections)
        .into_iter()
        .filter(|i| i.actionable())
        .map(Item::key)
        .collect();
    let at = highlight.and_then(|k| usable.iter().position(|u| u == k));
    match (at, down) {
        (None, true) => usable.first().cloned(),
        (None, false) => None,
        (Some(i), true) => usable.get(i + 1).or(usable.get(i)).cloned(),
        (Some(0), false) => None,
        (Some(i), false) => usable.get(i - 1).cloned(),
    }
}

/// The highlighted item, if any.
pub fn find<'a>(sections: &'a [Section], key: Option<&Key>) -> Option<&'a Item> {
    let key = key?;
    flat(sections).into_iter().find(|i| i.key() == *key)
}

// ---------------------------------------------------------------- choice ---

/// The provider and workspace a launch uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choice {
    /// `(id, profile)` of the chosen provider.
    pub provider: Option<(String, String)>,
    /// The workspace alias, when one is chosen.
    pub workspace: Option<String>,
}

impl Choice {
    pub fn is(&self, p: &Provider) -> bool {
        self.provider
            .as_ref()
            .is_some_and(|(id, profile)| *id == p.id && *profile == p.profile)
    }

    /// The default choice: the service's `default_provider` when connected,
    /// else the first connected provider; its `default_repo` when listed.
    /// A prefill (Run again) or an earlier pick wins when still usable.
    pub fn resolve(&mut self, store: &Store, prefer: Option<(&str, Option<&str>)>) {
        let connected = |p: &&Provider| p.readiness == Readiness::Connected;
        let usable = |id: &str, profile: Option<&str>| {
            store
                .providers
                .iter()
                .filter(connected)
                .find(|p| p.id == id && profile.is_none_or(|pr| p.profile == pr))
                .map(|p| (p.id.clone(), p.profile.clone()))
        };
        let keep = self
            .provider
            .as_ref()
            .and_then(|(id, profile)| usable(id, Some(profile)));
        let status = store.status.as_ref();
        self.provider = prefer
            .and_then(|(id, profile)| usable(id, profile))
            .or(keep)
            .or_else(|| {
                status
                    .and_then(|s| s.default_provider.as_deref())
                    .and_then(|id| usable(id, None))
            })
            .or_else(|| {
                store
                    .providers
                    .iter()
                    .find(connected)
                    .map(|p| (p.id.clone(), p.profile.clone()))
            });
        // Until the service's status is read, a prefilled workspace stands.
        let Some(status) = status else {
            return;
        };
        let listed = |alias: &String| status.repos.iter().any(|r| r.alias == *alias);
        if !self.workspace.as_ref().is_some_and(listed) {
            self.workspace = status.default_repo.clone().filter(listed);
        }
    }

    pub fn provider<'a>(&self, store: &'a Store) -> Option<&'a Provider> {
        store.providers.iter().find(|p| self.is(p))
    }
}

/// The Workspace menu's entries, in order: `(alias, usable)`, with `None`
/// for "No workspace". That entry is offered only while the service has no
/// listed `default_repo`: v1 can't express "none" once a default is set (an
/// omitted `repo` falls back to it at the service), so offering it would
/// make the Sends line claim less than is used.
pub fn workspace_options(status: Option<&agents::Status>) -> Vec<(Option<String>, bool)> {
    let Some(status) = status else {
        return vec![(None, true)];
    };
    let default_listed = status
        .default_repo
        .as_ref()
        .is_some_and(|d| status.repos.iter().any(|r| r.alias == *d));
    let mut out = Vec::new();
    if !default_listed {
        out.push((None, true));
    }
    out.extend(
        status
            .repos
            .iter()
            .map(|r| (Some(r.alias.clone()), r.exists)),
    );
    out
}

/// A Workspace menu entry's label.
pub fn workspace_label(alias: Option<&str>, status: Option<&agents::Status>) -> String {
    match alias {
        None => "No workspace".into(),
        Some(a) if status.and_then(|s| s.default_repo.as_deref()) == Some(a) => {
            format!("{a} \u{00b7} default")
        }
        Some(a) => a.to_string(),
    }
}

/// The provider a launch would use right now: the highlighted Launch row's,
/// else the chosen one.
pub fn active_provider<'a>(
    store: &'a Store,
    sections: &'a [Section],
    highlight: Option<&Key>,
    choice: &Choice,
) -> Option<&'a Provider> {
    match find(sections, highlight) {
        Some(Item::Launch { provider, .. }) => Some(provider),
        _ => choice.provider(store),
    }
}

/// What a launch with `provider` hands over, for the Sends line.
pub fn payload(provider: &Provider, all: &[Provider], workspace: Option<&str>) -> Payload {
    Payload {
        provider: provider_name(provider, all),
        workspace: workspace.map(|w| match provider.caps.kind {
            // A local agent works in the folder; nothing about it is sent.
            Kind::LocalProcess => Workspace::Local(w.to_string()),
            // v1 can't say whether a cloud provider is sent the alias.
            Kind::Cloud => Workspace::Named(w.to_string()),
        }),
        attachments: Vec::new(),
    }
}

/// The Sends line, live (design section 8). Blocking states and the
/// no-provider state say nothing has been sent.
pub fn sends_line(
    provider: Option<&Provider>,
    all: &[Provider],
    workspace: Option<&str>,
    simulated: bool,
    blocking: bool,
) -> String {
    if blocking {
        return agents::NOTHING_SENT.into();
    }
    if simulated {
        return agents::SANDBOX_SENDS.into();
    }
    match provider {
        Some(p) => agents::sends_line(&payload(p, all, workspace), false),
        None => agents::NOTHING_SENT.into(),
    }
}

// ---------------------------------------------------------------- launch ---

/// The request for a launch. `previous` is the last request this launcher
/// tried: an identical retry reuses its id (the runtime answers the same
/// session instead of starting a second), anything else gets a new one.
pub fn request(
    previous: Option<&LaunchRequest>,
    prompt: &str,
    provider: &Provider,
    workspace: Option<&str>,
) -> LaunchRequest {
    let mut next = LaunchRequest {
        id: String::new(),
        prompt: prompt.trim().to_string(),
        provider: provider.id.clone(),
        profile: (!provider.profile.is_empty()).then(|| provider.profile.clone()),
        repo: workspace.map(str::to_owned),
    };
    next.id = match previous {
        Some(p)
            if p.prompt == next.prompt
                && p.provider == next.provider
                && p.profile == next.profile
                && p.repo == next.repo =>
        {
            p.id.clone()
        }
        _ => agents::new_session_id(),
    };
    next
}

/// Checks made here before anything is sent; `Some(reason)` blocks.
pub fn local_check(
    prompt: &str,
    provider: &Provider,
    all: &[Provider],
    workspace: Option<&str>,
    max_chars: Option<u64>,
) -> Option<String> {
    let name = provider_name(provider, all);
    let n = prompt.trim().chars().count();
    if let Some(max) = max_chars.filter(|max| n as u64 > *max) {
        return Some(format!(
            "This task is longer than {name} accepts ({n} of {max} characters). Shorten it to launch."
        ));
    }
    if provider.caps.needs_cwd && workspace.is_none() {
        return Some(format!(
            "{name} works in a folder on {}. Choose a workspace first.",
            agents::this_computer()
        ));
    }
    None
}

/// A failed launch, as the banner says it. The task stays in the field.
pub fn launch_error(error: &AgentsError, provider: &str) -> String {
    match error.code() {
        Some("provider_not_ready") => {
            format!("{provider} isn't connected right now. Your task is kept here.")
        }
        Some("prompt_too_long") => {
            format!("This task is longer than {provider} accepts. Shorten it to launch.")
        }
        Some("queue_full") => {
            "Too many tasks are waiting at the agent service. Try again in a few seconds.".into()
        }
        Some("agents_disabled") => "Agents is turned off in LocalFlow's agent service.".into(),
        Some("id_conflict") => {
            "LocalFlow couldn't launch this task. Press Enter to try again.".into()
        }
        _ if *error == AgentsError::Offline => agents::SERVICE_DOWN.into(),
        _ => format!("{provider} didn't take this task. {}", error.reason())
            .trim_end()
            .to_string(),
    }
}

/// Whether a failed request's id may be replayed: never after the runtime
/// said the id is taken by another body.
pub fn replayable(error: &AgentsError) -> bool {
    error.code() != Some("id_conflict")
}

/// Every secret finding in the task, or none.
pub fn scan(task: &str) -> Vec<Finding> {
    agents_secrets::scan(task)
}

/// The footer's key hints for the current state: `(keys, word)`.
pub fn hints(typed: bool, blocking: bool, any_sessions: bool) -> Vec<(&'static str, &'static str)> {
    let open = if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    };
    let provider = if cfg!(target_os = "macos") {
        "cmd-p"
    } else {
        "ctrl-p"
    };
    if blocking {
        vec![("enter", "Choose"), ("escape", "Back to editing")]
    } else if typed {
        // Four hints fit the 640px footer with Ctrl/Shift caps; Esc still
        // closes (and is hinted in the other states).
        vec![
            ("enter", "Launch"),
            ("shift-enter", "New line"),
            (provider, "Provider"),
            (open, "Open Agents"),
        ]
    } else if any_sessions {
        vec![
            ("down", "Sessions"),
            (open, "Open Agents"),
            ("escape", "Close"),
        ]
    } else {
        vec![(open, "Open Agents"), ("escape", "Close")]
    }
}

/// A footer hint's keystroke (`"ctrl-enter"`) as a [`crate::ui::keys`] combo
/// of compact caps (`"Ctrl + Enter"`; `"⌘ + ↩"` on macOS). Off macOS the
/// caps are plain ASCII, so the mono keycap font never needs a fallback.
pub fn key_label(keystroke: &str) -> String {
    key_label_for(keystroke, cfg!(target_os = "macos"))
}

fn key_label_for(keystroke: &str, mac: bool) -> String {
    keystroke
        .split('-')
        .map(|part| {
            let name = match (part, mac) {
                ("cmd", true) => "⌘",
                ("ctrl", true) => "⌃",
                ("alt", true) => "⌥",
                ("shift", true) => "⇧",
                ("enter", true) => "↩",
                ("down", true) => "↓",
                ("up", true) => "↑",
                ("ctrl", false) => "Ctrl",
                ("alt", false) => "Alt",
                ("shift", false) => "Shift",
                ("enter", false) => "Enter",
                ("down", false) => "Down",
                ("up", false) => "Up",
                ("escape", _) => "Esc",
                _ => return part.to_uppercase(),
            };
            name.to_string()
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

// ----------------------------------------------------------------- draft ---

/// An unsent task (and the choice it was typed for).
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub text: String,
    pub provider: Option<(String, String)>,
    pub workspace: Option<String>,
    pub saved_at: f64,
}

/// The draft file: mode 0600, one JSON object, at most [`DRAFT_BYTES`].
pub struct DraftStore {
    path: PathBuf,
}

const DRAFT_BYTES: u64 = 128 * 1024;
pub const DRAFT_FILE: &str = "draft.json";

impl DraftStore {
    pub fn new(dir: &Path) -> Self {
        DraftStore {
            path: dir.join(DRAFT_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The draft saved within [`DRAFT_TTL`] of `now`; an expired or
    /// unreadable one is deleted.
    pub fn load(&self, now: f64) -> Option<Draft> {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| !m.file_type().is_file()) {
            return None;
        }
        let mut raw = Vec::new();
        std::fs::File::open(&self.path)
            .ok()?
            .take(DRAFT_BYTES)
            .read_to_end(&mut raw)
            .ok()?;
        let draft = serde_json::from_slice::<Value>(&raw).ok().and_then(|v| {
            let saved_at = v.get("saved_at")?.as_f64()?;
            let text = v.get("text")?.as_str()?.to_string();
            let provider = match (v["provider"].as_str(), v["profile"].as_str()) {
                (Some(id), profile) => Some((id.to_string(), profile.unwrap_or("").to_string())),
                _ => None,
            };
            Some(Draft {
                text,
                provider,
                workspace: v["workspace"].as_str().map(str::to_owned),
                saved_at,
            })
        });
        match draft {
            Some(d) if now - d.saved_at <= DRAFT_TTL && now - d.saved_at >= -60.0 => Some(d),
            _ => {
                self.clear();
                None
            }
        }
    }

    /// Keep `draft` (a blank one deletes the file). Errors are ignored: a
    /// draft is a convenience, and nothing about it is ever logged.
    pub fn save(&self, draft: &Draft) {
        if draft.text.trim().is_empty() {
            self.clear();
            return;
        }
        let value = json!({
            "text": draft.text,
            "provider": draft.provider.as_ref().map(|(id, _)| id),
            "profile": draft.provider.as_ref().map(|(_, p)| p),
            "workspace": draft.workspace,
            "saved_at": draft.saved_at,
        });
        let _ = write_private(&self.path, value.to_string().as_bytes());
    }

    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Write a file only this user can read, atomically (tmp + rename).
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::other("refusing a symbolic link"));
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// -------------------------------------------------------------- instance ---

/// The launcher's instance directory: beside the Hub's, never inside it,
/// so the launcher can't take (or wait on) the Hub's window lock.
pub const LAUNCHER_DIR: &str = "agents-launcher";
pub const LAUNCHER_LOCK: &str = "launcher.lock";
pub const LAUNCHER_REQUEST: &str = "launcher-request.json";
const SIGNAL_TTL: f64 = 20.0;

/// The launcher directory for a data root.
pub fn launcher_dir(root: &crate::data::DataRoot) -> PathBuf {
    match &root.sandbox_dir {
        Some(dir) => dir.join(LAUNCHER_DIR),
        None => root.localflow_dir.join(LAUNCHER_DIR),
    }
}

/// What a second launch asks of the open launcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// `--agents-launcher` again: close it.
    Toggle,
    /// Run again: show the prefill (now in the draft file) and stay open.
    Show,
}

impl Signal {
    fn name(self) -> &'static str {
        match self {
            Signal::Toggle => "toggle",
            Signal::Show => "show",
        }
    }
}

/// This process owns the launcher window.
pub struct LauncherInstance {
    _lock: lf_runtime::singleinstance::SingleInstance,
    dir: PathBuf,
    pending: std::cell::Cell<Option<Signal>>,
}

pub enum LauncherClaim {
    Owner(LauncherInstance),
    /// The open launcher got the signal; exit successfully.
    HandedOff,
}

fn check_dir(dir: &Path) -> std::io::Result<()> {
    for path in [
        dir.to_path_buf(),
        dir.join(LAUNCHER_LOCK),
        dir.join(LAUNCHER_REQUEST),
        dir.join(DRAFT_FILE),
    ] {
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "refusing to use {} for the launcher: it is a symbolic link",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn write_signal(dir: &Path, signal: Signal) -> bool {
    let value = json!({"signal": signal.name(), "ts": lf_ui::hub_route::system_now()});
    write_private(&dir.join(LAUNCHER_REQUEST), value.to_string().as_bytes()).is_ok()
}

fn consume_signal(dir: &Path) -> Option<Signal> {
    let path = dir.join(LAUNCHER_REQUEST);
    let claim = dir.join(format!("{LAUNCHER_REQUEST}.{}.claim", std::process::id()));
    std::fs::rename(&path, &claim).ok()?;
    let mut raw = Vec::new();
    let read = std::fs::File::open(&claim)
        .and_then(|f| f.take(4096).read_to_end(&mut raw))
        .is_ok();
    let _ = std::fs::remove_file(&claim);
    let value: Value = read.then(|| serde_json::from_slice(&raw).ok()).flatten()?;
    let age = lf_ui::hub_route::system_now() - value.get("ts")?.as_f64()?;
    if !(-5.0..=SIGNAL_TTL).contains(&age) {
        return None;
    }
    match value.get("signal")?.as_str()? {
        "toggle" => Some(Signal::Toggle),
        "show" => Some(Signal::Show),
        _ => None,
    }
}

impl LauncherInstance {
    /// The next signal from another launch, if any.
    pub fn take_signal(&self) -> Option<Signal> {
        self.pending.take().or_else(|| consume_signal(&self.dir))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Become the launcher, or hand `signal` to the open one. Race-free like
/// the Hub's claim: if the owner exits between our failed lock and the
/// signal write, the retry takes the lock and consumes the signal (a
/// toggle aimed at a launcher that already closed opens a new one).
pub fn claim(dir: &Path, signal: Signal) -> std::io::Result<LauncherClaim> {
    check_dir(dir)?;
    std::fs::create_dir_all(dir)?;
    let lock = dir.join(LAUNCHER_LOCK);
    let owner = |lock| LauncherInstance {
        _lock: lock,
        dir: dir.to_path_buf(),
        pending: std::cell::Cell::new(None),
    };
    if let Some(lock) = lf_runtime::singleinstance::acquire_at(&lock)? {
        // A signal left for a launcher that has since closed: consumed, not
        // deleted unread. A fresh Show (Run again) still applies here, so a
        // prefill written in that gap isn't lost; a Toggle aimed at the
        // closed launcher asked for exactly what this launch does (open
        // one), so it is dropped rather than closing this window.
        let instance = owner(lock);
        if let Some(Signal::Show) = consume_signal(dir) {
            instance.pending.set(Some(Signal::Show));
        }
        return Ok(LauncherClaim::Owner(instance));
    }
    if !write_signal(dir, signal) {
        return Err(std::io::Error::other(
            "couldn't reach the open agent launcher",
        ));
    }
    if let Some(lock) = lf_runtime::singleinstance::acquire_at(&lock)? {
        let instance = owner(lock);
        // Our own signal, consumed so it can't close the window we open.
        let _ = consume_signal(dir);
        return Ok(LauncherClaim::Owner(instance));
    }
    Ok(LauncherClaim::HandedOff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::agents::{
        parse_providers, AgentsBackend, SessionPage, Snapshot, Status,
    };
    use crate::sandbox::agents::{self as fake_ids, FakeAgents};

    fn store(fake: &FakeAgents) -> Store {
        let mut store = Store::default();
        let generation = store.begin();
        store.finish(generation, agents::load(fake), 0.0);
        store
    }

    fn chosen(store: &Store) -> Choice {
        let mut choice = Choice::default();
        choice.resolve(store, None);
        choice
    }

    #[test]
    fn an_empty_field_lists_needs_you_running_and_recent() {
        let fake = FakeAgents::fixture();
        let store = store(&fake);
        let rows = store.rows(fake_ids::FIXTURE_NOW);
        let sections = sections(&store, &rows, "", &chosen(&store), &[]);
        let shape: Vec<(&str, usize)> = sections
            .iter()
            .map(|s| (s.heading.as_str(), s.items.len()))
            .collect();
        assert_eq!(shape, [("Needs you", 1), ("Running", 3), ("Recent", 2)]);
        assert!(flat(&sections).len() <= MAX_SESSION_ROWS);
        assert_eq!(initial(&sections, false), None, "Enter does nothing");
        // ↓ highlights the first session; ↑ from there returns to none.
        let first = step(&sections, None, true);
        assert_eq!(first, Some(Key::Session(fake_ids::NEEDS_INPUT.into())));
        assert_eq!(step(&sections, first.as_ref(), false), None);
    }

    #[test]
    fn typed_text_lists_launch_rows_default_first_then_matches() {
        let fake = FakeAgents::fixture();
        let store = store(&fake);
        let rows = store.rows(fake_ids::FIXTURE_NOW);
        let choice = chosen(&store);
        assert_eq!(choice.provider, Some(("claude".into(), String::new())));
        assert_eq!(choice.workspace.as_deref(), Some("acme/web"));
        let sections = sections(&store, &rows, "release", &choice, &[]);
        assert_eq!(sections[0].heading, "Launch");
        let launch: Vec<(String, bool, String)> = sections[0]
            .items
            .iter()
            .map(|i| match i {
                Item::Launch {
                    title,
                    enabled,
                    detail,
                    ..
                } => (title.clone(), *enabled, detail.clone()),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            launch,
            [
                (
                    "Launch with Claude".into(),
                    true,
                    "Default provider · acme/web".into()
                ),
                ("Launch with Devin".into(), true, "acme/web".into()),
                ("Launch with Codex".into(), true, "acme/web".into()),
                (
                    "Launch with Codex · staging".into(),
                    false,
                    "Can't be reached right now · Offline".into()
                ),
            ]
        );
        assert_eq!(sections[1].heading, "Sessions matching “release”");
        assert_eq!(sections[1].items.len(), 1);
        let first = initial(&sections, true);
        assert_eq!(first, Some(Key::Launch("claude".into(), String::new())));
        // Arrows switch providers and skip the offline row.
        let devin = step(&sections, first.as_ref(), true);
        let codex = step(&sections, devin.as_ref(), true);
        let next = step(&sections, codex.as_ref(), true);
        assert_eq!(
            next,
            Some(Key::Session(fake_ids::CODEX_RUNNING.into())),
            "offline skipped"
        );
        assert_eq!(
            step(&sections, next.as_ref(), true),
            next,
            "stays at the end"
        );
        // The Sends line follows the highlighted provider, live.
        let sends = |key: Option<&Key>| {
            let p = active_provider(&store, &sections, key, &choice);
            sends_line(
                p,
                &store.providers,
                choice.workspace.as_deref(),
                false,
                false,
            )
        };
        assert_eq!(
            sends(first.as_ref()),
            format!(
                "Sends to Claude: your task only. It runs on {} in acme/web.",
                agents::this_computer()
            )
        );
        assert_eq!(
            sends(devin.as_ref()),
            "Sends to Devin: your task. Workspace: acme/web."
        );
        assert_eq!(
            sends_line(None, &[], None, true, false),
            "Sandbox: nothing is sent."
        );
        assert_eq!(
            sends_line(None, &[], None, false, true),
            "Nothing has been sent."
        );
    }

    #[test]
    fn live_updates_keep_the_highlight_unless_its_row_goes() {
        let fake = FakeAgents::fixture();
        let mut store = store(&fake);
        let rows = store.rows(0.0);
        let choice = chosen(&store);
        let before = sections(&store, &rows, "", &choice, &[]);
        let running = Some(Key::Session(fake_ids::CLAUDE_RUNNING.into()));
        // A new Needs input row appears above: the highlight stays put.
        fake.set_state(fake_ids::CODEX_RUNNING, "waiting_input");
        store.apply_changes(fake.changes(store.revision, 0).unwrap());
        let after = sections(&store, &store.rows(0.0), "", &choice, &[]);
        assert_ne!(flat(&before).len(), 0);
        assert_eq!(after[0].items.len(), 2, "two need you now");
        assert_eq!(keep(&after, running.as_ref(), false), running);
        // An offline provider's row can't keep the highlight.
        let typed = sections(&store, &store.rows(0.0), "x", &choice, &[]);
        let staging = Key::Launch("codex".into(), "staging".into());
        assert_eq!(
            keep(&typed, Some(&staging), true),
            Some(Key::Launch("claude".into(), String::new()))
        );
    }

    #[test]
    fn no_connected_provider_offers_no_launch() {
        let fake = FakeAgents::fixture();
        fake.disconnect_all();
        let store = store(&fake);
        let mut choice = Choice::default();
        choice.resolve(&store, None);
        assert_eq!(choice.provider, None);
        let sections = sections(&store, &[], "do it", &choice, &[]);
        assert!(sections.is_empty(), "{sections:?}");
        assert_eq!(initial(&sections, true), None);
        assert_eq!(
            sends_line(
                choice.provider(&store),
                &store.providers,
                None,
                false,
                false
            ),
            "Nothing has been sent."
        );
        // Needs sign-in: listed, disabled, with the reason.
        let fake = FakeAgents::fixture();
        fake.set_readiness("devin", "auth_failed");
        let signed_out = super::tests::store(&fake);
        let listed = super::sections(&signed_out, &[], "do it", &chosen(&signed_out), &[]);
        assert!(flat(&listed).iter().any(|i| matches!(i,
            Item::Launch { enabled: false, detail, .. } if detail == "Sign-in expired — open Agent settings")));
        // It can be reached (Enter opens Agent settings) but is never the
        // first highlight; an offline one can't be reached at all.
        let devin = Key::Launch("devin".into(), String::new());
        assert_ne!(initial(&listed, true), Some(devin.clone()));
        let mut at = initial(&listed, true);
        let mut reached = false;
        for _ in 0..8 {
            at = step(&listed, at.as_ref(), true);
            reached |= at.as_ref() == Some(&devin);
        }
        assert!(reached, "the arrows reach the sign-in row");
        assert_eq!(keep(&listed, Some(&devin), true), Some(devin));
        assert!(!flat(&listed).iter().any(|i| i.actionable()
            && !i.enabled()
            && i.key() == Key::Launch("codex".into(), "staging".into())));
    }

    /// H2: "No workspace" is offered only when the runtime would really use
    /// none; with a default set, the default is the first entry and the
    /// Sends line names it.
    #[test]
    fn no_workspace_is_offered_only_without_a_default() {
        let with_default = agents::parse_status(&serde_json::json!({
            "api_version": 1, "enabled": true, "default_repo": "web",
            "repos": [{"alias": "web", "exists": true}, {"alias": "api", "exists": false}],
        }));
        assert_eq!(
            workspace_options(Some(&with_default)),
            [(Some("web".into()), true), (Some("api".into()), false)]
        );
        assert_eq!(
            workspace_label(Some("web"), Some(&with_default)),
            "web \u{00b7} default"
        );
        let mut none = with_default.clone();
        none.default_repo = None;
        assert_eq!(workspace_options(Some(&none))[0], (None, true));
        assert_eq!(workspace_label(None, Some(&none)), "No workspace");
        // A default that isn't listed can't be used either: none is real.
        let mut stale = with_default.clone();
        stale.default_repo = Some("gone".into());
        assert_eq!(workspace_options(Some(&stale))[0], (None, true));
        // The resolved choice with a default is the default, never none,
        // so the disclosure names it.
        let fake = FakeAgents::fixture();
        let store = super::tests::store(&fake);
        let choice = chosen(&store);
        assert_eq!(choice.workspace.as_deref(), Some("acme/web"));
        let line = sends_line(
            choice.provider(&store),
            &store.providers,
            choice.workspace.as_deref(),
            false,
            false,
        );
        assert!(line.contains("acme/web"), "{line}");
        assert!(!line.ends_with("your task only."), "{line}");
    }

    #[test]
    fn defaults_fall_back_and_a_prefill_wins() {
        let fake = FakeAgents::fixture();
        fake.set_readiness("claude", "unreachable");
        let store = store(&fake);
        let choice = chosen(&store);
        assert_eq!(
            choice.provider,
            Some(("devin".into(), String::new())),
            "first ready"
        );
        let mut prefilled = Choice {
            workspace: Some("acme/api".into()),
            ..Default::default()
        };
        prefilled.resolve(&store, Some(("codex", Some(""))));
        assert_eq!(prefilled.provider, Some(("codex".into(), String::new())));
        assert_eq!(
            prefilled.workspace.as_deref(),
            Some("acme/api"),
            "kept: listed"
        );
        let mut unlisted = Choice {
            workspace: Some("gone/repo".into()),
            ..Default::default()
        };
        unlisted.resolve(&store, None);
        assert_eq!(
            unlisted.workspace.as_deref(),
            Some("acme/web"),
            "back to the default"
        );
        // A service with no status yet: no workspace, first connected.
        let mut bare = Store::default();
        let g = bare.begin();
        bare.finish(
            g,
            Ok(Snapshot {
                status: Status::default(),
                providers: parse_providers(&json!({"providers": [
                    {"id": "p", "profile": "", "label": "P", "capabilities": {},
                     "readiness": {"ready": true, "code": "ok"}}]})),
                page: SessionPage::default(),
            }),
            0.0,
        );
        let c = chosen(&bare);
        assert_eq!((c.provider.unwrap().0.as_str(), c.workspace), ("p", None));
    }

    #[test]
    fn requests_are_exact_and_retries_reuse_the_id() {
        let fake = FakeAgents::fixture();
        let store = store(&fake);
        let claude = store.provider("claude", "").unwrap();
        let first = request(None, "  Add retry backoff \n", claude, Some("acme/web"));
        assert!(agents::is_session_id(&first.id));
        assert_eq!(first.prompt, "Add retry backoff");
        assert_eq!(first.profile, None, "no profile for a provider without one");
        let body = first.body();
        assert_eq!(
            body,
            json!({"prompt": "Add retry backoff", "provider": "claude", "id": first.id,
                   "repo": "acme/web", "origin": "api"})
        );
        let retry = request(Some(&first), "Add retry backoff", claude, Some("acme/web"));
        assert_eq!(retry.id, first.id, "an identical retry is replay-safe");
        let edited = request(
            Some(&first),
            "Add retry backoff now",
            claude,
            Some("acme/web"),
        );
        assert_ne!(edited.id, first.id);
        let staging = store.provider("codex", "staging").unwrap();
        let with_profile = request(None, "x", staging, None);
        assert_eq!(with_profile.profile.as_deref(), Some("staging"));
        assert!(!with_profile
            .body()
            .as_object()
            .unwrap()
            .contains_key("repo"));
        // Local checks block before anything is sent.
        assert!(local_check("x", claude, &store.providers, None, None)
            .unwrap()
            .ends_with("Choose a workspace first."));
        assert_eq!(
            local_check(&"y".repeat(30), claude, &store.providers, Some("acme/web"), Some(20))
                .as_deref(),
            Some("This task is longer than Claude accepts (30 of 20 characters). Shorten it to launch.")
        );
        assert_eq!(
            local_check("x", claude, &store.providers, Some("acme/web"), Some(20)),
            None
        );
    }

    #[test]
    fn launch_errors_are_safe_copy() {
        let r = |code: &str| AgentsError::rejected(409, code, "detail token=abcdefghijklmnop");
        assert_eq!(
            launch_error(&r("provider_not_ready"), "Devin"),
            "Devin isn't connected right now. Your task is kept here."
        );
        assert_eq!(
            launch_error(&AgentsError::Offline, "Devin"),
            agents::SERVICE_DOWN
        );
        let other = launch_error(&r("validation"), "Devin");
        assert!(
            other.starts_with("Devin didn't take this task. Detail token="),
            "{other}"
        );
        assert!(!other.contains("abcdefghijklmnop"));
        assert!(!replayable(&r("id_conflict")) && replayable(&AgentsError::Offline));
        assert_eq!(hints(false, true, true)[0], ("enter", "Choose"));
        assert_eq!(hints(true, false, true)[0], ("enter", "Launch"));
    }

    #[test]
    fn footer_keys_are_separate_compact_caps() {
        assert_eq!(key_label_for("ctrl-enter", false), "Ctrl + Enter");
        assert_eq!(key_label_for("shift-enter", false), "Shift + Enter");
        assert_eq!(key_label_for("ctrl-p", false), "Ctrl + P");
        assert_eq!(key_label_for("escape", false), "Esc");
        assert_eq!(key_label_for("down", false), "Down");
        assert_eq!(key_label_for("down", true), "↓");
        assert_eq!(key_label_for("cmd-enter", true), "⌘ + ↩");
        assert_eq!(key_label_for("shift-enter", true), "⇧ + ↩");
        assert_eq!(key_label_for("cmd-p", true), "⌘ + P");
        // Every hint in every state renders as known caps, never a raw keystroke.
        for (typed, blocking, any) in [
            (true, false, true),
            (false, true, true),
            (false, false, true),
            (false, false, false),
        ] {
            for (keys, _) in hints(typed, blocking, any) {
                for mac in [false, true] {
                    let label = key_label_for(keys, mac);
                    let raw = label.contains('-')
                        || label
                            .split(" + ")
                            .any(|cap| cap.starts_with(char::is_lowercase));
                    assert!(!raw, "{label}");
                    assert!(mac || label.is_ascii(), "{label}");
                }
            }
        }
    }

    #[test]
    fn a_secret_blocks_with_two_choices_redact_first() {
        let fake = FakeAgents::fixture();
        let store = store(&fake);
        let findings = scan("Use password=hunter2hunter2 for the staging box");
        let s = sections(&store, &[], "x", &chosen(&store), &findings);
        assert_eq!(
            flat(&s),
            [
                &Item::Secret(SecretChoice::Redact),
                &Item::Secret(SecretChoice::SendUnchanged)
            ]
        );
        assert_eq!(
            initial(&s, false),
            Some(Key::Secret(SecretChoice::Redact)),
            "safest first"
        );
    }

    #[test]
    fn drafts_expire_and_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let drafts = DraftStore::new(dir.path());
        assert_eq!(drafts.load(100.0), None);
        let draft = Draft {
            text: "Half a task".into(),
            provider: Some(("devin".into(), String::new())),
            workspace: Some("acme/web".into()),
            saved_at: 1000.0,
        };
        drafts.save(&draft);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(drafts.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(drafts.load(1000.0 + DRAFT_TTL), Some(draft.clone()));
        assert_eq!(drafts.load(1001.0 + DRAFT_TTL), None, "expired");
        assert!(!drafts.path().exists(), "and deleted");
        drafts.save(&draft);
        drafts.save(&Draft {
            text: "   ".into(),
            ..draft.clone()
        });
        assert!(!drafts.path().exists(), "a blank draft deletes");
        std::fs::write(drafts.path(), "{garbage").unwrap();
        assert_eq!(drafts.load(1000.0), None);
        assert!(!drafts.path().exists());
    }

    #[test]
    fn a_second_launch_toggles_the_open_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LAUNCHER_DIR);
        let LauncherClaim::Owner(owner) = claim(&path, Signal::Toggle).unwrap() else {
            panic!("first launch owns");
        };
        assert_eq!(owner.take_signal(), None);
        assert!(matches!(
            claim(&path, Signal::Toggle).unwrap(),
            LauncherClaim::HandedOff
        ));
        assert_eq!(owner.take_signal(), Some(Signal::Toggle));
        assert_eq!(owner.take_signal(), None, "consumed once");
        assert!(matches!(
            claim(&path, Signal::Show).unwrap(),
            LauncherClaim::HandedOff
        ));
        assert_eq!(owner.take_signal(), Some(Signal::Show));
        // The Hub's lock is elsewhere and untouched.
        let hub = dir.path().join("desktop-instance");
        let crate::controllers::instance::Claim::Owner(_hub) =
            crate::controllers::instance::claim(&hub, None).unwrap()
        else {
            panic!("the Hub opens beside a launcher");
        };
        drop(owner);
        // A stale signal never closes the next launcher.
        assert!(write_signal(&path, Signal::Toggle));
        let LauncherClaim::Owner(next) = claim(&path, Signal::Toggle).unwrap() else {
            panic!()
        };
        assert_eq!(next.take_signal(), None);
        assert!(!path.join(LAUNCHER_REQUEST).exists(), "consumed");
        drop(next);
        // A Run again prefill written as the old launcher closed is kept.
        assert!(write_signal(&path, Signal::Show));
        let LauncherClaim::Owner(next) = claim(&path, Signal::Toggle).unwrap() else {
            panic!()
        };
        assert_eq!(next.take_signal(), Some(Signal::Show));
        assert_eq!(next.take_signal(), None);
        // A symlinked directory is refused.
        #[cfg(unix)]
        {
            let evil = dir.path().join("evil");
            std::os::unix::fs::symlink(&path, &evil).unwrap();
            assert!(claim(&evil, Signal::Toggle).is_err());
        }
    }
}
