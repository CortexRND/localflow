use super::*;
use lf_core::dictionary::DictionaryStore;
use lf_core::hotphrases::{HotPhraseStore, MAX_TEXT_CHARS};
use lf_core::phrases_model::PhrasesSettings;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

// ----------------------------------------------------------- layout ---

const HUB_PAGE_BOTTOM: i32 = crate::tokens::HUB_PAGE_BOTTOM;
const MIN: (i32, i32) = (596, 576 - 48 - HUB_PAGE_BOTTOM);
const WIDE: (i32, i32) = (1016, 796 - 48 - HUB_PAGE_BOTTOM);
const STATES: [(&str, bool, bool); 4] = [
    ("normal", false, false),
    ("error", true, false),
    ("undo", false, true),
    ("both", true, true),
];

fn overlaps(a: IRect, b: IRect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

fn frames_for(size: (i32, i32), error: bool, undo: bool) -> HotPhrasesLayout {
    hot_phrases_layout(f64::from(size.0), f64::from(size.1), error, undo)
}

fn assert_no_overlap(rows: &Frames, bounds: (i32, i32)) {
    let boxes: Vec<_> = rows.iter().collect();
    for (name, (x, y, w, h)) in &boxes {
        assert!(
            *x >= 0 && *y >= 0 && x + w <= bounds.0 && y + h <= bounds.1,
            "{name}"
        );
    }
    for (i, (first, a)) in boxes.iter().enumerate() {
        for (second, b) in &boxes[i + 1..] {
            assert!(!overlaps(*a, *b), "{first} overlaps {second}");
        }
    }
}

#[test]
fn notice_rows_exist_only_when_visible_and_nothing_overlaps() {
    for (label, size) in [("min", MIN), ("wide", WIDE)] {
        for (state, error, undo) in STATES {
            let frames = frames_for(size, error, undo);
            let rows = &frames.card_rows;
            assert_eq!(rows.contains("error_label"), error);
            assert_eq!(rows.contains("undo_bar"), undo);
            assert_no_overlap(rows, (frames.card.2, frames.card.3));
            if (label, state) == ("min", "both") {
                // Both notices plus the minimum text box need more than the
                // window offers: the page grows instead of squeezing.
                assert!(frames.height > size.1);
                assert_eq!(rows.at("text_scroll").3, TEXT_MIN_H);
            } else {
                assert_eq!(frames.height, size.1, "{label} {state}");
            }
        }
    }
}

#[test]
fn try_it_sits_directly_below_the_enabled_row_without_notices() {
    for size in [MIN, WIDE] {
        let rows = frames_for(size, false, false).card_rows;
        let (delete, switch) = (rows.at("delete_button"), rows.at("enabled_switch"));
        let enabled_bottom = (delete.1 + delete.3).max(switch.1 + switch.3);
        let gap = rows.at("preview_label").1 - enabled_bottom;
        assert!(0 < gap && gap <= 12);
    }
}

#[test]
fn visible_notices_sit_between_the_enabled_row_and_try_it() {
    for size in [MIN, WIDE] {
        let base = frames_for(size, false, false).card_rows;
        let both = frames_for(size, true, true).card_rows;
        let delete = both.at("delete_button");
        let (error, undo, preview) = (
            both.at("error_label"),
            both.at("undo_bar"),
            both.at("preview_label"),
        );
        assert!(error.1 >= delete.1 + delete.3);
        assert!(undo.1 >= error.1 + error.3);
        assert!(preview.1 >= undo.1 + undo.3);
        assert!(both.at("text_scroll").3 < base.at("text_scroll").3);
        assert!(both.at("text_scroll").3 >= TEXT_MIN_H);
    }
}

#[test]
fn each_notice_costs_exactly_its_own_height_plus_a_gap() {
    let text = |e, u| frames_for(WIDE, e, u).card_rows.at("text_scroll").3;
    let normal = text(false, false);
    assert_eq!(normal - text(true, false), 34 + NOTICE_GAP);
    assert_eq!(normal - text(false, true), 36 + NOTICE_GAP);
}

#[test]
fn short_window_grows_the_page_instead_of_squeezing_with_notices() {
    let frames = hot_phrases_layout(596.0, 120.0, true, true);
    assert!(frames.height > 120);
    assert_eq!(frames.card_rows.at("text_scroll").3, TEXT_MIN_H);
}

#[test]
fn saved_status_is_inline_in_the_enabled_row() {
    let rows = frames_for(MIN, false, false).card_rows;
    let (label, notice, delete) = (
        rows.at("enabled_label"),
        rows.at("enabled_notice"),
        rows.at("delete_button"),
    );
    assert!(!overlaps(label, notice) && !overlaps(notice, delete));
    assert!(notice.0 >= label.0 + label.2 && notice.0 + notice.2 <= delete.0);
    assert!((notice.1 - label.1).abs() <= 2);
}

#[test]
fn empty_card_is_compact_and_notices_extend_it() {
    for width in [MIN.0, WIDE.0] {
        let (rows, height) = empty_layout(width, false, false);
        assert!(height < 258);
        assert!(!rows.contains("error") && !rows.contains("undo"));
        let (new, example) = (rows.at("new"), rows.at("example"));
        let buttons_bottom = (new.1 + new.3).max(example.1 + example.3);
        assert!(height - buttons_bottom <= 24);
        let (with_error, error_height) = empty_layout(width, true, false);
        let (with_both, both_height) = empty_layout(width, true, true);
        assert!(error_height > height && both_height > error_height);
        assert!(with_error.at("error").1 >= buttons_bottom);
        let (error, undo) = (with_both.at("error"), with_both.at("undo"));
        assert!(undo.1 >= error.1 + error.3);
        let boxes: Vec<_> = with_both.iter().map(|(_, r)| *r).collect();
        for (i, first) in boxes.iter().enumerate() {
            assert!(first.1 + first.3 <= both_height);
            for second in &boxes[i + 1..] {
                assert!(!overlaps(*first, *second));
            }
        }
    }
}

#[test]
fn layout_has_no_overlap_or_clipping() {
    for (width, viewport) in [(596, 504), (1016, 724), (640, 300), (2000, 1200)] {
        let frames = hot_phrases_layout(f64::from(width), f64::from(viewport), false, false);
        let named = frames.named();
        for (name, (x, y, w, h)) in &named {
            assert!(
                *x >= 0 && *y >= 0 && x + w <= width.max(506) && y + h <= frames.height,
                "{name}"
            );
            assert!(*w > 0 && *h > 0, "{name}");
        }
        for (i, (first, a)) in named.iter().take(3).enumerate() {
            for (second, b) in named.iter().take(3).skip(i + 1) {
                assert!(!overlaps(*a, *b), "{first} {second}");
            }
            assert!(!overlaps(*a, frames.card), "{first}");
        }
        assert_no_overlap(&frames.card_rows, (frames.card.2, frames.card.3));
    }
}

#[test]
fn layout_is_width_aware_and_fills_a_tall_window() {
    let narrow = hot_phrases_layout(596.0, 504.0, false, false);
    let wide = hot_phrases_layout(1016.0, 724.0, false, false);
    assert_eq!(narrow.list.2, 190);
    assert_eq!(wide.list.2, 300);
    assert!(narrow.card.2 < wide.card.2);
    assert_eq!(wide.height, 724);
    assert!(wide.card_rows.at("text_scroll").3 > narrow.card_rows.at("text_scroll").3);
    assert!(TEXT_MIN_H <= narrow.card_rows.at("text_scroll").3);
}

#[test]
fn layout_grows_the_scrolling_page_instead_of_squeezing_controls() {
    let short = hot_phrases_layout(596.0, 200.0, false, false);
    assert!(short.height > 200);
    assert_eq!(short.card_rows.at("text_scroll").3, TEXT_MIN_H);
}

#[test]
fn layout_clamps_tiny_widths_and_truncates_floats() {
    let tiny = hot_phrases_layout(10.0, 10.0, false, false);
    assert_eq!(tiny.card.2 + tiny.card.0, LIST_MIN_W + GAP + 300);
    assert_eq!(
        hot_phrases_layout(700.9, 500.9, false, false),
        hot_phrases_layout(700.0, 500.0, false, false)
    );
    assert_eq!(hot_phrases_layout(650.0, 500.0, false, false).list.2, 195);
}

// ----------------------------------------------------- pure helpers ---

#[test]
fn labels_and_placeholders() {
    assert_eq!(enabled_label(true), "On");
    assert_eq!(enabled_label(false), "Off");
    assert_eq!(preview_placeholder("", "", ""), PREVIEW_PLACEHOLDER);
    assert_eq!(preview_placeholder("   ", "", ""), PREVIEW_PLACEHOLDER);
    assert_eq!(preview_placeholder("hi", "out", ""), "");
    assert_eq!(preview_placeholder("hi", "", PREVIEW_ERROR), "");
    assert_eq!(
        preview_placeholder("hi", "", "No hot phrase matched"),
        PREVIEW_EMPTY_RESULT
    );
    assert_eq!(
        EMPTY_MESSAGE,
        "Say a word, get a saved prompt. Add your first hot phrase."
    );
}

#[test]
fn splitlines_matches_python() {
    assert_eq!(py_splitlines("a\nb\r\nc\rd"), vec!["a", "b", "c", "d"]);
    assert_eq!(py_splitlines("a\n"), vec!["a"]);
    assert_eq!(py_splitlines("\n\nx"), vec!["", "", "x"]);
    assert_eq!(py_splitlines("a\u{2028}b\u{85}c"), vec!["a", "b", "c"]);
    assert!(py_splitlines("").is_empty());
}

// ----------------------------------------------------- editor state ---

/// Failure injection around the real model.
#[derive(Default)]
struct Flags {
    reads: Cell<bool>,
    add: Cell<bool>,
    update: Cell<bool>,
    delete: Cell<bool>,
    preview: Cell<bool>,
}

struct Flaky {
    inner: PhrasesModel,
    flags: Rc<Flags>,
}

fn boom(what: &str) -> anyhow::Error {
    anyhow::anyhow!("injected {what} failure")
}

impl PhrasesBackend for Flaky {
    fn entries(&self, q: &str) -> Result<Vec<Entry>> {
        if self.flags.reads.get() {
            return Err(boom("read"));
        }
        self.inner.entries(q)
    }
    fn get(&self, id: &str) -> Result<Option<Entry>> {
        if self.flags.reads.get() {
            return Err(boom("read"));
        }
        self.inner.get(id)
    }
    fn trigger_hint(&self, t: &str, ex: Option<&str>) -> Result<(bool, String)> {
        if self.flags.reads.get() {
            return Err(boom("read"));
        }
        self.inner.trigger_hint(t, ex)
    }
    fn text_hint(&self, t: &str) -> Result<(bool, String)> {
        Ok(self.inner.text_hint(t))
    }
    fn commit(
        &self,
        id: Option<&str>,
        t: &str,
        x: &str,
        e: bool,
    ) -> Result<(Option<Entry>, Option<String>)> {
        if self.flags.reads.get() || (id.is_none() && self.flags.add.get()) {
            return Err(boom("write"));
        }
        self.inner.commit(id, t, x, e)
    }
    fn set_enabled(&self, id: &str, e: bool) -> Result<Option<Entry>> {
        if self.flags.update.get() {
            return Err(boom("update"));
        }
        self.inner.set_enabled(id, e)
    }
    fn delete(&self, id: &str) -> Result<Option<Entry>> {
        if self.flags.delete.get() {
            return Err(boom("delete"));
        }
        self.inner.delete(id)
    }
    fn undo_delete(&self, token: &Entry) -> Result<(Option<Entry>, Option<String>)> {
        if self.flags.add.get() {
            return Err(boom("write"));
        }
        self.inner.undo_delete(token)
    }
    fn preview(&self, said: &str) -> Result<(String, Vec<String>)> {
        if self.flags.preview.get() {
            return Err(boom("preview"));
        }
        self.inner.preview(said)
    }
    fn changed_on_disk(&mut self) -> Result<bool> {
        if self.flags.reads.get() {
            return Err(boom("read"));
        }
        Ok(self.inner.changed_on_disk())
    }
    fn hot_phrases_enabled(&self) -> bool {
        self.inner.hot_phrases_enabled()
    }
}

struct Env {
    dir: tempfile::TempDir,
    store: HotPhraseStore,
    now: Rc<Cell<f64>>,
    flags: Rc<Flags>,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let store = HotPhraseStore::new(dir.path().join("hot_phrases.json"));
        Env {
            dir,
            store,
            now: Rc::new(Cell::new(100.0)),
            flags: Rc::new(Flags::default()),
        }
    }

    fn model(&self, hot_phrases: bool) -> PhrasesModel {
        let settings = PhrasesSettings {
            hot_phrases,
            style_other: "formal".into(),
            ..Default::default()
        };
        PhrasesModel::new(
            HotPhraseStore::new(self.dir.path().join("hot_phrases.json")),
            Some(settings),
        )
        .unwrap()
        .with_dictionary(DictionaryStore::new(Some(
            self.dir.path().join("dict.json"),
        )))
    }

    fn clock(&self) -> Box<dyn Fn() -> f64> {
        let now = self.now.clone();
        Box::new(move || now.get())
    }

    fn make(&self, entries: &[(&str, &str, bool)]) -> HotPhrasesEditor<Flaky> {
        self.make_with(entries, true)
    }

    fn make_with(
        &self,
        entries: &[(&str, &str, bool)],
        hot_phrases: bool,
    ) -> HotPhrasesEditor<Flaky> {
        for (trigger, text, enabled) in entries {
            self.store.add(trigger, text, *enabled).unwrap();
        }
        let model = Flaky {
            inner: self.model(hot_phrases),
            flags: self.flags.clone(),
        };
        HotPhrasesEditor::with_clock(model, self.clock())
    }

    fn advance(&self, seconds: f64) {
        self.now.set(self.now.get() + seconds);
    }

    fn triggers(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .store
            .list()
            .iter()
            .map(|e| e["trigger"].as_str().unwrap().to_string())
            .collect();
        all.sort();
        all
    }

    fn id_of(&self, trigger: &str) -> String {
        self.store
            .list()
            .iter()
            .find(|e| e["trigger"] == trigger)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Another process edits the file: apply `change`, force a new signature.
    fn external(&self, change: impl FnOnce(&HotPhraseStore)) {
        change(&self.store);
        let path = self.dir.path().join("hot_phrases.json");
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let mtime = file.metadata().unwrap().modified().unwrap();
        file.set_modified(mtime + Duration::from_secs(2)).unwrap();
    }
}

fn titles(ed: &HotPhrasesEditor<Flaky>) -> Vec<String> {
    ed.rows().into_iter().map(|r| r.title).collect()
}

const T: bool = true;
const F: bool = false;

// ------------------------------------------------------------ empty ---

#[test]
fn empty_store_shows_the_friendly_empty_state() {
    let env = Env::new();
    let ed = env.make(&[]);
    assert!(ed.empty_state_visible() && !ed.editor_visible());
    assert!(ed.rows().is_empty() && ed.current_id.is_none());
}

#[test]
fn untouched_empty_draft_never_traps_navigation() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    assert!(ed.new_phrase() && ed.editor_visible());
    assert!(ed.leave());
    assert!(ed.empty_state_visible());
    assert!(env.store.list().is_empty());
    assert_eq!(ed.error, "");
}

#[test]
fn autosave_of_an_untouched_empty_draft_writes_nothing() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    assert!(ed.flush());
    assert!(env.store.list().is_empty());
}

#[test]
fn new_phrase_twice_keeps_the_same_blank_draft() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    assert!(ed.new_phrase() && ed.draft);
}

#[test]
fn add_example_creates_and_selects_it() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    assert!(ed.add_example());
    assert_eq!(env.triggers(), vec![EXAMPLE_TRIGGER]);
    assert!(ed.current_id.is_some() && !ed.pending());
    assert!(ed.fields_stale && ed.text == EXAMPLE_TEXT);
}

// --------------------------------------------------------- autosave ---

#[test]
fn valid_edit_autosaves_after_the_debounce() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("review checklist"), Some("CHECK: tests, lint"), None);
    env.advance(0.3);
    ed.tick();
    assert!(env.store.list().is_empty());
    env.advance(0.4);
    assert!(ed.tick());
    assert_eq!(env.triggers(), vec!["review checklist"]);
    assert_eq!(ed.current_id.as_deref(), env.store.list()[0]["id"].as_str());
    assert!(ed.notice == "Saved ✓" && ed.error.is_empty() && !ed.pending());
    assert_eq!(titles(&ed), vec!["review checklist"]);
    assert!(ed.rows()[0].selected);
}

#[test]
fn each_keystroke_restarts_the_debounce() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("rev"), Some("x"), None);
    env.advance(0.5);
    ed.edit(Some("review"), Some("x"), None);
    env.advance(0.5);
    ed.tick();
    assert!(env.store.list().is_empty());
    env.advance(0.2);
    ed.tick();
    assert_eq!(env.triggers(), vec!["review"]);
}

#[test]
fn editing_an_existing_phrase_updates_it_in_place() {
    let env = Env::new();
    let mut ed = env.make(&[("sign off", "Thanks", T)]);
    ed.on_show();
    let original = ed.current_id.clone().unwrap();
    ed.edit(None, Some("Thanks, talk tomorrow."), None);
    env.advance(1.0);
    ed.tick();
    let entries = env.store.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], original.as_str());
    assert_eq!(entries[0]["text"], "Thanks, talk tomorrow.");
}

#[test]
fn edit_back_to_the_saved_values_cancels_the_pending_save() {
    let env = Env::new();
    let mut ed = env.make(&[("sign off", "Thanks", T)]);
    ed.on_show();
    ed.edit(None, Some("Changed"), None);
    assert!(ed.autosave_due.is_some());
    ed.edit(None, Some("Thanks"), None);
    assert!(ed.autosave_due.is_none() && !ed.pending());
}

#[test]
fn invalid_edit_stays_local_with_an_inline_error() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "x", T)]);
    ed.on_show();
    ed.new_phrase();
    ed.edit(Some("Review, checklist"), Some("y"), None);
    env.advance(1.0);
    ed.tick();
    assert_eq!(env.store.list().len(), 1);
    assert!(ed.error.starts_with("Already used by"));
    assert!(ed.trigger == "Review, checklist" && ed.text == "y");
    let (message, is_error) = ed.trigger_hint();
    assert!(is_error && message.starts_with("Already used by"));
    assert!(ed.pending() && ed.draft);
}

#[test]
fn hints_stay_neutral_until_the_user_touches_the_form() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    assert_eq!(
        ed.trigger_hint(),
        ("Type the words you'll say".to_string(), false)
    );
    ed.edit(Some(""), Some(""), None);
    assert!(ed.trigger_hint().1);
    assert!(ed.text_hint().1);
}

#[test]
fn fixing_an_invalid_draft_saves_it() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "x", T)]);
    ed.on_show();
    ed.new_phrase();
    ed.edit(Some("Review, checklist"), Some("y"), None);
    env.advance(1.0);
    ed.tick();
    ed.edit(Some("standup"), None, None);
    env.advance(1.0);
    ed.tick();
    assert_eq!(env.triggers(), vec!["review checklist", "standup"]);
    assert_eq!(ed.error, "");
}

#[test]
fn text_over_the_limit_is_rejected_by_the_model() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("big"), Some(&"x".repeat(MAX_TEXT_CHARS + 1)), None);
    assert!(!ed.flush());
    assert!(ed.error.contains("20,000") && env.store.list().is_empty());
}

#[test]
fn unicode_triggers_and_text_round_trip() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("  日本語 ✓ “quote”  "), Some("héllo\nwörld 🎉"), None);
    assert!(ed.flush());
    let saved = &env.store.list()[0];
    assert_eq!(saved["trigger"], "日本語 ✓ “quote”");
    assert_eq!(saved["text"], "héllo\nwörld 🎉");
    let rows = ed.rows();
    assert_eq!(rows[0].subtitle, "héllo");
    assert_eq!(rows[0].accessibility, "日本語 ✓ “quote”, on");
}

// -------------------------------------------------- navigation guard ---

#[test]
fn leave_commits_a_valid_dirty_draft() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("standup"), Some("yesterday:"), None);
    assert!(ed.leave());
    assert_eq!(env.triggers(), vec!["standup"]);
    assert!(ed.autosave_due.is_none());
}

#[test]
fn leave_keeps_the_user_on_an_invalid_draft_with_a_friendly_error() {
    let env = Env::new();
    let mut ed = env.make(&[("standup", "x", T)]);
    ed.on_show();
    ed.new_phrase();
    ed.edit(Some("Standup!"), Some("y"), None);
    assert!(!ed.leave());
    assert!(ed.error.contains("Fix it to continue") && ed.error.contains("Already used"));
    assert!(ed.trigger == "Standup!" && ed.text == "y");
    assert_eq!(env.store.list().len(), 1);
}

#[test]
fn leave_is_a_no_op_for_a_clean_selection() {
    let env = Env::new();
    let mut ed = env.make(&[("standup", "x", T)]);
    ed.on_show();
    assert!(ed.leave() && ed.error.is_empty());
}

#[test]
fn select_commits_a_valid_draft_then_switches() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T), ("beta", "b", T)]);
    ed.on_show();
    let alpha = ed.current_id.clone().unwrap();
    let beta = env.id_of("beta");
    ed.edit(None, Some("a2"), None);
    assert!(ed.select(&beta));
    assert_eq!(ed.current_id.as_deref(), Some(beta.as_str()));
    assert_eq!(ed.text, "b");
    let saved: Vec<_> = env
        .store
        .list()
        .into_iter()
        .filter(|e| e["id"] == alpha.as_str())
        .collect();
    assert_eq!(saved[0]["text"], "a2");
}

#[test]
fn select_is_blocked_by_an_invalid_edit_and_loses_nothing() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T), ("beta", "b", T)]);
    ed.on_show();
    let alpha = ed.current_id.clone();
    let beta = env.id_of("beta");
    ed.edit(None, Some("   "), None);
    assert!(!ed.select(&beta));
    assert_eq!(ed.current_id, alpha);
    assert_eq!(ed.text, "   ");
    assert!(ed.error.contains("Fix it to continue"));
}

#[test]
fn new_phrase_is_blocked_by_an_invalid_edit() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.edit(Some(""), None, None);
    assert!(!ed.new_phrase());
    assert!(ed.current_id.is_some() && ed.trigger.is_empty());
}

#[test]
fn selecting_the_current_phrase_is_a_no_op() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.edit(None, Some(""), None);
    let id = ed.current_id.clone().unwrap();
    assert!(ed.select(&id) && ed.text.is_empty());
}

#[test]
fn selecting_a_phrase_deleted_elsewhere_just_refreshes_the_list() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T), ("beta", "b", T)]);
    ed.on_show();
    let beta = env.id_of("beta");
    env.store.delete(&beta).unwrap();
    assert!(ed.select(&beta));
    assert_eq!(titles(&ed), vec!["alpha"]);
}

#[test]
fn cancel_stops_a_pending_autosave() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("standup"), Some("x"), None);
    ed.cancel();
    env.advance(5.0);
    ed.tick();
    assert!(env.store.list().is_empty());
}

// ----------------------------------------------------------- search ---

#[test]
fn search_filters_live_and_reports_no_matches() {
    let env = Env::new();
    let mut ed = env.make(&[
        ("review checklist", "x", T),
        ("sign off", "y", T),
        ("standup", "z", T),
    ]);
    ed.set_query("stand");
    assert_eq!(titles(&ed), vec!["standup"]);
    ed.set_query("zzz");
    assert!(ed.rows().is_empty() && ed.no_matches());
    ed.set_query("");
    assert!(ed.rows().len() == 3 && !ed.no_matches());
}

#[test]
fn search_matches_the_saved_text_too_and_unicode_case() {
    let env = Env::new();
    let mut ed = env.make(&[
        ("one", "contains needle", T),
        ("two", "nothing", T),
        ("straße", "x", T),
    ]);
    ed.set_query("NEEDLE");
    assert_eq!(titles(&ed), vec!["one"]);
    ed.set_query("STRASSE");
    assert_eq!(titles(&ed), vec!["straße"]);
}

#[test]
fn search_never_discards_an_invalid_draft() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.new_phrase();
    ed.edit(Some("!!!"), Some("typed text"), None);
    ed.set_query("alpha");
    ed.set_query("nothing matches");
    assert!(ed.trigger == "!!!" && ed.text == "typed text" && ed.draft);
    assert!(!ed.leave());
}

#[test]
fn search_keeps_the_selection_even_when_filtered_out() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T), ("beta", "b", T)]);
    ed.on_show();
    let current = ed.current_id.clone();
    ed.set_query("beta");
    assert_eq!(ed.current_id, current);
    assert_eq!(
        ed.rows().iter().map(|r| r.selected).collect::<Vec<_>>(),
        vec![false]
    );
}

#[test]
fn rows_expose_enabled_indicators_and_accessibility_labels() {
    let env = Env::new();
    let ed = env.make(&[("alpha", "first line\nsecond", T), ("beta", "b", F)]);
    let rows = ed.rows();
    assert_eq!(
        rows.iter()
            .map(|r| (r.title.as_str(), r.enabled))
            .collect::<Vec<_>>(),
        vec![("alpha", true), ("beta", false)]
    );
    assert_eq!(rows[0].subtitle, "first line");
    assert_eq!(
        rows.iter()
            .map(|r| r.accessibility.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha, on", "beta, off"]
    );
}

#[test]
fn subtitle_skips_blank_leading_lines() {
    let env = Env::new();
    let ed = env.make(&[("alpha", "\n  \n  real line  \nmore", T)]);
    assert_eq!(ed.rows()[0].subtitle, "real line");
}

// ---------------------------------------------------------- enabled ---

#[test]
fn enabled_toggle_saves_immediately_for_a_saved_phrase() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    assert!(ed.set_enabled(false));
    assert_eq!(env.store.list()[0]["enabled"], false);
    assert!(!ed.rows()[0].enabled);
    assert!(!ed.pending() && ed.autosave_due.is_none());
}

#[test]
fn enabled_toggle_leaves_other_unsaved_edits_pending() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.edit(None, Some("a changed"), None);
    ed.set_enabled(false);
    let saved = &env.store.list()[0];
    assert!(saved["enabled"] == false && saved["text"] == "a");
    assert!(ed.pending() && ed.autosave_due.is_some());
}

#[test]
fn enabled_toggle_creates_a_valid_draft() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("standup"), Some("x"), None);
    assert!(ed.set_enabled(false));
    let list = env.store.list();
    assert_eq!(
        (list[0]["trigger"].as_str(), list[0]["enabled"].as_bool()),
        (Some("standup"), Some(false))
    );
}

#[test]
fn enabled_toggle_on_an_invalid_draft_stays_local_with_an_error() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("standup"), Some(""), None);
    assert!(!ed.set_enabled(false));
    assert!(env.store.list().is_empty() && !ed.enabled);
    assert_eq!(ed.error, "Type the text to paste");
}

#[test]
fn enabled_toggle_after_external_deletion_re_adds_a_valid_phrase() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    env.store.delete(ed.current_id.as_deref().unwrap()).unwrap();
    assert!(ed.set_enabled(false));
    let list = env.store.list();
    assert_eq!(
        (list[0]["trigger"].as_str(), list[0]["enabled"].as_bool()),
        (Some("alpha"), Some(false))
    );
}

// ------------------------------------------------------ delete / undo ---

#[test]
fn delete_selects_the_neighbour_and_offers_undo() {
    let env = Env::new();
    let mut ed = env.make(&[("a one", "1", T), ("b two", "2", T), ("c three", "3", T)]);
    ed.on_show();
    let middle = env.id_of("b two");
    ed.select(&middle);
    assert!(ed.delete());
    assert_eq!(env.triggers(), vec!["a one", "c three"]);
    assert_eq!(ed.trigger, "c three");
    assert_eq!(ed.undo_label(), "Deleted “b two”");
    assert!(ed.fields_stale);
}

#[test]
fn delete_the_last_row_selects_the_previous_one() {
    let env = Env::new();
    let mut ed = env.make(&[("a one", "1", T), ("b two", "2", T)]);
    ed.on_show();
    let two = env.id_of("b two");
    ed.select(&two);
    ed.delete();
    assert_eq!(ed.trigger, "a one");
}

#[test]
fn deleting_the_only_phrase_returns_to_the_empty_state() {
    let env = Env::new();
    let mut ed = env.make(&[("only", "1", T)]);
    ed.on_show();
    ed.delete();
    assert!(ed.empty_state_visible() && env.store.list().is_empty());
    assert!(ed.undo_token.is_some());
}

#[test]
fn undo_restores_and_reselects_the_phrase() {
    let env = Env::new();
    let mut ed = env.make(&[("only", "1", T)]);
    ed.on_show();
    ed.delete();
    assert!(ed.undo_delete());
    assert_eq!(env.triggers(), vec!["only"]);
    assert_eq!(ed.trigger, "only");
    assert_eq!(ed.current_id.as_deref(), env.store.list()[0]["id"].as_str());
    assert!(ed.undo_token.is_none() && !ed.empty_state_visible());
}

#[test]
fn undo_bar_expires() {
    let env = Env::new();
    let mut ed = env.make(&[("only", "1", T)]);
    ed.on_show();
    ed.delete();
    env.advance(UNDO_SECONDS + 0.1);
    assert!(ed.tick());
    assert!(ed.undo_token.is_none() && ed.undo_label().is_empty());
    assert!(!ed.undo_delete());
}

#[test]
fn undo_reports_a_trigger_taken_in_the_meantime() {
    let env = Env::new();
    let mut ed = env.make(&[("only", "1", T)]);
    ed.on_show();
    ed.delete();
    env.store.add("Only", "other", true).unwrap();
    assert!(!ed.undo_delete());
    assert_eq!(ed.error, "trigger duplicates existing hot phrase 'Only'");
    assert!(ed.undo_token.is_some());
    assert_eq!(env.triggers(), vec!["Only"]);
}

#[test]
fn delete_with_unsaved_edits_removes_the_saved_phrase() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T), ("beta", "b", T)]);
    ed.on_show();
    ed.edit(None, Some("unsaved"), None);
    ed.delete();
    assert_eq!(env.triggers(), vec!["beta"]);
    assert_eq!(ed.trigger, "beta");
}

#[test]
fn delete_discards_an_unsaved_draft_without_touching_the_store() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.new_phrase();
    ed.edit(Some("draft"), Some("text"), None);
    ed.delete();
    assert_eq!(env.triggers(), vec!["alpha"]);
    assert!(ed.trigger == "alpha" && !ed.draft && ed.undo_token.is_none());
}

#[test]
fn notice_visibility_follows_error_and_undo_state() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "x", T), ("sign off", "y", T)]);
    assert_eq!(ed.notice_visibility(), (false, false));
    let id = env.id_of("sign off");
    ed.select(&id);
    ed.delete();
    assert_eq!(ed.notice_visibility(), (false, true));
    ed.new_phrase();
    ed.fill("review checklist", "dup");
    ed.flush();
    assert!(!ed.error.is_empty());
    assert_eq!(ed.notice_visibility(), (true, true));
    env.advance(UNDO_SECONDS + 1.0);
    assert!(ed.tick());
    assert_eq!(ed.notice_visibility(), (true, false));
    ed.edit(Some("a different trigger"), None, None);
    assert_eq!(ed.notice_visibility(), (false, false));
}

#[test]
fn saved_notice_never_changes_the_geometry() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "x", T)]);
    let id = ed.entries[0]["id"].as_str().unwrap().to_string();
    ed.select(&id);
    let before = ed.notice_visibility();
    ed.edit(None, Some("changed"), None);
    assert!(ed.flush());
    assert!(ed.notice == "Saved ✓" && ed.notice_visibility() == before);
}

// ------------------------------------------- external-change merges ---

#[test]
fn dirty_local_edits_win_over_an_external_change() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.edit(None, Some("local"), None);
    let id = ed.current_id.clone().unwrap();
    env.external(|s| {
        s.update(&id, None, Some("remote"), None).unwrap();
    });
    assert!(ed.poll());
    assert!(ed.text == "local" && ed.pending());
    ed.flush();
    assert_eq!(env.store.list()[0]["text"], "local");
}

#[test]
fn clean_selection_refreshes_from_an_external_edit() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.ack_fields();
    let id = ed.current_id.clone().unwrap();
    env.external(|s| {
        s.update(&id, None, Some("remote"), None).unwrap();
    });
    assert!(ed.poll());
    assert!(ed.text == "remote" && ed.fields_stale && !ed.pending());
}

#[test]
fn external_additions_show_up_in_the_list() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    env.external(|s| {
        s.add("beta", "b", true).unwrap();
    });
    ed.poll();
    assert_eq!(titles(&ed), vec!["alpha", "beta"]);
    assert_eq!(ed.trigger, "alpha");
}

#[test]
fn external_deletion_keeps_a_dirty_draft_and_re_adds_it_on_commit() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.edit(None, Some("mine"), None);
    let gone = ed.current_id.clone().unwrap();
    env.external(|s| {
        s.delete(&gone).unwrap();
    });
    assert!(ed.poll());
    assert!(ed.current_id.is_none() && ed.draft);
    assert!(ed.error == DELETED_ELSEWHERE && ed.text == "mine");
    assert!(env.store.list().is_empty());
    assert!(ed.flush());
    let entry = env.store.list().remove(0);
    assert_eq!(
        (entry["trigger"].as_str(), entry["text"].as_str()),
        (Some("alpha"), Some("mine"))
    );
    assert!(ed.error.is_empty());
    assert_eq!(ed.current_id.as_deref(), entry["id"].as_str());
}

#[test]
fn external_deletion_of_an_invalid_dirty_draft_stays_local() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.edit(None, Some(""), None);
    let id = ed.current_id.clone().unwrap();
    env.external(|s| {
        s.delete(&id).unwrap();
    });
    ed.poll();
    assert!(ed.draft && ed.text.is_empty() && env.store.list().is_empty());
    assert!(!ed.flush());
}

#[test]
fn external_deletion_of_a_clean_selection_moves_to_the_neighbour() {
    let env = Env::new();
    let mut ed = env.make(&[("a one", "1", T), ("b two", "2", T), ("c three", "3", T)]);
    ed.on_show();
    let middle = env.id_of("b two");
    ed.select(&middle);
    env.external(|s| {
        s.delete(&middle).unwrap();
    });
    assert!(ed.poll());
    assert!(ed.trigger == "c three" && !ed.draft && ed.error.is_empty());
}

#[test]
fn external_deletion_of_the_last_clean_phrase_shows_the_empty_state() {
    let env = Env::new();
    let mut ed = env.make(&[("only", "1", T)]);
    ed.on_show();
    let id = ed.current_id.clone().unwrap();
    env.external(|s| {
        s.delete(&id).unwrap();
    });
    ed.poll();
    assert!(ed.empty_state_visible() && ed.current_id.is_none());
}

#[test]
fn poll_without_a_disk_change_does_nothing() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    assert!(!ed.poll());
}

#[test]
fn tick_polls_on_its_own_interval() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    env.external(|s| {
        s.add("beta", "b", true).unwrap();
    });
    assert!(!ed.tick());
    env.advance(POLL_INTERVAL + 0.1);
    assert!(ed.tick());
    assert_eq!(ed.rows().len(), 2);
}

#[test]
fn on_show_selects_the_first_phrase_and_reloads() {
    let env = Env::new();
    let mut ed = env.make(&[("beta", "b", T), ("alpha", "a", T)]);
    ed.on_show();
    assert!(ed.trigger == "alpha" && ed.fields_stale);
}

#[test]
fn on_show_after_leaving_keeps_the_selection() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T), ("beta", "b", T)]);
    ed.on_show();
    let beta = env.id_of("beta");
    ed.select(&beta);
    ed.on_show();
    assert_eq!(ed.current_id.as_deref(), Some(beta.as_str()));
}

#[test]
fn empty_page_picks_up_an_external_addition() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.on_show();
    assert!(ed.empty_state_visible());
    env.external(|s| {
        s.add("beta", "b", true).unwrap();
    });
    env.advance(POLL_INTERVAL + 0.1);
    assert!(ed.tick());
    assert!(!ed.empty_state_visible() && ed.trigger == "beta");
}

// ------------------------------------------------- store failures ---

#[test]
fn unreadable_store_is_a_friendly_message_not_a_crash() {
    let env = Env::new();
    env.flags.reads.set(true);
    let mut ed = env.make(&[]);
    assert!(ed.error == READ_ERROR && ed.rows().is_empty());
    ed.on_show();
    let _ = ed.poll();
}

#[test]
fn write_failure_keeps_the_edit_and_reports_it() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    ed.new_phrase();
    ed.edit(Some("standup"), Some("x"), None);
    env.flags.add.set(true);
    assert!(!ed.flush());
    assert!(ed.error == WRITE_ERROR && ed.trigger == "standup");
    assert!(!ed.leave() && ed.error == WRITE_ERROR);
}

#[test]
fn toggle_write_failure_is_reported() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    env.flags.update.set(true);
    assert!(!ed.set_enabled(false));
    assert_eq!(ed.error, WRITE_ERROR);
}

#[test]
fn delete_failure_is_reported() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    env.flags.delete.set(true);
    assert!(!ed.delete() && ed.error == WRITE_ERROR);
    assert!(ed.current_id.is_some());
}

#[test]
fn failed_undo_restore_keeps_the_token_until_it_works_or_expires() {
    let env = Env::new();
    let mut ed = env.make(&[("only", "1", T)]);
    ed.on_show();
    ed.delete();
    env.flags.add.set(true);
    assert!(!ed.undo_delete());
    assert!(ed.error == WRITE_ERROR && ed.undo_token.is_some());
    assert!(!ed.undo_label().is_empty());
    env.flags.add.set(false);
    assert!(ed.undo_delete());
    assert!(ed.undo_token.is_none() && ed.error.is_empty() && ed.trigger == "only");
}

#[test]
fn last_delete_leaves_undo_and_expiry_on_the_empty_surface() {
    let env = Env::new();
    let mut ed = env.make(&[("only", "1", T)]);
    ed.on_show();
    ed.delete();
    assert!(ed.empty_state_visible() && ed.undo_label() == "Deleted “only”");
    env.advance(UNDO_SECONDS + 0.1);
    assert!(ed.tick());
    assert!(ed.undo_label().is_empty() && ed.empty_state_visible());
}

#[test]
fn errors_are_kept_for_the_empty_surface() {
    let env = Env::new();
    let mut ed = env.make(&[]);
    env.flags.add.set(true);
    assert!(!ed.add_example());
    assert!(ed.empty_state_visible() && ed.error == WRITE_ERROR);
}

#[test]
fn hints_flush_leave_and_autosave_survive_a_failing_store_read() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T)]);
    ed.on_show();
    ed.new_phrase();
    ed.edit(Some("standup"), Some("x"), None);
    env.flags.reads.set(true);
    assert_eq!(ed.trigger_hint(), (READ_ERROR.to_string(), true));
    assert!(!ed.text_hint().1);
    assert!(!ed.flush() && ed.error == READ_ERROR);
    env.advance(1.0);
    ed.edit(Some("standup"), Some("x2"), None);
    env.advance(1.0);
    ed.tick();
    assert_eq!(ed.error, READ_ERROR);
    assert!(!ed.leave());
    assert_eq!(ed.error, format!("{READ_ERROR} Your edits are kept here."));
    assert!(ed.trigger == "standup" && ed.text == "x2" && ed.draft);
}

#[test]
fn toggle_and_select_survive_a_failing_store_read() {
    let env = Env::new();
    let mut ed = env.make(&[("alpha", "a", T), ("beta", "b", T)]);
    ed.on_show();
    let other = env.id_of("beta");
    ed.edit(None, Some("changed"), None);
    env.flags.reads.set(true);
    let _ = ed.set_enabled(false);
    assert!(!ed.select(&other));
    assert_eq!(ed.text, "changed");
}

// --------------------------------------------------------- preview ---

#[test]
fn preview_needs_a_typed_phrase() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "CHECK: tests", T)]);
    assert_eq!(ed.preview(), (String::new(), PREVIEW_PROMPT.to_string()));
    ed.set_test_input("   ");
    assert_eq!(ed.preview(), (String::new(), PREVIEW_PROMPT.to_string()));
}

#[test]
fn preview_shows_live_output_and_the_matching_trigger() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "CHECK: tests", T)]);
    ed.set_test_input("please review checklist now");
    let (output, status) = ed.preview();
    assert!(output.contains("CHECK: tests"));
    assert_eq!(status, "Matched: “review checklist”");
}

#[test]
fn preview_says_when_nothing_matched() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "CHECK: tests", T)]);
    ed.set_test_input("hello world");
    let (output, status) = ed.preview();
    assert!(output.contains("hello world") && status == "No hot phrase matched");
}

#[test]
fn preview_ignores_disabled_phrases() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "CHECK", F)]);
    ed.set_test_input("review checklist");
    assert_eq!(ed.preview().1, "No hot phrase matched");
}

#[test]
fn preview_with_hot_phrases_off_warns_and_matches_nothing() {
    let env = Env::new();
    let mut ed = env.make_with(&[("review checklist", "CHECK", T)], false);
    ed.set_test_input("review checklist");
    assert_eq!(ed.banner(), DISABLED_BANNER);
    assert_eq!(ed.preview().1, "No hot phrase matched");
    let on = env.make(&[]);
    assert_eq!(on.banner(), "");
}

#[test]
fn preview_failure_never_raises() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "CHECK", T)]);
    env.flags.preview.set(true);
    ed.set_test_input("review checklist");
    assert_eq!(ed.preview(), (String::new(), PREVIEW_ERROR.to_string()));
}

#[test]
fn preview_runs_without_network_or_llm() {
    // The model preview only reads the temp store and dictionary.
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "CHECK", T)]);
    ed.set_test_input("review checklist");
    assert_eq!(ed.preview().0, "CHECK");
}

// ------------------------------------------------------- snapshots ---

#[test]
fn fill_types_for_the_view_and_an_invalid_draft_is_never_autosaved() {
    let env = Env::new();
    let mut ed = env.make(&[("review checklist", "x", T)]);
    ed.on_show();
    ed.ack_fields();
    ed.new_phrase();
    ed.ack_fields();
    ed.fill("Review, checklist", "y");
    ed.flush();
    assert!(ed.fields_stale && ed.error.starts_with("Already used"));
    for _ in 0..3 {
        env.advance(1.0);
        ed.tick();
    }
    assert!(env.store.list().len() == 1 && ed.draft && !ed.error.is_empty());
}
