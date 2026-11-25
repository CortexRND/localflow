"""Models & keys -> Speaker labels in the AppKit Hub.

The real hub_mac methods run against stub views (extracted from the source
as in test_models_change), with a fake speaker backend and a manual "worker
thread" and "main queue", so threading, guards and in-place updates are
executed, not just read.
"""

import ast
import re
import threading
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import pytest
from test_models_change import Control, Label, View, hub_class

from localflow import models_layout as ml
from localflow.config import Config
from localflow.diarization.models import NOT_DOWNLOADED, diarization_repos
from localflow.model_storage import CachedModel, ModelStorageError
from localflow.ui_tokens import HUB_PAGE_BOTTOM

ROOT = Path(__file__).resolve().parents[1]
HUB = ROOT / "localflow/hub_mac.py"
FIRST = 1000  # NSAlertFirstButtonReturn
EVICTED = "model.onnx failed its checksum and was removed. Run `lf speakers download`."
EVICTED_SHOWN = "model.onnx failed its checksum and was removed. Choose Download."
TOTAL = ml.speaker_total_bytes()
METHODS = (
    "_speaker_state", "_speaker_card", "_speaker_render", "_speaker_draw", "_speaker_resize",
    "_speaker_run", "_speaker_check", "_speaker_refresh", "speakerDownload_", "speakerCancel_",
    "speakerVerify_", "speakerRemove_", "_fit_body_height", "windowWillClose_",
)


class ToneLabel(Label):
    def __init__(self, text, rect, color):
        super().__init__(text=text, rect=rect)
        self.color = color


class Button(Control):
    def setAccessibilityLabel_(self, text):
        self.accessibility = text


class Bar(Control):
    def setDoubleValue_(self, value):
        self.calls.append("setDoubleValue_")
        self.fraction = value


class Backend:
    """Fake status/download/inventory/delete; `download` is set per test."""

    def __init__(self, *, ready=False, reason=NOT_DOWNLOADED, extra=True, refuse=()):
        self.calls, self.ready, self.reason, self.extra = [], ready, reason, extra
        self.refuse = set(refuse)
        self.fetch = None

    def status(self, cache=None):
        self.calls.append(("status", cache))
        return {"models_ready": self.ready, "reason": "" if self.ready else self.reason,
                "extra_installed": self.extra, "models": []}

    def download(self, on_progress, cancel=None, cache=None):
        self.calls.append(("download", cache))
        self.cancel = cancel
        if self.fetch:
            self.fetch(on_progress, cancel)
        self.ready = True

    def inventory(self, config, cache=None):
        self.calls.append(("inventory", config, cache))
        return [CachedModel(repo, repo, Path(repo), 5, "") for repo in diarization_repos()]

    def delete(self, repo, config, cache=None):
        self.calls.append(("delete", repo, config, cache))
        if repo in self.refuse:
            raise ModelStorageError("Speaker labels: voice embedding is used for speaker "
                                    "labels in your settings, so it was kept.")
        return 7


def namespace(fresh, alerts):
    def label(text, x, y, w, size=14, color="ink", *, height=28, **_kwargs):
        return ToneLabel(text, (x, y, w, height), color)

    def pill(text, kind, _palette=None):
        made = ToneLabel(text, (0, 0, 90, 24), kind[1])
        made.kind = kind
        return made

    class Alert:
        @classmethod
        def alloc(cls):
            return cls()

        def init(self):
            self.texts = []
            return self

        def setMessageText_(self, text):
            self.texts.append(text)

        def setInformativeText_(self, text):
            self.texts.append(text)

        def addButtonWithTitle_(self, title):
            pass

        def runModal(self):
            alerts.append(self.texts)
            return alerts.answer

    names = {name: getattr(ml, name) for name in dir(ml) if name.startswith("speaker_")}
    names["speaker_run_download"] = lambda backend, cache, cancel, report: (
        ml.speaker_run_download(backend, cache, cancel, report, interval=0))
    return {
        **names,
        "FlippedView": SimpleNamespace(alloc=lambda: SimpleNamespace(
            initWithFrame_=lambda frame: View(rect=frame))),
        "NSMakeRect": lambda *a: a, "NSMakePoint": lambda x, y: (x, y),
        "style_surface": lambda *a, **k: None, "_label": label, "_font": lambda *a: None,
        "measure_text": lambda text, _font: len(text) * 7, "make_pill": pill,
        "NSProgressIndicator": Bar, "NSAlert": Alert,
        "AppKit": SimpleNamespace(NSAlertFirstButtonReturn=FIRST),
        "CARD_PADDING": ml.CARD_PADDING, "MESSAGE_LINE": ml.MESSAGE_LINE,
        "HUB_PAGE_BOTTOM": HUB_PAGE_BOTTOM, "note_lines": ml.note_lines,
        "load_config": lambda: fresh[0], "threading": threading,
        "logging": __import__("logging"),
    }


class Alerts(list):
    answer = FIRST


def make_hub(tmp_path, config=None, backend=None, *, state=None, width=800):
    fresh, alerts = [config or Config()], Alerts()
    hub = hub_class(METHODS, namespace(fresh, alerts))()
    hub.fresh, hub.alerts = fresh, alerts
    hub.config, hub.tokens, hub.cursor_y = fresh[0], {}, 0
    hub.body = View(rect=(0, 0, width, 300))
    hub._viewport_height = lambda: 300
    hub.page, hub._closed, hub.provider_expanded = "Models", False, None
    hub.speaker_backend, hub.storage_cache = backend or Backend(), tmp_path
    hub.threads, hub.main, hub.rebuilds = [], [], 0
    hub._speaker_start = hub.threads.append  # run later, by hand
    hub._speaker_main = hub.main.append  # the main queue, drained by hand
    hub.phrases, hub._meeting_flush_on_close = None, lambda: None
    if state is not None:
        hub.speaker_state = state

    def button(title, action, x, y, tag=0, width=150, parent=None, kind="secondary",
               height=36):
        made = Button()
        made.title, made.action, made.kind, made.rect = title, action, kind, (x, y, width,
                                                                               height)
        parent.addSubview_(made)
        return made

    def rebuild():
        hub.rebuilds += 1
        hub.body.subviews.clear()
        hub.cursor_y = 0
        hub._speaker_card(width)

    hub._button = button
    hub._rebuild_current_page = rebuild
    hub._speaker_card(width)
    return hub


def run_threads(hub):
    while hub.threads:
        hub.threads.pop(0)()


def drain(hub, count=None):
    done = 0
    while hub.main and (count is None or done < count):
        hub.main.pop(0)()
        done += 1


def settle(hub):
    while hub.threads or hub.main:
        run_threads(hub)
        drain(hub)


def card(hub):
    return hub.speaker_controls["card"]


def buttons(hub):
    return {b.title: b for b in card(hub).subviews if isinstance(b, Button)}


def texts(hub):
    return [v.text for v in card(hub).subviews if isinstance(v, ToneLabel)]


def note(hub):
    """(text, color) of each note label under the detail line."""
    detail_bottom = ml.speaker_layout(800, buttons=0, progress=False)["detail"]
    bottom = detail_bottom[1] + detail_bottom[3]
    return [(v.text, v.color) for v in card(hub).subviews
            if isinstance(v, ToneLabel) and v.rect[1] > bottom and v.text
            and v is not hub.speaker_controls.get("progress_text")]


def pill(hub):
    # Stub views answer any attribute, so look at what was actually set.
    return next(v for v in card(hub).subviews
                if isinstance(v, Label) and "kind" in vars(v))


def ready_state():
    return {**ml.speaker_initial_state(), "phase": "ready"}


# ------------------------------------------------------------ status ---


def test_first_draw_checks_once_and_a_rebuild_reuses_the_cached_status(tmp_path):
    backend = Backend()
    hub = make_hub(tmp_path, backend=backend)
    assert len(hub.threads) == 1 and pill(hub).text == "Checking…"
    assert buttons(hub) == {}
    settle(hub)
    assert backend.calls == [("status", tmp_path)]
    assert pill(hub).text == "Not downloaded" and list(buttons(hub)) == ["Download"]
    assert buttons(hub)["Download"].kind == "primary"
    assert buttons(hub)["Download"].accessibility == "Download speaker models"
    hub.body.subviews.clear()
    hub._speaker_card(800)  # a resize/appearance rebuild: no re-hash
    assert hub.threads == [] and list(buttons(hub)) == ["Download"]


def test_ready_shows_verify_and_remove(tmp_path):
    hub = make_hub(tmp_path, backend=Backend(ready=True))
    settle(hub)
    assert pill(hub).text == "Ready" and pill(hub).kind == ("success", "success")
    assert [b.title for b in card(hub).subviews if isinstance(b, Button)] == [
        "Verify", "Remove…"]
    assert buttons(hub)["Remove…"].kind == "destructive_ghost"
    assert buttons(hub)["Remove…"].enabled is True and note(hub) == []


def test_missing_runtime_shows_the_hint_and_still_offers_download(tmp_path):
    hub = make_hub(tmp_path, backend=Backend(extra=False))
    settle(hub)
    assert note(hub) == [(ml.SPEAKER_HINT, "muted")]
    assert buttons(hub)["Download"].enabled is True


def test_checksum_eviction_is_shown_without_the_cli_hint(tmp_path):
    hub = make_hub(tmp_path, backend=Backend(reason=EVICTED))
    settle(hub)
    assert note(hub) == [(EVICTED_SHOWN, "red")]


# ---------------------------------------------------------- download ---


def test_download_progress_updates_in_place_then_ready(tmp_path):
    backend = Backend()
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)
    other = View(rect=(0, card(hub).rect[3] + 12, 800, 50))  # Free up space, below
    hub.body.addSubview_(other)
    before = other.rect

    backend.fetch = lambda report, _cancel: [report(d, TOTAL) for d in (0, TOTAL // 2, TOTAL)]
    hub.speakerDownload_(None)
    assert hub.speaker_state["phase"] == "downloading" and pill(hub).text == "Downloading"
    assert list(buttons(hub)) == ["Cancel"] and buttons(hub)["Cancel"].enabled
    bar = hub.speaker_controls["bar"]
    assert hub.speaker_controls["progress_text"].text == "Starting…"
    run_threads(hub)
    drain(hub, 2)  # the first two ticks
    assert hub.speaker_controls["bar"] is bar and bar.fraction == pytest.approx(0.5, abs=0.01)
    assert hub.speaker_controls["progress_text"].text == "17 MB of 34 MB · 50%"
    assert hub.rebuilds == 0 and other.rect == before  # in place: nothing below moved
    drain(hub)
    assert hub.speaker_state["phase"] == "ready" and hub.rebuilds == 1
    assert note(hub) == [("Speaker models downloaded.", "success")]
    assert backend.calls[-1] == ("download", tmp_path)
    assert isinstance(backend.cancel, threading.Event) and not backend.cancel.is_set()


def test_finished_download_with_an_editor_open_only_updates_the_card(tmp_path):
    backend = Backend()
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)
    hub.provider_expanded = "fireworks"  # a key may be typed there
    hub.speakerDownload_(None)
    settle(hub)
    assert hub.rebuilds == 0 and pill(hub).text == "Ready"


def test_cancel_sets_the_event_and_ends_not_downloaded(tmp_path):
    from localflow.models import DownloadCancelled

    backend = Backend()
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)

    def fetch(report, cancel):
        report(0, TOTAL)
        if cancel.is_set():
            raise DownloadCancelled("Download cancelled")

    backend.fetch = fetch
    hub.speakerDownload_(None)
    cancel = hub.speaker_state["cancel"]
    hub.speakerCancel_(None)
    assert cancel.is_set() and hub.speaker_state["phase"] == "cancelling"
    assert buttons(hub)["Cancel"].enabled is False
    assert hub.speaker_controls["progress_text"].text == "Cancelling…"
    settle(hub)
    assert hub.speaker_state["phase"] == "missing"
    assert note(hub) == [("Download cancelled.", "muted")]
    assert list(buttons(hub)) == ["Download"]


def test_download_error_is_shown_in_red(tmp_path):
    from localflow.models import DownloadError

    backend = Backend()
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)

    def fetch(_report, _cancel):
        raise DownloadError("Could not download model.onnx: HTTP 503")

    backend.fetch = fetch
    hub.speakerDownload_(None)
    settle(hub)
    assert hub.speaker_state["phase"] == "missing"
    assert note(hub) == [("Could not download model.onnx: HTTP 503", "red")]


def test_download_is_ignored_unless_not_downloaded(tmp_path):
    hub = make_hub(tmp_path, backend=Backend(ready=True))
    settle(hub)
    hub.speakerDownload_(None)
    assert hub.threads == [] and hub.speaker_state["phase"] == "ready"


# ------------------------------------------------------------ verify ---


def test_verify_pass_and_fail(tmp_path):
    backend = Backend(ready=True)
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)
    other = View(rect=(0, card(hub).rect[3] + 12, 800, 50))  # Free up space, below
    hub.body.addSubview_(other)
    top, height = other.rect[1], card(hub).rect[3]
    hub.speakerVerify_(None)
    assert pill(hub).text == "Checking…"
    settle(hub)
    assert note(hub) == [("Speaker models verified.", "success")]
    grown = ml.MODELS_ROW_GAP + ml.MESSAGE_LINE  # the card grew in place by one note line
    assert card(hub).rect[3] == height + grown and other.rect[1] == top + grown
    assert hub.rebuilds == 0
    backend.ready = False
    backend.reason = EVICTED
    hub.speakerVerify_(None)
    settle(hub)
    assert hub.speaker_state["phase"] == "missing" and list(buttons(hub)) == ["Download"]
    assert note(hub) == [(EVICTED_SHOWN, "red")]


# ------------------------------------------------------------ remove ---


def test_remove_cancelled_in_the_alert_touches_nothing(tmp_path):
    backend = Backend(ready=True)
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)
    hub.alerts.answer = FIRST + 1
    hub.speakerRemove_(None)
    assert hub.alerts[0][0] == "Remove speaker models?" and "34 MB" in hub.alerts[0][1]
    assert hub.threads == [] and backend.calls == [("status", tmp_path)]


def test_remove_confirmed_deletes_each_repo_with_a_fresh_config(tmp_path):
    backend = Backend(ready=True)
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)
    fresh = replace(Config(), model_size="small")
    hub.fresh[0] = fresh
    hub.speakerRemove_(None)
    assert pill(hub).text == "Removing…" and buttons(hub) == {}
    backend.ready = False
    settle(hub)
    deletes = [call for call in backend.calls if call[0] == "delete"]
    assert deletes == [("delete", repo, fresh, tmp_path) for repo in diarization_repos()]
    assert ("inventory", fresh, tmp_path) in backend.calls
    assert hub.rebuilds == 1 and backend.calls[-1] == ("status", tmp_path)  # checked again
    assert hub.speaker_state["phase"] == "missing"
    assert note(hub) == [("Removed speaker models and freed 14 bytes.", "success")]


def test_protected_models_disable_remove_and_never_delete(tmp_path):
    backend = Backend(ready=True)
    hub = make_hub(tmp_path, replace(Config(), speaker_diarization=True), backend)
    settle(hub)
    assert buttons(hub)["Remove…"].enabled is False
    assert note(hub) == [(ml.SPEAKER_PROTECTED_NOTE, "muted")]
    hub.speakerRemove_(None)
    assert hub.alerts == [] and hub.threads == []
    assert not any(call[0] == "delete" for call in backend.calls)


def test_turned_on_since_drawn_is_caught_at_click_time(tmp_path):
    backend = Backend(ready=True)
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)
    assert buttons(hub)["Remove…"].enabled is True
    hub.fresh[0] = replace(Config(), speaker_diarization=True)
    hub.speakerRemove_(None)
    assert hub.alerts == [] and hub.threads == []
    assert buttons(hub)["Remove…"].enabled is False
    assert note(hub) == [(ml.SPEAKER_PROTECTED_NOTE, "muted")]


def test_a_refused_delete_is_shown_and_the_models_stay_ready(tmp_path):
    embedding = list(diarization_repos())[1]
    backend = Backend(ready=True, refuse=[embedding])
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)
    hub.speakerRemove_(None)
    settle(hub)
    assert hub.speaker_state["phase"] == "ready"
    message = note(hub)[0]
    assert message[1] == "red" and "used for speaker labels" in message[0]


# ------------------------------------------------- guards and close ---


def test_stale_closed_and_off_page_results_change_no_views(tmp_path):
    backend = Backend(ready=True)
    hub = make_hub(tmp_path, backend=backend)
    run_threads(hub)
    stale = hub.main[:]
    hub.main.clear()
    hub.speakerVerify_(None)  # phase is still "checking": ignored
    hub._speaker_check()  # a newer check
    views = list(card(hub).subviews)
    for deliver in stale:
        deliver()
    assert hub.speaker_state["phase"] == "checking" and card(hub).subviews == views

    run_threads(hub)
    hub._closed = True
    drain(hub)
    assert hub.speaker_state["phase"] == "checking" and card(hub).subviews == views

    hub._closed = False
    hub._speaker_check()
    run_threads(hub)
    hub.page = "Home"
    drain(hub)
    assert hub.speaker_state["phase"] == "ready"  # state stays current...
    assert card(hub).subviews == views  # ...but nothing off-page is touched


def test_closing_mid_download_cancels_and_drops_the_late_result(tmp_path):
    backend = Backend()
    hub = make_hub(tmp_path, backend=backend)
    settle(hub)
    hub.speakerDownload_(None)
    cancel = hub.speaker_state["cancel"]
    hub.windowWillClose_(None)
    assert cancel.is_set() and hub._closed
    views = list(card(hub).subviews)
    settle(hub)
    assert hub.speaker_state["phase"] == "downloading" and card(hub).subviews == views


def test_snapshot_mode_starts_no_thread(tmp_path):
    hub = make_hub(tmp_path, state=ready_state())
    hub.provider_snapshot_mode = True
    hub.speakerVerify_(None)
    assert hub.threads == [] and hub.speaker_state["phase"] == "checking"


# ------------------------------------------------------------ source ---


def _source():
    source = HUB.read_text(encoding="utf-8")
    tree = ast.parse(source)
    controller = next(n for n in tree.body if isinstance(n, ast.ClassDef)
                      and n.name == "HubWindowController")
    methods = {n.name: n for n in controller.body if isinstance(n, ast.FunctionDef)}
    return source, methods


def method_source(name):
    source, methods = _source()
    return ast.get_source_segment(source, methods[name])


def test_page_wiring_and_existing_cards_kept():
    page = method_source("_page_models")
    assert page.count("self._speaker_card(width)") == 1
    assert page.count("self._models_quick_card(width)") == 1
    assert page.index("_models_provider_list") < page.index("_speaker_card") < page.index(
        "_models_storage_card(width)")
    show = method_source("show_page")
    assert "self.speaker_controls = {}" in show and "speaker_reset" in show
    assert "provider_expanded" not in show
    delete = method_source("deleteSpeechModel_")
    assert "diarization_repos()" in delete and "speaker_reset" in delete


def test_remove_confirms_and_rereads_the_config():
    remove = method_source("speakerRemove_")
    for needle in ("load_config()", "runModal()", "NSAlertFirstButtonReturn",
                   "speaker_protected(config)", "speaker_run_remove(backend, cache, load_config)"):
        assert needle in remove, needle


def test_the_download_stack_is_imported_only_by_the_download_worker():
    heavy = re.compile(r"\blocalflow\.models\b(?!_)|\bsherpa_onnx\b|\bhuggingface_hub\b"
                       r"|^\s*import requests|from requests")
    for path in ("localflow/hub_mac.py", "localflow/hub_tk.py"):
        assert not heavy.search((ROOT / path).read_text(encoding="utf-8")), path
    layout = ROOT / "localflow/models_layout.py"
    tree = ast.parse(layout.read_text(encoding="utf-8"))
    owners = [
        func.name for func in ast.walk(tree) if isinstance(func, ast.FunctionDef)
        for node in ast.walk(func)
        if isinstance(node, ast.ImportFrom) and node.module == "localflow.models"
    ]
    assert owners == ["speaker_run_download"]
    assert not any(isinstance(node, ast.ImportFrom) and node.module == "localflow.models"
                   for node in tree.body)


def test_workers_run_off_the_main_thread_and_close_cancels():
    run = method_source("_speaker_run")
    assert "if self._closed or self._speaker_state()[\"token\"] != token:" in run
    assert "provider_snapshot_mode" in run
    assert "threading.Thread" in method_source("_speaker_start")
    assert "addOperationWithBlock_" in method_source("_speaker_main")
    render = method_source("_speaker_render")
    assert 'self.page != "Models"' in render and "self._closed" in render
    assert "show_page" not in render and "_rebuild_current_page" not in render
    close = method_source("windowWillClose_")
    assert close.index("self._closed = True") < close.index("cancel.set()")
    _source_text, methods = _source()
    for name in [n for n in methods if n.startswith(("_speaker", "speaker"))]:
        text = method_source(name)
        assert "print(" not in text and "os.environ" not in text and "secure" not in text, name
