//! Prompt queue backend: the contract the Hub's Prompts page and the server
//! share (port of the parts of `localflow/dispatch.py` the UI uses), plus an
//! in-memory fake.
//!
//! Methods mirror what `hub_mac.py` / `hub_model.py` / the server call on
//! `PromptQueue`: `list`, `get`, `set_status` (compare-and-set) and `add`.
//! The guarded transitions ([`transition_prompt`], [`approve_prompt`],
//! [`reject_prompt`]) are pure functions over the trait, exactly like Python.
//! The orca dispatch itself (`dispatch()` / `orca_available()`) is CLI-only
//! and not part of this crate.

use std::fmt;
use std::sync::Mutex;

use chrono::{Local, NaiveDateTime, Timelike};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// One queue entry: the JSON object stored in `queue.json`.
pub type PromptEntry = Map<String, Value>;

/// `STATUSES` from `dispatch.py`.
pub const STATUSES: [&str; 5] = ["pending", "approved", "rejected", "dispatched", "failed"];
/// Approve doubles as retry: a `failed` entry is a dispatch that errored.
pub const APPROVE_FROM: [&str; 2] = ["pending", "failed"];
/// A failed entry can equally be rejected.
pub const REJECT_FROM: [&str; 2] = ["pending", "failed"];

/// A work prompt parsed from a meeting (`WorkPrompt`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPrompt {
    pub title: String,
    pub body: String,
}

impl WorkPrompt {
    pub fn new(title: &str, body: &str) -> Self {
        WorkPrompt {
            title: title.to_string(),
            body: body.to_string(),
        }
    }
}

/// A compare-and-set lost: the entry was not in an expected status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusConflict {
    pub entry_id: String,
    pub current: String,
    pub expected: Vec<String>,
}

impl fmt::Display for StatusConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot move {} from '{}'; expected one of {}",
            self.entry_id,
            self.current,
            self.expected.join(", ")
        )
    }
}

/// Everything a queue call can fail with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// [`StatusConflict`].
    Conflict(StatusConflict),
    /// A guarded transition refused by the HTTP host.
    Remote(PromptTransitionError),
    /// Python `ValueError` for an unknown status.
    InvalidStatus(String),
    /// Storage failure (unwritable queue file, ...). The text is for logs only.
    Io(String),
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::Conflict(conflict) => conflict.fmt(f),
            QueueError::Remote(refused) => refused.fmt(f),
            QueueError::InvalidStatus(message) | QueueError::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for QueueError {}

/// A guarded approve/reject was refused. `status_code` is 404 or 409.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTransitionError {
    pub status_code: u16,
    pub detail: String,
}

impl fmt::Display for PromptTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for PromptTransitionError {}

/// Why [`transition_prompt`] failed: a refusal the UI words nicely, or any
/// other queue failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionFailure {
    Refused(PromptTransitionError),
    Queue(QueueError),
}

impl fmt::Display for TransitionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransitionFailure::Refused(error) => error.fmt(f),
            TransitionFailure::Queue(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for TransitionFailure {}

/// The prompt queue the Hub and the server share.
pub trait PromptQueue: Send + Sync {
    /// Queue prompts idempotently on `(path, title)`; returns the (new or
    /// existing) entries in order.
    fn add(
        &self,
        prompts: &[WorkPrompt],
        paths: &[String],
        meeting_title: &str,
    ) -> Result<Vec<PromptEntry>, QueueError>;

    /// Entries in storage order, optionally only those with `status`.
    fn list(&self, status: Option<&str>) -> Result<Vec<PromptEntry>, QueueError>;

    fn get(&self, entry_id: &str) -> Result<Option<PromptEntry>, QueueError>;

    /// Move an entry to `status`; `Ok(None)` if the id is unknown. `expect`
    /// makes it a compare-and-set ([`QueueError::Conflict`] when the current
    /// status is not in it). Moving to `approved` clears the previous attempt
    /// (`error`, `worktree`, `handle`) before `fields` are applied.
    fn set_status(
        &self,
        entry_id: &str,
        status: &str,
        expect: Option<&[&str]>,
        fields: &Map<String, Value>,
    ) -> Result<Option<PromptEntry>, QueueError>;
}

/// Explicitly unavailable until the server transport is installed.
pub struct UnavailablePromptQueue;

impl PromptQueue for UnavailablePromptQueue {
    fn add(&self, _: &[WorkPrompt], _: &[String], _: &str) -> Result<Vec<PromptEntry>, QueueError> {
        Err(QueueError::Io("prompt queue transport unavailable".into()))
    }

    fn list(&self, _: Option<&str>) -> Result<Vec<PromptEntry>, QueueError> {
        Err(QueueError::Io("prompt queue transport unavailable".into()))
    }

    fn get(&self, _: &str) -> Result<Option<PromptEntry>, QueueError> {
        Err(QueueError::Io("prompt queue transport unavailable".into()))
    }

    fn set_status(
        &self,
        _: &str,
        _: &str,
        _: Option<&[&str]>,
        _: &Map<String, Value>,
    ) -> Result<Option<PromptEntry>, QueueError> {
        Err(QueueError::Io("prompt queue transport unavailable".into()))
    }
}

#[cfg(test)]
mod unavailable_tests {
    use super::*;

    #[test]
    fn unavailable_queue_never_claims_an_empty_queue_or_a_successful_write() {
        let queue = UnavailablePromptQueue;
        assert!(queue.list(None).is_err());
        assert!(queue.list(Some("pending")).is_err());
        assert!(queue.get("missing").is_err());
        assert!(queue.add(&[], &[], "").is_err());
        assert!(queue
            .add(
                &[WorkPrompt::new("title", "body")],
                &["note.md".into()],
                "meeting"
            )
            .is_err());
        assert!(queue
            .set_status("id", "approved", None, &Map::new())
            .is_err());
    }
}

/// `_entry_id`: stable dedupe key, first 8 hex chars of
/// `sha256("{path}\n{title}")`.
pub fn entry_id(path: &str, title: &str) -> String {
    let digest = Sha256::digest(format!("{path}\n{title}").as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// Guarded status change shared by the server API and the native Hub: 404 if
/// the entry is gone, 409 if the move is invalid. Compare-and-set inside the
/// queue, never get-then-set here.
pub fn transition_prompt(
    queue: &dyn PromptQueue,
    entry_id: &str,
    verb: &str,
    status: &str,
    allowed: &[&str],
) -> Result<PromptEntry, TransitionFailure> {
    match queue.set_status(entry_id, status, Some(allowed), &Map::new()) {
        Ok(Some(updated)) => Ok(updated),
        Ok(None) => Err(TransitionFailure::Refused(PromptTransitionError {
            status_code: 404,
            detail: format!("unknown prompt: {entry_id}"),
        })),
        Err(QueueError::Conflict(conflict)) => {
            Err(TransitionFailure::Refused(PromptTransitionError {
                status_code: 409,
                detail: format!(
                    "cannot {verb} a {} prompt (allowed from: {})",
                    conflict.current,
                    allowed.join(", ")
                ),
            }))
        }
        Err(QueueError::Remote(refused)) => Err(TransitionFailure::Refused(refused)),
        Err(other) => Err(TransitionFailure::Queue(other)),
    }
}

pub fn approve_prompt(
    queue: &dyn PromptQueue,
    entry_id: &str,
) -> Result<PromptEntry, TransitionFailure> {
    transition_prompt(queue, entry_id, "approve", "approved", &APPROVE_FROM)
}

pub fn reject_prompt(
    queue: &dyn PromptQueue,
    entry_id: &str,
) -> Result<PromptEntry, TransitionFailure> {
    transition_prompt(queue, entry_id, "reject", "rejected", &REJECT_FROM)
}

/// Python `datetime.now().isoformat()` (microseconds only when non-zero).
fn isoformat(now: NaiveDateTime) -> String {
    let micros = now.nanosecond() / 1000;
    if micros == 0 {
        now.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        format!("{}.{micros:06}", now.format("%Y-%m-%dT%H:%M:%S"))
    }
}

/// In-memory [`PromptQueue`] with the same semantics as the file-backed
/// Python queue (idempotent add, compare-and-set, approve clears the last
/// attempt). Failure injection models an unreadable/unwritable queue file.
#[derive(Default)]
pub struct FakePromptQueue {
    entries: Mutex<Vec<PromptEntry>>,
    fail_reads: Mutex<Option<String>>,
    fail_writes: Mutex<Option<String>>,
}

impl FakePromptQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed raw entries (as stored on disk).
    pub fn with_entries(entries: Vec<PromptEntry>) -> Self {
        let fake = Self::default();
        *fake.entries.lock().unwrap() = entries;
        fake
    }

    /// Make `list`/`get` fail with [`QueueError::Io`] (`None` heals).
    pub fn set_read_failure(&self, message: Option<&str>) {
        *self.fail_reads.lock().unwrap() = message.map(str::to_string);
    }

    /// Make `add`/`set_status` fail with [`QueueError::Io`] (`None` heals).
    pub fn set_write_failure(&self, message: Option<&str>) {
        *self.fail_writes.lock().unwrap() = message.map(str::to_string);
    }

    fn read_guard(&self) -> Result<(), QueueError> {
        match &*self.fail_reads.lock().unwrap() {
            Some(message) => Err(QueueError::Io(message.clone())),
            None => Ok(()),
        }
    }

    fn write_guard(&self) -> Result<(), QueueError> {
        match &*self.fail_writes.lock().unwrap() {
            Some(message) => Err(QueueError::Io(message.clone())),
            None => Ok(()),
        }
    }

    fn add_at(
        &self,
        prompts: &[WorkPrompt],
        paths: &[String],
        meeting_title: &str,
        now: &str,
    ) -> Result<Vec<PromptEntry>, QueueError> {
        self.write_guard()?;
        let mut entries = self.entries.lock().unwrap();
        let mut created = Vec::new();
        for (prompt, path) in prompts.iter().zip(paths) {
            let id = entry_id(path, &prompt.title);
            if let Some(existing) = entries
                .iter()
                .find(|e| e.get("id").and_then(Value::as_str) == Some(id.as_str()))
            {
                created.push(existing.clone());
                continue;
            }
            let mut entry = Map::new();
            entry.insert("id".into(), id.into());
            entry.insert("title".into(), prompt.title.clone().into());
            entry.insert("path".into(), path.clone().into());
            entry.insert("meeting".into(), meeting_title.into());
            entry.insert("created".into(), now.into());
            entry.insert("status".into(), "pending".into());
            for key in ["worktree", "handle", "repo", "error"] {
                entry.insert(key.into(), "".into());
            }
            entries.push(entry.clone());
            created.push(entry);
        }
        Ok(created)
    }
}

impl PromptQueue for FakePromptQueue {
    fn add(
        &self,
        prompts: &[WorkPrompt],
        paths: &[String],
        meeting_title: &str,
    ) -> Result<Vec<PromptEntry>, QueueError> {
        let now = isoformat(Local::now().naive_local());
        self.add_at(prompts, paths, meeting_title, &now)
    }

    fn list(&self, status: Option<&str>) -> Result<Vec<PromptEntry>, QueueError> {
        self.read_guard()?;
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| status.is_none_or(|s| e.get("status").and_then(Value::as_str) == Some(s)))
            .cloned()
            .collect())
    }

    fn get(&self, entry_id: &str) -> Result<Option<PromptEntry>, QueueError> {
        self.read_guard()?;
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(entry_id))
            .cloned())
    }

    fn set_status(
        &self,
        entry_id: &str,
        status: &str,
        expect: Option<&[&str]>,
        fields: &Map<String, Value>,
    ) -> Result<Option<PromptEntry>, QueueError> {
        if !STATUSES.contains(&status) {
            let mut sorted = STATUSES.to_vec();
            sorted.sort_unstable();
            return Err(QueueError::InvalidStatus(format!(
                "unknown status '{status}'; expected one of {}",
                sorted.join(", ")
            )));
        }
        self.read_guard()?;
        let mut entries = self.entries.lock().unwrap();
        let Some(entry) = entries
            .iter_mut()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(entry_id))
        else {
            return Ok(None);
        };
        let current = entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(expect) = expect {
            if !expect.contains(&current.as_str()) {
                return Err(QueueError::Conflict(StatusConflict {
                    entry_id: entry_id.to_string(),
                    current,
                    expected: expect.iter().map(|s| (*s).to_string()).collect(),
                }));
            }
        }
        self.write_guard()?;
        entry.insert("status".into(), status.into());
        if status == "approved" {
            for key in ["error", "worktree", "handle"] {
                entry.insert(key.into(), "".into());
            }
        }
        for (key, value) in fields {
            entry.insert(key.clone(), value.clone());
        }
        Ok(Some(entry.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn two(queue: &FakePromptQueue) -> (PromptEntry, PromptEntry) {
        let made = queue
            .add(
                &[
                    WorkPrompt::new("Migrate sessions", "body"),
                    WorkPrompt::new("Update docs", "body"),
                ],
                &["/tmp/a.md".to_string(), "/tmp/b.md".to_string()],
                "Weekly planning",
            )
            .unwrap();
        (made[0].clone(), made[1].clone())
    }

    fn id(entry: &PromptEntry) -> String {
        entry["id"].as_str().unwrap().to_string()
    }

    fn fields(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn entry_id_is_a_stable_eight_char_digest() {
        // sha256("a\nb")[:8], as `hashlib.sha256(b"a\nb").hexdigest()[:8]`.
        assert_eq!(entry_id("a", "b"), "7e18f737");
        assert_eq!(entry_id("a", "b").len(), 8);
        assert_ne!(entry_id("a", "b"), entry_id("a", "c"));
    }

    #[test]
    fn add_is_idempotent_on_path_and_title() {
        let queue = FakePromptQueue::new();
        let (first, _) = two(&queue);
        let (again, _) = two(&queue);
        assert_eq!(first["id"], again["id"]);
        assert_eq!(queue.list(None).unwrap().len(), 2);
        assert_eq!(first["status"], "pending");
        assert_eq!(first["meeting"], "Weekly planning");
    }

    #[test]
    fn approve_and_reject_follow_the_server_semantics() {
        let queue = FakePromptQueue::new();
        let (first, second) = two(&queue);
        assert_eq!(
            approve_prompt(&queue, &id(&first)).unwrap()["status"],
            "approved"
        );
        assert_eq!(
            reject_prompt(&queue, &id(&second)).unwrap()["status"],
            "rejected"
        );
        let Err(TransitionFailure::Refused(conflict)) = reject_prompt(&queue, &id(&first)) else {
            panic!("expected a refusal");
        };
        assert_eq!(conflict.status_code, 409);
        assert_eq!(
            conflict.detail,
            "cannot reject a approved prompt (allowed from: pending, failed)"
        );
        let Err(TransitionFailure::Refused(conflict)) = approve_prompt(&queue, &id(&second)) else {
            panic!("expected a refusal");
        };
        assert_eq!(conflict.status_code, 409);
        assert_eq!(
            queue.get(&id(&first)).unwrap().unwrap()["status"],
            "approved"
        );
    }

    #[test]
    fn failed_prompts_can_be_approved_again_or_rejected() {
        let queue = FakePromptQueue::new();
        let (entry, other) = two(&queue);
        for e in [&entry, &other] {
            queue
                .set_status(
                    &id(e),
                    "failed",
                    None,
                    &fields(json!({"error": "orca down"})),
                )
                .unwrap();
        }
        let retried = approve_prompt(&queue, &id(&entry)).unwrap();
        assert_eq!(
            (&retried["status"], &retried["error"]),
            (&json!("approved"), &json!(""))
        );
        // Rejecting keeps the failure on record.
        let rejected = reject_prompt(&queue, &id(&other)).unwrap();
        assert_eq!(rejected["status"], "rejected");
        assert_eq!(rejected["error"], "orca down");
    }

    #[test]
    fn dispatched_prompts_cannot_move() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        queue
            .set_status(&id(&entry), "dispatched", None, &Map::new())
            .unwrap();
        for action in [approve_prompt, reject_prompt] {
            let Err(TransitionFailure::Refused(conflict)) = action(&queue, &id(&entry)) else {
                panic!("expected a refusal");
            };
            assert_eq!(conflict.status_code, 409);
        }
        assert_eq!(
            queue.get(&id(&entry)).unwrap().unwrap()["status"],
            "dispatched"
        );
    }

    #[test]
    fn missing_id_is_a_404() {
        let queue = FakePromptQueue::new();
        for action in [approve_prompt, reject_prompt] {
            let Err(TransitionFailure::Refused(missing)) = action(&queue, "nope") else {
                panic!("expected a refusal");
            };
            assert_eq!(missing.status_code, 404);
            assert_eq!(missing.detail, "unknown prompt: nope");
        }
    }

    #[test]
    fn stale_click_loses_the_compare_and_set_race() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        // Another process dispatches after the Hub rendered the entry.
        queue
            .set_status(&id(&entry), "dispatched", None, &Map::new())
            .unwrap();
        let Err(TransitionFailure::Refused(lost)) =
            transition_prompt(&queue, &id(&entry), "approve", "approved", &APPROVE_FROM)
        else {
            panic!("expected a refusal");
        };
        assert_eq!(lost.status_code, 409);
        assert_eq!(
            queue.get(&id(&entry)).unwrap().unwrap()["status"],
            "dispatched"
        );
    }

    #[test]
    fn unknown_status_is_rejected() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        let error = queue
            .set_status(&id(&entry), "bogus", None, &Map::new())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown status 'bogus'; expected one of approved, dispatched, failed, pending, rejected"
        );
    }

    #[test]
    fn storage_failures_surface_as_queue_errors() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        queue.set_read_failure(Some("unreadable"));
        assert!(matches!(queue.list(None), Err(QueueError::Io(_))));
        queue.set_read_failure(None);
        queue.set_write_failure(Some("read-only"));
        assert!(matches!(
            approve_prompt(&queue, &id(&entry)),
            Err(TransitionFailure::Queue(QueueError::Io(_)))
        ));
        assert_eq!(
            queue.get(&id(&entry)).unwrap().unwrap()["status"],
            "pending"
        );
    }

    #[test]
    fn list_filters_by_status() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        approve_prompt(&queue, &id(&entry)).unwrap();
        assert_eq!(queue.list(Some("approved")).unwrap().len(), 1);
        assert_eq!(queue.list(Some("pending")).unwrap().len(), 1);
        assert!(queue.list(Some("failed")).unwrap().is_empty());
    }

    #[test]
    fn isoformat_matches_python() {
        let base = chrono::NaiveDate::from_ymd_opt(2026, 11, 15)
            .unwrap()
            .and_hms_opt(14, 30, 5)
            .unwrap();
        assert_eq!(isoformat(base), "2026-11-15T14:30:05");
        assert_eq!(
            isoformat(base + chrono::Duration::microseconds(42)),
            "2026-11-15T14:30:05.000042"
        );
    }
}
