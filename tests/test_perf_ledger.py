"""The local perf ledger (localflow/perf_ledger.py): schema, rotation,
corrupt-line tolerance, the perf_metrics switch, privacy, and the hooks.
Cross-runtime reading/writing is covered in tests/test_rust_parity.py."""

from __future__ import annotations

import json
from dataclasses import replace
from types import SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest

from localflow import perf_ledger
from localflow.config import (
    Config,
    ConfigError,
    _config_from_v2,
    apply_config_update,
    config_to_dict,
    config_toml,
)
from localflow.perf_ledger import (
    FIELDS,
    KINDS,
    MAX_BYTES,
    LlmLabels,
    PerfEvent,
    PerfLedger,
    SttLabels,
    default_ledger_path,
    parse_line,
)

TS = "2026-11-24T10:00:00.000Z"
CANONICAL = (
    '{"ts":"2026-11-24T10:00:00.000Z","runtime":"python","version":"0.2.2",'
    '"kind":"dictate","status":"ok","audio_s":3.42,"queue_ms":1.2,"stt_ms":145.0,'
    '"llm_ms":0.0,"render_ms":0.4,"paste_ms":null,"total_ms":154.7,'
    '"stt":{"backend":"faster-whisper","model":"base","provider":"cpu"},'
    '"llm":{"provider":"ollama","model":"llama3.2:3b"},"chars":42}'
)
SECRET = "my secret transcript about the acquisition"


def _dictation(**overrides) -> PerfEvent:
    values = {
        "kind": "dictate",
        "audio_s": 3.42,
        "queue_ms": 1.2,
        "stt_ms": 145.0,
        "llm_ms": 0.0,
        "render_ms": 0.4,
        "total_ms": 154.7,
        "stt": SttLabels("faster-whisper", "base", "cpu"),
        "llm": LlmLabels("ollama", "llama3.2:3b"),
        "chars": 42,
        "ts": TS,
        "version": "0.2.2",
        **overrides,
    }
    return PerfEvent(**values)


@pytest.fixture
def ledger(tmp_path) -> PerfLedger:
    return PerfLedger(tmp_path / "perf" / "events.jsonl")


# --- schema -----------------------------------------------------------------


def test_canonical_line_has_every_key_in_order_and_null_for_absent_values():
    assert _dictation().to_line() == CANONICAL
    bare = json.loads(PerfEvent("startup", status="error").to_line())
    assert tuple(bare) == FIELDS
    assert bare["stt"] is None and bare["llm"] is None and bare["chars"] is None
    assert bare["runtime"] == "python"
    assert bare["status"] == "error"
    assert perf_ledger._TS.fullmatch(bare["ts"])


def test_every_kind_round_trips(ledger):
    written = [replace(_dictation(), kind=kind) for kind in KINDS]
    for event in written:
        assert ledger.record(event)
    assert ledger.read_events() == written
    assert KINDS == (
        "dictate", "command", "transform", "meeting_summarize", "meeting_chat",
        "recall", "stt_load", "llm_warmup", "startup",
    )


def test_record_builds_an_event_from_field_values(ledger):
    assert ledger.record("recall", total_ms=12.5, status="error")
    (event,) = ledger.read_events()
    assert (event.kind, event.status, event.total_ms) == ("recall", "error", 12.5)
    assert event.version == perf_ledger.runtime_version()


def test_unknown_kind_status_or_runtime_is_rejected():
    for bad in ({"kind": "teleport"}, {"status": "silent"}, {"runtime": "go"}, {"ts": "now"}):
        with pytest.raises(ValueError):
            _dictation(**bad)


def test_numbers_are_rounded_and_invalid_ones_become_null():
    event = _dictation(
        stt_ms=1.23456, llm_ms=-1.0, total_ms=float("nan"),
        render_ms=float("inf"), queue_ms=True, chars=-3,
    )
    assert event.stt_ms == 1.235
    # Ties round away from zero, exactly as Rust's f64::round does.
    assert _dictation(stt_ms=1.0005, llm_ms=0.0005, total_ms=-0.0).stt_ms == 1.001
    assert _dictation(llm_ms=0.0005).llm_ms == 0.001
    assert _dictation(total_ms=-0.0).to_line().count('"total_ms":0.0') == 1
    assert _dictation(total_ms=10**400).total_ms is None
    assert event.llm_ms is None and event.total_ms is None
    assert event.render_ms is None and event.queue_ms is None
    assert event.chars is None


def test_max_bytes_and_default_path(monkeypatch, tmp_path):
    assert MAX_BYTES == 5 * 1024 * 1024
    assert PerfLedger(tmp_path / "x").max_bytes == MAX_BYTES
    monkeypatch.setenv("LOCALFLOW_PERF_LEDGER", str(tmp_path / "custom.jsonl"))
    assert default_ledger_path() == tmp_path / "custom.jsonl"
    monkeypatch.delenv("LOCALFLOW_PERF_LEDGER")
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("USERPROFILE", str(tmp_path))
    assert default_ledger_path() == tmp_path / ".localflow" / "perf" / "events.jsonl"


# --- rotation ---------------------------------------------------------------


def test_rotates_at_max_bytes_keeping_one_previous_file(tmp_path):
    line_len = len(_dictation().to_line()) + 1
    ledger = PerfLedger(tmp_path / "events.jsonl", max_bytes=line_len * 3)
    for _ in range(3):
        ledger.record(_dictation())
    assert not ledger.previous_path.exists()
    ledger.record(_dictation())
    assert ledger.previous_path == tmp_path / "events.jsonl.1"
    assert ledger.previous_path.stat().st_size == line_len * 3
    assert ledger.path.stat().st_size == line_len
    assert len(ledger.read_events()) == 4
    for _ in range(3):
        ledger.record(_dictation())
    # The second rotation replaces the first previous file.
    assert len(ledger.read_events()) == 4


def test_read_returns_older_file_first(ledger):
    ledger.path.parent.mkdir(parents=True)
    ledger.previous_path.write_text(_dictation(kind="stt_load").to_line() + "\n")
    ledger.record(_dictation(kind="recall"))
    assert [e.kind for e in ledger.read_events()] == ["stt_load", "recall"]


def test_clear_removes_both_files(tmp_path):
    ledger = PerfLedger(tmp_path / "events.jsonl", max_bytes=1)
    ledger.record(_dictation())
    ledger.record(_dictation())
    assert ledger.previous_path.exists()
    ledger.clear()
    assert not ledger.path.exists() and not ledger.previous_path.exists()
    ledger.clear()
    assert ledger.read_events() == []


# --- corrupt input ----------------------------------------------------------


def test_corrupt_and_truncated_lines_are_skipped(ledger):
    good = _dictation().to_line()
    ledger.path.parent.mkdir(parents=True)
    ledger.path.write_text(
        "not json\n\n[1,2]\n" + good + "\n"
        + '{"ts":"2026-11-24T10:00:00.000Z","runtime":"rust","version":"0.2.2",'
        '"kind":"teleport","status":"ok"}\n'
        + '{"ts":"yesterday","runtime":"rust","version":"0.2.2","kind":"recall","status":"ok"}\n'
        + good[: len(good) // 2]
    )
    ledger.previous_path.write_bytes(b"\xff\xfe garbage\n")
    assert ledger.read_events() == [_dictation()]


def test_missing_and_unknown_keys_are_tolerated():
    event = parse_line(json.dumps({
        "ts": TS, "runtime": "rust", "version": "0.2.2", "kind": "recall",
        "status": "ok", "total_ms": 12.5, "chars": "x", "stt_ms": "fast",
        "text": SECRET, "llm": {"provider": "ollama"},
    }))
    assert event is not None
    assert event.runtime == "rust" and event.total_ms == 12.5
    assert event.stt_ms is None and event.chars is None
    assert event.llm == LlmLabels("ollama", "")
    assert "secret" not in event.to_line()


# --- perf_metrics -----------------------------------------------------------


def test_perf_metrics_is_a_features_flag_defaulting_on():
    assert Config().perf_metrics is True
    assert config_to_dict(Config())["features"]["perf_metrics"] is True
    assert "[features]\n" in config_toml(Config())
    assert "perf_metrics = true\n" in config_toml(Config())
    off = apply_config_update(Config(), {"features": {"perf_metrics": False}})
    assert off.perf_metrics is False
    with pytest.raises(ConfigError):
        apply_config_update(Config(), {"features": {"perf_metrics": "no"}})
    loaded = _config_from_v2({"config_version": 2, "features": {"perf_metrics": 0}})
    assert loaded.perf_metrics is True  # wrong type falls back to the default


def test_disabled_ledger_writes_nothing(tmp_path):
    ledger = PerfLedger(tmp_path / "perf" / "events.jsonl", enabled=False)
    assert ledger.record(_dictation()) is False
    assert not (tmp_path / "perf").exists()
    config = replace(Config(), perf_metrics=False)
    assert PerfLedger.for_config(config, tmp_path / "x.jsonl").enabled is False
    assert perf_ledger.record_for(config, "recall", total_ms=1.0) is False
    assert not default_ledger_path().exists()
    assert perf_ledger.record_for(Config(), "recall", total_ms=1.0) is True
    assert len(PerfLedger().read_events()) == 1


def test_unwritable_ledger_never_raises(tmp_path):
    blocker = tmp_path / "file"
    blocker.write_text("x")
    assert PerfLedger(blocker / "perf" / "events.jsonl").record(_dictation()) is False


# --- privacy ----------------------------------------------------------------


@pytest.mark.parametrize("field", ["text", "raw_text", "final_text", "prompt",
                                   "app_name", "bundle_id", "window_title", "url"])
def test_no_text_fields_are_accepted(ledger, field):
    with pytest.raises(TypeError):
        PerfEvent(kind="dictate", **{field: SECRET})
    assert ledger.record("dictate", **{field: SECRET}) is False
    assert not ledger.path.exists()


@pytest.mark.parametrize("value", [
    SECRET, "https://example.com/x", "Slack — #general", "line\nbreak", "x" * 129, 42,
])
def test_labels_never_carry_text_or_urls(value):
    labels = SttLabels(backend=value, model=value, provider=value)
    assert labels.to_dict() == {"backend": "other", "model": "other", "provider": "other"}
    assert LlmLabels(provider=value).provider == "other"


def test_identifier_labels_are_kept():
    for value in ("llama3.2:3b", "accounts/fireworks/models/kimi-k3",
                  "mlx-community/whisper-small", ""):
        assert LlmLabels(model=value).model == value


def test_emitted_lines_contain_only_schema_keys(ledger):
    ledger.path.parent.mkdir(parents=True)
    ledger.path.write_text(json.dumps({
        "ts": TS, "runtime": "python", "version": "0.2.2", "kind": "dictate",
        "status": "ok", "transcript": SECRET, "stt": {"backend": SECRET, "x": SECRET},
    }) + "\n")
    (event,) = ledger.read_events()
    line = event.to_line()
    assert tuple(json.loads(line)) == FIELDS
    assert "secret" not in line


# --- hooks ------------------------------------------------------------------


def _job_config(**overrides) -> Config:
    return replace(
        Config(),
        cleanup_level="medium",
        styles_enabled=False,
        history_enabled=False,
        llm_model="main",
        model_size="tiny",
        **overrides,
    )


def test_dictation_job_records_timings_but_no_text():
    app = pytest.importorskip("localflow.app")
    from localflow.context import AppInfo
    from localflow.jobs import Job

    transcriber = Mock(transcribe=Mock(return_value=SECRET))
    cleaner = Mock(clean=Mock(side_effect=lambda text, **_: text.upper()))
    job = Job("dictate", audio=np.full(16000, 0.1, np.float32),
              app_info=AppInfo("Secret Messenger", "com.secret.app"))
    history = SimpleNamespace(enabled=False)

    def run(config):
        return app._process_job(job, config, transcriber, cleaner, Mock(), (), None, None,
                                history)

    text, error = run(_job_config())
    assert text and not error
    run(_job_config(perf_metrics=False))

    events = PerfLedger().read_events()
    assert len(events) == 1, "perf_metrics = false records nothing"
    (event,) = events
    assert (event.kind, event.status, event.audio_s) == ("dictate", "ok", 1.0)
    assert None not in (event.queue_ms, event.stt_ms, event.llm_ms, event.render_ms,
                        event.total_ms)
    assert event.paste_ms is None
    assert event.stt == SttLabels("auto", "tiny", "auto")
    assert event.llm == LlmLabels("ollama", "main")
    assert event.chars == len(SECRET)
    raw = PerfLedger().path.read_text()
    for private in ("secret", "SECRET", "Secret", "com.secret"):
        assert private not in raw


def test_transform_job_records_llm_time_without_audio():
    app = pytest.importorskip("localflow.app")
    from localflow.jobs import Job

    transformer = Mock(run=Mock(return_value="polished"))
    history = SimpleNamespace(enabled=False)
    job = Job("transform", selection=SECRET, preset="polish")
    app._process_job(job, _job_config(), Mock(), Mock(), transformer, (), None, None, history)
    (event,) = PerfLedger().read_events()
    assert event.kind == "transform" and event.status == "ok"
    assert event.audio_s is None and event.stt is None and event.stt_ms is None
    assert event.llm_ms is not None and event.llm == LlmLabels("ollama", "main")
    assert "secret" not in PerfLedger().path.read_text()


def test_stt_load_is_recorded(monkeypatch):
    from localflow.providers import factory

    loaded = SimpleNamespace(backend="faster-whisper", model_size="tiny")
    monkeypatch.setattr(factory, "Transcriber", lambda *a, **k: loaded)
    assert factory.build_stt(_job_config()) is loaded

    def broken(*_a, **_k):
        raise RuntimeError("no model")

    monkeypatch.setattr(factory, "Transcriber", broken)
    with pytest.raises(RuntimeError):
        factory.build_stt(_job_config())
    with pytest.raises(RuntimeError):
        factory.build_stt(_job_config(perf_metrics=False))
    events = PerfLedger().read_events()
    assert [(e.kind, e.status) for e in events] == [("stt_load", "ok"), ("stt_load", "error")]
    assert events[0].stt == SttLabels("faster-whisper", "tiny", "auto")
    assert events[0].stt_ms is not None and events[0].total_ms is not None
