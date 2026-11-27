"""Models & keys -> Speaker labels: the pure rules, workers and geometry
(models_layout speaker_*), shared by the AppKit and Tk hubs."""

import itertools
import subprocess
import sys
import threading
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import pytest

from localflow import models_layout as ml
from localflow.config import Config
from localflow.diarization.models import DIAR_MODELS, NOT_DOWNLOADED, diarization_repos
from localflow.model_storage import CachedModel, ModelStorageError

ROOT = Path(__file__).resolve().parents[1]
ON = replace(Config(), speaker_diarization=True)
OFF = Config()


def state(phase="missing", **changes):
    return {**ml.speaker_initial_state(), "phase": phase, **changes}


def status(ready, reason="", extra=True):
    return {"models_ready": ready, "reason": reason, "extra_installed": extra, "models": []}


# ------------------------------------------------------------ display ---


@pytest.mark.parametrize("phase, label, kind", [
    ("unknown", "Checking…", ("muted", "muted")),
    ("checking", "Checking…", ("muted", "muted")),
    ("missing", "Not downloaded", ("muted", "muted")),
    ("downloading", "Downloading", ("cobalt", "cobalt")),
    ("cancelling", "Downloading", ("cobalt", "cobalt")),
    ("removing", "Removing…", ("muted", "muted")),
    ("ready", "Ready", ("success", "success")),
])
def test_pill_for_every_phase(phase, label, kind):
    assert ml.speaker_pill(state(phase)) == (label, kind)


@pytest.mark.parametrize("extra", [True, False])
@pytest.mark.parametrize("config", [OFF, ON])
def test_buttons_per_phase_and_only_protection_disables_remove(config, extra):
    def ids(phase):
        return [(b[0], b[3]) for b in ml.speaker_buttons(state(phase, extra_installed=extra),
                                                          config)]

    assert ids("missing") == [("download", True)]  # the missing runtime never blocks
    assert ids("downloading") == [("cancel", True)]
    assert ids("cancelling") == [("cancel", False)]
    assert ids("ready") == [("verify", True), ("remove", config is OFF)]
    for phase in ("unknown", "checking", "removing"):
        assert ids(phase) == []
    kinds = {b[0]: b[2] for b in ml.speaker_buttons(state("ready"), config)}
    assert kinds == {"verify": "secondary", "remove": "destructive_ghost"}
    assert ml.speaker_buttons(state("missing"), config)[0][1:3] == ("Download", "primary")


@pytest.mark.parametrize("extra", [True, False])
@pytest.mark.parametrize("config", [OFF, ON])
def test_lines_message_protection_and_hint(config, extra):
    hint = [] if extra else [(ml.SPEAKER_HINT, "muted")]
    assert ml.speaker_lines(state("missing", extra_installed=extra), config) == hint
    ready = ml.speaker_lines(state("ready", extra_installed=extra), config)
    protected = [(ml.SPEAKER_PROTECTED_NOTE, "muted")] if config is ON else []
    assert ready == protected + hint
    failed = state("missing", extra_installed=extra, message=("Boom.", "red"))
    assert ml.speaker_lines(failed, config) == [("Boom.", "red"), *hint]
    assert "localflow[diarization]" in ml.SPEAKER_HINT


def test_progress_text():
    total = ml.speaker_total_bytes()
    assert total == sum(model.size_bytes for model in DIAR_MODELS)
    assert ml.speaker_progress(state("downloading")) == (0.0, "Starting…")
    fraction, text = ml.speaker_progress(state("downloading", done=12_000_000, total=total))
    assert fraction == pytest.approx(12_000_000 / total)
    assert text == "12 MB of 34 MB · 35%"
    assert ml.speaker_progress(state("cancelling", done=1, total=2))[1] == "Cancelling…"
    assert ml.speaker_detail() == (
        "Names who spoke in meeting notes. 2 models, 34 MB, run on this computer."
    )


def test_protection_follows_speaker_diarization():
    assert ml.speaker_protected(ON) and not ml.speaker_protected(OFF)
    assert not ml.speaker_protected(SimpleNamespace())


# -------------------------------------------------------- transitions ---


def test_begin_bumps_the_token_and_clears_unless_kept():
    before = state("ready", message=("Removed.", "success"), done=5, total=9)
    began = ml.speaker_begin(before, "checking")
    assert began["token"] == before["token"] + 1 and began["phase"] == "checking"
    assert began["message"] == ("", "muted") and (began["done"], began["total"]) == (0, 0)
    assert ml.speaker_begin(before, "checking", keep_message=True)["message"] == (
        "Removed.", "success")
    event = threading.Event()
    assert ml.speaker_begin(before, "downloading", cancel=event)["cancel"] is event
    assert before["phase"] == "ready"  # transitions never mutate


def test_reset_forgets_status_but_never_an_operation():
    assert ml.speaker_reset(state("ready", message=("x", "red")))["phase"] == "unknown"
    for phase in ml.SPEAKER_BUSY:
        busy = state(phase)
        assert ml.speaker_reset(busy) is busy


def test_status_ready_missing_and_errors():
    assert ml.speaker_apply_status(state("checking"), status(True))["phase"] == "ready"
    missing = ml.speaker_apply_status(state("checking"), status(False, NOT_DOWNLOADED))
    assert missing["phase"] == "missing" and missing["message"] == ("", "muted")
    no_extra = ml.speaker_apply_status(state("checking"), status(True, extra=False))
    assert no_extra["extra_installed"] is False
    evicted = ml.speaker_apply_status(state("checking"), status(
        False, "model.onnx failed its checksum and was removed. Run `lf speakers download`."))
    assert evicted["message"] == (
        "model.onnx failed its checksum and was removed. Choose Download.", "red")
    failed = ml.speaker_apply_status(state("checking"), None)
    assert failed["phase"] == "missing" and failed["message"] == (ml.SPEAKER_CHECK_FAILED, "red")
    kept = ml.speaker_apply_status(
        state("checking", message=("Removed speaker models.", "success")),
        status(False, NOT_DOWNLOADED))
    assert kept["message"] == ("Removed speaker models.", "success")


def test_verify_pass_and_fail_never_mention_the_cli():
    passed = ml.speaker_apply_status(state("checking"), status(True), verify=True)
    assert passed["message"] == ("Speaker models verified.", "success")
    failed = ml.speaker_apply_status(state("checking"), status(False, NOT_DOWNLOADED),
                                     verify=True)
    assert failed["phase"] == "missing" and failed["message"][1] == "red"
    assert "lf speakers" not in failed["message"][0] and "Choose Download." in failed["message"][0]
    assert ml.speaker_reason("") == "" and ml.speaker_reason("Plain.") == "Plain."


def test_progress_is_clamped_and_monotonic():
    s = state("downloading")
    s = ml.speaker_apply_progress(s, 10, 100)
    assert (s["done"], s["total"]) == (10, 100)
    assert ml.speaker_apply_progress(s, 5, 100)["done"] == 10  # never backwards
    assert ml.speaker_apply_progress(s, 500, 100)["done"] == 100
    assert ml.speaker_apply_progress(s, 7, 0)["done"] == 0
    settled = state("ready")
    assert ml.speaker_apply_progress(settled, 1, 2) is settled  # a late tick is ignored


def test_cancel_and_download_outcomes():
    downloading = state("downloading", cancel=threading.Event())
    assert ml.speaker_cancel(downloading)["phase"] == "cancelling"
    assert ml.speaker_cancel(state("ready"))["phase"] == "ready"
    done = ml.speaker_apply_download(downloading, "done")
    assert done["phase"] == "ready" and done["message"] == ("Speaker models downloaded.",
                                                            "success")
    assert done["cancel"] is None
    cancelled = ml.speaker_apply_download(downloading, "cancelled")
    assert cancelled["phase"] == "missing" and cancelled["message"] == ("Download cancelled.",
                                                                        "muted")
    error = ml.speaker_apply_download(downloading, "error", "Could not download x: HTTP 503")
    assert error["phase"] == "missing"
    assert error["message"] == ("Could not download x: HTTP 503", "red")
    assert ml.speaker_apply_download(downloading, "error")["message"][0] == (
        ml.SPEAKER_DOWNLOAD_FAILED)


def test_removed_with_and_without_errors():
    removed = ml.speaker_apply_removed(state("removing"), 34_274_078, [])
    assert removed["phase"] == "missing"
    assert removed["message"] == ("Removed speaker models and freed 34 MB.", "success")
    refused = ml.speaker_apply_removed(state("removing"), 0, ["Kept.", "Also kept."])
    assert refused["phase"] == "ready" and refused["message"] == ("Kept.", "red")


def test_remove_repos_are_the_speaker_repos_on_disk():
    repos = list(diarization_repos())
    found = [
        CachedModel("Systran/faster-whisper-base", "Base", Path("x"), 1, ""),
        CachedModel(repos[1], "Speaker labels: voice embedding", Path("y"), 2, ""),
    ]
    assert ml.speaker_remove_repos(found) == [repos[1]]
    assert ml.speaker_remove_repos([]) == []


# ------------------------------------------------------------ workers ---


class Backend:
    """A fake speaker backend recording its calls."""

    def __init__(self, *, ready=False, download=None, present=None, refuse=()):
        self.calls, self.ready, self.download_effect = [], ready, download
        self.present = list(diarization_repos()) if present is None else present
        self.refuse = set(refuse)

    def status(self, cache=None):
        self.calls.append(("status", cache))
        return status(self.ready)

    def download(self, on_progress, cancel=None, cache=None):
        self.calls.append(("download", cache))
        if self.download_effect:
            self.download_effect(on_progress, cancel)

    def inventory(self, config, cache=None):
        self.calls.append(("inventory", config, cache))
        return [CachedModel(repo, repo, Path(repo), 10, "") for repo in self.present]

    def delete(self, repo, config, cache=None):
        self.calls.append(("delete", repo, config, cache))
        if repo in self.refuse:
            raise ModelStorageError(f"{repo} is used for speaker labels in your settings.")
        return 10


def test_check_worker_returns_status_or_none(tmp_path):
    backend = Backend(ready=True)
    assert ml.speaker_run_check(backend, tmp_path)["models_ready"] is True
    assert backend.calls == [("status", tmp_path)]
    broken = SimpleNamespace(status=lambda cache=None: 1 / 0)
    assert ml.speaker_run_check(broken, tmp_path) is None


def test_download_worker_outcomes_and_throttled_progress(tmp_path):
    from localflow.models import DownloadCancelled, DownloadError

    ticks, clock = [], iter([0.0, 0.01, 0.02, 0.5, 0.51])

    def fetch(on_progress, _cancel):
        for done in (0, 10, 20, 30, 100):
            on_progress(done, 100)

    result = ml.speaker_run_download(Backend(download=fetch), tmp_path, None,
                                     lambda d, t: ticks.append(d), clock=lambda: next(clock))
    assert result == ("done", "")
    assert ticks == [0, 30, 100]  # the first, one per interval, and always the last

    def cancelled(_p, _c):
        raise DownloadCancelled("Download cancelled")

    def failed(_p, _c):
        raise DownloadError("Could not download model.onnx: HTTP 503")

    def leaky(_p, _c):
        raise RuntimeError("https://cdn.example/signed?token=secret")

    report = ticks.append
    assert ml.speaker_run_download(Backend(download=cancelled), tmp_path, None, report) == (
        "cancelled", "")
    assert ml.speaker_run_download(Backend(download=failed), tmp_path, None, report) == (
        "error", "Could not download model.onnx: HTTP 503")
    outcome = ml.speaker_run_download(Backend(download=leaky), tmp_path, None, report)
    assert outcome == ("error", ml.SPEAKER_DOWNLOAD_FAILED)  # never the exception's text


def test_remove_worker_loads_config_and_deletes_each_present_repo(tmp_path):
    repos = list(diarization_repos())
    fresh = replace(Config(), model_size="small")
    backend = Backend()
    assert ml.speaker_run_remove(backend, tmp_path, lambda: fresh) == (20, [])
    assert [c for c in backend.calls if c[0] == "delete"] == [
        ("delete", repo, fresh, tmp_path) for repo in repos]
    assert backend.calls[0] == ("inventory", fresh, tmp_path)

    one = Backend(present=[repos[0]])
    assert ml.speaker_run_remove(one, tmp_path, lambda: fresh) == (10, [])
    refused = Backend(refuse=[repos[1]])
    freed, errors = ml.speaker_run_remove(refused, tmp_path, lambda: fresh)
    assert freed == 10 and errors == [f"{repos[1]} is used for speaker labels in your settings."]
    nothing = Backend(present=[])
    assert ml.speaker_run_remove(nothing, tmp_path, lambda: fresh) == (
        0, ["The speaker models aren't downloaded."])

    def unreadable():
        raise OSError("no config")

    assert ml.speaker_run_remove(Backend(), tmp_path, unreadable) == (
        0, [ml.SPEAKER_REMOVE_FAILED])


# ------------------------------------------------------------- layout ---


def overlap(a, b):
    return a[0] < b[0] + b[2] and b[0] < a[0] + a[2] and a[1] < b[1] + b[3] and b[1] < a[1] + a[3]


@pytest.mark.parametrize("width", [620, 800, 1100])
@pytest.mark.parametrize("buttons", [0, 1, 2])
@pytest.mark.parametrize("progress", [True, False])
@pytest.mark.parametrize("lines", [0, 1, 3])
def test_layout_never_overlaps_and_fits(width, buttons, progress, lines):
    frames = ml.speaker_layout(width, buttons=buttons, progress=progress, note_lines=lines)
    height = frames.pop("height")
    for frame in frames.values():
        x, y, w, h = frame
        assert x >= 0 and x + w <= width and y >= 0 and y + h <= height
    for first, second in itertools.combinations(frames.values(), 2):
        assert not overlap(first, second)
    assert ("progress" in frames) is progress and ("note" in frames) is bool(lines)
    assert sum(key.startswith("button") for key in frames) == buttons
    if buttons:  # right-aligned in the title row
        last = frames[f"button{buttons - 1}"]
        assert last[0] + last[2] == width - ml.CARD_PADDING and last[1] == ml.CARD_PADDING


@pytest.mark.parametrize("width", [620, 1100])
def test_card_height_is_fixed_across_phases_and_grows_with_notes(width):
    heights = {
        ml.speaker_layout(width, buttons=buttons, progress=progress, note_lines=1)["height"]
        for buttons in (0, 1, 2) for progress in (True, False)
    }
    assert heights == {ml.speaker_card_height(width, 1)}
    assert ml.speaker_card_height(width, 2) - ml.speaker_card_height(width, 1) == ml.MESSAGE_LINE
    assert ml.speaker_card_height(width, 1) - ml.speaker_card_height(width, 0) == (
        ml.MODELS_ROW_GAP + ml.MESSAGE_LINE)


# ---------------------------------------------------- import hygiene ---


def test_helpers_never_import_the_download_stack():
    """Drawing the card (every helper but the download worker) loads no
    network, Hugging Face, faster-whisper or sherpa-onnx module."""
    code = """
import sys
from localflow import models_layout as ml
from localflow.config import Config
heavy = ("sherpa_onnx", "huggingface_hub", "localflow.models", "faster_whisper")
before = {name for name in heavy if name in sys.modules}
for phase in ml.SPEAKER_PHASES:
    s = dict(ml.speaker_initial_state(), phase=phase, done=1, total=2)
    for config in (Config(), Config(speaker_diarization=True)):
        ml.speaker_pill(s); ml.speaker_buttons(s, config); ml.speaker_lines(s, config)
    ml.speaker_progress(s); ml.speaker_reset(s); ml.speaker_cancel(s)
ml.speaker_detail(); ml.speaker_remove_repos([]); ml.speaker_layout(800, buttons=2, progress=True)
ml.speaker_apply_status(ml.speaker_initial_state(), {"models_ready": False, "reason": "x"})
ml.speaker_apply_removed(ml.speaker_initial_state(), 1, [])
ml.speaker_apply_download(ml.speaker_initial_state(), "done")
after = {name for name in heavy if name in sys.modules}
print(sorted(before), sorted(after))
"""
    result = subprocess.run(
        [sys.executable, "-c", code], cwd=ROOT, capture_output=True, text=True, check=True,
    )
    assert result.stdout.strip() == "[] []"
