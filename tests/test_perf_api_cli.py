"""The perf ledger read side: `perf_ledger.summarize_events`, `GET /api/perf`
and `lf perf`. The Rust port's byte-identical output is checked in
tests/test_rust_parity.py."""

from __future__ import annotations

import json
from datetime import UTC, datetime, timedelta

import pytest
from click.testing import CliRunner
from fastapi.testclient import TestClient

from localflow import perf_ledger, server
from localflow.cli import cli
from localflow.config import Config
from localflow.history import HistoryStore
from localflow.perf_ledger import (
    LlmLabels,
    PerfEvent,
    PerfLedger,
    SttLabels,
    parse_window,
    render_summary,
    summarize_events,
    summary_json,
)

NOW = datetime(2026, 11, 24, 12, 0, 0, tzinfo=UTC)
SECRET = "my secret transcript about the acquisition"


def _ts(moment: datetime) -> str:
    return moment.strftime("%Y-%m-%dT%H:%M:%S.") + f"{moment.microsecond // 1000:03d}Z"


def _event(kind="dictate", *, ago=timedelta(hours=1), now=NOW, **values) -> PerfEvent:
    return PerfEvent(kind=kind, ts=_ts(now - ago), version="0.2.2", **values)


def _dictations(now=NOW) -> list[PerfEvent]:
    stt = SttLabels("faster-whisper", "base", "cpu")
    return [
        _event(audio_s=2.0, queue_ms=1.0, stt_ms=100.0, llm_ms=20.0, total_ms=100.0, stt=stt, now=now),
        _event(audio_s=4.0, queue_ms=3.0, stt_ms=200.0, llm_ms=40.0, total_ms=200.0, now=now),
        _event(audio_s=1.0, queue_ms=2.0, stt_ms=300.0, llm_ms=60.0, total_ms=300.0, now=now),
        _event(audio_s=0.0, stt_ms=400.0, total_ms=400.0, now=now),
        # Failures count towards the rate but never towards timings.
        _event(status="error", stt_ms=9999.0, total_ms=9999.0, now=now),
    ]


def _loads(now=NOW) -> list[PerfEvent]:
    stt = SttLabels("faster-whisper", "base", "cpu")
    return [
        _event("stt_load", ago=timedelta(hours=3), total_ms=900.0, stt=stt, now=now),
        _event("stt_load", ago=timedelta(hours=2), total_ms=700.0, stt=stt, now=now),
        _event("llm_warmup", ago=timedelta(hours=2), total_ms=50.0,
               llm=LlmLabels("ollama", "llama3.2:3b"), now=now),
        _event("stt_load", ago=timedelta(hours=2), total_ms=80.0,
               stt=SttLabels("faster-whisper", "tiny", ""), now=now),
        _event("stt_load", ago=timedelta(hours=1), status="error", total_ms=5.0, stt=stt, now=now),
    ]


# --- aggregation -------------------------------------------------------------

def test_empty_ledger_has_every_key_and_nulls():
    summary = summarize_events([], now=NOW)
    assert summary == {
        "window": "7d",
        "window_seconds": 604800,
        "total_events": 0,
        "by_kind": {},
        "model_loads": [],
        "dictation": {
            "count": 0, "median_total_ms": None, "p95_total_ms": None,
            "median_rtf": None, "slowest_stage": None, "budget_ms": 1500.0, "hint": None,
        },
    }
    assert render_summary(summary) == ["No perf events in the last 7d."]


@pytest.mark.parametrize("window,seconds", [("24h", 86400), ("7d", 604800), ("30d", 2592000)])
def test_windows(window, seconds):
    assert parse_window(window) == seconds
    events = [
        _event("recall", ago=timedelta(hours=23)),
        _event("recall", ago=timedelta(days=6)),
        _event("recall", ago=timedelta(days=29)),
        _event("recall", ago=timedelta(days=31)),
        # Clock skew: a later timestamp still counts.
        _event("recall", ago=-timedelta(minutes=5)),
        # Exactly on the boundary is inside.
        _event("recall", ago=timedelta(seconds=seconds)),
        _event("recall", ago=timedelta(seconds=seconds, milliseconds=1)),
    ]
    summary = summarize_events(events, window=window, now=NOW)
    expected = {"24h": 3, "7d": 4, "30d": 5}[window]
    assert summary["window"] == window and summary["window_seconds"] == seconds
    assert summary["total_events"] == summary["by_kind"]["recall"]["count"] == expected


@pytest.mark.parametrize("window", ["", "1d", "7D", "24 h", None, 7])
def test_invalid_window(window):
    with pytest.raises(ValueError, match="window must be one of 24h, 7d, 30d"):
        summarize_events([], window=window, now=NOW)


def test_unreal_calendar_times_are_skipped():
    good = _event("recall")
    bad = [replace_ts(good, ts) for ts in (
        "2026-02-30T10:00:00Z", "2026-11-24T10:00:60Z", "0000-01-01T00:00:00Z",
    )]
    summary = summarize_events([good, *bad], window="30d", now=NOW)
    assert summary["total_events"] == 1


def replace_ts(event: PerfEvent, ts: str) -> PerfEvent:
    return PerfEvent.from_dict({**event.to_dict(), "ts": ts})


def test_failure_rate_stages_and_rtf():
    summary = summarize_events(_dictations(), now=NOW)
    dictate = summary["by_kind"]["dictate"]
    assert (dictate["count"], dictate["ok_count"], dictate["error_count"]) == (5, 4, 1)
    assert dictate["failure_rate"] == 0.2
    # Only stages with samples, in STAGES order; ok events only.
    assert list(dictate["stages"]) == ["queue_ms", "stt_ms", "llm_ms", "total_ms"]
    # Linear interpolation: p50 of 100..400 is 250; p95 index 2.85 -> 385
    # (0.95 * 3 is 2.8499999999999996 in binary, rounding fixes it).
    assert dictate["stages"]["total_ms"] == {"p50": 250.0, "p95": 385.0, "count": 4}
    assert dictate["stages"]["queue_ms"] == {"p50": 2.0, "p95": 2.9, "count": 3}
    # RTF = stt_ms / (audio_s * 1000) where audio_s > 0: 0.05, 0.05, 0.3.
    assert dictate["median_rtf"] == 0.05
    assert summary["dictation"] == {
        "count": 5, "median_total_ms": 250.0, "p95_total_ms": 385.0,
        "median_rtf": 0.05, "slowest_stage": "stt_ms", "budget_ms": 1500.0, "hint": None,
    }


def test_rounding_is_half_away_from_zero():
    events = [_event("recall", total_ms=value) for value in (0.001, 0.002)]
    # Midpoint 0.0015 is 0.0015000000000000000312... -> 0.002.
    assert summarize_events(events, now=NOW)["by_kind"]["recall"]["stages"]["total_ms"]["p50"] == 0.002
    rates = [_event("recall"), _event("recall"), _event("recall", status="error")]
    assert summarize_events(rates, now=NOW)["by_kind"]["recall"]["failure_rate"] == 0.3333


def test_model_loads_group_sort_and_keep_the_last():
    assert summarize_events(_loads(), now=NOW)["model_loads"] == [
        {"kind": "stt_load", "engine": "faster-whisper", "model": "base",
         "count": 2, "p50_ms": 800.0, "p95_ms": 890.0, "last_ms": 700.0},
        {"kind": "stt_load", "engine": "faster-whisper", "model": "tiny",
         "count": 1, "p50_ms": 80.0, "p95_ms": 80.0, "last_ms": 80.0},
        {"kind": "llm_warmup", "engine": "ollama", "model": "llama3.2:3b",
         "count": 1, "p50_ms": 50.0, "p95_ms": 50.0, "last_ms": 50.0},
    ]


def test_last_load_follows_timestamps_not_file_order():
    stt = SttLabels("parakeet", "x", "")
    later = _event("stt_load", ago=timedelta(hours=1), total_ms=10.0, stt=stt)
    earlier = _event("stt_load", ago=timedelta(hours=2), total_ms=20.0, stt=stt)
    (load,) = summarize_events([later, earlier], now=NOW)["model_loads"]
    assert load["last_ms"] == 10.0


@pytest.mark.parametrize("stage,hint", [
    ("stt_ms", "speech-to-text is the slow step"),
    ("llm_ms", "cleanup LLM is the slow step"),
    ("paste_ms", "pasting is the slow step"),
    ("queue_ms", "waiting for the previous dictation is the slow step"),
    ("render_ms", "text formatting is the slow step"),
])
def test_slowest_stage_and_hint_over_budget(stage, hint):
    others = {s: 10.0 for s in perf_ledger.STEP_STAGES}
    events = [_event(**{**others, stage: 1800.0, "total_ms": 1900.0 + i}) for i in range(3)]
    dictation = summarize_events(events, now=NOW)["dictation"]
    assert dictation["slowest_stage"] == stage
    assert dictation["hint"] == hint
    lines = render_summary(summarize_events(events, now=NOW))
    assert lines[-1] == f"Hint: p95 is over 1500.0 ms; {hint}."


def test_no_hint_within_budget_and_first_stage_wins_ties():
    events = [_event(stt_ms=50.0, llm_ms=50.0, total_ms=1500.0)]
    dictation = summarize_events(events, now=NOW)["dictation"]
    assert dictation["slowest_stage"] == "stt_ms"
    assert dictation["p95_total_ms"] == 1500.0 and dictation["hint"] is None


def test_summary_is_numbers_and_labels_only():
    events = [*_dictations(), *_loads(), _event("meeting_chat", llm=LlmLabels(SECRET, "m"))]
    text = summary_json(summarize_events(events, window="30d", now=NOW))
    assert SECRET not in text and "secret" not in text
    assert ", " not in text and ": " not in text  # compact separators


def test_render_summary_table():
    events = [*_dictations(), *_loads(), _event("recall", total_ms=12.5)]
    lines = render_summary(summarize_events(events, now=NOW))
    assert lines == [
        "Perf events in the last 7d: 11",
        "",
        "Kind        Count  Errors  Failure rate  Median RTF",
        "dictate     5      1       20.0%         0.05",
        "recall      1      0       0.0%          -",
        "stt_load    4      1       25.0%         -",
        "llm_warmup  1      0       0.0%          -",
        "",
        "Kind        Stage     p50 ms  p95 ms  Samples",
        "dictate     queue_ms  2.0     2.9     3",
        "dictate     stt_ms    250.0   385.0   4",
        "dictate     llm_ms    40.0    58.0    3",
        "dictate     total_ms  250.0   385.0   4",
        "recall      total_ms  12.5    12.5    1",
        "stt_load    total_ms  700.0   880.0   3",
        "llm_warmup  total_ms  50.0    50.0    1",
        "",
        "Load        Engine          Model        Count  p50 ms  p95 ms  Last ms",
        "stt_load    faster-whisper  base         2      800.0   890.0   700.0",
        "stt_load    faster-whisper  tiny         1      80.0    80.0    80.0",
        "llm_warmup  ollama          llama3.2:3b  1      50.0    50.0    50.0",
        "",
        "Dictation: median 250.0 ms, p95 385.0 ms, RTF 0.05, slowest stage stt_ms",
    ]


def test_summarize_reads_the_ledger_file(tmp_path):
    events = _dictations()
    # Rotates once, after the third line: the summary must read both files.
    first_three = sum(len(event.to_line()) + 1 for event in events[:3])
    ledger = PerfLedger(tmp_path / "events.jsonl", max_bytes=first_three)
    for event in events:
        ledger.record(event)
    assert ledger.previous_path.exists()
    assert len(ledger.path.read_text().splitlines()) == 2
    with ledger.path.open("a") as handle:
        handle.write("not json\n")
    summary = perf_ledger.summarize(ledger, now=NOW)
    assert summary == summarize_events(_dictations(), now=NOW)


# --- GET /api/perf -----------------------------------------------------------

@pytest.fixture
def ledger_path(tmp_path, monkeypatch):
    path = tmp_path / "perf" / "events.jsonl"
    monkeypatch.setenv(perf_ledger.ENV_PATH, str(path))
    return path


@pytest.fixture
def client(monkeypatch, tmp_path, ledger_path):
    # perf_metrics off: the server's own startup event must not reach the ledger.
    monkeypatch.setattr(server, "_config", Config(
        meetings_enabled=False, meeting_watch=False, perf_metrics=False,
    ))
    monkeypatch.setattr(server, "_history", HistoryStore(tmp_path / "history.sqlite"))
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as client:
        yield client


AUTH = {"X-Localflow-Token": "test-token"}


def _write(path, events):
    ledger = PerfLedger(path)
    for event in events:
        assert ledger.record(event)


def test_api_perf_requires_the_token(client):
    assert client.get("/api/perf").status_code == 401
    assert client.get("/api/perf", headers={"X-Localflow-Token": "wrong"}).json() == {
        "detail": "invalid API token",
    }
    remote = client.get("/api/perf", headers={"Host": "remote.example"})
    assert (remote.status_code, remote.json()) == (403, {"detail": "invalid host"})
    assert client.get("/api/perf?token=test-token").status_code == 200


def test_api_perf_empty_and_invalid_window(client, ledger_path):
    response = client.get("/api/perf", headers=AUTH)
    assert response.status_code == 200
    assert response.headers["content-type"] == "application/json"
    assert response.json() == summarize_events([], now=NOW)
    assert not ledger_path.exists()
    for window in ("1y", "", "7D"):
        bad = client.get("/api/perf", params={"window": window}, headers=AUTH)
        assert (bad.status_code, bad.json()) == (
            400, {"detail": "window must be one of 24h, 7d, 30d"},
        )


@pytest.mark.parametrize("window", ["24h", "7d", "30d"])
def test_api_perf_matches_the_fixture(client, ledger_path, window):
    now = datetime.now(UTC)
    events = [
        *_dictations(now), *_loads(now),
        _event("recall", ago=timedelta(days=3), total_ms=10.0, now=now),
        _event("recall", ago=timedelta(days=20), total_ms=20.0, now=now),
    ]
    _write(ledger_path, events)
    response = client.get("/api/perf", params={"window": window}, headers=AUTH)
    assert response.status_code == 200
    expected = summarize_events(events, window=window, now=now)
    assert response.text == summary_json(expected)
    recalls = response.json()["by_kind"].get("recall", {}).get("count", 0)
    assert recalls == {"24h": 0, "7d": 1, "30d": 2}[window]


# --- lf perf -----------------------------------------------------------------

def _lf(*args):
    return CliRunner().invoke(cli, ["perf", *args], catch_exceptions=False)


def test_lf_perf_empty_state(ledger_path):
    result = _lf()
    assert (result.exit_code, result.output) == (0, "No perf events in the last 7d.\n")
    assert _lf("--window", "24h").output == "No perf events in the last 24h.\n"
    assert not ledger_path.exists()


def test_lf_perf_table_json_and_window(ledger_path):
    now = datetime.now(UTC)
    events = [*_dictations(now), *_loads(now),
              _event("recall", ago=timedelta(days=20), total_ms=20.0, now=now)]
    _write(ledger_path, events)
    table = _lf()
    assert table.exit_code == 0
    assert table.output.startswith("Perf events in the last 7d: 10\n\nKind ")
    assert "Dictation: median 250.0 ms, p95 385.0 ms, RTF 0.05, slowest stage stt_ms\n" in table.output
    raw = _lf("--json", "--window", "30d")
    assert raw.exit_code == 0
    assert raw.output == json.dumps(summarize_events(events, window="30d", now=now), indent=2) + "\n"
    assert json.loads(raw.output)["by_kind"]["recall"]["count"] == 1
    assert "recall" not in json.loads(_lf("--json").output)["by_kind"]


def test_lf_perf_invalid_window_is_a_usage_error(ledger_path):
    result = CliRunner().invoke(cli, ["perf", "--window", "1y"])
    assert result.exit_code == 2
    assert "Invalid value for '--window': '1y' is not one of '24h', '7d', '30d'." in result.output


def test_lf_perf_clear_removes_both_files(ledger_path):
    ledger = PerfLedger(ledger_path, max_bytes=1)
    ledger.record(_event())
    ledger.record(_event())
    assert ledger.path.exists() and ledger.previous_path.exists()
    result = _lf("--clear")
    assert (result.exit_code, result.output) == (0, "Cleared the perf ledger.\n")
    assert not ledger.path.exists() and not ledger.previous_path.exists()
    assert _lf("--clear").output == "Cleared the perf ledger.\n"


def test_lf_perf_is_listed_in_help():
    result = CliRunner().invoke(cli, ["--help"])
    assert "  perf " in result.output
    assert "Show local speed metrics from the perf ledger." in _lf("--help").output
