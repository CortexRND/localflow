//! Settings → Agents: the provider-neutral model behind the tab
//! (`pages/settings_agents.rs`).
//!
//! GPUI-free. Everything the tab decides is here so it is unit-tested:
//! reading `GET /api/agents/status` with workspace *paths* (the only place
//! a path is parsed: [`crate::controllers::agents::Repo`] keeps its
//! aliases-only promise for the launcher), the copy of every row, the
//! readiness words, client-side workspace checks and the exact
//! `PUT /api/config` bodies. Providers are data: nothing here branches on a
//! provider's id; copy is chosen by `capabilities.kind` and the provider's
//! own label.
//!
//! A key the user types is moved out of its masked field (which keeps no
//! copy and no undo history, and zeroizes its own buffer when cleared or
//! dropped) into a [`SecretValue`]: no `Clone`, `Display` or `Serialize`, a
//! `Debug` that prints nothing of it, trimmed in place, and zeroized when
//! dropped. It goes into one request body and nowhere else. Wiping uses the
//! `zeroize` crate (volatile writes the optimiser can't remove). Copies
//! outside these buffers are beyond reach: the HTTP client's own buffers
//! (the serialised request), the platform's input method and the clipboard.

use std::fmt;
use std::path::Path;

use serde_json::{json, Value};

use crate::controllers::agents::{self, AgentsError, Kind, Provider, Readiness, SecretSlot};
use crate::controllers::agents_launcher::provider_name;
use crate::theme::Tone;
use zeroize::Zeroize;

// --------------------------------------------------------------- secret ---

/// A key on its way to the service. Never displayed, logged or kept.
pub struct SecretValue(String);

impl SecretValue {
    pub fn new(value: String) -> Self {
        SecretValue(value)
    }

    /// `value` without leading or trailing whitespace, trimmed in place
    /// (the dropped bytes are zeroized; no second copy is made).
    pub fn trimmed(value: String) -> Self {
        let trimmed = value.trim();
        let start = trimmed.as_ptr() as usize - value.as_ptr() as usize;
        let len = trimmed.len();
        // The same allocation throughout: shift the key to the front, then
        // zeroize everything after it before cutting it off.
        let mut bytes = value.into_bytes();
        bytes.copy_within(start..start + len, 0);
        bytes[len..].zeroize();
        bytes.truncate(len);
        // `bytes[..len]` is the trimmed `&str`: valid UTF-8.
        SecretValue(String::from_utf8(bytes).unwrap_or_else(|e| {
            e.into_bytes().zeroize();
            String::new()
        }))
    }

    /// The empty value: `PUT …/secret` with it removes the key.
    pub fn removal() -> Self {
        SecretValue(String::new())
    }

    /// The value, for the one request body that carries it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue([redacted])")
    }
}

/// The whole allocation (spare capacity included) is zeroized before it is
/// freed.
impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

// ------------------------------------------------------------- settings ---

/// One configured workspace (`agents.repos` entry `alias=/absolute/path`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoEntry {
    /// Exactly as the service sent it (re-sent unchanged in patches).
    pub alias: String,
    /// Exactly as the service sent it (re-sent unchanged in patches).
    pub path: String,
    /// The folder exists on the service's computer.
    pub exists: bool,
}

/// What Settings → Agents reads from `GET /api/agents/status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentSettings {
    pub api_version: u64,
    /// `agents.enabled`.
    pub enabled: bool,
    /// `agents.hot_commands` (voice triggers; read here, edited later).
    pub hot_commands: bool,
    /// `agents.sandbox`: the service runs mock providers.
    pub sandbox: bool,
    /// `agents.default_provider` (a provider id; `""` when unset).
    pub default_provider: String,
    /// `agents.default_repo` (an alias; `""` for none).
    pub default_repo: String,
    pub repos: Vec<RepoEntry>,
}

/// Field bounds when reading (the service enforces its own).
const ID_CHARS: usize = 80;
const ALIAS_CHARS: usize = 40;
const PATH_CHARS: usize = 4096;

/// A string field kept byte-exact (it is re-sent in patches), or `None`
/// when missing, empty, too long or holding a control character.
fn exact(value: Option<&Value>, max: usize) -> Option<String> {
    let s = value?.as_str()?;
    (!s.is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control))
        .then(|| s.to_string())
}

/// A workspace path, byte-exact: the service's own rule (only NUL, LF and
/// CR are refused; a tab or other control character is a valid path), so a
/// path it accepted is never dropped here and then erased by the next
/// `repos` patch.
fn path_field(value: Option<&Value>) -> Option<String> {
    let s = value?.as_str()?;
    (!s.is_empty()
        && s.chars().count() <= PATH_CHARS
        && !s.chars().any(|c| matches!(c, '\0' | '\n' | '\r')))
    .then(|| s.to_string())
}

/// `GET /api/agents/status` for the settings tab.
pub fn parse_settings(reply: &Value) -> AgentSettings {
    let flag = |name: &str| reply.get(name).and_then(Value::as_bool).unwrap_or(false);
    AgentSettings {
        api_version: reply
            .get("api_version")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        enabled: flag("enabled"),
        hot_commands: flag("hot_commands"),
        sandbox: flag("sandbox"),
        default_provider: exact(reply.get("default_provider"), ID_CHARS).unwrap_or_default(),
        default_repo: exact(reply.get("default_repo"), ALIAS_CHARS).unwrap_or_default(),
        repos: reply
            .get("repos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some(RepoEntry {
                    alias: exact(r.get("alias"), ALIAS_CHARS)?,
                    path: path_field(r.get("path"))?,
                    exists: r.get("exists").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .take(MAX_WORKSPACES)
            .collect(),
    }
}

// ----------------------------------------------------------------- copy ---

pub const TAB_LABEL: &str = "Agents";
pub const AGENTS_LABEL: &str = "Agents";
pub const AGENTS_DESCRIPTION: &str =
    "Start tasks on coding agents from LocalFlow and follow them on the Agents page.";
pub const PROVIDERS_HEADING: &str = "Providers";
pub const DEFAULTS_HEADING: &str = "Defaults";
pub const WORKSPACES_HEADING: &str = "Workspaces";
pub const WORKSPACES_SUPPORT: &str = "Folders agents may work in. Agents that run on this computer start in the folder; cloud agents only receive its name if you mention it.";
pub const NO_WORKSPACES: &str = "No workspaces yet. Agents that run on this computer need one.";
pub const DEFAULT_PROVIDER_LABEL: &str = "Default provider";
pub const DEFAULT_PROVIDER_DESCRIPTION: &str = "Used when a task doesn't name an agent.";
pub const DEFAULT_WORKSPACE_LABEL: &str = "Default workspace";
pub const DEFAULT_WORKSPACE_DESCRIPTION: &str = "Used when a task doesn't name a workspace.";
pub const NO_DEFAULT_WORKSPACE: &str = "None";
pub const CHECKING: &str = "Checking…";
pub const SERVICE_DOWN: &str =
    "LocalFlow's agent service isn't running, so these settings can't be changed.";
pub const TRY_AGAIN: &str = "Try again";
pub const SANDBOX: &str = "Sandbox: nothing is sent.";
pub const RUNS_HERE: &str = "Runs on this computer";
pub const CLOUD_AGENT: &str = "Cloud agent";
pub const CHECKED_NOW: &str = "Checked just now";
pub const CHECK: &str = "Check";
pub const ADD_KEY: &str = "Add key…";
pub const REPLACE_KEY: &str = "Replace key…";
pub const REMOVE_KEY: &str = "Remove key…";
pub const KEY_PLACEHOLDER: &str = "Paste the API key";
pub const KEY_FIELD_LABEL: &str = "API key";
pub const OPTIONAL: &str = "Optional.";
pub const SAVE: &str = "Save";
pub const CANCEL: &str = "Cancel";
pub const KEY_STORED: &str = "Stored in your system keyring. It's never shown again.";
pub const KEY_SANDBOX: &str =
    "Sandbox: keys are kept in memory for this window only and are never written to your keyring.";
pub const KEY_SAVED: &str = "Key saved";
pub const KEYRING_UNAVAILABLE: &str =
    "Your system keyring isn't available, so the key wasn't saved.";
pub const KEYS_LOCAL_ONLY: &str = "Keys can only be changed on this computer.";
pub const NAME_LABEL: &str = "Name";
pub const NAME_PLACEHOLDER: &str = "e.g. localflow";
pub const FOLDER_LABEL: &str = "Folder";
pub const FOLDER_PLACEHOLDER: &str = "/path/to/folder";
pub const CHOOSE: &str = "Choose…";
pub const ADD: &str = "Add";
pub const REMOVE: &str = "Remove…";
pub const FOLDER_NOT_FOUND: &str = "Folder not found";
pub const BAD_NAME: &str =
    "Use lowercase letters, numbers, spaces, hyphens or underscores (up to 40 characters).";
pub const NOT_ABSOLUTE: &str = "Enter the full path to a folder.";
pub const TOO_MANY: &str = "You can have up to 32 workspaces.";

/// `agents.repos` holds at most this many entries (the service's bound).
pub const MAX_WORKSPACES: usize = 32;

pub fn duplicate_workspace(alias: &str) -> String {
    format!("There's already a workspace called \u{201c}{alias}\u{201d}.")
}

/// The status word and its tone (dot **and** word; never colour alone).
pub fn readiness_word(readiness: Readiness) -> (&'static str, Tone) {
    let tone = match readiness {
        Readiness::Connected => Tone::Success,
        Readiness::NotConnected => Tone::Neutral,
        Readiness::NeedsSignIn | Readiness::Offline => Tone::Warning,
    };
    (readiness.label(), tone)
}

/// A provider row's title: its label, plus the profile when another listed
/// provider has the same label (the launcher's rule).
pub fn provider_title(provider: &Provider, all: &[Provider]) -> String {
    provider_name(provider, all)
}

/// The supporting line under a provider row: the service's own
/// `readiness.detail` (masked; one line), else where it runs; a deep
/// check's answer adds "Checked just now" until the next reload.
pub fn provider_support_line(provider: &Provider) -> String {
    let base = provider.detail.clone().unwrap_or_else(|| {
        match provider.caps.kind {
            Kind::LocalProcess => RUNS_HERE,
            Kind::Cloud => CLOUD_AGENT,
        }
        .to_string()
    });
    if provider.deep {
        format!("{base} \u{00b7} {CHECKED_NOW}")
    } else {
        base
    }
}

/// The line above the key field: what this provider receives, by kind.
pub fn disclosure(provider: &Provider, name: &str) -> String {
    match provider.caps.kind {
        Kind::Cloud => format!(
            "LocalFlow sends the tasks you launch and any context you attach to {name}. It sends nothing else."
        ),
        Kind::LocalProcess => format!("{name} runs on this computer, in the workspace you choose."),
    }
}

/// The key field's visible label: "API key" for a provider with one
/// secret, else the secret's own name (drawn in mono).
pub fn key_field_label(provider: &Provider, slot: &SecretSlot) -> String {
    if provider.secrets.len() == 1 {
        KEY_FIELD_LABEL.to_string()
    } else {
        slot.name.clone()
    }
}

/// The key field's accessible name.
pub fn key_accessible_label(provider: &Provider, slot: &SecretSlot, name: &str) -> String {
    format!("{} for {name}", key_field_label(provider, slot))
}

/// Under the key field: where the key goes. The in-process sandbox keeps
/// presence only, in memory.
pub fn key_storage_line(in_process: bool) -> &'static str {
    if in_process {
        KEY_SANDBOX
    } else {
        KEY_STORED
    }
}

/// A key change failed: the keyring and local-only cases in plain words,
/// anything else the service's (sanitised) detail.
pub fn key_error(error: &AgentsError) -> String {
    match error.code() {
        Some("secrets_unavailable") => KEYRING_UNAVAILABLE.into(),
        Some("local_only") => KEYS_LOCAL_ONLY.into(),
        _ => error.reason(),
    }
}

/// A refused write elsewhere on the tab: the service's detail.
pub fn write_error(error: &AgentsError) -> String {
    error.reason()
}

/// `(title, body, verb)` of the Remove key confirmation.
pub fn remove_key_confirm(name: &str, required: bool) -> (String, String, String) {
    let body = if required {
        format!(
            "The key is deleted from your system keyring. {name} can't start tasks until you add one."
        )
    } else {
        "The key is deleted from your system keyring.".to_string()
    };
    ("Remove this key?".into(), body, "Remove key".into())
}

/// `(title, body, verb)` of the Remove workspace confirmation.
pub fn remove_workspace_confirm(alias: &str) -> (String, String, String) {
    (
        "Remove this workspace?".into(),
        format!(
            "\u{201c}{alias}\u{201d} is removed from LocalFlow only. The folder and its files are not touched. Voice triggers that use it stop matching."
        ),
        "Remove".into(),
    )
}

/// Reload on window activation only while some provider isn't Connected
/// (a key or sign-in may have been fixed elsewhere meanwhile).
pub fn reload_on_activation(providers: &[Provider]) -> bool {
    providers
        .iter()
        .any(|p| p.readiness != Readiness::Connected)
}

// ------------------------------------------------------------- defaults ---

/// One select's options: labels, the value each stands for, the selected
/// index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choices {
    pub labels: Vec<String>,
    pub values: Vec<String>,
    pub selected: Option<usize>,
}

impl Choices {
    /// A stored value that isn't listed is shown as "Current: …" (never
    /// replaced by a default).
    fn finish(mut self, current: &str) -> Self {
        if !current.is_empty() && !self.values.iter().any(|v| v == current) {
            self.labels.push(format!("Current: {current}"));
            self.values.push(current.to_string());
        }
        self.selected = self.values.iter().position(|v| v == current);
        self
    }
}

/// Default provider: every listed provider without a profile (the config
/// key is a provider id; a profile can't be expressed), once each.
pub fn default_provider_choices(providers: &[Provider], current: &str) -> Choices {
    let mut choices = Choices {
        labels: Vec::new(),
        values: Vec::new(),
        selected: None,
    };
    for p in providers.iter().filter(|p| p.profile.is_empty()) {
        if !choices.values.contains(&p.id) {
            choices.labels.push(p.label.clone());
            choices.values.push(p.id.clone());
        }
    }
    choices.finish(current)
}

/// Default workspace: None, then each alias.
pub fn default_repo_choices(settings: &AgentSettings) -> Choices {
    let mut choices = Choices {
        labels: vec![NO_DEFAULT_WORKSPACE.to_string()],
        values: vec![String::new()],
        selected: None,
    };
    for r in &settings.repos {
        choices.labels.push(r.alias.clone());
        choices.values.push(r.alias.clone());
    }
    choices.finish(&settings.default_repo)
}

// ----------------------------------------------------------- workspaces ---

/// `^[a-z0-9][a-z0-9 _-]{0,39}$`, the service's alias rule.
pub fn valid_alias(alias: &str) -> bool {
    let mut chars = alias.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    alias.chars().count() <= 40
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, ' ' | '_' | '-'))
}

/// The service's duplicate key: the alias's letter/digit runs, lower-cased,
/// space-joined ("my-repo", "my repo" and "my_repo" are the same name).
pub fn alias_key(alias: &str) -> String {
    alias
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Python 3.13's `ntpath.isabs`: a drive-rooted or UNC/device path.
/// Root-only paths (`/srv`, `\srv`) were accepted by older Python's legacy
/// behavior but are rejected now, so don't accept them here either.
fn windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    let slash = |b: u8| matches!(b, b'/' | b'\\');
    (bytes.len() >= 2 && slash(bytes[0]) && slash(bytes[1]))
        || (bytes.len() >= 3 && bytes[1] == b':' && slash(bytes[2]))
}

/// Absolute after `~` expansion (the service's `isabs(expanduser(path))`),
/// with no NUL or line break.
pub fn absolute_folder(path: &str) -> bool {
    if path.is_empty() || path.chars().any(|c| matches!(c, '\0' | '\n' | '\r')) {
        return false;
    }
    path == "~"
        || path.starts_with("~/")
        || (cfg!(windows) && path.starts_with("~\\"))
        || if cfg!(windows) {
            windows_absolute(path)
        } else {
            Path::new(path).is_absolute()
        }
}

/// The client-side checks before an add (the service stays the
/// authority). `Ok((alias, path))` trimmed, ready to send.
pub fn validate_workspace(
    name: &str,
    folder: &str,
    existing: &[RepoEntry],
) -> Result<(String, String), String> {
    let (name, folder) = (name.trim(), folder.trim());
    if !valid_alias(name) {
        return Err(BAD_NAME.into());
    }
    if !absolute_folder(folder) {
        return Err(NOT_ABSOLUTE.into());
    }
    if existing.len() >= MAX_WORKSPACES {
        return Err(TOO_MANY.into());
    }
    let key = alias_key(name);
    if let Some(other) = existing.iter().find(|r| alias_key(&r.alias) == key) {
        return Err(duplicate_workspace(&other.alias));
    }
    Ok((name.to_string(), folder.to_string()))
}

/// The Name a chosen folder suggests: its last component, lower-cased.
pub fn name_from_folder(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// One `agents.repos` entry.
pub fn repo_entry(alias: &str, path: &str) -> String {
    format!("{alias}={path}")
}

fn repos_value<'a>(entries: impl Iterator<Item = &'a RepoEntry>) -> Value {
    Value::Array(
        entries
            .map(|r| Value::String(repo_entry(&r.alias, &r.path)))
            .collect(),
    )
}

/// A `PUT /api/config` the service accepted, applied to what is shown, so
/// the control doesn't show the old value until (or if a failing) reload
/// lands (review M5). Unknown keys are ignored; a new workspace's folder
/// is assumed present until the reload says otherwise.
pub fn apply_patch(settings: &mut AgentSettings, patch: &Value) {
    let Some(agents) = patch.get("agents").and_then(Value::as_object) else {
        return;
    };
    for (key, value) in agents {
        match (key.as_str(), value) {
            ("enabled", Value::Bool(b)) => settings.enabled = *b,
            ("hot_commands", Value::Bool(b)) => settings.hot_commands = *b,
            ("default_provider", Value::String(s)) => settings.default_provider = s.clone(),
            ("default_repo", Value::String(s)) => settings.default_repo = s.clone(),
            ("repos", Value::Array(entries)) => {
                let before = std::mem::take(&mut settings.repos);
                settings.repos = entries
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|e| e.split_once('='))
                    .map(|(alias, path)| {
                        let (alias, path) = (alias.trim(), path.trim());
                        RepoEntry {
                            alias: alias.to_string(),
                            path: path.to_string(),
                            exists: before
                                .iter()
                                .find(|r| r.alias == alias && r.path == path)
                                .is_none_or(|r| r.exists),
                        }
                    })
                    .collect();
            }
            _ => {}
        }
    }
}

/// `PUT /api/config` turning Agents on or off.
pub fn enabled_patch(on: bool) -> Value {
    json!({"agents": {"enabled": on}})
}

pub fn default_provider_patch(id: &str) -> Value {
    json!({"agents": {"default_provider": id}})
}

pub fn default_repo_patch(alias: &str) -> Value {
    json!({"agents": {"default_repo": alias}})
}

/// The whole new list with `alias=path` appended.
pub fn repos_add_patch(settings: &AgentSettings, alias: &str, path: &str) -> Value {
    let added = RepoEntry {
        alias: alias.into(),
        path: path.into(),
        exists: false,
    };
    json!({"agents": {"repos": repos_value(settings.repos.iter().chain(std::iter::once(&added)))}})
}

/// The whole new list without `alias`; removing the default workspace also
/// clears `default_repo` in the same body (the service refuses a default
/// that isn't in the list).
pub fn repos_remove_patch(settings: &AgentSettings, alias: &str) -> Value {
    let repos = repos_value(settings.repos.iter().filter(|r| r.alias != alias));
    if settings.default_repo == alias {
        json!({"agents": {"repos": repos, "default_repo": ""}})
    } else {
        json!({"agents": {"repos": repos}})
    }
}

/// A workspace path as shown (single line, bounded; cut at the end).
pub fn path_line(path: &str) -> String {
    agents::clean(path, 120)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::agents::parse_provider;

    fn provider(kind: &str, code: &str, detail: &str, secrets: Value) -> Provider {
        parse_provider(&json!({
            "id": "p1", "profile": "", "label": "Prov", "builtin": true,
            "capabilities": {"kind": kind},
            "secrets": secrets,
            "readiness": {"ready": code == "ok", "code": code, "detail": detail,
                          "checked": "shallow"},
        }))
        .unwrap()
    }

    /// `path` written absolute on the test platform (the input starts with
    /// `/`); Windows gets a drive root because root-only paths are not
    /// absolute to the service's current Python.
    fn abs(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{path}")
        } else {
            path.into()
        }
    }

    fn settings() -> AgentSettings {
        parse_settings(&json!({
            "api_version": 1, "enabled": true, "hot_commands": false, "sandbox": false,
            "default_provider": "claude", "default_repo": "web",
            "repos": [
                {"alias": "web", "path": "/srv/web", "exists": true},
                {"alias": "my-api", "path": "/srv/a  b=c", "exists": false},
            ],
        }))
    }

    #[test]
    fn status_parses_paths_byte_exact_and_drops_bad_entries() {
        let s = settings();
        assert!(s.enabled && !s.sandbox && s.api_version == 1);
        assert_eq!(s.default_provider, "claude");
        assert_eq!(s.default_repo, "web");
        assert_eq!(s.repos.len(), 2);
        assert_eq!(s.repos[1].path, "/srv/a  b=c", "spaces and '=' survive");
        assert!(!s.repos[1].exists);
        let odd = parse_settings(&json!({"repos": [
            {"alias": "x", "path": "/a\nb", "exists": true},
            {"alias": "", "path": "/a"},
            {"alias": 3, "path": "/a"},
            {"path": "/a"},
        ], "default_repo": ""}));
        assert!(odd.repos.is_empty());
        assert_eq!(odd.default_repo, "");
        assert_eq!(odd.api_version, 0);
    }

    /// A path the service accepts (a tab, another control character) is
    /// kept and re-sent unchanged by the next `repos` patch; aliases stay
    /// strict.
    #[test]
    fn a_control_character_path_round_trips() {
        let tabbed = abs("/srv/a\tb");
        let bell = abs("/srv/x\u{7}y");
        let web = abs("/srv/web");
        let docs = abs("/srv/docs");
        let s = parse_settings(&json!({"api_version": 1, "default_repo": "", "repos": [
            {"alias": "tabbed", "path": tabbed, "exists": true},
            {"alias": "bell", "path": bell, "exists": true},
            {"alias": "web", "path": web, "exists": true},
            {"alias": "bad\talias", "path": "/srv/z", "exists": true},
            {"alias": "nul", "path": "/srv/a\u{0}b", "exists": true},
            {"alias": "cr", "path": "/srv/a\rb", "exists": true},
        ]}));
        let shown: Vec<(&str, &str)> = s
            .repos
            .iter()
            .map(|r| (r.alias.as_str(), r.path.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("tabbed", tabbed.as_str()),
                ("bell", bell.as_str()),
                ("web", web.as_str())
            ]
        );
        assert_eq!(
            repos_remove_patch(&s, "web"),
            json!({"agents": {"repos": [repo_entry("tabbed", &tabbed), repo_entry("bell", &bell)]}})
        );
        assert_eq!(
            repos_add_patch(&s, "docs", &docs)["agents"]["repos"][0],
            repo_entry("tabbed", &tabbed)
        );
        assert!(absolute_folder(&tabbed), "the add check agrees");
    }

    #[test]
    fn every_readiness_code_has_a_word_and_tone() {
        for (code, ready, word, tone) in [
            ("ok", true, "Connected", Tone::Success),
            ("ok", false, "Not connected", Tone::Neutral),
            ("missing_secret", false, "Not connected", Tone::Neutral),
            ("missing_binary", false, "Not connected", Tone::Neutral),
            ("missing_wrapper", false, "Not connected", Tone::Neutral),
            ("missing_profile", false, "Not connected", Tone::Neutral),
            ("disabled", false, "Not connected", Tone::Neutral),
            ("something_new", false, "Not connected", Tone::Neutral),
            ("auth_failed", false, "Needs sign-in", Tone::Warning),
            ("unreachable", false, "Offline", Tone::Warning),
        ] {
            let r = Readiness::parse(Some(&json!({"ready": ready, "code": code})));
            assert_eq!(readiness_word(r), (word, tone), "{code}");
        }
    }

    #[test]
    fn provider_lines_and_disclosures_follow_kind_and_detail() {
        let cloud = provider("cloud", "missing_secret", "", json!([]));
        assert_eq!(provider_support_line(&cloud), "Cloud agent");
        let local = provider("local_process", "ok", "", json!([]));
        assert_eq!(provider_support_line(&local), "Runs on this computer");
        let detail = provider("cloud", "missing_secret", "Add a Devin API key.", json!([]));
        assert_eq!(provider_support_line(&detail), "Add a Devin API key.");
        let mut deep = detail.clone();
        deep.deep = true;
        assert_eq!(
            provider_support_line(&deep),
            "Add a Devin API key. \u{00b7} Checked just now"
        );
        assert_eq!(
            disclosure(&cloud, "Devin"),
            "LocalFlow sends the tasks you launch and any context you attach to Devin. It sends nothing else."
        );
        assert_eq!(
            disclosure(&local, "Claude"),
            "Claude runs on this computer, in the workspace you choose."
        );
        // An unknown kind is remote: no "stays on this computer" by guess.
        let unknown = provider("satellite", "ok", "", json!([]));
        assert!(disclosure(&unknown, "X").starts_with("LocalFlow sends"));
    }

    #[test]
    fn provider_detail_is_masked_and_secret_slots_allow_listed() {
        let p = provider(
            "cloud",
            "auth_failed",
            "bad key api_key=sk-abcdefghijklmnopqrstuvwxyz0123",
            json!([
                {"name": "agent_devin_api_key", "set": true, "required": true},
                {"name": "Agent Bad Name", "set": true},
                {"name": "agent_extra", "set": false, "required": false, "value": "nope"},
            ]),
        );
        assert!(!p.detail.as_deref().unwrap().contains("abcdefghij"));
        assert_eq!(p.code, "auth_failed");
        assert_eq!(
            p.secrets,
            [
                SecretSlot {
                    name: "agent_devin_api_key".into(),
                    set: true,
                    required: true
                },
                SecretSlot {
                    name: "agent_extra".into(),
                    set: false,
                    required: false
                },
            ]
        );
        assert!(p.builtin && !p.deep);
        let weird = parse_provider(&json!({"id": "x",
            "readiness": {"code": "Not A Slug!", "checked": "deep"}}))
        .unwrap();
        assert_eq!(weird.code, "");
        assert!(weird.deep);
        assert!(weird.secrets.is_empty());
    }

    #[test]
    fn key_copy_is_exact() {
        let one = provider(
            "cloud",
            "missing_secret",
            "",
            json!([{"name": "agent_a", "set": false, "required": true}]),
        );
        assert_eq!(key_field_label(&one, &one.secrets[0]), "API key");
        assert_eq!(
            key_accessible_label(&one, &one.secrets[0], "Devin"),
            "API key for Devin"
        );
        let two = provider(
            "cloud",
            "ok",
            "",
            json!([{"name": "agent_a", "set": true}, {"name": "agent_b", "set": false}]),
        );
        assert_eq!(key_field_label(&two, &two.secrets[1]), "agent_b");
        assert_eq!(
            key_storage_line(false),
            "Stored in your system keyring. It's never shown again."
        );
        assert_eq!(
            key_storage_line(true),
            "Sandbox: keys are kept in memory for this window only and are never written to your keyring."
        );
        let unavailable = AgentsError::rejected(503, "secrets_unavailable", "keyring gone");
        assert_eq!(
            key_error(&unavailable),
            "Your system keyring isn't available, so the key wasn't saved."
        );
        let local = AgentsError::rejected(403, "local_only", "x");
        assert_eq!(
            key_error(&local),
            "Keys can only be changed on this computer."
        );
        let other =
            AgentsError::rejected(400, "validation", "this provider does not use that secret");
        assert_eq!(key_error(&other), "This provider does not use that secret.");
        assert_eq!(
            remove_key_confirm("Devin", true),
            (
                "Remove this key?".into(),
                "The key is deleted from your system keyring. Devin can't start tasks until you add one.".into(),
                "Remove key".into()
            )
        );
        assert_eq!(
            remove_key_confirm("Claude", false).1,
            "The key is deleted from your system keyring."
        );
        assert_eq!(
            remove_workspace_confirm("web"),
            (
                "Remove this workspace?".into(),
                "\u{201c}web\u{201d} is removed from LocalFlow only. The folder and its files are not touched. Voice triggers that use it stop matching.".into(),
                "Remove".into()
            )
        );
    }

    #[test]
    fn a_secret_value_never_prints() {
        let value = SecretValue::new("sk-planted-0123456789".into());
        assert_eq!(format!("{value:?}"), "SecretValue([redacted])");
        assert_eq!(value.expose(), "sk-planted-0123456789");
        assert!(!value.is_empty());
        assert!(SecretValue::removal().is_empty());
        // Pasted with a trailing newline (and stray spaces): trimmed.
        let pasted = SecretValue::trimmed("  sk-planted-0123456789\n".into());
        assert_eq!(pasted.expose(), "sk-planted-0123456789");
        assert_eq!(
            SecretValue::trimmed("é-key\u{3000}".into()).expose(),
            "é-key"
        );
        assert!(SecretValue::trimmed(" \n\t".into()).is_empty());
    }

    #[test]
    fn windows_absolute_follows_python_not_rust() {
        for path in ["//srv/docs", r"\\srv\docs", "C:/src/docs", r"C:\src\docs"] {
            assert!(windows_absolute(path), "absolute");
        }
        for path in ["/srv/docs", r"\srv\docs", "C:relative", "srv/docs", ""] {
            assert!(!windows_absolute(path), "relative");
        }
    }

    #[test]
    fn workspace_checks_in_order_with_exact_copy() {
        let s = settings();
        let docs = abs("/srv/docs");
        assert_eq!(
            validate_workspace(" docs ", &format!(" {docs} "), &s.repos),
            Ok(("docs".into(), docs))
        );
        assert_eq!(
            validate_workspace("~home", &abs("/x"), &s.repos),
            Err(BAD_NAME.into())
        );
        for bad in ["", "Docs", "-docs", "docs/web", &"a".repeat(41)] {
            assert_eq!(
                validate_workspace(bad, &abs("/x"), &[]),
                Err(BAD_NAME.into()),
                "{bad}"
            );
        }
        assert!(validate_workspace(&"a".repeat(40), &abs("/x"), &[]).is_ok());
        assert!(validate_workspace("my repo_2-x", "~/src", &[]).is_ok());
        for bad in ["", "src/docs", "./docs", "/a\nb"] {
            assert_eq!(
                validate_workspace("docs", bad, &[]),
                Err(NOT_ABSOLUTE.into()),
                "{bad:?}"
            );
        }
        let full: Vec<RepoEntry> = (0..32)
            .map(|i| RepoEntry {
                alias: format!("r{i}"),
                path: abs("/x"),
                exists: true,
            })
            .collect();
        assert_eq!(
            validate_workspace("docs", &abs("/x"), &full),
            Err("You can have up to 32 workspaces.".into())
        );
        assert_eq!(
            validate_workspace("my_api", &abs("/x"), &s.repos),
            Err("There's already a workspace called \u{201c}my-api\u{201d}.".into()),
            "the service's duplicate rule"
        );
        assert_eq!(
            name_from_folder(Path::new("/home/me/LocalFlow")),
            "localflow"
        );
    }

    #[test]
    fn patches_are_exact_and_removing_the_default_clears_it() {
        let s = settings();
        assert_eq!(enabled_patch(true), json!({"agents": {"enabled": true}}));
        assert_eq!(
            default_provider_patch("devin"),
            json!({"agents": {"default_provider": "devin"}})
        );
        assert_eq!(
            default_repo_patch(""),
            json!({"agents": {"default_repo": ""}})
        );
        assert_eq!(
            repos_add_patch(&s, "docs", "/srv/docs"),
            json!({"agents": {"repos": ["web=/srv/web", "my-api=/srv/a  b=c", "docs=/srv/docs"]}})
        );
        assert_eq!(
            repos_remove_patch(&s, "my-api"),
            json!({"agents": {"repos": ["web=/srv/web"]}})
        );
        assert_eq!(
            repos_remove_patch(&s, "web"),
            json!({"agents": {"repos": ["my-api=/srv/a  b=c"], "default_repo": ""}}),
            "one body with both keys"
        );
    }

    /// M5: an accepted write shows at once (no snap back while reloading).
    #[test]
    fn an_accepted_patch_is_applied_to_what_is_shown() {
        let mut s = settings();
        apply_patch(&mut s, &enabled_patch(false));
        assert!(!s.enabled);
        apply_patch(&mut s, &default_provider_patch("devin"));
        assert_eq!(s.default_provider, "devin");
        let remove = repos_remove_patch(&s, "web");
        apply_patch(&mut s, &remove);
        assert_eq!(s.default_repo, "");
        assert_eq!(s.repos.len(), 1);
        assert!(!s.repos[0].exists, "a known folder keeps its state");
        let add = repos_add_patch(&s, "docs", "/srv/docs");
        apply_patch(&mut s, &add);
        assert_eq!(s.repos[1].alias, "docs");
        assert!(s.repos[1].exists, "assumed present until reloaded");
        apply_patch(
            &mut s,
            &json!({"agents": {"hot_commands": true, "other": 1}}),
        );
        assert!(s.hot_commands);
        apply_patch(&mut s, &json!({"general": {}}));
    }

    #[test]
    fn default_choices_list_ids_and_keep_an_unlisted_value() {
        let all = crate::controllers::agents::parse_providers(&json!({"providers": [
            {"id": "claude", "profile": "", "label": "Claude"},
            {"id": "http", "profile": "p1", "label": "My agent"},
            {"id": "devin", "profile": "", "label": "Devin"},
        ]}));
        let c = default_provider_choices(&all, "devin");
        assert_eq!(c.labels, ["Claude", "Devin"]);
        assert_eq!(c.values, ["claude", "devin"]);
        assert_eq!(c.selected, Some(1));
        let c = default_provider_choices(&all, "gone");
        assert_eq!(c.labels, ["Claude", "Devin", "Current: gone"]);
        assert_eq!(c.selected, Some(2));
        let r = default_repo_choices(&settings());
        assert_eq!(r.labels, ["None", "web", "my-api"]);
        assert_eq!(r.values, ["", "web", "my-api"]);
        assert_eq!(r.selected, Some(1));
        let mut none = settings();
        none.default_repo.clear();
        assert_eq!(default_repo_choices(&none).selected, Some(0));
    }

    #[test]
    fn activation_reloads_only_while_something_is_not_connected() {
        let ok = provider("cloud", "ok", "", json!([]));
        let off = provider("cloud", "unreachable", "", json!([]));
        assert!(!reload_on_activation(std::slice::from_ref(&ok)));
        assert!(reload_on_activation(&[ok, off]));
        assert!(!reload_on_activation(&[]));
    }

    #[test]
    fn section_copy_is_pinned() {
        assert_eq!(
            AGENTS_DESCRIPTION,
            "Start tasks on coding agents from LocalFlow and follow them on the Agents page."
        );
        assert_eq!(
            SERVICE_DOWN,
            "LocalFlow's agent service isn't running, so these settings can't be changed."
        );
        assert_eq!(SANDBOX, "Sandbox: nothing is sent.");
        assert_eq!(
            WORKSPACES_SUPPORT,
            "Folders agents may work in. Agents that run on this computer start in the folder; cloud agents only receive its name if you mention it."
        );
        assert_eq!(
            NO_WORKSPACES,
            "No workspaces yet. Agents that run on this computer need one."
        );
        assert_eq!(
            DEFAULT_PROVIDER_DESCRIPTION,
            "Used when a task doesn't name an agent."
        );
        assert_eq!(
            DEFAULT_WORKSPACE_DESCRIPTION,
            "Used when a task doesn't name a workspace."
        );
        assert_eq!(
            BAD_NAME,
            "Use lowercase letters, numbers, spaces, hyphens or underscores (up to 40 characters)."
        );
        assert_eq!(NOT_ABSOLUTE, "Enter the full path to a folder.");
        assert_eq!(path_line("/a   b"), "/a b");
    }
}
