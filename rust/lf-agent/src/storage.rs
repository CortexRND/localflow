use super::*;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
struct Setting {
    key: String,
    before: Value,
    after: Value,
}
#[derive(Clone, Serialize, Deserialize)]
struct Replacement {
    spoken: String,
    written: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created: Option<f64>,
    #[serde(default)]
    settings: Vec<Setting>,
    #[serde(default)]
    words: Vec<String>,
    #[serde(default)]
    replacements: Vec<Replacement>,
}
fn read_bounded(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("The undo record is unreadable; nothing was changed"),
    };
    let mut bytes = Vec::new();
    file.take(MAX_UNDO_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_UNDO_BYTES,
        "The undo record is unreadable; nothing was changed"
    );
    Ok(Some(bytes))
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("invalid undo path")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
impl<C: ChatClient> SettingsAgent<C> {
    fn replan(&self, changes: &[Change], config: &Config) -> Result<Config> {
        ensure!(
            !changes.is_empty() && changes.len() <= MAX_TOOL_CALLS,
            "Nothing to apply or too many changes"
        );
        let mut seen = HashSet::new();
        let mut keys = Vec::new();
        for change in changes {
            let id = match change {
                Change::Setting { key, before, value } => {
                    ensure!(
                        self.validator.value(key, value, config)? == *value,
                        "Invalid preview"
                    );
                    ensure!(
                        config.field(key) == Some(before) && before != value,
                        "Setting changed since the preview; preview again"
                    );
                    keys.push(key.clone());
                    format!("setting:{key}")
                }
                Change::Word(w) => {
                    ensure!(clean_text(w, 64)? == *w, "Invalid preview");
                    self.check_dictionary(change)?;
                    format!("word:{}", folded(w))
                }
                Change::Replacement { spoken, written } => {
                    ensure!(
                        clean_text(spoken, 100)? == *spoken
                            && clean_text(written, 100)? == *written,
                        "Invalid preview"
                    );
                    self.check_dictionary(change)?;
                    format!("replacement:{}", folded(spoken))
                }
            };
            ensure!(seen.insert(id), "Duplicate change");
        }
        let updated = merge(config, changes)?;
        self.validator.conflicts(&updated, &keys)?;
        Ok(updated)
    }
    pub fn apply(&self, changes: &[Change]) -> Result<ApplyResult> {
        let config = self.load()?;
        let updated = self.replan(changes, &config)?;
        let previous = read_bounded(&self.undo_path)?;
        let mut record = Record {
            version: 1,
            created: Some(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64()),
            settings: vec![],
            words: vec![],
            replacements: vec![],
        };
        let result = (|| -> Result<()> {
            let settings: Vec<_> = changes
                .iter()
                .filter_map(|c| {
                    if let Change::Setting { key, before, value } = c {
                        Some(Setting {
                            key: key.clone(),
                            before: before.clone(),
                            after: value.clone(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            if !settings.is_empty() {
                config::save_config(&updated, Some(&self.config_path))?;
                record.settings = settings;
            }
            for c in changes {
                match c {
                    Change::Word(w) if self.dictionary.add_word(w)? => record.words.push(w.clone()),
                    Change::Replacement { spoken, written }
                        if self.dictionary.add_replacement(spoken, written)? =>
                    {
                        record.replacements.push(Replacement {
                            spoken: spoken.clone(),
                            written: written.clone(),
                        })
                    }
                    _ => (),
                }
            }
            if !record.settings.is_empty()
                || !record.words.is_empty()
                || !record.replacements.is_empty()
            {
                write_private(&self.undo_path, &serde_json::to_vec(&record)?)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let rollback = self.restore(&record);
            let undo_restored = match previous {
                Some(b) => write_private(&self.undo_path, &b).is_ok(),
                None => !self.undo_path.exists() || fs::remove_file(&self.undo_path).is_ok(),
            };
            ensure!(
                rollback.is_ok() && undo_restored,
                "Couldn't finish saving; some changes or undo state could not be restored"
            );
            bail!("Nothing was changed: couldn't save");
        }
        let applied: Vec<_> = changes
            .iter()
            .filter(|c| match c {
                Change::Setting { key, .. } => record.settings.iter().any(|s| s.key == *key),
                Change::Word(w) => record.words.contains(w),
                Change::Replacement { spoken, written } => record
                    .replacements
                    .iter()
                    .any(|r| r.spoken == *spoken && r.written == *written),
            })
            .cloned()
            .collect();
        let message = format!("Applied {} changes.", applied.len());
        Ok(ApplyResult {
            changes: applied,
            message,
        })
    }
    fn load_record(&self) -> Result<Option<Record>> {
        let Some(bytes) = read_bounded(&self.undo_path)? else {
            return Ok(None);
        };
        let check = (|| -> Result<Record> {
            let record: Record = serde_json::from_slice(&bytes)?;
            ensure!(
                record.version == 1
                    && record.settings.len() + record.words.len() + record.replacements.len()
                        <= MAX_UNDO_ENTRIES,
                "invalid record"
            );
            let config = self.load()?;
            let mut seen = HashSet::new();
            for s in &record.settings {
                ensure!(
                    seen.insert(format!("setting:{}", s.key)),
                    "duplicate setting"
                );
                for value in [&s.before, &s.after] {
                    ensure!(
                        self.validator.value(&s.key, value, &config)? == *value,
                        "invalid saved value"
                    );
                }
            }
            for w in &record.words {
                ensure!(
                    clean_text(w, 64)? == *w && seen.insert(format!("word:{}", folded(w))),
                    "invalid saved word"
                );
            }
            for r in &record.replacements {
                ensure!(
                    clean_text(&r.spoken, 100)? == r.spoken
                        && clean_text(&r.written, 100)? == r.written
                        && seen.insert(format!("replacement:{}", folded(&r.spoken))),
                    "invalid saved replacement"
                );
            }
            Ok(record)
        })();
        check
            .map(Some)
            .map_err(|_| anyhow::anyhow!("The undo record is unreadable; nothing was changed"))
    }
    fn restore(&self, record: &Record) -> Result<Vec<String>> {
        let config = self.load()?;
        let mut updated = config.clone();
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        for s in &record.settings {
            if config.field(&s.key) == Some(&s.after) {
                updated = update(&updated, &s.key, &s.before)?;
                keys.push(s.key.clone());
                lines.push(format!("Restored {}", safe_text(&s.key, 64)));
            }
        }
        self.validator.conflicts(&updated, &keys)?;
        if !keys.is_empty() {
            config::save_config(&updated, Some(&self.config_path))?;
        }
        for w in &record.words {
            if self.dictionary.remove_exact_word(w)? {
                lines.push(format!("Removed word “{}”", safe_text(w, 100)));
            }
        }
        for r in &record.replacements {
            if self
                .dictionary
                .remove_exact_replacement(&r.spoken, &r.written)?
            {
                lines.push(format!(
                    "Removed replacement “{}”",
                    safe_text(&r.spoken, 100)
                ));
            }
        }
        Ok(lines)
    }
    pub fn pending_undo(&self) -> Option<Vec<String>> {
        let r = self.load_record().ok()??;
        let mut lines: Vec<_> = r
            .settings
            .iter()
            .map(|s| {
                format!(
                    "{}: {} → {}",
                    safe_text(label(&s.key), 64),
                    display_value(&s.key, &s.after),
                    display_value(&s.key, &s.before)
                )
            })
            .collect();
        lines.extend(
            r.words
                .iter()
                .map(|w| format!("Remove word “{}”", safe_text(w, 100))),
        );
        lines.extend(r.replacements.iter().map(|r| {
            format!(
                "Remove replacement “{}” → “{}”",
                safe_text(&r.spoken, 100),
                safe_text(&r.written, 100)
            )
        }));
        if lines.is_empty() {
            None
        } else {
            Some(lines)
        }
    }
    pub fn undo(&self) -> Result<UndoResult> {
        let r = self.load_record()?.context("Nothing to undo")?;
        let lines = self.restore(&r)?;
        ensure!(
            !lines.is_empty(),
            "Nothing to undo; recorded values changed since applying"
        );
        fs::remove_file(&self.undo_path)?;
        let total = r.settings.len() + r.words.len() + r.replacements.len();
        let message = if lines.len() < total {
            format!("Undid {} changes. Left alone newer edits.", lines.len())
        } else {
            format!("Undid {} changes.", lines.len())
        };
        Ok(UndoResult { lines, message })
    }
}
