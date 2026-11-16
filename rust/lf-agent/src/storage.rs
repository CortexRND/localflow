use super::*;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
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
    version: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created: Option<Value>,
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
        Err(_) => return Ok(None),
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_UNDO_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Ok(None);
    }
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
        ensure!(!changes.is_empty(), "There's nothing to apply.");
        ensure!(
            changes.len() <= MAX_TOOL_CALLS,
            "Too many changes in one batch."
        );
        let mut seen = HashSet::new();
        let mut keys = Vec::new();
        let mut problems = Vec::new();
        for change in changes {
            let id = match change {
                Change::Setting { key, before, value } => {
                    ensure!(
                        self.validator.value(key, value, config)? == *value,
                        "This preview isn't valid; preview again."
                    );
                    if config.field(key) != Some(before) {
                        problems.push(format!(
                            "{} changed to {} since the preview",
                            label(key),
                            display_value(key, config.field(key).unwrap_or(&Value::Null))
                        ));
                        continue;
                    }
                    keys.push(key.clone());
                    format!("setting:{key}")
                }
                Change::Word(w) => {
                    ensure!(
                        clean_text(w, 64)? == *w,
                        "This preview isn't valid; preview again."
                    );
                    if let Err(e) = self.check_dictionary(change) {
                        problems.push(e.to_string());
                        continue;
                    }
                    format!("word:{}", folded(w))
                }
                Change::Replacement { spoken, written } => {
                    ensure!(
                        replacement_change(spoken, written)? == *change,
                        "This preview isn't valid; preview again."
                    );
                    if let Err(e) = self.check_dictionary(change) {
                        problems.push(e.to_string());
                        continue;
                    }
                    format!("replacement:{}", folded(spoken))
                }
            };
            if !seen.insert(id) {
                match change {
                    Change::Setting { .. } => bail!("This preview isn't valid; preview again."),
                    Change::Word(w) => {
                        problems.push(format!("“{w}” is already in your dictionary"))
                    }
                    Change::Replacement { spoken, .. } => {
                        problems.push(format!("“{spoken}” already has a replacement"))
                    }
                }
            }
        }
        ensure!(
            problems.is_empty(),
            "Nothing was changed: {}. Preview again.",
            problems.join("; ")
        );
        let updated = merge(config, changes)?;
        self.validator
            .conflicts(&updated, &keys)
            .map_err(|e| anyhow::anyhow!("Nothing was changed: {e}."))?;
        Ok(updated)
    }
    pub fn apply(&self, changes: &[Change]) -> Result<ApplyResult> {
        let config = self.load()?;
        let updated = self.replan(changes, &config)?;
        let previous = read_bounded(&self.undo_path)?;
        let mut record = Record {
            version: json!(1),
            created: Some(json!((self.clock)())),
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
                if let Change::Word(w) = c {
                    if self.dictionary.add_word(w)? {
                        record.words.push(w.clone());
                    }
                }
            }
            for c in changes {
                if let Change::Replacement { spoken, written } = c {
                    if self.dictionary.add_replacement(spoken, written)? {
                        record.replacements.push(Replacement {
                            spoken: spoken.clone(),
                            written: written.clone(),
                        });
                    }
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
        if let Err(error) = result {
            let leftovers = self.rollback(&record);
            let undo_restored = if read_bounded(&self.undo_path).ok().flatten() == previous {
                true
            } else {
                match previous {
                    Some(b) => write_private(&self.undo_path, &b).is_ok(),
                    None => !self.undo_path.exists() || fs::remove_file(&self.undo_path).is_ok(),
                }
            };
            let note = if undo_restored {
                ""
            } else {
                " The previous change can no longer be undone."
            };
            ensure!(leftovers.is_empty(), "Couldn't finish saving ({error}). These changes stayed and could not be undone automatically: {}.{note}", leftovers.join(", "));
            bail!("Nothing was changed: couldn't save ({error}).{note}");
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
        let message = if applied.is_empty() {
            "Nothing needed changing.".into()
        } else {
            format!(
                "Applied {} change{}.",
                applied.len(),
                if applied.len() == 1 { "" } else { "s" }
            )
        };
        Ok(ApplyResult {
            changes: applied,
            message,
        })
    }
    fn load_record(&self) -> Result<Option<Record>> {
        let Some(bytes) = read_bounded(&self.undo_path)? else {
            return Ok(None);
        };
        let bad = "The undo record is unreadable; nothing was changed.";
        ensure!(bytes.len() <= MAX_UNDO_BYTES, "{bad}");
        let mut record: Record =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("{bad}"))?;
        let check = (|| -> Result<()> {
            ensure!(
                (record.version.as_f64() == Some(1.0) || record.version == true)
                    && record.settings.len() + record.words.len() + record.replacements.len()
                        <= MAX_UNDO_ENTRIES,
                "invalid record"
            );
            let mut seen = HashSet::new();
            for s in &record.settings {
                ensure!(ALLOWLIST.contains(&s.key.as_str()), "unknown setting");
                ensure!(
                    seen.insert(format!("setting:{}", s.key)),
                    "duplicate setting"
                );
                let reference = Config::default();
                let expected = reference.field(&s.key).context("unknown field")?;
                ensure!(
                    std::mem::discriminant(expected) == std::mem::discriminant(&s.before)
                        && std::mem::discriminant(expected) == std::mem::discriminant(&s.after),
                    "wrong type"
                );
                ensure!(
                    [&s.before, &s.after]
                        .iter()
                        .all(|v| v.as_str().is_none_or(|s| s.chars().count() <= 200)),
                    "overlong saved value"
                );
            }
            for w in &mut record.words {
                *w = clean_text(w, 64)?;
                ensure!(
                    clean_text(w, 64)? == *w && seen.insert(format!("word:{}", folded(w))),
                    "invalid saved word"
                );
            }
            for r in &mut record.replacements {
                let Change::Replacement { spoken, written } =
                    replacement_change(&r.spoken, &r.written)?
                else {
                    unreachable!()
                };
                r.spoken = spoken;
                r.written = written;
                ensure!(
                    clean_text(&r.spoken, 100)? == r.spoken
                        && clean_text(&r.written, 100)? == r.written
                        && seen.insert(format!("replacement:{}", folded(&r.spoken))),
                    "invalid saved replacement"
                );
            }
            Ok(())
        })();
        check.map_err(|_| anyhow::anyhow!("{bad}"))?;
        let config = self.load()?;
        for s in &record.settings {
            for value in [&s.before, &s.after] {
                ensure!(self.validator.value(&s.key, value, &config).ok().as_ref() == Some(value), "The undo record holds a {} value LocalFlow can't restore ({}); nothing was changed. Change it on the Settings page instead.", label(&s.key), display_value(&s.key, value));
            }
        }
        Ok(Some(record))
    }
    fn rollback(&self, record: &Record) -> Vec<String> {
        let mut leftovers = Vec::new();
        for word in &record.words {
            if !self.dictionary.remove_exact_word(word).unwrap_or(false) {
                leftovers.push(format!("word “{word}”"));
            }
        }
        for r in &record.replacements {
            if !self
                .dictionary
                .remove_exact_replacement(&r.spoken, &r.written)
                .unwrap_or(false)
            {
                leftovers.push(format!("replacement “{}”", r.spoken));
            }
        }
        if !record.settings.is_empty() {
            let result = (|| -> Result<()> {
                let config = self.load()?;
                let mut updated = config.clone();
                let mut changed = false;
                for s in &record.settings {
                    if config.field(&s.key) == Some(&s.after) {
                        updated = update(&updated, &s.key, &s.before)?;
                        changed = true;
                    }
                }
                if changed {
                    config::save_config(&updated, Some(&self.config_path))?;
                }
                Ok(())
            })();
            if result.is_err() {
                leftovers.extend(record.settings.iter().map(|s| label(&s.key).to_string()));
            }
        }
        leftovers
    }
    fn restore(&self, record: &Record) -> Result<(Vec<String>, Vec<String>)> {
        let config = self.load()?;
        let mut updated = config.clone();
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        let mut skipped = Vec::new();
        for s in &record.settings {
            if config.field(&s.key) == Some(&s.after) {
                updated = update(&updated, &s.key, &s.before)?;
                keys.push(s.key.clone());
            } else {
                skipped.push(format!(
                    "{} (changed to {} since)",
                    label(&s.key),
                    display_value(&s.key, config.field(&s.key).unwrap_or(&Value::Null))
                ));
            }
        }
        self.validator
            .conflicts(&updated, &keys)
            .map_err(|e| anyhow::anyhow!("Can't undo without a shortcut clash: {e}."))?;
        let result = (|| -> Result<()> {
            if !keys.is_empty() {
                config::save_config(&updated, Some(&self.config_path))?;
                lines.extend(
                    record
                        .settings
                        .iter()
                        .filter(|s| keys.contains(&s.key))
                        .map(|s| {
                            format!("{}: {}", label(&s.key), display_value(&s.key, &s.before))
                        }),
                );
            }
            for w in &record.words {
                if self.dictionary.remove_exact_word(w)? {
                    lines.push(format!("Removed word “{}”", safe_text(w, 100)));
                } else {
                    skipped.push(format!("word “{w}” (edited or removed since)"));
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
                } else {
                    skipped.push(format!(
                        "replacement “{}” (edited or removed since)",
                        r.spoken
                    ));
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            bail!(
                "Undo stopped part way ({e}). Already undone: {}. The undo record was kept.",
                if lines.is_empty() {
                    "nothing".into()
                } else {
                    lines.join(", ")
                }
            );
        }
        Ok((lines, skipped))
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
        Some(lines)
    }
    pub fn undo(&self) -> Result<UndoResult> {
        let r = self.load_record()?.context("There's no change to undo.")?;
        let (lines, skipped) = self.restore(&r)?;
        let cleared = !self.undo_path.exists() || fs::remove_file(&self.undo_path).is_ok();
        ensure!(
            !lines.is_empty(),
            "Nothing to undo: everything was changed again since. {}",
            skipped.join("; ")
        );
        let mut message = "Undid the last change.".to_string();
        if !cleared {
            message
                .push_str(" (The undo record couldn't be cleared; undoing again changes nothing.)");
        }
        if !skipped.is_empty() {
            message.push_str(&format!(" Left alone: {}.", skipped.join("; ")));
        }
        Ok(UndoResult {
            lines,
            skipped,
            message,
        })
    }
}
