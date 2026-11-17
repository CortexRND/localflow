//! Pure geometry for the native Ask page (no AppKit); port of
//! `localflow/ask_layout.py`.
//!
//! The platform layer measures real text and passes the measuring closure in
//! (`measure(text, size, weight) -> width in points of one line`); tests pass
//! a fake one. Everything here is deterministic, so the layout invariants
//! (one compact controls row, left-aligned wrapping chips, a composer pinned
//! inside the Hub viewport) are testable on any platform.

pub use crate::text_wrap::wrap_lines;
use crate::text_wrap::TextMeasure;
use crate::tokens::*;

use crate::backends::ask::{SOURCES, TIME_FILTERS};

/// Checkbox glyph plus its gap to the title.
pub const CHECKBOX_EXTRA: f64 = 24.0;
/// Popup arrows and insets.
pub const POPUP_EXTRA: f64 = 36.0;
/// Between the controls on the single compact row.
pub const ROW_GAP: f64 = 12.0;
pub const CHIP_FONT: f64 = 12.0;
pub const CAPTION_FONT: f64 = 11.0;
pub const BODY_FONT: f64 = 13.0;
pub const ASK_BUTTON_MIN_WIDTH: f64 = 72.0;
/// A wrapped label never ends up a hair narrower than its text.
pub const SLACK: f64 = 4.0;

/// `(x, y, w, h)`.
pub type Frame = (f64, f64, f64, f64);

/// Left-aligned wrapping rows of compact chips (`flow_chips`): frames and the
/// total height. A chip is as wide as its measured text plus padding and is
/// only narrower than that when the label alone is wider than the row.
pub fn flow_chips<S: AsRef<str>>(
    labels: &[S],
    width: f64,
    measure: TextMeasure<'_>,
    size: f64,
) -> (Vec<Frame>, f64) {
    let chip_h = f64::from(ASK_CHIP_HEIGHT);
    let (pad, gap) = (f64::from(ASK_CHIP_PADDING), f64::from(ASK_CHIP_GAP));
    let mut frames = Vec::new();
    let (mut x, mut y) = (0.0, 0.0);
    for label in labels {
        let chip_w = (measure(label.as_ref(), size, Weight::Semibold) + 2.0 * pad).min(width);
        if x != 0.0 && x + chip_w > width {
            x = 0.0;
            y += chip_h + gap;
        }
        frames.push((x, y, chip_w, chip_h));
        x += chip_w + gap;
    }
    let height = if frames.is_empty() { 0.0 } else { y + chip_h };
    (frames, height)
}

/// One source toggle of the toolbar.
#[derive(Clone, Debug, PartialEq)]
pub struct Toggle {
    pub key: &'static str,
    pub x: f64,
    pub w: f64,
}

/// The one compact controls row.
#[derive(Clone, Debug, PartialEq)]
pub struct Toolbar {
    pub height: f64,
    /// `(x, w)` of the time popup.
    pub popup: (f64, f64),
    pub toggles: Vec<Toggle>,
    /// `(x, w)` of the Clear button.
    pub clear: (f64, f64),
    pub clear_title: &'static str,
    pub fits: bool,
}

/// The one compact controls row: time popup, three toggles, Clear on the
/// right. If the full "Clear conversation" label would crowd the toggles, the
/// shorter "Clear" is used so the row never wraps at the minimum Hub width.
pub fn toolbar_layout(width: f64, measure: TextMeasure<'_>) -> Toolbar {
    let font = f64::from(ASK_CONTROL_FONT);
    let popup_w = TIME_FILTERS
        .iter()
        .map(|(title, _)| measure(title, font, Weight::Regular))
        .fold(f64::NEG_INFINITY, f64::max)
        + POPUP_EXTRA;
    let mut x = popup_w + ROW_GAP;
    let mut toggles = Vec::new();
    for (key, label) in SOURCES {
        let w = measure(label, font, Weight::Regular) + CHECKBOX_EXTRA;
        toggles.push(Toggle { key, x, w });
        x += w + ROW_GAP;
    }
    let pad = f64::from(ACTION_PADDING);
    let mut clear_title = "Clear conversation";
    let mut clear_w = measure(clear_title, font, Weight::Semibold) + 2.0 * pad;
    if width - clear_w < x {
        clear_title = "Clear";
        clear_w = measure(clear_title, font, Weight::Semibold) + 2.0 * pad;
    }
    Toolbar {
        height: f64::from(ASK_TOOLBAR_HEIGHT),
        popup: (0.0, popup_w),
        toggles,
        clear: (width - clear_w, clear_w),
        clear_title,
        fits: width - clear_w >= x - ROW_GAP,
    }
}

/// One conversation item handed to [`conversation_layout`].
#[derive(Clone, Debug, PartialEq)]
pub enum ConversationItem {
    Intro(String),
    Examples(Vec<String>),
    User(String),
    Assistant { text: String, is_error: bool },
    Citations(Vec<String>),
}

/// Kind of a placed element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementKind {
    Intro,
    Chip,
    Cite,
    User,
    Assistant,
}

/// One placed element of the conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub kind: PlacementKind,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub text: String,
    /// Index inside the chip group (chips and citation chips only).
    pub tag: Option<usize>,
    /// `"examples"` / `"citations"` (chips only).
    pub group: Option<&'static str>,
    /// Assistant bubbles only.
    pub error: bool,
    /// Wrapped line count (bubbles only).
    pub lines: Option<usize>,
    /// Text width (bubbles only).
    pub text_w: Option<f64>,
}

impl Placement {
    fn plain(kind: PlacementKind, frame: Frame, text: &str) -> Self {
        Placement {
            kind,
            x: frame.0,
            y: frame.1,
            w: frame.2,
            h: frame.3,
            text: text.to_string(),
            tag: None,
            group: None,
            error: false,
            lines: None,
            text_w: None,
        }
    }
}

/// Result of [`conversation_layout`].
#[derive(Clone, Debug, PartialEq)]
pub struct ConversationLayout {
    pub placements: Vec<Placement>,
    pub content_height: f64,
    /// Top of the newest user bubble, where a freshly arrived turn starts.
    pub anchor_y: f64,
}

/// Place the conversation inside a document `width` wide.
pub fn conversation_layout(
    items: &[ConversationItem],
    width: f64,
    measure: TextMeasure<'_>,
) -> ConversationLayout {
    let turn_gap = f64::from(ASK_TURN_GAP);
    let line_h = f64::from(ASK_ANSWER_LINE);
    let bubble_pad = f64::from(ASK_BUBBLE_PADDING);
    let body = |t: &str| measure(t, BODY_FONT, Weight::Regular);
    let mut placements = Vec::new();
    let (mut y, mut anchor) = (0.0, 0.0);
    // `int(...)` truncates toward zero.
    let max_bubble = (width * ASK_BUBBLE_MAX_FRACTION).trunc().max(120.0);
    for item in items {
        match item {
            ConversationItem::Intro(text) => {
                let lines = wrap_lines(text, width, &body);
                let height = lines.len() as f64 * line_h;
                placements.push(Placement::plain(
                    PlacementKind::Intro,
                    (0.0, y, width, height),
                    text,
                ));
                y += height + turn_gap;
            }
            ConversationItem::Examples(labels) | ConversationItem::Citations(labels) => {
                let examples = matches!(item, ConversationItem::Examples(_));
                let (frames, height) = flow_chips(labels, width, measure, CHIP_FONT);
                for (index, (fx, fy, fw, fh)) in frames.into_iter().enumerate() {
                    let mut placement = Placement::plain(
                        if examples {
                            PlacementKind::Chip
                        } else {
                            PlacementKind::Cite
                        },
                        (fx, y + fy, fw, fh),
                        &labels[index],
                    );
                    placement.tag = Some(index);
                    placement.group = Some(if examples { "examples" } else { "citations" });
                    placements.push(placement);
                }
                y += height + turn_gap;
            }
            ConversationItem::User(text) | ConversationItem::Assistant { text, .. } => {
                let (is_user, error) = match item {
                    ConversationItem::Assistant { is_error, .. } => (false, *is_error),
                    _ => (true, false),
                };
                let limit = (if is_user { max_bubble } else { width }) - 2.0 * bubble_pad;
                let lines = wrap_lines(text, limit, &body);
                let widest = lines
                    .iter()
                    .map(|l| body(l))
                    .fold(f64::NEG_INFINITY, f64::max);
                let text_w = (widest + SLACK).min(limit);
                let bubble_w = text_w + 2.0 * bubble_pad;
                let bubble_h = lines.len() as f64 * line_h + 2.0 * bubble_pad;
                if is_user {
                    anchor = y;
                }
                let x = if is_user { width - bubble_w } else { 0.0 };
                let mut placement = Placement::plain(
                    if is_user {
                        PlacementKind::User
                    } else {
                        PlacementKind::Assistant
                    },
                    (x, y, bubble_w, bubble_h),
                    text,
                );
                placement.error = error;
                placement.lines = Some(lines.len());
                placement.text_w = Some(text_w);
                placements.push(placement);
                y += bubble_h + turn_gap;
            }
        }
    }
    ConversationLayout {
        placements,
        content_height: (y - turn_gap).max(0.0),
        anchor_y: anchor,
    }
}

/// Frames for the whole Ask page.
#[derive(Clone, Debug, PartialEq)]
pub struct PageLayout {
    pub toolbar_y: f64,
    pub toolbar: Toolbar,
    pub conversation: Frame,
    pub field: Frame,
    pub ask: Frame,
    pub privacy: Frame,
    pub bottom: f64,
}

/// Frames for the whole Ask page inside a Hub viewport of height `viewport`.
///
/// The composer and the privacy note sit at the bottom, the conversation takes
/// the height between the controls row and the composer, and `bottom` is
/// `<= viewport - HUB_PAGE_BOTTOM` so the outer Hub document never scrolls.
/// Only when even the minimum conversation height cannot fit does `bottom`
/// exceed that, and then the outer scroller takes over instead of
/// overlapping views.
pub fn page_layout(
    width: f64,
    viewport: f64,
    top: f64,
    measure: TextMeasure<'_>,
    privacy: &str,
) -> PageLayout {
    let toolbar = toolbar_layout(width, measure);
    let privacy_lines = wrap_lines(privacy, width, &|t: &str| {
        measure(t, CAPTION_FONT, Weight::Regular)
    });
    let privacy_h = privacy_lines.len() as f64 * f64::from(LINE_HEIGHT_CAPTION);
    let ask_w = ASK_BUTTON_MIN_WIDTH
        .max(measure("Ask", BODY_FONT, Weight::Bold) + 2.0 * f64::from(ACTION_PADDING));
    let section_gap = f64::from(ASK_SECTION_GAP);
    let composer_h = f64::from(ASK_COMPOSER_HEIGHT);
    let control_gap = f64::from(CONTROL_GAP);
    let convo_y = top + f64::from(ASK_TOOLBAR_HEIGHT) + section_gap;
    let mut bottom = viewport - f64::from(HUB_PAGE_BOTTOM);
    let mut privacy_y = bottom - privacy_h;
    let mut composer_y = privacy_y - control_gap - composer_h;
    let mut convo_h = composer_y - section_gap - convo_y;
    if convo_h < f64::from(ASK_CONVERSATION_MIN_HEIGHT) {
        convo_h = f64::from(ASK_CONVERSATION_MIN_HEIGHT);
        composer_y = convo_y + convo_h + section_gap;
        privacy_y = composer_y + composer_h + control_gap;
        bottom = privacy_y + privacy_h;
    }
    PageLayout {
        toolbar_y: top,
        toolbar,
        conversation: (0.0, convo_y, width, convo_h),
        field: (0.0, composer_y, width - ask_w - control_gap, composer_h),
        ask: (width - ask_w, composer_y, ask_w, composer_h),
        privacy: (0.0, privacy_y, width, privacy_h),
        bottom,
    }
}

/// NSRange counts UTF-16 code units, not characters.
pub fn utf16_length(text: &str) -> i64 {
    text.encode_utf16().count() as i64
}

/// `(location, length)` clamped to `text` in UTF-16 units; the caret goes to
/// the end when nothing was remembered.
pub fn clamp_selection(selection: Option<(i64, i64)>, text: &str) -> (i64, i64) {
    let size = utf16_length(text);
    let (location, length) = selection.unwrap_or((size, 0));
    let location = location.clamp(0, size.max(0)).max(0);
    (location, length.max(0).min(size - location))
}

/// Default `slack` of [`at_bottom`].
pub const AT_BOTTOM_SLACK: f64 = 4.0;

pub fn at_bottom(y: f64, content_height: f64, view_height: f64, slack: f64) -> bool {
    y >= (content_height - view_height).max(0.0) - slack
}

/// Where the conversation scroller should rest after a rebuild.
///
/// A resize or appearance change keeps the saved offset (or the bottom, if the
/// reader was already there) and never jumps. A freshly arrived or just-sent
/// turn is followed: the bottom, unless the new turn is taller than the view,
/// in which case its start so the reader begins at the top of the answer.
pub fn resolve_scroll(
    saved_y: Option<f64>,
    was_at_bottom: bool,
    follow: bool,
    content_height: f64,
    view_height: f64,
    anchor_y: f64,
) -> f64 {
    let limit = (content_height - view_height).max(0.0);
    if follow {
        return if content_height - anchor_y > view_height {
            anchor_y.max(0.0)
        } else {
            limit
        };
    }
    match saved_y {
        Some(saved) if !was_at_bottom => saved.max(0.0).min(limit),
        _ => limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::ask::{EXAMPLES, PRIVACY};
    use crate::pystr::PyStr;

    const BODY_MIN: f64 = (HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER) as f64;
    const MIN_VIEWPORT: f64 = (HUB_MIN_HEIGHT - HUB_CONTENT_TOP) as f64;
    const WIDTHS: [f64; 3] = [BODY_MIN, 760.0, 1016.0];

    /// Deliberately wider than the real system font.
    fn measure(text: &str, size: f64, weight: Weight) -> f64 {
        let per_char = if weight != Weight::Regular { 0.6 } else { 0.56 };
        (text.chars().count() as f64 * size * per_char).ceil()
    }

    fn overlaps(a: &Placement, b: &Placement) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn page_title_clears_the_header_offset() {
        const { assert!(PAGE_TITLE_HEIGHT <= HUB_HEADER_OFFSET) };
    }

    #[test]
    fn controls_fit_one_compact_row_at_every_width() {
        for width in WIDTHS {
            let toolbar = toolbar_layout(width, &measure);
            assert!(toolbar.fits, "{width}");
            let mut edges = vec![(toolbar.popup.0, toolbar.popup.0 + toolbar.popup.1)];
            edges.extend(toolbar.toggles.iter().map(|t| (t.x, t.x + t.w)));
            edges.push((toolbar.clear.0, toolbar.clear.0 + toolbar.clear.1));
            let keys: Vec<_> = toolbar.toggles.iter().map(|t| t.key).collect();
            assert_eq!(keys, ["dictation", "meeting", "notes"]);
            for pair in edges.windows(2) {
                assert!(pair[0].1 < pair[1].0); // ordered, never overlapping
            }
            assert_eq!(edges.last().unwrap().1, width); // Clear hugs the right edge
            assert!(toolbar.height <= 32.0);
        }
    }

    #[test]
    fn clear_label_shortens_rather_than_wrapping_the_row() {
        let narrow = toolbar_layout(420.0, &measure);
        assert_eq!(narrow.clear_title, "Clear");
        assert!(narrow.fits);
        assert_eq!(
            toolbar_layout(BODY_MIN, &measure).clear_title,
            "Clear conversation"
        );
    }

    #[test]
    fn wrap_lines_breaks_words_and_oversized_tokens() {
        let one = |t: &str| measure(t, 13.0, Weight::Regular);
        assert_eq!(wrap_lines("", 100.0, &one), [""]);
        assert_eq!(wrap_lines("a\nb", 100.0, &one), ["a", "b"]);
        let lines = wrap_lines("alpha beta gamma delta", 100.0, &one);
        assert!(lines.len() > 1 && lines.iter().all(|l| one(l) <= 100.0));
        let long = wrap_lines(&"x".repeat(80), 100.0, &one);
        assert!(long.len() > 1 && long.iter().all(|l| one(l) <= 100.0));
        assert_eq!(long.concat(), "x".repeat(80));
        // Unicode scalars, not bytes.
        let wide = wrap_lines(&"é日".repeat(30), 100.0, &one);
        assert_eq!(wide.concat(), "é日".repeat(30));
    }

    #[test]
    fn example_chips_are_compact_left_aligned_and_wrap() {
        for width in WIDTHS {
            let (frames, height) = flow_chips(&EXAMPLES, width, &measure, CHIP_FONT);
            assert_eq!(frames.len(), EXAMPLES.len());
            assert!(frames.iter().all(|f| f.3 == f64::from(ASK_CHIP_HEIGHT)));
            for (frame, label) in frames.iter().zip(EXAMPLES) {
                assert!(frame.0 + frame.2 <= width);
                assert_eq!(
                    frame.2,
                    (measure(label, 12.0, Weight::Semibold) + 24.0).min(width)
                );
            }
            assert_eq!(frames[0].0, 0.0);
            let mut rows: Vec<f64> = frames.iter().map(|f| f.1).collect();
            rows.dedup();
            for row in &rows {
                assert_eq!(frames.iter().find(|f| f.1 == *row).unwrap().0, 0.0);
            }
            assert_eq!(height, rows.last().unwrap() + f64::from(ASK_CHIP_HEIGHT));
            if width == BODY_MIN {
                assert!(rows.len() > 1);
                assert!(frames.iter().all(|f| f.2 < width));
            }
        }
    }

    #[test]
    fn a_label_wider_than_the_row_is_clipped_and_empty_has_no_height() {
        let (frames, _) = flow_chips(&["w".repeat(200)], 300.0, &measure, CHIP_FONT);
        assert_eq!(frames[0].2, 300.0);
        let (none, height) = flow_chips::<&str>(&[], 300.0, &measure, CHIP_FONT);
        assert!(none.is_empty());
        assert_eq!(height, 0.0);
    }

    fn conversation(width: f64, long_answer: bool, citations: usize) -> ConversationLayout {
        let answer = "A fairly long answer sentence. ".repeat(if long_answer { 40 } else { 2 });
        let items = vec![
            ConversationItem::User("What did we decide about the launch?".into()),
            ConversationItem::Assistant {
                text: answer.py_strip().into(),
                is_error: false,
            },
            ConversationItem::Citations(
                (0..citations)
                    .map(|n| format!("[{n}] Meeting · 2026-04-1{n} · Launch planning review"))
                    .collect(),
            ),
            ConversationItem::User("And who owns the checklist?".into()),
            ConversationItem::Assistant {
                text: "Couldn't reach the model.".into(),
                is_error: true,
            },
        ];
        conversation_layout(&items, width, &measure)
    }

    #[test]
    fn bubbles_align_by_role_and_nothing_overlaps() {
        for width in WIDTHS {
            for long_answer in [false, true] {
                let layout = conversation(width, long_answer, 3);
                let p = &layout.placements;
                let users: Vec<_> = p.iter().filter(|x| x.kind == PlacementKind::User).collect();
                let assistants: Vec<_> = p
                    .iter()
                    .filter(|x| x.kind == PlacementKind::Assistant)
                    .collect();
                assert!(!users.is_empty() && !assistants.is_empty());
                for bubble in &users {
                    assert_eq!(bubble.x + bubble.w, width);
                    assert!(bubble.w <= (width * ASK_BUBBLE_MAX_FRACTION).trunc() + 2.0);
                }
                for card in &assistants {
                    assert!(card.x == 0.0 && card.w <= width);
                }
                assert!(assistants.last().unwrap().error && !assistants[0].error);
                for (i, first) in p.iter().enumerate() {
                    assert!(first.x >= 0.0 && first.x + first.w <= width);
                    assert!(first.y + first.h <= layout.content_height);
                    for second in &p[i + 1..] {
                        assert!(!overlaps(first, second));
                    }
                }
                assert_eq!(layout.anchor_y, users.last().unwrap().y);
            }
        }
    }

    #[test]
    fn citation_chips_are_measured_and_wrap_left_aligned() {
        let layout = conversation(BODY_MIN, false, 6);
        let chips: Vec<_> = layout
            .placements
            .iter()
            .filter(|p| p.kind == PlacementKind::Cite)
            .collect();
        assert_eq!(chips.len(), 6);
        for chip in &chips {
            assert_eq!(
                chip.w,
                (measure(&chip.text, 12.0, Weight::Semibold) + 24.0).min(BODY_MIN)
            );
            assert!(chip.w < BODY_MIN);
            assert_eq!(chip.group, Some("citations"));
        }
        let mut rows: Vec<f64> = chips.iter().map(|c| c.y).collect();
        rows.dedup();
        assert!(rows.len() > 1);
        for row in rows {
            let min_x = chips
                .iter()
                .filter(|c| c.y == row)
                .map(|c| c.x)
                .fold(f64::MAX, f64::min);
            assert_eq!(min_x, 0.0);
        }
    }

    #[test]
    fn taller_answers_make_taller_cards() {
        assert!(
            conversation(BODY_MIN, true, 3).content_height
                > conversation(BODY_MIN, false, 3).content_height
        );
    }

    #[test]
    fn empty_conversation_and_intro_with_examples() {
        let empty = conversation_layout(&[], 600.0, &measure);
        assert_eq!((empty.content_height, empty.anchor_y), (0.0, 0.0));
        assert!(empty.placements.is_empty());
        let items = [
            ConversationItem::Intro(crate::backends::ask::INTRO.into()),
            ConversationItem::Examples(EXAMPLES.iter().map(|s| s.to_string()).collect()),
        ];
        let layout = conversation_layout(&items, BODY_MIN, &measure);
        assert_eq!(layout.placements[0].kind, PlacementKind::Intro);
        let chip = &layout.placements[1];
        assert_eq!(
            (chip.kind, chip.tag, chip.group),
            (PlacementKind::Chip, Some(0), Some("examples"))
        );
        assert!(chip.y >= layout.placements[0].y + layout.placements[0].h);
    }

    #[test]
    fn page_is_built_inside_the_viewport_with_a_pinned_composer() {
        for width in WIDTHS {
            for viewport in [MIN_VIEWPORT, (796 - HUB_CONTENT_TOP) as f64, 1200.0] {
                let page = page_layout(
                    width,
                    viewport,
                    f64::from(HUB_HEADER_OFFSET),
                    &measure,
                    PRIVACY,
                );
                assert_eq!(page.bottom, viewport - f64::from(HUB_PAGE_BOTTOM));
                let (convo_y, convo_h) = (page.conversation.1, page.conversation.3);
                let (field, ask, privacy) = (page.field, page.ask, page.privacy);
                assert!(convo_y >= f64::from(HUB_HEADER_OFFSET) + page.toolbar.height);
                assert!(convo_h >= f64::from(ASK_CONVERSATION_MIN_HEIGHT));
                assert!(convo_y + convo_h <= field.1);
                assert!(field.1 == ask.1 && field.3 == ask.3);
                assert!(field.0 + field.2 <= ask.0 && ask.0 + ask.2 == width);
                assert!(field.1 + field.3 <= privacy.1);
                assert_eq!(privacy.1 + privacy.3, page.bottom);
                assert!(privacy.3 <= 32.0);
            }
        }
    }

    #[test]
    fn page_spills_to_the_outer_scroller_only_when_it_cannot_fit() {
        let page = page_layout(
            BODY_MIN,
            200.0,
            f64::from(HUB_HEADER_OFFSET),
            &measure,
            PRIVACY,
        );
        assert_eq!(page.conversation.3, f64::from(ASK_CONVERSATION_MIN_HEIGHT));
        assert!(page.conversation.1 + page.conversation.3 <= page.field.1);
        assert!(page.bottom > 200.0 - f64::from(HUB_PAGE_BOTTOM));
    }

    #[test]
    fn page_follows_the_width_on_resize() {
        let narrow = page_layout(BODY_MIN, MIN_VIEWPORT, 48.0, &measure, PRIVACY);
        let wide = page_layout(1016.0, MIN_VIEWPORT, 48.0, &measure, PRIVACY);
        assert!(wide.conversation.2 == 1016.0 && 1016.0 > narrow.conversation.2);
        assert!(wide.field.2 > narrow.field.2);
        assert!(wide.privacy.3 <= narrow.privacy.3);
    }

    #[test]
    fn caret_ranges_count_utf16_units_so_emoji_drafts_keep_the_caret() {
        let text = "hi 😀 there!"; // 11 characters, 12 UTF-16 code units
        assert_eq!(text.chars().count(), 11);
        assert_eq!(utf16_length(text), 12);
        assert_eq!(clamp_selection(Some((12, 0)), text), (12, 0));
        assert_eq!(clamp_selection(Some((3, 2)), text), (3, 2));
        assert_eq!(clamp_selection(Some((10, 9)), text), (10, 2));
        assert_eq!(clamp_selection(None, text), (12, 0));
        assert_eq!(clamp_selection(Some((99, 1)), ""), (0, 0));
        assert_eq!(clamp_selection(Some((-5, -1)), text), (0, 0));
    }

    #[test]
    fn scroll_resolution_follows_new_turns_but_never_jumps_on_resize() {
        assert_eq!(
            resolve_scroll(None, true, false, 900.0, 300.0, 700.0),
            600.0
        );
        assert_eq!(
            resolve_scroll(Some(120.0), false, false, 900.0, 300.0, 700.0),
            120.0
        );
        assert_eq!(
            resolve_scroll(Some(500.0), false, false, 450.0, 300.0, 400.0),
            150.0
        );
        assert_eq!(
            resolve_scroll(Some(600.0), true, false, 1100.0, 300.0, 700.0),
            800.0
        );
        assert_eq!(
            resolve_scroll(Some(0.0), false, true, 900.0, 300.0, 700.0),
            600.0
        );
        assert_eq!(
            resolve_scroll(Some(0.0), false, true, 1500.0, 300.0, 700.0),
            700.0
        );
        assert_eq!(resolve_scroll(None, false, false, 100.0, 300.0, 0.0), 0.0);
        assert!(at_bottom(600.0, 900.0, 300.0, AT_BOTTOM_SLACK));
        assert!(!at_bottom(100.0, 900.0, 300.0, AT_BOTTOM_SLACK));
    }
}
