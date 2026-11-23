//! Reusable native components. Pages build only from these and theme tokens.
//!
//! The look follows macOS: inset grouped lists on a calm surface, neutral
//! controls with a hairline and a faint shadow, one accent for the primary
//! action, selection and focus.

pub mod assets;
pub mod input;
pub mod select;
pub mod text_edit;

use std::rc::Rc;

use gpui::{
    div, point, prelude::*, px, svg, AnyElement, App, BoxShadow, ClickEvent, Div, ElementId,
    FocusHandle, FontWeight, Hsla, IntoElement, Pixels, SharedString, Stateful, Window,
};

use crate::theme::{elevation, size, space, text, theme, Theme, Tone};

pub type Handler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// Callback with a value (switch state, chosen index, tab).
pub type ValueHandler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;
/// Callback without an event (toast actions, confirmations).
pub type Action = Rc<dyn Fn(&mut Window, &mut App)>;

/// Wrap a closure as a click handler.
pub fn handler(f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Handler {
    Rc::new(f)
}

/// Blur of [`surface_ring`]'s shadows. Never zero: GPUI 0.2.2's Linux
/// (blade) and Windows shadow shaders integrate the blur over
/// `±3 × blur_radius`, so a zero radius gives a zero-width range and paints
/// nothing (only the macOS shader special-cases zero). A sub-pixel blur
/// still draws a crisp edge.
const RING_BLUR: f32 = 0.5;

/// The ring's two tokenized shadows: a solid 2 px accent edge inside a soft
/// accent halo (drawn outside the box; no layout shift).
fn ring_with_blur(t: &Theme, blur: f32) -> Vec<BoxShadow> {
    [(t.focus_ring, 4.), (t.icon_accent, 2.)]
        .into_iter()
        .map(|(color, spread)| BoxShadow {
            color,
            offset: point(px(0.), px(0.)),
            blur_radius: px(blur),
            spread_radius: px(spread),
        })
        .collect()
}

/// The focus ring.
pub fn ring(t: &Theme) -> Vec<BoxShadow> {
    ring_with_blur(t, 0.)
}

/// The focus ring with a blur every platform paints. Shadows are filled
/// shapes painted under the element, so use it only on an element whose
/// fill is opaque in every state (or the ring shows through as a fill).
pub fn surface_ring(t: &Theme) -> Vec<BoxShadow> {
    ring_with_blur(t, RING_BLUR)
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

/// A rounded square holding a glyph (Home's status tile, empty states).
pub fn glyph_tile(name: &str, fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(40.))
        .rounded(size::radius_group())
        .bg(bg)
        .child(icon_lg(name, fg))
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
    /// Opaque surface the button sits on, when it opts into
    /// [`surface_ring`] (see [`Button::surface_ring`]).
    surface: Option<Hsla>,
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
            surface: None,
        }
    }
    /// Draw [`surface_ring`] when focused. A ghost button fills with
    /// `surface` (the opaque fill it sits on), its hover and pressed tints
    /// pre-blended onto it, so the ring stays an edge.
    pub fn surface_ring(mut self, surface: Hsla) -> Self {
        self.surface = Some(surface);
        self
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
        let clear = gpui::transparent_black();
        // (fill, label, hairline, hover fill, pressed fill)
        let (bg, fg, border, hover, pressed) = match self.variant {
            Variant::Primary => (
                t.accent,
                t.text_on_accent,
                t.accent,
                t.accent_hover,
                t.accent_hover,
            ),
            Variant::Secondary => (t.bg_control, t.text, t.separator, t.bg_muted, t.bg_muted),
            Variant::Ghost => match self.surface {
                Some(s) => (s, t.text_accent, s, s.blend(t.hover), s.blend(t.pressed)),
                None => (clear, t.text_accent, clear, t.hover, t.pressed),
            },
            Variant::Destructive => {
                let (fg, bg) = t.tone(Tone::Error);
                (bg, fg, bg, bg.opacity(0.8), bg.opacity(0.65))
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
        let raised = self.variant == Variant::Secondary && !self.disabled;
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
        let ring = if self.surface.is_some() {
            surface_ring(&t)
        } else {
            ring(&t)
        };
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
            .when(raised, |d| d.shadow(elevation::control(&t)))
            .text_color(fg)
            .text_size(sz)
            .line_height(lh)
            .font_weight(FontWeight::MEDIUM)
            .whitespace_nowrap()
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
                .active(move |s| s.bg(pressed))
                .focus(move |s| s.shadow(ring.clone()));
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
        let (hover, pressed) = (t.hover, t.pressed);
        let ring = ring(&t);
        let label = self.label.clone();
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(size::control_sm())
            .rounded(size::radius_inner())
            .child(icon(self.icon, color))
            .tooltip(move |_, cx| Tooltip::view(label.clone(), cx));
        if !self.disabled {
            el = el
                .tab_index(0)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .active(move |s| s.bg(pressed))
                .focus(move |s| s.shadow(ring.clone()));
            if let Some(f) = self.on_click {
                el = el.on_click(move |e, w, cx| f(e, w, cx));
            }
        }
        el
    }
}

/// Tooltip view: a small popover with a hairline and a soft shadow.
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
            .border_1()
            .border_color(t.border)
            .bg(t.bg_popover)
            .shadow(elevation::popover(&t))
            .text_color(t.text)
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
        // Off keeps a >= 3:1 boundary; disabled drops the accent entirely.
        let (track, border) = match (self.on, self.disabled) {
            (true, false) => (t.accent, t.accent),
            (false, false) => (t.bg_muted, t.border_control),
            (true, true) => (t.text_disabled, t.text_disabled),
            (false, true) => (t.bg_muted, t.text_disabled),
        };
        let knob = t.knob;
        let knob_shadow = elevation::raised(&t);
        let ring = ring(&t);
        let on = self.on;
        let label = self.label.clone();
        let mut el = div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .w(px(38.))
            .h(px(22.))
            .px(px(1.))
            .rounded_full()
            .border_1()
            .border_color(border)
            .bg(track)
            .when(on, |d| d.justify_end())
            .child(
                div()
                    .size(px(18.))
                    .rounded_full()
                    .bg(knob)
                    .shadow(knob_shadow)
                    .when(self.disabled, |d| d.opacity(0.7)),
            )
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

/// One segment of a muted track: the selected one is a raised surface with
/// a shadow and a heavier weight (never colour alone).
fn segment(
    t: &Theme,
    id: ElementId,
    label: impl IntoElement,
    selected: bool,
    disabled: bool,
) -> Stateful<Div> {
    let (sz, lh) = text::body();
    // A disabled selection stays recognizable (fill, shadow, weight) but sits
    // on the control fill with secondary text so its label stays readable;
    // the raised fill is too close to the disabled text colour in Dark.
    let fg = if disabled && selected {
        t.text_secondary
    } else if disabled {
        t.text_disabled
    } else if selected {
        t.text
    } else {
        t.tone(Tone::Neutral).0
    };
    let ring = ring(t);
    let hover = t.hover;
    let mut seg = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px(space::s5())
        .rounded(size::radius_inner())
        .text_size(sz)
        .line_height(lh)
        .text_color(fg)
        .whitespace_nowrap()
        .when(selected, |d| {
            d.bg(if disabled { t.bg_control } else { t.bg_raised })
                .shadow(elevation::raised(t))
                .font_weight(FontWeight::SEMIBOLD)
        })
        .when(!selected, |d| d.font_weight(FontWeight::MEDIUM))
        .child(label);
    if !disabled {
        seg = seg
            .tab_index(0)
            .cursor_pointer()
            .when(!selected, move |d| d.hover(move |s| s.bg(hover)))
            .focus(move |s| s.shadow(ring.clone()));
    }
    seg
}

/// The muted track segments sit on.
fn segment_track(t: &Theme) -> Div {
    div()
        .flex()
        .flex_none()
        .h(size::control_md())
        .p(px(2.))
        .gap(px(2.))
        .rounded(size::radius_element())
        .bg(t.bg_muted)
}

impl RenderOnce for Segmented {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = theme(cx).clone();
        let id = self.id.clone();
        let disabled = self.disabled;
        let on_select = self.on_select.clone();
        let selected_index = self.selected;
        segment_track(&t)
            .id(self.id)
            .children(self.options.into_iter().enumerate().map(|(i, option)| {
                let selected = selected_index == Some(i);
                let mut seg = segment(
                    &t,
                    ElementId::NamedInteger(format!("{id:?}-seg").into(), i as u64),
                    option,
                    selected,
                    disabled,
                );
                if !disabled {
                    if let Some(f) = on_select.clone() {
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
        .bg(t.bg_muted)
        .shadow(elevation::control(t))
        .font_family(text::MONO_FAMILY)
        .text_size(sz)
        .font_weight(FontWeight::MEDIUM)
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
        .items_center()
        .gap(space::s2())
        .children(combo.split(" + ").map(|k| keycap(t, k.to_string())))
}

/// The bare dot of a status tone.
pub fn dot(t: &Theme, tone: Tone) -> Div {
    div()
        .size(px(8.))
        .flex_none()
        .rounded_full()
        .bg(t.dot(tone))
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
        .child(dot(t, tone))
        .child(word.into())
}

/// A tinted capsule.
pub fn token(t: &Theme, tone: Tone, s: impl Into<SharedString>) -> Div {
    let (fg, bg) = t.tone(tone);
    let (sz, lh) = text::supporting();
    div()
        .flex_none()
        .px(space::s4())
        .py(px(1.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(sz)
        .line_height(lh)
        .font_weight(FontWeight::MEDIUM)
        .whitespace_nowrap()
        .child(s.into())
}

pub fn badge(t: &Theme, n: usize) -> Div {
    let (sz, _) = text::supporting();
    div()
        .flex_none()
        .min_w(px(20.))
        .h(px(18.))
        .px(space::s3())
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
/// The fill is a non-text mark, so it uses `icon_accent` at full opacity
/// (>= 3:1 on the muted track in both palettes).
pub fn progress(t: &Theme, fraction: Option<f32>) -> Div {
    let fill = match fraction {
        Some(f) => gpui::relative(f.clamp(0.0, 1.0)),
        None => gpui::relative(0.3),
    };
    div()
        .w_full()
        .h(px(4.))
        .flex_none()
        .rounded_full()
        .bg(t.bg_muted)
        .overflow_hidden()
        .child(div().h_full().w(fill).rounded_full().bg(t.icon_accent))
}

/// A 6 px microphone level meter (0.0–1.0) in a tone's dot colour.
pub fn level_meter(t: &Theme, level: f32, tone: Tone) -> Div {
    div()
        .w_full()
        .h(px(6.))
        .flex_none()
        .rounded_full()
        .bg(t.bg_muted)
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(gpui::relative(level.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(t.dot(tone)),
        )
}

/// A persistent condition strip: tinted, icon-led, optional action.
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
        .min_h(size::row())
        .px(space::s6())
        .py(space::s4())
        .rounded(size::radius_group())
        .bg(bg)
        .text_color(t.text)
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
        .gap(space::s3())
        .py(space::s12())
        .px(space::s8())
        .w_full()
        .child(
            div()
                .pb(space::s2())
                .child(glyph_tile(icon_name, t.icon_secondary, t.bg_muted)),
        )
        .child(heading(t, heading_text))
        .child(secondary(t, detail).text_center().max_w(px(420.)))
        .when_some(action, |d, a| d.child(div().pt(space::s4()).child(a)))
}

/// Section: heading, optional supporting line and action, then content
/// (usually one group).
pub fn section(
    t: &Theme,
    heading_text: impl Into<SharedString>,
    support: Option<SharedString>,
    action: Option<AnyElement>,
) -> Div {
    div().flex().flex_col().gap(space::s4()).w_full().child(
        div()
            .flex()
            .items_end()
            .justify_between()
            .gap(space::s6())
            .min_h(size::control_sm())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s1())
                    .min_w_0()
                    .child(heading(t, heading_text))
                    .when_some(support, |d, s| d.child(supporting(t, s))),
            )
            .when_some(action, |d, a| d.child(a)),
    )
}

// ----------------------------------------------------------------- groups ---

/// An inset grouped container: card fill, group radius, hairline, clipped.
/// The 4 px inner padding keeps row hover fills inside the rounded corners
/// and leaves room for a focused row's ring, so the clip never hides it.
pub fn group(t: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .w_full()
        .p(space::s2())
        .rounded(size::radius_group())
        .border_1()
        .border_color(t.border)
        .bg(t.bg_card)
        .overflow_hidden()
}

/// A hairline that starts `inset` from the group's left edge.
pub fn separator(t: &Theme, inset: Pixels) -> Div {
    div().h(px(1.)).flex_none().ml(inset).bg(t.separator)
}

/// The horizontal inset of rows inside a group.
pub fn row_inset() -> Pixels {
    space::s6()
}

/// A group whose rows are separated by inset hairlines starting at the text
/// line (`inset` from the left edge; [`row_inset`] for rows without a
/// leading glyph).
pub fn group_rows(t: &Theme, rows: impl IntoIterator<Item = AnyElement>, inset: Pixels) -> Div {
    let mut out = group(t);
    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            out = out.child(separator(t, inset));
        }
        out = out.child(row);
    }
    out
}

/// A list container (rows supplied by the page, which separates them).
pub fn list(t: &Theme) -> Div {
    group(t)
}

/// One settings row: label + description at left, control at right. Sits
/// inside a group (see [`group_rows`]).
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
        .min_h(size::row())
        .px(row_inset())
        .py(space::s5())
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
        .p(space::s7())
        .rounded(size::radius_container())
        .border_1()
        .border_color(t.border)
        .bg(t.bg_card)
}

/// Clickable row with hover, focus ring and a soft selection fill.
pub fn row_button(t: &Theme, id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    let ring = ring(t);
    let (hover, pressed) = (t.hover, t.pressed);
    div()
        .id(id)
        .relative()
        .flex()
        .w_full()
        .tab_index(0)
        .cursor_pointer()
        .when(selected, |d| d.bg(t.selected))
        .when(!selected, move |d| d.hover(move |s| s.bg(hover)))
        .active(move |s| s.bg(pressed))
        .focus(move |s| s.shadow(ring.clone()))
}

/// Page tabs as a quiet wrapping strip of capsules, deliberately unlike the
/// [`Segmented`] control: no track, the selected tab is a filled capsule in
/// a heavier weight (not colour alone), the others are plain secondary text.
/// Wraps onto a second line rather than overflowing a narrow window.
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
        .flex_wrap()
        .items_center()
        .gap(space::s2())
        .w_full()
        .children(items.iter().enumerate().map(|(i, (value, name))| {
            let selected = *value == current;
            let value = *value;
            let f = on_select.clone();
            let ring = ring(t);
            let hover = t.hover;
            let strong = t.text;
            div()
                .id(ElementId::NamedInteger(id.into(), i as u64))
                .tab_index(0)
                .cursor_pointer()
                .flex()
                .flex_none()
                .items_center()
                .h(size::control_sm())
                .px(space::s5())
                .rounded_full()
                .text_size(sz)
                .line_height(lh)
                .whitespace_nowrap()
                .when(selected, |d| {
                    d.bg(t.bg_muted)
                        .text_color(t.text)
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .when(!selected, move |d| {
                    d.text_color(t.text_secondary)
                        .font_weight(FontWeight::MEDIUM)
                        .hover(move |s| s.bg(hover).text_color(strong))
                })
                .focus(move |s| s.shadow(ring.clone()))
                .on_click(move |_, w, cx| f(value, w, cx))
                .child(*name)
        }))
}
