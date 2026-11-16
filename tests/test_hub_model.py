"""Hub logic that runs on any platform, plus static checks of the AppKit hub.

AppKit cannot be imported off macOS, so the AppKit wiring is checked through
the AST and source text, in the style of test_hub_mac_static.py.
"""

import ast
from pathlib import Path
from types import SimpleNamespace

import pytest

from localflow import hub_model
from localflow.dictionary import DictionaryStore
from localflow.dispatch import (
    APPROVE_FROM,
    REJECT_FROM,
    PromptQueue,
    PromptTransitionError,
    WorkPrompt,
    approve_prompt,
    reject_prompt,
    transition_prompt,
)
from localflow.ui_tokens import HUB_CONTENT_X, HUB_MIN_HEIGHT, HUB_MIN_WIDTH

ROOT = Path(__file__).parent.parent
HUB_MAC = ROOT / "localflow" / "hub_mac.py"
EXPECTED_PAGES = (
    "Home", "Ask", "Hot Phrases", "Dictionary", "Prompts",
    "Meetings", "History", "Style", "Models",
)


def _tree(path):
    return ast.parse(path.read_text(encoding="utf-8"))


def _controller_sources():
    source = HUB_MAC.read_text(encoding="utf-8")
    cls = next(
        node for node in _tree(HUB_MAC).body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    return {
        node.name: ast.get_source_segment(source, node)
        for node in cls.body if isinstance(node, ast.FunctionDef)
    }


def _literal_tuple_assigned_to(tree, name):
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(
            isinstance(target, ast.Name) and target.id == name
            for target in node.targets
        ):
            return ast.literal_eval(node.value)
    raise AssertionError(f"{name} not found")


# ----------------------------------------------------------- sidebar / CLI ---

def test_sidebar_pages_have_the_exact_order():
    assert _literal_tuple_assigned_to(_tree(HUB_MAC), "PAGES") == EXPECTED_PAGES


def test_hub_window_cli_whitelist_matches_the_sidebar_order():
    from localflow import hub_route

    assert hub_route.HUB_PAGES == EXPECTED_PAGES
    tree = _tree(ROOT / "localflow" / "hub_window.py")
    choices = next(
        keyword.value
        for node in ast.walk(tree)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Attribute) and node.func.attr == "add_argument"
        for keyword in node.keywords if keyword.arg == "choices"
    )
    assert isinstance(choices, ast.Name) and choices.id == "HUB_PAGES"


def test_tk_fallback_never_receives_a_page_it_does_not_have():
    source = (ROOT / "localflow" / "hub_window.py").read_text(encoding="utf-8")
    assert "page if page in TK_PAGES else \"Home\"" in source


def test_every_page_has_a_builder():
    sources = _controller_sources()
    for page in EXPECTED_PAGES:
        assert f"_page_{page.lower().replace(' ', '_')}" in sources
    assert "_page_{page.lower().replace(\" \", \"_\")}" in sources["show_page"]


def test_hot_phrases_page_is_the_embedded_editor_not_a_launcher():
    """Handoff A's placeholder opened the standalone window; B embeds it."""
    from localflow import hot_phrases_editor

    sources = _controller_sources()
    page = sources["_page_hot_phrases"]
    empty = sources["_phrases_build_empty"]
    assert "EMPTY_MESSAGE" in empty and "phrasesNew:" in empty
    assert hot_phrases_editor.EMPTY_MESSAGE == (
        "Say a word, get a saved prompt. Add your first hot phrase."
    )
    # The editor surface named in the brief.
    for text in ('"When I say"', '"Type this"', '"+ New phrase"', "NSSwitch", "NSSearchField"):
        assert text in HUB_MAC.read_text(encoding="utf-8")
    assert "hot_phrases_layout(" in page and "self.body.frame().size.width" in page
    assert "NSTextView" in sources["_phrases_text_area"]
    # No launcher left behind: nothing spawns the standalone editor window.
    assert "openHotPhrases_" not in sources
    assert "localflow.phrases_window" not in HUB_MAC.read_text(encoding="utf-8")
    assert "phrases_mac" not in HUB_MAC.read_text(encoding="utf-8")
    # Preview goes through the model, with a typed test phrase.
    assert "self.phrases.preview()" in sources["_phrases_render_preview"]
    assert "PhrasesModel(HotPhraseStore(), self.config)" in sources["_phrases"]


def test_hot_phrases_page_follows_the_hub_navigation_and_timer():
    sources = _controller_sources()
    show = sources["show_page"]
    # The guard lives in the one navigation entry point, before the rebuild.
    assert '"Hot Phrases"' in show and "self._phrases_leave()" in show
    assert show.index("_phrases_leave()") < show.index("self.page = page")
    assert "entering_phrases" in show and ".on_show()" in show
    assert "_phrases_tick()" in sources["meetingTimerTick_"]  # reuses the one timer
    assert "_phrases_leave()" in sources["windowShouldClose_"]
    assert "cancel()" in sources["windowWillClose_"]
    # Rebuilds preserve the editor, they never run the guard.
    assert "_phrases_leave" not in sources["_rebuild_current_page"]
    assert '"hot_phrases"' in sources["_preserve_page_state"]
    assert '"hot_phrases"' in sources["_restore_page_state"]


def test_text_delegates_dispatch_by_page_and_control():
    sources = _controller_sources()
    text = sources["textDidChange_"]
    assert text.index('self.page == "Hot Phrases"') < text.index('self.page != "Meetings"')
    assert "self._phrases_edited()" in text
    assert "source is not notes" in text  # meeting notes only for the notes view
    control = sources["controlTextDidChange_"]
    for name in ("phrase_search", "phrase_trigger", "phrase_test"):
        assert f'getattr(self, "{name}", None)' in control
    assert 'self.page != "Hot Phrases"' in control


def test_hot_phrases_page_reuses_the_shared_phrases_model():
    sources = _controller_sources()
    assert "PhrasesModel" in HUB_MAC.read_text(encoding="utf-8")
    assert "HotPhrasesEditor(model)" in sources["_phrases"]
    editor = (ROOT / "localflow" / "hot_phrases_editor.py").read_text(encoding="utf-8")
    for call in ("model.trigger_hint", "model.text_hint", "model.commit",
                 "model.delete", "model.undo_delete", "model.changed_on_disk",
                 "model.preview", "model.set_enabled"):
        assert call in editor


def test_empty_surface_polls_shows_errors_and_keeps_undo_reachable():
    sources = _controller_sources()
    # Activity depends on page and model, not on editor-only controls.
    active = sources["_phrases_active"]
    assert "phrase_status_label" not in active and "hasattr" not in active
    assert "_phrases_active()" in sources["_phrases_tick"]
    # Sync renders the empty surface instead of returning early.
    sync = sources["_phrases_sync"]
    assert "self._phrases_render_empty()" in sync
    empty = sources["_phrases_build_empty"]
    assert '"Undo", "phrasesUndo:"' in empty
    assert "phrase_empty_error" in empty and "phrase_empty_undo_bar" in empty
    assert "self._phrases_render_empty()" in empty
    render = sources["_phrases_render_empty"]
    assert "editor.error" in render and "editor.undo_label" in render
    assert "editor.undo_token is None" in render


# ------------------------------------------------------------- responsive ---

def test_shell_is_responsive_with_a_documented_minimum():
    assert (HUB_MIN_WIDTH, HUB_MIN_HEIGHT) == (860, 600)
    sources = _controller_sources()
    build = sources["_build_window"]
    assert "setContentMinSize_(NSMakeSize(HUB_MIN_WIDTH, HUB_MIN_HEIGHT))" in build
    assert "self.sidebar.setAutoresizingMask_(AppKit.NSViewHeightSizable)" in build
    assert "AppKit.NSViewWidthSizable | AppKit.NSViewHeightSizable" in build
    assert "self.body.setAutoresizingMask_(AppKit.NSViewWidthSizable)" in build
    # The sidebar is fixed width; the body sits in a scroll view for overflow.
    assert "setDocumentView_(self.body)" in build


def test_live_resize_and_appearance_rebuild_the_page_keeping_drafts():
    sources = _controller_sources()
    assert "_rebuild_current_page()" in sources["windowDidEndLiveResize_"]
    assert "_rebuild_current_page()" in sources["windowDidChangeEffectiveAppearance_"]
    rebuild = sources["_rebuild_current_page"]
    assert rebuild.index("_preserve_page_state()") < rebuild.index("self.show_page(")
    assert rebuild.index("self.show_page(") < rebuild.index("_restore_page_state(state)")
    assert "scrollPoint_" in rebuild


def test_preserved_state_covers_every_draft_page():
    sources = _controller_sources()
    preserve = sources["_preserve_page_state"]
    restore = sources["_restore_page_state"]
    for kind in ("history", "style", "dictionary", "meetings"):
        assert f'"{kind}"' in preserve
        assert f'"{kind}"' in restore
    # Dictionary drafts include both cards' fields and both editing ids.
    for name in ("word_field", "from_field", "to_field", "editing_word", "editing_source"):
        assert name in preserve
    # Prompt drafts do not exist: the queue is the state, re-read on rebuild.
    assert "_prompt_entries()" in sources["_page_prompts"]


def test_page_builders_use_the_current_width_not_fixed_numbers():
    tree = _tree(HUB_MAC)
    cls = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    fixed = {720, 740, 760, 780, 1040}
    offenders = []
    for method in cls.body:
        if not isinstance(method, ast.FunctionDef):
            continue
        if method.name in {"_build_window"}:
            continue
        for node in ast.walk(method):
            if isinstance(node, ast.Constant) and node.value in fixed:
                offenders.append((method.name, node.value))
    assert not offenders
    sources = _controller_sources()
    for builder in (
        "_page_home", "_page_history", "_render_history", "_page_dictionary",
        "_render_dictionary", "_page_style", "_page_meetings", "_meeting_layout",
        "_page_models", "_page_prompts", "_page_hot_phrases",
    ):
        assert "self.body.frame().size.width" in sources[builder], builder


def test_pages_that_can_overflow_the_height_scroll():
    sources = _controller_sources()
    assert "_fit_body_height()" in sources["show_page"]
    assert "_fit_body_height()" in sources["_render_history"]
    assert "_fit_body_height()" in sources["_meeting_layout"]
    fit = sources["_fit_body_height"]
    assert "max(bottom + HUB_PAGE_BOTTOM, self._viewport_height())" in fit


def test_meetings_respects_the_standard_header_offset():
    sources = _controller_sources()
    page = sources["_page_meetings"]
    assert "top = self.cursor_y" in page
    assert "0, top, width, 13" in page  # the status message, not y=0
    assert "HUB_HEADER_OFFSET" in sources["show_page"]
    # Controls track the available width instead of fixed 740/720 columns:
    # the form frames come from the pure layout, sized by the page width.
    assert "meeting_form_layout(width, top, notes_height)" in page
    assert 'rect("notes")' in page
    layout = sources["_meeting_layout"]
    assert "width - 140" in layout and "width - 120" in layout
    # The autosave / state machine is still the same one.
    assert "self._meeting_request_notes_save()" in sources["meetingTimerTick_"]
    assert "_meeting_fill_templates()" in page


def test_stat_cards_wrap_and_notes_box_fits_the_minimum():
    body_min = HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * 16
    columns, card_width = hub_model.stat_layout(body_min)
    assert columns == 2 and card_width * columns + 12 <= body_min
    columns, card_width = hub_model.stat_layout(1040 - HUB_CONTENT_X - 32)
    assert columns == 4 and card_width >= hub_model.STAT_FOUR_COLUMN_MIN_CARD
    assert 110 <= hub_model.meeting_notes_height(HUB_MIN_HEIGHT - 24, 48) <= 220
    assert hub_model.meeting_notes_height(2000, 48) == 220
    assert hub_model.meeting_notes_height(100, 48) == 110


def test_snapshot_script_covers_the_new_states_at_both_sizes():
    snapshots = (ROOT / "scripts" / "mac_ui_snapshots.py").read_text(encoding="utf-8")
    assert '("min", HUB_MIN_WIDTH, HUB_MIN_HEIGHT)' in snapshots
    assert '("wide", 1280, 820)' in snapshots
    for name in (
        "hub-home-populated", "hub-hot-phrases", "hub-prompts-empty",
        "hub-prompts-two", "hub-dictionary-entries", "hub-dictionary-word-error",
        "hub-dictionary-replacement-error", 'f"hub-meetings-{state}"',
    ):
        assert name in snapshots
    assert "f\"{name}-{label}\"" in snapshots
    # light and dark come from _shot; temp stores keep real data untouched.
    assert "for dark in (False, True):" in snapshots
    assert 'PromptQueue(base / "queue-empty.json")' in snapshots
    assert 'DictionaryStore(base / "resp-dictionary.json")' in snapshots
    assert "_hub_responsive_snapshots(tmp)" in snapshots


# ------------------------------------------------------------- dictionary ---

@pytest.fixture
def store(tmp_path):
    return DictionaryStore(tmp_path / "dictionary.json")


def test_dictionary_page_has_two_labelled_cards_and_one_field_each():
    page = _controller_sources()["_page_dictionary"]
    for text in (
        "Words LocalFlow should know", "Auto-replace", "e.g. Kubernetes",
        '"Add word"', '"Add replacement"', '"When I say"', '"Type this"',
    ):
        assert text in page
    assert page.count("self.word_field = self._field(") == 1
    assert 'action="addWord:"' in page
    assert page.count('action="addReplacement:"') == 2  # Return in either field


def test_dictionary_handlers_use_their_own_card_and_fields():
    sources = _controller_sources()
    word = sources["addWord_"]
    assert "self.word_field.stringValue()" in word
    assert "self.from_field" not in word and "self.to_field" not in word
    assert '_reload_dictionary(error, "word", clear="word")' in word
    replacement = sources["addReplacement_"]
    assert "self.from_field.stringValue()" in replacement
    assert "self.to_field.stringValue()" in replacement
    assert "self.word_field" not in replacement
    assert '_reload_dictionary(error, "replacement", clear="replacement")' in replacement


def test_dictionary_success_clears_errors_and_fields_failure_keeps_input():
    reload = _controller_sources()["_reload_dictionary"]
    assert reload.index("if error:") < reload.index('self.page_errors.pop("Dictionary", None)')
    assert 'self.page_errors["Dictionary"] = error' in reload
    assert 'self.page_notices.pop("Dictionary", None)' in reload
    assert 'word, editing_word = "", None' in reload
    assert 'source, target, editing_source = "", "", None' in reload
    # State is read before the rebuild and restored after it, errors included.
    assert reload.index("_preserve_page_state()") < reload.index('self.show_page("Dictionary")')
    assert reload.index('self.show_page("Dictionary")') < reload.index("_restore_page_state(")
    # Dictionary errors are inline, never the generic banner.
    show = _controller_sources()["show_page"]
    assert 'page not in ("Dictionary", "Prompts")' in show


def test_dictionary_lists_every_entry_each_with_edit_and_remove():
    source = HUB_MAC.read_text(encoding="utf-8")
    assert "[:10]" not in source and "min(10" not in source
    render = _controller_sources()["_render_dictionary"]
    assert "enumerate(entries)" in render
    assert '"Remove", "removeDictionary:"' in render
    assert '"Edit", "editDictionary:"' in render
    assert "NSScrollView" in render
    assert "remove_entry(self.dictionary, key, kind or None)" in (
        _controller_sources()["removeDictionary_"]
    )


def test_dictionary_entries_are_not_capped():
    data = {
        "words": [f"word{i}" for i in range(25)],
        "replacements": [{"from": f"s{i}", "to": f"t{i}"} for i in range(7)],
    }
    entries = hub_model.dictionary_entries(data)
    assert len(entries) == 32
    assert entries[0] == ("word0", "word0", "word", "")
    assert entries[25] == ("s0 → t0", "s0", "replacement", "t0")


def test_empty_word_gets_a_friendly_message_and_changes_nothing(store):
    assert hub_model.submit_word(store, "", None) == "Type a word first."
    assert hub_model.submit_word(store, "   ", None) == "Type a word first."
    assert hub_model.submit_word(store, "", "Old") == "Type a word first."
    assert store.list() == {"words": [], "replacements": []}


def test_word_add_edit_and_duplicate(store):
    assert hub_model.submit_word(store, "Kubernetes") is None
    assert "already in your dictionary" in hub_model.submit_word(store, "kubernetes")
    assert hub_model.submit_word(store, "K8s", editing="Kubernetes") is None
    assert store.words() == ["K8s"]
    assert "removed" in hub_model.submit_word(store, "x", editing="Gone")
    # A word never ends up as a replacement.
    assert store.replacements() == []


def test_replacement_validation_names_the_missing_field(store):
    assert hub_model.submit_replacement(store, "", "the") == "Type what you say first."
    assert hub_model.submit_replacement(store, "teh", "") == "Type what it should become."
    assert store.list() == {"words": [], "replacements": []}
    assert hub_model.submit_replacement(store, "teh", "the") is None
    assert store.replacements() == [("teh", "the")]
    assert store.words() == []
    assert "already have a replacement" in hub_model.submit_replacement(store, "TEH", "the")
    assert hub_model.submit_replacement(store, "teh", "then", editing="teh") is None
    assert store.replacements() == [("teh", "then")]


def test_dictionary_errors_are_friendly_not_raw_exceptions():
    class Broken:
        def add_word(self, _word):
            raise OSError("disk exploded: /private/path")

        def add_replacement(self, _source, _target):
            raise OSError("disk exploded: /private/path")

        def remove(self, _key):
            raise OSError("disk exploded: /private/path")

    for message in (
        hub_model.submit_word(Broken(), "x"),
        hub_model.submit_replacement(Broken(), "a", "b"),
        hub_model.remove_entry(Broken(), "x"),
    ):
        assert message == hub_model.DICTIONARY_SAVE_FAILED
        assert "private" not in message and "Something went wrong" not in message


def test_remove_entry_removes_and_reports_success(store):
    store.add_word("Kubernetes")
    assert hub_model.remove_entry(store, "Kubernetes") is None
    assert store.words() == []


# ---------------------------------------------------------------- prompts ---

@pytest.fixture
def queue(tmp_path):
    return PromptQueue(tmp_path / "queue.json")


def _two(queue, tmp_path):
    return queue.add(
        [WorkPrompt("Migrate sessions", "body"), WorkPrompt("Update docs", "body")],
        [tmp_path / "a.md", tmp_path / "b.md"],
        "Weekly planning",
    )


def test_prompts_page_sections_and_copy():
    sources = _controller_sources()
    page = sources["_page_prompts"]
    for text in (
        "Prompt shortcuts", "Meeting work prompts", "shortcut_rows(self.config)",
        "Say a word, get a saved prompt", '"See Hot Phrases", "showHotPhrases:"',
    ):
        assert text in page
    assert hub_model.PROMPTS_EMPTY == (
        "Prompts generated from meetings show up here for approval."
    )
    assert "PROMPTS_EMPTY" in page
    assert '"Approve", "approvePrompt:"' in sources["_prompt_row"]
    assert '"Reject", "rejectPrompt:"' in sources["_prompt_row"]
    assert 'self.show_page("Hot Phrases")' in sources["showHotPhrases_"]


def test_hub_builds_with_an_injectable_queue_and_keeps_the_old_initialiser():
    sources = _controller_sources()
    newest = (
        "initWithHistory_dictionary_config_meetingClient_promptQueue_"
        "phrasesModel_initialPage_"
    )
    new = "initWithHistory_dictionary_config_meetingClient_promptQueue_initialPage_"
    old = "initWithHistory_dictionary_config_meetingClient_initialPage_"
    assert "prompt_queue" in sources[newest]
    assert "PromptQueue()" in sources[newest]
    assert "phrases_model" in sources[newest]
    assert newest in sources[new]  # backwards compatible, model defaults
    assert new in sources[old]  # backwards compatible, queue defaults
    source = HUB_MAC.read_text(encoding="utf-8")
    assert 'initial_page="Home", prompt_queue=None' in source
    assert "phrase_store=None" in source and "phrases_model=None" in source


def test_hub_never_imports_the_server():
    tree = _tree(HUB_MAC)
    modules = {
        node.module for node in ast.walk(tree) if isinstance(node, ast.ImportFrom)
    } | {
        alias.name for node in ast.walk(tree) if isinstance(node, ast.Import)
        for alias in node.names
    }
    assert "localflow.server" not in modules
    assert "localflow.dispatch" in modules
    assert "localflow.hub_model" in modules


def test_hub_review_goes_through_the_shared_transition_helpers():
    review = _controller_sources()["_review_prompt"]
    assert "review_prompt(self.prompt_queue, entry_id, action)" in review
    model = (ROOT / "localflow" / "hub_model.py").read_text(encoding="utf-8")
    assert '{"approve": approve_prompt, "reject": reject_prompt}' in model
    dispatch = (ROOT / "localflow" / "dispatch.py").read_text(encoding="utf-8")
    assert "queue.set_status(entry_id, status, expect=allowed)" in dispatch


def test_server_delegates_to_the_same_transition_code():
    source = (ROOT / "localflow" / "server.py").read_text(encoding="utf-8")
    tree = ast.parse(source)
    transition = next(
        node for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "_transition"
    )
    body = ast.get_source_segment(source, transition)
    assert "transition_prompt(_queue, entry_id, verb, status, allowed)" in body
    assert "status_code=exc.status_code" in body and "detail=exc.detail" in body
    assert "set_status" not in body
    assert 'return _transition(entry_id, "approve", "approved", APPROVE_FROM)' in source
    assert 'return _transition(entry_id, "reject", "rejected", REJECT_FROM)' in source
    assert "_APPROVE_FROM" not in source and "_REJECT_FROM" not in source
    assert APPROVE_FROM == ("pending", "failed") and REJECT_FROM == ("pending", "failed")


def test_approve_and_reject_follow_the_server_semantics(queue, tmp_path):
    first, second = _two(queue, tmp_path)
    assert approve_prompt(queue, first["id"])["status"] == "approved"
    assert reject_prompt(queue, second["id"])["status"] == "rejected"
    # approved -> reject and rejected -> approve are conflicts (409).
    with pytest.raises(PromptTransitionError) as conflict:
        reject_prompt(queue, first["id"])
    assert conflict.value.status_code == 409
    assert (
        "cannot reject a approved prompt (allowed from: pending, failed)"
        == conflict.value.detail
    )
    with pytest.raises(PromptTransitionError) as conflict:
        approve_prompt(queue, second["id"])
    assert conflict.value.status_code == 409
    assert queue.get(first["id"])["status"] == "approved"


def test_failed_prompts_can_be_approved_again(queue, tmp_path):
    entry, _other = _two(queue, tmp_path)
    queue.set_status(entry["id"], "failed", error="orca down")
    retried = approve_prompt(queue, entry["id"])
    assert retried["status"] == "approved" and retried["error"] == ""


def test_failed_prompts_can_be_rejected(queue, tmp_path):
    entry, _other = _two(queue, tmp_path)
    queue.set_status(entry["id"], "failed", error="orca down")
    rejected = reject_prompt(queue, entry["id"])
    # Rejecting keeps the failure on record; it is never dispatched now.
    assert rejected["status"] == "rejected" and rejected["error"] == "orca down"
    assert hub_model.review_prompt(queue, entry["id"], "approve") is not None
    assert queue.get(entry["id"])["status"] == "rejected"


@pytest.mark.parametrize("final", ["approved", "rejected", "dispatched"])
def test_reject_is_refused_from_every_other_status(queue, tmp_path, final):
    # Allowing failed must not open any other transition.
    entry, _other = _two(queue, tmp_path)
    queue.set_status(entry["id"], final)
    with pytest.raises(PromptTransitionError) as conflict:
        reject_prompt(queue, entry["id"])
    assert conflict.value.status_code == 409
    assert queue.get(entry["id"])["status"] == final
    message = hub_model.review_prompt(queue, entry["id"], "reject")
    assert "changed before it could be rejected" in message
    assert queue.get(entry["id"])["status"] == final


@pytest.mark.parametrize("final", ["approved", "rejected", "dispatched"])
def test_approve_is_still_refused_from_final_statuses(queue, tmp_path, final):
    entry, _other = _two(queue, tmp_path)
    queue.set_status(entry["id"], final)
    with pytest.raises(PromptTransitionError) as conflict:
        approve_prompt(queue, entry["id"])
    assert conflict.value.status_code == 409
    assert queue.get(entry["id"])["status"] == final


def test_server_hub_and_cli_share_the_same_guards():
    # The server endpoint and the hub both go through transition_prompt with
    # the shared tuples; the CLI uses the same tuples (no local copies).
    server = (ROOT / "localflow" / "server.py").read_text(encoding="utf-8")
    assert 'return _transition(entry_id, "reject", "rejected", REJECT_FROM)' in server
    cli = (ROOT / "localflow" / "cli.py").read_text(encoding="utf-8")
    assert '_set_reviewed(ids, "rejected", REJECT_FROM)' in cli
    assert '_set_reviewed(ids, "approved", APPROVE_FROM)' in cli
    assert '("pending",)' not in cli and '("pending", "failed"))' not in cli
    for row in hub_model.prompt_rows(
        [{"id": s, "status": s} for s in ("pending", "approved", "rejected",
                                          "dispatched", "failed")]
    ):
        assert row["can_reject"] == (row["status"] in REJECT_FROM)
        assert row["can_approve"] == (row["status"] in APPROVE_FROM)


def test_server_reject_endpoint_accepts_failed_and_refuses_final(tmp_path, monkeypatch):
    fastapi = pytest.importorskip("fastapi")
    from localflow import server

    queue = PromptQueue(tmp_path / "server-queue.json")
    entries = queue.add(
        [WorkPrompt("A", "a"), WorkPrompt("B", "b")],
        [tmp_path / "a.md", tmp_path / "b.md"], "Planning",
    )
    queue.set_status(entries[0]["id"], "failed", error="orca down")
    queue.set_status(entries[1]["id"], "dispatched")
    monkeypatch.setattr(server, "_queue", queue)
    assert server.prompts_reject(entries[0]["id"])["status"] == "rejected"
    with pytest.raises(fastapi.HTTPException) as refused:
        server.prompts_reject(entries[1]["id"])
    assert refused.value.status_code == 409
    assert queue.get(entries[1]["id"])["status"] == "dispatched"


def test_dispatched_prompts_cannot_move(queue, tmp_path):
    entry, _other = _two(queue, tmp_path)
    queue.set_status(entry["id"], "dispatched")
    for action in (approve_prompt, reject_prompt):
        with pytest.raises(PromptTransitionError) as conflict:
            action(queue, entry["id"])
        assert conflict.value.status_code == 409
    assert queue.get(entry["id"])["status"] == "dispatched"


def test_missing_id_is_a_404(queue):
    for action in (approve_prompt, reject_prompt):
        with pytest.raises(PromptTransitionError) as missing:
            action(queue, "nope")
        assert missing.value.status_code == 404
        assert missing.value.detail == "unknown prompt: nope"


def test_stale_click_loses_the_compare_and_set_race(queue, tmp_path):
    entry, _other = _two(queue, tmp_path)
    # The CLI (another process, another instance) dispatches after the hub
    # rendered the entry as pending; the click must not undo that.
    PromptQueue(queue.path).set_status(entry["id"], "dispatched")
    with pytest.raises(PromptTransitionError) as lost:
        transition_prompt(queue, entry["id"], "approve", "approved", APPROVE_FROM)
    assert lost.value.status_code == 409
    assert queue.get(entry["id"])["status"] == "dispatched"
    message = hub_model.review_prompt(queue, entry["id"], "approve")
    assert "changed before it could be approved" in message
    assert queue.get(entry["id"])["status"] == "dispatched"


def test_review_prompt_reports_friendly_messages(queue, tmp_path):
    entry, other = _two(queue, tmp_path)
    assert hub_model.review_prompt(queue, entry["id"], "approve") is None
    assert hub_model.review_prompt(queue, other["id"], "reject") is None
    assert "changed before it could be rejected" in hub_model.review_prompt(
        queue, entry["id"], "reject"
    )
    missing = hub_model.review_prompt(queue, "nope", "approve")
    assert "no longer in the queue" in missing
    assert "unknown prompt" not in missing


def test_review_prompt_survives_an_unwritable_queue(tmp_path):
    class Broken:
        def set_status(self, *_args, **_kwargs):
            raise OSError("read-only /private/path")

    message = hub_model.review_prompt(Broken(), "abc", "approve")
    assert message == (
        "Couldn't update the prompt queue. Check ~/.localflow and try again."
    )


def test_prompt_rows_use_repo_then_meeting_and_guard_the_buttons():
    entries = [
        {"id": "a", "title": "Pending", "status": "pending", "meeting": "Standup"},
        {"id": "b", "title": "Failed", "status": "failed", "repo": "localflow",
         "meeting": "Planning", "error": "Orca is not running"},
        {"id": "c", "title": "Done", "status": "dispatched"},
        {"id": "d", "title": "", "status": "rejected", "meeting": "Retro"},
    ]
    rows = {row["id"]: row for row in hub_model.prompt_rows(entries)}
    assert [row["id"] for row in hub_model.prompt_rows(entries)] == ["d", "c", "b", "a"]
    assert rows["a"]["context"] == "Standup"
    # The stored technical error becomes the shared friendly sentence.
    assert rows["b"]["context"] == (
        "localflow · Couldn't send — the Orca agent app isn't open"
    )
    assert rows["b"]["error"] == hub_model.ORCA_NOT_OPEN
    assert "Orca is not running" not in rows["b"]["context"]
    assert rows["c"]["context"] == "Unknown meeting"
    assert rows["d"]["title"] == "Untitled prompt"
    flags = {k: (v["can_approve"], v["can_reject"]) for k, v in rows.items()}
    assert flags == {
        "a": (True, True), "b": (True, True),
        "c": (False, False), "d": (False, False),
    }
    assert hub_model.prompt_rows([]) == []


def test_prompt_errors_map_only_the_known_orca_cause():
    text = hub_model.prompt_error_text
    for stored in (
        "Orca is not running",
        "Orca app is not running — run `orca open`",
        (
            "Orca app is not running — run `orca open` (not possible over SSH: it "
            "launches a GUI app on the Mac's own display)"
        ),
    ):
        assert text(stored) == "Couldn't send — the Orca agent app isn't open"
    # Unrelated failures keep their own meaning; nothing is masked.
    for stored in (
        "orca exited 2: repo not found",
        "orca CLI not found on PATH",
        "Orca runtime not reachable (stopped) — run `orca open`",
        "cannot read prompt file /tmp/x.md: [Errno 2] No such file",
        "orca worktree create timed out after 120s",
    ):
        assert text(stored) == stored
    assert text("") == "" and text(None) == ""
    rows = hub_model.prompt_rows([
        {"id": "x", "status": "failed", "repo": "r", "error": "orca exited 2: repo not found"},
        {"id": "y", "status": "pending", "error": "Orca is not running"},
    ])
    assert rows[1]["context"] == "r · orca exited 2: repo not found"
    assert rows[0]["context"] == "Unknown meeting" and rows[0]["error"] == ""


def _text_measure(text):
    # A deliberately generous 11pt width estimate (wider than SF Pro).
    return len(text) * 6.8


@pytest.mark.parametrize("width", [
    HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * 16, 1280 - HUB_CONTENT_X - 2 * 16,
])
def test_prompt_rows_wrap_the_failure_instead_of_truncating(width):
    row = hub_model.prompt_rows([
        {"id": "f", "status": "failed", "repo": "localflow",
         "error": "Orca is not running", "title": "Migrate"},
    ])[0]
    assert row["can_approve"] and row["can_reject"]
    geometry = hub_model.prompt_row_layout(width, row, 60, _text_measure)
    lines = hub_model.wrap_lines(row["context"], geometry["text_width"], _text_measure)
    # Every wrapped line fits the text column, and the column is tall enough.
    assert all(_text_measure(line) <= geometry["text_width"] for line in lines)
    assert " ".join(lines) == row["context"]
    assert geometry["context_h"] >= len(lines) * hub_model.PROMPT_CONTEXT_LINE
    assert geometry["height"] >= hub_model.PROMPT_CONTEXT_Y + geometry["context_h"]
    # Approve + Reject + pill + text never overlap and stay inside the row.
    assert 14 + geometry["text_width"] <= geometry["pill_x"]
    assert geometry["pill_x"] + 60 <= geometry["action_x"]
    assert geometry["action_x"] + 88 + 8 + 80 <= width - 12
    if width < 700:
        assert len(lines) >= 2 and geometry["height"] > hub_model.PROMPT_ROW_MIN_H


def test_prompt_rows_keep_the_compact_height_for_short_context():
    row = hub_model.prompt_rows([{"id": "p", "status": "pending", "meeting": "Standup"}])[0]
    geometry = hub_model.prompt_row_layout(600, row, 60, _text_measure)
    assert geometry["height"] == hub_model.PROMPT_ROW_MIN_H and geometry["lines"] == 1


def test_prompts_page_rows_use_the_wrapping_layout_and_both_actions():
    sources = _controller_sources()
    row = sources["_prompt_row"]
    assert "prompt_row_layout(" in row and "wrap=True" in row
    assert 'height=geometry["context_h"]' in row
    assert 'return geometry["height"]' in row
    page = sources["_page_prompts"]
    assert "y += height" in page
    snapshots = (ROOT / "scripts" / "mac_ui_snapshots.py").read_text(encoding="utf-8")
    assert '"hub-prompts-failures"' in snapshots
    assert "orca exited 2: repo not found" in snapshots


def test_shortcut_rows_follow_the_config_and_invent_no_enable_flag():
    config = SimpleNamespace(
        prompt_hotkey="<ctrl>+<alt>+2", polish_hotkey="", command_hotkey="alt_r",
    )
    rows = hub_model.shortcut_rows(config)
    assert [row["name"] for row in rows] == ["Prompt Engineer", "Polish", "Command Mode"]
    assert [row["hotkey"] for row in rows] == [
        "Control + Option + 2", "Off", "Right Option",
    ]
    assert all(set(row) == {"name", "detail", "hotkey"} for row in rows)
    assert "toggle" not in _controller_sources()["_page_prompts"].lower()
    assert "Select rough text" in rows[0]["detail"]


def test_prompts_page_reads_the_queue_without_crashing_on_errors():
    page = _controller_sources()["_prompt_entries"]
    assert "self.prompt_queue.list()" in page
    assert "PROMPTS_UNREADABLE" in page


def test_prompt_and_empty_copy_wrap_instead_of_truncating():
    sources = _controller_sources()
    assert "wrap=True" in sources["_page_prompts"]
    assert "wrap=True" in sources["_empty_state"]
    assert "wrap=True" in sources["_models_editor"]
    assert "wrap=True" in sources["_page_hot_phrases"]
    # The teaser label stops before its button rather than spanning the card.
    page = sources["_page_prompts"]
    assert "width - 3 * CARD_PADDING - 160" in page
    label = next(
        node for node in _tree(HUB_MAC).body
        if isinstance(node, ast.FunctionDef) and node.name == "_label"
    )
    assert "wrap" in {arg.arg for arg in label.args.kwonlyargs}


# -------------------------------------------- dictionary remove by row kind ---

def test_store_remove_default_still_removes_both_kinds(store):
    store.add_word("teh")
    store.add_replacement("teh", "the")
    assert store.remove("teh") is True
    assert store.list() == {"words": [], "replacements": []}


def test_store_remove_can_target_one_kind(store):
    store.add_word("teh")
    store.add_replacement("TEH", "the")
    assert store.remove("teh", "word") is True
    assert store.words() == [] and store.replacements() == [("TEH", "the")]
    assert store.remove("teh", "word") is False
    store.add_word("teh")
    assert store.remove("teh", "replacement") is True
    assert store.words() == ["teh"] and store.replacements() == []
    with pytest.raises(ValueError):
        store.remove("teh", "nonsense")


# ------------------------------------- controller handlers, run for real ---
# The real handler source is extracted from hub_mac.py (AppKit cannot be
# imported here) and run against fake fields and a real temporary store.

_HANDLERS = (
    "addWord_", "addReplacement_", "removeDictionary_", "editDictionary_",
    "_reload_dictionary", "_preserve_page_state", "_restore_page_state",
    "_sync_dictionary_buttons",
)


class _Field:
    def __init__(self, value=""):
        self.value = value

    def stringValue(self):
        return self.value

    def setStringValue_(self, value):
        self.value = value


class _Button:
    title = ""

    def setTitle_(self, title):
        self.title = title


class _Sender:
    def __init__(self, tag):
        self._tag = tag

    def tag(self):
        return self._tag


def _extract_handlers():
    cls = next(
        node for node in _tree(HUB_MAC).body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    functions = []
    for node in cls.body:
        if isinstance(node, ast.FunctionDef) and node.name in _HANDLERS:
            node.decorator_list = []
            functions.append(node)
    assert {f.name for f in functions} == set(_HANDLERS)
    module = ast.fix_missing_locations(ast.Module(body=functions, type_ignores=[]))
    namespace = {
        "submit_word": hub_model.submit_word,
        "submit_replacement": hub_model.submit_replacement,
        "remove_entry": hub_model.remove_entry,
    }
    exec(compile(module, "hub_mac_handlers", "exec"), namespace)  # noqa: S102
    return {name: namespace[name] for name in _HANDLERS}


class _Controller:
    """Stands in for HubWindowController: real handlers, fake widgets."""

    def __init__(self, store):
        self.dictionary = store
        self.page = "Dictionary"
        self.page_errors = {}
        self.page_notices = {}
        self.dictionary_error_card = None
        self.dictionary_keys = {}
        self.rebuilds = 0
        self.show_page("Dictionary")

    def show_page(self, page):
        # Like _page_dictionary: a rebuild creates empty fields and no selection.
        self.rebuilds += 1
        self.page = page
        self.word_field, self.from_field, self.to_field = _Field(), _Field(), _Field()
        self.add_word_button, self.add_replacement_button = _Button(), _Button()
        self.editing_word = None
        self.editing_source = None

    def typed(self):
        return (
            self.word_field.value, self.from_field.value, self.to_field.value,
            self.editing_word, self.editing_source,
        )


for _name, _function in _extract_handlers().items():
    setattr(_Controller, _name, _function)


@pytest.fixture
def controller(store):
    return _Controller(store)


def _seed_error(controller, card):
    controller.page_errors["Dictionary"] = "old error"
    controller.dictionary_error_card = card


def test_add_word_success_clears_its_fields_and_the_error_only(controller, store):
    controller.word_field.value = "Kubernetes"
    controller.from_field.value, controller.to_field.value = "brb", "be right back"
    controller.editing_source = "brb"
    _seed_error(controller, "word")
    controller.addWord_(None)
    assert store.words() == ["Kubernetes"] and store.replacements() == []
    assert "Dictionary" not in controller.page_errors
    assert controller.dictionary_error_card is None
    assert controller.rebuilds == 2
    # The word card is cleared; the other card's draft and selection survive.
    assert controller.typed() == ("", "brb", "be right back", None, "brb")


def test_add_replacement_success_clears_its_fields_and_the_error_only(controller, store):
    controller.word_field.value = "half typed"
    controller.editing_word = "Old"
    controller.from_field.value, controller.to_field.value = "teh", "the"
    _seed_error(controller, "replacement")
    controller.addReplacement_(None)
    assert store.replacements() == [("teh", "the")] and store.words() == []
    assert "Dictionary" not in controller.page_errors
    assert controller.typed() == ("half typed", "", "", "Old", None)
    assert controller.add_word_button.title == "Save word"
    assert controller.add_replacement_button.title == "Add replacement"


def test_editing_a_word_saves_in_place_and_leaves_edit_mode(controller, store):
    store.add_word("Kubernetes")
    controller.editing_word = "Kubernetes"
    controller.word_field.value = "K8s"
    controller.addWord_(None)
    assert store.words() == ["K8s"]
    assert controller.typed()[0] == "" and controller.editing_word is None
    assert controller.add_word_button.title == "Add word"


def test_word_error_keeps_every_typed_value_and_editing_id(controller, store):
    controller.word_field.value = "   "
    controller.from_field.value, controller.to_field.value = "brb", "be right back"
    controller.editing_word, controller.editing_source = "Old", "brb"
    controller.addWord_(None)
    assert controller.page_errors["Dictionary"] == "Type a word first."
    assert controller.dictionary_error_card == "word"
    assert controller.typed() == ("   ", "brb", "be right back", "Old", "brb")
    assert controller.add_word_button.title == "Save word"
    assert controller.add_replacement_button.title == "Save replacement"
    assert store.list() == {"words": [], "replacements": []}


def test_replacement_error_keeps_every_typed_value_and_editing_id(controller, store):
    controller.word_field.value = "draft word"
    controller.from_field.value, controller.to_field.value = "brb", ""
    controller.editing_word, controller.editing_source = "Old", "brb"
    controller.addReplacement_(None)
    assert controller.page_errors["Dictionary"] == "Type what it should become."
    assert controller.dictionary_error_card == "replacement"
    assert controller.typed() == ("draft word", "brb", "", "Old", "brb")
    assert store.list() == {"words": [], "replacements": []}


def test_a_later_success_replaces_an_earlier_error(controller, store):
    controller.addWord_(None)
    assert controller.page_errors["Dictionary"] == "Type a word first."
    controller.word_field.value = "Kubernetes"
    controller.addWord_(None)
    assert "Dictionary" not in controller.page_errors
    assert store.words() == ["Kubernetes"]


def test_duplicate_word_error_keeps_the_text(controller, store):
    store.add_word("Kubernetes")
    controller.word_field.value = "kubernetes"
    controller.addWord_(None)
    assert "already in your dictionary" in controller.page_errors["Dictionary"]
    assert controller.word_field.value == "kubernetes"


def test_remove_deletes_only_the_clicked_row(controller, store):
    store.add_word("teh")
    store.add_replacement("teh", "the")
    controller.dictionary_keys = {
        0: ("teh", "word", ""), 1: ("teh", "replacement", "the"),
    }
    controller.removeDictionary_(_Sender(0))
    assert store.words() == [] and store.replacements() == [("teh", "the")]
    store.add_word("teh")
    controller.removeDictionary_(_Sender(1))
    assert store.words() == ["teh"] and store.replacements() == []


def test_remove_only_forgets_the_editing_id_of_the_removed_kind(controller, store):
    store.add_word("teh")
    store.add_replacement("teh", "the")
    controller.dictionary_keys = {0: ("teh", "word", "")}
    controller.editing_word, controller.editing_source = "teh", "teh"
    controller.word_field.value = "teh"
    controller.removeDictionary_(_Sender(0))
    assert controller.editing_word is None and controller.editing_source == "teh"
    assert controller.word_field.value == "teh"


def test_remove_failure_is_friendly_and_keeps_drafts(controller, store):
    class Broken:
        def remove(self, *_args):
            raise OSError("nope")

    controller.dictionary = Broken()
    controller.dictionary_keys = {0: ("teh", "word", "")}
    controller.word_field.value = "draft"
    controller.removeDictionary_(_Sender(0))
    assert controller.page_errors["Dictionary"] == hub_model.DICTIONARY_SAVE_FAILED
    assert controller.word_field.value == "draft"
