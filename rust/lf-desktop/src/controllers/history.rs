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

/// One background History read, stamped when it was dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadTicket {
    /// The page's request generation (newer requests supersede it).
    pub generation: u64,
    /// `RetryJobs::seq` at dispatch: the retries already committed.
    pub retry_seq: u64,
}

/// What to do with a read when it arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// A newer request superseded it: drop its rows and its error.
    Obsolete,
    /// A retry committed while it ran, so its rows may predate that
    /// commit: read the same query and offset again instead.
    Reread,
    Apply,
}

impl ReadTicket {
    pub fn judge(&self, generation: u64, retry_seq: u64) -> Delivery {
        if self.generation != generation {
            Delivery::Obsolete
        } else if self.retry_seq != retry_seq {
            Delivery::Reread
        } else {
            Delivery::Apply
        }
    }
}

/// What one History read asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Read {
    pub offset: i64,
    pub limit: i64,
    /// A refresh of the loaded prefix: the selected entry stays shown
    /// (cited if it no longer matches) instead of being deselected.
    pub keep_selection: bool,
}

impl Read {
    /// One page from `offset` (0 = a new list).
    pub fn page(offset: i64) -> Self {
        Read {
            offset,
            limit: PAGE_SIZE,
            keep_selection: false,
        }
    }

    /// The first `len` rows again (at least a page), replacing the list.
    pub fn prefix(len: i64) -> Self {
        Read {
            offset: 0,
            limit: len.max(PAGE_SIZE),
            keep_selection: true,
        }
    }

    /// This read again after a retry commit crossed it. A retry can move a
    /// row into or out of a search, which shifts filtered offsets: a
    /// filtered append becomes a reread of everything it would have
    /// loaded. Unfiltered membership and order never change, and an offset
    /// 0 read already replaces the list.
    pub fn again(self, query: &str) -> Self {
        if self.offset > 0 && !query.trim().is_empty() {
            Read::prefix(self.offset + self.limit)
        } else {
            self
        }
    }
}

/// The read a finished retry needs, given the query the list shows, its
/// cursor, and the read in flight (if any). Unfiltered rows are just
/// replaced in place. A filtered list rereads its loaded prefix from the
/// store (the retried row may have left or joined the search), superseding
/// an in-flight append but never a new list or search, whose own fence
/// rereads it.
pub fn after_retry(listed_query: &str, cursor: i64, in_flight: Option<Read>) -> Option<Read> {
    if listed_query.trim().is_empty() {
        return None;
    }
    match in_flight {
        Some(read) if read.offset == 0 => None,
        Some(read) => Some(Read::prefix(cursor.max(read.offset + read.limit))),
        None => Some(Read::prefix(cursor)),
    }
}

/// Search `query` (empty = everything) from `offset`, one page. Blocking.
pub fn load(
    root: &DataRoot,
    generation: u64,
    query: &str,
    offset: i64,
    with_stats: bool,
) -> Result<Loaded> {
    read(root, generation, query, Read::page(offset), with_stats)
}

/// Search `query` for `read` (its offset and limit). Blocking.
pub fn read(
    root: &DataRoot,
    generation: u64,
    query: &str,
    read: Read,
    with_stats: bool,
) -> Result<Loaded> {
    let Read { offset, limit, .. } = read;
    let store = open_history(root)?;
    let q = Some(query.trim()).filter(|q| !q.is_empty());
    let mut entries = store.recent(limit + 1, q, offset)?;
    let more = entries.len() as i64 > limit;
    entries.truncate(limit as usize);
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

/// The History list: backend pages plus, separately, one entry opened from
/// elsewhere (an Ask citation, a Home row) that isn't in the loaded pages.
///
/// The page cursor counts only rows that came from backend pages, so opening
/// an older entry never shifts `Load more` (no skipped rows) and that entry
/// is never listed twice when its page arrives.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listing {
    /// Rows from backend pages, newest first, no duplicates.
    pub entries: Vec<HistoryEntry>,
    /// Backend offset of the next page.
    cursor: i64,
    pub more: bool,
    /// Shown in the detail pane only.
    cited: Option<HistoryEntry>,
}

impl Listing {
    pub fn next_offset(&self) -> i64 {
        self.cursor
    }

    /// Apply a loaded page (offset 0 replaces the list).
    pub fn apply(&mut self, loaded: Loaded) {
        if loaded.offset == 0 {
            self.entries.clear();
        }
        let fetched = loaded.entries.len() as i64;
        for entry in loaded.entries {
            // Rows added at the top between pages shift offsets; never list one twice.
            if !self.entries.iter().any(|e| e.id == entry.id) {
                self.entries.push(entry);
            }
        }
        self.cursor = loaded.offset + fetched;
        self.more = loaded.more;
        if self
            .cited
            .as_ref()
            .is_some_and(|c| self.entries.iter().any(|e| e.id == c.id))
        {
            self.cited = None; // its page arrived: it's a normal row now
        }
    }

    /// Apply a [`Read::prefix`] refresh. The `selected` entry (as shown
    /// now, with any in-place update) stays findable: cited if it is no
    /// longer in the pages.
    pub fn refresh(&mut self, loaded: Loaded, selected: Option<i64>) {
        let kept = selected.and_then(|id| self.find(id).cloned());
        self.apply(loaded);
        if let Some(entry) = kept.filter(|e| !self.in_pages(e.id)) {
            self.cited = Some(entry);
        }
    }

    /// Show an entry that isn't in the loaded pages (detail only).
    pub fn cite(&mut self, entry: HistoryEntry) {
        if self.entries.iter().any(|e| e.id == entry.id) {
            self.cited = None;
        } else {
            self.cited = Some(entry);
        }
    }

    pub fn in_pages(&self, id: i64) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    /// A listed or cited entry.
    pub fn find(&self, id: i64) -> Option<&HistoryEntry> {
        self.entries
            .iter()
            .find(|e| e.id == id)
            .or(self.cited.as_ref().filter(|c| c.id == id))
    }

    /// A row changed in the store (a finished retry): swap it in where it
    /// is, listed or cited. Order, the page cursor and which row is cited are
    /// unchanged; a row that is neither (same id but another timestamp, i.e.
    /// a reused id) is left alone. Returns whether anything was replaced.
    pub fn replace(&mut self, entry: HistoryEntry) -> bool {
        let same = |e: &HistoryEntry| e.id == entry.id && e.ts == entry.ts;
        if let Some(slot) = self.entries.iter_mut().find(|e| same(e)) {
            *slot = entry;
            return true;
        }
        match self.cited.as_mut() {
            Some(cited) if same(cited) => {
                *cited = entry;
                true
            }
            _ => false,
        }
    }

    /// The entry was deleted from the store. Returns its list index. A
    /// deleted paged row moves every later backend offset up by one.
    pub fn remove(&mut self, id: i64) -> Option<usize> {
        if self.cited.as_ref().is_some_and(|c| c.id == id) {
            self.cited = None;
        }
        let index = self.entries.iter().position(|e| e.id == id)?;
        self.entries.remove(index);
        self.cursor = (self.cursor - 1).max(0);
        Some(index)
    }
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

/// Single-line list preview: all whitespace runs (including newlines)
/// collapse to one space so the row ellipsizes instead of showing a break.
pub fn row_preview(entry: &HistoryEntry) -> String {
    row_text(entry)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
    fn row_preview_is_single_line() {
        let (_d, root) = seeded(1);
        let mut entry = load(&root, 1, "", 0, false).unwrap().entries.remove(0);
        entry.final_text = "  first line\n\nsecond\tline  ".into();
        assert_eq!(row_preview(&entry), "first line second line");
        assert!(entry.final_text.contains('\n'));
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

    fn ids(rows: &[HistoryEntry]) -> Vec<i64> {
        rows.iter().map(|e| e.id).collect()
    }

    /// 130 entries, an older one opened from a citation before paging: every
    /// page is complete and ordered, nothing is skipped or listed twice.
    #[test]
    fn citing_an_older_entry_does_not_disturb_paging() {
        let (_d, root) = seeded(130);
        let all = open_history(&root).unwrap().recent(1000, None, 0).unwrap();
        assert_eq!(all.len(), 130);
        let mut list = Listing::default();
        list.apply(load(&root, 1, "", 0, false).unwrap());
        assert_eq!(list.next_offset(), 50);
        // Open entry #120 (far beyond the first page) from a citation.
        let older = all[120].clone();
        assert!(!list.in_pages(older.id));
        list.cite(older.clone());
        assert_eq!(
            list.find(older.id),
            Some(&older),
            "shown in the detail pane"
        );
        assert_eq!(list.entries.len(), 50, "but not inserted into the list");
        assert_eq!(list.next_offset(), 50, "the page cursor is unchanged");
        while list.more {
            let offset = list.next_offset();
            list.apply(load(&root, 2, "", offset, false).unwrap());
        }
        assert_eq!(
            ids(&list.entries),
            ids(&all),
            "complete, ordered, no duplicates"
        );
        assert!(list.find(older.id).is_some(), "now a normal row");
        assert_eq!(list.entries.iter().filter(|e| e.id == older.id).count(), 1);
    }

    #[test]
    fn deleting_a_loaded_row_keeps_the_next_page_aligned() {
        let (_d, root) = seeded(120);
        let all = open_history(&root).unwrap().recent(1000, None, 0).unwrap();
        let mut list = Listing::default();
        list.apply(load(&root, 1, "", 0, false).unwrap());
        let victim = list.entries[10].id;
        assert!(delete(&root, victim).unwrap());
        assert_eq!(list.remove(victim), Some(10));
        while list.more {
            let offset = list.next_offset();
            list.apply(load(&root, 2, "", offset, false).unwrap());
        }
        let expected: Vec<i64> = ids(&all).into_iter().filter(|id| *id != victim).collect();
        assert_eq!(ids(&list.entries), expected);
        // Removing a cited (out-of-page) entry doesn't move the cursor.
        let mut list = Listing::default();
        list.apply(load(&root, 3, "", 0, false).unwrap());
        list.cite(all[100].clone());
        assert_eq!(list.remove(all[100].id), None);
        assert_eq!(list.next_offset(), 50);
        assert!(list.find(all[100].id).is_none());
    }

    #[test]
    fn replacing_a_retried_row_keeps_order_cursor_and_citation() {
        let (_d, root) = seeded(120);
        let all = open_history(&root).unwrap().recent(1000, None, 0).unwrap();
        let mut list = Listing::default();
        list.apply(load(&root, 1, "", 0, false).unwrap());
        list.cite(all[100].clone());
        let order = ids(&list.entries);
        let mut retried = list.entries[7].clone();
        retried.final_text = "Retried.".into();
        assert!(list.replace(retried.clone()));
        assert_eq!(ids(&list.entries), order);
        assert_eq!(list.entries[7], retried);
        assert_eq!(list.next_offset(), 50);
        let mut cited = all[100].clone();
        cited.status = "ok".into();
        assert!(list.replace(cited.clone()));
        assert_eq!(list.find(cited.id), Some(&cited));
        assert_eq!(list.next_offset(), 50, "still cited, not paged");
        assert!(!list.in_pages(cited.id));
        // A reused id (another timestamp) or an unknown row changes nothing.
        let before = list.clone();
        let mut reused = retried.clone();
        reused.ts += 5.0;
        assert!(!list.replace(reused));
        assert!(!list.replace(all[110].clone()));
        assert_eq!(list, before);
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
