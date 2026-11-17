//! Prompts page (port of `_page_prompts`, `_prompt_row`, `_review_prompt`,
//! `reloadPrompts_`, `openMeetings_` and the "See Hot Phrases" teaser of
//! `localflow/hub_mac.py`).
//!
//! Three sections: the prompt shortcuts read from the live config, the meeting
//! work prompts awaiting approval (read from the injected [`PromptQueue`]) and
//! a teaser linking to Hot Phrases. Approve / Reject go through the guarded
//! compare-and-set helpers ([`review_prompt`]); a refusal is shown inline at
//! the top of the page (Prompts never uses the shell callout). Dispatching an
//! approved prompt is CLI-only in Python and is not offered here either.

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAccessibility, NSView};
use objc2_foundation::{NSPoint, NSString};

use crate::hub_model::{
    prompt_entries, prompt_row_layout, prompt_rows, prompt_status_pill, review_prompt,
    shortcut_rows, PromptRow, ReviewAction, PROMPTS_EMPTY,
};
use crate::mac::page::{HubPage, PageCtx};
use crate::mac::style::{
    color, hide_from_accessibility, make_pill, rect, style_surface, ButtonKind, FlippedView,
    SurfaceStyle,
};
use crate::mac::widgets::{button, label, LabelStyle};
use crate::tokens::{Palette, Weight, CARD_PADDING};

/// The Prompts page. Stateless between renders: the queue is re-read on every
/// render, so Reload is just a rebuild.
#[derive(Default)]
pub struct PromptsPage;

impl PromptsPage {
    pub fn new() -> Self {
        PromptsPage
    }
}

fn card_label(
    mtm: MainThreadMarker,
    card: &FlippedView,
    text: &str,
    y: f64,
    style: LabelStyle,
    palette: &Palette,
) {
    let width = card.frame().size.width - 2.0 * f64::from(CARD_PADDING);
    card.addSubview(&label(
        mtm,
        text,
        f64::from(CARD_PADDING),
        y,
        width,
        style,
        palette,
    ));
}

/// `_empty_state`: a title, wrapped detail and one action button.
#[allow(clippy::too_many_arguments)]
fn empty_state(
    ctx: &PageCtx,
    y: f64,
    height: f64,
    title: &str,
    detail: &str,
    action: &str,
    on_action: impl Fn() + 'static,
) {
    let mtm = ctx.mtm;
    let p = &ctx.palette;
    let state = FlippedView::new(mtm, rect(0.0, y, ctx.width, height));
    let w = ctx.width - 48.0;
    state.addSubview(&label(
        mtm,
        title,
        24.0,
        58.0,
        w,
        LabelStyle {
            size: 15.0,
            height: 28.0,
            weight: Weight::Semibold,
            ..LabelStyle::default()
        },
        p,
    ));
    state.addSubview(&label(
        mtm,
        detail,
        24.0,
        92.0,
        w,
        LabelStyle {
            size: 13.0,
            color: "muted",
            height: 44.0,
            wrap: true,
            ..LabelStyle::default()
        },
        p,
    ));
    let btn = button(
        mtm,
        action,
        24.0,
        140.0,
        138.0,
        36.0,
        ButtonKind::Secondary,
        p,
    );
    ctx.target.on_action(&btn, on_action);
    state.addSubview(&btn);
    ctx.body.addSubview(&state);
}

impl PromptsPage {
    /// One queue row at `y` inside `listing`; returns its height.
    fn row(&self, ctx: &PageCtx, listing: &FlippedView, row: &PromptRow, y: f64) -> f64 {
        let mtm = ctx.mtm;
        let p = &ctx.palette;
        let (tint, text_token) = prompt_status_pill(&row.status).unwrap_or(("line", "muted"));
        let pill = make_pill(mtm, &row.status_label, tint, text_token, p);
        let pill_width = pill.frame().size.width;
        let geometry = prompt_row_layout(
            ctx.width as i32,
            row,
            pill_width.ceil() as i32,
            Some(&*ctx.measure),
        );

        let mut x = f64::from(geometry.action_x);
        let mut actions: Vec<(&str, f64, ButtonKind, ReviewAction)> = Vec::new();
        if row.can_approve {
            actions.push(("Approve", 88.0, ButtonKind::Primary, ReviewAction::Approve));
        }
        if row.can_reject {
            actions.push((
                "Reject",
                80.0,
                ButtonKind::DestructiveGhost,
                ReviewAction::Reject,
            ));
        }
        for (title, width, kind, action) in actions {
            let btn = button(mtm, title, x, y + 16.0, width, 36.0, kind, p);
            btn.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                "{title} prompt: {}",
                row.title
            ))));
            let services = ctx.services.clone();
            let navigate = ctx.navigate.clone();
            let id = row.id.clone();
            ctx.target.on_action(&btn, move || {
                // The same guarded compare-and-set the server API uses.
                match review_prompt(&*services.prompt_queue, &id, action) {
                    Some(error) => services.set_page_error("Prompts", &error),
                    None => services.clear_page_error("Prompts"),
                }
                navigate("Prompts");
            });
            listing.addSubview(&btn);
            x += width + 8.0;
        }
        pill.setFrameOrigin(NSPoint::new(f64::from(geometry.pill_x), y + 22.0));
        listing.addSubview(&pill);
        let text_width = f64::from(geometry.text_width);
        listing.addSubview(&label(
            mtm,
            &row.title,
            14.0,
            y + 12.0,
            text_width,
            LabelStyle {
                size: 13.0,
                height: 20.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            p,
        ));
        listing.addSubview(&label(
            mtm,
            &row.context,
            14.0,
            y + 36.0,
            text_width,
            LabelStyle {
                size: 11.0,
                color: if row.error.is_empty() { "muted" } else { "red" },
                height: f64::from(geometry.context_h),
                wrap: true,
                ..LabelStyle::default()
            },
            p,
        ));
        f64::from(geometry.height)
    }
}

impl HubPage for PromptsPage {
    fn name(&self) -> &'static str {
        "Prompts"
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        let mtm = ctx.mtm;
        let p = ctx.palette;
        let width = ctx.width;
        let inner = width - 2.0 * f64::from(CARD_PADDING);
        let mut cursor = top;

        // Inline error from the last approve/reject (never the shell callout).
        if let Some(error) = ctx.services.page_error("Prompts") {
            ctx.body.addSubview(&label(
                mtm,
                &error,
                0.0,
                cursor,
                width,
                LabelStyle {
                    size: 12.0,
                    color: "red",
                    height: 22.0,
                    ..LabelStyle::default()
                },
                &p,
            ));
            cursor += 30.0;
        }

        // Section 1: prompt shortcuts, read from the current config.
        let shortcuts = shortcut_rows(&ctx.services.config());
        let card_height = 76.0 + shortcuts.len() as f64 * 70.0;
        let card = FlippedView::new(mtm, rect(0.0, cursor, width, card_height));
        style_surface(&card, &p, &SurfaceStyle::default());
        ctx.body.addSubview(&card);
        card_label(
            mtm,
            &card,
            "Prompt shortcuts",
            f64::from(CARD_PADDING),
            LabelStyle {
                size: 15.0,
                weight: Weight::Semibold,
                height: 22.0,
                ..LabelStyle::default()
            },
            &p,
        );
        card_label(
            mtm,
            &card,
            "Select rough text in any app, then press a shortcut.",
            f64::from(CARD_PADDING) + 24.0,
            LabelStyle {
                size: 12.0,
                color: "muted",
                height: 18.0,
                ..LabelStyle::default()
            },
            &p,
        );
        for (index, row) in shortcuts.iter().enumerate() {
            let y = 64.0 + index as f64 * 70.0;
            // The hotkey pill shares the name row; the explanation gets the
            // full card width below it and wraps instead of truncating.
            let pill = make_pill(mtm, &row.hotkey, "line", "ink", &p);
            let pill_width = pill.frame().size.width;
            pill.setFrameOrigin(NSPoint::new(
                width - f64::from(CARD_PADDING) - pill_width,
                y,
            ));
            card.addSubview(&pill);
            card.addSubview(&label(
                mtm,
                row.name,
                f64::from(CARD_PADDING),
                y + 2.0,
                inner - pill_width - 12.0,
                LabelStyle {
                    size: 13.0,
                    height: 20.0,
                    weight: Weight::Semibold,
                    ..LabelStyle::default()
                },
                &p,
            ));
            card.addSubview(&label(
                mtm,
                row.detail,
                f64::from(CARD_PADDING),
                y + 28.0,
                inner,
                LabelStyle {
                    size: 12.0,
                    color: "muted",
                    height: 34.0,
                    wrap: true,
                    ..LabelStyle::default()
                },
                &p,
            ));
        }
        cursor += card_height + 12.0;

        // Section 2: meeting work prompts awaiting approval.
        ctx.body.addSubview(&label(
            mtm,
            "Meeting work prompts",
            0.0,
            cursor,
            width,
            LabelStyle {
                size: 15.0,
                height: 22.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &p,
        ));
        cursor += 22.0 + 8.0;
        match prompt_entries(&*ctx.services.prompt_queue) {
            Err(unreadable) => {
                let rebuild_services = ctx.services.clone();
                let navigate = ctx.navigate.clone();
                empty_state(
                    ctx,
                    cursor,
                    190.0,
                    "Can't read the queue",
                    unreadable,
                    "Try again",
                    move || {
                        // reloadPrompts_: drop a stale error and re-read.
                        rebuild_services.clear_page_error("Prompts");
                        navigate("Prompts");
                    },
                );
                cursor += 202.0;
            }
            Ok(entries) => {
                let rows = prompt_rows(&entries);
                if rows.is_empty() {
                    let navigate = ctx.navigate.clone();
                    empty_state(
                        ctx,
                        cursor,
                        190.0,
                        "No prompts to review yet",
                        PROMPTS_EMPTY,
                        "Open Meetings",
                        move || navigate("Meetings"),
                    );
                    cursor += 202.0;
                } else {
                    let listing =
                        FlippedView::new(mtm, rect(0.0, cursor, width, rows.len() as f64 * 68.0));
                    style_surface(&listing, &p, &SurfaceStyle::default());
                    ctx.body.addSubview(&listing);
                    // Rows grow with their wrapped context line (a failure
                    // sentence is never cut off), so each row's top is the sum
                    // of those above.
                    let mut y = 0.0;
                    for (index, row) in rows.iter().enumerate() {
                        y += self.row(ctx, &listing, row, y);
                        if index + 1 < rows.len() {
                            listing.addSubview(&divider(mtm, y - 1.0, width, &p));
                        }
                    }
                    listing.setFrame(rect(0.0, cursor, width, y));
                    cursor += y + 12.0;
                }
            }
        }

        // Section 3: teaser linking to the Hot Phrases page.
        let teaser = FlippedView::new(mtm, rect(0.0, cursor, width, 64.0));
        style_surface(&teaser, &p, &SurfaceStyle::default());
        ctx.body.addSubview(&teaser);
        // The label stops before the button instead of running behind it.
        teaser.addSubview(&label(
            mtm,
            "Say a word, get a saved prompt",
            f64::from(CARD_PADDING),
            18.0,
            width - 3.0 * f64::from(CARD_PADDING) - 160.0,
            LabelStyle {
                size: 13.0,
                height: 28.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &p,
        ));
        let see = button(
            mtm,
            "See Hot Phrases",
            width - 160.0 - f64::from(CARD_PADDING),
            14.0,
            160.0,
            36.0,
            ButtonKind::Secondary,
            &p,
        );
        let navigate = ctx.navigate.clone();
        ctx.target.on_action(&see, move || navigate("Hot Phrases"));
        teaser.addSubview(&see);
        cursor + 64.0
    }
}

/// A 1pt `line`-coloured row divider (decorative: hidden from accessibility).
fn divider(mtm: MainThreadMarker, y: f64, width: f64, palette: &Palette) -> Retained<NSView> {
    let view = NSView::initWithFrame(mtm.alloc::<NSView>(), rect(12.0, y, width - 24.0, 1.0));
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setBackgroundColor(Some(&color("line", palette).CGColor()));
    }
    hide_from_accessibility(&view);
    view
}
