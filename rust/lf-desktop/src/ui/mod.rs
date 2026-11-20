//! Reusable native components. Pages build only from these and theme tokens.

pub mod assets;
pub mod input;
pub mod select;
pub mod text_edit;

use std::rc::Rc;

use gpui::{
    div, point, prelude::*, px, svg, AnyElement, App, BoxShadow, ClickEvent, Div, ElementId,
    FocusHandle, FontWeight, Hsla, IntoElement, SharedString, Stateful, Window,
};

use crate::theme::{size, space, text, theme, Theme, Tone};

pub type Handler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// Callback with a value (switch state, chosen index, tab).
pub type ValueHandler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;
/// Callback without an event (toast actions, confirmations).
pub type Action = Rc<dyn Fn(&mut Window, &mut App)>;

/// Wrap a closure as a click handler.
pub fn handler(f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Handler {
    Rc::new(f)
}

/// The 2 px accent focus ring (drawn outside the box; no layout shift).
pub fn ring(t: &Theme) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: t.accent,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(2.),
    }]
}

pub fn icon(name: &str, color: Hsla) -> gpui::Svg {
    svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(size::icon())
        .flex_none()
        .text_color(color)
}

pub fn icon_lg(name: &str, color: Hsla) -> gpui::Svg {
    icon(name, color).size(size::icon_lg())
}

pub fn brand(t: &Theme) -> gpui::Svg {
    svg()
        .path("brand.svg")
        .size(size::icon_lg())
        .flex_none()
        .text_color(t.accent)
}

// ------------------------------------------------------------------ text ---

pub fn title(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::title();
    div()
        .text_size(sz)
        .line_height(lh)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text)
        .child(s.into())
}

pub fn heading(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::heading();
    div()
        .text_size(sz)
        .line_height(lh)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text)
        .child(s.into())
}

pub fn label(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::body();
    div()
        .text_size(sz)
        .line_height(lh)
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.text)
        .child(s.into())
}

pub fn body(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::body();
    div()
        .text_size(sz)
        .line_height(lh)
        .text_color(t.text)
        .child(s.into())
}

pub fn secondary(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::body();
    div()
        .text_size(sz)
        .line_height(lh)
        .text_color(t.text_secondary)
        .child(s.into())
}

pub fn supporting(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::supporting();
    div()
        .text_size(sz)
        .line_height(lh)
        .text_color(t.text_secondary)
        .child(s.into())
}

pub fn reading(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::reading();
    div()
        .text_size(sz)
        .line_height(lh)
        .text_color(t.text)
        .child(s.into())
}

pub fn mono(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::mono();
    div()
        .font_family(text::MONO_FAMILY)
        .text_size(sz)
        .line_height(lh)
        .text_color(t.text)
        .child(s.into())
}

pub fn error_line(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (fg, _) = t.tone(Tone::Error);
    let (sz, lh) = text::supporting();
    div()
        .flex()
        .gap(space::s2())
        .items_center()
        .text_size(sz)
        .line_height(lh)
        .text_color(fg)
        .child(icon("circle-alert", fg).size(px(14.)))
        .child(s.into())
}

pub fn divider(t: &Theme) -> Div {
    div().h(px(1.)).w_full().flex_none().bg(t.border)
}

// ---------------------------------------------------------------- button ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
    Ghost,
    Destructive,
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    variant: Variant,
    icon: Option<&'static str>,
    disabled: bool,
    busy: bool,
    small: bool,
    tooltip: Option<SharedString>,
    on_click: Option<Handler>,
    /// Use this handle instead of an automatic tab stop, so the owner can
    /// move focus to the button (dialogs).
    focus: Option<FocusHandle>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Button {
            id: id.into(),
            label: label.into(),
            variant: Variant::Secondary,
            icon: None,
            disabled: false,
            busy: false,
            small: false,
            tooltip: None,
            on_click: None,
            focus: None,
        }
    }
    pub fn primary(mut self) -> Self {
        self.variant = Variant::Primary;
        self
    }
    pub fn ghost(mut self) -> Self {
        self.variant = Variant::Ghost;
        self
    }
    pub fn destructive(mut self) -> Self {
        self.variant = Variant::Destructive;
        self
    }
    pub fn variant(mut self, v: Variant) -> Self {
        self.variant = v;
        self
    }
    pub fn icon(mut self, name: &'static str) -> Self {
        self.icon = Some(name);
        self
    }
    pub fn disabled(mut self, d: bool) -> Self {
        self.disabled = d;
        self
    }
    pub fn busy(mut self, b: bool) -> Self {
        self.busy = b;
        self
    }
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }
    pub fn tooltip(mut self, t: impl Into<SharedString>) -> Self {
        self.tooltip = Some(t.into());
        self
    }
    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
    pub fn on_click_rc(mut self, f: Handler) -> Self {
        self.on_click = Some(f);
        self
    }
    pub fn track_focus(mut self, handle: &FocusHandle) -> Self {
        self.focus = Some(handle.clone());
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = theme(cx).clone();
        let inactive = self.disabled || self.busy;
        let (bg, fg, border, hover) = match self.variant {
            Variant::Primary => (t.accent, t.text_on_accent, t.accent, t.accent_hover),
            Variant::Secondary => (t.bg_card, t.text, t.border_control, t.hover),
            Variant::Ghost => (
                gpui::transparent_black(),
                t.text,
                gpui::transparent_black(),
                t.hover,
            ),
            Variant::Destructive => {
                let (fg, bg) = t.tone(Tone::Error);
                (bg, fg, bg, bg.opacity(0.8))
            }
        };
        let (bg, fg, border) = if self.disabled {
            match self.variant {
                Variant::Ghost => (bg, t.text_disabled, border),
                _ => (t.bg_muted, t.text_disabled, t.bg_muted),
            }
        } else {
            (bg, fg, border)
        };
        let height = if self.small {
            size::control_sm()
        } else {
            size::control_md()
        };
        let (sz, lh) = text::body();
        let icon_name = if self.busy {
            Some("loader-circle")
        } else {
            self.icon
        };
        let ring = ring(&t);
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap(space::s3())
            .h(height)
            .px(if self.small { space::s4() } else { space::s5() })
            .rounded(size::radius_element())
            .border_1()
            .border_color(border)
            .bg(bg)
            .text_color(fg)
            .text_size(sz)
            .line_height(lh)
            .font_weight(FontWeight::MEDIUM)
            .when_some(icon_name, |el, name| el.child(icon(name, fg)))
            .child(self.label.clone());
        if !inactive {
            el = match &self.focus {
                Some(handle) => el.track_focus(handle),
                None => el.tab_index(0),
            };
            el = el
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .focus(move |s| s.shadow(ring.clone()));
            if self.variant == Variant::Ghost || self.variant == Variant::Secondary {
                let pressed = t.pressed;
                el = el.active(move |s| s.bg(pressed));
            }
            if let Some(f) = self.on_click {
                el = el.on_click(move |e, w, cx| f(e, w, cx));
            }
        }
        if let Some(tip) = self.tooltip {
            el = el.tooltip(move |_, cx| Tooltip::view(tip.clone(), cx));
        }
        el
    }
}

/// Square icon-only button; always has a tooltip and an accessible name.
#[derive(IntoElement)]
pub struct IconButton {
    id: ElementId,
    icon: &'static str,
    label: SharedString,
    disabled: bool,
    on_click: Option<Handler>,
}

impl IconButton {
    pub fn new(
        id: impl Into<ElementId>,
        icon: &'static str,
        label: impl Into<SharedString>,
    ) -> Self {
        IconButton {
            id: id.into(),
            icon,
            label: label.into(),
            disabled: false,
            on_click: None,
        }
    }
    pub fn disabled(mut self, d: bool) -> Self {
        self.disabled = d;
        self
    }
    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = theme(cx).clone();
        let color = if self.disabled {
            t.text_disabled
        } else {
            t.icon_secondary
        };
        let hover = t.hover;
        let ring = ring(&t);
        let label = self.label.clone();
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(size::control_sm())
            .rounded(size::radius_element())
            .child(icon(self.icon, color))
            .tooltip(move |_, cx| Tooltip::view(label.clone(), cx));
        if !self.disabled {
            el = el
                .tab_index(0)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .focus(move |s| s.shadow(ring.clone()));
            if let Some(f) = self.on_click {
                el = el.on_click(move |e, w, cx| f(e, w, cx));
            }
        }
        el
    }
}

/// Tooltip view.
pub struct Tooltip(SharedString);

impl Tooltip {
    pub fn view(text: SharedString, cx: &mut App) -> gpui::AnyView {
        cx.new(|_| Tooltip(text)).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let (sz, lh) = text::supporting();
        div()
            .px(space::s4())
            .py(space::s2())
            .rounded(size::radius_inner())
            .bg(t.bg_inverted)
            .text_color(t.text_inverted)
            .text_size(sz)
            .line_height(lh)
            .max_w(px(320.))
            .child(self.0.clone())
    }
}

// ---------------------------------------------------------------- switch ---

#[derive(IntoElement)]
pub struct Switch {
    id: ElementId,
    on: bool,
    disabled: bool,
    label: SharedString,
    on_toggle: Option<ValueHandler<bool>>,
}

impl Switch {
    pub fn new(id: impl Into<ElementId>, on: bool, label: impl Into<SharedString>) -> Self {
        Switch {
            id: id.into(),
            on,
            disabled: false,
            label: label.into(),
            on_toggle: None,
        }
    }
    pub fn disabled(mut self, d: bool) -> Self {
        self.disabled = d;
        self
    }
    pub fn on_toggle(mut self, f: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_toggle = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Switch {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = theme(cx).clone();
        let (track, knob, border) = match (self.on, self.disabled) {
            (true, false) => (t.accent, t.text_on_accent, t.accent),
            (false, false) => (t.bg_card, t.border_control, t.border_control),
            (true, true) => (t.text_disabled, t.bg_card, t.text_disabled),
            (false, true) => (t.bg_muted, t.text_disabled, t.text_disabled),
        };
        let ring = ring(&t);
        let on = self.on;
        let label = self.label.clone();
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .w(px(36.))
            .h(px(20.))
            .px(px(2.))
            .rounded_full()
            .border_1()
            .border_color(border)
            .bg(track)
            .when(on, |d| d.justify_end())
            .child(div().size(px(14.)).rounded_full().bg(knob))
            .tooltip(move |_, cx| {
                Tooltip::view(
                    format!("{label}: {}", if on { "On" } else { "Off" }).into(),
                    cx,
                )
            });
        if !self.disabled {
            el = el
                .tab_index(0)
                .cursor_pointer()
                .focus(move |s| s.shadow(ring.clone()));
            if let Some(f) = self.on_toggle {
                el = el.on_click(move |_, w, cx| f(!on, w, cx));
            }
        }
        el
    }
}

// ----------------------------------------------------- segmented control ---

#[derive(IntoElement)]
pub struct Segmented {
    id: ElementId,
    options: Vec<SharedString>,
    selected: Option<usize>,
    disabled: bool,
    on_select: Option<ValueHandler<usize>>,
}

impl Segmented {
    pub fn new(
        id: impl Into<ElementId>,
        options: impl IntoIterator<Item = impl Into<SharedString>>,
        selected: Option<usize>,
    ) -> Self {
        Segmented {
            id: id.into(),
            options: options.into_iter().map(Into::into).collect(),
            selected,
            disabled: false,
            on_select: None,
        }
    }
    pub fn disabled(mut self, d: bool) -> Self {
        self.disabled = d;
        self
    }
    pub fn on_select(mut self, f: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Segmented {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = theme(cx).clone();
        let (sz, lh) = text::body();
        let id = self.id.clone();
        div()
            .id(self.id)
            .flex()
            .flex_none()
            .h(size::control_md())
            .p(px(2.))
            .gap(px(2.))
            .rounded(size::radius_element())
            .bg(t.bg_muted)
            .children(self.options.into_iter().enumerate().map(|(i, option)| {
                let selected = self.selected == Some(i);
                let fg = if self.disabled {
                    t.text_disabled
                } else if selected {
                    t.text
                } else {
                    t.text_secondary
                };
                let ring = ring(&t);
                let hover = t.hover;
                let mut seg = div()
                    .id(ElementId::NamedInteger(
                        format!("{id:?}-seg").into(),
                        i as u64,
                    ))
                    .flex()
                    .items_center()
                    .gap(space::s2())
                    .px(space::s5())
                    .rounded(px(6.))
                    .text_size(sz)
                    .line_height(lh)
                    .text_color(fg)
                    .when(selected, |d| {
                        d.bg(t.bg_card)
                            .font_weight(FontWeight::SEMIBOLD)
                            .border_1()
                            .border_color(t.border)
                    })
                    .when(selected && !self.disabled, |d| {
                        d.child(icon("check", t.icon_accent).size(px(12.)))
                    })
                    .child(option);
                if !self.disabled {
                    seg = seg
                        .tab_index(0)
                        .cursor_pointer()
                        .when(!selected, move |d| d.hover(move |s| s.bg(hover)))
                        .focus(move |s| s.shadow(ring.clone()));
                    if let Some(f) = self.on_select.clone() {
                        seg = seg.on_click(move |_, w, cx| f(i, w, cx));
                    }
                }
                seg
            }))
    }
}

// --------------------------------------------------- small presentational ---

pub fn keycap(t: &Theme, s: impl Into<SharedString>) -> Div {
    let (sz, _) = text::supporting();
    div()
        .flex()
        .items_center()
        .h(px(22.))
        .px(space::s3())
        .rounded(size::radius_inner())
        .border_1()
        .border_color(t.border)
        .bg(t.bg_muted)
        .font_family(text::MONO_FAMILY)
        .text_size(sz)
        .text_color(t.text)
        .child(s.into())
}

/// A key combination as keycaps (`"Left Option + K"` -> two caps).
pub fn keys(t: &Theme, combo: &str) -> Div {
    if combo == "Off" || combo.is_empty() {
        return supporting(t, "Not set");
    }
    div()
        .flex()
        .gap(space::s2())
        .children(combo.split(" + ").map(|k| keycap(t, k.to_string())))
}

pub fn status_dot(t: &Theme, tone: Tone, word: impl Into<SharedString>) -> Div {
    let (sz, lh) = text::body();
    div()
        .flex()
        .items_center()
        .gap(space::s3())
        .text_size(sz)
        .line_height(lh)
        .text_color(t.text)
        .child(
            div()
                .size(px(8.))
                .flex_none()
                .rounded_full()
                .bg(t.dot(tone)),
        )
        .child(word.into())
}

pub fn token(t: &Theme, tone: Tone, s: impl Into<SharedString>) -> Div {
    let (fg, bg) = t.tone(tone);
    let (sz, lh) = text::supporting();
    div()
        .flex_none()
        .px(space::s3())
        .rounded(size::radius_inner())
        .bg(bg)
        .text_color(fg)
        .text_size(sz)
        .line_height(lh)
        .font_weight(FontWeight::MEDIUM)
        .child(s.into())
}

pub fn badge(t: &Theme, n: usize) -> Div {
    let (sz, _) = text::supporting();
    div()
        .flex_none()
        .min_w(px(20.))
        .h(px(18.))
        .px(space::s2())
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(t.accent_muted)
        .text_color(t.text_accent)
        .text_size(sz)
        .font_weight(FontWeight::SEMIBOLD)
        .child(n.to_string())
}

/// A 4 px progress track. `None` = indeterminate (a short accent segment).
pub fn progress(t: &Theme, fraction: Option<f32>) -> Div {
    let fill = match fraction {
        Some(f) => gpui::relative(f.clamp(0.0, 1.0)),
        None => gpui::relative(0.3),
    };
    div()
        .w_full()
        .h(px(4.))
        .flex_none()
        .rounded(size::radius_inner())
        .bg(t.bg_muted)
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(fill)
                .rounded(size::radius_inner())
                .bg(t.accent)
                .when(fraction.is_none(), |d| d.opacity(0.6)),
        )
}

/// An 8 px microphone level meter (0.0–1.0) in a tone's dot colour.
pub fn level_meter(t: &Theme, level: f32, tone: Tone) -> Div {
    div()
        .w_full()
        .h(px(8.))
        .flex_none()
        .rounded(size::radius_inner())
        .bg(t.bg_muted)
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(gpui::relative(level.clamp(0.0, 1.0)))
                .rounded(size::radius_inner())
                .bg(t.dot(tone)),
        )
}

/// A persistent condition strip: icon, sentence, optional action.
pub fn banner(
    t: &Theme,
    tone: Tone,
    message: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> Div {
    let (fg, bg) = t.tone(tone);
    let name = match tone {
        Tone::Error => "circle-alert",
        Tone::Warning => "triangle-alert",
        Tone::Success => "circle-check",
        _ => "info",
    };
    let (sz, lh) = text::body();
    div()
        .flex()
        .items_center()
        .gap(space::s5())
        .w_full()
        .px(space::s6())
        .py(space::s4())
        .rounded(size::radius_element())
        .bg(bg)
        .text_color(fg)
        .text_size(sz)
        .line_height(lh)
        .child(icon(name, fg))
        .child(div().flex_1().min_w_0().child(message.into()))
        .when_some(action, |d, a| d.child(a))
}

pub fn empty_state(
    t: &Theme,
    icon_name: &'static str,
    heading_text: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    action: Option<AnyElement>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(space::s4())
        .py(space::s12())
        .px(space::s8())
        .w_full()
        .child(icon_lg(icon_name, t.icon_secondary))
        .child(heading(t, heading_text))
        .child(secondary(t, detail).text_center().max_w(px(420.)))
        .when_some(action, |d, a| d.child(div().pt(space::s4()).child(a)))
}

/// Section: heading, optional supporting line and action, then content.
pub fn section(
    t: &Theme,
    heading_text: impl Into<SharedString>,
    support: Option<SharedString>,
    action: Option<AnyElement>,
) -> Div {
    div().flex().flex_col().gap(space::s5()).w_full().child(
        div()
            .flex()
            .items_end()
            .justify_between()
            .gap(space::s6())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s1())
                    .child(heading(t, heading_text))
                    .when_some(support, |d, s| d.child(secondary(t, s))),
            )
            .when_some(action, |d, a| d.child(a)),
    )
}

/// A list container with dividers between rows (rows supplied by the page).
pub fn list(t: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .w_full()
        .border_t_1()
        .border_b_1()
        .border_color(t.border)
}

/// One settings row: label + description at left, control at right.
pub fn setting_row(
    t: &Theme,
    label_text: impl Into<SharedString>,
    description: impl Into<SharedString>,
    control: AnyElement,
    note: Option<(Tone, SharedString)>,
) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(space::s8())
        .w_full()
        .py(space::s5())
        .border_b_1()
        .border_color(t.border)
        .child(
            div()
                .flex()
                .flex_col()
                .gap(space::s1())
                .flex_1()
                .min_w_0()
                .max_w(px(440.))
                .child(label(t, label_text))
                .child(supporting(t, description))
                .when_some(note, |d, (tone, text)| {
                    d.child(match tone {
                        Tone::Error => error_line(t, text),
                        _ => supporting(t, text).text_color(t.tone(tone).0),
                    })
                }),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(space::s4())
                .child(control),
        )
}

/// The one-per-page card (hard boundary for the critical control).
pub fn card(t: &Theme) -> Div {
    div()
        .flex()
        .w_full()
        .p(space::s8())
        .rounded(size::radius_container())
        .border_1()
        .border_color(t.border)
        .bg(t.bg_card)
}

/// Clickable row with hover, focus ring and selection bar.
pub fn row_button(t: &Theme, id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    let ring = ring(t);
    let hover = t.hover;
    div()
        .id(id)
        .relative()
        .flex()
        .w_full()
        .tab_index(0)
        .cursor_pointer()
        .when(selected, |d| d.bg(t.selected))
        .when(!selected, move |d| d.hover(move |s| s.bg(hover)))
        .focus(move |s| s.shadow(ring.clone()))
}

/// Underlined text tabs.
pub fn tabs<T: Copy + PartialEq + 'static>(
    t: &Theme,
    id: &'static str,
    items: &[(T, &'static str)],
    current: T,
    on_select: ValueHandler<T>,
) -> Div {
    let (sz, lh) = text::body();
    div()
        .flex()
        .gap(space::s6())
        .w_full()
        .border_b_1()
        .border_color(t.border)
        .children(items.iter().enumerate().map(|(i, (value, name))| {
            let selected = *value == current;
            let value = *value;
            let f = on_select.clone();
            let ring = ring(t);
            div()
                .id(ElementId::NamedInteger(id.into(), i as u64))
                .tab_index(0)
                .cursor_pointer()
                .pb(space::s4())
                .pt(space::s2())
                .text_size(sz)
                .line_height(lh)
                .border_b_2()
                .border_color(if selected {
                    t.accent
                } else {
                    gpui::transparent_black()
                })
                .text_color(if selected {
                    t.text_accent
                } else {
                    t.text_secondary
                })
                .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                .hover(|s| s.opacity(0.85))
                .focus(move |s| s.shadow(ring.clone()))
                .on_click(move |_, w, cx| f(value, w, cx))
                .child(*name)
        }))
}
