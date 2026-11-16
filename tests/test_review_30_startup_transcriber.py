"""PR30 review finding 1: app.main must not pin the startup speech model.

`main()` binds the startup model to a local (`transcriber`) and then blocks in
`listener.run_forever()` for the life of the process. Once RuntimeModels swaps
in a newly selected model, nothing in the app uses the startup one any more,
but that local keeps it (and its weights) alive until exit.

The real `main()` runs here with every external boundary faked (lock, logs,
config, model factories, recorder, controller, keyboard hub, watcher thread,
STT lease); the dictation worker thread is the real one. The checks run
inside the fake `run_forever`, i.e. while `main()`'s frame is still live, and
use weakrefs + gc, so they test reachability rather than source text. No model
is loaded and nothing touches the network, microphone or keyboard.
"""

import gc
import threading
import time
import weakref
from dataclasses import replace
from types import SimpleNamespace

import pytest

# Sibling test module (pytest puts tests/ on sys.path): the same lease fake.
from test_runtime_reload import FakeLease

from localflow import app as app_module
from localflow.config import Config
from localflow.jobs import Job
from localflow.stt_lease import selection_backend

WAIT = 5.0


class SpeechModel:
    """Stands in for a loaded Transcriber; weak-referenceable."""

    def __init__(self, name, backend="faster-whisper"):
        self.name, self.backend = name, backend


class Lease(FakeLease):
    selection_backend = staticmethod(selection_backend)


def _alive(ref):
    # Kept out of `assert` expressions so no rewritten temporary holds the object.
    return ref() is not None


def _collected(ref):
    gc.collect()
    return ref() is None


@pytest.fixture
def boot(monkeypatch):
    """Patch main()'s boundaries; `state.during(state)` runs inside run_forever."""
    state = SimpleNamespace(
        built=[], models=None, controller=None, on_change=None, on_tick=None,
        during=None, config=Config(stt_backend="faster-whisper", model_size="base"),
        lease=Lease(), stopped=False,
    )

    def build_stt(config):
        made = SpeechModel(config.model_size)
        state.built.append(weakref.ref(made))  # weak only: the test never pins it
        return made

    real_models = app_module.RuntimeModels

    def runtime_models(*args, **kwargs):
        kwargs["lease"] = state.lease
        kwargs["start_thread"] = lambda target: target()  # load inline, no thread
        state.models = real_models(*args, **kwargs)
        return state.models

    class Controller:
        def __init__(self, recorder, config, work_queue, **_kwargs):
            self.work_queue = work_queue
            self.results = []
            state.controller = self

        def is_idle(self):
            return True

        def update_config(self, _config):
            pass

        def shutdown(self):
            pass

        def note_job_result(self, text, *_args, **_kwargs):
            self.results.append(text)

        def __getattr__(self, name):
            if name.startswith("note_"):
                return lambda *args, **kwargs: None
            raise AttributeError(name)

    class Watcher:
        def __init__(self, on_change, *, on_tick=None, **_kwargs):
            state.on_change, state.on_tick = on_change, on_tick

        def start(self):
            return self

        def stop(self):
            state.stopped = True

    class Hub:
        reconfigure_pending = False

        def __init__(self, **_kwargs):
            pass

        def reconfigure(self, **_kwargs):
            return True

        def apply_pending(self):
            pass

        def run_forever(self):
            state.during(state)

    patches = {
        "acquire": lambda name: object(),
        "read_holder": lambda name: (None, None),
        "prepare_process_logs": lambda: None,
        "setup_logging": lambda: "localflow.log",
        "load_config": lambda: state.config,
        "ConfigWatcher": Watcher,
        "build_stt": build_stt,
        "build_llm": lambda config: SimpleNamespace(model=config.llm_model),
        "stt_lease": state.lease,
        "discover_commands": lambda *a, **k: [],
        "HotPhraseStore": lambda: SimpleNamespace(enabled=list),
        "Recorder": lambda **kwargs: SimpleNamespace(),
        "DictionaryStore": lambda: SimpleNamespace(),
        "HistoryStore": lambda enabled: SimpleNamespace(enabled=False),
        "write_state": lambda *a, **k: None,
        "write_last_text": lambda *a, **k: None,
        "PushToTalkController": Controller,
        "KeyboardHub": Hub,
        "RuntimeModels": runtime_models,
    }
    for name, value in patches.items():
        monkeypatch.setattr(app_module, name, value)
    return state


def run_main(state):
    with pytest.raises(SystemExit) as exited:
        app_module.main()
    assert exited.value.code == 1  # run_forever returned: the listener "died"
    assert state.stopped


def test_startup_model_is_released_once_a_new_speech_model_is_swapped_in(boot):
    def during(state):
        startup = state.built[0]
        assert _alive(startup)
        assert state.models.snapshot()[0].name == "base"

        # A speech-model change lands while the listener runs; nothing is busy.
        state.on_change(replace(state.config, model_size="small"))
        assert state.models.snapshot()[0].name == "small"
        assert not state.models.stt_pending
        assert len(state.built) == 2

        # No job keeps a snapshot of the startup model and RuntimeModels no
        # longer references it: it must be collectable while main() still runs.
        assert _collected(startup), (
            "main() still references the startup transcriber after the swap"
        )
        assert _alive(state.built[1])  # the model in use is of course kept

    boot.during = during
    run_main(boot)


def test_a_job_in_flight_keeps_the_startup_model_until_it_ends(boot, monkeypatch):
    started, release = threading.Event(), threading.Event()
    seen = []

    def process_job(item, config, transcriber, *_rest):
        seen.append(weakref.ref(transcriber))  # what this job's snapshot holds
        started.set()
        if not release.wait(WAIT):
            raise RuntimeError("test never released the job")
        return "done", False

    monkeypatch.setattr(app_module, "_process_job", process_job)

    def during(state):
        startup = state.built[0]
        work_queue = state.controller.work_queue
        try:
            work_queue.put(Job(kind="dictate"))
            assert started.wait(WAIT)
            assert seen[0]() is startup()  # the real worker took the startup snapshot

            # The change lands mid-job: the new model loads but must wait.
            state.on_change(replace(state.config, model_size="small"))
            assert state.models.stt_pending
            assert state.models.snapshot()[0].name == "base"
            assert _alive(startup) and not _collected(startup)  # the job still uses it
        finally:
            release.set()

        deadline = time.monotonic() + WAIT
        while work_queue.unfinished_tasks and time.monotonic() < deadline:
            time.sleep(0.01)
        assert work_queue.unfinished_tasks == 0
        assert state.controller.results == ["done"]

        state.on_tick()  # the watcher tick retries the deferred swap
        assert state.models.snapshot()[0].name == "small"
        assert not state.models.stt_pending
        assert _collected(startup), (
            "the job ended and the model was swapped, but main() still "
            "references the startup transcriber"
        )

    boot.during = during
    run_main(boot)
