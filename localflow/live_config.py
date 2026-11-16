"""Pick up config edits in long-running processes without a restart.

The dictation app, the local server and the menu bar each load the config
once at start. `ConfigWatcher` notices when the file changes (one `stat` per
interval on a background thread that sleeps on an Event; no busy polling)
and hands the reloaded config to a callback. `merge_live` copies over only
the settings that are safe to change at runtime (the Settings agent's
ALLOWLIST); anything else still needs a restart and is only logged.
"""

from __future__ import annotations

import logging
import threading
from collections.abc import Callable
from dataclasses import replace
from pathlib import Path

from localflow.config import Config, ConfigError, config_path, load_config
from localflow.settings_agent import ALLOWLIST

log = logging.getLogger("localflow.live_config")

LIVE_FIELDS = frozenset(ALLOWLIST)
HOTKEY_FIELDS = frozenset({
    "hotkey", "command_hotkey", "polish_hotkey", "prompt_hotkey", "paste_last_hotkey",
})
DEFAULT_INTERVAL = 2.0


def merge_live(running: Config, fresh: Config) -> tuple[Config, frozenset[str]]:
    """`running` with the live fields taken from `fresh`; also the changed names."""
    changed = frozenset(
        name for name in LIVE_FIELDS if getattr(running, name) != getattr(fresh, name)
    )
    others = [
        name for name in running.__dataclass_fields__
        if name not in LIVE_FIELDS and getattr(running, name) != getattr(fresh, name)
    ]
    if others:
        log.info("config changed; restart LocalFlow to apply: %s", ", ".join(sorted(others)))
    if not changed:
        return running, changed
    return replace(running, **{name: getattr(fresh, name) for name in changed}), changed


def file_signature(path: Path) -> tuple[int, int, int] | None:
    try:
        stat = path.stat()
    except OSError:
        return None
    return (stat.st_mtime_ns, stat.st_size, stat.st_ino)


class LazyConfig:
    """A config that refreshes its live fields when read after a file change.

    For processes that only need the config on user actions (the menu bar's
    "Paste last"): one `stat` per read, no thread.
    """

    def __init__(
        self,
        config: Config,
        *,
        path: Callable[[], Path] | None = None,
        loader: Callable[[], Config] = load_config,
    ):
        self._config = config
        self._path = path or config_path
        self._loader = loader
        self._signature = file_signature(self._path())

    def get(self) -> Config:
        signature = file_signature(self._path())
        if signature != self._signature:
            self._signature = signature
            try:
                self._config, _changed = merge_live(self._config, self._loader())
            except (ConfigError, OSError, ValueError) as exc:
                log.warning("config reload skipped: %s", exc)
        return self._config


class ConfigWatcher:
    """Calls `on_change(config)` after the config file changes on disk.

    `check()` is the whole step (tests call it directly); `start()` runs it
    every `interval` seconds on a daemon thread, plus `on_tick()` (used to
    retry deferred work such as a pending shortcut rebind).
    """

    def __init__(
        self,
        on_change: Callable[[Config], None],
        *,
        path: Callable[[], Path] | Path | None = None,
        loader: Callable[[], Config] = load_config,
        interval: float = DEFAULT_INTERVAL,
        on_tick: Callable[[], None] | None = None,
    ):
        self._on_change = on_change
        self._path = path if callable(path) else (lambda p=path: p or config_path())
        self._loader = loader
        self.interval = interval
        self._on_tick = on_tick
        self._signature = file_signature(self._path())
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def check(self) -> bool:
        """Reload and notify if the file changed. True when notified."""
        signature = file_signature(self._path())
        if signature == self._signature:
            return False
        self._signature = signature
        try:
            config = self._loader()
        except (ConfigError, OSError, ValueError) as exc:
            # A half-written or invalid file: keep running with what we have.
            log.warning("config reload skipped: %s", exc)
            return False
        try:
            self._on_change(config)
        except Exception:
            log.exception("applying the reloaded config failed")
            return False
        return True

    def _run(self) -> None:
        while not self._stop.wait(self.interval):
            try:
                self.check()
                if self._on_tick is not None:
                    self._on_tick()
            except Exception:
                log.exception("config watcher tick failed")

    def start(self) -> ConfigWatcher:
        if self._thread is None:
            self._thread = threading.Thread(
                target=self._run, name="localflow-config-watcher", daemon=True
            )
            self._thread.start()
        return self

    def stop(self) -> None:
        self._stop.set()
