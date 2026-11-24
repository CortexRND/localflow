//! Local perf ledger (port of `localflow/perf_ledger.py`): one JSON line of
//! timings per dictation, meeting or recall.
//!
//! `~/.localflow/perf/events.jsonl` (or `LOCALFLOW_PERF_LEDGER`), append-only,
//! rotated to `events.jsonl.1` at [`MAX_BYTES`] (one previous file kept). Both
//! runtimes write the same canonical line: every key in schema order, compact
//! separators, `null` for an absent value (never omitted), `stt`/`llm` either
//! `null` or an object with all their keys. Readers also accept missing keys
//! as `null`, ignore unknown keys and skip lines that do not parse.
//!
//! Privacy: numbers and labels only. [`PerfEvent`] has no field that can hold
//! text, and every string is a [`Label`], which only accepts identifier-shaped
//! values (no spaces, no URLs), so transcript text, app names, window titles,
//! URLs and prompts can never be written.
//!
//! Writing is best effort: [`PerfLedger::record`] never fails or panics; a
//! failure is logged at debug. `perf_metrics = false` makes it a no-op.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;

/// Rotate once `events.jsonl` reaches this size.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Full path override for the ledger file (tests, parity).
pub const ENV_PATH: &str = "LOCALFLOW_PERF_LEDGER";
/// The config field (`[features] perf_metrics`).
pub const CONFIG_FIELD: &str = "perf_metrics";
/// Placeholder for a label that is not identifier-shaped.
pub const OTHER_LABEL: &str = "other";
const LOG_TARGET: &str = "localflow.perf_ledger";

/// Keys of a canonical line, in order.
pub const FIELDS: &[&str] = &[
    "ts",
    "runtime",
    "version",
    "kind",
    "status",
    "audio_s",
    "queue_ms",
    "stt_ms",
    "llm_ms",
    "render_ms",
    "paste_ms",
    "total_ms",
    "stt",
    "llm",
    "chars",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Dictate,
    Command,
    Transform,
    MeetingSummarize,
    MeetingChat,
    Recall,
    SttLoad,
    LlmWarmup,
    Startup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Python,
    Rust,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Error,
}

impl Status {
    pub fn from_ok(ok: bool) -> Self {
        if ok {
            Status::Ok
        } else {
            Status::Error
        }
    }
}

/// An identifier-shaped string (`[A-Za-z0-9._:/@+-]{0,128}`, no `://`).
/// Anything else becomes `"other"`, so no sentence or URL can be stored.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize)]
#[serde(transparent)]
pub struct Label(String);

impl Label {
    pub fn new(value: &str) -> Self {
        let valid = value.chars().count() <= 128
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:/@+-".contains(c))
            && !value.contains("://");
        Label(if valid { value } else { OTHER_LABEL }.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Label {
    fn from(value: &str) -> Self {
        Label::new(value)
    }
}

impl<'de> Deserialize<'de> for Label {
    /// Like Python `label()`: a non-string value becomes `"other"`.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::String(text) => Label::new(&text),
            _ => Label(OTHER_LABEL.to_string()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SttLabels {
    #[serde(default)]
    pub backend: Label,
    #[serde(default)]
    pub model: Label,
    #[serde(default)]
    pub provider: Label,
}

impl SttLabels {
    pub fn new(backend: &str, model: &str, provider: &str) -> Self {
        Self {
            backend: Label::new(backend),
            model: Label::new(model),
            provider: Label::new(provider),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LlmLabels {
    #[serde(default)]
    pub provider: Label,
    #[serde(default)]
    pub model: Label,
}

impl LlmLabels {
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: Label::new(provider),
            model: Label::new(model),
        }
    }

    /// The configured cleanup/command LLM.
    pub fn from_config(config: &Config) -> Self {
        Self::new(
            config.field_str("llm_provider").unwrap_or_default(),
            config.field_str("llm_model").unwrap_or_default(),
        )
    }
}

/// One ledger line. Field order is the canonical key order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerfEvent {
    pub ts: Label,
    pub runtime: Runtime,
    pub version: Label,
    pub kind: Kind,
    pub status: Status,
    #[serde(default)]
    pub audio_s: Option<f64>,
    #[serde(default)]
    pub queue_ms: Option<f64>,
    #[serde(default)]
    pub stt_ms: Option<f64>,
    #[serde(default)]
    pub llm_ms: Option<f64>,
    #[serde(default)]
    pub render_ms: Option<f64>,
    #[serde(default)]
    pub paste_ms: Option<f64>,
    #[serde(default)]
    pub total_ms: Option<f64>,
    #[serde(default)]
    pub stt: Option<SttLabels>,
    #[serde(default)]
    pub llm: Option<LlmLabels>,
    #[serde(default)]
    pub chars: Option<u64>,
}

/// Milliseconds as f64 from a `Duration`.
pub fn ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// 3 decimals, ties away from zero (Python `_number` does the same).
fn number(value: Option<f64>) -> Option<f64> {
    value
        .map(|v| v * 1000.0)
        .filter(|v| v.is_finite() && *v >= 0.0)
        // `+ 0.0` turns -0.0 into 0.0, as Python's int arithmetic does.
        .map(|v| v.round() / 1000.0 + 0.0)
}

/// Python `_TS`: `YYYY-MM-DDTHH:MM:SS[.f{1,9}]Z`.
fn valid_ts(ts: &str) -> bool {
    static TS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    TS.get_or_init(|| {
        regex::Regex::new(
            r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]{1,9})?Z$",
        )
        .unwrap()
    })
    .is_match(ts)
}

impl PerfEvent {
    /// A Rust-runtime event stamped now; set the remaining fields directly.
    pub fn new(kind: Kind, status: Status) -> Self {
        Self {
            ts: Label::new(
                &chrono::Utc::now()
                    .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                    .to_string(),
            ),
            runtime: Runtime::Rust,
            version: Label::new(env!("CARGO_PKG_VERSION")),
            kind,
            status,
            audio_s: None,
            queue_ms: None,
            stt_ms: None,
            llm_ms: None,
            render_ms: None,
            paste_ms: None,
            total_ms: None,
            stt: None,
            llm: None,
            chars: None,
        }
    }

    /// Numbers rounded to 3 decimals; negative or non-finite ones become null.
    pub fn normalized(mut self) -> Self {
        for value in [
            &mut self.audio_s,
            &mut self.queue_ms,
            &mut self.stt_ms,
            &mut self.llm_ms,
            &mut self.render_ms,
            &mut self.paste_ms,
            &mut self.total_ms,
        ] {
            *value = number(*value);
        }
        self
    }

    /// The canonical JSON line (no trailing newline).
    pub fn to_line(&self) -> String {
        serde_json::to_string(&self.clone().normalized()).expect("a perf event always serializes")
    }

    /// One line written by either runtime, or `None` when it is blank,
    /// truncated or invalid.
    pub fn parse_line(line: &str) -> Option<Self> {
        if line.trim().is_empty() {
            return None;
        }
        let value: Value = serde_json::from_str(line).ok()?;
        if !value.is_object() {
            return None;
        }
        // Numbers of the wrong type are dropped (null), as Python does.
        let mut value = value;
        let object = value.as_object_mut()?;
        for key in [
            "audio_s",
            "queue_ms",
            "stt_ms",
            "llm_ms",
            "render_ms",
            "paste_ms",
            "total_ms",
        ] {
            if object.get(key).is_some_and(|v| !v.is_number()) {
                object.insert(key.into(), Value::Null);
            }
        }
        if object.get("chars").is_some_and(|v| !v.is_u64()) {
            object.insert("chars".into(), Value::Null);
        }
        for key in ["stt", "llm"] {
            if object.get(key).is_some_and(|v| !v.is_object()) {
                object.insert(key.into(), Value::Null);
            }
        }
        let event: PerfEvent = serde_json::from_value(value).ok()?;
        valid_ts(event.ts.as_str()).then(|| event.normalized())
    }
}

/// `LOCALFLOW_PERF_LEDGER` when set, else `~/.localflow/perf/events.jsonl`.
pub fn default_ledger_path() -> PathBuf {
    if let Some(path) = std::env::var_os(ENV_PATH).filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    crate::home::localflow_dir()
        .join("perf")
        .join("events.jsonl")
}

/// The `[features] perf_metrics` flag (on unless explicitly `false`).
pub fn enabled(config: &Config) -> bool {
    config.field_bool(CONFIG_FIELD) != Some(false)
}

#[derive(Clone, Debug)]
pub struct PerfLedger {
    path: PathBuf,
    max_bytes: u64,
    enabled: bool,
}

impl Default for PerfLedger {
    fn default() -> Self {
        Self::new(default_ledger_path())
    }
}

impl PerfLedger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_bytes: MAX_BYTES,
            enabled: true,
        }
    }

    /// The default ledger, disabled when `perf_metrics` is false.
    pub fn for_config(config: &Config) -> Self {
        Self::default().with_enabled(enabled(config))
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn previous_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".1");
        self.path.with_file_name(name)
    }

    /// Append one event. Never fails; `false` when disabled or not written.
    pub fn record(&self, event: &PerfEvent) -> bool {
        if !self.enabled {
            return false;
        }
        match self.try_record(event) {
            Ok(()) => true,
            Err(error) => {
                log::debug!(target: LOG_TARGET, "perf ledger: could not record an event: {error}");
                false
            }
        }
    }

    /// [`record`](Self::record), skipped when `config` turns metrics off.
    pub fn record_if(&self, config: &Config, event: &PerfEvent) -> bool {
        enabled(config) && self.record(event)
    }

    fn try_record(&self, event: &PerfEvent) -> io::Result<()> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        self.rotate_if_full();
        let mut line = event.to_line();
        line.push('\n');
        // One O_APPEND write per line, so concurrent writers never interleave.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())
    }

    fn rotate_if_full(&self) {
        let Ok(meta) = fs::metadata(&self.path) else {
            return;
        };
        if meta.len() >= self.max_bytes {
            // A concurrent writer may rotate at the same moment; losing one
            // rotation's worth of old events is acceptable for local metrics.
            if let Err(error) = fs::rename(&self.path, self.previous_path()) {
                log::debug!(target: LOG_TARGET, "perf ledger: rotate failed: {error}");
            }
        }
    }

    /// Older file first, then the current one; bad lines are skipped.
    pub fn read_events(&self) -> Vec<PerfEvent> {
        let mut events = Vec::new();
        for path in [self.previous_path(), self.path.clone()] {
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            for raw in bytes.split(|b| *b == b'\n') {
                let Ok(line) = std::str::from_utf8(raw) else {
                    continue;
                };
                if let Some(event) = PerfEvent::parse_line(line.trim_end_matches('\r')) {
                    events.push(event);
                }
            }
        }
        events
    }

    /// Delete the ledger and its rotated file.
    pub fn clear(&self) -> io::Result<()> {
        for path in [self.path.clone(), self.previous_path()] {
            match fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ledger(dir: &tempfile::TempDir) -> PerfLedger {
        PerfLedger::new(dir.path().join("perf").join("events.jsonl"))
    }

    fn dictation() -> PerfEvent {
        PerfEvent {
            audio_s: Some(3.42),
            queue_ms: Some(1.2),
            stt_ms: Some(145.0),
            llm_ms: Some(0.0),
            render_ms: Some(0.4),
            paste_ms: None,
            total_ms: Some(154.7),
            stt: Some(SttLabels::new("faster-whisper", "base", "cpu")),
            llm: Some(LlmLabels::new("ollama", "llama3.2:3b")),
            chars: Some(42),
            // Fixed so two calls compare equal across a millisecond tick.
            ts: Label::new("2026-11-24T10:00:00.000Z"),
            ..PerfEvent::new(Kind::Dictate, Status::Ok)
        }
    }

    #[test]
    fn line_has_every_key_in_schema_order_with_nulls() {
        let mut event = dictation();
        event.ts = Label::new("2026-11-24T10:00:00.000Z");
        event.version = Label::new("0.2.2");
        assert_eq!(
            event.to_line(),
            r#"{"ts":"2026-11-24T10:00:00.000Z","runtime":"rust","version":"0.2.2","kind":"dictate","status":"ok","audio_s":3.42,"queue_ms":1.2,"stt_ms":145.0,"llm_ms":0.0,"render_ms":0.4,"paste_ms":null,"total_ms":154.7,"stt":{"backend":"faster-whisper","model":"base","provider":"cpu"},"llm":{"provider":"ollama","model":"llama3.2:3b"},"chars":42}"#
        );
        let bare = PerfEvent::new(Kind::Startup, Status::Error).to_line();
        let value: Value = serde_json::from_str(&bare).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, FIELDS);
        assert_eq!(value["stt"], Value::Null);
        assert_eq!(value["status"], "error");
        assert!(valid_ts(value["ts"].as_str().unwrap()));
    }

    #[test]
    fn every_kind_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(&dir);
        let kinds = [
            Kind::Dictate,
            Kind::Command,
            Kind::Transform,
            Kind::MeetingSummarize,
            Kind::MeetingChat,
            Kind::Recall,
            Kind::SttLoad,
            Kind::LlmWarmup,
            Kind::Startup,
        ];
        let written: Vec<PerfEvent> = kinds
            .iter()
            .map(|kind| PerfEvent {
                kind: *kind,
                ..dictation()
            })
            .collect();
        for event in &written {
            assert!(ledger.record(event));
        }
        assert_eq!(ledger.read_events(), written);
        let names: Vec<String> = written
            .iter()
            .map(|e| {
                serde_json::to_value(e.kind)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            names,
            [
                "dictate",
                "command",
                "transform",
                "meeting_summarize",
                "meeting_chat",
                "recall",
                "stt_load",
                "llm_warmup",
                "startup"
            ]
        );
    }

    #[test]
    fn rotates_at_max_bytes_keeping_one_previous_file() {
        let dir = tempfile::tempdir().unwrap();
        let line_len = dictation().to_line().len() as u64 + 1;
        let ledger = ledger(&dir).with_max_bytes(line_len * 3);
        for _ in 0..3 {
            ledger.record(&dictation());
        }
        assert!(!ledger.previous_path().exists());
        ledger.record(&dictation());
        assert!(ledger.previous_path().exists());
        assert_eq!(
            fs::metadata(ledger.previous_path()).unwrap().len(),
            line_len * 3
        );
        assert_eq!(fs::metadata(ledger.path()).unwrap().len(), line_len);
        assert_eq!(ledger.read_events().len(), 4);
        for _ in 0..3 {
            ledger.record(&dictation());
        }
        // The second rotation replaces the first previous file.
        assert_eq!(ledger.read_events().len(), 4);
        assert_eq!(MAX_BYTES, 5 * 1024 * 1024);
    }

    #[test]
    fn corrupt_and_truncated_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(&dir);
        let good = dictation().to_line();
        let mut contents = String::new();
        contents.push_str("not json\n\n[1,2]\n");
        contents.push_str(&good);
        contents.push('\n');
        contents.push_str(r#"{"ts":"2026-11-24T10:00:00.000Z","runtime":"rust","version":"0.2.2","kind":"teleport","status":"ok"}"#);
        contents.push('\n');
        contents.push_str(r#"{"ts":"yesterday","runtime":"rust","version":"0.2.2","kind":"recall","status":"ok"}"#);
        contents.push('\n');
        contents.push_str(&good[..good.len() / 2]);
        fs::create_dir_all(ledger.path().parent().unwrap()).unwrap();
        fs::write(ledger.path(), contents.as_bytes()).unwrap();
        fs::write(ledger.previous_path(), b"\xff\xfe garbage\n").unwrap();
        assert_eq!(ledger.read_events(), vec![dictation().normalized()]);
    }

    #[test]
    fn missing_and_unknown_keys_are_tolerated() {
        let line = json!({
            "ts": "2026-11-24T10:00:00.000Z", "runtime": "python", "version": "0.2.2",
            "kind": "recall", "status": "ok", "total_ms": 12.5, "chars": "x",
            "stt_ms": "fast", "text": "secret words", "llm": {"provider": "ollama"},
        })
        .to_string();
        let event = PerfEvent::parse_line(&line).unwrap();
        assert_eq!(event.runtime, Runtime::Python);
        assert_eq!(event.total_ms, Some(12.5));
        assert_eq!(event.stt_ms, None);
        assert_eq!(event.chars, None);
        assert_eq!(event.llm, Some(LlmLabels::new("ollama", "")));
        assert!(!event.to_line().contains("secret"));
    }

    #[test]
    fn disabled_ledger_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(&dir).with_enabled(false);
        assert!(!ledger.record(&dictation()));
        assert!(!dir.path().join("perf").exists());

        let mut config = Config::default();
        assert!(enabled(&config));
        config.set_field(CONFIG_FIELD, json!(false));
        assert!(!enabled(&config));
        let on = self::ledger(&dir);
        assert!(!on.record_if(&config, &dictation()));
        assert!(!dir.path().join("perf").exists());
        config.set_field(CONFIG_FIELD, json!(true));
        assert!(on.record_if(&config, &dictation()));
        assert_eq!(on.read_events().len(), 1);
    }

    #[test]
    fn labels_never_carry_text_or_urls() {
        for bad in [
            "hello world this is my transcript",
            "https://example.com/x",
            "Slack — #general",
            "line\nbreak",
            &"x".repeat(129),
        ] {
            assert_eq!(Label::new(bad).as_str(), OTHER_LABEL, "{bad:?}");
        }
        for good in [
            "llama3.2:3b",
            "accounts/fireworks/models/kimi-k3",
            "mlx-community/whisper-small",
            "",
        ] {
            assert_eq!(Label::new(good).as_str(), good);
        }
        let parsed: SttLabels =
            serde_json::from_value(json!({"backend": "my secret app", "model": "base"})).unwrap();
        assert_eq!(parsed, SttLabels::new("other", "base", ""));
    }

    #[test]
    fn numbers_are_rounded_and_invalid_ones_dropped() {
        let event = PerfEvent {
            stt_ms: Some(1.23456),
            llm_ms: Some(-1.0),
            total_ms: Some(f64::NAN),
            render_ms: Some(f64::INFINITY),
            ..PerfEvent::new(Kind::Dictate, Status::Ok)
        }
        .normalized();
        assert_eq!(event.stt_ms, Some(1.235));
        // Ties round away from zero (Python `_number` matches); -0.0 is 0.0.
        assert_eq!(number(Some(1.0005)), Some(1.001));
        assert_eq!(number(Some(0.0005)), Some(0.001));
        assert!(number(Some(-0.0)).unwrap().is_sign_positive());
        assert_eq!(number(Some(f64::MAX)), None);
        assert_eq!(event.llm_ms, None);
        assert_eq!(event.total_ms, None);
        assert_eq!(event.render_ms, None);
    }

    #[test]
    fn clear_removes_both_files_and_unwritable_path_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = ledger(&dir).with_max_bytes(1);
        ledger.record(&dictation());
        ledger.record(&dictation());
        assert!(ledger.previous_path().exists());
        ledger.clear().unwrap();
        assert!(!ledger.path().exists() && !ledger.previous_path().exists());
        ledger.clear().unwrap();
        assert!(ledger.read_events().is_empty());

        let blocker = dir.path().join("file");
        fs::write(&blocker, b"x").unwrap();
        let broken = PerfLedger::new(blocker.join("perf").join("events.jsonl"));
        assert!(!broken.record(&dictation()));
    }

    #[test]
    fn default_path_follows_env_override_then_home() {
        use crate::test_support::{EnvGuard, GLOBAL_LOCK};
        let _lock = GLOBAL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::new();
        env.set(ENV_PATH, dir.path().join("x.jsonl"));
        assert_eq!(default_ledger_path(), dir.path().join("x.jsonl"));
        env.remove(ENV_PATH);
        assert_eq!(
            default_ledger_path(),
            crate::home::localflow_dir()
                .join("perf")
                .join("events.jsonl")
        );
    }
}
