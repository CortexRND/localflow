"""Personal vocabulary and transcription corrections."""

from __future__ import annotations

import csv
import io
import json
import logging
import os
import re
import threading
from contextlib import contextmanager
from pathlib import Path
from tempfile import NamedTemporaryFile

try:
    import fcntl
except ImportError:  # non-POSIX; the store falls back to in-process locking
    fcntl = None

try:
    import msvcrt
except ImportError:
    msvcrt = None

log = logging.getLogger("localflow.dictionary")

DICTIONARY_PATH = Path("~/.localflow/dictionary.json").expanduser()
MAX_IMPORT_BYTES = 3 * 1024 * 1024
MAX_IMPORT_ENTRIES = 1000
MAX_PROMPT_CHARS = 800


class DictionaryError(ValueError):
    """Invalid dictionary entry or import file."""


def _word(value: object) -> str | None:
    return value.strip() if isinstance(value, str) and value.strip() else None


def _normalize_data(data: object) -> dict[str, list]:
    if not isinstance(data, dict):
        raise DictionaryError("dictionary JSON must be an object")
    raw_words = data.get("words", [])
    raw_replacements = data.get("replacements", [])
    if not isinstance(raw_words, list) or not isinstance(raw_replacements, list):
        raise DictionaryError("dictionary needs words and replacements lists")
    words = []
    seen_words = set()
    for value in raw_words:
        normalized = _word(value)
        if normalized and normalized.casefold() not in seen_words:
            words.append(normalized)
            seen_words.add(normalized.casefold())
    replacements = []
    sources = set()
    for value in raw_replacements:
        if not isinstance(value, dict):
            continue
        source, target = _word(value.get("from")), _word(value.get("to"))
        if source and target and source.casefold() not in sources:
            replacements.append({"from": source, "to": target})
            sources.add(source.casefold())
    return {"words": words, "replacements": replacements}


class DictionaryStore:
    """mtime-aware, atomic JSON dictionary shared by the app and CLI."""

    def __init__(self, path: Path | None = None):
        self.path = Path(path) if path is not None else DICTIONARY_PATH
        self._lock = threading.Lock()
        self._lock_path = self.path.with_suffix(self.path.suffix + ".lock")
        self._cache: dict[str, list] | None = None
        self._cache_stat: tuple[int, int] | None = None

    @contextmanager
    def _exclusive(self):
        with self._lock:
            if fcntl is None and msvcrt is None:
                yield
                return
            try:
                self.path.parent.mkdir(parents=True, exist_ok=True)
                lock_file = self._lock_path.open("a+b" if fcntl is None else "a+")
            except OSError:
                log.warning(
                    "dictionary lock unavailable, proceeding unlocked: %s",
                    self._lock_path,
                    exc_info=True,
                )
                yield
                return
            with lock_file as handle:
                try:
                    if fcntl is not None:
                        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
                    else:
                        handle.seek(0)
                        msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
                except OSError:
                    log.warning(
                        "dictionary lock unavailable, proceeding unlocked: %s",
                        self._lock_path,
                        exc_info=True,
                    )
                    yield
                    return
                try:
                    yield
                finally:
                    if fcntl is not None:
                        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
                    else:
                        handle.seek(0)
                        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)

    def _read(self) -> dict[str, list]:
        try:
            return _normalize_data(json.loads(self.path.read_text(encoding="utf-8")))
        except FileNotFoundError:
            return {"words": [], "replacements": []}
        except Exception:
            log.exception("dictionary unreadable; treating as empty: %s", self.path)
            return {"words": [], "replacements": []}

    def _write(self, data: dict[str, list]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = None
        try:
            with NamedTemporaryFile(
                "w",
                encoding="utf-8",
                dir=self.path.parent,
                prefix=f".{self.path.name}.",
                suffix=".tmp",
                delete=False,
            ) as handle:
                temporary = Path(handle.name)
                json.dump(data, handle, ensure_ascii=False, indent=2)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, self.path)
        finally:
            if temporary is not None and temporary.exists():
                temporary.unlink()
        self._cache = {
            "words": list(data["words"]),
            "replacements": [dict(item) for item in data["replacements"]],
        }
        stat = self.path.stat()
        self._cache_stat = (stat.st_mtime_ns, stat.st_size)

    def _snapshot(self) -> dict[str, list]:
        try:
            stat = self.path.stat()
        except FileNotFoundError:
            self._cache = {"words": [], "replacements": []}
            self._cache_stat = None
            return self._cache
        except OSError:
            log.exception("dictionary stat failed; treating as empty: %s", self.path)
            self._cache = {"words": [], "replacements": []}
            self._cache_stat = None
            return self._cache
        signature = (stat.st_mtime_ns, stat.st_size)
        if self._cache_stat == signature and self._cache is not None:
            return {
                "words": list(self._cache["words"]),
                "replacements": [dict(item) for item in self._cache["replacements"]],
            }
        with self._exclusive():
            data = self._read()
        self._cache = data
        self._cache_stat = signature
        return {
            "words": list(data["words"]),
            "replacements": [dict(item) for item in data["replacements"]],
        }

    def list(self) -> dict[str, list]:
        return self._snapshot()

    def words(self) -> list[str]:
        return self._snapshot()["words"]

    def replacements(self) -> list[tuple[str, str]]:
        return [
            (item["from"], item["to"])
            for item in self._snapshot()["replacements"]
        ]

    def add_word(self, word: str) -> bool:
        value = _word(word)
        if value is None:
            raise DictionaryError("word is empty")
        with self._exclusive():
            data = self._read()
            if value.casefold() in {item.casefold() for item in data["words"]}:
                return False
            data["words"].append(value)
            self._write(data)
        return True

    def add_replacement(self, source: str, target: str) -> bool:
        source_value, target_value = _word(source), _word(target)
        if source_value is None or target_value is None:
            raise DictionaryError("replacement source and target must be non-empty")
        with self._exclusive():
            data = self._read()
            if source_value.casefold() in {
                item["from"].casefold() for item in data["replacements"]
            }:
                return False
            data["replacements"].append(
                {"from": source_value, "to": target_value}
            )
            self._write(data)
        return True

    def remove(self, value: str) -> bool:
        normalized = _word(value)
        if normalized is None:
            return False
        folded = normalized.casefold()
        with self._exclusive():
            data = self._read()
            words = [
                item for item in data["words"] if item.casefold() != folded
            ]
            replacements = [
                item
                for item in data["replacements"]
                if item["from"].casefold() != folded
            ]
            removed = len(words) != len(data["words"]) or len(replacements) != len(
                data["replacements"]
            )
            if removed:
                data = {"words": words, "replacements": replacements}
                self._write(data)
        return removed

    def import_file(self, path: Path) -> dict[str, int]:
        source = Path(path)
        raw = source.read_bytes()
        if len(raw) > MAX_IMPORT_BYTES:
            raise DictionaryError("import file exceeds 3 MB")
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise DictionaryError("import file must be UTF-8") from exc
        suffix = source.suffix.casefold()
        if suffix == ".csv":
            candidates, invalid = _csv_entries(text)
        elif suffix == ".json":
            candidates, invalid = _json_entries(text)
        else:
            raise DictionaryError("import file must be CSV or JSON")
        if len(candidates) > MAX_IMPORT_ENTRIES:
            raise DictionaryError("import file contains more than 1000 valid entries")

        counts = {
            "added": 0,
            "skipped_duplicate": 0,
            "skipped_invalid": invalid,
        }
        with self._exclusive():
            data = self._read()
            words = {item.casefold() for item in data["words"]}
            replacements = {
                item["from"].casefold() for item in data["replacements"]
            }
            for kind, first, second in candidates:
                if kind == "word":
                    key = first.casefold()
                    if key in words:
                        counts["skipped_duplicate"] += 1
                    else:
                        data["words"].append(first)
                        words.add(key)
                        counts["added"] += 1
                else:
                    key = first.casefold()
                    if key in replacements:
                        counts["skipped_duplicate"] += 1
                    else:
                        data["replacements"].append({"from": first, "to": second})
                        replacements.add(key)
                        counts["added"] += 1
            if counts["added"]:
                self._write(data)
        return counts


def _csv_entries(text: str) -> tuple[list[tuple[str, str, str]], int]:
    rows = list(csv.reader(io.StringIO(text)))
    if rows and [cell.strip().casefold() for cell in rows[0]] in (
        ["word"],
        ["from", "to"],
    ):
        rows.pop(0)
    candidates = []
    invalid = 0
    for row in rows:
        if len(row) == 1:
            word = _word(row[0])
            if word is None:
                invalid += 1
            else:
                candidates.append(("word", word, ""))
        elif len(row) == 2:
            source, target = _word(row[0]), _word(row[1])
            if source is None or target is None:
                invalid += 1
            else:
                candidates.append(("replacement", source, target))
        else:
            invalid += 1
    return candidates, invalid


def _json_entries(text: str) -> tuple[list[tuple[str, str, str]], int]:
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise DictionaryError(f"invalid JSON: {exc.msg}") from exc
    if not isinstance(data, dict):
        raise DictionaryError("dictionary JSON must be an object")
    candidates = []
    invalid = 0
    words = data.get("words", [])
    replacements = data.get("replacements", [])
    if not isinstance(words, list) or not isinstance(replacements, list):
        raise DictionaryError("dictionary JSON needs words and replacements lists")
    for value in words:
        word = _word(value)
        if word is None:
            invalid += 1
        else:
            candidates.append(("word", word, ""))
    for value in replacements:
        if not isinstance(value, dict):
            invalid += 1
            continue
        source, target = _word(value.get("from")), _word(value.get("to"))
        if source is None or target is None:
            invalid += 1
        else:
            candidates.append(("replacement", source, target))
    return candidates, invalid


def apply_replacements(
    text: str, pairs: list[tuple[str, str]] | list[dict[str, str]]
) -> str:
    normalized = []
    for pair in pairs:
        if isinstance(pair, dict):
            source, target = pair.get("from", ""), pair.get("to", "")
        else:
            source, target = pair
        source = _word(source)
        if source and isinstance(target, str):
            normalized.append((source, target))
    if not normalized:
        return text
    ordered = sorted(normalized, key=lambda pair: len(pair[0]), reverse=True)
    pattern = re.compile(
        r"(?<!\w)(?:" + "|".join(re.escape(source) for source, _ in ordered) + r")(?!\w)",
        re.IGNORECASE,
    )
    targets = {source.casefold(): target for source, target in ordered}
    def replace(match: re.Match) -> str:
        matched = match.group()
        target = targets.get(matched.casefold())
        if target is not None:
            return target
        for source, replacement in ordered:
            if re.fullmatch(re.escape(source), matched, re.IGNORECASE):
                return replacement
        return matched

    return pattern.sub(replace, text)


def stt_prompt(words: list[str] | tuple[str, ...]) -> str | None:
    prefix = "Vocabulary: "
    selected = []
    for raw in words:
        word = _word(raw)
        if not word:
            continue
        candidate = prefix + ", ".join([*selected, word]) + "."
        if len(candidate) > MAX_PROMPT_CHARS:
            break
        selected.append(word)
    return prefix + ", ".join(selected) + "." if selected else None
