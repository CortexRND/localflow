"""Models -> Speaker labels in the Tk Hub: a real window, a fake backend
gated with threading.Events, and the root.after queue pumped by hand."""

import gc
import sys
import threading
import time
from dataclasses import replace
from pathlib import Path

import pytest

tk = pytest.importorskip("tkinter")
pytestmark = pytest.mark.skipif(
    sys.platform == "darwin",
    reason="macOS uses the AppKit Hub, covered by test_hub_speaker_models.",
)

from test_hub_meetings_tk import FakeMeetingClient, _pump

from localflow import hub_tk
from localflow import models_layout as ml
from localflow.config import Config
from localflow.diarization.models import NOT_DOWNLOADED, diarization_repos
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.model_storage import CachedModel, ModelStorageError

TOTAL = ml.speaker_total_bytes()
EVICTED = "model.onnx failed its checksum and was removed. Run `lf speakers download`."
EVICTED_SHOWN = "model.onnx failed its checksum and was removed. Choose Download."


class Backend:
    """status/download/inventory/delete; a download waits on `gate`."""

    def __init__(self, *, ready=False, extra=True, refuse=()):
        self.ready, self.extra, self.refuse = ready, extra, set(refuse)
        self.reason = NOT_DOWNLOADED
        self.calls = []
        self.gate = threading.Event()
        self.started = threading.Event()
        self.fail = None
        self.cancel = None

    def status(self, cache=None):
        self.calls.append(("status", cache))
        return {"models_ready": self.ready, "reason": "" if self.ready else self.reason,
                "extra_installed": self.extra, "models": []}

    def download(self, on_progress, cancel=None, cache=None):
        from localflow.models import DownloadCancelled

        self.calls.append(("download", cache))
        self.cancel = cancel
        on_progress(0, TOTAL)
        time.sleep(0.15)  # past the progress throttle
        on_progress(TOTAL // 2, TOTAL)
        self.started.set()
        while not self.gate.wait(0.01):
            if cancel.is_set():
                raise DownloadCancelled("Download cancelled")
        if self.fail:
            raise self.fail
        on_progress(TOTAL, TOTAL)
        self.ready = True

    def inventory(self, config, cache=None):
        self.calls.append(("inventory", config, cache))
        return [CachedModel(repo, repo, Path(repo), 5, "") for repo in diarization_repos()]

    def delete(self, repo, config, cache=None):
        self.calls.append(("delete", repo, config, cache))
        if repo in self.refuse:
            raise ModelStorageError("It is used for speaker labels in your settings, "
                                    "so it was kept.")
        self.ready = False
        return 7


@pytest.fixture
def config_holder(monkeypatch):
    holder = [Config()]
    monkeypatch.setattr(hub_tk, "load_config", lambda: holder[0])
    return holder


@pytest.fixture(autouse=True)
def _collect_tk_vars_on_main_thread():
    yield
    gc.collect()


def _window(tmp_path, backend, config_holder):
    try:
        root = tk.Tk()
    except tk.TclError as exc:
        pytest.skip(f"Tk unavailable: {exc}")
    root.withdraw()
    window = hub_tk.HubWindow(
        root, HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips"),
        DictionaryStore(tmp_path / "dictionary.json"), config_holder[0],
        meeting_client=FakeMeetingClient(), initial_page="Models",
        storage_cache=tmp_path, speaker_backend=backend,
    )
    return root, window


def _destroy(root):
    try:
        root.destroy()
    except tk.TclError:
        pass


def phase_is(window, phase):
    return lambda: window._speaker["phase"] == phase


def button(window, button_id):
    return window._speaker_widgets["button"].get(button_id)


def notes(window):
    return [(label.cget("text"), label.cget("fg"))
            for label in window._speaker_widgets["notes"].winfo_children()]


def packed(widget):
    return widget.winfo_manager() == "pack"


def test_not_downloaded_then_download_with_progress_then_ready(tmp_path, config_holder):
    backend = Backend()
    root, window = _window(tmp_path, backend, config_holder)
    try:
        widgets = window._speaker_widgets
        assert widgets["pill"].cget("text") == "Checking…"
        _pump(root, phase_is(window, "missing"))
        assert backend.calls == [("status", tmp_path)]
        assert widgets["pill"].cget("text") == "Not downloaded"
        assert button(window, "download").cget("state") == "normal"
        assert not packed(widgets["progress"])

        button(window, "download").invoke()
        assert widgets["pill"].cget("text") == "Downloading"
        assert button(window, "cancel").cget("state") == "normal"
        _pump(root, lambda: "50%" in widgets["progress_text"].cget("text"))
        assert packed(widgets["progress"])
        assert float(widgets["bar"].cget("value")) == pytest.approx(0.5, abs=0.01)
        assert widgets["progress_text"].cget("text") == "17 MB of 34 MB · 50%"

        backend.gate.set()
        _pump(root, phase_is(window, "ready"))
        assert widgets["pill"].cget("text") == "Ready"
        assert not packed(widgets["progress"])
        assert set(window._speaker_widgets["button"]) == {"verify", "remove"}
        assert notes(window) == [("Speaker models downloaded.", window.tokens["success"])]
        assert backend.cancel is not None and not backend.cancel.is_set()
    finally:
        _destroy(root)


def test_cancel_mid_download(tmp_path, config_holder):
    backend = Backend()
    root, window = _window(tmp_path, backend, config_holder)
    try:
        _pump(root, phase_is(window, "missing"))
        button(window, "download").invoke()
        assert backend.started.wait(5)
        button(window, "cancel").invoke()
        assert backend.cancel.is_set()
        assert button(window, "cancel").cget("state") == "disabled"
        assert window._speaker_widgets["progress_text"].cget("text") == "Cancelling…"
        _pump(root, phase_is(window, "missing"))
        assert notes(window) == [("Download cancelled.", window.tokens["muted"])]
        assert button(window, "download") is not None
    finally:
        _destroy(root)


def test_download_error_is_shown_in_red(tmp_path, config_holder):
    from localflow.models import DownloadError

    backend = Backend()
    backend.fail = DownloadError("Could not download model.onnx: HTTP 503")
    backend.gate.set()
    root, window = _window(tmp_path, backend, config_holder)
    try:
        _pump(root, phase_is(window, "missing"))
        button(window, "download").invoke()
        _pump(root, lambda: bool(notes(window)))
        assert window._speaker["phase"] == "missing"
        assert notes(window) == [("Could not download model.onnx: HTTP 503",
                                  window.tokens["red"])]
    finally:
        _destroy(root)


def test_verify_pass_and_fail(tmp_path, config_holder):
    backend = Backend(ready=True)
    root, window = _window(tmp_path, backend, config_holder)
    try:
        _pump(root, phase_is(window, "ready"))
        button(window, "verify").invoke()
        _pump(root, lambda: bool(notes(window)))
        assert notes(window) == [("Speaker models verified.", window.tokens["success"])]
        backend.ready = False
        backend.reason = EVICTED
        button(window, "verify").invoke()
        _pump(root, phase_is(window, "missing"))
        assert notes(window) == [(EVICTED_SHOWN, window.tokens["red"])]
    finally:
        _destroy(root)


def test_remove_confirmed_and_cancelled(tmp_path, config_holder, monkeypatch):
    backend = Backend(ready=True)
    asked = []
    answer = [False]
    monkeypatch.setattr(hub_tk.messagebox, "askyesno",
                        lambda title, text: asked.append(text) or answer[0])
    root, window = _window(tmp_path, backend, config_holder)
    try:
        _pump(root, phase_is(window, "ready"))
        button(window, "remove").invoke()
        assert len(asked) == 1 and "34 MB" in asked[0]
        assert not any(call[0] == "delete" for call in backend.calls)

        fresh = replace(Config(), model_size="small")
        config_holder[0] = fresh
        answer[0] = True
        button(window, "remove").invoke()
        assert window._speaker_widgets["pill"].cget("text") == "Removing…"
        _pump(root, lambda: window._speaker["phase"] == "missing" and notes(window))
        deletes = [call for call in backend.calls if call[0] == "delete"]
        assert deletes == [("delete", repo, fresh, tmp_path) for repo in diarization_repos()]
        assert backend.calls[-1] == ("status", tmp_path)  # checked again afterwards
        assert notes(window) == [("Removed speaker models and freed 14 bytes.",
                                  window.tokens["success"])]
    finally:
        _destroy(root)


def test_remove_protected_by_speaker_labels(tmp_path, config_holder, monkeypatch):
    config_holder[0] = replace(Config(), speaker_diarization=True)
    backend = Backend(ready=True)
    asked = []
    monkeypatch.setattr(hub_tk.messagebox, "askyesno",
                        lambda *args: asked.append(args) or True)
    root, window = _window(tmp_path, backend, config_holder)
    try:
        _pump(root, phase_is(window, "ready"))
        assert button(window, "remove").cget("state") == "disabled"
        assert notes(window) == [(ml.SPEAKER_PROTECTED_NOTE, window.tokens["muted"])]
        window._speaker_remove()
        assert asked == [] and not any(call[0] == "delete" for call in backend.calls)
    finally:
        _destroy(root)


def test_a_refused_delete_keeps_the_models_ready(tmp_path, config_holder, monkeypatch):
    backend = Backend(ready=True, refuse=list(diarization_repos()))
    monkeypatch.setattr(hub_tk.messagebox, "askyesno", lambda *args: True)
    root, window = _window(tmp_path, backend, config_holder)
    try:
        _pump(root, phase_is(window, "ready"))
        button(window, "remove").invoke()
        _pump(root, lambda: window._speaker["phase"] == "ready" and notes(window))
        text, color = notes(window)[0]
        assert "used for speaker labels" in text and color == window.tokens["red"]
    finally:
        _destroy(root)


def test_missing_runtime_shows_the_hint_and_keeps_download(tmp_path, config_holder):
    backend = Backend(extra=False)
    root, window = _window(tmp_path, backend, config_holder)
    try:
        _pump(root, phase_is(window, "missing"))
        assert notes(window) == [(ml.SPEAKER_HINT, window.tokens["muted"])]
        assert button(window, "download").cget("state") == "normal"
    finally:
        _destroy(root)


def test_leaving_and_returning_mid_download(tmp_path, config_holder):
    backend = Backend()
    root, window = _window(tmp_path, backend, config_holder)
    try:
        _pump(root, phase_is(window, "missing"))
        button(window, "download").invoke()
        assert backend.started.wait(5)
        window.show_page("Home")
        _pump(root, lambda: window._speaker["done"] > 0)
        window.show_page("Models")  # still downloading: shown as such, no re-check
        assert window._speaker_widgets["pill"].cget("text") == "Downloading"
        _pump(root, lambda: "50%" in window._speaker_widgets["progress_text"].cget("text"))
        backend.gate.set()
        window.show_page("Home")
        _pump(root, phase_is(window, "ready"))
        window.show_page("Models")  # entering again: checked again (offline)
        _pump(root, phase_is(window, "ready"))
        assert window._speaker_widgets["pill"].cget("text") == "Ready"
        assert backend.calls.count(("status", tmp_path)) == 2
    finally:
        _destroy(root)


def test_close_mid_download_cancels_and_ignores_late_results(tmp_path, config_holder):
    backend = Backend()
    root, window = _window(tmp_path, backend, config_holder)
    _pump(root, phase_is(window, "missing"))
    button(window, "download").invoke()
    assert backend.started.wait(5)
    window.close()
    assert backend.cancel.is_set() and window._closed
    deadline = time.monotonic() + 2
    while window._speaker_results.qsize() == 0 and time.monotonic() < deadline:
        time.sleep(0.01)
    window._speaker_dispatch()  # a late drain after close changes nothing
    assert window._speaker["phase"] == "downloading"
    window.close()  # idempotent
    gc.collect()  # no tk.Variable left to complain on teardown
