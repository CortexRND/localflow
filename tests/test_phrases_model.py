import json
from types import SimpleNamespace

import pytest

from localflow.hotphrases import (
    MAX_TEXT_CHARS,
    MAX_TRIGGER_CHARS,
    MAX_TRIGGER_WORDS,
    HotPhraseStore,
)
from localflow.phrases_model import PhrasesModel


def _config(hot_phrases=True, spoken_symbols=True):
    return SimpleNamespace(
        hot_phrases=hot_phrases,
        spoken_symbols=spoken_symbols,
        command_names=[],
        skills_dirs=[],
    )


@pytest.fixture
def model(tmp_path):
    store = HotPhraseStore(tmp_path / "hot_phrases.json")
    return PhrasesModel(store, _config()), store


# ------------------------------------------------------------- trigger_hint ---

def test_trigger_hint_blank(model):
    m, _ = model
    assert m.trigger_hint("") == (False, "Type the words you'll say")
    assert m.trigger_hint("   ") == (False, "Type the words you'll say")


def test_trigger_hint_too_long_chars(model):
    m, _ = model
    assert m.trigger_hint("x" * (MAX_TRIGGER_CHARS + 1)) == (
        False,
        f"Keep it under {MAX_TRIGGER_CHARS} characters",
    )


def test_trigger_hint_only_punctuation(model):
    m, _ = model
    assert m.trigger_hint("!!! ...") == (False, "Use at least one word")


def test_trigger_hint_too_many_words(model):
    m, _ = model
    trigger = " ".join(f"w{i}" for i in range(MAX_TRIGGER_WORDS + 1))
    assert m.trigger_hint(trigger) == (
        False,
        f"Use {MAX_TRIGGER_WORDS} words or fewer",
    )


def test_trigger_hint_duplicate(model):
    m, store = model
    other = store.add("Review Checklist", "x")
    assert m.trigger_hint("review, checklist") == (
        False,
        "Already used by \u201cReview Checklist\u201d",
    )
    # Updating the same entry with its own trigger is fine.
    assert m.trigger_hint("review, checklist", exclude_id=other["id"])[0] is True


def test_trigger_hint_ok_singular_and_plural(model):
    m, _ = model
    assert m.trigger_hint("deploy") == (True, "\u2713 1 word")
    assert m.trigger_hint("deploy prod") == (True, "\u2713 2 words")


def test_trigger_hint_normalization_note(model):
    m, _ = model
    ok, hint = m.trigger_hint("Review,  CHECKLIST")
    assert ok
    assert hint == (
        "\u2713 2 words \u00b7 matches \u201creview checklist\u201d "
        "(case and punctuation ignored)"
    )
    # Clean lowercase input gets no note.
    assert m.trigger_hint("review checklist") == (True, "\u2713 2 words")


# ---------------------------------------------------------------- text_hint ---

def test_text_hint(model):
    m, _ = model
    assert m.text_hint("") == (False, "Type the text to paste")
    assert m.text_hint("  \n ") == (False, "Type the text to paste")
    assert m.text_hint("x" * (MAX_TEXT_CHARS + 1)) == (
        False,
        f"Keep it under {MAX_TEXT_CHARS:,} characters",
    )
    assert m.text_hint("x") == (True, "1 line \u00b7 1 character")
    assert m.text_hint("one") == (True, "1 line \u00b7 3 characters")
    assert m.text_hint("one\ntwo") == (True, "2 lines \u00b7 7 characters")


# ------------------------------------------------------------------- commit ---

def test_commit_invalid_writes_nothing(model):
    m, store = model
    entry, err = m.commit(None, "", "", True)
    assert entry is None and err == "Type the words you'll say"
    assert not store.path.exists()  # nothing written
    entry, err = m.commit(None, "valid", "   ", True)
    assert entry is None and err == "Type the text to paste"
    assert not store.path.exists()


def test_commit_adds_and_updates(model):
    m, store = model
    entry, err = m.commit(None, "  my trigger ", "my text", True)
    assert err is None
    assert entry["trigger"] == "my trigger"
    assert len(store.list()) == 1

    updated, err = m.commit(entry["id"], "my trigger", "new text", True)
    assert err is None and updated["text"] == "new text"


def test_commit_noop_skips_write(model):
    m, store = model
    entry, _ = m.commit(None, "trig", "text", True)
    before = store.path.read_bytes()
    again, err = m.commit(entry["id"], " trig ", "text", True)
    assert err is None and again["id"] == entry["id"]
    assert store.path.read_bytes() == before


def test_commit_duplicate(model):
    m, store = model
    store.add("review checklist", "x")
    entry, err = m.commit(None, "Review, checklist", "y", True)
    assert entry is None
    assert err == "Already used by \u201creview checklist\u201d"
    assert len(store.list()) == 1


def test_commit_deleted_elsewhere(model):
    m, store = model
    entry, _ = m.commit(None, "trig", "text", True)
    store.delete(entry["id"])
    out, err = m.commit(entry["id"], "trig", "text", True)
    assert out is None and err == "This hot phrase was deleted elsewhere"


# ------------------------------------------------------------------ entries ---

def test_entries_sorted_and_filtered(model):
    m, store = model
    store.add("beta", "b text")
    store.add("Alpha", "a text about shipping")
    store.add("gamma", "g text")
    assert [e["trigger"] for e in m.entries()] == ["Alpha", "beta", "gamma"]
    assert [e["trigger"] for e in m.entries("ALP")] == ["Alpha"]
    # Filter matches the expansion text too.
    assert [e["trigger"] for e in m.entries("shipping")] == ["Alpha"]
    assert m.entries("zzz") == []


def test_set_enabled_and_get(model):
    m, store = model
    entry = store.add("trig", "t")
    assert m.set_enabled(entry["id"], False)["enabled"] is False
    assert m.get(entry["id"])["enabled"] is False
    assert m.get("nope") is None
    assert m.set_enabled("nope", True) is None


def test_delete_and_undo(model):
    m, store = model
    entry = store.add("trig", "the text", enabled=False)
    token = m.delete(entry["id"])
    assert token["trigger"] == "trig" and store.list() == []
    restored, err = m.undo_delete(token)
    assert err is None
    assert restored["trigger"] == "trig"
    assert restored["text"] == "the text"
    assert restored["enabled"] is False
    assert restored["id"] != entry["id"]  # fresh id is fine


def test_undo_after_trigger_reused(model):
    m, store = model
    entry = store.add("trig", "t")
    token = m.delete(entry["id"])
    store.add("Trig", "other")
    restored, err = m.undo_delete(token)
    assert restored is None and "duplicates" in err


def test_delete_unknown(model):
    m, _ = model
    assert m.delete("nope") is None


# ------------------------------------------------------------------ preview ---

def test_preview_whole_and_inline(model):
    m, store = model
    store.add("review checklist", "use a/b")
    out, matched = m.preview("Review checklist.")
    assert out == "use a/b" and matched == ["review checklist"]
    out, matched = m.preview("a dash b review checklist")
    assert out == "a-b use a/b" and matched == ["review checklist"]
    out, matched = m.preview("nothing here")
    assert out == "nothing here" and matched == []


def test_preview_disabled_trigger_not_matched(model):
    m, store = model
    store.add("review checklist", "x", enabled=False)
    out, matched = m.preview("review checklist")
    assert matched == []


def test_preview_dash_trigger(model):
    m, store = model
    store.add("review dash checklist", "CHECK")
    out, matched = m.preview("please review dash checklist now")
    assert out == "please CHECK now"
    assert matched == ["review dash checklist"]


def test_preview_hot_phrases_off(tmp_path):
    store = HotPhraseStore(tmp_path / "hp.json")
    store.add("trig", "expansion")
    m = PhrasesModel(store, _config(hot_phrases=False))
    out, matched = m.preview("trig")
    assert out == "trig" and matched == []


# ----------------------------------------------------------- changed_on_disk ---

def test_changed_on_disk(model, tmp_path):
    m, store = model
    assert m.changed_on_disk() is False  # primes
    assert m.changed_on_disk() is False
    HotPhraseStore(tmp_path / "hot_phrases.json").add("trig", "t")
    assert m.changed_on_disk() is True
    assert m.changed_on_disk() is False


def test_changed_on_disk_file_appears(tmp_path):
    m = PhrasesModel(HotPhraseStore(tmp_path / "hp.json"), _config())
    assert m.changed_on_disk() is False  # primes (missing)
    m.store.add("trig", "t")  # file now exists
    assert m.changed_on_disk() is True


def test_store_file_is_json(model):
    m, store = model
    m.commit(None, "trig", "t", True)
    data = json.loads(store.path.read_text())
    assert data[0]["trigger"] == "trig"
