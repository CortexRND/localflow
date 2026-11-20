//! Dictionary edits through the existing `hub_model` submit/remove helpers,
//! which own the validation and the friendly error text.

use lf_core::dictionary::DictionaryStore;
use lf_ui::hub_model::{self, DictionaryRow, EntryKind};

/// Rows filtered in memory (case-insensitive).
pub fn rows(store: &DictionaryStore, filter: &str) -> (Vec<DictionaryRow>, Vec<DictionaryRow>) {
    let filter = filter.trim().to_lowercase();
    let all = hub_model::dictionary_entries(&store.list());
    let keep = |r: &DictionaryRow| {
        filter.is_empty()
            || r.key.to_lowercase().contains(&filter)
            || r.target.to_lowercase().contains(&filter)
    };
    let words = all
        .iter()
        .filter(|r| r.kind == EntryKind::Word && keep(r))
        .cloned()
        .collect();
    let replacements = all
        .iter()
        .filter(|r| r.kind == EntryKind::Replacement && keep(r))
        .cloned()
        .collect();
    (words, replacements)
}

/// Add or rename a word; `Err(message)` is shown under the field.
pub fn submit_word(
    store: &DictionaryStore,
    text: &str,
    editing: Option<&str>,
) -> Result<(), String> {
    match hub_model::submit_word(store, text, editing) {
        None => Ok(()),
        Some(message) => Err(message),
    }
}

pub fn submit_replacement(
    store: &DictionaryStore,
    from: &str,
    to: &str,
    editing: Option<&str>,
) -> Result<(), String> {
    match hub_model::submit_replacement(store, from, to, editing) {
        None => Ok(()),
        Some(message) => Err(message),
    }
}

pub fn remove(store: &DictionaryStore, row: &DictionaryRow) -> Result<(), String> {
    match hub_model::remove_entry(store, &row.key, Some(row.kind)) {
        None => Ok(()),
        Some(message) => Err(message),
    }
}

/// Re-add a removed row (the Undo of a removal).
pub fn restore(store: &DictionaryStore, row: &DictionaryRow) -> Result<(), String> {
    match row.kind {
        EntryKind::Word => submit_word(store, &row.key, None),
        EntryKind::Replacement => submit_replacement(store, &row.key, &row.target, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_is_validated_and_durable_in_the_existing_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dictionary.json");
        let store = DictionaryStore::new(Some(path.clone()));
        assert!(submit_word(&store, "   ", None).is_err());
        submit_word(&store, "Kubernetes", None).unwrap();
        assert!(submit_word(&store, "Kubernetes", None)
            .unwrap_err()
            .contains("already"));
        submit_replacement(&store, "k8s", "Kubernetes", None).unwrap();
        assert!(submit_replacement(&store, "x", " ", None).is_err());
        submit_word(&store, "Kubernetes2", Some("Kubernetes")).unwrap();
        // Reopen from disk: same JSON store the runtime reads.
        let reopened = DictionaryStore::new(Some(path.clone()));
        let (words, reps) = rows(&reopened, "");
        assert_eq!(
            words.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
            ["Kubernetes2"]
        );
        assert_eq!(reps[0].target, "Kubernetes");
        let text = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(json.get("words").is_some() && json.get("replacements").is_some());
        let (_, filtered) = rows(&reopened, "K8S");
        assert_eq!(filtered.len(), 1);
        remove(&reopened, &reps[0]).unwrap();
        assert!(rows(&reopened, "").1.is_empty());
        restore(&reopened, &reps[0]).unwrap();
        assert_eq!(rows(&reopened, "").1.len(), 1);
    }
}
