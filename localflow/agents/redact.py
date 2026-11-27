"""Secret redaction for everything the launcher persists, returns or logs."""

from __future__ import annotations

import re
import threading
from collections.abc import Callable, Iterable, Mapping

MIN_SECRET_CHARS = 8

_PATTERNS = (
    (re.compile(r"sk-ant-[A-Za-z0-9_\-]{6,}"), "[redacted:anthropic_key]"),
    (re.compile(r"\bapk_[A-Za-z0-9_\-]{6,}"), "[redacted:devin_key]"),
    (re.compile(r"(?i)\b(authorization|x-api-key|proxy-authorization)(\s*[:=]\s*)"
                r"(?:bearer\s+|basic\s+|token\s+)?[^\s,;\"']+"),
     r"\1\2[redacted]"),
    (re.compile(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=\-]{8,}"), "Bearer [redacted]"),
)

_SCRUB_EXACT = frozenset({"FIREWORKS_API_KEY", "DEVIN_API_KEY"})


def scrub_env(env: Mapping[str, str]) -> dict[str, str]:
    """A child environment without any LocalFlow secret."""
    return {
        name: value for name, value in env.items()
        if not name.upper().startswith("LOCALFLOW_") and name.upper() not in _SCRUB_EXACT
    }


class Redactor:
    """Exact-value replacement of every resolvable agent secret plus patterns.

    `names()` returns the secret names to resolve; `get_secret` resolves one.
    `refresh()` rebuilds the value table (after a secret changes).
    """

    def __init__(self, names: Callable[[], Iterable[str]] | None = None,
                 get_secret: Callable[[str], str] | None = None):
        self._names = names or (lambda: ())
        self._get_secret = get_secret or (lambda _name: "")
        self._lock = threading.Lock()
        self._values: list[tuple[str, str]] = []
        self.refresh()

    def refresh(self) -> None:
        values: dict[str, str] = {}
        for name in self._names():
            try:
                value = self._get_secret(name) or ""
            except Exception:  # noqa: BLE001 - a broken keyring never breaks redaction
                value = ""
            if len(value) >= MIN_SECRET_CHARS:
                values[value] = name
        ordered = sorted(values.items(), key=lambda item: -len(item[0]))
        with self._lock:
            self._values = ordered

    def add(self, name: str, value: str) -> None:
        if value and len(value) >= MIN_SECRET_CHARS:
            with self._lock:
                if any(v == value for v, _name in self._values):
                    return
                self._values = sorted(
                    [*(v for v in self._values if v[0] != value), (value, name)],
                    key=lambda item: -len(item[0]),
                )

    def __call__(self, text: str) -> str:
        return self.redact(text)

    def redact_literals(self, text: str) -> str:
        """Replace only exact registered secret values, never generic patterns.

        For identifiers (provider run ids) that must round-trip intact unless they
        literally contain a known secret.
        """
        if not text:
            return text or ""
        with self._lock:
            values = list(self._values)
        for value, name in values:
            if value in text:
                text = text.replace(value, f"[redacted:{name}]")
        return text

    def redact(self, text: str) -> str:
        if not text:
            return text or ""
        text = self.redact_literals(text)
        for pattern, replacement in _PATTERNS:
            text = pattern.sub(replacement, text)
        return text

    def redact_obj(self, value):
        if isinstance(value, str):
            return self.redact(value)
        if isinstance(value, list):
            return [self.redact_obj(v) for v in value]
        if isinstance(value, dict):
            return {k: self.redact_obj(v) for k, v in value.items()}
        return value
