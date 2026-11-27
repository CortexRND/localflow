import logging
import time
from types import SimpleNamespace
from unittest.mock import Mock

import numpy as np
import pytest

pytest.importorskip("sounddevice")

from fixtures.agents.stub_agent_server import StubAgentServer
from fixtures.agents.support import isolate

from localflow import app
from localflow.agents import rules as rule_ops
from localflow.agents.handoff import HotCommandLauncher, HotCommandSeam
from localflow.agents.store import HotCommandStore
from localflow.dictionary import DictionaryStore
from localflow.history import HistoryStore
from localflow.hotphrases import HotPhraseStore
from localflow.jobs import Job

AUDIO = np.full(16000, 0.1, np.float32)
KNOWN = {"claude", "devin", "mock", "http"}.__contains__


def _config(**overrides):
    return SimpleNamespace(**{**BASE_CONFIG, **overrides})


BASE_CONFIG = {
    "cleanup_enabled": False, "cleanup_level": "light", "styles_enabled": False,
    "style_work": "formal", "style_email": "formal", "style_personal": "casual",
    "style_other": "formal", "app_category_overrides": [], "spoken_symbols": True,
    "spoken_punctuation": True, "silence_threshold_dbfs": -50.0, "paste_method": "auto",
    "hot_phrases": True, "sample_rate": 16000, "keep_audio_clips": 5, "history_enabled": True,
    "sounds_enabled": True, "command_names": (), "skills_dirs": (), "llm_provider": "ollama",
    "llm_model": "x", "agents_enabled": True, "agent_hot_commands": True,
    "agent_repos": ["localflow=/r/localflow"], "agent_default_repo": "",
    "agent_default_provider": "claude", "perf_metrics": False,
}


class RecordingLauncher:
    def __init__(self):
        self.matches = []

    def submit(self, match):
        self.matches.append(match)
        return "0" * 12


@pytest.fixture
def env(tmp_path, monkeypatch):
    isolate(tmp_path, monkeypatch)
    rules = HotCommandStore(tmp_path / "hot_commands.json")
    from localflow.config import Config

    cfg = Config(agent_repos=["localflow=/r/localflow"])
    rule = rule_ops.add_rule(rules, {"trigger": "launch claude", "provider": "claude"}, cfg,
                             known_provider=KNOWN, phrases=[])
    clock = {"now": 100.0}
    launcher = RecordingLauncher()
    seam = HotCommandSeam(rules, launcher, clock=lambda: clock["now"], known_provider=KNOWN)
    history = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    pasted = []
    monkeypatch.setattr(app, "paste_text", lambda text, method: pasted.append(text))
    return SimpleNamespace(tmp_path=tmp_path, rules=rules, rule=rule, seam=seam,
                           launcher=launcher, history=history, clock=clock, pasted=pasted)


def _job(env, text, config=None, kind="dictate", seam=True):
    transcriber = Mock(transcribe=Mock(return_value=text))
    cleaner = Mock(clean=Mock(side_effect=lambda value, **_kw: value))
    transformer = Mock(run=Mock(return_value="transformed"))
    return app._process_job(
        Job(kind=kind, audio=AUDIO, selection="sel" if kind != "dictate" else ""),
        config or _config(), transcriber, cleaner, transformer, (),
        HotPhraseStore(env.tmp_path / "phrases.json"),
        DictionaryStore(env.tmp_path / "dictionary.json"), env.history,
        hot_command_seam=env.seam if seam else None,
    ), cleaner


def test_a_hot_command_launches_instead_of_pasting(env):
    (text, error), cleaner = _job(env, "Launch Claude in localflow to fix the flaky test.")
    assert (text, error) == (None, False)
    assert len(env.launcher.matches) == 1
    match = env.launcher.matches[0]
    assert (match.rule_id, match.provider, match.repo, match.prompt) == (
        env.rule["id"], "claude", "localflow", "fix the flaky test.")
    row = env.history.recent()[0]
    assert row["kind"] == "command" and row["final_text"] == "" and row["status"] == "ok"
    assert row["raw_text"] == "Launch Claude in localflow to fix the flaky test."
    assert not row["audio_path"]
    assert not list((env.tmp_path / "clips").glob("*"))
    assert env.history.last_ok() is None  # paste-last is unaffected
    assert not cleaner.clean.called


@pytest.mark.parametrize("text", [
    "please launch claude to fix the flaky test",
    "launching claude to fix the flaky test",
    "launch claude",
    "launch claude to fix it",
])
def test_non_matches_dictate_byte_identically(env, text):
    (with_seam, _), _ = _job(env, text)
    (without, _), _ = _job(env, text, config=_config(agents_enabled=False), seam=False)
    assert with_seam == without and with_seam
    assert env.launcher.matches == []


def test_an_unknown_alias_stays_in_the_prompt(env):
    _job(env, "launch claude in nowhere to do the thing")
    assert env.launcher.matches[0].prompt == "in nowhere to do the thing"
    assert env.launcher.matches[0].repo == ""


@pytest.mark.parametrize("flags", [{"agents_enabled": False}, {"agent_hot_commands": False}])
def test_either_flag_off_never_matches(env, flags):
    (text, _), _ = _job(env, "launch claude to fix the flaky test", config=_config(**flags))
    assert text == "launch claude to fix the flaky test" and env.launcher.matches == []
    assert env.history.recent()[0]["kind"] == "dictate"


def test_disabled_rule_falls_through(env):
    from localflow.config import Config

    rule_ops.update_rule(env.rules, env.rule["id"], {"enabled": False}, Config(),
                         known_provider=KNOWN, phrases=[])
    (text, _), _ = _job(env, "launch claude to fix the flaky test")
    assert text and env.launcher.matches == []


def test_cooldown_ignores_a_quick_repeat(env):
    _job(env, "launch claude to fix the flaky test")
    env.clock["now"] += 1
    (text, _), _ = _job(env, "launch claude to fix the flaky test")
    assert text == "launch claude to fix the flaky test" and len(env.launcher.matches) == 1
    env.clock["now"] += 3
    _job(env, "launch claude to fix the flaky test")
    assert len(env.launcher.matches) == 2


@pytest.mark.parametrize("kind", ["command", "transform"])
def test_command_and_transform_jobs_never_intercept(env, kind):
    _job(env, "launch claude to fix the flaky test", kind=kind)
    assert env.launcher.matches == []


def test_history_retry_never_intercepts(env):
    _job(env, "plain words here", seam=False)
    entry = env.history.recent()[0]
    transcriber = Mock(transcribe=Mock(return_value="launch claude to fix the flaky test"))
    retried = app.retry_history_entry(
        entry["id"], env.history, config=_config(), transcriber=transcriber,
        cleaner=Mock(clean=Mock(side_effect=lambda v, **_k: v)),
        dictionary_store=DictionaryStore(env.tmp_path / "dictionary.json"),
        hot_phrases=HotPhraseStore(env.tmp_path / "phrases.json"))
    assert retried == "launch claude to fix the flaky test" and env.launcher.matches == []


def test_a_broken_seam_dictates_normally(env):
    env.seam.intercept = Mock(side_effect=RuntimeError("boom"))
    (text, error), _ = _job(env, "launch claude to fix the flaky test")
    assert text == "launch claude to fix the flaky test" and error is False


def test_the_default_seam_is_built_lazily(env, monkeypatch, tmp_path):
    monkeypatch.setattr(app, "_hot_command_seam", None)
    assert app._agents_seam(_config(agent_hot_commands=False)) is None
    assert app._hot_command_seam is None
    seam = app._agents_seam(_config())
    assert isinstance(seam, HotCommandSeam) and app._agents_seam(_config()) is seam


def test_launcher_posts_to_the_server_and_plays_cues(tmp_path):
    stub = StubAgentServer()
    try:
        stub.script("POST", "/api/agents/sessions", (202, {"session": {}}))
        cues = []
        launcher = HotCommandLauncher(base_url=lambda: stub.url, token=lambda: "tok",
                                      play=cues.append)
        from localflow.agents.hotcommands import HotCommandMatch

        match = HotCommandMatch("r1", "claude", "", "localflow", "fix the test")
        session_id = launcher.submit(match)
        launcher.join()
        request = stub.requests[0]
        assert request["json"] == {"id": session_id, "provider": "claude",
                                   "prompt": "fix the test", "origin": "hot_command",
                                   "rule_id": "r1", "repo": "localflow"}
        assert request["headers"]["X-Localflow-Token"] == "tok"
        stub.script("POST", "/api/agents/sessions", (409, {"code": "agents_disabled"}))
        launcher.submit(match)
        launcher.join()
        assert cues == ["lock", "error"]
    finally:
        stub.close()


def test_server_down_never_blocks_or_raises(caplog):
    from localflow.agents.hotcommands import HotCommandMatch

    cues = []
    launcher = HotCommandLauncher(base_url=lambda: "http://127.0.0.1:9", token=lambda: None,
                                  play=cues.append, sounds_enabled=lambda: True)
    match = HotCommandMatch("r1", "claude", "", "", "a secret prompt text")
    started = time.perf_counter()
    launcher.submit(match)
    assert time.perf_counter() - started < 0.005
    with caplog.at_level(logging.INFO, logger="localflow.agents.handoff"):
        launcher.join(timeout=15)
    assert cues == ["error"]
    assert "server unreachable" in caplog.text
    assert "a secret prompt text" not in caplog.text and "20 chars" in caplog.text


def test_a_full_queue_drops_with_a_warning(caplog):
    from localflow.agents.hotcommands import HotCommandMatch

    block = Mock()
    launcher = HotCommandLauncher(post=lambda *a: block.wait(), play=lambda cue: None)
    launcher._thread = Mock(is_alive=Mock(return_value=True))  # nothing drains the queue
    match = HotCommandMatch("r1", "claude", "", "", "x y z")
    ids = [launcher.submit(match) for _ in range(9)]
    assert ids[-1] is None and all(ids[:8])
    assert "queue full" in caplog.text
