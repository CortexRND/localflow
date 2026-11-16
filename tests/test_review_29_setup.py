"""PR29 review finding 2: finishing first-run setup must bring up the runtime.

First launch: no config, so `WindowRouter.show` opens setup. Setup saves the
config but leaves no page request, and `WindowRouter.poll` (the ~250 ms
run-loop timer) returns early when there is no request, so the runtime is
never ensured and the Hub never opens until the user relaunches.

Also, setup's "Launch localflow now" spawns `python -m localflow.app` itself
instead of going through `main_app.ensure_runtime` (dictation + local server
+ menu bar, each started at most once).

Fakes only: no AppKit, no processes, no real request files.
"""

import ast
import subprocess
import sys
from pathlib import Path
from unittest.mock import Mock

import pytest

from localflow import main_app

ROOT = Path(__file__).resolve().parents[1]
SETUP_MAC = ROOT / "localflow" / "setup_mac.py"


class Harness:
    def __init__(self, monkeypatch, *, configured=False):
        self.configured = configured
        self.requests = []  # page requests the next poll will consume
        self.runtime = 0
        self.hub_pages = []
        self.setups = 0
        monkeypatch.setattr(
            main_app, "consume_request",
            lambda *a, **k: self.requests.pop(0) if self.requests else None,
        )
        self.router = main_app.WindowRouter(
            lambda: self.configured, self._runtime, self._open_hub, self._open_setup,
        )

    def _runtime(self):
        self.runtime += 1

    def _open_hub(self, existing, page):
        self.hub_pages.append(page)
        return existing or object()

    def _open_setup(self, existing):
        self.setups += 1
        return existing or object()


@pytest.fixture
def first_run(monkeypatch):
    harness = Harness(monkeypatch)
    harness.router.show()
    assert harness.setups == 1 and harness.runtime == 0 and harness.hub_pages == []
    return harness


def test_poll_after_setup_saves_starts_runtime_and_opens_the_hub(first_run):
    first_run.configured = True  # setup wrote the config; no page request left

    first_run.router.poll()

    assert first_run.runtime == 1
    assert len(first_run.hub_pages) == 1
    assert first_run.hub_pages[0] in (None, "Home")


def test_saved_route_is_honoured_once_setup_finishes(monkeypatch):
    harness = Harness(monkeypatch)
    harness.router.desired = "Hot Phrases"  # e.g. launched via `lf phrases`
    harness.router.show()
    assert harness.setups == 1 and harness.hub_pages == []
    assert harness.router.desired == "Hot Phrases"  # kept while setup runs

    harness.configured = True
    harness.router.poll()

    assert harness.runtime == 1
    assert harness.hub_pages == ["Hot Phrases"]
    assert harness.router.desired is None


def test_a_request_made_during_setup_wins_over_the_launch_route(monkeypatch):
    harness = Harness(monkeypatch)
    harness.router.desired = "Hot Phrases"
    harness.router.show()
    harness.requests.append("History")
    harness.router.poll()  # still unconfigured: setup comes to the front again
    assert harness.hub_pages == [] and harness.runtime == 0

    harness.configured = True
    harness.router.poll()

    assert harness.hub_pages == ["History"] and harness.runtime == 1


def test_idle_polls_after_the_transition_do_not_restart_anything(first_run):
    first_run.configured = True
    for _ in range(40):  # ~10 s of 250 ms ticks
        first_run.router.poll()

    assert first_run.runtime == 1
    assert len(first_run.hub_pages) == 1


def test_idle_polls_while_unconfigured_start_nothing(first_run):
    for _ in range(40):
        first_run.router.poll()

    assert first_run.runtime == 0 and first_run.hub_pages == []
    assert first_run.setups == 1  # setup is not rebuilt or re-fronted every tick


def test_page_requests_after_the_transition_still_work(first_run):
    first_run.configured = True
    first_run.router.poll()
    first_run.requests.append("Settings")

    assert first_run.router.poll() is True

    assert first_run.hub_pages[-1] == "Settings"
    assert len(first_run.hub_pages) == 2


def test_already_configured_launch_is_not_shown_twice_by_poll(monkeypatch):
    harness = Harness(monkeypatch, configured=True)
    harness.router.show()
    for _ in range(10):
        harness.router.poll()

    assert harness.runtime == 1 and harness.hub_pages == [None]


# ------------------------------------------------- setup's Launch button ---


def setup_method_source(name):
    tree = ast.parse(SETUP_MAC.read_text(encoding="utf-8"))
    cls = next(
        n for n in tree.body
        if isinstance(n, ast.ClassDef) and n.name == "SetupWindowController"
    )
    return next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == name)


def run_launch_action(monkeypatch):
    spawned, ensured = [], []
    monkeypatch.setattr(
        subprocess, "Popen", lambda argv, *a, **k: spawned.append(list(argv)),
    )
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: ensured.append(1))
    method = setup_method_source("launchAction_")
    method.decorator_list = []
    namespace = {}
    exec(  # noqa: S102 - our own source
        compile(ast.Module(body=[method], type_ignores=[]), str(SETUP_MAC), "exec"),
        namespace,
    )
    namespace["launchAction_"](Mock(), None)  # any controller attribute is a fake
    return spawned, ensured


def test_setup_launch_goes_through_ensure_runtime(monkeypatch):
    spawned, ensured = run_launch_action(monkeypatch)

    assert ensured == [1]
    assert [sys.executable, "-m", "localflow.app"] not in spawned


def test_setup_launch_twice_never_spawns_a_second_dictation_process(monkeypatch):
    spawned, ensured = run_launch_action(monkeypatch)
    more_spawned, more_ensured = run_launch_action(monkeypatch)

    assert ensured == [1] and more_ensured == [1]  # each click ensures, idempotently
    assert not any(argv[-1:] == ["localflow.app"] for argv in spawned + more_spawned)
