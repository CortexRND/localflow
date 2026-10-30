"""Drives the Tk hot-phrases window headlessly-ish (needs a display)."""

import os
import sys
from unittest import mock

import pytest

tk = pytest.importorskip("tkinter")
if sys.platform.startswith("linux") and not os.environ.get("DISPLAY"):
    pytest.skip("no display for Tk", allow_module_level=True)

from localflow.config import Config  # noqa: E402
from localflow.hotphrases import HotPhraseStore  # noqa: E402
from localflow.phrases_tk import HotPhrasesWindow  # noqa: E402


@pytest.fixture
def window(tmp_path):
    root = tk.Tk()
    root.withdraw()
    store = HotPhraseStore(tmp_path / "hot_phrases.json")
    w = HotPhrasesWindow(root, store, Config())
    w.pump = lambda: [root.update() for _ in range(5)]
    w.pump()
    yield w, store
    root.destroy()


def test_add_edit_toggle_delete(window):
    w, store = window
    assert w.current_id is None
    w.trigger_var.set("  review checklist ")
    w.text.insert("1.0", "Line1\nuse a/b")
    assert w.dirty()
    w.save()
    (e,) = store.list()
    assert (e["trigger"], e["text"], e["enabled"]) == ("review checklist", "Line1\nuse a/b", True)
    assert not w.dirty() and w.current_id == e["id"]

    w.text.insert("end", "\nmore")
    w.save()
    assert store.list()[0]["text"] == "Line1\nuse a/b\nmore"

    w.toggle_selected()
    assert store.list()[0]["enabled"] is False and not w.dirty()
    w.toggle_selected()
    assert store.list()[0]["enabled"] is True

    with mock.patch("tkinter.messagebox.askyesno", return_value=True):
        w.delete_selected()
    assert store.list() == [] and w.current_id is None


def test_duplicate_trigger_shows_error(window):
    w, store = window
    store.add("review checklist", "x")
    w.refresh()
    w.new_phrase()
    w.trigger_var.set("Review, checklist")
    w.text.insert("1.0", "y")
    w.save()
    assert len(store.list()) == 1
    assert "duplicates" in w.status_var.get()


def test_selecting_loads_entry_and_dirty_switch_can_be_cancelled(window):
    w, store = window
    a = store.add("alpha", "A text")
    b = store.add("beta", "B text")
    w.refresh()
    w.tree.selection_set(a["id"])
    w.pump()
    assert w.current_id == a["id"] and w.text.get("1.0", "end-1c") == "A text"
    w.trigger_var.set("alpha edited")
    with mock.patch("tkinter.messagebox.askyesno", return_value=False):
        w.tree.selection_set(b["id"])
        w.pump()
    assert w.current_id == a["id"] and w.trigger_var.get() == "alpha edited"


def test_external_edits_are_picked_up(window, tmp_path):
    w, _store = window
    HotPhraseStore(tmp_path / "hot_phrases.json").add("sign off", "Thanks")
    w._poll()
    assert [e["trigger"] for e in w.entries.values()] == ["sign off"]


def test_preview_matches_dictation_pipeline(window):
    w, store = window
    store.add("review checklist", "use a/b")
    assert w.preview_text("Review checklist.") == "use a/b"
    assert w.preview_text("a dash b review checklist") == "a-b use a/b"
    assert w.preview_text("nothing here") == "nothing here"
