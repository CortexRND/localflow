//! Headless layout checks for `setting_row` inside `group_rows`, laid out in
//! the same scrolling column the pages use.

use gpui::{size, Bounds, Context, Render, TestAppContext};

use super::*;
use crate::pages::column;

/// One real row per case: (label, description, control width).
struct Case {
    name: &'static str,
    label: &'static str,
    description: String,
    control_w: f32,
    /// Render the real four-option Style `Segmented` instead of a fixed box.
    segmented: bool,
    /// Fits on one line at the unclamped slot width but wraps at 440 px, so
    /// only a row measured at the clamped width gets its height right.
    wraps_only_at_measure: bool,
}

/// Longer than 440 px of text, shorter than the ~570 px a 120 px control
/// leaves in a 1280 px window (the test text system's glyphs are wider than a
/// real font's, hence the short string).
const NARROW_WRAP: &str =
    "Automatic picks the best engine for this computer when LocalFlow starts.";

fn cases() -> Vec<Case> {
    let work_style = format!(
        "For example: “Please share the updated timeline.” {}",
        lf_providers::cleanup::style_line("formal").unwrap_or("")
    );
    vec![
        Case {
            name: "setup",
            label: "Setup",
            description: "Pick and download a speech model, test your microphone, choose cleanup and a shortcut. Nothing changes until you apply.".into(),
            control_w: 120.,
            segmented: false,
            wraps_only_at_measure: false,
        },
        Case {
            name: "launch at login",
            label: "Launch at login",
            description: crate::pages::settings::launch_at_login_description("linux").into(),
            control_w: 120.,
            segmented: false,
            wraps_only_at_measure: false,
        },
        Case {
            name: "engine",
            label: "Engine",
            description: "Automatic picks the best engine for this computer when LocalFlow starts.".into(),
            control_w: 120.,
            segmented: false,
            wraps_only_at_measure: false,
        },
        Case {
            name: "work style",
            label: "Work",
            description: work_style,
            control_w: 0.,
            segmented: true,
            wraps_only_at_measure: false,
        },
        Case {
            name: "wraps only at 440",
            label: "Engine",
            description: NARROW_WRAP.into(),
            control_w: 120.,
            segmented: false,
            wraps_only_at_measure: true,
        },
    ]
}

struct Harness {
    label: &'static str,
    description: String,
    control_w: f32,
    segmented: bool,
    /// Widths the two reference descriptions are laid out at: the unclamped
    /// slot and the 440 px measure.
    reference_w: Option<(Pixels, Pixels)>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::light();
        let row = div()
            .debug_selector(|| "row".into())
            .child(setting_row(
                &t,
                self.label,
                self.description.clone(),
                if self.segmented {
                    let titles: Vec<String> = lf_ui::hub_model::STYLE_OPTIONS
                        .iter()
                        .map(|o| lf_ui::hub_model::style_option_title(o))
                        .collect();
                    div()
                        .debug_selector(|| "control".into())
                        .child(Segmented::new("tone", titles, Some(0)))
                        .into_any_element()
                } else {
                    div()
                        .debug_selector(|| "control".into())
                        .w(px(self.control_w))
                        .h(px(28.))
                        .into_any_element()
                },
                None,
            ))
            .into_any_element();
        let references = self.reference_w.map(|(open, clamped)| {
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .w(open)
                        .debug_selector(|| "reference-open".into())
                        .child(supporting(&t, self.description.clone())),
                )
                .child(
                    div()
                        .w(clamped)
                        .debug_selector(|| "reference-clamped".into())
                        .child(supporting(&t, self.description.clone())),
                )
        });
        let content = div()
            .flex()
            .flex_col()
            .child(group_rows(&t, [row], row_inset()))
            .children(references);
        // Sidebar spacer, then the page column exactly as `pages::column`
        // builds it (padding, `size::COLUMN` cap, scrolling).
        div()
            .flex()
            .size_full()
            .child(div().flex_none().w(px(size::SIDEBAR)))
            .child(column("column", content))
    }
}

fn check(case: &Case, window_w: f32, cx: &mut TestAppContext) {
    // `Segmented` reads the theme from GPUI globals.
    cx.update(|cx| cx.set_global(Theme::light()));
    let (view, cx) = cx.add_window_view(|_, _| Harness {
        label: case.label,
        description: case.description.clone(),
        control_w: case.control_w,
        segmented: case.segmented,
        reference_w: None,
    });
    cx.simulate_resize(size(px(window_w), px(850.)));
    cx.run_until_parked();

    let row = cx.debug_bounds("row").expect("row laid out");
    let control = cx.debug_bounds("control").expect("control laid out");
    let text = cx.debug_bounds("setting-text").expect("text laid out");
    let desc = cx
        .debug_bounds("setting-desc")
        .expect("description laid out");
    let ctx = format!("{} at {window_w}px", case.name);

    // Horizontal: the text column and description stay left of the control,
    // and the control stays inside the row's right inset (not clipped).
    assert!(
        row.left() <= text.left(),
        "{ctx}: text starts at {:?}, left of the row at {:?}",
        text.left(),
        row.left()
    );
    assert!(
        control.right() <= row.right() - row_inset(),
        "{ctx}: control ends at {:?}, row's right inset starts at {:?}",
        control.right(),
        row.right() - row_inset()
    );
    assert!(
        text.right() + space::s8() <= control.left(),
        "{ctx}: text column ends at {:?}, control starts at {:?}",
        text.right(),
        control.left()
    );
    assert!(
        desc.right() + space::s8() <= control.left(),
        "{ctx}: description ends at {:?}, control starts at {:?}",
        desc.right(),
        control.left()
    );

    // Vertical: lay the description out alone at min(available, 440 px) and
    // require the row to hold it plus label, gap and both paddings.
    let available = row.size.width - row_inset() * 2. - control.size.width - space::s8();
    let measure = available.min(px(440.));
    view.update(cx, |h, cx| {
        h.reference_w = Some((available, measure));
        cx.notify();
    });
    cx.run_until_parked();
    let open = cx.debug_bounds("reference-open").expect("open reference");
    let wrapped = cx
        .debug_bounds("reference-clamped")
        .expect("clamped reference")
        .size
        .height;
    let line = text::supporting().1;
    assert!(wrapped >= line * 2., "{ctx}: description should wrap");
    // At 960 px the slot (~470 px) is too close to the measure for a string
    // that fits one but not the other; the property is checked at 1280 px.
    if case.wraps_only_at_measure && window_w >= 1280. {
        assert!(
            open.size.height < line * 1.5,
            "{ctx}: test string must fit on one line at {available:?}"
        );
    }
    assert!(
        desc.size.width <= measure,
        "{ctx}: description is {:?} wide, limit {measure:?}",
        desc.size.width
    );
    assert!(
        desc.size.height >= wrapped,
        "{ctx}: description is {:?} tall, wrapped text needs {wrapped:?}",
        desc.size.height
    );
    let needed = text::body().1 + space::s1() + wrapped + space::s5() * 2.;
    assert!(
        row.size.height >= needed,
        "{ctx}: row is {:?} tall, needs {needed:?}",
        row.size.height
    );
    assert!(
        row.bottom() - desc.bottom() >= space::s5(),
        "{ctx}: description ends {:?} above the row's bottom edge, needs {:?}",
        row.bottom() - desc.bottom(),
        space::s5()
    );
}

#[gpui::test]
fn setting_rows_fit_at_1280(cx: &mut TestAppContext) {
    for case in cases() {
        check(&case, 1280., cx);
    }
}

#[gpui::test]
fn setting_rows_fit_at_960(cx: &mut TestAppContext) {
    for case in cases() {
        check(&case, 960., cx);
    }
}

/// The Settings tab strip, as the page builds it, in the same sidebar +
/// column frame, with the longest label selected.
struct TabsHarness {
    density: TabDensity,
}

impl Render for TabsHarness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        use crate::controllers::settings::Tab;
        let t = Theme::light();
        let items: Vec<(Tab, &'static str)> = Tab::ALL.iter().map(|t| (*t, t.label())).collect();
        let strip = div().debug_selector(|| "strip".into()).child(tabs(
            &t,
            "settings-tab",
            &items,
            Tab::Dictation,
            self.density,
            Rc::new(|_, _, _| {}),
        ));
        div()
            .flex()
            .size_full()
            .child(div().flex_none().w(px(size::SIDEBAR)))
            .child(column("column", div().child(strip)))
    }
}

/// Settings' seven tabs at the 960 px minimum: the 656 px budget, the compact
/// capsule (6 px sides, same height), wrap-not-overflow, and one line. The
/// test font is a 0.6 em monospace (7.8 px a glyph at 13 px), wider than the
/// real UI font: the 69 label glyphs plus gaps take 730 px at `Regular`, which
/// must wrap Advanced (so the check can fail), and 646 px at Settings' own
/// density, which must not.
#[gpui::test]
fn settings_tabs_are_compact_and_stay_inside_at_960(cx: &mut TestAppContext) {
    let settings = crate::pages::settings::TAB_DENSITY;
    let tabs = [TabDensity::Regular, settings].map(|density| {
        let (_view, cx) = cx.add_window_view(|_, _| TabsHarness { density });
        cx.simulate_resize(size(px(960.), px(640.)));
        cx.run_until_parked();
        let strip = cx.debug_bounds("strip").expect("strip laid out");
        assert_eq!(strip.size.width, px(656.), "{density:?}: strip budget");
        [
            "settings-tab-0",
            "settings-tab-1",
            "settings-tab-2",
            "settings-tab-3",
            "settings-tab-4",
            "settings-tab-5",
            "settings-tab-6",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, selector)| {
            let tab = cx.debug_bounds(selector).expect("tab laid out");
            assert_eq!(tab.size.height, size::control_sm(), "{density:?}: tab {i}");
            assert!(
                strip.left() <= tab.left() && tab.right() <= strip.right(),
                "{density:?}: tab {i} at {tab:?} overflows the strip {strip:?}"
            );
            tab
        })
        .collect::<Vec<_>>()
    });
    let [regular, compact] = &tabs;
    for (i, (r, c)) in regular.iter().zip(compact).enumerate() {
        assert_eq!(
            r.size.width - c.size.width,
            (space::s5() - space::s3()) * 2.,
            "tab {i} should lose 6 px a side"
        );
    }
    // Negative control: the same harness at `Regular` pushes Advanced to a
    // second line, so a one-line pass below is not vacuous.
    assert!(
        regular[6].top() > regular[0].top(),
        "Regular: Advanced at {:?} should wrap below General at {:?}",
        regular[6],
        regular[0]
    );
    for (i, tab) in compact.iter().enumerate() {
        assert_eq!(
            tab.top(),
            compact[0].top(),
            "{settings:?}: tab {i} at {tab:?} left the first line"
        );
    }
}

/// Home's real status row (`pages::home::status_row`) in a card, in the page
/// column.
struct HomeStatus {
    tone: Tone,
    line: String,
}

impl Render for HomeStatus {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::light();
        let row = crate::pages::home::status_row(&t, self.tone, self.line.clone())
            .debug_selector(|| "status-row".into())
            .into_any_element();
        let group = group_rows(&t, [row], row_inset());
        let group = group.debug_selector(|| "status-group".into());
        let content = div().flex().flex_col().child(group);
        div()
            .flex()
            .size_full()
            .child(div().flex_none().w(px(size::SIDEBAR)))
            .child(column("column", content))
    }
}

/// Lays the line out at `window_w` and checks it stays whole inside the row;
/// returns the row and text bounds.
fn check_status(
    tone: Tone,
    line: &str,
    window_w: f32,
    cx: &mut TestAppContext,
) -> (Bounds<Pixels>, Bounds<Pixels>) {
    let line = line.to_string();
    let (_, cx) = cx.add_window_view(|_, _| HomeStatus { tone, line });
    cx.simulate_resize(size(px(window_w), px(850.)));
    cx.run_until_parked();
    let group = cx.debug_bounds("status-group").expect("group laid out");
    let row = cx.debug_bounds("status-row").expect("row laid out");
    let dot = cx.debug_bounds("status-dot").expect("dot laid out");
    let words = cx.debug_bounds("status-text").expect("text laid out");
    let ctx = format!("status row at {window_w}px");
    let lh = text::body().1;

    // The text ends inside the row's right inset, so the group's
    // `overflow_hidden` has nothing to clip.
    assert!(
        words.right() <= row.right() - row_inset(),
        "{ctx}: text ends at {:?}, row's right inset starts at {:?}",
        words.right(),
        row.right() - row_inset()
    );
    assert!(
        row.right() <= group.right(),
        "{ctx}: row ends at {:?}, card at {:?}",
        row.right(),
        group.right()
    );
    // Vertically too: the row grows to hold every line plus its padding.
    let (above, below) = (words.top() - row.top(), row.bottom() - words.bottom());
    assert!(
        above >= space::s3() && below >= space::s3(),
        "{ctx}: text {words:?} doesn't sit inside the row's padding {row:?}"
    );
    assert!(
        row.bottom() <= group.bottom(),
        "{ctx}: row ends at {:?}, card at {:?}",
        row.bottom(),
        group.bottom()
    );
    // The dot is centred on the first line.
    let dot_mid = dot.top() + dot.size.height * 0.5;
    let first_mid = words.top() + lh * 0.5;
    assert!(
        dot_mid >= first_mid - px(1.) && dot_mid <= first_mid + px(1.),
        "{ctx}: dot centre {dot_mid:?}, first line centre {first_mid:?}"
    );
    (row, words)
}

fn wayland_line() -> (Tone, String) {
    use crate::controllers::runtime_state::{shortcut_line, Session, State};
    let (signal, line) = shortcut_line(Session::Wayland, false, &State::Unknown);
    (crate::app::signal_tone(signal), line)
}

#[gpui::test]
fn home_wayland_shortcut_wraps_at_1280_and_960(cx: &mut TestAppContext) {
    let (tone, line) = wayland_line();
    for window_w in [1280., 960.] {
        let (_, words) = check_status(tone, &line, window_w, cx);
        assert!(
            words.size.height >= text::body().1 * 2.,
            "{window_w}px: text is {:?} tall, should wrap",
            words.size.height
        );
    }
}

#[gpui::test]
fn home_short_status_row_is_unchanged(cx: &mut TestAppContext) {
    let (row, words) = check_status(Tone::Success, "Microphone: ready", 1280., cx);
    let lh = text::body().1;
    assert!(words.size.height < lh * 1.5, "short line wrapped");
    assert_eq!(row.size.height, px(36.), "short row should stay 36 px");
    // Still vertically centred in the row, as before.
    let mid = row.top() + row.size.height * 0.5;
    let text_mid = words.top() + words.size.height * 0.5;
    assert!(text_mid >= mid - px(1.) && text_mid <= mid + px(1.));
}
