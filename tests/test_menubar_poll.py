"""Menu bar idle cost: cached status reads and off-main-thread server polls."""

import os
from pathlib import Path

from localflow import menubar_poll as mp
from localflow import status


class Clock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now


def _poller(responses, clock):
    """A poller whose 'thread' runs only when the test says so."""
    pending = []

    def fetch():
        value = responses.pop(0)
        if isinstance(value, Exception):
            raise value
        return value

    poller = mp.MeetingStatusPoller(fetch, clock=clock, spawn=pending.append)
    return poller, pending


def test_requests_run_off_the_main_thread_one_at_a_time():
    clock = Clock()
    poller, pending = _poller([{"session": None}], clock)
    assert poller.tick() is None and len(pending) == 1  # started, not awaited
    clock.now += 60
    assert poller.tick() is None and len(pending) == 1  # still in flight: no pile-up
    pending.pop()()  # the background request finishes
    assert poller.tick() == ("ok", {"session": None})
    assert poller.tick() is None


def test_schedule_adapts_to_meeting_idle_and_down():
    clock = Clock()
    poller, pending = _poller(
        [{"session": {"seconds": 1}}, {"session": None}, OSError("down"),
         OSError("down"), OSError("down"), {"session": None}],
        clock,
    )

    def step():
        poller.tick()
        pending.pop()()
        start = clock.now
        result = poller.tick()
        # Find when the next request starts.
        while not pending:
            clock.now += 0.5
            poller.tick()
        return result, clock.now - start

    assert step() == (("ok", {"session": {"seconds": 1}}), mp.ACTIVE_INTERVAL)
    assert step() == (("ok", {"session": None}), mp.IDLE_INTERVAL)
    assert step() == (("down", None), 5.0)
    assert step() == (("down", None), 10.0)
    assert step() == (("down", None), 20.0)
    result, _gap = step()
    assert result == ("ok", {"session": None})  # recovered: back to the idle pace


def test_poke_after_a_user_action_polls_on_the_next_tick():
    clock = Clock()
    poller, pending = _poller([{"session": None}, {"session": None}], clock)
    poller.tick()
    pending.pop()()
    poller.tick()
    assert not pending
    poller.poke()
    poller.tick()
    assert len(pending) == 1


def test_menubar_uses_one_cheap_timer_and_no_blocking_request(tmp_path):
    source = (Path(mp.__file__).parent / "menubar.py").read_text(encoding="utf-8")
    assert source.count("rumps.Timer(") == 1
    assert "TICK_SECONDS = 0.5" in source
    assert "LiveStateCache()" in source and "live_state()" not in source
    refresh = source.split("def refresh(", 1)[1].split("def ", 1)[0]
    assert "requests." not in refresh  # the main thread only pokes the poller
    assert "_live_config.get().paste_method" in source


def test_status_cache_stats_only_while_idle(tmp_path, monkeypatch):
    path = tmp_path / "status.json"
    checks = []
    monkeypatch.setattr(status, "_pid_is_alive", lambda pid: checks.append(pid) or True)
    clock = Clock()
    cache = status.LiveStateCache(path, pid_recheck=5.0, clock=clock)

    assert cache.state() == "idle"  # no file
    status.write_state("idle", path)
    for _ in range(20):
        assert cache.state() == "idle"
    assert checks == []  # idle never spawns a pid check

    status.write_state("recording", path)
    assert cache.state() == "recording"
    assert checks == [os.getpid()]
    for _ in range(9):
        clock.now += 0.5
        cache.state()
    assert len(checks) == 1  # within pid_recheck: no new subprocess
    clock.now += 1
    cache.state()
    assert len(checks) == 2


def test_status_cache_falls_back_to_idle_for_a_dead_writer(tmp_path, monkeypatch):
    path = tmp_path / "status.json"
    alive = {"value": True}
    monkeypatch.setattr(status, "_pid_is_alive", lambda pid: alive["value"])
    clock = Clock()
    cache = status.LiveStateCache(path, pid_recheck=5.0, clock=clock)
    status.write_state("locked", path)
    assert cache.state() == "locked"
    alive["value"] = False  # recorder crashed while locked
    clock.now += 6
    assert cache.state() == "idle"
    path.write_text("{broken")
    assert cache.state() == "idle"
