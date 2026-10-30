"""Local SQLite storage for dictation history and retryable audio clips."""

from __future__ import annotations

import logging
import os
import sqlite3
import threading
import time
import uuid
import wave
from contextlib import contextmanager
from datetime import date, datetime, timedelta
from pathlib import Path
from tempfile import NamedTemporaryFile

import numpy as np

log = logging.getLogger("localflow.history")

_SCHEMA_VERSION = 1
_KINDS = {"dictate", "command", "transform"}
_STATUSES = {"ok", "failed", "silent"}
_AUDIO_RATE = 16000


def history_dir() -> Path:
    return Path.home() / ".localflow"


def history_path() -> Path:
    return history_dir() / "history.sqlite"


def clips_dir() -> Path:
    return history_dir() / "clips"


class HistoryStore:
    """Thread-safe local history backed by SQLite and private WAV clips."""

    def __init__(
        self,
        path: Path | None = None,
        clip_dir: Path | None = None,
        enabled: bool = True,
    ):
        self.path = Path(path) if path is not None else history_path()
        self.clip_dir = (
            Path(clip_dir) if clip_dir is not None else self.path.parent / "clips"
        )
        self.enabled = enabled
        self._lock = threading.RLock()
        if enabled:
            self._initialize()

    @contextmanager
    def _connect(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(self.path, timeout=30)
        connection.row_factory = sqlite3.Row
        try:
            try:
                os.chmod(self.path, 0o600)
            except OSError:
                log.warning("could not restrict history database permissions: %s", self.path)
            yield connection
            connection.commit()
        except Exception:
            connection.rollback()
            raise
        finally:
            connection.close()

    def _initialize(self) -> None:
        with self._lock, self._connect() as connection:
            version = connection.execute("PRAGMA user_version").fetchone()[0]
            if version > _SCHEMA_VERSION:
                raise RuntimeError(
                    f"history schema {version} is newer than supported "
                    f"({_SCHEMA_VERSION})"
                )
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS dictations (
                    id INTEGER PRIMARY KEY,
                    ts REAL NOT NULL,
                    kind TEXT NOT NULL CHECK(kind IN ('dictate', 'command', 'transform')),
                    app_name TEXT,
                    bundle_id TEXT,
                    category TEXT,
                    duration_s REAL NOT NULL,
                    raw_text TEXT NOT NULL,
                    final_text TEXT NOT NULL,
                    words INTEGER NOT NULL,
                    status TEXT NOT NULL CHECK(status IN ('ok', 'failed', 'silent')),
                    audio_path TEXT
                )
                """
            )
            connection.execute(
                "CREATE INDEX IF NOT EXISTS dictations_ts ON dictations(ts DESC, id DESC)"
            )
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS achievements (
                    milestone INTEGER NOT NULL,
                    day TEXT NOT NULL,
                    PRIMARY KEY (milestone, day)
                )
                """
            )
            connection.execute(f"PRAGMA user_version = {_SCHEMA_VERSION}")

    @staticmethod
    def _row(row: sqlite3.Row | None) -> dict | None:
        return dict(row) if row is not None else None

    def add(
        self,
        *,
        kind: str,
        app_name: str | None = None,
        bundle_id: str | None = None,
        category: str | None = None,
        duration_s: float = 0,
        raw_text: str = "",
        final_text: str = "",
        status: str = "ok",
        audio_path: str | Path | None = None,
        ts: float | None = None,
    ) -> int | None:
        if not self.enabled:
            return None
        if kind not in _KINDS:
            raise ValueError(f"invalid history kind: {kind!r}")
        if status not in _STATUSES:
            raise ValueError(f"invalid history status: {status!r}")
        if duration_s < 0:
            raise ValueError("duration_s must be non-negative")
        words = len(final_text.split())
        with self._lock, self._connect() as connection:
            cursor = connection.execute(
                """
                INSERT INTO dictations
                    (ts, kind, app_name, bundle_id, category, duration_s,
                     raw_text, final_text, words, status, audio_path)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    time.time() if ts is None else float(ts),
                    kind,
                    app_name,
                    bundle_id,
                    category,
                    float(duration_s),
                    raw_text,
                    final_text,
                    words,
                    status,
                    str(audio_path) if audio_path is not None else None,
                ),
            )
            return int(cursor.lastrowid)

    def update(
        self,
        entry_id: int,
        *,
        status: str | None = None,
        raw_text: str | None = None,
        final_text: str | None = None,
        duration_s: float | None = None,
        audio_path: str | Path | None = None,
    ) -> bool:
        if not self.enabled:
            return False
        changes = {}
        if status is not None:
            if status not in _STATUSES:
                raise ValueError(f"invalid history status: {status!r}")
            changes["status"] = status
        if raw_text is not None:
            changes["raw_text"] = raw_text
        if final_text is not None:
            changes["final_text"] = final_text
            changes["words"] = len(final_text.split())
        if duration_s is not None:
            if duration_s < 0:
                raise ValueError("duration_s must be non-negative")
            changes["duration_s"] = float(duration_s)
        if audio_path is not None:
            changes["audio_path"] = str(audio_path)
        if not changes:
            return self.get(entry_id) is not None
        assignments = ", ".join(f"{key} = ?" for key in changes)
        with self._lock, self._connect() as connection:
            cursor = connection.execute(
                f"UPDATE dictations SET {assignments} WHERE id = ?",
                (*changes.values(), entry_id),
            )
            return cursor.rowcount > 0

    def recent(
        self, limit: int = 50, query: str | None = None, offset: int = 0
    ) -> list[dict]:
        if not self.enabled:
            return []
        limit = max(0, int(limit))
        offset = max(0, int(offset))
        if limit == 0:
            return []
        sql = "SELECT * FROM dictations"
        params: list[object] = []
        if query:
            escaped = query.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
            sql += " WHERE raw_text LIKE ? ESCAPE '\\' OR final_text LIKE ? ESCAPE '\\'"
            params.extend((f"%{escaped}%", f"%{escaped}%"))
        sql += " ORDER BY ts DESC, id DESC LIMIT ? OFFSET ?"
        params.extend((limit, offset))
        with self._lock, self._connect() as connection:
            return [dict(row) for row in connection.execute(sql, params).fetchall()]

    def get(self, entry_id: int) -> dict | None:
        if not self.enabled:
            return None
        with self._lock, self._connect() as connection:
            row = connection.execute(
                "SELECT * FROM dictations WHERE id = ?", (entry_id,)
            ).fetchone()
            return self._row(row)

    def last_ok(self) -> dict | None:
        if not self.enabled:
            return None
        with self._lock, self._connect() as connection:
            row = connection.execute(
                """
                SELECT * FROM dictations
                WHERE status = 'ok' AND final_text != ''
                ORDER BY ts DESC, id DESC LIMIT 1
                """
            ).fetchone()
            return self._row(row)

    def latest_text(self, fallback: str = "") -> str:
        row = self.last_ok()
        return (row or {}).get("final_text") or fallback

    def delete(self, entry_id: int) -> bool:
        if not self.enabled:
            return False
        with self._lock, self._connect() as connection:
            row = connection.execute(
                "SELECT audio_path FROM dictations WHERE id = ?", (entry_id,)
            ).fetchone()
            if row is None:
                return False
            connection.execute("DELETE FROM dictations WHERE id = ?", (entry_id,))
        self._remove_clip(row["audio_path"])
        return True

    def clear(self) -> int:
        if not self.enabled:
            return 0
        with self._lock, self._connect() as connection:
            count = connection.execute("SELECT count(*) FROM dictations").fetchone()[0]
            paths = [
                row[0]
                for row in connection.execute(
                    "SELECT audio_path FROM dictations WHERE audio_path IS NOT NULL"
                )
            ]
            connection.execute("DELETE FROM dictations")
        for path in paths:
            self._remove_clip(path)
        if self.clip_dir.exists():
            for path in self.clip_dir.glob("*.wav"):
                self._remove_clip(path)
        return int(count)

    def stats(self, now: float | datetime | None = None) -> dict:
        if not self.enabled:
            return {
                "total_words": 0,
                "total_dictations": 0,
                "avg_wpm": 0.0,
                "words_today": 0,
                "words_7d": 0,
                "current_streak_days": 0,
                "longest_streak_days": 0,
                "top_apps": [],
            }
        current = (
            now.astimezone().date()
            if isinstance(now, datetime) and now.tzinfo is not None
            else now.date()
            if isinstance(now, datetime)
            else datetime.fromtimestamp(time.time() if now is None else now).date()
        )
        with self._lock, self._connect() as connection:
            rows = connection.execute(
                """
                SELECT ts, app_name, duration_s, words FROM dictations
                WHERE status = 'ok' AND kind = 'dictate'
                """
            ).fetchall()
        dates: set[date] = set()
        total_words = words_today = words_7d = 0
        wpm_words = wpm_seconds = 0
        app_words: dict[str, int] = {}
        for row in rows:
            words = int(row["words"])
            day = datetime.fromtimestamp(row["ts"]).date()
            dates.add(day)
            total_words += words
            if day == current:
                words_today += words
            if current - timedelta(days=6) <= day <= current:
                words_7d += words
            if row["duration_s"] >= 1:
                wpm_words += words
                wpm_seconds += row["duration_s"]
            name = row["app_name"] or "Unknown"
            app_words[name] = app_words.get(name, 0) + words

        current_streak = self._current_streak(dates, current)
        longest_streak = self._longest_streak(dates)
        top_apps = sorted(
            app_words.items(), key=lambda item: (-item[1], item[0].casefold())
        )[:5]
        return {
            "total_words": total_words,
            "total_dictations": len(rows),
            "avg_wpm": wpm_words / (wpm_seconds / 60) if wpm_seconds else 0.0,
            "words_today": words_today,
            "words_7d": words_7d,
            "current_streak_days": current_streak,
            "longest_streak_days": longest_streak,
            "top_apps": top_apps,
        }

    @staticmethod
    def _current_streak(days: set[date], today: date) -> int:
        start = today if today in days else today - timedelta(days=1)
        if start not in days:
            return 0
        streak = 0
        while start in days:
            streak += 1
            start -= timedelta(days=1)
        return streak

    @staticmethod
    def _longest_streak(days: set[date]) -> int:
        longest = current = 0
        previous = None
        for day in sorted(days):
            current = current + 1 if previous is not None and day == previous + timedelta(days=1) else 1
            longest = max(longest, current)
            previous = day
        return longest

    def claim_milestone(self, milestone: int, day: date | str | None = None) -> bool:
        if not self.enabled:
            return False
        milestone_day = (
            day.isoformat() if isinstance(day, date) else day or date.today().isoformat()
        )
        with self._lock, self._connect() as connection:
            cursor = connection.execute(
                """
                INSERT OR IGNORE INTO achievements (milestone, day) VALUES (?, ?)
                """,
                (milestone, milestone_day),
            )
            return cursor.rowcount > 0

    def write_clip(self, audio: np.ndarray, sample_rate: int) -> Path:
        if not self.enabled:
            raise RuntimeError("history is disabled")
        if sample_rate <= 0:
            raise ValueError("sample_rate must be positive")
        samples = np.asarray(audio, dtype=np.float32).reshape(-1)
        if sample_rate != _AUDIO_RATE and samples.size:
            size = max(1, round(samples.size * _AUDIO_RATE / sample_rate))
            samples = np.interp(
                np.linspace(0, samples.size - 1, size),
                np.arange(samples.size),
                samples,
            ).astype(np.float32)
        pcm = (np.clip(samples, -1, 1) * 32767).astype("<i2")
        self.clip_dir.mkdir(parents=True, exist_ok=True)
        target = self.clip_dir / f"{uuid.uuid4().hex}.wav"
        temporary = None
        try:
            with NamedTemporaryFile(
                "wb",
                dir=self.clip_dir,
                prefix=".clip-",
                suffix=".tmp",
                delete=False,
            ) as handle:
                temporary = Path(handle.name)
                with wave.open(handle, "wb") as wav:
                    wav.setnchannels(1)
                    wav.setsampwidth(2)
                    wav.setframerate(_AUDIO_RATE)
                    wav.writeframes(pcm.tobytes())
                handle.flush()
                os.fsync(handle.fileno())
            os.chmod(temporary, 0o600)
            os.replace(temporary, target)
        finally:
            if temporary is not None and temporary.exists():
                temporary.unlink()
        return target

    def load_clip(self, value: str | Path) -> tuple[np.ndarray, int]:
        if not self.enabled:
            raise RuntimeError("history is disabled")
        path = Path(value)
        try:
            resolved = path.resolve()
            resolved.relative_to(self.clip_dir.resolve())
        except (OSError, ValueError) as exc:
            raise ValueError("audio clip is outside the LocalFlow clips directory") from exc
        with wave.open(str(resolved), "rb") as audio:
            if audio.getnchannels() != 1 or audio.getsampwidth() != 2:
                raise ValueError("audio clip must be mono 16-bit PCM")
            samples = np.frombuffer(audio.readframes(audio.getnframes()), dtype="<i2")
            return samples.astype(np.float32) / 32768.0, audio.getframerate()

    def prune_audio_clips(self, keep: int) -> int:
        if not self.enabled:
            return 0
        keep = max(0, int(keep))
        with self._lock, self._connect() as connection:
            rows = connection.execute(
                """
                SELECT id, audio_path FROM dictations
                WHERE kind = 'dictate' AND status = 'ok' AND audio_path IS NOT NULL
                ORDER BY ts DESC, id DESC
                """
            ).fetchall()
            expired = rows[keep:]
            connection.executemany(
                "UPDATE dictations SET audio_path = NULL WHERE id = ?",
                ((row["id"],) for row in expired),
            )
        for row in expired:
            self._remove_clip(row["audio_path"])
        return len(expired)

    def _remove_clip(self, value: str | Path | None) -> None:
        if value is None:
            return
        path = Path(value)
        try:
            resolved = path.resolve()
            resolved.relative_to(self.clip_dir.resolve())
        except (OSError, ValueError):
            return
        try:
            resolved.unlink(missing_ok=True)
        except OSError:
            log.warning("could not remove audio clip: %s", resolved, exc_info=True)

    def delete_clip(self, value: str | Path | None) -> None:
        self._remove_clip(value)
