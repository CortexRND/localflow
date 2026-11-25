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
