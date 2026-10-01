"""Drive the AppKit hot-phrases window (macOS only — skipped on Linux)."""

from types import SimpleNamespace

import pytest

pytest.importorskip("AppKit")

from localflow.hotphrases import HotPhraseStore  # noqa: E402
from localflow.phrases_mac import build_window  # noqa: E402


def _config(hot_phrases=True):
    return SimpleNamespace(
        hot_phrases=hot_phrases,
        spoken_symbols=True,
        command_names=[],
        skills_dirs=[],
    )


@pytest.fixture
def controller(tmp_path):
    store = HotPhraseStore(tmp_path / "hot_phrases.json")
    c = build_window(store, _config())
    c.confirm_discard = lambda _err: True
    yield c, store
    c.window.close()


def test_empty_state_and_example(controller):
    c, store = controller
    assert c.empty_state_visible()
    c.add_example()
    assert not c.empty_state_visible()
    (e,) = store.list()
    assert e["trigger"] == "review checklist"


def test_draft_autosave_and_invalid(controller):
    c, store = controller
    c.new_phrase()
    c.set_fields("", "")
    c.flush_autosave()
    assert store.list() == []
    c.set_fields("my trigger", "my text")
    c.flush_autosave()
    assert [e["trigger"] for e in store.list()] == ["my trigger"]

    entry = store.list()[0]
    c.set_fields("Review checklist", "x")
    assert "Already used" not in c.trigger_hint_text()
    store.add("other", "y")
    c.set_fields("other", "x")
    c.flush_autosave()
    assert "Already used" in c.trigger_hint_text()
    # the invalid commit wrote nothing for the edit target
    assert store.get(entry["id"])["text"] == "my text"


def test_search_filters(controller):
    c, store = controller
    store.add("alpha", "A")
    store.add("beta", "B")
    c.reload_sidebar()
    c.set_search("alp")
    assert c.visible_triggers() == ["alpha"]
    c.set_search("")
    assert c.visible_triggers() == ["alpha", "beta"]


def test_toggle_persists(controller):
    c, store = controller
    e = store.add("trig", "t")
    c.reload_sidebar()
    c.select_entry(e["id"])
    c.toggle_enabled()
    assert store.list()[0]["enabled"] is False


def test_delete_and_undo(controller):
    c, store = controller
    e = store.add("trig", "t")
    c.reload_sidebar()
    c.select_entry(e["id"])
    c.delete_current()
    assert c.undo_bar_visible() and store.list() == []
    c.undo_delete()
    assert [x["trigger"] for x in store.list()] == ["trig"]
    assert not c.undo_bar_visible()


def test_test_input_shows_expansion(controller):
    c, store = controller
    store.add("review checklist", "CHECK")
    c.reload_sidebar()
    c.set_test_input("please review checklist now")
    assert c.preview_output() == "please CHECK now"


def test_external_change_poll(controller, tmp_path):
    c, store = controller
    c.model.changed_on_disk()  # prime
    HotPhraseStore(tmp_path / "hot_phrases.json").add("sign off", "Thanks")
    c.poll_store()
    assert "sign off" in c.visible_triggers()


def test_snapshot_png(controller, tmp_path):
    from localflow.phrases_mac import snapshot_png
    for dark in (False, True):
        c, _ = controller
        out = tmp_path / f"snap-{'dark' if dark else 'light'}.png"
        snapshot_png(c, out, dark=dark)
        assert out.exists() and out.stat().st_size > 0
