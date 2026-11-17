//! Meeting backend: the contract of `localflow/meeting_client.py` (the local
//! server's `/meeting/*` endpoints), the Hub's note-save ordering helper and an
//! in-memory fake.
//!
//! Only the methods the Hub uses are in the trait: `status`, `templates`,
//! `start`, `save_notes`, `stop`, `chat`, `dismiss`. Payloads are the server's
//! JSON objects ([`serde_json::Value`]), read by `hub_model` helpers such as
//! `meeting_capture_status` and `saved_notes_message`. The real HTTP client is
//! a platform-layer concern (no network in this crate).

use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use serde_json::{json, Value};

use lf_providers::pyfmt::py_repr;

use crate::pystr::PyStr;

/// `server.MAX_USER_NOTES_CHARS` / `MAX_CHAT_QUESTION_CHARS`.
pub const MAX_USER_NOTES_CHARS: usize = 100_000;
pub const MAX_CHAT_QUESTION_CHARS: usize = 2_000;
/// `meetings.DEFAULT_TITLE`.
pub const DEFAULT_TITLE: &str = "Meeting";

/// `meetings.is_default_title` for a string title: empty or the plain default
/// "Meeting" in any case or spacing (`" ".join(title.split()).casefold()`).
pub fn is_default_title(title: &str) -> bool {
    let folded = caseless::default_case_fold_str(&title.py_split().join(" "));
    folded.is_empty() || folded == caseless::default_case_fold_str(DEFAULT_TITLE)
}

/// Shown when the local server cannot be reached.
pub const SERVER_DOWN: &str =
    "LocalFlow server not running — open LocalFlow.app (it starts the server) or run `lf serve`";

/// A meeting request failed or the local server is unavailable
/// (`MeetingClientError`). The message is user-facing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingClientError(pub String);

impl fmt::Display for MeetingClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MeetingClientError {}

pub type MeetingResult<T> = Result<T, MeetingClientError>;

/// The meeting endpoints the Hub talks to.
pub trait MeetingClient: Send + Sync {
    /// `GET /meeting/status`.
    fn status(&self) -> MeetingResult<Value>;
    /// `GET /meeting/templates`: `{"default": id, "templates": [{id, name, ...}]}`.
    fn templates(&self) -> MeetingResult<Value>;
    /// `POST /meeting/start`. `auto_title`: `Some(true)` = the title is a
    /// placeholder to name on save, `Some(false)` = the user typed it (kept,
    /// even "Meeting"), `None` = let the server decide.
    fn start(
        &self,
        title: &str,
        category: &str,
        template: &str,
        user_notes: &str,
        auto_title: Option<bool>,
    ) -> MeetingResult<Value>;
    /// `POST /meeting/notes`.
    fn save_notes(&self, notes: &str) -> MeetingResult<Value>;
    /// `POST /meeting/stop`.
    fn stop(&self) -> MeetingResult<Value>;
    /// `POST /meeting/chat`.
    fn chat(&self, question: &str) -> MeetingResult<Value>;
    /// `POST /meeting/dismiss`.
    fn dismiss(&self) -> MeetingResult<()>;
}

/// The resolved notes file if it is an existing `.md` file inside the vault,
/// else `None`. The path comes from whatever answers on the local port, so it
/// must never reach the OS opener unchecked (`vault_notes_path`).
pub fn vault_notes_path(notes_path: &str, vault_path: &str) -> Option<String> {
    if notes_path.py_strip().is_empty() || vault_path.is_empty() {
        return None;
    }
    let expand = |value: &str| lf_core::home::expand_user(Path::new(value)).ok();
    let vault = std::fs::canonicalize(expand(vault_path)?).ok()?;
    let target = std::fs::canonicalize(expand(notes_path)?).ok()?;
    let is_md = target
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().to_lowercase() == "md");
    if !is_md || !target.is_file() || !target.starts_with(&vault) {
        return None;
    }
    Some(target.to_string_lossy().into_owned())
}

/// Puts meeting notes on the server in the order the Hub asked for them
/// (`_MeetingNotesWriter`).
///
/// Every save takes a ticket on the main thread; writes are serialized and a
/// write older than one already sent is dropped, so the flush a closing Hub
/// sends (the newest ticket) is never overwritten by an earlier save still in
/// flight.
#[derive(Debug, Default)]
pub struct MeetingNotesWriter {
    issued: Mutex<u64>,
    sent: Mutex<u64>,
}

impl MeetingNotesWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the next ticket (call on the thread that decides the order).
    pub fn ticket(&self) -> u64 {
        let mut issued = self.issued.lock().unwrap_or_else(|e| e.into_inner());
        *issued += 1;
        *issued
    }

    /// Send `text` unless a newer ticket was already sent (then `{}`).
    pub fn save(
        &self,
        client: &dyn MeetingClient,
        ticket: u64,
        text: &str,
    ) -> MeetingResult<Value> {
        let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
        if ticket < *sent {
            return Ok(json!({}));
        }
        *sent = ticket;
        client.save_notes(text)
    }
}

/// One recorded call on a [`FakeMeetingClient`].
#[derive(Clone, Debug, PartialEq)]
pub enum MeetingCall {
    Status,
    Templates,
    Start {
        title: String,
        category: String,
        template: String,
        user_notes: String,
        auto_title: Option<bool>,
    },
    SaveNotes(String),
    Stop,
    Chat(String),
    Dismiss,
}

#[derive(Default)]
struct FakeState {
    session: Option<serde_json::Map<String, Value>>,
    last_saved: Option<Value>,
    templates: Vec<Value>,
    default_template: String,
    calls: Vec<MeetingCall>,
    down: Option<String>,
    chat_answer: Option<String>,
    saved_notes_path: String,
    saved_log_path: String,
}

/// In-memory [`MeetingClient`] following the server's rules: one running
/// session, notes need a session (409), `stop` needs a session (409), chat
/// needs a saved meeting (404) and a non-empty question (400). No network.
pub struct FakeMeetingClient {
    state: Mutex<FakeState>,
}

impl Default for FakeMeetingClient {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeMeetingClient {
    pub fn new() -> Self {
        let state = FakeState {
            templates: vec![json!({
                "id": "general", "name": "General", "description": "General notes",
                "sections": ["Summary", "Action items"],
            })],
            default_template: "general".to_string(),
            saved_notes_path: "/vault/notes/meetings/Other/2026-11-15 1430 Meeting.md".into(),
            saved_log_path: "/vault/logs/2026-11-15 1430 Meeting.md".into(),
            ..FakeState::default()
        };
        FakeMeetingClient {
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every call so far, in order.
    pub fn calls(&self) -> Vec<MeetingCall> {
        self.lock().calls.clone()
    }

    /// Make every call fail with `message` (e.g. [`SERVER_DOWN`]); `None` heals.
    pub fn set_down(&self, message: Option<&str>) {
        self.lock().down = message.map(str::to_string);
    }

    /// Replace the template list and the default id.
    pub fn set_templates(&self, templates: Vec<Value>, default: &str) {
        let mut state = self.lock();
        state.templates = templates;
        state.default_template = default.to_string();
    }

    /// Where `stop` says the notes were saved.
    pub fn set_saved_notes_path(&self, path: &str) {
        self.lock().saved_notes_path = path.to_string();
    }

    /// Canned answer for `chat` (default: echoes the question).
    pub fn set_chat_answer(&self, answer: Option<&str>) {
        self.lock().chat_answer = answer.map(str::to_string);
    }

    /// Set live-session fields such as `system_audio` / `system_audio_error`.
    pub fn update_session(&self, fields: Value) {
        let mut state = self.lock();
        if let (Some(session), Some(fields)) = (state.session.as_mut(), fields.as_object()) {
            for (key, value) in fields {
                session.insert(key.clone(), value.clone());
            }
        }
    }

    /// Seed the "last saved" record.
    pub fn set_last_saved(&self, saved: Option<Value>) {
        self.lock().last_saved = saved;
    }

    fn check(state: &FakeState) -> MeetingResult<()> {
        match &state.down {
            Some(message) => Err(MeetingClientError(message.clone())),
            None => Ok(()),
        }
    }
}

impl MeetingClient for FakeMeetingClient {
    fn status(&self) -> MeetingResult<Value> {
        let mut state = self.lock();
        state.calls.push(MeetingCall::Status);
        Self::check(&state)?;
        Ok(json!({
            "watching": false,
            "watch_error": "",
            "mic_busy": false,
            "detected": false,
            "platform": "",
            "auto_start": false,
            "capture_system_audio": false,
            "session": state.session.clone().map(Value::Object),
            "last_saved": state.last_saved,
            "chat_available": state.last_saved.is_some(),
        }))
    }

    fn templates(&self) -> MeetingResult<Value> {
        let mut state = self.lock();
        state.calls.push(MeetingCall::Templates);
        Self::check(&state)?;
        Ok(json!({"default": state.default_template, "templates": state.templates}))
    }

    fn start(
        &self,
        title: &str,
        category: &str,
        template: &str,
        user_notes: &str,
        auto_title: Option<bool>,
    ) -> MeetingResult<Value> {
        let mut state = self.lock();
        state.calls.push(MeetingCall::Start {
            title: title.into(),
            category: category.into(),
            template: template.into(),
            user_notes: user_notes.into(),
            auto_title,
        });
        Self::check(&state)?;
        // `_start_session`: nothing typed -> always named on save; otherwise an
        // explicit `auto_title` wins, else the title decides (`is_default_title`).
        let (title_auto, stored_title) = if title.py_strip().is_empty() {
            (true, "")
        } else {
            (auto_title.unwrap_or_else(|| is_default_title(title)), title)
        };
        let template_id = if template.is_empty() {
            state.default_template.clone()
        } else {
            template.to_string()
        };
        let known = state
            .templates
            .iter()
            .any(|t| t.get("id").and_then(Value::as_str) == Some(template));
        if !template.is_empty() && !known {
            return Err(MeetingClientError(format!(
                "unknown template {}",
                py_repr(template)
            )));
        }
        if user_notes.chars().count() > MAX_USER_NOTES_CHARS {
            return Err(MeetingClientError("notes too long".into()));
        }
        if state.session.is_some() {
            return Err(MeetingClientError(
                "a meeting session is already running".into(),
            ));
        }
        // Without a watcher there is no detected platform to fall back to.
        let category = if category.is_empty() {
            "Other"
        } else {
            category
        };
        let session = json!({
            "title": if stored_title.is_empty() { "Meeting" } else { stored_title },
            "title_auto": title_auto, "category": category,
            "template": template_id, "user_notes": user_notes,
            "system_audio": "off", "system_audio_error": "", "seconds": 0,
            "segments": [], "segment_count": 0,
        });
        state.session = session.as_object().cloned();
        Ok(json!({
            "ok": true, "category": category, "template": template_id,
            "system_audio": "off", "system_audio_error": "",
        }))
    }

    fn save_notes(&self, notes: &str) -> MeetingResult<Value> {
        let mut state = self.lock();
        state.calls.push(MeetingCall::SaveNotes(notes.into()));
        Self::check(&state)?;
        if notes.chars().count() > MAX_USER_NOTES_CHARS {
            return Err(MeetingClientError("notes too long".into()));
        }
        let Some(session) = state.session.as_mut() else {
            return Err(MeetingClientError("no meeting session running".into()));
        };
        session.insert("user_notes".into(), notes.into());
        Ok(json!({"ok": true, "chars": notes.chars().count()}))
    }

    fn stop(&self) -> MeetingResult<Value> {
        let mut state = self.lock();
        state.calls.push(MeetingCall::Stop);
        Self::check(&state)?;
        let Some(session) = state.session.take() else {
            return Err(MeetingClientError("no meeting session running".into()));
        };
        let text = |key: &str| {
            session
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        // No model here: an automatic title stays the stand-in ("Meeting") and
        // `title_generated` is false, like the server's fallback path.
        let title = match text("title") {
            t if t.is_empty() => "Meeting".to_string(),
            t => t,
        };
        let segments = session
            .get("segment_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let saved = json!({
            "title": title,
            "title_generated": false,
            "notes_path": state.saved_notes_path,
            "log_path": state.saved_log_path,
            "segments": segments,
            "template": text("template"),
            "enhanced": !text("user_notes").py_strip().is_empty(),
        });
        state.last_saved = Some(saved.clone());
        Ok(saved)
    }

    fn chat(&self, question: &str) -> MeetingResult<Value> {
        let mut state = self.lock();
        state.calls.push(MeetingCall::Chat(question.into()));
        Self::check(&state)?;
        let question = question.py_strip();
        if question.is_empty() {
            return Err(MeetingClientError("question is empty".into()));
        }
        if question.chars().count() > MAX_CHAT_QUESTION_CHARS {
            return Err(MeetingClientError("question too long".into()));
        }
        // The server also falls back to the newest transcript log on disk; the
        // fake has no disk, so only a saved meeting counts.
        if state.last_saved.is_none() {
            return Err(MeetingClientError("no saved meeting transcript yet".into()));
        }
        let answer = state
            .chat_answer
            .clone()
            .unwrap_or_else(|| format!("(fake answer to: {question})"));
        Ok(json!({"answer": answer, "log_path": state.saved_log_path}))
    }

    fn dismiss(&self) -> MeetingResult<()> {
        let mut state = self.lock();
        state.calls.push(MeetingCall::Dismiss);
        Self::check(&state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_status_notes_stop_round_trip() {
        let client = FakeMeetingClient::new();
        assert_eq!(client.status().unwrap()["session"], Value::Null);
        client.start("", "Zoom", "general", "agenda", None).unwrap();
        let status = client.status().unwrap();
        assert_eq!(status["session"]["user_notes"], "agenda");
        assert_eq!(status["session"]["title"], "Meeting");
        assert_eq!(status["session"]["title_auto"], true);
        client.save_notes("agenda\n- item").unwrap();
        assert_eq!(
            client.status().unwrap()["session"]["user_notes"],
            "agenda\n- item"
        );
        let saved = client.stop().unwrap();
        assert_eq!(saved["title"], "Meeting");
        assert!(client.status().unwrap()["session"].is_null());
        assert_eq!(client.status().unwrap()["last_saved"], saved);
    }

    #[test]
    fn start_refusals_use_the_servers_exact_details() {
        let client = FakeMeetingClient::new();
        // server._start_session: 409 detail while a session is running.
        client.start("Standup", "", "", "", None).unwrap();
        assert_eq!(
            client.start("Again", "", "", "", None).unwrap_err().0,
            "a meeting session is already running"
        );
        client.stop().unwrap();
        // 400 `unknown template {template!r}` (Python repr quoting).
        assert_eq!(
            client.start("T", "", "nope", "", None).unwrap_err().0,
            "unknown template 'nope'"
        );
        assert_eq!(
            client.start("T", "", "it's", "", None).unwrap_err().0,
            "unknown template \"it's\""
        );
        // 413 `notes too long` counts characters, not bytes.
        let long = "é".repeat(MAX_USER_NOTES_CHARS + 1);
        assert_eq!(
            client.start("T", "", "", &long, None).unwrap_err().0,
            "notes too long"
        );
        assert_eq!(client.save_notes(&long).unwrap_err().0, "notes too long");
        let exact = "é".repeat(MAX_USER_NOTES_CHARS);
        assert!(client.start("T", "", "", &exact, None).is_ok());
        // The refused attempts did not start anything.
        assert_eq!(client.status().unwrap()["session"]["title"], "T");
    }

    #[test]
    fn start_response_and_title_rules_follow_the_server() {
        let client = FakeMeetingClient::new();
        let reply = client.start("  ", "", "", "", Some(false)).unwrap();
        assert_eq!(
            reply,
            json!({
                "ok": true, "category": "Other", "template": "general",
                "system_audio": "off", "system_audio_error": "",
            })
        );
        // Nothing typed is always named on save, whatever auto_title says.
        let session = client.status().unwrap()["session"].clone();
        assert_eq!(
            (session["title"].as_str(), session["title_auto"].as_bool()),
            (Some("Meeting"), Some(true))
        );
        client.stop().unwrap();
        // A typed title is kept as is, even "Meeting", when the caller says so.
        client
            .start("Meeting", "Zoom", "general", "", Some(false))
            .unwrap();
        assert_eq!(client.status().unwrap()["session"]["title_auto"], false);
        client.stop().unwrap();
        // With no flag the title decides: the default in any case/spacing is automatic.
        client.start("  mEETing ", "", "", "", None).unwrap();
        assert_eq!(client.status().unwrap()["session"]["title_auto"], true);
        client.stop().unwrap();
        client.start("Planning", "", "", "", None).unwrap();
        assert_eq!(client.status().unwrap()["session"]["title_auto"], false);
        let saved = client.stop().unwrap();
        assert_eq!(saved["title"], "Planning");
        assert_eq!(saved["enhanced"], false);
        assert_eq!(saved["template"], "general");
    }

    #[test]
    fn is_default_title_matches_python() {
        assert!(is_default_title(""));
        assert!(is_default_title("   "));
        assert!(is_default_title("meeting"));
        assert!(is_default_title("\u{1c}MEETING\u{2003}"));
        assert!(!is_default_title("Meeting notes"));
        assert!(!is_default_title("Me eting"));
    }

    #[test]
    fn chat_limits_and_log_path() {
        let client = FakeMeetingClient::new();
        assert_eq!(
            client.chat("\u{1c}\u{1f}").unwrap_err().0,
            "question is empty"
        );
        let long = "é".repeat(MAX_CHAT_QUESTION_CHARS + 1);
        assert_eq!(client.chat(&long).unwrap_err().0, "question too long");
        client.start("T", "", "", "notes", None).unwrap();
        let saved = client.stop().unwrap();
        assert_eq!(saved["enhanced"], true);
        let reply = client.chat(" what? ").unwrap();
        assert_eq!(reply["answer"], "(fake answer to: what?)");
        assert_eq!(reply["log_path"], saved["log_path"]);
    }

    #[test]
    fn server_rules_are_enforced() {
        let client = FakeMeetingClient::new();
        assert_eq!(
            client.save_notes("x").unwrap_err().0,
            "no meeting session running"
        );
        assert_eq!(client.stop().unwrap_err().0, "no meeting session running");
        assert_eq!(
            client.chat("hi").unwrap_err().0,
            "no saved meeting transcript yet"
        );
        client
            .start("Typed", "Other", "general", "", Some(false))
            .unwrap();
        assert_eq!(client.status().unwrap()["session"]["title_auto"], false);
        assert!(client.start("Again", "Other", "general", "", None).is_err());
        client.stop().unwrap();
        assert_eq!(client.chat("  ").unwrap_err().0, "question is empty");
        assert_eq!(
            client.chat("what?").unwrap()["answer"],
            "(fake answer to: what?)"
        );
        client.set_chat_answer(Some("42"));
        assert_eq!(client.chat("what?").unwrap()["answer"], "42");
    }

    #[test]
    fn a_down_server_reports_the_friendly_message_for_every_call() {
        let client = FakeMeetingClient::new();
        client.set_down(Some(SERVER_DOWN));
        assert_eq!(client.status().unwrap_err().0, SERVER_DOWN);
        assert_eq!(client.templates().unwrap_err().0, SERVER_DOWN);
        assert_eq!(client.dismiss().unwrap_err().0, SERVER_DOWN);
        client.set_down(None);
        assert_eq!(client.templates().unwrap()["default"], "general");
        assert_eq!(client.calls().len(), 4);
    }

    #[test]
    fn notes_writer_drops_superseded_saves() {
        let client = FakeMeetingClient::new();
        client
            .start("T", "Other", "general", "", Some(false))
            .unwrap();
        let writer = MeetingNotesWriter::new();
        let older = writer.ticket();
        let newer = writer.ticket();
        assert_eq!((older, newer), (1, 2));
        // The newest (closing flush) reaches the server first; the older save
        // that was still in flight is dropped.
        writer.save(&client, newer, "final").unwrap();
        assert_eq!(writer.save(&client, older, "stale").unwrap(), json!({}));
        assert_eq!(client.status().unwrap()["session"]["user_notes"], "final");
        // Same ticket again is not older than what was sent.
        writer.save(&client, newer, "final 2").unwrap();
        assert_eq!(client.status().unwrap()["session"]["user_notes"], "final 2");
    }

    #[test]
    fn notes_writer_propagates_client_errors() {
        let client = FakeMeetingClient::new();
        let writer = MeetingNotesWriter::new();
        let ticket = writer.ticket();
        assert!(writer.save(&client, ticket, "x").is_err());
    }

    #[test]
    fn vault_notes_path_only_accepts_markdown_inside_the_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(vault.join("notes")).unwrap();
        let note = vault.join("notes/a.md");
        std::fs::write(&note, "# A").unwrap();
        let text = vault.join("notes/a.txt");
        std::fs::write(&text, "x").unwrap();
        let outside = tmp.path().join("b.md");
        std::fs::write(&outside, "x").unwrap();
        let vault_str = vault.to_str().unwrap();
        let resolved = std::fs::canonicalize(&note).unwrap();
        assert_eq!(
            vault_notes_path(note.to_str().unwrap(), vault_str),
            Some(resolved.to_string_lossy().into_owned())
        );
        assert_eq!(vault_notes_path(text.to_str().unwrap(), vault_str), None);
        assert_eq!(vault_notes_path(outside.to_str().unwrap(), vault_str), None);
        assert_eq!(
            vault_notes_path(vault.join("missing.md").to_str().unwrap(), vault_str),
            None
        );
        assert_eq!(vault_notes_path("  ", vault_str), None);
        assert_eq!(vault_notes_path(note.to_str().unwrap(), ""), None);
        // `..` escapes are resolved before the containment check.
        let sneaky = format!("{}/notes/../../b.md", vault_str);
        assert_eq!(vault_notes_path(&sneaky, vault_str), None);
    }
}
