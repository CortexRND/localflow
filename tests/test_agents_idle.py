"""Zero idle cost: with the feature off nothing of localflow.agents loads or runs."""

import os
import subprocess
import sys
import threading

import pytest
from fixtures.agents.support import (
    agents_config,
    api_client,
    isolate,
    make_manager,
    wait_for,
)

from localflow import server
from localflow.agents import api as agents_api

pytest.importorskip("sounddevice")

_CHILD = r"""
import sys, threading
import numpy as np
from unittest.mock import Mock
from localflow import app
from localflow.config import Config
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.hotphrases import HotPhraseStore
from localflow.jobs import Job
tmp = sys.argv[1]

def loaded():
    return sorted(m for m in sys.modules if m.startswith("localflow.agents"))

app.paste_text = lambda text, method: None
text, error = app._process_job(
    Job(kind="dictate", audio=np.full(16000, 0.1, np.float32)), Config(meetings_enabled=False),
    Mock(transcribe=Mock(return_value="launch claude to fix the flaky test")),
    Mock(clean=Mock(side_effect=lambda v, **k: v)), Mock(), (),
    HotPhraseStore(tmp + "/phrases.json"), DictionaryStore(tmp + "/dictionary.json"),
    HistoryStore(tmp + "/history.sqlite", tmp + "/clips"))
assert text == "launch claude to fix the flaky test", text
assert loaded() == [], loaded()

from fastapi.testclient import TestClient
from localflow import server
server._config = Config(meetings_enabled=False, meeting_watch=False)
server._api_token = "t"
client = TestClient(server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1),
                    headers={"X-Localflow-Token": "t"})
for path in ("/healthz", "/api/config", "/api/secrets", "/prompts", "/api/providers/llm"):
    assert client.get(path).status_code == 200, path
assert loaded() == [], loaded()
assert not [t.name for t in threading.enumerate() if t.name.startswith("lf-agents")]
print("IDLE-OK")
"""


def test_feature_off_imports_nothing_in_the_app_or_server(tmp_path):
    # HOME stays (pynput needs the display's auth file); every store path is explicit.
    env = {**os.environ, "LOCALFLOW_CONFIG": str(tmp_path / "c.toml"),
           "XDG_CONFIG_HOME": str(tmp_path / "xdg"),  # the server writes its API token there
           "LOCALFLOW_AGENTS_DIR": str(tmp_path / "agents"),
           "PYTHON_KEYRING_BACKEND": "keyring.backends.fail.Keyring"}
    result = subprocess.run([sys.executable, "-c", _CHILD, str(tmp_path)], env=env,
                            capture_output=True, text=True, timeout=180, check=False)
    assert result.returncode == 0, result.stderr[-3000:]
    assert "IDLE-OK" in result.stdout
    assert not (tmp_path / "agents").exists()


def _agent_threads(baseline=frozenset()) -> list[str]:
    """lf-agents-* threads, minus ones an earlier test left behind (`baseline`)."""
    return [t.name for t in threading.enumerate()
            if t.name.startswith("lf-agents") and t.ident not in baseline]


def _baseline() -> frozenset:
    return frozenset(t.ident for t in threading.enumerate())


def test_feature_on_without_sessions_runs_nothing(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)
    baseline = _baseline()
    config = agents_config(tmp_path)
    manager = make_manager(tmp_path, lambda: server._config, supervise=True)
    client = api_client(monkeypatch, config, manager)
    try:
        assert client.get("/api/agents/status").status_code == 200
        assert client.get("/api/agents/providers").status_code == 200
        assert client.get("/api/agents/sessions").status_code == 200
        assert _agent_threads(baseline) == []
        assert manager._providers == {} and not manager.supervisor_alive
        response = client.post("/api/agents/sessions", json={"prompt": "hi", "provider": "mock"})
        session_id = response.json()["session"]["id"]
        wait_for(lambda: client.get(f"/api/agents/sessions/{session_id}").json()["session"][
            "state"] == "completed")
        wait_for(lambda: not manager.supervisor_alive, timeout=1.5)
        wait_for(lambda: _agent_threads(baseline) == [], timeout=1.5)
    finally:
        manager.shutdown()
        agents_api.set_manager(None)


def test_dictation_with_the_feature_on_but_no_rules_reads_one_file(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)
    baseline = _baseline()
    from localflow.agents.handoff import HotCommandSeam
    from localflow.agents.store import HotCommandStore

    store = HotCommandStore(tmp_path / "hot_commands.json")
    seam = HotCommandSeam(store, launcher=object(), known_provider=lambda _id: True)
    calls = []
    real = store.file.signature
    monkeypatch.setattr(store.file, "signature", lambda: calls.append(1) or real())

    class Cfg:
        agents_enabled = True
        agent_hot_commands = True
        agent_repos = ()
        agent_default_repo = ""
        spoken_punctuation = True

    assert seam.intercept("launch claude to fix the tests", Cfg()) is False
    assert len(calls) == 1  # one stat, no read, no thread
    assert _agent_threads(baseline) == []


def test_the_handoff_thread_exits_when_idle():
    from localflow.agents.handoff import HotCommandLauncher
    from localflow.agents.hotcommands import HotCommandMatch

    posted = []
    launcher = HotCommandLauncher(post=lambda url, payload, token: posted.append(payload),
                                  play=lambda cue: None)
    launcher.idle_seconds = 0.05
    launcher.submit(HotCommandMatch("r1", "claude", "", "", "x y z"))
    thread = launcher._thread
    wait_for(lambda: not thread.is_alive(), timeout=2)
    assert len(posted) == 1 and launcher._thread is None
    launcher.submit(HotCommandMatch("r1", "claude", "", "", "x y z"))  # starts a new one
    wait_for(lambda: len(posted) == 2, timeout=2)
