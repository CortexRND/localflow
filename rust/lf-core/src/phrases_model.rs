//! UI-free model behind the hot-phrase editor windows (port of
//! `localflow/phrases_model.py`): validation hints, commit/delete/undo, the
//! dictation preview, and on-disk change detection.

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::dictionary::{apply_replacements, DictionaryStore};
use crate::formatting::{apply_spoken_formatting, apply_style};
use crate::hotphrases::{
    matched_triggers, normalize_trigger, render_dictation, Entry, HotPhraseError, HotPhraseStore,
    RenderSteps, MAX_TEXT_CHARS, MAX_TRIGGER_CHARS, MAX_TRIGGER_WORDS,
};
use crate::symbols::apply_spoken_symbols;
use crate::symbols::discover_commands;
use crate::symbols::pycompat::{
    casefold, file_signature, split_ws, splitlines_count, strip, thousands, truthy,
};

/// The config values the model reads (Python reads them off `Config`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhrasesSettings {
    pub hot_phrases: bool,
    pub spoken_symbols: bool,
    pub spoken_punctuation: bool,
    pub styles_enabled: bool,
    /// `style_<category_for(None)>`, i.e. `style_other`.
    pub style_other: String,
    pub command_names: Vec<String>,
    pub skills_dirs: Vec<String>,
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

impl PhrasesSettings {
    /// From flat Python config field names (`Config.fields`).
    pub fn from_fields(fields: &BTreeMap<String, Value>) -> Self {
        let flag = |key: &str| fields.get(key).map(truthy).unwrap_or(false);
        PhrasesSettings {
            hot_phrases: flag("hot_phrases"),
            spoken_symbols: flag("spoken_symbols"),
            spoken_punctuation: flag("spoken_punctuation"),
            styles_enabled: flag("styles_enabled"),
            style_other: fields
                .get("style_other")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            command_names: string_list(fields.get("command_names")),
            skills_dirs: string_list(fields.get("skills_dirs")),
        }
    }

    pub fn from_config(config: &crate::config::Config) -> Self {
        Self::from_fields(&config.fields)
    }
}

type Signature = Option<(i128, u64)>;

pub struct PhrasesModel {
    pub store: HotPhraseStore,
    pub settings: PhrasesSettings,
    pub commands: Vec<String>,
    /// Python constructs `DictionaryStore()` (the default path) per preview;
    /// injectable here so tests never touch the real home directory.
    pub dictionary: DictionaryStore,
    file_sig: Option<Signature>,
}

fn entry_of(value: &Value) -> Result<&Entry> {
    value
        .as_object()
        .ok_or_else(|| anyhow!("hot phrase entry is not an object: {value}"))
}

fn str_field<'a>(entry: &'a Entry, key: &str) -> Result<&'a str> {
    match entry.get(key) {
        Some(Value::String(s)) => Ok(s),
        Some(other) => Err(anyhow!("hot phrase {key} is not a string: {other}")),
        None => Err(anyhow!("KeyError: '{key}'")),
    }
}

fn hot_phrase_message(err: &anyhow::Error) -> Option<String> {
    err.downcast_ref::<HotPhraseError>()
        .map(|e| e.message.clone())
}

impl PhrasesModel {
    /// `settings: None` loads `~/.localflow.toml` via `config::load_config()`.
    pub fn new(store: HotPhraseStore, settings: Option<PhrasesSettings>) -> Result<Self> {
        let settings = match settings {
            Some(settings) => settings,
            None => PhrasesSettings::from_config(&crate::config::load_config()?),
        };
        let commands = if settings.spoken_symbols {
            discover_commands(&settings.command_names, &settings.skills_dirs)
        } else {
            Vec::new()
        };
        Ok(PhrasesModel {
            store,
            settings,
            commands,
            dictionary: DictionaryStore::new(None),
            file_sig: None,
        })
    }

    pub fn with_dictionary(mut self, dictionary: DictionaryStore) -> Self {
        self.dictionary = dictionary;
        self
    }

    pub fn hot_phrases_enabled(&self) -> bool {
        self.settings.hot_phrases
    }

    // ------------------------------------------------------------- data ---

    /// Entries sorted by casefolded trigger, filtered by `query` against the
    /// trigger and the expansion text.
    pub fn entries(&self, query: &str) -> Result<Vec<Entry>> {
        let mut keyed: Vec<(String, Entry)> = Vec::new();
        for value in self.store.list() {
            let entry = entry_of(&value)?.clone();
            keyed.push((casefold(str_field(&entry, "trigger")?), entry));
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = Vec::new();
        let needle = casefold(query);
        for (trigger_key, entry) in keyed {
            if query.is_empty()
                || trigger_key.contains(&needle)
                || casefold(str_field(&entry, "text")?).contains(&needle)
            {
                out.push(entry);
            }
        }
        Ok(out)
    }

    pub fn get(&self, entry_id: &str) -> Result<Option<Entry>> {
        for value in self.store.list() {
            let entry = entry_of(&value)?;
            if entry.get("id").and_then(Value::as_str) == Some(entry_id) {
                return Ok(Some(entry.clone()));
            }
        }
        Ok(None)
    }

    // ------------------------------------------------------------ hints ---

    /// (ok, message) for the trigger field — mirrors validation and the
    /// duplicate check so the UI can warn early.
    pub fn trigger_hint(&self, trigger: &str, exclude_id: Option<&str>) -> Result<(bool, String)> {
        let stripped = strip(trigger);
        if stripped.is_empty() {
            return Ok((false, "Type the words you'll say".into()));
        }
        if stripped.chars().count() > MAX_TRIGGER_CHARS {
            return Ok((
                false,
                format!("Keep it under {MAX_TRIGGER_CHARS} characters"),
            ));
        }
        let normalized = normalize_trigger(stripped);
        if normalized.is_empty() {
            return Ok((false, "Use at least one word".into()));
        }
        let n = normalized.split(' ').count();
        if n > MAX_TRIGGER_WORDS {
            return Ok((false, format!("Use {MAX_TRIGGER_WORDS} words or fewer")));
        }
        for value in self.store.list() {
            let other = entry_of(&value)?;
            let other_id = other.get("id");
            let excluded = match (other_id, exclude_id) {
                (Some(Value::String(a)), Some(b)) => a == b,
                (None | Some(Value::Null), None) => true,
                _ => false,
            };
            if excluded {
                continue;
            }
            let other_trigger = match other.get("trigger") {
                None => "",
                Some(Value::String(s)) => s.as_str(),
                Some(v) => return Err(anyhow!("hot phrase trigger is not a string: {v}")),
            };
            if normalize_trigger(other_trigger) == normalized {
                return Ok((
                    false,
                    format!(
                        "Already used by \u{201c}{}\u{201d}",
                        str_field(other, "trigger")?
                    ),
                ));
            }
        }
        let mut hint = format!("\u{2713} {n} word{}", if n != 1 { "s" } else { "" });
        if normalized != split_ws(&stripped.to_lowercase()).join(" ") {
            hint.push_str(&format!(
                " \u{b7} matches \u{201c}{normalized}\u{201d} (case and punctuation ignored)"
            ));
        }
        Ok((true, hint))
    }

    pub fn text_hint(&self, text: &str) -> (bool, String) {
        if strip(text).is_empty() {
            return (false, "Type the text to paste".into());
        }
        let length = text.chars().count();
        if length > MAX_TEXT_CHARS {
            return (
                false,
                format!("Keep it under {} characters", thousands(MAX_TEXT_CHARS)),
            );
        }
        let lines = splitlines_count(text).max(1);
        let line_word = if lines == 1 { "line" } else { "lines" };
        let char_word = if length == 1 {
            "character"
        } else {
            "characters"
        };
        (
            true,
            format!(
                "{lines} {line_word} \u{b7} {} {char_word}",
                thousands(length)
            ),
        )
    }

    // ---------------------------------------------------------- actions ---

    /// Validate and write. `(Some(entry), None)` or `(None, Some(message))`;
    /// an invalid draft writes nothing and a no-op commit skips the write.
    pub fn commit(
        &self,
        entry_id: Option<&str>,
        trigger: &str,
        text: &str,
        enabled: bool,
    ) -> Result<(Option<Entry>, Option<String>)> {
        let (ok, message) = self.trigger_hint(trigger, entry_id)?;
        if !ok {
            return Ok((None, Some(message)));
        }
        let (ok, message) = self.text_hint(text);
        if !ok {
            return Ok((None, Some(message)));
        }
        let stripped = strip(trigger);
        if let Some(id) = entry_id {
            if let Some(existing) = self.get(id)? {
                let same_trigger =
                    existing.get("trigger") == Some(&Value::String(stripped.to_string()));
                let same_text = existing.get("text") == Some(&Value::String(text.to_string()));
                let same_enabled = existing.get("enabled").map(truthy).unwrap_or(false) == enabled;
                if same_trigger && same_text && same_enabled {
                    return Ok((Some(existing), None));
                }
            }
        }
        let result = match entry_id {
            None => self.store.add(stripped, text, enabled).map(Some),
            Some(id) => self
                .store
                .update(id, Some(stripped), Some(text), Some(enabled)),
        };
        match result {
            Ok(Some(entry)) => Ok((Some(entry), None)),
            Ok(None) => Ok((None, Some("This hot phrase was deleted elsewhere".into()))),
            // Race guard: another process may have claimed the trigger.
            Err(err) => match hot_phrase_message(&err) {
                Some(message) => Ok((None, Some(message))),
                None => Err(err),
            },
        }
    }

    pub fn set_enabled(&self, entry_id: &str, enabled: bool) -> Result<Option<Entry>> {
        self.store.update(entry_id, None, None, Some(enabled))
    }

    /// Delete and return the entry as an undo token.
    pub fn delete(&self, entry_id: &str) -> Result<Option<Entry>> {
        let Some(entry) = self.get(entry_id)? else {
            return Ok(None);
        };
        if !self.store.delete(entry_id)? {
            return Ok(None);
        }
        Ok(Some(entry))
    }

    /// Re-add a deleted entry (with a fresh id).
    pub fn undo_delete(&self, token: &Entry) -> Result<(Option<Entry>, Option<String>)> {
        let trigger = str_field(token, "trigger")?;
        let text = str_field(token, "text")?;
        let enabled = token.get("enabled").map(truthy).unwrap_or(false);
        match self.store.add(trigger, text, enabled) {
            Ok(entry) => Ok((Some(entry), None)),
            Err(err) => match hot_phrase_message(&err) {
                Some(message) => Ok((None, Some(message))),
                None => Err(err),
            },
        }
    }

    // ---------------------------------------------------------- preview ---

    /// What dictation would paste for `said` (minus LLM cleanup), plus the
    /// enabled triggers that matched.
    pub fn preview(&self, said: &str) -> Result<(String, Vec<String>)> {
        let phrases = if self.settings.hot_phrases {
            self.store.enabled()?
        } else {
            Vec::new()
        };
        let words = self.dictionary.words();
        let style_name = if self.settings.styles_enabled && !self.settings.style_other.is_empty() {
            Some(self.settings.style_other.clone())
        } else {
            None
        };
        let replace = |value: &str| apply_replacements(value, &self.dictionary.replacements());
        let fmt = |value: &str| apply_spoken_formatting(value);
        let symbols = |value: &str| apply_spoken_symbols(value, &self.commands);
        let style = |value: &str| apply_style(value, style_name.as_deref().unwrap_or(""), &words);
        let steps = RenderSteps {
            replace: Some(&replace),
            fmt: if self.settings.spoken_punctuation {
                Some(&fmt)
            } else {
                None
            },
            clean: None,
            symbols: if self.settings.spoken_symbols {
                Some(&symbols)
            } else {
                None
            },
            style: if style_name.is_some() {
                Some(&style)
            } else {
                None
            },
        };
        let rendered = render_dictation(said, &phrases, &steps);
        Ok((rendered, matched_triggers(said, &phrases)))
    }

    // ------------------------------------------------------- disk watch ---

    /// True when the store file changed since the last call; the first call
    /// primes the signature and returns false.
    pub fn changed_on_disk(&mut self) -> bool {
        let signature = file_signature(self.store.path()).ok();
        match self.file_sig.replace(signature) {
            None => false,
            Some(previous) => previous != signature,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(hot_phrases: bool) -> PhrasesSettings {
        PhrasesSettings {
            hot_phrases,
            spoken_symbols: true,
            style_other: "formal".into(),
            ..Default::default()
        }
    }

    fn model(dir: &tempfile::TempDir) -> PhrasesModel {
        let store = HotPhraseStore::new(dir.path().join("hot_phrases.json"));
        PhrasesModel::new(store, Some(settings(true)))
            .unwrap()
            .with_dictionary(DictionaryStore::new(Some(
                dir.path().join("dictionary.json"),
            )))
    }

    #[test]
    fn trigger_hints() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(&dir);
        assert_eq!(
            m.trigger_hint("", None).unwrap(),
            (false, "Type the words you'll say".into())
        );
        assert_eq!(
            m.trigger_hint("   ", None).unwrap(),
            (false, "Type the words you'll say".into())
        );
        assert_eq!(
            m.trigger_hint(&"x".repeat(MAX_TRIGGER_CHARS + 1), None)
                .unwrap(),
            (false, "Keep it under 80 characters".into())
        );
        assert_eq!(
            m.trigger_hint("!!! ...", None).unwrap(),
            (false, "Use at least one word".into())
        );
        let many: Vec<String> = (0..=MAX_TRIGGER_WORDS).map(|i| format!("w{i}")).collect();
        assert_eq!(
            m.trigger_hint(&many.join(" "), None).unwrap(),
            (false, "Use 8 words or fewer".into())
        );
        assert_eq!(
            m.trigger_hint("deploy", None).unwrap(),
            (true, "\u{2713} 1 word".into())
        );
        assert_eq!(
            m.trigger_hint("deploy prod", None).unwrap(),
            (true, "\u{2713} 2 words".into())
        );
        assert_eq!(
            m.trigger_hint("Review,  CHECKLIST", None).unwrap(),
            (
                true,
                "\u{2713} 2 words \u{b7} matches \u{201c}review checklist\u{201d} (case and punctuation ignored)".into()
            )
        );
        assert_eq!(
            m.trigger_hint("review checklist", None).unwrap(),
            (true, "\u{2713} 2 words".into())
        );
    }

    #[test]
    fn trigger_hint_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(&dir);
        let other = m.store.add("Review Checklist", "x", true).unwrap();
        assert_eq!(
            m.trigger_hint("review, checklist", None).unwrap(),
            (
                false,
                "Already used by \u{201c}Review Checklist\u{201d}".into()
            )
        );
        assert!(
            m.trigger_hint("review, checklist", other["id"].as_str())
                .unwrap()
                .0
        );
    }

    #[test]
    fn text_hints() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(&dir);
        assert_eq!(m.text_hint(""), (false, "Type the text to paste".into()));
        assert_eq!(
            m.text_hint("  \n "),
            (false, "Type the text to paste".into())
        );
        assert_eq!(
            m.text_hint(&"x".repeat(MAX_TEXT_CHARS + 1)),
            (false, "Keep it under 20,000 characters".into())
        );
        assert_eq!(m.text_hint("x"), (true, "1 line \u{b7} 1 character".into()));
        assert_eq!(
            m.text_hint("one"),
            (true, "1 line \u{b7} 3 characters".into())
        );
        assert_eq!(
            m.text_hint("one\ntwo"),
            (true, "2 lines \u{b7} 7 characters".into())
        );
        assert_eq!(
            m.text_hint(&"y".repeat(1234)),
            (true, "1 line \u{b7} 1,234 characters".into())
        );
    }

    #[test]
    fn commit_paths() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(&dir);
        assert_eq!(
            m.commit(None, "", "", true).unwrap(),
            (None, Some("Type the words you'll say".into()))
        );
        assert_eq!(
            m.commit(None, "valid", "   ", true).unwrap(),
            (None, Some("Type the text to paste".into()))
        );
        assert!(!m.store.path().exists());

        let (entry, err) = m.commit(None, "  my trigger ", "my text", true).unwrap();
        assert!(err.is_none());
        let entry = entry.unwrap();
        assert_eq!(entry["trigger"], "my trigger");
        let id = entry["id"].as_str().unwrap().to_string();
        let (updated, err) = m.commit(Some(&id), "my trigger", "new text", true).unwrap();
        assert!(err.is_none());
        assert_eq!(updated.unwrap()["text"], "new text");

        let before = std::fs::read(m.store.path()).unwrap();
        let (again, err) = m
            .commit(Some(&id), " my trigger ", "new text", true)
            .unwrap();
        assert!(err.is_none());
        assert_eq!(again.unwrap()["id"], id.as_str());
        assert_eq!(std::fs::read(m.store.path()).unwrap(), before);

        m.store.add("review checklist", "x", true).unwrap();
        assert_eq!(
            m.commit(None, "Review, checklist", "y", true).unwrap(),
            (
                None,
                Some("Already used by \u{201c}review checklist\u{201d}".into())
            )
        );
        m.store.delete(&id).unwrap();
        assert_eq!(
            m.commit(Some(&id), "trig", "text", true).unwrap(),
            (None, Some("This hot phrase was deleted elsewhere".into()))
        );
    }

    #[test]
    fn entries_sorted_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(&dir);
        m.store.add("beta", "b text", true).unwrap();
        m.store.add("Alpha", "a text about shipping", true).unwrap();
        m.store.add("gamma", "g text", true).unwrap();
        let triggers = |q: &str| -> Vec<String> {
            m.entries(q)
                .unwrap()
                .iter()
                .map(|e| e["trigger"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(triggers(""), vec!["Alpha", "beta", "gamma"]);
        assert_eq!(triggers("ALP"), vec!["Alpha"]);
        assert_eq!(triggers("shipping"), vec!["Alpha"]);
        assert!(triggers("zzz").is_empty());
    }

    #[test]
    fn set_enabled_get_delete_undo() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(&dir);
        let entry = m.store.add("trig", "t", true).unwrap();
        let id = entry["id"].as_str().unwrap();
        assert_eq!(m.set_enabled(id, false).unwrap().unwrap()["enabled"], false);
        assert_eq!(m.get(id).unwrap().unwrap()["enabled"], false);
        assert!(m.get("nope").unwrap().is_none());
        assert!(m.set_enabled("nope", true).unwrap().is_none());
        assert!(m.delete("nope").unwrap().is_none());

        let token = m.delete(id).unwrap().unwrap();
        assert_eq!(token["trigger"], "trig");
        assert!(m.store.list().is_empty());
        let (restored, err) = m.undo_delete(&token).unwrap();
        assert!(err.is_none());
        let restored = restored.unwrap();
        assert_eq!(
            (restored["trigger"].as_str(), restored["text"].as_str()),
            (Some("trig"), Some("t"))
        );
        assert_eq!(restored["enabled"], false);
        assert_ne!(restored["id"], token["id"]);

        let token = m.delete(restored["id"].as_str().unwrap()).unwrap().unwrap();
        m.store.add("Trig", "other", true).unwrap();
        let (restored, err) = m.undo_delete(&token).unwrap();
        assert!(restored.is_none() && err.unwrap().contains("duplicates"));
        assert!(m.undo_delete(&Entry::new()).is_err());
    }

    #[test]
    fn preview_paths() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(&dir);
        m.store.add("review checklist", "use a/b", true).unwrap();
        assert_eq!(
            m.preview("Review checklist.").unwrap(),
            ("use a/b".into(), vec!["review checklist".into()])
        );
        assert_eq!(
            m.preview("a dash b review checklist").unwrap(),
            ("a-b use a/b".into(), vec!["review checklist".into()])
        );
        assert_eq!(
            m.preview("nothing here").unwrap(),
            ("nothing here".into(), vec![])
        );
        m.store.add("review dash checklist", "CHECK", true).unwrap();
        assert_eq!(
            m.preview("please review dash checklist now").unwrap(),
            (
                "please CHECK now".into(),
                vec!["review dash checklist".into()]
            )
        );
        m.store.add("off one", "x", false).unwrap();
        assert!(m.preview("off one").unwrap().1.is_empty());
        // dictionary replacements apply in the preview
        m.dictionary.add_replacement("teh", "the").unwrap();
        assert_eq!(m.preview("teh end").unwrap().0, "the end");
    }

    #[test]
    fn preview_with_formatting_and_style() {
        let dir = tempfile::tempdir().unwrap();
        let store = HotPhraseStore::new(dir.path().join("hp.json"));
        let mut s = settings(true);
        s.spoken_punctuation = true;
        s.styles_enabled = true;
        s.style_other = "very_casual".into();
        let m = PhrasesModel::new(store, Some(s))
            .unwrap()
            .with_dictionary(DictionaryStore::new(Some(dir.path().join("d.json"))));
        m.dictionary.add_word("Ada").unwrap();
        assert_eq!(
            m.preview("Hello new line world period").unwrap().0,
            "hello\nWorld"
        );
        assert_eq!(m.preview("Ada says hi period").unwrap().0, "Ada says hi");
    }

    #[test]
    fn preview_hot_phrases_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = HotPhraseStore::new(dir.path().join("hp.json"));
        store.add("trig", "expansion", true).unwrap();
        let m = PhrasesModel::new(store, Some(settings(false)))
            .unwrap()
            .with_dictionary(DictionaryStore::new(Some(dir.path().join("d.json"))));
        assert!(!m.hot_phrases_enabled());
        assert_eq!(m.preview("trig").unwrap(), ("trig".into(), vec![]));
    }

    #[test]
    fn changed_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = model(&dir);
        assert!(!m.changed_on_disk()); // primes (missing)
        assert!(!m.changed_on_disk());
        HotPhraseStore::new(dir.path().join("hot_phrases.json"))
            .add("trig", "t", true)
            .unwrap();
        assert!(m.changed_on_disk());
        assert!(!m.changed_on_disk());
    }

    #[test]
    fn settings_from_fields_use_python_truthiness() {
        let mut fields = BTreeMap::new();
        fields.insert("hot_phrases".to_string(), Value::Bool(true));
        fields.insert("spoken_symbols".to_string(), serde_json::json!(0));
        fields.insert("style_other".to_string(), serde_json::json!("casual"));
        fields.insert(
            "command_names".to_string(),
            serde_json::json!(["a", 3, "b"]),
        );
        let s = PhrasesSettings::from_fields(&fields);
        assert!(s.hot_phrases && !s.spoken_symbols && !s.styles_enabled);
        assert_eq!(s.style_other, "casual");
        assert_eq!(s.command_names, vec!["a", "b"]);
        assert!(s.skills_dirs.is_empty());
        assert_eq!(
            PhrasesSettings::from_fields(&BTreeMap::new()),
            PhrasesSettings::default()
        );
    }

    #[test]
    fn store_file_is_json() {
        let dir = tempfile::tempdir().unwrap();
        let m = model(&dir);
        m.commit(None, "trig", "t", true).unwrap();
        let data: Value =
            serde_json::from_str(&std::fs::read_to_string(m.store.path()).unwrap()).unwrap();
        assert_eq!(data[0]["trigger"], "trig");
    }
}
