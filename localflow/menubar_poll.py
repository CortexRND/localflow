"""Menu bar polling policy, kept free of rumps/AppKit so it can be tested.

The menu bar used to run two timers on the main thread: every 0.5 s it read
status.json and spawned `ps` to check the writer's pid, and every 3 s it made
a blocking HTTP request (3 s timeout) to the local server. Now one 0.5 s
timer does a `stat` (see status.LiveStateCache) and asks this poller for
news; the HTTP request runs on a short-lived background thread on an
adaptive schedule and the main thread only applies finished results.
"""

from __future__ import annotations

import threading
import time
from collections.abc import Callable

# Seconds between meeting-status requests.
ACTIVE_INTERVAL = 3.0   # a meeting is being recorded: keep the elapsed time fresh
IDLE_INTERVAL = 5.0     # watching; the server needs >= 12 s of mic use to detect
DOWN_MIN_INTERVAL = 5.0
DOWN_MAX_INTERVAL = 30.0
REQUEST_TIMEOUT = 2.0   # loopback; a hung server must not pile up threads


class MeetingStatusPoller:
    """Runs `fetch()` off the main thread when due; `tick()` returns results.

    `fetch` returns the status dict or raises when the server is down.
    `tick()` (main thread) returns ("ok", status) / ("down", None) once per
    finished request, else None. At most one request is in flight.
    """

    def __init__(
        self,
        fetch: Callable[[], dict],
        clock: Callable[[], float] = time.monotonic,
        spawn: Callable[[Callable[[], None]], None] | None = None,
    ):
        self._fetch = fetch
        self._clock = clock
        self._spawn = spawn or _daemon
        self._lock = threading.Lock()
        self._result = None
        self._in_flight = False
        self._next_due = 0.0
        self._down_interval = DOWN_MIN_INTERVAL

    def poke(self) -> None:
        """Ask for a request on the next tick (after a user action)."""
        self._next_due = 0.0

    def _run(self) -> None:
        try:
            status = self._fetch()
            result = ("ok", status if isinstance(status, dict) else {})
        except Exception:  # noqa: BLE001 - any failure means "server down"
            result = ("down", None)
        with self._lock:
            self._result = result
            self._in_flight = False

    def tick(self):
        now = self._clock()
        with self._lock:
            result, self._result = self._result, None
            start = not self._in_flight and now >= self._next_due
            if start:
                self._in_flight = True
        if result is not None:
            self._schedule(result, now)
        if start:
            # Until the request finishes, a later tick must not start another.
            self._next_due = now + DOWN_MAX_INTERVAL
            self._spawn(self._run)
        return result

    def _schedule(self, result, now: float) -> None:
        kind, status = result
        if kind == "down":
            interval = self._down_interval
            self._down_interval = min(self._down_interval * 2, DOWN_MAX_INTERVAL)
        else:
            self._down_interval = DOWN_MIN_INTERVAL
            interval = ACTIVE_INTERVAL if status.get("session") else IDLE_INTERVAL
        # A poke while the request was in flight still wins.
        if self._next_due != 0.0:
            self._next_due = now + interval


def _daemon(target: Callable[[], None]) -> None:
    threading.Thread(target=target, name="menubar-status", daemon=True).start()
