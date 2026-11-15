"""Hot Phrases card geometry: error / Undo notices only take room when shown."""

import ast
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from localflow import hot_phrases_editor as hpe
from localflow import phrases_model
from localflow.dictionary import DictionaryStore
from localflow.hot_phrases_editor import (
    HotPhrasesEditor,
    empty_layout,
    hot_phrases_layout,
)
from localflow.hotphrases import HotPhraseStore
from localflow.phrases_model import PhrasesModel
from localflow.ui_tokens import HUB_PAGE_BOTTOM

HUB = Path(__file__).resolve().parents[1] / "localflow/hub_mac.py"
SIZES = {"min": (596, 576 - 48 - HUB_PAGE_BOTTOM), "wide": (1016, 796 - 48 - HUB_PAGE_BOTTOM)}
STATES = {
    "normal": (False, False),
    "error": (True, False),
    "undo": (False, True),
    "both": (True, True),
}


def overlaps(a, b):
    return (
        a[0] < b[0] + b[2] and b[0] < a[0] + a[2]
        and a[1] < b[1] + b[3] and b[1] < a[1] + a[3]
    )


def frames_for(size, state):
    width, viewport = SIZES[size]
    error, undo = STATES[state]
    return hot_phrases_layout(width, viewport, error_visible=error, undo_visible=undo)


@pytest.mark.parametrize("size", SIZES)
@pytest.mark.parametrize("state", STATES)
def test_notice_rows_exist_only_when_visible_and_nothing_overlaps(size, state):
    frames = frames_for(size, state)
    rows = frames["card_rows"]
    error, undo = STATES[state]
    assert ("error_label" in rows) is error
    assert ("undo_bar" in rows) is undo
    card = frames["card"]
    boxes = list(rows.items())
    for name, (x, y, w, h) in boxes:
        assert x >= 0 and y >= 0 and x + w <= card[2] and y + h <= card[3], name
    for index, (first, a) in enumerate(boxes):
        for second, b in boxes[index + 1:]:
            assert not overlaps(a, b), (first, second)
    if (size, state) == ("min", "both"):
        # Both notices plus the minimum text box need more than the window
        # offers: the page grows (and scrolls) instead of squeezing controls.
        assert frames["height"] > SIZES[size][1]
        assert rows["text_scroll"][3] == hpe.TEXT_MIN_H
    else:
        assert frames["height"] == SIZES[size][1]  # fits the viewport exactly


@pytest.mark.parametrize("size", SIZES)
def test_try_it_sits_directly_below_the_enabled_row_without_notices(size):
    rows = frames_for(size, "normal")["card_rows"]
    enabled_bottom = max(
        rows["delete_button"][1] + rows["delete_button"][3],
        rows["enabled_switch"][1] + rows["enabled_switch"][3],
    )
    gap = rows["preview_label"][1] - enabled_bottom
    assert 0 < gap <= 12  # no large blank slot reserved for a hidden notice


@pytest.mark.parametrize("size", SIZES)
def test_visible_notices_sit_between_the_enabled_row_and_try_it(size):
    base = frames_for(size, "normal")["card_rows"]
    both = frames_for(size, "both")["card_rows"]
    enabled_bottom = both["delete_button"][1] + both["delete_button"][3]
    assert both["error_label"][1] >= enabled_bottom
    assert both["undo_bar"][1] >= both["error_label"][1] + both["error_label"][3]
    assert both["preview_label"][1] >= both["undo_bar"][1] + both["undo_bar"][3]
    # The text box gives the room up (and takes it back) instead of the page growing.
    assert both["text_scroll"][3] < base["text_scroll"][3]
    assert both["text_scroll"][3] >= hpe.TEXT_MIN_H


def test_each_notice_costs_exactly_its_own_height_plus_a_gap():
    normal = frames_for("wide", "normal")["card_rows"]["text_scroll"][3]
    error = frames_for("wide", "error")["card_rows"]["text_scroll"][3]
    undo = frames_for("wide", "undo")["card_rows"]["text_scroll"][3]
    assert normal - error == 34 + hpe.NOTICE_GAP
    assert normal - undo == 36 + hpe.NOTICE_GAP


def test_short_window_grows_the_page_instead_of_squeezing_with_notices():
    frames = hot_phrases_layout(596, 120, error_visible=True, undo_visible=True)
    assert frames["height"] > 120
    assert frames["card_rows"]["text_scroll"][3] == hpe.TEXT_MIN_H


def test_saved_status_is_inline_in_the_enabled_row():
    rows = frames_for("min", "normal")["card_rows"]
    label, notice, delete = rows["enabled_label"], rows["enabled_notice"], rows["delete_button"]
    assert not overlaps(label, notice) and not overlaps(notice, delete)
    assert notice[0] >= label[0] + label[2] and notice[0] + notice[2] <= delete[0]
    assert abs(notice[1] - label[1]) <= 2  # same band as "Enabled"


@pytest.mark.parametrize("width", [SIZES["min"][0], SIZES["wide"][0]])
def test_empty_card_is_compact_and_notices_extend_it(width):
    rows, height = empty_layout(width)
    assert height < 258  # the old fixed card reserved room for hidden notices
    assert "error" not in rows and "undo" not in rows
    buttons_bottom = max(rows["new"][1] + rows["new"][3], rows["example"][1] + rows["example"][3])
    assert height - buttons_bottom <= 24
    with_error, error_height = empty_layout(width, error_visible=True)
    with_both, both_height = empty_layout(width, error_visible=True, undo_visible=True)
    assert error_height > height and both_height > error_height
    assert with_error["error"][1] >= buttons_bottom
    assert with_both["undo"][1] >= with_both["error"][1] + with_both["error"][3]
    boxes = list(with_both.values())
    for index, first in enumerate(boxes):
        assert first[1] + first[3] <= both_height
        for second in boxes[index + 1:]:
            assert not overlaps(first, second)


# ---------------------------------------------------------- editor state ---

class Clock:
    now = 100.0

    def __call__(self):
        return self.now


@pytest.fixture
def make_editor(tmp_path, monkeypatch):
    monkeypatch.setattr(
        phrases_model, "DictionaryStore", lambda: DictionaryStore(tmp_path / "dict.json")
    )
    store = HotPhraseStore(tmp_path / "hot_phrases.json")
    clock = Clock()
    config = SimpleNamespace(
        hot_phrases=True, spoken_symbols=False, spoken_punctuation=False,
        styles_enabled=False, style_other="formal", command_names=[], skills_dirs=[],
    )

    def make(*entries):
        for trigger, text in entries:
            store.add(trigger, text, True)
        return HotPhrasesEditor(PhrasesModel(store, config), clock=clock), clock

    return make


def test_notice_visibility_follows_error_and_undo_state(make_editor):
    editor, clock = make_editor(("review checklist", "x"), ("sign off", "y"))
    assert editor.notice_visibility == (False, False)
    editor.select(next(e["id"] for e in editor.entries if e["trigger"] == "sign off"))
    editor.delete()
    assert editor.notice_visibility == (False, True)  # Undo bar
    editor.new_phrase()
    editor.fill("review checklist", "dup")  # collides after normalising
    editor.flush()
    assert editor.error and editor.notice_visibility == (True, True)
    clock.now += hpe.UNDO_SECONDS + 1
    assert editor.tick()  # Undo expired
    assert editor.notice_visibility == (True, False)
    editor.edit(trigger="a different trigger")  # typing clears the error
    assert editor.notice_visibility == (False, False)


def test_saved_notice_never_changes_the_geometry(make_editor):
    editor, _ = make_editor(("review checklist", "x"))
    editor.select(editor.entries[0]["id"])
    before = editor.notice_visibility
    editor.edit(text="changed")
    assert editor.flush()
    assert editor.notice == "Saved ✓" and editor.notice_visibility == before


# ----------------------------------------------------- hub view behaviour ---

def relayout_controller(vis=(False, False)):
    namespace = {
        "hot_phrases_layout": hot_phrases_layout,
        "NSMakeRect": lambda *rect: tuple(rect),
        "NSMakeSize": lambda *size: tuple(size),
        "HUB_PAGE_BOTTOM": HUB_PAGE_BOTTOM,
    }
    tree = ast.parse(HUB.read_text())
    cls = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "HubWindowController")
    method = next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == "_phrases_relayout")
    method.decorator_list = []
    exec(compile(ast.Module(body=[method], type_ignores=[]), str(HUB), "exec"), namespace)  # noqa: S102
    rows = hot_phrases_layout(596, 504, error_visible=True, undo_visible=True)["card_rows"]
    scroll = Mock()
    scroll.contentSize.return_value = SimpleNamespace(height=140)
    controller = SimpleNamespace(
        phrases=SimpleNamespace(notice_visibility=vis),
        _phrases_vis=vis, phrase_width=596, phrase_top=48,
        _viewport_height=lambda: 576,
        phrase_card=Mock(), phrase_list_scroll=Mock(),
        phrase_views={key: Mock() for key in rows},
        phrase_text_scroll=scroll, phrase_text=Mock(),
        phrase_preview_scroll=scroll, phrase_preview=Mock(),
        _phrases_render_list=Mock(), _fit_body_height=Mock(), cursor_y=0,
    )
    return namespace["_phrases_relayout"], controller


def test_relayout_is_a_no_op_while_visibility_is_unchanged():
    relayout, controller = relayout_controller()
    relayout(controller)
    controller.phrase_card.setFrame_.assert_not_called()
    controller._fit_body_height.assert_not_called()
    for view in controller.phrase_views.values():
        view.setFrame_.assert_not_called()


def test_relayout_moves_views_in_place_once_per_visibility_change():
    relayout, controller = relayout_controller()
    preview = controller.phrase_views["preview_label"]
    controller.phrases.notice_visibility = (True, False)
    relayout(controller)
    assert controller._phrases_vis == (True, False)
    controller.phrase_views["error_label"].setHidden_.assert_called_with(False)
    controller.phrase_views["undo_bar"].setHidden_.assert_called_with(True)
    assert preview.setFrame_.call_count == 1
    controller._fit_body_height.assert_called_once()
    controller._phrases_render_list.assert_called_once()
    controller.phrase_text.setMinSize_.assert_called_once()
    assert controller.cursor_y > 48
    # Same visibility again: nothing moves.
    relayout(controller)
    assert preview.setFrame_.call_count == 1
    controller._fit_body_height.assert_called_once()
    # Notice gone: back to the compact geometry.
    controller.phrases.notice_visibility = (False, False)
    relayout(controller)
    controller.phrase_views["error_label"].setHidden_.assert_called_with(True)
    assert preview.setFrame_.call_count == 2


def test_relayout_never_rebuilds_so_typing_selection_and_timers_survive():
    tree = ast.parse(HUB.read_text())
    cls = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "HubWindowController")
    source = {
        n.name: ast.get_source_segment(HUB.read_text(), n)
        for n in cls.body if isinstance(n, ast.FunctionDef)
    }
    relayout = source["_phrases_relayout"]
    for forbidden in (
        "show_page", "_rebuild_current_page", "removeFromSuperview", "setStringValue_",
        "setString_", "makeFirstResponder_", "cancel", "timer",
    ):
        assert forbidden not in relayout, forbidden
    # Status refreshes (typing, saves, undo expiry) relayout only through the
    # visibility check; the empty-state toggle remains the one rebuild trigger.
    assert "self._phrases_relayout()" in source["_phrases_render_status"]
    assert "self._phrases_relayout_empty()" in source["_phrases_render_empty"]
    empty = source["_phrases_relayout_empty"]
    assert "if vis == self._phrases_vis:" in empty and "_rebuild_current_page" not in empty
    # The notice slots are only created hidden-capable, never reserved.
    page = source["_page_hot_phrases"]
    assert "error_visible=self._phrases_vis[0]" in page
    assert "undo_visible=self._phrases_vis[1]" in page
