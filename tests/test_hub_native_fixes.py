"""Native Hub fixes: Meetings form, recent notes, Style autosave, Models label,
Prompts, Hot Phrases switch/preview, Home decoration and sidebar label.

AppKit cannot be imported off macOS, so (as in test_hub_model.py) the pure
layout/copy helpers are tested directly and controller handlers are extracted
from hub_mac.py's AST and run against fakes.
"""

import ast
import os
import sys
from dataclasses import replace
from datetime import datetime
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from localflow import hot_phrases_editor as hpe
from localflow import hub_model
from localflow import model_storage as ms
from localflow.config import Config, ConfigError, apply_config_update
from localflow.diarization.models import DIAR_MODELS
from localflow.ui_tokens import (
    HUB_CONTENT_X,
    HUB_HEADER_OFFSET,
    HUB_MIN_HEIGHT,
    HUB_MIN_WIDTH,
    INPUT_HEIGHT,
    halftone_dots,
    input_inner_frame,
)

ROOT = Path(__file__).parent.parent
HUB_MAC = ROOT / "localflow" / "hub_mac.py"
MAC_STYLE = ROOT / "localflow" / "mac_style.py"
BODY_MIN = HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * 16
BODY_WIDE = 1280 - HUB_CONTENT_X - 2 * 16
WIDTHS = (BODY_MIN, BODY_WIDE)


def _sources(path=HUB_MAC, cls_name="HubWindowController"):
    source = path.read_text(encoding="utf-8")
    cls = next(
        node for node in ast.parse(source).body
        if isinstance(node, ast.ClassDef) and node.name == cls_name
    )
    return {
        node.name: ast.get_source_segment(source, node)
        for node in cls.body if isinstance(node, ast.FunctionDef)
    }


def _extract(names, namespace):
    tree = ast.parse(HUB_MAC.read_text(encoding="utf-8"))
    cls = next(
        node for node in tree.body
        if isinstance(node, ast.ClassDef) and node.name == "HubWindowController"
    )
    functions = []
    for node in cls.body:
        if isinstance(node, ast.FunctionDef) and node.name in names:
            node.decorator_list = []
            functions.append(node)
    assert {f.name for f in functions} == set(names)
    module = ast.fix_missing_locations(ast.Module(body=functions, type_ignores=[]))
    exec(compile(module, "hub_mac_extract", "exec"), namespace)  # noqa: S102
    return {name: namespace[name] for name in names}


def _overlaps(a, b):
    ax, ay, aw, ah = a
    bx, by, bw, bh = b
    return ax < bx + bw and bx < ax + aw and ay < by + bh and by < ay + ah


# ------------------------------------------------------------- meetings form ---

@pytest.mark.parametrize("width", WIDTHS)
@pytest.mark.parametrize("error_height", [0, 34])
def test_meeting_form_layout_has_no_overlaps_and_aligned_inputs(width, error_height):
    top = HUB_HEADER_OFFSET
    notes = hub_model.meeting_notes_height(HUB_MIN_HEIGHT - 24, top)
    frames = hub_model.meeting_form_layout(width, top, notes, error_height=error_height)
    title, template = frames["title_field"], frames["template_field"]
    # Same row, same height, same top edge: they look the same height.
    assert title[1] == template[1] and title[3] == template[3] == INPUT_HEIGHT
    assert frames["title_label"][1] == frames["template_label"][1]
    assert title[0] + title[2] < template[0] and template[0] + template[2] == width
    boxes = {k: v for k, v in frames.items() if isinstance(v, tuple) and k != "retry"}
    boxes["action"] = (0, frames["action_y"], 250, 36)
    keys = sorted(boxes)
    for i, a in enumerate(keys):
        for b in keys[i + 1:]:
            assert not _overlaps(boxes[a], boxes[b]), (a, b)
        assert boxes[a][0] >= 0 and boxes[a][0] + boxes[a][2] <= width, a
    # The subtitle sits under the page title, inside the header offset band.
    assert frames["subtitle"][1] == top
    notes_bottom = frames["notes"][1] + frames["notes"][3]
    if error_height:
        assert frames["error"][1] == notes_bottom + 12
        assert frames["action_y"] == frames["error"][1] + error_height + 12
    else:
        # No reserved blank band: Start sits a compact 12pt under the card.
        assert "error" not in frames
        assert frames["action_y"] == notes_bottom + 12
    # The autosave status is on the heading row, not under the card.
    assert frames["notes_status"][1] < frames["notes"][1]


@pytest.mark.parametrize("intrinsic", [17, 19, 22, 26, 30])
def test_inputs_share_one_box_and_are_centred(intrinsic):
    # A text field (~19-22pt cell) and a combo box (~22-26pt cell) in boxes of
    # the same INPUT_HEIGHT: both inner controls are vertically centred, so
    # the visible boxes are identical whatever the intrinsic height.
    x, y, w, h = input_inner_frame(300, INPUT_HEIGHT, intrinsic)
    assert h <= INPUT_HEIGHT - 2 and y >= 1 and y + h <= INPUT_HEIGHT - 1
    assert abs((INPUT_HEIGHT - (y + h)) - y) <= 1
    assert x > 0 and x + w <= 300


def test_meeting_page_uses_the_shared_input_box_for_both_controls():
    page = _sources()["_page_meetings"]
    assert page.count("input_box(") == 2
    assert "self.meeting_title, frames[\"title_field\"]" in page
    assert "self.meeting_template, frames[\"template_field\"]" in page
    # Neither control keeps its own native bezel: mac_style switches them off.
    style = MAC_STYLE.read_text(encoding="utf-8")
    for call in ("control.setBezeled_(False)", "control.setBordered_(False)",
                 "control.setControlSize_(AppKit.NSControlSizeRegular)",
                 "control.setFont_(font(13))", "input_inner_frame("):
        assert call in style
    # Notes card + inputs use the chunky ink outline helpers.
    assert "width=CARD_BORDER_WIDTH" in page and 'border="ink"' in page
    assert "MEETING_SUBTITLE" in page


class _Title:
    def __init__(self, value):
        self.value, self.editable, self.selectable = value, True, True

    def stringValue(self):
        return self.value

    def setStringValue_(self, value):
        self.value = value

    def setEditable_(self, value):
        self.editable = value

    def setSelectable_(self, value):
        self.selectable = value

    def setTextColor_(self, _color):
        pass

    def isEditable(self):
        return self.editable


class _Notes:
    def __init__(self, text=""):
        self.text = text

    def string(self):
        return self.text


class _Combo:
    def indexOfSelectedItem(self):
        return -1


def _title_controller():
    styled = []
    namespace = {
        "_color": lambda *_args: None,
        "style_input_box": lambda box, _tokens, readonly=False: styled.append(readonly),
    }
    methods = _extract(("_meeting_show_title", "_meeting_capture_state"), namespace)
    controller = SimpleNamespace(
        meeting_title=_Title("Next sync"), meeting_title_box=object(),
        meeting_template_box=object(), meeting_title_shows_session=False,
        meeting_title_value="Next sync", meeting_notes=_Notes(), tokens={},
        meeting_template=_Combo(), meeting_templates=[], meeting_notes_text="",
    )
    for name, function in methods.items():
        setattr(controller, name, function.__get__(controller))
    return controller, styled


def test_running_meeting_keeps_the_title_visible_read_only_and_restores_the_draft():
    controller, styled = _title_controller()
    controller.meeting_title.setStringValue_("Draft for later")
    controller._meeting_show_title({"title": "Planning"})
    assert controller.meeting_title.value == "Planning"
    assert controller.meeting_title.editable is False
    assert controller.meeting_title.selectable is True  # readable / copyable
    assert styled[-2:] == [True, True]  # quiet read-only boxes
    # A rebuild / navigation while recording must not capture the session
    # title as the draft.
    controller._meeting_capture_state()
    assert controller.meeting_title_value == "Draft for later"
    controller._meeting_show_title({"title": "Planning"})  # every poll
    assert controller.meeting_title_value == "Draft for later"
    # Session over: the user's draft comes back, editable.
    controller._meeting_show_title(None)
    assert controller.meeting_title.value == "Draft for later"
    assert controller.meeting_title.editable is True and styled[-1] is False
    controller._meeting_capture_state()
    assert controller.meeting_title_value == "Draft for later"


def test_meeting_render_no_longer_hides_the_title():
    render = _sources()["_meeting_render"]
    assert "meeting_title.setHidden_(True)" not in render
    assert "meeting_title_label.setHidden_(True)" not in render
    assert "self._meeting_show_title(session)" in render


@pytest.mark.parametrize(("session", "expected"), [
    ({"system_audio": "on"}, "Recording your mic and system audio."),
    ({"system_audio": "off"}, "Recording your mic only."),
    ({}, "Recording your mic only."),
    ({"system_audio": "error", "system_audio_error":
      "ScreenCaptureKit: no display available — grant Screen Recording "
      "permission to this Python binary (None)"},
     "Recording your mic only. Allow system audio to also capture others."),
    ({"system_audio": "error", "system_audio_error":
      "System audio permission unavailable"},
     "Recording your mic only. Allow system audio to also capture others."),
    ({"system_audio": "error", "system_audio_error":
      "no system audio source found: set [meetings] system_audio_device"},
     hub_model.SYSTEM_AUDIO_UNAVAILABLE),
    ({"system_audio": "error", "system_audio_error":
      "ScreenCaptureKit: start timed out"}, hub_model.SYSTEM_AUDIO_UNAVAILABLE),
    ({"system_audio": "error", "system_audio_error": "no input device matching 'X'"},
     hub_model.SYSTEM_AUDIO_UNAVAILABLE),
])
def test_capture_status_is_one_plain_honest_sentence(session, expected):
    message = hub_model.meeting_capture_status(session)
    assert message == expected
    # Deduplicated: "mic" once, never the raw error text, never the title.
    assert message.lower().count("mic") == 1
    assert "ScreenCaptureKit" not in message and "—" not in message
    if "permission" not in (session.get("system_audio_error") or "").lower():
        assert "Allow" not in message


def test_running_status_uses_the_single_sentence():
    render = _sources()["_meeting_render"]
    assert "meeting_capture_status(session)" in render
    assert 'session["system_audio_error"]' not in render
    assert "mic only — system audio unavailable" not in HUB_MAC.read_text(encoding="utf-8")


@pytest.mark.parametrize(("saved", "expected"), [
    ({"notes_path": "/v/notes/meetings/Zoom/2026-11-15 1430 Planning.md"},
     "Notes saved: Planning"),
    ({"notes_path": "/v/notes/meetings/Zoom/2026-11-15 1430 Planning.md",
      "title": "Q4 Planning", "enhanced": True}, "Notes saved: Q4 Planning"),
    ({"notes_path": "/v/notes/Retro.md"}, "Notes saved: Retro"),
    ({"notes_path": ""}, "Notes saved: Meeting"),
])
def test_saved_status_names_the_note_not_its_path(saved, expected):
    message = hub_model.saved_notes_message(saved)
    assert message == expected
    assert "/" not in message and ".md" not in message


def test_saved_render_and_stop_use_the_title_and_keep_the_vault_check():
    sources = _sources()
    render = sources["_meeting_render"]
    assert "saved_notes_message(saved)" in render
    assert 'saved.get("notes_path", "")' not in render
    event = sources["_meeting_event"]
    assert 'result.setdefault("title", previous["title"])' in event
    for name in ("meetingOpenNotes_", "meetingOpenRecent_"):
        body = sources[name]
        assert "vault_notes_path(" in body and "load_config().vault_path" in body


def test_notes_placeholder_is_drawn_never_inserted():
    source = HUB_MAC.read_text(encoding="utf-8")
    page = _sources()["_page_meetings"]
    # The placeholder is only ever handed to the drawing API ...
    assert source.count("MEETING_NOTES_PLACEHOLDER") == 2  # import + one use
    assert "setPlaceholder_color_(\n            MEETING_NOTES_PLACEHOLDER" in page
    assert "PlaceholderTextView.alloc()" in page
    # ... and autosave / Start / stop flush read only the real text.
    sources = _sources()
    assert "self.meeting_notes.string()" in sources["_meeting_current_notes_text"]
    assert "self.meeting_notes.string()" in sources["meetingAction_"]
    assert "MEETING_NOTES_PLACEHOLDER" not in sources["_meeting_advance_notes_save"]
    # The view draws it in drawRect_ and never puts it into its storage.
    view = _sources(MAC_STYLE, "PlaceholderTextView")
    assert "drawInRect_" in view["drawRect_"]
    assert "if self.string() or not self.placeholder" in view["drawRect_"]
    for body in view.values():
        assert "insertText" not in body
        assert "setString_(self.placeholder" not in body
        assert "textStorage" not in body


def test_notes_status_and_errors_take_room_only_when_shown():
    sources = _sources()
    page, layout = sources["_page_meetings"], sources["_meeting_layout"]
    assert "notes_bottom + 54" not in page and "notes_bottom + 26" not in page
    assert 'frames["notes_status"]' in page
    assert "error_height=self._meeting_error_height(width)" in layout
    assert 'self.meeting_error_label.setHidden_("error" not in frames)' in layout
    # Autosave is still serialized and flushed before stop.
    assert "self.meeting_stop_flush_pending = True" in sources["_meeting_request_notes_save"]
    assert 'self._meeting_worker("stop")' in sources["_meeting_notes_save_completed"]


# -------------------------------------------------------------- recent notes ---

def _note(root, category, stem, heading=None, body="Body.\n", mtime=None):
    folder = root / category
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / f"{stem}.md"
    text = "---\ndate: x\ntags: [meeting, localflow]\n---\n\n"
    if heading is not None:
        text += f"# {heading}\n\n"
    path.write_text(text + body, encoding="utf-8")
    if mtime is not None:
        os.utime(path, (mtime, mtime))
    return path


@pytest.fixture
def vault(tmp_path):
    vault = tmp_path / "vault"
    notes = vault / "notes" / "meetings"
    for day, title in enumerate(
        ("Design review", "Hiring sync", "Customer call", "Standup", "Retro", "Planning"),
        start=10,
    ):
        _note(notes, "Zoom", f"2026-11-{day} 1{day - 10}00 {title}", heading=title)
    # Transcripts and files outside notes_folder are never listed.
    _note(notes, "Zoom", "2026-11-20 0900 Planning (transcript)", heading="T")
    _note(vault / "notes" / "transcripts", "Zoom", "2026-11-21 0900 X (transcript)")
    (vault / "Inbox.md").write_text("# Not a meeting\n", encoding="utf-8")
    return vault


def test_recent_notes_lists_the_newest_titles(vault):
    now = datetime(2026, 11, 15, 18, 0)
    notes, error = hub_model.recent_notes(vault, "notes/meetings", now=now)
    assert error == ""
    assert [n["title"] for n in notes] == [
        "Planning", "Retro", "Standup", "Customer call", "Hiring sync",
    ]
    assert notes[0]["date_label"] == "Today, 3:00 PM"
    assert notes[1]["date_label"] == "Nov 14, 2:00 PM"
    assert all(n["path"].endswith(".md") and "(transcript)" not in n["path"] for n in notes)
    assert len(hub_model.recent_notes(vault, "notes/meetings", limit=6)[0]) == 6


def test_recent_notes_prefers_the_saved_heading_and_strips_the_date(tmp_path):
    root = tmp_path / "v" / "n"
    _note(root, "Meet", "2026-11-01 0900 Weekly sync", heading="Weekly sync: Q4 goals")
    _note(root, "Meet", "2026-11-02 0900 No heading", heading=None, body="Text\n")
    _note(root, "Other", "Loose note", heading=None, body="", mtime=1_000_000)
    notes, _ = hub_model.recent_notes(tmp_path / "v", "n")
    titles = [n["title"] for n in notes]
    assert titles[:2] == ["No heading", "Weekly sync: Q4 goals"]
    assert "Loose note" in titles
    assert not any(t.startswith("2026-") for t in titles)


def test_recent_notes_is_friendly_when_missing_or_empty(tmp_path):
    assert hub_model.recent_notes(tmp_path / "nope", "notes/meetings") == ([], "")
    (tmp_path / "v" / "notes" / "meetings").mkdir(parents=True)
    assert hub_model.recent_notes(tmp_path / "v", "notes/meetings") == ([], "")
    assert hub_model.recent_notes("", "notes/meetings") == ([], "")
    assert hub_model.recent_notes(tmp_path / "v", "") == ([], "")


@pytest.mark.skipif(
    sys.platform == "win32" or (hasattr(os, "geteuid") and os.geteuid() == 0),
    reason="permission bits are not enforced",
)
def test_recent_notes_reports_an_unreadable_folder_without_raising(tmp_path):
    root = tmp_path / "v" / "notes" / "meetings"
    _note(root, "Zoom", "2026-11-01 0900 A", heading="A")
    root.chmod(0)
    try:
        assert hub_model.recent_notes(tmp_path / "v", "notes/meetings") == (
            [], hub_model.RECENT_NOTES_UNREADABLE,
        )
    finally:
        root.chmod(0o755)


def test_recent_notes_skips_unreadable_files_and_symlinks_outside(tmp_path):
    root = tmp_path / "v" / "notes" / "meetings"
    good = _note(root, "Zoom", "2026-11-01 0900 Good", heading="Good")
    outside = tmp_path / "elsewhere"
    _note(outside, "Secret", "2026-12-01 0900 Secret", heading="Secret")
    (root / "linked-dir").symlink_to(outside / "Secret", target_is_directory=True)
    (root / "Zoom" / "2026-12-02 0900 Link.md").symlink_to(
        outside / "Secret" / "2026-12-01 0900 Secret.md"
    )
    notes, error = hub_model.recent_notes(tmp_path / "v", "notes/meetings")
    assert error == ""
    assert [n["path"] for n in notes] == [str(good)]


def test_recent_notes_never_read_a_folder_outside_the_vault(tmp_path):
    vault = tmp_path / "vault"
    vault.mkdir()
    outside = tmp_path / "outside"
    _note(outside, "Zoom", "2026-11-01 0900 Secret", heading="Secret")
    outside_error = ([], hub_model.RECENT_NOTES_OUTSIDE)
    # Absolute notes_folder and a ../ escape.
    assert hub_model.recent_notes(vault, str(outside)) == outside_error
    assert hub_model.recent_notes(vault, "../outside") == outside_error
    # The notes folder itself is a symlink pointing out of the vault.
    (vault / "notes").symlink_to(outside, target_is_directory=True)
    assert hub_model.recent_notes(vault, "notes") == outside_error


def test_recent_notes_turn_filesystem_errors_into_a_friendly_message(tmp_path, monkeypatch):
    root = tmp_path / "v" / "notes"
    _note(root, "Zoom", "2026-11-01 0900 A", heading="A")

    def broken_walk(*_args, **_kwargs):
        raise OSError("I/O error")

    monkeypatch.setattr(hub_model.os, "walk", broken_walk)
    assert hub_model.recent_notes(tmp_path / "v", "notes") == (
        [], hub_model.RECENT_NOTES_UNREADABLE,
    )

    def broken_resolve(self, strict=False):
        raise RuntimeError("symlink loop")

    monkeypatch.setattr(hub_model.Path, "resolve", broken_resolve)
    assert hub_model.recent_notes(tmp_path / "v", "notes") == (
        [], hub_model.RECENT_NOTES_UNREADABLE,
    )


def test_recent_notes_are_loaded_on_show_and_after_stop_only():
    sources = _sources()
    show = sources["show_page"]
    assert 'entering_meetings = page == "Meetings" and self.page != "Meetings"' in show
    assert "self._meeting_load_recent()" in show
    assert "recent_notes(" in sources["_meeting_load_recent"]
    assert "self._meeting_load_recent()" in sources["_meeting_event"]
    for name in ("_meeting_poll", "meetingTimerTick_", "_meeting_render", "_page_meetings"):
        assert "_meeting_load_recent" not in sources[name], name
        assert "recent_notes(" not in sources[name], name
    # The listing reads the injected config's vault (snapshots/tests isolate it).
    load = sources["_meeting_load_recent"]
    assert 'getattr(self.config, "vault_path", "")' in load
    assert "load_config" not in load


def test_recent_notes_reuse_the_writer_storage_layout(tmp_path):
    meetings = pytest.importorskip("localflow.meetings")
    writer = meetings.ObsidianWriter(str(tmp_path), "notes/meetings", "notes/transcripts")
    saved = writer.write(
        title="Planning", category="Zoom", started_at=datetime(2026, 11, 15, 14, 30),
        duration_seconds=60, notes_md="## Summary\n- ok", segments=[],
    )
    notes, error = hub_model.recent_notes(tmp_path, "notes/meetings")
    assert error == "" and len(notes) == 1
    assert notes[0]["title"] == "Planning"
    assert Path(notes[0]["path"]).resolve() == saved.notes_path.resolve()


# --------------------------------------------------------------------- style ---

class _Group:
    pass


def _style_controller(config, save):
    calls = SimpleNamespace(segments=[], rebuilds=0, shows=0)
    namespace = {
        "apply_config_update": apply_config_update,
        "save_config": save,
        "style_update": hub_model.style_update,
        "ConfigError": ConfigError,
        "STYLE_SAVE_FAILED": hub_model.STYLE_SAVE_FAILED,
    }
    methods = _extract(
        ("selectCleanupLevel_", "selectStyle_", "_style_autosave",
         "_preserve_page_state", "_restore_page_state"),
        namespace,
    )

    class Controller:
        pass

    for name, function in methods.items():
        setattr(Controller, name, function)
    controller = Controller()
    controller.page = "Style"
    controller.config = config
    controller.page_errors = {}

    def show_page(page):
        # Like _page_style: selections come from the committed config.
        calls.shows += 1
        controller.page = page
        controller.cleanup_options = ("none", "light", "medium")
        controller.style_options = ("formal", "casual", "very_casual", "excited")
        controller.cleanup_selection = controller.config.cleanup_level
        controller.style_selections = {
            c: getattr(controller.config, f"style_{c}")
            for c in ("work", "email", "personal", "other")
        }
        controller.cleanup_group, controller.cleanup_buttons = _Group(), []
        controller.style_groups = {c: _Group() for c in controller.style_selections}
        controller.style_buttons = {c: [] for c in controller.style_selections}
        controller.style_tag_map = {
            i * 4 + j: (c, v)
            for i, c in enumerate(("work", "email", "personal", "other"))
            for j, v in enumerate(controller.style_options)
        }

    def update_segments(group, _buttons, selected):
        calls.segments.append((group, selected))

    def rebuild():
        calls.rebuilds += 1
        state = controller._preserve_page_state()
        show_page(controller.page)
        controller._restore_page_state(state)

    controller.show_page = show_page
    controller._update_segment_buttons = update_segments
    controller._rebuild_current_page = rebuild
    show_page("Style")
    calls.shows = 0
    return controller, calls


def _sender(tag):
    return SimpleNamespace(tag=lambda: tag)


def test_style_click_saves_immediately_without_a_rebuild():
    save = Mock()
    controller, calls = _style_controller(Config(), save)
    assert controller.selectCleanupLevel_(_sender(2)) is None
    save.assert_called_once()
    saved = save.call_args.args[0]
    assert saved.cleanup_level == "medium"
    assert controller.config is saved  # committed only after the save
    controller.selectStyle_(_sender(1))  # work -> casual
    assert save.call_count == 2
    assert controller.config.style_work == "casual"
    assert controller.config.cleanup_level == "medium"
    assert calls.rebuilds == 0 and calls.shows == 0  # in-place segment update
    assert calls.segments[-1] == (controller.style_groups["work"], 1)
    assert "Style" not in controller.page_errors


def test_style_save_failure_keeps_committed_config_and_pending_choice():
    original = Config()
    save = Mock(side_effect=OSError("read-only /private/path"))
    controller, calls = _style_controller(original, save)
    controller.selectStyle_(_sender(4 + 3))  # email -> excited
    assert controller.config is original  # committed config untouched
    assert controller.config.style_email == Config().style_email
    assert controller.page_errors["Style"] == hub_model.STYLE_SAVE_FAILED
    assert "/private/path" not in controller.page_errors["Style"]
    # The page was rebuilt to show the error, and the clicked (pending)
    # selection is still the one on screen.
    assert calls.shows == 1
    assert controller.style_selections["email"] == "excited"
    assert (controller.style_groups["email"], 3) in calls.segments
    # The next successful click saves the pending choice too and clears the error.
    save.side_effect = None
    controller.selectCleanupLevel_(_sender(0))
    assert controller.config.style_email == "excited"
    assert controller.config.cleanup_level == "none"
    assert "Style" not in controller.page_errors
    assert calls.rebuilds == 1  # only to drop the error callout


def test_style_validation_error_is_shown_inline():
    save = Mock()
    controller, _calls = _style_controller(Config(), save)
    controller.style_tag_map[99] = ("work", "shouting")
    controller.style_options = (*controller.style_options, "shouting")
    controller.selectStyle_(_sender(99))
    # Config validation still runs first; nothing is written or committed.
    save.assert_not_called()
    assert controller.config.style_work == Config().style_work
    assert "invalid value for config key 'cleanup.style_work'" in (
        controller.page_errors["Style"]
    )
    assert controller.style_selections["work"] == "shouting"  # pending kept


def test_style_page_has_no_save_button_and_wraps_examples():
    source = HUB_MAC.read_text(encoding="utf-8")
    assert "Save style" not in source and "saveStyle" not in source
    page = _sources()["_page_style"]
    assert "style_row_frames(y, group_width)" in page and "wrap=True" in page
    assert "STYLE_AUTOSAVE_HINT" in page
    assert "self._style_autosave()" in _sources()["selectCleanupLevel_"]
    assert "self._style_autosave()" in _sources()["selectStyle_"]


@pytest.mark.parametrize("width", WIDTHS)
def test_style_rows_never_overlap(width):
    group_width = min(560, width - 180)
    previous_bottom = 0
    for index in range(4):
        y = 58 + index * hub_model.STYLE_ROW_H
        frames = hub_model.style_row_frames(y, group_width)
        assert not _overlaps(frames["label"], frames["example"])
        assert not _overlaps(frames["example"], frames["group"])
        assert frames["example"][0] + frames["example"][2] < frames["group"][0]
        assert frames["group"][0] + frames["group"][2] <= width
        bottom = max(f[1] + f[3] for f in frames.values())
        assert bottom <= y + hub_model.STYLE_ROW_H and y >= previous_bottom
        previous_bottom = bottom
        # Two 11pt lines fit the example frame.
        assert frames["example"][3] >= 2 * 15


# --------------------------------------------------------------------- home ---

@pytest.mark.parametrize("width", WIDTHS)
def test_home_decoration_stays_in_the_header_band(width):
    x, y, w, h = hub_model.home_decoration_frame(width)
    assert y >= 0 and y + h <= HUB_HEADER_OFFSET  # stat cards start at the offset
    assert x >= 0 and x + w <= width
    columns, card_width = hub_model.stat_layout(width)
    for column in range(columns):
        card = (column * (card_width + 12), HUB_HEADER_OFFSET, card_width, 104)
        assert not _overlaps((x, y, w, h), card)
    dots = halftone_dots(w, h, step=9, max_radius=3, pad=4, anchor="top-right")
    assert dots
    for dx, dy, diameter in dots:
        assert 0 <= dx and dx + diameter <= w and 0 <= dy and dy + diameter <= h


def test_halftone_never_draws_outside_any_bounds():
    for width, height in ((180, 160), (150, 116), (150, 40), (20, 20), (0, 0)):
        for anchor in ("bottom-left", "top-right"):
            for x, y, d in halftone_dots(width, height, anchor=anchor):
                assert 0 <= x and x + d <= width and 0 <= y and y + d <= height
    view = _sources(MAC_STYLE, "HalftoneView")["drawRect_"]
    assert "clipRect_" in view and "halftone_dots(" in view
    assert "saveGraphicsState" in view and "restoreGraphicsState" in view
    home = _sources()["_page_home"]
    assert "home_decoration_frame(width)" in home and "116" not in home.split("cards =")[0]


# ------------------------------------------------------------------ sidebar ---

def _sidebar_measure(text):
    # Generous 13pt semibold estimate (SF Pro averages well under 8pt/char).
    return len(text) * 8


def test_models_sidebar_label_is_display_only_and_fits():
    from localflow import hub_route

    assert hub_model.sidebar_label("Models") == "Models & keys"
    assert "Models" in hub_route.HUB_PAGES  # routing id unchanged
    assert hub_route.HUB_PAGES[-2:] == ("Models", "Settings")
    title = hub_model.sidebar_title("Models", _sidebar_measure)
    assert title.strip() == "Models & keys"
    assert _sidebar_measure(title) <= (
        hub_model.SIDEBAR_BUTTON_WIDTH - hub_model.SIDEBAR_TITLE_INSET
    )
    for page in hub_route.HUB_PAGES:
        assert _sidebar_measure(hub_model.sidebar_title(page, _sidebar_measure)) <= 152
    # A label that would not fit falls back to the page id, never truncates.
    assert hub_model.sidebar_title("Models", lambda _t: 999).strip() == "Models"


def test_sidebar_buttons_route_by_tag_and_show_the_label():
    sources = _sources()
    build = sources["_build_window"]
    assert "sidebar_title(page, lambda text: measure_text(text, sidebar_font))" in build
    assert "button.setTag_(index)" in build
    assert "page = PAGES[sender.tag()]" in sources["showPage_"]
    assert "page_title(\n                sidebar_label(page)" in sources["show_page"]


# ---------------------------------------------------------------- hot phrases ---

def test_switch_label_follows_the_state_and_fits():
    assert hpe.enabled_label(True) == "On" and hpe.enabled_label(False) == "Off"
    assert hpe.enabled_label(0) == "Off"
    for text in hpe.ENABLED_LABELS.values():
        assert len(text) * 9 <= hpe.ENABLED_LABEL_W
    assert hpe.ENABLED_ACCESSIBILITY == "Phrase enabled"


def test_switch_label_is_set_on_load_and_on_toggle():
    sources = _sources()
    assert "self._phrases_render_enabled()" in sources["_phrases_push_fields"]
    assert "self._phrases_render_enabled()" in sources["_phrases_render_status"]
    assert "enabled_label(self.phrases.enabled)" in sources["_phrases_render_enabled"]
    page = sources["_page_hot_phrases"]
    assert "setAccessibilityLabel_(ENABLED_ACCESSIBILITY)" in page
    assert 'add_label(\n            "Enabled"' not in page


def test_switch_label_renders_for_toggle_and_load():
    labels = []
    namespace = {"enabled_label": hpe.enabled_label}
    method = _extract(("_phrases_render_enabled",), namespace)["_phrases_render_enabled"]
    controller = SimpleNamespace(
        phrases=SimpleNamespace(enabled=False),
        phrase_enabled_label=SimpleNamespace(setStringValue_=labels.append),
    )
    method(controller)
    controller.phrases.enabled = True
    method(controller)
    assert labels == ["Off", "On"]


@pytest.mark.parametrize(("test_input", "output", "status", "expected"), [
    ("", "", hpe.PREVIEW_PROMPT, hpe.PREVIEW_PLACEHOLDER),
    ("   ", "", hpe.PREVIEW_PROMPT, hpe.PREVIEW_PLACEHOLDER),
    ("please review", "", "No hot phrase matched", hpe.PREVIEW_EMPTY_RESULT),
    ("please review", "", hpe.PREVIEW_ERROR, ""),
    ("please review checklist", "please CHECK", "Matched: “x”", ""),
])
def test_preview_placeholder_only_when_the_box_is_empty(test_input, output, status, expected):
    assert hpe.preview_placeholder(test_input, output, status) == expected


def test_preview_box_keeps_real_output_and_draws_the_placeholder():
    render = _sources()["_phrases_render_preview"]
    assert "self.phrase_preview.setString_(output)" in render
    assert "preview_placeholder(self.phrases.test_input, output, status)" in render
    assert "setPlaceholder_color_(" in render
    assert "PlaceholderTextView.alloc()" in _sources()["_phrases_text_area"]
    # phrases_model.py is not where any of this lives.
    model = (ROOT / "localflow" / "phrases_model.py").read_text(encoding="utf-8")
    assert "placeholder" not in model.lower()


# ---------------------------------------------------------------- snapshots ---

def test_snapshots_cover_the_new_states_with_isolated_stores():
    snapshots = (ROOT / "scripts" / "mac_ui_snapshots.py").read_text(encoding="utf-8")
    for name in (
        '"running-no-system-audio"', '"hub-meetings-recent-empty"',
        '"hub-style-default"', '"hub-style-autosaved"', '"hub-style-save-error"',
        '"hub-hot-phrases-preview-result"', '"hub-prompts-failures"',
        '"hub-models-parakeet"',
    ):
        assert name in snapshots, name
    # Style autosave never writes the real config; the vault is always temp.
    assert 'patch("localflow.hub_mac.save_config")' in snapshots
    assert 'side_effect=OSError("read-only")' in snapshots
    assert 'Config(vault_path=str(Path(tmp) / "vault-default"))' in snapshots
    assert "_isolated_config(" in snapshots
    tree = ast.parse(snapshots)
    bare = [
        node.lineno for node in ast.walk(tree)
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
        and node.func.id == "Config" and not node.keywords
    ]
    assert bare == [], f"Config() without a temp vault at lines {bare}"
    # The preview reads the dictionary: it is redirected to a temp store.
    assert '"localflow.phrases_model.DictionaryStore"' in snapshots


def test_failed_prompt_fixture_maps_and_wraps():
    row = hub_model.prompt_rows([
        {"id": "a", "status": "failed", "repo": "localflow", "error": "Orca is not running"},
    ])[0]
    assert row["context"].endswith(hub_model.ORCA_NOT_OPEN)
    assert replace(Config(), vault_path="/tmp/x").vault_path == "/tmp/x"


EMBEDDER = DIAR_MODELS[1]  # its HF repo also holds models LocalFlow never fetched


@pytest.mark.parametrize(("repo", "files", "frees"), [
    (EMBEDDER.repo, {f"snapshots/{EMBEDDER.revision}/{EMBEDDER.filename}": 3000,
                     f"snapshots/{EMBEDDER.revision}/wespeaker.onnx": 900_000}, "3.0 KB"),
    ("Systran/faster-whisper-base", {"snapshots/rev/model.bin": 200}, "200 bytes"),
])
def test_delete_model_dialog_names_the_listed_size_and_cancel_keeps_it(
    tmp_path, repo, files, frees,
):
    folder = tmp_path / ms.repo_folder_name(repo)
    for name, size in files.items():  # copy layout: plain files, no symlinks
        (folder / name).parent.mkdir(parents=True, exist_ok=True)
        (folder / name).write_bytes(b"x" * size)
    alert, load_config = Mock(), Mock()
    alert.runModal.return_value = 1001  # Cancel
    namespace = {"NSAlert": Mock(), "load_config": load_config,
                 "AppKit": SimpleNamespace(NSAlertFirstButtonReturn=1000)}
    namespace["NSAlert"].alloc.return_value.init.return_value = alert
    action = _extract(("deleteSpeechModel_",), namespace)["deleteSpeechModel_"]
    controller = SimpleNamespace(storage_repos=[repo], storage_cache=tmp_path, show_page=Mock())

    action(controller, SimpleNamespace(tag=lambda: 0))

    alert.setMessageText_.assert_called_once_with(f"Delete {ms.catalog()[repo]}?")
    alert.setInformativeText_.assert_called_once_with(
        f"This frees {frees}. LocalFlow can download it again later."
    )
    load_config.assert_not_called()
    controller.show_page.assert_not_called()
    assert {name: (folder / name).stat().st_size for name in files} == files
