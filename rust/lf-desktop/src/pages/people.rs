//! People: the voices LocalFlow remembers, read-only.
//!
//! Shown only while `controllers::people::enabled` (both speaker flags on);
//! the shell hides the sidebar entry and refuses the route otherwise. The
//! page reads `GET /api/speakers` and nothing else: there is no toggle,
//! consent text, enrollment, rename or forget here, and only a name, the
//! date added, a sample count and a voice-model status ever reach it (see
//! `controllers::people`).

use gpui::{div, prelude::*, Context, SharedString, Window};

use crate::app::PageCx;
use crate::controllers::people::{self as ctl, People, PeopleState};
use crate::page::Page;
use crate::pages::{column, header, page, sandbox_note, service_banner};
use crate::remote::Health;
use crate::theme::{size, space, theme, Tone};
use crate::ui::{self, Button};

pub const SUBTITLE: &str = "Voices LocalFlow remembers for meetings. This list is read-only.";

pub struct PeoplePage {
    pcx: PageCx,
    state: PeopleState,
}

impl PeoplePage {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let mut page = PeoplePage {
            pcx,
            state: PeopleState::default(),
        };
        page.refresh(cx);
        page
    }

    fn enabled(&self) -> bool {
        ctl::enabled(&self.pcx.services.config())
    }

    /// Read the list again, only while the page is shown and turned on
    /// (a closed gate clears it and requests nothing).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let shown = *self.pcx.current_page.borrow() == Page::People;
        let enabled = self.enabled();
        if !enabled {
            self.state.clear();
            cx.notify();
            return;
        }
        if !shown {
            return;
        }
        let Some(generation) = self.state.begin(true) else {
            return;
        };
        let remote = self.pcx.services.remote.clone();
        let config = self.pcx.services.config();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { remote.people(&config) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let enabled = this.enabled();
                if this.state.finish(generation, result, enabled) {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn retry_button(&self, id: &'static str, cx: &mut Context<Self>) -> gpui::AnyElement {
        Button::new(id, "Try again")
            .small()
            .icon("refresh-cw")
            .on_click(cx.listener(|this, _, _, cx| {
                this.pcx.recheck_health(cx);
                this.refresh(cx);
            }))
            .into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let t = theme(cx).clone();
        match &self.state.people {
            None => div().child(ui::secondary(
                &t,
                if self.state.loading {
                    "Loading the people LocalFlow remembers…"
                } else {
                    ""
                },
            )),
            Some(People::List(people)) if people.is_empty() => div().child(ui::empty_state(
                &t,
                "user-round",
                "No one is remembered yet",
                "Voices LocalFlow remembers for meetings are listed here.",
                None,
            )),
            Some(People::List(people)) => {
                let rows = people.iter().map(|person| {
                    let detail = ctl::detail(person);
                    let note = person.voice.note();
                    div()
                        .flex()
                        .items_center()
                        .gap(space::s5())
                        .min_h(size::row())
                        .px(ui::row_inset())
                        .py(space::s4())
                        .child(ui::glyph_tile("user-round", t.icon_secondary, t.bg_muted))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(space::s1())
                                .min_w_0()
                                .child(ui::label(&t, person.name.clone()))
                                .when(!detail.is_empty(), |d| d.child(ui::supporting(&t, detail)))
                                .when(!note.is_empty(), |d| d.child(ui::supporting(&t, note))),
                        )
                        .into_any_element()
                });
                let count = match people.len() {
                    1 => "1 person".to_string(),
                    n => format!("{n} people"),
                };
                ui::section(&t, "Remembered", Some(SharedString::from(count)), None)
                    .child(ui::group_rows(&t, rows, ui::row_inset()))
            }
            Some(People::Off) => div().child(ui::empty_state(
                &t,
                "user-round",
                "Speaker recognition is off on the LocalFlow service",
                "The service answered that it isn't recognising speakers, so there's no one to list.",
                Some(self.retry_button("people-retry-off", cx)),
            )),
            Some(People::Offline) => div().child(ui::empty_state(
                &t,
                "user-round",
                "People need the LocalFlow service",
                "Start LocalFlow, then try again.",
                Some(self.retry_button("people-retry", cx)),
            )),
            Some(People::Error(error)) => div().child(ui::banner(
                &t,
                Tone::Error,
                format!("Couldn't read the people LocalFlow remembers: {error}"),
                Some(self.retry_button("people-retry-error", cx)),
            )),
        }
    }
}

impl Render for PeoplePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        // Fixture scenes only: the remembered people are listed.
        if matches!(&self.state.people, Some(People::List(people)) if !people.is_empty()) {
            crate::sandbox::scene_ready(crate::sandbox::Fixture::SpeakersPeople);
        }
        let refresh = Button::new("people-refresh", "Refresh")
            .small()
            .icon("refresh-cw")
            .busy(self.state.loading)
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
            .into_any_element();
        let head = header(&t, "People", Some(SUBTITLE.into()), vec![refresh]);
        let health = self.pcx.health(cx);
        let config = self.pcx.services.config();
        let address = self.pcx.services.remote.base_url(&config);
        let pcx = self.pcx.clone();
        let banner = if health == Health::Simulated {
            Some(sandbox_note(&t))
        } else {
            service_banner(&t, &health, &address, move |_, _, cx| {
                pcx.recheck_health(cx)
            })
        };
        let body = div().children(banner).child(self.render_list(cx));
        page(head, column("people-scroll", body))
    }
}
