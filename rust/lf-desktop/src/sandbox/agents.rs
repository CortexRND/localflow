//! The sandbox's agents service: an in-process fake of `agents-api v1`.
//!
//! No socket, subprocess or keyring. Replies are built as the exact JSON
//! shapes of `agents-api.v1.schema.json` (every required field, RFC 3339
//! `YYYY-MM-DDTHH:MM:SSZ` times, 12-hex session ids, `error: {code,
//! message} | null`) and read through the same parsers, so the page and the
//! launcher see exactly what they would see in real mode. Seeded with three
//! fake providers with different capability sets (one with everything, one
//! with no message and no cancel, one with a profile offline) and the
//! sessions of mockup M4; [`FakeAgents::add_state_examples`] adds the rows
//! that make every UI state appear (`agents-states`).
//!
//! Writes behave like the runtime: launches are validated field by field
//! (unknown fields are refused) and replay-safe by id, cancel/delete/
//! messages are capability- and state-checked with the v1 error codes.
//! Launched sessions advance one deterministic step at a time
//! ([`FakeAgents::step`]); in an interactive agents sandbox (not a
//! screenshot scene) every `/changes` read is a step, so a launched task
//! visibly starts and finishes. "Nothing is sent": the fake keeps what it
//! "received" in memory only, for the containment tests.
//!
//! Between the launcher and the Hub (two processes) an interactive agents
//! sandbox shares its state through one mode-0600 file in the sandbox
//! directory ([`STATE_FILE`]); fixture scenes and tests stay in memory.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::controllers::agents::{
    self, AgentsBackend, AgentsError, Changes, Event, EventsPage, LaunchRequest, Provider, Session,
    SessionPage, Status,
};

/// The interactive agents sandbox's shared state, in the sandbox directory.
pub const STATE_FILE: &str = "agents-sandbox.json";

pub struct FakeAgents {
    inner: Mutex<Inner>,
}

#[derive(Clone)]
struct Inner {
    enabled: bool,
    revision: u64,
    providers: Vec<Value>,
    /// Full `Session` objects (summaries drop `prompt`/`result_text`).
    sessions: Vec<Value>,
    /// Per-session event logs.
    events: BTreeMap<String, Vec<Value>>,
    /// `(id, revision)` of removed sessions, for `/changes`.
    deleted: Vec<(String, u64)>,
    /// Launch bodies by id (replay check).
    bodies: BTreeMap<String, Value>,
    /// Ids this fake launched (only these advance on [`FakeAgents::step`]).
    launched: Vec<String>,
    /// Every launch body and message text "received" (tests only; never
    /// persisted, never logged).
    received: Vec<Value>,
    /// Every read fails with this (tests: a stopped or refusing service).
    failure: Option<AgentsError>,
    /// Every write fails with this.
    write_failure: Option<AgentsError>,
    /// The next `/changes` answers `reset: true`.
    reset_next: bool,
    /// Each `/changes` read advances launched sessions one step.
    autoplay: bool,
    /// The clock: pinned (fixture scenes) or the wall clock, plus `skew`.
    pinned: Option<f64>,
    skew: f64,
    /// Calls per route, for the page's poll-loop tests.
    calls: Calls,
    /// Shared state file (interactive agents sandbox only).
    store: Option<PathBuf>,
}

/// How many times each route was called.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Calls {
    pub status: usize,
    pub sessions: usize,
    pub session: usize,
    pub changes: usize,
    pub events: usize,
    pub launch: usize,
    pub cancel: usize,
    pub remove: usize,
    pub message: usize,
}

/// The fixture scenes' "now" (2026-11-20 12:22 UTC): pinned so their clock
/// labels ("started 12:04") are the same on every run.
pub const FIXTURE_NOW: f64 = 1_795_177_320.0;

/// Seeded session ids (mockup M4 and the state examples).
pub const NEEDS_INPUT: &str = "a1f3a2c80001";
pub const CLAUDE_RUNNING: &str = "b41b7e000002";
pub const CODEX_RUNNING: &str = "c77ad1900003";
pub const CLAUDE_DONE: &str = "d2c90de00004";
pub const DEVIN_FAILED: &str = "e5e61ab00005";
pub const CLAUDE_CANCELLED: &str = "f9b3f4200006";
pub const CODEX_STAGING: &str = "a1d08c600007";
pub const DEVIN_UNKNOWN: &str = "be4a7c300008";
pub const QUEUED_EXAMPLE: &str = "c6a52f100009";
pub const CANCELLING_EXAMPLE: &str = "d3f8e270000a";

/// The workspaces the fake service lists (`repos`).
const REPOS: [&str; 5] = [
    "acme/web",
    "acme/billing",
    "acme/sync-worker",
    "acme/api",
    "acme/desktop",
];

/// RFC 3339 `YYYY-MM-DDTHH:MM:SSZ`, exactly as the schema's `Timestamp`.
pub fn rfc3339(secs: f64) -> String {
    chrono::DateTime::from_timestamp(secs.floor() as i64, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn provider(id: &str, profile: &str, label: &str, caps: Value, code: &str) -> Value {
    json!({
        "id": id, "profile": profile, "label": label, "builtin": true,
        "capabilities": caps,
        "secrets": [{"name": format!("agent_{id}_api_key"), "set": code != "missing_secret",
                     "required": true}],
        "readiness": {"ready": code == "ok", "code": code, "detail": "", "checked": "shallow"},
    })
}

fn seed_providers() -> Vec<Value> {
    let cloud = |cancel: bool, message: bool| {
        json!({"kind": "cloud", "resume": true, "live_message": message, "cancel": cancel,
               "streaming": message, "needs_cwd": false, "reattach": false})
    };
    vec![
        provider(
            "claude",
            "",
            "Claude",
            json!({"kind": "local_process", "resume": true, "live_message": true,
                   "cancel": true, "streaming": true, "needs_cwd": true, "reattach": true}),
            "ok",
        ),
        provider("devin", "", "Devin", cloud(false, false), "ok"),
        provider("codex", "", "Codex", cloud(true, false), "ok"),
        provider(
            "codex",
            "staging",
            "Codex",
            cloud(true, false),
            "unreachable",
        ),
    ]
}

/// One seeded session.
struct Seed {
    id: &'static str,
    provider: &'static str,
    profile: &'static str,
    state: &'static str,
    title: &'static str,
    task: &'static str,
    repo: &'static str,
    /// Minutes since the last update / since it started.
    updated: f64,
    started: f64,
    finished: bool,
    error: Option<(&'static str, &'static str)>,
    result: &'static str,
}

/// Mockup M4, top to bottom.
const SEED: &[Seed] = &[
    Seed {
        id: NEEDS_INPUT,
        provider: "devin",
        profile: "",
        state: "waiting_input",
        title: "Migrate billing webhooks to v2",
        task: "Migrate the billing webhook handlers from the v1 payload to v2. Keep the \
               signature check, update the fixtures, and open a draft pull request.",
        repo: "acme/billing",
        updated: 2.0,
        started: 18.0,
        finished: false,
        error: None,
        result: "",
    },
    Seed {
        id: CLAUDE_RUNNING,
        provider: "claude",
        profile: "",
        state: "running",
        title: "Fix the flaky retry test in the sync worker",
        task: "Fix the flaky retry test in the sync worker. It fails about one run in ten on CI.",
        repo: "acme/sync-worker",
        updated: 6.0,
        started: 9.0,
        finished: false,
        error: None,
        result: "",
    },
    Seed {
        id: CODEX_RUNNING,
        provider: "codex",
        profile: "",
        state: "running",
        title: "Draft release notes for 2.4",
        task: "Draft release notes for 2.4 from the merged pull requests since 2.3.",
        repo: "acme/web",
        updated: 14.0,
        started: 16.0,
        finished: false,
        error: None,
        result: "",
    },
    Seed {
        id: CLAUDE_DONE,
        provider: "claude",
        profile: "",
        state: "completed",
        title: "Add dark-mode tokens to email templates",
        task: "Add dark-mode tokens to the email templates and update the snapshots.",
        repo: "acme/web",
        updated: 60.0,
        started: 85.0,
        finished: true,
        error: None,
        result: "Added dark-mode colour tokens to the six email templates and regenerated \
                 their snapshots.\n\nThe footer link colour now meets 4.5:1 in both modes. \
                 Two snapshots changed beyond colour (a spacing fix in the receipt \
                 template); see the branch email-dark-tokens.",
    },
    Seed {
        id: DEVIN_FAILED,
        provider: "devin",
        profile: "",
        state: "failed",
        title: "Upgrade CI runners to Node 22",
        task: "Upgrade the CI runners to Node 22 and fix what breaks.",
        repo: "acme/web",
        updated: 180.0,
        started: 200.0,
        finished: true,
        error: Some((
            "exit_nonzero",
            "The runner image has no Node 22 build, so the upgrade stopped.",
        )),
        result: "",
    },
    Seed {
        id: CLAUDE_CANCELLED,
        provider: "claude",
        profile: "",
        state: "cancelled",
        title: "Profile cold start on Linux",
        task: "Profile cold start on Linux and list the three slowest steps.",
        repo: "acme/desktop",
        updated: 2.0 * 1440.0,
        started: 2.0 * 1440.0 + 12.0,
        finished: true,
        error: None,
        result: "",
    },
    Seed {
        id: CODEX_STAGING,
        provider: "codex",
        profile: "staging",
        state: "running",
        title: "Audit unused feature flags",
        task: "Audit unused feature flags and propose which to remove.",
        repo: "acme/web",
        updated: 2.0 * 1440.0 + 30.0,
        started: 2.0 * 1440.0 + 50.0,
        finished: false,
        error: None,
        result: "",
    },
    Seed {
        id: DEVIN_UNKNOWN,
        provider: "devin",
        profile: "",
        state: "blocked",
        title: "Rename telemetry fields",
        task: "Rename the telemetry fields to the new schema and keep the old names as aliases.",
        repo: "acme/api",
        updated: 3.0 * 1440.0,
        started: 3.0 * 1440.0 + 40.0,
        finished: false,
        error: None,
        result: "",
    },
];

/// The two states M4 doesn't show, for `agents-states`.
const STATE_EXAMPLES: &[Seed] = &[
    Seed {
        id: QUEUED_EXAMPLE,
        provider: "claude",
        profile: "",
        state: "queued",
        title: "Write tests for the rate limiter",
        task: "Write tests for the rate limiter, including the burst case.",
        repo: "acme/api",
        updated: 0.5,
        started: 0.0,
        finished: false,
        error: None,
        result: "",
    },
    Seed {
        id: CANCELLING_EXAMPLE,
        provider: "codex",
        profile: "",
        state: "cancelling",
        title: "Try a lighter bundle split",
        task: "Try a lighter bundle split for the settings page.",
        repo: "acme/web",
        updated: 4.0,
        started: 20.0,
        finished: false,
        error: None,
        result: "",
    },
];

/// Is this provider a local process (its sessions get a working folder
/// and no web address)?
fn local(providers: &[Value], id: &str) -> bool {
    providers
        .iter()
        .find(|p| p["id"] == id)
        .is_some_and(|p| p["capabilities"]["kind"] == "local_process")
}

/// A complete v1 `Session` object.
#[allow(clippy::too_many_arguments)]
fn session_value(
    id: &str,
    provider: &str,
    profile: &str,
    state: &str,
    title: &str,
    task: &str,
    repo: &str,
    is_local: bool,
    created: f64,
    started: Option<f64>,
    updated: f64,
    finished: Option<f64>,
    revision: u64,
) -> Value {
    json!({
        "id": id, "provider": provider, "profile": profile, "state": state,
        "origin": "api", "rule_id": "", "title": title, "prompt": task,
        "repo": repo,
        "cwd": if is_local && !repo.is_empty() { format!("/srv/{repo}") } else { String::new() },
        "parent_id": "", "root_id": id,
        "external_id": if is_local { String::new() } else { format!("ext-{id}") },
        "external_url": if is_local {
            String::new()
        } else {
            format!("https://agents.example.com/sessions/{id}")
        },
        "sandbox": true,
        "created_at": rfc3339(created),
        "started_at": started.map(rfc3339),
        "updated_at": rfc3339(updated),
        "finished_at": finished.map(rfc3339),
        "timeout_seconds": 3600, "last_seq": 0, "revision": revision,
        "result_text": "", "result_truncated": false,
        "error": null, "usage": null, "cancel_requested": false,
    })
}

fn seed_value(seed: &Seed, providers: &[Value], now: f64, revision: u64) -> Value {
    let at = |minutes: f64| now - minutes * 60.0;
    let never_started = seed.state == "queued";
    let mut session = session_value(
        seed.id,
        seed.provider,
        seed.profile,
        seed.state,
        seed.title,
        seed.task,
        seed.repo,
        local(providers, seed.provider),
        at(seed.started + 0.2),
        (!never_started).then(|| at(seed.started)),
        at(seed.updated),
        seed.finished.then(|| at(seed.updated)),
        revision,
    );
    if let Some((code, message)) = seed.error {
        session["error"] = json!({"code": code, "message": message});
    }
    if !seed.result.is_empty() {
        session["result_text"] = json!(seed.result);
        session["usage"] = json!({"cost_usd": 0.42, "duration_ms": 1_500_000, "turns": 7});
    }
    if seed.state == "cancelling" {
        session["cancel_requested"] = json!(true);
    }
    session
}

/// A v1 `Event` object.
fn event_value(
    seq: u64,
    ts: f64,
    kind: &str,
    stream: &str,
    text: &str,
    state: Option<&str>,
) -> Value {
    json!({"seq": seq, "ts": rfc3339(ts), "type": kind, "stream": stream, "text": text,
           "state": state, "data": {}})
}

/// The seeded activity: the running Claude session's log (one line carries
/// a credential-like value, masked on display) and the question the Devin
/// session paused on.
fn seed_events(now: f64) -> BTreeMap<String, Vec<Value>> {
    let at = |minutes: f64| now - minutes * 60.0;
    let mut events = BTreeMap::new();
    events.insert(
        CLAUDE_RUNNING.to_string(),
        vec![
            event_value(1, at(9.0), "state", "system", "", Some("running")),
            event_value(2, at(8.5), "output", "agent", "Reading tests/sync/test_retry.py and the worker's backoff code.", None),
            event_value(3, at(7.5), "output", "agent", "The test sleeps 0.1 s and races the retry timer. Switching it to the fake clock.", None),
            event_value(4, at(7.0), "output", "stderr", "warning: CI_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123 is set in the environment", None),
            event_value(5, at(6.0), "output", "agent", "Ran the test 200 times with the fake clock: no failures. See https://ci.example.com/runs/4821", None),
        ],
    );
    events.insert(
        NEEDS_INPUT.to_string(),
        vec![
            event_value(1, at(18.0), "state", "system", "", Some("running")),
            event_value(
                2,
                at(3.0),
                "output",
                "agent",
                "Should the v1 handler stay for one release, or be removed now?",
                None,
            ),
            event_value(3, at(2.0), "state", "system", "", Some("waiting_input")),
        ],
    );
    events.insert(
        CLAUDE_DONE.to_string(),
        vec![
            event_value(1, at(85.0), "state", "system", "", Some("running")),
            event_value(2, at(61.0), "usage", "system", "7 turns", None),
            event_value(3, at(60.0), "state", "system", "", Some("completed")),
        ],
    );
    events
}

fn summary(session: &Value) -> Value {
    let mut summary = session.clone();
    if let Some(map) = summary.as_object_mut() {
        let preview: String = map
            .remove("result_text")
            .and_then(|r| r.as_str().map(|r| r.chars().take(200).collect()))
            .unwrap_or_default();
        map.remove("prompt");
        // Keep the schema's key order: `result_preview` where `result_text` was.
        let mut ordered = serde_json::Map::new();
        for (key, value) in std::mem::take(map) {
            if key == "result_truncated" {
                ordered.insert("result_preview".into(), json!(preview));
            }
            ordered.insert(key, value);
        }
        *map = ordered;
    }
    summary
}

fn terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "cancelled" | "timed_out")
}

fn rejected(status: u16, code: &str, detail: &str) -> AgentsError {
    AgentsError::rejected(status, code, detail)
}

impl Inner {
    fn seeded(enabled: bool, now: f64) -> Inner {
        let providers = seed_providers();
        let sessions: Vec<Value> = SEED
            .iter()
            .enumerate()
            .map(|(i, seed)| seed_value(seed, &providers, now, i as u64 + 1))
            .collect();
        let mut inner = Inner {
            enabled,
            revision: sessions.len() as u64,
            providers,
            sessions,
            events: seed_events(now),
            deleted: Vec::new(),
            bodies: BTreeMap::new(),
            launched: Vec::new(),
            received: Vec::new(),
            failure: None,
            write_failure: None,
            reset_next: false,
            autoplay: false,
            pinned: None,
            skew: 0.0,
            calls: Calls::default(),
            store: None,
        };
        for (id, log) in &inner.events {
            let last = log.len() as u64;
            if let Some(s) = inner.sessions.iter_mut().find(|s| s["id"] == id.as_str()) {
                s["last_seq"] = json!(last);
            }
        }
        inner
    }

    fn now(&self) -> f64 {
        self.pinned.unwrap_or_else(agents::now) + self.skew
    }

    /// `counts` as the service reports it (by runtime state).
    fn counts(&self) -> Value {
        let (mut active, mut queued, mut terminal_n) = (0, 0, 0);
        for session in &self.sessions {
            match session["state"].as_str().unwrap_or("") {
                "queued" => queued += 1,
                s if terminal(s) => terminal_n += 1,
                _ => active += 1,
            }
        }
        json!({"active": active, "queued": queued, "terminal": terminal_n})
    }

    fn provider(&self, id: &str, profile: &str) -> Option<&Value> {
        self.providers
            .iter()
            .find(|p| p["id"] == id && p["profile"] == profile)
    }

    fn caps_of(&self, session: &Value) -> Value {
        let id = session["provider"].as_str().unwrap_or("");
        let profile = session["profile"].as_str().unwrap_or("");
        self.provider(id, profile)
            .map(|p| p["capabilities"].clone())
            .unwrap_or(Value::Null)
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.sessions.iter().position(|s| s["id"] == id)
    }

    /// Bump the revision and stamp a session with it.
    fn touch(&mut self, index: usize) -> u64 {
        self.revision += 1;
        let now = self.now();
        let session = &mut self.sessions[index];
        session["revision"] = json!(self.revision);
        session["updated_at"] = json!(rfc3339(now));
        self.revision
    }

    fn add_event(
        &mut self,
        id: &str,
        kind: &str,
        stream: &str,
        text: &str,
        state: Option<&str>,
    ) -> Value {
        let now = self.now();
        let log = self.events.entry(id.to_string()).or_default();
        let seq = log.len() as u64 + 1;
        let event = event_value(seq, now, kind, stream, text, state);
        log.push(event.clone());
        if let Some(i) = self.position(id) {
            self.sessions[i]["last_seq"] = json!(seq);
        }
        event
    }

    fn set_state(&mut self, index: usize, state: &str) {
        let id = self.sessions[index]["id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let now = self.now();
        let session = &mut self.sessions[index];
        session["state"] = json!(state);
        if state == "running" && session["started_at"].is_null() {
            session["started_at"] = json!(rfc3339(now));
        }
        if terminal(state) {
            session["finished_at"] = json!(rfc3339(now));
        }
        self.add_event(&id, "state", "system", "", Some(state));
        self.touch(index);
    }

    /// Advance every session this fake launched one step: queued →
    /// running → completed (with a result); cancelling → cancelled.
    fn step(&mut self) -> bool {
        let mut moved = false;
        for id in self.launched.clone() {
            let Some(i) = self.position(&id) else {
                continue;
            };
            match self.sessions[i]["state"].as_str().unwrap_or("") {
                "queued" | "starting" => {
                    self.set_state(i, "running");
                    self.add_event(
                        &id,
                        "output",
                        "agent",
                        "Working on it in the sandbox. Nothing was sent to a provider.",
                        None,
                    );
                }
                "running" => {
                    self.sessions[i]["result_text"] =
                        json!("Sandbox run finished. Nothing was sent to a provider.");
                    self.sessions[i]["usage"] =
                        json!({"cost_usd": null, "duration_ms": 4000, "turns": 1});
                    self.set_state(i, "completed");
                }
                _ => continue,
            }
            moved = true;
        }
        // A stop under way finishes on the next step, launched or seeded.
        for i in 0..self.sessions.len() {
            if self.sessions[i]["state"] == "cancelling"
                && self.sessions[i]["id"] != CANCELLING_EXAMPLE
            {
                self.set_state(i, "cancelled");
                moved = true;
            }
        }
        moved
    }

    fn persisted(&self) -> Value {
        json!({
            "revision": self.revision, "sessions": self.sessions, "events": self.events,
            "deleted": self.deleted, "bodies": self.bodies, "launched": self.launched,
        })
    }

    fn restore(&mut self, saved: &Value) {
        let Some(revision) = saved["revision"].as_u64() else {
            return;
        };
        self.revision = revision;
        if let Some(sessions) = saved["sessions"].as_array() {
            self.sessions = sessions.clone();
        }
        if let Ok(events) = serde_json::from_value(saved["events"].clone()) {
            self.events = events;
        }
        if let Ok(deleted) = serde_json::from_value(saved["deleted"].clone()) {
            self.deleted = deleted;
        }
        if let Ok(bodies) = serde_json::from_value(saved["bodies"].clone()) {
            self.bodies = bodies;
        }
        if let Ok(launched) = serde_json::from_value(saved["launched"].clone()) {
            self.launched = launched;
        }
    }
}

fn save(path: &std::path::Path, value: &Value) {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes));
    if written.is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

impl FakeAgents {
    /// The seeded service; `enabled` is `agents.enabled` at the service.
    pub fn seeded(enabled: bool) -> Self {
        Self::seeded_at(enabled, agents::now())
    }

    /// The `agents-*` fixture scenes: on, with its clock pinned at
    /// [`FIXTURE_NOW`].
    pub fn fixture() -> Self {
        let fake = Self::seeded_at(true, FIXTURE_NOW);
        fake.with(|inner| inner.pinned = Some(FIXTURE_NOW));
        fake
    }

    /// An interactive agents sandbox (seeded with the agents fixture, run
    /// without a scene): on, on the wall clock, launched tasks advancing on
    /// every `/changes` read, its state shared through `path` with the
    /// sandbox's other window (the launcher or the Hub).
    pub fn shared(path: PathBuf) -> Self {
        let fake = Self::seeded(true);
        fake.with(|inner| {
            inner.autoplay = true;
            if let Some(saved) = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            {
                inner.restore(&saved);
            } else {
                save(&path, &inner.persisted());
            }
            inner.store = Some(path);
        });
        fake
    }

    pub fn seeded_at(enabled: bool, now: f64) -> Self {
        FakeAgents {
            inner: Mutex::new(Inner::seeded(enabled, now)),
        }
    }

    /// Switch to the fixture scenes' service: in memory, its clock pinned,
    /// the seed restored (a scene never sees another run's launches).
    pub fn use_fixture(&self) {
        self.with(|inner| {
            let calls = inner.calls;
            *inner = Inner::seeded(true, FIXTURE_NOW);
            inner.pinned = Some(FIXTURE_NOW);
            inner.calls = calls;
        });
    }

    /// One operation on the state; a shared sandbox reads the file first
    /// and, for a write, saves it after.
    fn op<T>(&self, write: bool, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(path) = inner.store.clone() {
            if let Some(saved) = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            {
                inner.restore(&saved);
            }
            let out = f(&mut inner);
            if write {
                save(&path, &inner.persisted());
            }
            return out;
        }
        f(&mut inner)
    }

    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut inner)
    }

    /// A read: counted, and failed while a failure is set.
    fn read<T>(
        &self,
        count: impl FnOnce(&mut Calls) -> &mut usize,
        f: impl FnOnce(&mut Inner) -> Result<T, AgentsError>,
    ) -> Result<T, AgentsError> {
        self.op(false, |inner| {
            *count(&mut inner.calls) += 1;
            match &inner.failure {
                Some(error) => Err(error.clone()),
                None => f(inner),
            }
        })
    }

    /// A write: counted, failed while a write failure (or a read failure:
    /// nothing answering) is set, and saved in a shared sandbox.
    fn write<T>(
        &self,
        count: impl FnOnce(&mut Calls) -> &mut usize,
        f: impl FnOnce(&mut Inner) -> Result<T, AgentsError>,
    ) -> Result<T, AgentsError> {
        self.op(true, |inner| {
            *count(&mut inner.calls) += 1;
            match inner.write_failure.as_ref().or(inner.failure.as_ref()) {
                Some(error) => Err(error.clone()),
                None => f(inner),
            }
        })
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.with(|inner| inner.enabled = enabled);
    }

    /// No provider connected (launcher scene M5): every provider reports a
    /// missing key.
    pub fn disconnect_all(&self) {
        self.with(|inner| {
            for p in &mut inner.providers {
                p["readiness"] = json!({"ready": false, "code": "missing_secret",
                    "detail": "", "checked": "shallow"});
                p["secrets"][0]["set"] = json!(false);
            }
        });
    }

    /// One provider (every profile of it) reports `code` (tests: offline,
    /// needs sign-in).
    #[cfg(test)]
    pub(crate) fn set_readiness(&self, id: &str, code: &str) {
        self.with(|inner| {
            for p in inner.providers.iter_mut().filter(|p| p["id"] == id) {
                p["readiness"] = json!({"ready": code == "ok", "code": code, "detail": "",
                    "checked": "shallow"});
            }
        });
    }

    /// Fail every read with `error` (`None`: answer again).
    #[cfg(test)]
    pub(crate) fn fail(&self, error: Option<AgentsError>) {
        self.with(|inner| inner.failure = error);
    }

    /// Fail every write with `error` (`None`: accept again).
    #[cfg(test)]
    pub(crate) fn fail_writes(&self, error: Option<AgentsError>) {
        self.with(|inner| inner.write_failure = error);
    }

    /// The next `/changes` reply says the cursor is too old.
    #[cfg(test)]
    pub(crate) fn reset_next(&self) {
        self.with(|inner| inner.reset_next = true);
    }

    /// Move this service's clock forward.
    #[cfg(test)]
    pub(crate) fn advance(&self, secs: f64) {
        self.with(|inner| inner.skew += secs);
    }

    #[cfg(test)]
    pub(crate) fn calls(&self) -> Calls {
        self.with(|inner| inner.calls)
    }

    /// Every launch body and reply text this fake "received".
    #[cfg(test)]
    pub(crate) fn received(&self) -> Vec<Value> {
        self.with(|inner| inner.received.clone())
    }

    /// The stored session objects, as JSON text (containment tests).
    #[cfg(test)]
    pub(crate) fn stored(&self) -> String {
        self.with(|inner| inner.persisted().to_string())
    }

    /// Advance launched sessions one step; `true` when one moved.
    pub fn step(&self) -> bool {
        self.op(true, Inner::step)
    }

    /// `n` more finished sessions, older than the seed (paging tests).
    #[cfg(test)]
    pub(crate) fn add_history(&self, n: usize) {
        let now = self.now();
        self.with(|inner| {
            for i in 0..n {
                inner.revision += 1;
                let id = format!("{:012x}", 0xf00000000000u64 + i as u64);
                let at = now - 86_400.0 * 7.0 - i as f64 * 60.0;
                let session = session_value(
                    &id,
                    "claude",
                    "",
                    "completed",
                    &format!("Older task {i}"),
                    "x",
                    "",
                    true,
                    at,
                    Some(at),
                    at,
                    Some(at),
                    inner.revision,
                );
                // Oldest at the front: the list reads newest first.
                inner.sessions.insert(0, session);
            }
        });
    }

    /// Add the Starting and Stopping examples (once).
    pub fn add_state_examples(&self) {
        let now = self.now();
        self.op(true, |inner| {
            for seed in STATE_EXAMPLES {
                if inner.sessions.iter().any(|s| s["id"] == seed.id) {
                    continue;
                }
                inner.revision += 1;
                let session = seed_value(seed, &inner.providers, now, inner.revision);
                inner.sessions.push(session);
            }
        });
    }

    /// Forget every session, and every provider unless `keep_providers`
    /// (the page's empty states).
    #[cfg(test)]
    pub(crate) fn clear(&self, keep_providers: bool) {
        self.with(|inner| {
            inner.sessions.clear();
            if !keep_providers {
                inner.providers.clear();
            }
            inner.revision += 1;
        });
    }

    /// Report `state` (any string, recognised or not) for one session.
    pub fn set_state(&self, id: &str, state: &str) -> bool {
        self.op(true, |inner| match inner.position(id) {
            Some(i) => {
                inner.sessions[i]["state"] = json!(state);
                inner.touch(i);
                true
            }
            None => false,
        })
    }

    /// Append an agent output line to a session's log (tests: live output,
    /// credential-like output).
    #[cfg(test)]
    pub(crate) fn emit(&self, id: &str, text: &str) {
        self.with(|inner| {
            inner.add_event(id, "output", "agent", text, None);
            if let Some(i) = inner.position(id) {
                inner.touch(i);
            }
        });
    }

    /// Give a session a result (tests: truncation, masking).
    #[cfg(test)]
    pub(crate) fn set_result(&self, id: &str, text: &str, truncated: bool) {
        self.with(|inner| {
            if let Some(i) = inner.position(id) {
                inner.sessions[i]["result_text"] = json!(text);
                inner.sessions[i]["result_truncated"] = json!(truncated);
                inner.touch(i);
            }
        });
    }

    fn launch_inner(inner: &mut Inner, request: &LaunchRequest) -> Result<Session, AgentsError> {
        let body = request.body();
        inner.received.push(body.clone());
        let map = body.as_object().cloned().unwrap_or_default();
        if let Some(extra) = map
            .keys()
            .find(|k| !agents::LAUNCH_FIELDS.contains(&k.as_str()))
        {
            return Err(rejected(
                400,
                "validation",
                &format!("unknown field: {extra}"),
            ));
        }
        if !inner.enabled {
            return Err(rejected(409, "agents_disabled", "agents are turned off"));
        }
        let prompt = map.get("prompt").and_then(Value::as_str).unwrap_or("");
        if prompt.trim().is_empty() {
            return Err(rejected(400, "validation", "prompt is required"));
        }
        if prompt.chars().count() > 20_000 {
            return Err(rejected(413, "prompt_too_long", "the task is too long"));
        }
        let id = map.get("id").and_then(Value::as_str).unwrap_or("");
        if !agents::is_session_id(id) {
            return Err(rejected(
                400,
                "validation",
                "id must be 12 lowercase hex characters",
            ));
        }
        if map.contains_key("repo") && map.contains_key("cwd") {
            return Err(rejected(400, "validation", "send repo or cwd, not both"));
        }
        if let Some(previous) = inner.bodies.get(id) {
            if *previous != body {
                return Err(rejected(
                    409,
                    "id_conflict",
                    "this id was used for another task",
                ));
            }
            let i = inner
                .position(id)
                .ok_or_else(|| rejected(409, "id_conflict", "removed"))?;
            return agents::parse_session(&inner.sessions[i]).ok_or(AgentsError::Offline);
        }
        let provider_id = map.get("provider").and_then(Value::as_str).unwrap_or("");
        let profile = map.get("profile").and_then(Value::as_str).unwrap_or("");
        let Some(provider) = inner.provider(provider_id, profile).cloned() else {
            return Err(rejected(400, "validation", "unknown provider"));
        };
        if provider["readiness"]["ready"] != true {
            return Err(rejected(
                409,
                "provider_not_ready",
                "the provider isn't connected",
            ));
        }
        let repo = map.get("repo").and_then(Value::as_str).unwrap_or("");
        if !repo.is_empty() && !REPOS.contains(&repo) {
            return Err(rejected(400, "validation", "unknown repo"));
        }
        let is_local = provider["capabilities"]["kind"] == "local_process";
        if is_local && provider["capabilities"]["needs_cwd"] == true && repo.is_empty() {
            return Err(rejected(
                400,
                "validation",
                "this provider needs a workspace",
            ));
        }
        let title: String = prompt
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let now = inner.now();
        inner.revision += 1;
        let session = session_value(
            id,
            provider_id,
            profile,
            "queued",
            &title,
            prompt,
            repo,
            is_local,
            now,
            None,
            now,
            None,
            inner.revision,
        );
        inner.sessions.push(session.clone());
        inner.bodies.insert(id.to_string(), body);
        inner.launched.push(id.to_string());
        inner.add_event(id, "state", "system", "", Some("queued"));
        let i = inner.sessions.len() - 1;
        agents::parse_session(&inner.sessions[i]).ok_or(AgentsError::Offline)
    }
}

impl AgentsBackend for FakeAgents {
    fn simulated(&self) -> bool {
        true
    }

    fn status(&self) -> Result<Status, AgentsError> {
        self.read(
            |c| &mut c.status,
            |inner| {
                Ok(agents::parse_status(&json!({
                    "api_version": 1, "enabled": inner.enabled, "hot_commands": false,
                    "sandbox": true, "allow_remote": false, "default_provider": "claude",
                    "default_repo": "acme/web",
                    "repos": REPOS.iter().map(|alias| json!({
                        "alias": alias, "path": format!("/srv/{alias}"), "exists": true,
                    })).collect::<Vec<_>>(),
                    "limits": {"max_concurrent": 4, "max_queued": 16,
                               "max_prompt_chars": 20000, "max_wait_seconds": 25},
                    "counts": inner.counts(), "revision": inner.revision,
                })))
            },
        )
    }

    fn providers(&self) -> Result<Vec<Provider>, AgentsError> {
        self.read(
            |c| &mut c.status,
            |inner| {
                Ok(agents::parse_providers(
                    &json!({ "providers": inner.providers }),
                ))
            },
        )
    }

    /// Newest first; the cursor is the position to continue from.
    fn sessions(&self, limit: u32, before: Option<&str>) -> Result<SessionPage, AgentsError> {
        self.read(
            |c| &mut c.sessions,
            |inner| {
                let start = before.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
                let end = (start + limit as usize).min(inner.sessions.len());
                let sessions: Vec<Value> = inner
                    .sessions
                    .iter()
                    .rev()
                    .skip(start)
                    .take(limit as usize)
                    .map(summary)
                    .collect();
                let next = (end < inner.sessions.len()).then(|| end.to_string());
                Ok(agents::parse_session_page(&json!({
                    "sessions": sessions, "next_cursor": next,
                    "revision": inner.revision, "counts": inner.counts(),
                })))
            },
        )
    }

    fn session(&self, id: &str) -> Result<Session, AgentsError> {
        self.read(
            |c| &mut c.session,
            |inner| {
                inner
                    .sessions
                    .iter()
                    .find(|s| s["id"] == id)
                    .and_then(agents::parse_session)
                    .ok_or_else(|| AgentsError::Refused("session not found".into()))
            },
        )
    }

    /// Answers at once (the sandbox never holds a long-poll). In an
    /// interactive sandbox each read first advances launched sessions.
    fn changes(&self, since: u64, _wait: u32) -> Result<Changes, AgentsError> {
        let autoplay = self.with(|inner| inner.autoplay && inner.failure.is_none());
        if autoplay {
            self.step();
        }
        self.read(
            |c| &mut c.changes,
            |inner| {
                let reset = std::mem::take(&mut inner.reset_next);
                let sessions: Vec<Value> = inner
                    .sessions
                    .iter()
                    .filter(|s| !reset && s["revision"].as_u64().unwrap_or(0) > since)
                    .map(summary)
                    .collect();
                let deleted: Vec<&str> = inner
                    .deleted
                    .iter()
                    .filter(|(_, rev)| !reset && *rev > since)
                    .map(|(id, _)| id.as_str())
                    .collect();
                Ok(agents::parse_changes(&json!({
                    "revision": inner.revision, "sessions": sessions, "deleted": deleted,
                    "reset": reset, "counts": inner.counts(), "waited": false,
                })))
            },
        )
    }

    fn launch(&self, request: &LaunchRequest) -> Result<Session, AgentsError> {
        self.write(
            |c| &mut c.launch,
            |inner| Self::launch_inner(inner, request),
        )
    }

    fn cancel(&self, id: &str) -> Result<Session, AgentsError> {
        self.write(
            |c| &mut c.cancel,
            |inner| {
                let i = inner
                    .position(id)
                    .ok_or_else(|| rejected(404, "not_found", "no such session"))?;
                if inner.caps_of(&inner.sessions[i])["cancel"] != true {
                    return Err(rejected(
                        409,
                        "unsupported",
                        "this provider can't stop sessions",
                    ));
                }
                let state = inner.sessions[i]["state"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if state == "cancelling" || state == "cancelled" {
                    return agents::parse_session(&inner.sessions[i]).ok_or(AgentsError::Offline);
                }
                if terminal(&state) {
                    return Err(rejected(
                        409,
                        "session_conflict",
                        "the session already finished",
                    ));
                }
                inner.sessions[i]["cancel_requested"] = json!(true);
                inner.set_state(i, "cancelling");
                agents::parse_session(&inner.sessions[i]).ok_or(AgentsError::Offline)
            },
        )
    }

    fn remove(&self, id: &str) -> Result<(), AgentsError> {
        self.write(
            |c| &mut c.remove,
            |inner| {
                let i = inner
                    .position(id)
                    .ok_or_else(|| rejected(404, "not_found", "no such session"))?;
                if !terminal(inner.sessions[i]["state"].as_str().unwrap_or("")) {
                    return Err(rejected(
                        409,
                        "session_conflict",
                        "the session is still active",
                    ));
                }
                inner.sessions.remove(i);
                inner.events.remove(id);
                inner.revision += 1;
                let revision = inner.revision;
                inner.deleted.push((id.to_string(), revision));
                Ok(())
            },
        )
    }

    fn message(&self, id: &str, text: &str) -> Result<(Session, Event), AgentsError> {
        self.write(
            |c| &mut c.message,
            |inner| {
                inner.received.push(json!({ "text": text }));
                let i = inner
                    .position(id)
                    .ok_or_else(|| rejected(404, "not_found", "no such session"))?;
                if inner.caps_of(&inner.sessions[i])["live_message"] != true {
                    return Err(rejected(
                        409,
                        "unsupported",
                        "this provider doesn't take messages",
                    ));
                }
                let state = inner.sessions[i]["state"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if state != "running" && state != "waiting_input" {
                    return Err(rejected(
                        409,
                        "session_conflict",
                        "the session isn't running",
                    ));
                }
                if text.trim().is_empty() {
                    return Err(rejected(400, "validation", "text is required"));
                }
                if text.chars().count() > 20_000 {
                    return Err(rejected(413, "prompt_too_long", "the message is too long"));
                }
                let event = inner.add_event(id, "message", "user", text, None);
                if state == "waiting_input" {
                    inner.set_state(i, "running");
                } else {
                    inner.touch(i);
                }
                let session =
                    agents::parse_session(&inner.sessions[i]).ok_or(AgentsError::Offline)?;
                let event = agents::parse_event(&event).ok_or(AgentsError::Offline)?;
                Ok((session, event))
            },
        )
    }

    fn events(
        &self,
        id: &str,
        after: u64,
        limit: u32,
        _wait: u32,
    ) -> Result<EventsPage, AgentsError> {
        self.read(
            |c| &mut c.events,
            |inner| {
                let session = inner
                    .sessions
                    .iter()
                    .find(|s| s["id"] == id)
                    .ok_or_else(|| AgentsError::Refused("session not found".into()))?;
                let state = session["state"].clone();
                let log = inner.events.get(id).cloned().unwrap_or_default();
                let newer: Vec<Value> = log
                    .iter()
                    .filter(|e| e["seq"].as_u64().unwrap_or(0) > after)
                    .cloned()
                    .collect();
                let page: Vec<Value> = newer.iter().take(limit.max(1) as usize).cloned().collect();
                let next_after = page.last().and_then(|e| e["seq"].as_u64()).unwrap_or(after);
                Ok(agents::parse_events(&json!({
                    "events": page, "next_after": next_after, "last_seq": log.len(),
                    "has_more": newer.len() > page.len(), "state": state,
                    "terminal": terminal(state.as_str().unwrap_or("")), "waited": false,
                })))
            },
        )
    }

    fn now(&self) -> f64 {
        self.with(|inner| inner.now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::agents::{Filter, Readiness, RuntimeState, Store, UiState};

    fn store(fake: &FakeAgents) -> Store {
        let mut store = Store::default();
        let generation = store.begin();
        assert!(store.finish(generation, agents::load(fake), agents::now()));
        store
    }

    fn request(id: &str, prompt: &str, provider: &str, repo: Option<&str>) -> LaunchRequest {
        LaunchRequest {
            id: id.into(),
            prompt: prompt.into(),
            provider: provider.into(),
            profile: None,
            repo: repo.map(str::to_owned),
        }
    }

    #[test]
    fn the_seed_matches_mockup_m4() {
        let fake = FakeAgents::seeded(true);
        assert!(fake.simulated());
        assert!(agents::available(&fake.status()));
        let store = store(&fake);
        let rows = store.rows(agents::now());
        let shown: Vec<(&str, UiState, &str)> = rows
            .iter()
            .map(|r| (r.title.as_str(), r.state, r.provider.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "Migrate billing webhooks to v2",
                    UiState::NeedsInput,
                    "Devin"
                ),
                (
                    "Fix the flaky retry test in the sync worker",
                    UiState::Running,
                    "Claude"
                ),
                ("Draft release notes for 2.4", UiState::Running, "Codex"),
                ("Audit unused feature flags", UiState::Disconnected, "Codex"),
                ("Rename telemetry fields", UiState::Unknown, "Devin"),
                (
                    "Add dark-mode tokens to email templates",
                    UiState::Completed,
                    "Claude"
                ),
                ("Upgrade CI runners to Node 22", UiState::Failed, "Devin"),
                ("Profile cold start on Linux", UiState::Canceled, "Claude"),
            ]
        );
        assert_eq!(agents::support_line(&rows, None), "2 running · 1 needs you");
        assert_eq!(
            agents::support_line(&rows, store.counts),
            "2 running · 1 needs you",
            "the service's counts agree"
        );
        // Summaries carry no task text or result; the detail read does.
        assert!(rows.iter().all(|r| r.session.prompt.is_none()));
        assert!(rows.iter().all(|r| r.session.result_text.is_none()));
        let done = rows.iter().find(|r| r.session.id == CLAUDE_DONE).unwrap();
        assert!(done
            .session
            .result_preview
            .as_deref()
            .unwrap()
            .starts_with("Added dark-mode"));
        let detail = fake.session(CLAUDE_DONE).unwrap();
        assert!(detail.prompt.is_some() && detail.result_text.is_some());
        assert!(detail.result_preview.is_none(), "detail has no preview");
        // Every id is a v1 id, every session is marked sandboxed.
        assert!(rows.iter().all(|r| agents::is_session_id(&r.session.id)));
        assert!(rows.iter().all(|r| r.session.sandbox));
        // Varied capability sets, one provider profile offline.
        let caps = |id: &str| store.provider(id, "").unwrap().caps;
        assert!(caps("claude").live_message && caps("claude").cancel);
        assert!(!caps("devin").live_message && !caps("devin").cancel);
        assert_eq!(
            store.provider("codex", "staging").unwrap().readiness,
            Readiness::Offline
        );
        let status = fake.status().unwrap();
        assert_eq!(status.default_provider.as_deref(), Some("claude"));
        assert_eq!(status.default_repo.as_deref(), Some("acme/web"));
        assert_eq!(status.repos.len(), REPOS.len());
    }

    /// The fake's replies are the schema's exact shapes: every required
    /// field, nothing more, `YYYY-MM-DDTHH:MM:SSZ` times.
    #[test]
    fn replies_have_the_schemas_exact_shapes() {
        const SESSION: [&str; 27] = [
            "id",
            "provider",
            "profile",
            "state",
            "origin",
            "rule_id",
            "title",
            "prompt",
            "repo",
            "cwd",
            "parent_id",
            "root_id",
            "external_id",
            "external_url",
            "sandbox",
            "created_at",
            "started_at",
            "updated_at",
            "finished_at",
            "timeout_seconds",
            "last_seq",
            "revision",
            "result_text",
            "result_truncated",
            "error",
            "usage",
            "cancel_requested",
        ];
        let fake = FakeAgents::fixture();
        fake.add_state_examples();
        let inner = fake.with(|inner| inner.clone());
        let ts = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap();
        for session in &inner.sessions {
            let keys: Vec<&str> = session
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(keys, SESSION, "{}", session["id"]);
            for field in ["created_at", "updated_at"] {
                assert!(ts.is_match(session[field].as_str().unwrap()), "{field}");
            }
            for field in ["started_at", "finished_at"] {
                assert!(session[field].is_null() || ts.is_match(session[field].as_str().unwrap()));
            }
            assert!(session["error"].is_null() || session["error"].as_object().unwrap().len() == 2);
            let summary = summary(session);
            let keys: Vec<&str> = summary
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert!(!keys.contains(&"prompt") && !keys.contains(&"result_text"));
            assert!(keys.contains(&"result_preview"));
            assert_eq!(keys.len(), 26, "SessionSummary has 26 fields");
        }
        let failed = inner
            .sessions
            .iter()
            .find(|s| s["id"] == DEVIN_FAILED)
            .unwrap();
        assert_eq!(failed["error"]["code"], "exit_nonzero");
        let parsed = agents::parse_session(failed).unwrap();
        assert_eq!(parsed.error.unwrap().code, "exit_nonzero");
        let started = fake.session(NEEDS_INPUT).unwrap().started_at.unwrap();
        assert_eq!(started, FIXTURE_NOW - 18.0 * 60.0, "exact to the second");
        for (_, log) in inner.events {
            for event in log {
                let keys: Vec<&str> = event
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    keys,
                    ["seq", "ts", "type", "stream", "text", "state", "data"]
                );
                assert!(ts.is_match(event["ts"].as_str().unwrap()));
            }
        }
    }

    #[test]
    fn state_examples_cover_every_ui_state_and_changes_report_them() {
        let fake = FakeAgents::seeded(true);
        let mut store = store(&fake);
        let since = store.revision;
        fake.add_state_examples();
        fake.add_state_examples();
        let changes = fake.changes(since, 25).unwrap();
        assert_eq!(changes.sessions.len(), 2, "added once");
        assert!(!store.apply_changes(changes));
        let now = agents::now();
        let rows = store.rows(now);
        for state in UiState::ALL {
            assert!(rows.iter().any(|r| r.state == state), "{state:?} is shown");
        }
        assert!(fake.set_state(CLAUDE_RUNNING, "waiting_input"));
        assert!(!fake.set_state("nope", "running"));
        let changes = fake.changes(store.revision, 0).unwrap();
        assert_eq!(changes.sessions.len(), 1);
        store.apply_changes(changes);
        let rows = store.rows(now);
        let needs = agents::grouped(&rows, Filter::NeedsYou, "");
        assert_eq!(needs[0].1.len(), 2);
    }

    /// The service keeps up to 200 sessions; the list reads every page of
    /// them (not just the first 50) and no more than that.
    #[test]
    fn loading_follows_the_cursor_to_every_retained_session() {
        let fake = FakeAgents::seeded(true);
        fake.add_history(192);
        let rows = store(&fake).rows(fake.now());
        assert_eq!(rows.len(), agents::MAX_SESSIONS);
        assert_eq!(fake.calls().sessions, 4, "four pages of 50");
        assert!(rows.iter().any(|r| r.session.id == NEEDS_INPUT));
        assert!(rows
            .iter()
            .any(|r| r.session.id == format!("{:012x}", 0xf00000000000u64 + 189)));
        let fake = FakeAgents::seeded(true);
        fake.add_history(300);
        let loaded = store(&fake);
        assert_eq!(loaded.rows(0.0).len(), agents::MAX_SESSIONS);
        assert_eq!(fake.calls().sessions, 4);
        assert_eq!(loaded.counts.unwrap().terminal, 303);
    }

    #[test]
    fn test_controls_fail_reset_and_pin_the_clock() {
        let fake = FakeAgents::seeded(true);
        fake.fail(Some(AgentsError::Offline));
        assert_eq!(fake.status(), Err(AgentsError::Offline));
        assert_eq!(fake.changes(0, 0), Err(AgentsError::Offline));
        fake.fail(None);
        fake.reset_next();
        let changes = fake.changes(0, 0).unwrap();
        assert!(changes.reset && changes.sessions.is_empty());
        assert!(!fake.changes(0, 0).unwrap().reset, "once");
        let pinned = FakeAgents::fixture();
        assert_eq!(pinned.now(), FIXTURE_NOW);
        pinned.advance(30.0);
        assert_eq!(pinned.now(), FIXTURE_NOW + 30.0);
        let started = pinned.session(NEEDS_INPUT).unwrap().started_at.unwrap();
        assert_eq!(started, FIXTURE_NOW - 18.0 * 60.0, "same every run");
    }

    #[test]
    fn the_gate_is_the_services_enabled_flag() {
        let fake = FakeAgents::seeded(false);
        assert!(!agents::available(&fake.status()));
        assert!(fake.sessions(50, None).is_ok(), "GETs still work when off");
        let err = fake
            .launch(&request("0123456789ab", "x", "claude", Some("acme/web")))
            .unwrap_err();
        assert_eq!(err.code(), Some("agents_disabled"));
        fake.set_enabled(true);
        assert!(agents::available(&fake.status()));
    }

    /// Launches are validated like the runtime and replay-safe by id; a
    /// launched session steps queued → running → completed with a result.
    #[test]
    fn launches_validate_replay_and_progress_deterministically() {
        let fake = FakeAgents::fixture();
        let req = request("0123456789ab", "Add a retry", "claude", Some("acme/web"));
        let first = fake.launch(&req).unwrap();
        assert_eq!(first.runtime, RuntimeState::Queued);
        assert_eq!(first.title, "Add a retry");
        assert!(first.sandbox);
        let again = fake.launch(&req).unwrap();
        assert_eq!(again.id, first.id, "replay answers the same session");
        assert_eq!(
            fake.status().unwrap().counts.unwrap().queued,
            1,
            "one session"
        );
        let other = LaunchRequest {
            prompt: "Something else".into(),
            ..req.clone()
        };
        assert_eq!(fake.launch(&other).unwrap_err().code(), Some("id_conflict"));
        for (bad, code) in [
            (
                request("XYZ", "x", "claude", Some("acme/web")),
                "validation",
            ),
            (
                request("0123456789ac", " ", "claude", Some("acme/web")),
                "validation",
            ),
            (request("0123456789ad", "x", "claude", None), "validation"),
            (
                request("0123456789ae", "x", "claude", Some("nope/x")),
                "validation",
            ),
            (request("0123456789af", "x", "nobody", None), "validation"),
            (
                request("0123456789b0", &"x".repeat(20_001), "devin", None),
                "prompt_too_long",
            ),
        ] {
            assert_eq!(
                fake.launch(&bad).unwrap_err().code(),
                Some(code),
                "{:?}",
                bad.id
            );
        }
        fake.set_readiness("devin", "unreachable");
        assert_eq!(
            fake.launch(&request("0123456789b1", "x", "devin", None))
                .unwrap_err()
                .code(),
            Some("provider_not_ready")
        );
        assert!(fake.step());
        assert_eq!(
            fake.session(&first.id).unwrap().runtime,
            RuntimeState::Running
        );
        assert!(fake.step());
        let done = fake.session(&first.id).unwrap();
        assert_eq!(done.runtime, RuntimeState::Completed);
        assert!(done.result_text.is_some());
        assert!(!fake.step(), "nothing left to move");
        let log = fake.events(&first.id, 0, 200, 0).unwrap();
        assert!(log.terminal && !log.has_more);
        assert_eq!(
            log.events.first().unwrap().state,
            Some(RuntimeState::Queued)
        );
    }

    #[test]
    fn cancel_delete_and_messages_check_capability_and_state() {
        let fake = FakeAgents::fixture();
        // Devin can't cancel or take messages.
        assert_eq!(
            fake.cancel(NEEDS_INPUT).unwrap_err().code(),
            Some("unsupported")
        );
        assert_eq!(
            fake.message(NEEDS_INPUT, "yes").unwrap_err().code(),
            Some("unsupported")
        );
        // Claude can: stop is asynchronous and idempotent.
        let stopping = fake.cancel(CLAUDE_RUNNING).unwrap();
        assert_eq!(stopping.runtime, RuntimeState::Cancelling);
        assert!(stopping.cancel_requested);
        assert_eq!(
            fake.cancel(CLAUDE_RUNNING).unwrap().runtime,
            RuntimeState::Cancelling
        );
        fake.step();
        assert_eq!(
            fake.session(CLAUDE_RUNNING).unwrap().runtime,
            RuntimeState::Cancelled
        );
        assert_eq!(
            fake.cancel(CLAUDE_DONE).unwrap_err().code(),
            Some("session_conflict")
        );
        // Delete: terminal only, reported as deleted by /changes.
        assert_eq!(
            fake.remove(CODEX_RUNNING).unwrap_err().code(),
            Some("session_conflict")
        );
        let since = fake.status().unwrap().revision;
        fake.remove(CLAUDE_DONE).unwrap();
        assert_eq!(
            fake.remove(CLAUDE_DONE).unwrap_err().code(),
            Some("not_found")
        );
        assert_eq!(fake.changes(since, 0).unwrap().deleted, [CLAUDE_DONE]);
        // Messages: a user event, waiting_input resumes running.
        fake.set_state(CLAUDE_CANCELLED, "waiting_input");
        let (session, event) = fake
            .message(CLAUDE_CANCELLED, "Keep v1 for a release")
            .unwrap();
        assert_eq!(session.runtime, RuntimeState::Running);
        assert_eq!(event.stream, agents::Stream::User);
        assert_eq!(
            fake.message(CLAUDE_CANCELLED, "  ").unwrap_err().code(),
            Some("validation")
        );
        let calls = fake.calls();
        assert_eq!((calls.cancel, calls.remove, calls.message), (4, 3, 3));
    }

    #[test]
    fn events_page_with_a_cursor() {
        let fake = FakeAgents::fixture();
        let first = fake.events(CLAUDE_RUNNING, 0, 2, 25).unwrap();
        assert_eq!(first.events.len(), 2);
        assert!(first.has_more && !first.terminal);
        let rest = fake
            .events(CLAUDE_RUNNING, first.next_after, 200, 0)
            .unwrap();
        assert_eq!(rest.events.first().unwrap().seq, 3);
        assert!(!rest.has_more);
        assert_eq!(rest.last_seq, 5);
        // The credential-like stderr line is masked at parse time.
        assert!(rest
            .events
            .iter()
            .all(|e| !e.text.contains("abcdefghijklmnop")));
        assert!(fake.events("ffffffffffff", 0, 10, 0).is_err());
    }

    #[test]
    fn a_shared_sandbox_carries_launches_between_windows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let launcher = FakeAgents::shared(path.clone());
        let hub = FakeAgents::shared(path.clone());
        launcher
            .launch(&request("0123456789ab", "Shared", "devin", None))
            .unwrap();
        assert_eq!(hub.session("0123456789ab").unwrap().title, "Shared");
        // The Hub's /changes advances it (autoplay) and the launcher sees it.
        hub.changes(0, 0).unwrap();
        assert_eq!(
            launcher.session("0123456789ab").unwrap().runtime,
            RuntimeState::Running
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A scene goes back to the in-memory seed.
        hub.use_fixture();
        assert!(hub.session("0123456789ab").is_err());
        assert_eq!(hub.now(), FIXTURE_NOW);
    }
}
