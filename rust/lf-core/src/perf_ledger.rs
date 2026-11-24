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
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(parent)?;
            }
        }
        self.rotate_if_full();
        let mut line = event.to_line();
        line.push('\n');
        // One O_APPEND write per line, so concurrent writers never interleave.
        let mut opts = OpenOptions::new();
        opts.read(true).create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut file = opts.open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
        if file.metadata().is_ok_and(|meta| meta.len() > 0) && file.seek(SeekFrom::End(-1)).is_ok()
        {
            let mut tail = [0u8; 1];
            if file.read_exact(&mut tail).is_ok() && tail[0] != b'\n' {
                line.insert(0, '\n');
            }
        }
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

// --- read side: the `/api/perf` / `lf perf` summary --------------------------
//
// Line-for-line port of `summarize_events` in `localflow/perf_ledger.py`:
// same window filter, percentile formula, rounding and key order, so both
// runtimes serialize byte-identical JSON ([`summary_json`]) and tables
// ([`render_summary`]) for the same events and `now`.
//
// Canonical rules (shared with Python):
// - window: `ts >= now - window_seconds` (later timestamps, e.g. clock skew,
//   are kept); events whose `ts` is not a real calendar time are skipped.
// - `by_kind`: only kinds with events, in [`Kind::ALL`] order.
// - timings, RTF and model loads use `ok` events only; `failure_rate` covers
//   every event.
// - `stages`: only stages with at least one sample, in [`STAGES`] order.
// - percentiles: linear interpolation at `p * (n - 1)` on the sorted samples,
//   then 3 decimals (RTF and rates 4), ties away from zero.
// Privacy: only counts, numbers and identifier-shaped labels.

/// `(name, seconds)` of every accepted window.
pub const WINDOWS: [(&str, i64); 3] = [("24h", 86_400), ("7d", 604_800), ("30d", 2_592_000)];
pub const DEFAULT_WINDOW: &str = "7d";
pub const WINDOW_ERROR: &str = "window must be one of 24h, 7d, 30d";
/// Timing stages, in summary order.
pub const STAGES: [&str; 6] = [
    "queue_ms",
    "stt_ms",
    "llm_ms",
    "render_ms",
    "paste_ms",
    "total_ms",
];
/// p95 dictation time above which the summary names the slowest stage.
pub const BUDGET_MS: f64 = 1500.0;
const LOAD_KINDS: [Kind; 2] = [Kind::SttLoad, Kind::LlmWarmup];

impl Kind {
    /// Every kind, in canonical (Python `KINDS`) order.
    pub const ALL: [Kind; 9] = [
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

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Dictate => "dictate",
            Kind::Command => "command",
            Kind::Transform => "transform",
            Kind::MeetingSummarize => "meeting_summarize",
            Kind::MeetingChat => "meeting_chat",
            Kind::Recall => "recall",
            Kind::SttLoad => "stt_load",
            Kind::LlmWarmup => "llm_warmup",
            Kind::Startup => "startup",
        }
    }
}

/// A `window` other than `24h`, `7d` or `30d`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidWindow;

impl std::fmt::Display for InvalidWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(WINDOW_ERROR)
    }
}

impl std::error::Error for InvalidWindow {}

/// Seconds in `window`.
pub fn parse_window(window: &str) -> Result<i64, InvalidWindow> {
    WINDOWS
        .iter()
        .find(|(name, _)| *name == window)
        .map(|(_, seconds)| *seconds)
        .ok_or(InvalidWindow)
}

/// `digits` decimals, ties away from zero (Python `_round`).
fn round_to(value: f64, digits: i32) -> f64 {
    let factor = 10f64.powi(digits);
    (value * factor).round() / factor
}

/// Linear interpolation at `p * (n - 1)` of non-empty sorted samples.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let index = p * (sorted.len() - 1) as f64;
    let low = index.floor();
    let high = index.ceil();
    let (a, b) = (sorted[low as usize], sorted[high as usize]);
    a + (b - a) * (index - low)
}

fn sorted(samples: &[f64]) -> Vec<f64> {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted
}

/// `(p50, p95)` rounded to 3 decimals.
fn timing(samples: &[f64]) -> (f64, f64) {
    let sorted = sorted(samples);
    (
        round_to(percentile(&sorted, 0.5), 3),
        round_to(percentile(&sorted, 0.95), 3),
    )
}

fn median(samples: &[f64], digits: i32) -> Option<f64> {
    (!samples.is_empty()).then(|| round_to(percentile(&sorted(samples), 0.5), digits))
}

/// `(epoch seconds, nanoseconds)` of a ledger `ts`, `None` when it is not a
/// real UTC calendar time (Python `datetime` rules: year >= 1, no second 60).
fn instant(ts: &str) -> Option<(i64, u32)> {
    if !valid_ts(ts) {
        return None;
    }
    let field = |range: std::ops::Range<usize>| ts[range].parse::<u32>().ok();
    let year = field(0..4)?;
    if year == 0 {
        return None;
    }
    let date = chrono::NaiveDate::from_ymd_opt(year as i32, field(5..7)?, field(8..10)?)?;
    let time = chrono::NaiveTime::from_hms_opt(field(11..13)?, field(14..16)?, field(17..19)?)?;
    let fraction = &ts[19..ts.len() - 1];
    let nanos = match fraction.strip_prefix('.') {
        Some(digits) => format!("{digits:0<9}").parse().ok()?,
        None => 0,
    };
    Some((date.and_time(time).and_utc().timestamp(), nanos))
}

fn stage(event: &PerfEvent, name: &str) -> Option<f64> {
    match name {
        "queue_ms" => event.queue_ms,
        "stt_ms" => event.stt_ms,
        "llm_ms" => event.llm_ms,
        "render_ms" => event.render_ms,
        "paste_ms" => event.paste_ms,
        "total_ms" => event.total_ms,
        _ => None,
    }
}

fn rtf(event: &PerfEvent) -> Option<f64> {
    match (event.audio_s, event.stt_ms) {
        (Some(audio), Some(stt)) if audio > 0.0 => Some(stt / (audio * 1000.0)),
        _ => None,
    }
}

fn load_labels(event: &PerfEvent) -> (String, String) {
    let pair = |a: &Label, b: &Label| (a.as_str().to_string(), b.as_str().to_string());
    match (event.kind, &event.stt, &event.llm) {
        (Kind::SttLoad, Some(stt), _) => pair(&stt.backend, &stt.model),
        (Kind::LlmWarmup, _, Some(llm)) => pair(&llm.provider, &llm.model),
        _ => (String::new(), String::new()),
    }
}

fn hint(stage: &str) -> &'static str {
    match stage {
        "queue_ms" => "waiting for the previous dictation is the slow step",
        "stt_ms" => "speech-to-text is the slow step",
        "llm_ms" => "cleanup LLM is the slow step",
        "render_ms" => "text formatting is the slow step",
        _ => "pasting is the slow step",
    }
}

/// The `/api/perf` summary of `events` inside `window` before `now`.
pub fn summarize_events(
    events: &[PerfEvent],
    window: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Value, InvalidWindow> {
    let window_seconds = parse_window(window)?;
    let cutoff = (
        now.timestamp() - window_seconds,
        now.timestamp_subsec_nanos(),
    );
    let mut timed: Vec<((i64, u32), &PerfEvent)> = events
        .iter()
        .filter_map(|event| instant(event.ts.as_str()).map(|at| (at, event)))
        .filter(|(at, _)| *at >= cutoff)
        .collect();
    // Stable: equal timestamps keep file order (the last load is the last line).
    timed.sort_by_key(|(at, _)| *at);
    let selected: Vec<&PerfEvent> = timed.into_iter().map(|(_, event)| event).collect();

    let mut by_kind = serde_json::Map::new();
    for kind in Kind::ALL {
        let of_kind: Vec<&PerfEvent> = selected
            .iter()
            .copied()
            .filter(|e| e.kind == kind)
            .collect();
        if of_kind.is_empty() {
            continue;
        }
        let ok: Vec<&PerfEvent> = of_kind
            .iter()
            .copied()
            .filter(|e| e.status == Status::Ok)
            .collect();
        let errors = of_kind.len() - ok.len();
        let mut stages = serde_json::Map::new();
        for name in STAGES {
            let samples: Vec<f64> = ok.iter().filter_map(|e| stage(e, name)).collect();
            if !samples.is_empty() {
                let (p50, p95) = timing(&samples);
                stages.insert(
                    name.into(),
                    serde_json::json!({"p50": p50, "p95": p95, "count": samples.len()}),
                );
            }
        }
        let rtfs: Vec<f64> = ok.iter().filter_map(|e| rtf(e)).collect();
        by_kind.insert(
            kind.as_str().into(),
            serde_json::json!({
                "count": of_kind.len(),
                "ok_count": ok.len(),
                "error_count": errors,
                "failure_rate": round_to(errors as f64 / of_kind.len() as f64, 4),
                "stages": stages,
                "median_rtf": median(&rtfs, 4),
            }),
        );
    }

    let mut loads: std::collections::BTreeMap<(usize, String, String), Vec<f64>> =
        std::collections::BTreeMap::new();
    for event in &selected {
        let Some(index) = LOAD_KINDS.iter().position(|k| *k == event.kind) else {
            continue;
        };
        if let (Status::Ok, Some(total)) = (event.status, event.total_ms) {
            let (engine, model) = load_labels(event);
            loads.entry((index, engine, model)).or_default().push(total);
        }
    }
    let model_loads: Vec<Value> = loads
        .into_iter()
        .map(|((index, engine, model), samples)| {
            let (p50, p95) = timing(&samples);
            serde_json::json!({
                "kind": LOAD_KINDS[index].as_str(),
                "engine": engine,
                "model": model,
                "count": samples.len(),
                "p50_ms": p50,
                "p95_ms": p95,
                "last_ms": samples[samples.len() - 1],
            })
        })
        .collect();

    let dictation = dictation_summary(by_kind.get("dictate"));
    Ok(serde_json::json!({
        "window": window,
        "window_seconds": window_seconds,
        "total_events": selected.len(),
        "by_kind": by_kind,
        "model_loads": model_loads,
        "dictation": dictation,
    }))
}

/// The Hub "Speed" card numbers (`dictate` events only).
fn dictation_summary(dictate: Option<&Value>) -> Value {
    let empty = serde_json::Map::new();
    let stages = dictate
        .and_then(|d| d["stages"].as_object())
        .unwrap_or(&empty);
    let p50 = |name: &str| stages.get(name).and_then(|s| s["p50"].as_f64());
    let mut slowest: Option<&str> = None;
    for name in &STAGES[..STAGES.len() - 1] {
        if let Some(value) = p50(name) {
            if slowest.is_none_or(|best| value > p50(best).unwrap_or(f64::MIN)) {
                slowest = Some(name);
            }
        }
    }
    let total = stages.get("total_ms");
    let p95 = total.and_then(|t| t["p95"].as_f64());
    let over_budget = p95.is_some_and(|p| p > BUDGET_MS);
    serde_json::json!({
        "count": dictate.map_or(0, |d| d["count"].as_u64().unwrap_or(0)),
        "median_total_ms": total.and_then(|t| t["p50"].as_f64()),
        "p95_total_ms": p95,
        "median_rtf": dictate.and_then(|d| d["median_rtf"].as_f64()),
        "slowest_stage": slowest,
        "budget_ms": BUDGET_MS,
        "hint": slowest.filter(|_| over_budget).map(hint),
    })
}

/// [`summarize_events`] over `ledger`, now.
pub fn summarize(ledger: &PerfLedger, window: &str) -> Result<Value, InvalidWindow> {
    parse_window(window)?;
    summarize_events(&ledger.read_events(), window, chrono::Utc::now())
}

/// Compact canonical JSON (the `/api/perf` body), byte-identical to Python's
/// `json.dumps(summary, separators=(",", ":"))`: floats use Python `repr`.
pub fn summary_json(summary: &Value) -> String {
    let mut out = String::new();
    write_compact(&mut out, summary);
    out
}

fn write_compact(out: &mut String, value: &Value) {
    match value {
        Value::Number(n) if n.as_i64().is_none() && n.as_u64().is_none() => {
            out.push_str(&float_repr(n.as_f64().unwrap_or(0.0)));
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(out, &Value::String(key.clone()));
                out.push(':');
                write_compact(out, item);
            }
            out.push('}');
        }
        // Null, bools, integers and strings: the summary only holds ASCII
        // labels, for which serde_json and `ensure_ascii=True` agree.
        other => out.push_str(&other.to_string()),
    }
}

fn float_repr(value: f64) -> String {
    crate::symbols::pycompat::float_repr(value)
}

fn cell(value: &Value) -> String {
    value.as_f64().map_or_else(|| "-".to_string(), float_repr)
}

/// Left-aligned columns two spaces apart, trailing spaces trimmed.
fn table(rows: &[Vec<String>]) -> Vec<String> {
    let columns = rows[0].len();
    let widths: Vec<usize> = (0..columns)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    rows.iter()
        .map(|row| {
            let mut cells: Vec<String> = row[..columns - 1]
                .iter()
                .enumerate()
                .map(|(c, cell)| format!("{cell:<width$}", width = widths[c]))
                .collect();
            cells.push(row[columns - 1].clone());
            cells.join("  ").trim_end().to_string()
        })
        .collect()
}

fn strings(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|c| c.to_string()).collect()
}

/// The `lf perf` table, line by line (Python `render_summary`).
pub fn render_summary(summary: &Value) -> Vec<String> {
    let window = summary["window"].as_str().unwrap_or_default();
    let total = summary["total_events"].as_u64().unwrap_or(0);
    if total == 0 {
        return vec![format!("No perf events in the last {window}.")];
    }
    let empty = serde_json::Map::new();
    let by_kind = summary["by_kind"].as_object().unwrap_or(&empty);
    let mut lines = vec![
        format!("Perf events in the last {window}: {total}"),
        String::new(),
    ];
    let mut rows = vec![strings(&[
        "Kind",
        "Count",
        "Errors",
        "Failure rate",
        "Median RTF",
    ])];
    for (kind, entry) in by_kind {
        let count = entry["count"].as_u64().unwrap_or(0);
        let errors = entry["error_count"].as_u64().unwrap_or(0);
        let percent = round_to((errors * 100) as f64 / count.max(1) as f64, 1);
        rows.push(vec![
            kind.clone(),
            count.to_string(),
            errors.to_string(),
            format!("{}%", float_repr(percent)),
            cell(&entry["median_rtf"]),
        ]);
    }
    lines.extend(table(&rows));
    let mut rows = vec![strings(&["Kind", "Stage", "p50 ms", "p95 ms", "Samples"])];
    for (kind, entry) in by_kind {
        for (name, timing) in entry["stages"].as_object().unwrap_or(&empty) {
            rows.push(vec![
                kind.clone(),
                name.clone(),
                cell(&timing["p50"]),
                cell(&timing["p95"]),
                timing["count"].as_u64().unwrap_or(0).to_string(),
            ]);
        }
    }
    if rows.len() > 1 {
        lines.push(String::new());
        lines.extend(table(&rows));
    }
    let loads = summary["model_loads"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    if !loads.is_empty() {
        let mut rows = vec![strings(&[
            "Load", "Engine", "Model", "Count", "p50 ms", "p95 ms", "Last ms",
        ])];
        let label = |value: &Value| match value.as_str() {
            Some("") | None => "-".to_string(),
            Some(text) => text.to_string(),
        };
        for load in loads {
            rows.push(vec![
                load["kind"].as_str().unwrap_or_default().to_string(),
                label(&load["engine"]),
                label(&load["model"]),
                load["count"].as_u64().unwrap_or(0).to_string(),
                cell(&load["p50_ms"]),
                cell(&load["p95_ms"]),
                cell(&load["last_ms"]),
            ]);
        }
        lines.push(String::new());
        lines.extend(table(&rows));
    }
    let dictation = &summary["dictation"];
    if dictation["median_total_ms"].is_f64() {
        let mut line = format!(
            "Dictation: median {} ms, p95 {} ms",
            cell(&dictation["median_total_ms"]),
            cell(&dictation["p95_total_ms"])
        );
        if dictation["median_rtf"].is_f64() {
            line.push_str(&format!(", RTF {}", cell(&dictation["median_rtf"])));
        }
        if let Some(stage) = dictation["slowest_stage"].as_str() {
            line.push_str(&format!(", slowest stage {stage}"));
        }
        lines.push(String::new());
        lines.push(line);
        if let Some(hint) = dictation["hint"].as_str() {
            lines.push(format!(
                "Hint: p95 is over {} ms; {hint}.",
                cell(&dictation["budget_ms"])
            ));
        }
    }
    lines
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

    // --- summary (goldens shared with tests/test_perf_api_cli.py) ---------

    fn now() -> chrono::DateTime<chrono::Utc> {
        "2026-11-24T12:00:00Z".parse().unwrap()
    }

    fn at(kind: Kind, ago: chrono::Duration) -> PerfEvent {
        PerfEvent {
            ts: Label::new(&(now() - ago).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
            ..PerfEvent::new(kind, Status::Ok)
        }
    }

    fn hours(n: i64) -> chrono::Duration {
        chrono::Duration::hours(n)
    }

    fn timed(kind: Kind, total: f64) -> PerfEvent {
        PerfEvent {
            total_ms: Some(total),
            ..at(kind, hours(1))
        }
    }

    fn dictations() -> Vec<PerfEvent> {
        let row = |audio: f64, queue: Option<f64>, stt: f64, llm: Option<f64>| PerfEvent {
            audio_s: Some(audio),
            queue_ms: queue,
            stt_ms: Some(stt),
            llm_ms: llm,
            total_ms: Some(stt),
            ..at(Kind::Dictate, hours(1))
        };
        vec![
            PerfEvent {
                stt: Some(SttLabels::new("faster-whisper", "base", "cpu")),
                ..row(2.0, Some(1.0), 100.0, Some(20.0))
            },
            row(4.0, Some(3.0), 200.0, Some(40.0)),
            row(1.0, Some(2.0), 300.0, Some(60.0)),
            row(0.0, None, 400.0, None),
            PerfEvent {
                status: Status::Error,
                stt_ms: Some(9999.0),
                total_ms: Some(9999.0),
                ..at(Kind::Dictate, hours(1))
            },
        ]
    }

    fn loads() -> Vec<PerfEvent> {
        let stt = |backend: &str, model: &str, ago: i64, total: f64| PerfEvent {
            total_ms: Some(total),
            stt: Some(SttLabels::new(backend, model, "cpu")),
            ..at(Kind::SttLoad, hours(ago))
        };
        vec![
            stt("faster-whisper", "base", 3, 900.0),
            stt("faster-whisper", "base", 2, 700.0),
            PerfEvent {
                total_ms: Some(50.0),
                llm: Some(LlmLabels::new("ollama", "llama3.2:3b")),
                ..at(Kind::LlmWarmup, hours(2))
            },
            stt("faster-whisper", "tiny", 2, 80.0),
            PerfEvent {
                status: Status::Error,
                ..stt("faster-whisper", "base", 1, 5.0)
            },
        ]
    }

    #[test]
    fn empty_summary_has_every_key() {
        let summary = summarize_events(&[], DEFAULT_WINDOW, now()).unwrap();
        assert_eq!(
            summary_json(&summary),
            r#"{"window":"7d","window_seconds":604800,"total_events":0,"by_kind":{},"model_loads":[],"dictation":{"count":0,"median_total_ms":null,"p95_total_ms":null,"median_rtf":null,"slowest_stage":null,"budget_ms":1500.0,"hint":null}}"#
        );
        assert_eq!(render_summary(&summary), ["No perf events in the last 7d."]);
    }

    #[test]
    fn windows_filter_by_timestamp_and_reject_unknown_names() {
        for (window, seconds, expected) in [
            ("24h", 86_400, 3),
            ("7d", 604_800, 4),
            ("30d", 2_592_000, 5),
        ] {
            assert_eq!(parse_window(window), Ok(seconds));
            let boundary = chrono::Duration::seconds(seconds);
            let events = [
                at(Kind::Recall, hours(23)),
                at(Kind::Recall, chrono::Duration::days(6)),
                at(Kind::Recall, chrono::Duration::days(29)),
                at(Kind::Recall, chrono::Duration::days(31)),
                at(Kind::Recall, -chrono::Duration::minutes(5)),
                at(Kind::Recall, boundary),
                at(Kind::Recall, boundary + chrono::Duration::milliseconds(1)),
            ];
            let summary = summarize_events(&events, window, now()).unwrap();
            assert_eq!(summary["window_seconds"], seconds, "{window}");
            assert_eq!(summary["total_events"], expected, "{window}");
            assert_eq!(summary["by_kind"]["recall"]["count"], expected);
        }
        for bad in ["", "1d", "7D", "24 h"] {
            assert_eq!(summarize_events(&[], bad, now()), Err(InvalidWindow));
            assert_eq!(InvalidWindow.to_string(), WINDOW_ERROR);
        }
        let mut unreal = Vec::new();
        for ts in [
            "2026-02-30T10:00:00Z",
            "2026-11-24T10:00:60Z",
            "0000-01-01T00:00:00Z",
        ] {
            let mut event = at(Kind::Recall, hours(1));
            event.ts = Label::new(ts);
            unreal.push(event);
        }
        unreal.push(at(Kind::Recall, hours(1)));
        let summary = summarize_events(&unreal, "30d", now()).unwrap();
        assert_eq!(summary["total_events"], 1);
    }

    #[test]
    fn failure_rate_stage_percentiles_and_rtf() {
        let summary = summarize_events(&dictations(), "7d", now()).unwrap();
        let dictate = &summary["by_kind"]["dictate"];
        assert_eq!(
            (
                &dictate["count"],
                &dictate["ok_count"],
                &dictate["error_count"]
            ),
            (&json!(5), &json!(4), &json!(1))
        );
        assert_eq!(dictate["failure_rate"], 0.2);
        let stages: Vec<&String> = dictate["stages"].as_object().unwrap().keys().collect();
        assert_eq!(stages, ["queue_ms", "stt_ms", "llm_ms", "total_ms"]);
        assert_eq!(
            dictate["stages"]["total_ms"],
            json!({"p50": 250.0, "p95": 385.0, "count": 4})
        );
        assert_eq!(
            dictate["stages"]["queue_ms"],
            json!({"p50": 2.0, "p95": 2.9, "count": 3})
        );
        assert_eq!(dictate["median_rtf"], 0.05);
        assert_eq!(
            summary["dictation"],
            json!({"count": 5, "median_total_ms": 250.0, "p95_total_ms": 385.0,
                   "median_rtf": 0.05, "slowest_stage": "stt_ms", "budget_ms": 1500.0,
                   "hint": null})
        );
    }

    #[test]
    fn rounding_is_half_away_from_zero() {
        let events = [timed(Kind::Recall, 0.001), timed(Kind::Recall, 0.002)];
        let summary = summarize_events(&events, "7d", now()).unwrap();
        assert_eq!(
            summary["by_kind"]["recall"]["stages"]["total_ms"]["p50"],
            0.002
        );
        let mut rates = vec![at(Kind::Recall, hours(1)), at(Kind::Recall, hours(1))];
        rates.push(PerfEvent {
            status: Status::Error,
            ..at(Kind::Recall, hours(1))
        });
        let summary = summarize_events(&rates, "7d", now()).unwrap();
        assert_eq!(summary["by_kind"]["recall"]["failure_rate"], 0.3333);
    }

    #[test]
    fn model_loads_group_sort_and_keep_the_last() {
        let summary = summarize_events(&loads(), "7d", now()).unwrap();
        assert_eq!(
            summary["model_loads"],
            json!([
                {"kind": "stt_load", "engine": "faster-whisper", "model": "base",
                 "count": 2, "p50_ms": 800.0, "p95_ms": 890.0, "last_ms": 700.0},
                {"kind": "stt_load", "engine": "faster-whisper", "model": "tiny",
                 "count": 1, "p50_ms": 80.0, "p95_ms": 80.0, "last_ms": 80.0},
                {"kind": "llm_warmup", "engine": "ollama", "model": "llama3.2:3b",
                 "count": 1, "p50_ms": 50.0, "p95_ms": 50.0, "last_ms": 50.0},
            ])
        );
        // The last load is the latest timestamp, not the last line.
        let stt = Some(SttLabels::new("parakeet", "x", ""));
        let later = PerfEvent {
            total_ms: Some(10.0),
            stt: stt.clone(),
            ..at(Kind::SttLoad, hours(1))
        };
        let earlier = PerfEvent {
            total_ms: Some(20.0),
            stt,
            ..at(Kind::SttLoad, hours(2))
        };
        let summary = summarize_events(&[later, earlier], "7d", now()).unwrap();
        assert_eq!(summary["model_loads"][0]["last_ms"], 10.0);
    }

    #[test]
    fn slowest_stage_names_a_hint_only_over_budget() {
        for (name, text) in [
            ("stt_ms", "speech-to-text is the slow step"),
            ("llm_ms", "cleanup LLM is the slow step"),
            ("paste_ms", "pasting is the slow step"),
            (
                "queue_ms",
                "waiting for the previous dictation is the slow step",
            ),
            ("render_ms", "text formatting is the slow step"),
        ] {
            let events: Vec<PerfEvent> = (0..3)
                .map(|i| {
                    let mut event = at(Kind::Dictate, hours(1));
                    for value in [
                        &mut event.queue_ms,
                        &mut event.stt_ms,
                        &mut event.llm_ms,
                        &mut event.render_ms,
                        &mut event.paste_ms,
                    ] {
                        *value = Some(10.0);
                    }
                    match name {
                        "stt_ms" => event.stt_ms = Some(1800.0),
                        "llm_ms" => event.llm_ms = Some(1800.0),
                        "paste_ms" => event.paste_ms = Some(1800.0),
                        "queue_ms" => event.queue_ms = Some(1800.0),
                        _ => event.render_ms = Some(1800.0),
                    }
                    event.total_ms = Some(1900.0 + f64::from(i));
                    event
                })
                .collect();
            let summary = summarize_events(&events, "7d", now()).unwrap();
            assert_eq!(summary["dictation"]["slowest_stage"], name);
            assert_eq!(summary["dictation"]["hint"], text);
            assert_eq!(
                render_summary(&summary).last().unwrap(),
                &format!("Hint: p95 is over 1500.0 ms; {text}.")
            );
        }
        let tie = PerfEvent {
            stt_ms: Some(50.0),
            llm_ms: Some(50.0),
            total_ms: Some(1500.0),
            ..at(Kind::Dictate, hours(1))
        };
        let summary = summarize_events(&[tie], "7d", now()).unwrap();
        assert_eq!(summary["dictation"]["slowest_stage"], "stt_ms");
        assert_eq!(summary["dictation"]["hint"], Value::Null);
    }

    #[test]
    fn render_summary_table() {
        let mut events = dictations();
        events.extend(loads());
        events.push(timed(Kind::Recall, 12.5));
        let summary = summarize_events(&events, "7d", now()).unwrap();
        assert_eq!(
            render_summary(&summary),
            [
                "Perf events in the last 7d: 11",
                "",
                "Kind        Count  Errors  Failure rate  Median RTF",
                "dictate     5      1       20.0%         0.05",
                "recall      1      0       0.0%          -",
                "stt_load    4      1       25.0%         -",
                "llm_warmup  1      0       0.0%          -",
                "",
                "Kind        Stage     p50 ms  p95 ms  Samples",
                "dictate     queue_ms  2.0     2.9     3",
                "dictate     stt_ms    250.0   385.0   4",
                "dictate     llm_ms    40.0    58.0    3",
                "dictate     total_ms  250.0   385.0   4",
                "recall      total_ms  12.5    12.5    1",
                "stt_load    total_ms  700.0   880.0   3",
                "llm_warmup  total_ms  50.0    50.0    1",
                "",
                "Load        Engine          Model        Count  p50 ms  p95 ms  Last ms",
                "stt_load    faster-whisper  base         2      800.0   890.0   700.0",
                "stt_load    faster-whisper  tiny         1      80.0    80.0    80.0",
                "llm_warmup  ollama          llama3.2:3b  1      50.0    50.0    50.0",
                "",
                "Dictation: median 250.0 ms, p95 385.0 ms, RTF 0.05, slowest stage stt_ms",
            ]
        );
        let text = summary_json(&summary);
        assert!(!text.contains(", ") && !text.contains(": "));
    }

    #[test]
    fn summarize_reads_both_ledger_files() {
        let dir = tempfile::tempdir().unwrap();
        let events = dictations();
        let first_three: u64 = events[..3]
            .iter()
            .map(|e| e.to_line().len() as u64 + 1)
            .sum();
        let ledger = ledger(&dir).with_max_bytes(first_three);
        for event in &events {
            ledger.record(event);
        }
        assert!(ledger.previous_path().exists());
        assert_eq!(
            summarize_events(&ledger.read_events(), "7d", now()).unwrap(),
            summarize_events(&events, "7d", now()).unwrap()
        );
        assert_eq!(summarize(&ledger, "1y"), Err(InvalidWindow));
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
