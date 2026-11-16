//! Personal vocabulary and transcription corrections (port of
//! `localflow/dictionary.py`).
//!
//! Storage: `~/.localflow/dictionary.json`, `{"words": [...], "replacements":
//! [{"from": ..., "to": ...}]}` written with `json.dump(..., ensure_ascii=False,
//! indent=2)` layout via a private temp file + atomic replace, guarded by a
//! sidecar `dictionary.json.lock` (flock on Unix, LockFileEx on Windows).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use crate::symbols::pycompat::re::{self, Regex};
use crate::symbols::pycompat::{
    casefold, chars, file_signature, is_word, json_dumps, path_suffix, sibling_with_extra, strip,
    SidecarLock,
};

pub const MAX_IMPORT_BYTES: usize = 3 * 1024 * 1024;
pub const MAX_IMPORT_ENTRIES: usize = 1000;
pub const MAX_PROMPT_CHARS: usize = 800;

/// `~/.localflow/dictionary.json`.
pub fn dictionary_path() -> PathBuf {
    crate::home::localflow_dir().join("dictionary.json")
}

/// Invalid dictionary entry or import file (Python `DictionaryError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryError(pub String);

impl fmt::Display for DictionaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DictionaryError {}

fn derr(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(DictionaryError(message.into()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DictionaryData {
    pub words: Vec<String>,
    pub replacements: Vec<Replacement>,
}

impl DictionaryData {
    fn to_json(&self) -> Value {
        json!({
            "words": self.words,
            "replacements": self.replacements.iter()
                .map(|r| json!({"from": r.from, "to": r.to}))
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ImportCounts {
    pub added: usize,
    pub skipped_duplicate: usize,
    pub skipped_invalid: usize,
}

/// `_word`: stripped non-empty string or None.
fn word_value(value: &str) -> Option<String> {
    let stripped = strip(value);
    (!stripped.is_empty()).then(|| stripped.to_string())
}

fn json_word(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => word_value(s),
        _ => None,
    }
}

fn normalize_data(data: &Value) -> Result<DictionaryData> {
    let Value::Object(map) = data else {
        return Err(derr("dictionary JSON must be an object"));
    };
    let empty = Value::Array(Vec::new());
    let (Value::Array(raw_words), Value::Array(raw_replacements)) = (
        map.get("words").unwrap_or(&empty),
        map.get("replacements").unwrap_or(&empty),
    ) else {
        return Err(derr("dictionary needs words and replacements lists"));
    };
    let mut out = DictionaryData::default();
    let mut seen = HashSet::new();
    for value in raw_words {
        if let Some(word) = json_word(Some(value)) {
            if seen.insert(casefold(&word)) {
                out.words.push(word);
            }
        }
    }
    let mut sources = HashSet::new();
    for value in raw_replacements {
        let Value::Object(item) = value else { continue };
        let (Some(from), Some(to)) = (json_word(item.get("from")), json_word(item.get("to")))
        else {
            continue;
        };
        if sources.insert(casefold(&from)) {
            out.replacements.push(Replacement { from, to });
        }
    }
    Ok(out)
}

type Signature = (i128, u64);

/// mtime-aware, atomic JSON dictionary shared by the app and CLI.
pub struct DictionaryStore {
    path: PathBuf,
    lock_path: PathBuf,
    lock: Mutex<()>,
    cache: Mutex<Option<(Signature, DictionaryData)>>,
}

impl DictionaryStore {
    /// `None` uses `~/.localflow/dictionary.json`.
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.unwrap_or_else(dictionary_path);
        let lock_path = sibling_with_extra(&path, ".lock");
        DictionaryStore {
            path,
            lock_path,
            lock: Mutex::new(()),
            cache: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn exclusive(&self) -> (MutexGuard<'_, ()>, SidecarLock) {
        let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let parent = self.path.parent().unwrap_or(Path::new("."));
        let sidecar = SidecarLock::acquire(&self.lock_path, parent, "dictionary");
        (guard, sidecar)
    }

    fn read(&self) -> DictionaryData {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return DictionaryData::default()
            }
            Err(err) => {
                log::error!(
                    "dictionary unreadable; treating as empty: {} ({err})",
                    self.path.display()
                );
                return DictionaryData::default();
            }
        };
        let parsed = std::str::from_utf8(&raw)
            .map_err(anyhow::Error::from)
            .and_then(|text| serde_json::from_str::<Value>(text).map_err(anyhow::Error::from))
            .and_then(|value| normalize_data(&value));
        match parsed {
            Ok(data) => data,
            Err(err) => {
                log::error!(
                    "dictionary unreadable; treating as empty: {} ({err})",
                    self.path.display()
                );
                DictionaryData::default()
            }
        }
    }

    fn write(&self, data: &DictionaryData) -> Result<()> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // NamedTemporaryFile(prefix=".<name>.", suffix=".tmp"): 0600 on Unix.
        let mut handle = tempfile::Builder::new()
            .prefix(&format!(".{name}."))
            .suffix(".tmp")
            .tempfile_in(parent)
            .with_context(|| format!("creating temporary file in {}", parent.display()))?;
        handle.write_all(json_dumps(&data.to_json(), Some(2), false).as_bytes())?;
        handle.flush()?;
        handle.as_file().sync_all()?;
        handle
            .persist(&self.path)
            .map_err(|e| anyhow::Error::new(e.error))
            .with_context(|| format!("replacing {}", self.path.display()))?;
        let signature = file_signature(&self.path)?;
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((signature, data.clone()));
        Ok(())
    }

    fn snapshot(&self) -> DictionaryData {
        let signature = match file_signature(&self.path) {
            Ok(sig) => sig,
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::error!(
                        "dictionary stat failed; treating as empty: {} ({err})",
                        self.path.display()
                    );
                }
                *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return DictionaryData::default();
            }
        };
        if let Some((cached_sig, data)) = &*self.cache.lock().unwrap_or_else(|e| e.into_inner()) {
            if *cached_sig == signature {
                return data.clone();
            }
        }
        let data = {
            let _held = self.exclusive();
            self.read()
        };
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((signature, data.clone()));
        data
    }

    pub fn list(&self) -> DictionaryData {
        self.snapshot()
    }

    pub fn words(&self) -> Vec<String> {
        self.snapshot().words
    }

    pub fn replacements(&self) -> Vec<(String, String)> {
        self.snapshot()
            .replacements
            .into_iter()
            .map(|r| (r.from, r.to))
            .collect()
    }

    pub fn add_word(&self, word: &str) -> Result<bool> {
        let value = word_value(word).ok_or_else(|| derr("word is empty"))?;
        let _held = self.exclusive();
        let mut data = self.read();
        let key = casefold(&value);
        if data.words.iter().any(|w| casefold(w) == key) {
            return Ok(false);
        }
        data.words.push(value);
        self.write(&data)?;
        Ok(true)
    }

    pub fn add_replacement(&self, source: &str, target: &str) -> Result<bool> {
        let (Some(from), Some(to)) = (word_value(source), word_value(target)) else {
            return Err(derr("replacement source and target must be non-empty"));
        };
        let _held = self.exclusive();
        let mut data = self.read();
        let key = casefold(&from);
        if data.replacements.iter().any(|r| casefold(&r.from) == key) {
            return Ok(false);
        }
        data.replacements.push(Replacement { from, to });
        self.write(&data)?;
        Ok(true)
    }

    pub fn update_word(&self, old: &str, new: &str) -> Result<bool> {
        let _held = self.exclusive();
        let (Some(old_value), Some(new_value)) = (word_value(old), word_value(new)) else {
            return Err(derr("word is empty"));
        };
        let mut data = self.read();
        let old_key = casefold(&old_value);
        let new_key = casefold(&new_value);
        let Some(index) = data.words.iter().position(|w| casefold(w) == old_key) else {
            return Ok(false);
        };
        if data
            .words
            .iter()
            .enumerate()
            .any(|(i, w)| i != index && casefold(w) == new_key)
        {
            return Ok(false);
        }
        data.words[index] = new_value;
        self.write(&data)?;
        Ok(true)
    }

    pub fn update_replacement(&self, old_from: &str, new_from: &str, new_to: &str) -> Result<bool> {
        let _held = self.exclusive();
        let (Some(old_value), Some(from), Some(to)) = (
            word_value(old_from),
            word_value(new_from),
            word_value(new_to),
        ) else {
            return Err(derr("replacement source and target must be non-empty"));
        };
        let mut data = self.read();
        let old_key = casefold(&old_value);
        let new_key = casefold(&from);
        let Some(index) = data
            .replacements
            .iter()
            .position(|r| casefold(&r.from) == old_key)
        else {
            return Ok(false);
        };
        if data
            .replacements
            .iter()
            .enumerate()
            .any(|(i, r)| i != index && casefold(&r.from) == new_key)
        {
            return Ok(false);
        }
        data.replacements[index] = Replacement { from, to };
        self.write(&data)?;
        Ok(true)
    }

    /// Remove entries matching `value` case-insensitively. `kind` ("word" or
    /// "replacement") limits removal to that list; None removes from both.
    pub fn remove(&self, value: &str, kind: Option<&str>) -> Result<bool> {
        if let Some(kind) = kind {
            if kind != "word" && kind != "replacement" {
                bail!(
                    "unknown dictionary entry kind: {}",
                    crate::symbols::pycompat::repr_str(kind)
                );
            }
        }
        let Some(normalized) = word_value(value) else {
            return Ok(false);
        };
        let folded = casefold(&normalized);
        let _held = self.exclusive();
        let data = self.read();
        let words: Vec<String> = data
            .words
            .iter()
            .filter(|w| kind == Some("replacement") || casefold(w) != folded)
            .cloned()
            .collect();
        let replacements: Vec<Replacement> = data
            .replacements
            .iter()
            .filter(|r| kind == Some("word") || casefold(&r.from) != folded)
            .cloned()
            .collect();
        let removed =
            words.len() != data.words.len() || replacements.len() != data.replacements.len();
        if removed {
            self.write(&DictionaryData {
                words,
                replacements,
            })?;
        }
        Ok(removed)
    }

    /// Remove `word` only if stored with exactly this text.
    pub fn remove_exact_word(&self, word: &str) -> Result<bool> {
        let _held = self.exclusive();
        let mut data = self.read();
        if !data.words.iter().any(|w| w == word) {
            return Ok(false);
        }
        data.words.retain(|w| w != word);
        self.write(&data)?;
        Ok(true)
    }

    /// Remove the replacement only if it is exactly `source` -> `target`.
    pub fn remove_exact_replacement(&self, source: &str, target: &str) -> Result<bool> {
        let _held = self.exclusive();
        let mut data = self.read();
        let before = data.replacements.len();
        data.replacements
            .retain(|r| !(r.from == source && r.to == target));
        if data.replacements.len() == before {
            return Ok(false);
        }
        self.write(&data)?;
        Ok(true)
    }

    /// Import words/replacements from a UTF-8 CSV or JSON file.
    pub fn import_file(&self, path: &Path) -> Result<ImportCounts> {
        let raw = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        if raw.len() > MAX_IMPORT_BYTES {
            return Err(derr("import file exceeds 3 MB"));
        }
        let text = String::from_utf8(raw).map_err(|_| derr("import file must be UTF-8"))?;
        let suffix = casefold(&path_suffix(path));
        let (candidates, invalid) = match suffix.as_str() {
            ".csv" => csv_entries(&text)?,
            ".json" => json_entries(&text)?,
            _ => return Err(derr("import file must be CSV or JSON")),
        };
        if candidates.len() > MAX_IMPORT_ENTRIES {
            return Err(derr("import file contains more than 1000 valid entries"));
        }
        let mut counts = ImportCounts {
            skipped_invalid: invalid,
            ..Default::default()
        };
        let _held = self.exclusive();
        let mut data = self.read();
        let mut words: HashSet<String> = data.words.iter().map(|w| casefold(w)).collect();
        let mut sources: HashSet<String> = data
            .replacements
            .iter()
            .map(|r| casefold(&r.from))
            .collect();
        for candidate in candidates {
            match candidate {
                Candidate::Word(word) => {
                    if words.insert(casefold(&word)) {
                        data.words.push(word);
                        counts.added += 1;
                    } else {
                        counts.skipped_duplicate += 1;
                    }
                }
                Candidate::Replacement(from, to) => {
                    if sources.insert(casefold(&from)) {
                        data.replacements.push(Replacement { from, to });
                        counts.added += 1;
                    } else {
                        counts.skipped_duplicate += 1;
                    }
                }
            }
        }
        if counts.added > 0 {
            self.write(&data)?;
        }
        Ok(counts)
    }
}

enum Candidate {
    Word(String),
    Replacement(String, String),
}

fn csv_entries(text: &str) -> Result<(Vec<Candidate>, usize)> {
    let mut rows = python_csv_rows(text)?;
    if let Some(first) = rows.first() {
        let header: Vec<String> = first.iter().map(|c| casefold(strip(c))).collect();
        if header == ["word"] || header == ["from", "to"] {
            rows.remove(0);
        }
    }
    let mut candidates = Vec::new();
    let mut invalid = 0;
    for row in rows {
        match row.as_slice() {
            [single] => match word_value(single) {
                Some(word) => candidates.push(Candidate::Word(word)),
                None => invalid += 1,
            },
            [source, target] => match (word_value(source), word_value(target)) {
                (Some(from), Some(to)) => candidates.push(Candidate::Replacement(from, to)),
                _ => invalid += 1,
            },
            _ => invalid += 1,
        }
    }
    Ok((candidates, invalid))
}

/// Python's `csv.reader(io.StringIO(text))` with the default excel dialect
/// (delimiter ",", quotechar '"', doublequote, strict=False). Unlike the
/// `csv` crate it yields `[]` for blank lines, which the importer counts as
/// invalid rows.
fn python_csv_rows(text: &str) -> Result<Vec<Vec<String>>> {
    const CSV_FIELD_LIMIT: usize = 128 * 1024;
    #[derive(PartialEq, Clone, Copy)]
    enum State {
        StartRecord,
        StartField,
        InField,
        InQuotedField,
        QuoteInQuotedField,
        EatCrnl,
    }
    #[derive(Clone, Copy)]
    enum Ch {
        C(char),
        Eol,
    }
    struct Parser {
        state: State,
        fields: Vec<String>,
        field: String,
        field_len: usize,
    }
    impl Parser {
        fn save(&mut self) {
            self.fields.push(std::mem::take(&mut self.field));
            self.field_len = 0;
        }
        /// `parse_add_char`: CPython's default `csv.field_size_limit()`.
        fn add(&mut self, c: char) -> Result<()> {
            if self.field_len >= CSV_FIELD_LIMIT {
                bail!("field larger than field limit ({CSV_FIELD_LIMIT})");
            }
            self.field.push(c);
            self.field_len += 1;
            Ok(())
        }
        fn process(&mut self, c: Ch) -> Result<()> {
            let is_crlf = |c: Ch| matches!(c, Ch::C('\n') | Ch::C('\r'));
            match self.state {
                State::StartRecord => {
                    if matches!(c, Ch::Eol) {
                        return Ok(());
                    }
                    if is_crlf(c) {
                        self.state = State::EatCrnl;
                        return Ok(());
                    }
                    self.state = State::StartField;
                    self.process(c)?;
                }
                State::StartField => match c {
                    Ch::Eol => {
                        self.save();
                        self.state = State::StartRecord;
                    }
                    Ch::C('\n') | Ch::C('\r') => {
                        self.save();
                        self.state = State::EatCrnl;
                    }
                    Ch::C('"') => self.state = State::InQuotedField,
                    Ch::C(',') => self.save(),
                    Ch::C(other) => {
                        self.add(other)?;
                        self.state = State::InField;
                    }
                },
                State::InField => match c {
                    Ch::Eol => {
                        self.save();
                        self.state = State::StartRecord;
                    }
                    Ch::C('\n') | Ch::C('\r') => {
                        self.save();
                        self.state = State::EatCrnl;
                    }
                    Ch::C(',') => {
                        self.save();
                        self.state = State::StartField;
                    }
                    Ch::C(other) => self.add(other)?,
                },
                State::InQuotedField => match c {
                    Ch::Eol => {}
                    Ch::C('"') => self.state = State::QuoteInQuotedField,
                    Ch::C(other) => self.add(other)?,
                },
                State::QuoteInQuotedField => match c {
                    Ch::C('"') => {
                        self.add('"')?;
                        self.state = State::InQuotedField;
                    }
                    Ch::C(',') => {
                        self.save();
                        self.state = State::StartField;
                    }
                    Ch::Eol => {
                        self.save();
                        self.state = State::StartRecord;
                    }
                    Ch::C('\n') | Ch::C('\r') => {
                        self.save();
                        self.state = State::EatCrnl;
                    }
                    Ch::C(other) => {
                        self.add(other)?;
                        self.state = State::InField;
                    }
                },
                State::EatCrnl => match c {
                    Ch::C('\n') | Ch::C('\r') => {}
                    Ch::Eol => self.state = State::StartRecord,
                    Ch::C(_) => bail!(
                        "new-line character seen in unquoted field - do you need to open the file with newline=''?"
                    ),
                },
            }
            Ok(())
        }
    }

    // io.StringIO iteration: lines end after each "\n" (kept).
    let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
    if text.is_empty() {
        lines.clear();
    }
    let mut rows = Vec::new();
    let mut parser = Parser {
        state: State::StartRecord,
        fields: Vec::new(),
        field: String::new(),
        field_len: 0,
    };
    let mut iter = lines.into_iter();
    loop {
        parser.fields.clear();
        parser.field.clear();
        parser.field_len = 0;
        parser.state = State::StartRecord;
        let mut finished = false;
        loop {
            let Some(line) = iter.next() else {
                if parser.field_len != 0 || parser.state == State::InQuotedField {
                    parser.save();
                    rows.push(std::mem::take(&mut parser.fields));
                }
                finished = true;
                break;
            };
            for c in line.chars() {
                parser.process(Ch::C(c))?;
            }
            parser.process(Ch::Eol)?;
            if parser.state == State::StartRecord {
                break;
            }
        }
        if finished {
            break;
        }
        rows.push(std::mem::take(&mut parser.fields));
    }
    Ok(rows)
}

fn json_entries(text: &str) -> Result<(Vec<Candidate>, usize)> {
    let data: Value = serde_json::from_str(text).map_err(|e| derr(format!("invalid JSON: {e}")))?;
    let Value::Object(map) = &data else {
        return Err(derr("dictionary JSON must be an object"));
    };
    let empty = Value::Array(Vec::new());
    let (Value::Array(words), Value::Array(replacements)) = (
        map.get("words").unwrap_or(&empty),
        map.get("replacements").unwrap_or(&empty),
    ) else {
        return Err(derr("dictionary JSON needs words and replacements lists"));
    };
    let mut candidates = Vec::new();
    let mut invalid = 0;
    for value in words {
        match json_word(Some(value)) {
            Some(word) => candidates.push(Candidate::Word(word)),
            None => invalid += 1,
        }
    }
    for value in replacements {
        let Value::Object(item) = value else {
            invalid += 1;
            continue;
        };
        match (json_word(item.get("from")), json_word(item.get("to"))) {
            (Some(from), Some(to)) => candidates.push(Candidate::Replacement(from, to)),
            _ => invalid += 1,
        }
    }
    Ok((candidates, invalid))
}

/// Apply replacement pairs: whole-"word" (Python `\w` boundaries),
/// case-insensitive (Python IGNORECASE), longest source first, literal.
pub fn apply_replacements<S: AsRef<str>, T: AsRef<str>>(text: &str, pairs: &[(S, T)]) -> String {
    let mut ordered: Vec<(String, String)> = pairs
        .iter()
        .filter_map(|(s, t)| word_value(s.as_ref()).map(|s| (s, t.as_ref().to_string())))
        .collect();
    if ordered.is_empty() {
        return text.to_string();
    }
    ordered.sort_by_key(|(s, _)| std::cmp::Reverse(s.chars().count()));
    let pattern = Regex::new(
        re::seq(vec![
            re::not_after(is_word),
            re::alt(ordered.iter().map(|(s, _)| re::lit_ci(s)).collect()),
            re::not_ahead(re::word()),
        ]),
        0,
    );
    let mut targets: HashMap<String, String> = HashMap::new();
    for (source, target) in &ordered {
        targets.insert(casefold(source), target.clone());
    }
    let literals: Vec<Regex> = ordered
        .iter()
        .map(|(s, _)| Regex::new(re::lit_ci(s), 0))
        .collect();
    pattern.sub(text, |m, t| {
        let matched = m.text(t);
        if let Some(target) = targets.get(&casefold(&matched)) {
            return target.clone();
        }
        let matched_chars = chars(&matched);
        for ((_, replacement), literal) in ordered.iter().zip(&literals) {
            if literal.fullmatch(&matched_chars) {
                return replacement.clone();
            }
        }
        matched
    })
}

/// Whisper-style initial prompt listing vocabulary, capped at 800 chars.
pub fn stt_prompt<S: AsRef<str>>(words: &[S]) -> Option<String> {
    let prefix = "Vocabulary: ";
    let mut selected: Vec<String> = Vec::new();
    for raw in words {
        let Some(word) = word_value(raw.as_ref()) else {
            continue;
        };
        let mut joined = selected.clone();
        joined.push(word.clone());
        let candidate = format!("{prefix}{}.", joined.join(", "));
        if candidate.chars().count() > MAX_PROMPT_CHARS {
            break;
        }
        selected.push(word);
    }
    if selected.is_empty() {
        None
    } else {
        Some(format!("{prefix}{}.", selected.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> DictionaryStore {
        DictionaryStore::new(Some(dir.path().join("dictionary.json")))
    }

    fn is_dict_err(err: &anyhow::Error, needle: &str) -> bool {
        err.downcast_ref::<DictionaryError>()
            .map(|e| e.0.contains(needle))
            .unwrap_or(false)
    }

    #[test]
    fn round_trip_and_cross_instance_reload() {
        let dir = tempfile::tempdir().unwrap();
        let first = store(&dir);
        let second = store(&dir);
        assert!(first.add_word("Ada Lovelace").unwrap());
        assert!(first.add_replacement("teh", "the").unwrap());
        assert_eq!(second.words(), vec!["Ada Lovelace"]);
        assert_eq!(
            second.replacements(),
            vec![("teh".to_string(), "the".to_string())]
        );
        assert!(!second.add_word("ADA LOVELACE").unwrap());
        assert!(second.remove("TEH", None).unwrap());
        assert!(second.replacements().is_empty());
        assert!(first.replacements().is_empty());
    }

    #[test]
    fn file_format_matches_python_json_dump() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.add_word("Café").unwrap();
        s.add_replacement("teh", "the").unwrap();
        let text = fs::read_to_string(s.path()).unwrap();
        assert_eq!(
            text,
            "{\n  \"words\": [\n    \"Café\"\n  ],\n  \"replacements\": [\n    {\n      \"from\": \"teh\",\n      \"to\": \"the\"\n    }\n  ]\n}"
        );
        assert!(dir.path().join("dictionary.json.lock").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // no stray temp files
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn external_edits_are_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        assert!(s.words().is_empty());
        fs::write(s.path(), r#"{"words": ["X", " x ", 3, "Y"], "replacements": [{"from": "a", "to": "b"}, {"from": "A", "to": "c"}, 7, {"from": "", "to": "d"}]}"#).unwrap();
        assert_eq!(s.words(), vec!["X", "Y"]);
        assert_eq!(s.replacements(), vec![("a".to_string(), "b".to_string())]);
    }

    #[test]
    fn updates_validate_and_only_change_same_kind_entries() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.add_word("Ada").unwrap();
        s.add_word("Grace").unwrap();
        s.add_replacement("teh", "the").unwrap();
        s.add_replacement("recieve", "receive").unwrap();
        let original = s.list();
        assert!(!s.update_word("Ada", "Grace").unwrap());
        assert!(!s.update_replacement("teh", "recieve", "receive").unwrap());
        assert!(!s.update_word("missing", "x").unwrap());
        assert_eq!(s.list(), original);
        assert!(s.update_word("Ada", "Ada Lovelace").unwrap());
        assert!(s.update_replacement("teh", "teh", "thee").unwrap());
        assert_eq!(
            s.list(),
            DictionaryData {
                words: vec!["Ada Lovelace".into(), "Grace".into()],
                replacements: vec![
                    Replacement {
                        from: "teh".into(),
                        to: "thee".into()
                    },
                    Replacement {
                        from: "recieve".into(),
                        to: "receive".into()
                    },
                ],
            }
        );
        let err = s.update_word("Ada Lovelace", " ").unwrap_err();
        assert!(is_dict_err(&err, "word is empty"));
        let err = s.update_replacement("teh", "", "the").unwrap_err();
        assert!(is_dict_err(&err, "replacement"));
        assert!(is_dict_err(&s.add_word("  ").unwrap_err(), "word is empty"));
        assert!(is_dict_err(
            &s.add_replacement("x", " ").unwrap_err(),
            "non-empty"
        ));
    }

    #[test]
    fn remove_kinds_and_exact_removal() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        s.add_word("teh").unwrap();
        s.add_replacement("TEH", "the").unwrap();
        assert!(s.remove("teh", Some("word")).unwrap());
        assert_eq!(s.replacements().len(), 1);
        assert!(!s.remove("  ", None).unwrap());
        assert!(s
            .remove("x", Some("bogus"))
            .unwrap_err()
            .to_string()
            .contains("'bogus'"));
        assert!(!s.remove_exact_replacement("teh", "the").unwrap());
        assert!(s.remove_exact_replacement("TEH", "the").unwrap());
        s.add_word("Ada").unwrap();
        assert!(!s.remove_exact_word("ada").unwrap());
        assert!(s.remove_exact_word("Ada").unwrap());
        assert_eq!(s.list(), DictionaryData::default());
    }

    #[test]
    fn missing_and_corrupt_files_read_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        assert_eq!(s.list(), DictionaryData::default());
        fs::write(s.path(), "{broken").unwrap();
        assert_eq!(s.list(), DictionaryData::default());
        fs::write(s.path(), "[1, 2]").unwrap();
        assert_eq!(s.list(), DictionaryData::default());
        fs::write(s.path(), r#"{"words": "nope"}"#).unwrap();
        assert_eq!(s.list(), DictionaryData::default());
        fs::write(s.path(), [0xff, 0xfe]).unwrap();
        assert_eq!(s.list(), DictionaryData::default());
        // Python behaviour: the next write replaces unreadable content.
        assert!(s.add_word("Ada").unwrap());
        assert_eq!(s.words(), vec!["Ada"]);
    }

    #[test]
    fn replacements_longest_first_case_insensitive_and_literal() {
        assert_eq!(
            apply_replacements(
                "CATALOG, cat and c++",
                &[("cat", "dog"), ("catalog", "map"), ("c++", "C sharp")]
            ),
            "map, dog and C sharp"
        );
        assert_eq!(
            apply_replacements("concatenate", &[("cat", "dog")]),
            "concatenate"
        );
        assert_eq!(apply_replacements("ı İ", &[("i", "I")]), "I I");
        assert_eq!(apply_replacements("c++x c++", &[("c++", "C#")]), "c++x C#");
        assert_eq!(
            apply_replacements("café cafés", &[("café", "coffee")]),
            "coffee cafés"
        );
        assert_eq!(
            apply_replacements("x\u{301}cat", &[("cat", "dog")]),
            "x\u{301}dog"
        );
        assert_eq!(
            apply_replacements("$1 \\1", &[("$1", "\\2"), ("\\1", "$2")]),
            "\\2 $2"
        );
        let empty: [(&str, &str); 0] = [];
        assert_eq!(apply_replacements("text", &empty), "text");
        assert_eq!(apply_replacements("", &[("a", "b")]), "");
        assert_eq!(apply_replacements("a b", &[("  ", "x")]), "a b");
        // same casefold key: last pair (in longest-first order) wins
        assert_eq!(
            apply_replacements("TEH", &[("teh", "one"), ("Teh", "two")]),
            "two"
        );
    }

    #[test]
    fn replacements_handle_long_entries_and_many_pairs() {
        let long = "w".repeat(50_000);
        let text = format!("x {long} y");
        assert_eq!(
            apply_replacements(&text, &[(long.as_str(), "LONG")]),
            "x LONG y"
        );
        let pairs: Vec<(String, String)> = (0..1000)
            .map(|n| (format!("word{n}"), format!("W{n}")))
            .collect();
        let text = "word999 and word5 and word10x ".repeat(50);
        let out = apply_replacements(&text, &pairs);
        assert!(out.starts_with("W999 and W5 and word10x W999"));
    }

    #[test]
    fn stt_prompt_keeps_order_and_drops_overflow() {
        let long = "x".repeat(800);
        let prompt = stt_prompt(&["Ada", "   ", "Lovelace", long.as_str()]).unwrap();
        assert_eq!(prompt, "Vocabulary: Ada, Lovelace.");
        let empty: [&str; 0] = [];
        assert_eq!(stt_prompt(&empty), None);
        assert_eq!(stt_prompt(&["  "]), None);
    }

    #[test]
    fn csv_import_word_header_duplicates_and_invalid_rows() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let source = dir.path().join("words.csv");
        fs::write(&source, "word\n Ada \nada\n\n,invalid\n").unwrap();
        assert_eq!(
            s.import_file(&source).unwrap(),
            ImportCounts {
                added: 1,
                skipped_duplicate: 1,
                skipped_invalid: 2
            }
        );
        assert_eq!(s.words(), vec!["Ada"]);
    }

    #[test]
    fn csv_import_replacement_header_and_two_columns() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let source = dir.path().join("replacements.CSV");
        fs::write(
            &source,
            "from,to\n teh , the \nrecieve,receive\n\"a,b\",\"say \"\"hi\"\"\"\na,b,c\n",
        )
        .unwrap();
        assert_eq!(
            s.import_file(&source).unwrap(),
            ImportCounts {
                added: 3,
                skipped_duplicate: 0,
                skipped_invalid: 1
            }
        );
        assert_eq!(
            s.replacements(),
            vec![
                ("teh".to_string(), "the".to_string()),
                ("recieve".to_string(), "receive".to_string()),
                ("a,b".to_string(), "say \"hi\"".to_string()),
            ]
        );
    }

    #[test]
    fn python_csv_edge_cases() {
        assert_eq!(python_csv_rows("").unwrap(), Vec::<Vec<String>>::new());
        assert_eq!(python_csv_rows("a").unwrap(), vec![vec!["a".to_string()]]);
        assert_eq!(python_csv_rows("\n").unwrap(), vec![Vec::<String>::new()]);
        assert_eq!(
            python_csv_rows("a,\r\n").unwrap(),
            vec![vec!["a".to_string(), String::new()]]
        );
        assert_eq!(
            python_csv_rows("\"multi\nline\",x\n").unwrap(),
            vec![vec!["multi\nline".to_string(), "x".to_string()]]
        );
        assert_eq!(
            python_csv_rows("\"unterminated").unwrap(),
            vec![vec!["unterminated".to_string()]]
        );
        assert_eq!(
            python_csv_rows("\"ab\"cd,e").unwrap(),
            vec![vec!["abcd".to_string(), "e".to_string()]]
        );
        assert!(python_csv_rows("a\rb\n").is_err());
        // CPython's default csv.field_size_limit(): quoted or not.
        let at_limit = "a".repeat(128 * 1024);
        assert_eq!(
            python_csv_rows(&format!("{at_limit}\n")).unwrap()[0][0].len(),
            128 * 1024
        );
        for text in [
            format!("{at_limit}a\n"),
            format!("\"{at_limit}a"),
            format!("x,\"{at_limit}\"\"\""),
        ] {
            let err = python_csv_rows(&text).unwrap_err().to_string();
            assert_eq!(err, "field larger than field limit (131072)");
        }
    }

    #[test]
    fn json_import_uses_dictionary_shape_and_reports_counts() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let source = dir.path().join("import.json");
        fs::write(
            &source,
            r#"{"words": [" Ada ", "ada", "", 12], "replacements": [{"from": "teh", "to": "the"}, {"from": "", "to": "x"}]}"#,
        )
        .unwrap();
        assert_eq!(
            s.import_file(&source).unwrap(),
            ImportCounts {
                added: 2,
                skipped_duplicate: 1,
                skipped_invalid: 3
            }
        );
    }

    #[test]
    fn import_errors() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(&dir);
        let big = dir.path().join("large.json");
        let words: Vec<String> = (0..1001).map(|n| format!("word-{n}")).collect();
        fs::write(
            &big,
            serde_json::to_string(&json!({"words": words})).unwrap(),
        )
        .unwrap();
        assert!(is_dict_err(
            &s.import_file(&big).unwrap_err(),
            "more than 1000"
        ));
        assert!(s.words().is_empty());

        let huge = dir.path().join("huge.csv");
        fs::write(&huge, vec![b'a'; MAX_IMPORT_BYTES + 1]).unwrap();
        assert!(is_dict_err(
            &s.import_file(&huge).unwrap_err(),
            "exceeds 3 MB"
        ));

        let txt = dir.path().join("words.txt");
        fs::write(&txt, "Ada").unwrap();
        assert!(is_dict_err(
            &s.import_file(&txt).unwrap_err(),
            "CSV or JSON"
        ));

        let bad = dir.path().join("bad.csv");
        fs::write(&bad, [0xff, b'a']).unwrap();
        assert!(is_dict_err(&s.import_file(&bad).unwrap_err(), "UTF-8"));

        let bad_json = dir.path().join("bad.json");
        fs::write(&bad_json, "{nope").unwrap();
        assert!(is_dict_err(
            &s.import_file(&bad_json).unwrap_err(),
            "invalid JSON"
        ));
        fs::write(&bad_json, "[]").unwrap();
        assert!(is_dict_err(
            &s.import_file(&bad_json).unwrap_err(),
            "must be an object"
        ));
        fs::write(&bad_json, r#"{"words": {}}"#).unwrap();
        assert!(is_dict_err(&s.import_file(&bad_json).unwrap_err(), "lists"));

        assert!(s.import_file(&dir.path().join("missing.csv")).is_err());
        let empty_csv = dir.path().join("empty.csv");
        fs::write(&empty_csv, "").unwrap();
        assert_eq!(s.import_file(&empty_csv).unwrap(), ImportCounts::default());
        assert!(!s.path().exists(), "no write when nothing was added");
    }
}
