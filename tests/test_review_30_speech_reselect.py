"""PR30 review finding 3: re-picking a speech model that failed to load.

When the speech model chosen in the Hub fails to load (download error, bad
cache), RuntimeModels keeps the current model and its lease - but the running
app's LiveSettings has already adopted the failed choice as its config. Picking
the same model again then does nothing: `set_speech` returns without saving
when the saved fields are equal, and even a save would not help because
`LiveSettings.apply` returns early when nothing differs from its config. The
user can never retry without picking another model first or restarting.

The full path runs here: the real Hub card actions (AST-extracted from
hub_mac, stub controls, as in test_models_change) save through the real
`save_config` to a temporary config file; the real `ConfigWatcher` notices it
and calls the real `LiveSettings.apply`; RuntimeModels loads through a fake
factory (no model, no download) with the fake lease from test_runtime_reload.
"""

from dataclasses import replace

import pytest

# Sibling test modules (pytest puts tests/ on sys.path).
from test_models_change import CARD, View, hub_class, namespace, popup_title
from test_runtime_reload import FakeTranscriber, Recorder, models

from localflow import app as app_module
from localflow import config as config_module
from localflow import speech_retry
from localflow.live_config import ConfigWatcher

WIDTH = 800


class Desk:
    """The Hub, the config file and the running dictation app, wired together."""

    def __init__(self, path, runtime):
        config_module.save_config(
            config_module.Config(stt_backend="faster-whisper", model_size="base"), path,
        )
        self.runtime = runtime
        # Re-picks reach the app as real retry requests beside `path`.
        self.live = app_module.LiveSettings(
            config_module.load_config(), Recorder(), models=runtime,
            retry_requests=speech_retry.RetryRequests(path),
        )
        self.watcher = ConfigWatcher(self.live.apply, path=path, on_tick=self.live.retry)
        ns = namespace([], [None], retry=speech_retry.request_retry)
        # The real config file, not the in-memory lists of test_models_change.
        ns.update({"load_config": config_module.load_config,
                   "save_config": config_module.save_config})
        self.hub = hub = hub_class(CARD, ns)()
        hub.tokens, hub.page, hub._closed, hub.shown = {}, "Models", False, []
        hub._viewport_height = lambda: 300
        hub._quick_fetch = lambda force=False: None
        hub.provider_state = {}
        hub._button = lambda *a, parent=None, **k: View()
        hub._field = lambda *a, **k: View()

        def show_page(page):
            hub.shown.append(page)
            hub.config = config_module.load_config()
            hub.body, hub.cursor_y = View(rect=(0, 0, WIDTH, 300)), 0
            hub._models_quick_card(WIDTH)

        hub.show_page = show_page
        show_page("Models")
        hub.shown.clear()

    def pick_size(self, label):
        """Choose a speech model size in the Hub, then one watcher interval."""
        sizes = self.hub.quick_controls["speech_model"]
        sizes.selectItemAtIndex_(sizes.items.index(label))
        self.hub.quickSpeechModel_(sizes)
        assert self.hub.quick_state["message"][1] is True  # the Hub reports success
        self.tick()

    def tick(self):
        # ConfigWatcher._run: check() then on_tick(), every interval.
        self.watcher.check()
        self.live.retry()

    def in_use(self):
        return self.runtime.snapshot()[0].name


@pytest.fixture
def config_file(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    return path


def factory(fail_once=()):
    """A speech-model factory that fails the first load of each named size."""
    built, failing = [], set(fail_once)

    def build(config):
        built.append((config.stt_backend, config.model_size))
        if config.model_size in failing:
            failing.discard(config.model_size)
            raise RuntimeError("download failed")
        return FakeTranscriber(config.model_size)

    return build, built


def test_reselecting_a_model_that_failed_to_load_loads_it_again(config_file):
    build, built = factory(fail_once={"small"})
    runtime, lease = models(build_stt_fn=build)
    desk = Desk(config_file, runtime)

    desk.pick_size("Small")
    assert built == [("faster-whisper", "small")]  # tried, and failed
    assert desk.in_use() == "base"  # the current model keeps working...
    assert not runtime.stt_pending
    assert lease.held() == []  # ...and the failed one holds no lease
    assert config_module.load_config().model_size == "small"  # the saved choice
    assert popup_title(desk.hub.quick_controls["speech_model"]) == "Small"

    # The user picks the very same (saved) model again to retry.
    desk.pick_size("Small")
    assert built == [("faster-whisper", "small")] * 2, (
        "re-picking the saved model after a failed load must load it again"
    )
    assert desk.in_use() == "small"
    assert lease.records[-1] == ("faster-whisper", "small", False)  # now the main lease
    assert lease.held() == [] and not runtime.stt_pending

    # Healthy now: picking it once more loads nothing.
    desk.pick_size("Small")
    desk.tick()
    assert len(built) == 2 and desk.in_use() == "small"


def test_a_healthy_model_is_never_reloaded_by_repicks_or_unrelated_edits(config_file):
    build, built = factory()
    runtime, lease = models(build_stt_fn=build)
    desk = Desk(config_file, runtime)

    desk.pick_size("Base")  # the model already in use
    desk.tick()
    assert built == [] and lease.records == []

    saved = config_module.load_config()
    config_module.save_config(replace(saved, sounds_enabled=not saved.sounds_enabled))
    desk.tick()
    assert desk.live.config.sounds_enabled is (not saved.sounds_enabled)  # adopted live
    assert built == [] and lease.records == [] and desk.in_use() == "base"

    desk.pick_size("Small")  # a real change still loads exactly once
    desk.pick_size("Small")
    assert built == [("faster-whisper", "small")] and desk.in_use() == "small"


def test_repicking_a_model_still_loading_or_waiting_never_downloads_it_twice(config_file):
    build, built = factory()
    started, busy = [], [True]
    runtime, _lease = models(
        build_stt_fn=build, start_thread=started.append, can_swap=lambda: not busy[0],
    )
    desk = Desk(config_file, runtime)

    desk.pick_size("Medium")
    assert len(started) == 1 and built == []  # loader queued, not yet run
    desk.pick_size("Medium")  # re-picked while it is loading
    assert len(started) == 1 and built == []

    started.pop()()  # the loader runs: the model is ready but dictation is busy
    assert built == [("faster-whisper", "medium")]
    assert runtime.stt_pending and desk.in_use() == "base"
    desk.pick_size("Medium")  # re-picked while it waits to be swapped in
    assert started == [] and built == [("faster-whisper", "medium")]

    busy[0] = False
    desk.tick()
    assert desk.in_use() == "medium" and not runtime.stt_pending
    assert started == [] and built == [("faster-whisper", "medium")]
