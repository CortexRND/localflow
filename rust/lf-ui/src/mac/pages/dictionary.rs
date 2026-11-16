//! Dictionary page (port of `_page_dictionary`, `_render_dictionary`,
//! `_reload_dictionary`, `addWord_`, `addReplacement_`, `removeDictionary_`,
//! `editDictionary_` and `importDictionary_` of `localflow/hub_mac.py`).
//!
//! Two cards (a word field with *Add word*, a from/to pair with *Add
//! replacement*), an *Import CSV/JSON…* button with a real `NSOpenPanel`, an
//! optional import notice banner and the scrolling list of every entry with
//! *Edit* / *Remove*. Friendly errors show inline beside the card they belong
//! to (never through the shell callout). The typed fields and the entries being
//! edited live in a [`DictionaryDraft`] that survives every rebuild
//! (`capture()` reads the live controls into it, `render()` puts it back).
//!
//! All handlers hold only `Weak` references to the page state, so a rebuild or
//! a closed Hub never keeps views or the state alive through the controls'
//! unretained target.
#![allow(deprecated)]

use std::cell::RefCell;
use std::path::Path;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2_app_kit::{
    NSAccessibility, NSAutoresizingMaskOptions, NSButton, NSModalResponseOK, NSOpenPanel,
    NSScrollView, NSTextField, NSView,
};
use objc2_foundation::{NSArray, NSString};

use crate::hub_model::{
    dictionary_entries, dictionary_import_notice, remove_entry, submit_replacement, submit_word,
    DictionaryCard, DictionaryDraft, DictionaryRow, EntryKind, DICTIONARY_IMPORT_FAILED,
};
use crate::mac::page::{HubPage, HubServices, PageCtx};
use crate::mac::style::{color, rect, style_surface, ButtonKind, FlippedView, SurfaceStyle};
use crate::mac::widgets::{button, field, label, LabelStyle};
use crate::tokens::{Palette, Weight, CARD_PADDING, HUB_PAGE_BOTTOM, SUN_TEXT};

const PAGE: &str = "Dictionary";
const ROW_HEIGHT: f64 = 56.0;

/// Live controls of the page (dropped with the views on every rebuild).
struct Fields {
    word: Retained<NSTextField>,
    from: Retained<NSTextField>,
    to: Retained<NSTextField>,
    add_word: Retained<NSButton>,
    add_replacement: Retained<NSButton>,
}

#[derive(Default)]
struct State {
    draft: DictionaryDraft,
    fields: Option<Fields>,
    /// The card the current inline error belongs to (`dictionary_error_card`).
    error_card: Option<DictionaryCard>,
}

type Shared = Rc<RefCell<State>>;

/// The Dictionary page.
#[derive(Default)]
pub struct DictionaryPage {
    state: Shared,
}

impl DictionaryPage {
    pub fn new() -> Self {
        DictionaryPage::default()
    }
}

/// `_preserve_page_state` for Dictionary: read the live fields into the draft.
/// No `RefCell` borrow is held while AppKit is called.
fn pull(state: &RefCell<State>) {
    let controls = state
        .borrow()
        .fields
        .as_ref()
        .map(|f| (f.word.clone(), f.from.clone(), f.to.clone()));
    if let Some((word, from, to)) = controls {
        let (word, from, to) = (
            word.stringValue().to_string(),
            from.stringValue().to_string(),
            to.stringValue().to_string(),
        );
        let mut state = state.borrow_mut();
        state.draft.word = word;
        state.draft.source = from;
        state.draft.target = to;
    }
}

/// `_sync_dictionary_buttons`.
fn sync_buttons(state: &RefCell<State>) {
    let pair = {
        let state = state.borrow();
        state.fields.as_ref().map(|f| {
            (
                f.add_word.clone(),
                f.add_replacement.clone(),
                state.draft.word_button_title(),
                state.draft.replacement_button_title(),
            )
        })
    };
    if let Some((add_word, add_replacement, word_title, replacement_title)) = pair {
        add_word.setTitle(&NSString::from_str(word_title));
        add_replacement.setTitle(&NSString::from_str(replacement_title));
    }
}

/// What a reload changes (`_reload_dictionary` arguments).
#[derive(Default)]
struct Reload {
    error: Option<String>,
    card: Option<DictionaryCard>,
    clear: Option<DictionaryCard>,
    forget: Option<(String, Option<EntryKind>)>,
    notice: Option<String>,
}

/// `_reload_dictionary`: keep what was typed and the editing ids, apply the
/// outcome, then rebuild. On failure `error` is shown beside `card`; success
/// clears the error and notice, clears the submitted card's fields and drops
/// the editing id of an entry that no longer exists.
fn reload(state: &Shared, services: &HubServices, rebuild: &dyn Fn(), change: Reload) {
    pull(state);
    {
        let mut state = state.borrow_mut();
        match change.error {
            Some(error) => {
                services.set_page_error(PAGE, &error);
                state.error_card = change.card;
            }
            None => {
                services.clear_page_error(PAGE);
                services.page_notices.borrow_mut().remove(PAGE);
                state.error_card = None;
                let forget = change
                    .forget
                    .as_ref()
                    .map(|(key, kind)| (key.as_str(), *kind));
                state.draft.after_success(change.clear, forget);
                if let Some(notice) = change.notice {
                    services
                        .page_notices
                        .borrow_mut()
                        .insert(PAGE.to_string(), notice);
                }
            }
        }
    }
    rebuild();
}

fn add_word(state: &Shared, services: &HubServices, rebuild: &dyn Fn()) {
    pull(state);
    let (text, editing) = {
        let state = state.borrow();
        (state.draft.word.clone(), state.draft.editing_word.clone())
    };
    let error = submit_word(&*services.dictionary, &text, editing.as_deref());
    reload(
        state,
        services,
        rebuild,
        Reload {
            error,
            card: Some(DictionaryCard::Word),
            clear: Some(DictionaryCard::Word),
            ..Reload::default()
        },
    );
}

fn add_replacement(state: &Shared, services: &HubServices, rebuild: &dyn Fn()) {
    pull(state);
    let (source, target, editing) = {
        let state = state.borrow();
        (
            state.draft.source.clone(),
            state.draft.target.clone(),
            state.draft.editing_source.clone(),
        )
    };
    let error = submit_replacement(&*services.dictionary, &source, &target, editing.as_deref());
    reload(
        state,
        services,
        rebuild,
        Reload {
            error,
            card: Some(DictionaryCard::Replacement),
            clear: Some(DictionaryCard::Replacement),
            ..Reload::default()
        },
    );
}

fn remove(state: &Shared, services: &HubServices, rebuild: &dyn Fn(), row: &DictionaryRow) {
    let error = remove_entry(&*services.dictionary, &row.key, Some(row.kind));
    reload(
        state,
        services,
        rebuild,
        Reload {
            error,
            forget: Some((row.key.clone(), Some(row.kind))),
            ..Reload::default()
        },
    );
}

/// `editDictionary_`: load the row into its card's fields; no rebuild.
fn edit(state: &Shared, row: &DictionaryRow) {
    state.borrow_mut().draft.begin_edit(row);
    let controls = state
        .borrow()
        .fields
        .as_ref()
        .map(|f| (f.word.clone(), f.from.clone(), f.to.clone()));
    if let Some((word, from, to)) = controls {
        match row.kind {
            EntryKind::Word => word.setStringValue(&NSString::from_str(&row.key)),
            EntryKind::Replacement => {
                from.setStringValue(&NSString::from_str(&row.key));
                to.setStringValue(&NSString::from_str(&row.target));
            }
        }
    }
    sync_buttons(state);
}

/// `importDictionary_`: a real open panel for CSV/JSON, then the store's import.
fn import(
    state: &Shared,
    services: &HubServices,
    rebuild: &dyn Fn(),
    mtm: objc2::MainThreadMarker,
) {
    let panel = NSOpenPanel::openPanel(mtm);
    let types =
        NSArray::from_retained_slice(&[NSString::from_str("csv"), NSString::from_str("json")]);
    panel.setAllowedFileTypes(Some(&types));
    if panel.runModal() != NSModalResponseOK {
        return;
    }
    let path = panel
        .URL()
        .and_then(|url| url.path())
        .map(|path| path.to_string());
    let imported = path
        .ok_or_else(|| anyhow::anyhow!("the chosen file has no path"))
        .and_then(|path| services.dictionary.import_file(Path::new(&path)));
    let change = match imported {
        Ok(counts) => Reload {
            notice: Some(dictionary_import_notice(&counts)),
            ..Reload::default()
        },
        Err(_) => Reload {
            error: Some(DICTIONARY_IMPORT_FAILED.to_string()),
            ..Reload::default()
        },
    };
    reload(state, services, rebuild, change);
}

/// `_card_label`: a label inside a card at the card padding.
fn card_label(ctx: &PageCtx, card: &FlippedView, text: &str, y: f64, style: LabelStyle) {
    let width = card.frame().size.width - 2.0 * f64::from(CARD_PADDING);
    card.addSubview(&label(
        ctx.mtm,
        text,
        f64::from(CARD_PADDING),
        y,
        width,
        style,
        &ctx.palette,
    ));
}

/// `_dictionary_card`: a titled card at `y`; returns it.
fn dictionary_card(
    ctx: &PageCtx,
    y: f64,
    title: &str,
    detail: &str,
    height: f64,
) -> Retained<FlippedView> {
    let card = FlippedView::new(ctx.mtm, rect(0.0, y, ctx.width, height));
    style_surface(&card, &ctx.palette, &SurfaceStyle::default());
    ctx.body.addSubview(&card);
    card_label(
        ctx,
        &card,
        title,
        f64::from(CARD_PADDING),
        LabelStyle {
            size: 15.0,
            weight: Weight::Semibold,
            height: 22.0,
            ..LabelStyle::default()
        },
    );
    card_label(
        ctx,
        &card,
        detail,
        f64::from(CARD_PADDING) + 24.0,
        LabelStyle {
            size: 12.0,
            color: "muted",
            height: 18.0,
            ..LabelStyle::default()
        },
    );
    card
}

fn divider(ctx: &PageCtx, y: f64, width: f64, palette: &Palette) -> Retained<NSView> {
    let line = FlippedView::new(ctx.mtm, rect(12.0, y, width - 24.0, 1.0));
    line.setWantsLayer(true);
    if let Some(layer) = line.layer() {
        layer.setBackgroundColor(Some(&color("line", palette).CGColor()));
    }
    // Decorative.
    line.setAccessibilityElement(false);
    Retained::into_super(line)
}

impl HubPage for DictionaryPage {
    fn name(&self) -> &'static str {
        PAGE
    }

    fn capture(&mut self) {
        pull(&self.state);
    }

    fn shutdown(&mut self) {
        // Drop the control references; there is nothing to write (every
        // action saves immediately).
        self.state.borrow_mut().fields = None;
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        let mtm = ctx.mtm;
        let p = ctx.palette;
        let width = ctx.width;
        let inner = width - 2.0 * f64::from(CARD_PADDING);
        let pad = f64::from(CARD_PADDING);
        let mut cursor = top;
        let weak: Weak<RefCell<State>> = Rc::downgrade(&self.state);

        // Handlers: each holds a Weak state and the shared service handles.
        let make = |run: fn(&Shared, &HubServices, &dyn Fn())| {
            let weak = weak.clone();
            let services = ctx.services.clone();
            let rebuild = ctx.rebuild.clone();
            move || {
                if let Some(state) = weak.upgrade() {
                    run(&state, &services, &*rebuild);
                }
            }
        };

        let error = ctx.services.page_error(PAGE);
        let error_card = if error.is_some() {
            self.state.borrow().error_card
        } else {
            None
        };
        if let (Some(error), None) = (&error, error_card) {
            // Not tied to a card (import, load): shown once, under the title.
            ctx.body.addSubview(&label(
                mtm,
                error,
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
        let card_error =
            |card: DictionaryCard| error.as_deref().filter(|_| error_card == Some(card));

        // ---- words card
        let word_card = dictionary_card(
            ctx,
            cursor,
            "Words LocalFlow should know",
            "Names, products and jargon that dictation should spell your way.",
            118.0
                + if card_error(DictionaryCard::Word).is_some() {
                    22.0
                } else {
                    0.0
                },
        );
        cursor += word_card.frame().size.height + 12.0;
        let add_width = 120.0;
        let word = field(mtm, "", pad, 70.0, inner - add_width - 12.0, "Word");
        word.setPlaceholderString(Some(&NSString::from_str("e.g. Kubernetes")));
        word_card.addSubview(&word);
        let add_word = button(
            mtm,
            "Add word",
            width - pad - add_width,
            68.0,
            add_width,
            36.0,
            ButtonKind::Primary,
            &p,
        );
        word_card.addSubview(&add_word);
        if let Some(message) = card_error(DictionaryCard::Word) {
            card_label(
                ctx,
                &word_card,
                message,
                108.0,
                LabelStyle {
                    size: 12.0,
                    color: "red",
                    height: 20.0,
                    ..LabelStyle::default()
                },
            );
        }

        // ---- auto-replace card
        let replace_card = dictionary_card(
            ctx,
            cursor,
            "Auto-replace",
            "When you say one thing, LocalFlow types another.",
            138.0
                + if card_error(DictionaryCard::Replacement).is_some() {
                    22.0
                } else {
                    0.0
                },
        );
        cursor += replace_card.frame().size.height + 12.0;
        let button_width = 160.0;
        let arrow_width = 28.0;
        let field_width = ((inner - button_width - 12.0 - arrow_width - 16.0) / 2.0).floor();
        let second_x = pad + field_width + 8.0 + arrow_width + 8.0;
        let caption = LabelStyle {
            size: 11.0,
            height: 18.0,
            weight: Weight::Semibold,
            ..LabelStyle::default()
        };
        replace_card.addSubview(&label(
            mtm,
            "When I say",
            pad,
            66.0,
            field_width,
            caption,
            &p,
        ));
        replace_card.addSubview(&label(
            mtm,
            "Type this",
            second_x,
            66.0,
            field_width,
            caption,
            &p,
        ));
        let from = field(mtm, "", pad, 88.0, field_width, "When I say");
        from.setPlaceholderString(Some(&NSString::from_str("e.g. my email")));
        replace_card.addSubview(&from);
        let arrow = label(
            mtm,
            "→",
            pad + field_width + 8.0,
            92.0,
            arrow_width,
            LabelStyle {
                size: 15.0,
                color: "muted",
                height: 24.0,
                ..LabelStyle::default()
            },
            &p,
        );
        arrow.setAccessibilityElement(false); // decorative
        replace_card.addSubview(&arrow);
        let to = field(mtm, "", second_x, 88.0, field_width, "Type this");
        to.setPlaceholderString(Some(&NSString::from_str("e.g. me@example.com")));
        replace_card.addSubview(&to);
        let add_replacement = button(
            mtm,
            "Add replacement",
            width - pad - button_width,
            86.0,
            button_width,
            36.0,
            ButtonKind::Primary,
            &p,
        );
        replace_card.addSubview(&add_replacement);
        if let Some(message) = card_error(DictionaryCard::Replacement) {
            card_label(
                ctx,
                &replace_card,
                message,
                126.0,
                LabelStyle {
                    size: 12.0,
                    color: "red",
                    height: 20.0,
                    ..LabelStyle::default()
                },
            );
        }

        // ---- actions wiring
        let submit_word_handler = make(add_word_run);
        ctx.target.on_action(&add_word, submit_word_handler);
        let submit_word_enter = make(add_word_run);
        ctx.target.on_action(&word, submit_word_enter);
        let submit_replacement_click = make(add_replacement_run);
        ctx.target
            .on_action(&add_replacement, submit_replacement_click);
        let submit_replacement_from = make(add_replacement_run);
        ctx.target.on_action(&from, submit_replacement_from);
        let submit_replacement_to = make(add_replacement_run);
        ctx.target.on_action(&to, submit_replacement_to);

        // ---- import
        let import_button = button(
            mtm,
            "Import CSV/JSON…",
            0.0,
            cursor,
            170.0,
            36.0,
            ButtonKind::Secondary,
            &p,
        );
        {
            let weak = weak.clone();
            let services = ctx.services.clone();
            let rebuild = ctx.rebuild.clone();
            ctx.target.on_action(&import_button, move || {
                if let Some(state) = weak.upgrade() {
                    import(&state, &services, &*rebuild, mtm);
                }
            });
        }
        ctx.body.addSubview(&import_button);
        cursor += 48.0;

        // ---- notice banner
        if let Some(notice) = ctx.services.page_notices.borrow().get(PAGE).cloned() {
            let banner = FlippedView::new(mtm, rect(0.0, cursor, width, 36.0));
            banner.setWantsLayer(true);
            if let Some(layer) = banner.layer() {
                layer.setBackgroundColor(Some(&color("sun", &p).CGColor()));
            }
            banner.setAccessibilityElement(false);
            ctx.body.addSubview(&banner);
            ctx.body.addSubview(&label(
                mtm,
                &notice,
                10.0,
                cursor + 8.0,
                width - 20.0,
                LabelStyle {
                    size: 11.0,
                    color: SUN_TEXT,
                    height: 20.0,
                    ..LabelStyle::default()
                },
                &p,
            ));
            cursor += 44.0;
        }

        // ---- list
        ctx.body.addSubview(&label(
            mtm,
            "Your dictionary",
            0.0,
            cursor,
            width,
            LabelStyle {
                size: 15.0,
                weight: Weight::Semibold,
                height: 22.0,
                ..LabelStyle::default()
            },
            &p,
        ));
        cursor += 22.0 + 8.0;

        let rows = dictionary_entries(&ctx.services.dictionary.list());
        let listing_height = if rows.is_empty() {
            220.0
        } else {
            rows.len() as f64 * ROW_HEIGHT
        };
        let results = FlippedView::new(mtm, rect(0.0, 0.0, width, listing_height));
        results.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        style_surface(&results, &p, &SurfaceStyle::default());
        if rows.is_empty() {
            // `_empty_state`: title, wrapped detail and an action focusing the word field.
            let w = width - 48.0;
            results.addSubview(&label(
                mtm,
                "No vocabulary yet",
                24.0,
                58.0,
                w,
                LabelStyle {
                    size: 15.0,
                    height: 28.0,
                    weight: Weight::Semibold,
                    ..LabelStyle::default()
                },
                &p,
            ));
            results.addSubview(&label(
                mtm,
                "Add a word or replacement to personalize dictation.",
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
                &p,
            ));
            let focus = button(
                mtm,
                "Add a word",
                24.0,
                140.0,
                138.0,
                36.0,
                ButtonKind::Secondary,
                &p,
            );
            let word_ref = word.clone();
            ctx.target.on_action(&focus, move || {
                if let Some(window) = word_ref.window() {
                    window.makeFirstResponder(Some(&word_ref));
                }
            });
            results.addSubview(&focus);
        }
        for (index, row) in rows.iter().enumerate() {
            let y = index as f64 * ROW_HEIGHT;
            results.addSubview(&label(
                mtm,
                &row.label,
                14.0,
                y + 17.0,
                width - 214.0,
                LabelStyle {
                    size: 13.0,
                    height: 20.0,
                    ..LabelStyle::default()
                },
                &p,
            ));
            let edit_button = button(
                mtm,
                "Edit",
                width - 180.0,
                y + 10.0,
                72.0,
                36.0,
                ButtonKind::Ghost,
                &p,
            );
            edit_button
                .setAccessibilityLabel(Some(&NSString::from_str(&format!("Edit {}", row.label))));
            {
                let weak = weak.clone();
                let row = row.clone();
                ctx.target.on_action(&edit_button, move || {
                    if let Some(state) = weak.upgrade() {
                        edit(&state, &row);
                    }
                });
            }
            results.addSubview(&edit_button);
            let remove_button = button(
                mtm,
                "Remove",
                width - 100.0,
                y + 10.0,
                88.0,
                36.0,
                ButtonKind::DestructiveGhost,
                &p,
            );
            remove_button
                .setAccessibilityLabel(Some(&NSString::from_str(&format!("Remove {}", row.label))));
            {
                let weak = weak.clone();
                let services = ctx.services.clone();
                let rebuild = ctx.rebuild.clone();
                let row = row.clone();
                ctx.target.on_action(&remove_button, move || {
                    if let Some(state) = weak.upgrade() {
                        remove(&state, &services, &*rebuild, &row);
                    }
                });
            }
            results.addSubview(&remove_button);
            if index + 1 < rows.len() {
                results.addSubview(&divider(
                    ctx,
                    (index as f64 + 1.0) * ROW_HEIGHT - 1.0,
                    width,
                    &p,
                ));
            }
        }
        let available = ctx.viewport_height - cursor - f64::from(HUB_PAGE_BOTTOM);
        let scroll_height = listing_height.min(available.max(168.0));
        let scroll = NSScrollView::initWithFrame(
            mtm.alloc::<NSScrollView>(),
            rect(0.0, cursor, width, scroll_height),
        );
        scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        scroll.setDocumentView(Some(&results));
        scroll.setHasVerticalScroller(listing_height > scroll_height);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        ctx.body.addSubview(&scroll);
        cursor += scroll_height;

        // ---- restore the draft (drafts survive rebuilds)
        {
            let draft = self.state.borrow().draft.clone();
            word.setStringValue(&NSString::from_str(&draft.word));
            from.setStringValue(&NSString::from_str(&draft.source));
            to.setStringValue(&NSString::from_str(&draft.target));
        }
        self.state.borrow_mut().fields = Some(Fields {
            word,
            from,
            to,
            add_word,
            add_replacement,
        });
        sync_buttons(&self.state);
        cursor
    }
}

fn add_word_run(state: &Shared, services: &HubServices, rebuild: &dyn Fn()) {
    add_word(state, services, rebuild);
}

fn add_replacement_run(state: &Shared, services: &HubServices, rebuild: &dyn Fn()) {
    add_replacement(state, services, rebuild);
}
