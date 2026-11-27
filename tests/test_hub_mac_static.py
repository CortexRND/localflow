import ast
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).parent.parent
SOURCE = ROOT / "localflow" / "hub_mac.py"


def _controller_methods():
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
    cls = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    return [node for node in cls.body if isinstance(node, ast.FunctionDef)]


def _python_method(function):
    return any(
        isinstance(decorator, ast.Attribute)
        and isinstance(decorator.value, ast.Name)
        and decorator.value.id == "objc"
        and decorator.attr == "python_method"
        for decorator in function.decorator_list
    )


def test_appkit_hub_has_valid_selector_names():
    invalid = []
    for method in _controller_methods():
        if _python_method(method):
            continue
        args = len(method.args.args) + len(method.args.kwonlyargs) - 1
        if method.name.startswith("_") or method.name.count("_") != args:
            invalid.append(method.name)
    assert not invalid


def test_ask_page_wiring_and_background_work():
    source = SOURCE.read_text()
    assert '"Home", "Ask", "Hot Phrases"' in source
    assert "def _page_ask(self):" in source
    assert "self.ask_page.render()" in source
    assert '"Home", "Ask", "Hot Phrases"' in (ROOT / "localflow/hub_route.py").read_text()
    ask_source = (ROOT / "localflow/ask_mac.py").read_text()
    assert 'threading.Thread(target=run, name="localflow-ask", daemon=True).start()' in ask_source
    assert 'THINKING = "Thinking..."' in ask_source
    assert '("assistant", THINKING, False)' in ask_source
    assert "self.events.get_nowait()" in ask_source
    assert "self.hub.history.get(target)" in ask_source
    assert "current().open_path(target)" in ask_source
    tree = ast.parse(ask_source)
    cls = next(n for n in tree.body if isinstance(n, ast.ClassDef))
    for method in cls.body:
        if isinstance(method, ast.FunctionDef) and not _python_method(method):
            assert method.name.count("_") == len(method.args.args) - 1
    snapshots = (ROOT / "scripts/mac_ui_snapshots.py").read_text()
    for state in ("empty", "conversation", "thinking", "error"):
        assert f'"{state}"' in snapshots
    assert '_hub_sized_shots(controller, f"hub-ask-{state}")' in snapshots


def test_appkit_hub_imports_foundation_timer():
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
    assert any(
        isinstance(node, ast.ImportFrom)
        and node.module == "Foundation"
        and any(alias.name == "NSTimer" for alias in node.names)
        for node in tree.body
    )


def test_appkit_meeting_note_saves_are_serialized_before_stop():
    source = SOURCE.read_text(encoding="utf-8")
    assert "def _meeting_request_notes_save(self, stop_flush=False):" in source
    assert "if self.meeting_notes_save_in_flight:" in source
    assert "self.meeting_stop_flush_pending = True" in source
    assert "self.meeting_notes_dirty = current_text != text" in source
    assert 'self._meeting_worker("stop")' in source


def test_appkit_meeting_completions_drain_while_page_is_hidden():
    source = SOURCE.read_text(encoding="utf-8")
    timer = next(
        method for method in _controller_methods()
        if method.name == "meetingTimerTick_"
    )
    timer_source = ast.get_source_segment(source, timer)
    assert "self._meeting_event(operation, result, error)" in timer_source
    assert timer_source.index("self._meeting_event(operation, result, error)") < (
        timer_source.index('if self.page == "Meetings"')
    )


def test_appkit_meeting_status_and_transcript_follow_the_action():
    source = SOURCE.read_text(encoding="utf-8")
    methods = {
        method.name: ast.get_source_segment(source, method)
        for method in _controller_methods()
    }
    layout = methods["_meeting_layout"]
    render = methods["_meeting_render"]
    action_title = methods["_meeting_set_action_title"]

    assert "action_frame = self.meeting_action.frame()" in layout
    assert "action_frame.origin.y + action_frame.size.height + 4" in layout
    assert "self.meeting_live_card.setHidden_(not bool(session))" in layout
    assert "saved_y = live_y + (108 if session else 8)" in layout
    assert "self.meeting_action.sizeToFit()" in action_title
    assert "frame.size.width + 24" in action_title

    for view in (
        "meeting_chat_heading",
        "meeting_question",
        "meeting_ask",
    ):
        assert f"self.{view}.setHidden_(not chat_available)" in render
    assert "self.meeting_answer_scroll.setHidden_(" in render
    assert "not chat_available or not self.meeting_answers" in render
    assert "not session" in render


def test_appkit_speaker_label_line_sits_under_the_watch_row():
    source = SOURCE.read_text(encoding="utf-8")
    methods = {
        method.name: ast.get_source_segment(source, method)
        for method in _controller_methods()
    }
    render = methods["_meeting_render"]
    layout = methods["_meeting_layout"]
    page = methods["_page_meetings"]
    controls = methods["_meeting_page_controls"]

    assert "meeting_capture_status(session)" in render
    assert "meeting_diarization_status(session)" in render
    idle = render.split("else:", 1)[1]
    assert 'self.meeting_diarization.setStringValue_("")' in idle
    assert "'Me' if segment.get('source') == 'me' else 'Others'" in render

    assert "meeting_diarization_height(" in layout
    assert "self.meeting_diarization.setHidden_(not diarization_h)" in layout
    assert (
        "live_y = watch_y + (26 if watch_visible else 0) + diarization_h" in layout
    )
    assert "saved_y = live_y + (108 if session else 8)" in layout

    built = page.split("self.meeting_diarization = _label(", 1)[1].split(")", 1)[0]
    assert "wrap=True" in built
    assert "self.meeting_diarization.setHidden_(True)" in page
    assert "self.meeting_diarization" in controls

    # The view only shows the sanitized sentence, never the raw error.
    assert "diarization_error" not in source


def test_hub_snapshot_workflow_covers_each_page_and_error_state():
    snapshots = (ROOT / "scripts" / "mac_ui_snapshots.py").read_text()
    workflow = (ROOT / ".github" / "workflows" / "macos-ui.yml").read_text()
    assert "from localflow.hub_mac import build_window" in snapshots
    assert "SnapshotMeetingClient" in snapshots
    assert '("idle", "running", "running-no-system-audio", "saved")' in snapshots
    assert 'f"hub-meetings-{state}"' in snapshots
    assert '"Home", "History", "Dictionary", "Style", "Models"' in snapshots
    assert '"Unable to load local data."' in snapshots
    assert "localflow/hub*" in workflow


def test_label_calls_do_not_repeat_positional_parameters():
    tree = ast.parse(SOURCE.read_text(encoding="utf-8"))
    helper = next(
        node for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "_label"
    )
    parameter_names = [argument.arg for argument in helper.args.args]
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        if not isinstance(node.func, ast.Name) or node.func.id != "_label":
            continue
        positional = set(parameter_names[:len(node.args)])
        keywords = {keyword.arg for keyword in node.keywords}
        assert not positional & keywords


def test_appkit_label_helpers_keep_optional_layout_options_keyword_only():
    modules = (
        ("hub_mac.py", {"palette", "height", "weight"}),
        ("setup_mac.py", {"height"}),
        ("phrases_mac.py", {"font", "color", "wrap"}),
    )
    for filename, keyword_only in modules:
        tree = ast.parse((ROOT / "localflow" / filename).read_text(encoding="utf-8"))
        helper = next(
            node for node in ast.walk(tree)
            if isinstance(node, ast.FunctionDef) and node.name == "_label"
        )
        assert keyword_only <= {arg.arg for arg in helper.args.kwonlyargs}
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            is_label = (
                isinstance(node.func, ast.Name) and node.func.id == "_label"
            ) or (
                isinstance(node.func, ast.Attribute) and node.func.attr == "_label"
            )
            if is_label:
                assert len(node.args) <= 6, f"{filename}:{node.lineno}"


def test_appkit_history_uses_a_cleared_paged_results_container():
    source = SOURCE.read_text(encoding="utf-8")
    assert "self.history_results.subviews()" in source
    assert "view.removeFromSuperview()" in source
    assert "offset=self.history_offset" in source
    assert '"olderHistory:"' in source
    assert '"newerHistory:"' in source
    assert "page_y = listing_height + 16" in source
    assert "self.history_results.setFrame_(" in source
    assert "self.history_results_height - 52" not in source


def test_history_search_and_clear_history_use_the_header_layout():
    source = SOURCE.read_text(encoding="utf-8")
    assert 'self.search_field.setPlaceholderString_("Search transcripts")' in source
    # Pinned to the right edge of the current page width, on the title row.
    assert '"Clear history", "clearHistory:", width - 130, 0' in source
    assert 'kind="destructive_ghost"' in source
    assert '"Clear history", "clearHistory:", width - 130, self.cursor_y' not in source


def test_window_appearance_callback_is_registered_on_a_retained_controller():
    source = SOURCE.read_text(encoding="utf-8")
    tree = ast.parse(source)
    controller = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    assert any(
        isinstance(base, ast.Name) and base.id == "NSObject"
        for base in controller.bases
    )
    assert "self.window.setDelegate_(self)" in source
    assert "controller = build_window(initial_page=initial_page)" in source
    assert "AppHelper.runEventLoop()" in source


def test_hub_settings_use_segmented_controls_and_reload_models_config():
    source = SOURCE.read_text(encoding="utf-8")
    methods = {
        method.name: ast.get_source_segment(source, method)
        for method in _controller_methods()
    }
    assert "NSPopUpButton" not in methods["_page_style"]
    assert "NSPopUpButton.alloc()" in methods["_quick_picker"]
    assert "NSComboBox" not in methods["_quick_picker"]
    assert '"strict"' not in source
    assert '("formal", "casual", "very_casual", "excited")' in source
    assert "self.config = load_config()" in source
    # Edit-in-place still goes through the store's update APIs, via hub_model.
    model = (ROOT / "localflow" / "hub_model.py").read_text(encoding="utf-8")
    assert "store.update_word(" in model
    assert "store.update_replacement(" in model
    assert "submit_word(" in source
    assert "submit_replacement(" in source
    assert '"selectCleanupLevel_"' in source
    assert '"selectStyle_"' in source


def test_hub_buttons_use_setup_pop_art_tokens():
    source = SOURCE.read_text(encoding="utf-8")
    style = (ROOT / "localflow" / "mac_style.py").read_text(encoding="utf-8")
    assert "color as _color" in source
    assert "from localflow.ui_tokens import" in style
    assert "def style_flat_button(button, selected=False, palette=None)" in style
    assert '_color("sidebar", self.tokens)' in source
    assert "button.setAlignment_(AppKit.NSTextAlignmentLeft)" in source
    assert '"sidebar_idle": palette["sidebar"]' in style
    assert "color(palette[\"shadow\"])" in style
    assert 'selected=page == self.page' in source
    assert "selected=PAGES[button.tag()] == page" in source
    assert "layer.setBorderWidth_(" in style


def test_hub_sidebar_items_use_borderless_flat_button_style():
    source = SOURCE.read_text(encoding="utf-8")
    style_path = ROOT / "localflow" / "mac_style.py"
    style_source = style_path.read_text(encoding="utf-8")
    tree = ast.parse(style_source)
    flat_style = next(
        node for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name == "style_flat_button"
    )
    flat_source = ast.get_source_segment(style_source, flat_style)
    assert "style_flat_button as _style_button" in source
    assert "_style_button(" in source
    assert "selected=PAGES[button.tag()] == page" in source
    assert "button.setBordered_(False)" in flat_source
    assert "layer.setBorderWidth_(0)" in flat_source
    assert 'color("sun" if selected else "sidebar", palette)' in flat_source


def test_hub_style_rows_use_the_shared_segmented_helper():
    hub_source = SOURCE.read_text(encoding="utf-8")
    setup_path = ROOT / "localflow" / "setup_mac.py"
    setup_source = setup_path.read_text(encoding="utf-8")
    hub_tree = ast.parse(hub_source)
    controller = next(
        node for node in hub_tree.body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    style_page = next(
        node for node in controller.body
        if isinstance(node, ast.FunctionDef) and node.name == "_page_style"
    )
    style_page_source = ast.get_source_segment(hub_source, style_page)
    assert "segmented as _segmented" in hub_source
    assert "self._segment_group(y, 180, group_width)" in style_page_source
    assert "group_width = min(560, self.body.frame().size.width - 180)" in style_page_source
    assert "self.style_groups[category] = group" in style_page_source
    assert "_segmented(" in style_page_source
    assert "segmented as _segmented" in setup_source
    assert (
        "_segmented(group, self.cleanup_level_buttons, selected, self.tokens)"
        in setup_source
    )


def test_appkit_open_notes_only_opens_vault_markdown():
    method = next(
        node for node in _controller_methods() if node.name == "meetingOpenNotes_"
    )
    body = ast.unparse(method)
    assert "vault_notes_path(" in body
    assert "load_config().vault_path" in body
    calls = [
        node for node in ast.walk(method)
        if isinstance(node, ast.Call)
        and isinstance(node.func, ast.Attribute)
        and node.func.attr == "open_path"
    ]
    assert calls and all(
        isinstance(call.args[0], ast.Name) and call.args[0].id == "path"
        for call in calls
    )


# ----------------------------------------------------------------- speakers ---

SPEAKER_SELECTORS = (
    "meetingSpeakerRename_", "meetingSpeakerMerge_", "meetingSpeakerTarget_",
    "meetingSpeakerSave_", "meetingSpeakerConfirmMerge_", "meetingSpeakerCancel_",
)
SPEAKER_MID = "2026-11-25-1430"


def _method_sources():
    source = SOURCE.read_text(encoding="utf-8")
    return {
        method.name: ast.get_source_segment(source, method)
        for method in _controller_methods()
    }


def _extract(names, namespace=None):
    """Run controller methods outside AppKit; an empty namespace by default,
    so a method reaching for a module-level name fails loudly."""
    namespace = {} if namespace is None else namespace
    functions = []
    for node in _controller_methods():
        if node.name in names:
            node.decorator_list = []
            functions.append(node)
    assert {f.name for f in functions} == set(names)
    module = ast.fix_missing_locations(ast.Module(body=functions, type_ignores=[]))
    exec(compile(module, "hub_mac_extract", "exec"), namespace)  # noqa: S102
    return {name: namespace[name] for name in names}


def _bind(controller, methods):
    for name, function in methods.items():
        setattr(controller, name, function.__get__(controller))
    return controller


def _speaker_panel(*speakers):
    from localflow.hub_model import SpeakerPanel

    panel = SpeakerPanel()
    panel.sync({
        "meeting_id": SPEAKER_MID,
        "speakers": [
            {"id": sid, "label": label, "seconds": 30}
            for sid, label in (speakers or (("S1", "Speaker 1"), ("S2", "Speaker 2")))
        ],
    })
    return panel


class _FakeView:
    def __init__(self, value=""):
        self.frames = []
        self.value = value

    def setFrame_(self, frame):
        self.frames.append(frame)

    def stringValue(self):
        return self.value


def test_meeting_layout_inserts_the_speakers_block_after_the_saved_row():
    layout = _method_sources()["_meeting_layout"]
    saved = "saved_y = live_y + (108 if session else 8)"
    chat = "chat_y = saved_y + (48 if saved else 0)"
    speakers = "chat_y = self._meeting_layout_speakers(chat_y, width)"
    assert saved in layout and chat in layout and speakers in layout
    assert layout.index(saved) < layout.index(chat) < layout.index(speakers)
    assert layout.index(speakers) < layout.index("self.meeting_chat_heading.setFrame_(")


def test_page_meetings_builds_no_speakers_view():
    page = _method_sources()["_page_meetings"]
    for name in ("SPEAKERS_HEADING", "_meeting_speakers_build", "SpeakerPanel("):
        assert name not in page


def test_meeting_render_draws_speakers_before_layout():
    render = _method_sources()["_meeting_render"]
    assert "self._meeting_render_speakers(saved)" in render
    assert render.index("self._meeting_render_speakers(saved)") < render.index(
        "self._meeting_layout("
    )


def test_speaker_selectors_never_call_the_client_directly():
    methods = {method.name: method for method in _controller_methods()}
    sources = _method_sources()
    for name in SPEAKER_SELECTORS:
        assert name in methods, name
        assert not _python_method(methods[name])
        assert "self.meeting_client" not in sources[name]
        assert 'if self.page != "Meetings":' in sources[name]
    send = sources["_meeting_speaker_send"]
    assert "threading.Thread(" in send
    assert 'self.meeting_events.put(("speakers", (token, result), error))' in send


def test_status_sync_uses_the_polls_own_last_saved_and_ticket():
    event = _method_sources()["_meeting_event"]
    assert 'panel.sync(result["last_saved"], getattr(result, "speaker_ticket", None))' in event
    assert "panel.sync(self.meeting_last_saved" not in event
    worker = _method_sources()["_meeting_worker"]
    assert "panel.stamp(result, ticket)" in worker
    assert 'getattr(self, "meeting_speakers", None)' in worker


def test_layout_speakers_without_a_card_returns_y():
    methods = _extract(("_meeting_layout_speakers",))
    controller = _bind(SimpleNamespace(meeting_speakers_card=None), methods)
    assert controller._meeting_layout_speakers(312, 600) == 312


def test_layout_speakers_places_the_block_from_the_pure_layout():
    from localflow.hub_model import meeting_speakers_layout

    methods = _extract(
        ("_meeting_layout_speakers",),
        {"meeting_speakers_layout": meeting_speakers_layout, "NSMakeRect": lambda *a: a},
    )
    heading, card, error = _FakeView(), _FakeView(), _FakeView()
    controller = _bind(
        SimpleNamespace(
            meeting_speakers=_speaker_panel(), meeting_speakers_heading=heading,
            meeting_speakers_card=card, meeting_speakers_error_label=error,
            _meeting_speakers_error_height=lambda _width: 0,
        ),
        methods,
    )
    assert controller._meeting_layout_speakers(300, 600) == 300 + 128
    expected = meeting_speakers_layout(600, 300, 2)
    assert heading.frames == [expected["heading"]]
    assert card.frames == [expected["card"]]
    assert error.frames == []
    controller._meeting_speakers_error_height = lambda _width: 34
    assert controller._meeting_layout_speakers(300, 600) == 300 + 128 + 40
    assert error.frames == [meeting_speakers_layout(600, 300, 2, error_height=34)["error"]]


def _event_controller(page="Meetings"):
    methods = _extract(("_meeting_event",))
    renders = []
    controller = SimpleNamespace(
        page=page, meeting_speakers=_speaker_panel(),
        _meeting_render=lambda: renders.append(True),
    )
    return _bind(controller, methods), renders


def _renamed():
    return {"meeting_id": SPEAKER_MID, "speakers": [
        {"id": "S1", "label": "Sarah"}, {"id": "S2", "label": "Speaker 2"},
    ]}


def test_speakers_event_updates_the_panel_and_renders():
    controller, renders = _event_controller()
    panel = controller.meeting_speakers
    panel.begin_rename("S1")
    _op, _args, token = panel.submit_rename("Sarah")
    controller._meeting_event("speakers", (token - 1, _renamed()), None)
    assert renders == [] and panel.busy == "rename"  # wrong token: nothing
    controller._meeting_event("speakers", (token, _renamed()), None)
    assert renders == [True]
    assert panel.row("S1")["label"] == "Sarah" and panel.busy == ""


def test_speakers_event_off_the_page_updates_without_rendering():
    controller, renders = _event_controller(page="Home")
    panel = controller.meeting_speakers
    panel.begin_rename("S1")
    _op, _args, token = panel.submit_rename("Sarah")
    controller._meeting_event("speakers", (token, _renamed()), None)
    assert renders == [] and panel.row("S1")["label"] == "Sarah"


def test_status_event_with_a_stale_ticket_keeps_the_rows():
    from localflow.hub_model import PolledStatus

    methods = _extract(("_meeting_event",), {"time": SimpleNamespace(monotonic=lambda: 0)})
    controller = _bind(
        SimpleNamespace(
            page="Home", meeting_speakers=_speaker_panel(), meeting_status={},
            meeting_last_saved=None, meeting_session_active=False,
            meeting_notes_dirty=False, meeting_templates=[], meeting_error="",
        ),
        methods,
    )
    panel = controller.meeting_speakers
    stale = panel.ticket()
    panel.begin_rename("S1")
    _op, _args, token = panel.submit_rename("Sarah")
    panel.finish(token, _renamed(), None)
    old = {"session": None, "last_saved": {
        "meeting_id": SPEAKER_MID,
        "speakers": [{"id": "S1", "label": "Speaker 1"}, {"id": "S2", "label": "Speaker 2"}],
    }}
    controller._meeting_event("status", PolledStatus(old, stale), None)
    assert panel.row("S1")["label"] == "Sarah"
    assert controller.meeting_last_saved == old["last_saved"]  # the rest still applies
    controller._meeting_event("status", PolledStatus(old, panel.ticket()), None)
    assert panel.row("S1")["label"] == "Speaker 1"


def test_meeting_speaker_save_sends_one_request():
    methods = _extract(("meetingSpeakerSave_",))
    sent, renders = [], []
    panel = _speaker_panel()
    panel.begin_rename("S2")
    controller = _bind(
        SimpleNamespace(
            page="Meetings", meeting_speakers=panel,
            meeting_speaker_field=_FakeView("  Bo  "),
            _meeting_render=lambda: renders.append(panel.busy),
            _meeting_speaker_send=sent.append,
        ),
        methods,
    )
    controller.meetingSpeakerSave_(None)
    assert sent == [("rename_speaker", (SPEAKER_MID, "S2", "Bo"), panel.epoch)]
    assert renders == ["rename"]  # rendered busy before the send
    controller.meetingSpeakerSave_(None)
    assert len(sent) == 1
    controller.page = "Home"
    panel.busy = ""
    controller.meetingSpeakerSave_(None)
    assert len(sent) == 1


def test_meeting_capture_state_keeps_the_rename_draft():
    methods = _extract(("_meeting_capture_state",))
    panel = _speaker_panel()
    panel.begin_rename("S1")

    class _Combo:
        def indexOfSelectedItem(self):
            return -1

    controller = _bind(
        SimpleNamespace(
            meeting_notes=SimpleNamespace(string=lambda: "notes"),
            meeting_title=_FakeView("Title"), meeting_title_shows_session=False,
            meeting_template=_Combo(), meeting_templates=[],
            meeting_speakers=panel, meeting_speaker_field=_FakeView("Half"),
            meeting_speaker_field_id="S1",
        ),
        methods,
    )
    controller._meeting_capture_state()
    assert panel.draft == "Half"


# ---------------------------------------------------- recent meetings speakers ---

RECENT_SELECTORS = (
    "meetingRecentSpeakers_", "meetingRecentSpeakersRetry_",
    "meetingRecentSpeakerRename_", "meetingRecentSpeakerMerge_",
    "meetingRecentSpeakerTarget_", "meetingRecentSpeakerSave_",
    "meetingRecentSpeakerConfirmMerge_", "meetingRecentSpeakerCancel_",
)
RECENT_MID = "0123456789abcdef0123456789abcdef"
OTHER_MID = "fedcba9876543210fedcba9876543210"
RECENT_NOTE = {"title": "Sync", "date_label": "Today", "path": "/v/sync.md",
               "meeting_id": RECENT_MID}


def _recent_payload(meeting_id=RECENT_MID, first="Speaker 1"):
    return {"meeting_id": meeting_id, "speakers": [
        {"id": "S1", "label": first, "seconds": 30},
        {"id": "S2", "label": "Speaker 2", "seconds": 30},
    ]}


def test_recent_speaker_selectors_never_call_the_client_directly():
    methods = {method.name: method for method in _controller_methods()}
    sources = _method_sources()
    for name in RECENT_SELECTORS:
        assert name in methods, name
        assert not _python_method(methods[name])
        assert "self.meeting_client" not in sources[name]
        assert sources[name].count('if self.page != "Meetings":') == 1
    send = sources["_meeting_recent_send"]
    assert "threading.Thread(" in send
    assert "self.meeting_events.put((event, (token, result), error))" in send
    # The U3 sender is left as it was.
    assert 'self.meeting_events.put(("speakers", (token, result), error))' in (
        sources["_meeting_speaker_send"]
    )


def test_recent_card_uses_the_pure_layout_and_lazy_state():
    sources = _method_sources()
    render = sources["_meeting_render_recent"]
    assert "recent_layout(" in render
    assert "Edit speakers:" in render and "Hide speakers:" in render
    assert '"meetingRecentSpeakers:"' in render
    assert "self.meeting_client" not in render and "_meeting_recent_send" not in render
    page = sources["_page_meetings"]
    assert "self._meeting_recent_reset_views()" in page
    # Built once with the controller, so the hidden set outlives page visits.
    builders = [name for name, text in sources.items() if "RecentSpeakers(" in text]
    assert len(builders) == 1 and builders[0].startswith("initWith"), builders
    # No speakers request when Meetings opens or on the poll.
    for name in ("_page_meetings", "_meeting_poll", "meetingTimerTick_", "_meeting_render"):
        assert "_meeting_recent_send" not in sources[name], name
    assert "state.reconcile(self.meeting_recent)" in sources["_meeting_load_recent"]
    drawer = sources["_meeting_recent_drawer"]
    assert "RECENT_SPEAKER_ACTIONS" in drawer and "self._meeting_speaker_rows(" in drawer
    assert "SPEAKER_ACTIONS" in sources["_meeting_speakers_fill"]


def test_recent_speaker_actions_name_existing_selectors():
    import re as _re

    source = SOURCE.read_text(encoding="utf-8")
    names = {method.name for method in _controller_methods()}
    for table in ("SPEAKER_ACTIONS", "RECENT_SPEAKER_ACTIONS"):
        block = source[source.index(f"\n{table} = {{"):]
        block = block[:block.index("}")]
        selectors = _re.findall(r'"(meeting\w+:)"', block)
        assert len(selectors) == 6, table
        for selector in selectors:
            assert selector.replace(":", "_") in names, selector


def _recent_event_controller(page="Meetings", post_stop=None):
    from localflow.hub_model import RecentSpeakers, SpeakerPanel, mirror_speakers

    methods = _extract(
        ("_meeting_event", "_meeting_speakers_mirror"), {"mirror_speakers": mirror_speakers},
    )
    refreshes, renders = [], []
    controller = SimpleNamespace(
        page=page, meeting_recent_speakers=RecentSpeakers(),
        meeting_speakers=post_stop or SpeakerPanel(), meeting_recent_card=object(),
        meeting_last_saved=None,
        _meeting_recent_refresh=lambda: refreshes.append(True),
        _meeting_render=lambda: renders.append(True),
    )
    return _bind(controller, methods), refreshes, renders


def test_recent_load_event_renders_and_drops_stale_answers():
    controller, refreshes, _renders = _recent_event_controller()
    state = controller.meeting_recent_speakers
    _op, _args, old = state.toggle(RECENT_NOTE)
    _op, _args, token = state.toggle({**RECENT_NOTE, "path": "/v/b.md", "meeting_id": OTHER_MID})
    controller._meeting_event("recent_speakers_load", (old, _recent_payload()), None)
    assert refreshes == [] and state.state == "loading"
    controller._meeting_event("recent_speakers_load", (token, _recent_payload(OTHER_MID)), None)
    assert refreshes == [True] and state.state == "ready"
    assert [row["id"] for row in state.panel.rows] == ["S1", "S2"]


def test_recent_load_404_hides_the_button_without_an_error():
    from localflow.meeting_client import MeetingClientError

    controller, refreshes, _renders = _recent_event_controller()
    state = controller.meeting_recent_speakers
    _op, _args, token = state.toggle(RECENT_NOTE)
    controller._meeting_event(
        "recent_speakers_load", (token, None), MeetingClientError("Not Found", 404),
    )
    assert refreshes == [True]
    assert state.hidden == {RECENT_MID} and state.load_error == "" and state.state == ""


def test_recent_load_off_the_page_updates_without_rendering():
    controller, refreshes, renders = _recent_event_controller(page="Home")
    state = controller.meeting_recent_speakers
    _op, _args, token = state.toggle(RECENT_NOTE)
    controller._meeting_event("recent_speakers_load", (token, _recent_payload()), None)
    assert refreshes == [] and renders == [] and state.state == "ready"


def _post_stop_panel(meeting_id):
    from localflow.hub_model import SpeakerPanel

    panel = SpeakerPanel()
    panel.sync(_recent_payload(meeting_id))
    return panel


def test_recent_rename_mirrors_into_the_post_stop_panel_of_the_same_meeting():
    for meeting_id, mirrored in ((RECENT_MID, True), (OTHER_MID, False)):
        post_stop = _post_stop_panel(meeting_id)
        controller, refreshes, _renders = _recent_event_controller(post_stop=post_stop)
        saved = {**_recent_payload(meeting_id), "notes_path": "/v/sync.md"}
        controller.meeting_last_saved = saved
        state = controller.meeting_recent_speakers
        _op, _args, token = state.toggle(RECENT_NOTE)
        state.loaded(token, _recent_payload(), None)
        state.panel.begin_rename("S1")
        _op, _args, token = state.panel.submit_rename("Sarah")
        controller._meeting_event("recent_speakers", (token, _recent_payload(first="Sarah")), None)
        assert refreshes == [True] and state.panel.row("S1")["label"] == "Sarah"
        expected = "Sarah" if mirrored else "Speaker 1"
        assert post_stop.row("S1")["label"] == expected
        if mirrored:
            assert controller.meeting_last_saved["speakers"][0]["label"] == "Sarah"
            assert controller.meeting_last_saved["notes_path"] == "/v/sync.md"
        else:
            assert controller.meeting_last_saved is saved


def test_post_stop_rename_mirrors_into_the_open_drawer():
    post_stop = _post_stop_panel(RECENT_MID)
    controller, refreshes, renders = _recent_event_controller(post_stop=post_stop)
    state = controller.meeting_recent_speakers
    _op, _args, token = state.toggle(RECENT_NOTE)
    state.loaded(token, _recent_payload(), None)
    post_stop.begin_rename("S1")
    _op, _args, token = post_stop.submit_rename("Sarah")
    controller._meeting_event("speakers", (token, _recent_payload(first="Sarah")), None)
    assert state.panel.row("S1")["label"] == "Sarah"
    assert refreshes == [True] and renders == []
    # A failed rename mirrors nothing and only redraws the post-stop card.
    post_stop.begin_rename("S2")
    _op, _args, token = post_stop.submit_rename("Bo")
    controller._meeting_event("speakers", (token, None), Exception("boom"))
    assert state.panel.row("S2")["label"] == "Speaker 2"
    assert renders == [True] and refreshes == [True]


def test_recent_rename_after_the_drawer_closed_is_dropped():
    controller, refreshes, _renders = _recent_event_controller()
    state = controller.meeting_recent_speakers
    _op, _args, token = state.toggle(RECENT_NOTE)
    state.loaded(token, _recent_payload(), None)
    state.panel.begin_rename("S1")
    _op, _args, token = state.panel.submit_rename("Sarah")
    state.close()
    controller._meeting_event("recent_speakers", (token, _recent_payload(first="Sarah")), None)
    assert refreshes == [] and not state.panel.visible


def test_meeting_recent_speaker_save_sends_one_request():
    from localflow.hub_model import RecentSpeakers

    methods = _extract(("meetingRecentSpeakerSave_",))
    state = RecentSpeakers()
    _op, _args, token = state.toggle(RECENT_NOTE)
    state.loaded(token, _recent_payload(), None)
    state.panel.begin_rename("S2")
    sent, renders = [], []
    controller = _bind(
        SimpleNamespace(
            page="Meetings", meeting_recent_speakers=state,
            meeting_recent_field=_FakeView("  Bo  "),
            _meeting_recent_refresh=lambda: renders.append(state.panel.busy),
            _meeting_recent_send=lambda request, event: sent.append((request, event)),
        ),
        methods,
    )
    controller.meetingRecentSpeakerSave_(None)
    assert sent == [(
        ("rename_speaker", (RECENT_MID, "S2", "Bo"), state.panel.epoch), "recent_speakers",
    )]
    assert renders == ["rename"]
    controller.meetingRecentSpeakerSave_(None)
    assert len(sent) == 1


def test_meeting_recent_speakers_toggle_sends_the_lazy_load():
    from localflow.hub_model import RecentSpeakers

    methods = _extract(("meetingRecentSpeakers_",))
    state = RecentSpeakers()
    sent, renders = [], []
    controller = _bind(
        SimpleNamespace(
            page="Meetings", meeting_recent_speakers=state,
            meeting_recent=[{**RECENT_NOTE, "meeting_id": None}, RECENT_NOTE],
            _meeting_recent_refresh=lambda: renders.append(state.state),
            _meeting_recent_send=lambda request, event: sent.append((request, event)),
        ),
        methods,
    )
    sender = SimpleNamespace(tag=lambda: 1)
    controller.meetingRecentSpeakers_(sender)
    assert sent == [(("speakers", (RECENT_MID,), state.seq), "recent_speakers_load")]
    assert renders == ["loading"]
    controller.meetingRecentSpeakers_(sender)  # Done closes, no request
    assert len(sent) == 1 and renders == ["loading", ""]
    controller.meetingRecentSpeakers_(SimpleNamespace(tag=lambda: 0))  # no meeting_id
    controller.meetingRecentSpeakers_(SimpleNamespace(tag=lambda: 7))
    assert len(sent) == 1


def test_meeting_capture_state_keeps_the_recent_rename_draft():
    from localflow.hub_model import RecentSpeakers

    methods = _extract(("_meeting_capture_state",))
    state = RecentSpeakers()
    _op, _args, token = state.toggle(RECENT_NOTE)
    state.loaded(token, _recent_payload(), None)
    state.panel.begin_rename("S2")

    class _Combo:
        def indexOfSelectedItem(self):
            return -1

    controller = _bind(
        SimpleNamespace(
            meeting_notes=SimpleNamespace(string=lambda: "notes"),
            meeting_title=_FakeView("Title"), meeting_title_shows_session=False,
            meeting_template=_Combo(), meeting_templates=[],
            meeting_recent_speakers=state, meeting_recent_field=_FakeView("Hal"),
            meeting_recent_field_id="S2",
        ),
        methods,
    )
    controller._meeting_capture_state()
    assert state.panel.draft == "Hal"
    # A field drawn for another row never overwrites the draft.
    controller.meeting_recent_field_id = "S1"
    controller.meeting_recent_field = _FakeView("Stale")
    controller._meeting_capture_state()
    assert state.panel.draft == "Hal"


# ------------------------------------------------- people (recognition) ---

PEOPLE_SELECTORS = (
    "peopleToggle_", "peopleRetry_", "peopleRename_", "peopleSave_", "peopleCancel_",
    "peopleForget_", "peopleForgetAll_", "peopleExport_", "peopleEnroll_",
    "peopleRecord_", "peopleStop_", "meetingSpeakerRemember_",
    "meetingRecentSpeakerRemember_",
)
VP = "a" * 32


def _people(diarization=True, recognition=True, *people):
    from localflow.people_model import PeopleState

    state = PeopleState()
    request = state.set_flags(diarization, recognition)
    if request is not None:
        state.loaded(request[2], {"speakers": list(people), "embedder": "e"}, None)
    return state


def test_people_selectors_are_guarded_and_never_call_the_client():
    methods = {method.name: method for method in _controller_methods()}
    sources = _method_sources()
    for name in PEOPLE_SELECTORS:
        assert name in methods, name
        assert not _python_method(methods[name]), name
        assert sources[name].count('if self.page != "Meetings":') == 1, name
        assert "self.meeting_client" not in sources[name], name
    send = sources["_people_send"]
    assert "threading.Thread(" in send
    assert "self.meeting_events.put((event, (token, result), error))" in send


def test_remember_actions_name_existing_selectors():
    import re as _re

    source = SOURCE.read_text(encoding="utf-8")
    names = {method.name for method in _controller_methods()}
    for table, selector in (
        ("SPEAKER_ACTIONS", "meetingSpeakerRemember:"),
        ("RECENT_SPEAKER_ACTIONS", "meetingRecentSpeakerRemember:"),
    ):
        assert f'{table}["remember"] = "{selector}"' in source
        assert selector.replace(":", "_") in names
    for selector in _re.findall(r'"(people\w+:)"', source):
        assert selector.replace(":", "_") in names, selector


def test_page_meetings_builds_no_people_view_and_sends_after_render():
    sources = _method_sources()
    page = sources["_page_meetings"]
    for name in ("PeopleState(", "_people_build", "PEOPLE_HEADING", "CobaltSwitch"):
        assert name not in page, name
    assert "self._people_reset_views()" in page
    assert page.index("self._meeting_render()") < page.index("self._people_send(people_request)")
    builders = [name for name, text in sources.items() if "PeopleState(" in text]
    assert len(builders) == 1 and builders[0].startswith("initWith"), builders
    render = sources["_meeting_render"]
    assert render.index("self._meeting_render_speakers(saved)") < render.index(
        "self._people_render()"
    ) < render.index("self._meeting_layout(")
    layout = sources["_meeting_layout"]
    assert layout.index("self._people_layout(") < layout.index("self._fit_body_height()")


def test_people_layout_without_a_card_touches_nothing():
    methods = _extract(("_people_layout",))
    controller = _bind(SimpleNamespace(people_card=None), methods)
    assert controller._people_layout(400, 600) is None


def test_people_layout_places_the_block_from_the_pure_layout():
    from localflow.people_model import people_layout

    methods = _extract(
        ("_people_layout",), {"people_layout": people_layout, "NSMakeRect": lambda *a: a},
    )
    heading, card, message = _FakeView(), _FakeView(), _FakeView()
    state = _people(True, False)
    controller = _bind(
        SimpleNamespace(
            people=state, people_heading=heading, people_card=card, people_message=message,
            _people_message_height=lambda _width: 0,
        ),
        methods,
    )
    controller._people_layout(500, 600)
    expected = people_layout(600, 500, state)
    assert heading.frames == [expected["heading"]] and card.frames == [expected["card"]]
    assert message.frames == []


def test_people_render_with_both_flags_off_builds_and_sends_nothing():
    methods = _extract(("_people_render",))
    calls = []

    def fail():
        raise AssertionError("built a People view")

    controller = _bind(
        SimpleNamespace(
            people=_people(False, False), people_card=None,
            _people_teardown=lambda: calls.append("teardown"),
            _people_build=fail, _people_send=fail, _people_fill=fail,
        ),
        methods,
    )
    controller._people_render()
    assert calls == ["teardown"]


class _Switch:
    def __init__(self):
        self.states = []

    def setState_(self, value):
        self.states.append(value)


def _toggle_controller(state):
    methods = _extract(("peopleToggle_", "_people_consent_done"))
    calls = []
    controller = SimpleNamespace(
        page="Meetings", people=state,
        _people_consent=lambda: calls.append("sheet"),
        _people_write=lambda on: calls.append(("write", on)),
        _meeting_render=lambda: calls.append("render"),
    )
    return _bind(controller, methods), calls


def test_toggle_on_writes_only_after_consent():
    state = _people(True, False)
    controller, calls = _toggle_controller(state)
    switch = _Switch()
    controller.peopleToggle_(switch)
    assert calls == ["sheet"] and switch.states == [0]  # snapped back, nothing written
    controller.peopleToggle_(switch)  # the sheet is already open
    assert calls == ["sheet"]
    controller._people_consent_done(False)
    assert ("write", True) not in calls and not state.consent_open
    controller.peopleToggle_(switch)
    controller._people_consent_done(True)
    assert calls[-1] == ("write", True)
    assert [call for call in calls if isinstance(call, tuple)] == [("write", True)]


def test_toggle_off_writes_without_a_sheet():
    state = _people(True, True)
    controller, calls = _toggle_controller(state)
    switch = _Switch()
    controller.peopleToggle_(switch)
    assert calls == [("write", False)] and switch.states == [1]
    controller.page = "Home"
    controller.peopleToggle_(switch)
    assert calls == [("write", False)]


def test_people_write_saves_and_loads_after_consent(tmp_path, monkeypatch):
    from localflow.config import Config, ConfigError, apply_config_update, save_config
    from localflow.people_model import SAVE_FAILED, recognition_update

    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    save_config(Config(speaker_diarization=True))
    from localflow.config import load_config

    methods = _extract(("_people_write",), {
        "apply_config_update": apply_config_update, "load_config": load_config,
        "save_config": save_config, "recognition_update": recognition_update,
        "ConfigError": ConfigError, "SAVE_FAILED": SAVE_FAILED,
    })
    state = _people(True, False)
    sent, renders = [], []

    def sync():
        config = load_config()
        return state.set_flags(config.speaker_diarization, config.speaker_recognition)

    controller = _bind(
        SimpleNamespace(
            page="Meetings", people=state, config=None,
            _people_sync_flags=sync, _people_send=sent.append,
            _meeting_render=lambda: renders.append(True),
        ),
        methods,
    )
    state.request_recognition(True)
    assert state.consent_result(True)
    controller._people_write(True)
    assert load_config().speaker_recognition is True
    assert controller.config.speaker_recognition is True
    assert sent == [("enrolled", (), state.seq)] and state.grace == 3
    assert renders == [True]


def _people_event_controller(page="Meetings", state=None):
    methods = _extract(
        ("_meeting_event",), {"time": SimpleNamespace(monotonic=lambda: 50.0)},
    )
    sent, renders = [], []
    controller = SimpleNamespace(
        page=page, people=state or _people(True, True), people_retry_at=None,
        people_capture=object(),
        _people_send=sent.append, _meeting_render=lambda: renders.append(True),
    )
    return _bind(controller, methods), sent, renders


def test_people_load_event_renders_on_the_page_only_and_drops_stale_answers():
    state = _people(True, True)
    old = state.seq
    _op, _args, token = state.load()
    controller, _sent, renders = _people_event_controller(state=state)
    controller._meeting_event("people_load", (old, {"speakers": []}), None)
    assert renders == [] and state.phase == "loading"
    controller._meeting_event("people_load", (token, {"speakers": [], "embedder": "e"}), None)
    assert renders == [True] and state.phase == "ready"
    _op, _args, token = state.load()
    controller.page = "Home"
    controller._meeting_event("people_load", (token, {"speakers": []}), None)
    assert renders == [True] and state.phase == "ready"


def test_people_load_403_after_consent_schedules_a_retry():
    from localflow.meeting_client import MeetingClientError

    state = _people(True, False)
    state.request_recognition(True)
    state.consent_result(True)
    _op, _args, token = state.recognition_saved(True)
    controller, sent, renders = _people_event_controller(state=state)
    off = MeetingClientError("speaker recognition is off", 403)
    controller._meeting_event("people_load", (token, None), off)
    assert controller.people_retry_at == 51.0 and renders == [] and sent == []
    assert state.phase == "loading"


def test_people_action_success_sends_exactly_one_reload():
    state = _people(True, True, {"id": VP, "name": "Dana", "model_id": "e", "samples": 1,
                                 "created": "2026-11-12T00:00:00Z"})
    controller, sent, renders = _people_event_controller(state=state)
    _op, _args, token = state.submit_forget(VP)
    controller._meeting_event("people", (token - 1, {}), None)
    assert sent == [] and renders == []
    controller._meeting_event("people", (token, {"forgotten": 1}), None)
    assert sent == [("enrolled", (), state.seq)] and renders == [True]


def test_people_rec_event_failure_drops_the_capture():
    state = _people(True, True)
    state.begin_enroll("http://127.0.0.1:8765")
    _op, _args, token = state.start_recording("Me")
    controller, _sent, renders = _people_event_controller(state=state)
    controller._meeting_event("people_rec", (token, None), OSError("no mic"))
    assert controller.people_capture is None and state.rec == "" and renders == [True]


def test_remember_through_speakers_reloads_people_without_mirroring():
    from localflow.hub_model import RecentSpeakers, mirror_speakers

    methods = _extract(
        ("_meeting_event", "_meeting_speakers_mirror"), {"mirror_speakers": mirror_speakers},
    )
    panel = _post_stop_panel(RECENT_MID)
    recent = RecentSpeakers()
    _op, _args, token = recent.toggle(RECENT_NOTE)
    recent.loaded(token, _recent_payload(), None)
    recent.panel.remembered = panel.remembered
    sent, refreshes, renders = [], [], []
    controller = _bind(
        SimpleNamespace(
            page="Meetings", meeting_speakers=panel, meeting_recent_speakers=recent,
            meeting_recent_card=object(), meeting_last_saved=None, people=_people(True, True),
            _people_send=sent.append, _meeting_recent_refresh=lambda: refreshes.append(True),
            _meeting_render=lambda: renders.append(True),
        ),
        methods,
    )
    panel.set_remember(True)
    panel.begin_remember("S1", "")
    _op, args, token = panel.submit_remember("Sarah")
    assert args == ("Sarah", RECENT_MID, "S1")
    controller._meeting_event("speakers", (token, {"id": VP, "name": "Sarah"}), None)
    assert [row["label"] for row in recent.panel.rows] == ["Speaker 1", "Speaker 2"]
    assert [row["label"] for row in panel.rows] == ["Speaker 1", "Speaker 2"]
    assert recent.panel.is_remembered("S1")  # one shared set
    assert sent == [("enrolled", (), controller.people.seq)] and refreshes == [True]


def test_meeting_speaker_save_in_remember_mode_sends_one_enroll():
    methods = _extract(("meetingSpeakerSave_",))
    sent = []
    panel = _speaker_panel()
    panel.set_remember(True)
    panel.begin_remember("S2", "")
    controller = _bind(
        SimpleNamespace(
            page="Meetings", meeting_speakers=panel,
            meeting_speaker_field=_FakeView("  Bo "),
            _meeting_render=lambda: None, _meeting_speaker_send=sent.append,
        ),
        methods,
    )
    controller.meetingSpeakerSave_(None)
    controller.meetingSpeakerSave_(None)
    assert sent == [("enroll", ("Bo", SPEAKER_MID, "S2"), panel.epoch)]


def test_people_code_uses_the_consent_sheet_and_never_prints_or_reads_the_environment():
    sources = _method_sources()
    consent = sources["_people_consent"]
    assert "beginSheetModalForWindow_completionHandler_" in consent
    assert "CONSENT_POINTS" in consent and "CONSENT_DRAFT_NOTE" in consent
    assert 'setKeyEquivalent_("\\r")' in consent
    assert "save_config" not in sources["peopleToggle_"]
    assert "save_config" not in consent
    people_code = [
        text for name, text in sources.items()
        if name.startswith(("_people", "people")) or name in PEOPLE_SELECTORS
    ]
    assert people_code
    for text in people_code:
        assert "print(" not in text and "os.environ" not in text and "getenv" not in text
    model = (ROOT / "localflow" / "people_model.py").read_text(encoding="utf-8")
    assert "print(" not in model and "os.environ" not in model


def test_leaving_meetings_and_closing_stop_the_recording():
    sources = _method_sources()
    show = sources["show_page"]
    assert 'if self.page == "Meetings" and page != "Meetings":' in show
    assert "self._people_leave()" in show
    assert "self._people_leave()" in sources["windowWillClose_"]
    assert "capture.discard" in sources["_people_cancel_capture"]
    assert "self._people_tick()" in sources["meetingTimerTick_"]


def test_people_leave_discards_a_running_capture():
    methods = _extract(
        ("_people_leave", "_people_cancel_capture"),
        {"threading": SimpleNamespace(Thread=lambda target, **_kw: SimpleNamespace(
            start=target))},
    )
    discarded = []
    state = _people(True, True)
    state.begin_enroll("http://127.0.0.1:8765")
    _op, _args, token = state.start_recording("Me")
    state.recording_started(token, None, 0.0)
    controller = _bind(
        SimpleNamespace(
            people=state, people_capture=SimpleNamespace(discard=lambda: discarded.append(1)),
        ),
        methods,
    )
    controller._people_leave()
    assert discarded == [1] and controller.people_capture is None and state.rec == ""
    controller._people_leave()
    assert discarded == [1]
