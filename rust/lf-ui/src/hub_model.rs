//! Pure Home, History and Hub shell presentation helpers.

use crate::text_wrap::TextMeasure;
use crate::tokens::{Weight, HUB_HEADER_OFFSET};
use chrono::{Datelike, Local, NaiveDateTime, TimeZone, Timelike};
use lf_core::config::Config;

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
