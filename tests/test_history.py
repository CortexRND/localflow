import os
import sqlite3
import stat
import wave
from datetime import date, datetime, time, timedelta

import numpy as np
import pytest

from localflow.history import HistoryStore


def _stamp(day: date, hour: int = 12) -> float:
    return datetime.combine(day, time(hour=hour)).timestamp()


def _store(tmp_path):
    return HistoryStore(tmp_path / "history.sqlite")


def test_schema_crud_search_and_last_ok(tmp_path):
    store = _store(tmp_path)
    assert sqlite3.connect(store.path).execute("PRAGMA user_version").fetchone()[0] == 1

    first = store.add(
        kind="dictate",
        raw_text="Raw words",
        final_text="Clean final words",
        status="ok",
        app_name="Editor",
        duration_s=12,
        ts=1,
    )
    failed = store.add(
        kind="dictate",
        raw_text="Needs retry",
        status="failed",
        ts=2,
    )

    assert store.get(first)["words"] == 3
    assert store.last_ok()["id"] == first
    assert [item["id"] for item in store.recent(query="raw")] == [first]
    assert [item["id"] for item in store.recent(query="retry")] == [failed]
    assert store.recent(limit=0) == []
    assert store.delete(failed)
    assert store.get(failed) is None
    assert not store.delete(failed)


def test_history_search_escapes_like_metacharacters_and_supports_offsets(tmp_path):
    store = _store(tmp_path)
    literal_percent = store.add(
        kind="dictate", raw_text="100% ready", final_text="literal", ts=3
    )
    store.add(kind="dictate", raw_text="100 ready", final_text="plain", ts=2)
    underscore = store.add(
        kind="dictate", raw_text="a_b", final_text="underscore", ts=1
    )

    assert [row["id"] for row in store.recent(query="%")] == [literal_percent]
    assert [row["id"] for row in store.recent(query="_")] == [underscore]
    assert [row["id"] for row in store.recent(limit=2, offset=2)] == [
        row["id"] for row in store.recent(limit=3)[2:]
    ]


def test_disabled_history_does_not_add_rows(tmp_path):
    path = tmp_path / "history.sqlite"
    enabled = HistoryStore(path)
    enabled.add(kind="dictate", final_text="existing")
    store = HistoryStore(path, enabled=False)

    assert store.add(kind="dictate", final_text="not saved") is None
    assert store.recent() == []
    assert store.last_ok() is None
    assert store.get(1) is None
    assert store.stats()["total_dictations"] == 0


def test_last_ok_text_falls_back_when_history_disabled_or_empty(tmp_path):
    store = _store(tmp_path)
    store.add(kind="dictate", final_text="saved", status="ok")
    assert store.latest_text("last file") == "saved"

    empty = HistoryStore(tmp_path / "empty.sqlite")
    assert empty.latest_text("last file") == "last file"
    disabled = HistoryStore(tmp_path / "disabled.sqlite", enabled=False)
    assert disabled.latest_text("last file") == "last file"


def test_clear_removes_rows_and_audio_clips(tmp_path):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    clip = store.write_clip(np.zeros(1600, dtype=np.float32), 16000)
    store.add(kind="dictate", status="failed", audio_path=clip)
    orphan = store.clip_dir / "orphan.wav"
    orphan.write_bytes(b"clip")

    assert store.clear() == 1
    assert store.recent() == []
    assert not clip.exists()
    assert not orphan.exists()


def test_clip_is_16khz_mono_wav_and_private_on_posix(tmp_path):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")

    path = store.write_clip(np.linspace(-0.5, 0.5, 800, dtype=np.float32), 8000)

    with wave.open(str(path), "rb") as audio:
        assert audio.getnchannels() == 1
        assert audio.getframerate() == 16000
        assert audio.getnframes() == 1600
    if os.name == "posix":
        assert stat.S_IMODE(path.stat().st_mode) == 0o600


def test_pruning_keeps_only_latest_ok_clips_and_preserves_failed(tmp_path):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    ok_clips = []
    for ts in (1, 2, 3):
        clip = store.write_clip(np.zeros(32, dtype=np.float32), 16000)
        ok_clips.append(clip)
        store.add(
            kind="dictate",
            status="ok",
            final_text="spoken words",
            audio_path=clip,
            ts=ts,
        )
    failed_clip = store.write_clip(np.zeros(32, dtype=np.float32), 16000)
    failed_id = store.add(
        kind="dictate", status="failed", audio_path=failed_clip, ts=4
    )

    assert store.prune_audio_clips(1) == 2
    assert not ok_clips[0].exists()
    assert not ok_clips[1].exists()
    assert ok_clips[2].exists()
    assert failed_clip.exists()
    assert store.get(failed_id)["audio_path"] == str(failed_clip)
    assert store.prune_audio_clips(0) == 1
    assert not ok_clips[2].exists()
    assert failed_clip.exists()


def test_stats_words_wpm_apps_and_streaks(tmp_path):
    store = _store(tmp_path)
    today = date.today()
    store.add(
        kind="dictate",
        status="ok",
        final_text="one two three four",
        app_name="Editor",
        duration_s=0.5,
        ts=_stamp(today),
    )
    store.add(
        kind="dictate",
        status="ok",
        final_text="five six",
        app_name="Editor",
        duration_s=60,
        ts=_stamp(today),
    )
    store.add(
        kind="command",
        status="ok",
        final_text="seven eight",
        app_name="Browser",
        duration_s=30,
        ts=_stamp(today - timedelta(days=1)),
    )
    store.add(
        kind="transform",
        status="failed",
        final_text="excluded",
        duration_s=1,
        ts=_stamp(today),
    )
    store.add(
        kind="dictate",
        status="ok",
        final_text="nine",
        duration_s=10,
        ts=_stamp(today - timedelta(days=2)),
    )
    store.add(
        kind="dictate",
        status="ok",
        final_text="older",
        duration_s=10,
        ts=_stamp(today - timedelta(days=5)),
    )
    store.add(
        kind="dictate",
        status="ok",
        final_text="older streak",
        duration_s=10,
        ts=_stamp(today - timedelta(days=6)),
    )

    stats = store.stats(_stamp(today))

    assert stats["total_words"] == 10
    assert stats["total_dictations"] == 5
    assert stats["avg_wpm"] == pytest.approx(4.0)
    assert stats["words_today"] == 6
    assert stats["words_7d"] == 10
    assert stats["current_streak_days"] == 1
    assert stats["longest_streak_days"] == 2
    assert stats["top_apps"][:2] == [("Editor", 6), ("Unknown", 4)]


def test_current_streak_can_start_yesterday_and_empty_today_resets(tmp_path):
    store = _store(tmp_path)
    today = date.today()
    store.add(
        kind="dictate",
        status="ok",
        final_text="yesterday",
        duration_s=1,
        ts=_stamp(today - timedelta(days=1)),
    )

    assert store.stats(_stamp(today))["current_streak_days"] == 1
    assert store.stats(_stamp(today + timedelta(days=1)))["current_streak_days"] == 0


def test_milestone_is_claimed_once_per_day(tmp_path):
    store = _store(tmp_path)

    assert store.claim_milestone(7, "2026-01-01")
    assert not store.claim_milestone(7, "2026-01-01")
    assert store.claim_milestone(7, "2026-01-02")
