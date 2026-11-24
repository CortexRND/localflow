//! `~/.localflow/status.json` and `last.txt` (`localflow/status.py`).

use std::io::Write;
use std::path::{Path, PathBuf};

pub fn status_path() -> PathBuf {
    lf_core::home::localflow_dir().join("status.json")
}

pub fn last_text_path() -> PathBuf {
    lf_core::home::localflow_dir().join("last.txt")
}

pub fn title_prefix(state: &str) -> &'static str {
    match state {
        "recording" => "● ",
        "locked" => "🔒 ",
        "command" => "⌘ ",
        "processing" => "… ",
        _ => "",
    }
}

pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .tempfile_in(parent)?;
    tmp.write_all(content.as_bytes())?;
    tmp.as_file().sync_all()?;
    replace_with(tmp, path)
}

#[cfg(not(windows))]
fn replace_with(tmp: tempfile::NamedTempFile, path: &Path) -> std::io::Result<()> {
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// First pause before retrying a replace that a holder of `path` blocked.
#[cfg(windows)]
const REPLACE_FIRST_PAUSE: std::time::Duration = std::time::Duration::from_millis(1);
/// Total time a blocked replace waits before giving up.
#[cfg(windows)]
const REPLACE_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// `persist` is `MoveFileExW`, which cannot replace a target any reader has
/// open; std's rename uses POSIX semantics and can, as long as the holder
/// shares delete. The bounded retry covers holders that do not (they keep
/// it open briefly). The staged file never outlives a failed replace.
#[cfg(windows)]
fn replace_with(tmp: tempfile::NamedTempFile, path: &Path) -> std::io::Result<()> {
    let (file, staged) = tmp.keep().map_err(|e| e.error)?;
    drop(file);
    let mut waited = std::time::Duration::ZERO;
    let mut pause = REPLACE_FIRST_PAUSE;
    let result = loop {
        match std::fs::rename(&staged, path) {
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    && waited < REPLACE_BUDGET =>
            {
                let step = pause.min(REPLACE_BUDGET - waited);
                std::thread::sleep(step);
                waited += step;
                pause *= 2;
            }
            other => break other,
        }
    };
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    result
}

pub fn write_state(state: &str, path: &Path) -> std::io::Result<()> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    let body = serde_json::json!({"state": state, "since": since, "pid": std::process::id()});
    write_atomic(path, &body.to_string())
}

// ------------------------------------------------------- protocol 2 ---
//
// Additive fields on top of `{state, since, pid}` (protocol 1, still written
// by the Python runtime and older Rust builds). Readers that only know
// protocol 1 keep working; readers that know protocol 2 must treat a file
// without `protocol: 2` as "running, readiness unknown", never as ready.
//
// - `protocol`: 2.
// - `instance`: random per runtime start, so a reader can tell restarts apart.
//   The process that owns the runtime gets it from `RuntimeHandle`, which is
//   how an in-process owner proves a file is its *current* runtime's.
// - `lock`: the identity line of the `localflow.lock` acquisition this
//   runtime holds (`<pid> <timestamp>`, rewritten on every acquisition). A
//   reader compares it with the lock file: a file from an earlier
//   acquisition (same-process restart, recycled PID) no longer matches.
// - `heartbeat`: rewritten every [`HEARTBEAT`] while the runtime lives. A
//   file whose heartbeat is older than [`FRESH_SECONDS`] is stale, even when
//   its pid was recycled by an unrelated process.
// - `listener`: `starting` until the platform accepted the keyboard listener,
//   then `ok`; `denied` / `dead` / `unsupported` when it failed or exited.
// - `listener_detail`: the user-facing reason for a failed listener.
// - `session`: `macos`, `x11`, `wayland` or `unknown`.
// - `mic`: `{configured, observed}`; `observed` is `unknown` unless the
//   runtime actually opened the device (it never probes just to report).
// - `warnings`: short codes (`input-monitoring`, `dead-tap`, `listener`,
//   `speech-model`).

/// Current status-file protocol written by [`StatusReporter`].
pub const PROTOCOL: u64 = 2;
/// How often a live runtime rewrites its heartbeat.
pub const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(5);
/// A heartbeat older than this means the runtime stopped responding.
pub const FRESH_SECONDS: f64 = 15.0;

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// What the keyboard listener is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerFact {
    Starting,
    Ok,
    Denied,
    Dead,
    Unsupported,
}

impl ListenerFact {
    pub fn as_str(self) -> &'static str {
        match self {
            ListenerFact::Starting => "starting",
            ListenerFact::Ok => "ok",
            ListenerFact::Denied => "denied",
            ListenerFact::Dead => "dead",
            ListenerFact::Unsupported => "unsupported",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "starting" => ListenerFact::Starting,
            "ok" => ListenerFact::Ok,
            "denied" => ListenerFact::Denied,
            "dead" => ListenerFact::Dead,
            "unsupported" => ListenerFact::Unsupported,
            _ => return None,
        })
    }
}

/// The desktop session this process runs in (`session` field).
pub fn current_session() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        lf_input::linux::session().as_str()
    } else {
        "unknown"
    }
}

#[derive(Clone, Debug)]
struct Facts {
    state: String,
    since: f64,
    listener: ListenerFact,
    listener_detail: String,
    mic_configured: String,
    warnings: Vec<String>,
}

/// Writes protocol-2 status for one runtime process. Every change rewrites
/// the file atomically; [`StatusReporter::beat`] refreshes the heartbeat.
pub struct StatusReporter {
    path: PathBuf,
    instance: String,
    lock_identity: String,
    started: f64,
    session: &'static str,
    facts: std::sync::Mutex<Facts>,
    /// When the file last took a write; `None` after a failed write, so the
    /// next [`StatusReporter::beat`] republishes at once.
    last_beat: std::sync::Mutex<Option<std::time::Instant>>,
}

impl StatusReporter {
    pub fn new(path: &Path, mic_configured: &str) -> Self {
        let started = now_seconds();
        let instance = instance_id();
        StatusReporter {
            path: path.to_path_buf(),
            instance,
            lock_identity: String::new(),
            started,
            session: current_session(),
            facts: std::sync::Mutex::new(Facts {
                state: "idle".into(),
                since: started,
                listener: ListenerFact::Starting,
                listener_detail: String::new(),
                mic_configured: mic_configured.to_string(),
                warnings: Vec::new(),
            }),
            last_beat: std::sync::Mutex::new(Some(std::time::Instant::now())),
        }
    }

    /// Record the lock acquisition this runtime holds (see `lock`).
    pub fn with_lock_identity(mut self, identity: &str) -> Self {
        self.lock_identity = identity.trim().chars().take(200).collect();
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This runtime start's identifier, as written to the file.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// Mutate and publish under one lock: concurrent state, listener and
    /// heartbeat writes land in mutation order, so an older snapshot can
    /// never overwrite a newer one on disk.
    fn change(&self, f: impl FnOnce(&mut Facts)) {
        let mut facts = self.facts.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut facts);
        let written = match write_atomic(&self.path, &self.body(&facts).to_string()) {
            Ok(()) => Some(std::time::Instant::now()),
            Err(error) => {
                log::warn!("could not write {}: {error}", self.path.display());
                None
            }
        };
        *self.last_beat.lock().unwrap_or_else(|p| p.into_inner()) = written;
    }

    fn body(&self, facts: &Facts) -> serde_json::Value {
        serde_json::json!({
            "state": facts.state,
            "since": facts.since,
            "pid": std::process::id(),
            "protocol": PROTOCOL,
            "instance": self.instance,
            "lock": self.lock_identity,
            "started": self.started,
            "heartbeat": now_seconds(),
            "listener": facts.listener.as_str(),
            "listener_detail": facts.listener_detail,
            "session": self.session,
            "mic": {"configured": facts.mic_configured, "observed": "unknown"},
            "warnings": facts.warnings,
        })
    }

    /// Write the current facts (the first write of a starting runtime).
    pub fn publish(&self) {
        self.change(|_| {});
    }

    pub fn set_state(&self, state: &str) {
        self.change(|facts| {
            if facts.state != state {
                facts.state = state.to_string();
                facts.since = now_seconds();
            }
        });
    }

    /// Record a listener fact. `ok` clears an earlier listener warning; a
    /// failure keeps `detail` (never key data) for the reader to show.
    pub fn set_listener(&self, fact: ListenerFact, detail: &str) {
        self.change(|facts| {
            facts.listener = fact;
            facts.listener_detail = detail.chars().take(400).collect();
            let code = "listener".to_string();
            facts.warnings.retain(|w| *w != code);
            if !matches!(fact, ListenerFact::Ok | ListenerFact::Starting) {
                facts.warnings.push(code);
            }
        });
    }

    /// The listener stopped seeing keys (macOS dead tap): no longer ok.
    pub fn listener_dead(&self, code: &str, detail: &str) {
        self.change(|facts| {
            if facts.listener == ListenerFact::Ok {
                facts.listener = ListenerFact::Dead;
                facts.listener_detail = detail.chars().take(400).collect();
            }
            if !facts.warnings.iter().any(|w| w == code) {
                facts.warnings.push(code.to_string());
            }
        });
    }

    pub fn add_warning(&self, code: &str) {
        self.change(|facts| {
            if !facts.warnings.iter().any(|w| w == code) {
                facts.warnings.push(code.to_string());
            }
        });
    }

    pub fn set_mic_configured(&self, configured: &str) {
        self.change(|facts| facts.mic_configured = configured.to_string());
    }

    /// Rewrite the heartbeat when [`HEARTBEAT`] has passed since the last
    /// write, or at once when the last write failed. Cheap to call often.
    pub fn beat(&self) {
        let due = match *self.last_beat.lock().unwrap_or_else(|p| p.into_inner()) {
            Some(at) => at.elapsed() >= HEARTBEAT,
            None => true,
        };
        if due {
            self.publish();
        }
    }
}

/// A per-start identifier (randomly keyed hash of time and pid); only used to
/// tell process starts apart, never as a secret.
fn instance_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    hasher.write_u32(std::process::id());
    format!("{:016x}", hasher.finish())
}

/// A parsed `status.json` of any protocol. Missing fields are `None`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusFile {
    pub state: Option<String>,
    pub since: Option<f64>,
    pub pid: Option<u32>,
    pub protocol: u64,
    pub instance: Option<String>,
    /// The lock acquisition identity the writer held (protocol 2).
    pub lock_identity: Option<String>,
    pub heartbeat: Option<f64>,
    pub listener: Option<ListenerFact>,
    pub listener_detail: String,
    pub session: Option<String>,
    pub mic_configured: Option<String>,
    pub mic_observed: Option<String>,
    pub warnings: Vec<String>,
}

/// Largest status file a reader accepts.
pub const MAX_STATUS_BYTES: u64 = 16 * 1024;

impl StatusFile {
    /// Parse untrusted JSON; `None` when it isn't a status object.
    pub fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        let text_field = |key: &str| {
            object
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(|s| s.chars().take(400).collect::<String>())
        };
        let protocol = object
            .get("protocol")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        let mic = object.get("mic").and_then(serde_json::Value::as_object);
        let mic_text = |key: &str| {
            mic.and_then(|m| m.get(key))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        Some(StatusFile {
            state: text_field("state"),
            since: object.get("since").and_then(serde_json::Value::as_f64),
            pid: object
                .get("pid")
                .and_then(serde_json::Value::as_u64)
                .and_then(|p| u32::try_from(p).ok()),
            protocol,
            instance: text_field("instance"),
            lock_identity: text_field("lock").filter(|l| !l.is_empty()),
            heartbeat: object.get("heartbeat").and_then(serde_json::Value::as_f64),
            listener: text_field("listener")
                .as_deref()
                .and_then(ListenerFact::parse),
            listener_detail: text_field("listener_detail").unwrap_or_default(),
            session: text_field("session"),
            mic_configured: mic_text("configured"),
            mic_observed: mic_text("observed"),
            warnings: object
                .get("warnings")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .take(16)
                        .map(|s| s.chars().take(40).collect())
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Read `path` (size-capped). `Ok(None)` when the file is absent.
    pub fn read(path: &Path) -> std::io::Result<Option<Self>> {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.take(MAX_STATUS_BYTES).read_to_string(&mut text)?;
        Ok(Some(Self::parse(&text).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "not a status file")
        })?))
    }

    /// Protocol 2 with a heartbeat younger than [`FRESH_SECONDS`] at `now`.
    pub fn fresh(&self, now: f64) -> bool {
        self.protocol >= PROTOCOL
            && self
                .heartbeat
                .is_some_and(|beat| now - beat <= FRESH_SECONDS && beat - now <= 5.0)
    }
}

pub fn write_last_text(text: &str, path: &Path) -> std::io::Result<()> {
    write_atomic(path, text)
}

pub fn read_last_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_and_last_text_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let status = dir.path().join("status.json");
        write_state("recording", &status).unwrap();
        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&status).unwrap()).unwrap();
        assert_eq!(data["state"], "recording");
        assert_eq!(data["pid"], std::process::id());
        let last = dir.path().join("last.txt");
        assert_eq!(read_last_text(&last), "");
        write_last_text("hello", &last).unwrap();
        assert_eq!(read_last_text(&last), "hello");
        assert_eq!(title_prefix("locked"), "🔒 ");
        assert_eq!(title_prefix("idle"), "");
    }

    #[test]
    fn protocol_two_keeps_the_protocol_one_fields_and_tracks_the_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        let reporter = StatusReporter::new(&path, "USB Mic");
        reporter.publish();
        let first = StatusFile::read(&path).unwrap().unwrap();
        assert_eq!(first.state.as_deref(), Some("idle"));
        assert_eq!(first.pid, Some(std::process::id()));
        assert!(first.since.is_some());
        assert_eq!(first.protocol, PROTOCOL);
        assert_eq!(first.listener, Some(ListenerFact::Starting));
        assert_eq!(first.mic_configured.as_deref(), Some("USB Mic"));
        assert_eq!(first.mic_observed.as_deref(), Some("unknown"));
        assert!(first.fresh(now_seconds()));
        assert!(!first.fresh(now_seconds() + FRESH_SECONDS + 1.0));

        reporter.set_listener(ListenerFact::Ok, "");
        reporter.set_state("recording");
        let ok = StatusFile::read(&path).unwrap().unwrap();
        assert_eq!(ok.listener, Some(ListenerFact::Ok));
        assert_eq!(ok.state.as_deref(), Some("recording"));
        assert_eq!(ok.instance, first.instance);
        assert!(ok.warnings.is_empty());

        reporter.listener_dead("dead-tap", "no keyboard events");
        let dead = StatusFile::read(&path).unwrap().unwrap();
        assert_eq!(dead.listener, Some(ListenerFact::Dead));
        assert_eq!(dead.warnings, vec!["dead-tap".to_string()]);

        reporter.set_listener(ListenerFact::Unsupported, "Wayland");
        let unsupported = StatusFile::read(&path).unwrap().unwrap();
        assert_eq!(unsupported.listener_detail, "Wayland");
        assert!(unsupported.warnings.contains(&"listener".to_string()));
    }

    #[test]
    fn concurrent_writers_never_leave_an_older_snapshot_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        let reporter = std::sync::Arc::new(StatusReporter::new(&path, ""));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let reporter = reporter.clone();
                std::thread::spawn(move || {
                    for n in 0..40 {
                        match (i + n) % 3 {
                            0 => reporter.set_state(if n % 2 == 0 { "recording" } else { "idle" }),
                            1 => reporter.set_listener(ListenerFact::Ok, ""),
                            _ => reporter.publish(),
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // The last mutation is what's published: the file equals the facts.
        reporter.set_state("processing");
        let facts = reporter.facts.lock().unwrap().clone();
        let file = StatusFile::read(&path).unwrap().unwrap();
        assert_eq!(file.state.as_deref(), Some(facts.state.as_str()));
        assert_eq!(file.listener, Some(facts.listener));
        assert_eq!(file.state.as_deref(), Some("processing"));
    }

    #[test]
    fn instance_and_lock_identity_are_published() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        let first = StatusReporter::new(&path, "").with_lock_identity("42 2026-01-01T00:00:00\n");
        first.publish();
        let file = StatusFile::read(&path).unwrap().unwrap();
        assert_eq!(file.instance.as_deref(), Some(first.instance()));
        assert_eq!(
            file.lock_identity.as_deref(),
            Some("42 2026-01-01T00:00:00")
        );
        let second = StatusReporter::new(&path, "");
        assert_ne!(
            first.instance(),
            second.instance(),
            "every start is distinct"
        );
        second.publish();
        assert_eq!(
            StatusFile::read(&path).unwrap().unwrap().lock_identity,
            None
        );
    }

    #[test]
    fn protocol_one_and_garbage_are_never_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        write_state("idle", &path).unwrap();
        let legacy = StatusFile::read(&path).unwrap().unwrap();
        assert_eq!(legacy.protocol, 1);
        assert_eq!(legacy.listener, None);
        assert!(!legacy.fresh(now_seconds()));
        assert!(StatusFile::read(&dir.path().join("missing"))
            .unwrap()
            .is_none());
        std::fs::write(&path, "[1,2]").unwrap();
        assert!(StatusFile::read(&path).is_err());
        let forged =
            StatusFile::parse(r#"{"protocol":2,"heartbeat":9e12,"listener":"ok"}"#).unwrap();
        assert!(
            !forged.fresh(now_seconds()),
            "a heartbeat from the future is not fresh"
        );
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_change_lands_while_a_reader_holds_the_status_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        let reporter = StatusReporter::new(&path, "");
        reporter.publish();
        let reader = std::fs::File::open(&path).unwrap();
        reporter.set_listener(ListenerFact::Ok, "");
        let file = StatusFile::read(&path).unwrap().unwrap();
        assert_eq!(file.listener, Some(ListenerFact::Ok));
        drop(reader);
        assert_eq!(dir_names(dir.path()), vec!["status.json".to_string()]);
    }

    #[test]
    fn a_failed_write_is_republished_by_the_next_beat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        let reporter = StatusReporter::new(&path, "");
        std::fs::create_dir(&path).unwrap();
        reporter.publish();
        reporter.set_listener(ListenerFact::Ok, "");
        std::fs::remove_dir(&path).unwrap();
        reporter.beat();
        let file = StatusFile::read(&path).unwrap().expect("beat republished");
        assert_eq!(file.listener, Some(ListenerFact::Ok));

        // After a successful write the next beat waits for HEARTBEAT.
        std::fs::remove_file(&path).unwrap();
        reporter.beat();
        assert!(!path.exists(), "a beat right after a good write is a no-op");
    }

    #[cfg(windows)]
    fn open_without_delete_sharing(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(path)
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn a_replace_waits_for_a_holder_that_does_not_share_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        write_atomic(&path, "old").unwrap();
        let holder = open_without_delete_sharing(&path);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            drop(holder);
        });
        write_atomic(&path, "new").unwrap();
        release.join().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(dir_names(dir.path()), vec!["status.json".to_string()]);
    }

    #[cfg(windows)]
    #[test]
    fn a_replace_blocked_past_its_budget_fails_and_leaves_no_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status.json");
        write_atomic(&path, "old").unwrap();
        let holder = open_without_delete_sharing(&path);
        let error = write_atomic(&path, "new").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(dir_names(dir.path()), vec!["status.json".to_string()]);
        drop(holder);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }
}
