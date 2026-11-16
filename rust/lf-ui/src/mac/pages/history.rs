//! History page (port of `_page_history`, `_render_history`, `_history_row`,
//! `_empty_state` and the search / paging / per-entry actions of
//! `localflow/hub_mac.py`), plus the row and empty-state builders the Home page
//! reuses.
//!
//! The search text and page offset live in [`HistoryPage`], so they survive the
//! full re-layout the shell performs on resize and appearance changes;
//! [`HubPage::capture`] reads the live search field first, like Python's
//! `_preserve_page_state`. Every action mutates the store and then asks the
//! shell to rebuild; handlers hold only `Weak` references to the page state.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use lf_core::history::HistoryEntry;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAccessibility, NSAlert, NSAlertFirstButtonReturn, NSPasteboard, NSPasteboardTypeString,
    NSSearchField, NSView,
};
use objc2_foundation::NSString;

use crate::hub_model::{
    history_action_start, history_empty_state, history_meta_line, history_newer_offset,
    history_older_offset, history_page_label, history_row_actions, history_row_text,
    history_row_text_layout, history_status_chip, HISTORY_CLEAR_CONFIRM, HISTORY_PAGE_SIZE,
    HISTORY_RETRY_UNAVAILABLE, HISTORY_ROW_HEIGHT,
};
use crate::mac::page::{HubPage, HubServices, PageCtx};
use crate::mac::style::{
    color, hide_from_accessibility, rect, style_surface, ButtonKind, FlippedView, SurfaceStyle,
};
use crate::mac::widgets::{button, label, search_field, LabelStyle};
use crate::tokens::{Palette, Weight};

const PAGE: &str = "History";

/// Per-row action handlers (Copy / Original / Retry / Delete).
pub struct RowHandlers {
    pub copy: Rc<dyn Fn()>,
    pub raw: Rc<dyn Fn()>,
    pub retry: Rc<dyn Fn()>,
    pub delete: Rc<dyn Fn()>,
}

/// A native empty state: title, wrapped detail and one action button
/// (`_empty_state`), added to `parent` at `(x, y)`.
#[allow(clippy::too_many_arguments)]
pub fn empty_state(
    ctx: &PageCtx,
    parent: &NSView,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    title: &str,
    detail: &str,
    action: &str,
    on_action: impl Fn() + 'static,
) {
    let mtm = ctx.mtm;
    let p = &ctx.palette;
    let state = FlippedView::new(mtm, rect(x, y, width, height));
    let w = width - 48.0;
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
    parent.addSubview(&state);
}

/// A 1pt divider line between rows (decorative: hidden from accessibility).
fn divider(mtm: MainThreadMarker, listing: &FlippedView, y: f64, width: f64, palette: &Palette) {
    let line = FlippedView::new(mtm, rect(12.0, y, width - 24.0, 1.0));
    line.setWantsLayer(true);
    if let Some(layer) = line.layer() {
        layer.setBackgroundColor(Some(&color("line", palette).CGColor()));
    }
    hide_from_accessibility(&line);
    listing.addSubview(&line);
}

/// One entry row at `y` inside `parent` (`_history_row`). `actions` adds the
/// per-row buttons (History); Home passes `None`.
#[allow(clippy::too_many_arguments)]
pub fn history_row(
    ctx: &PageCtx,
    parent: &NSView,
    entry: &HistoryEntry,
    y: f64,
    width: f64,
    actions: Option<RowHandlers>,
    now: chrono::NaiveDateTime,
) {
    let mtm = ctx.mtm;
    let p = &ctx.palette;
    let w = width as i32;
    let chip = history_status_chip(&entry.status);
    let action_start = actions.as_ref().map(|handlers| {
        let specs = history_row_actions(&entry.kind, entry.audio_path.is_some());
        let start = history_action_start(w, &specs);
        let mut x = f64::from(start);
        for (title, button_width) in specs {
            let kind = if title == "Delete" {
                ButtonKind::DestructiveGhost
            } else {
                ButtonKind::Ghost
            };
            let btn = button(
                mtm,
                title,
                x,
                y + 10.0,
                f64::from(button_width),
                36.0,
                kind,
                p,
            );
            btn.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                "{title} entry {}",
                entry.id
            ))));
            let handler = match title {
                "Copy" => handlers.copy.clone(),
                "Original" => handlers.raw.clone(),
                "Retry" => handlers.retry.clone(),
                _ => handlers.delete.clone(),
            };
            ctx.target.on_action(&btn, move || handler());
            parent.addSubview(&btn);
            x += f64::from(button_width) + 8.0;
        }
        start
    });
    let (text_width, chip_x) = history_row_text_layout(w, action_start, chip.is_some());
    let text_width = f64::from(text_width.max(0));
    parent.addSubview(&label(
        mtm,
        &history_row_text(&entry.final_text, &entry.raw_text),
        14.0,
        y + 7.0,
        text_width,
        LabelStyle {
            size: 13.0,
            height: 18.0,
            ..LabelStyle::default()
        },
        p,
    ));
    parent.addSubview(&label(
        mtm,
        &history_meta_line(entry.app_name.as_deref(), entry.ts, &entry.kind, now),
        14.0,
        y + 30.0,
        text_width,
        LabelStyle {
            size: 11.0,
            color: "muted",
            height: 16.0,
            ..LabelStyle::default()
        },
        p,
    ));
    if let (Some((text, failed)), Some(chip_x)) = (chip, chip_x) {
        let pill = FlippedView::new(mtm, rect(f64::from(chip_x), y + 16.0, 72.0, 22.0));
        pill.setWantsLayer(true);
        if let Some(layer) = pill.layer() {
            layer.setCornerRadius(11.0);
            layer.setBackgroundColor(Some(
                &color(if failed { "red" } else { "line" }, p).CGColor(),
            ));
        }
        pill.addSubview(&label(
            mtm,
            text,
            8.0,
            2.0,
            58.0,
            LabelStyle {
                size: 9.0,
                color: if failed { "paper" } else { "muted" },
                height: 18.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            p,
        ));
        parent.addSubview(&pill);
    }
}

/// A bordered listing of `rows` with dividers; returns its height.
pub fn history_listing(
    ctx: &PageCtx,
    parent: &NSView,
    y: f64,
    rows: &[HistoryEntry],
    handlers: impl Fn(&HistoryEntry) -> Option<RowHandlers>,
) -> f64 {
    let step = f64::from(HISTORY_ROW_HEIGHT);
    let height = rows.len() as f64 * step;
    let listing = FlippedView::new(ctx.mtm, rect(0.0, y, ctx.width, height));
    style_surface(&listing, &ctx.palette, &SurfaceStyle::default());
    parent.addSubview(&listing);
    let now = chrono::Local::now().naive_local();
    for (index, entry) in rows.iter().enumerate() {
        history_row(
            ctx,
            &listing,
            entry,
            index as f64 * step,
            ctx.width,
            handlers(entry),
            now,
        );
        if index + 1 < rows.len() {
            divider(
                ctx.mtm,
                &listing,
                (index + 1) as f64 * step - 1.0,
                ctx.width,
                &ctx.palette,
            );
        }
    }
    height
}

/// Copy `text` to the system clipboard; false when the pasteboard refused it.
fn copy_to_clipboard(text: &str) -> bool {
    let board = NSPasteboard::generalPasteboard();
    board.clearContents();
    // SAFETY: immutable global pasteboard-type constant.
    let kind = unsafe { NSPasteboardTypeString };
    board.setString_forType(&NSString::from_str(text), kind)
}

/// State that outlives a rebuild.
#[derive(Default)]
struct State {
    query: RefCell<String>,
    offset: Cell<i64>,
    search_field: RefCell<Option<Retained<NSSearchField>>>,
}

impl State {
    /// Python's `capture`: the typed-but-unsubmitted text becomes the query.
    fn read_field(&self) {
        if let Some(field) = self.search_field.borrow().as_ref() {
            *self.query.borrow_mut() = field.stringValue().to_string();
        }
    }

    /// Back to a clean first page (what `show_page("History")` does).
    fn reset(&self) {
        if let Some(field) = self.search_field.borrow().as_ref() {
            field.setStringValue(&NSString::from_str(""));
        }
        self.query.borrow_mut().clear();
        self.offset.set(0);
    }
}

/// The History page.
pub struct HistoryPage {
    state: Rc<State>,
}

impl Default for HistoryPage {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryPage {
    pub fn new() -> Self {
        HistoryPage {
            state: Rc::new(State::default()),
        }
    }

    /// The current search text (the last captured / submitted query).
    pub fn query(&self) -> String {
        self.state.query.borrow().clone()
    }

    pub fn offset(&self) -> i64 {
        self.state.offset.get()
    }

    /// Set the query and go to its first page (before the next render).
    pub fn set_query(&self, query: &str) {
        *self.state.query.borrow_mut() = query.to_string();
        self.state.offset.set(0);
    }
}

/// Run `action`, surfacing an error inline like Python's `page_errors`.
fn report(services: &HubServices, result: Result<(), String>) {
    if let Err(message) = result {
        services.set_page_error(PAGE, &message);
    }
}

impl HubPage for HistoryPage {
    fn name(&self) -> &'static str {
        PAGE
    }

    fn capture(&mut self) {
        self.state.read_field();
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        let mtm = ctx.mtm;
        let p = ctx.palette;
        let width = ctx.width;
        let state = &self.state;

        // "Clear history": the page places it in the title row.
        let clear = button(
            mtm,
            "Clear history",
            width - 130.0,
            0.0,
            130.0,
            36.0,
            ButtonKind::DestructiveGhost,
            &p,
        );
        {
            let services = ctx.services.clone();
            let rebuild = ctx.rebuild.clone();
            let weak: Weak<State> = Rc::downgrade(state);
            ctx.target.on_action(&clear, move || {
                let alert = NSAlert::new(mtm);
                alert.setMessageText(&NSString::from_str(HISTORY_CLEAR_CONFIRM));
                alert.addButtonWithTitle(&NSString::from_str("Clear"));
                alert.addButtonWithTitle(&NSString::from_str("Cancel"));
                if alert.runModal() == NSAlertFirstButtonReturn {
                    report(
                        &services,
                        services
                            .history
                            .clear()
                            .map(|_| ())
                            .map_err(|e| e.to_string()),
                    );
                    if let Some(state) = weak.upgrade() {
                        state.reset();
                    }
                    rebuild();
                }
            });
        }
        ctx.body.addSubview(&clear);

        // Search row.
        let query = state.query.borrow().clone();
        let field = search_field(mtm, &query, 0.0, top, width - 112.0, "Search transcripts");
        let search_btn = button(
            mtm,
            "Search",
            width - 100.0,
            top,
            100.0,
            36.0,
            ButtonKind::Secondary,
            &p,
        );
        let submit = {
            let weak: Weak<State> = Rc::downgrade(state);
            let rebuild = ctx.rebuild.clone();
            let field = field.clone();
            Rc::new(move || {
                if let Some(state) = weak.upgrade() {
                    *state.query.borrow_mut() = field.stringValue().to_string();
                    state.offset.set(0);
                }
                rebuild();
            })
        };
        {
            let submit = submit.clone();
            ctx.target.on_action(&field, move || submit());
        }
        ctx.target.on_action(&search_btn, move || submit());
        ctx.body.addSubview(&field);
        ctx.body.addSubview(&search_btn);
        *state.search_field.borrow_mut() = Some(field);

        // Results.
        let results_top = top + 48.0;
        let offset = state.offset.get();
        let (rows, read_error) = match ctx.services.history.recent(
            HISTORY_PAGE_SIZE + 1,
            (!query.is_empty()).then_some(query.as_str()),
            offset,
        ) {
            Ok(rows) => (rows, None),
            Err(e) => (Vec::new(), Some(format!("Couldn't read history: {e}"))),
        };
        let has_older = rows.len() as i64 > HISTORY_PAGE_SIZE;
        let rows: Vec<HistoryEntry> = rows.into_iter().take(HISTORY_PAGE_SIZE as usize).collect();
        let mut listing_height = 330.0;
        if let Some(message) = &read_error {
            ctx.body.addSubview(&label(
                mtm,
                message,
                0.0,
                results_top,
                width,
                LabelStyle {
                    size: 12.0,
                    color: "red",
                    height: 22.0,
                    ..LabelStyle::default()
                },
                &p,
            ));
        }
        if rows.is_empty() {
            let (title, detail, action, id) = history_empty_state(!query.is_empty(), false);
            let services = ctx.services.clone();
            let rebuild = ctx.rebuild.clone();
            let weak: Weak<State> = Rc::downgrade(state);
            empty_state(
                ctx,
                &ctx.body,
                0.0,
                results_top + 110.0,
                width,
                220.0,
                title,
                detail,
                action,
                move || {
                    if id == "clearSearch" {
                        if let Some(state) = weak.upgrade() {
                            state.reset();
                        }
                    } else {
                        let opener = services.setup_opener.borrow().clone();
                        if let Some(open) = opener {
                            open();
                        } else {
                            services
                                .set_page_error(PAGE, "Setup is not available in this window.");
                        }
                    }
                    rebuild();
                },
            );
        } else {
            let services = ctx.services.clone();
            let rebuild = ctx.rebuild.clone();
            let weak: Weak<State> = Rc::downgrade(state);
            listing_height = history_listing(ctx, &ctx.body, results_top, &rows, move |entry| {
                let id = entry.id;
                let copy = {
                    let services = services.clone();
                    let rebuild = rebuild.clone();
                    Rc::new(move || {
                        if let Ok(Some(row)) = services.history.get(id) {
                            if !copy_to_clipboard(&row.final_text) {
                                services.set_page_error(PAGE, "Couldn't copy to the clipboard.");
                                rebuild();
                            }
                        }
                    }) as Rc<dyn Fn()>
                };
                let raw = {
                    let services = services.clone();
                    let rebuild = rebuild.clone();
                    Rc::new(move || {
                        if let Ok(Some(row)) = services.history.get(id) {
                            if !copy_to_clipboard(&row.raw_text) {
                                services.set_page_error(PAGE, "Couldn't copy to the clipboard.");
                                rebuild();
                            }
                        }
                    }) as Rc<dyn Fn()>
                };
                let retry = {
                    let services = services.clone();
                    let rebuild = rebuild.clone();
                    let weak = weak.clone();
                    Rc::new(move || {
                        // Re-transcription needs speech-to-text, which this
                        // build does not contain: say so, never fake a retry.
                        services.set_page_error(PAGE, HISTORY_RETRY_UNAVAILABLE);
                        if let Some(state) = weak.upgrade() {
                            state.reset();
                        }
                        rebuild();
                    }) as Rc<dyn Fn()>
                };
                let delete = {
                    let services = services.clone();
                    let rebuild = rebuild.clone();
                    let weak = weak.clone();
                    Rc::new(move || {
                        report(
                            &services,
                            services
                                .history
                                .delete(id)
                                .map(|_| ())
                                .map_err(|e| e.to_string()),
                        );
                        if let Some(state) = weak.upgrade() {
                            state.reset();
                        }
                        rebuild();
                    }) as Rc<dyn Fn()>
                };
                Some(RowHandlers {
                    copy,
                    raw,
                    retry,
                    delete,
                })
            });
        }

        // Pager.
        let page_y = results_top + listing_height + 16.0;
        let middle = (width / 2.0).floor();
        if offset > 0 {
            let newer = button(
                mtm,
                "Newer",
                middle - 145.0,
                page_y,
                90.0,
                36.0,
                ButtonKind::Secondary,
                &p,
            );
            let weak: Weak<State> = Rc::downgrade(state);
            let rebuild = ctx.rebuild.clone();
            ctx.target.on_action(&newer, move || {
                if let Some(state) = weak.upgrade() {
                    state.read_field();
                    state.offset.set(history_newer_offset(state.offset.get()));
                }
                rebuild();
            });
            ctx.body.addSubview(&newer);
        }
        ctx.body.addSubview(&label(
            mtm,
            &history_page_label(offset),
            middle - 45.0,
            page_y + 8.0,
            90.0,
            LabelStyle {
                size: 13.0,
                color: "muted",
                height: 18.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &p,
        ));
        if has_older {
            let older = button(
                mtm,
                "Older",
                middle + 55.0,
                page_y,
                90.0,
                36.0,
                ButtonKind::Secondary,
                &p,
            );
            let weak: Weak<State> = Rc::downgrade(state);
            let rebuild = ctx.rebuild.clone();
            ctx.target.on_action(&older, move || {
                if let Some(state) = weak.upgrade() {
                    state.read_field();
                    state.offset.set(history_older_offset(state.offset.get()));
                }
                rebuild();
            });
            ctx.body.addSubview(&older);
        }
        page_y + 36.0
    }

    fn shutdown(&mut self) {
        // Drop the reference to the (about to be released) search field.
        *self.state.search_field.borrow_mut() = None;
    }
}
