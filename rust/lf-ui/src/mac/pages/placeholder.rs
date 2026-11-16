//! Explicit placeholder for the pages that arrive in Step 2b (Ask, Hot Phrases,
//! Meetings, Models, Settings).
//!
//! It is deliberately not a facsimile of the real page: the shell still draws
//! the shared page title, and the page body is one card that says plainly that
//! the page is not available in this Rust Hub yet.

use objc2::rc::Retained;

use crate::mac::page::{HubPage, PageCtx};
use crate::mac::style::{rect, style_surface, FlippedView, SurfaceStyle};
use crate::mac::widgets::{label, LabelStyle};
use crate::text_wrap::wrap_text;
use crate::tokens::{Weight, CARD_PADDING};

/// Heading of the placeholder card.
pub fn placeholder_heading(page: &str) -> String {
    format!("{page} isn't available in the Rust Hub yet")
}

/// Detail line of the placeholder card.
pub const PLACEHOLDER_DETAIL: &str =
    "This page arrives in a later step of the Rust port (Step 2b). \
Until then, use the Python LocalFlow app for it.";

pub struct PlaceholderPage {
    name: &'static str,
}

impl PlaceholderPage {
    pub fn new(name: &'static str) -> Self {
        PlaceholderPage { name }
    }
}

impl HubPage for PlaceholderPage {
    fn name(&self) -> &'static str {
        self.name
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        let pad = f64::from(CARD_PADDING);
        let inner = (ctx.width - 2.0 * pad).max(40.0);
        let lines = wrap_text(
            PLACEHOLDER_DETAIL,
            inner,
            &*ctx.measure,
            13.0,
            Weight::Regular,
        )
        .len();
        let detail_height = 18.0 * lines as f64;
        let height = pad + 22.0 + 8.0 + detail_height + pad;
        let card: Retained<FlippedView> =
            FlippedView::new(ctx.mtm, rect(0.0, top, ctx.width, height));
        style_surface(&card, &ctx.palette, &SurfaceStyle::default());
        card.addSubview(&label(
            ctx.mtm,
            &placeholder_heading(self.name),
            pad,
            pad,
            inner,
            LabelStyle {
                size: 15.0,
                height: 22.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
        card.addSubview(&label(
            ctx.mtm,
            PLACEHOLDER_DETAIL,
            pad,
            pad + 22.0 + 8.0,
            inner,
            LabelStyle {
                size: 13.0,
                color: "muted",
                height: detail_height,
                wrap: true,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
        ctx.body.addSubview(&card);
        top + height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_names_the_page() {
        assert_eq!(
            placeholder_heading("Models"),
            "Models isn't available in the Rust Hub yet"
        );
        assert!(PLACEHOLDER_DETAIL.contains("Step 2b"));
    }
}
