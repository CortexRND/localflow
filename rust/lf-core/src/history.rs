//! Local SQLite storage for dictation history and retryable audio clips
//! (port of `localflow/history.py`).
//!
//! Database: `~/.localflow/history.sqlite` (schema version 1, identical DDL,
//! so Python and Rust read and write the same rows). Clips: 16 kHz mono
//! 16-bit PCM WAV files in `~/.localflow/clips`, only ever deleted when they
//! resolve inside the clips directory.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;

use crate::symbols::pycompat::{casefold, repr_str, split_ws};

const SCHEMA_VERSION: i64 = 1;
const KINDS: [&str; 3] = ["dictate", "command", "transform"];
const STATUSES: [&str; 3] = ["ok", "failed", "silent"];
const AUDIO_RATE: u32 = 16000;

pub fn history_dir() -> PathBuf {
    crate::home::localflow_dir()
}

pub fn history_path() -> PathBuf {
    history_dir().join("history.sqlite")
}

pub fn clips_dir() -> PathBuf {
    history_dir().join("clips")
}

/// One `dictations` row; field order matches the table (and Python's
/// `dict(row)`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub ts: f64,
    pub kind: String,
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub category: Option<String>,
    pub duration_s: f64,
    pub raw_text: String,
    pub final_text: String,
    pub words: i64,
    pub status: String,
    pub audio_path: Option<String>,
}

impl HistoryEntry {
    fn from_row(row: &Row) -> rusqlite::Result<Self> {
        Ok(HistoryEntry {
            id: row.get("id")?,
            ts: row.get("ts")?,
            kind: row.get("kind")?,
            app_name: row.get("app_name")?,
            bundle_id: row.get("bundle_id")?,
            category: row.get("category")?,
            duration_s: row.get("duration_s")?,
            raw_text: row.get("raw_text")?,
            final_text: row.get("final_text")?,
            words: row.get("words")?,
            status: row.get("status")?,
            audio_path: row.get("audio_path")?,
        })
    }
}

/// Arguments of `HistoryStore.add`; [`NewEntry::new`] gives Python's defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEntry {
    pub kind: String,
    pub app_name: Option<String>,
    pub bundle_id: Option<String>,
    pub category: Option<String>,
    pub duration_s: f64,
    pub raw_text: String,
    pub final_text: String,
    pub status: String,
    pub audio_path: Option<String>,
    pub ts: Option<f64>,
}

impl NewEntry {
    pub fn new(kind: &str) -> Self {
        NewEntry {
            kind: kind.to_string(),
            app_name: None,
            bundle_id: None,
            category: None,
            duration_s: 0.0,
            raw_text: String::new(),
            final_text: String::new(),
            status: "ok".to_string(),
            audio_path: None,
            ts: None,
        }
    }
}

/// Arguments of `HistoryStore.update`; `None` leaves a column unchanged.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntryUpdate {
    pub status: Option<String>,
    pub raw_text: Option<String>,
    pub final_text: Option<String>,
    pub duration_s: Option<f64>,
    pub audio_path: Option<String>,
}

/// `HistoryStore.stats()`; serializes with Python's key order. `top_apps`
/// items serialize as `[name, words]` like Python tuples.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stats {
    pub total_words: i64,
    pub total_dictations: i64,
    pub avg_wpm: f64,
    pub words_today: i64,
    pub words_7d: i64,
    pub current_streak_days: i64,
    pub longest_streak_days: i64,
    pub top_apps: Vec<(String, i64)>,
}

impl Stats {
    fn empty() -> Self {
        Stats {
            total_words: 0,
            total_dictations: 0,
            avg_wpm: 0.0,
            words_today: 0,
            words_7d: 0,
            current_streak_days: 0,
            longest_streak_days: 0,
            top_apps: Vec::new(),
        }
    }
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `datetime.fromtimestamp(ts).date()` in the local timezone.
fn local_date(ts: f64) -> Result<NaiveDate> {
    if !ts.is_finite() {
        bail!("cannot convert float {ts} to a timestamp");
    }
    // datetime._fromtimestamp: split off the fraction, round it to
    // microseconds half-even, carry into the seconds.
    let whole = ts.trunc();
    let mut us = ((ts - whole) * 1e6).round_ties_even();
    let mut secs = whole as i64;
    if us >= 1e6 {
        secs += 1;
        us -= 1e6;
    } else if us < 0.0 {
        secs -= 1;
        us += 1e6;
    }
    let dt = Local
        .timestamp_opt(secs, (us as u32) * 1000)
        .earliest()
        .ok_or_else(|| anyhow!("timestamp out of range: {ts}"))?;
    Ok(dt.date_naive())
}

fn check_kind(kind: &str) -> Result<()> {
    if !KINDS.contains(&kind) {
        bail!("invalid history kind: {}", repr_str(kind));
    }
    Ok(())
}

fn check_status(status: &str) -> Result<()> {
    if !STATUSES.contains(&status) {
        bail!("invalid history status: {}", repr_str(status));
    }
    Ok(())
}

/// `Path.resolve()` (non-strict): absolute, symlinks resolved where the path
/// exists, ".." applied after resolution, missing tail kept lexically.
fn resolve_lenient(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    let mut pending: Vec<std::ffi::OsString> = Vec::new();
    let mut prefix_parts: Vec<Component> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => prefix_parts.push(component),
            other => pending.push(other.as_os_str().to_os_string()),
        }
    }
    for part in prefix_parts {
        resolved.push(part.as_os_str());
    }
    let root = resolved.clone();
    pending.reverse();
    let mut hops = 0;
    while let Some(part) = pending.pop() {
        if part == "." {
            continue;
        }
        if part == ".." {
            if resolved != root {
                resolved.pop();
            }
            continue;
        }
        let candidate = resolved.join(&part);
        match fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    return Err(std::io::Error::other("symlink loop"));
                }
                let target = fs::read_link(&candidate)?;
                if target.is_absolute() {
                    resolved = PathBuf::new();
                    let mut parts = Vec::new();
                    for component in target.components() {
                        match component {
                            Component::Prefix(_) | Component::RootDir => {
                                resolved.push(component.as_os_str())
                            }
                            other => parts.push(other.as_os_str().to_os_string()),
                        }
                    }
                    pending.extend(parts.into_iter().rev());
                } else {
                    let parts: Vec<_> = target
                        .components()
                        .map(|c| c.as_os_str().to_os_string())
                        .collect();
                    pending.extend(parts.into_iter().rev());
                }
            }
            _ => resolved = candidate,
        }
    }
    Ok(resolved)
}

/// Thread-safe local history backed by SQLite and private WAV clips.
pub struct HistoryStore {
    pub path: PathBuf,
    pub clip_dir: PathBuf,
    pub enabled: bool,
    lock: Mutex<()>,
}

impl HistoryStore {
    /// `HistoryStore(path, clip_dir, enabled)`; initializes the schema when
    /// enabled. `None` path -> `~/.localflow/history.sqlite`; `None` clip dir
    /// -> `<db dir>/clips`.
    pub fn open(path: Option<PathBuf>, clip_dir: Option<PathBuf>, enabled: bool) -> Result<Self> {
        let path = path.unwrap_or_else(history_path);
        let clip_dir =
            clip_dir.unwrap_or_else(|| path.parent().unwrap_or(Path::new(".")).join("clips"));
        let store = HistoryStore {
            path,
            clip_dir,
            enabled,
            lock: Mutex::new(()),
        };
        if enabled {
            store.initialize()?;
        }
        Ok(store)
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Open a connection like Python's `_connect` (30 s busy timeout,
    /// 0600 permissions), run `f` in a transaction, commit or roll back.
    fn with_connection<T>(&self, f: impl FnOnce(&rusqlite::Transaction) -> Result<T>) -> Result<T> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let mut connection = Connection::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        connection.busy_timeout(Duration::from_secs(30))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600)).is_err() {
                log::warn!(
                    "could not restrict history database permissions: {}",
                    self.path.display()
                );
            }
        }
        let tx = connection.transaction()?;
        let result = f(&tx)?;
        tx.commit()?;
        Ok(result)
    }

    fn initialize(&self) -> Result<()> {
        let _held = self.guard();
        self.with_connection(|c| {
            let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version > SCHEMA_VERSION {
                bail!("history schema {version} is newer than supported ({SCHEMA_VERSION})");
            }
            c.execute_batch(
                "
                CREATE TABLE IF NOT EXISTS dictations (
                    id INTEGER PRIMARY KEY,
                    ts REAL NOT NULL,
                    kind TEXT NOT NULL CHECK(kind IN ('dictate', 'command', 'transform')),
                    app_name TEXT,
                    bundle_id TEXT,
                    category TEXT,
                    duration_s REAL NOT NULL,
                    raw_text TEXT NOT NULL,
                    final_text TEXT NOT NULL,
                    words INTEGER NOT NULL,
                    status TEXT NOT NULL CHECK(status IN ('ok', 'failed', 'silent')),
                    audio_path TEXT
                )
                ;
                CREATE INDEX IF NOT EXISTS dictations_ts ON dictations(ts DESC, id DESC);

                CREATE TABLE IF NOT EXISTS achievements (
                    milestone INTEGER NOT NULL,
                    day TEXT NOT NULL,
                    PRIMARY KEY (milestone, day)
                )
                ;",
            )?;
            c.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            Ok(())
        })
    }

    pub fn add(&self, entry: &NewEntry) -> Result<Option<i64>> {
        if !self.enabled {
            return Ok(None);
        }
        check_kind(&entry.kind)?;
        check_status(&entry.status)?;
        if entry.duration_s < 0.0 {
            bail!("duration_s must be non-negative");
        }
        let words = split_ws(&entry.final_text).len() as i64;
        let ts = entry.ts.unwrap_or_else(now_ts);
        let _held = self.guard();
        self.with_connection(|c| {
            c.execute(
                "
                INSERT INTO dictations
                    (ts, kind, app_name, bundle_id, category, duration_s,
                     raw_text, final_text, words, status, audio_path)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ",
                params![
                    ts,
                    entry.kind,
                    entry.app_name,
                    entry.bundle_id,
                    entry.category,
                    entry.duration_s,
                    entry.raw_text,
                    entry.final_text,
                    words,
                    entry.status,
                    entry.audio_path,
                ],
            )?;
            Ok(Some(c.last_insert_rowid()))
        })
    }

    pub fn update(&self, entry_id: i64, changes: &EntryUpdate) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        let mut assignments: Vec<&str> = Vec::new();
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        use rusqlite::types::Value as V;
        if let Some(status) = &changes.status {
            check_status(status)?;
            assignments.push("status = ?");
            values.push(V::Text(status.clone()));
        }
        if let Some(raw) = &changes.raw_text {
            assignments.push("raw_text = ?");
            values.push(V::Text(raw.clone()));
        }
        if let Some(text) = &changes.final_text {
            assignments.push("final_text = ?");
            values.push(V::Text(text.clone()));
            assignments.push("words = ?");
            values.push(V::Integer(split_ws(text).len() as i64));
        }
        if let Some(duration) = changes.duration_s {
            if duration < 0.0 {
                bail!("duration_s must be non-negative");
            }
            assignments.push("duration_s = ?");
            values.push(V::Real(duration));
        }
        if let Some(audio) = &changes.audio_path {
            assignments.push("audio_path = ?");
            values.push(V::Text(audio.clone()));
        }
        if assignments.is_empty() {
            return Ok(self.get(entry_id)?.is_some());
        }
        values.push(V::Integer(entry_id));
        let sql = format!(
            "UPDATE dictations SET {} WHERE id = ?",
            assignments.join(", ")
        );
        let _held = self.guard();
        self.with_connection(|c| {
            let changed = c.execute(&sql, rusqlite::params_from_iter(values.iter()))?;
            Ok(changed > 0)
        })
    }

    /// Newest first; `query` filters raw/final text with a literal,
    /// ASCII-case-insensitive LIKE (`%`/`_`/`\` escaped).
    pub fn recent(
        &self,
        limit: i64,
        query: Option<&str>,
        offset: i64,
    ) -> Result<Vec<HistoryEntry>> {
        if !self.enabled {
            return Ok(Vec::new());
        }
        let limit = limit.max(0);
        let offset = offset.max(0);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut sql = String::from("SELECT * FROM dictations");
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(query) = query.filter(|q| !q.is_empty()) {
            let escaped = query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            sql.push_str(" WHERE raw_text LIKE ? ESCAPE '\\' OR final_text LIKE ? ESCAPE '\\'");
            values.push(rusqlite::types::Value::Text(format!("%{escaped}%")));
            values.push(rusqlite::types::Value::Text(format!("%{escaped}%")));
        }
        sql.push_str(" ORDER BY ts DESC, id DESC LIMIT ? OFFSET ?");
        values.push(rusqlite::types::Value::Integer(limit));
        values.push(rusqlite::types::Value::Integer(offset));
        let _held = self.guard();
        self.with_connection(|c| {
            let mut statement = c.prepare(&sql)?;
            let rows = statement
                .query_map(
                    rusqlite::params_from_iter(values.iter()),
                    HistoryEntry::from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn get(&self, entry_id: i64) -> Result<Option<HistoryEntry>> {
        if !self.enabled {
            return Ok(None);
        }
        let _held = self.guard();
        self.with_connection(|c| {
            Ok(c.query_row(
                "SELECT * FROM dictations WHERE id = ?",
                [entry_id],
                HistoryEntry::from_row,
            )
            .optional()?)
        })
    }

    pub fn last_ok(&self) -> Result<Option<HistoryEntry>> {
        if !self.enabled {
            return Ok(None);
        }
        let _held = self.guard();
        self.with_connection(|c| {
            Ok(c.query_row(
                "
                SELECT * FROM dictations
                WHERE status = 'ok' AND final_text != ''
                ORDER BY ts DESC, id DESC LIMIT 1
                ",
                [],
                HistoryEntry::from_row,
            )
            .optional()?)
        })
    }

    /// The newest successful text, or `fallback`.
    pub fn latest_text(&self, fallback: &str) -> Result<String> {
        Ok(match self.last_ok()? {
            Some(row) if !row.final_text.is_empty() => row.final_text,
            _ => fallback.to_string(),
        })
    }

    pub fn delete(&self, entry_id: i64) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        let audio = {
            let _held = self.guard();
            self.with_connection(|c| {
                let row: Option<Option<String>> = c
                    .query_row(
                        "SELECT audio_path FROM dictations WHERE id = ?",
                        [entry_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if row.is_some() {
                    c.execute("DELETE FROM dictations WHERE id = ?", [entry_id])?;
                }
                Ok(row)
            })?
        };
        match audio {
            None => Ok(false),
            Some(path) => {
                self.remove_clip(path.as_deref().map(Path::new));
                Ok(true)
            }
        }
    }

    /// Delete every row and every clip (including orphan `*.wav` files in
    /// the clips directory); returns the number of rows removed.
    pub fn clear(&self) -> Result<i64> {
        if !self.enabled {
            return Ok(0);
        }
        let (count, paths) = {
            let _held = self.guard();
            self.with_connection(|c| {
                let count: i64 =
                    c.query_row("SELECT count(*) FROM dictations", [], |r| r.get(0))?;
                let mut statement =
                    c.prepare("SELECT audio_path FROM dictations WHERE audio_path IS NOT NULL")?;
                let paths = statement
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);
                c.execute("DELETE FROM dictations", [])?;
                Ok((count, paths))
            })?
        };
        for path in &paths {
            self.remove_clip(Some(Path::new(path)));
        }
        if self.clip_dir.exists() {
            if let Ok(entries) = fs::read_dir(&self.clip_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_wav = if cfg!(windows) {
                        name.to_lowercase().ends_with(".wav")
                    } else {
                        name.ends_with(".wav")
                    };
                    if is_wav {
                        self.remove_clip(Some(&entry.path()));
                    }
                }
            }
        }
        Ok(count)
    }

    /// Stats relative to the local day of `now` (unix seconds; None = now).
    pub fn stats(&self, now: Option<f64>) -> Result<Stats> {
        if !self.enabled {
            return Ok(Stats::empty());
        }
        let today = local_date(now.unwrap_or_else(now_ts))?;
        self.stats_on(today)
    }

    /// Stats relative to `today` (Python's datetime argument form).
    pub fn stats_on(&self, today: NaiveDate) -> Result<Stats> {
        if !self.enabled {
            return Ok(Stats::empty());
        }
        let rows: Vec<(f64, Option<String>, f64, i64)> = {
            let _held = self.guard();
            self.with_connection(|c| {
                let mut statement = c.prepare(
                    "
                SELECT ts, app_name, duration_s, words FROM dictations
                WHERE status = 'ok' AND kind = 'dictate'
                ",
                )?;
                let rows = statement
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })?
        };
        let mut dates: HashSet<NaiveDate> = HashSet::new();
        let (mut total_words, mut words_today, mut words_7d) = (0i64, 0i64, 0i64);
        let mut wpm_words = 0i64;
        let mut wpm_seconds = 0f64;
        let mut app_words: Vec<(String, i64)> = Vec::new();
        let week_start = today - chrono::Days::new(6);
        for (ts, app_name, duration_s, words) in &rows {
            let day = local_date(*ts)?;
            dates.insert(day);
            total_words += words;
            if day == today {
                words_today += words;
            }
            if week_start <= day && day <= today {
                words_7d += words;
            }
            if *duration_s >= 1.0 {
                wpm_words += words;
                wpm_seconds += duration_s;
            }
            let name = app_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "Unknown".into());
            match app_words.iter_mut().find(|(n, _)| *n == name) {
                Some(item) => item.1 += words,
                None => app_words.push((name, *words)),
            }
        }
        // sorted(key=(-words, name.casefold())) is stable.
        app_words.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| casefold(&a.0).cmp(&casefold(&b.0)))
        });
        app_words.truncate(5);
        Ok(Stats {
            total_words,
            total_dictations: rows.len() as i64,
            avg_wpm: if wpm_seconds != 0.0 {
                wpm_words as f64 / (wpm_seconds / 60.0)
            } else {
                0.0
            },
            words_today,
            words_7d,
            current_streak_days: current_streak(&dates, today),
            longest_streak_days: longest_streak(&dates),
            top_apps: app_words,
        })
    }

    /// Record a milestone once per day (`day` "YYYY-MM-DD"; None = local today).
    pub fn claim_milestone(&self, milestone: i64, day: Option<&str>) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        let day = match day.filter(|d| !d.is_empty()) {
            Some(d) => d.to_string(),
            None => Local::now().date_naive().format("%Y-%m-%d").to_string(),
        };
        let _held = self.guard();
        self.with_connection(|c| {
            let inserted = c.execute(
                "
                INSERT OR IGNORE INTO achievements (milestone, day) VALUES (?, ?)
                ",
                params![milestone, day],
            )?;
            Ok(inserted > 0)
        })
    }

    /// Write a private 16 kHz mono 16-bit WAV clip (resampled linearly like
    /// `np.interp`) and return its path.
    pub fn write_clip(&self, samples: &[f32], sample_rate: i64) -> Result<PathBuf> {
        if !self.enabled {
            bail!("history is disabled");
        }
        if sample_rate <= 0 {
            bail!("sample_rate must be positive");
        }
        let resampled: Vec<f32>;
        let samples = if sample_rate != AUDIO_RATE as i64 && !samples.is_empty() {
            resampled = resample(samples, sample_rate);
            &resampled[..]
        } else {
            samples
        };
        let mut data = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            let pcm = (s.clamp(-1.0, 1.0) * 32767f32) as i16;
            data.extend_from_slice(&pcm.to_le_bytes());
        }
        fs::create_dir_all(&self.clip_dir)?;
        let target = self
            .clip_dir
            .join(format!("{}.wav", uuid::Uuid::new_v4().simple()));
        let mut handle = tempfile::Builder::new()
            .prefix(".clip-")
            .suffix(".tmp")
            .tempfile_in(&self.clip_dir)?;
        handle.write_all(&wav_bytes(&data))?;
        handle.flush()?;
        handle.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(handle.path(), fs::Permissions::from_mode(0o600))?;
        }
        handle
            .persist(&target)
            .map_err(|e| anyhow::Error::new(e.error))?;
        Ok(target)
    }

    /// Read a clip from inside the clips directory as float samples in
    /// [-1, 1) plus its frame rate.
    pub fn load_clip(&self, value: &Path) -> Result<(Vec<f32>, u32)> {
        if !self.enabled {
            bail!("history is disabled");
        }
        let outside = || anyhow!("audio clip is outside the LocalFlow clips directory");
        let resolved = resolve_lenient(value).map_err(|_| outside())?;
        let root = resolve_lenient(&self.clip_dir).map_err(|_| outside())?;
        if !resolved.starts_with(&root) {
            return Err(outside());
        }
        let bytes = fs::read(&resolved)?;
        let wav = parse_wav(&bytes)?;
        if wav.channels != 1 || wav.sample_width != 2 {
            bail!("audio clip must be mono 16-bit PCM");
        }
        if wav.data.len() % 2 != 0 {
            bail!("buffer size must be a multiple of element size");
        }
        let samples = wav
            .data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
            .collect();
        Ok((samples, wav.frame_rate))
    }

    /// Keep audio only for the newest `keep` ok dictations; returns how many
    /// clips were released. Failed/silent rows keep their clips for retry.
    pub fn prune_audio_clips(&self, keep: i64) -> Result<usize> {
        if !self.enabled {
            return Ok(0);
        }
        let keep = keep.max(0) as usize;
        let expired = {
            let _held = self.guard();
            self.with_connection(|c| {
                let mut statement = c.prepare(
                    "
                SELECT id, audio_path FROM dictations
                WHERE kind = 'dictate' AND status = 'ok' AND audio_path IS NOT NULL
                ORDER BY ts DESC, id DESC
                ",
                )?;
                let rows: Vec<(i64, String)> = statement
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);
                let expired: Vec<(i64, String)> = rows.into_iter().skip(keep).collect();
                for (id, _) in &expired {
                    c.execute("UPDATE dictations SET audio_path = NULL WHERE id = ?", [id])?;
                }
                Ok(expired)
            })?
        };
        for (_, path) in &expired {
            self.remove_clip(Some(Path::new(path)));
        }
        Ok(expired.len())
    }

    fn remove_clip(&self, value: Option<&Path>) {
        let Some(path) = value else { return };
        let (Ok(resolved), Ok(root)) = (resolve_lenient(path), resolve_lenient(&self.clip_dir))
        else {
            return;
        };
        if !resolved.starts_with(&root) {
            return;
        }
        match fs::remove_file(&resolved) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => log::warn!(
                "could not remove audio clip: {} ({err})",
                resolved.display()
            ),
        }
    }

    /// Delete a clip if (and only if) it resolves inside the clips directory.
    pub fn delete_clip(&self, value: Option<&Path>) {
        self.remove_clip(value);
    }
}

fn current_streak(days: &HashSet<NaiveDate>, today: NaiveDate) -> i64 {
    let mut start = if days.contains(&today) {
        today
    } else {
        today - chrono::Days::new(1)
    };
    let mut streak = 0;
    while days.contains(&start) {
        streak += 1;
        start = start - chrono::Days::new(1);
    }
    streak
}

fn longest_streak(days: &HashSet<NaiveDate>) -> i64 {
    let mut sorted: Vec<NaiveDate> = days.iter().copied().collect();
    sorted.sort();
    let (mut longest, mut current) = (0, 0);
    let mut previous: Option<NaiveDate> = None;
    for day in sorted {
        current = match previous {
            Some(p) if p.succ_opt() == Some(day) => current + 1,
            _ => 1,
        };
        longest = i64::max(longest, current);
        previous = Some(day);
    }
    longest
}

/// `np.interp(np.linspace(0, n-1, size), np.arange(n), samples)` in float64,
/// cast back to float32, with `size = max(1, round(n * 16000 / rate))`.
fn resample(samples: &[f32], sample_rate: i64) -> Vec<f32> {
    let n = samples.len();
    let size = ((n as f64 * AUDIO_RATE as f64) / sample_rate as f64).round_ties_even();
    let size = (size as usize).max(1);
    let stop = (n - 1) as f64;
    let positions: Vec<f64> = if size == 1 {
        vec![0.0]
    } else {
        let step = stop / (size - 1) as f64;
        let mut ys: Vec<f64> = (0..size)
            .map(|i| {
                if step == 0.0 {
                    i as f64 / (size - 1) as f64 * stop
                } else {
                    i as f64 * step
                }
            })
            .collect();
        ys[size - 1] = stop;
        ys
    };
    let fp: Vec<f64> = samples.iter().map(|&s| s as f64).collect();
    positions
        .into_iter()
        .map(|x| {
            if n == 1 {
                return fp[0] as f32;
            }
            if x.is_nan() {
                return x as f32;
            }
            if x >= stop {
                return fp[n - 1] as f32;
            }
            let j = x.floor().max(0.0) as usize;
            if x == j as f64 {
                return fp[j] as f32;
            }
            let slope = (fp[j + 1] - fp[j]) / 1.0;
            let mut value = slope * (x - j as f64) + fp[j];
            if value.is_nan() {
                value = slope * (x - (j + 1) as f64) + fp[j + 1];
                if value.is_nan() && fp[j] == fp[j + 1] {
                    value = fp[j];
                }
            }
            value as f32
        })
        .collect()
}

/// The 44-byte RIFF header Python's `wave` module writes for mono 16-bit.
fn wav_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&AUDIO_RATE.to_le_bytes());
    out.extend_from_slice(&(AUDIO_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

struct Wav<'a> {
    channels: u16,
    sample_width: u16,
    frame_rate: u32,
    data: &'a [u8],
}

/// Minimal port of `wave.open(..., "rb")` chunk parsing: RIFF/WAVE header,
/// `fmt ` (PCM or WAVE_FORMAT_EXTENSIBLE PCM) before `data`, word-aligned
/// chunks; `readframes(nframes)` truncates to whole frames.
fn parse_wav(bytes: &[u8]) -> Result<Wav<'_>> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" {
        bail!("file does not start with RIFF id");
    }
    if &bytes[8..12] != b"WAVE" {
        bail!("not a WAVE file");
    }
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32)> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_at(bytes, pos + 4) as usize;
        let body_start = pos + 8;
        let body_end = body_start.saturating_add(size).min(bytes.len());
        let body = &bytes[body_start..body_end];
        if id == b"fmt " {
            if body.len() < 14 {
                bail!("EOF");
            }
            let tag = u16_at(body, 0);
            let channels = u16_at(body, 2);
            let rate = u32_at(body, 4);
            if body.len() < 16 {
                bail!("EOF");
            }
            let bits = u16_at(body, 14);
            let pcm = match tag {
                0x0001 => true,
                0xFFFE => {
                    // cbSize(2) validBits(2) channelMask(4) SubFormat GUID(16)
                    body.len() >= 40 && body[24..26] == [0x01, 0x00]
                }
                _ => false,
            };
            if !pcm {
                bail!("unknown format: {tag}");
            }
            if bits == 0 {
                bail!("bad sample width");
            }
            if channels == 0 {
                bail!("bad # of channels");
            }
            fmt = Some((channels, bits.div_ceil(8), rate));
        } else if id == b"data" {
            let Some((channels, width, rate)) = fmt else {
                bail!("data chunk before fmt chunk");
            };
            let frame = channels as usize * width as usize;
            let frames = size / frame;
            let wanted = (frames * frame).min(body.len());
            return Ok(Wav {
                channels,
                sample_width: width,
                frame_rate: rate,
                data: &body[..wanted],
            });
        }
        pos = body_start.saturating_add(size + (size & 1));
    }
    bail!("fmt chunk and/or data chunk missing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    fn store(dir: &tempfile::TempDir) -> HistoryStore {
        HistoryStore::open(Some(dir.path().join("history.sqlite")), None, true).unwrap()
    }

    fn stamp(day: NaiveDate) -> f64 {
        Local
            .from_local_datetime(&day.and_time(NaiveTime::from_hms_opt(12, 0, 0).unwrap()))
            .earliest()
            .unwrap()
            .timestamp() as f64
    }

    fn entry(kind: &str, status: &str, final_text: &str) -> NewEntry {
        NewEntry {
            status: status.into(),
            final_text: final_text.into(),
            ..NewEntry::new(kind)
        }
    }

    #[test]
    fn schema_crud_search_and_last_ok() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let version: i64 = Connection::open(&s.path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let first = s
            .add(&NewEntry {
                raw_text: "Raw words".into(),
                app_name: Some("Editor".into()),
                duration_s: 12.0,
                ts: Some(1.0),
                ..entry("dictate", "ok", "Clean final words")
            })
            .unwrap()
            .unwrap();
        let failed = s
            .add(&NewEntry {
                raw_text: "Needs retry".into(),
                ts: Some(2.0),
                ..entry("dictate", "failed", "")
            })
            .unwrap()
            .unwrap();
        assert_eq!(s.get(first).unwrap().unwrap().words, 3);
        assert_eq!(s.last_ok().unwrap().unwrap().id, first);
        let ids = |rows: Vec<HistoryEntry>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(s.recent(50, Some("raw"), 0).unwrap()), vec![first]);
        assert_eq!(ids(s.recent(50, Some("retry"), 0).unwrap()), vec![failed]);
        assert_eq!(ids(s.recent(50, Some(""), 0).unwrap()), vec![failed, first]);
        assert!(s.recent(0, None, 0).unwrap().is_empty());
        assert!(s.recent(-5, None, 0).unwrap().is_empty());
        assert!(s.delete(failed).unwrap());
        assert!(s.get(failed).unwrap().is_none());
        assert!(!s.delete(failed).unwrap());
        let json = serde_json::to_string(&s.get(first).unwrap().unwrap()).unwrap();
        assert!(
            json.starts_with("{\"id\":1,\"ts\":1.0,\"kind\":\"dictate\",\"app_name\":\"Editor\"")
        );
    }

    #[test]
    fn validation_errors() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        assert_eq!(
            s.add(&NewEntry::new("bogus")).unwrap_err().to_string(),
            "invalid history kind: 'bogus'"
        );
        assert_eq!(
            s.add(&entry("dictate", "nope", ""))
                .unwrap_err()
                .to_string(),
            "invalid history status: 'nope'"
        );
        assert!(s
            .add(&NewEntry {
                duration_s: -1.0,
                ..NewEntry::new("dictate")
            })
            .is_err());
        let id = s.add(&NewEntry::new("command")).unwrap().unwrap();
        assert!(s
            .update(
                id,
                &EntryUpdate {
                    status: Some("bad".into()),
                    ..Default::default()
                }
            )
            .is_err());
        assert!(s
            .update(
                id,
                &EntryUpdate {
                    duration_s: Some(-1.0),
                    ..Default::default()
                }
            )
            .is_err());
    }

    #[test]
    fn update_changes_columns() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let id = s.add(&entry("dictate", "failed", "")).unwrap().unwrap();
        assert!(s.update(id, &EntryUpdate::default()).unwrap());
        assert!(!s.update(999, &EntryUpdate::default()).unwrap());
        assert!(s
            .update(
                id,
                &EntryUpdate {
                    status: Some("ok".into()),
                    final_text: Some("now  four words here".into()),
                    duration_s: Some(2.5),
                    audio_path: Some("/x.wav".into()),
                    raw_text: Some("raw".into()),
                }
            )
            .unwrap());
        let row = s.get(id).unwrap().unwrap();
        assert_eq!(
            (row.words, row.status.as_str(), row.duration_s),
            (4, "ok", 2.5)
        );
        assert_eq!(row.audio_path.as_deref(), Some("/x.wav"));
        assert!(!s
            .update(
                999,
                &EntryUpdate {
                    raw_text: Some("x".into()),
                    ..Default::default()
                }
            )
            .unwrap());
    }

    #[test]
    fn search_escapes_like_metacharacters_and_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let pct = s
            .add(&NewEntry {
                raw_text: "100% ready".into(),
                ts: Some(3.0),
                ..entry("dictate", "ok", "literal")
            })
            .unwrap()
            .unwrap();
        s.add(&NewEntry {
            raw_text: "100 ready".into(),
            ts: Some(2.0),
            ..entry("dictate", "ok", "plain")
        })
        .unwrap();
        let under = s
            .add(&NewEntry {
                raw_text: "a_b".into(),
                ts: Some(1.0),
                ..entry("dictate", "ok", "underscore")
            })
            .unwrap()
            .unwrap();
        let back = s
            .add(&NewEntry {
                raw_text: "c\\d".into(),
                ts: Some(0.5),
                ..entry("dictate", "ok", "x")
            })
            .unwrap()
            .unwrap();
        let ids = |rows: Vec<HistoryEntry>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(s.recent(50, Some("%"), 0).unwrap()), vec![pct]);
        assert_eq!(ids(s.recent(50, Some("_"), 0).unwrap()), vec![under]);
        assert_eq!(ids(s.recent(50, Some("\\"), 0).unwrap()), vec![back]);
        assert_eq!(ids(s.recent(50, Some("LITERAL"), 0).unwrap()), vec![pct]);
        let all = ids(s.recent(3, None, 0).unwrap());
        assert_eq!(ids(s.recent(2, None, 2).unwrap()), vec![all[2], back]);
        assert_eq!(ids(s.recent(2, None, -3).unwrap()), all[..2].to_vec());
    }

    #[test]
    fn disabled_history_is_inert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let enabled = HistoryStore::open(Some(path.clone()), None, true).unwrap();
        enabled.add(&entry("dictate", "ok", "existing")).unwrap();
        let s = HistoryStore::open(Some(path), None, false).unwrap();
        assert_eq!(s.add(&entry("dictate", "ok", "not saved")).unwrap(), None);
        assert!(s.recent(50, None, 0).unwrap().is_empty());
        assert!(s.last_ok().unwrap().is_none());
        assert!(s.get(1).unwrap().is_none());
        assert_eq!(s.stats(None).unwrap().total_dictations, 0);
        assert!(!s.delete(1).unwrap());
        assert_eq!(s.clear().unwrap(), 0);
        assert!(!s.claim_milestone(7, None).unwrap());
        assert_eq!(s.prune_audio_clips(0).unwrap(), 0);
        assert!(s.write_clip(&[0.0], 16000).is_err());
        assert_eq!(s.latest_text("last file").unwrap(), "last file");
        let fresh = HistoryStore::open(Some(dir.path().join("never.sqlite")), None, false).unwrap();
        assert!(!fresh.path.exists());
    }

    #[test]
    fn latest_text_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.add(&entry("dictate", "ok", "saved")).unwrap();
        assert_eq!(s.latest_text("last file").unwrap(), "saved");
        let empty = HistoryStore::open(Some(dir.path().join("empty.sqlite")), None, true).unwrap();
        assert_eq!(empty.latest_text("last file").unwrap(), "last file");
    }

    #[test]
    fn newer_schema_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 2")
            .unwrap();
        let err = HistoryStore::open(Some(path), None, true).err().unwrap();
        assert_eq!(
            err.to_string(),
            "history schema 2 is newer than supported (1)"
        );
    }

    #[test]
    fn opens_python_created_database_and_old_version_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        // A version-0 database that already has the table (as an older build
        // could leave it) is upgraded in place by stamping user_version.
        let c = Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE dictations (id INTEGER PRIMARY KEY, ts REAL NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('dictate', 'command', 'transform')), app_name TEXT, bundle_id TEXT, category TEXT, duration_s REAL NOT NULL, raw_text TEXT NOT NULL, final_text TEXT NOT NULL, words INTEGER NOT NULL, status TEXT NOT NULL CHECK(status IN ('ok', 'failed', 'silent')), audio_path TEXT);
             INSERT INTO dictations VALUES (5, 1700000000.25, 'dictate', 'App', NULL, 'work', 3, 'r', 'f g', 2, 'ok', NULL);",
        )
        .unwrap();
        drop(c);
        let s = HistoryStore::open(Some(path.clone()), None, true).unwrap();
        let row = s.get(5).unwrap().unwrap();
        assert_eq!(row.ts, 1700000000.25);
        assert_eq!(row.category.as_deref(), Some("work"));
        let version: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn clear_removes_rows_and_audio_clips() {
        let dir = tempfile::tempdir().unwrap();
        let s = HistoryStore::open(
            Some(dir.path().join("history.sqlite")),
            Some(dir.path().join("clips")),
            true,
        )
        .unwrap();
        let clip = s.write_clip(&vec![0.0; 1600], 16000).unwrap();
        s.add(&NewEntry {
            audio_path: Some(clip.to_string_lossy().into()),
            ..entry("dictate", "failed", "")
        })
        .unwrap();
        let orphan = s.clip_dir.join("orphan.wav");
        fs::write(&orphan, b"clip").unwrap();
        let keep = s.clip_dir.join("notes.txt");
        fs::write(&keep, b"x").unwrap();
        assert_eq!(s.clear().unwrap(), 1);
        assert!(s.recent(50, None, 0).unwrap().is_empty());
        assert!(!clip.exists() && !orphan.exists());
        assert!(keep.exists());
        assert_eq!(s.clear().unwrap(), 0);
    }

    #[test]
    fn clip_is_16khz_mono_wav_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let s = HistoryStore::open(
            Some(dir.path().join("history.sqlite")),
            Some(dir.path().join("clips")),
            true,
        )
        .unwrap();
        let samples: Vec<f32> = (0..800).map(|i| -0.5 + i as f32 / 799.0).collect();
        let path = s.write_clip(&samples, 8000).unwrap();
        let bytes = fs::read(&path).unwrap();
        let wav = parse_wav(&bytes).unwrap();
        assert_eq!(
            (wav.channels, wav.frame_rate, wav.data.len() / 2),
            (1, 16000, 1600)
        );
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(bytes.len(), 44 + 3200);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 32 + 4);
        let (loaded, rate) = s.load_clip(&path).unwrap();
        assert_eq!((loaded.len(), rate), (1600, 16000));
        assert!((loaded[0] - (-16383.0 / 32768.0)).abs() < 1e-6);
        assert!((loaded[1599] - (16383.0 / 32768.0)).abs() < 1e-6);
        // empty input writes an empty clip; same-rate input is not resampled
        let empty = s.write_clip(&[], 44100).unwrap();
        assert_eq!(s.load_clip(&empty).unwrap().0.len(), 0);
        let clipped = s.write_clip(&[2.0, -2.0, 0.5], 16000).unwrap();
        let (values, _) = s.load_clip(&clipped).unwrap();
        assert_eq!(
            values,
            vec![32767.0 / 32768.0, -32767.0 / 32768.0, 16383.0 / 32768.0]
        );
        assert!(s.write_clip(&[0.0], 0).is_err());
    }

    #[test]
    fn load_clip_rejects_outside_paths_and_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = HistoryStore::open(
            Some(dir.path().join("history.sqlite")),
            Some(dir.path().join("clips")),
            true,
        )
        .unwrap();
        let outside = dir.path().join("outside.wav");
        fs::write(&outside, wav_bytes(&[0, 0])).unwrap();
        assert!(s
            .load_clip(&outside)
            .unwrap_err()
            .to_string()
            .contains("outside"));
        fs::create_dir_all(&s.clip_dir).unwrap();
        let sneaky = s.clip_dir.join("..").join("outside.wav");
        assert!(s
            .load_clip(&sneaky)
            .unwrap_err()
            .to_string()
            .contains("outside"));
        let garbage = s.clip_dir.join("garbage.wav");
        fs::write(&garbage, b"not a wav").unwrap();
        assert!(s.load_clip(&garbage).is_err());
        assert!(s.load_clip(&s.clip_dir.join("missing.wav")).is_err());
    }

    #[test]
    fn remove_clip_never_deletes_outside_clip_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = HistoryStore::open(
            Some(dir.path().join("history.sqlite")),
            Some(dir.path().join("clips")),
            true,
        )
        .unwrap();
        let outside = dir.path().join("precious.wav");
        fs::write(&outside, b"x").unwrap();
        let id = s
            .add(&NewEntry {
                audio_path: Some(outside.to_string_lossy().into()),
                ..entry("dictate", "ok", "w")
            })
            .unwrap()
            .unwrap();
        assert!(s.delete(id).unwrap());
        assert!(outside.exists());
        fs::create_dir_all(&s.clip_dir).unwrap();
        s.delete_clip(Some(&s.clip_dir.join("..").join("precious.wav")));
        assert!(outside.exists());
        #[cfg(unix)]
        {
            let link = s.clip_dir.join("link.wav");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            s.delete_clip(Some(&link));
            assert!(outside.exists());
            s.clear().unwrap();
            assert!(outside.exists());
        }
        s.delete_clip(None);
        s.delete_clip(Some(Path::new("relative/missing.wav")));
    }

    #[test]
    fn pruning_keeps_latest_ok_clips_and_preserves_failed() {
        let dir = tempfile::tempdir().unwrap();
        let s = HistoryStore::open(
            Some(dir.path().join("history.sqlite")),
            Some(dir.path().join("clips")),
            true,
        )
        .unwrap();
        let mut ok = Vec::new();
        for ts in [1.0, 2.0, 3.0] {
            let clip = s.write_clip(&[0.0; 32], 16000).unwrap();
            s.add(&NewEntry {
                audio_path: Some(clip.to_string_lossy().into()),
                ts: Some(ts),
                ..entry("dictate", "ok", "spoken words")
            })
            .unwrap();
            ok.push(clip);
        }
        let failed_clip = s.write_clip(&[0.0; 32], 16000).unwrap();
        let failed_id = s
            .add(&NewEntry {
                audio_path: Some(failed_clip.to_string_lossy().into()),
                ts: Some(4.0),
                ..entry("dictate", "failed", "")
            })
            .unwrap()
            .unwrap();
        assert_eq!(s.prune_audio_clips(1).unwrap(), 2);
        assert!(!ok[0].exists() && !ok[1].exists() && ok[2].exists());
        assert!(failed_clip.exists());
        assert_eq!(
            s.get(failed_id).unwrap().unwrap().audio_path,
            Some(failed_clip.to_string_lossy().into())
        );
        assert_eq!(s.prune_audio_clips(0).unwrap(), 1);
        assert!(!ok[2].exists() && failed_clip.exists());
        assert_eq!(s.prune_audio_clips(-1).unwrap(), 0);
    }

    #[test]
    fn stats_words_wpm_apps_and_streaks() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let today = Local::now().date_naive();
        let day = |n: u64| today - chrono::Days::new(n);
        let add =
            |kind: &str, status: &str, text: &str, app: Option<&str>, dur: f64, d: NaiveDate| {
                s.add(&NewEntry {
                    app_name: app.map(String::from),
                    duration_s: dur,
                    ts: Some(stamp(d)),
                    ..entry(kind, status, text)
                })
                .unwrap();
            };
        add(
            "dictate",
            "ok",
            "one two three four",
            Some("Editor"),
            0.5,
            today,
        );
        add("dictate", "ok", "five six", Some("Editor"), 60.0, today);
        add(
            "command",
            "ok",
            "seven eight",
            Some("Browser"),
            30.0,
            day(1),
        );
        add("transform", "failed", "excluded", None, 1.0, today);
        add("dictate", "ok", "nine", None, 10.0, day(2));
        add("dictate", "ok", "older", None, 10.0, day(5));
        add("dictate", "ok", "older streak", Some(""), 10.0, day(6));
        let stats = s.stats(Some(stamp(today))).unwrap();
        assert_eq!(stats.total_words, 10);
        assert_eq!(stats.total_dictations, 5);
        assert!((stats.avg_wpm - 4.0).abs() < 1e-9);
        assert_eq!(stats.words_today, 6);
        assert_eq!(stats.words_7d, 10);
        assert_eq!(stats.current_streak_days, 1);
        assert_eq!(stats.longest_streak_days, 2);
        assert_eq!(
            stats.top_apps[..2],
            [("Editor".to_string(), 6), ("Unknown".to_string(), 4)]
        );
        assert_eq!(s.stats_on(today).unwrap(), stats);
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.starts_with("{\"total_words\":10,\"total_dictations\":5,\"avg_wpm\":4.0,\"words_today\":6,\"words_7d\":10,\"current_streak_days\":1,\"longest_streak_days\":2,\"top_apps\":[[\"Editor\",6]"));
    }

    #[test]
    fn stats_empty_and_streak_from_yesterday() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let empty = s.stats(None).unwrap();
        assert_eq!(empty, Stats::empty());
        let today = Local::now().date_naive();
        s.add(&NewEntry {
            duration_s: 1.0,
            ts: Some(stamp(today - chrono::Days::new(1))),
            ..entry("dictate", "ok", "yesterday")
        })
        .unwrap();
        assert_eq!(s.stats(Some(stamp(today))).unwrap().current_streak_days, 1);
        assert_eq!(
            s.stats(Some(stamp(today + chrono::Days::new(1))))
                .unwrap()
                .current_streak_days,
            0
        );
        assert!(s.stats(Some(f64::NAN)).is_err());
    }

    #[test]
    fn top_apps_ties_sort_casefolded_and_cap_five() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        for app in ["b", "A", "c", "D", "e", "f"] {
            s.add(&NewEntry {
                app_name: Some(app.into()),
                ..entry("dictate", "ok", "w")
            })
            .unwrap();
        }
        let top: Vec<String> = s
            .stats(None)
            .unwrap()
            .top_apps
            .into_iter()
            .map(|t| t.0)
            .collect();
        assert_eq!(top, vec!["A", "b", "c", "D", "e"]);
    }

    #[test]
    fn milestone_is_claimed_once_per_day() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        assert!(s.claim_milestone(7, Some("2026-01-01")).unwrap());
        assert!(!s.claim_milestone(7, Some("2026-01-01")).unwrap());
        assert!(s.claim_milestone(7, Some("2026-01-02")).unwrap());
        assert!(s.claim_milestone(8, None).unwrap());
        assert!(!s.claim_milestone(8, None).unwrap());
    }

    #[test]
    fn resample_matches_numpy_interp() {
        assert_eq!(resample(&[1.0], 8000), vec![1.0, 1.0]);
        assert_eq!(
            resample(&[0.0, 1.0], 8000),
            vec![0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0]
        );
        assert_eq!(resample(&[0.0, 1.0, 0.0, 1.0], 64000), vec![0.0]);
    }
}
