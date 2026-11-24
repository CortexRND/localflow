"""The Hub Home "Speed" card: pure model (`localflow.hub_model`) and the Tk
wiring. Mirrors `speed_card_tests` in `rust/lf-ui/src/hub_model.rs`."""

from dataclasses import replace

import pytest

from localflow import perf_ledger
from localflow.config import Config
from localflow.hub_model import (
    SPEED_CARD_H,
    SPEED_CARD_HINT_H,
    SPEED_NONE,
    SPEED_WINDOW,
    SpeedCard,
    format_rtf,
    format_speed_ms,
    home_speed_card,
    load_perf_summary,
    speed_card_from_summary,
    speed_stage_label,
)


def _summary(dictation):
    return {"window": "7d", "total_events": 3, "dictation": dictation}


def _full(hint=None):
    return _summary({
        "count": 12,
        "median_total_ms": 420.4,
        "p95_total_ms": 1800.0,
        "median_rtf": 0.1234,
        "slowest_stage": "llm_ms",
        "budget_ms": 1500.0,
        "hint": hint,
    })


@pytest.mark.parametrize(("ms", "text"), [
    (420, "420 ms"), (420.5, "421 ms"), (0, "0 ms"), (999.4, "999 ms"),
    (999.6, "1.0 s"), (1234, "1.2 s"), (1250, "1.3 s"), (61_049.0, "61.0 s"),
    (-1, SPEED_NONE), ("420", SPEED_NONE), (None, SPEED_NONE), (True, SPEED_NONE),
    (float("nan"), SPEED_NONE),
])
def test_format_speed_ms(ms, text):
    assert format_speed_ms(ms) == text


@pytest.mark.parametrize(("rtf", "text"), [
    (0.1234, "0.12×"), (0.125, "0.13×"), (1.5, "1.50×"), (0, "0.00×"),
    (0.001, "<0.01×"), (None, SPEED_NONE), (-0.5, SPEED_NONE),
])
def test_format_rtf(rtf, text):
    assert format_rtf(rtf) == text


def test_stage_labels():
    stages = ["stt_ms", "llm_ms", "queue_ms", "render_ms", "paste_ms"]
    assert [speed_stage_label(s) for s in stages] == [
        "STT", "Cleanup LLM", "Queue", "Render", "Paste",
    ]
    assert speed_stage_label("total_ms") == SPEED_NONE
    assert speed_stage_label(None) == SPEED_NONE


def test_card_with_and_without_hint():
    card = speed_card_from_summary(_full("cleanup LLM is the slow step"))
    assert card == SpeedCard(
        count=12, median="420 ms", rtf="0.12×", slowest="Cleanup LLM",
        hint="Cleanup LLM is the slow step", caption="Last 7 days · 12 dictations",
    )
    assert card.metrics() == [
        ("Median time", "420 ms"), ("Median RTF", "0.12×"), ("Slowest step", "Cleanup LLM"),
    ]
    assert card.height() == SPEED_CARD_H + SPEED_CARD_HINT_H
    quiet = speed_card_from_summary(_full())
    assert quiet.hint is None and quiet.height() == SPEED_CARD_H
    assert speed_card_from_summary(_full("  ")).hint is None


def test_single_dictation_without_rtf_or_stage():
    card = speed_card_from_summary(_summary({
        "count": 1, "median_total_ms": 1500.0, "median_rtf": None,
        "slowest_stage": None, "hint": None,
    }))
    assert (card.median, card.rtf, card.slowest) == ("1.5 s", SPEED_NONE, SPEED_NONE)
    assert card.caption == "Last 7 days · 1 dictation"


@pytest.mark.parametrize("bad", [
    None, [], {}, {"dictation": None},
    _summary({"count": 0, "median_total_ms": None}),
    _summary({"count": 3, "median_total_ms": None}),
    _summary({"count": True, "median_total_ms": 400}),
    _summary({"count": -1, "median_total_ms": 400}),
    _summary({"count": "3", "median_total_ms": 400}),
])
def test_hidden_when_empty_or_malformed(bad):
    assert speed_card_from_summary(bad) is None


def test_hidden_when_disabled():
    assert speed_card_from_summary(_full(), enabled=False) is None
    off = replace(Config(), perf_metrics=False)
    assert home_speed_card(_full, off) is None
    assert load_perf_summary(off) is None


def test_home_card_source_handling():
    assert home_speed_card(_full, Config()) is not None
    assert home_speed_card(None, Config()) is None
    assert home_speed_card(lambda: None, Config()) is None

    def boom():
        raise OSError("ledger unreadable")

    assert home_speed_card(boom, Config()) is None


def test_empty_ledger_hides_and_real_summary_shows(tmp_path, monkeypatch):
    path = tmp_path / "events.jsonl"
    monkeypatch.setenv("LOCALFLOW_PERF_LEDGER", str(path))
    assert speed_card_from_summary(load_perf_summary(Config())) is None
    ledger = perf_ledger.PerfLedger(path)
    for total in (2400.0, 2600.0, 2500.0):
        assert ledger.record("dictate", audio_s=4.0, stt_ms=400.0, llm_ms=1800.0,
                             paste_ms=20.0, total_ms=total)
    summary = load_perf_summary(Config())
    assert summary["window"] == SPEED_WINDOW
    card = speed_card_from_summary(summary)
    assert card.median == "2.5 s"
    assert card.rtf == "0.10×"
    assert card.slowest == "Cleanup LLM"
    assert card.hint == "Cleanup LLM is the slow step"
    assert card.caption == "Last 7 days · 3 dictations"


# --------------------------------------------------------------------- Tk ---


def _tk_window(tmp_path, **kwargs):
    tk = pytest.importorskip("tkinter")
    from localflow.dictionary import DictionaryStore
    from localflow.history import HistoryStore
    from localflow.hub_tk import HubWindow

    try:
        root = tk.Tk()
    except tk.TclError as exc:
        pytest.skip(f"Tk unavailable: {exc}")

    class Meetings:
        def templates(self):
            return {"default": "general", "templates": []}

        def status(self):
            return {"session": None, "last_saved": None, "chat_available": False}

    window = HubWindow(
        root, HistoryStore(tmp_path / "h.sqlite", tmp_path / "clips"),
        DictionaryStore(tmp_path / "d.json"), Config(), meeting_client=Meetings(), **kwargs,
    )
    return root, window


def _texts(widget):
    import tkinter as tk

    out = []
    for child in widget.winfo_children():
        if isinstance(child, tk.Label | tk.Button):  # frames and canvases have no text
            out.append(child.cget("text"))
        out.extend(_texts(child))
    return out


def test_tk_home_hides_card_without_source(tmp_path):
    root, window = _tk_window(tmp_path)
    try:
        assert window.speed_card is None
        assert "SPEED" not in _texts(window.body)
    finally:
        root.destroy()


def test_tk_home_shows_card_with_hint(tmp_path):
    root, window = _tk_window(tmp_path, perf_summary=lambda: _full("cleanup LLM is the slow step"))
    try:
        texts = _texts(window.body)
        for text in ("SPEED", "420 ms", "0.12×", "Cleanup LLM", "Cleanup LLM is the slow step",
                     "Last 7 days · 12 dictations"):
            assert text in texts
        assert texts.index("SPEED") < texts.index("Recent dictations")
    finally:
        root.destroy()
