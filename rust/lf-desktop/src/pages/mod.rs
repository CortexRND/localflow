//! One module per navigation destination, plus the shared page frames.

pub mod ask;
pub mod dictionary;
pub mod history;
pub mod home;
pub mod meetings;
pub mod models;
pub mod prompts;
pub mod settings;
pub mod snippets;
pub mod style;

use gpui::{div, prelude::*, px, AnyElement, Div, ElementId, SharedString, Stateful};

use crate::remote::Health;
use crate::theme::{size, space, Theme, Tone};
use crate::ui::{self, Button};

/// 72 px page header: title, supporting line, right-aligned actions.
pub fn header(
    t: &Theme,
    title: &str,
    support: Option<SharedString>,
    actions: Vec<AnyElement>,
) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .gap(space::s6())
        .min_h(px(72.))
        .px(space::s10())
        .py(space::s5())
        .border_b_1()
        .border_color(t.border)
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .child(ui::title(t, title.to_string()))
                .when_some(support, |d, s| d.child(ui::secondary(t, s))),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(space::s4())
                .children(actions),
        )
}

/// Scrolling column frame: a centred column capped at 760 px.
pub fn column(id: impl Into<ElementId>, content: Div) -> Stateful<Div> {
    div().id(id).flex_1().min_h_0().overflow_y_scroll().child(
        div()
            .flex()
            .justify_center()
            .px(space::s10())
            .pt(space::s8())
            .pb(space::s12())
            .child(
                content
                    .flex()
                    .flex_col()
                    .gap(space::s10())
                    .w_full()
                    .max_w(px(size::COLUMN)),
            ),
    )
}

/// The full page: header over a body, filling the main region.
pub fn page(header: Div, body: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .size_full()
        .child(header)
        .child(div().flex().flex_col().flex_1().min_h_0().child(body))
}

/// Split frame: fixed list pane, divider, detail pane; each scrolls alone.
pub fn split(t: &Theme, list: impl IntoElement, detail: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .w(px(size::LIST_PANE))
                .h_full()
                .border_r_1()
                .border_color(t.border)
                .child(list),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(detail),
        )
}

/// Banner for a page that needs the local service, or `None` when usable.
pub fn service_banner(
    t: &Theme,
    health: &Health,
    address: &str,
    retry: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> Option<Div> {
    match health {
        Health::Online | Health::Simulated => None,
        Health::Unknown => Some(ui::banner(t, Tone::Info, "Checking the LocalFlow service…", None)),
        Health::Offline => Some(ui::banner(
            t,
            Tone::Warning,
            format!(
                "LocalFlow's local service isn't running at {address}. Start LocalFlow (`localflow`, or `lf serve`) and try again."
            ),
            Some(Button::new("retry-service", "Try again").small().icon("refresh-cw").on_click(retry).into_any_element()),
        )),
    }
}

/// Sandbox note for service-backed pages.
pub fn sandbox_note(t: &Theme) -> Div {
    ui::banner(
        t,
        Tone::Info,
        "Sandbox: answers come from example services in this window. Nothing is sent anywhere.",
        None,
    )
}
