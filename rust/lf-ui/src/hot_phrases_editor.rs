//! AppKit-free state and layout behind the Hot Phrases page (port of
//! `localflow/hot_phrases_editor.py`).
//!
//! A native view only renders [`HotPhrasesEditor`] and forwards user events to
//! it. Everything that must be testable without a window lives here:
//! autosave debounce, validation through the phrases model, the navigation
//! guard, selection, search, delete/undo, external-change reconciliation and
//! the dictation preview. Nothing here touches a UI toolkit or the network.
//!
//! Vocabulary used below:
//!   * "pending": the editor values differ from what was last loaded/committed.
//!   * "draft": a phrase that has no saved record yet (`current_id` is `None`).
//!
//! Local edits always win over external changes, and an invalid edit is never
//! written: it stays in the editor with an inline message.

use std::time::Instant;

use anyhow::Result;
use lf_core::hotphrases::Entry;
use lf_core::phrases_model::PhrasesModel;
use lf_core::symbols::pycompat::strip;
use serde_json::Value;

pub const AUTOSAVE_DELAY: f64 = 0.6;
pub const POLL_INTERVAL: f64 = 1.5;
pub const UNDO_SECONDS: f64 = 6.0;

pub const EMPTY_MESSAGE: &str = "Say a word, get a saved prompt. Add your first hot phrase.";
pub const READ_ERROR: &str =
    "Couldn't read your hot phrases. Check that ~/.localflow is readable, then try again.";
pub const WRITE_ERROR: &str =
    "Couldn't save your hot phrases. Your edits are kept here; try again.";
pub const DELETED_ELSEWHERE: &str =
    "This phrase was deleted elsewhere. Your edits will be saved as a new phrase.";
pub const PREVIEW_PROMPT: &str = "Type a test phrase to see what dictation would paste.";
pub const PREVIEW_ERROR: &str = "Preview isn't available right now.";
/// Drawn (never inserted) in the empty read-only preview box.
pub const PREVIEW_PLACEHOLDER: &str = "What dictation would paste appears here.";
pub const PREVIEW_EMPTY_RESULT: &str = "Nothing would be pasted.";
pub const ENABLED_ACCESSIBILITY: &str = "Phrase enabled";
pub const DISABLED_BANNER: &str =
    "Hot phrases are turned off in your settings, so they won't expand yet.";

pub const EXAMPLE_TRIGGER: &str = "review checklist";
pub const EXAMPLE_TEXT: &str = "Review this PR:\n\
- correctness and edge cases\n\
- tests cover the change\n\
- no unrelated churn\n\
Report findings by severity.";

/// The text beside the enabled switch for its current state ("Off" fits
/// [`ENABLED_LABEL_W`]).
pub fn enabled_label(enabled: bool) -> &'static str {
    if enabled {
        "On"
    } else {
        "Off"
    }
}

/// Muted text drawn in the preview box only while it has no output.
///
/// No test phrase: say what the box is for. A test phrase whose result is
/// empty: say so, rather than leaving a blank box. Real output: nothing (the
/// output itself is shown, unchanged). A preview error is already in the
/// status line, so the box stays plain.
pub fn preview_placeholder(test_input: &str, output: &str, status: &str) -> &'static str {
    if !output.is_empty() {
        return "";
    }
    if strip(test_input).is_empty() {
        return PREVIEW_PLACEHOLDER;
    }
    if status == PREVIEW_ERROR {
        return "";
    }
    PREVIEW_EMPTY_RESULT
}

// ------------------------------------------------------------------ model ---

/// What the editor needs from the phrases model. [`PhrasesModel`] implements
/// it; tests (and a future platform layer) can wrap or replace it, e.g. to
/// inject store failures.
pub trait PhrasesBackend {
    fn entries(&self, query: &str) -> Result<Vec<Entry>>;
    fn get(&self, entry_id: &str) -> Result<Option<Entry>>;
    fn trigger_hint(&self, trigger: &str, exclude_id: Option<&str>) -> Result<(bool, String)>;
    fn text_hint(&self, text: &str) -> Result<(bool, String)>;
    fn commit(
        &self,
        entry_id: Option<&str>,
        trigger: &str,
        text: &str,
        enabled: bool,
    ) -> Result<(Option<Entry>, Option<String>)>;
    fn set_enabled(&self, entry_id: &str, enabled: bool) -> Result<Option<Entry>>;
    fn delete(&self, entry_id: &str) -> Result<Option<Entry>>;
    fn undo_delete(&self, token: &Entry) -> Result<(Option<Entry>, Option<String>)>;
    fn preview(&self, said: &str) -> Result<(String, Vec<String>)>;
    fn changed_on_disk(&mut self) -> Result<bool>;
    fn hot_phrases_enabled(&self) -> bool;
}

impl PhrasesBackend for PhrasesModel {
    fn entries(&self, query: &str) -> Result<Vec<Entry>> {
        PhrasesModel::entries(self, query)
    }
    fn get(&self, entry_id: &str) -> Result<Option<Entry>> {
        PhrasesModel::get(self, entry_id)
    }
    fn trigger_hint(&self, trigger: &str, exclude_id: Option<&str>) -> Result<(bool, String)> {
        PhrasesModel::trigger_hint(self, trigger, exclude_id)
    }
    fn text_hint(&self, text: &str) -> Result<(bool, String)> {
        Ok(PhrasesModel::text_hint(self, text))
    }
    fn commit(
        &self,
        entry_id: Option<&str>,
        trigger: &str,
        text: &str,
        enabled: bool,
    ) -> Result<(Option<Entry>, Option<String>)> {
        PhrasesModel::commit(self, entry_id, trigger, text, enabled)
    }
    fn set_enabled(&self, entry_id: &str, enabled: bool) -> Result<Option<Entry>> {
        PhrasesModel::set_enabled(self, entry_id, enabled)
    }
    fn delete(&self, entry_id: &str) -> Result<Option<Entry>> {
        PhrasesModel::delete(self, entry_id)
    }
    fn undo_delete(&self, token: &Entry) -> Result<(Option<Entry>, Option<String>)> {
        PhrasesModel::undo_delete(self, token)
    }
    fn preview(&self, said: &str) -> Result<(String, Vec<String>)> {
        PhrasesModel::preview(self, said)
    }
    fn changed_on_disk(&mut self) -> Result<bool> {
        Ok(PhrasesModel::changed_on_disk(self))
    }
    fn hot_phrases_enabled(&self) -> bool {
        PhrasesModel::hot_phrases_enabled(self)
    }
}

/// One list row for the view.
#[derive(Clone, Debug, PartialEq)]
pub struct PhraseRow {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub enabled: bool,
    pub selected: bool,
    pub accessibility: String,
}

fn field(entry: &Entry, key: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn entry_enabled(entry: &Entry) -> bool {
    match entry.get("enabled") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `str.splitlines()` (without keepends): splits on `\n`, `\r`, `\r\n`,
/// `\v`, `\f`, `\x1c`-`\x1e`, `\x85`, ` ` and ` `; no trailing empty
/// line.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let is_break = matches!(
            ch,
            '\n' | '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        lines.push(&text[start..index]);
        let mut end = index + ch.len_utf8();
        if ch == '\r' {
            if let Some(&(next, '\n')) = chars.peek() {
                end = next + 1;
                chars.next();
            }
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

type Values = (String, String, bool);

/// State and actions for the embedded editor; the clock is injectable.
pub struct HotPhrasesEditor<M: PhrasesBackend = PhrasesModel> {
    pub model: M,
    clock: Box<dyn Fn() -> f64>,
    pub query: String,
    pub current_id: Option<String>,
    pub draft: bool,
    pub trigger: String,
    pub text: String,
    pub enabled: bool,
    pub loaded: (String, String, bool),
    pub touched: bool,
    pub error: String,
    pub notice: String,
    pub undo_token: Option<Entry>,
    pub undo_due: Option<f64>,
    pub autosave_due: Option<f64>,
    pub next_poll: f64,
    pub test_input: String,
    pub all_entries: Vec<Entry>,
    pub entries: Vec<Entry>,
    /// The view pushes trigger/text/enabled into its controls when this is set
    /// (programmatic loads), then calls [`ack_fields`](Self::ack_fields).
    /// Typing never sets it, so the caret is never disturbed.
    pub fields_stale: bool,
}

/// A monotonic clock in seconds (the Python default `time.monotonic`).
pub fn monotonic_clock() -> Box<dyn Fn() -> f64> {
    let origin = Instant::now();
    Box::new(move || origin.elapsed().as_secs_f64())
}

impl<M: PhrasesBackend> HotPhrasesEditor<M> {
    pub fn new(model: M) -> Self {
        Self::with_clock(model, monotonic_clock())
    }

    pub fn with_clock(model: M, clock: Box<dyn Fn() -> f64>) -> Self {
        let mut editor = HotPhrasesEditor {
            model,
            clock,
            query: String::new(),
            current_id: None,
            draft: false,
            trigger: String::new(),
            text: String::new(),
            enabled: true,
            loaded: (String::new(), String::new(), true),
            touched: false,
            error: String::new(),
            notice: String::new(),
            undo_token: None,
            undo_due: None,
            autosave_due: None,
            next_poll: 0.0,
            test_input: String::new(),
            all_entries: Vec::new(),
            entries: Vec::new(),
            fields_stale: true,
        };
        editor.reload();
        editor
    }

    fn now(&self) -> f64 {
        (self.clock)()
    }

    // ------------------------------------------------------------ helpers ---

    /// `Some(value)`, or `None` with a friendly `error` set.
    fn call<T>(&mut self, message: &str, f: impl FnOnce(&mut M) -> Result<T>) -> Option<T> {
        match f(&mut self.model) {
            Ok(value) => Some(value),
            Err(_) => {
                self.error = message.to_string();
                None
            }
        }
    }

    pub fn ack_fields(&mut self) {
        self.fields_stale = false;
    }

    fn values(&self) -> Values {
        (
            strip(&self.trigger).to_string(),
            self.text.clone(),
            self.enabled,
        )
    }

    pub fn pending(&self) -> bool {
        self.values() != self.loaded
    }

    pub fn blank(&self) -> bool {
        strip(&self.trigger).is_empty() && strip(&self.text).is_empty()
    }

    pub fn is_untouched_draft(&self) -> bool {
        self.current_id.is_none() && self.blank()
    }

    pub fn empty_state_visible(&self) -> bool {
        self.all_entries.is_empty() && self.current_id.is_none() && !self.draft
    }

    pub fn editor_visible(&self) -> bool {
        !self.empty_state_visible()
    }

    /// Re-read the entry list. A failing store keeps the previous list.
    pub fn reload(&mut self) -> bool {
        let Some(everything) = self.call(READ_ERROR, |m| m.entries("")) else {
            return false;
        };
        self.all_entries = everything;
        if !self.query.is_empty() {
            let query = self.query.clone();
            if let Some(visible) = self.call(READ_ERROR, |m| m.entries(&query)) {
                self.entries = visible;
            }
        } else {
            self.entries = self.all_entries.clone();
        }
        true
    }

    /// List rows for the view: id, title, subtitle, enabled, selected.
    pub fn rows(&self) -> Vec<PhraseRow> {
        self.entries
            .iter()
            .map(|entry| {
                let text = field(entry, "text");
                let first = py_splitlines(&text)
                    .into_iter()
                    .map(strip)
                    .find(|line| !line.is_empty())
                    .unwrap_or("")
                    .to_string();
                let enabled = entry_enabled(entry);
                let id = field(entry, "id");
                let trigger = field(entry, "trigger");
                PhraseRow {
                    selected: Some(&id) == self.current_id.as_ref(),
                    accessibility: format!("{trigger}, {}", if enabled { "on" } else { "off" }),
                    id,
                    title: trigger,
                    subtitle: first,
                    enabled,
                }
            })
            .collect()
    }

    pub fn no_matches(&self) -> bool {
        !self.query.is_empty() && self.entries.is_empty() && !self.all_entries.is_empty()
    }

    // -------------------------------------------------------------- hints ---

    /// (ok, message). The duplicate check reads the store, which can fail
    /// (permissions, lock); that becomes a friendly read error, never a raise.
    fn check_trigger(&self) -> (bool, String) {
        self.model
            .trigger_hint(&self.trigger, self.current_id.as_deref())
            .unwrap_or((false, READ_ERROR.to_string()))
    }

    fn check_text(&self) -> (bool, String) {
        self.model
            .text_hint(&self.text)
            .unwrap_or((false, READ_ERROR.to_string()))
    }

    /// `(message, is_error)`. Neutral until the user has touched the form.
    pub fn trigger_hint(&self) -> (String, bool) {
        let (ok, message) = self.check_trigger();
        (message, !ok && (self.touched || !self.error.is_empty()))
    }

    pub fn text_hint(&self) -> (String, bool) {
        let (ok, message) = self.check_text();
        (message, !ok && (self.touched || !self.error.is_empty()))
    }

    fn first_problem(&self) -> String {
        for (ok, message) in [self.check_trigger(), self.check_text()] {
            if !ok {
                return message;
            }
        }
        String::new()
    }

    pub fn banner(&self) -> &'static str {
        if self.model.hot_phrases_enabled() {
            ""
        } else {
            DISABLED_BANNER
        }
    }

    // ------------------------------------------------------------ editing ---

    /// The user changed a field. Debounces an autosave.
    pub fn edit(&mut self, trigger: Option<&str>, text: Option<&str>, enabled: Option<bool>) {
        if let Some(trigger) = trigger {
            self.trigger = trigger.to_string();
        }
        if let Some(text) = text {
            self.text = text.to_string();
        }
        if let Some(enabled) = enabled {
            self.enabled = enabled;
        }
        self.touched = true;
        self.notice.clear();
        self.error.clear();
        self.autosave_due = if self.pending() {
            Some(self.now() + AUTOSAVE_DELAY)
        } else {
            None
        };
    }

    /// Programmatic typing (snapshots, tests): like [`edit`](Self::edit), but
    /// the view is told to push the values into its controls, which hold
    /// stale text.
    pub fn fill(&mut self, trigger: &str, text: &str) {
        self.edit(Some(trigger), Some(text), None);
        self.fields_stale = true;
    }

    /// `Ok(entry)` or `Err(message)`; handles a record deleted elsewhere.
    fn commit_current(&mut self) -> std::result::Result<Entry, String> {
        if let Some(id) = self.current_id.clone() {
            match self.call(WRITE_ERROR, |m| m.get(&id)) {
                None => return Err(self.error.clone()),
                Some(None) => {
                    self.current_id = None;
                    self.draft = true;
                }
                Some(Some(_)) => {}
            }
        }
        let (id, trigger, text, enabled) = (
            self.current_id.clone(),
            self.trigger.clone(),
            self.text.clone(),
            self.enabled,
        );
        let Some((entry, message)) = self.call(WRITE_ERROR, |m| {
            m.commit(id.as_deref(), &trigger, &text, enabled)
        }) else {
            return Err(self.error.clone());
        };
        match (entry, message) {
            (Some(entry), None) => Ok(entry),
            (_, Some(message)) => Err(message),
            (None, None) => Err(String::new()),
        }
    }

    fn adopt(&mut self, entry: &Entry) {
        self.current_id = Some(field(entry, "id"));
        self.draft = false;
        self.loaded = (
            field(entry, "trigger"),
            field(entry, "text"),
            entry_enabled(entry),
        );
        self.error.clear();
        self.notice = "Saved ✓".to_string();
        self.autosave_due = None;
        self.reload();
    }

    /// Commit now. `true` when nothing remains unsaved; invalid edits stay
    /// local and set `error`.
    pub fn flush(&mut self) -> bool {
        self.autosave_due = None;
        if self.is_untouched_draft() {
            return true;
        }
        if !self.pending() {
            return true;
        }
        let problem = self.first_problem();
        if !problem.is_empty() {
            self.error = problem;
            return false;
        }
        match self.commit_current() {
            Ok(entry) => {
                self.adopt(&entry);
                true
            }
            Err(message) => {
                self.error = if message.is_empty() {
                    WRITE_ERROR.to_string()
                } else {
                    message
                };
                false
            }
        }
    }

    /// The switch changed: persist it right away (invalid drafts stay local).
    pub fn set_enabled(&mut self, value: bool) -> bool {
        self.enabled = value;
        self.touched = true;
        self.notice.clear();
        self.error.clear();
        let Some(id) = self.current_id.clone() else {
            return self.flush();
        };
        let Some(existing) = self.call(WRITE_ERROR, |m| m.get(&id)) else {
            return false;
        };
        if existing.is_none() {
            self.current_id = None;
            self.draft = true;
            return self.flush();
        }
        if value == self.loaded.2 {
            return true;
        }
        let Some(updated) = self.call(WRITE_ERROR, |m| m.set_enabled(&id, value)) else {
            return false;
        };
        if updated.is_none() {
            self.current_id = None;
            self.draft = true;
            return self.flush();
        }
        // Only the switch was persisted: other pending edits stay pending.
        self.loaded.2 = value;
        self.notice = "Saved ✓".to_string();
        self.reload();
        if self.pending() {
            self.autosave_due = Some(self.now() + AUTOSAVE_DELAY);
        }
        true
    }

    // --------------------------------------------------------- navigation ---

    /// Guard for leaving the editor (page change, selection, new phrase,
    /// window close). Commits a valid dirty draft; `true` means "go ahead".
    ///
    /// An untouched empty draft never traps the user. An invalid edit keeps
    /// the user here with a friendly inline message and nothing is lost.
    pub fn leave(&mut self) -> bool {
        self.autosave_due = None;
        if self.is_untouched_draft() {
            self.draft = false;
            self.error.clear();
            return true;
        }
        if !self.pending() {
            return true;
        }
        if self.flush() {
            return true;
        }
        let message = if self.error.is_empty() {
            WRITE_ERROR.to_string()
        } else {
            self.error.clone()
        };
        self.error = if message == READ_ERROR {
            format!("{message} Your edits are kept here.")
        } else if message != WRITE_ERROR {
            // WRITE_ERROR already says the edits are kept.
            format!("{message} Fix it to continue, or clear the fields.")
        } else {
            message
        };
        false
    }

    /// Stop pending autosave/undo timers (page hidden or window closed).
    pub fn cancel(&mut self) {
        self.autosave_due = None;
    }

    fn load(&mut self, entry: &Entry) {
        self.current_id = Some(field(entry, "id"));
        self.draft = false;
        self.trigger = field(entry, "trigger");
        self.text = field(entry, "text");
        self.enabled = entry_enabled(entry);
        self.loaded = (self.trigger.clone(), self.text.clone(), self.enabled);
        self.touched = false;
        self.error.clear();
        self.notice.clear();
        self.autosave_due = None;
        self.fields_stale = true;
    }

    fn clear(&mut self, draft: bool) {
        self.current_id = None;
        self.draft = draft;
        self.trigger.clear();
        self.text.clear();
        self.enabled = true;
        self.loaded = (String::new(), String::new(), true);
        self.touched = false;
        self.error.clear();
        self.autosave_due = None;
        self.fields_stale = true;
    }

    /// Select the neighbour at `old_index` (clamped), else show empty.
    fn settle(&mut self, old_index: usize) {
        let pool = if self.entries.is_empty() {
            &self.all_entries
        } else {
            &self.entries
        };
        if pool.is_empty() {
            self.clear(false);
        } else {
            let entry = pool[old_index.min(pool.len() - 1)].clone();
            self.load(&entry);
        }
    }

    fn index_of(&self, entry_id: &str) -> usize {
        self.entries
            .iter()
            .position(|e| field(e, "id") == entry_id)
            .unwrap_or(0)
    }

    pub fn select(&mut self, entry_id: &str) -> bool {
        if self.current_id.as_deref() == Some(entry_id) {
            return true;
        }
        if !self.leave() {
            return false;
        }
        let Some(entry) = self.call(WRITE_ERROR, |m| m.get(entry_id)) else {
            return false;
        };
        match entry {
            None => {
                self.reload();
                self.error.clear();
                true
            }
            Some(entry) => {
                self.load(&entry);
                true
            }
        }
    }

    pub fn new_phrase(&mut self) -> bool {
        if self.current_id.is_none() && self.draft && self.blank() {
            return true;
        }
        if !self.leave() {
            return false;
        }
        self.clear(true);
        self.notice.clear();
        true
    }

    pub fn add_example(&mut self) -> bool {
        if !self.leave() {
            return false;
        }
        let Some((entry, message)) = self.call(WRITE_ERROR, |m| {
            m.commit(None, EXAMPLE_TRIGGER, EXAMPLE_TEXT, true)
        }) else {
            return false;
        };
        let Some(entry) = entry else {
            self.error = message.unwrap_or_else(|| WRITE_ERROR.to_string());
            return false;
        };
        self.reload();
        self.load(&entry);
        true
    }

    /// Search is live. It filters the list only: the editor (and any invalid
    /// draft in it) is untouched, so nothing can be lost.
    pub fn set_query(&mut self, query: &str) {
        self.query = query.to_string();
        self.reload();
    }

    // ------------------------------------------------------ delete / undo ---

    pub fn delete(&mut self) -> bool {
        let Some(id) = self.current_id.clone() else {
            if self.draft || !self.blank() {
                self.clear(false);
                self.reload();
                self.settle(0);
            }
            return true;
        };
        let old_index = self.index_of(&id);
        let Some(token) = self.call(WRITE_ERROR, |m| m.delete(&id)) else {
            return false;
        };
        if let Some(token) = token {
            self.undo_token = Some(token);
            self.undo_due = Some(self.now() + UNDO_SECONDS);
        }
        self.reload();
        self.settle(old_index);
        true
    }

    pub fn undo_delete(&mut self) -> bool {
        let Some(token) = self.undo_token.clone() else {
            return false;
        };
        // The token is cleared only on success (or expiry), so a failed restore
        // can be retried while the Undo bar is still showing.
        let Some((entry, message)) = self.call(WRITE_ERROR, |m| m.undo_delete(&token)) else {
            return false;
        };
        let Some(entry) = entry else {
            self.error = message.unwrap_or_else(|| WRITE_ERROR.to_string());
            return false;
        };
        self.undo_token = None;
        self.undo_due = None;
        self.error.clear();
        self.reload();
        if !self.select(&field(&entry, "id")) {
            self.notice = format!("Restored “{}”", field(&entry, "trigger"));
        }
        true
    }

    pub fn undo_label(&self) -> String {
        match &self.undo_token {
            None => String::new(),
            Some(token) => format!("Deleted “{}”", field(token, "trigger")),
        }
    }

    /// `(error shown, Undo shown)`: the view re-lays out only when this changes.
    pub fn notice_visibility(&self) -> (bool, bool) {
        (!self.error.is_empty(), self.undo_token.is_some())
    }

    // ---------------------------------------------------- external changes ---

    /// The page became visible: prime the disk signature, reconcile.
    pub fn on_show(&mut self) {
        self.call(READ_ERROR, |m| m.changed_on_disk());
        self.next_poll = self.now() + POLL_INTERVAL;
        self.reconcile();
    }

    pub fn poll(&mut self) -> bool {
        match self.call(READ_ERROR, |m| m.changed_on_disk()) {
            Some(true) => {
                self.reconcile();
                true
            }
            _ => false,
        }
    }

    /// Merge the on-disk state: dirty local edits win, clean ones refresh.
    pub fn reconcile(&mut self) {
        let old_index = match &self.current_id {
            Some(id) => self.index_of(id),
            None => 0,
        };
        if !self.reload() {
            return;
        }
        if let Some(id) = self.current_id.clone() {
            let Some(current) = self.call(READ_ERROR, |m| m.get(&id)) else {
                return;
            };
            match current {
                None => {
                    if self.pending() {
                        // Keep the draft; the next valid commit re-adds it.
                        self.current_id = None;
                        self.draft = true;
                        self.error = DELETED_ELSEWHERE.to_string();
                    } else {
                        self.settle(old_index);
                    }
                }
                Some(current) => {
                    let on_disk = (
                        field(&current, "trigger"),
                        field(&current, "text"),
                        entry_enabled(&current),
                    );
                    if !self.pending() && on_disk != self.loaded {
                        self.load(&current);
                    }
                }
            }
            return;
        }
        if !self.draft {
            self.settle(0);
        }
    }

    /// Timer hook. `true` when something visible changed.
    pub fn tick(&mut self) -> bool {
        let now = self.now();
        let mut changed = false;
        if self.undo_due.is_some_and(|due| now >= due) {
            self.undo_token = None;
            self.undo_due = None;
            changed = true;
        }
        if self.autosave_due.is_some_and(|due| now >= due) {
            self.flush();
            changed = true;
        }
        if now >= self.next_poll {
            self.next_poll = now + POLL_INTERVAL;
            changed = self.poll() || changed;
        }
        changed
    }

    // ------------------------------------------------------------ preview ---

    pub fn set_test_input(&mut self, said: &str) {
        self.test_input = said.to_string();
    }

    /// `(output, status)` for the typed test phrase; no LLM, no network.
    pub fn preview(&self) -> (String, String) {
        if strip(&self.test_input).is_empty() {
            return (String::new(), PREVIEW_PROMPT.to_string());
        }
        match self.model.preview(&self.test_input) {
            Err(_) => (String::new(), PREVIEW_ERROR.to_string()),
            Ok((output, matched)) if !matched.is_empty() => {
                let quoted = matched.join("”, “");
                (output, format!("Matched: “{quoted}”"))
            }
            Ok((output, _)) => (output, "No hot phrase matched".to_string()),
        }
    }
}

// ------------------------------------------------------------------ layout ---

/// `(x, y, w, h)` in whole points.
pub type IRect = (i32, i32, i32, i32);

pub const PAD: i32 = 16;
pub const GAP: i32 = 16;
pub const SEARCH_H: i32 = 32;
pub const BUTTON_H: i32 = 36;
pub const LIST_TOP: i32 = SEARCH_H + 8 + BUTTON_H + 8;
pub const LIST_MIN_W: i32 = 190;
pub const LIST_MAX_W: i32 = 300;
pub const TEXT_MIN_H: i32 = 110;

pub const ENABLED_LABEL_W: i32 = 64;
pub const ENABLED_NOTICE_X: i32 = PAD + 48 + ENABLED_LABEL_W;
/// Between the enabled row and an error / Undo notice below it.
pub const NOTICE_GAP: i32 = 8;

/// Named frames in insertion order (a Python dict of rects).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frames(pub Vec<(&'static str, IRect)>);

impl Frames {
    pub fn get(&self, name: &str) -> Option<IRect> {
        self.0.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = &(&'static str, IRect)> {
        self.0.iter()
    }

    /// The frame, panicking on an unknown name (a layout bug, like `KeyError`).
    pub fn at(&self, name: &str) -> IRect {
        self.get(name)
            .unwrap_or_else(|| panic!("no frame named {name:?}"))
    }

    fn put(&mut self, name: &'static str, rect: IRect) {
        self.0.push((name, rect));
    }
}

/// Card-relative frames for the editor controls, top down, and the content
/// height.
///
/// The error and Undo notices take room only while they are visible; hidden
/// ones are absent from the result, so "Try it" sits directly below the
/// enabled row. A short status ("Saved ✓") is shown inline in the enabled row
/// and never moves anything.
fn editor_rows(inner: i32, text_h: i32, error_visible: bool, undo_visible: bool) -> (Frames, i32) {
    let mut rows = Frames::default();
    let mut y = PAD;
    rows.put("trigger_label", (PAD, y, inner, 18));
    y += 20;
    rows.put("trigger_field", (PAD, y, inner, 32));
    y += 36;
    rows.put("trigger_hint", (PAD, y, inner, 16));
    y += 24;
    rows.put("text_label", (PAD, y, inner, 18));
    y += 20;
    rows.put("text_scroll", (PAD, y, inner, text_h));
    y += text_h + 4;
    rows.put("text_hint", (PAD, y, inner, 16));
    y += 28;
    rows.put("enabled_switch", (PAD, y + 5, 40, 22));
    rows.put("enabled_label", (PAD + 48, y + 7, ENABLED_LABEL_W, 18));
    rows.put(
        "enabled_notice",
        (
            ENABLED_NOTICE_X,
            y + 7,
            (inner - 48 - ENABLED_LABEL_W - 96).max(40),
            18,
        ),
    );
    rows.put("delete_button", (PAD + inner - 88, y, 88, 32));
    y += 40;
    if error_visible {
        rows.put("error_label", (PAD, y, inner, 34));
        y += 34 + NOTICE_GAP;
    }
    if undo_visible {
        rows.put("undo_bar", (PAD, y, inner, 36));
        y += 36 + NOTICE_GAP;
    }
    rows.put("preview_label", (PAD, y, inner, 18));
    y += 20;
    rows.put("test_field", (PAD, y, inner, 32));
    y += 36;
    rows.put("preview_output", (PAD, y, inner, 56));
    y += 60;
    rows.put("preview_status", (PAD, y, inner, 16));
    y += 16 + PAD;
    (rows, y)
}

/// Frames for the two-column page, relative to the page origin.
#[derive(Clone, Debug, PartialEq)]
pub struct HotPhrasesLayout {
    pub height: i32,
    pub search: IRect,
    pub new: IRect,
    pub list: IRect,
    pub card: IRect,
    pub card_rows: Frames,
}

impl HotPhrasesLayout {
    /// The top-level (non-`card_rows`) frames by name, like the Python dict's
    /// tuple values.
    pub fn named(&self) -> Vec<(&'static str, IRect)> {
        vec![
            ("search", self.search),
            ("new", self.new),
            ("list", self.list),
            ("card", self.card),
        ]
    }
}

/// Frames for the two-column page, relative to the page origin.
///
/// Width-aware: the list column is 30% of the page (190-300). The editor
/// takes the rest. Height-aware: the text box grows into spare height; when
/// even the minimum does not fit, the page grows and the Hub scrolls it, so
/// controls are never squeezed, overlapped or clipped. Error and Undo notices
/// reserve height only while visible (the text box gives it up and takes it
/// back), so "Try it" stays right below the enabled row.
///
/// `width` and `available_height` are truncated to whole points like
/// Python's `int()`.
pub fn hot_phrases_layout(
    width: f64,
    available_height: f64,
    error_visible: bool,
    undo_visible: bool,
) -> HotPhrasesLayout {
    let width = (width.trunc() as i32).max(LIST_MIN_W + GAP + 300);
    let list_w = LIST_MAX_W.min(LIST_MIN_W.max((f64::from(width) * 0.3).round_ties_even() as i32));
    let editor_x = list_w + GAP;
    let editor_w = width - editor_x;
    let inner = editor_w - 2 * PAD;
    let (_, needed) = editor_rows(inner, TEXT_MIN_H, error_visible, undo_visible);
    let height = (available_height.trunc() as i32)
        .max(needed)
        .max(LIST_TOP + 120);
    let text_h = TEXT_MIN_H + (height - needed);
    let (card, _) = editor_rows(inner, text_h, error_visible, undo_visible);
    HotPhrasesLayout {
        height,
        search: (0, 0, list_w, SEARCH_H),
        new: (0, SEARCH_H + 8, list_w, BUTTON_H),
        list: (0, LIST_TOP, list_w, height - LIST_TOP),
        card: (editor_x, 0, editor_w, height),
        card_rows: card,
    }
}

/// `(frames, card height)` for the empty-state card; notices only when shown.
pub fn empty_layout(width: i32, error_visible: bool, undo_visible: bool) -> (Frames, i32) {
    let inner = width - 2 * PAD;
    let mut rows = Frames::default();
    rows.put("message", (PAD, PAD + 8, inner, 48));
    rows.put("detail", (PAD, PAD + 62, inner, 36));
    rows.put("new", (PAD, 124, 150, BUTTON_H));
    rows.put("example", (PAD + 162, 124, 160, BUTTON_H));
    let mut y = 124 + BUTTON_H;
    if error_visible {
        rows.put("error", (PAD, y + NOTICE_GAP, inner, 34));
        y += NOTICE_GAP + 34;
    }
    if undo_visible {
        rows.put("undo", (PAD, y + NOTICE_GAP, inner, 36));
        y += NOTICE_GAP + 36;
    }
    (rows, y + PAD)
}

#[cfg(test)]
#[path = "hot_phrases_editor/tests.rs"]
mod tests;
