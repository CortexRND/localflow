//! Dictionary: words LocalFlow should know and replacements, stored in the
//! existing `dictionary.json` the runtime reads.

use std::path::PathBuf;

use gpui::{div, prelude::*, Context, Entity, PathPromptOptions, SharedString, Window};
use lf_core::dictionary::{MAX_IMPORT_BYTES, MAX_IMPORT_ENTRIES};
use lf_ui::hub_model::{self, DictionaryRow, EntryKind};

use crate::app::PageCx;
use crate::controllers::dictionary as ctl;
use crate::pages::{column, header, page};
use crate::theme::{size, space, theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::{self, Button, IconButton};

pub struct DictionaryPage {
    pcx: PageCx,
    filter: Entity<TextInput>,
    word: Entity<TextInput>,
    from: Entity<TextInput>,
    to: Entity<TextInput>,
    import_path: Entity<TextInput>,
    editing: Option<DictionaryRow>,
    word_error: Option<String>,
    rep_error: Option<String>,
    import_open: bool,
    import_notice: Option<(Tone, String)>,
}

impl DictionaryPage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx, "Filter words and replacements"));
        let word = cx.new(|cx| {
            TextInput::new(cx, "Add a word, name or term")
                .label("New word")
                .max_chars(200)
        });
        let from = cx.new(|cx| {
            TextInput::new(cx, "When I say…")
                .label("When I say")
                .max_chars(200)
        });
        let to = cx.new(|cx| TextInput::new(cx, "Write…").label("Write").max_chars(500));
        let import_path = cx.new(|cx| TextInput::new(cx, "/path/to/words.csv or .json"));
        cx.subscribe(&filter, |_, _, e, cx| {
            if *e == InputEvent::Changed {
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&word, |this, _, e, cx| match e {
            InputEvent::Submit => this.submit_word(cx),
            InputEvent::Changed => {
                this.word_error = None;
                cx.notify();
            }
        })
        .detach();
        for input in [&from, &to] {
            cx.subscribe(input, |this, _, e, cx| match e {
                InputEvent::Submit => this.submit_replacement(cx),
                InputEvent::Changed => {
                    this.rep_error = None;
                    cx.notify();
                }
            })
            .detach();
        }
        cx.subscribe(&import_path, |this, input, e, cx| {
            if *e == InputEvent::Submit {
                let path = input.read(cx).text().trim().to_string();
                if !path.is_empty() {
                    this.import(PathBuf::from(path), cx);
                }
            }
        })
        .detach();
        DictionaryPage {
            pcx,
            filter,
            word,
            from,
            to,
            import_path,
            editing: None,
            word_error: None,
            rep_error: None,
            import_open: false,
            import_notice: None,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Words plus replacements.
    fn total(&self) -> usize {
        let store = &self.pcx.services.dictionary;
        store.words().len() + store.replacements().len()
    }

    /// The filter is only shown once the list is long enough to need it.
    fn shows_filter(total: usize) -> bool {
        total > 12
    }

    /// Find: focus the filter when it is shown; otherwise keep focus where
    /// it is (focusing a hidden input would leave nothing focused).
    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if Self::shows_filter(self.total()) {
            self.filter.read(cx).focus(window);
        }
    }

    /// The filter's focus handle, for shell tests.
    #[cfg(test)]
    pub(crate) fn filter_focus(&self, cx: &gpui::App) -> gpui::FocusHandle {
        gpui::Focusable::focus_handle(self.filter.read(cx), cx)
    }

    pub fn focus_new_word(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.word.read(cx).focus(window);
    }

    fn editing_key(&self, kind: EntryKind) -> Option<String> {
        self.editing
            .as_ref()
            .filter(|r| r.kind == kind)
            .map(|r| r.key.clone())
    }

    fn submit_word(&mut self, cx: &mut Context<Self>) {
        let text = self.word.read(cx).text().to_string();
        let editing = self.editing_key(EntryKind::Word);
        match ctl::submit_word(&self.pcx.services.dictionary, &text, editing.as_deref()) {
            Ok(()) => {
                self.word.update(cx, |w, cx| w.clear(cx));
                self.word_error = None;
                if editing.is_some() {
                    self.editing = None;
                    self.pcx.toast("Word updated", cx);
                } else {
                    self.pcx.toast(format!("Added “{}”", text.trim()), cx);
                }
            }
            Err(message) => self.word_error = Some(message),
        }
        cx.notify();
    }

    fn submit_replacement(&mut self, cx: &mut Context<Self>) {
        let from = self.from.read(cx).text().to_string();
        let to = self.to.read(cx).text().to_string();
        let editing = self.editing_key(EntryKind::Replacement);
        match ctl::submit_replacement(
            &self.pcx.services.dictionary,
            &from,
            &to,
            editing.as_deref(),
        ) {
            Ok(()) => {
                self.from.update(cx, |w, cx| w.clear(cx));
                self.to.update(cx, |w, cx| w.clear(cx));
                self.rep_error = None;
                if editing.is_some() {
                    self.editing = None;
                }
                self.pcx.toast("Replacement saved", cx);
            }
            Err(message) => self.rep_error = Some(message),
        }
        cx.notify();
    }

    fn begin_edit(&mut self, row: DictionaryRow, window: &mut Window, cx: &mut Context<Self>) {
        match row.kind {
            EntryKind::Word => {
                self.word
                    .update(cx, |w, cx| w.set_text(row.key.clone(), cx));
                self.word.read(cx).focus(window);
            }
            EntryKind::Replacement => {
                self.from
                    .update(cx, |w, cx| w.set_text(row.key.clone(), cx));
                self.to
                    .update(cx, |w, cx| w.set_text(row.target.clone(), cx));
                self.from.read(cx).focus(window);
            }
        }
        self.editing = Some(row);
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.editing.take() {
            match row.kind {
                EntryKind::Word => self.word.update(cx, |w, cx| w.clear(cx)),
                EntryKind::Replacement => {
                    self.from.update(cx, |w, cx| w.clear(cx));
                    self.to.update(cx, |w, cx| w.clear(cx));
                }
            }
        }
        cx.notify();
    }

    fn remove(&mut self, row: DictionaryRow, cx: &mut Context<Self>) {
        match ctl::remove(&self.pcx.services.dictionary, &row) {
            Ok(()) => {
                let this = cx.entity().downgrade();
                let label = row.label.clone();
                self.pcx.toast_with(
                    format!("Removed “{label}”"),
                    "Undo",
                    move |_, cx| {
                        let row = row.clone();
                        let _ = this.update(cx, |p, cx| {
                            if let Err(e) = ctl::restore(&p.pcx.services.dictionary, &row) {
                                p.pcx.toast(e, cx);
                            }
                            cx.notify();
                        });
                    },
                    cx,
                );
            }
            Err(message) => self.pcx.toast(message, cx),
        }
        cx.notify();
    }

    fn pick_file(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        this.import(path, cx);
                    }
                }
                Ok(Ok(None)) => {}
                _ => {
                    this.import_notice = Some((
                        Tone::Warning,
                        "Couldn't open the file picker on this desktop. Type the file's path instead.".into(),
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn import(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.import_notice = Some(match self.pcx.services.dictionary.import_file(&path) {
            Ok(counts) => (Tone::Success, hub_model::dictionary_import_notice(&counts)),
            Err(err) => (
                Tone::Error,
                format!("{} ({err})", hub_model::DICTIONARY_IMPORT_FAILED),
            ),
        });
        cx.notify();
    }

    fn render_rows(
        &self,
        rows: Vec<DictionaryRow>,
        empty: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = theme(cx).clone();
        if rows.is_empty() {
            return div()
                .py(space::s6())
                .child(ui::secondary(&t, empty.to_string()));
        }
        let mut list = ui::list(&t);
        for (i, row) in rows.into_iter().enumerate() {
            let editing = self.editing.as_ref() == Some(&row);
            let edit_row = row.clone();
            let remove_row = row.clone();
            let kind = row.kind as u64;
            let label: SharedString = match row.kind {
                EntryKind::Word => row.key.clone().into(),
                EntryKind::Replacement => format!("{}  →  {}", row.key, row.target).into(),
            };
            if i > 0 {
                list = list.child(ui::separator(&t, ui::row_inset()));
            }
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .pl(ui::row_inset())
                    .pr(space::s4())
                    .py(space::s3())
                    .rounded(size::radius_inner())
                    .when(editing, |d| d.bg(t.selected))
                    .child(div().flex_1().min_w_0().child(ui::body(&t, label)))
                    .when(editing, |d| d.child(ui::token(&t, Tone::Accent, "Editing")))
                    .child(
                        IconButton::new(
                            ("edit", kind * 100_000 + i as u64),
                            "pencil",
                            format!("Edit {}", row.label),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.begin_edit(edit_row.clone(), window, cx)
                            },
                        )),
                    )
                    .child(
                        IconButton::new(
                            ("remove", kind * 100_000 + i as u64),
                            "trash-2",
                            format!("Remove {}", row.label),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.remove(remove_row.clone(), cx)),
                        ),
                    ),
            );
        }
        div().child(list)
    }
}

impl Render for DictionaryPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let store = &self.pcx.services.dictionary;
        let total = self.total();
        let filter_text = if Self::shows_filter(total) {
            self.filter.read(cx).text().to_string()
        } else {
            String::new()
        };
        let (words, reps) = ctl::rows(store, &filter_text);
        let editing_word = self
            .editing
            .as_ref()
            .is_some_and(|r| r.kind == EntryKind::Word);
        let editing_rep = self
            .editing
            .as_ref()
            .is_some_and(|r| r.kind == EntryKind::Replacement);

        let import_btn = Button::new("import-open", "Import…")
            .icon("upload")
            .on_click(cx.listener(|this, _, _, cx| {
                this.import_open = !this.import_open;
                cx.notify();
            }))
            .into_any_element();

        let mut body = div();
        if self.import_open || self.import_notice.is_some() {
            body = body.child(
                ui::section(
                    &t,
                    "Import words",
                    Some(format!(
                        "A CSV or JSON file of words and replacements, up to {} MB and {} entries. Duplicates are skipped.",
                        MAX_IMPORT_BYTES / (1024 * 1024),
                        MAX_IMPORT_ENTRIES
                    ).into()),
                    None,
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::s4())
                        .child(div().flex_1().child(self.import_path.clone()))
                        .child(Button::new("import-path", "Import").on_click(cx.listener(|this, _, _, cx| {
                            let path = this.import_path.read(cx).text().trim().to_string();
                            if !path.is_empty() {
                                this.import(PathBuf::from(path), cx);
                            }
                        })))
                        .child(
                            Button::new("import-pick", "Choose file…")
                                .icon("folder-open")
                                .on_click(cx.listener(|this, _, _, cx| this.pick_file(cx))),
                        ),
                )
                .when_some(self.import_notice.clone(), |d, (tone, msg)| d.child(ui::banner(&t, tone, msg, None))),
            );
        }
        if Self::shows_filter(total) {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .child(ui::icon("search", t.icon_secondary))
                    .child(self.filter.clone()),
            );
        }
        let word_btn = Button::new(
            "add-word",
            if editing_word {
                "Save word"
            } else {
                "Add word"
            },
        )
        .primary()
        .icon(if editing_word { "check" } else { "plus" })
        .on_click(cx.listener(|this, _, _, cx| this.submit_word(cx)));
        body = body.child(
            ui::section(
                &t,
                "Words",
                Some("Names, jargon and spellings LocalFlow should recognise.".into()),
                None,
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .child(div().flex_1().child(self.word.clone()))
                    .child(word_btn)
                    .when(editing_word, |d| {
                        d.child(
                            Button::new("cancel-word", "Cancel")
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_edit(cx))),
                        )
                    }),
            )
            .when_some(self.word_error.clone(), |d, e| {
                d.child(ui::error_line(&t, e))
            })
            .child(self.render_rows(
                words,
                "No words yet. Add names and terms you use often.",
                cx,
            )),
        );
        let rep_btn = Button::new(
            "add-rep",
            if editing_rep {
                "Save"
            } else {
                "Add replacement"
            },
        )
        .primary()
        .icon(if editing_rep { "check" } else { "plus" })
        .on_click(cx.listener(|this, _, _, cx| this.submit_replacement(cx)));
        body = body.child(
            ui::section(
                &t,
                "Replacements",
                Some("Corrections applied to every dictation: say one thing, get another.".into()),
                None,
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .child(div().flex_1().child(self.from.clone()))
                    .child(ui::icon("chevron-right", t.icon_secondary))
                    .child(div().flex_1().child(self.to.clone()))
                    .child(rep_btn)
                    .when(editing_rep, |d| {
                        d.child(
                            Button::new("cancel-rep", "Cancel")
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_edit(cx))),
                        )
                    }),
            )
            .when_some(self.rep_error.clone(), |d, e| {
                d.child(ui::error_line(&t, e))
            })
            .child(self.render_rows(reps, "No replacements yet.", cx)),
        );
        page(
            header(
                &t,
                "Dictionary",
                Some(format!("{total} entries · stored on this device").into()),
                vec![import_btn],
            ),
            column("dictionary-scroll", body),
        )
    }
}
