"""AppKit components using LocalFlow's shared semantic tokens."""

import math

import AppKit
from AppKit import NSColor
from Foundation import NSMakeRect, NSMakeSize

from localflow.ui_tokens import (
    ACTION_HEIGHT,
    ACTION_PADDING,
    LIGHT,
    PILL_BORDER_WIDTH,
    PILL_HEIGHT,
    PILL_PADDING,
    PILL_TINT,
    RADIUS,
    SHADOW_OFFSET,
    SUN_TEXT,
    TYPE,
    surface_tint,
    tokens_for,
)


class FlippedView(AppKit.NSView):
    def isFlipped(self):
        return True


class HalftoneView(AppKit.NSView):
    def initWithFrame_palette_(self, frame, palette):
        self = super().initWithFrame_(frame)  # noqa: PLW0642
        if self is not None:
            self.palette = palette
        return self

    def drawRect_(self, _dirty_rect):
        color(self.palette["paper"]).set()
        AppKit.NSRectFill(self.bounds())
        color(self.palette["sun"]).set()
        for row in range(12):
            for column in range(12 - row):
                radius = max(1, 4 - (row + column) // 5)
                x, y = 8 + column * 14, 8 + row * 14
                path = AppKit.NSBezierPath.bezierPathWithOvalInRect_(
                    AppKit.NSMakeRect(x, y, radius * 2, radius * 2)
                )
                path.fill()


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
