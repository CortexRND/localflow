"""Pick up config edits in long-running processes without a restart.

The dictation app, the local server and the menu bar each load the config
once at start. `ConfigWatcher` notices when the file changes (one `stat` per
interval on a background thread that sleeps on an Event; no busy polling)
and hands the reloaded config to a callback. `merge_live` copies over only
the settings that are safe to change at runtime (the Settings agent's
ALLOWLIST); anything else still needs a restart and is only logged.

The model choices are reloaded too, but never field by field: `RUNTIME_GROUPS`
names the fields that only make sense together (the LLM provider, endpoint,
model and endpoint-bound key reference; the speech provider, model, language
and device). `runtime_changes` says which groups moved and `adopt_groups`
copies each moved group whole, so a reader can never pair one provider's
endpoint with another's key or model. Rebuilding the clients is the caller's
job (app.RuntimeModels, server._apply_live_config): they swap them between
jobs and never under a running recording or meeting.
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
HOTKEY_FIELDS = frozenset(
    {
        "dictation_toggle_hotkey",
        "hotkey",
        "command_hotkey",
        "polish_hotkey",
        "prompt_hotkey",
        "paste_last_hotkey",
    }
)
DEFAULT_INTERVAL = 2.0
LLM_FIELDS = (
    "llm_provider", "llm_base_url", "llm_model", "llm_key_ref",
    "cleanup_timeout", "cleanup_num_ctx",
)
STT_FIELDS = ("stt_backend", "model_size", "language", "stt_device")
RUNTIME_GROUPS = {"llm": LLM_FIELDS, "stt": STT_FIELDS}
RUNTIME_FIELDS = frozenset(LLM_FIELDS) | frozenset(STT_FIELDS)


def merge_live(
    running: Config, fresh: Config, *, reloaded: frozenset[str] = frozenset(),
) -> tuple[Config, frozenset[str]]:
    """`running` with the live fields taken from `fresh`; also the changed names.

    `reloaded`: fields the caller adopts itself (RUNTIME_FIELDS for the app
    and server), left out of the restart-only log line.
    """
    changed = frozenset(
        name for name in LIVE_FIELDS if getattr(running, name) != getattr(fresh, name)
    )
    others = [
        name for name in running.__dataclass_fields__
        if name not in LIVE_FIELDS and name not in reloaded
        and getattr(running, name) != getattr(fresh, name)
    ]
    if others:
        log.info("config changed; restart LocalFlow to apply: %s", ", ".join(sorted(others)))
    if not changed:
        return running, changed
    return replace(running, **{name: getattr(fresh, name) for name in changed}), changed


def runtime_changes(running: Config, fresh: Config) -> frozenset[str]:
    """Names of the RUNTIME_GROUPS whose fields differ between the configs."""
    return frozenset(
        group for group, fields in RUNTIME_GROUPS.items()
        if any(getattr(running, name) != getattr(fresh, name) for name in fields)
    )


def adopt_groups(running: Config, fresh: Config, groups) -> Config:
    """`running` with every field of each named group taken from `fresh`,
    always as a whole group (never a model without its provider/key ref)."""
    values = {
        name: getattr(fresh, name) for group in groups for name in RUNTIME_GROUPS[group]
    }
    return replace(running, **values) if values else running


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
