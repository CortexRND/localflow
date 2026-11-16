"""PR30 review finding 4: only an explicit re-pick retries a failed speech model.

Finding 3 made a failed speech model loadable again by re-picking it, but the
running app inferred that request from any config save that changed nothing
live: `merge_live` ignores restart-only edits, so saving e.g. `server_port`
(or re-saving identical contents) reached `LiveSettings.apply` as "nothing
changed" and loaded the failed model again although nobody asked for it.

Now the Hub's re-pick (`models_layout.set_speech` with the saved selection)
writes a config-scoped request (speech_retry) and the app's watcher tick
takes it once (`LiveSettings.retry`); config saves never retry. The full path
runs through the real Hub card, `save_config`, `ConfigWatcher`, LiveSettings
and RuntimeModels (the Desk of test_review_30_speech_reselect, real requests).
"""

import json
from dataclasses import replace

import pytest

# Sibling test modules (pytest puts tests/ on sys.path).
from test_models_change import available
from test_review_30_speech_reselect import Desk
from test_runtime_reload import FakeTranscriber, Recorder, models

from localflow import app as app_module
from localflow import config as config_module
from localflow import models_layout as ml
from localflow import speech_retry
from localflow.config import Config
from localflow.live_config import STT_FIELDS


@pytest.fixture
def config_file(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    return path


def factory(failures):
    """A speech-model factory failing the first `failures[size]` loads of a size."""
    built, left = [], dict(failures)

    def build(config):
        built.append(config.model_size)
        if left.get(config.model_size, 0) > 0:
            left[config.model_size] -= 1
            raise RuntimeError("download failed")
        return FakeTranscriber(config.model_size)

    return build, built


def failed_small(config_file, failures=1):
    """A Desk whose first pick of Small failed to load (base still serving)."""
    build, built = factory({"small": failures})
    runtime, lease = models(build_stt_fn=build)
    desk = Desk(config_file, runtime)
    desk.pick_size("Small")
    assert built == ["small"] and desk.in_use() == "base"
    return desk, built, lease


def test_a_restart_only_or_unchanged_save_never_retries_a_failed_model(config_file):
    desk, built, lease = failed_small(config_file)
    # The changed choice loaded from the config change alone: no request made.
    assert not speech_retry.request_path(config_file).exists()

    saved = config_module.load_config()
    config_module.save_config(replace(saved, server_port=saved.server_port + 1))
    desk.tick()
    assert built == ["small"], "an unrelated restart-only save retried the failed model"
    config_module.save_config(config_module.load_config())  # identical contents
    desk.tick()
    for _ in range(3):  # and nothing retries on its own
        desk.tick()
    assert built == ["small"] and desk.in_use() == "base"
    assert lease.held() == [] and not desk.runtime.stt_pending

    desk.pick_size("Small")  # the explicit re-pick still retries, once
    assert built == ["small", "small"] and desk.in_use() == "small"
    desk.tick()
    desk.pick_size("Small")  # healthy now: a re-pick loads nothing
    assert built == ["small", "small"]


def test_each_explicit_repick_retries_once_and_ticks_never_do(config_file):
    desk, built, _lease = failed_small(config_file, failures=2)
    first = speech_retry.request_path(config_file)

    desk.pick_size("Small")  # retried and failed again
    assert built == ["small"] * 2 and desk.in_use() == "base"
    request = first.read_text(encoding="utf-8")
    for _ in range(3):
        desk.tick()
    assert built == ["small"] * 2  # the taken request is not retried again

    desk.pick_size("Small")  # identical choice: still a new request
    assert first.read_text(encoding="utf-8") != request
    assert built == ["small"] * 3 and desk.in_use() == "small"


def test_a_stale_or_different_request_never_retries(config_file):
    desk, built, _lease = failed_small(config_file)
    small = config_module.load_config()

    # A request left by an earlier process: the app's new reader ignores it.
    speech_retry.request_retry(small)
    desk.live.retry_requests = speech_retry.RetryRequests(config_file)
    desk.tick()
    assert built == ["small"]

    # A request for another selection than the one the app wants now.
    speech_retry.request_retry(replace(small, model_size="medium"))
    desk.tick()
    speech_retry.request_retry(replace(small, language="de"))
    desk.tick()
    assert built == ["small"] and desk.in_use() == "base"

    desk.pick_size("Small")
    assert built == ["small", "small"] and desk.in_use() == "small"


def test_an_old_request_does_not_retry_a_newer_failed_choice(config_file):
    build, built = factory({"small": 1, "medium": 1})
    runtime, _lease = models(build_stt_fn=build)
    desk = Desk(config_file, runtime)
    desk.pick_size("Small")
    speech_retry.request_retry(config_module.load_config())  # a re-pick of Small...
    desk.pick_size("Medium")  # ...overtaken by a newer choice that fails too
    assert built == ["small", "medium"] and desk.in_use() == "base"
    for _ in range(2):
        desk.tick()
    assert built == ["small", "medium"]
    desk.pick_size("Medium")
    assert built == ["small", "medium", "medium"] and desk.in_use() == "medium"


def test_set_speech_requests_only_after_saving_the_same_selection():
    retries, saved = [], []
    config = replace(Config(), stt_backend="faster-whisper", model_size="base")
    changed = ml.set_speech("faster-whisper", "small", config,
                            available=available("faster-whisper"), save=saved.append,
                            retry=retries.append)
    assert changed.model_size == "small" and retries == []

    same = ml.set_speech("faster-whisper", "base", config,
                         available=available("faster-whisper"), save=saved.append,
                         retry=retries.append)
    assert retries == [same] and saved[-1] == same

    def broken(_config):
        raise OSError("read-only")

    with pytest.raises(OSError):
        ml.set_speech("faster-whisper", "base", config,
                      available=available("faster-whisper"), save=broken,
                      retry=retries.append)
    assert retries == [same]  # a failed save asks for nothing


def test_live_settings_survives_a_broken_reader_and_ignores_saves():
    calls = []

    class Fake:
        def retry_failed_stt(self, config):
            calls.append(config.model_size)
            return True

        def retry(self):
            return False

    class Broken:
        def take(self):
            raise RuntimeError("bad reader")

    live = app_module.LiveSettings(Config(), Recorder(), models=Fake(),
                                   retry_requests=Broken())
    live.retry()
    assert live.apply(replace(Config(), server_port=Config().server_port + 1)) == frozenset()
    assert live.apply(Config()) == frozenset()
    live.retry_requests = None
    live.retry()
    assert calls == []


# ---------------------------------------------------------------- helper ---


def test_requests_are_scoped_to_the_config_file_and_hold_no_secrets(tmp_path):
    a, b = tmp_path / "a" / "config.toml", tmp_path / "b" / "config.toml"
    reader_a, reader_b = speech_retry.RetryRequests(a), speech_retry.RetryRequests(b)
    config = replace(Config(), model_size="small", llm_key_ref="keychain:secret-ref")
    path = speech_retry.request_retry(config, a)
    assert path == speech_retry.request_path(a) and path.parent == a.parent
    data = json.loads(path.read_text(encoding="utf-8"))
    assert set(data) == {"version", "id", "selection"} and set(data["selection"]) == set(
        STT_FIELDS)
    assert "secret-ref" not in path.read_text(encoding="utf-8")
    assert reader_b.take() is None
    assert reader_a.take() == tuple(getattr(config, name) for name in STT_FIELDS)
    assert reader_a.take() is None  # taken once
    speech_retry.request_retry(config, a)  # the same choice again: a new request
    assert reader_a.take() == tuple(getattr(config, name) for name in STT_FIELDS)
    assert list(a.parent.glob("*.tmp")) == []


def test_reader_follows_the_config_path_callable(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    reader = speech_retry.RetryRequests(config_module.config_path)
    speech_retry.request_retry(Config())
    assert speech_retry.request_path(path).exists()
    assert reader.take() == tuple(getattr(Config(), name) for name in STT_FIELDS)


@pytest.mark.parametrize("content", [
    b"", b"not json", b"[]", b"{}", b'{"version": 2, "id": "x", "selection": {}}',
    b'{"version": 1, "id": "", "selection": {}}',
    b'{"version": 1, "id": "x", "selection": {"model_size": "small"}}',
    (b'{"version": 1, "id": "x", "selection": {"stt_backend": 1, "model_size": "small",'
     b' "language": null, "stt_device": "auto"}}'),
    b"\xff\xfe", b"{" + b" " * speech_retry.MAX_BYTES + b"}",
])
def test_empty_or_malformed_requests_are_ignored(tmp_path, content):
    config = tmp_path / "config.toml"
    reader = speech_retry.RetryRequests(config)
    speech_retry.request_path(config).write_bytes(content)
    assert reader.take() is None
    assert speech_retry.RetryRequests(config).take() is None


def test_a_missing_request_or_folder_is_nothing(tmp_path):
    assert speech_retry.RetryRequests(tmp_path / "nowhere" / "config.toml").take() is None
