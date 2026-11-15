"""Ask page geometry (pure): compact row, chips, bubbles, pinned composer."""

import math
from itertools import pairwise

import pytest

from localflow import ask_layout as layout
from localflow.ask_model import EXAMPLES, PRIVACY
from localflow.ui_tokens import (
    ASK_BUBBLE_MAX_FRACTION,
    ASK_CHIP_HEIGHT,
    ASK_CONVERSATION_MIN_HEIGHT,
    HUB_CONTENT_GUTTER,
    HUB_CONTENT_TOP,
    HUB_CONTENT_X,
    HUB_HEADER_OFFSET,
    HUB_MIN_HEIGHT,
    HUB_MIN_WIDTH,
    HUB_PAGE_BOTTOM,
    PAGE_TITLE_HEIGHT,
)

# Deliberately wider than the real system font, so anything that fits here
# also fits on screen.
BODY_MIN = HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER
MIN_VIEWPORT = HUB_MIN_HEIGHT - HUB_CONTENT_TOP
WIDTHS = (BODY_MIN, 760, 1016)


def measure(text, size, weight="regular"):
    per_char = 0.6 if weight != "regular" else 0.56
    return math.ceil(len(text) * size * per_char)


def overlaps(a, b):
    return (
        a["x"] < b["x"] + b["w"] and b["x"] < a["x"] + a["w"]
        and a["y"] < b["y"] + b["h"] and b["y"] < a["y"] + a["h"]
    )


def test_page_title_clears_the_header_offset():
    assert PAGE_TITLE_HEIGHT <= HUB_HEADER_OFFSET


@pytest.mark.parametrize("width", WIDTHS)
def test_controls_fit_one_compact_row_at_every_width(width):
    toolbar = layout.toolbar_layout(width, measure)
    assert toolbar["fits"]
    popup_x, popup_w = toolbar["popup"]
    clear_x, clear_w = toolbar["clear"]
    edges = [(popup_x, popup_x + popup_w)]
    edges += [(t["x"], t["x"] + t["w"]) for t in toolbar["toggles"]]
    edges.append((clear_x, clear_x + clear_w))
    assert [key["key"] for key in toolbar["toggles"]] == ["dictation", "meeting", "notes"]
    for (_, left_end), (right_start, _) in pairwise(edges):
        assert left_end < right_start  # ordered and never overlapping
    assert edges[-1][1] == width  # Clear hugs the right edge
    assert toolbar["height"] <= 32  # one compact row, not a stack of 36pt controls


def test_clear_label_shortens_rather_than_wrapping_the_row():
    narrow = layout.toolbar_layout(420, measure)
    assert narrow["clear_title"] == "Clear" and narrow["fits"]
    assert layout.toolbar_layout(BODY_MIN, measure)["clear_title"] == "Clear conversation"


def test_wrap_lines_breaks_words_and_oversized_tokens():
    def one(text):
        return measure(text, 13)

    assert layout.wrap_lines("", 100, one) == [""]
    assert layout.wrap_lines("a\nb", 100, one) == ["a", "b"]
    lines = layout.wrap_lines("alpha beta gamma delta", 100, one)
    assert len(lines) > 1 and all(one(line) <= 100 for line in lines)
    long_token = layout.wrap_lines("x" * 80, 100, one)
    assert len(long_token) > 1 and all(one(line) <= 100 for line in long_token)
    assert "".join(long_token) == "x" * 80


@pytest.mark.parametrize("width", WIDTHS)
def test_example_chips_are_compact_left_aligned_and_wrap(width):
    frames, height = layout.flow_chips(EXAMPLES, width, measure)
    assert len(frames) == len(EXAMPLES)
    assert all(fh == ASK_CHIP_HEIGHT for _x, _y, _w, fh in frames)
    for (x, _y, chip_w, _h), label in zip(frames, EXAMPLES):
        assert x + chip_w <= width
        # Measured text plus padding, never a full-width bar.
        assert chip_w == min(measure(label, 12, "semibold") + 24, width)
    assert frames[0][0] == 0
    rows = sorted({y for _x, y, _w, _h in frames})
    for row in rows:
        in_row = [f for f in frames if f[1] == row]
        assert in_row[0][0] == 0  # every row starts at the left edge
    assert height == rows[-1] + ASK_CHIP_HEIGHT
    if width == BODY_MIN:
        assert len(rows) > 1  # wrapped
        assert all(chip_w < width for _x, _y, chip_w, _h in frames)


def test_a_label_wider_than_the_row_is_clipped_not_overflowing():
    frames, _ = layout.flow_chips(["w" * 200], 300, measure)
    assert frames[0][2] == 300


def _conversation(width, *, long_answer=False, citations=3):
    answer = ("A fairly long answer sentence. " * (40 if long_answer else 2)).strip()
    items = [
        ("user", "What did we decide about the launch?"),
        ("assistant", answer, False),
        ("citations", [f"[{n}] Meeting · 2026-04-1{n} · Launch planning review" for n in range(citations)]),
        ("user", "And who owns the checklist?"),
        ("assistant", "Couldn't reach the model.", True),
    ]
    return layout.conversation_layout(items, width, measure)


@pytest.mark.parametrize("width", WIDTHS)
@pytest.mark.parametrize("long_answer", [False, True])
def test_bubbles_align_by_role_and_nothing_overlaps(width, long_answer):
    placements, height, anchor = _conversation(width, long_answer=long_answer)
    users = [p for p in placements if p["kind"] == "user"]
    assistants = [p for p in placements if p["kind"] == "assistant"]
    assert users and assistants
    for bubble in users:  # right aligned, tinted bubble, never full width
        assert bubble["x"] + bubble["w"] == width
        assert bubble["w"] <= int(width * ASK_BUBBLE_MAX_FRACTION) + 2
    for card in assistants:  # left aligned card
        assert card["x"] == 0 and card["w"] <= width
    assert assistants[-1]["error"] and not assistants[0]["error"]
    for index, first in enumerate(placements):
        assert first["x"] >= 0 and first["x"] + first["w"] <= width
        assert first["y"] + first["h"] <= height
        for second in placements[index + 1:]:
            assert not overlaps(first, second)
    assert anchor == users[-1]["y"]


def test_citation_chips_are_measured_and_wrap_left_aligned():
    placements, _, _ = _conversation(BODY_MIN, citations=6)
    chips = [p for p in placements if p["kind"] == "cite"]
    assert len(chips) == 6
    for chip in chips:
        assert chip["w"] == min(measure(chip["text"], 12, "semibold") + 24, BODY_MIN)
        assert chip["w"] < BODY_MIN
    assert len({chip["y"] for chip in chips}) > 1  # wrapped onto more rows
    for row in {chip["y"] for chip in chips}:
        assert min(c["x"] for c in chips if c["y"] == row) == 0


def test_taller_answers_make_taller_cards():
    short = _conversation(BODY_MIN)[1]
    long = _conversation(BODY_MIN, long_answer=True)[1]
    assert long > short


@pytest.mark.parametrize("width", WIDTHS)
@pytest.mark.parametrize("viewport", [MIN_VIEWPORT, 796 - HUB_CONTENT_TOP, 1200])
def test_page_is_built_inside_the_viewport_with_a_pinned_composer(width, viewport):
    page = layout.page_layout(width, viewport, HUB_HEADER_OFFSET, measure, PRIVACY)
    # The outer Hub document never grows past the viewport, so it never scrolls
    # the composer away.
    assert page["bottom"] == viewport - HUB_PAGE_BOTTOM
    convo_y, convo_h = page["conversation"][1], page["conversation"][3]
    field, ask, privacy = page["field"], page["ask"], page["privacy"]
    assert convo_y >= HUB_HEADER_OFFSET + page["toolbar"]["height"]
    assert convo_h >= ASK_CONVERSATION_MIN_HEIGHT
    assert convo_y + convo_h <= field[1]
    assert field[1] == ask[1] and field[3] == ask[3]
    assert field[0] + field[2] <= ask[0] and ask[0] + ask[2] == width
    assert field[1] + field[3] <= privacy[1]
    assert privacy[1] + privacy[3] == page["bottom"]  # privacy is below the composer
    assert privacy[3] <= 2 * 16  # small: at most two caption lines


def test_page_spills_to_the_outer_scroller_only_when_it_cannot_fit():
    page = layout.page_layout(BODY_MIN, 200, HUB_HEADER_OFFSET, measure, PRIVACY)
    assert page["conversation"][3] == ASK_CONVERSATION_MIN_HEIGHT
    assert page["conversation"][1] + page["conversation"][3] <= page["field"][1]
    assert page["bottom"] > 200 - HUB_PAGE_BOTTOM


def test_page_follows_the_width_on_resize():
    narrow = layout.page_layout(BODY_MIN, MIN_VIEWPORT, 48, measure, PRIVACY)
    wide = layout.page_layout(1016, MIN_VIEWPORT, 48, measure, PRIVACY)
    assert wide["conversation"][2] == 1016 > narrow["conversation"][2]
    assert wide["field"][2] > narrow["field"][2]
    assert wide["privacy"][3] <= narrow["privacy"][3]


def test_caret_ranges_count_utf16_units_so_emoji_drafts_keep_the_caret():
    text = "hi 😀 there!"  # 11 characters, 12 UTF-16 code units
    assert len(text) == 11 and layout.utf16_length(text) == 12
    assert layout.clamp_selection((12, 0), text) == (12, 0)  # caret at the end
    assert layout.clamp_selection((3, 2), text) == (3, 2)  # selects the emoji
    assert layout.clamp_selection((10, 9), text) == (10, 2)  # clamped to the end
    assert layout.clamp_selection(None, text) == (12, 0)
    assert layout.clamp_selection((99, 1), "") == (0, 0)


def test_scroll_resolution_follows_new_turns_but_never_jumps_on_resize():
    # Fresh paint with nothing remembered: rest at the bottom.
    assert layout.resolve_scroll(None, True, False, 900, 300, 700) == 600
    # Resize / appearance: keep the reader's place...
    assert layout.resolve_scroll(120, False, False, 900, 300, 700) == 120
    assert layout.resolve_scroll(500, False, False, 450, 300, 400) == 150  # clamped
    # ...or the bottom if they were already there.
    assert layout.resolve_scroll(600, True, False, 1100, 300, 700) == 800
    # A new short answer is followed to the bottom,
    assert layout.resolve_scroll(0, False, True, 900, 300, 700) == 600
    # a taller-than-the-view answer starts at its own question.
    assert layout.resolve_scroll(0, False, True, 1500, 300, 700) == 700
    assert layout.at_bottom(600, 900, 300) and not layout.at_bottom(100, 900, 300)
