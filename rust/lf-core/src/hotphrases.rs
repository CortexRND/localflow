//! User-defined hot phrases: a short spoken trigger expands into a saved long
//! prompt mid-dictation (port of `localflow/hotphrases.py`).
//!
//! Storage is a JSON list at `~/.localflow/hot_phrases.json` written with
//! Python's `json.dumps(entries, indent=2)` layout (ASCII-escaped) to a
//! per-pid temp file and atomically replaced, guarded by a sidecar
//! `hot_phrases.json.lock` flock. An unreadable or missing file reads as an
//! empty list.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{anyhow, Result};
use serde_json::{Map, Value};

use crate::symbols::pycompat::re::{self, Node, Regex};
use crate::symbols::pycompat::{
    chars, file_signature, is_alnum, is_space, json_dumps, lower, py_type_name, repr_str,
    sibling_with_extra, strip, truthy,
};

pub const MAX_TRIGGER_CHARS: usize = 80;
pub const MAX_TRIGGER_WORDS: usize = 8;
pub const MAX_TEXT_CHARS: usize = 20000;

/// A stored entry; insertion order of keys is preserved.
pub type Entry = Map<String, Value>;
/// (trigger, expansion) pairs.
pub type Phrases = [(String, String)];

/// `~/.localflow/hot_phrases.json`.
pub fn hot_phrases_path() -> PathBuf {
    crate::home::localflow_dir().join("hot_phrases.json")
}

/// Invalid trigger or expansion text; `conflict` marks Python's
/// `HotPhraseConflict` (a duplicate trigger after normalization).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotPhraseError {
    pub conflict: bool,
    pub message: String,
}

impl fmt::Display for HotPhraseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HotPhraseError {}

fn invalid(message: String) -> anyhow::Error {
    anyhow::Error::new(HotPhraseError {
        conflict: false,
        message,
    })
}

/// Lowercase words separated by single spaces; punctuation and underscore
/// are dropped, so "Review, checklist" == "review checklist".
pub fn normalize_trigger(s: &str) -> String {
    let lowered = lower(s);
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in lowered.chars() {
        if is_alnum(c) {
            current.push(c);
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words.join(" ")
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn validate_trigger(trigger: &str) -> Result<String> {
    let trigger = strip(trigger);
    let length = trigger.chars().count();
    if length > MAX_TRIGGER_CHARS {
        return Err(invalid(format!(
            "trigger is too long ({length} chars, max {MAX_TRIGGER_CHARS})"
        )));
    }
    let normalized = normalize_trigger(trigger);
    if normalized.is_empty() {
        return Err(invalid("trigger has no usable words".into()));
    }
    let words = normalized.split(' ').count();
    if words > MAX_TRIGGER_WORDS {
        return Err(invalid(format!(
            "trigger is too long ({words} words, max {MAX_TRIGGER_WORDS})"
        )));
    }
    Ok(trigger.to_string())
}

fn validate_text(text: &str) -> Result<String> {
    if strip(text).is_empty() {
        return Err(invalid("expansion text is empty".into()));
    }
    let length = text.chars().count();
    if length > MAX_TEXT_CHARS {
        return Err(invalid(format!(
            "expansion text is too long ({length} chars, max {MAX_TEXT_CHARS})"
        )));
    }
    Ok(text.to_string()) // verbatim: newlines and indentation matter in prompts
}

/// Python would raise AttributeError/TypeError on a non-object list item or
/// a non-string trigger; surface those as errors instead of guessing.
fn as_entry(value: &Value) -> Result<&Entry> {
    value
        .as_object()
        .ok_or_else(|| anyhow!("'{}' object has no attribute 'get'", py_type_name(value)))
}

fn trigger_of(entry: &Entry) -> Result<String> {
    match entry.get("trigger") {
        None => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(anyhow!(
            "'{}' object has no attribute 'lower'",
            py_type_name(other)
        )),
    }
}

fn id_matches(entry: &Entry, id: Option<&str>) -> bool {
    match (entry.get("id"), id) {
        (Some(Value::String(s)), Some(id)) => s == id,
        (None, None) | (Some(Value::Null), None) => true,
        _ => false,
    }
}

type Signature = (i128, u64);

/// A JSON list of hot-phrase entries shared across processes.
pub struct HotPhraseStore {
    path: PathBuf,
    lock_path: PathBuf,
    lock: Mutex<()>,
    enabled_cache: Mutex<Option<EnabledCache>>,
}

type EnabledCache = (Signature, Vec<(String, String)>);

impl HotPhraseStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = sibling_with_extra(&path, ".lock");
        HotPhraseStore {
            path,
            lock_path,
            lock: Mutex::new(()),
            enabled_cache: Mutex::new(None),
        }
    }

    /// The store at `~/.localflow/hot_phrases.json`.
    pub fn default_store() -> Self {
        Self::new(hot_phrases_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn exclusive(&self) -> (MutexGuard<'_, ()>, crate::symbols::pycompat::SidecarLock) {
        let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let parent = self.path.parent().unwrap_or(Path::new("."));
        let sidecar =
            crate::symbols::pycompat::SidecarLock::acquire(&self.lock_path, parent, "hot phrase");
        (guard, sidecar)
    }

    fn read(&self) -> Vec<Value> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                log::error!(
                    "hot phrases unreadable; treating as empty: {} ({err})",
                    self.path.display()
                );
                return Vec::new();
            }
        };
        let parsed = std::str::from_utf8(&raw)
            .map_err(anyhow::Error::from)
            .and_then(|t| serde_json::from_str::<Value>(t).map_err(anyhow::Error::from));
        match parsed {
            Ok(Value::Array(items)) => items,
            Ok(_) => Vec::new(),
            Err(err) => {
                log::error!(
                    "hot phrases unreadable; treating as empty: {} ({err})",
                    self.path.display()
                );
                Vec::new()
            }
        }
    }

    fn write(&self, entries: &[Value]) -> Result<()> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        // Per-pid temp name, as in Python.
        let tmp = sibling_with_extra(&self.path, &format!(".{}.tmp", std::process::id()));
        fs::write(
            &tmp,
            json_dumps(&Value::Array(entries.to_vec()), Some(2), true),
        )?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn check_conflict(entries: &[Value], normalized: &str, exclude_id: Option<&str>) -> Result<()> {
        for value in entries {
            let entry = as_entry(value)?;
            if id_matches(entry, exclude_id) {
                continue;
            }
            if normalize_trigger(&trigger_of(entry)?) == normalized {
                let shown = match entry.get("trigger") {
                    Some(Value::String(s)) => repr_str(s),
                    _ => "None".to_string(),
                };
                return Err(anyhow::Error::new(HotPhraseError {
                    conflict: true,
                    message: format!("trigger duplicates existing hot phrase {shown}"),
                }));
            }
        }
        Ok(())
    }

    /// The raw stored list (non-object items included, as Python returns it).
    pub fn list(&self) -> Vec<Value> {
        let _held = self.exclusive();
        self.read()
    }

    pub fn add(&self, trigger: &str, text: &str, enabled: bool) -> Result<Entry> {
        let trigger = validate_trigger(trigger)?;
        let text = validate_text(text)?;
        let normalized = normalize_trigger(&trigger);
        let _held = self.exclusive();
        let mut entries = self.read();
        Self::check_conflict(&entries, &normalized, None)?;
        let stamp = now();
        let mut id_bytes = [0u8; 4];
        getrandom::fill(&mut id_bytes).map_err(|e| anyhow!("random id: {e}"))?;
        let id: String = id_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut entry = Entry::new();
        entry.insert("id".into(), Value::String(id));
        entry.insert("trigger".into(), Value::String(trigger));
        entry.insert("text".into(), Value::String(text));
        entry.insert("enabled".into(), Value::Bool(enabled));
        entry.insert("created_at".into(), Value::String(stamp.clone()));
        entry.insert("updated_at".into(), Value::String(stamp));
        entries.push(Value::Object(entry.clone()));
        self.write(&entries)?;
        Ok(entry)
    }

    /// Update the first entry with `entry_id`; `Ok(None)` if there is none.
    pub fn update(
        &self,
        entry_id: &str,
        trigger: Option<&str>,
        text: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<Option<Entry>> {
        let _held = self.exclusive();
        let mut entries = self.read();
        for index in 0..entries.len() {
            if !id_matches(as_entry(&entries[index])?, Some(entry_id)) {
                continue;
            }
            let mut entry = as_entry(&entries[index])?.clone();
            if let Some(trigger) = trigger {
                let trigger = validate_trigger(trigger)?;
                Self::check_conflict(&entries, &normalize_trigger(&trigger), Some(entry_id))?;
                entry.insert("trigger".into(), Value::String(trigger));
            }
            if let Some(text) = text {
                entry.insert("text".into(), Value::String(validate_text(text)?));
            }
            if let Some(enabled) = enabled {
                entry.insert("enabled".into(), Value::Bool(enabled));
            }
            entry.insert("updated_at".into(), Value::String(now()));
            entries[index] = Value::Object(entry.clone());
            self.write(&entries)?;
            return Ok(Some(entry));
        }
        Ok(None)
    }

    pub fn delete(&self, entry_id: &str) -> Result<bool> {
        let _held = self.exclusive();
        let entries = self.read();
        let mut remaining = Vec::with_capacity(entries.len());
        for value in &entries {
            if !id_matches(as_entry(value)?, Some(entry_id)) {
                remaining.push(value.clone());
            }
        }
        if remaining.len() == entries.len() {
            return Ok(false);
        }
        self.write(&remaining)?;
        Ok(true)
    }

    /// (trigger, expansion) pairs for enabled entries, re-read only when the
    /// file's (mtime_ns, size) changes.
    pub fn enabled(&self) -> Result<Vec<(String, String)>> {
        let signature = match file_signature(&self.path) {
            Ok(sig) => sig,
            Err(_) => {
                *self.enabled_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Ok(Vec::new());
            }
        };
        if let Some((cached, pairs)) =
            &*self.enabled_cache.lock().unwrap_or_else(|e| e.into_inner())
        {
            if *cached == signature {
                return Ok(pairs.clone());
            }
        }
        let entries = {
            let _held = self.exclusive();
            self.read()
        };
        let mut pairs = Vec::new();
        for value in &entries {
            let entry = as_entry(value)?;
            let field = |key: &str| entry.get(key).filter(|v| truthy(v));
            if field("enabled").is_none() {
                continue;
            }
            let (Some(trigger), Some(text)) = (field("trigger"), field("text")) else {
                continue;
            };
            // Python passes non-string values through and fails later in
            // matching; fail here instead of guessing a conversion.
            let (Value::String(trigger), Value::String(text)) = (trigger, text) else {
                return Err(anyhow!(
                    "hot phrase trigger and text must be strings in {}",
                    self.path.display()
                ));
            };
            pairs.push((trigger.clone(), text.clone()));
        }
        *self.enabled_cache.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((signature, pairs.clone()));
        Ok(pairs)
    }
}

/// Whole-utterance match: "Review checklist." expands verbatim.
pub fn match_whole(text: &str, phrases: &Phrases) -> Option<String> {
    let normalized = normalize_trigger(text);
    if normalized.is_empty() {
        return None;
    }
    phrases
        .iter()
        .find(|(trigger, _)| normalize_trigger(trigger) == normalized)
        .map(|(_, expansion)| expansion.clone())
}

/// `\x01(\d+)\x01` — distinct from symbols.rs's `\x00` placeholders.
static PARK: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| crate::symbols::placeholder_pattern('\u{1}'));

/// One IGNORECASE matcher over all triggers, longest first (word count, then
/// length). Group `i` (1-based) is the i-th pair.
fn trigger_regex(phrases: &Phrases) -> Option<(Regex, Vec<(String, String)>)> {
    let mut ordered: Vec<&(String, String)> = phrases.iter().collect();
    ordered.sort_by_key(|(trigger, _)| {
        let normalized = normalize_trigger(trigger);
        let words = if normalized.is_empty() {
            0
        } else {
            normalized.split(' ').count()
        };
        (
            std::cmp::Reverse(words),
            std::cmp::Reverse(normalized.chars().count()),
        )
    });
    let mut alternatives: Vec<Node> = Vec::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (trigger, expansion) in ordered {
        let normalized = normalize_trigger(trigger);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        let mut parts = Vec::new();
        for (i, word) in normalized.split(' ').enumerate() {
            if i > 0 {
                parts.push(re::plus(re::non_alnum()));
            }
            parts.push(re::lit_ci(word));
        }
        alternatives.push(re::group(pairs.len() + 1, re::seq(parts)));
        pairs.push((trigger.clone(), expansion.clone()));
    }
    if alternatives.is_empty() {
        return None;
    }
    let groups = pairs.len();
    let regex = Regex::new(
        re::seq(vec![
            re::not_after(is_alnum),
            re::alt(alternatives),
            re::not_ahead(re::alnum()),
            re::opt(re::seq(vec![
                re::plus(re::set(".,!?;:")),
                re::ahead(re::seq(vec![re::star(re::pred(is_space)), re::dollar()])),
            ])),
        ]),
        groups,
    );
    Some((regex, pairs))
}

fn matched_index(m: &re::Match, count: usize) -> usize {
    (1..=count)
        .find(|&i| m.span(i).is_some())
        .map(|i| i - 1)
        .unwrap_or(0)
}

/// Replace inline trigger matches with `\x01N\x01` placeholders; returns the
/// parked text and the expansions by N (literal, never re-scanned).
pub fn park_hot_phrases(text: &str, phrases: &Phrases) -> (String, Vec<String>) {
    let Some((regex, pairs)) = trigger_regex(phrases) else {
        return (text.to_string(), Vec::new());
    };
    if text.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut parked: Vec<String> = Vec::new();
    let out = regex.sub(text, |m, _| {
        parked.push(pairs[matched_index(m, pairs.len())].1.clone());
        format!("\u{1}{}\u{1}", parked.len() - 1)
    });
    (out, parked)
}

pub fn restore_hot_phrases(text: &str, parked: &[String]) -> String {
    if parked.is_empty() {
        return text.to_string();
    }
    crate::symbols::restore_placeholders(&PARK, text, parked)
}

/// The triggers that matched in `text`: a whole-utterance match, or the
/// distinct triggers hit by inline matches, in match order.
pub fn matched_triggers(text: &str, phrases: &Phrases) -> Vec<String> {
    if phrases.is_empty() || text.is_empty() {
        return Vec::new();
    }
    let whole = normalize_trigger(text);
    for (trigger, _) in phrases {
        if normalize_trigger(trigger) == whole {
            return vec![trigger.clone()];
        }
    }
    let Some((regex, pairs)) = trigger_regex(phrases) else {
        return Vec::new();
    };
    let mut matched: Vec<String> = Vec::new();
    for m in regex.find_iter(&chars(text)) {
        let trigger = &pairs[matched_index(&m, pairs.len())].0;
        if !matched.contains(trigger) {
            matched.push(trigger.clone());
        }
    }
    matched
}

/// Whole-utterance match -> verbatim expansion; otherwise inline.
pub fn expand_hot_phrases(text: &str, phrases: &Phrases) -> String {
    if phrases.is_empty() || text.is_empty() {
        return text.to_string();
    }
    if let Some(whole) = match_whole(text, phrases) {
        return whole;
    }
    let (parked_text, parked) = park_hot_phrases(text, phrases);
    restore_hot_phrases(&parked_text, &parked)
}

/// Optional pipeline stages for [`render_dictation`].
#[derive(Default)]
pub struct RenderSteps<'a> {
    pub replace: Option<&'a dyn Fn(&str) -> String>,
    pub fmt: Option<&'a dyn Fn(&str) -> String>,
    pub clean: Option<&'a dyn Fn(&str) -> String>,
    pub symbols: Option<&'a dyn Fn(&str) -> String>,
    pub style: Option<&'a dyn Fn(&str) -> String>,
}

/// Shared text pipeline: a whole-utterance trigger expands verbatim and skips
/// every stage; otherwise replacement, formatting, cleanup, (park), symbols,
/// style, then parked phrases are restored.
pub fn render_dictation(text: &str, phrases: &Phrases, steps: &RenderSteps) -> String {
    if let Some(whole) = match_whole(text, phrases) {
        return whole;
    }
    let mut text = text.to_string();
    for stage in [steps.replace, steps.fmt, steps.clean]
        .into_iter()
        .flatten()
    {
        text = stage(&text);
    }
    let (mut text, parked) = park_hot_phrases(&text, phrases);
    for stage in [steps.symbols, steps.style].into_iter().flatten() {
        text = stage(&text);
    }
    restore_hot_phrases(&text, &parked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formatting::apply_style;
    use crate::symbols::apply_spoken_symbols;
    use std::cell::RefCell;

    fn store(dir: &tempfile::TempDir) -> HotPhraseStore {
        HotPhraseStore::new(dir.path().join("hot_phrases.json"))
    }

    fn phrases() -> Vec<(String, String)> {
        vec![
            (
                "review checklist".into(),
                "CHECK: tests, lint, types".into(),
            ),
            ("pr review".into(), "short pr review".into()),
            ("pr review full".into(), "FULL PR REVIEW".into()),
        ]
    }

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn err_of(e: &anyhow::Error) -> &HotPhraseError {
        e.downcast_ref::<HotPhraseError>().expect("HotPhraseError")
    }

    #[test]
    fn add_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let entry = s
            .add("Review Checklist", "line one\n  line two", true)
            .unwrap();
        assert_eq!(entry["trigger"], "Review Checklist");
        assert_eq!(entry["text"], "line one\n  line two");
        assert_eq!(entry["enabled"], true);
        let id = entry["id"].as_str().unwrap();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        let keys: Vec<&str> = entry.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "trigger",
                "text",
                "enabled",
                "created_at",
                "updated_at"
            ]
        );
        assert_eq!(entry["created_at"].as_str().unwrap().len(), 19);
        let listed = s.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], entry["id"]);
    }

    #[test]
    fn add_strips_trigger_but_not_text() {
        let dir = tempfile::tempdir().unwrap();
        let entry = store(&dir)
            .add("  spaced trigger  ", "  keep\n  this \n", true)
            .unwrap();
        assert_eq!(entry["trigger"], "spaced trigger");
        assert_eq!(entry["text"], "  keep\n  this \n");
    }

    #[test]
    fn update_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let entry = s.add("one", "first", true).unwrap();
        let id = entry["id"].as_str().unwrap();
        let updated = s
            .update(id, None, Some("second"), Some(false))
            .unwrap()
            .unwrap();
        assert_eq!(updated["text"], "second");
        assert_eq!(updated["enabled"], false);
        assert!(updated["updated_at"].as_str() >= entry["created_at"].as_str());
        assert!(s
            .update("nosuchid", None, Some("x"), None)
            .unwrap()
            .is_none());
        let same = s
            .update(id, Some("One!"), Some("y"), None)
            .unwrap()
            .unwrap();
        assert_eq!(same["trigger"], "One!");
        assert!(s.delete(id).unwrap());
        assert!(s.list().is_empty());
        assert!(!s.delete(id).unwrap());
    }

    #[test]
    fn validation() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        for bad in [
            "!!!",
            "   ",
            &"x".repeat(81),
            "one two three four five six seven eight nine",
        ] {
            let e = s.add(bad, "x", true).unwrap_err();
            assert!(!err_of(&e).conflict, "{bad}");
        }
        assert_eq!(
            err_of(&s.add(&"x".repeat(81), "x", true).unwrap_err()).message,
            "trigger is too long (81 chars, max 80)"
        );
        assert_eq!(
            err_of(&s.add("a b c d e f g h i", "x", true).unwrap_err()).message,
            "trigger is too long (9 words, max 8)"
        );
        for bad in ["", "   \n  ", &"x".repeat(20001)] {
            assert!(s.add("ok", bad, true).is_err());
        }
        assert!(s.add("ok", &"é".repeat(20000), true).is_ok());
        assert!(!s.path().with_file_name("missing").exists());
    }

    #[test]
    fn duplicate_trigger_conflicts_via_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.add("Review Checklist", "x", true).unwrap();
        let e = s.add("review, checklist", "y", true).unwrap_err();
        assert!(err_of(&e).conflict);
        assert_eq!(
            err_of(&e).message,
            "trigger duplicates existing hot phrase 'Review Checklist'"
        );
        let other = s.add("other", "z", true).unwrap();
        let e = s
            .update(
                other["id"].as_str().unwrap(),
                Some("review  checklist"),
                None,
                None,
            )
            .unwrap_err();
        assert!(err_of(&e).conflict);
    }

    #[test]
    fn enabled_and_cross_instance_reload() {
        let dir = tempfile::tempdir().unwrap();
        let a = store(&dir);
        let b = store(&dir);
        assert!(b.enabled().unwrap().is_empty());
        a.add("on", "yes", true).unwrap();
        a.add("off", "no", false).unwrap();
        let toggled = a.add("toggled", "maybe", false).unwrap();
        a.update(toggled["id"].as_str().unwrap(), None, None, Some(true))
            .unwrap();
        assert_eq!(
            b.enabled().unwrap(),
            p(&[("on", "yes"), ("toggled", "maybe")])
        );
        a.update(
            toggled["id"].as_str().unwrap(),
            None,
            Some("updated runbook"),
            None,
        )
        .unwrap();
        assert_eq!(b.enabled().unwrap()[1].1, "updated runbook");
        for item in a.list() {
            a.delete(item["id"].as_str().unwrap()).unwrap();
        }
        assert!(b.enabled().unwrap().is_empty());
    }

    #[test]
    fn unreadable_and_foreign_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        fs::write(s.path(), "not json {{{").unwrap();
        assert!(s.list().is_empty());
        assert!(s.enabled().unwrap().is_empty());
        fs::write(s.path(), r#"{"not": "a list"}"#).unwrap();
        assert!(s.list().is_empty());
        // Unknown keys and key order survive a rewrite.
        fs::write(
            s.path(),
            r#"[{"zeta": 1.5, "id": "abc", "trigger": "keep me", "text": "t", "enabled": 1}]"#,
        )
        .unwrap();
        assert_eq!(s.enabled().unwrap(), p(&[("keep me", "t")]));
        s.add("new one", "café ☕", true).unwrap();
        let raw = fs::read_to_string(s.path()).unwrap();
        assert!(raw.starts_with("[\n  {\n    \"zeta\": 1.5,\n    \"id\": \"abc\""));
        assert!(raw.contains("\"caf\\u00e9 \\u2615\""));
        // Python raises on non-object items; so do we (no silent rewrite).
        fs::write(s.path(), r#"[3]"#).unwrap();
        assert!(s.add("x", "y", true).is_err());
        assert!(s.enabled().is_err());
        assert_eq!(fs::read_to_string(s.path()).unwrap(), "[3]");
    }

    #[test]
    fn written_file_is_json_list() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.add("one", "x", true).unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(s.path()).unwrap()).unwrap();
        assert_eq!(data[0]["trigger"], "one");
        assert!(dir.path().join("hot_phrases.json.lock").exists());
    }

    #[test]
    fn matching() {
        let ph = phrases();
        assert_eq!(
            match_whole("Review checklist.", &ph).as_deref(),
            Some("CHECK: tests, lint, types")
        );
        assert_eq!(match_whole("no match here", &ph), None);
        assert_eq!(match_whole("", &ph), None);
        assert_eq!(
            expand_hot_phrases("Review checklist.", &ph),
            "CHECK: tests, lint, types"
        );
        assert_eq!(
            expand_hot_phrases("please review checklist then commit", &ph),
            "please CHECK: tests, lint, types then commit"
        );
        assert_eq!(
            expand_hot_phrases("ship it, review checklist.", &ph),
            "ship it, CHECK: tests, lint, types"
        );
        assert_eq!(
            expand_hot_phrases("review checklist, then commit", &ph),
            "CHECK: tests, lint, types, then commit"
        );
        assert_eq!(
            expand_hot_phrases("do a pr review full please", &ph),
            "do a FULL PR REVIEW please"
        );
        assert_eq!(
            expand_hot_phrases("do a pr review please", &ph),
            "do a short pr review please"
        );
        assert_eq!(
            expand_hot_phrases("reviews checklist", &ph),
            "reviews checklist"
        );
        assert_eq!(
            expand_hot_phrases("pre review checklist", &ph),
            "pre CHECK: tests, lint, types"
        );
        assert_eq!(
            expand_hot_phrases("please review-checklist now", &ph),
            "please CHECK: tests, lint, types now"
        );
        assert_eq!(
            expand_hot_phrases("please review, checklist now", &ph),
            "please CHECK: tests, lint, types now"
        );
        assert_eq!(
            expand_hot_phrases("x_review checklist", &ph),
            "x_CHECK: tests, lint, types"
        );
    }

    #[test]
    fn expansions_are_literal_and_unicode_safe() {
        let ph = p(&[("regex note", r"use \1 and \\ in the pattern")]);
        assert_eq!(
            expand_hot_phrases("regex note", &ph),
            r"use \1 and \\ in the pattern"
        );
        assert_eq!(
            expand_hot_phrases("a regex note", &ph),
            r"a use \1 and \\ in the pattern"
        );
        let ph = p(&[("standup", "yesterday:\ntoday:\nblockers:")]);
        assert_eq!(
            expand_hot_phrases("write my standup please", &ph),
            "write my yesterday:\ntoday:\nblockers: please"
        );
        let ph = p(&[("i", "EXP")]);
        assert_eq!(expand_hot_phrases("use ı now", &ph), "use EXP now"); // Python IGNORECASE
        assert_eq!(expand_hot_phrases("use i now", &ph), "use EXP now");
        let ph = p(&[("café notes", "MENU")]);
        assert_eq!(
            expand_hot_phrases("open café notes please", &ph),
            "open MENU please"
        );
        assert_eq!(expand_hot_phrases("Café Notes.", &ph), "MENU");
        assert_eq!(expand_hot_phrases("cafés notes", &ph), "cafés notes");
        let empty: Vec<(String, String)> = Vec::new();
        assert_eq!(
            expand_hot_phrases("review checklist", &empty),
            "review checklist"
        );
        assert_eq!(expand_hot_phrases("", &phrases()), "");
        assert_eq!(normalize_trigger("Review, checklist"), "review checklist");
        assert_eq!(normalize_trigger("a_b  C"), "a b c");
        assert_eq!(normalize_trigger("!!!"), "");
    }

    #[test]
    fn park_and_restore() {
        let (text, parked) = park_hot_phrases("a review checklist b", &phrases());
        assert_eq!(text, "a \u{1}0\u{1} b");
        assert_eq!(parked, vec!["CHECK: tests, lint, types"]);
        assert_eq!(
            restore_hot_phrases(&text, &parked),
            "a CHECK: tests, lint, types b"
        );
        assert_eq!(
            restore_hot_phrases("x \u{1}9\u{1}", &parked),
            "x \u{1}9\u{1}"
        );
        assert_eq!(restore_hot_phrases("x", &[]), "x");
        assert_eq!(park_hot_phrases("", &phrases()), (String::new(), vec![]));
    }

    #[test]
    fn matched_triggers_order_and_whole() {
        let ph = phrases();
        assert_eq!(
            matched_triggers("Review checklist!", &ph),
            vec!["review checklist"]
        );
        assert_eq!(
            matched_triggers("pr review then review checklist and pr review", &ph),
            vec!["pr review", "review checklist"]
        );
        assert!(matched_triggers("nothing", &ph).is_empty());
        assert!(matched_triggers("", &ph).is_empty());
        let odd = p(&[("!!!", "x")]);
        assert_eq!(matched_triggers("???", &odd), vec!["!!!"]);
    }

    #[test]
    fn render_dictation_symbol_word_trigger() {
        let ph = p(&[("review dash checklist", "use slash here")]);
        let none: [&str; 0] = [];
        let symbols = |t: &str| apply_spoken_symbols(t, &none);
        let steps = RenderSteps {
            symbols: Some(&symbols),
            ..Default::default()
        };
        assert_eq!(
            render_dictation("please review dash checklist now", &ph, &steps),
            "please use slash here now"
        );
        assert_eq!(
            render_dictation("a dash b review dash checklist", &ph, &steps),
            "a-b use slash here"
        );
        let clean = |t: &str| format!("{t}!");
        let steps = RenderSteps {
            clean: Some(&clean),
            ..Default::default()
        };
        assert_eq!(
            render_dictation("review dash checklist.", &ph, &steps),
            "use slash here"
        );
    }

    #[test]
    fn render_dictation_pipeline_order_and_whole_match_bypass() {
        let calls = RefCell::new(Vec::new());
        let stage = |name: &'static str| {
            let calls = &calls;
            move |t: &str| {
                calls.borrow_mut().push(name);
                format!("{t} {name}")
            }
        };
        let (r, f, c, s, st) = (
            stage("replace"),
            stage("format"),
            stage("clean"),
            stage("symbols"),
            stage("style"),
        );
        let steps = RenderSteps {
            replace: Some(&r),
            fmt: Some(&f),
            clean: Some(&c),
            symbols: Some(&s),
            style: Some(&st),
        };
        let ph = p(&[("ship it", "DEPLOY")]);
        assert_eq!(
            render_dictation("please ship it", &ph, &steps),
            "please DEPLOY replace format clean symbols style"
        );
        assert_eq!(
            *calls.borrow(),
            vec!["replace", "format", "clean", "symbols", "style"]
        );
        calls.borrow_mut().clear();
        assert_eq!(render_dictation("ship it.", &ph, &steps), "DEPLOY");
        assert!(calls.borrow().is_empty());
        let empty: [&str; 0] = [];
        let style = |t: &str| apply_style(t, "very_casual", &empty);
        let steps = RenderSteps {
            style: Some(&style),
            ..Default::default()
        };
        assert_eq!(render_dictation("Say ship it.", &ph, &steps), "say DEPLOY");
    }

    #[test]
    fn enabled_pairs_feed_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.add("ship it", "DEPLOY NOW", true).unwrap();
        s.add("skip me", "NOPE", false).unwrap();
        let ph = s.enabled().unwrap();
        assert_eq!(expand_hot_phrases("go ship it", &ph), "go DEPLOY NOW");
        assert_eq!(expand_hot_phrases("go skip me", &ph), "go skip me");
    }
}
