"""AppKit components using LocalFlow's shared semantic tokens."""

import math

import AppKit
import objc
from AppKit import NSColor
from Foundation import NSMakeRect, NSMakeSize

from localflow.ui_tokens import (
    ACTION_HEIGHT,
    ACTION_PADDING,
    INPUT_BORDER_WIDTH,
    INPUT_HEIGHT,
    LIGHT,
    PAGE_TITLE_HEIGHT,
    PILL_BORDER_WIDTH,
    PILL_HEIGHT,
    PILL_PADDING,
    PILL_TINT,
    RADIUS,
    SHADOW_OFFSET,
    SUN_TEXT,
    SWITCH_BORDER_WIDTH,
    TYPE,
    halftone_dots,
    input_inner_frame,
    surface_tint,
    switch_colors,
    switch_parts,
    tokens_for,
)


class FlippedView(AppKit.NSView):
    def isFlipped(self):
        return True


class HalftoneView(AppKit.NSView):
    """Sun halftone decoration. Dots come from `halftone_dots`, which only
    returns dots inside the bounds, and drawing is clipped to the bounds too,
    so the pattern can never paint over neighbouring views."""

    def initWithFrame_palette_(self, frame, palette):
        self = super().initWithFrame_(frame)  # noqa: PLW0642
        if self is not None:
            self.palette = palette
            # macOS 14+ no longer clips views to their bounds by default.
            if self.respondsToSelector_("setClipsToBounds:"):
                self.setClipsToBounds_(True)
            self.anchor = "bottom-left"
            self.step = 14
            self.max_radius = 4
            self.pad = 8
        return self

    def drawRect_(self, _dirty_rect):
        bounds = self.bounds()
        AppKit.NSGraphicsContext.saveGraphicsState()
        try:
            AppKit.NSBezierPath.clipRect_(bounds)
            color(self.palette["paper"]).set()
            AppKit.NSRectFill(bounds)
            color(self.palette["sun"]).set()
            for x, y, diameter in halftone_dots(
                bounds.size.width, bounds.size.height, step=self.step,
                max_radius=self.max_radius, pad=self.pad, anchor=self.anchor,
            ):
                AppKit.NSBezierPath.bezierPathWithOvalInRect_(
                    AppKit.NSMakeRect(x, y, diameter, diameter)
                ).fill()
        finally:
            AppKit.NSGraphicsContext.restoreGraphicsState()


class PlaceholderTextView(AppKit.NSTextView):
    """An NSTextView that draws a muted placeholder while it holds no text.

    The placeholder is only drawn, never inserted: `string()` stays empty, so
    it cannot be copied, autosaved, sent to a model or seen by tests as text.
    """

    placeholder = ""
    placeholder_color = None

    def setPlaceholder_color_(self, text, placeholder_color):
        self.placeholder = text or ""
        self.placeholder_color = placeholder_color
        self.setNeedsDisplay_(True)

    # Overrides call NSTextView through objc.super: builtin super() does not
    # resolve these selectors on a PyObjC subclass (AttributeError in drawRect_).
    def didChangeText(self):
        objc.super(PlaceholderTextView, self).didChangeText()
        # Typing the first character / deleting the last one must repaint the
        # whole view, not just the edited glyph rect.
        self.setNeedsDisplay_(True)

    def setString_(self, text):
        objc.super(PlaceholderTextView, self).setString_(text)
        self.setNeedsDisplay_(True)

    def drawRect_(self, dirty_rect):
        objc.super(PlaceholderTextView, self).drawRect_(dirty_rect)
        if self.string() or not self.placeholder:
            return
        inset = self.textContainerInset()
        padding = self.textContainer().lineFragmentPadding()
        width = self.bounds().size.width - 2 * (inset.width + padding)
        attributes = {
            AppKit.NSFontAttributeName: self.font() or font(13),
            AppKit.NSForegroundColorAttributeName: (
                self.placeholder_color or NSColor.placeholderTextColor()
            ),
        }
        AppKit.NSAttributedString.alloc().initWithString_attributes_(
            self.placeholder, attributes
        ).drawInRect_(
            NSMakeRect(
                inset.width + padding, inset.height, max(width, 0),
                self.bounds().size.height - 2 * inset.height,
            )
        )


class CobaltSwitch(AppKit.NSButton):
    """Settings switch: a switch-type NSButton (native toggle, keyboard,
    target/action, state and accessibility) that draws only its own track,
    knob and On/Off word from palette tokens (ui_tokens.switch_parts /
    switch_colors); the button's own drawing never runs."""

    palette = None

    @objc.python_method
    def configure(self, palette, on, label):
        """Set up a freshly allocated switch; returns it."""
        self.palette = palette
        self.setButtonType_(AppKit.NSButtonTypeSwitch)
        self.setBordered_(False)
        self.setTitle_("")
        self.setState_(AppKit.NSControlStateValueOn if on else AppKit.NSControlStateValueOff)
        self.setAccessibilityLabel_(label)
        return self

    @objc.python_method
    def is_on(self):
        return self.state() == AppKit.NSControlStateValueOn

    def wantsUpdateLayer(self):
        return False  # always drawRect_, never the cell's layer drawing

    def accessibilityValueDescription(self):
        return "On" if self.is_on() else "Off"

    def drawRect_(self, _dirty_rect):
        bounds = self.bounds()
        parts = switch_parts(bounds.size.width, bounds.size.height, self.is_on())
        colors = switch_colors(self.palette or LIGHT, self.is_on(), self.isEnabled())

        def paint(hex_color):
            return color(hex_color).colorWithAlphaComponent_(colors["alpha"])

        AppKit.NSGraphicsContext.saveGraphicsState()
        try:
            x, y, w, h = parts["track"]
            half = SWITCH_BORDER_WIDTH / 2  # keep the stroke inside the bounds
            radius = max(parts["radius"] - half, 0)
            track = AppKit.NSBezierPath.bezierPathWithRoundedRect_xRadius_yRadius_(
                NSMakeRect(x + half, y + half, w - 2 * half, h - 2 * half), radius, radius,
            )
            paint(colors["track"]).set()
            track.fill()
            paint(colors["outline"]).set()
            track.setLineWidth_(SWITCH_BORDER_WIDTH)
            track.stroke()
            knob = AppKit.NSBezierPath.bezierPathWithOvalInRect_(NSMakeRect(*parts["knob"]))
            paint(colors["knob"]).set()
            knob.fill()
            paint(colors["outline"]).set()
            knob.setLineWidth_(1)
            knob.stroke()
            style = AppKit.NSMutableParagraphStyle.alloc().init()
            style.setAlignment_(AppKit.NSTextAlignmentRight)
            AppKit.NSAttributedString.alloc().initWithString_attributes_(
                parts["label"],
                {
                    AppKit.NSFontAttributeName: font(12, "semibold"),
                    AppKit.NSForegroundColorAttributeName: paint(colors["text"]),
                    AppKit.NSParagraphStyleAttributeName: style,
                },
            ).drawInRect_(NSMakeRect(*parts["text"]))
        finally:
            AppKit.NSGraphicsContext.restoreGraphicsState()

    def drawFocusRingMask(self):
        bounds = self.bounds()
        parts = switch_parts(bounds.size.width, bounds.size.height, self.is_on())
        radius = parts["radius"]
        AppKit.NSBezierPath.bezierPathWithRoundedRect_xRadius_yRadius_(
            NSMakeRect(*parts["track"]), radius, radius,
        ).fill()

    def focusRingMaskBounds(self):
        bounds = self.bounds()
        return NSMakeRect(*switch_parts(bounds.size.width, bounds.size.height, False)["track"])


def tokens_for_window(window):
    appearance = window.effectiveAppearance()
    dark_name = AppKit.NSAppearanceNameDarkAqua
    match = appearance.bestMatchFromAppearancesWithNames_(
        [AppKit.NSAppearanceNameAqua, dark_name]
    )
    return tokens_for("dark" if match == dark_name else "light")


def color(token, palette=None):
    value = (palette or LIGHT).get(token, token).lstrip("#")
    red, green, blue = (int(value[index:index + 2], 16) / 255 for index in (0, 2, 4))
    return NSColor.colorWithCalibratedRed_green_blue_alpha_(red, green, blue, 1.0)


def font(size, weight="regular"):
    weights = {
        "regular": AppKit.NSFontWeightRegular,
        "semibold": AppKit.NSFontWeightSemibold,
        "bold": AppKit.NSFontWeightBold,
    }
    return AppKit.NSFont.systemFontOfSize_weight_(size, weights[weight])


def style_button(button, kind="secondary", palette=None):
    palette = palette or LIGHT
    backgrounds = {
        "primary": palette["cobalt"],
        "secondary": palette["surface"],
        "ghost": palette["paper"],
        "destructive_ghost": palette["paper"],
        "selected": palette["ink"],
        "sidebar": palette["sun"],
        "sidebar_idle": palette["sidebar"],
        "destructive": palette["red"],
    }
    foregrounds = {
        "primary": "#FFFFFF",
        "secondary": palette["ink"],
        "ghost": palette["muted"],
        "destructive_ghost": palette["red"],
        "selected": palette["paper"],
        "sidebar": SUN_TEXT,
        "sidebar_idle": palette["sidebar_text"],
        "destructive": "#FFFFFF",
    }
    button.setBordered_(False)
    button.setFocusRingType_(AppKit.NSFocusRingTypeExterior)
    button.setFont_(font(13, "bold" if kind == "primary" else "semibold"))
    button.setWantsLayer_(True)
    layer = button.layer()
    if kind in ("ghost", "destructive_ghost"):
        layer.setBackgroundColor_(NSColor.clearColor().CGColor())
    else:
        layer.setBackgroundColor_(color(backgrounds[kind]).CGColor())
    layer.setCornerRadius_(RADIUS["button"])
    layer.setBorderColor_(color(palette["ink"], {"fill": palette["ink"]}).CGColor())
    layer.setBorderWidth_(
        1.5 if kind == "primary"
        else (0 if kind in ("ghost", "destructive_ghost") else 1)
    )
    if kind == "primary":
        layer.setShadowColor_(color(palette["shadow"]).CGColor())
        layer.setShadowOffset_(NSMakeSize(SHADOW_OFFSET, -SHADOW_OFFSET))
        layer.setShadowRadius_(0)
        layer.setShadowOpacity_(1)
    else:
        layer.setShadowOpacity_(0)
    foreground = foregrounds[kind].lstrip("#")
    red, green, blue = (
        int(foreground[index:index + 2], 16) / 255 for index in (0, 2, 4)
    )
    button.setContentTintColor_(
        NSColor.colorWithCalibratedRed_green_blue_alpha_(red, green, blue, 1)
    )
    return button


def style_flat_button(button, selected=False, palette=None):
    palette = palette or LIGHT
    button.setBordered_(False)
    button.setFont_(font(13, "semibold"))
    button.setWantsLayer_(True)
    layer = button.layer()
    layer.setBackgroundColor_(
        color("sun" if selected else "sidebar", palette).CGColor()
    )
    layer.setCornerRadius_(RADIUS["button"])
    layer.setBorderColor_(NSColor.clearColor().CGColor())
    layer.setBorderWidth_(0)
    layer.setShadowOpacity_(0)
    button.setContentTintColor_(
        color(SUN_TEXT if selected else palette["sidebar_text"])
    )
    return button


def segmented(group, buttons, selected, palette=None):
    palette = palette or LIGHT
    group.setWantsLayer_(True)
    layer = group.layer()
    layer.setBackgroundColor_(color("surface", palette).CGColor())
    layer.setBorderColor_(color("line", palette).CGColor())
    layer.setBorderWidth_(1)
    layer.setCornerRadius_(RADIUS["button"])
    layer.setMasksToBounds_(True)
    if not buttons:
        return buttons
    bounds = group.bounds()
    width = bounds.size.width / len(buttons)
    height = bounds.size.height
    for index, button in enumerate(buttons):
        is_selected = index == selected
        button.setFrame_(
            NSMakeRect(index * width, 1, width, height - 2)
        )
        button.setBordered_(False)
        button.setFont_(font(13, "semibold"))
        button.setAlignment_(AppKit.NSTextAlignmentCenter)
        button.setWantsLayer_(True)
        button.layer().setBackgroundColor_(
            color("ink", palette).CGColor()
            if is_selected else NSColor.clearColor().CGColor()
        )
        button.layer().setBorderWidth_(0)
        button.layer().setCornerRadius_(8 if is_selected else 0)
        button.setContentTintColor_(
            color("paper" if is_selected else "ink", palette)
        )
    return buttons


def measure_text(text, text_font):
    """Width in points of `text` drawn on one line in `text_font`."""
    attributed = AppKit.NSAttributedString.alloc().initWithString_attributes_(
        text, {AppKit.NSFontAttributeName: text_font}
    )
    return math.ceil(attributed.size().width)


def fit_button(button, *, x=0, y=0):
    """Size `button` to its title plus token padding; returns the button."""
    width = measure_text(button.title(), button.font()) + 2 * ACTION_PADDING
    button.setFrame_(NSMakeRect(x, y, width, ACTION_HEIGHT))
    return button


def make_pill(text, kind, palette=None):
    """A status pill measured to fit `text`. `kind` is a CHECK_STATUS tint pair."""
    palette = palette or LIGHT
    tint, text_token = kind
    label = AppKit.NSTextField.labelWithString_(text)
    label.setFont_(font(*TYPE["caption"]))
    label.setTextColor_(color(text_token, palette))
    label.setLineBreakMode_(AppKit.NSLineBreakByClipping)
    # sizeToFit measures the text cell, including its horizontal insets, so
    # the label is never narrower than the text it draws.
    label.sizeToFit()
    label_size = label.frame().size
    label_width = math.ceil(label_size.width)
    label_height = math.ceil(label_size.height)
    width = label_width + 2 * PILL_PADDING
    pill = FlippedView.alloc().initWithFrame_(NSMakeRect(0, 0, width, PILL_HEIGHT))
    label.setFrame_(
        NSMakeRect(PILL_PADDING, (PILL_HEIGHT - label_height) / 2,
                   label_width, label_height)
    )
    pill.addSubview_(label)
    pill.setWantsLayer_(True)
    layer = pill.layer()
    layer.setBackgroundColor_(
        color(surface_tint(tint, palette, PILL_TINT), {}).CGColor()
    )
    layer.setBorderColor_(color(palette[text_token], {}).CGColor())
    layer.setBorderWidth_(PILL_BORDER_WIDTH)
    layer.setCornerRadius_(min(RADIUS["pill"], PILL_HEIGHT / 2))
    return pill


def page_title(text, width, palette=None):
    """The shared page title: display type, one display line tall.

    Setup steps and every Hub page use this style, so a title is the same size
    and weight everywhere and never reaches into what the Hub header offset
    (PAGE_TITLE_HEIGHT plus a gap) leaves free below it.
    """
    label = AppKit.NSTextField.labelWithString_(text)
    label.setFrame_(NSMakeRect(0, 0, width, PAGE_TITLE_HEIGHT))
    label.setFont_(font(*TYPE["display"]))
    label.setTextColor_(color("ink", palette))
    label.setLineBreakMode_(AppKit.NSLineBreakByTruncatingTail)
    return label


def style_chip(button, palette=None, *, size=12):
    """A compact pill-shaped button (example questions, citations)."""
    palette = palette or LIGHT
    button.setBordered_(False)
    button.setFocusRingType_(AppKit.NSFocusRingTypeExterior)
    button.setFont_(font(size, "semibold"))
    # Truncation is a cell property, not an NSButton one.
    button.cell().setLineBreakMode_(AppKit.NSLineBreakByTruncatingTail)
    button.setWantsLayer_(True)
    layer = button.layer()
    layer.setBackgroundColor_(color(palette["surface"], palette).CGColor())
    layer.setBorderColor_(color(palette["line"], palette).CGColor())
    layer.setBorderWidth_(1)
    layer.setCornerRadius_(button.frame().size.height / 2)
    layer.setShadowOpacity_(0)
    button.setContentTintColor_(color(palette["ink"], palette))
    return button


def style_bubble(view, palette=None, *, role="assistant"):
    """Conversation bubble: a cobalt-tinted one for the user, a card otherwise."""
    palette = palette or LIGHT
    if role == "assistant":
        style_surface(view, palette)
        return view
    view.setWantsLayer_(True)
    layer = view.layer()
    layer.setBackgroundColor_(
        color(surface_tint("cobalt", palette, 0.16), {}).CGColor()
    )
    layer.setBorderColor_(color(palette["cobalt"], palette).CGColor())
    layer.setBorderWidth_(1)
    layer.setCornerRadius_(RADIUS["card"])
    layer.setShadowOpacity_(0)
    return view


def style_input_box(box, palette=None, *, readonly=False):
    """The shared ink-outlined input box; read-only boxes go quiet (paper, line)."""
    style_surface(
        box, palette, fill="paper" if readonly else "surface",
        border="line" if readonly else "ink", width=INPUT_BORDER_WIDTH,
    )
    box.layer().setCornerRadius_(RADIUS["button"])
    return box


def input_box(control, frame, palette=None, *, readonly=False):
    """Put a text field or combo box in one shared input box; returns the box.

    The native bezels are switched off and both kinds of control use the same
    font and control size, centred at their own cell height inside a box of
    INPUT_HEIGHT, so a field and a picker side by side are visibly the same
    height with the same top edge (a bezeled NSComboBox keeps its intrinsic
    ~22pt look whatever its frame says).
    """
    x, y, width, height = frame
    box = FlippedView.alloc().initWithFrame_(NSMakeRect(x, y, width, height or INPUT_HEIGHT))
    control.setFont_(font(13))
    control.setControlSize_(AppKit.NSControlSizeRegular)
    control.setBezeled_(False)
    control.setBordered_(False)
    control.setDrawsBackground_(False)
    if control.respondsToSelector_("setButtonBordered:"):
        control.setButtonBordered_(False)
    cell_height = math.ceil(control.cell().cellSize().height)
    control.setFrame_(
        NSMakeRect(*input_inner_frame(width, box.frame().size.height, cell_height))
    )
    box.addSubview_(control)
    style_input_box(box, palette, readonly=readonly)
    return box


def style_surface(
    view, palette=None, *, fill="surface", border="line", shadow=False, width=None,
):
    palette = palette or LIGHT
    view.setWantsLayer_(True)
    layer = view.layer()
    layer.setBackgroundColor_(color(palette[fill], {"fill": palette[fill]}).CGColor())
    layer.setBorderColor_(color(palette[border], {"fill": palette[border]}).CGColor())
    layer.setBorderWidth_(width if width is not None else (1.5 if shadow else 1))
    layer.setCornerRadius_(RADIUS["card"])
    if shadow:
        layer.setShadowColor_(color(palette["shadow"]).CGColor())
        layer.setShadowOffset_(NSMakeSize(SHADOW_OFFSET, -SHADOW_OFFSET))
        layer.setShadowRadius_(0)
        layer.setShadowOpacity_(1)
