"""Model changes reach running processes without a restart, safely.

The LLM group (provider, endpoint, model, key ref) and the speech group are
adopted whole; the dictation app swaps clients between jobs and loads a new
speech model off-thread; the server rebuilds its LLM clients at once and
waits for a running meeting before dropping its speech model.
"""

from dataclasses import replace
from types import SimpleNamespace

import pytest

from localflow import app as app_module
from localflow import live_config
from localflow.config import Config
from localflow.stt_lease import selection_backend


class Recorder:
    def __init__(self):
        self.configs = []

    def update_config(self, config):
        self.configs.append(config)


# ------------------------------------------------------------- groups ---


def test_model_groups_move_whole_never_field_by_field():
    running = Config(llm_provider="openai-compatible", llm_base_url="https://a.example/v1",
                     llm_model="a", llm_key_ref="")
    fresh = replace(running, llm_base_url="https://b.example/v1", llm_model="b")
    assert live_config.runtime_changes(running, fresh) == {"llm"}
    adopted = live_config.adopt_groups(running, fresh, {"llm"})
    for name in live_config.LLM_FIELDS:
        assert getattr(adopted, name) == getattr(fresh, name)
    assert adopted.stt_backend == running.stt_backend
    assert live_config.runtime_changes(running, running) == frozenset()
    speech = replace(running, stt_backend="parakeet")
    assert live_config.runtime_changes(running, speech) == {"stt"}
    # The allowlist merge itself is unchanged: models are not "live fields".
    merged, changed = live_config.merge_live(running, fresh)
    assert changed == frozenset() and merged.llm_model == "a"


# -------------------------------------------------------- dictation app ---


class FakeTranscriber:
    def __init__(self, name, backend="faster-whisper"):
        self.name, self.backend = name, backend


class FakeLease:
    def __init__(self):
        self.records, self.released = [], []

    def record(self, backend, model, *, scoped=False):
        path = f"{backend}:{model}:{'scoped' if scoped else 'main'}:{len(self.records)}"
        self.records.append((backend, model, scoped))
        return path

    def release(self, path):
        self.released.append(path)

    def held(self):
        """Scoped leases recorded and not yet released."""
        return [
            path for index, (backend, model, scoped) in enumerate(self.records)
            if scoped and (path := f"{backend}:{model}:scoped:{index}") not in self.released
        ]


def models(**overrides):
    lease = FakeLease()
    options = {
        "can_swap": lambda: True,
        "build_stt_fn": lambda config: FakeTranscriber(config.model_size),
        "build_llm_fn": lambda config: SimpleNamespace(model=config.llm_model),
        "lease": lease,
        "start_thread": lambda target: target(),  # run the loader inline
    }
    options.update(overrides)
    return app_module.RuntimeModels(
        FakeTranscriber("base"), "old-cleaner", "old-transformer", **options
    ), lease


def test_llm_reload_swaps_clients_but_a_running_job_keeps_its_snapshot():
    runtime, _lease = models()
    job = runtime.snapshot()
    runtime.reload_llm(Config(llm_model="qwen3:4b"))
    assert job[1] == "old-cleaner"  # the job in progress is untouched
    _t, cleaner, transformer = runtime.snapshot()
    assert cleaner.llm.model == "qwen3:4b" and transformer.llm.model == "qwen3:4b"


def test_speech_model_waits_while_busy_and_keeps_both_leased():
    busy = [True]
    runtime, lease = models(can_swap=lambda: not busy[0])
    runtime.reload_stt(Config(model_size="small"))
    assert runtime.snapshot()[0].name == "base"  # still recording: no swap
    assert runtime.stt_pending
    # Selection lease first (auto = unknown: protects every cached model),
    # then handed over to a lease naming what actually loaded.
    assert lease.records == [("unknown", "small", True), ("faster-whisper", "small", True)]
    assert lease.released == ["unknown:small:scoped:0"]
    assert lease.held() == ["faster-whisper:small:scoped:1"]  # ready model protected
    assert runtime.retry() is False

    busy[0] = False
    assert runtime.retry() is True
    assert runtime.snapshot()[0].name == "small"
    assert lease.records[-1] == ("faster-whisper", "small", False)  # now the main lease
    assert lease.held() == []
    assert not runtime.stt_pending
    assert runtime.retry() is False  # nothing left to swap


def test_the_model_is_protected_before_and_while_it_loads():
    seen = []

    def build(config):
        seen.append(list(lease.held()))
        return FakeTranscriber(config.model_size, backend="mlx")

    runtime, lease = models(build_stt_fn=build)
    runtime.reload_stt(Config(stt_backend="mlx-whisper", model_size="small"))
    assert seen == [["mlx:small:scoped:0"]]  # held before the download/load began
    assert runtime.snapshot()[0].name == "small"


def test_failed_speech_load_keeps_the_old_model_and_releases_its_lease():
    def boom(_config):
        raise RuntimeError("download failed")

    runtime, lease = models(build_stt_fn=boom)
    runtime.reload_stt(Config(stt_backend="parakeet", model_size="base"))
    assert runtime.snapshot()[0].name == "base"
    assert lease.records == [("parakeet", "base", True)]
    assert lease.held() == [] and not runtime.stt_pending


def test_a_superseded_ready_model_keeps_protection_until_replaced():
    busy = [True]
    runtime, lease = models(can_swap=lambda: not busy[0])
    runtime.reload_stt(Config(stt_backend="faster-whisper", model_size="small"))
    first_ready = lease.held()
    assert first_ready == ["faster-whisper:small:scoped:1"]
    runtime.reload_stt(Config(stt_backend="faster-whisper", model_size="medium"))
    # The newer one is ready and protected; only then is the older released.
    assert lease.held() == ["faster-whisper:medium:scoped:3"]
    assert first_ready[0] in lease.released
    busy[0] = False
    assert runtime.retry() and runtime.snapshot()[0].name == "medium"


def test_the_newest_speech_choice_wins_and_loads_once():
    started, built = [], []
    runtime, _lease = models(
        start_thread=started.append,
        build_stt_fn=lambda config: built.append(config.model_size) or FakeTranscriber(
            config.model_size),
    )
    runtime.reload_stt(Config(model_size="small"))
    runtime.reload_stt(Config(model_size="medium"))  # changed again before loading
    assert len(started) == 1  # one loader thread
    started[0]()
    assert built == ["medium"]
    assert runtime.snapshot()[0].name == "medium"


def test_live_settings_adopts_the_group_and_reloads_off_the_watcher():
    calls = []
    fake = SimpleNamespace(
        reload_llm=lambda config: calls.append(("llm", config.llm_model, config.llm_key_ref)),
        reload_stt=lambda config: calls.append(("stt", config.stt_backend, config.model_size)),
        retry=lambda: calls.append(("retry",)),
    )
    controller = Recorder()
    live = app_module.LiveSettings(Config(), controller, models=fake)
    changed = live.apply(Config(llm_model="gemma4:e4b", sounds_enabled=False))
    assert changed == {"llm_model", "sounds_enabled"}
    assert calls == [("llm", "gemma4:e4b", "")]
    assert live.config.llm_model == "gemma4:e4b" and live.config.sounds_enabled is False
    assert controller.configs[-1].llm_model == "gemma4:e4b"

    live.apply(replace(live.config, stt_backend="parakeet", model_size="base"))
    assert calls[-1] == ("stt", "parakeet", "base")
    live.retry()
    assert calls[-1] == ("retry",)
    assert live.apply(live.config) == frozenset()  # nothing moved: nothing reloads


def test_worker_takes_one_model_snapshot_per_job():
    with open(app_module.__file__, encoding="utf-8") as handle:
        source = handle.read()
    process = source.split("def process_item(item):", 1)[1].split("\n    def ", 1)[0]
    assert "models.snapshot()" in process
    assert "work_queue.unfinished_tasks == 0" in source and "controller.is_idle()" in source


# ---------------------------------------------------------------- server ---


@pytest.fixture
def server_state(monkeypatch):
    from localflow import server

    built = []

    def fake_build_llm(config, **kwargs):
        built.append((config.llm_model, kwargs))
        return SimpleNamespace(model=config.llm_model, **kwargs)

    monkeypatch.setattr(server, "build_llm", fake_build_llm)
    # Restored after each test: the reload replaces these module clients.
    monkeypatch.setattr(server, "_ollama", server._ollama)
    monkeypatch.setattr(server, "_summarizer", server._summarizer)
    monkeypatch.setattr(server, "_config", Config())
    monkeypatch.setattr(server, "_transcriber", "old-transcriber")
    monkeypatch.setattr(server, "_cleaner", "old-cleaner")
    monkeypatch.setattr(server, "_stt_reload_pending", False)
    monkeypatch.setattr(server, "_session_state", "idle")
    return server, built


def test_server_rebuilds_llm_clients_for_a_new_cleanup_model(server_state):
    server, built = server_state
    server._apply_live_config(Config(llm_model="gemma4:e4b"))
    assert server._config.llm_model == "gemma4:e4b"
    assert server._ollama.model == "gemma4:e4b"
    assert server._summarizer.llm.model == "gemma4:e4b"  # summaries and titles
    assert server._cleaner is None  # rebuilt lazily from the new client
    assert server._transcriber == "old-transcriber"  # speech untouched
    assert [model for model, _kw in built] == ["gemma4:e4b", "gemma4:e4b"]


def test_server_never_swaps_speech_mid_meeting(server_state):
    server, _built = server_state
    server._session_state = "running"
    server._apply_live_config(Config(stt_backend="parakeet"))
    assert server._config.stt_backend == "parakeet"
    assert server._transcriber == "old-transcriber"  # the meeting keeps its model
    assert server._stt_reload_pending is True
    assert server._release_stale_transcriber() is False  # still running

    server._session_state = "idle"
    assert server._release_stale_transcriber() is True
    assert server._transcriber is None  # next use loads parakeet (and its lease)
    assert server._stt_reload_pending is False


def test_server_idle_speech_change_applies_at_once(server_state):
    server, _built = server_state
    server._apply_live_config(Config(model_size="small"))
    assert server._transcriber is None and server._stt_reload_pending is False


class ServerLease(FakeLease):
    selection_backend = staticmethod(selection_backend)


@pytest.mark.parametrize("backend,expected,known", [
    ("auto", "unknown", False), ("my-plugin", "unknown", False), (None, "unknown", False),
    ("parakeet", "parakeet", True), ("mlx-whisper", "mlx", True), ("mlx", "mlx", True),
    ("faster-whisper", "faster-whisper", True),
])
def test_selection_leases_are_conservative_for_auto_and_plugins(backend, expected, known):
    from localflow.stt_lease import runtime_repo

    assert selection_backend(backend) == expected
    assert runtime_repo(expected, "base")[1] is known  # unknown => nothing deletable


def _real_kind_transcriber(model_size, backend="faster-whisper"):
    """A Transcriber instance without loading anything (fields only)."""
    from localflow.stt import Transcriber

    transcriber = Transcriber.__new__(Transcriber)
    transcriber.backend, transcriber.model_size, transcriber.language = (
        backend, model_size, None)
    return transcriber


def test_server_protects_a_model_before_it_loads(server_state, monkeypatch):
    server, _built = server_state
    lease = ServerLease()
    monkeypatch.setattr(server, "stt_lease", lease)
    monkeypatch.setattr(server, "_transcriber", None)
    monkeypatch.setattr(server, "_config", Config(stt_backend="auto", model_size="small"))
    seen = []

    def build(config):
        seen.append(list(lease.held()))
        return _real_kind_transcriber(config.model_size)

    monkeypatch.setattr(server, "build_stt", build)
    loaded = server._get_transcriber()
    assert seen == [["unknown:small:scoped:0"]]  # auto: every cache protected meanwhile
    assert lease.records[-1] == ("faster-whisper", "small", False)
    assert lease.held() == [] and server._get_transcriber() is loaded


def test_server_leases_what_loaded_even_if_the_config_changes_mid_load(
    server_state, monkeypatch,
):
    server, _built = server_state
    lease = ServerLease()
    monkeypatch.setattr(server, "stt_lease", lease)
    monkeypatch.setattr(server, "_transcriber", None)
    monkeypatch.setattr(server, "_config", Config(stt_backend="faster-whisper",
                                                  model_size="small"))
    built_from = []

    def slow_build(config):
        built_from.append(config.model_size)
        # A reload lands while the model is still loading.
        server._config = Config(stt_backend="parakeet", model_size="large-v3")
        return _real_kind_transcriber(config.model_size)

    monkeypatch.setattr(server, "build_stt", slow_build)
    server._get_transcriber()
    assert built_from == ["small"]
    assert lease.records == [
        ("faster-whisper", "small", True),   # selection: the config being loaded
        ("faster-whisper", "small", False),  # what actually loaded, not large-v3
    ]
    assert lease.held() == []


def test_server_reload_cannot_retire_a_model_mid_transcription(server_state, monkeypatch):
    server, _built = server_state
    lease = ServerLease()
    monkeypatch.setattr(server, "stt_lease", lease)
    old = _real_kind_transcriber("base")
    monkeypatch.setattr(server, "_transcriber", old)
    during = []

    with server._borrowed_transcriber() as transcriber:
        assert transcriber is old
        # A speech-model change lands while the phone upload is transcribing.
        server._apply_live_config(Config(model_size="small"))
        during.append((server._transcriber, list(lease.held())))
    assert during == [(None, ["faster-whisper:base:scoped:0"])]  # old one still leased
    assert lease.held() == []  # released once the transcription returned


def test_borrowing_a_stub_transcriber_writes_no_lease(server_state, monkeypatch):
    server, _built = server_state
    lease = ServerLease()
    monkeypatch.setattr(server, "stt_lease", lease)
    monkeypatch.setattr(server, "_get_transcriber", lambda: "stub")
    with server._borrowed_transcriber() as transcriber:
        assert transcriber == "stub"
    assert lease.records == []


def test_meeting_stop_releases_a_waiting_speech_change():
    from localflow import server

    with open(server.__file__, encoding="utf-8") as handle:
        source = handle.read()
    stop = source.split("async def meeting_stop", 1)[1].split("\n@app.", 1)[0]
    finally_block = stop.split("finally:", 1)[1]
    assert finally_block.index('_session_state = "idle"') < finally_block.index(
        "_release_stale_transcriber()"
    )
    assert "on_tick=_release_stale_transcriber" in source
