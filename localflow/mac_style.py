"""Shared AppKit colors and flat-button styling."""

import AppKit
from AppKit import NSColor, NSFont

from localflow.setup_mac import TOKENS


def color(token):
    value = TOKENS[token].lstrip("#")
    red, green, blue = (int(value[index:index + 2], 16) / 255 for index in (0, 2, 4))
    return NSColor.colorWithCalibratedRed_green_blue_alpha_(red, green, blue, 1.0)


def style_flat_button(button, selected=False):
    button.setBezelStyle_(AppKit.NSBezelStyleRounded)
    button.setFont_(NSFont.boldSystemFontOfSize_(12))
    button.setWantsLayer_(True)
    button.layer().setBackgroundColor_(
        color("sun" if selected else "card").CGColor()
    )
    button.layer().setBorderColor_(color("ink").CGColor())
    button.layer().setBorderWidth_(2)
    button.layer().setCornerRadius_(10)
    button.setContentTintColor_(color("ink"))
    return button
