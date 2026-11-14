import ast
from pathlib import Path

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
        timer_source.index('if self.page == "Meetings":')
    )


def test_hub_snapshot_workflow_covers_each_page_and_error_state():
    snapshots = (ROOT / "scripts" / "mac_ui_snapshots.py").read_text()
    workflow = (ROOT / ".github" / "workflows" / "macos-ui.yml").read_text()
    assert "from localflow.hub_mac import build_window" in snapshots
    assert "SnapshotMeetingClient" in snapshots
    assert '("idle", "running", "saved")' in snapshots
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
    assert '"Clear history", "clearHistory:", 610, 0' in source
    assert 'kind="destructive_ghost"' in source
    assert '"Clear history", "clearHistory:", 610, self.cursor_y' not in source


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
    assert "NSPopUpButton" not in source
    assert '"strict"' not in source
    assert '("formal", "casual", "very_casual", "excited")' in source
    assert "self.config = load_config()" in source
    assert "self.dictionary.update_word(" in source
    assert "self.dictionary.update_replacement(" in source
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
    assert "self._segment_group(y, 180, 540)" in style_page_source
    assert "self.style_groups[category] = group" in style_page_source
    assert "_segmented(" in style_page_source
    assert "segmented as _segmented" in setup_source
    assert (
        "_segmented(group, self.cleanup_level_buttons, selected, self.tokens)"
        in setup_source
    )
