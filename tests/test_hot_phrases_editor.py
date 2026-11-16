"""Behaviour of the AppKit-free Hot Phrases page state (no macOS needed).

The real PhrasesModel and HotPhraseStore run against a temp directory; only
the clock is faked. The Hub view (hub_mac) is a thin renderer over this state.
"""

import os
from types import SimpleNamespace

import pytest

from localflow import hot_phrases_editor as hpe
from localflow import phrases_model
from localflow.dictionary import DictionaryStore
from localflow.hot_phrases_editor import HotPhrasesEditor, hot_phrases_layout
from localflow.hotphrases import MAX_TEXT_CHARS, HotPhraseStore
from localflow.phrases_model import PhrasesModel


class Clock:
    def __init__(self):
        self.now = 100.0

    def __call__(self):
        return self.now

    def advance(self, seconds):
        self.now += seconds


def _config(hot_phrases=True):
    return SimpleNamespace(
        hot_phrases=hot_phrases,
        spoken_symbols=False,
        spoken_punctuation=False,
        styles_enabled=False,
        style_other="formal",
        command_names=[],
        skills_dirs=[],
    )


@pytest.fixture
def env(tmp_path, monkeypatch):
    # Previews must never read the real user's dictionary.
    monkeypatch.setattr(
        phrases_model, "DictionaryStore", lambda: DictionaryStore(tmp_path / "dict.json")
    )
    store = HotPhraseStore(tmp_path / "hot_phrases.json")
    clock = Clock()

    def make(*entries, hot_phrases=True):
        for trigger, text, *rest in entries:
            store.add(trigger, text, rest[0] if rest else True)
        model = PhrasesModel(store, _config(hot_phrases))
        return HotPhrasesEditor(model, clock=clock)

    return SimpleNamespace(store=store, clock=clock, make=make, path=store.path)


def _triggers(store):
    return [e["trigger"] for e in sorted(store.list(), key=lambda e: e["trigger"])]


def _external(env, change):
    """Another process edits the file: apply `change`, force a new signature."""
    change()
    stat = env.path.stat()
    os.utime(env.path, ns=(stat.st_atime_ns, stat.st_mtime_ns + 2_000_000_000))


# ------------------------------------------------------------------ empty ---


def test_empty_store_shows_the_friendly_empty_state(env):
    ed = env.make()

    assert ed.empty_state_visible and not ed.editor_visible
    assert hpe.EMPTY_MESSAGE == "Say a word, get a saved prompt. Add your first hot phrase."
    assert ed.rows() == [] and ed.current_id is None


def test_untouched_empty_draft_never_traps_navigation(env):
    ed = env.make()
    assert ed.new_phrase() and ed.editor_visible

    assert ed.leave() is True

    assert ed.empty_state_visible
    assert env.store.list() == []
    assert ed.error == ""


def test_autosave_of_an_untouched_empty_draft_writes_nothing(env):
    ed = env.make()
    ed.new_phrase()

    assert ed.flush() is True
    assert env.store.list() == []


def test_new_phrase_twice_keeps_the_same_blank_draft(env):
    ed = env.make()
    ed.new_phrase()

    assert ed.new_phrase() is True and ed.draft


def test_add_example_creates_and_selects_it(env):
    ed = env.make()

    assert ed.add_example() is True

    assert _triggers(env.store) == [hpe.EXAMPLE_TRIGGER]
    assert ed.current_id is not None and not ed.pending()
    assert ed.fields_stale and ed.text == hpe.EXAMPLE_TEXT


# --------------------------------------------------------------- autosave ---


def test_valid_edit_autosaves_after_the_debounce(env):
    ed = env.make()
    ed.new_phrase()
    ed.edit("review checklist", "CHECK: tests, lint")

    env.clock.advance(0.3)
    ed.tick()
    assert env.store.list() == []  # still debouncing

    env.clock.advance(0.4)
    assert ed.tick() is True

    assert _triggers(env.store) == ["review checklist"]
    assert ed.current_id == env.store.list()[0]["id"]
    assert ed.notice == "Saved ✓" and ed.error == "" and not ed.pending()
    assert [r["title"] for r in ed.rows()] == ["review checklist"]
    assert ed.rows()[0]["selected"] is True


def test_each_keystroke_restarts_the_debounce(env):
    ed = env.make()
    ed.new_phrase()
    ed.edit("rev", "x")
    env.clock.advance(0.5)
    ed.edit("review", "x")
    env.clock.advance(0.5)

    ed.tick()
    assert env.store.list() == []

    env.clock.advance(0.2)
    ed.tick()
    assert _triggers(env.store) == ["review"]


def test_editing_an_existing_phrase_updates_it_in_place(env):
    ed = env.make(("sign off", "Thanks"))
    ed.on_show()
    original = ed.current_id

    ed.edit(text="Thanks, talk tomorrow.")
    env.clock.advance(1)
    ed.tick()

    entries = env.store.list()
    assert len(entries) == 1 and entries[0]["id"] == original
    assert entries[0]["text"] == "Thanks, talk tomorrow."


def test_edit_back_to_the_saved_values_cancels_the_pending_save(env):
    ed = env.make(("sign off", "Thanks"))
    ed.on_show()
    ed.edit(text="Changed")
    assert ed.autosave_due is not None

    ed.edit(text="Thanks")

    assert ed.autosave_due is None and not ed.pending()


def test_invalid_edit_stays_local_with_an_inline_error(env):
    ed = env.make(("review checklist", "x"))
    ed.on_show()
    ed.new_phrase()
    ed.edit("Review, checklist", "y")  # same trigger after normalisation

    env.clock.advance(1)
    ed.tick()

    assert len(env.store.list()) == 1  # nothing written
    assert ed.error.startswith("Already used by")
    assert ed.trigger == "Review, checklist" and ed.text == "y"
    message, is_error = ed.trigger_hint()
    assert is_error and message.startswith("Already used by")
    assert ed.pending() and ed.draft


def test_hints_stay_neutral_until_the_user_touches_the_form(env):
    ed = env.make()
    ed.new_phrase()

    assert ed.trigger_hint() == ("Type the words you'll say", False)
    ed.edit("", "")
    assert ed.trigger_hint()[1] is True


def test_fixing_an_invalid_draft_saves_it(env):
    ed = env.make(("review checklist", "x"))
    ed.on_show()
    ed.new_phrase()
    ed.edit("Review, checklist", "y")
    env.clock.advance(1)
    ed.tick()

    ed.edit(trigger="standup")
    env.clock.advance(1)
    ed.tick()

    assert _triggers(env.store) == ["review checklist", "standup"]
    assert ed.error == ""


def test_text_over_the_limit_is_rejected_by_the_model(env):
    ed = env.make()
    ed.new_phrase()
    ed.edit("big", "x" * (MAX_TEXT_CHARS + 1))

    assert ed.flush() is False
    assert "20,000" in ed.error and env.store.list() == []


# ------------------------------------------------------ navigation guard ---


def test_leave_commits_a_valid_dirty_draft(env):
    ed = env.make()
    ed.new_phrase()
    ed.edit("standup", "yesterday:")

    assert ed.leave() is True

    assert _triggers(env.store) == ["standup"]
    assert ed.autosave_due is None  # the pending autosave was cancelled


def test_leave_keeps_the_user_on_an_invalid_draft_with_a_friendly_error(env):
    ed = env.make(("standup", "x"))
    ed.on_show()
    ed.new_phrase()
    ed.edit("Standup!", "y")

    assert ed.leave() is False

    assert "Fix it to continue" in ed.error and "Already used" in ed.error
    assert ed.trigger == "Standup!" and ed.text == "y"
    assert len(env.store.list()) == 1


def test_leave_is_a_no_op_for_a_clean_selection(env):
    ed = env.make(("standup", "x"))
    ed.on_show()

    assert ed.leave() is True and ed.error == ""


def test_select_commits_a_valid_draft_then_switches(env):
    ed = env.make(("alpha", "a"), ("beta", "b"))
    ed.on_show()  # selects alpha
    alpha, beta = ed.current_id, next(e for e in env.store.list() if e["trigger"] == "beta")
    ed.edit(text="a2")

    assert ed.select(beta["id"]) is True

    assert ed.current_id == beta["id"] and ed.text == "b"
    assert [e["text"] for e in env.store.list() if e["id"] == alpha] == ["a2"]


def test_select_is_blocked_by_an_invalid_edit_and_loses_nothing(env):
    ed = env.make(("alpha", "a"), ("beta", "b"))
    ed.on_show()
    alpha = ed.current_id
    beta = next(e for e in env.store.list() if e["trigger"] == "beta")["id"]
    ed.edit(text="   ")

    assert ed.select(beta) is False

    assert ed.current_id == alpha and ed.text == "   "
    assert "Fix it to continue" in ed.error


def test_new_phrase_is_blocked_by_an_invalid_edit(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.edit(trigger="")

    assert ed.new_phrase() is False

    assert ed.current_id is not None and ed.trigger == ""


def test_selecting_the_current_phrase_is_a_no_op(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.edit(text="")  # invalid, pending

    assert ed.select(ed.current_id) is True and ed.text == ""


def test_selecting_a_phrase_deleted_elsewhere_just_refreshes_the_list(env):
    ed = env.make(("alpha", "a"), ("beta", "b"))
    ed.on_show()
    beta = next(e for e in env.store.list() if e["trigger"] == "beta")["id"]
    env.store.delete(beta)

    assert ed.select(beta) is True
    assert [r["title"] for r in ed.rows()] == ["alpha"]


def test_cancel_stops_a_pending_autosave(env):
    ed = env.make()
    ed.new_phrase()
    ed.edit("standup", "x")

    ed.cancel()
    env.clock.advance(5)
    ed.tick()

    assert env.store.list() == []


# --------------------------------------------------------------- search ---


def test_search_filters_live_and_reports_no_matches(env):
    ed = env.make(("review checklist", "x"), ("sign off", "y"), ("standup", "z"))

    ed.set_query("stand")
    assert [r["title"] for r in ed.rows()] == ["standup"]
    ed.set_query("zzz")
    assert ed.rows() == [] and ed.no_matches
    ed.set_query("")
    assert len(ed.rows()) == 3 and not ed.no_matches


def test_search_matches_the_saved_text_too(env):
    ed = env.make(("one", "contains needle"), ("two", "nothing"))

    ed.set_query("NEEDLE")

    assert [r["title"] for r in ed.rows()] == ["one"]


def test_search_never_discards_an_invalid_draft(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.new_phrase()
    ed.edit("!!!", "typed text")

    ed.set_query("alpha")
    ed.set_query("nothing matches")

    assert ed.trigger == "!!!" and ed.text == "typed text" and ed.draft
    assert ed.leave() is False


def test_search_keeps_the_selection_even_when_filtered_out(env):
    ed = env.make(("alpha", "a"), ("beta", "b"))
    ed.on_show()
    current = ed.current_id

    ed.set_query("beta")

    assert ed.current_id == current
    assert [r["selected"] for r in ed.rows()] == [False]


def test_rows_expose_enabled_indicators_and_accessibility_labels(env):
    ed = env.make(("alpha", "first line\nsecond"), ("beta", "b", False))

    rows = ed.rows()

    assert [(r["title"], r["enabled"]) for r in rows] == [("alpha", True), ("beta", False)]
    assert rows[0]["subtitle"] == "first line"
    assert [r["accessibility"] for r in rows] == ["alpha, on", "beta, off"]


# -------------------------------------------------------------- enabled ---


def test_enabled_toggle_saves_immediately_for_a_saved_phrase(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()

    assert ed.set_enabled(False) is True

    assert env.store.list()[0]["enabled"] is False
    assert ed.rows()[0]["enabled"] is False  # the indicator is current
    assert not ed.pending() and ed.autosave_due is None


def test_enabled_toggle_leaves_other_unsaved_edits_pending(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.edit(text="a changed")

    ed.set_enabled(False)

    saved = env.store.list()[0]
    assert saved["enabled"] is False and saved["text"] == "a"
    assert ed.pending() and ed.autosave_due is not None


def test_enabled_toggle_creates_a_valid_draft(env):
    ed = env.make()
    ed.new_phrase()
    ed.edit("standup", "x")

    assert ed.set_enabled(False) is True

    assert [(e["trigger"], e["enabled"]) for e in env.store.list()] == [("standup", False)]


def test_enabled_toggle_on_an_invalid_draft_stays_local_with_an_error(env):
    ed = env.make()
    ed.new_phrase()
    ed.edit("standup", "")

    assert ed.set_enabled(False) is False

    assert env.store.list() == [] and ed.enabled is False
    assert ed.error == "Type the text to paste"


def test_enabled_toggle_after_external_deletion_re_adds_a_valid_phrase(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    env.store.delete(ed.current_id)

    assert ed.set_enabled(False) is True

    assert [(e["trigger"], e["enabled"]) for e in env.store.list()] == [("alpha", False)]


# ---------------------------------------------------------- delete / undo ---


def test_delete_selects_the_neighbour_and_offers_undo(env):
    ed = env.make(("a one", "1"), ("b two", "2"), ("c three", "3"))
    ed.on_show()
    middle = next(e for e in env.store.list() if e["trigger"] == "b two")["id"]
    ed.select(middle)

    assert ed.delete() is True

    assert _triggers(env.store) == ["a one", "c three"]
    assert ed.trigger == "c three"  # same index, clamped to the list
    assert ed.undo_label == "Deleted “b two”"
    assert ed.fields_stale


def test_delete_the_last_row_selects_the_previous_one(env):
    ed = env.make(("a one", "1"), ("b two", "2"))
    ed.on_show()
    ed.select(next(e for e in env.store.list() if e["trigger"] == "b two")["id"])

    ed.delete()

    assert ed.trigger == "a one"


def test_deleting_the_only_phrase_returns_to_the_empty_state(env):
    ed = env.make(("only", "1"))
    ed.on_show()

    ed.delete()

    assert ed.empty_state_visible and env.store.list() == []
    assert ed.undo_token is not None


def test_undo_restores_and_reselects_the_phrase(env):
    ed = env.make(("only", "1"))
    ed.on_show()
    ed.delete()

    assert ed.undo_delete() is True

    assert _triggers(env.store) == ["only"]
    assert ed.trigger == "only" and ed.current_id == env.store.list()[0]["id"]
    assert ed.undo_token is None and not ed.empty_state_visible


def test_undo_bar_expires(env):
    ed = env.make(("only", "1"))
    ed.on_show()
    ed.delete()

    env.clock.advance(hpe.UNDO_SECONDS + 0.1)
    assert ed.tick() is True

    assert ed.undo_token is None and ed.undo_label == ""
    assert ed.undo_delete() is False


def test_undo_reports_a_trigger_taken_in_the_meantime(env):
    ed = env.make(("only", "1"))
    ed.on_show()
    ed.delete()
    env.store.add("Only", "other")

    assert ed.undo_delete() is False

    assert ed.error == "trigger duplicates existing hot phrase 'Only'"
    assert ed.undo_token is not None and _triggers(env.store) == ["Only"]  # retryable


def test_delete_with_unsaved_edits_removes_the_saved_phrase(env):
    ed = env.make(("alpha", "a"), ("beta", "b"))
    ed.on_show()
    ed.edit(text="unsaved")

    ed.delete()

    assert _triggers(env.store) == ["beta"] and ed.trigger == "beta"


def test_delete_discards_an_unsaved_draft_without_touching_the_store(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.new_phrase()
    ed.edit("draft", "text")

    ed.delete()

    assert _triggers(env.store) == ["alpha"]
    assert ed.trigger == "alpha" and not ed.draft and ed.undo_token is None


# ------------------------------------------------- external-change merges ---


def test_dirty_local_edits_win_over_an_external_change(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.edit(text="local")
    _external(env, lambda: env.store.update(ed.current_id, text="remote"))

    assert ed.poll() is True

    assert ed.text == "local" and ed.pending()
    ed.flush()
    assert env.store.list()[0]["text"] == "local"


def test_clean_selection_refreshes_from_an_external_edit(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.ack_fields()
    _external(env, lambda: env.store.update(ed.current_id, text="remote"))

    assert ed.poll() is True

    assert ed.text == "remote" and ed.fields_stale and not ed.pending()


def test_external_additions_show_up_in_the_list(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    _external(env, lambda: env.store.add("beta", "b"))

    ed.poll()

    assert [r["title"] for r in ed.rows()] == ["alpha", "beta"]
    assert ed.trigger == "alpha"


def test_external_deletion_keeps_a_dirty_draft_and_re_adds_it_on_commit(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.edit(text="mine")
    gone = ed.current_id
    _external(env, lambda: env.store.delete(gone))

    assert ed.poll() is True

    assert ed.current_id is None and ed.draft
    assert ed.error == hpe.DELETED_ELSEWHERE and ed.text == "mine"
    assert env.store.list() == []
    assert ed.flush() is True
    entry = env.store.list()[0]
    assert (entry["trigger"], entry["text"]) == ("alpha", "mine")
    assert ed.error == "" and ed.current_id == entry["id"]


def test_external_deletion_of_an_invalid_dirty_draft_stays_local(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.edit(text="")
    _external(env, lambda: env.store.delete(ed.current_id))

    ed.poll()

    assert ed.draft and ed.text == "" and env.store.list() == []
    assert ed.flush() is False


def test_external_deletion_of_a_clean_selection_moves_to_the_neighbour(env):
    ed = env.make(("a one", "1"), ("b two", "2"), ("c three", "3"))
    ed.on_show()
    middle = next(e for e in env.store.list() if e["trigger"] == "b two")["id"]
    ed.select(middle)
    _external(env, lambda: env.store.delete(middle))

    assert ed.poll() is True

    assert ed.trigger == "c three" and not ed.draft and ed.error == ""


def test_external_deletion_of_the_last_clean_phrase_shows_the_empty_state(env):
    ed = env.make(("only", "1"))
    ed.on_show()
    _external(env, lambda: env.store.delete(ed.current_id))

    ed.poll()

    assert ed.empty_state_visible and ed.current_id is None


def test_poll_without_a_disk_change_does_nothing(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()

    assert ed.poll() is False


def test_tick_polls_on_its_own_interval(env):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    _external(env, lambda: env.store.add("beta", "b"))

    assert ed.tick() is False  # not due yet
    env.clock.advance(hpe.POLL_INTERVAL + 0.1)
    assert ed.tick() is True
    assert len(ed.rows()) == 2


def test_on_show_selects_the_first_phrase_and_reloads(env):
    ed = env.make(("beta", "b"), ("alpha", "a"))

    ed.on_show()

    assert ed.trigger == "alpha" and ed.fields_stale


def test_on_show_after_leaving_keeps_the_selection(env):
    ed = env.make(("alpha", "a"), ("beta", "b"))
    ed.on_show()
    beta = next(e for e in env.store.list() if e["trigger"] == "beta")["id"]
    ed.select(beta)

    ed.on_show()

    assert ed.current_id == beta


# ---------------------------------------------------------- store failures ---


def test_unreadable_store_is_a_friendly_message_not_a_crash(env, monkeypatch):
    monkeypatch.setattr(
        env.store, "list", lambda: (_ for _ in ()).throw(OSError("disk"))
    )

    ed = env.make()

    assert ed.error == hpe.READ_ERROR and ed.rows() == []
    ed.on_show()
    assert ed.poll() in (True, False)


def test_write_failure_keeps_the_edit_and_reports_it(env, monkeypatch):
    ed = env.make()
    ed.new_phrase()
    ed.edit("standup", "x")
    monkeypatch.setattr(
        env.store, "add", lambda *a, **k: (_ for _ in ()).throw(PermissionError("ro"))
    )

    assert ed.flush() is False

    assert ed.error == hpe.WRITE_ERROR and ed.trigger == "standup"
    assert ed.leave() is False and ed.error == hpe.WRITE_ERROR


def test_toggle_write_failure_is_reported(env, monkeypatch):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    monkeypatch.setattr(
        env.store, "update", lambda *a, **k: (_ for _ in ()).throw(OSError("ro"))
    )

    assert ed.set_enabled(False) is False
    assert ed.error == hpe.WRITE_ERROR


def test_delete_failure_is_reported(env, monkeypatch):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    monkeypatch.setattr(
        env.store, "delete", lambda *a, **k: (_ for _ in ()).throw(OSError("ro"))
    )

    assert ed.delete() is False and ed.error == hpe.WRITE_ERROR
    assert ed.current_id is not None


def test_failed_undo_restore_keeps_the_token_until_it_works_or_expires(env, monkeypatch):
    ed = env.make(("only", "1"))
    ed.on_show()
    ed.delete()
    real_add = env.store.add
    monkeypatch.setattr(
        env.store, "add", lambda *a, **k: (_ for _ in ()).throw(OSError("ro"))
    )

    assert ed.undo_delete() is False
    assert ed.error == hpe.WRITE_ERROR and ed.undo_token is not None
    assert ed.undo_label  # the bar stays visible for a retry

    monkeypatch.setattr(env.store, "add", real_add)
    assert ed.undo_delete() is True
    assert ed.undo_token is None and ed.error == "" and ed.trigger == "only"


def test_last_delete_leaves_undo_and_expiry_on_the_empty_surface(env):
    ed = env.make(("only", "1"))
    ed.on_show()

    ed.delete()

    assert ed.empty_state_visible and ed.undo_label == "Deleted “only”"
    env.clock.advance(hpe.UNDO_SECONDS + 0.1)
    assert ed.tick() is True  # the view re-renders and hides the bar
    assert ed.undo_label == "" and ed.empty_state_visible


def test_empty_page_picks_up_an_external_addition(env):
    ed = env.make()
    ed.on_show()
    assert ed.empty_state_visible
    _external(env, lambda: env.store.add("beta", "b"))
    env.clock.advance(hpe.POLL_INTERVAL + 0.1)

    assert ed.tick() is True

    assert not ed.empty_state_visible and ed.trigger == "beta"


def test_errors_are_kept_for_the_empty_surface(env, monkeypatch):
    ed = env.make()
    monkeypatch.setattr(
        env.store, "add", lambda *a, **k: (_ for _ in ()).throw(OSError("ro"))
    )

    assert ed.add_example() is False

    assert ed.empty_state_visible and ed.error == hpe.WRITE_ERROR


# ------------------------------------------- real model, failing store reads ---


def _break_reads(env, monkeypatch):
    monkeypatch.setattr(
        env.store, "list", lambda: (_ for _ in ()).throw(PermissionError("locked"))
    )


def test_hints_flush_leave_and_autosave_survive_a_failing_store_read(env, monkeypatch):
    ed = env.make(("alpha", "a"))
    ed.on_show()
    ed.new_phrase()
    ed.edit("standup", "x")
    _break_reads(env, monkeypatch)  # the real PhrasesModel now hits the error

    assert ed.trigger_hint() == (hpe.READ_ERROR, True)
    assert ed.text_hint()[1] is False  # text validation needs no store
    assert ed.flush() is False and ed.error == hpe.READ_ERROR
    env.clock.advance(1)
    ed.edit("standup", "x2")
    env.clock.advance(1)
    ed.tick()  # autosave path
    assert ed.error == hpe.READ_ERROR
    assert ed.leave() is False
    assert ed.error == hpe.READ_ERROR + " Your edits are kept here."
    assert ed.trigger == "standup" and ed.text == "x2" and ed.draft


def test_toggle_and_select_survive_a_failing_store_read(env, monkeypatch):
    ed = env.make(("alpha", "a"), ("beta", "b"))
    ed.on_show()
    other = next(e for e in env.store.list() if e["trigger"] == "beta")["id"]
    ed.edit(text="changed")
    _break_reads(env, monkeypatch)

    assert ed.set_enabled(False) in (True, False)
    assert ed.select(other) is False
    assert ed.text == "changed"


# ---------------------------------------------------------------- preview ---


def test_preview_needs_a_typed_phrase(env):
    ed = env.make(("review checklist", "CHECK: tests"))

    assert ed.preview() == ("", hpe.PREVIEW_PROMPT)
    ed.set_test_input("   ")
    assert ed.preview() == ("", hpe.PREVIEW_PROMPT)


def test_preview_shows_live_output_and_the_matching_trigger(env):
    ed = env.make(("review checklist", "CHECK: tests"))

    ed.set_test_input("please review checklist now")
    output, status = ed.preview()

    assert "CHECK: tests" in output
    assert status == "Matched: “review checklist”"


def test_preview_says_when_nothing_matched(env):
    ed = env.make(("review checklist", "CHECK: tests"))

    ed.set_test_input("hello world")

    output, status = ed.preview()
    assert "hello world" in output and status == "No hot phrase matched"


def test_preview_ignores_disabled_phrases(env):
    ed = env.make(("review checklist", "CHECK", False))

    ed.set_test_input("review checklist")

    assert ed.preview()[1] == "No hot phrase matched"


def test_preview_with_hot_phrases_off_warns_and_matches_nothing(env):
    ed = env.make(("review checklist", "CHECK"), hot_phrases=False)

    ed.set_test_input("review checklist")

    assert ed.banner == hpe.DISABLED_BANNER
    assert ed.preview()[1] == "No hot phrase matched"


def test_preview_failure_never_raises(env, monkeypatch):
    ed = env.make(("review checklist", "CHECK"))
    monkeypatch.setattr(
        ed.model, "preview", lambda said: (_ for _ in ()).throw(RuntimeError("x"))
    )
    ed.set_test_input("review checklist")

    assert ed.preview() == ("", hpe.PREVIEW_ERROR)


def test_preview_runs_without_network_or_llm(env, monkeypatch):
    import socket

    def refuse(*args, **kwargs):
        raise AssertionError("preview must not use the network")

    monkeypatch.setattr(socket, "create_connection", refuse)
    monkeypatch.setattr(socket.socket, "connect", refuse)
    ed = env.make(("review checklist", "CHECK"))
    ed.set_test_input("review checklist")

    assert ed.preview()[0] == "CHECK"


# ------------------------------------------------------------- snapshots ---


def test_fill_types_for_the_view_and_an_invalid_draft_is_never_autosaved(env):
    ed = env.make(("review checklist", "x"))
    ed.on_show()
    ed.ack_fields()
    ed.new_phrase()
    ed.ack_fields()

    ed.fill("Review, checklist", "y")
    ed.flush()

    assert ed.fields_stale and ed.error.startswith("Already used")
    for _ in range(3):  # the snapshot run loop keeps ticking
        env.clock.advance(1)
        ed.tick()
    assert len(env.store.list()) == 1 and ed.draft and ed.error


def test_snapshot_script_covers_hot_phrases_states_with_isolated_stores():
    import ast
    from pathlib import Path

    path = Path(__file__).parent.parent / "scripts" / "mac_ui_snapshots.py"
    source = path.read_text(encoding="utf-8")
    for name in (
        "hub-hot-phrases-empty",
        "hub-hot-phrases-populated3-selected",
        "hub-hot-phrases-invalid-draft",
    ):
        assert f'"{name}"' in source
    assert "_hub_hot_phrases_snapshots(tmp)" in source
    assert "_hub_sized_shots(controller, " in source  # min and wide, light and dark
    # Every store is a temp one and every Hub is built with an injected model.
    tree = ast.parse(source)
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and getattr(node.func, "id", "") == "HotPhraseStore":
            assert node.args, "HotPhraseStore() would use the real ~/.localflow store"
    hub_calls = [
        node for node in ast.walk(tree)
        if isinstance(node, ast.Call) and getattr(node.func, "id", "") == "build_hub_window"
    ]
    injected = [
        node for node in hub_calls
        if any(k.arg in ("phrases_model", "phrase_store") for k in node.keywords)
    ]
    # The Hot Phrases pages (responsive + dedicated) never use the real store.
    assert len(injected) >= 2
    # Nothing starts the app singleton or the runtime.
    for forbidden in ("main_app", "ensure_runtime", "run_app", "AppHelper"):
        assert forbidden not in source


# ------------------------------------------------------------------ layout ---


def _overlaps(a, b):
    return (
        a[0] < b[0] + b[2] and b[0] < a[0] + a[2]
        and a[1] < b[1] + b[3] and b[1] < a[1] + a[3]
    )


@pytest.mark.parametrize(
    ("width", "viewport"),
    [(596, 576 - 48 - 24), (1016, 796 - 48 - 24), (640, 300), (2000, 1200)],
)
def test_layout_has_no_overlap_or_clipping(width, viewport):
    frames = hot_phrases_layout(width, viewport)
    height = frames["height"]
    named = {k: v for k, v in frames.items() if isinstance(v, tuple)}

    for name, (x, y, w, h) in named.items():
        assert x >= 0 and y >= 0 and x + w <= width and y + h <= height, name
        assert w > 0 and h > 0, name
    for first in ("search", "new", "list"):
        for second in ("search", "new", "list"):
            if first < second:
                assert not _overlaps(named[first], named[second]), (first, second)
        assert not _overlaps(named[first], named["card"]), first

    card = named["card"]
    rows = frames["card_rows"]
    for name, (x, y, w, h) in rows.items():
        assert x >= 0 and y >= 0 and x + w <= card[2] and y + h <= card[3], name
    items = list(rows.items())
    for index, (first, a) in enumerate(items):
        for second, b in items[index + 1:]:
            assert not _overlaps(a, b), (first, second)


def test_layout_is_width_aware_and_fills_a_tall_window():
    narrow = hot_phrases_layout(596, 504)
    wide = hot_phrases_layout(1016, 724)

    assert narrow["list"][2] == 190 and wide["list"][2] == 300
    assert narrow["card"][2] < wide["card"][2]
    assert wide["height"] == 724  # spare height goes to the text box
    assert wide["card_rows"]["text_scroll"][3] > narrow["card_rows"]["text_scroll"][3]
    assert hpe.TEXT_MIN_H <= narrow["card_rows"]["text_scroll"][3]


def test_layout_grows_the_scrolling_page_instead_of_squeezing_controls():
    short = hot_phrases_layout(596, 200)

    assert short["height"] > 200
    assert short["card_rows"]["text_scroll"][3] == hpe.TEXT_MIN_H
