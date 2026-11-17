//! AppKit components using LocalFlow's shared semantic tokens (port of
//! `localflow/mac_style.py`).
//!
//! Colours always come from a [`Palette`] and are applied to layers when a
//! control is *styled*, so callers re-run the `style_*` functions with the
//! window's current palette on every render (see [`tokens_for_window`]); that
//! is how dark mode is reapplied.
//!
//! Everything here must run on the main thread: constructors take a
//! [`MainThreadMarker`].
#![allow(deprecated)]

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSBezierPath, NSButton, NSButtonType, NSColor,
    NSControl, NSControlSize, NSControlStateValueOff, NSControlStateValueOn, NSFocusRingType,
    NSFont, NSFontAttributeName, NSFontWeightBold, NSFontWeightRegular, NSFontWeightSemibold,
    NSForegroundColorAttributeName, NSGraphicsContext, NSLineBreakMode, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSRectFill, NSTextAlignment, NSTextField, NSTextView, NSView,
    NSWindow,
};
use objc2_foundation::{NSArray, NSDictionary, NSPoint, NSRect, NSSize, NSString};

use crate::text_wrap::TextMeasure;
use crate::tokens::{
    halftone_dots, input_inner_frame, surface_tint, switch_colors, switch_parts, tokens_for,
    Palette, Weight, ACTION_HEIGHT, ACTION_PADDING, INPUT_BORDER_WIDTH, INPUT_HEIGHT, LIGHT,
    PAGE_TITLE_HEIGHT, PILL_BORDER_WIDTH, PILL_HEIGHT, PILL_PADDING, PILL_TINT, RADIUS_BUTTON,
    RADIUS_CARD, RADIUS_PILL, SHADOW_OFFSET, SUN_TEXT, SWITCH_BORDER_WIDTH, TYPE_CAPTION,
    TYPE_DISPLAY,
};

// ------------------------------------------------------------------ helpers ---

/// `NSMakeRect`.
pub fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// A frame tuple `(x, y, w, h)` (the layout modules' `Rect`) as an `NSRect`.
pub fn rect_of(frame: (f64, f64, f64, f64)) -> NSRect {
    rect(frame.0, frame.1, frame.2, frame.3)
}

/// Whether the window is currently drawn in the dark appearance, and so which
/// palette to use. Call this on every render: the Hub re-renders on
/// `windowDidChangeEffectiveAppearance:`.
pub fn tokens_for_window(window: &NSWindow) -> Palette {
    let appearance = window.effectiveAppearance();
    // SAFETY: the appearance-name constants are immutable globals.
    let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
    let names = NSArray::from_slice(&[aqua, dark]);
    let best = appearance.bestMatchFromAppearancesWithNames(&names);
    tokens_for(match best {
        Some(name) if &*name == dark => "dark",
        _ => "light",
    })
}

fn channels(hex: &str) -> (f64, f64, f64) {
    let value = hex.trim_start_matches('#');
    let part = |index: usize| {
        value
            .get(index..index + 2)
            .and_then(|digits| u8::from_str_radix(digits, 16).ok())
            .map_or(0.0, |byte| f64::from(byte) / 255.0)
    };
    (part(0), part(2), part(4))
}

/// An opaque calibrated colour from `#RRGGBB`.
pub fn hex_color(hex: &str) -> Retained<NSColor> {
    let (r, g, b) = channels(hex);
    NSColor::colorWithCalibratedRed_green_blue_alpha(r, g, b, 1.0)
}

/// `color(token, palette)`: a palette token name (`"ink"`) or a literal hex.
pub fn color(token: &str, palette: &Palette) -> Retained<NSColor> {
    hex_color(palette.get(token).unwrap_or(token))
}

fn cg(color: &NSColor) -> Retained<objc2_core_graphics::CGColor> {
    color.CGColor()
}

#[allow(unused_imports)]
use objc2::AllocAnyThread as _;
/// Traits whose methods the styling code calls.
#[allow(unused_imports)]
use objc2_app_kit::{NSAccessibility as _, NSAppearanceCustomization as _, NSStringDrawing as _};

/// The system font at `size` and `weight`.
pub fn font(size: f64, weight: Weight) -> Retained<NSFont> {
    // SAFETY: immutable global constants.
    let w = unsafe {
        match weight {
            Weight::Regular => NSFontWeightRegular,
            Weight::Semibold => NSFontWeightSemibold,
            Weight::Bold => NSFontWeightBold,
        }
    };
    NSFont::systemFontOfSize_weight(size, w)
}

fn attributes(pairs: &[(&NSString, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<&NSString> = pairs.iter().map(|(k, _)| *k).collect();
    let values: Vec<&AnyObject> = pairs.iter().map(|(_, v)| *v).collect();
    NSDictionary::from_slices(&keys, &values)
}

/// Width in points of `text` drawn on one line in `text_font` (rounded up).
pub fn measure_text(text: &str, text_font: &NSFont) -> f64 {
    // SAFETY: the attribute-name constant is an immutable global.
    let key = unsafe { NSFontAttributeName };
    let attrs = attributes(&[(key, text_font.as_ref())]);
    // SAFETY: `sizeWithAttributes:` only reads the attribute dictionary.
    let size = unsafe { NSString::from_str(text).sizeWithAttributes(Some(&attrs)) };
    size.width.ceil()
}

/// Draw `text` with `attrs` into `frame` (inside `drawRect:`).
fn draw_text(text: &str, frame: NSRect, attrs: &NSDictionary<NSString, AnyObject>) {
    // SAFETY: only called from `drawRect:` on the main thread.
    unsafe { NSString::from_str(text).drawInRect_withAttributes(frame, Some(attrs)) }
}

/// [`measure_text`] as the step-1 layout closure: `(text, size, weight) -> width`.
///
/// Pass `&text_measure()` wherever a [`TextMeasure`] is expected.
pub fn text_measure() -> impl Fn(&str, f64, Weight) -> f64 {
    |text: &str, size: f64, weight: Weight| measure_text(text, &font(size, weight))
}

/// Boxed form of [`text_measure`] for storing in controllers.
pub fn boxed_measure() -> Box<MeasureFn> {
    Box::new(text_measure())
}

/// The unsized measure closure type.
pub type MeasureFn = dyn Fn(&str, f64, Weight) -> f64;

/// Borrow a boxed measure as the [`TextMeasure`] reference the layout code takes.
pub fn as_measure(measure: &dyn Fn(&str, f64, Weight) -> f64) -> TextMeasure<'_> {
    measure
}

// -------------------------------------------------------------- view classes ---

define_class!(
    /// An `NSView` whose origin is the top-left corner.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "LFFlippedView"]
    pub struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl FlippedView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(());
        // SAFETY: plain `initWithFrame:` on the NSView superclass.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// Mutable halftone parameters (`anchor`, `step`, `max_radius`, `pad`).
#[derive(Clone, Copy, Debug)]
pub struct HalftoneState {
    pub palette: Palette,
    pub anchor: &'static str,
    pub step: i32,
    pub max_radius: i32,
    pub pad: i32,
}

define_class!(
    /// Sun halftone decoration. Dots come from [`halftone_dots`], which only
    /// returns dots inside the bounds, and drawing is clipped to the bounds
    /// too, so the pattern never paints over neighbouring views. Decorative:
    /// hidden from accessibility.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Cell<HalftoneState>]
    #[name = "LFHalftoneView"]
    pub struct HalftoneView;

    impl HalftoneView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let state = self.ivars().get();
            let bounds = self.bounds();
            NSGraphicsContext::saveGraphicsState_class();
            NSBezierPath::clipRect(bounds);
            color("paper", &state.palette).set();
            NSRectFill(bounds);
            color("sun", &state.palette).set();
            for (x, y, diameter) in halftone_dots(
                bounds.size.width,
                bounds.size.height,
                state.step,
                state.max_radius,
                state.pad,
                state.anchor,
            ) {
                NSBezierPath::bezierPathWithOvalInRect(rect(x, y, diameter, diameter)).fill();
            }
            NSGraphicsContext::restoreGraphicsState_class();
        }

        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            false
        }
    }
);

impl HalftoneView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, palette: Palette) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(Cell::new(HalftoneState {
            palette,
            anchor: "bottom-left",
            step: 14,
            max_radius: 4,
            pad: 8,
        }));
        // SAFETY: plain `initWithFrame:` on the NSView superclass.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // macOS 14+ no longer clips views to their bounds by default.
        this.setClipsToBounds(true);
        this
    }

    /// Re-tint after an appearance change.
    pub fn set_palette(&self, palette: Palette) {
        let mut state = self.ivars().get();
        state.palette = palette;
        self.ivars().set(state);
        self.setNeedsDisplay(true);
    }

    pub fn set_anchor(&self, anchor: &'static str) {
        let mut state = self.ivars().get();
        state.anchor = anchor;
        self.ivars().set(state);
        self.setNeedsDisplay(true);
    }
}

/// Ivars of [`PlaceholderTextView`].
#[derive(Default)]
pub struct PlaceholderState {
    pub placeholder: RefCell<String>,
    pub color: RefCell<Option<Retained<NSColor>>>,
}

define_class!(
    /// An `NSTextView` that draws a muted placeholder while it holds no text.
    ///
    /// The placeholder is only drawn, never inserted: `string()` stays empty,
    /// so it cannot be copied, autosaved or sent to a model.
    #[unsafe(super(NSTextView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = PlaceholderState]
    #[name = "LFPlaceholderTextView"]
    pub struct PlaceholderTextView;

    impl PlaceholderTextView {
        #[unsafe(method(didChangeText))]
        fn did_change_text(&self) {
            // SAFETY: forwards to NSTextView.
            let () = unsafe { msg_send![super(self), didChangeText] };
            // Typing the first character / deleting the last one must repaint
            // the whole view, not just the edited glyph rect.
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(setString:))]
        fn set_string_override(&self, text: &NSString) {
            // SAFETY: forwards to NSTextView.
            let () = unsafe { msg_send![super(self), setString: text] };
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            // SAFETY: forwards to NSTextView.
            let () = unsafe { msg_send![super(self), drawRect: dirty] };
            let state = self.ivars();
            let placeholder = state.placeholder.borrow();
            if !self.string().is_empty() || placeholder.is_empty() {
                return;
            }
            let inset = self.textContainerInset();
            // SAFETY: the container is only read on the main thread.
            let padding = unsafe { self.textContainer() }
                .map_or(0.0, |container| container.lineFragmentPadding());
            let bounds = self.bounds();
            let width = bounds.size.width - 2.0 * (inset.width + padding);
            let text_font = self.font().unwrap_or_else(|| font(13.0, Weight::Regular));
            let tint = state
                .color
                .borrow()
                .clone()
                .unwrap_or_else(NSColor::placeholderTextColor);
            // SAFETY: attribute-name constants are immutable globals.
            let (font_key, color_key) = unsafe { (NSFontAttributeName, NSForegroundColorAttributeName) };
            let attrs = attributes(&[(font_key, text_font.as_ref()), (color_key, tint.as_ref())]);
            draw_text(
                &placeholder,
                rect(
                    inset.width + padding,
                    inset.height,
                    width.max(0.0),
                    bounds.size.height - 2.0 * inset.height,
                ),
                &attrs,
            );
        }
    }
);

impl PlaceholderTextView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(PlaceholderState::default());
        // SAFETY: plain `initWithFrame:` on the NSTextView superclass.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Set the drawn placeholder and its colour (`None`: the system colour).
    pub fn set_placeholder(&self, text: &str, color: Option<Retained<NSColor>>) {
        *self.ivars().placeholder.borrow_mut() = text.to_string();
        *self.ivars().color.borrow_mut() = color;
        self.setNeedsDisplay(true);
    }

    pub fn placeholder(&self) -> String {
        self.ivars().placeholder.borrow().clone()
    }
}

define_class!(
    /// Settings switch: a switch-type `NSButton` (native toggle, keyboard,
    /// target/action, state and accessibility) that draws only its own track,
    /// knob and On/Off word from palette tokens; the button's own drawing
    /// never runs.
    #[unsafe(super(NSButton))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Cell<Palette>]
    #[name = "LFCobaltSwitch"]
    pub struct CobaltSwitch;

    impl CobaltSwitch {
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            false // always drawRect:, never the cell's layer drawing
        }

        #[unsafe(method_id(accessibilityValueDescription))]
        fn accessibility_value_description(&self) -> Option<Retained<NSString>> {
            Some(NSString::from_str(if self.is_on() { "On" } else { "Off" }))
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let on = self.is_on();
            let parts = switch_parts(bounds.size.width, bounds.size.height, on);
            let palette = self.ivars().get();
            let Ok(colors) = switch_colors(&palette, on, self.isEnabled()) else {
                return;
            };
            let paint = |hex: &str| {
                let base = color(hex, &palette);
                NSColor::colorWithAlphaComponent(&base, colors.alpha)
            };
            // SAFETY: drawing happens inside `drawRect:` on the main thread.
            unsafe {
                NSGraphicsContext::saveGraphicsState_class();
                let (x, y, w, h) = parts.track;
                let half = f64::from(SWITCH_BORDER_WIDTH) / 2.0; // keep the stroke inside
                let radius = (parts.radius - half).max(0.0);
                let track = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    rect(x + half, y + half, w - 2.0 * half, h - 2.0 * half),
                    radius,
                    radius,
                );
                paint(colors.track).set();
                track.fill();
                paint(colors.outline).set();
                track.setLineWidth(f64::from(SWITCH_BORDER_WIDTH));
                track.stroke();
                let knob = NSBezierPath::bezierPathWithOvalInRect(rect_of(parts.knob));
                paint(colors.knob).set();
                knob.fill();
                paint(colors.outline).set();
                knob.setLineWidth(1.0);
                knob.stroke();
                let style = NSMutableParagraphStyle::new();
                style.setAlignment(NSTextAlignment::Right);
                let text_font = font(12.0, Weight::Semibold);
                let text_color = paint(colors.text);
                let (font_key, color_key, style_key) = (
                    NSFontAttributeName,
                    NSForegroundColorAttributeName,
                    NSParagraphStyleAttributeName,
                );
                let attrs = attributes(&[
                    (font_key, text_font.as_ref()),
                    (color_key, text_color.as_ref()),
                    (style_key, style.as_ref()),
                ]);
                draw_text(parts.label, rect_of(parts.text), &attrs);
                NSGraphicsContext::restoreGraphicsState_class();
            }
        }

        #[unsafe(method(drawFocusRingMask))]
        fn draw_focus_ring_mask(&self) {
            let bounds = self.bounds();
            let parts = switch_parts(bounds.size.width, bounds.size.height, self.is_on());
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                rect_of(parts.track),
                parts.radius,
                parts.radius,
            )
            .fill();
        }

        #[unsafe(method(focusRingMaskBounds))]
        fn focus_ring_mask_bounds(&self) -> NSRect {
            let bounds = self.bounds();
            rect_of(switch_parts(bounds.size.width, bounds.size.height, false).track)
        }
    }
);

impl CobaltSwitch {
    /// A configured switch: native switch-type button with an accessibility
    /// label, drawn from `palette`.
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        palette: Palette,
        on: bool,
        label: &str,
    ) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(Cell::new(palette));
        // SAFETY: plain `initWithFrame:` on the NSButton superclass.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setButtonType(NSButtonType::Switch);
        this.setBordered(false);
        this.setTitle(&NSString::from_str(""));
        this.set_on(on);
        this.setAccessibilityLabel(Some(&NSString::from_str(label)));
        this
    }

    pub fn is_on(&self) -> bool {
        self.state() == NSControlStateValueOn
    }

    pub fn set_on(&self, on: bool) {
        self.setState(if on {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        self.redraw();
    }

    /// Re-tint after an appearance change.
    pub fn set_palette(&self, palette: Palette) {
        self.ivars().set(palette);
        self.redraw();
    }

    fn redraw(&self) {
        // `NSControl` has its own argument-less `setNeedsDisplay`; go through
        // `NSView` for the dirty-flag variant.
        let view: &NSView = self;
        view.setNeedsDisplay(true);
    }
}

// ----------------------------------------------------------------- styling ---

/// The visual kinds of [`style_button`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
    DestructiveGhost,
    Selected,
    Sidebar,
    SidebarIdle,
    Destructive,
}

fn set_layer_colors(
    view: &NSView,
    background: Option<&NSColor>,
    border: &NSColor,
    border_width: f64,
    radius: f64,
) {
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        let clear = NSColor::clearColor();
        layer.setBackgroundColor(Some(&cg(background.unwrap_or(&clear))));
        layer.setBorderColor(Some(&cg(border)));
        layer.setBorderWidth(border_width);
        layer.setCornerRadius(radius);
    }
}

fn set_shadow(view: &NSView, palette: &Palette, on: bool) {
    if let Some(layer) = view.layer() {
        if on {
            layer.setShadowColor(Some(&cg(&color("shadow", palette))));
            layer.setShadowOffset(NSSize::new(
                f64::from(SHADOW_OFFSET),
                -f64::from(SHADOW_OFFSET),
            ));
            layer.setShadowRadius(0.0);
            layer.setShadowOpacity(1.0);
        } else {
            layer.setShadowOpacity(0.0);
        }
    }
}

/// Token styling of a native button (`style_button`).
pub fn style_button(button: &NSButton, kind: ButtonKind, palette: &Palette) {
    let (background, foreground): (&str, &str) = match kind {
        ButtonKind::Primary => (palette.cobalt, "#FFFFFF"),
        ButtonKind::Secondary => (palette.surface, palette.ink),
        ButtonKind::Ghost => (palette.paper, palette.muted),
        ButtonKind::DestructiveGhost => (palette.paper, palette.red),
        ButtonKind::Selected => (palette.ink, palette.paper),
        ButtonKind::Sidebar => (palette.sun, SUN_TEXT),
        ButtonKind::SidebarIdle => (palette.sidebar, palette.sidebar_text),
        ButtonKind::Destructive => (palette.red, "#FFFFFF"),
    };
    let ghost = matches!(kind, ButtonKind::Ghost | ButtonKind::DestructiveGhost);
    button.setBordered(false);
    button.setFocusRingType(NSFocusRingType::Exterior);
    button.setFont(Some(&font(
        13.0,
        if kind == ButtonKind::Primary {
            Weight::Bold
        } else {
            Weight::Semibold
        },
    )));
    let fill = hex_color(background);
    let border = hex_color(palette.ink);
    set_layer_colors(
        button,
        if ghost { None } else { Some(&fill) },
        &border,
        if kind == ButtonKind::Primary {
            1.5
        } else if ghost {
            0.0
        } else {
            1.0
        },
        f64::from(RADIUS_BUTTON),
    );
    set_shadow(button, palette, kind == ButtonKind::Primary);
    button.setContentTintColor(Some(&hex_color(foreground)));
}

/// Flat sidebar-style button (`style_flat_button`).
pub fn style_flat_button(button: &NSButton, selected: bool, palette: &Palette) {
    button.setBordered(false);
    button.setFont(Some(&font(13.0, Weight::Semibold)));
    let fill = color(if selected { "sun" } else { "sidebar" }, palette);
    set_layer_colors(
        button,
        Some(&fill),
        &NSColor::clearColor(),
        0.0,
        f64::from(RADIUS_BUTTON),
    );
    set_shadow(button, palette, false);
    button.setContentTintColor(Some(&hex_color(if selected {
        SUN_TEXT
    } else {
        palette.sidebar_text
    })));
}

/// Lay the `buttons` out as one segmented control inside `group`
/// (`segmented`).
pub fn segmented(
    group: &NSView,
    buttons: &[Retained<NSButton>],
    selected: usize,
    palette: &Palette,
) {
    let fill = color("surface", palette);
    let line = color("line", palette);
    set_layer_colors(group, Some(&fill), &line, 1.0, f64::from(RADIUS_BUTTON));
    if let Some(layer) = group.layer() {
        layer.setMasksToBounds(true);
    }
    if buttons.is_empty() {
        return;
    }
    let bounds = group.bounds();
    let width = bounds.size.width / buttons.len() as f64;
    let height = bounds.size.height;
    for (index, button) in buttons.iter().enumerate() {
        let is_selected = index == selected;
        button.setFrame(rect(index as f64 * width, 1.0, width, height - 2.0));
        button.setBordered(false);
        button.setFont(Some(&font(13.0, Weight::Semibold)));
        button.setAlignment(NSTextAlignment::Center);
        button.setWantsLayer(true);
        if let Some(layer) = button.layer() {
            let background = if is_selected {
                cg(&color("ink", palette))
            } else {
                cg(&NSColor::clearColor())
            };
            layer.setBackgroundColor(Some(&background));
            layer.setBorderWidth(0.0);
            layer.setCornerRadius(if is_selected { 8.0 } else { 0.0 });
        }
        button.setContentTintColor(Some(&color(
            if is_selected { "paper" } else { "ink" },
            palette,
        )));
    }
}

/// Size `button` to its title plus token padding (`fit_button`).
pub fn fit_button(button: &NSButton, x: f64, y: f64) {
    let title = button.title().to_string();
    let text_font = button
        .font()
        .unwrap_or_else(|| font(13.0, Weight::Semibold));
    let width = measure_text(&title, &text_font) + 2.0 * f64::from(ACTION_PADDING);
    button.setFrame(rect(x, y, width, f64::from(ACTION_HEIGHT)));
}

/// A status pill measured to fit `text`. `tint`/`text_token` are a
/// `CHECK_STATUS` tint pair.
pub fn make_pill(
    mtm: MainThreadMarker,
    text: &str,
    tint: &str,
    text_token: &str,
    palette: &Palette,
) -> Retained<FlippedView> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFont(Some(&font(TYPE_CAPTION.size, TYPE_CAPTION.weight)));
    label.setTextColor(Some(&color(text_token, palette)));
    label.setLineBreakMode(NSLineBreakMode::ByClipping);
    // sizeToFit measures the text cell, including its horizontal insets, so
    // the label is never narrower than the text it draws.
    label.sizeToFit();
    let size = label.frame().size;
    let label_width = size.width.ceil();
    let label_height = size.height.ceil();
    let pill_height = f64::from(PILL_HEIGHT);
    let width = label_width + 2.0 * f64::from(PILL_PADDING);
    let pill = FlippedView::new(mtm, rect(0.0, 0.0, width, pill_height));
    label.setFrame(rect(
        f64::from(PILL_PADDING),
        (pill_height - label_height) / 2.0,
        label_width,
        label_height,
    ));
    pill.addSubview(&label);
    let fill = surface_tint(tint, palette, PILL_TINT)
        .map(|hex| hex_color(&hex))
        .unwrap_or_else(|| color(tint, palette));
    set_layer_colors(
        &pill,
        Some(&fill),
        &color(text_token, palette),
        f64::from(PILL_BORDER_WIDTH),
        f64::from(RADIUS_PILL).min(pill_height / 2.0),
    );
    pill
}

/// The shared page title: display type, one display line tall.
pub fn page_title(
    mtm: MainThreadMarker,
    text: &str,
    width: f64,
    palette: &Palette,
) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFrame(rect(0.0, 0.0, width, f64::from(PAGE_TITLE_HEIGHT)));
    label.setFont(Some(&font(TYPE_DISPLAY.size, TYPE_DISPLAY.weight)));
    label.setTextColor(Some(&color("ink", palette)));
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    label
}

/// A compact pill-shaped button (example questions, citations).
pub fn style_chip(button: &NSButton, palette: &Palette, size: f64) {
    button.setBordered(false);
    button.setFocusRingType(NSFocusRingType::Exterior);
    button.setFont(Some(&font(size, Weight::Semibold)));
    // Truncation is a cell property, not an NSButton one.
    if let Some(cell) = button.cell() {
        cell.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    }
    set_layer_colors(
        button,
        Some(&color("surface", palette)),
        &color("line", palette),
        1.0,
        button.frame().size.height / 2.0,
    );
    set_shadow(button, palette, false);
    button.setContentTintColor(Some(&color("ink", palette)));
}

/// The bubble roles of [`style_bubble`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BubbleRole {
    Assistant,
    User,
}

/// Conversation bubble: a cobalt-tinted one for the user, a card otherwise.
pub fn style_bubble(view: &NSView, palette: &Palette, role: BubbleRole) {
    match role {
        BubbleRole::Assistant => style_surface(view, palette, &SurfaceStyle::default()),
        BubbleRole::User => {
            let fill = surface_tint("cobalt", palette, 0.16)
                .map(|hex| hex_color(&hex))
                .unwrap_or_else(|| color("cobalt", palette));
            set_layer_colors(
                view,
                Some(&fill),
                &color("cobalt", palette),
                1.0,
                f64::from(RADIUS_CARD),
            );
            set_shadow(view, palette, false);
        }
    }
}

/// Options of [`style_surface`] (Python keyword arguments).
#[derive(Clone, Copy, Debug)]
pub struct SurfaceStyle {
    pub fill: &'static str,
    pub border: &'static str,
    pub shadow: bool,
    /// `None`: 1.5 with a shadow, else 1.
    pub width: Option<f64>,
}

impl Default for SurfaceStyle {
    fn default() -> Self {
        SurfaceStyle {
            fill: "surface",
            border: "line",
            shadow: false,
            width: None,
        }
    }
}

/// Card styling (`style_surface`).
pub fn style_surface(view: &NSView, palette: &Palette, style: &SurfaceStyle) {
    set_layer_colors(
        view,
        Some(&color(style.fill, palette)),
        &color(style.border, palette),
        style.width.unwrap_or(if style.shadow { 1.5 } else { 1.0 }),
        f64::from(RADIUS_CARD),
    );
    set_shadow(view, palette, style.shadow);
}

/// The shared ink-outlined input box; read-only boxes go quiet (paper, line).
pub fn style_input_box(view: &NSView, palette: &Palette, readonly: bool) {
    style_surface(
        view,
        palette,
        &SurfaceStyle {
            fill: if readonly { "paper" } else { "surface" },
            border: if readonly { "line" } else { "ink" },
            shadow: false,
            width: Some(INPUT_BORDER_WIDTH),
        },
    );
    if let Some(layer) = view.layer() {
        layer.setCornerRadius(f64::from(RADIUS_BUTTON));
    }
}

/// Put a text field, combo box or popup in one shared input box (`input_box`);
/// returns the box. A `height` of 0 means [`INPUT_HEIGHT`].
///
/// The native bezels are switched off and every control uses the same font and
/// control size, centred at its own cell height inside a box of `INPUT_HEIGHT`,
/// so a field and a picker side by side are visibly the same height.
pub fn input_box(
    mtm: MainThreadMarker,
    control: &NSControl,
    frame: (f64, f64, f64, f64),
    palette: &Palette,
    readonly: bool,
) -> Retained<FlippedView> {
    let (x, y, width, height) = frame;
    let height = if height == 0.0 {
        f64::from(INPUT_HEIGHT)
    } else {
        height
    };
    let boxed = FlippedView::new(mtm, rect(x, y, width, height));
    control.setFont(Some(&font(13.0, Weight::Regular)));
    control.setControlSize(NSControlSize::Regular);
    if let Some(field) = control.downcast_ref::<NSTextField>() {
        field.setBezeled(false);
        field.setBordered(false);
        field.setDrawsBackground(false);
    }
    let cell_height = control.cell().map_or(f64::from(INPUT_HEIGHT), |cell| {
        cell.cellSize().height.ceil()
    });
    control.setFrame(rect_of(input_inner_frame(width, height, cell_height)));
    boxed.addSubview(control);
    style_input_box(&boxed, palette, readonly);
    boxed
}

/// Make a view invisible to accessibility (decorative).
pub fn hide_from_accessibility(view: &NSView) {
    view.setAccessibilityElement(false);
}

/// The fallback palette when no window is available yet.
pub fn default_palette() -> Palette {
    LIGHT
}

/// Parent class marker so callers can name the superclass of the style views.
pub fn is_view_class<T: ClassType>() -> &'static str {
    T::NAME
}
