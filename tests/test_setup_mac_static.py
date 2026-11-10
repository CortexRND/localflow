"""Check AppKit setup selectors without importing PyObjC on Linux."""

import ast
from pathlib import Path

SRC = Path(__file__).parent.parent / "localflow" / "setup_mac.py"
SNAPSHOTS = Path(__file__).parent.parent / "scripts" / "mac_ui_snapshots.py"


def _methods():
    tree = ast.parse(SRC.read_text(encoding="utf-8"))
    cls = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "SetupWindowController"
    )
    return [node for node in cls.body if isinstance(node, ast.FunctionDef)]


def _is_python_method(function):
    return any(
        isinstance(dec, ast.Attribute)
        and isinstance(dec.value, ast.Name)
        and dec.value.id == "objc"
        and dec.attr == "python_method"
        for dec in function.decorator_list
    )


def test_non_python_methods_are_valid_selectors():
    invalid = []
    for function in _methods():
        if _is_python_method(function):
            continue
        args = len(function.args.args) + len(function.args.kwonlyargs) - 1
        if function.name.startswith("_"):
            invalid.append(f"{function.name}: private method exported as a selector")
        elif function.name.count("_") != args:
            invalid.append(
                f"{function.name}: {function.name.count('_')} underscores but {args} args"
            )
    assert not invalid, "malformed selectors:\n" + "\n".join(invalid)


def test_setup_helpers_are_not_exported_as_selectors():
    names = {function.name for function in _methods() if _is_python_method(function)}
    assert {
        "_build_window", "_render", "_start_worker", "_handle_events",
        "_start_checks", "_start_download", "_start_pull", "_start_trial",
        "_advance", "_select_stt", "_select_llm", "_fix_check",
    } <= names


def test_mac_snapshot_script_captures_each_setup_step_and_provider_states():
    source = SNAPSHOTS.read_text(encoding="utf-8")
    assert "from localflow.setup_mac import SetupWindowController" in source
    assert "for step in range(7):" in source
    assert "setup-cleanup-level-medium" in source
    assert 'setup-non-ollama-provider' in source
    assert 'setup-download-progress' in source
    assert 'setup-download-error' in source
    assert "cleanupLevelAction_" in SRC.read_text(encoding="utf-8")


def test_setup_mac_requires_ollama_for_selected_cleanup_and_reads_login_choice():
    source = SRC.read_text(encoding="utf-8")

    assert "Ollama isn't running — start it, or choose No cleanup." in source
    assert "start_on_login=bool(self.start_on_login)" in source
    assert "if self.step == 2 and self.stt_id == model_id:" in source
    assert "if self.step == 3 and self.llm_id == model_id:" in source
    assert 'if "stt-download" in self.busy:' in source
    assert 'if "llm-pull" in self.busy:' in source
    assert 'model_id=model.id' in source
    assert (
        'self._start_worker("llm-pull", pull, model_id=model_id, job_token=token)'
        in source
    )
    assert 'self.back_button.setEnabled_(not model_busy)' in source
    assert 'if self.busy & {"stt-download", "llm-pull"}:' in source
    assert '("progress", "stt-download", token, done, total)' in source
    assert 'job_token=token' in source
    assert 'self.job_token = None' in source
    assert 'friendly_download_error(error) if model_job' in source
    assert 'elif self._progress_stalled:' in source
    assert 'self.busy.difference_update({"stt-download", "llm-pull"})' in source


def test_setup_mac_places_refresh_models_and_shortcuts_without_overlap():
    source = SRC.read_text(encoding="utf-8")

    assert 'self._button("Refresh models", 250, 198' in source
    assert "Hold {self.hotkey} · Double-tap locks · Esc cancels" in source
    assert "Pause music while I talk" in source
    assert "self.pauseMediaChanged_" in source


def test_setup_mac_shows_errors_on_non_download_steps():
    source = SRC.read_text(encoding="utf-8")
    assert "elif self.error_message and self.step != 5:" in source
    assert 'self.status.setTextColor_(_color("red", self.tokens))' in source


def test_setup_mac_cleanup_levels_are_light_medium_and_hidden_for_none():
    source = SRC.read_text(encoding="utf-8")
    assert 'if self.llm_id == "none":' in source
    assert 'levels = (("light", "Light"), ("medium", "Medium"))' in source
    assert 'levels = ("light", "medium")' in source


def test_setup_mac_keeps_one_description_and_uses_rounded_model_controls():
    source = SRC.read_text(encoding="utf-8")
    tree = ast.parse(source)
    controller = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "SetupWindowController"
    )
    renderer = next(
        node for node in controller.body
        if isinstance(node, ast.FunctionDef) and node.name == "_render"
    )
    render_source = ast.get_source_segment(source, renderer)
    for duplicate in (
        "Choose the speech model that turns your voice into text.",
        "Cleanup fixes punctuation and filler words; raw transcript skips the extra model.",
        "Pick a microphone and the key you'll hold to talk.",
        "Record five seconds of speech.",
        "Try a short phrase near your microphone.",
    ):
        assert duplicate not in render_source
    assert "NSMakeRect(17, 28, 16, 16)" in source
    assert "NSMakeRect(4, 4, 8, 8)" in source
    assert "NSMakeRect(meter_x + dot_index * 11, 48, 7, 7)" in source
    assert "setCornerRadius_(3.5)" in source
    assert "NSMakeRect(548, 248, 180, 160)" in render_source


def test_setup_header_labels_refresh_from_current_appearance_tokens():
    source = SRC.read_text(encoding="utf-8")
    tree = ast.parse(source)
    controller = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "SetupWindowController"
    )
    methods = {
        node.name: node for node in controller.body
        if isinstance(node, ast.FunctionDef)
    }
    refresh = ast.get_source_segment(source, methods["_refresh_header_colors"])
    render = ast.get_source_segment(source, methods["_render"])
    assert "tokens_for_window(self.window)" in render
    assert "_refresh_header_colors()" in render
    for label in ("brand", "step_label", "title", "subtitle", "status"):
        assert f"self.{label}" in refresh
    assert "_color(token, self.tokens)" in refresh


def test_setup_model_cards_use_scroll_content_width_and_measured_badges():
    source = SRC.read_text(encoding="utf-8")
    assert "content_width = scroll.contentSize().width" in source
    assert "NSMakeRect(0, 0, content_width, height)" in source
    assert (
        "card_width = parent.bounds().size.width - 8 "
        "if parent is not None else 710"
    ) in source
    assert "pill_label.sizeToFit()" in source
    assert "self._label(meter_name, meter_x, 43, 60, 11, height=18)" in source


def test_setup_cleanup_uses_shared_segmented_control_and_exact_width():
    source = SRC.read_text(encoding="utf-8")
    assert "segmented as _segmented" in source
    assert "group_width = len(levels) * 102" in source
    assert "_segmented(group, self.cleanup_level_buttons, selected, self.tokens)" in source
