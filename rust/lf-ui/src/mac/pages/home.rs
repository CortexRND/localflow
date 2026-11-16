//! Home page (port of `_page_home` and `toggleDictation_` of
//! `localflow/hub_mac.py`): the dictation switch, the four stat cards, the
//! Today / 7-day / streak line and the four most recent dictations.
//!
//! Stats and rows are read from the injected [`lf_core::history::HistoryStore`]
//! on every render. The dictation switch is shown only when the loaded
//! `Config` has a `dictation_enabled` field (see
//! [`crate::hub_model::dictation_state`]); this branch's `lf-core` does not
//! define it yet, so the toggle is hidden rather than faked, and the page
//! layout starts at the header offset.

use std::rc::Rc;

use lf_core::history::Stats;
use objc2::DefinedClass;
use objc2_app_kit::{NSAccessibility, NSControlStateValueOff, NSControlStateValueOn, NSSwitch};
use objc2_foundation::NSString;
use serde_json::json;

use crate::hub_model::{
    dictation_state, history_empty_state, home_decoration_frame, home_stat_cards,
    home_stats_summary, stat_layout,
};
use crate::mac::page::{HubPage, PageCtx};
use crate::mac::pages::history::{empty_state, history_listing};
use crate::mac::style::{
    hide_from_accessibility, rect, rect_of, style_surface, FlippedView, HalftoneView, SurfaceStyle,
};
use crate::mac::widgets::{label, LabelStyle};
use crate::tokens::{Weight, SUN_TEXT};

const PAGE: &str = "Home";

/// The Home page (stateless: everything is re-read on every render).
#[derive(Default)]
pub struct HomePage;

impl HomePage {
    pub fn new() -> Self {
        HomePage
    }
}

fn zero_stats() -> Stats {
    Stats {
        total_words: 0,
        total_dictations: 0,
        avg_wpm: 0.0,
        words_today: 0,
        words_7d: 0,
        current_streak_days: 0,
        longest_streak_days: 0,
        top_apps: Vec::new(),
    }
}

impl HubPage for HomePage {
    fn name(&self) -> &'static str {
        PAGE
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        let mtm = ctx.mtm;
        let p = ctx.palette;
        let width = ctx.width;
        let mut cursor = top;

        // Dictation switch (only when the config can store it).
        if let Some(state) = dictation_state(&ctx.services.config()) {
            let switch = NSSwitch::initWithFrame(
                mtm.alloc::<NSSwitch>(),
                rect(width - 54.0, cursor + 4.0, 54.0, 32.0),
            );
            switch.setAccessibilityLabel(Some(&NSString::from_str("Dictation")));
            switch.setState(if state.enabled {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            {
                let services = ctx.services.clone();
                let rebuild = ctx.rebuild.clone();
                let sender = switch.clone();
                ctx.target.on_action(&switch, move || {
                    let on = sender.state() == NSControlStateValueOn;
                    match services.update_config(&json!({"dictation": {"enabled": on}})) {
                        Ok(_) => services.clear_page_error(PAGE),
                        Err(e) => services
                            .set_page_error(PAGE, &format!("Couldn't change dictation: {e}")),
                    }
                    rebuild();
                });
            }
            ctx.body.addSubview(&switch);
            ctx.body.addSubview(&label(
                mtm,
                &state.label,
                0.0,
                cursor,
                width - 68.0,
                LabelStyle {
                    size: 17.0,
                    height: 26.0,
                    weight: Weight::Semibold,
                    ..LabelStyle::default()
                },
                &p,
            ));
            ctx.body.addSubview(&label(
                mtm,
                state.detail,
                0.0,
                cursor + 30.0,
                width,
                LabelStyle {
                    size: 12.0,
                    color: "muted",
                    height: 22.0,
                    ..LabelStyle::default()
                },
                &p,
            ));
            cursor += 76.0;
        }

        let (stats, stats_error) = match ctx.services.history.stats(None) {
            Ok(stats) => (stats, None),
            Err(e) => (zero_stats(), Some(format!("Couldn't read history: {e}"))),
        };

        // The decoration lives in the header band only (it ends above the stat
        // cards) and clips its own drawing, so it never paints over a card.
        let halftone = HalftoneView::new(
            mtm,
            rect_of(frame_f64(home_decoration_frame(width as i32))),
            p,
        );
        let mut tuning = halftone.ivars().get();
        tuning.anchor = "top-right";
        tuning.step = 9;
        tuning.max_radius = 3;
        tuning.pad = 4;
        halftone.ivars().set(tuning);
        hide_from_accessibility(&halftone);
        ctx.body.addSubview(&halftone);

        let cards = home_stat_cards(&stats);
        let (columns, card_width) = stat_layout(width as i32, cards.len() as i32);
        let (columns, card_width) = (f64::from(columns), f64::from(card_width));
        let stat_rows = (cards.len() as f64 / columns).ceil();
        for (index, (title, value)) in cards.iter().enumerate() {
            let (row, column) = ((index as f64 / columns).floor(), index as f64 % columns);
            let streak = *title == "Streak";
            let card = FlippedView::new(
                mtm,
                rect(
                    column * (card_width + 12.0),
                    cursor + row * 116.0,
                    card_width,
                    104.0,
                ),
            );
            style_surface(
                &card,
                &p,
                &SurfaceStyle {
                    fill: if streak { "sun" } else { "surface" },
                    border: if streak { "ink" } else { "line" },
                    shadow: streak,
                    width: None,
                },
            );
            card.addSubview(&label(
                mtm,
                &title.to_uppercase(),
                16.0,
                16.0,
                card_width - 32.0,
                LabelStyle {
                    size: 16.0,
                    color: if streak { SUN_TEXT } else { "muted" },
                    height: 18.0,
                    weight: Weight::Semibold,
                    wrap: false,
                },
                &p,
            ));
            card.addSubview(&label(
                mtm,
                value,
                16.0,
                42.0,
                card_width - 32.0,
                LabelStyle {
                    size: 28.0,
                    color: if streak { SUN_TEXT } else { "ink" },
                    height: 34.0,
                    weight: Weight::Bold,
                    wrap: false,
                },
                &p,
            ));
            ctx.body.addSubview(&card);
        }
        cursor += stat_rows * 116.0 + 4.0;
        ctx.body.addSubview(&label(
            mtm,
            &stats_error.unwrap_or_else(|| home_stats_summary(&stats)),
            0.0,
            cursor,
            width,
            LabelStyle {
                size: 12.0,
                color: "muted",
                height: 20.0,
                ..LabelStyle::default()
            },
            &p,
        ));
        cursor += 36.0;

        ctx.body.addSubview(&label(
            mtm,
            "Recent dictations",
            0.0,
            cursor,
            width,
            LabelStyle {
                size: 15.0,
                height: 24.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &p,
        ));
        cursor += 36.0;

        let rows = ctx.services.history.recent(4, None, 0).unwrap_or_default();
        if rows.is_empty() {
            let (title, detail, action, _) = history_empty_state(false, true);
            let services = Rc::clone(&ctx.services);
            let rebuild = ctx.rebuild.clone();
            empty_state(
                ctx,
                &ctx.body,
                0.0,
                cursor,
                width,
                220.0,
                title,
                detail,
                action,
                move || {
                    // The Python opens the setup window in a separate process; this
                    // build has no setup window, so say so instead of doing nothing.
                    services
                        .set_page_error(PAGE, "The setup window isn't available in this build.");
                    rebuild();
                },
            );
            return cursor + 220.0;
        }
        cursor + history_listing(ctx, &ctx.body, cursor, &rows, |_| None)
    }
}

fn frame_f64(frame: (i32, i32, i32, i32)) -> (f64, f64, f64, f64) {
    (
        f64::from(frame.0),
        f64::from(frame.1),
        f64::from(frame.2),
        f64::from(frame.3),
    )
}
