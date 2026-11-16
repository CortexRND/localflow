//! Small native-control constructors shared by the Hub pages (the Python
//! `_label`, `_button`, `_field`, `_add_text` helpers of `hub_mac.py`).
//!
//! Every control here is a real AppKit control (`NSTextField`, `NSButton`,
//! `NSPopUpButton`, `NSSearchField`) so keyboard, focus and accessibility work
//! natively; nothing is a custom-drawn click target. Controls that are not
//! self-describing get an accessibility label.

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAccessibility, NSButton, NSLineBreakMode, NSPopUpButton, NSSearchField, NSTextField, NSView,
};
use objc2_foundation::NSString;

use super::style::{color, font, rect, style_button, ButtonKind};
use crate::tokens::{Palette, Weight};

/// Options of [`label`] (Python keyword arguments of `_label`).
#[derive(Clone, Copy, Debug)]
pub struct LabelStyle {
    pub size: f64,
    pub color: &'static str,
    pub height: f64,
    pub weight: Weight,
    pub wrap: bool,
}

impl Default for LabelStyle {
    fn default() -> Self {
        LabelStyle {
            size: 14.0,
            color: "ink",
            height: 28.0,
            weight: Weight::Regular,
            wrap: false,
        }
    }
}

/// A static text label at `(x, y)` of `width`. With `wrap` the frame height is
/// the room for the wrapped lines.
pub fn label(
    mtm: MainThreadMarker,
    text: &str,
    x: f64,
    y: f64,
    width: f64,
    style: LabelStyle,
    palette: &Palette,
) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setFrame(rect(x, y, width, style.height));
    label.setFont(Some(&font(style.size, style.weight)));
    label.setTextColor(Some(&color(style.color, palette)));
    if style.wrap {
        label.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
        label.setMaximumNumberOfLines(0);
    } else {
        label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    }
    label
}

/// A token-styled push button (`_button`).
#[allow(clippy::too_many_arguments)]
pub fn button(
    mtm: MainThreadMarker,
    title: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    kind: ButtonKind,
    palette: &Palette,
) -> Retained<NSButton> {
    // SAFETY: no target/action yet; `ActionTarget::on_action` installs them.
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
    };
    button.setFrame(rect(x, y, width, height));
    style_button(&button, kind, palette);
    button
}

/// A native single-line text field with the Hub's 13pt font (`_field`).
pub fn field(
    mtm: MainThreadMarker,
    value: &str,
    x: f64,
    y: f64,
    width: f64,
    accessibility_label: &str,
) -> Retained<NSTextField> {
    let field = NSTextField::initWithFrame(mtm.alloc::<NSTextField>(), rect(x, y, width, 32.0));
    field.setStringValue(&NSString::from_str(value));
    field.setFont(Some(&font(13.0, Weight::Regular)));
    field.setAccessibilityLabel(Some(&NSString::from_str(accessibility_label)));
    field
}

/// A native search field with a placeholder and an accessibility label.
pub fn search_field(
    mtm: MainThreadMarker,
    value: &str,
    x: f64,
    y: f64,
    width: f64,
    placeholder: &str,
) -> Retained<NSSearchField> {
    let field = NSSearchField::initWithFrame(mtm.alloc::<NSSearchField>(), rect(x, y, width, 32.0));
    field.setStringValue(&NSString::from_str(value));
    field.setFont(Some(&font(13.0, Weight::Regular)));
    field.setPlaceholderString(Some(&NSString::from_str(placeholder)));
    field.setAccessibilityLabel(Some(&NSString::from_str(placeholder)));
    field
}

/// A native pop-up button with `titles`, `selected` chosen.
pub fn popup(
    mtm: MainThreadMarker,
    titles: &[String],
    selected: usize,
    x: f64,
    y: f64,
    width: f64,
    accessibility_label: &str,
) -> Retained<NSPopUpButton> {
    let popup = NSPopUpButton::initWithFrame_pullsDown(
        mtm.alloc::<NSPopUpButton>(),
        rect(x, y, width, 32.0),
        false,
    );
    for title in titles {
        popup.addItemWithTitle(&NSString::from_str(title));
    }
    if !titles.is_empty() {
        popup.selectItemAtIndex(selected.min(titles.len() - 1) as isize);
    }
    popup.setAccessibilityLabel(Some(&NSString::from_str(accessibility_label)));
    popup
}

/// Remove every subview of `view` (a page teardown).
pub fn remove_subviews(view: &NSView) {
    for sub in view.subviews().iter() {
        sub.removeFromSuperview();
    }
}

/// The lowest `y + height` among the subviews of `view` (0 when empty).
pub fn content_bottom(view: &NSView) -> f64 {
    view.subviews()
        .iter()
        .map(|sub| {
            let frame = sub.frame();
            frame.origin.y + frame.size.height
        })
        .fold(0.0, f64::max)
}
