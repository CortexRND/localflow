//! History: search (debounced, background), continuous paging, detail pane,
//! copy final/original, delete and clear with confirmation.

use std::time::Duration;

use gpui::{
    div, prelude::*, px, ClipboardItem, Context, Entity, FocusHandle, FontWeight, KeyDownEvent,
    ScrollHandle, Window,
};
use lf_core::history::HistoryEntry;
use lf_ui::hub_model;

use crate::app::{Confirm, PageCx};
use crate::controllers::history::{self, Loaded};
use crate::page::Page;
use crate::pages::{header, page, split};
use crate::theme::{size, space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::{self, Button};

pub struct HistoryPage {
    pcx: PageCx,
    search: Entity<TextInput>,
    /// Backend pages and the detail-only cited entry.
    list: history::Listing,
    loading: bool,
    error: Option<String>,
    selected: Option<i64>,
    pending_select: Option<i64>,
    generation: u64,
    debounce: u64,
    show_original: bool,
    list_focus: FocusHandle,
    scroll: ScrollHandle,
    total: Option<i64>,
}

impl HistoryPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| TextInput::new(cx, "Search your dictations").label("Search history"));
        cx.subscribe(&search, |this, _, event, cx| match event {
            InputEvent::Changed => this.schedule_search(cx),
            InputEvent::Submit => this.load(0, cx),
        })
        .detach();
        let mut page = HistoryPage {
            pcx,
            search,
            list: history::Listing::default(),
            loading: true,
            error: None,
            selected: None,
            pending_select: None,
            generation: 0,
            debounce: 0,
            show_original: false,
            list_focus: cx.focus_handle().tab_stop(true),
            scroll: ScrollHandle::new(),
            total: None,
        };
        page.load(0, cx);
        page
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(0, cx);
    }

    fn query(&self, cx: &Context<Self>) -> String {
        self.search.read(cx).text().to_string()
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.debounce += 1;
        let ticket = self.debounce;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.debounce == ticket {
                    this.load(0, cx);
                }
            });
        })
        .detach();
    }

    /// Load from `offset` (0 replaces the list; otherwise appends).
    fn load(&mut self, offset: i64, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let query = self.query(cx);
        let root = self.pcx.services.root.clone();
        self.loading = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { history::load(&root, generation, &query, offset, offset == 0) })
                .await;
            let _ = this.update(cx, |this, cx| this.loaded(result, cx));
        })
        .detach();
        cx.notify();
    }

    fn loaded(&mut self, result: anyhow::Result<Loaded>, cx: &mut Context<Self>) {
        let Ok(loaded) = result else {
            if let Err(err) = result {
                self.error = Some(format!("Couldn't read your history: {err:#}"));
            }
            self.loading = false;
            cx.notify();
            return;
        };
        if loaded.generation != self.generation {
            return; // stale
        }
        self.loading = false;
        self.error = None;
        if let Some(stats) = &loaded.stats {
            self.total = Some(stats.total_dictations);
        }
        self.list.apply(loaded);
        if let Some(id) = self.pending_select.take() {
            if self.list.in_pages(id) {
                self.selected = Some(id);
            } else if let Ok(Some(entry)) = self.pcx.services.history().and_then(|h| h.get(id)) {
                // Detail only: the page cursor is untouched.
                self.list.cite(entry);
                self.selected = Some(id);
            }
        }
        if self.selected.is_some_and(|id| self.list.find(id).is_none()) {
            self.selected = None;
        }
        cx.notify();
    }

    pub fn select_id(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.list.find(id).is_some() {
            self.selected = Some(id);
            self.show_original = false;
            cx.notify();
        } else {
            self.pending_select = Some(id);
            self.load(0, cx);
        }
    }

    /// The list's focus handle (what a row click focuses), for shell tests.
    #[cfg(test)]
    pub(crate) fn list_focus(&self) -> FocusHandle {
        self.list_focus.clone()
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.read(cx).focus(window);
    }

    pub fn clear_search(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if !self.search.read(cx).is_empty() {
            self.search.update(cx, |s, cx| s.set_text("", cx));
            self.load(0, cx);
        }
    }

    fn selected_entry(&self) -> Option<&HistoryEntry> {
        self.selected.and_then(|id| self.list.find(id))
    }

    fn copy(&self, text: String, what: &str, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.pcx.toast(format!("Copied {what}"), cx);
    }

    fn ask_delete(&mut self, id: i64, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        self.pcx.confirm(
            Confirm {
                title: "Delete this dictation?".into(),
                body: "It's removed from your history on this device. This can't be undone.".into(),
                verb: "Delete".into(),
                destructive: true,
                on_confirm: std::rc::Rc::new(move |_, cx| {
                    let _ = this.update(cx, |p, cx| p.delete(id, cx));
                }),
            },
            cx,
        );
    }

    fn delete(&mut self, id: i64, cx: &mut Context<Self>) {
        match history::delete(&self.pcx.services.root, id) {
            Ok(_) => {
                let index = self.list.remove(id);
                self.selected = index
                    .and_then(|i| {
                        self.list
                            .entries
                            .get(i.min(self.list.entries.len().saturating_sub(1)))
                    })
                    .map(|e| e.id);
                self.total = self.total.map(|n| (n - 1).max(0));
                self.pcx.toast("Dictation deleted", cx);
            }
            Err(err) => self.pcx.toast(format!("Couldn't delete: {err:#}"), cx),
        }
        cx.notify();
    }

    fn ask_clear(&mut self, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        self.pcx.confirm(
            Confirm {
                title: "Clear all history?".into(),
                body: format!(
                    "{} Every dictation and stored audio clip on this device is deleted. This can't be undone.",
                    hub_model::HISTORY_CLEAR_CONFIRM
                )
                .into(),
                verb: "Clear history".into(),
                destructive: true,
                on_confirm: std::rc::Rc::new(move |_, cx| {
                    let _ = this.update(cx, |p, cx| {
                        match history::clear(&p.pcx.services.root) {
                            Ok(n) => p.pcx.toast(format!("Cleared {n} dictations"), cx),
                            Err(err) => p.pcx.toast(format!("Couldn't clear history: {err:#}"), cx),
                        }
                        p.selected = None;
                        p.load(0, cx);
                    });
                }),
            },
            cx,
        );
    }

    fn on_list_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let index = self
            .selected
            .and_then(|id| self.list.entries.iter().position(|e| e.id == id));
        let n = self.list.entries.len();
        if n == 0 {
            return;
        }
        let next = match ks.key.as_str() {
            "down" => Some(index.map_or(0, |i| (i + 1).min(n - 1))),
            "up" => Some(index.map_or(0, |i| i.saturating_sub(1))),
            "home" => Some(0),
            "end" => Some(n - 1),
            "pagedown" => Some(index.map_or(0, |i| (i + 10).min(n - 1))),
            "pageup" => Some(index.map_or(0, |i| i.saturating_sub(10))),
            "delete" => {
                if let Some(id) = self.selected {
                    self.ask_delete(id, cx);
                }
                None
            }
            "c" if ks.modifiers.secondary() => {
                if let Some(e) = self.selected_entry() {
                    self.copy(e.final_text.clone(), "text", cx);
                }
                None
            }
            _ => return,
        };
        if let Some(i) = next {
            self.selected = Some(self.list.entries[i].id);
            self.show_original = false;
            if i + 1 >= n && self.list.more && !self.loading {
                let offset = self.list.next_offset();
                self.load(offset, cx);
            }
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn render_list(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let now = history::now();
        let searching = !self.search.read(cx).is_empty();
        let focused = self.list_focus.is_focused(window);
        let mut rows = div().flex().flex_col();
        if self.list.entries.is_empty() && !self.loading {
            let (h, d, button, _) = hub_model::history_empty_state(searching, false);
            let action = if searching {
                Some(
                    Button::new("clear-search", button)
                        .on_click(cx.listener(|this, _, w, cx| this.clear_search(w, cx)))
                        .into_any_element(),
                )
            } else {
                let pcx = self.pcx.clone();
                Some(
                    Button::new("history-empty-home", "Setup steps")
                        .on_click(move |_, _, cx| pcx.navigate(Page::Home, None, cx))
                        .into_any_element(),
                )
            };
            rows = rows.child(ui::empty_state(&t, "history", h, d, action));
        }
        let mut last_day = String::new();
        for entry in &self.list.entries {
            let day = history::day_label(entry.ts, now);
            if day != last_day {
                rows = rows.child(
                    ui::supporting(&t, day.clone())
                        .font_weight(FontWeight::MEDIUM)
                        .px(space::s6())
                        .pt(space::s6())
                        .pb(space::s2()),
                );
                last_day = day;
            }
            let id = entry.id;
            let selected = self.selected == Some(id);
            let chip = hub_model::history_status_chip(&entry.status);
            rows = rows.child(
                div()
                    .id(("history-row", id as u64))
                    .relative()
                    .flex()
                    .flex_col()
                    .gap(space::s1())
                    .px(space::s6())
                    .py(space::s4())
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(t.border)
                    .when(selected, |d| d.bg(t.selected))
                    .when(!selected, |d| d.hover(|s| s.bg(t.hover)))
                    .when(selected, |d| {
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.selected = Some(id);
                        this.show_original = false;
                        window.focus(&this.list_focus);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .when(selected, |d| d.font_weight(FontWeight::MEDIUM))
                                    .child(history::row_text(entry)),
                            )
                            .when_some(chip, |d, (label, failed)| {
                                d.child(ui::token(
                                    &t,
                                    if failed { Tone::Error } else { Tone::Neutral },
                                    label,
                                ))
                            }),
                    )
                    .child(ui::supporting(
                        &t,
                        format!(
                            "{}  ·  {}",
                            history::time_label(entry.ts),
                            entry
                                .app_name
                                .clone()
                                .filter(|a| !a.is_empty())
                                .unwrap_or_else(|| "Unknown app".into())
                        ),
                    )),
            );
        }
        if self.list.more {
            let offset = self.list.next_offset();
            rows = rows.child(
                div().p(space::s6()).child(
                    Button::new("load-more", "Load more")
                        .busy(self.loading)
                        .on_click(cx.listener(move |this, _, _, cx| this.load(offset, cx))),
                ),
            );
        }
        let ring = ui::ring(&t);
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
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .px(space::s6())
                        .child(ui::banner(&t, Tone::Error, e, None)),
                )
            })
            .child(
                div()
                    .id("history-list")
                    .track_focus(&self.list_focus)
                    .track_scroll(&self.scroll)
                    .on_key_down(cx.listener(Self::on_list_key))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .bg(t.bg_surface)
                    .when(focused, move |d| d.shadow(ring.clone()))
                    .child(rows),
            )
    }

    fn render_detail(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let Some(entry) = self.selected_entry().cloned() else {
            return div()
                .id("history-detail")
                .size_full()
                .child(ui::empty_state(
                    &t,
                    "file-text",
                    "Select a dictation to read it",
                    "Use the list, or the arrow keys once it's focused.",
                    None,
                ));
        };
        let now = history::now();
        let distinct = history::has_distinct_original(&entry);
        let final_text = entry.final_text.clone();
        let raw = entry.raw_text.clone();
        let id = entry.id;
        let has_audio = entry.audio_path.as_deref().is_some_and(|p| !p.is_empty());
        let can_retry = hub_model::history_row_actions(&entry.kind, has_audio)
            .iter()
            .any(|(name, _)| *name == "Retry");
        let mut actions = div().flex().flex_wrap().gap(space::s4());
        actions = actions.child(
            Button::new("copy-final", "Copy")
                .primary()
                .icon("copy")
                .disabled(final_text.is_empty())
                .on_click(
                    cx.listener(move |this, _, _, cx| this.copy(final_text.clone(), "text", cx)),
                ),
        );
        if distinct {
            let raw = raw.clone();
            actions = actions.child(
                Button::new("copy-raw", "Copy original")
                    .icon("copy")
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.copy(raw.clone(), "original", cx)),
                    ),
            );
        }
        if can_retry {
            actions = actions.child(
                Button::new("retry", "Retry")
                    .icon("rotate-ccw")
                    .disabled(true)
                    .tooltip(hub_model::HISTORY_RETRY_UNAVAILABLE),
            );
        }
        actions = actions.child(
            Button::new("delete-entry", "Delete")
                .ghost()
                .icon("trash-2")
                .on_click(cx.listener(move |this, _, _, cx| this.ask_delete(id, cx))),
        );
        let text_body = if entry.final_text.is_empty() && entry.raw_text.is_empty() {
            ui::secondary(
                &t,
                match entry.status.as_str() {
                    "failed" => "This dictation failed, so there's no text.",
                    _ => "Nothing was heard in this recording.",
                },
            )
        } else {
            ui::reading(&t, history::row_text(&entry))
        };
        div()
            .id("history-detail")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s6())
                    .p(space::s10())
                    .max_w(px(size::READING + 64.))
                    .when(narrow, |d| {
                        d.child(
                            Button::new("history-back", "Back to list")
                                .ghost()
                                .icon("arrow-left")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.selected = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s1())
                            .child(ui::label(
                                &t,
                                hub_model::history_when(entry.ts)
                                    .format("%A, %B %-d · %-I:%M %p")
                                    .to_string(),
                            ))
                            .child(ui::supporting(
                                &t,
                                format!(
                                    "{}  ·  {:.0} s  ·  {} words",
                                    hub_model::history_meta_line(
                                        entry.app_name.as_deref(),
                                        entry.ts,
                                        &entry.kind,
                                        now
                                    ),
                                    entry.duration_s,
                                    entry.words
                                ),
                            )),
                    )
                    .child(text_body)
                    .child(actions)
                    .when(can_retry, |d| {
                        d.child(ui::supporting(&t, hub_model::HISTORY_RETRY_UNAVAILABLE))
                    })
                    .when(distinct, |d| {
                        let open = self.show_original;
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(space::s4())
                                .pt(space::s4())
                                .child(
                                    Button::new(
                                        "toggle-original",
                                        if open {
                                            "Hide original"
                                        } else {
                                            "Show original"
                                        },
                                    )
                                    .ghost()
                                    .small()
                                    .icon(if open {
                                        "chevron-down"
                                    } else {
                                        "chevron-right"
                                    })
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.show_original = !this.show_original;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .when(open, |d| {
                                    d.child(
                                        div()
                                            .p(space::s6())
                                            .rounded(size::radius_element())
                                            .bg(t.bg_muted)
                                            .child(ui::reading(&t, raw.clone())),
                                    )
                                }),
                        )
                    }),
            )
    }
}

impl Render for HistoryPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let narrow = window.viewport_size().width < px(1100.);
        let clear = Button::new("clear-all", "Clear all…")
            .ghost()
            .icon("trash-2")
            .disabled(self.list.entries.is_empty() && self.search.read(cx).is_empty())
            .on_click(cx.listener(|this, _, _, cx| this.ask_clear(cx)))
            .into_any_element();
        let support = self
            .total
            .map(|n| format!("{n} dictations on this device").into());
        let history_off = !self
            .pcx
            .services
            .config()
            .field_bool("history_enabled")
            .unwrap_or(true);
        let banner = history_off.then(|| {
            let pcx = self.pcx.clone();
            div().px(space::s10()).pt(space::s5()).child(ui::banner(
                &t,
                Tone::Info,
                "History is off. New dictations aren't being saved.",
                Some(
                    Button::new("history-off-settings", "Privacy settings")
                        .small()
                        .on_click(move |_, _, cx| pcx.navigate(Page::Settings, Some("privacy"), cx))
                        .into_any_element(),
                ),
            ))
        });
        let body = if narrow && self.selected.is_some() {
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(self.render_detail(true, cx))
        } else if narrow {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(self.render_list(window, cx))
        } else {
            let list = self.render_list(window, cx);
            let detail = self.render_detail(false, cx);
            split(&t, list, detail)
        };
        page(
            header(&t, "History", support, vec![clear]),
            div()
                .flex()
                .flex_col()
                .size_full()
                .children(banner)
                .child(body),
        )
    }
}
