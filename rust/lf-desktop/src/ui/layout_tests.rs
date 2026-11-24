//! Headless layout checks for `setting_row` inside `group_rows`, laid out in
//! the same scrolling column the pages use.

use gpui::{size, Context, Render, TestAppContext};

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
