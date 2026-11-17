//! Pure presentation and state helpers for the ported Hub pages.

use crate::backends::prompts::{
    approve_prompt, reject_prompt, PromptEntry, PromptQueue, TransitionFailure, APPROVE_FROM,
    REJECT_FROM,
};
use crate::pystr::PyStr;
pub use crate::text_wrap::wrap_lines;
use crate::text_wrap::TextMeasure;
use crate::tokens::{Weight, HUB_HEADER_OFFSET};
use chrono::{Datelike, Local, NaiveDateTime, TimeZone, Timelike};
use lf_core::config::Config;
use lf_core::dictionary::{DictionaryData, DictionaryError, DictionaryStore};
use regex::Regex;
use serde_json::{json, Map, Value};

pub type Frame = (i32, i32, i32, i32);

fn floor_div(a: i32, b: i32) -> i32 {
    a.div_euclid(b)
}

pub const STAT_FOUR_COLUMN_MIN_CARD: i32 = 150;
pub const STAT_GAP: i32 = 12;

/// Display labels only: routing keeps the page ids.
pub const SIDEBAR_LABELS: [(&str, &str); 1] = [("Models", "Models & keys")];
pub const SIDEBAR_BUTTON_WIDTH: i32 = 168;
/// Room the borderless button cell keeps around its title (both sides).
pub const SIDEBAR_TITLE_INSET: i32 = 16;
/// Two U+2002 EN SPACE characters, exactly as `hub_model.SIDEBAR_TITLE_PREFIX`.
pub const SIDEBAR_TITLE_PREFIX: &str = "\u{2002}\u{2002}";
/// Font of the sidebar buttons: 13pt semibold.
pub const SIDEBAR_FONT_SIZE: f64 = 13.0;
pub const SIDEBAR_FONT_WEIGHT: Weight = Weight::Semibold;

/// What the sidebar button and the page title say for `page`.
pub fn sidebar_label(page: &str) -> &str {
    SIDEBAR_LABELS
        .iter()
        .find(|(id, _)| *id == page)
        .map_or(page, |(_, label)| label)
}

/// The sidebar button title; the plain page id if the label would not fit.
///
/// `measure` is the real font measurement (13pt semibold) in the AppKit Hub;
/// without it the label is used as is.
pub fn sidebar_title(page: &str, measure: Option<TextMeasure<'_>>, width: i32) -> String {
    let title = format!("{SIDEBAR_TITLE_PREFIX}{}", sidebar_label(page));
    if let Some(measure) = measure {
        if measure(&title, SIDEBAR_FONT_SIZE, SIDEBAR_FONT_WEIGHT)
            > f64::from(width - SIDEBAR_TITLE_INSET)
        {
            return format!("{SIDEBAR_TITLE_PREFIX}{page}");
        }
    }
    title
}

// --------------------------------------------------------------------- home ---

pub const HOME_DECORATION_WIDTH: i32 = 150;
/// Between the decoration and the stat cards below it.
pub const HOME_DECORATION_GAP: i32 = 8;

/// The halftone's frame: the right end of the header band only, ending above
/// the stat cards (which start at `HUB_HEADER_OFFSET`).
pub fn home_decoration_frame(width: i32) -> Frame {
    (
        width - HOME_DECORATION_WIDTH,
        0,
        HOME_DECORATION_WIDTH,
        HUB_HEADER_OFFSET - HOME_DECORATION_GAP,
    )
}

/// `(columns, card_width)` for `count` stat cards across `width` points.
pub fn stat_layout(width: i32, count: i32) -> (i32, i32) {
    let mut columns = count.max(1);
    while columns > 1
        && floor_div(width - STAT_GAP * (columns - 1), columns) < STAT_FOUR_COLUMN_MIN_CARD
    {
        columns /= 2;
    }
    (
        columns,
        floor_div(width - STAT_GAP * (columns - 1), columns),
    )
}

/// [`stat_layout`] for the usual four cards.
pub fn stat_layout_default(width: i32) -> (i32, i32) {
    stat_layout(width, 4)
}

fn local_from_timestamp(seconds: f64) -> NaiveDateTime {
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9) as u32;
    Local
        .timestamp_opt(whole as i64, nanos.min(999_999_999))
        .single()
        .map_or(chrono::DateTime::UNIX_EPOCH.naive_utc(), |t| {
            t.naive_local()
        })
}

// Pure helpers of the Home and History pages (lf-ui `mac::pages::{home,history}`).

/// The Home page's dictation switch state (`hub_model.dictation_state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictationState {
    pub enabled: bool,
    pub label: String,
    pub action: &'static str,
    pub detail: &'static str,
}

/// `dictation_state(config)`.
pub fn dictation_state(config: &Config) -> DictationState {
    let enabled = config.field_bool("dictation_enabled").unwrap_or(true);
    DictationState {
        enabled,
        label: format!("Dictation: {}", if enabled { "On" } else { "Off" }),
        action: if enabled {
            "Turn Dictation Off"
        } else {
            "Turn Dictation On"
        },
        detail: if enabled {
            "Dictation is on — push-to-talk is ready"
        } else {
            "Dictation is off — push-to-talk is paused"
        },
    }
}

/// Rows per History page (`_render_history` shows 8 and probes a 9th).
pub const HISTORY_PAGE_SIZE: i64 = 8;
/// Height of one History/Home row.
pub const HISTORY_ROW_HEIGHT: i32 = 56;
/// Python's `(text, size)` fallback when an entry has no text.
pub const HISTORY_NO_TEXT: &str = "(no text)";

/// `final_text or raw_text or "(no text)"`.
pub fn history_row_text(final_text: &str, raw_text: &str) -> String {
    if !final_text.is_empty() {
        final_text.to_string()
    } else if !raw_text.is_empty() {
        raw_text.to_string()
    } else {
        HISTORY_NO_TEXT.to_string()
    }
}

/// `"3:05 PM"` for today, `"Nov 5"` otherwise (`_history_row`).
pub fn history_date_label(when: NaiveDateTime, now: NaiveDateTime) -> String {
    if when.date() == now.date() {
        let (pm, hour12) = when.hour12();
        format!(
            "{hour12}:{:02} {}",
            when.minute(),
            if pm { "PM" } else { "AM" }
        )
    } else {
        // `strftime("%b %d").replace(" 0", " ")`
        format!("{} {}", when.format("%b"), when.day())
    }
}

/// Local date-time of a history timestamp (seconds since the epoch).
pub fn history_when(ts: f64) -> NaiveDateTime {
    local_from_timestamp(ts)
}

/// `"{app or Unknown}  ·  {date}  ·  {Kind}"` (`kind.title()`).
pub fn history_meta_line(
    app_name: Option<&str>,
    ts: f64,
    kind: &str,
    now: NaiveDateTime,
) -> String {
    let app = app_name.filter(|a| !a.is_empty()).unwrap_or("Unknown");
    let mut title = String::new();
    let mut start = true;
    for c in kind.chars() {
        if c.is_alphabetic() {
            if start {
                title.extend(c.to_uppercase());
            } else {
                title.extend(c.to_lowercase());
            }
            start = false;
        } else {
            title.push(c);
            start = true;
        }
    }
    format!(
        "{app}  ·  {}  ·  {title}",
        history_date_label(history_when(ts), now)
    )
}

/// The status chip of a non-`ok` entry: `(label, failed)`.
pub fn history_status_chip(status: &str) -> Option<(&'static str, bool)> {
    match status {
        "ok" => None,
        "failed" => Some(("Failed", true)),
        _ => Some(("Silent", false)),
    }
}

/// `(title, width)` of the per-row action buttons, in order (Copy, Original,
/// Retry for dictations that kept their audio, Delete).
pub fn history_row_actions(kind: &str, has_audio: bool) -> Vec<(&'static str, i32)> {
    let mut entries = vec![("Copy", 56), ("Original", 76)];
    if kind == "dictate" && has_audio {
        entries.push(("Retry", 60));
    }
    entries.push(("Delete", 72));
    entries
}

/// x where the action buttons start in a row `width` wide (`width` when the
/// row has no actions).
pub fn history_action_start(width: i32, actions: &[(&'static str, i32)]) -> i32 {
    if actions.is_empty() {
        return width;
    }
    width - 12 - actions.iter().map(|(_, w)| *w).sum::<i32>() - 8 * (actions.len() as i32 - 1)
}

/// Geometry of one row's text and chip: `(text_width, chip_x)`; `chip_x` is
/// `None` for an `ok` entry.
pub fn history_row_text_layout(
    width: i32,
    action_start: Option<i32>,
    failed_or_silent: bool,
) -> (i32, Option<i32>) {
    let mut text_width = match action_start {
        Some(start) => start - 24,
        None => width - 40,
    };
    if !failed_or_silent {
        return (text_width, None);
    }
    let chip_x = match action_start {
        Some(start) => start - 82,
        None => width - 102,
    };
    text_width = chip_x - 26;
    (text_width, Some(chip_x))
}

/// The four Home stat cards `(title, value)` (`_page_home`).
pub fn home_stat_cards(stats: &lf_core::history::Stats) -> [(&'static str, String); 4] {
    let plural = |n: i64| if n == 1 { "" } else { "s" };
    [
        ("Words", stats.total_words.to_string()),
        ("Avg WPM", format!("{:.1}", stats.avg_wpm)),
        (
            "Streak",
            format!(
                "{} day{}",
                stats.current_streak_days,
                plural(stats.current_streak_days)
            ),
        ),
        ("Dictations", stats.total_dictations.to_string()),
    ]
}

/// The "Today … Last 7 days … Longest streak …" line under the cards.
pub fn home_stats_summary(stats: &lf_core::history::Stats) -> String {
    format!(
        "Today  {} words     Last 7 days  {} words     Longest streak  {} day{}",
        stats.words_today,
        stats.words_7d,
        stats.longest_streak_days,
        if stats.longest_streak_days == 1 {
            ""
        } else {
            "s"
        }
    )
}

/// `(title, detail, button, action id)` of an empty list. `query` is the
/// active History search (`None` on Home, which says "Your recent dictations").
pub fn history_empty_state(
    searching: bool,
    home: bool,
) -> (&'static str, &'static str, &'static str, &'static str) {
    if searching {
        (
            "No matching entries",
            "Try another search or start a new dictation.",
            "Clear search",
            "clearSearch",
        )
    } else if home {
        (
            "No dictations yet",
            "Your recent dictations will appear here.",
            "Open setup",
            "openSetup",
        )
    } else {
        (
            "No dictations yet",
            "Your dictations will appear here.",
            "Open setup",
            "openSetup",
        )
    }
}

/// `"Page N"` for the History pager.
pub fn history_page_label(offset: i64) -> String {
    format!("Page {}", offset / HISTORY_PAGE_SIZE + 1)
}

/// Offset after "Newer".
pub fn history_newer_offset(offset: i64) -> i64 {
    (offset - HISTORY_PAGE_SIZE).max(0)
}

/// Offset after "Older".
pub fn history_older_offset(offset: i64) -> i64 {
    offset + HISTORY_PAGE_SIZE
}

/// Text copied by Copy (`final_text`) / Original (`raw_text`).
pub const HISTORY_CLEAR_CONFIRM: &str = "Clear all history and stored clips?";
/// Shown when Retry is pressed: re-transcription needs speech-to-text, which
/// this Rust build does not contain.
pub const HISTORY_RETRY_UNAVAILABLE: &str =
    "Retry needs speech-to-text, which isn't available in this build.";

#[cfg(test)]
mod home_history_tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn dictation_state_tracks_enabled_config() {
        let mut config = Config::defaults_for(true);
        let state = dictation_state(&config);
        assert!(state.enabled);
        assert_eq!(state.label, "Dictation: On");
        assert_eq!(state.action, "Turn Dictation Off");
        assert_eq!(state.detail, "Dictation is on — push-to-talk is ready");
        config.set_field("dictation_enabled", serde_json::Value::Bool(false));
        let state = dictation_state(&config);
        assert!(!state.enabled);
        assert_eq!(state.label, "Dictation: Off");
        assert_eq!(state.action, "Turn Dictation On");
        assert_eq!(state.detail, "Dictation is off — push-to-talk is paused");
    }

    #[test]
    fn row_text_and_dates_follow_python() {
        assert_eq!(history_row_text("final", "raw"), "final");
        assert_eq!(history_row_text("", "raw"), "raw");
        assert_eq!(history_row_text("", ""), "(no text)");
        let now = dt(2026, 11, 16, 20, 0);
        assert_eq!(history_date_label(dt(2026, 11, 16, 15, 5), now), "3:05 PM");
        assert_eq!(history_date_label(dt(2026, 11, 16, 0, 7), now), "12:07 AM");
        assert_eq!(history_date_label(dt(2026, 11, 5, 9, 0), now), "Nov 5");
        assert_eq!(history_date_label(dt(2026, 11, 15, 9, 0), now), "Nov 15");
    }

    #[test]
    fn row_actions_and_layout() {
        let a = history_row_actions("dictate", true);
        assert_eq!(
            a.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
            ["Copy", "Original", "Retry", "Delete"]
        );
        assert_eq!(history_row_actions("dictate", false).len(), 3);
        assert_eq!(history_row_actions("note", true).len(), 3);
        // 1000 - 12 - (56+76+60+72) - 8*3
        assert_eq!(history_action_start(1000, &a), 1000 - 12 - 264 - 24);
        assert_eq!(history_action_start(1000, &[]), 1000);
        assert_eq!(history_row_text_layout(1000, None, false), (960, None));
        assert_eq!(
            history_row_text_layout(1000, None, true),
            (1000 - 102 - 26, Some(898))
        );
        assert_eq!(history_row_text_layout(1000, Some(600), false), (576, None));
        assert_eq!(
            history_row_text_layout(1000, Some(600), true),
            (492, Some(518))
        );
    }

    #[test]
    fn chips_and_titles() {
        assert_eq!(history_status_chip("ok"), None);
        assert_eq!(history_status_chip("failed"), Some(("Failed", true)));
        assert_eq!(history_status_chip("silent"), Some(("Silent", false)));
        let now = dt(2026, 11, 16, 20, 0);
        let when = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        let ts = Local
            .from_local_datetime(&when.and_hms_opt(9, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .timestamp() as f64;
        assert_eq!(
            history_meta_line(None, ts, "dictate", now),
            "Unknown  ·  Nov 5  ·  Dictate"
        );
        assert_eq!(
            history_meta_line(Some("Mail"), ts, "MEETING note", now),
            "Mail  ·  Nov 5  ·  Meeting Note"
        );
    }

    #[test]
    fn paging_and_empty_states() {
        assert_eq!(history_page_label(0), "Page 1");
        assert_eq!(history_page_label(16), "Page 3");
        assert_eq!(history_newer_offset(4), 0);
        assert_eq!(history_newer_offset(16), 8);
        assert_eq!(history_older_offset(8), 16);
        assert_eq!(history_empty_state(true, false).3, "clearSearch");
        assert_eq!(
            history_empty_state(false, true).1,
            "Your recent dictations will appear here."
        );
        assert_eq!(
            history_empty_state(false, false).1,
            "Your dictations will appear here."
        );
    }

    #[test]
    fn stat_cards_pluralise() {
        let stats = lf_core::history::Stats {
            total_words: 1234,
            total_dictations: 9,
            avg_wpm: 101.25,
            words_today: 3,
            words_7d: 40,
            current_streak_days: 1,
            longest_streak_days: 5,
            top_apps: vec![],
        };
        let cards = home_stat_cards(&stats);
        assert_eq!(cards[1], ("Avg WPM", "101.2".to_string()));
        assert_eq!(cards[2], ("Streak", "1 day".to_string()));
        assert_eq!(
            home_stats_summary(&stats),
            "Today  3 words     Last 7 days  40 words     Longest streak  5 days"
        );
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;
    use crate::pystr::PyStr;
    #[test]
    fn models_sidebar_label_is_display_only_and_fits() {
        assert_eq!(sidebar_label("Models"), "Models & keys");
        assert_eq!(sidebar_label("Home"), "Home");
        let measure = |text: &str, size: f64, weight: Weight| {
            assert_eq!((size, weight), (13.0, Weight::Semibold));
            text.chars().count() as f64 * 8.0
        };
        let title = sidebar_title("Models", Some(&measure), SIDEBAR_BUTTON_WIDTH);
        assert_eq!(title.py_strip(), "Models & keys");
        assert!(
            measure(&title, 13.0, Weight::Semibold)
                <= f64::from(SIDEBAR_BUTTON_WIDTH - SIDEBAR_TITLE_INSET)
        );
        for page in crate::hub_route::HUB_PAGES {
            let title = sidebar_title(page, Some(&measure), SIDEBAR_BUTTON_WIDTH);
            assert!(measure(&title, 13.0, Weight::Semibold) <= 152.0, "{page}");
        }
        // A label that would not fit falls back to the page id, never truncates.
        let huge = |_: &str, _: f64, _: Weight| 999.0;
        assert_eq!(
            sidebar_title("Models", Some(&huge), SIDEBAR_BUTTON_WIDTH).py_strip(),
            "Models"
        );
        // Without a measure the label is used as is.
        assert_eq!(
            sidebar_title("Models", None, 1),
            "\u{2002}\u{2002}Models & keys"
        );
    }

    /// `hub_model.SIDEBAR_TITLE_PREFIX` is two U+2002 EN SPACE characters, not
    /// ASCII spaces; the codepoints are spelled out so a normalising edit fails.
    #[test]
    fn sidebar_title_prefix_is_two_en_spaces() {
        assert_eq!(
            SIDEBAR_TITLE_PREFIX
                .chars()
                .map(|c| c as u32)
                .collect::<Vec<_>>(),
            vec![0x2002, 0x2002]
        );
        assert_eq!(SIDEBAR_TITLE_PREFIX.len(), 6);
        assert!(!SIDEBAR_TITLE_PREFIX.contains(' '));
        let huge = |_: &str, _: f64, _: Weight| 999.0;
        let fallback = sidebar_title("Models", Some(&huge), SIDEBAR_BUTTON_WIDTH);
        assert_eq!(
            fallback
                .chars()
                .take(2)
                .map(|c| c as u32)
                .collect::<Vec<_>>(),
            vec![0x2002, 0x2002]
        );
        assert_eq!(fallback, "\u{2002}\u{2002}Models");
    }

    #[test]
    fn stat_layout_and_decoration_stay_within_the_page() {
        assert_eq!(stat_layout_default(640), (4, 151));
        assert_eq!(stat_layout_default(500), (2, 244));
        assert_eq!(stat_layout(100, 0), (1, 100));
        for width in [400, 640, 1040] {
            let (x, _, w, _) = home_decoration_frame(width);
            assert!(x >= 0 && x + w <= width);
        }
    }
}

/// Python `str.title()` (first letter of each cased run upper, rest lower).
fn py_title(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_cased = false;
    for c in text.chars() {
        if prev_cased {
            out.extend(c.to_lowercase());
        } else {
            out.extend(c.to_uppercase());
        }
        prev_cased = c.is_lowercase() || c.is_uppercase();
    }
    out
}

/// A non-empty string field of an entry, like Python `entry.get(key) or ...`
/// (numbers stringify; other types are treated as absent).
fn text_field(entry: &Map<String, Value>, key: &str) -> Option<String> {
    match entry.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub const STYLE_LABEL_W: i32 = 160;
pub const STYLE_GROUP_X: i32 = 180;
pub const STYLE_ROW_H: i32 = 86;
pub const STYLE_EXAMPLE_Y: i32 = 28;
/// Two 11pt lines: examples wrap instead of truncating.
pub const STYLE_EXAMPLE_H: i32 = 34;
pub const STYLE_SAVE_FAILED: &str =
    "Couldn't save your style settings. Check that ~/.localflow can be written to and try again.";
pub const STYLE_AUTOSAVE_HINT: &str = "Changes save automatically.";

/// Label, wrapped example and segmented group of one style row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyleRowFrames {
    pub label: Frame,
    pub example: Frame,
    pub group: Frame,
}

impl StyleRowFrames {
    pub fn all(&self) -> [Frame; 3] {
        [self.label, self.example, self.group]
    }
}

/// The label and its example share the left column (`STYLE_LABEL_W` wide); the
/// example wraps over two lines inside the row, and the group starts right of
/// the column, so nothing overlaps at any width.
pub fn style_row_frames(y: i32, group_width: i32) -> StyleRowFrames {
    StyleRowFrames {
        label: (0, y + 4, STYLE_LABEL_W, 22),
        example: (0, y + STYLE_EXAMPLE_Y, STYLE_LABEL_W, STYLE_EXAMPLE_H),
        group: (STYLE_GROUP_X, y, group_width, 36),
    }
}

/// The config update (for `lf_core::config::apply_config_update`) for the
/// Style page's current selections: `{"cleanup": {"level": .., "style_<name>": ..}}`.
pub fn style_update(cleanup: &str, styles: &[(&str, &str)]) -> Value {
    let mut section = Map::new();
    section.insert("level".into(), cleanup.into());
    for (name, value) in styles {
        section.insert(format!("style_{name}"), (*value).into());
    }
    json!({ "cleanup": Value::Object(section) })
}

// ------------------------------------------------------------------ hotkeys ---

const KEY_NAMES: [(&str, &str); 12] = [
    ("alt_r", "Right Option"),
    ("alt_l", "Left Option"),
    ("alt", "Option"),
    ("ctrl_r", "Right Control"),
    ("ctrl_l", "Left Control"),
    ("ctrl", "Control"),
    ("shift_r", "Right Shift"),
    ("shift_l", "Left Shift"),
    ("shift", "Shift"),
    ("cmd_r", "Right Command"),
    ("cmd_l", "Left Command"),
    ("cmd", "Command"),
];

/// Human label for a config hotkey; an empty value means disabled.
pub fn format_hotkey(value: Option<&str>) -> String {
    let value = value.unwrap_or("").py_strip();
    if value.is_empty() {
        return "Off".to_string();
    }
    value
        .split('+')
        .map(|part| {
            let part = part.py_strip().trim_matches(['<', '>']).to_lowercase();
            if let Some((_, name)) = KEY_NAMES.iter().find(|(key, _)| *key == part) {
                (*name).to_string()
            } else if part.chars().count() == 1 {
                part.to_uppercase()
            } else {
                py_title(&part)
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// One row of the Prompts page's shortcut card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutRow {
    pub name: &'static str,
    pub detail: &'static str,
    pub hotkey: String,
}

/// The dictation shortcuts, read from the config's current hotkeys. None of
/// them has an enable flag of its own: an empty hotkey disables it.
pub fn shortcut_rows(config: &Config) -> Vec<ShortcutRow> {
    let hotkey = |field: &str| format_hotkey(config.field_str(field));
    vec![
        ShortcutRow {
            name: "Prompt Engineer",
            detail: "Select rough text, press the shortcut, get an AI-ready prompt.",
            hotkey: hotkey("prompt_hotkey"),
        },
        ShortcutRow {
            name: "Polish",
            detail: "Select text and clean up its wording and grammar in place.",
            hotkey: hotkey("polish_hotkey"),
        },
        ShortcutRow {
            name: "Command Mode",
            detail: "Hold the key and speak an instruction to rewrite your selection.",
            hotkey: hotkey("command_hotkey"),
        },
        ShortcutRow {
            name: "Toggle Dictation",
            detail: "Turn push-to-talk on or off without quitting LocalFlow.",
            hotkey: hotkey("dictation_toggle_hotkey"),
        },
    ]
}

// --------------------------------------------------------------- dictionary ---

pub const DICTIONARY_SAVE_FAILED: &str =
    "Couldn't save to your dictionary. Check that ~/.localflow can be written to and try again.";

/// The dictionary operations the Hub performs. Implemented for
/// [`DictionaryStore`]; tests inject failing stores.
pub trait DictionaryEditor {
    fn add_word(&self, word: &str) -> anyhow::Result<bool>;
    fn update_word(&self, old: &str, new: &str) -> anyhow::Result<bool>;
    fn add_replacement(&self, source: &str, target: &str) -> anyhow::Result<bool>;
    fn update_replacement(
        &self,
        old_from: &str,
        new_from: &str,
        new_to: &str,
    ) -> anyhow::Result<bool>;
    fn remove(&self, value: &str, kind: Option<&str>) -> anyhow::Result<bool>;
}

impl DictionaryEditor for DictionaryStore {
    fn add_word(&self, word: &str) -> anyhow::Result<bool> {
        DictionaryStore::add_word(self, word)
    }
    fn update_word(&self, old: &str, new: &str) -> anyhow::Result<bool> {
        DictionaryStore::update_word(self, old, new)
    }
    fn add_replacement(&self, source: &str, target: &str) -> anyhow::Result<bool> {
        DictionaryStore::add_replacement(self, source, target)
    }
    fn update_replacement(
        &self,
        old_from: &str,
        new_from: &str,
        new_to: &str,
    ) -> anyhow::Result<bool> {
        DictionaryStore::update_replacement(self, old_from, new_from, new_to)
    }
    fn remove(&self, value: &str, kind: Option<&str>) -> anyhow::Result<bool> {
        DictionaryStore::remove(self, value, kind)
    }
}

/// What a dictionary list row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Word,
    Replacement,
}

impl EntryKind {
    /// The store's `kind` argument (`"word"` / `"replacement"`).
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Word => "word",
            EntryKind::Replacement => "replacement",
        }
    }
}

/// `(label, key, kind, target)` of a dictionary row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictionaryRow {
    pub label: String,
    pub key: String,
    pub kind: EntryKind,
    pub target: String,
}

/// A row for every word and replacement; nothing capped.
pub fn dictionary_entries(data: &DictionaryData) -> Vec<DictionaryRow> {
    let mut entries: Vec<DictionaryRow> = data
        .words
        .iter()
        .map(|word| DictionaryRow {
            label: word.clone(),
            key: word.clone(),
            kind: EntryKind::Word,
            target: String::new(),
        })
        .collect();
    entries.extend(data.replacements.iter().map(|pair| DictionaryRow {
        label: format!("{} → {}", pair.from, pair.to),
        key: pair.from.clone(),
        kind: EntryKind::Replacement,
        target: pair.to.clone(),
    }));
    entries
}

fn is_dictionary_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DictionaryError>().is_some()
}

/// Add (or, when `editing`, rename) a word. Returns a friendly error or `None`.
pub fn submit_word(
    store: &dyn DictionaryEditor,
    text: &str,
    editing: Option<&str>,
) -> Option<String> {
    if text.py_strip().is_empty() {
        return Some("Type a word first.".to_string());
    }
    let outcome = match editing.filter(|e| !e.is_empty()) {
        Some(old) => store.update_word(old, text).map(|ok| {
            (!ok).then(|| "That word was removed or is already in your dictionary.".to_string())
        }),
        None => store.add_word(text).map(|ok| {
            (!ok).then(|| format!("“{}” is already in your dictionary.", text.py_strip()))
        }),
    };
    match outcome {
        Ok(message) => message,
        Err(error) if is_dictionary_error(&error) => Some("Type a word first.".to_string()),
        Err(_) => Some(DICTIONARY_SAVE_FAILED.to_string()),
    }
}

/// Add (or edit) an auto-replace pair. Returns a friendly error or `None`.
pub fn submit_replacement(
    store: &dyn DictionaryEditor,
    source: &str,
    target: &str,
    editing: Option<&str>,
) -> Option<String> {
    if source.py_strip().is_empty() {
        return Some("Type what you say first.".to_string());
    }
    if target.py_strip().is_empty() {
        return Some("Type what it should become.".to_string());
    }
    let outcome = match editing.filter(|e| !e.is_empty()) {
        Some(old) => store
            .update_replacement(old, source, target)
            .map(|ok| (!ok).then(|| "That replacement was removed or already exists.".to_string())),
        None => store.add_replacement(source, target).map(|ok| {
            (!ok).then(|| {
                format!(
                    "You already have a replacement for “{}”.",
                    source.py_strip()
                )
            })
        }),
    };
    match outcome {
        Ok(message) => message,
        Err(error) if is_dictionary_error(&error) => {
            Some("Fill in both “When I say” and “Type this”.".to_string())
        }
        Err(_) => Some(DICTIONARY_SAVE_FAILED.to_string()),
    }
}

/// Remove one dictionary entry (of `kind`). Returns a friendly error or `None`.
pub fn remove_entry(
    store: &dyn DictionaryEditor,
    key: &str,
    kind: Option<EntryKind>,
) -> Option<String> {
    match store.remove(key, kind.map(EntryKind::as_str)) {
        Ok(_) => None,
        Err(_) => Some(DICTIONARY_SAVE_FAILED.to_string()),
    }
}

// ------------------------------------------------------------------ prompts ---

pub const PROMPTS_EMPTY: &str = "Prompts generated from meetings show up here for approval.";
pub const PROMPTS_UNREADABLE: &str =
    "Couldn't read your prompt queue. Check ~/.localflow and try again.";
/// Status pill: `(status, (tint token, text token))`.
pub const PROMPT_STATUS_PILLS: [(&str, (&str, &str)); 5] = [
    ("pending", ("sun", "ink")),
    ("approved", ("success", "success")),
    ("rejected", ("line", "muted")),
    ("dispatched", ("cobalt", "cobalt")),
    ("failed", ("red", "red")),
];

/// `PROMPT_STATUS_PILLS[status]`.
pub fn prompt_status_pill(status: &str) -> Option<(&'static str, &'static str)> {
    PROMPT_STATUS_PILLS
        .iter()
        .find(|(name, _)| *name == status)
        .map(|(_, pill)| *pill)
}

/// Where a prompt came from. The queue stores `repo` and `meeting`, not a
/// project.
pub fn prompt_context(entry: &PromptEntry) -> String {
    text_field(entry, "repo")
        .or_else(|| text_field(entry, "meeting"))
        .unwrap_or_else(|| "Unknown meeting".to_string())
}

pub const ORCA_NOT_OPEN: &str = "Couldn't send — the Orca agent app isn't open";

/// The failure shown on a prompt row: shared copy for a known cause only.
///
/// "Orca app is not running — run `orca open`…" (what `orca_available`
/// records) and the older "Orca is not running" map to [`ORCA_NOT_OPEN`];
/// other failures (a missing CLI, an unreachable runtime, a bad repo, a
/// timeout) keep their own text, whitespace collapsed.
pub fn prompt_error_text(error: &str) -> String {
    use std::sync::OnceLock;
    static NOT_RUNNING: OnceLock<Regex> = OnceLock::new();
    let re = NOT_RUNNING
        .get_or_init(|| Regex::new(r"(?i)\borca(?: app)? is not running\b").expect("static regex"));
    let text = error.py_split().join(" ");
    if re.is_match(&text) {
        ORCA_NOT_OPEN.to_string()
    } else {
        text
    }
}

/// One row of the Prompts page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRow {
    pub id: String,
    pub title: String,
    pub context: String,
    pub error: String,
    pub status: String,
    pub status_label: String,
    pub can_approve: bool,
    pub can_reject: bool,
}

/// Newest first. Buttons follow the same guards the server API enforces.
pub fn prompt_rows(entries: &[PromptEntry]) -> Vec<PromptRow> {
    entries
        .iter()
        .rev()
        .map(|entry| {
            let status = text_field(entry, "status").unwrap_or_else(|| "pending".to_string());
            let mut context = prompt_context(entry);
            let error = if status == "failed" {
                prompt_error_text(&text_field(entry, "error").unwrap_or_default())
            } else {
                String::new()
            };
            if !error.is_empty() {
                context = format!("{context} · {error}");
            }
            PromptRow {
                id: text_field(entry, "id").unwrap_or_default(),
                title: text_field(entry, "title").unwrap_or_else(|| "Untitled prompt".to_string()),
                context,
                error,
                status_label: py_title(&status),
                can_approve: APPROVE_FROM.contains(&status.as_str()),
                can_reject: REJECT_FROM.contains(&status.as_str()),
                status,
            }
        })
        .collect()
}

pub const PROMPT_ROW_MIN_H: i32 = 68;
pub const PROMPT_TITLE_Y: i32 = 12;
pub const PROMPT_CONTEXT_Y: i32 = 36;
pub const PROMPT_CONTEXT_LINE: i32 = 16;
pub const PROMPT_ROW_BOTTOM: i32 = 14;
pub const PROMPT_BUTTON_APPROVE: i32 = 88;
pub const PROMPT_BUTTON_REJECT: i32 = 80;
pub const PROMPT_BUTTON_GAP: i32 = 8;
pub const PROMPT_EDGE: i32 = 12;
/// Font of the prompt context line: 11pt regular.
pub const PROMPT_CONTEXT_FONT_SIZE: f64 = 11.0;

/// Geometry of one prompt row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRowLayout {
    pub action_x: i32,
    pub pill_x: i32,
    pub text_width: i32,
    pub context_h: i32,
    pub height: i32,
    pub lines: usize,
}

/// Geometry of one prompt row: buttons, pill, text column and height.
///
/// The context line (source plus any failure) wraps inside the text column
/// and the row grows to fit it, so a friendly failure sentence is never
/// truncated even at the minimum window width with both actions shown.
/// `measure` is the real 11pt measurement in AppKit; without it a
/// conservative `6.5 pt per character` estimate is used.
pub fn prompt_row_layout(
    width: i32,
    row: &PromptRow,
    pill_width: i32,
    measure: Option<TextMeasure<'_>>,
) -> PromptRowLayout {
    let mut widths = Vec::new();
    if row.can_approve {
        widths.push(PROMPT_BUTTON_APPROVE);
    }
    if row.can_reject {
        widths.push(PROMPT_BUTTON_REJECT);
    }
    let gaps = PROMPT_BUTTON_GAP * (widths.len() as i32 - 1).max(0);
    let action_x = width - PROMPT_EDGE - widths.iter().sum::<i32>() - gaps;
    let pill_x = action_x - pill_width - 12;
    let text_width = (pill_x - 12 - 14).max(40);
    let estimate = |text: &str, _size: f64, _weight: Weight| text.chars().count() as f64 * 6.5;
    let measure: TextMeasure<'_> = measure.unwrap_or(&estimate);
    let lines = wrap_lines(&row.context, f64::from(text_width), &|text: &str| {
        measure(text, PROMPT_CONTEXT_FONT_SIZE, Weight::Regular)
    });
    let context_h = (lines.len() as i32).max(1) * PROMPT_CONTEXT_LINE + 2;
    let height = PROMPT_ROW_MIN_H.max(PROMPT_CONTEXT_Y + context_h + PROMPT_ROW_BOTTOM);
    PromptRowLayout {
        action_x,
        pill_x,
        text_width,
        context_h,
        height,
        lines: lines.len(),
    }
}

/// Approve or reject (the two buttons of a prompt row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewAction {
    Approve,
    Reject,
}

/// Approve or reject through the same guarded helpers the server uses.
///
/// Returns a friendly, actionable message when it could not be done.
pub fn review_prompt(
    queue: &dyn PromptQueue,
    entry_id: &str,
    action: ReviewAction,
) -> Option<String> {
    let outcome = match action {
        ReviewAction::Approve => approve_prompt(queue, entry_id),
        ReviewAction::Reject => reject_prompt(queue, entry_id),
    };
    match outcome {
        Ok(_) => None,
        Err(TransitionFailure::Refused(refused)) => {
            if refused.status_code == 404 {
                return Some(
                    "That prompt is no longer in the queue. The list was refreshed.".into(),
                );
            }
            let past = match action {
                ReviewAction::Approve => "approved",
                ReviewAction::Reject => "rejected",
            };
            Some(format!(
                "That prompt changed before it could be {past}, so nothing was changed. The list was refreshed."
            ))
        }
        Err(TransitionFailure::Queue(_)) => {
            Some("Couldn't update the prompt queue. Check ~/.localflow and try again.".into())
        }
    }
}

/// Queue entries for the Prompts page, or the friendly error when the queue is
/// unreadable (`_prompt_entries`).
pub fn prompt_entries(queue: &dyn PromptQueue) -> Result<Vec<PromptEntry>, &'static str> {
    queue.list(None).map_err(|_| PROMPTS_UNREADABLE)
}

/// Shown when importing a dictionary file fails (`importDictionary_`).
pub const DICTIONARY_IMPORT_FAILED: &str =
    "Couldn't import that file. Choose a CSV or JSON file of words and replacements.";

/// `Imported {added}; duplicates {skipped_duplicate}; invalid {skipped_invalid}.`
pub fn dictionary_import_notice(counts: &lf_core::dictionary::ImportCounts) -> String {
    format!(
        "Imported {}; duplicates {}; invalid {}.",
        counts.added, counts.skipped_duplicate, counts.skipped_invalid
    )
}

/// The dictionary card a submit belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictionaryCard {
    Word,
    Replacement,
}

/// What the Dictionary page keeps across rebuilds: the three typed fields and
/// the entries being edited (`_preserve_page_state` / `_restore_page_state`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DictionaryDraft {
    pub word: String,
    pub source: String,
    pub target: String,
    pub editing_word: Option<String>,
    pub editing_source: Option<String>,
}

impl DictionaryDraft {
    /// `_sync_dictionary_buttons`: the add-word button title.
    pub fn word_button_title(&self) -> &'static str {
        if self.editing_word.as_deref().is_some_and(|w| !w.is_empty()) {
            "Save word"
        } else {
            "Add word"
        }
    }

    /// `_sync_dictionary_buttons`: the add-replacement button title.
    pub fn replacement_button_title(&self) -> &'static str {
        if self
            .editing_source
            .as_deref()
            .is_some_and(|s| !s.is_empty())
        {
            "Save replacement"
        } else {
            "Add replacement"
        }
    }

    /// `editDictionary_`: load a row into its card's fields.
    pub fn begin_edit(&mut self, row: &DictionaryRow) {
        match row.kind {
            EntryKind::Word => {
                self.editing_word = Some(row.key.clone());
                self.word = row.key.clone();
            }
            EntryKind::Replacement => {
                self.editing_source = Some(row.key.clone());
                self.source = row.key.clone();
                self.target = row.target.clone();
            }
        }
    }

    /// The success branch of `_reload_dictionary`: clear the submitted card's
    /// fields and drop the editing id of an entry that no longer exists
    /// (compared case-insensitively, restricted to `forget_kind` when given).
    pub fn after_success(
        &mut self,
        clear: Option<DictionaryCard>,
        forget: Option<(&str, Option<EntryKind>)>,
    ) {
        match clear {
            Some(DictionaryCard::Word) => {
                self.word.clear();
                self.editing_word = None;
            }
            Some(DictionaryCard::Replacement) => {
                self.source.clear();
                self.target.clear();
                self.editing_source = None;
            }
            None => {}
        }
        if let Some((key, kind)) = forget.filter(|(key, _)| !key.is_empty()) {
            let same = |editing: &Option<String>| {
                editing.as_deref().is_some_and(|e| {
                    !e.is_empty()
                        && caseless::default_case_fold_str(e)
                            == caseless::default_case_fold_str(key)
                })
            };
            if matches!(kind, None | Some(EntryKind::Word)) && same(&self.editing_word) {
                self.editing_word = None;
            }
            if matches!(kind, None | Some(EntryKind::Replacement)) && same(&self.editing_source) {
                self.editing_source = None;
            }
        }
    }
}

/// Cleanup levels in segment order (`cleanup_options`).
pub const STYLE_CLEANUP_OPTIONS: [&str; 3] = ["none", "light", "medium"];
/// Segment titles of [`STYLE_CLEANUP_OPTIONS`].
pub const STYLE_CLEANUP_TITLES: [&str; 3] = ["None", "Light", "Medium"];
/// Style options in segment order (`style_options`).
pub const STYLE_OPTIONS: [&str; 4] = ["formal", "casual", "very_casual", "excited"];
/// `(category, example)` rows of the Style page, in display order.
pub const STYLE_CATEGORIES: [(&str, &str); 4] = [
    ("work", "Please share the updated timeline."),
    ("email", "Thanks for following up."),
    ("personal", "Let's catch up soon!"),
    ("other", "The main point is clear."),
];

/// `value.replace("_", " ").title()`: `very_casual` -> `Very Casual`.
pub fn style_option_title(value: &str) -> String {
    py_title(&value.replace('_', " "))
}

/// The row label, `f"{category.title()} style"`.
pub fn style_category_label(category: &str) -> String {
    format!("{} style", py_title(category))
}

/// `_style_autosave`'s failure text: a rejected config shows its own message,
/// anything else (an unwritable file) the generic one.
pub fn style_save_error_text(error: &anyhow::Error) -> String {
    match error.downcast_ref::<lf_core::config::ConfigError>() {
        Some(config_error) => config_error.to_string(),
        None => STYLE_SAVE_FAILED.to_string(),
    }
}

#[cfg(test)]
mod p2_page_logic_tests {
    use super::*;

    fn row(kind: EntryKind, key: &str, target: &str) -> DictionaryRow {
        DictionaryRow {
            label: key.into(),
            key: key.into(),
            kind,
            target: target.into(),
        }
    }

    #[test]
    fn edit_loads_the_row_and_switches_button_titles() {
        let mut draft = DictionaryDraft::default();
        assert_eq!(draft.word_button_title(), "Add word");
        assert_eq!(draft.replacement_button_title(), "Add replacement");
        draft.begin_edit(&row(EntryKind::Word, "Kubernetes", ""));
        assert_eq!(
            (draft.word.as_str(), draft.word_button_title()),
            ("Kubernetes", "Save word")
        );
        assert_eq!(draft.replacement_button_title(), "Add replacement");
        draft.begin_edit(&row(EntryKind::Replacement, "my email", "me@example.com"));
        assert_eq!(
            (
                draft.source.as_str(),
                draft.target.as_str(),
                draft.replacement_button_title()
            ),
            ("my email", "me@example.com", "Save replacement")
        );
        // An empty editing id is "not editing", like Python's truthiness.
        draft.editing_word = Some(String::new());
        assert_eq!(draft.word_button_title(), "Add word");
    }

    #[test]
    fn success_clears_only_the_submitted_card() {
        let mut draft = DictionaryDraft {
            word: "w".into(),
            source: "s".into(),
            target: "t".into(),
            editing_word: Some("w".into()),
            editing_source: Some("s".into()),
        };
        draft.after_success(Some(DictionaryCard::Word), None);
        assert_eq!(
            (draft.word.as_str(), draft.editing_word.clone()),
            ("", None)
        );
        assert_eq!((draft.source.as_str(), draft.target.as_str()), ("s", "t"));
        assert_eq!(draft.editing_source.as_deref(), Some("s"));
        draft.after_success(Some(DictionaryCard::Replacement), None);
        assert_eq!(draft, DictionaryDraft::default());
    }

    #[test]
    fn removing_an_edited_entry_forgets_it_case_insensitively_by_kind() {
        let mut draft = DictionaryDraft {
            editing_word: Some("Kubernetes".into()),
            editing_source: Some("kubernetes".into()),
            ..DictionaryDraft::default()
        };
        // A replacement removal leaves the word being edited alone.
        draft.after_success(None, Some(("KUBERNETES", Some(EntryKind::Replacement))));
        assert_eq!(draft.editing_word.as_deref(), Some("Kubernetes"));
        assert_eq!(draft.editing_source, None);
        draft.after_success(None, Some(("kubernetes", Some(EntryKind::Word))));
        assert_eq!(draft.editing_word, None);
        // Unrestricted kind drops both; an empty key forgets nothing; `ß` folds to `ss`.
        let mut both = DictionaryDraft {
            editing_word: Some("Straße".into()),
            editing_source: Some("STRASSE".into()),
            ..DictionaryDraft::default()
        };
        both.after_success(None, Some(("", None)));
        assert!(both.editing_word.is_some() && both.editing_source.is_some());
        both.after_success(None, Some(("strasse", None)));
        assert_eq!((both.editing_word, both.editing_source), (None, None));
    }

    #[test]
    fn import_notice_and_failure_texts_match_python() {
        let counts = lf_core::dictionary::ImportCounts {
            added: 3,
            skipped_duplicate: 1,
            skipped_invalid: 2,
        };
        assert_eq!(
            dictionary_import_notice(&counts),
            "Imported 3; duplicates 1; invalid 2."
        );
        assert_eq!(
            DICTIONARY_IMPORT_FAILED,
            "Couldn't import that file. Choose a CSV or JSON file of words and replacements."
        );
    }

    #[test]
    fn style_titles_labels_and_error_text() {
        assert_eq!(style_option_title("very_casual"), "Very Casual");
        assert_eq!(style_option_title("formal"), "Formal");
        assert_eq!(style_category_label("work"), "Work style");
        assert_eq!(STYLE_CLEANUP_OPTIONS.len(), STYLE_CLEANUP_TITLES.len());
        assert_eq!(STYLE_CATEGORIES[3].0, "other");
        let rejected = anyhow::Error::new(lf_core::config::ConfigError("bad level".into()));
        assert_eq!(style_save_error_text(&rejected), "bad level");
        assert_eq!(
            style_save_error_text(&anyhow::anyhow!("disk full")),
            STYLE_SAVE_FAILED
        );
    }
}

#[cfg(test)]
mod dictionary_style_prompt_tests {
    use super::*;
    use crate::backends::prompts::{FakePromptQueue, WorkPrompt};
    use crate::tokens::{HUB_CONTENT_X, HUB_MIN_WIDTH};
    const WIDTHS: [i32; 5] = [
        HUB_MIN_WIDTH - HUB_CONTENT_X - 32,
        640,
        760,
        1040 - HUB_CONTENT_X - 32,
        1400 - HUB_CONTENT_X - 32,
    ];
    fn overlaps(a: Frame, b: Frame) -> bool {
        a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
    }
    fn entry(value: Value) -> PromptEntry {
        value.as_object().unwrap().clone()
    }
    #[test]
    fn style_rows_never_overlap() {
        for width in WIDTHS {
            let group_width = 560.min(width - 180);
            let mut previous_bottom = 0;
            for index in 0..4 {
                let y = 58 + index * STYLE_ROW_H;
                let frames = style_row_frames(y, group_width);
                assert!(!overlaps(frames.label, frames.example));
                assert!(!overlaps(frames.example, frames.group));
                assert!(frames.example.0 + frames.example.2 < frames.group.0);
                assert!(frames.group.0 + frames.group.2 <= width);
                let bottom = frames.all().iter().map(|f| f.1 + f.3).max().unwrap();
                assert!(bottom <= y + STYLE_ROW_H && y >= previous_bottom);
                previous_bottom = bottom;
                // Two 11pt lines fit the example frame.
                assert!(frames.example.3 >= 2 * 15);
            }
        }
    }

    #[test]
    fn style_update_is_accepted_by_the_shared_config_validation() {
        use lf_core::config::apply_config_update;
        let config = Config::defaults_for(false);
        let update = style_update(
            "medium",
            &[
                ("work", "casual"),
                ("email", "formal"),
                ("personal", "very_casual"),
                ("other", "excited"),
            ],
        );
        assert_eq!(update["cleanup"]["level"], "medium");
        assert_eq!(update["cleanup"]["style_work"], "casual");
        let applied = apply_config_update(&config, &update).unwrap();
        assert_eq!(applied.field_str("cleanup_level"), Some("medium"));
        assert_eq!(applied.field_str("style_work"), Some("casual"));
        assert_eq!(applied.field_str("style_other"), Some("excited"));
        // Validation still runs: nothing is committed for a bad value.
        let bad = style_update("light", &[("work", "shouting")]);
        let error = apply_config_update(&config, &bad).unwrap_err().to_string();
        assert!(
            error.contains("invalid value for config key 'cleanup.style_work'"),
            "{error}"
        );
        assert!(!error.contains("/private"));
        assert!(!STYLE_SAVE_FAILED.contains("shouting"));
        assert_eq!(STYLE_AUTOSAVE_HINT, "Changes save automatically.");
    }

    #[test]
    fn hotkeys_are_formatted_for_humans() {
        assert_eq!(format_hotkey(None), "Off");
        assert_eq!(format_hotkey(Some("  ")), "Off");
        assert_eq!(format_hotkey(Some("alt_r")), "Right Option");
        assert_eq!(
            format_hotkey(Some("<ctrl>+<alt>+2")),
            "Control + Option + 2"
        );
        assert_eq!(
            format_hotkey(Some("<cmd>+<shift>+k")),
            "Command + Shift + K"
        );
        assert_eq!(format_hotkey(Some("<f5>")), "F5");
        assert_eq!(format_hotkey(Some("page_up")), "Page_Up");
        assert_eq!(format_hotkey(Some("ctrl + ÿ")), "Control + Ÿ");
        assert_eq!(py_title("hello wORLD 3rd"), "Hello World 3Rd");
    }

    #[test]
    fn shortcut_rows_follow_the_config_and_invent_no_enable_flag() {
        let mut config = Config::defaults_for(false);
        config.set_field("prompt_hotkey", json!("<ctrl>+<alt>+2"));
        config.set_field("polish_hotkey", json!(""));
        config.set_field("command_hotkey", json!("alt_r"));
        config.set_field("dictation_toggle_hotkey", json!(""));
        let rows = shortcut_rows(&config);
        let names: Vec<_> = rows.iter().map(|r| r.name).collect();
        assert_eq!(
            names,
            [
                "Prompt Engineer",
                "Polish",
                "Command Mode",
                "Toggle Dictation"
            ]
        );
        let keys: Vec<_> = rows.iter().map(|r| r.hotkey.as_str()).collect();
        assert_eq!(keys, ["Control + Option + 2", "Off", "Right Option", "Off"]);
        assert!(rows[0].detail.contains("Select rough text"));
        assert_eq!(
            rows[3].detail,
            "Turn push-to-talk on or off without quitting LocalFlow."
        );
        config.set_field("dictation_toggle_hotkey", json!("<ctrl>+<alt>+d"));
        assert_eq!(shortcut_rows(&config)[3].hotkey, "Control + Option + D");
        config.fields.remove("dictation_toggle_hotkey");
        assert_eq!(shortcut_rows(&config)[3].hotkey, "Off");
    }

    // ---- dictionary -----------------------------------------------------------

    fn store() -> (tempfile::TempDir, DictionaryStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = DictionaryStore::new(Some(tmp.path().join("dictionary.json")));
        (tmp, store)
    }

    #[test]
    fn dictionary_entries_are_not_capped() {
        let data = DictionaryData {
            words: (0..25).map(|i| format!("word{i}")).collect(),
            replacements: (0..7)
                .map(|i| lf_core::dictionary::Replacement {
                    from: format!("s{i}"),
                    to: format!("t{i}"),
                })
                .collect(),
        };
        let entries = dictionary_entries(&data);
        assert_eq!(entries.len(), 32);
        assert_eq!(
            entries[0],
            DictionaryRow {
                label: "word0".into(),
                key: "word0".into(),
                kind: EntryKind::Word,
                target: String::new()
            }
        );
        assert_eq!(
            entries[25],
            DictionaryRow {
                label: "s0 → t0".into(),
                key: "s0".into(),
                kind: EntryKind::Replacement,
                target: "t0".into()
            }
        );
        assert!(dictionary_entries(&DictionaryData::default()).is_empty());
    }

    #[test]
    fn empty_word_gets_a_friendly_message_and_changes_nothing() {
        let (_tmp, store) = store();
        assert_eq!(
            submit_word(&store, "", None).as_deref(),
            Some("Type a word first.")
        );
        assert_eq!(
            submit_word(&store, "   ", None).as_deref(),
            Some("Type a word first.")
        );
        assert_eq!(
            submit_word(&store, "", Some("Old")).as_deref(),
            Some("Type a word first.")
        );
        assert_eq!(store.list(), DictionaryData::default());
    }

    #[test]
    fn word_add_edit_and_duplicate() {
        let (_tmp, store) = store();
        assert_eq!(submit_word(&store, "Kubernetes", None), None);
        assert!(submit_word(&store, "kubernetes", None)
            .unwrap()
            .contains("already in your dictionary"));
        assert_eq!(
            submit_word(&store, "  kubernetes ", None).as_deref(),
            Some("“kubernetes” is already in your dictionary.")
        );
        assert_eq!(submit_word(&store, "K8s", Some("Kubernetes")), None);
        assert_eq!(store.words(), ["K8s"]);
        assert!(submit_word(&store, "x", Some("Gone"))
            .unwrap()
            .contains("removed"));
        // A word never ends up as a replacement.
        assert!(store.replacements().is_empty());
        // Unicode words round-trip.
        assert_eq!(submit_word(&store, "Zürich", None), None);
        assert_eq!(store.words(), ["K8s", "Zürich"]);
    }

    #[test]
    fn replacement_validation_names_the_missing_field() {
        let (_tmp, store) = store();
        assert_eq!(
            submit_replacement(&store, "", "the", None).as_deref(),
            Some("Type what you say first.")
        );
        assert_eq!(
            submit_replacement(&store, "teh", "", None).as_deref(),
            Some("Type what it should become.")
        );
        assert_eq!(store.list(), DictionaryData::default());
        assert_eq!(submit_replacement(&store, "teh", "the", None), None);
        assert_eq!(
            store.replacements(),
            [("teh".to_string(), "the".to_string())]
        );
        assert!(store.words().is_empty());
        assert!(submit_replacement(&store, "TEH", "the", None)
            .unwrap()
            .contains("already have a replacement"));
        assert_eq!(submit_replacement(&store, "teh", "then", Some("teh")), None);
        assert_eq!(
            store.replacements(),
            [("teh".to_string(), "then".to_string())]
        );
        assert!(submit_replacement(&store, "a", "b", Some("missing"))
            .unwrap()
            .contains("removed or already exists"));
    }

    struct Broken;

    impl DictionaryEditor for Broken {
        fn add_word(&self, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
        fn update_word(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
        fn add_replacement(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
        fn update_replacement(&self, _: &str, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
        fn remove(&self, _: &str, _: Option<&str>) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("disk exploded: /private/path"))
        }
    }

    struct Invalid;

    impl DictionaryEditor for Invalid {
        fn add_word(&self, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::Error::new(DictionaryError("word is empty".into())))
        }
        fn update_word(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::Error::new(DictionaryError("word is empty".into())))
        }
        fn add_replacement(&self, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::Error::new(DictionaryError("empty".into())))
        }
        fn update_replacement(&self, _: &str, _: &str, _: &str) -> anyhow::Result<bool> {
            Err(anyhow::Error::new(DictionaryError("empty".into())))
        }
        fn remove(&self, _: &str, _: Option<&str>) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn dictionary_errors_are_friendly_not_raw_exceptions() {
        for message in [
            submit_word(&Broken, "x", None),
            submit_word(&Broken, "x", Some("y")),
            submit_replacement(&Broken, "a", "b", None),
            submit_replacement(&Broken, "a", "b", Some("a")),
            remove_entry(&Broken, "x", None),
        ] {
            let message = message.unwrap();
            assert_eq!(message, DICTIONARY_SAVE_FAILED);
            assert!(!message.contains("private") && !message.contains("Something went wrong"));
        }
        // Store-side validation errors map to their own friendly text.
        assert_eq!(
            submit_word(&Invalid, "x", None).as_deref(),
            Some("Type a word first.")
        );
        assert_eq!(
            submit_replacement(&Invalid, "a", "b", None).as_deref(),
            Some("Fill in both “When I say” and “Type this”.")
        );
        // Removing something that is not there is not an error.
        assert_eq!(remove_entry(&Invalid, "x", None), None);
    }

    #[test]
    fn remove_entry_removes_and_can_target_one_kind() {
        let (_tmp, store) = store();
        store.add_word("Kubernetes").unwrap();
        assert_eq!(remove_entry(&store, "Kubernetes", None), None);
        assert!(store.words().is_empty());
        // Same key as a word and as a replacement: kind decides.
        store.add_word("teh").unwrap();
        store.add_replacement("teh", "the").unwrap();
        assert_eq!(
            remove_entry(&store, "teh", Some(EntryKind::Replacement)),
            None
        );
        assert_eq!(store.words(), ["teh"]);
        assert!(store.replacements().is_empty());
        store.add_replacement("teh", "the").unwrap();
        assert_eq!(remove_entry(&store, "teh", Some(EntryKind::Word)), None);
        assert!(store.words().is_empty());
        assert_eq!(store.replacements().len(), 1);
        assert_eq!(remove_entry(&store, "teh", None), None);
        assert_eq!(store.list(), DictionaryData::default());
    }

    // ---- prompts ----------------------------------------------------------------

    fn two(queue: &FakePromptQueue) -> (PromptEntry, PromptEntry) {
        let made = queue
            .add(
                &[
                    WorkPrompt::new("Migrate sessions", "body"),
                    WorkPrompt::new("Update docs", "body"),
                ],
                &["/tmp/a.md".to_string(), "/tmp/b.md".to_string()],
                "Weekly planning",
            )
            .unwrap();
        (made[0].clone(), made[1].clone())
    }

    fn id(entry: &PromptEntry) -> String {
        entry["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn review_prompt_reports_friendly_messages() {
        let queue = FakePromptQueue::new();
        let (first, other) = two(&queue);
        assert_eq!(
            review_prompt(&queue, &id(&first), ReviewAction::Approve),
            None
        );
        assert_eq!(
            review_prompt(&queue, &id(&other), ReviewAction::Reject),
            None
        );
        assert!(review_prompt(&queue, &id(&first), ReviewAction::Reject)
            .unwrap()
            .contains("changed before it could be rejected"));
        let missing = review_prompt(&queue, "nope", ReviewAction::Approve).unwrap();
        assert!(missing.contains("no longer in the queue"));
        assert!(!missing.contains("unknown prompt"));
    }

    #[test]
    fn stale_click_reports_the_conflict_and_changes_nothing() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        queue
            .set_status(&id(&entry), "dispatched", None, &Map::new())
            .unwrap();
        let message = review_prompt(&queue, &id(&entry), ReviewAction::Approve).unwrap();
        assert!(message.contains("changed before it could be approved"));
        assert_eq!(
            queue.get(&id(&entry)).unwrap().unwrap()["status"],
            "dispatched"
        );
    }

    #[test]
    fn review_prompt_survives_an_unwritable_queue() {
        let queue = FakePromptQueue::new();
        let (entry, _) = two(&queue);
        queue.set_write_failure(Some("read-only /private/path"));
        let message = review_prompt(&queue, &id(&entry), ReviewAction::Approve).unwrap();
        assert_eq!(
            message,
            "Couldn't update the prompt queue. Check ~/.localflow and try again."
        );
        assert!(!message.contains("private"));
    }

    #[test]
    fn prompt_entries_reports_an_unreadable_queue() {
        let queue = FakePromptQueue::new();
        two(&queue);
        assert_eq!(prompt_entries(&queue).unwrap().len(), 2);
        queue.set_read_failure(Some("corrupt"));
        assert_eq!(prompt_entries(&queue), Err(PROMPTS_UNREADABLE));
    }

    #[test]
    fn prompt_rows_use_repo_then_meeting_and_guard_the_buttons() {
        let entries = vec![
            entry(
                json!({"id": "a", "title": "Pending", "status": "pending", "meeting": "Standup"}),
            ),
            entry(
                json!({"id": "b", "title": "Failed", "status": "failed", "repo": "localflow", "meeting": "Planning", "error": "Orca is not running"}),
            ),
            entry(json!({"id": "c", "title": "Done", "status": "dispatched"})),
            entry(json!({"id": "d", "title": "", "status": "rejected", "meeting": "Retro"})),
        ];
        let rows = prompt_rows(&entries);
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["d", "c", "b", "a"]);
        let by = |id: &str| rows.iter().find(|r| r.id == id).unwrap().clone();
        assert_eq!(by("a").context, "Standup");
        // The stored technical error becomes the shared friendly sentence.
        assert_eq!(
            by("b").context,
            "localflow · Couldn't send — the Orca agent app isn't open"
        );
        assert_eq!(by("b").error, ORCA_NOT_OPEN);
        assert!(!by("b").context.contains("Orca is not running"));
        assert_eq!(by("c").context, "Unknown meeting");
        assert_eq!(by("d").title, "Untitled prompt");
        assert_eq!(by("b").status_label, "Failed");
        let flags: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .map(|k| (by(k).can_approve, by(k).can_reject))
            .collect();
        assert_eq!(
            flags,
            [(true, true), (true, true), (false, false), (false, false)]
        );
        assert!(prompt_rows(&[]).is_empty());
        // A missing status is pending.
        let rows = prompt_rows(&[entry(json!({"id": "z"}))]);
        assert_eq!(
            (rows[0].status.as_str(), rows[0].can_approve),
            ("pending", true)
        );
        assert_eq!(prompt_status_pill("failed"), Some(("red", "red")));
        assert_eq!(prompt_status_pill("pending"), Some(("sun", "ink")));
        assert_eq!(prompt_status_pill("nope"), None);
    }

    #[test]
    fn prompt_errors_map_only_the_known_orca_cause() {
        for stored in [
            "Orca is not running",
            "Orca app is not running — run `orca open`",
            "Orca app is not running — run `orca open` (not possible over SSH: it launches a GUI app on the Mac's own display)",
            "  orca   APP is\nnot running ",
        ] {
            assert_eq!(prompt_error_text(stored), ORCA_NOT_OPEN, "{stored}");
        }
        // Unrelated failures keep their own meaning; nothing is masked.
        for stored in [
            "orca exited 2: repo not found",
            "orca CLI not found on PATH",
            "Orca runtime not reachable (stopped) — run `orca open`",
            "cannot read prompt file /tmp/x.md: [Errno 2] No such file",
            "orca worktree create timed out after 120s",
        ] {
            assert_eq!(prompt_error_text(stored), stored);
        }
        assert_eq!(prompt_error_text(""), "");
        let rows = prompt_rows(&[
            entry(
                json!({"id": "x", "status": "failed", "repo": "r", "error": "orca exited 2: repo not found"}),
            ),
            entry(json!({"id": "y", "status": "pending", "error": "Orca is not running"})),
        ]);
        assert_eq!(rows[1].context, "r · orca exited 2: repo not found");
        assert_eq!(
            (rows[0].context.as_str(), rows[0].error.as_str()),
            ("Unknown meeting", "")
        );
    }

    fn generous(text: &str, size: f64, weight: Weight) -> f64 {
        // A deliberately generous 11pt width estimate (wider than SF Pro).
        assert_eq!((size, weight), (11.0, Weight::Regular));
        text.chars().count() as f64 * 6.8
    }

    #[test]
    fn prompt_rows_wrap_the_failure_instead_of_truncating() {
        for width in [
            HUB_MIN_WIDTH - HUB_CONTENT_X - 32,
            1280 - HUB_CONTENT_X - 32,
        ] {
            let row = prompt_rows(&[entry(json!({
                "id": "f", "status": "failed", "repo": "localflow",
                "error": "Orca is not running", "title": "Migrate"
            }))])
            .remove(0);
            assert!(row.can_approve && row.can_reject);
            let geometry = prompt_row_layout(width, &row, 60, Some(&generous));
            let lines = wrap_lines(&row.context, f64::from(geometry.text_width), &|t| {
                generous(t, 11.0, Weight::Regular)
            });
            // Every wrapped line fits the text column, and the column is tall enough.
            assert!(lines
                .iter()
                .all(|l| generous(l, 11.0, Weight::Regular) <= f64::from(geometry.text_width)));
            assert_eq!(lines.join(" "), row.context);
            assert!(geometry.context_h >= lines.len() as i32 * PROMPT_CONTEXT_LINE);
            assert!(geometry.height >= PROMPT_CONTEXT_Y + geometry.context_h);
            assert_eq!(geometry.lines, lines.len());
            // Approve + Reject + pill + text never overlap and stay inside the row.
            assert!(14 + geometry.text_width <= geometry.pill_x);
            assert!(geometry.pill_x + 60 <= geometry.action_x);
            assert!(geometry.action_x + 88 + 8 + 80 <= width - 12);
            if width < 700 {
                assert!(lines.len() >= 2 && geometry.height > PROMPT_ROW_MIN_H);
            }
        }
    }

    #[test]
    fn prompt_rows_keep_the_compact_height_for_short_context() {
        let row = prompt_rows(&[entry(
            json!({"id": "p", "status": "pending", "meeting": "Standup"}),
        )])
        .remove(0);
        let geometry = prompt_row_layout(600, &row, 60, Some(&generous));
        assert_eq!((geometry.height, geometry.lines), (PROMPT_ROW_MIN_H, 1));
        // Default estimate (no measure) and rows without actions.
        let done = prompt_rows(&[entry(json!({"id": "d", "status": "dispatched"}))]).remove(0);
        let geometry = prompt_row_layout(600, &done, 60, None);
        assert_eq!(geometry.action_x, 600 - PROMPT_EDGE);
        assert_eq!(geometry.height, PROMPT_ROW_MIN_H);
        // A tiny width keeps the 40pt minimum text column.
        assert_eq!(prompt_row_layout(100, &row, 60, None).text_width, 40);
    }

    #[test]
    fn prompt_row_layout_wraps_long_unicode_without_splitting_characters() {
        let long = "日本語のとても長いコンテキスト".repeat(8);
        let row = PromptRow {
            id: "u".into(),
            title: "T".into(),
            context: long.clone(),
            error: String::new(),
            status: "pending".into(),
            status_label: "Pending".into(),
            can_approve: true,
            can_reject: true,
        };
        let measure = |text: &str, _: f64, _: Weight| text.chars().count() as f64 * 11.0;
        let geometry = prompt_row_layout(600, &row, 60, Some(&measure));
        assert!(geometry.lines > 1);
        let lines = wrap_lines(&long, f64::from(geometry.text_width), &|t| {
            measure(t, 11.0, Weight::Regular)
        });
        assert_eq!(lines.concat(), long);
    }
}
