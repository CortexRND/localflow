//! Process-wide serialization of UI config read/modify/write transactions.

use std::cell::Cell;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use lf_core::config::{load_config, load_from, save_config, Config};

static WRITES: Mutex<()> = Mutex::new(());
thread_local! { static WRITING: Cell<bool> = const { Cell::new(false) }; }

/// Workers and adapters may nest transactions on the same thread.
pub fn with_lock<T>(write: impl FnOnce() -> T) -> T {
    if WRITING.get() {
        return write();
    }
    let _lock = WRITES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            WRITING.set(false);
        }
    }
    WRITING.set(true);
    let _reset = Reset;
    write()
}

pub fn update(
    path: Option<&Path>,
    change: impl FnOnce(&Config) -> Result<Config>,
) -> Result<Config> {
    with_lock(|| {
        let latest = match path {
            Some(path) => load_from(path)?,
            None => load_config()?,
        };
        let updated = change(&latest)?;
        save_config(&updated, path)?;
        Ok(updated)
    })
}

/// Merge a model/provider edit without restoring its unchanged snapshot fields.
pub fn merge(path: Option<&Path>, before: &Config, edited: &Config) -> Result<Config> {
    update(path, |latest| {
        let mut merged = latest.clone();
        for (key, value) in &edited.fields {
            if before.field(key) != Some(value) {
                merged.set_field(key, value.clone());
            }
        }
        for key in before.fields.keys() {
            if !edited.fields.contains_key(key) {
                merged.fields.remove(key);
            }
        }
        Ok(merged)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stale_model_edits_merge_only_changed_fields() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let before = Config::defaults_for(false);
        save_config(&before, Some(&path)).unwrap();
        update(Some(&path), |latest| {
            let mut updated = latest.clone();
            updated.set_field("dictation_enabled", json!(false));
            Ok(updated)
        })
        .unwrap();
        let mut edited = before.clone();
        edited.set_field("llm_model", json!("new-model"));
        let saved = merge(Some(&path), &before, &edited).unwrap();
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
        assert_eq!(saved.field_str("llm_model"), Some("new-model"));
        let unchanged = merge(Some(&path), &before, &before).unwrap();
        assert_eq!(unchanged.fields, saved.fields);
    }

    #[test]
    fn nested_transactions_release_the_lock_on_error() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        save_config(&Config::defaults_for(false), Some(&path)).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(with_lock(|| update(Some(&path), |_| anyhow::bail!("invalid edit"))).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        with_lock(|| update(Some(&path), |latest| Ok(latest.clone()))).unwrap();
    }
}
