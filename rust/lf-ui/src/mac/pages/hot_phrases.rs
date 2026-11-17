//! Native Hot Phrases Hub page (the AppKit half of
//! `localflow/hub_mac.py::_page_hot_phrases` and its actions).
//!
//! [`HotPhrasesEditor`] owns validation, selection, external-change merging,
//! debounce/undo deadlines and preview. This file only renders that state with
//! real AppKit controls and forwards events to it. The long-lived page owns its
//! editor and captures live values before every Hub resize/appearance rebuild;
//! ordinary edits, notices and polling update the existing views in place so
//! the text selection and first responder are not reset.
//!
//! All timers and control callbacks execute on AppKit's main thread. They hold
//! only [`Weak`] references, and a generation counter rejects callbacks from a
//! timer replaced by a rebuild. There is no background worker.
#![allow(deprecated)]

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use anyhow::Result;
use lf_core::dictionary::DictionaryStore;
use lf_core::hotphrases::HotPhraseStore;
use lf_core::phrases_model::{PhrasesModel, PhrasesSettings};
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAccessibility, NSAutoresizingMaskOptions, NSBorderType, NSButton, NSControlStateValueOff,
    NSControlStateValueOn, NSScrollView, NSScrollerStyle, NSSwitch, NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSSize, NSString};

use crate::hot_phrases_editor::{
    empty_layout, enabled_label, hot_phrases_layout, preview_placeholder, HotPhrasesEditor,
    HotPhrasesLayout, IRect, DISABLED_BANNER, EMPTY_MESSAGE, ENABLED_ACCESSIBILITY, READ_ERROR,
};
use crate::mac::actions::{ActionTarget, Timer};
use crate::mac::page::{HubPage, HubServices, PageCtx};
use crate::mac::style::{
    color, font, hide_from_accessibility, rect, style_surface, FlippedView, PlaceholderTextView,
    SurfaceStyle,
};
use crate::mac::widgets::{
    button, content_bottom, field, label, remove_subviews, search_field, LabelStyle,
};
use crate::tokens::{surface_tint, Palette, Weight, HUB_PAGE_BOTTOM};

const PAGE: &str = "Hot Phrases";
const TICK_SECONDS: f64 = 0.2;
const ROW_HEIGHT: f64 = 54.0;
const ROW_GAP: f64 = 4.0;
const EMPTY_DETAIL: &str =
    "Hot phrases swap a short spoken trigger for text you saved, right as you dictate.";

/// The editor's controls. These references die with the current render; the
/// `HotPhrasesEditor` and captured values outlive them.
#[derive(Clone)]
struct EditorViews {
    search: Retained<objc2_app_kit::NSSearchField>,
    new: Retained<NSButton>,
    list_scroll: Retained<NSScrollView>,
    list: Retained<FlippedView>,
    card: Retained<FlippedView>,
    trigger_label: Retained<NSTextField>,
    trigger: Retained<NSTextField>,
    trigger_hint: Retained<NSTextField>,
    text_label: Retained<NSTextField>,
    text_scroll: Retained<NSScrollView>,
    text: Retained<PlaceholderTextView>,
    text_hint: Retained<NSTextField>,
    enabled_switch: Retained<NSSwitch>,
    enabled_label: Retained<NSTextField>,
    enabled_notice: Retained<NSTextField>,
    delete: Retained<NSButton>,
    error: Retained<NSTextField>,
    undo_bar: Retained<FlippedView>,
    undo_label: Retained<NSTextField>,
    preview_label: Retained<NSTextField>,
    test: Retained<NSTextField>,
    preview_scroll: Retained<NSScrollView>,
    preview: Retained<PlaceholderTextView>,
    preview_status: Retained<NSTextField>,
}

#[derive(Clone)]
struct EmptyViews {
    card: Retained<FlippedView>,
    error: Retained<NSTextField>,
    undo_bar: Retained<FlippedView>,
    undo_label: Retained<NSTextField>,
}

#[derive(Clone)]
enum Views {
    Editor(EditorViews),
    Empty(EmptyViews),
}

#[derive(Clone)]
struct Geometry {
    top: f64,
    width: f64,
    available_height: f64,
    viewport_height: f64,
    body: Retained<FlippedView>,
}

#[derive(Default)]
struct State {
    editor: Option<HotPhrasesEditor>,
    /// A failure constructing the model itself (the ordinary store failures
    /// are already represented by `editor.error`).
    model_error: Option<String>,
    views: Option<Views>,
    geometry: Option<Geometry>,
    rebuild: Option<Rc<dyn Fn()>>,
    services: Option<Rc<HubServices>>,
    palette: Option<Palette>,
    mtm: Option<MainThreadMarker>,
    action_target: Option<Retained<ActionTarget>>,
    active: bool,
    rebuild_requested: bool,
    focus_trigger: bool,
    last_notices: (bool, bool),
    list_scroll_y: f64,
    timer: Option<Timer>,
    timer_generation: u64,
}

type Shared = Rc<RefCell<State>>;

/// The embedded native Hot Phrases editor.
pub struct HotPhrasesPage {
    state: Shared,
}

impl Default for HotPhrasesPage {
    fn default() -> Self {
        Self::new()
    }
}

impl HotPhrasesPage {
    /// Construct the page without touching disk. The real store and settings
    /// are loaded on the first render, once the Hub's injected services exist.
    pub fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(State::default())),
        }
    }

    /// Supply a real, prepared editor for an isolated native snapshot.
    ///
    /// Snapshot code seeds an `lf_core` temporary store, then drives this
    /// editor's normal `on_show`, `select`, `fill`, `delete`, preview and flush
    /// methods before calling here. The next render uses that exact state;
    /// there is no parallel fixture model or fake screenshot content. Call
    /// before showing the page.
    pub fn seed_for_snapshot(&mut self, editor: HotPhrasesEditor) {
        let timer = {
            let mut state = self.state.borrow_mut();
            state.timer_generation = state.timer_generation.wrapping_add(1);
            state.editor = Some(editor);
            state.model_error = None;
            state.views = None;
            state.geometry = None;
            state.active = false;
            state.rebuild_requested = false;
            state.focus_trigger = false;
            state.last_notices = (false, false);
            state.list_scroll_y = 0.0;
            state.timer.take()
        };
        drop(timer);
    }
}

/// Build the model lazily. The preview uses the same dictionary path as the
/// injected Hub service, which also keeps isolated snapshots off the real home.
fn make_editor(services: &HubServices) -> Result<HotPhrasesEditor> {
    let settings = PhrasesSettings::from_config(&services.config());
    let store = if services.memory_only_config.get() {
        HotPhraseStore::new(
            services
                .dictionary
                .path()
                .with_file_name("hot_phrases.json"),
        )
    } else {
        HotPhraseStore::default_store()
    };
    let model = PhrasesModel::new(store, Some(settings))?.with_dictionary(DictionaryStore::new(
        Some(services.dictionary.path().to_path_buf()),
    ));
    Ok(HotPhrasesEditor::new(model))
}

fn ensure_editor(state: &Shared) {
    let services = state.borrow().services.clone();
    let Some(services) = services else { return };
    if state.borrow().editor.is_some() || state.borrow().model_error.is_some() {
        return;
    }
    match make_editor(&services) {
        Ok(editor) => state.borrow_mut().editor = Some(editor),
        Err(_) => state.borrow_mut().model_error = Some(READ_ERROR.to_string()),
    }
}

fn with_editor_mut(state: &Shared, run: impl FnOnce(&mut HotPhrasesEditor)) {
    if let Some(editor) = state.borrow_mut().editor.as_mut() {
        run(editor);
    }
}

fn native_rect(frame: IRect, dy: f64) -> objc2_foundation::NSRect {
    rect(
        f64::from(frame.0),
        f64::from(frame.1) + dy,
        f64::from(frame.2),
        f64::from(frame.3),
    )
}

/// Refit the Hub's document after an in-place notice layout change. The shell
/// will do the same calculation after a full render.
fn fit_body(geometry: &Geometry, minimum_bottom: f64) {
    let height = (content_bottom(&geometry.body).max(minimum_bottom) + f64::from(HUB_PAGE_BOTTOM))
        .max(geometry.viewport_height);
    let width = geometry.body.frame().size.width;
    geometry.body.setFrame(rect(0.0, 0.0, width, height));
}

fn request_rebuild(state: &Shared) {
    let rebuild = {
        let mut state = state.borrow_mut();
        if !state.active || state.rebuild_requested {
            return;
        }
        state.rebuild_requested = true;
        state.rebuild.clone()
    };
    if let Some(rebuild) = rebuild {
        rebuild();
    }
}

// ------------------------------------------------------------- field state ---

/// `_phrases_pull`: only read the form after programmatic field loading has
/// been acknowledged. Programmatic loads must not become spurious autosaves.
fn pull_fields(state: &Shared) {
    let controls = {
        let state = state.borrow();
        match &state.views {
            Some(Views::Editor(views)) => Some((
                views.trigger.clone(),
                views.text.clone(),
                views.enabled_switch.clone(),
            )),
            _ => None,
        }
    };
    let Some((trigger, text, enabled_switch)) = controls else {
        return;
    };
    let (trigger, text, enabled) = (
        trigger.stringValue().to_string(),
        text.string().to_string(),
        enabled_switch.state() == NSControlStateValueOn,
    );
    let mut state = state.borrow_mut();
    if !state.active {
        return;
    }
    let Some(editor) = state.editor.as_mut() else {
        return;
    };
    if editor.fields_stale {
        return;
    }
    if (trigger.as_str(), text.as_str(), enabled)
        != (
            editor.trigger.as_str(),
            editor.text.as_str(),
            editor.enabled,
        )
    {
        editor.edit(Some(&trigger), Some(&text), Some(enabled));
    }
}

fn pull_capture(state: &Shared) {
    pull_fields(state);
    let controls = {
        let state = state.borrow();
        match &state.views {
            Some(Views::Editor(views)) => Some((
                views.search.clone(),
                views.test.clone(),
                views.list_scroll.clone(),
            )),
            _ => None,
        }
    };
    let Some((search, test, list_scroll)) = controls else {
        return;
    };
    let query = search.stringValue().to_string();
    let test_input = test.stringValue().to_string();
    let scroll_y = list_scroll.contentView().bounds().origin.y;
    let mut state = state.borrow_mut();
    state.list_scroll_y = scroll_y;
    if let Some(editor) = state.editor.as_mut() {
        // Search and preview already update live, but capturing them here makes
        // a resize robust even if AppKit has not delivered the latest delegate
        // callback yet.
        if editor.query != query {
            editor.set_query(&query);
        }
        editor.set_test_input(&test_input);
    }
}

fn push_fields(state: &Shared) {
    let (controls, values) = {
        let state = state.borrow();
        let values = state
            .editor
            .as_ref()
            .map(|editor| (editor.trigger.clone(), editor.text.clone(), editor.enabled));
        let controls = match &state.views {
            Some(Views::Editor(views)) => Some((
                views.trigger.clone(),
                views.text.clone(),
                views.enabled_switch.clone(),
            )),
            _ => None,
        };
        (controls, values)
    };
    let (Some((trigger, text, enabled_switch)), Some((trigger_value, text_value, enabled))) =
        (controls, values)
    else {
        return;
    };
    trigger.setStringValue(&NSString::from_str(&trigger_value));
    text.setString(&NSString::from_str(&text_value));
    enabled_switch.setState(if enabled {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    with_editor_mut(state, |editor| editor.ack_fields());
    render_enabled(state);
}

fn focus_trigger(state: &Shared) {
    let control = {
        let mut state = state.borrow_mut();
        if !state.focus_trigger {
            return;
        }
        state.focus_trigger = false;
        match &state.views {
            Some(Views::Editor(views)) => Some(views.trigger.clone()),
            _ => None,
        }
    };
    if let Some(control) = control {
        if let Some(window) = control.window() {
            window.makeFirstResponder(Some(&control));
        }
    }
}

// --------------------------------------------------------------- list view ---

/// `_phrases_render_list`. Rows carry stable IDs rather than relying on an
/// index that can change when the model re-sorts after a save.
fn render_list(state: &Shared, restore_scroll: bool) {
    let (views, palette, mtm, target, rows, no_matches, saved_scroll) = {
        let state = state.borrow();
        let views = match &state.views {
            Some(Views::Editor(views)) => views.clone(),
            _ => return,
        };
        let (Some(palette), Some(mtm), Some(target)) =
            (state.palette, state.mtm, state.action_target.clone())
        else {
            return;
        };
        let rows = state
            .editor
            .as_ref()
            .map(|editor| editor.rows())
            .unwrap_or_default();
        let no_matches = state
            .editor
            .as_ref()
            .is_some_and(|editor| editor.no_matches());
        (
            views,
            palette,
            mtm,
            target,
            rows,
            no_matches,
            state.list_scroll_y,
        )
    };
    remove_subviews(&views.list);
    let width = views.list_scroll.contentSize().width.max(40.0);
    if rows.is_empty() {
        views.list.addSubview(&label(
            mtm,
            if no_matches {
                "No matches"
            } else {
                "No phrases yet"
            },
            8.0,
            8.0,
            (width - 16.0).max(40.0),
            LabelStyle {
                size: 12.0,
                color: "muted",
                height: 20.0,
                ..LabelStyle::default()
            },
            &palette,
        ));
    }
    let weak = Rc::downgrade(state);
    for (index, row) in rows.iter().enumerate() {
        let holder = FlippedView::new(
            mtm,
            rect(
                0.0,
                index as f64 * (ROW_HEIGHT + ROW_GAP),
                width,
                ROW_HEIGHT,
            ),
        );
        holder.setWantsLayer(true);
        if let Some(layer) = holder.layer() {
            layer.setCornerRadius(8.0);
            if row.selected {
                let tint = surface_tint("cobalt", &palette, 0.16)
                    .unwrap_or_else(|| palette.surface.to_string());
                layer.setBackgroundColor(Some(&color(&tint, &palette).CGColor()));
            }
        }

        let dot = NSView::initWithFrame(mtm.alloc::<NSView>(), rect(10.0, 21.0, 10.0, 10.0));
        dot.setWantsLayer(true);
        if let Some(layer) = dot.layer() {
            layer.setCornerRadius(5.0);
            if row.enabled {
                layer.setBackgroundColor(Some(&color("success", &palette).CGColor()));
            } else {
                layer.setBorderWidth(1.5);
                layer.setBorderColor(Some(&color("muted", &palette).CGColor()));
            }
        }
        hide_from_accessibility(&dot);
        holder.addSubview(&dot);
        let text_width = (width - 38.0).max(40.0);
        holder.addSubview(&label(
            mtm,
            &row.title,
            28.0,
            8.0,
            text_width,
            LabelStyle {
                size: 13.0,
                height: 18.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &palette,
        ));
        holder.addSubview(&label(
            mtm,
            &row.subtitle,
            28.0,
            28.0,
            text_width,
            LabelStyle {
                size: 11.0,
                color: "muted",
                height: 16.0,
                ..LabelStyle::default()
            },
            &palette,
        ));

        let hit =
            NSButton::initWithFrame(mtm.alloc::<NSButton>(), rect(0.0, 0.0, width, ROW_HEIGHT));
        hit.setTitle(&NSString::from_str(""));
        hit.setBordered(false);
        hit.setAccessibilityLabel(Some(&NSString::from_str(&row.accessibility)));
        let id = row.id.clone();
        let weak = weak.clone();
        target.on_action(&hit, move || {
            select_phrase(&weak, &id);
        });
        holder.addSubview(&hit);
        views.list.addSubview(&holder);
    }
    let total =
        (rows.len() as f64 * (ROW_HEIGHT + ROW_GAP)).max(views.list_scroll.contentSize().height);
    views.list.setFrame(rect(0.0, 0.0, width, total));
    if restore_scroll {
        let limit = (total - views.list_scroll.contentSize().height).max(0.0);
        views
            .list
            .scrollPoint(NSPoint::new(0.0, saved_scroll.min(limit)));
    }
}

// --------------------------------------------------------------- rendering ---

fn text_area(
    ctx: &PageCtx,
    frame: IRect,
    value: &str,
    editable: bool,
    accessibility: &str,
) -> (Retained<NSScrollView>, Retained<PlaceholderTextView>) {
    let scroll =
        NSScrollView::initWithFrame(ctx.mtm.alloc::<NSScrollView>(), native_rect(frame, 0.0));
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(false);
    scroll.setScrollerStyle(NSScrollerStyle::Legacy);
    scroll.setBorderType(NSBorderType::BezelBorder);
    scroll.setDrawsBackground(true);
    scroll.setBackgroundColor(&color("surface", &ctx.palette));
    let size = scroll.contentSize();
    let view = PlaceholderTextView::new(ctx.mtm, rect(0.0, 0.0, size.width, size.height));
    view.setMinSize(NSSize::new(0.0, size.height));
    view.setMaxSize(NSSize::new(10_000_000.0, 10_000_000.0));
    view.setVerticallyResizable(true);
    view.setHorizontallyResizable(false);
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    // SAFETY: the text container is owned by this live view and used only on
    // the main thread, as required by AppKit.
    if let Some(container) = unsafe { view.textContainer() } {
        container.setContainerSize(NSSize::new(size.width, 10_000_000.0));
        container.setWidthTracksTextView(true);
    }
    view.setFont(Some(&font(13.0, Weight::Regular)));
    view.setTextColor(Some(&color("ink", &ctx.palette)));
    view.setBackgroundColor(&color("surface", &ctx.palette));
    view.setInsertionPointColor(Some(&color("ink", &ctx.palette)));
    view.setRichText(false);
    view.setEditable(editable);
    view.setSelectable(true);
    // Saved prompts and preview output are verbatim, never typographically
    // rewritten by AppKit.
    view.setAutomaticQuoteSubstitutionEnabled(false);
    view.setAutomaticDashSubstitutionEnabled(false);
    view.setAutomaticTextReplacementEnabled(false);
    view.setAutomaticSpellingCorrectionEnabled(false);
    if editable {
        view.setAllowsUndo(true);
    }
    view.setString(&NSString::from_str(value));
    view.setAccessibilityLabel(Some(&NSString::from_str(accessibility)));
    scroll.setDocumentView(Some(&view));
    (scroll, view)
}

#[allow(clippy::too_many_arguments)]
fn card_label(
    ctx: &PageCtx,
    card: &FlippedView,
    text: &str,
    frame: IRect,
    size: f64,
    color_name: &'static str,
    weight: Weight,
    wrap: bool,
) -> Retained<NSTextField> {
    let view = label(
        ctx.mtm,
        text,
        f64::from(frame.0),
        f64::from(frame.1),
        f64::from(frame.2),
        LabelStyle {
            size,
            color: color_name,
            height: f64::from(frame.3),
            weight,
            wrap,
        },
        &ctx.palette,
    );
    card.addSubview(&view);
    view
}

fn undo_bar(
    ctx: &PageCtx,
    frame: IRect,
    on_undo: impl Fn() + 'static,
) -> (Retained<FlippedView>, Retained<NSTextField>) {
    let bar = FlippedView::new(ctx.mtm, native_rect(frame, 0.0));
    style_surface(
        &bar,
        &ctx.palette,
        &SurfaceStyle {
            fill: "paper",
            ..SurfaceStyle::default()
        },
    );
    let text = label(
        ctx.mtm,
        "",
        12.0,
        9.0,
        f64::from((frame.2 - 96).max(40)),
        LabelStyle {
            size: 12.0,
            height: 18.0,
            ..LabelStyle::default()
        },
        &ctx.palette,
    );
    bar.addSubview(&text);
    let undo = button(
        ctx.mtm,
        "Undo",
        f64::from((frame.2 - 84).max(0)),
        0.0,
        76.0,
        36.0,
        crate::mac::style::ButtonKind::Secondary,
        &ctx.palette,
    );
    ctx.target.on_action(&undo, on_undo);
    bar.addSubview(&undo);
    (bar, text)
}

fn temporary_frame(frame: IRect, height: i32) -> IRect {
    (16, 0, frame.2, height)
}

fn render_empty(state: &Shared, ctx: &PageCtx, top: f64, weak: &Weak<RefCell<State>>) -> f64 {
    let (error, undo_label, notices) = {
        let state = state.borrow();
        let error = state
            .editor
            .as_ref()
            .map(|editor| editor.error.clone())
            .or_else(|| state.model_error.clone())
            .unwrap_or_default();
        let undo_label = state
            .editor
            .as_ref()
            .map(|editor| editor.undo_label())
            .unwrap_or_default();
        let error_visible = !error.is_empty();
        (
            error,
            undo_label,
            (
                error_visible,
                state
                    .editor
                    .as_ref()
                    .is_some_and(|e| e.undo_token.is_some()),
            ),
        )
    };
    let (rows, height) = empty_layout(ctx.width as i32, notices.0, notices.1);
    let card = FlippedView::new(ctx.mtm, rect(0.0, top, ctx.width, f64::from(height)));
    style_surface(&card, &ctx.palette, &SurfaceStyle::default());
    ctx.body.addSubview(&card);
    card_label(
        ctx,
        &card,
        EMPTY_MESSAGE,
        rows.at("message"),
        15.0,
        "ink",
        Weight::Semibold,
        true,
    );
    card_label(
        ctx,
        &card,
        EMPTY_DETAIL,
        rows.at("detail"),
        12.0,
        "muted",
        Weight::Regular,
        true,
    );
    let new_frame = rows.at("new");
    let new = button(
        ctx.mtm,
        "+ New phrase",
        f64::from(new_frame.0),
        f64::from(new_frame.1),
        f64::from(new_frame.2),
        f64::from(new_frame.3),
        crate::mac::style::ButtonKind::Primary,
        &ctx.palette,
    );
    {
        let weak = weak.clone();
        ctx.target.on_action(&new, move || new_phrase(&weak));
    }
    card.addSubview(&new);
    let example_frame = rows.at("example");
    let example = button(
        ctx.mtm,
        "Add an example",
        f64::from(example_frame.0),
        f64::from(example_frame.1),
        f64::from(example_frame.2),
        f64::from(example_frame.3),
        crate::mac::style::ButtonKind::Secondary,
        &ctx.palette,
    );
    {
        let weak = weak.clone();
        ctx.target.on_action(&example, move || add_example(&weak));
    }
    card.addSubview(&example);

    let error_frame = rows
        .get("error")
        .unwrap_or_else(|| temporary_frame(rows.at("message"), 34));
    let error_view = card_label(
        ctx,
        &card,
        &error,
        error_frame,
        12.0,
        "red",
        Weight::Regular,
        true,
    );
    error_view.setHidden(!notices.0);
    let undo_frame = rows
        .get("undo")
        .unwrap_or_else(|| temporary_frame(rows.at("message"), 36));
    let (undo_bar_view, undo_text) = {
        let weak = weak.clone();
        undo_bar(ctx, undo_frame, move || undo_delete(&weak))
    };
    undo_text.setStringValue(&NSString::from_str(&undo_label));
    undo_bar_view.setHidden(!notices.1);
    card.addSubview(&undo_bar_view);

    state.borrow_mut().views = Some(Views::Empty(EmptyViews {
        card,
        error: error_view,
        undo_bar: undo_bar_view,
        undo_label: undo_text,
    }));
    state.borrow_mut().last_notices = notices;
    top + f64::from(height) + 12.0
}

#[allow(clippy::too_many_lines)]
fn render_editor(state: &Shared, ctx: &PageCtx, top: f64, weak: &Weak<RefCell<State>>) -> f64 {
    let (layout, query, trigger_value, text_value, enabled, test_value) = {
        let state = state.borrow();
        let editor = state
            .editor
            .as_ref()
            .expect("editor surface requires editor");
        let notices = editor.notice_visibility();
        (
            hot_phrases_layout(
                state.geometry.as_ref().map_or(ctx.width, |g| g.width),
                state.geometry.as_ref().map_or(
                    ctx.viewport_height - top - f64::from(HUB_PAGE_BOTTOM),
                    |g| g.available_height,
                ),
                notices.0,
                notices.1,
            ),
            editor.query.clone(),
            editor.trigger.clone(),
            editor.text.clone(),
            editor.enabled,
            editor.test_input.clone(),
        )
    };
    state.borrow_mut().last_notices = (
        layout.card_rows.contains("error_label"),
        layout.card_rows.contains("undo_bar"),
    );

    let search = search_field(
        ctx.mtm,
        &query,
        f64::from(layout.search.0),
        top + f64::from(layout.search.1),
        f64::from(layout.search.2),
        "Search phrases",
    );
    search.setAccessibilityLabel(Some(&NSString::from_str("Search hot phrases")));
    search.setSendsSearchStringImmediately(true);
    ctx.body.addSubview(&search);

    let new = button(
        ctx.mtm,
        "+ New phrase",
        f64::from(layout.new.0),
        top + f64::from(layout.new.1),
        f64::from(layout.new.2),
        f64::from(layout.new.3),
        crate::mac::style::ButtonKind::Primary,
        &ctx.palette,
    );
    ctx.body.addSubview(&new);

    let list_scroll = NSScrollView::initWithFrame(
        ctx.mtm.alloc::<NSScrollView>(),
        rect(
            f64::from(layout.list.0),
            top + f64::from(layout.list.1),
            f64::from(layout.list.2),
            f64::from(layout.list.3),
        ),
    );
    list_scroll.setHasVerticalScroller(true);
    list_scroll.setAutohidesScrollers(false);
    list_scroll.setScrollerStyle(NSScrollerStyle::Legacy);
    list_scroll.setBorderType(NSBorderType::NoBorder);
    list_scroll.setDrawsBackground(false);
    list_scroll.setAccessibilityLabel(Some(&NSString::from_str("Hot phrases")));
    let list = FlippedView::new(
        ctx.mtm,
        rect(0.0, 0.0, f64::from(layout.list.2), f64::from(layout.list.3)),
    );
    list_scroll.setDocumentView(Some(&list));
    ctx.body.addSubview(&list_scroll);

    let card = FlippedView::new(
        ctx.mtm,
        rect(
            f64::from(layout.card.0),
            top + f64::from(layout.card.1),
            f64::from(layout.card.2),
            f64::from(layout.card.3),
        ),
    );
    style_surface(&card, &ctx.palette, &SurfaceStyle::default());
    ctx.body.addSubview(&card);
    let rows = &layout.card_rows;

    let trigger_label = card_label(
        ctx,
        &card,
        "When I say",
        rows.at("trigger_label"),
        12.0,
        "ink",
        Weight::Semibold,
        false,
    );
    let trigger_frame = rows.at("trigger_field");
    let trigger = field(
        ctx.mtm,
        &trigger_value,
        f64::from(trigger_frame.0),
        f64::from(trigger_frame.1),
        f64::from(trigger_frame.2),
        "When I say",
    );
    trigger.setPlaceholderString(Some(&NSString::from_str("e.g. review checklist")));
    card.addSubview(&trigger);
    let trigger_hint = card_label(
        ctx,
        &card,
        "",
        rows.at("trigger_hint"),
        11.0,
        "muted",
        Weight::Regular,
        false,
    );
    let text_label = card_label(
        ctx,
        &card,
        "Type this",
        rows.at("text_label"),
        12.0,
        "ink",
        Weight::Semibold,
        false,
    );
    let (text_scroll, text) = text_area(
        ctx,
        rows.at("text_scroll"),
        &text_value,
        true,
        "Text to type",
    );
    card.addSubview(&text_scroll);
    let text_hint = card_label(
        ctx,
        &card,
        "",
        rows.at("text_hint"),
        11.0,
        "muted",
        Weight::Regular,
        false,
    );

    let switch_frame = rows.at("enabled_switch");
    let enabled_switch =
        NSSwitch::initWithFrame(ctx.mtm.alloc::<NSSwitch>(), native_rect(switch_frame, 0.0));
    enabled_switch.setAccessibilityLabel(Some(&NSString::from_str(ENABLED_ACCESSIBILITY)));
    enabled_switch.setState(if enabled {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    card.addSubview(&enabled_switch);
    let enabled_label_view = card_label(
        ctx,
        &card,
        enabled_label(enabled),
        rows.at("enabled_label"),
        13.0,
        "ink",
        Weight::Regular,
        false,
    );
    let enabled_notice = card_label(
        ctx,
        &card,
        "",
        rows.at("enabled_notice"),
        12.0,
        "success",
        Weight::Regular,
        false,
    );
    let delete_frame = rows.at("delete_button");
    let delete = button(
        ctx.mtm,
        "Delete",
        f64::from(delete_frame.0),
        f64::from(delete_frame.1),
        f64::from(delete_frame.2),
        f64::from(delete_frame.3),
        crate::mac::style::ButtonKind::DestructiveGhost,
        &ctx.palette,
    );
    card.addSubview(&delete);

    let error_frame = rows
        .get("error_label")
        .unwrap_or_else(|| temporary_frame(trigger_frame, 34));
    let error = card_label(
        ctx,
        &card,
        "",
        error_frame,
        12.0,
        "red",
        Weight::Regular,
        true,
    );
    let undo_frame = rows
        .get("undo_bar")
        .unwrap_or_else(|| temporary_frame(trigger_frame, 36));
    let (undo_bar_view, undo_label_view) = {
        let weak = weak.clone();
        undo_bar(ctx, undo_frame, move || undo_delete(&weak))
    };
    card.addSubview(&undo_bar_view);

    let preview_label = card_label(
        ctx,
        &card,
        "Try it: say a phrase",
        rows.at("preview_label"),
        12.0,
        "ink",
        Weight::Semibold,
        false,
    );
    let test_frame = rows.at("test_field");
    let test = field(
        ctx.mtm,
        &test_value,
        f64::from(test_frame.0),
        f64::from(test_frame.1),
        f64::from(test_frame.2),
        "Test phrase",
    );
    test.setPlaceholderString(Some(&NSString::from_str(
        "e.g. please review checklist now",
    )));
    card.addSubview(&test);
    let (preview_scroll, preview) = text_area(
        ctx,
        rows.at("preview_output"),
        "",
        false,
        "Dictation preview",
    );
    card.addSubview(&preview_scroll);
    let preview_status = card_label(
        ctx,
        &card,
        "",
        rows.at("preview_status"),
        11.0,
        "muted",
        Weight::Regular,
        false,
    );

    let views = EditorViews {
        search: search.clone(),
        new: new.clone(),
        list_scroll,
        list,
        card,
        trigger_label,
        trigger: trigger.clone(),
        trigger_hint,
        text_label,
        text_scroll,
        text: text.clone(),
        text_hint,
        enabled_switch: enabled_switch.clone(),
        enabled_label: enabled_label_view,
        enabled_notice,
        delete: delete.clone(),
        error,
        undo_bar: undo_bar_view,
        undo_label: undo_label_view,
        preview_label,
        test: test.clone(),
        preview_scroll,
        preview,
        preview_status,
    };
    error_label_visibility(&views, &layout);
    state.borrow_mut().views = Some(Views::Editor(views));

    // Handlers are attached after initial values are installed. Text change
    // callbacks therefore always describe user edits.
    {
        let changed = weak.clone();
        ctx.target
            .on_field_change(&search, move || search_changed(&changed));
        let action = weak.clone();
        ctx.target
            .on_action(&search, move || search_changed(&action));
    }
    {
        let weak = weak.clone();
        ctx.target.on_action(&new, move || new_phrase(&weak));
    }
    {
        let field_changed = weak.clone();
        ctx.target
            .on_field_change(&trigger, move || fields_edited(&field_changed));
        let text_changed = weak.clone();
        ctx.target
            .on_text_change(&text, move || fields_edited(&text_changed));
    }
    {
        let weak = weak.clone();
        ctx.target
            .on_action(&enabled_switch, move || toggle_enabled(&weak));
    }
    {
        let weak = weak.clone();
        ctx.target.on_action(&delete, move || delete_phrase(&weak));
    }
    {
        let weak = weak.clone();
        ctx.target
            .on_field_change(&test, move || test_changed(&weak));
    }

    push_fields(state);
    sync(state);
    top + f64::from(layout.height)
}

fn error_label_visibility(views: &EditorViews, layout: &HotPhrasesLayout) {
    views
        .error
        .setHidden(!layout.card_rows.contains("error_label"));
    views
        .undo_bar
        .setHidden(!layout.card_rows.contains("undo_bar"));
}

// ---------------------------------------------------------- in-place sync ---

fn render_enabled(state: &Shared) {
    let (view, label_value) = {
        let state = state.borrow();
        let label_value = state
            .editor
            .as_ref()
            .map(|editor| enabled_label(editor.enabled));
        let view = match &state.views {
            Some(Views::Editor(views)) => Some(views.enabled_label.clone()),
            _ => None,
        };
        (view, label_value)
    };
    if let (Some(view), Some(label_value)) = (view, label_value) {
        view.setStringValue(&NSString::from_str(label_value));
    }
}

fn render_preview(state: &Shared) {
    let (views, palette, test_input, output, status) = {
        let state = state.borrow();
        let views = match &state.views {
            Some(Views::Editor(views)) => views.clone(),
            _ => return,
        };
        let Some(editor) = state.editor.as_ref() else {
            return;
        };
        let (output, status) = editor.preview();
        (
            views,
            state.palette.unwrap_or(crate::tokens::LIGHT),
            editor.test_input.clone(),
            output,
            status,
        )
    };
    let placeholder = preview_placeholder(&test_input, &output, &status);
    views
        .preview
        .set_placeholder(placeholder, Some(color("muted", &palette)));
    views.preview.setString(&NSString::from_str(&output));
    views
        .preview_status
        .setStringValue(&NSString::from_str(&status));
}

fn apply_editor_layout(state: &Shared, layout: &HotPhrasesLayout, geometry: &Geometry) {
    let views = {
        let state = state.borrow();
        match &state.views {
            Some(Views::Editor(views)) => views.clone(),
            _ => return,
        }
    };
    views
        .search
        .setFrame(native_rect(layout.search, geometry.top));
    views.new.setFrame(native_rect(layout.new, geometry.top));
    views
        .list_scroll
        .setFrame(native_rect(layout.list, geometry.top));
    views.card.setFrame(native_rect(layout.card, geometry.top));
    let rows = &layout.card_rows;
    if let Some(frame) = rows.get("trigger_label") {
        views.trigger_label.setFrame(native_rect(frame, 0.0));
    }
    // Heterogeneous controls are set explicitly; this keeps their concrete
    // types available to the editing callbacks above.
    macro_rules! place {
        ($name:literal, $view:expr) => {
            if let Some(frame) = rows.get($name) {
                $view.setFrame(native_rect(frame, 0.0));
            }
        };
    }
    place!("trigger_field", views.trigger);
    place!("trigger_hint", views.trigger_hint);
    place!("text_label", views.text_label);
    place!("text_scroll", views.text_scroll);
    place!("text_hint", views.text_hint);
    place!("enabled_switch", views.enabled_switch);
    place!("enabled_label", views.enabled_label);
    place!("enabled_notice", views.enabled_notice);
    place!("delete_button", views.delete);
    place!("preview_label", views.preview_label);
    place!("test_field", views.test);
    place!("preview_output", views.preview_scroll);
    place!("preview_status", views.preview_status);
    if let Some(frame) = rows.get("error_label") {
        views.error.setFrame(native_rect(frame, 0.0));
    }
    if let Some(frame) = rows.get("undo_bar") {
        views.undo_bar.setFrame(native_rect(frame, 0.0));
    }
    error_label_visibility(&views, layout);
    for (scroll, text) in [
        (&views.text_scroll, &views.text),
        (&views.preview_scroll, &views.preview),
    ] {
        text.setMinSize(NSSize::new(0.0, scroll.contentSize().height));
    }
    fit_body(geometry, geometry.top + f64::from(layout.height));
}

fn render_status(state: &Shared) {
    let (views, palette, trigger_hint, text_hint, notices, error, notice, undo, can_delete) = {
        let state = state.borrow();
        let views = match &state.views {
            Some(Views::Editor(views)) => views.clone(),
            _ => return,
        };
        let Some(editor) = state.editor.as_ref() else {
            return;
        };
        (
            views,
            state.palette.unwrap_or(crate::tokens::LIGHT),
            editor.trigger_hint(),
            editor.text_hint(),
            editor.notice_visibility(),
            editor.error.clone(),
            editor.notice.clone(),
            editor.undo_label(),
            editor.current_id.is_some() || !editor.blank(),
        )
    };
    let geometry = state.borrow().geometry.clone();
    if state.borrow().last_notices != notices {
        if let Some(geometry) = geometry {
            let layout = hot_phrases_layout(
                geometry.width,
                geometry.available_height,
                notices.0,
                notices.1,
            );
            apply_editor_layout(state, &layout, &geometry);
            state.borrow_mut().last_notices = notices;
            render_list(state, false);
        }
    }
    views
        .trigger_hint
        .setStringValue(&NSString::from_str(&trigger_hint.0));
    views.trigger_hint.setTextColor(Some(&color(
        if trigger_hint.1 { "red" } else { "muted" },
        &palette,
    )));
    views
        .text_hint
        .setStringValue(&NSString::from_str(&text_hint.0));
    views.text_hint.setTextColor(Some(&color(
        if text_hint.1 { "red" } else { "muted" },
        &palette,
    )));
    views.error.setStringValue(&NSString::from_str(&error));
    views
        .enabled_notice
        .setStringValue(&NSString::from_str(if error.is_empty() {
            &notice
        } else {
            ""
        }));
    views.undo_bar.setHidden(undo.is_empty());
    views.undo_label.setStringValue(&NSString::from_str(&undo));
    views.delete.setEnabled(can_delete);
    render_enabled(state);
    render_preview(state);
}

fn sync_empty(state: &Shared) {
    let (views, error, undo, notices, geometry) = {
        let state = state.borrow();
        let views = match &state.views {
            Some(Views::Empty(views)) => views.clone(),
            _ => return,
        };
        let error = state
            .editor
            .as_ref()
            .map(|editor| editor.error.clone())
            .or_else(|| state.model_error.clone())
            .unwrap_or_default();
        let undo = state
            .editor
            .as_ref()
            .map(|editor| editor.undo_label())
            .unwrap_or_default();
        let notices = (
            !error.is_empty(),
            state
                .editor
                .as_ref()
                .is_some_and(|e| e.undo_token.is_some()),
        );
        (views, error, undo, notices, state.geometry.clone())
    };
    if state.borrow().last_notices != notices {
        let (rows, height) =
            empty_layout(views.card.frame().size.width as i32, notices.0, notices.1);
        let frame = views.card.frame();
        views.card.setFrame(rect(
            frame.origin.x,
            frame.origin.y,
            frame.size.width,
            f64::from(height),
        ));
        if let Some(frame) = rows.get("error") {
            views.error.setFrame(native_rect(frame, 0.0));
        }
        if let Some(frame) = rows.get("undo") {
            views.undo_bar.setFrame(native_rect(frame, 0.0));
        }
        views.error.setHidden(!notices.0);
        views.undo_bar.setHidden(!notices.1);
        state.borrow_mut().last_notices = notices;
        if let Some(geometry) = geometry {
            fit_body(&geometry, frame.origin.y + f64::from(height) + 12.0);
        }
    }
    views.error.setStringValue(&NSString::from_str(&error));
    views.undo_label.setStringValue(&NSString::from_str(&undo));
    views.undo_bar.setHidden(undo.is_empty());
}

fn sync(state: &Shared) {
    let (empty, actual_empty, stale) = {
        let state = state.borrow();
        let empty = state
            .editor
            .as_ref()
            .is_none_or(|editor| editor.empty_state_visible());
        let actual_empty = matches!(state.views, Some(Views::Empty(_)));
        let stale = state
            .editor
            .as_ref()
            .is_some_and(|editor| editor.fields_stale);
        (empty, actual_empty, stale)
    };
    if empty != actual_empty {
        request_rebuild(state);
        return;
    }
    if empty {
        sync_empty(state);
        return;
    }
    if stale {
        push_fields(state);
    }
    render_list(state, false);
    render_status(state);
    focus_trigger(state);
}

// ------------------------------------------------------------------ actions ---

fn select_phrase(weak: &Weak<RefCell<State>>, id: &str) {
    let Some(state) = weak.upgrade() else { return };
    pull_fields(&state);
    with_editor_mut(&state, |editor| {
        editor.select(id);
    });
    sync(&state);
}

fn new_phrase(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else { return };
    pull_fields(&state);
    let changed = state
        .borrow_mut()
        .editor
        .as_mut()
        .is_some_and(|editor| editor.new_phrase());
    if changed {
        state.borrow_mut().focus_trigger = true;
    }
    sync(&state);
}

fn add_example(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else { return };
    with_editor_mut(&state, |editor| {
        editor.add_example();
    });
    sync(&state);
}

fn fields_edited(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else { return };
    pull_fields(&state);
    let is_editor = matches!(state.borrow().views, Some(Views::Editor(_)));
    if is_editor {
        render_status(&state);
    }
}

fn search_changed(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else { return };
    let search = {
        let state = state.borrow();
        match &state.views {
            Some(Views::Editor(views)) => Some(views.search.clone()),
            _ => None,
        }
    };
    let Some(search) = search else { return };
    let query = search.stringValue().to_string();
    with_editor_mut(&state, |editor| editor.set_query(&query));
    render_list(&state, false);
}

fn toggle_enabled(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else { return };
    pull_fields(&state);
    let enabled = {
        let state = state.borrow();
        match &state.views {
            Some(Views::Editor(views)) => {
                Some(views.enabled_switch.state() == NSControlStateValueOn)
            }
            _ => None,
        }
    };
    if let Some(enabled) = enabled {
        with_editor_mut(&state, |editor| {
            editor.set_enabled(enabled);
        });
    }
    sync(&state);
}

fn delete_phrase(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else { return };
    with_editor_mut(&state, |editor| {
        editor.delete();
    });
    sync(&state);
}

fn undo_delete(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else { return };
    with_editor_mut(&state, |editor| {
        editor.undo_delete();
    });
    sync(&state);
}

fn test_changed(weak: &Weak<RefCell<State>>) {
    let Some(state) = weak.upgrade() else { return };
    let test = {
        let state = state.borrow();
        match &state.views {
            Some(Views::Editor(views)) => Some(views.test.clone()),
            _ => None,
        }
    };
    let Some(test) = test else { return };
    let value = test.stringValue().to_string();
    with_editor_mut(&state, |editor| editor.set_test_input(&value));
    render_preview(&state);
}

// ------------------------------------------------------------------- timer ---

fn stop_timer(state: &Shared) {
    let timer = {
        let mut state = state.borrow_mut();
        state.timer_generation = state.timer_generation.wrapping_add(1);
        state.timer.take()
    };
    drop(timer);
}

fn start_timer(state: &Shared) {
    stop_timer(state);
    let generation = state.borrow().timer_generation;
    let weak = Rc::downgrade(state);
    let timer = Timer::schedule(TICK_SECONDS, true, move || {
        let Some(state) = weak.upgrade() else { return };
        let allowed = {
            let state = state.borrow();
            state.active && state.timer_generation == generation
        };
        if !allowed {
            return;
        }
        let changed = state
            .borrow_mut()
            .editor
            .as_mut()
            .is_some_and(|editor| editor.tick());
        if changed {
            sync(&state);
        }
    });
    state.borrow_mut().timer = Some(timer);
}

// --------------------------------------------------------------- Hub contract ---

impl HubPage for HotPhrasesPage {
    fn prepare_snapshot(&mut self, preparation: &str, root: &std::path::Path) -> Result<()> {
        let model = PhrasesModel::new(
            HotPhraseStore::new(root.join("hot_phrases.json")),
            Some(PhrasesSettings {
                hot_phrases: true,
                ..PhrasesSettings::default()
            }),
        )?
        .with_dictionary(DictionaryStore::new(Some(root.join("dictionary.json"))));
        if preparation != "empty" {
            model.store.add(
                "review checklist",
                "Review this PR:\n- tests\n- types\n- no churn",
                true,
            )?;
            model
                .store
                .add("sign off", "Thanks — talk tomorrow.", false)?;
            model
                .store
                .add("standup", "yesterday:\ntoday:\nblockers:", true)?;
        }
        let mut editor = HotPhrasesEditor::new(model);
        editor.on_show();
        let selected = |editor: &HotPhrasesEditor, trigger: &str| {
            editor
                .entries
                .iter()
                .find(|entry| entry["trigger"].as_str() == Some(trigger))
                .and_then(|entry| entry["id"].as_str())
                .unwrap_or_default()
                .to_string()
        };
        match preparation {
            "empty" => {}
            "basic" => {}
            "populated3-selected" => {
                let id = selected(&editor, "sign off");
                editor.select(&id);
            }
            "preview-result" => {
                let id = selected(&editor, "review checklist");
                editor.select(&id);
                editor.set_test_input("please review checklist now");
            }
            "invalid-draft" => {
                editor.new_phrase();
                editor.fill("Review, checklist", "y");
                editor.flush();
            }
            "undo" => {
                let id = selected(&editor, "sign off");
                editor.select(&id);
                editor.delete();
            }
            "error-and-undo" => {
                let id = selected(&editor, "sign off");
                editor.select(&id);
                editor.delete();
                editor.new_phrase();
                editor.fill("Review, checklist", "y");
                editor.flush();
            }
            other => anyhow::bail!("unknown Hot Phrases snapshot preparation {other:?}"),
        }
        self.seed_for_snapshot(editor);
        Ok(())
    }

    fn name(&self) -> &'static str {
        PAGE
    }

    fn capture(&mut self) {
        pull_capture(&self.state);
    }

    fn can_leave(&mut self) -> bool {
        pull_fields(&self.state);
        let allowed = {
            let mut state = self.state.borrow_mut();
            if !state.active {
                return true;
            }
            state.editor.as_mut().is_none_or(|editor| editor.leave())
        };
        if !allowed {
            // A geometry-changing validation error must be visible while the
            // shell vetoes navigation. No rebuild means no loss of the draft or
            // the caret.
            sync(&self.state);
        }
        allowed
    }

    fn flush(&mut self) {
        self.capture();
        let active = self.state.borrow().active;
        if active {
            let mut state = self.state.borrow_mut();
            if let Some(editor) = state.editor.as_mut() {
                let _ = editor.flush();
                editor.cancel();
            }
        }
        self.state.borrow_mut().active = false;
        stop_timer(&self.state);
        self.state.borrow_mut().views = None;
        self.state.borrow_mut().geometry = None;
        self.state.borrow_mut().rebuild = None;
    }

    fn shutdown(&mut self) {
        self.flush();
        if let Some(editor) = self.state.borrow_mut().editor.as_mut() {
            editor.cancel();
        }
        self.state.borrow_mut().services = None;
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        stop_timer(&self.state);
        {
            let mut state = self.state.borrow_mut();
            state.views = None;
            state.services = Some(ctx.services.clone());
            state.rebuild = Some(ctx.rebuild.clone());
            state.rebuild_requested = false;
        }
        ensure_editor(&self.state);
        let entering = !self.state.borrow().active;
        if entering {
            let mut state = self.state.borrow_mut();
            state.active = true;
            if let Some(editor) = state.editor.as_mut() {
                editor.on_show();
            }
        }
        let banner = self.state.borrow().editor.as_ref().map_or("", |editor| {
            if editor.banner().is_empty() {
                ""
            } else {
                DISABLED_BANNER
            }
        });
        let mut cursor = top;
        if !banner.is_empty() {
            ctx.body.addSubview(&label(
                ctx.mtm,
                banner,
                0.0,
                cursor,
                ctx.width,
                LabelStyle {
                    size: 12.0,
                    color: "muted",
                    height: 20.0,
                    ..LabelStyle::default()
                },
                &ctx.palette,
            ));
            cursor += 28.0;
        }
        self.state.borrow_mut().geometry = Some(Geometry {
            top: cursor,
            width: ctx.width,
            available_height: ctx.viewport_height - cursor - f64::from(HUB_PAGE_BOTTOM),
            viewport_height: ctx.viewport_height,
            body: ctx.body.clone(),
        });
        self.state.borrow_mut().palette = Some(ctx.palette);
        self.state.borrow_mut().mtm = Some(ctx.mtm);
        self.state.borrow_mut().action_target = Some(ctx.target.clone());
        let weak = Rc::downgrade(&self.state);
        let empty = self
            .state
            .borrow()
            .editor
            .as_ref()
            .is_none_or(|editor| editor.empty_state_visible());
        let bottom = if empty {
            render_empty(&self.state, ctx, cursor, &weak)
        } else {
            render_editor(&self.state, ctx, cursor, &weak)
        };
        if empty {
            sync_empty(&self.state);
        }
        render_list(&self.state, true);
        focus_trigger(&self.state);
        start_timer(&self.state);
        bottom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(dir: &tempfile::TempDir) -> PhrasesModel {
        PhrasesModel::new(
            HotPhraseStore::new(dir.path().join("hot_phrases.json")),
            Some(PhrasesSettings {
                hot_phrases: true,
                ..PhrasesSettings::default()
            }),
        )
        .unwrap()
        .with_dictionary(DictionaryStore::new(Some(
            dir.path().join("dictionary.json"),
        )))
    }

    fn populated(dir: &tempfile::TempDir) -> HotPhrasesEditor {
        let model = model(dir);
        model.store.add("review checklist", "CHECK", true).unwrap();
        model.store.add("sign off", "Thanks", false).unwrap();
        let mut editor = HotPhrasesEditor::new(model);
        editor.on_show();
        editor
    }

    #[test]
    fn page_contract_and_construction_are_lazy() {
        let page = HotPhrasesPage::new();
        assert_eq!(page.name(), "Hot Phrases");
        assert!(page.state.borrow().editor.is_none());
        assert!(!page.state.borrow().active);
    }

    #[test]
    fn snapshot_seeding_accepts_empty_populated_editing_and_error_states() {
        let empty_dir = tempfile::tempdir().unwrap();
        let empty = HotPhrasesEditor::new(model(&empty_dir));
        let mut page = HotPhrasesPage::new();
        page.seed_for_snapshot(empty);
        assert!(page
            .state
            .borrow()
            .editor
            .as_ref()
            .unwrap()
            .empty_state_visible());

        let populated_dir = tempfile::tempdir().unwrap();
        let editor = populated(&populated_dir);
        page.seed_for_snapshot(editor);
        let state = page.state.borrow();
        let editor = state.editor.as_ref().unwrap();
        assert_eq!(editor.rows().len(), 2);
        assert_eq!(editor.trigger, "review checklist");
        drop(state);

        let editing_dir = tempfile::tempdir().unwrap();
        let mut editing = populated(&editing_dir);
        editing.new_phrase();
        editing.fill("standup", "today: tests");
        page.seed_for_snapshot(editing);
        let state = page.state.borrow();
        assert!(state.editor.as_ref().unwrap().draft);
        assert!(state.editor.as_ref().unwrap().pending());
        drop(state);

        let error_dir = tempfile::tempdir().unwrap();
        let mut invalid = populated(&error_dir);
        invalid.new_phrase();
        invalid.fill("Review, checklist", "duplicate");
        assert!(!invalid.flush());
        page.seed_for_snapshot(invalid);
        let state = page.state.borrow();
        let editor = state.editor.as_ref().unwrap();
        assert!(editor.error.starts_with("Already used by"));
        assert!(editor.draft && editor.pending());
        assert!(!state.active);
    }

    #[test]
    fn no_control_flush_commits_and_deactivates_a_valid_draft() {
        let dir = tempfile::tempdir().unwrap();
        let store = HotPhraseStore::new(dir.path().join("hot_phrases.json"));
        let mut editor = HotPhrasesEditor::new(model(&dir));
        editor.new_phrase();
        editor.fill("standup", "today: tests");
        let mut page = HotPhrasesPage::new();
        page.seed_for_snapshot(editor);
        page.state.borrow_mut().active = true;
        page.flush();
        let entries = store.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["trigger"], "standup");
        assert!(!page.state.borrow().active);
        assert!(page.state.borrow().timer.is_none());
    }

    #[test]
    fn navigation_guard_keeps_an_invalid_draft_local() {
        let dir = tempfile::tempdir().unwrap();
        let store = HotPhraseStore::new(dir.path().join("hot_phrases.json"));
        let mut editor = populated(&dir);
        editor.new_phrase();
        editor.fill("Review, checklist", "duplicate");
        let mut page = HotPhrasesPage::new();
        page.seed_for_snapshot(editor);
        page.state.borrow_mut().active = true;
        assert!(!page.can_leave());
        let state = page.state.borrow();
        let editor = state.editor.as_ref().unwrap();
        assert!(editor.error.contains("Fix it to continue"));
        assert_eq!(editor.trigger, "Review, checklist");
        assert_eq!(store.list().len(), 2);
    }
}
