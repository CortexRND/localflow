"""AppKit components using LocalFlow's shared semantic tokens."""

import AppKit
from AppKit import NSColor
from Foundation import NSMakeSize

from localflow.ui_tokens import LIGHT, RADIUS, SHADOW_OFFSET, SUN_TEXT, tokens_for


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
        "sidebar_idle": palette["ink"],
        "destructive": palette["red"],
    }
    foregrounds = {
        "primary": "#FFFFFF",
        "secondary": palette["ink"],
        "ghost": palette["muted"],
        "destructive_ghost": palette["red"],
        "selected": palette["paper"],
        "sidebar": SUN_TEXT,
        "sidebar_idle": palette["muted"],
        "destructive": "#FFFFFF",
    }
    button.setBordered_(False)
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
        layer.setShadowColor_(color(palette["ink"], {"fill": palette["ink"]}).CGColor())
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
    return style_button(button, "sidebar" if selected else "sidebar_idle", palette)


def style_surface(view, palette=None, *, fill="surface", border="line", shadow=False):
    palette = palette or LIGHT
    view.setWantsLayer_(True)
    layer = view.layer()
    layer.setBackgroundColor_(color(palette[fill], {"fill": palette[fill]}).CGColor())
    layer.setBorderColor_(color(palette[border], {"fill": palette[border]}).CGColor())
    layer.setBorderWidth_(1.5 if shadow else 1)
    layer.setCornerRadius_(RADIUS["card"])
    if shadow:
        layer.setShadowColor_(color(palette["ink"], {"fill": palette["ink"]}).CGColor())
        layer.setShadowOffset_(NSMakeSize(SHADOW_OFFSET, -SHADOW_OFFSET))
        layer.setShadowRadius_(0)
        layer.setShadowOpacity_(1)
