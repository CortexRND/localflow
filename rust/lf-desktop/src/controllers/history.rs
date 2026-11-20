//! History queries (run on the background executor) and row helpers.

use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use lf_core::history::{HistoryEntry, Stats};
use lf_ui::hub_model;

use crate::data::{open_history, DataRoot};

pub const PAGE_SIZE: i64 = 50;

/// One background load: a page of results plus the stats.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub generation: u64,
    pub query: String,
    pub offset: i64,
    pub entries: Vec<HistoryEntry>,
    /// A further page exists.
    pub more: bool,
    pub stats: Option<Stats>,
}

/// Search `query` (empty = everything) from `offset`. Blocking.
pub fn load(
    root: &DataRoot,
    generation: u64,
    query: &str,
    offset: i64,
    with_stats: bool,
) -> Result<Loaded> {
    let store = open_history(root)?;
    let q = Some(query.trim()).filter(|q| !q.is_empty());
    let mut entries = store.recent(PAGE_SIZE + 1, q, offset)?;
    let more = entries.len() as i64 > PAGE_SIZE;
    entries.truncate(PAGE_SIZE as usize);
    let stats = if with_stats {
        Some(store.stats(None)?)
    } else {
        None
    };
    Ok(Loaded {
        generation,
        query: query.to_string(),
        offset,
        entries,
        more,
        stats,
    })
}

pub fn now() -> NaiveDateTime {
    Local::now().naive_local()
}

/// "Today", "Yesterday" or a date, for day separators.
pub fn day_label(ts: f64, now: NaiveDateTime) -> String {
    let when = hub_model::history_when(ts).date();
    let today = now.date();
    if when == today {
        "Today".into()
    } else if Some(when) == today.pred_opt() {
        "Yesterday".into()
    } else {
        when.format("%A, %B %-d").to_string()
    }
}

/// Time of day for a row.
pub fn time_label(ts: f64) -> String {
    hub_model::history_when(ts).format("%-I:%M %p").to_string()
}

/// The text shown for an entry.
pub fn row_text(entry: &HistoryEntry) -> String {
    hub_model::history_row_text(&entry.final_text, &entry.raw_text)
}

/// Whether "Copy original" adds anything.
pub fn has_distinct_original(entry: &HistoryEntry) -> bool {
    !entry.raw_text.is_empty() && entry.raw_text != entry.final_text
}

pub fn delete(root: &DataRoot, id: i64) -> Result<bool> {
    open_history(root)?.delete(id)
}

pub fn clear(root: &DataRoot) -> Result<i64> {
    open_history(root)?.clear()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_core::history::NewEntry;

    fn seeded(n: usize) -> (tempfile::TempDir, DataRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::sandbox(dir.path());
        let store = open_history(&root).unwrap();
        for i in 0..n {
            let mut e = NewEntry::new("dictate");
            e.raw_text = format!("raw {i}");
            e.final_text = if i % 2 == 0 {
                format!("Even item {i}.")
            } else {
                format!("Odd item {i}.")
            };
            e.ts = Some(1_800_000_000.0 + i as f64);
            store.add(&e).unwrap();
        }
        (dir, root)
    }

    #[test]
    fn pages_and_searches() {
        let (_d, root) = seeded(120);
        let first = load(&root, 1, "", 0, true).unwrap();
        assert_eq!(first.entries.len(), PAGE_SIZE as usize);
        assert!(first.more);
        assert_eq!(first.stats.unwrap().total_dictations, 120);
        let last = load(&root, 2, "", 100, false).unwrap();
        assert_eq!(last.entries.len(), 20);
        assert!(!last.more);
        let odd = load(&root, 3, "odd", 0, false).unwrap();
        assert!(odd.entries.iter().all(|e| e.final_text.starts_with("Odd")));
        assert_eq!(odd.entries.len(), PAGE_SIZE as usize);
        let none = load(&root, 4, "zzz-no-match", 0, false).unwrap();
        assert!(none.entries.is_empty());
    }

    #[test]
    fn delete_and_clear_are_durable() {
        let (_d, root) = seeded(3);
        let id = load(&root, 1, "", 0, false).unwrap().entries[0].id;
        assert!(delete(&root, id).unwrap());
        assert!(!delete(&root, id).unwrap());
        assert_eq!(load(&root, 2, "", 0, false).unwrap().entries.len(), 2);
        assert_eq!(clear(&root).unwrap(), 2);
        assert!(load(&root, 3, "", 0, false).unwrap().entries.is_empty());
    }

    #[test]
    fn original_is_distinct_only_when_different() {
        let (_d, root) = seeded(1);
        let mut e = load(&root, 1, "", 0, false).unwrap().entries.remove(0);
        assert!(has_distinct_original(&e));
        e.raw_text = e.final_text.clone();
        assert!(!has_distinct_original(&e));
    }
}
