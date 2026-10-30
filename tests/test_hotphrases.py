import json

import pytest

from localflow.hotphrases import (
    HotPhraseConflict,
    HotPhraseError,
    HotPhraseStore,
    expand_hot_phrases,
    match_whole,
    normalize_trigger,
)


@pytest.fixture
def store(tmp_path):
    return HotPhraseStore(tmp_path / "hot_phrases.json")


# ------------------------------------------------------------------- store ---

def test_add_and_list(store):
    entry = store.add("Review Checklist", "line one\n  line two")
    assert entry["trigger"] == "Review Checklist"
    assert entry["text"] == "line one\n  line two"
    assert entry["enabled"] is True
    assert entry["id"]
    assert [e["id"] for e in store.list()] == [entry["id"]]


def test_add_strips_trigger_but_not_text(store):
    entry = store.add("  spaced trigger  ", "  keep\n  this \n")
    assert entry["trigger"] == "spaced trigger"
    assert entry["text"] == "  keep\n  this \n"


def test_update(store):
    entry = store.add("one", "first")
    updated = store.update(entry["id"], text="second", enabled=False)
    assert updated["text"] == "second"
    assert updated["enabled"] is False
    assert updated["updated_at"] >= entry["created_at"]
    assert store.update("nosuchid", text="x") is None


def test_update_to_same_trigger_allowed(store):
    entry = store.add("review checklist", "x")
    # Updating text while re-submitting the same trigger must not self-conflict.
    updated = store.update(entry["id"], trigger="Review, checklist!", text="y")
    assert updated["text"] == "y"


def test_delete(store):
    entry = store.add("one", "x")
    assert store.delete(entry["id"]) is True
    assert store.list() == []
    assert store.delete(entry["id"]) is False


# -------------------------------------------------------------- validation ---

def test_trigger_validation(store):
    with pytest.raises(HotPhraseError):
        store.add("!!!", "x")                       # nothing usable
    with pytest.raises(HotPhraseError):
        store.add("   ", "x")
    with pytest.raises(HotPhraseError):
        store.add("x" * 81, "x")                    # too many chars
    with pytest.raises(HotPhraseError):
        store.add("one two three four five six seven eight nine", "x")  # 9 words


def test_text_validation(store):
    with pytest.raises(HotPhraseError):
        store.add("ok", "")
    with pytest.raises(HotPhraseError):
        store.add("ok", "   \n  ")
    with pytest.raises(HotPhraseError):
        store.add("ok", "x" * 20001)


def test_duplicate_trigger_conflicts_via_normalization(store):
    store.add("Review Checklist", "x")
    with pytest.raises(HotPhraseConflict):
        store.add("review, checklist", "y")
    other = store.add("other", "z")
    with pytest.raises(HotPhraseConflict):
        store.update(other["id"], trigger="review  checklist")


def test_enabled_excludes_disabled(store):
    store.add("on", "yes")
    store.add("off", "no", enabled=False)
    entry = store.add("toggled", "maybe", enabled=False)
    store.update(entry["id"], enabled=True)
    assert dict(store.enabled()) == {"on": "yes", "toggled": "maybe"}


def test_enabled_missing_file(store):
    assert store.enabled() == []


def test_unreadable_file_reads_empty(store, tmp_path):
    store.path.write_text("not json {{{", encoding="utf-8")
    assert store.list() == []
    assert store.enabled() == []


def test_cross_instance_reload(tmp_path):
    path = tmp_path / "hot_phrases.json"
    a = HotPhraseStore(path)
    b = HotPhraseStore(path)
    assert b.enabled() == []

    a.add("deploy steps", "run the deploy runbook")
    assert dict(b.enabled()) == {"deploy steps": "run the deploy runbook"}

    entry = a.list()[0]
    a.update(entry["id"], text="updated runbook")
    assert dict(b.enabled()) == {"deploy steps": "updated runbook"}

    a.delete(entry["id"])
    assert b.enabled() == []


# ------------------------------------------------------------------ matching ---

PHRASES = [
    ("review checklist", "CHECK: tests, lint, types"),
    ("pr review", "short pr review"),
    ("pr review full", "FULL PR REVIEW"),
]


def test_match_whole():
    assert match_whole("Review checklist.", PHRASES) == "CHECK: tests, lint, types"
    assert match_whole("no match here", PHRASES) is None
    assert match_whole("", PHRASES) is None


def test_expand_whole_utterance():
    assert expand_hot_phrases("Review checklist.", PHRASES) == "CHECK: tests, lint, types"


def test_expand_inline():
    assert (
        expand_hot_phrases("please review checklist then commit", PHRASES)
        == "please CHECK: tests, lint, types then commit"
    )


def test_expand_consumes_trailing_punctuation_at_end():
    assert (
        expand_hot_phrases("ship it, review checklist.", PHRASES)
        == "ship it, CHECK: tests, lint, types"
    )


def test_expand_keeps_mid_sentence_punctuation():
    out = expand_hot_phrases("review checklist, then commit", PHRASES)
    assert out == "CHECK: tests, lint, types, then commit"


def test_expand_longest_first():
    assert expand_hot_phrases("do a pr review full please", PHRASES) == (
        "do a FULL PR REVIEW please"
    )
    assert expand_hot_phrases("do a pr review please", PHRASES) == (
        "do a short pr review please"
    )


def test_expand_no_partial_word_match():
    assert expand_hot_phrases("reviews checklist", PHRASES) == "reviews checklist"
    assert expand_hot_phrases("pre review checklist", PHRASES) == (
        "pre CHECK: tests, lint, types"
    )


def test_expand_separators_inside_trigger():
    assert expand_hot_phrases("please review-checklist now", PHRASES) == (
        "please CHECK: tests, lint, types now"
    )
    assert expand_hot_phrases("please review, checklist now", PHRASES) == (
        "please CHECK: tests, lint, types now"
    )


def test_expand_literal_backslashes():
    phrases = [("regex note", r"use \1 and \\ in the pattern")]
    assert expand_hot_phrases("regex note", phrases) == r"use \1 and \\ in the pattern"
    # Whole-match also returns verbatim.
    assert match_whole("regex note", phrases) == r"use \1 and \\ in the pattern"


def test_expand_multiline_expansion():
    phrases = [("standup", "yesterday:\ntoday:\nblockers:")]
    assert expand_hot_phrases("write my standup please", phrases) == (
        "write my yesterday:\ntoday:\nblockers: please"
    )


def test_expand_casefold_unicode_mismatch_no_crash():
    # IGNORECASE can match a spelling that normalizes differently (dotless
    # ı case-folds to i). Whatever it does, it must not raise KeyError.
    phrases = [("i", "EXP")]
    result = expand_hot_phrases("use ı now", phrases)
    assert isinstance(result, str)
    # The ASCII trigger still works elsewhere.
    assert expand_hot_phrases("use i now", phrases) == "use EXP now"


def test_expand_non_ascii_trigger():
    phrases = [("café notes", "MENU")]
    assert expand_hot_phrases("open café notes please", phrases) == "open MENU please"
    assert expand_hot_phrases("Café Notes.", phrases) == "MENU"


def test_park_and_restore():
    from localflow.hotphrases import park_hot_phrases, restore_hot_phrases
    text, parked = park_hot_phrases("a review checklist b", PHRASES)
    assert text == "a \x010\x01 b"
    assert parked == ["CHECK: tests, lint, types"]
    assert restore_hot_phrases(text, parked) == "a CHECK: tests, lint, types b"


def test_render_dictation_symbol_word_trigger():
    from localflow.hotphrases import render_dictation
    from localflow.symbols import apply_spoken_symbols

    phrases = [("review dash checklist", "use slash here")]
    out = render_dictation(
        "please review dash checklist now",
        phrases,
        symbols=lambda t: apply_spoken_symbols(t, ()),
    )
    assert out == "please use slash here now"
    # Symbols still apply to speech around the trigger.
    out = render_dictation(
        "a dash b review dash checklist",
        phrases,
        symbols=lambda t: apply_spoken_symbols(t, ()),
    )
    assert out == "a-b use slash here"
    # Whole match skips the steps entirely.
    assert render_dictation("review dash checklist.", phrases,
                            clean=lambda t: t + "!", symbols=None) == "use slash here"


def test_expand_empty_phrases_and_empty_text():
    assert expand_hot_phrases("review checklist", []) == "review checklist"
    assert expand_hot_phrases("", PHRASES) == ""
    assert normalize_trigger("Review, checklist") == "review checklist"


def test_enabled_pairs_feed_expansion(store):
    store.add("ship it", "DEPLOY NOW", enabled=True)
    store.add("skip me", "NOPE", enabled=False)
    phrases = store.enabled()
    assert expand_hot_phrases("go ship it", phrases) == "go DEPLOY NOW"
    assert expand_hot_phrases("go skip me", phrases) == "go skip me"


def test_written_file_is_json_list(store):
    store.add("one", "x")
    data = json.loads(store.path.read_text(encoding="utf-8"))
    assert isinstance(data, list) and data[0]["trigger"] == "one"
