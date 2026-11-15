"""Pure geometry for the native Ask page (no AppKit).

`ask_mac` measures real text with AppKit and passes the measuring function in;
tests pass a fake one. Everything here is deterministic, so the layout
invariants (one compact controls row, left-aligned wrapping chips, a composer
pinned inside the Hub viewport) are testable on any platform.

`measure(text, size, weight="regular")` returns the width in points of `text`
drawn on one line.
"""

from localflow.ask_model import SOURCES, TIME_FILTERS
from localflow.ui_tokens import (
    ACTION_PADDING,
    ASK_ANSWER_LINE,
    ASK_BUBBLE_MAX_FRACTION,
    ASK_BUBBLE_PADDING,
    ASK_CHIP_GAP,
    ASK_CHIP_HEIGHT,
    ASK_CHIP_PADDING,
    ASK_COMPOSER_HEIGHT,
    ASK_CONTROL_FONT,
    ASK_CONVERSATION_MIN_HEIGHT,
    ASK_SECTION_GAP,
    ASK_TOOLBAR_HEIGHT,
    ASK_TURN_GAP,
    CONTROL_GAP,
    HUB_PAGE_BOTTOM,
    LINE_HEIGHT,
)

CHECKBOX_EXTRA = 24  # checkbox glyph plus its gap to the title
POPUP_EXTRA = 36  # popup arrows and insets
ROW_GAP = 12  # between the controls on the single compact row
CHIP_FONT = 12
CAPTION_FONT = 11
BODY_FONT = 13
ASK_BUTTON_MIN_WIDTH = 72
SLACK = 4  # a wrapped label never ends up a hair narrower than its text


def wrap_lines(text, width, measure_line):
    """Greedy word wrap of `text` into lines no wider than `width`.

    `measure_line(str) -> width`. A single word wider than `width` is broken
    by characters, like a wrapping label does.
    """
    lines = []
    for paragraph in str(text).split("\n"):
        current = ""
        for word in paragraph.split(" "):
            candidate = word if not current else current + " " + word
            if measure_line(candidate) <= width:
                current = candidate
                continue
            if current:
                lines.append(current)
                current = ""
            while len(word) > 1 and measure_line(word) > width:
                cut = len(word) - 1
                while cut > 1 and measure_line(word[:cut]) > width:
                    cut -= 1
                lines.append(word[:cut])
                word = word[cut:]
            current = word
        lines.append(current)
    return lines


def flow_chips(labels, width, measure, *, size=CHIP_FONT):
    """Left-aligned wrapping rows of compact chips: [(x, y, w, h)], total height.

    A chip is as wide as its measured text plus padding and is only narrower
    than that when the label alone is wider than the row.
    """
    frames, x, y = [], 0, 0
    for label in labels:
        chip_w = min(measure(label, size, "semibold") + 2 * ASK_CHIP_PADDING, width)
        if x and x + chip_w > width:
            x, y = 0, y + ASK_CHIP_HEIGHT + ASK_CHIP_GAP
        frames.append((x, y, chip_w, ASK_CHIP_HEIGHT))
        x += chip_w + ASK_CHIP_GAP
    return frames, (y + ASK_CHIP_HEIGHT if frames else 0)


def toolbar_layout(width, measure):
    """The one compact controls row: time popup, three toggles, Clear on the right.

    Returns x/w for each control plus `fits`. If the full "Clear conversation"
    label would crowd the toggles, the shorter "Clear" is used so the row never
    wraps at the minimum Hub width.
    """
    popup_w = (
        max(measure(title, ASK_CONTROL_FONT, "regular") for title in TIME_FILTERS)
        + POPUP_EXTRA
    )
    x = popup_w + ROW_GAP
    toggles = []
    for key, label in SOURCES.items():
        toggle_w = measure(label, ASK_CONTROL_FONT, "regular") + CHECKBOX_EXTRA
        toggles.append({"key": key, "x": x, "w": toggle_w})
        x += toggle_w + ROW_GAP
    clear_title = "Clear conversation"
    clear_w = measure(clear_title, ASK_CONTROL_FONT, "semibold") + 2 * ACTION_PADDING
    if width - clear_w < x:
        clear_title = "Clear"
        clear_w = measure(clear_title, ASK_CONTROL_FONT, "semibold") + 2 * ACTION_PADDING
    return {
        "height": ASK_TOOLBAR_HEIGHT,
        "popup": (0, popup_w),
        "toggles": toggles,
        "clear": (width - clear_w, clear_w),
        "clear_title": clear_title,
        "fits": width - clear_w >= x - ROW_GAP,
    }


def conversation_layout(items, width, measure):
    """Place the conversation inside a document `width` wide.

    `items`: ("intro", text) | ("examples", [text]) | ("user", text) |
    ("assistant", text, is_error) | ("citations", [label]).
    Returns (placements, content_height, anchor_y); anchor_y is the top of the
    newest user bubble, where a freshly arrived turn should start.
    """
    placements, y, anchor = [], 0, 0
    max_bubble = max(int(width * ASK_BUBBLE_MAX_FRACTION), 120)
    for item in items:
        kind = item[0]
        if kind == "intro":
            lines = wrap_lines(
                item[1], width, lambda t: measure(t, BODY_FONT, "regular")
            )
            height = len(lines) * ASK_ANSWER_LINE
            placements.append(
                {"kind": "intro", "x": 0, "y": y, "w": width, "h": height, "text": item[1]}
            )
            y += height + ASK_TURN_GAP
        elif kind in ("examples", "citations"):
            frames, height = flow_chips(item[1], width, measure)
            for index, (fx, fy, fw, fh) in enumerate(frames):
                placements.append(
                    {"kind": "chip" if kind == "examples" else "cite",
                     "x": fx, "y": y + fy, "w": fw, "h": fh,
                     "text": item[1][index], "tag": index, "group": kind}
                )
            y += height + ASK_TURN_GAP
        else:
            is_user = kind == "user"
            limit = (max_bubble if is_user else width) - 2 * ASK_BUBBLE_PADDING
            lines = wrap_lines(
                item[1], limit, lambda t: measure(t, BODY_FONT, "regular")
            )
            text_w = min(
                max(measure(line, BODY_FONT, "regular") for line in lines) + SLACK,
                limit,
            )
            bubble_w = text_w + 2 * ASK_BUBBLE_PADDING
            bubble_h = len(lines) * ASK_ANSWER_LINE + 2 * ASK_BUBBLE_PADDING
            if is_user:
                anchor = y
            placements.append(
                {"kind": kind, "x": width - bubble_w if is_user else 0, "y": y,
                 "w": bubble_w, "h": bubble_h, "text": item[1],
                 "error": bool(len(item) > 2 and item[2]), "lines": len(lines),
                 "text_w": text_w}
            )
            y += bubble_h + ASK_TURN_GAP
    return placements, max(y - ASK_TURN_GAP, 0), anchor


def page_layout(width, viewport, top, measure, privacy):
    """Frames for the whole Ask page inside a Hub viewport of height `viewport`.

    The composer and the privacy note sit at the bottom, the conversation takes
    the height between the controls row and the composer, and `bottom` is
    <= viewport - HUB_PAGE_BOTTOM so the outer Hub document never scrolls. Only
    when even the minimum conversation height cannot fit does `bottom` exceed
    that, and then the outer scroller takes over instead of overlapping views.
    """
    toolbar = toolbar_layout(width, measure)
    privacy_lines = wrap_lines(
        privacy, width, lambda t: measure(t, CAPTION_FONT, "regular")
    )
    privacy_h = len(privacy_lines) * LINE_HEIGHT["caption"]
    ask_w = max(
        ASK_BUTTON_MIN_WIDTH,
        measure("Ask", BODY_FONT, "bold") + 2 * ACTION_PADDING,
    )
    convo_y = top + ASK_TOOLBAR_HEIGHT + ASK_SECTION_GAP
    bottom = viewport - HUB_PAGE_BOTTOM
    privacy_y = bottom - privacy_h
    composer_y = privacy_y - CONTROL_GAP - ASK_COMPOSER_HEIGHT
    convo_h = composer_y - ASK_SECTION_GAP - convo_y
    if convo_h < ASK_CONVERSATION_MIN_HEIGHT:
        convo_h = ASK_CONVERSATION_MIN_HEIGHT
        composer_y = convo_y + convo_h + ASK_SECTION_GAP
        privacy_y = composer_y + ASK_COMPOSER_HEIGHT + CONTROL_GAP
        bottom = privacy_y + privacy_h
    return {
        "toolbar_y": top,
        "toolbar": toolbar,
        "conversation": (0, convo_y, width, convo_h),
        "field": (0, composer_y, width - ask_w - CONTROL_GAP, ASK_COMPOSER_HEIGHT),
        "ask": (width - ask_w, composer_y, ask_w, ASK_COMPOSER_HEIGHT),
        "privacy": (0, privacy_y, width, privacy_h),
        "bottom": bottom,
    }


def utf16_length(text):
    """NSRange counts UTF-16 code units, not Python characters."""
    return len(str(text).encode("utf-16-le")) // 2


def clamp_selection(selection, text):
    """(location, length) clamped to `text` in UTF-16 units; the caret goes to
    the end when nothing was remembered."""
    size = utf16_length(text)
    location, length = selection or (size, 0)
    location = min(max(location, 0), size)
    return location, min(max(length, 0), size - location)


def at_bottom(y, content_height, view_height, slack=4):
    return y >= max(0, content_height - view_height) - slack


def resolve_scroll(saved_y, was_at_bottom, follow, content_height, view_height, anchor_y):
    """Where the conversation scroller should rest after a rebuild.

    A resize or appearance change keeps the saved offset (or the bottom, if the
    reader was already there) and never jumps. A freshly arrived or just-sent
    turn is followed: the bottom, unless the new turn is taller than the view,
    in which case its start so the reader begins at the top of the answer.
    """
    limit = max(0, content_height - view_height)
    if follow:
        return max(anchor_y, 0) if content_height - anchor_y > view_height else limit
    if saved_y is None or was_at_bottom:
        return limit
    return min(max(saved_y, 0), limit)
