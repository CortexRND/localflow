//! Snippets (route "Hot Phrases"): the existing `HotPhrasesEditor` holds all
//! behaviour (validation, 0.6 s autosave, undo delete, external changes);
//! this page renders it and feeds it input.

use std::time::Duration;

use gpui::{div, prelude::*, px, Context, Entity, FontWeight, Window};
use lf_core::phrases_model::{PhrasesModel, PhrasesSettings};
use lf_ui::hot_phrases_editor::{self as hpe, HotPhrasesEditor};
use serde_json::json;

use crate::app::PageCx;
use crate::pages::{header, page, split};
use crate::theme::{size, space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::{self, Button, Switch};

pub struct SnippetsPage {
    pcx: PageCx,
    editor: Result<HotPhrasesEditor<PhrasesModel>, String>,
    search: Entity<TextInput>,
    trigger: Entity<TextInput>,
    text: Entity<TextInput>,
    test: Entity<TextInput>,
    active: bool,
    undo_shown: bool,
    settings: PhrasesSettings,
}

impl SnippetsPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new(cx, "Search snippets"));
        let trigger =
            cx.new(|cx| TextInput::new(cx, "What you say, e.g. “my address”").label("Trigger"));
        let text = cx.new(|cx| {
            TextInput::new(cx, "The text LocalFlow types for you")
                .multiline(4, 14)
                .label("Text")
        });
        let test = cx.new(|cx| TextInput::new(cx, "Type what you'd say, e.g. “send my address”"));
        cx.subscribe(&search, |this, input, e, cx| {
            if *e == InputEvent::Changed {
                let q = input.read(cx).text().to_string();
                if let Ok(ed) = this.editor.as_mut() {
                    ed.set_query(&q);
                }
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&trigger, |this, input, e, cx| {
            if *e == InputEvent::Changed {
                let v = input.read(cx).text().to_string();
                if let Ok(ed) = this.editor.as_mut() {
                    ed.edit(Some(&v), None, None);
                }
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&text, |this, input, e, cx| {
            if *e == InputEvent::Changed {
                let v = input.read(cx).text().to_string();
                if let Ok(ed) = this.editor.as_mut() {
                    ed.edit(None, Some(&v), None);
                }
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&test, |this, input, e, cx| {
            if *e == InputEvent::Changed {
                let v = input.read(cx).text().to_string();
                if let Ok(ed) = this.editor.as_mut() {
                    ed.set_test_input(&v);
                }
                cx.notify();
            }
        })
        .detach();
        let editor = Self::open_editor(&pcx);
        let settings = PhrasesSettings::from_config(&pcx.services.config());
        let mut page = SnippetsPage {
            settings,
            pcx,
            editor,
            search,
            trigger,
            text,
            test,
            active: false,
            undo_shown: false,
        };
        page.show(cx);
        // The editor's 200 ms tick (autosave, undo expiry, disk poll).
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let alive = this.update(cx, |this, cx| {
                if !this.active {
                    return;
                }
                let changed = this.editor.as_mut().map(|e| e.tick()).unwrap_or(false);
                if changed {
                    this.sync_fields(cx);
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        })
        .detach();
        page
    }

    fn open_editor(pcx: &PageCx) -> Result<HotPhrasesEditor<PhrasesModel>, String> {
        pcx.services
            .phrases_model()
            .map(HotPhrasesEditor::new)
            .map_err(|e| format!("{} ({e:#})", hpe::READ_ERROR))
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // Settings (feature switch, spoken punctuation...) feed the model.
        let pending = self.editor.as_ref().map(|e| e.pending()).unwrap_or(false);
        let settings = PhrasesSettings::from_config(&self.pcx.services.config());
        if !pending && (settings != self.settings || self.editor.is_err()) {
            self.settings = settings;
            self.editor = Self::open_editor(&self.pcx);
            self.fields_stale();
        }
        if self.active {
            if let Ok(ed) = self.editor.as_mut() {
                ed.on_show();
            }
        }
        self.sync_fields(cx);
        cx.notify();
    }

    fn fields_stale(&mut self) {
        if let Ok(ed) = self.editor.as_mut() {
            ed.fields_stale = true;
        }
    }

    pub fn show(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        if let Ok(ed) = self.editor.as_mut() {
            ed.on_show();
        }
        self.sync_fields(cx);
        cx.notify();
    }

    /// Navigation away: flush; refuse while the snippet can't be saved.
    pub fn leave(&mut self, cx: &mut Context<Self>) -> bool {
        let ok = self.editor.as_mut().map(|e| e.leave()).unwrap_or(true);
        if ok {
            self.active = false;
        }
        cx.notify();
        ok
    }

    /// Window close / quit: commit the snippet being edited now (no
    /// debounce). `Err` explains what was kept unsaved.
    pub fn flush_for_close(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let result = match self.editor.as_mut() {
            Ok(ed) => crate::controllers::shutdown::flush_snippets(ed),
            Err(_) => Ok(()),
        };
        cx.notify();
        result
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.read(cx).focus(window);
    }

    pub fn new_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Ok(ed) = self.editor.as_mut() {
            if ed.new_phrase() {
                self.sync_fields(cx);
                self.trigger.read(cx).focus(window);
            }
        }
        cx.notify();
    }

    /// Push editor values into the inputs after programmatic loads only.
    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        let Ok(ed) = self.editor.as_mut() else { return };
        if ed.fields_stale {
            let (tr, tx) = (ed.trigger.clone(), ed.text.clone());
            ed.ack_fields();
            self.trigger.update(cx, |i, cx| i.set_text(tr, cx));
            self.text.update(cx, |i, cx| i.set_text(tx, cx));
        }
        let undo = ed.undo_token.is_some();
        if undo && !self.undo_shown {
            let label = ed.undo_label();
            let this = cx.entity().downgrade();
            self.pcx.toast_with(
                label,
                "Undo",
                move |_, cx| {
                    let _ = this.update(cx, |p, cx| {
                        if let Ok(ed) = p.editor.as_mut() {
                            ed.undo_delete();
                        }
                        p.undo_shown = false;
                        p.sync_fields(cx);
                        cx.notify();
                    });
                },
                cx,
            );
        }
        self.undo_shown = undo;
    }

    fn with_editor(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut HotPhrasesEditor<PhrasesModel>) -> bool,
    ) {
        if let Ok(ed) = self.editor.as_mut() {
            f(ed);
        }
        self.sync_fields(cx);
        cx.notify();
    }

    fn enable_feature(&mut self, cx: &mut Context<Self>) {
        match self
            .pcx
            .services
            .apply(&json!({"features": {"hot_phrases": true}}))
        {
            Ok(_) => {
                self.pcx.toast("Snippets turned on", cx);
                self.pcx.config_changed(cx);
                self.refresh(cx);
            }
            Err(e) => self.pcx.toast(format!("Couldn't save: {e:#}"), cx),
        }
    }

    fn render_list(
        &self,
        ed: &HotPhrasesEditor<PhrasesModel>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut list = div().flex().flex_col();
        let rows = ed.rows();
        if rows.is_empty() && ed.no_matches() {
            list = list.child(
                div()
                    .p(space::s6())
                    .child(ui::secondary(&t, "No snippets match your search.")),
            );
        }
        for row in rows {
            let id = row.id.clone();
            list = list.child(
                div()
                    .id(gpui::ElementId::Name(format!("snippet-{}", row.id).into()))
                    .relative()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .px(space::s6())
                    .py(space::s4())
                    .border_b_1()
                    .border_color(t.border)
                    .tab_index(0)
                    .cursor_pointer()
                    .when(row.selected, |d| d.bg(t.selected))
                    .when(!row.selected, |d| d.hover(|s| s.bg(t.hover)))
                    .focus(|s| s.shadow(ui::ring_over(&t, t.bg_surface)))
                    .when(row.selected, |d| {
                        d.child(
                            div()
                                .absolute()
                                .left(px(0.))
                                .top(px(0.))
                                .bottom(px(0.))
                                .w(px(2.))
                                .bg(t.accent),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.with_editor(cx, |ed| ed.select(&id));
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .when(row.selected, |d| d.font_weight(FontWeight::MEDIUM))
                                    .child(row.title.clone()),
                            )
                            .child(
                                ui::supporting(&t, row.subtitle.clone())
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis(),
                            ),
                    )
                    .when(!row.enabled, |d| {
                        d.child(ui::token(&t, Tone::Neutral, "Off"))
                    }),
            );
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .p(space::s6())
                    .child(ui::icon("search", t.icon_secondary))
                    .child(self.search.clone()),
            )
            .child(
                div()
                    .id("snippet-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
    }

    fn render_detail(
        &self,
        ed: &HotPhrasesEditor<PhrasesModel>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme(cx).clone();
        if ed.empty_state_visible() {
            return div()
                .id("snippet-detail")
                .size_full()
                .child(ui::empty_state(
                    &t,
                    "zap",
                    "No snippets yet",
                    hpe::EMPTY_MESSAGE,
                    Some(
                        Button::new("add-example", "Add an example")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.with_editor(cx, |ed| ed.add_example())
                            }))
                            .into_any_element(),
                    ),
                ));
        }
        if !ed.editor_visible() {
            return div()
                .id("snippet-detail")
                .size_full()
                .child(ui::empty_state(
                    &t,
                    "zap",
                    "Select a snippet",
                    "Or create a new one.",
                    None,
                ));
        }
        let (trigger_hint, trigger_bad) = ed.trigger_hint();
        let (text_hint, text_bad) = ed.text_hint();
        let (output, status) = ed.preview();
        let enabled = ed.enabled;
        let this = cx.entity().downgrade();
        let save_state = if !ed.error.is_empty() {
            ui::error_line(&t, ed.error.clone())
        } else if ed.pending() {
            ui::supporting(&t, "Saving…")
        } else if !ed.notice.is_empty() {
            ui::supporting(&t, ed.notice.clone())
        } else {
            ui::supporting(&t, "All changes saved")
        };
        let hint = |msg: String, bad: bool| {
            if bad {
                ui::error_line(&t, msg)
            } else {
                ui::supporting(&t, msg)
            }
        };
        div()
            .id("snippet-detail")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s6())
                    .p(space::s10())
                    .max_w(px(size::READING + 64.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(save_state)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(space::s4())
                                    .child(ui::label(&t, hpe::enabled_label(enabled)))
                                    .child(
                                        Switch::new(
                                            "snippet-enabled",
                                            enabled,
                                            hpe::ENABLED_ACCESSIBILITY,
                                        )
                                        .on_toggle(
                                            move |on, _, cx| {
                                                let _ = this.update(cx, |p, cx| {
                                                    p.with_editor(cx, |ed| ed.set_enabled(on))
                                                });
                                            },
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s2())
                            .child(ui::label(&t, "Trigger"))
                            .child(self.trigger.clone())
                            .child(hint(trigger_hint, trigger_bad)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s2())
                            .child(ui::label(&t, "Text"))
                            .child(self.text.clone())
                            .child(hint(text_hint, text_bad)),
                    )
                    .child(
                        div().flex().gap(space::s4()).child(
                            Button::new("delete-snippet", "Delete")
                                .ghost()
                                .icon("trash-2")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.with_editor(cx, |ed| ed.delete())
                                })),
                        ),
                    )
                    .child(ui::divider(&t))
                    .child(
                        ui::section(
                            &t,
                            "Try it",
                            Some("See what dictation would type. Nothing is saved.".into()),
                            None,
                        )
                        .child(self.test.clone())
                        .child(ui::supporting(&t, status))
                        .child(
                            div()
                                .p(space::s6())
                                .rounded(size::radius_element())
                                .bg(t.bg_muted)
                                .child(if output.is_empty() {
                                    ui::secondary(
                                        &t,
                                        hpe::preview_placeholder(&ed.test_input, &output, ""),
                                    )
                                } else {
                                    ui::reading(&t, output)
                                }),
                        ),
                    ),
            )
    }
}

impl Render for SnippetsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let new_btn = Button::new("new-snippet", "New snippet")
            .primary()
            .icon("plus")
            .on_click(cx.listener(|this, _, window, cx| this.new_snippet(window, cx)))
            .into_any_element();
        let head = header(
            &t,
            "Snippets",
            Some("Also called hot phrases. Say a trigger, get saved text.".into()),
            vec![new_btn],
        );
        let editor = std::mem::replace(&mut self.editor, Err(String::new()));
        let body = match &editor {
            Err(message) => {
                div()
                    .p(space::s10())
                    .child(ui::banner(&t, Tone::Error, message.clone(), None))
            }
            Ok(ed) => {
                let banner = (!ed.banner().is_empty()).then(|| {
                    div().px(space::s10()).pt(space::s5()).child(ui::banner(
                        &t,
                        Tone::Info,
                        ed.banner(),
                        Some(
                            Button::new("enable-snippets", "Turn on snippets")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.enable_feature(cx)))
                                .into_any_element(),
                        ),
                    ))
                });
                let list = self.render_list(ed, cx);
                let detail = self.render_detail(ed, cx);
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .children(banner)
                    .child(split(&t, list, detail))
            }
        };
        self.editor = editor;
        page(head, body)
    }
}
