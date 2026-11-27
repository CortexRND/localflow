//! Agents: the provider-neutral session model behind the Agents page (and,
//! later, the launcher), read against `agents-api v1`.
//!
//! GPUI-free. Providers are data: a name, a monogram and a capability set;
//! nothing here (or in the page) branches on a provider's id. Replies are
//! read through an allow-list at parse time: only the fields the page draws
//! are kept, provider-supplied prose is masked for credential-like strings
//! before it is stored, and nothing here ever logs.
//!
//! State mapping (`ui-contract-reconciliation.md` section 3) is exact:
//! `queued|starting` → Starting, `running` → Running, `waiting_input` →
//! Needs input, `cancelling` → Stopping, `completed` → Completed,
//! `failed|timed_out` → Failed, `cancelled` → Canceled. v1 has no top-level
//! unknown state (the runtime keeps the last known one), so Unknown is only
//! shown when a reply carries a state string outside that list, or none at
//! all; it is never coerced to the nearest state. Disconnected is not a
//! session state: it is LocalFlow saying a non-terminal session's state is
//! stale (the service stopped answering for longer than [`GRACE_SECS`], or
//! the session's provider reports itself unreachable).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::controllers::assistant::mask_secrets;
use crate::theme::Tone;

/// How long `/changes` may fail before non-terminal rows show Disconnected
/// (reconciliation answer 3), so a blip doesn't flash every row.
pub const GRACE_SECS: f64 = 20.0;
/// A session title derived from its task: the first line, this long at most.
pub const TITLE_CHARS: usize = 80;
pub const UNTITLED: &str = "Untitled task";
/// Longest provider-supplied text kept (titles, reasons, raw states).
const TEXT_CHARS: usize = 400;
const RAW_STATE_CHARS: usize = 40;

// --------------------------------------------------------------- states ---

/// A session's `state` exactly as the runtime reported it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RuntimeState {
    Queued,
    Starting,
    Running,
    WaitingInput,
    Cancelling,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    /// A state string outside the v1 list (kept, trimmed and masked).
    Unrecognised(String),
    /// No usable `state` field.
    #[default]
    Missing,
}

impl RuntimeState {
    pub fn parse(value: Option<&Value>) -> Self {
        let Some(raw) = value.and_then(Value::as_str).map(str::trim) else {
            return RuntimeState::Missing;
        };
        match raw {
            "" => RuntimeState::Missing,
            "queued" => RuntimeState::Queued,
            "starting" => RuntimeState::Starting,
            "running" => RuntimeState::Running,
            "waiting_input" => RuntimeState::WaitingInput,
            "cancelling" => RuntimeState::Cancelling,
            "completed" => RuntimeState::Completed,
            "failed" => RuntimeState::Failed,
            "timed_out" => RuntimeState::TimedOut,
            "cancelled" => RuntimeState::Cancelled,
            other => RuntimeState::Unrecognised(clean(&mask_secrets(other), RAW_STATE_CHARS)),
        }
    }

    pub fn terminal(&self) -> bool {
        matches!(
            self,
            RuntimeState::Completed
                | RuntimeState::Failed
                | RuntimeState::TimedOut
                | RuntimeState::Cancelled
        )
    }

    /// The UI state while the information is fresh (no Disconnected).
    pub fn ui(&self) -> UiState {
        match self {
            RuntimeState::Queued | RuntimeState::Starting => UiState::Starting,
            RuntimeState::Running => UiState::Running,
            RuntimeState::WaitingInput => UiState::NeedsInput,
            RuntimeState::Cancelling => UiState::Stopping,
            RuntimeState::Completed => UiState::Completed,
            RuntimeState::Failed | RuntimeState::TimedOut => UiState::Failed,
            RuntimeState::Cancelled => UiState::Canceled,
            RuntimeState::Unrecognised(_) | RuntimeState::Missing => UiState::Unknown,
        }
    }
}

/// What a session row shows: glyph + word + tone (never colour alone).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum UiState {
    Starting,
    Running,
    NeedsInput,
    /// `cancelling`: a stop was asked for and is under way.
    Stopping,
    Completed,
    Failed,
    Canceled,
    Disconnected,
    Unknown,
}

impl UiState {
    pub const ALL: [UiState; 9] = [
        UiState::Starting,
        UiState::Running,
        UiState::NeedsInput,
        UiState::Stopping,
        UiState::Completed,
        UiState::Failed,
        UiState::Canceled,
        UiState::Disconnected,
        UiState::Unknown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            UiState::Starting => "Starting",
            UiState::Running => "Running",
            UiState::NeedsInput => "Needs input",
            UiState::Stopping => "Stopping",
            UiState::Completed => "Completed",
            UiState::Failed => "Failed",
            UiState::Canceled => "Canceled",
            UiState::Disconnected => "Disconnected",
            UiState::Unknown => "Unknown",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            UiState::Starting | UiState::Running | UiState::Stopping => Tone::Info,
            UiState::NeedsInput => Tone::Warning,
            UiState::Completed => Tone::Success,
            UiState::Failed => Tone::Error,
            UiState::Canceled | UiState::Disconnected | UiState::Unknown => Tone::Neutral,
        }
    }

    /// Lucide icon name (embedded in `ui::assets`).
    pub fn icon(self) -> &'static str {
        match self {
            UiState::Starting | UiState::Running | UiState::Stopping => "loader-circle",
            UiState::NeedsInput => "message-square-dot",
            UiState::Completed => "circle-check",
            UiState::Failed => "circle-alert",
            UiState::Canceled => "ban",
            UiState::Disconnected => "cloud-off",
            UiState::Unknown => "circle-help",
        }
    }

    pub fn terminal(self) -> bool {
        matches!(
            self,
            UiState::Completed | UiState::Failed | UiState::Canceled
        )
    }

    pub fn group(self) -> Group {
        match self {
            UiState::NeedsInput => Group::NeedsYou,
            s if s.terminal() => Group::Earlier,
            _ => Group::Running,
        }
    }
}

/// The UI state of a session given what LocalFlow knows about its
/// connection: terminal states are final facts and never go stale; any other
/// state shows Disconnected while it can't be refreshed.
pub fn ui_state(runtime: &RuntimeState, stale: bool) -> UiState {
    if stale && !runtime.terminal() {
        UiState::Disconnected
    } else {
        runtime.ui()
    }
}

/// List groups, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    NeedsYou,
    Running,
    Earlier,
}

impl Group {
    pub fn heading(self) -> &'static str {
        match self {
            Group::NeedsYou => "Needs you",
            Group::Running => "Running",
            Group::Earlier => "Earlier",
        }
    }
}

/// The page's segmented filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Active,
    NeedsYou,
    Done,
}

impl Filter {
    pub const ALL: [Filter; 4] = [Filter::All, Filter::Active, Filter::NeedsYou, Filter::Done];

    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Active => "Active",
            Filter::NeedsYou => "Needs you",
            Filter::Done => "Done",
        }
    }

    pub fn index(self) -> usize {
        Filter::ALL.iter().position(|f| *f == self).unwrap_or(0)
    }

    pub fn matches(self, state: UiState) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => !state.terminal(),
            Filter::NeedsYou => state == UiState::NeedsInput,
            Filter::Done => state.terminal(),
        }
    }
}

// ------------------------------------------------------------ providers ---

/// `capabilities.kind`: where the agent runs (switches local/cloud copy).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// A supervised process on this computer.
    LocalProcess,
    /// A remote service.
    #[default]
    Cloud,
}

/// A provider's optional capabilities. Missing or non-boolean fields are
/// absent (the control is not drawn), never assumed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub kind: Kind,
    pub resume: bool,
    pub live_message: bool,
    pub cancel: bool,
    pub streaming: bool,
    pub needs_cwd: bool,
    pub reattach: bool,
}

impl Capabilities {
    pub fn parse(value: Option<&Value>) -> Self {
        let flag = |name: &str| {
            value
                .and_then(|v| v.get(name))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        Capabilities {
            // Anything but an explicit local process is treated as remote,
            // so copy never claims "nothing leaves this computer" by guess.
            kind: match value.and_then(|v| v.get("kind")).and_then(Value::as_str) {
                Some("local_process") => Kind::LocalProcess,
                _ => Kind::Cloud,
            },
            resume: flag("resume"),
            live_message: flag("live_message"),
            cancel: flag("cancel"),
            streaming: flag("streaming"),
            needs_cwd: flag("needs_cwd"),
            reattach: flag("reattach"),
        }
    }
}

/// `readiness` as the four cases the UI distinguishes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Readiness {
    Connected,
    #[default]
    NotConnected,
    NeedsSignIn,
    Offline,
}

impl Readiness {
    pub fn parse(value: Option<&Value>) -> Self {
        let ready = value
            .and_then(|v| v.get("ready"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match value.and_then(|v| v.get("code")).and_then(Value::as_str) {
            Some("ok") if ready => Readiness::Connected,
            Some("auth_failed") => Readiness::NeedsSignIn,
            Some("unreachable") => Readiness::Offline,
            // `missing_secret|missing_binary|missing_wrapper|disabled`, an
            // unknown code, or `ok` without `ready`: not usable.
            _ => Readiness::NotConnected,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Readiness::Connected => "Connected",
            Readiness::NotConnected => "Not connected",
            Readiness::NeedsSignIn => "Needs sign-in",
            Readiness::Offline => "Offline",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: String,
    pub profile: String,
    pub label: String,
    pub caps: Capabilities,
    pub readiness: Readiness,
}

impl Provider {
    /// The one-letter tile drawn for this provider (no brand art).
    pub fn monogram(&self) -> String {
        monogram(&self.label)
    }
}

/// The first letter of a display name, upper-cased ("?" when none).
pub fn monogram(label: &str) -> String {
    label
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

/// `GET /api/agents/providers`.
pub fn parse_providers(reply: &Value) -> Vec<Provider> {
    reply
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = text(item.get("id"), 80)?;
            let label = text(item.get("label"), 80).unwrap_or_else(|| id.clone());
            Some(Provider {
                profile: text(item.get("profile"), 80).unwrap_or_default(),
                caps: Capabilities::parse(item.get("capabilities")),
                readiness: Readiness::parse(item.get("readiness")),
                label,
                id,
            })
        })
        .collect()
}

// --------------------------------------------------------------- status ---

/// The service's own session counts (`counts`), over every session it
/// keeps, not just the pages read. The three buckets partition the store,
/// so `active` covers every non-terminal state except `queued` (running,
/// starting, waiting for input, cancelling).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub active: u64,
    pub queued: u64,
    pub terminal: u64,
}

/// `counts` when the reply has one with at least one number in it.
pub fn parse_counts(reply: &Value) -> Option<Counts> {
    let counts = reply.get("counts")?;
    let n = |name: &str| counts.get(name).and_then(Value::as_u64);
    let (active, queued, terminal) = (n("active"), n("queued"), n("terminal"));
    (active.is_some() || queued.is_some() || terminal.is_some()).then(|| Counts {
        active: active.unwrap_or(0),
        queued: queued.unwrap_or(0),
        terminal: terminal.unwrap_or(0),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub api_version: u64,
    /// `agents.enabled`: the feature is on at the service.
    pub enabled: bool,
    /// `agents.sandbox`: the service runs mock providers; nothing is sent.
    pub sandbox: bool,
    pub max_prompt_chars: Option<u64>,
    pub counts: Option<Counts>,
    pub revision: u64,
    /// `default_provider` (`""` → `None`).
    pub default_provider: Option<String>,
    /// `default_repo`: a workspace alias (`""` → `None`).
    pub default_repo: Option<String>,
    /// `repos`: the configured workspaces, aliases only.
    pub repos: Vec<Repo>,
}

/// One configured workspace (`repos[]`). Its local path is not kept: only
/// the alias is ever shown or sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Repo {
    pub alias: String,
    /// The folder exists on this computer.
    pub exists: bool,
}

/// `GET /api/agents/status`. Repository paths are not kept (only aliases
/// are ever shown or sent).
pub fn parse_status(reply: &Value) -> Status {
    Status {
        default_provider: text(reply.get("default_provider"), 80),
        default_repo: text(reply.get("default_repo"), 120),
        repos: reply
            .get("repos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some(Repo {
                    alias: text(r.get("alias"), 120)?,
                    exists: r.get("exists").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect(),
        api_version: reply
            .get("api_version")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        enabled: reply
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        sandbox: reply
            .get("sandbox")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        max_prompt_chars: reply
            .get("limits")
            .and_then(|l| l.get("max_prompt_chars"))
            .and_then(Value::as_u64),
        counts: parse_counts(reply),
        revision: reply.get("revision").and_then(Value::as_u64).unwrap_or(0),
    }
}

/// Whether the Agents page is shown: the service speaks v1 and the feature
/// is on there (`agents.enabled`). A 404 (an older or Rust server) or any
/// other answer hides it.
pub fn available(status: &Result<Status, AgentsError>) -> bool {
    matches!(status, Ok(s) if s.enabled && s.api_version == 1)
}

/// Nothing a session's receipt lists actually left: the in-process fake,
/// or a real service running mock providers (`agents.sandbox`).
pub fn simulated(in_process: bool, status: Option<&Status>) -> bool {
    in_process || status.is_some_and(|s| s.sandbox)
}

/// What a status probe means for the shell's Agents gate: `Some(open)` when
/// the answer is decisive, `None` to keep the last answer. Only a 404 (no
/// agents API) or a parsed reply decides; a transient failure (nothing
/// answering, a 401 or 5xx, a malformed body) never closes the gate, so one
/// bad reply can't drop the page and send the user Home.
pub fn gate(status: &Result<Status, AgentsError>) -> Option<bool> {
    match status {
        Ok(_) => Some(available(status)),
        Err(AgentsError::Absent) => Some(false),
        Err(AgentsError::Offline | AgentsError::Refused(_) | AgentsError::Rejected(_)) => None,
    }
}

// ------------------------------------------------------------- sessions ---

/// `origin`: who started the session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Origin {
    /// `api`: LocalFlow's own windows (and other API clients).
    #[default]
    Api,
    Cli,
    /// A spoken hot command.
    HotCommand,
    /// A value outside the v1 list (or none).
    Other,
}

impl Origin {
    fn parse(value: Option<&Value>) -> Self {
        match value.and_then(Value::as_str) {
            Some("api") => Origin::Api,
            Some("cli") => Origin::Cli,
            Some("hot_command") => Origin::HotCommand,
            _ => Origin::Other,
        }
    }
}

/// `error: {code, message} | null`. The message is provider prose: masked
/// over the whole value before it is cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionError {
    /// The v1 `ErrorCode` slug (`spawn_failed`, `rate_limited`, …).
    pub code: String,
    pub message: Option<String>,
}

/// `usage` (numbers only, never shown as provider prose).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub turns: Option<u64>,
}

/// One session, allow-listed against the v1 `Session` / `SessionSummary`
/// shapes. Summaries carry no `prompt` or `result_text` (a 200-character
/// `result_preview` instead); both arrive with the detail read and are kept
/// across later summary updates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub id: String,
    pub provider: String,
    pub profile: String,
    pub runtime: RuntimeState,
    pub origin: Origin,
    /// The hot-command rule that started it (`""` → `None`).
    pub rule_id: Option<String>,
    /// Provider/runtime title, cleaned and masked (may be empty).
    pub title: String,
    /// The task as sent, when the detail was read (and history kept it).
    pub prompt: Option<String>,
    /// The workspace alias.
    pub repo: Option<String>,
    /// The working folder a local agent ran in. Kept for later slices and
    /// never drawn (a full local path).
    pub cwd: Option<String>,
    /// The session this one continues (`resume_of`), and its first ancestor.
    pub parent_id: Option<String>,
    pub root_id: Option<String>,
    pub external_id: Option<String>,
    /// Only `http(s)` addresses are kept.
    pub external_url: Option<String>,
    /// The runtime ran this on its deterministic mock: nothing was sent.
    pub sandbox: bool,
    pub created_at: Option<f64>,
    pub updated_at: Option<f64>,
    pub started_at: Option<f64>,
    pub finished_at: Option<f64>,
    pub timeout_seconds: Option<u64>,
    /// The newest event's `seq` (the Activity cursor's upper bound).
    pub last_seq: u64,
    pub revision: u64,
    /// The first 200 characters of the result (summaries), masked.
    pub result_preview: Option<String>,
    /// The result (detail reads only), masked before it is bounded.
    pub result_text: Option<String>,
    /// The runtime kept only the start of a longer result.
    pub result_truncated: bool,
    pub error: Option<SessionError>,
    pub usage: Option<Usage>,
    /// A stop was asked for (it may still be under way).
    pub cancel_requested: bool,
}

/// Longest result kept (the v1 bound on `result_text`).
pub const RESULT_CHARS: usize = 16_384;
const PREVIEW_CHARS: usize = 200;

/// One `Session` or `SessionSummary` object.
pub fn parse_session(item: &Value) -> Option<Session> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty() && id.len() <= 128)?
        .to_string();
    let error = item.get("error").filter(|e| e.is_object()).and_then(|e| {
        Some(SessionError {
            code: text(e.get("code"), 60)?,
            message: masked(e.get("message"), TEXT_CHARS),
        })
    });
    let usage = item.get("usage").filter(|u| u.is_object()).map(|u| Usage {
        cost_usd: u
            .get("cost_usd")
            .and_then(Value::as_f64)
            .filter(|c| c.is_finite()),
        duration_ms: u.get("duration_ms").and_then(Value::as_u64),
        turns: u.get("turns").and_then(Value::as_u64),
    });
    let flag = |name: &str| item.get(name).and_then(Value::as_bool).unwrap_or(false);
    Some(Session {
        id,
        provider: text(item.get("provider"), 80).unwrap_or_default(),
        profile: text(item.get("profile"), 80).unwrap_or_default(),
        runtime: RuntimeState::parse(item.get("state")),
        origin: Origin::parse(item.get("origin")),
        rule_id: text(item.get("rule_id"), 40),
        title: masked(item.get("title"), TEXT_CHARS).unwrap_or_default(),
        prompt: item
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .map(str::to_owned),
        repo: text(item.get("repo"), 120),
        cwd: text(item.get("cwd"), 4096),
        parent_id: text(item.get("parent_id"), 128),
        root_id: text(item.get("root_id"), 128),
        external_id: text(item.get("external_id"), 200),
        external_url: item
            .get("external_url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .filter(|u| u.len() <= 2048 && !u.contains(char::is_whitespace))
            .map(str::to_owned),
        sandbox: flag("sandbox"),
        created_at: timestamp(item.get("created_at")),
        updated_at: timestamp(item.get("updated_at")),
        started_at: timestamp(item.get("started_at")),
        finished_at: timestamp(item.get("finished_at")),
        timeout_seconds: item.get("timeout_seconds").and_then(Value::as_u64),
        last_seq: item.get("last_seq").and_then(Value::as_u64).unwrap_or(0),
        revision: item.get("revision").and_then(Value::as_u64).unwrap_or(0),
        result_preview: masked(item.get("result_preview"), PREVIEW_CHARS),
        result_text: output(item.get("result_text"), RESULT_CHARS),
        result_truncated: flag("result_truncated"),
        error,
        usage,
        cancel_requested: flag("cancel_requested"),
    })
}

/// Provider output kept as plain text: masked over the whole value, then
/// bounded; line breaks are kept (unlike [`masked`]); `None` when blank.
fn output(value: Option<&Value>, max: usize) -> Option<String> {
    let raw = value?.as_str()?;
    if raw.trim().is_empty() {
        return None;
    }
    let shown: String = mask_secrets(raw)
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .take(max)
        .collect();
    Some(shown.trim_end().to_string())
}

/// `{sessions: [...]}` (list and `/changes`).
pub fn parse_sessions(reply: &Value) -> Vec<Session> {
    reply
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_session)
        .collect()
}

/// `GET /api/agents/sessions`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionPage {
    pub sessions: Vec<Session>,
    pub next_cursor: Option<String>,
    pub revision: u64,
    pub counts: Option<Counts>,
}

pub fn parse_session_page(reply: &Value) -> SessionPage {
    SessionPage {
        sessions: parse_sessions(reply),
        next_cursor: text(reply.get("next_cursor"), 200),
        revision: reply.get("revision").and_then(Value::as_u64).unwrap_or(0),
        counts: parse_counts(reply),
    }
}

/// `GET /api/agents/changes`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Changes {
    pub revision: u64,
    pub sessions: Vec<Session>,
    pub deleted: Vec<String>,
    /// The cursor is too old: refetch the list.
    pub reset: bool,
    /// The server held the request (long-poll) before answering.
    pub waited: bool,
    pub counts: Option<Counts>,
}

pub fn parse_changes(reply: &Value) -> Changes {
    Changes {
        revision: reply.get("revision").and_then(Value::as_u64).unwrap_or(0),
        sessions: parse_sessions(reply),
        deleted: reply
            .get("deleted")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        reset: reply.get("reset").and_then(Value::as_bool).unwrap_or(false),
        waited: reply
            .get("waited")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        counts: parse_counts(reply),
    }
}

/// Seconds since the epoch from a number (seconds, or milliseconds when
/// implausibly large) or an RFC 3339 / naive ISO string (UTC).
fn timestamp(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => {
            let n = n.as_f64()?;
            let secs = if n > 1e11 { n / 1000.0 } else { n };
            (secs.is_finite() && secs > 0.0).then_some(secs)
        }
        Value::String(s) => {
            let s = s.trim();
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(t.timestamp_millis() as f64 / 1000.0);
            }
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|t| t.and_utc().timestamp_millis() as f64 / 1000.0)
        }
        _ => None,
    }
}

/// A string field, single-spaced and bounded; `None` when empty.
fn text(value: Option<&Value>, max: usize) -> Option<String> {
    let cleaned = clean(value?.as_str()?, max);
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Provider prose: masked for credential-like strings over the whole value
/// *before* it is cut, so a secret straddling the bound can't survive as an
/// unrecognisable prefix.
fn masked(value: Option<&Value>, max: usize) -> Option<String> {
    let cleaned = clean(&mask_secrets(value?.as_str()?), max);
    (!cleaned.is_empty()).then_some(cleaned)
}

fn clean(raw: &str, max: usize) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

// -------------------------------------------------------------- backend ---

/// Why a read failed, as safe human text. Never a token, URL with a
/// token, prompt, environment value or raw server body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentsError {
    /// `GET /api/agents/status` is 404: this service has no agents API.
    Absent,
    /// Nothing answered (or no API token could be read).
    Offline,
    /// The service answered with an error; its `detail`, sanitised.
    Refused(String),
    /// A write (launch, stop, remove, reply) was answered with an error
    /// envelope: its status, `code` slug and sanitised `detail`.
    Rejected(ApiError),
}

/// A v1 error envelope (`{detail, code}`), sanitised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub detail: String,
}

impl AgentsError {
    pub fn message(&self) -> String {
        match self {
            AgentsError::Absent => "This LocalFlow service doesn't run agents.".into(),
            AgentsError::Offline => SERVICE_DOWN.into(),
            AgentsError::Refused(detail) => format!("LocalFlow couldn't read sessions: {detail}"),
            AgentsError::Rejected(e) => e.detail.clone(),
        }
    }

    /// The envelope's `code`, for a rejected write.
    pub fn code(&self) -> Option<&str> {
        match self {
            AgentsError::Rejected(e) => Some(e.code.as_str()),
            _ => None,
        }
    }

    /// A rejected write, from its parts (`detail` is sanitised here).
    pub fn rejected(status: u16, code: &str, detail: &str) -> Self {
        AgentsError::Rejected(ApiError {
            status,
            code: clean(code, 40),
            detail: safe_detail(detail),
        })
    }

    /// Short reason for inline errors and toasts ("{action}. {reason}").
    pub fn reason(&self) -> String {
        match self {
            AgentsError::Offline => "LocalFlow's agent service isn't answering.".into(),
            AgentsError::Absent => "This LocalFlow service doesn't run agents.".into(),
            AgentsError::Refused(detail) => sentence(detail),
            AgentsError::Rejected(e) => sentence(&e.detail),
        }
    }
}

/// A detail as a sentence: capitalised, with a full stop.
fn sentence(detail: &str) -> String {
    let mut chars = detail.trim().chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().chain(chars).collect();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// The error banner while the service can't be reached (design section 10).
pub const SERVICE_DOWN: &str =
    "LocalFlow's agent service isn't running, so tasks can't be launched.";

/// Sanitise a server `detail` (already "safe" by contract; masked again).
pub fn safe_detail(detail: &str) -> String {
    let detail = clean(&mask_secrets(detail), 200);
    if detail.is_empty() {
        "the request failed".into()
    } else {
        detail
    }
}

/// The `agents-api v1` routes the Agents page and launcher use. Blocking:
/// call on a background thread. Real mode is `agents_client::HttpAgents`;
/// `--sandbox` is the in-process `sandbox::agents::FakeAgents` (no socket,
/// subprocess or keyring).
///
/// Callers gate every optional route on the capability that offers it
/// ([`actions`]); a backend never has to guess.
pub trait AgentsBackend: Send + Sync {
    /// In-process fake: "Sandbox: nothing is sent."
    fn simulated(&self) -> bool;
    fn status(&self) -> Result<Status, AgentsError>;
    fn providers(&self) -> Result<Vec<Provider>, AgentsError>;
    /// Newest first; `before` is a `next_cursor`.
    fn sessions(&self, limit: u32, before: Option<&str>) -> Result<SessionPage, AgentsError>;
    fn session(&self, id: &str) -> Result<Session, AgentsError>;
    /// Changes after `since`; `wait` seconds of long-poll (0..=25).
    fn changes(&self, since: u64, wait: u32) -> Result<Changes, AgentsError>;
    /// `POST /api/agents/sessions` with exactly [`LaunchRequest::body`].
    /// Replaying the same id and body answers the same session.
    fn launch(&self, request: &LaunchRequest) -> Result<Session, AgentsError>;
    /// `POST /api/agents/sessions/{id}/cancel` (empty body; idempotent).
    fn cancel(&self, id: &str) -> Result<Session, AgentsError>;
    /// `DELETE /api/agents/sessions/{id}` (terminal sessions only).
    fn remove(&self, id: &str) -> Result<(), AgentsError>;
    /// `POST /api/agents/sessions/{id}/messages` with `{"text": …}`.
    fn message(&self, id: &str, text: &str) -> Result<(Session, Event), AgentsError>;
    /// `GET /api/agents/sessions/{id}/events?after=&limit=&wait=`.
    fn events(
        &self,
        id: &str,
        after: u64,
        limit: u32,
        wait: u32,
    ) -> Result<EventsPage, AgentsError>;
    /// Seconds since the epoch on this backend's clock (the sandbox can pin
    /// it so fixture scenes draw the same times on every run).
    fn now(&self) -> f64 {
        now()
    }
}

/// Everything the page reads at once.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub status: Status,
    pub providers: Vec<Provider>,
    /// Every page read, concatenated (newest first).
    pub page: SessionPage,
}

/// Status, providers and the session list: [`PAGE_SIZE`] at a time,
/// following `next_cursor` until the server has no more or
/// [`MAX_SESSIONS`] (everything it retains) are read.
pub fn load(backend: &dyn AgentsBackend) -> Result<Snapshot, AgentsError> {
    let status = backend.status()?;
    let providers = backend.providers()?;
    let mut page = backend.sessions(PAGE_SIZE, None)?;
    let mut cursor = page.next_cursor.take();
    while let Some(before) = cursor.filter(|_| page.sessions.len() < MAX_SESSIONS) {
        let next = backend.sessions(PAGE_SIZE, Some(&before))?;
        // A cursor that doesn't move would loop forever.
        cursor = next.next_cursor.filter(|c| *c != before);
        if next.sessions.is_empty() {
            break;
        }
        page.sessions.extend(next.sessions);
        page.revision = page.revision.max(next.revision);
        page.counts = next.counts.or(page.counts);
    }
    page.sessions.truncate(MAX_SESSIONS);
    Ok(Snapshot {
        status,
        providers,
        page,
    })
}

// --------------------------------------------------------------- launch ---

/// `POST /api/agents/sessions`. The runtime accepts exactly `prompt`
/// (required), `provider`, `id`, `profile`, `repo`, `cwd`, `resume_of`,
/// `title`, `timeout_seconds`, `origin`, `rule_id` and `options`, and
/// refuses any other field. LocalFlow's windows send only what they need:
/// the task, the provider, a client id (replay-safe), the profile when the
/// provider has one, the workspace *alias* when one is chosen (never `cwd`
/// as well: the runtime resolves the alias), and `origin: "api"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub id: String,
    pub prompt: String,
    pub provider: String,
    pub profile: Option<String>,
    pub repo: Option<String>,
}

/// Every field the runtime accepts on a launch (unknown fields are 400).
pub const LAUNCH_FIELDS: [&str; 12] = [
    "prompt",
    "provider",
    "id",
    "profile",
    "repo",
    "cwd",
    "resume_of",
    "title",
    "timeout_seconds",
    "origin",
    "rule_id",
    "options",
];

impl LaunchRequest {
    /// The JSON body, exactly.
    pub fn body(&self) -> Value {
        let mut body = serde_json::Map::new();
        body.insert("prompt".into(), Value::String(self.prompt.clone()));
        body.insert("provider".into(), Value::String(self.provider.clone()));
        body.insert("id".into(), Value::String(self.id.clone()));
        if let Some(profile) = self.profile.as_ref().filter(|p| !p.is_empty()) {
            body.insert("profile".into(), Value::String(profile.clone()));
        }
        if let Some(repo) = self.repo.as_ref().filter(|r| !r.is_empty()) {
            body.insert("repo".into(), Value::String(repo.clone()));
        }
        body.insert("origin".into(), Value::String("api".into()));
        Value::Object(body)
    }
}

/// A v1 session id: 12 lowercase hex characters.
pub fn is_session_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A fresh client-generated session id (12 lowercase hex characters). It
/// correlates the optimistic row with the runtime's and makes a retry of
/// the same launch replay-safe (same id and body → the same session).
pub fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // A per-process random SipHash key over the time, the process and a
    // counter: unpredictable enough for a correlation id, never repeated.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.write_u32(std::process::id());
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:012x}", hasher.finish() & 0xffff_ffff_ffff)
}

// --------------------------------------------------------------- events ---

/// `Event.type`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EventKind {
    State,
    #[default]
    Output,
    Message,
    Error,
    Usage,
    Truncated,
    /// A type outside the v1 list.
    Other,
}

/// `Event.stream`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stream {
    #[default]
    Agent,
    Stderr,
    User,
    System,
    Other,
}

/// One activity line, allow-listed: `data` is never kept (it may hold raw
/// provider fields), and `text` is masked before it is bounded.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    pub seq: u64,
    pub ts: Option<f64>,
    pub kind: EventKind,
    pub stream: Stream,
    /// Plain text, masked (may be empty).
    pub text: String,
    pub state: Option<RuntimeState>,
}

/// Longest event text kept (the v1 bound).
const EVENT_CHARS: usize = 8192;

pub fn parse_event(item: &Value) -> Option<Event> {
    let seq = item
        .get("seq")
        .and_then(Value::as_u64)
        .filter(|s| *s >= 1)?;
    Some(Event {
        seq,
        ts: timestamp(item.get("ts")),
        kind: match item.get("type").and_then(Value::as_str) {
            Some("state") => EventKind::State,
            Some("output") => EventKind::Output,
            Some("message") => EventKind::Message,
            Some("error") => EventKind::Error,
            Some("usage") => EventKind::Usage,
            Some("truncated") => EventKind::Truncated,
            _ => EventKind::Other,
        },
        stream: match item.get("stream").and_then(Value::as_str) {
            Some("agent") => Stream::Agent,
            Some("stderr") => Stream::Stderr,
            Some("user") => Stream::User,
            Some("system") => Stream::System,
            _ => Stream::Other,
        },
        text: output(item.get("text"), EVENT_CHARS).unwrap_or_default(),
        state: item
            .get("state")
            .filter(|s| s.is_string())
            .map(|s| RuntimeState::parse(Some(s))),
    })
}

/// `GET /api/agents/sessions/{id}/events`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventsPage {
    pub events: Vec<Event>,
    pub next_after: u64,
    pub last_seq: u64,
    pub has_more: bool,
    pub state: RuntimeState,
    pub terminal: bool,
    pub waited: bool,
}

pub fn parse_events(reply: &Value) -> EventsPage {
    let n = |name: &str| reply.get(name).and_then(Value::as_u64).unwrap_or(0);
    let flag = |name: &str| reply.get(name).and_then(Value::as_bool).unwrap_or(false);
    EventsPage {
        events: reply
            .get("events")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_event)
            .collect(),
        next_after: n("next_after"),
        last_seq: n("last_seq"),
        has_more: flag("has_more"),
        state: RuntimeState::parse(reply.get("state")),
        terminal: flag("terminal"),
        waited: flag("waited"),
    }
}

/// Events kept per session (oldest dropped first), so a flooding agent
/// can't grow memory without bound.
pub const MAX_EVENTS: usize = 500;
/// Events asked for per read.
pub const EVENTS_LIMIT: u32 = 200;

/// The selected session's Activity: a bounded, de-duplicated event log and
/// its read cursor (`after`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Activity {
    pub session: String,
    events: std::collections::VecDeque<Event>,
    /// The highest `seq` read (the next read's `after`).
    pub after: u64,
    /// The runtime said the log is final (`terminal && !has_more`).
    pub done: bool,
    pub loaded: bool,
}

impl Activity {
    pub fn new(session: &str) -> Self {
        Activity {
            session: session.into(),
            ..Default::default()
        }
    }

    /// Apply one read; `true` when another read should follow at once
    /// (`has_more`).
    pub fn apply(&mut self, page: EventsPage) -> bool {
        self.loaded = true;
        for event in page.events {
            self.push(event);
        }
        self.after = self.after.max(page.next_after);
        self.done = page.terminal && !page.has_more;
        page.has_more
    }

    /// One event (a read's, or the one a sent reply returned); older or
    /// repeated `seq`s are ignored.
    pub fn push(&mut self, event: Event) {
        if self.events.iter().any(|e| e.seq == event.seq) {
            return;
        }
        self.after = self.after.max(event.seq);
        let at = self.events.partition_point(|e| e.seq < event.seq);
        self.events.insert(at, event);
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Newest first (as drawn).
    pub fn newest_first(&self) -> impl Iterator<Item = &Event> {
        self.events.iter().rev()
    }

    /// What the agent last wrote before pausing for input: the newest agent
    /// `output` line (v1's only source for a question).
    pub fn question(&self) -> Option<&str> {
        self.events
            .iter()
            .rev()
            .find(|e| e.kind == EventKind::Output && e.stream == Stream::Agent)
            .map(|e| e.text.as_str())
            .filter(|t| !t.trim().is_empty())
    }
}

/// One Activity line as drawn: `(label, text)`. Plain text only: nothing
/// is parsed as markup and no link is made clickable here.
pub fn activity_line(event: &Event, provider: &str) -> (String, String) {
    let text = event.text.trim().to_string();
    match event.kind {
        EventKind::State => {
            let state = event
                .state
                .as_ref()
                .map(|s| s.ui().label().to_string())
                .unwrap_or_else(|| "Unknown".into());
            (
                "State".into(),
                if text.is_empty() {
                    state
                } else {
                    format!("{state} \u{00b7} {text}")
                },
            )
        }
        EventKind::Message | EventKind::Output if event.stream == Stream::User => {
            ("You".into(), text)
        }
        EventKind::Output if event.stream == Stream::Stderr => ("Log".into(), text),
        EventKind::Output | EventKind::Message => (provider.into(), text),
        EventKind::Error => ("Error".into(), text),
        EventKind::Usage => ("Usage".into(), text),
        EventKind::Truncated => (
            "Note".into(),
            if text.is_empty() {
                "Earlier output was cut to keep the log small.".into()
            } else {
                text
            },
        ),
        EventKind::Other => ("Note".into(), text),
    }
}

/// Usage as one plain line ("3 turns · 2 min · $0.42"), when reported.
pub fn usage_line(usage: &Usage) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(turns) = usage.turns {
        parts.push(if turns == 1 {
            "1 turn".to_string()
        } else {
            format!("{turns} turns")
        });
    }
    if let Some(ms) = usage.duration_ms {
        parts.push(duration(ms as f64 / 1000.0));
    }
    if let Some(cost) = usage.cost_usd {
        parts.push(format!("${cost:.2}"));
    }
    (!parts.is_empty()).then(|| parts.join(" \u{00b7} "))
}

// -------------------------------------------------------------- actions ---

/// What the detail pane offers for one session. A missing capability is a
/// missing control (`None`); one that can't be used right now (the session
/// is final, Disconnected, or a stop is already under way) is `Some(false)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actions {
    /// "Open in {P}" and "Copy link": the session has a web address.
    pub open: Option<String>,
    /// Stop: the provider can cancel and the session isn't final.
    pub stop: Option<bool>,
    /// Reply: the provider accepts live messages and the session isn't final.
    pub reply: Option<bool>,
    /// Remove from LocalFlow: final sessions only.
    pub remove: bool,
}

/// The capability gate for one row (design section 7, "Actions").
pub fn actions(row: &Row) -> Actions {
    let live = !row.session.runtime.terminal();
    let reachable = row.state != UiState::Disconnected;
    Actions {
        open: row.session.external_url.clone(),
        stop: (row.caps.cancel && live).then_some(
            reachable && row.state != UiState::Stopping && !row.session.cancel_requested,
        ),
        reply: (row.caps.live_message && live)
            .then_some(reachable && matches!(row.state, UiState::Running | UiState::NeedsInput)),
        remove: row.state.terminal(),
    }
}

/// `{T}`: a title cut to 40 characters for confirmations and toasts.
pub fn short_title(title: &str) -> String {
    if title.chars().count() > 40 {
        let cut: String = title.chars().take(39).collect();
        format!("{}…", cut.trim_end())
    } else {
        title.to_string()
    }
}

/// `(title, body, verb)` of the Stop confirmation.
pub fn stop_confirm(row: &Row) -> (String, String, String) {
    (
        "Stop this session?".into(),
        format!(
            "{} will stop working on \u{201c}{}\u{201d}. Work it has already pushed stays where it is.",
            row.provider,
            short_title(&row.title)
        ),
        "Stop".into(),
    )
}

/// `(title, body, verb)` of the Remove confirmation. Local only: the
/// session stays at a cloud provider; a local agent's work stays on disk.
pub fn remove_confirm(row: &Row) -> (String, String, String) {
    let stays = match row.caps.kind {
        Kind::Cloud => format!("It stays in {}.", row.provider),
        Kind::LocalProcess => format!(
            "Work it already did on {} stays where it is.",
            this_computer()
        ),
    };
    (
        "Remove from LocalFlow?".into(),
        format!(
            "This removes \u{201c}{}\u{201d} from this list only. {stays}",
            short_title(&row.title)
        ),
        "Remove".into(),
    )
}

pub fn stop_failed(row: &Row, error: &AgentsError) -> String {
    format!(
        "Couldn't stop \u{201c}{}\u{201d}. {}",
        short_title(&row.title),
        error.reason()
    )
    .trim_end()
    .to_string()
}

pub fn remove_failed(error: &AgentsError) -> String {
    format!("Couldn't remove this session. {}", error.reason())
        .trim_end()
        .to_string()
}

pub const REPLY_FAILED: &str = "Couldn't send your reply. It's still in the field.";

/// The reply field's placeholder (design section 10).
pub fn reply_placeholder(provider: &str) -> String {
    format!("Reply to {provider} \u{2014} sends only this message")
}

/// Why a reply couldn't be sent, under the field: the exact design line,
/// then the service's reason when it gave one (409/502 bodies are safe).
pub fn reply_error(error: &AgentsError) -> String {
    match error {
        AgentsError::Rejected(e) if !e.detail.is_empty() => {
            format!("{REPLY_FAILED} {}", sentence(&e.detail))
        }
        _ => REPLY_FAILED.into(),
    }
}

// -------------------------------------------------------------- results ---

/// The Results section, from v1 data only (there are no artifact rows in
/// v1, so none are drawn).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Results {
    /// The result text (masked), and whether the runtime cut it.
    Text { text: String, truncated: bool },
    /// Only the 200-character preview is in hand (the detail is loading).
    Preview(String),
    /// Completed without a result: `(heading, body)`.
    Empty(String, String),
}

pub const RESULT_TRUNCATED: &str =
    "This is the start of a longer result. LocalFlow keeps the first 16 KB.";
pub const NO_RESULTS_HEADING: &str = "No results were reported";

/// Results for a row; `None` when the section is left out.
pub fn results(row: &Row) -> Option<Results> {
    let s = &row.session;
    if let Some(text) = &s.result_text {
        return Some(Results::Text {
            text: text.clone(),
            truncated: s.result_truncated,
        });
    }
    if let Some(preview) = &s.result_preview {
        return Some(Results::Preview(preview.clone()));
    }
    (s.runtime == RuntimeState::Completed).then(|| {
        Results::Empty(
            NO_RESULTS_HEADING.into(),
            format!(
                "{} finished without attaching a summary or links.",
                row.provider
            ),
        )
    })
}

/// Sessions read per list request ("Earlier loads 50 at a time").
pub const PAGE_SIZE: u32 = 50;
/// The most sessions the service keeps (`max_sessions`), so the most read.
pub const MAX_SESSIONS: usize = 200;

// ---------------------------------------------------------------- store ---

/// One row as drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub session: Session,
    pub state: UiState,
    /// What the UI would have shown with fresh information (for
    /// "Disconnected · was Running").
    pub last_known: UiState,
    pub title: String,
    pub provider: String,
    pub monogram: String,
    pub caps: Capabilities,
}

impl Row {
    /// Most recent activity (sorting non-terminal rows).
    fn activity(&self) -> f64 {
        let s = &self.session;
        s.updated_at
            .or(s.started_at)
            .or(s.created_at)
            .unwrap_or(0.0)
    }

    /// When it finished (sorting terminal rows).
    fn finished(&self) -> f64 {
        let s = &self.session;
        s.finished_at
            .or(s.updated_at)
            .or(s.created_at)
            .unwrap_or(0.0)
    }

    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || [
                self.title.as_str(),
                self.provider.as_str(),
                self.session.repo.as_deref().unwrap_or(""),
            ]
            .iter()
            .any(|field| field.to_lowercase().contains(&query))
    }
}

/// The page's view of the service: last snapshot, live updates, and how
/// long the service has been failing. Replies carry a generation so a late
/// one never replaces newer state.
#[derive(Clone, Debug, Default)]
pub struct Store {
    pub status: Option<Status>,
    pub providers: Vec<Provider>,
    sessions: BTreeMap<String, Session>,
    pub revision: u64,
    /// The service's newest `counts` (the header's numbers).
    pub counts: Option<Counts>,
    pub loaded: bool,
    pub error: Option<AgentsError>,
    /// When reads started failing (seconds since the epoch).
    failing_since: Option<f64>,
    generation: u64,
}

impl Store {
    /// Start a full read.
    pub fn begin(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// A full read answered; `false` (ignored) unless it is the newest.
    pub fn finish(
        &mut self,
        generation: u64,
        result: Result<Snapshot, AgentsError>,
        now: f64,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        match result {
            Ok(snapshot) => {
                let previous = std::mem::take(&mut self.sessions);
                for mut session in snapshot.page.sessions {
                    if let Some(old) = previous.get(&session.id) {
                        keep_detail(&mut session, old);
                    }
                    self.sessions.insert(session.id.clone(), session);
                }
                self.revision = snapshot.page.revision.max(snapshot.status.revision);
                self.counts = snapshot.page.counts.or(snapshot.status.counts);
                self.status = Some(snapshot.status);
                self.providers = snapshot.providers;
                self.loaded = true;
                self.succeeded();
            }
            Err(error) => self.failed(error, now),
        }
        true
    }

    /// A `/changes` reply. `true` when the list must be read again.
    pub fn apply_changes(&mut self, changes: Changes) -> bool {
        self.succeeded();
        if changes.counts.is_some() {
            self.counts = changes.counts;
        }
        if changes.reset {
            return true;
        }
        for id in &changes.deleted {
            self.sessions.remove(id);
        }
        for session in changes.sessions {
            self.upsert(session);
        }
        self.revision = self.revision.max(changes.revision);
        false
    }

    /// A detail read (`GET /api/agents/sessions/{id}`).
    pub fn apply_detail(&mut self, session: Session) {
        self.upsert(session);
    }

    /// Keep the task and result a summary omits; never go back in revision.
    fn upsert(&mut self, mut session: Session) {
        if let Some(old) = self.sessions.get(&session.id) {
            if old.revision > session.revision {
                return;
            }
            keep_detail(&mut session, old);
        }
        self.sessions.insert(session.id.clone(), session);
    }

    /// A session this window launched or changed (its reply is a full
    /// `Session`), applied like a detail read.
    pub fn apply_session(&mut self, session: Session) {
        self.upsert(session);
    }

    /// A session removed from the store (`DELETE` answered).
    pub fn remove(&mut self, id: &str) {
        self.sessions.remove(id);
    }

    pub fn succeeded(&mut self) {
        self.failing_since = None;
        self.error = None;
    }

    /// A read failed at `now`. Rows already shown stay (Disconnected after
    /// the grace period); [`AgentsError::Absent`] clears them.
    pub fn failed(&mut self, error: AgentsError, now: f64) {
        if error == AgentsError::Absent {
            self.sessions.clear();
            self.providers.clear();
            self.status = None;
            self.counts = None;
        }
        self.failing_since.get_or_insert(now);
        self.error = Some(error);
        self.loaded = true;
    }

    /// The service has failed for longer than the grace period.
    pub fn stale(&self, now: f64) -> bool {
        self.failing_since
            .is_some_and(|since| now - since >= GRACE_SECS)
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    pub fn provider(&self, id: &str, profile: &str) -> Option<&Provider> {
        self.providers
            .iter()
            .find(|p| p.id == id && p.profile == profile)
            .or_else(|| self.providers.iter().find(|p| p.id == id))
    }

    /// Any provider is usable right now.
    pub fn any_connected(&self) -> bool {
        self.providers
            .iter()
            .any(|p| p.readiness == Readiness::Connected)
    }

    /// Every session as a row, in display order (design section 6):
    /// Needs input, then the other non-terminal states by most recent
    /// activity, then terminal states by finish time, newest first.
    pub fn rows(&self, now: f64) -> Vec<Row> {
        let stale = self.stale(now);
        let mut rows: Vec<Row> = self
            .sessions
            .values()
            .map(|session| {
                let provider = self.provider(&session.provider, &session.profile);
                let offline = provider.is_some_and(|p| p.readiness == Readiness::Offline);
                let label = provider
                    .map(|p| p.label.clone())
                    .unwrap_or_else(|| session.provider.clone());
                Row {
                    state: ui_state(&session.runtime, stale || offline),
                    last_known: session.runtime.ui(),
                    title: title(session),
                    monogram: monogram(&label),
                    provider: label,
                    caps: provider.map(|p| p.caps).unwrap_or_default(),
                    session: session.clone(),
                }
            })
            .collect();
        sort(&mut rows);
        rows
    }
}

/// A summary (no `prompt`, no `result_text`) keeps what an earlier detail
/// read of the same session brought. The task never changes; the result is
/// kept only while the summary's revision is the detail's, or once it was
/// final (a later summary of a finished session can't change its result).
fn keep_detail(session: &mut Session, old: &Session) {
    if session.prompt.is_none() {
        session.prompt = old.prompt.clone();
    }
    if session.result_text.is_none() && (old.revision == session.revision || old.runtime.terminal())
    {
        session.result_text = old.result_text.clone();
    }
}

/// Display order of rows (see [`Store::rows`]); ties fall back to the id
/// so the order is stable.
pub fn sort(rows: &mut [Row]) {
    rows.sort_by(|a, b| {
        a.state
            .group()
            .cmp(&b.state.group())
            .then_with(|| {
                let (x, y) = if a.state.terminal() {
                    (a.finished(), b.finished())
                } else {
                    (a.activity(), b.activity())
                };
                y.total_cmp(&x)
            })
            .then_with(|| a.session.id.cmp(&b.session.id))
    });
}

/// Rows passing the filter and search, grouped in order. Empty groups are
/// left out.
pub fn grouped<'a>(rows: &'a [Row], filter: Filter, query: &str) -> Vec<(Group, Vec<&'a Row>)> {
    let mut out: Vec<(Group, Vec<&Row>)> = Vec::new();
    for row in rows
        .iter()
        .filter(|r| filter.matches(r.state) && r.matches(query))
    {
        let group = row.state.group();
        match out.last_mut() {
            Some((g, list)) if *g == group => list.push(row),
            _ => out.push((group, vec![row])),
        }
    }
    out
}

/// The title rule: the provider's title; else the first line of the task
/// (up to [`TITLE_CHARS`]); else "Untitled task".
pub fn title(session: &Session) -> String {
    if !session.title.is_empty() {
        return session.title.clone();
    }
    let first = session
        .prompt
        .as_deref()
        .and_then(|p| p.lines().map(str::trim).find(|l| !l.is_empty()));
    match first {
        Some(line) => {
            let masked = clean(&mask_secrets(line), TEXT_CHARS);
            if masked.chars().count() > TITLE_CHARS {
                let cut: String = masked.chars().take(TITLE_CHARS - 1).collect();
                format!("{}…", cut.trim_end())
            } else {
                masked
            }
        }
        None => UNTITLED.into(),
    }
}

// ----------------------------------------------------------------- copy ---

/// Header supporting line: "2 running · 1 needs you" / "No active sessions".
/// "Running" counts sessions known to be working (Starting, Running,
/// Stopping); Disconnected and Unknown rows are listed but not counted.
///
/// With the service's `counts` the running number covers every session it
/// keeps, including any past the pages read: its non-terminal total
/// (`active + queued`) less the live rows this window shows as needing
/// you, Disconnected or Unknown. `counts` has no needs-input bucket, so
/// "needs you" is always the rows' own count.
pub fn support_line(rows: &[Row], counts: Option<Counts>) -> String {
    let needs = rows
        .iter()
        .filter(|r| r.state == UiState::NeedsInput)
        .count();
    let working = |r: &&Row| {
        matches!(
            r.state,
            UiState::Starting | UiState::Running | UiState::Stopping
        )
    };
    let running = match counts {
        Some(c) => {
            let other_live = rows
                .iter()
                .filter(|r| !r.state.terminal() && !working(r))
                .count() as u64;
            (c.active + c.queued).saturating_sub(other_live) as usize
        }
        None => rows.iter().filter(working).count(),
    };
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if needs > 0 {
        parts.push(format!("{needs} needs you"));
    }
    if parts.is_empty() {
        "No active sessions".into()
    } else {
        parts.join(" · ")
    }
}

/// "this Mac" / "this PC" / "this computer".
pub fn this_computer() -> &'static str {
    if cfg!(target_os = "macos") {
        "this Mac"
    } else if cfg!(target_os = "windows") {
        "this PC"
    } else {
        "this computer"
    }
}

pub const NO_PROVIDER_HEADING: &str = "Connect an agent to get started";

pub fn no_provider_body() -> String {
    format!(
        "LocalFlow starts tasks on cloud agents you connect, such as Claude or Devin. \
         Nothing leaves {} until you connect one and launch a task.",
        this_computer()
    )
}

pub const NO_SESSIONS_HEADING: &str = "No sessions yet";

/// The empty list's body. Section 10 names the launcher's shortcut; until a
/// launcher exists (`None`) the line doesn't promise a key that does nothing.
pub fn no_sessions_body(hotkey: Option<&str>) -> String {
    match hotkey {
        Some(key) => format!("Press {key} in any app to give an agent a task."),
        None => "Tasks you give an agent through LocalFlow appear here.".into(),
    }
}

pub const FILTER_EMPTY: &str = "Nothing with this status";

pub fn search_empty(query: &str) -> String {
    format!("No sessions match \u{201c}{}\u{201d}", query.trim())
}

pub const SELECT_HEADING: &str = "Select a session";
pub const SELECT_BODY: &str = "Its task, activity and results appear here.";

/// Longest question quoted in the Needs input banner.
const QUESTION_CHARS: usize = 240;

/// A state banner for the detail pane: `(tone, message, offers "Try again")`.
///
/// v1 has no question field. `question` is the newest `output` line the
/// agent wrote before it paused (the reconciliation's fallback), already
/// masked; without one the generic line shows and nothing is invented.
pub fn banner(
    row: &Row,
    updated: Option<&str>,
    question: Option<&str>,
) -> Option<(Tone, String, bool)> {
    let p = &row.provider;
    let s = &row.session;
    match row.state {
        UiState::NeedsInput => {
            if let Some(q) = question.map(|q| clean(q, QUESTION_CHARS)) {
                if !q.is_empty() {
                    return Some((
                        Tone::Warning,
                        format!("{p} is waiting for your answer \u{2014} \u{201c}{q}\u{201d}"),
                        false,
                    ));
                }
            }
            let open = if s.external_url.is_some() {
                format!(" Open it in {p} to see what it needs.")
            } else {
                String::new()
            };
            Some((
                Tone::Warning,
                format!("{p} is waiting for you.{open}"),
                false,
            ))
        }
        UiState::Failed => {
            let started = s.started_at.is_some();
            let reason = s.error.as_ref().and_then(|e| e.message.clone());
            let message = match (&reason, started, &s.runtime) {
                (Some(reason), false, _) => format!("{p} didn't start this task. {reason}"),
                (Some(reason), true, _) => reason.clone(),
                (None, false, _) => format!("{p} didn't start this task and gave no reason."),
                (None, true, RuntimeState::TimedOut) => {
                    format!("{p} ran out of time before finishing.")
                }
                (None, true, _) => format!("{p} stopped with an error and gave no reason."),
            };
            Some((Tone::Error, message, false))
        }
        UiState::Disconnected => Some((
            Tone::Info,
            format!(
                "LocalFlow can't reach this session. Last known state: {}{}.",
                row.last_known.label(),
                updated
                    .map(|u| format!(", updated {u}"))
                    .unwrap_or_default()
            ),
            true,
        )),
        _ => None,
    }
}

/// The detail header's state note, when there is one: Unknown names the
/// provider's own word, Disconnected the last known state.
pub fn state_note(row: &Row, updated: Option<&str>) -> Option<String> {
    match row.state {
        UiState::Unknown => Some(match &row.session.runtime {
            RuntimeState::Unrecognised(raw) => format!("{} reports: {raw}", row.provider),
            _ => format!("{} reported no state.", row.provider),
        }),
        UiState::Disconnected => Some(format!(
            "Disconnected · was {}{}",
            row.last_known.label(),
            updated
                .map(|u| format!(", last updated {u}"))
                .unwrap_or_default()
        )),
        _ => None,
    }
}

// ------------------------------------------------------ the Sends line ---

/// How a launch uses its workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Workspace {
    /// The repository's name goes to the provider.
    Sent(String),
    /// A local agent works in this folder; nothing about it is sent.
    Local(String),
    /// The session names this workspace, but nothing says whether the
    /// provider was sent it (v1 has no such capability: one cloud provider
    /// substitutes it into the request, another only records it locally).
    /// Shown as "Workspace: {name}", never as sent.
    Named(String),
}

/// One attached item: how the sentence names it, and its receipt token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// "the selection (18 lines)"
    pub phrase: String,
    /// "Selection · 18 lines"
    pub token: String,
}

/// Exactly what a launch hands over: the Sends line and the receipt are
/// derived from this, never from UI state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub provider: String,
    pub workspace: Option<Workspace>,
    pub attachments: Vec<Attachment>,
}

pub const SANDBOX_SENDS: &str = "Sandbox: nothing is sent.";
pub const NOTHING_SENT: &str = "Nothing has been sent.";

/// "a, b and c".
fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The one-sentence disclosure (design section 8).
pub fn sends_line(payload: &Payload, sandbox: bool) -> String {
    if sandbox {
        return SANDBOX_SENDS.into();
    }
    let mut items = vec!["your task".to_string()];
    items.extend(payload.attachments.iter().map(|a| a.phrase.clone()));
    if matches!(payload.workspace, Some(Workspace::Sent(_))) {
        items.push("the repository name".into());
    }
    // "only" / "Nothing else" are claims about everything sent: not made
    // while it is unknown whether the workspace went too.
    let certain = !matches!(payload.workspace, Some(Workspace::Named(_)));
    let mut line = format!("Sends to {}: {}", payload.provider, join(&items));
    if items.len() == 1 && certain {
        line.push_str(" only.");
    } else {
        line.push('.');
        if payload.attachments.is_empty() && certain {
            line.push_str(" Nothing else.");
        }
    }
    match &payload.workspace {
        Some(Workspace::Local(folder)) => {
            line.push_str(&format!(" It runs on {} in {folder}.", this_computer()))
        }
        Some(Workspace::Named(name)) => line.push_str(&format!(" Workspace: {name}.")),
        _ => {}
    }
    line
}

/// The frozen launch receipt: "Sent to {P}" and one token per item.
pub fn receipt(payload: &Payload) -> (String, Vec<String>) {
    let mut items = vec!["Task".to_string()];
    items.extend(payload.attachments.iter().map(|a| a.token.clone()));
    if matches!(payload.workspace, Some(Workspace::Sent(_))) {
        items.push("Repository name".into());
    }
    (format!("Sent to {}", payload.provider), items)
}

/// The receipt's neutral workspace line, when the workspace is named but
/// not known to have been sent: "Workspace: acme/web".
pub fn workspace_note(payload: &Payload) -> Option<String> {
    match &payload.workspace {
        Some(Workspace::Named(name)) => Some(format!("Workspace: {name}")),
        _ => None,
    }
}

/// What a stored session's launch sent. v1 stores no attachment list (text
/// context is appended to the task), so only the task and workspace are
/// known. Whether a cloud provider was sent the repository's name isn't
/// (v1 has no capability for it), so it is only named, never claimed sent.
pub fn payload_for(row: &Row) -> Payload {
    Payload {
        provider: row.provider.clone(),
        workspace: row.session.repo.clone().map(|repo| match row.caps.kind {
            Kind::LocalProcess => Workspace::Local(repo),
            Kind::Cloud => Workspace::Named(repo),
        }),
        attachments: Vec::new(),
    }
}

// ---------------------------------------------------------------- times ---

/// Compact relative time for list rows: "now", "6m", "3h", "Tue", "Nov 12".
pub fn relative(ts: f64, now: f64) -> String {
    let age = (now - ts).max(0.0);
    if age < 60.0 {
        "now".into()
    } else if age < 3600.0 {
        format!("{}m", (age / 60.0) as i64)
    } else if age < 86_400.0 {
        format!("{}h", (age / 3600.0) as i64)
    } else {
        let when = lf_ui::hub_model::history_when(ts);
        if age < 6.0 * 86_400.0 {
            when.format("%a").to_string()
        } else {
            when.format("%b %-d").to_string()
        }
    }
}

/// "18 min", "1 h 5 min", "under a minute".
pub fn duration(secs: f64) -> String {
    let minutes = (secs.max(0.0) / 60.0) as i64;
    match minutes {
        0 => "under a minute".into(),
        m if m < 60 => format!("{m} min"),
        m if m % 60 == 0 => format!("{} h", m / 60),
        m => format!("{} h {} min", m / 60, m % 60),
    }
}

/// The detail header's line: "Devin · acme/billing · started 12:04 · 18 min".
pub fn meta_line(row: &Row, now: f64, clock: impl Fn(f64) -> String) -> String {
    let s = &row.session;
    let mut parts = vec![row.provider.clone()];
    if let Some(repo) = &s.repo {
        parts.push(repo.clone());
    }
    match (s.started_at, s.created_at) {
        (Some(start), _) => {
            parts.push(format!("started {}", clock(start)));
            let end = s.finished_at.unwrap_or(now);
            parts.push(duration(end - start));
        }
        (None, Some(created)) => parts.push(format!("sent {}", clock(created))),
        (None, None) => {}
    }
    parts.join(" · ")
}

/// Seconds since the epoch, now.
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::sandbox::agents::rfc3339;

    /// A schema-shaped `SessionSummary` (RFC 3339 times, `error: null`).
    fn summary_json(id: &str, state: &str, updated: f64) -> Value {
        json!({
            "id": id, "provider": "p1", "profile": "", "state": state, "origin": "api",
            "rule_id": "", "title": format!("Task {id}"), "repo": "", "cwd": "",
            "parent_id": "", "root_id": id, "external_id": "", "external_url": "",
            "sandbox": false, "created_at": rfc3339(updated - 60.0), "started_at": null,
            "updated_at": rfc3339(updated), "finished_at": rfc3339(updated),
            "timeout_seconds": 3600, "last_seq": 0, "revision": 0, "result_preview": "",
            "result_truncated": false, "error": null, "usage": null, "cancel_requested": false,
        })
    }

    fn session(id: &str, state: &str, updated: f64) -> Session {
        let mut s = parse_session(&summary_json(id, state, updated)).unwrap();
        // Most tests treat these as started sessions.
        s.started_at = s.created_at;
        s
    }

    fn provider(id: &str, profile: &str, kind: &str, code: &str) -> Value {
        json!({
            "id": id, "profile": profile, "label": id.to_uppercase(), "builtin": true,
            "capabilities": {"kind": kind, "cancel": true},
            "secrets": [{"name": "key", "set": true, "required": true}],
            "readiness": {"ready": code == "ok", "code": code, "detail": "", "checked": "shallow"},
        })
    }

    fn store(sessions: Vec<Session>, providers: Value) -> Store {
        let mut store = Store::default();
        let generation = store.begin();
        store.finish(
            generation,
            Ok(Snapshot {
                status: Status::default(),
                providers: parse_providers(&json!({ "providers": providers })),
                page: SessionPage {
                    sessions,
                    ..Default::default()
                },
            }),
            0.0,
        );
        store
    }

    #[test]
    fn every_runtime_state_maps_exactly() {
        let table = [
            ("queued", UiState::Starting, false),
            ("starting", UiState::Starting, false),
            ("running", UiState::Running, false),
            ("waiting_input", UiState::NeedsInput, false),
            ("cancelling", UiState::Stopping, false),
            ("completed", UiState::Completed, true),
            ("failed", UiState::Failed, true),
            ("timed_out", UiState::Failed, true),
            ("cancelled", UiState::Canceled, true),
        ];
        for (raw, ui, terminal) in table {
            let state = RuntimeState::parse(Some(&json!(raw)));
            assert_eq!(state.ui(), ui, "{raw}");
            assert_eq!(state.terminal(), terminal, "{raw}");
            assert_eq!(ui.terminal(), terminal, "{raw}");
        }
    }

    #[test]
    fn unrecognised_and_missing_states_are_unknown_never_guessed() {
        for value in [
            Some(json!("blocked")),
            Some(json!("Running")),
            Some(json!("canceled")),
            Some(json!("")),
            Some(json!(3)),
            Some(Value::Null),
            None,
        ] {
            let state = RuntimeState::parse(value.as_ref());
            assert_eq!(state.ui(), UiState::Unknown, "{value:?}");
            assert!(!state.terminal(), "{value:?}");
        }
        assert_eq!(
            RuntimeState::parse(Some(&json!("  blocked \n on  review "))),
            RuntimeState::Unrecognised("blocked on review".into())
        );
        // The raw word reaches the detail line; a missing one says so.
        let mut rows = store(vec![session("a", "blocked", 10.0)], json!([])).rows(10.0);
        assert_eq!(
            state_note(&rows[0], None).as_deref(),
            Some("p1 reports: blocked")
        );
        rows[0].session.runtime = RuntimeState::Missing;
        assert_eq!(
            state_note(&rows[0], None).as_deref(),
            Some("p1 reported no state.")
        );
    }

    #[test]
    fn every_ui_state_has_a_label_tone_and_icon() {
        let expect = [
            (UiState::Starting, "Starting", Tone::Info, "loader-circle"),
            (UiState::Running, "Running", Tone::Info, "loader-circle"),
            (
                UiState::NeedsInput,
                "Needs input",
                Tone::Warning,
                "message-square-dot",
            ),
            (UiState::Stopping, "Stopping", Tone::Info, "loader-circle"),
            (
                UiState::Completed,
                "Completed",
                Tone::Success,
                "circle-check",
            ),
            (UiState::Failed, "Failed", Tone::Error, "circle-alert"),
            (UiState::Canceled, "Canceled", Tone::Neutral, "ban"),
            (
                UiState::Disconnected,
                "Disconnected",
                Tone::Neutral,
                "cloud-off",
            ),
            (UiState::Unknown, "Unknown", Tone::Neutral, "circle-help"),
        ];
        assert_eq!(expect.len(), UiState::ALL.len());
        for (state, label, tone, icon) in expect {
            assert_eq!(state.label(), label);
            assert_eq!(state.tone(), tone, "{label}");
            assert_eq!(state.icon(), icon, "{label}");
            assert!(
                crate::ui::assets::ICONS.iter().any(|(n, _)| *n == icon),
                "{icon} is embedded"
            );
        }
    }

    #[test]
    fn stale_information_shows_disconnected_only_for_live_sessions() {
        let live = RuntimeState::Running;
        assert_eq!(ui_state(&live, false), UiState::Running);
        assert_eq!(ui_state(&live, true), UiState::Disconnected);
        assert_eq!(
            ui_state(&RuntimeState::Unrecognised("x".into()), true),
            UiState::Disconnected
        );
        for done in [
            RuntimeState::Completed,
            RuntimeState::Failed,
            RuntimeState::Cancelled,
        ] {
            assert_eq!(ui_state(&done, true), done.ui(), "final facts stay");
        }

        // The service: a 20 s grace period, then Disconnected with the last
        // known state; a success recovers.
        let mut s = store(
            vec![
                session("a", "running", 100.0),
                session("b", "completed", 90.0),
            ],
            json!([]),
        );
        s.failed(AgentsError::Offline, 1000.0);
        assert!(!s.stale(1019.0));
        assert_eq!(s.rows(1019.0)[0].state, UiState::Running);
        s.failed(AgentsError::Offline, 1010.0);
        assert!(s.stale(1020.0), "grace counts from the first failure");
        let rows = s.rows(1020.0);
        assert_eq!(rows[0].state, UiState::Disconnected);
        assert_eq!(rows[0].last_known, UiState::Running);
        assert_eq!(rows[1].state, UiState::Completed);
        assert_eq!(
            state_note(&rows[0], Some("12:21")).as_deref(),
            Some("Disconnected · was Running, last updated 12:21")
        );
        let (tone, message, retry) = banner(&rows[0], Some("12:21"), None).unwrap();
        assert_eq!(tone, Tone::Info);
        assert!(retry);
        assert_eq!(
            message,
            "LocalFlow can't reach this session. Last known state: Running, updated 12:21."
        );
        assert!(!s.apply_changes(Changes::default()));
        assert!(!s.stale(5000.0));
        assert_eq!(s.rows(5000.0)[0].state, UiState::Running);

        // A provider reporting itself unreachable: its live sessions only.
        let s = store(
            vec![
                Session {
                    provider: "x".into(),
                    profile: "night".into(),
                    ..session("c", "running", 5.0)
                },
                Session {
                    provider: "x".into(),
                    ..session("d", "running", 4.0)
                },
            ],
            json!([
                provider("x", "night", "cloud", "unreachable"),
                provider("x", "", "cloud", "ok"),
            ]),
        );
        let rows = s.rows(10.0);
        let state = |id: &str| rows.iter().find(|r| r.session.id == id).unwrap().state;
        assert_eq!(state("c"), UiState::Disconnected);
        assert_eq!(state("d"), UiState::Running);
    }

    #[test]
    fn rows_sort_and_group_as_designed() {
        let s = store(
            vec![
                session("done-old", "completed", 10.0),
                session("run-old", "running", 20.0),
                session("needs", "waiting_input", 5.0),
                session("done-new", "failed", 50.0),
                session("run-new", "queued", 40.0),
                session("odd", "blocked", 30.0),
                session("stop", "cancelling", 35.0),
                session("gone", "cancelled", 30.0),
            ],
            json!([]),
        );
        let rows = s.rows(100.0);
        let ids: Vec<&str> = rows.iter().map(|r| r.session.id.as_str()).collect();
        assert_eq!(
            ids,
            ["needs", "run-new", "stop", "odd", "run-old", "done-new", "gone", "done-old"]
        );
        let groups = grouped(&rows, Filter::All, "");
        let shape: Vec<(Group, usize)> = groups.iter().map(|(g, r)| (*g, r.len())).collect();
        assert_eq!(
            shape,
            [
                (Group::NeedsYou, 1),
                (Group::Running, 4),
                (Group::Earlier, 3)
            ]
        );
        assert_eq!(
            groups.iter().map(|(g, _)| g.heading()).collect::<Vec<_>>(),
            ["Needs you", "Running", "Earlier"]
        );
        let count =
            |f: Filter| -> usize { grouped(&rows, f, "").iter().map(|(_, r)| r.len()).sum() };
        assert_eq!(count(Filter::Active), 5);
        assert_eq!(count(Filter::NeedsYou), 1);
        assert_eq!(count(Filter::Done), 3);
        assert_eq!(
            Filter::ALL.map(Filter::label),
            ["All", "Active", "Needs you", "Done"]
        );
        assert_eq!(support_line(&rows, None), "3 running · 1 needs you");
        assert_eq!(support_line(&rows[5..], None), "No active sessions");
        assert_eq!(support_line(&rows[..1], None), "1 needs you");
    }

    /// The service's counts cover sessions past the pages read; live rows
    /// shown as needing you or Unknown aren't counted as running.
    #[test]
    fn the_header_counts_come_from_the_service() {
        let s = store(
            vec![
                session("needs", "waiting_input", 5.0),
                session("run", "running", 4.0),
                session("odd", "blocked", 3.0),
                session("done", "completed", 2.0),
            ],
            json!([]),
        );
        let rows = s.rows(10.0);
        assert_eq!(support_line(&rows, None), "1 running · 1 needs you");
        // 60 non-terminal sessions at the service; this window read three.
        let counts = Counts {
            active: 57,
            queued: 3,
            terminal: 140,
        };
        assert_eq!(
            support_line(&rows, Some(counts)),
            "58 running · 1 needs you"
        );
        let idle = Counts {
            active: 0,
            queued: 0,
            terminal: 9,
        };
        assert_eq!(support_line(&rows[3..], Some(idle)), "No active sessions");
        // Parsed from list, changes and status replies; kept by the store.
        let reply = json!({"counts": {"active": 2, "queued": 1, "terminal": 4}});
        assert_eq!(
            parse_counts(&reply),
            Some(Counts {
                active: 2,
                queued: 1,
                terminal: 4
            })
        );
        assert_eq!(parse_counts(&json!({"counts": {}})), None);
        let mut s = s;
        s.apply_changes(Changes {
            counts: parse_counts(&reply),
            ..Default::default()
        });
        assert_eq!(s.counts.map(|c| c.active), Some(2));
        s.apply_changes(Changes::default());
        assert_eq!(s.counts.map(|c| c.active), Some(2), "kept when absent");
    }

    /// A real service running mock providers marks its sessions Simulated
    /// just like the in-process fake.
    #[test]
    fn a_sandboxed_service_is_simulated() {
        let real = parse_status(&json!({"api_version": 1, "enabled": true, "sandbox": false}));
        let mock = parse_status(&json!({"api_version": 1, "enabled": true, "sandbox": true}));
        assert!(mock.sandbox && !real.sandbox);
        assert!(!simulated(false, Some(&real)));
        assert!(!simulated(false, None));
        assert!(simulated(false, Some(&mock)));
        assert!(simulated(true, Some(&real)));
    }

    /// Only a 404 or a parsed reply moves the shell's gate.
    #[test]
    fn only_a_decisive_status_moves_the_gate() {
        let on = parse_status(&json!({"api_version": 1, "enabled": true}));
        assert_eq!(gate(&Ok(on.clone())), Some(true));
        assert_eq!(
            gate(&Ok(Status {
                enabled: false,
                ..on.clone()
            })),
            Some(false)
        );
        assert_eq!(
            gate(&Ok(Status {
                api_version: 2,
                ..on
            })),
            Some(false)
        );
        assert_eq!(gate(&Err(AgentsError::Absent)), Some(false));
        assert_eq!(gate(&Err(AgentsError::Offline)), None);
        assert_eq!(gate(&Err(AgentsError::Refused("bad gateway".into()))), None);
    }

    /// A secret cut by the length bound must still be masked: masking runs
    /// over the whole value first.
    #[test]
    fn provider_text_is_masked_before_it_is_cut() {
        let key = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
        // The key starts 10 characters before the bound.
        let prose = format!("{} {key}", "x".repeat(TEXT_CHARS - 11));
        let s = parse_session(&json!({
            "id": "a", "title": prose, "error": {"code": "internal", "message": prose},
            "state": format!("{} {key}", "y".repeat(RAW_STATE_CHARS - 6)),
        }))
        .unwrap();
        let message = s.error.as_ref().unwrap().message.as_ref().unwrap();
        for shown in [&s.title, message] {
            assert!(shown.chars().count() <= TEXT_CHARS);
            assert!(!shown.contains("sk-abcdefg"), "{shown}");
        }
        let RuntimeState::Unrecognised(raw) = &s.runtime else {
            panic!("{:?}", s.runtime);
        };
        assert!(!raw.contains("sk-a"), "the raw state is masked: {raw}");
        let raw = RuntimeState::parse(Some(&json!("blocked token=hunter2hunter2")));
        assert_eq!(
            state_note(
                &Row {
                    session: Session {
                        runtime: raw,
                        ..Default::default()
                    },
                    state: UiState::Unknown,
                    last_known: UiState::Unknown,
                    title: String::new(),
                    provider: "P".into(),
                    monogram: "P".into(),
                    caps: Capabilities::default(),
                },
                None
            )
            .map(|n| n.contains("hunter2")),
            Some(false)
        );
        // The task's first line too.
        let mut t = session("t", "running", 1.0);
        t.title.clear();
        t.prompt = Some(format!("{} {key}", "z".repeat(TITLE_CHARS - 8)));
        assert!(!title(&t).contains("sk-abc"), "{}", title(&t));
    }

    #[test]
    fn search_matches_title_provider_and_workspace() {
        let mut s = session("a", "running", 1.0);
        s.repo = Some("acme/Billing".into());
        let rows = store(
            vec![s, session("b", "running", 2.0)],
            json!([provider("p1", "", "cloud", "ok")]),
        )
        .rows(3.0);
        let ids = |q: &str| -> Vec<String> {
            grouped(&rows, Filter::All, q)
                .into_iter()
                .flat_map(|(_, r)| r.into_iter().map(|r| r.session.id.clone()))
                .collect()
        };
        assert_eq!(ids("billing"), ["a"]);
        assert_eq!(ids("  TASK B "), ["b"]);
        assert_eq!(ids("P1").len(), 2, "provider label");
        assert!(ids("zebra").is_empty());
        assert_eq!(search_empty(" zebra "), "No sessions match “zebra”");
    }

    #[test]
    fn titles_fall_back_to_the_first_task_line_then_untitled() {
        let mut s = session("a", "running", 1.0);
        assert_eq!(title(&s), "Task a");
        s.title.clear();
        assert_eq!(title(&s), UNTITLED, "no title, no task (history off)");
        s.prompt = Some("\n\n   Add retry backoff  to the sender \nand open a PR".into());
        assert_eq!(title(&s), "Add retry backoff to the sender");
        s.prompt = Some("x".repeat(200));
        let long = title(&s);
        assert_eq!(long.chars().count(), TITLE_CHARS);
        assert!(long.ends_with('…'));
        // Provider titles are cleaned and masked at parse time.
        let parsed = parse_session(&json!({
            "id": "z", "title": "  Rotate   key sk-abcdefghijklmnopqrstu  "
        }))
        .unwrap();
        assert!(parsed.title.starts_with("Rotate key "));
        assert!(
            !parsed.title.contains("abcdefghijklmnop"),
            "{}",
            parsed.title
        );
    }

    #[test]
    fn sends_line_covers_each_payload_combination() {
        let selection = Attachment {
            phrase: "the selection (18 lines)".into(),
            token: "Selection · 18 lines".into(),
        };
        let payload = |workspace: Option<Workspace>, attachments: Vec<Attachment>| Payload {
            provider: "Claude".into(),
            workspace,
            attachments,
        };
        let repo = || Some(Workspace::Sent("acme/web".into()));
        let local = || Some(Workspace::Local("acme/web".into()));
        let named = || Some(Workspace::Named("acme/web".into()));
        let cases = [
            (
                payload(named(), vec![]),
                "Sends to Claude: your task. Workspace: acme/web.",
            ),
            (
                payload(named(), vec![selection.clone()]),
                "Sends to Claude: your task and the selection (18 lines). Workspace: acme/web.",
            ),
            (
                payload(repo(), vec![]),
                "Sends to Claude: your task and the repository name. Nothing else.",
            ),
            (
                payload(repo(), vec![selection.clone()]),
                "Sends to Claude: your task, the selection (18 lines) and the repository name.",
            ),
            (payload(None, vec![]), "Sends to Claude: your task only."),
            (
                payload(None, vec![selection.clone()]),
                "Sends to Claude: your task and the selection (18 lines).",
            ),
            (
                payload(local(), vec![]),
                &format!(
                    "Sends to Claude: your task only. It runs on {} in acme/web.",
                    this_computer()
                ),
            ),
        ];
        for (p, expected) in &cases {
            assert_eq!(sends_line(p, false), *expected);
            assert_eq!(sends_line(p, true), SANDBOX_SENDS);
        }
        assert_eq!(
            receipt(&payload(repo(), vec![selection])),
            (
                "Sent to Claude".to_string(),
                vec![
                    "Task".to_string(),
                    "Selection · 18 lines".into(),
                    "Repository name".into()
                ]
            )
        );
        assert_eq!(receipt(&payload(local(), vec![])).1, ["Task"]);
        assert_eq!(receipt(&payload(named(), vec![])).1, ["Task"]);
        assert_eq!(
            workspace_note(&payload(named(), vec![])).as_deref(),
            Some("Workspace: acme/web")
        );
        assert_eq!(workspace_note(&payload(repo(), vec![])), None);
        assert_eq!(NOTHING_SENT, "Nothing has been sent.");
    }

    /// v1 can't say whether a cloud provider was sent the repository's name
    /// (one substitutes it, one only records it locally), so the receipt
    /// never claims it was: the workspace is only named.
    #[test]
    fn a_stored_sessions_receipt_never_claims_the_workspace_was_sent() {
        let mut s = session("a", "running", 1.0);
        s.repo = Some("acme/web".into());
        let rows =
            |kind: &str| store(vec![s.clone()], json!([provider("p1", "", kind, "ok")])).rows(2.0);
        let cloud = payload_for(&rows("cloud")[0]);
        assert_eq!(cloud.workspace, Some(Workspace::Named("acme/web".into())));
        assert_eq!(cloud.provider, "P1");
        let (sent_to, items) = receipt(&cloud);
        assert_eq!(
            (sent_to.as_str(), items.as_slice()),
            ("Sent to P1", &["Task".to_string()][..])
        );
        assert_eq!(
            workspace_note(&cloud).as_deref(),
            Some("Workspace: acme/web")
        );
        let local = payload_for(&rows("local_process")[0]);
        assert_eq!(local.workspace, Some(Workspace::Local("acme/web".into())));
        // An unknown kind is treated as remote: never "runs locally" by guess.
        let other = payload_for(&rows("teleport")[0]);
        assert!(matches!(other.workspace, Some(Workspace::Named(_))));
    }

    #[test]
    fn capabilities_degrade_to_absent() {
        assert_eq!(Capabilities::parse(None), Capabilities::default());
        let caps = Capabilities::parse(Some(&json!({
            "kind": "local_process", "cancel": true, "live_message": "yes", "resume": 1,
        })));
        assert_eq!(caps.kind, Kind::LocalProcess);
        assert!(caps.cancel);
        assert!(
            !caps.live_message && !caps.resume,
            "non-booleans are absent"
        );
        // A session whose provider isn't listed has no capabilities at all.
        let rows = store(vec![session("a", "running", 1.0)], json!([])).rows(2.0);
        assert_eq!(rows[0].caps, Capabilities::default());
        assert_eq!(rows[0].provider, "p1", "falls back to the id");
    }

    #[test]
    fn readiness_codes_map_to_four_cases() {
        let r = |ready: bool, code: &str| {
            Readiness::parse(Some(&json!({"ready": ready, "code": code})))
        };
        assert_eq!(r(true, "ok"), Readiness::Connected);
        assert_eq!(r(false, "ok"), Readiness::NotConnected);
        for code in [
            "missing_secret",
            "missing_binary",
            "missing_wrapper",
            "disabled",
            "new",
        ] {
            assert_eq!(r(false, code), Readiness::NotConnected, "{code}");
        }
        assert_eq!(r(false, "auth_failed"), Readiness::NeedsSignIn);
        assert_eq!(r(false, "unreachable"), Readiness::Offline);
        assert_eq!(Readiness::parse(None), Readiness::NotConnected);
    }

    #[test]
    fn parsing_keeps_only_allow_listed_fields() {
        let reply = json!({"session": {}, "sessions": [{
            "id": "s1", "provider": "p1", "state": "failed", "title": "T",
            "prompt": "do the thing", "result_text": "done; key sk-abcdefghijklmnopqrstu0",
            "cwd": "/home/someone/private", "usage": {"tokens": 12, "turns": 3},
            "external_url": "javascript:alert(1)", "x-raw": "RAW-PROVIDER-FIELD",
            "error": {"code": "exit_nonzero",
                      "message": "push failed: token=ghp_abcdefghijklmnopqrstuvwxyz123456"},
            "created_at": "2026-11-20T12:04:00Z", "started_at": 1_795_000_000_000u64,
        }, {"provider": "no id"}]});
        let sessions = parse_sessions(&reply);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        let shown = format!("{s:?}");
        for hidden in [
            "RAW-PROVIDER-FIELD",
            "tokens",
            "javascript",
            "ghp_abcdefghij",
            "abcdefghijklmnopqrstu0",
        ] {
            assert!(!shown.contains(hidden), "{hidden} leaked: {shown}");
        }
        assert_eq!(s.external_url, None);
        assert_eq!(s.prompt.as_deref(), Some("do the thing"));
        assert!(s.result_text.as_deref().unwrap().starts_with("done; key "));
        assert_eq!(s.usage.unwrap().turns, Some(3));
        assert_eq!(
            s.cwd.as_deref(),
            Some("/home/someone/private"),
            "kept, never drawn"
        );
        assert_eq!(s.created_at, Some(1_795_176_240.0), "2026-11-20 12:04 UTC");
        assert_eq!(s.started_at, Some(1_795_000_000.0), "milliseconds");
        let error = s.error.as_ref().unwrap();
        assert_eq!(error.code, "exit_nonzero");
        assert!(error
            .message
            .as_deref()
            .unwrap()
            .starts_with("push failed: token="));
    }

    /// The v1 shapes, field by field: `error` is an object or null (never
    /// top-level `error_code`/`error_detail`), summaries carry a preview
    /// and no task or result, details carry both, times are exact RFC 3339.
    #[test]
    fn v1_summary_and_detail_shapes_parse_exactly() {
        let mut summary = summary_json("0123456789ab", "completed", 1_795_177_320.0);
        summary["result_preview"] = json!("Added the tokens.");
        summary["usage"] = json!({"cost_usd": 0.5, "duration_ms": 120000, "turns": 2});
        summary["origin"] = json!("hot_command");
        summary["rule_id"] = json!("1a2b3c4d");
        summary["parent_id"] = json!("aaaaaaaaaaaa");
        summary["external_id"] = json!("devin-77");
        summary["sandbox"] = json!(true);
        summary["last_seq"] = json!(9);
        summary["cancel_requested"] = json!(true);
        summary["started_at"] = json!("2026-11-20T12:04:00Z");
        let s = parse_session(&summary).unwrap();
        assert_eq!(s.result_preview.as_deref(), Some("Added the tokens."));
        assert_eq!(
            (s.prompt.as_deref(), s.result_text.as_deref()),
            (None, None)
        );
        assert_eq!(s.origin, Origin::HotCommand);
        assert_eq!(s.rule_id.as_deref(), Some("1a2b3c4d"));
        assert_eq!(s.parent_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(s.root_id.as_deref(), Some("0123456789ab"));
        assert_eq!(s.external_id.as_deref(), Some("devin-77"));
        assert!(s.sandbox && s.cancel_requested);
        assert_eq!((s.last_seq, s.timeout_seconds), (9, Some(3600)));
        assert_eq!(s.started_at, Some(1_795_176_240.0));
        assert_eq!(s.finished_at, Some(1_795_177_320.0));
        assert_eq!(
            usage_line(&s.usage.unwrap()).as_deref(),
            Some("2 turns · 2 min · $0.50")
        );
        assert_eq!(s.error, None, "error: null");
        assert!(
            s.rule_id.is_some()
                && parse_session(&summary_json("b", "running", 1.0))
                    .unwrap()
                    .rule_id
                    .is_none()
        );
        // The old top-level fields are not v1: ignored.
        let mut old = summary_json("c", "failed", 1.0);
        old["error_code"] = json!("spawn_failed");
        old["error_detail"] = json!("old shape");
        assert_eq!(parse_session(&old).unwrap().error, None);
        old["error"] = json!({"code": "spawn_failed", "message": "Couldn't start: no binary."});
        assert_eq!(
            parse_session(&old).unwrap().error,
            Some(SessionError {
                code: "spawn_failed".into(),
                message: Some("Couldn't start: no binary.".into())
            })
        );
        // A detail read brings task and result; later summaries keep them.
        let mut store = store(vec![session("0123456789ab", "running", 1.0)], json!([]));
        let mut detail = summary_json("0123456789ab", "completed", 2.0);
        detail.as_object_mut().unwrap().remove("result_preview");
        detail["prompt"] = json!("The task");
        detail["result_text"] = json!("Line one\nLine two");
        detail["revision"] = json!(5);
        store.apply_detail(parse_session(&detail).unwrap());
        let mut later = summary_json("0123456789ab", "completed", 2.0);
        later["revision"] = json!(5);
        assert!(!store.apply_changes(Changes {
            sessions: vec![parse_session(&later).unwrap()],
            ..Default::default()
        }));
        let kept = store.session("0123456789ab").unwrap();
        assert_eq!(kept.prompt.as_deref(), Some("The task"));
        assert_eq!(
            kept.result_text.as_deref(),
            Some("Line one\nLine two"),
            "newlines kept"
        );
        // Timestamps: the exact schema format and the robust fallbacks.
        for (raw, secs) in [
            (json!("2026-11-20T12:22:00Z"), 1_795_177_320.0),
            (json!("2026-11-20T12:22:00"), 1_795_177_320.0),
            (json!(1_795_177_320u64), 1_795_177_320.0),
        ] {
            assert_eq!(timestamp(Some(&raw)), Some(secs), "{raw}");
        }
        assert_eq!(timestamp(Some(&Value::Null)), None, "nullable");
    }

    #[test]
    fn launch_bodies_hold_only_accepted_fields_and_ids_are_v1() {
        let request = LaunchRequest {
            id: new_session_id(),
            prompt: "Add retry backoff".into(),
            provider: "claude".into(),
            profile: Some("night".into()),
            repo: Some("acme/web".into()),
        };
        assert!(is_session_id(&request.id), "{}", request.id);
        let body = request.body();
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["prompt", "provider", "id", "profile", "repo", "origin"]
        );
        assert!(keys.iter().all(|k| LAUNCH_FIELDS.contains(k)));
        assert!(!keys.contains(&"cwd"), "never cwd beside repo");
        assert_eq!(body["origin"], "api");
        let bare = LaunchRequest {
            profile: Some(String::new()),
            repo: None,
            ..request
        };
        let keys: Vec<String> = bare.body().as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["prompt", "provider", "id", "origin"]);
        let ids: std::collections::HashSet<String> = (0..500).map(|_| new_session_id()).collect();
        assert_eq!(ids.len(), 500, "never repeated");
        assert!(ids.iter().all(|id| is_session_id(id)));
        for bad in [
            "0123456789AB",
            "0123456789a",
            "0123456789abc",
            "g123456789ab",
            "",
        ] {
            assert!(!is_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn events_parse_mask_dedupe_and_stay_bounded() {
        let page = parse_events(&json!({
            "events": [
                {"seq": 1, "ts": "2026-11-20T12:04:00Z", "type": "state", "stream": "system",
                 "text": "", "state": "running", "data": {"raw_status": "x"}},
                {"seq": 2, "ts": "2026-11-20T12:05:00Z", "type": "output", "stream": "agent",
                 "text": "<b>Using</b> token=hunter2hunter2\nnext line", "state": null, "data": {}},
                {"seq": 0, "type": "output"},
            ],
            "next_after": 2, "last_seq": 2, "has_more": true, "state": "running",
            "terminal": false, "waited": false,
        }));
        assert_eq!(page.events.len(), 2, "seq 0 is not an event");
        let out = &page.events[1];
        assert!(!out.text.contains("hunter2"), "{}", out.text);
        assert!(
            out.text.starts_with("<b>Using</b>"),
            "kept as plain text, not markup"
        );
        assert!(out.text.contains('\n'));
        assert_eq!(
            activity_line(&page.events[0], "Claude"),
            ("State".to_string(), "Running".to_string())
        );
        assert_eq!(activity_line(out, "Claude").0, "Claude");
        let mut activity = Activity::new("a");
        assert!(activity.apply(page.clone()), "has_more: read again");
        activity.apply(page);
        assert_eq!(activity.len(), 2, "a repeat adds nothing");
        assert_eq!((activity.len(), activity.after), (2, 2));
        assert_eq!(
            activity.question().map(|q| q.starts_with("<b>")),
            Some(true)
        );
        let newest: Vec<u64> = activity.newest_first().map(|e| e.seq).collect();
        assert_eq!(newest, [2, 1]);
        for seq in 3..(MAX_EVENTS as u64 + 50) {
            activity.push(Event {
                seq,
                ..Default::default()
            });
        }
        assert_eq!(activity.len(), MAX_EVENTS, "bounded");
        assert_eq!(
            activity.newest_first().last().unwrap().seq,
            50,
            "oldest dropped"
        );
        let user = Event {
            seq: 9999,
            kind: EventKind::Message,
            stream: Stream::User,
            text: "yes".into(),
            ..Default::default()
        };
        assert_eq!(activity_line(&user, "Claude"), ("You".into(), "yes".into()));
    }

    #[test]
    fn actions_follow_capabilities_and_state() {
        let both = json!([{"id": "p1", "profile": "", "label": "P1",
            "capabilities": {"kind": "cloud", "cancel": true, "live_message": true},
            "readiness": {"ready": true, "code": "ok"}}]);
        let none = json!([{"id": "p1", "profile": "", "label": "P1",
            "capabilities": {"kind": "cloud"}, "readiness": {"ready": true, "code": "ok"}}]);
        let row = |state: &str, providers: &Value| {
            store(vec![session("a", state, 1.0)], providers.clone())
                .rows(2.0)
                .remove(0)
        };
        let running = actions(&row("running", &both));
        assert_eq!(
            running,
            Actions {
                open: None,
                stop: Some(true),
                reply: Some(true),
                remove: false
            }
        );
        let absent = actions(&row("running", &none));
        assert_eq!(
            (absent.stop, absent.reply),
            (None, None),
            "no capability, no control"
        );
        let done = actions(&row("completed", &both));
        assert_eq!((done.stop, done.reply, done.remove), (None, None, true));
        let starting = actions(&row("queued", &both));
        assert_eq!((starting.stop, starting.reply), (Some(true), Some(false)));
        let stopping = actions(&row("cancelling", &both));
        assert_eq!(
            stopping.stop,
            Some(false),
            "a stop under way can't be asked again"
        );
        // Disconnected: drawn but disabled; Open/Run again stay.
        let mut s = store(vec![session("a", "running", 1.0)], both.clone());
        s.failed(AgentsError::Offline, 0.0);
        let mut stale = s.rows(100.0).remove(0);
        assert_eq!(stale.state, UiState::Disconnected);
        stale.session.external_url = Some("https://example.com/s/a".into());
        let disconnected = actions(&stale);
        assert_eq!(disconnected.stop, Some(false));
        assert_eq!(disconnected.reply, Some(false));
        assert_eq!(
            disconnected.open.as_deref(),
            Some("https://example.com/s/a")
        );
        assert!(!disconnected.remove);
        // Confirmation copy.
        let r = row("running", &both);
        assert_eq!(
            stop_confirm(&r),
            (
                "Stop this session?".into(),
                "P1 will stop working on “Task a”. Work it has already pushed stays where it is."
                    .into(),
                "Stop".into()
            )
        );
        assert_eq!(
            remove_confirm(&r).1,
            "This removes “Task a” from this list only. It stays in P1."
        );
        assert_eq!(short_title(&"x".repeat(60)).chars().count(), 40);
        assert_eq!(
            stop_failed(
                &r,
                &AgentsError::rejected(409, "cancel_failed", "the provider refused")
            ),
            "Couldn't stop “Task a”. The provider refused."
        );
        assert_eq!(
            reply_placeholder("Claude"),
            "Reply to Claude — sends only this message"
        );
        assert_eq!(reply_error(&AgentsError::Offline), REPLY_FAILED);
        assert_eq!(
            reply_error(&AgentsError::rejected(502, "rate_limited", "slow down")),
            "Couldn't send your reply. It's still in the field. Slow down."
        );
    }

    #[test]
    fn results_come_from_v1_fields_only() {
        let providers = json!([{"id": "p1", "profile": "", "label": "P1",
            "capabilities": {"kind": "cloud"}, "readiness": {"ready": true, "code": "ok"}}]);
        let row = |s: Session| store(vec![s], providers.clone()).rows(2.0).remove(0);
        let mut s = session("a", "completed", 1.0);
        assert_eq!(
            results(&row(s.clone())),
            Some(Results::Empty(
                "No results were reported".into(),
                "P1 finished without attaching a summary or links.".into()
            ))
        );
        s.result_preview = Some("Short".into());
        assert_eq!(
            results(&row(s.clone())),
            Some(Results::Preview("Short".into()))
        );
        s.result_text = Some("Full text".into());
        s.result_truncated = true;
        assert_eq!(
            results(&row(s.clone())),
            Some(Results::Text {
                text: "Full text".into(),
                truncated: true
            })
        );
        for state in ["running", "failed", "cancelled"] {
            assert_eq!(results(&row(session("b", state, 1.0))), None, "{state}");
        }
    }

    #[test]
    fn status_gates_the_feature() {
        let on = parse_status(&json!({"api_version": 1, "enabled": true, "revision": 7,
            "limits": {"max_prompt_chars": 20000}}));
        assert!(available(&Ok(on.clone())));
        assert_eq!(on.max_prompt_chars, Some(20000));
        assert!(!available(&Ok(Status {
            enabled: false,
            ..on.clone()
        })));
        assert!(!available(&Ok(Status {
            api_version: 2,
            ..on
        })));
        assert!(!available(&Err(AgentsError::Absent)));
        assert!(!available(&Err(AgentsError::Offline)));
    }

    #[test]
    fn changes_upsert_delete_keep_the_task_and_never_go_back() {
        let mut s = store(vec![session("a", "running", 1.0)], json!([]));
        let mut detail = session("a", "running", 1.0);
        detail.prompt = Some("the task".into());
        detail.revision = 3;
        s.apply_detail(detail);
        let mut summary = session("a", "waiting_input", 2.0);
        summary.revision = 4;
        assert!(!s.apply_changes(Changes {
            revision: 9,
            sessions: vec![summary, session("b", "queued", 3.0)],
            ..Default::default()
        }));
        assert_eq!(s.revision, 9);
        let a = s.session("a").unwrap();
        assert_eq!(a.runtime, RuntimeState::WaitingInput);
        assert_eq!(
            a.prompt.as_deref(),
            Some("the task"),
            "summary omits prompt"
        );
        let mut older = session("a", "running", 0.0);
        older.revision = 1;
        s.apply_detail(older);
        assert_eq!(s.session("a").unwrap().runtime, RuntimeState::WaitingInput);
        assert!(!s.apply_changes(Changes {
            deleted: vec!["b".into()],
            ..Default::default()
        }));
        assert!(s.session("b").is_none());
        assert!(s.apply_changes(Changes {
            reset: true,
            ..Default::default()
        }));
    }

    #[test]
    fn late_full_reads_are_dropped_and_absent_clears() {
        let mut s = Store::default();
        let first = s.begin();
        let second = s.begin();
        assert!(!s.finish(first, Err(AgentsError::Offline), 0.0));
        assert!(!s.loaded);
        assert!(s.finish(second, Err(AgentsError::Absent), 0.0));
        assert!(s.is_empty() && s.loaded);
        assert_eq!(
            AgentsError::Refused("bad".into()).message(),
            "LocalFlow couldn't read sessions: bad"
        );
        assert_eq!(safe_detail("  "), "the request failed");
        assert!(!safe_detail("Bearer abcdefghijklmnop1234").contains("abcdefghijklmnop1234"));
    }

    #[test]
    fn banners_follow_the_state_and_capabilities() {
        let rows =
            |s: Session| store(vec![s], json!([provider("p1", "", "cloud", "ok")])).rows(0.0);
        let mut s = session("a", "waiting_input", 1.0);
        assert_eq!(
            banner(&rows(s.clone())[0], None, None),
            Some((Tone::Warning, "P1 is waiting for you.".into(), false))
        );
        s.external_url = Some("https://example.com/s/a".into());
        assert_eq!(
            banner(&rows(s.clone())[0], None, Some("  ")).unwrap().1,
            "P1 is waiting for you. Open it in P1 to see what it needs.",
            "a blank line is no question"
        );
        assert_eq!(
            banner(&rows(s)[0], None, Some("Keep the v1\nhandler?"))
                .unwrap()
                .1,
            "P1 is waiting for your answer — “Keep the v1 handler?”"
        );
        let mut f = session("b", "failed", 1.0);
        f.started_at = None;
        assert_eq!(
            banner(&rows(f.clone())[0], None, None).unwrap().1,
            "P1 didn't start this task and gave no reason."
        );
        f.error = Some(SessionError {
            code: "spawn_failed".into(),
            message: Some("Quota exceeded.".into()),
        });
        assert_eq!(
            banner(&rows(f.clone())[0], None, None).unwrap().1,
            "P1 didn't start this task. Quota exceeded."
        );
        f.started_at = Some(0.5);
        assert_eq!(
            banner(&rows(f.clone())[0], None, None).unwrap().1,
            "Quota exceeded."
        );
        f.error = None;
        f.runtime = RuntimeState::TimedOut;
        assert_eq!(
            banner(&rows(f)[0], None, None).unwrap(),
            (
                Tone::Error,
                "P1 ran out of time before finishing.".into(),
                false
            )
        );
        for quiet in ["running", "completed", "cancelled", "queued"] {
            assert_eq!(
                banner(&rows(session("c", quiet, 1.0))[0], None, None),
                None,
                "{quiet}"
            );
        }
    }

    #[test]
    fn empty_state_copy_matches_section_10() {
        assert_eq!(NO_PROVIDER_HEADING, "Connect an agent to get started");
        assert!(no_provider_body().starts_with(
            "LocalFlow starts tasks on cloud agents you connect, such as Claude or Devin. Nothing leaves this "
        ));
        assert_eq!(NO_SESSIONS_HEADING, "No sessions yet");
        assert_eq!(
            no_sessions_body(Some("Ctrl+Alt+3")),
            "Press Ctrl+Alt+3 in any app to give an agent a task."
        );
        assert!(!no_sessions_body(None).contains("Press"));
        assert_eq!(FILTER_EMPTY, "Nothing with this status");
        assert_eq!(SELECT_HEADING, "Select a session");
        assert_eq!(SELECT_BODY, "Its task, activity and results appear here.");
    }

    #[test]
    fn times_read_compactly() {
        let now = 1_800_000_000.0;
        assert_eq!(relative(now - 5.0, now), "now");
        assert_eq!(relative(now - 120.0, now), "2m");
        assert_eq!(relative(now - 3.5 * 3600.0, now), "3h");
        assert_eq!(relative(now + 30.0, now), "now", "clock skew");
        assert_eq!(relative(now - 2.0 * 86_400.0, now).len(), 3, "weekday");
        assert_eq!(duration(30.0), "under a minute");
        assert_eq!(duration(18.0 * 60.0), "18 min");
        assert_eq!(duration(120.0 * 60.0), "2 h");
        assert_eq!(duration(65.0 * 60.0), "1 h 5 min");
        let mut s = session("a", "running", now);
        s.repo = Some("acme/billing".into());
        s.started_at = Some(now - 18.0 * 60.0);
        s.finished_at = None;
        let rows = store(vec![s], json!([provider("p1", "", "cloud", "ok")])).rows(now);
        assert_eq!(
            meta_line(&rows[0], now, |_| "12:04".into()),
            "P1 · acme/billing · started 12:04 · 18 min"
        );
        assert_eq!(monogram("  devin"), "D");
        assert_eq!(monogram(""), "?");
    }
}
