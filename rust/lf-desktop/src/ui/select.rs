//! Dropdown select: a 32 px trigger that opens a popover menu. One Tab stop;
//! Up/Down/Home/End move, Enter/Space choose, Escape closes, type-ahead jumps.

use gpui::{
    actions, anchored, deferred, div, prelude::*, px, App, Context, ElementId, EventEmitter,
    FocusHandle, Focusable, FontWeight, KeyBinding, KeyDownEvent, MouseButton, SharedString,
    Window,
};

actions!(
    select,
    [
        MenuUp,
        MenuDown,
        MenuFirst,
        MenuLast,
        MenuConfirm,
        MenuCancel
    ]
);

pub fn bind_keys(cx: &mut App) {
    let c = Some("Select");
    cx.bind_keys([
        KeyBinding::new("up", MenuUp, c),
        KeyBinding::new("down", MenuDown, c),
        KeyBinding::new("home", MenuFirst, c),
        KeyBinding::new("end", MenuLast, c),
        KeyBinding::new("enter", MenuConfirm, c),
        KeyBinding::new("space", MenuConfirm, c),
        KeyBinding::new("escape", MenuCancel, c),
    ]);
}

use super::{icon, ring, Action, FocusRing};
use crate::theme::{elevation, size, space, text, theme};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectEvent {
    Changed(usize),
}

pub struct Select {
    focus: FocusHandle,
    options: Vec<SharedString>,
    selected: Option<usize>,
    highlighted: usize,
    open: bool,
    disabled: bool,
    placeholder: SharedString,
    width: f32,
    /// Opt-in: the trigger fills its container (which sets the bounds)
    /// instead of a fixed `width`; the menu stays at least `width` wide.
    fill: bool,
    /// Opt-in focus decoration of the trigger (see [`FocusRing`]).
    focus_ring: FocusRing,
    /// Opt-in: told of any mouse press inside the open menu (see
    /// [`Select::on_menu_pointer`]).
    on_menu_pointer: Option<Action>,
}

impl EventEmitter<SelectEvent> for Select {}

impl Focusable for Select {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Select {
    pub fn new(
        cx: &mut Context<Self>,
        options: Vec<SharedString>,
        selected: Option<usize>,
    ) -> Self {
        Select {
            focus: cx.focus_handle().tab_stop(true),
            options,
            highlighted: selected.unwrap_or(0),
            selected,
            open: false,
            disabled: false,
            placeholder: "Choose…".into(),
            width: 220.,
            fill: false,
            focus_ring: FocusRing::Default,
            on_menu_pointer: None,
        }
    }

    /// Call `f` on any mouse press inside the open menu (options, padding
    /// or scrolling), before it is handled. The menu is deferred and
    /// occludes what's under it, so the page's own mouse listeners never see
    /// these presses. Nothing is stopped.
    pub fn on_menu_pointer(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_menu_pointer = Some(std::rc::Rc::new(f));
        self
    }

    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }

    /// Fill the container's width (it may shrink below `width` in a narrow
    /// pane; a long choice ends in an ellipsis). The open menu keeps at
    /// least `width`, so full titles stay readable there.
    pub fn fill(mut self) -> Self {
        self.fill = true;
        self
    }

    /// Replace or hide the trigger's focus ring (default: its own).
    pub fn set_focus_ring(&mut self, ring: FocusRing, cx: &mut Context<Self>) {
        if self.focus_ring != ring {
            self.focus_ring = ring;
            cx.notify();
        }
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    #[cfg(test)]
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    pub fn set_options(
        &mut self,
        options: Vec<SharedString>,
        selected: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.options = options;
        self.selected = selected.filter(|i| *i < self.options.len());
        self.highlighted = self.selected.unwrap_or(0);
        cx.notify();
    }

    pub fn set_selected(&mut self, selected: Option<usize>, cx: &mut Context<Self>) {
        self.selected = selected.filter(|i| *i < self.options.len());
        cx.notify();
    }

    pub fn set_disabled(&mut self, d: bool, cx: &mut Context<Self>) {
        self.disabled = d;
        if d {
            self.open = false;
        }
        cx.notify();
    }

    fn choose(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.open = false;
        window.focus(&self.focus);
        if index < self.options.len() && self.selected != Some(index) {
            self.selected = Some(index);
            cx.emit(SelectEvent::Changed(index));
        }
        cx.notify();
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.disabled || self.options.is_empty() {
            return;
        }
        self.open = !self.open;
        self.highlighted = self.selected.unwrap_or(0);
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.options.len();
        if self.disabled || n == 0 {
            return;
        }
        if !self.open {
            self.open = true;
            self.highlighted = self.selected.unwrap_or(0);
        } else {
            self.highlighted = (self.highlighted as isize + delta).rem_euclid(n as isize) as usize;
        }
        cx.notify();
    }

    fn menu_up(&mut self, _: &MenuUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn menu_down(&mut self, _: &MenuDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn menu_first(&mut self, _: &MenuFirst, _: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.highlighted = 0;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn menu_last(&mut self, _: &MenuLast, _: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.highlighted = self.options.len().saturating_sub(1);
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn menu_confirm(&mut self, _: &MenuConfirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            let h = self.highlighted;
            self.choose(h, window, cx);
        } else {
            self.toggle(cx);
        }
    }

    fn menu_cancel(&mut self, _: &MenuCancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    /// Type-ahead while the menu is open.
    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.open || event.keystroke.modifiers.modified() {
            return;
        }
        let n = self.options.len();
        if let Some(ch) = event.keystroke.key_char.as_deref() {
            let ch = ch.to_lowercase();
            let start = self.highlighted + 1;
            if let Some(i) = (0..n)
                .map(|k| (start + k) % n)
                .find(|i| self.options[*i].to_lowercase().starts_with(&ch))
            {
                self.highlighted = i;
                cx.stop_propagation();
                cx.notify();
            }
        }
    }
}

impl Render for Select {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let (sz, lh) = text::body();
        let focused = self.focus.is_focused(window);
        if !focused && self.open && !window.is_window_active() {
            self.open = false;
        }
        let current = self
            .selected
            .and_then(|i| self.options.get(i).cloned())
            .unwrap_or_else(|| self.placeholder.clone());
        let fg = if self.disabled {
            t.text_disabled
        } else {
            t.text
        };
        let hover = t.hover;
        let trigger_hover = t.bg_muted;
        // A hidden ring leaves a focused trigger looking unfocused.
        let ring_style = self.focus_ring.shadows(|| ring(&t)).filter(|_| focused);
        let control_shadow = elevation::control(&t);
        let trigger = div()
            .id("select-trigger")
            .flex()
            .items_center()
            .justify_between()
            .gap(space::s4())
            .map(|d| {
                if self.fill {
                    d.w_full().min_w_0()
                } else {
                    d.w(px(self.width))
                }
            })
            .h(size::control_md())
            .px(space::s5())
            .rounded(size::radius_element())
            .border_1()
            .border_color(if self.disabled {
                t.bg_muted
            } else {
                t.border_control
            })
            .bg(if self.disabled {
                t.bg_muted
            } else {
                t.bg_control
            })
            .text_size(sz)
            .line_height(lh)
            .text_color(fg)
            .when(ring_style.is_none() && !self.disabled, move |d| {
                d.shadow(control_shadow)
            })
            .when_some(ring_style, |d, ring| d.shadow(ring))
            .when(!self.disabled, move |d| {
                d.cursor_pointer().hover(move |s| s.bg(trigger_hover))
            })
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.focus);
                this.toggle(cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .when(self.fill, |d| d.text_ellipsis())
                    .child(current),
            )
            .child(icon(
                "chevron-down",
                if self.disabled {
                    t.text_disabled
                } else {
                    t.icon_secondary
                },
            ));

        let menu = self.open.then(|| {
            let items = self.options.iter().cloned().enumerate().map(|(i, option)| {
                let chosen = self.selected == Some(i);
                let lit = self.highlighted == i;
                div()
                    .id(ElementId::NamedInteger("select-option".into(), i as u64))
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .px(space::s4())
                    .h(size::control_md())
                    .rounded(size::radius_inner())
                    .cursor_pointer()
                    .text_size(sz)
                    .text_color(t.text)
                    .when(lit, |d| d.bg(t.selected))
                    .when(chosen, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.choose(i, window, cx)))
                    .child(
                        div()
                            .w(px(16.))
                            .flex_none()
                            .when(chosen, |d| d.child(icon("check", t.icon_accent))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(option),
                    )
            });
            deferred(
                anchored().snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("select-menu")
                        .occlude()
                        .mt(space::s2())
                        .min_w(px(self.width))
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .p(space::s2())
                        .rounded(size::radius_group())
                        .border_1()
                        .border_color(t.border)
                        .bg(t.bg_popover)
                        .shadow(elevation::popover(&t))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.open = false;
                            cx.notify();
                        }))
                        .when_some(self.on_menu_pointer.clone(), |d, f| {
                            d.capture_any_mouse_down(move |_, window, cx| f(window, cx))
                        })
                        .children(items),
                ),
            )
            .with_priority(2)
        });

        div()
            .key_context("Select")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(Self::menu_up))
            .on_action(cx.listener(Self::menu_down))
            .on_action(cx.listener(Self::menu_first))
            .on_action(cx.listener(Self::menu_last))
            .on_action(cx.listener(Self::menu_confirm))
            .on_action(cx.listener(Self::menu_cancel))
            .on_mouse_down(MouseButton::Left, |_, _, _| {})
            .flex()
            .flex_col()
            .map(|d| {
                if self.fill {
                    d.w_full().min_w_0()
                } else {
                    d.flex_none()
                }
            })
            .child(trigger)
            .children(menu)
    }
}
