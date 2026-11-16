"""`lf tweak` and the case-insensitive `lf hub --page`."""

import pytest
import requests
from click.testing import CliRunner

# Sibling test module (pytest puts tests/ on sys.path): the same fake Ollama.
from test_settings_agent import FakeOllama, call

from localflow import cli as cli_module
from localflow import settings_agent as sa
from localflow.config import Config, _config_from_v2, _parse, save_config
from localflow.dictionary import DictionaryStore


@pytest.fixture
def tweak_env(tmp_path, monkeypatch):
    config_path = tmp_path / "config.toml"
    save_config(Config(), config_path)
    sessions = []

    def agent():
        session = FakeOllama([
            call("set_setting", key="sounds_enabled", value="off"),
            call("add_dictionary_word", word="Kubernetes"),
        ])
        sessions.append(session)
        return sa.SettingsAgent(
            config_path=config_path,
            dictionary=DictionaryStore(tmp_path / "dictionary.json"),
            undo_path=tmp_path / "settings-undo.json",
            session=session,
            platform="darwin",
            templates=lambda _c: ("general",),
        )

    monkeypatch.setattr(cli_module, "_tweak_agent", agent)
    return {
        "config": lambda: _config_from_v2(_parse(config_path)),
        "words": lambda: DictionaryStore(tmp_path / "dictionary.json").words(),
        "undo": tmp_path / "settings-undo.json",
        "sessions": sessions,
    }


def run(*args, input=None):
    return CliRunner().invoke(cli_module.cli, ["tweak", *args], input=input)


def test_preview_then_cancel_changes_nothing(tweak_env):
    result = run("turn off sounds and learn Kubernetes", input="n\n")

    assert result.exit_code == 0, result.output
    assert "Preview (runs on your computer with gemma4:e4b):" in result.output
    assert "Sounds: On → Off" in result.output
    assert "Apply these changes?" in result.output
    assert "Nothing changed." in result.output
    assert tweak_env["config"]().sounds_enabled is True
    assert tweak_env["words"]() == []
    assert not tweak_env["undo"].exists()


def test_confirm_applies_and_undo_reverts(tweak_env):
    # Both changes are named: unnamed ones are refused (test_settings_security).
    result = run("turn off sounds and learn Kubernetes", input="y\n")
    assert result.exit_code == 0, result.output
    assert "Applied 2 changes." in result.output
    assert "lf tweak --undo" in result.output
    assert tweak_env["config"]().sounds_enabled is False
    assert tweak_env["words"]() == ["Kubernetes"]

    undone = run("--undo")
    assert undone.exit_code == 0, undone.output
    assert "Undid the last change." in undone.output
    assert tweak_env["config"]().sounds_enabled is True
    assert tweak_env["words"]() == []
    # Undo is local file work: it never calls the model.
    assert len(tweak_env["sessions"]) == 2 and tweak_env["sessions"][1].requests == []


def test_yes_skips_the_prompt(tweak_env):
    result = run("turn off sounds", "--yes")
    assert result.exit_code == 0, result.output
    assert "Apply these changes?" not in result.output
    assert tweak_env["config"]().sounds_enabled is False


def test_eof_at_the_prompt_aborts_without_changes(tweak_env):
    result = run("turn off sounds", input="")
    assert result.exit_code != 0
    assert "Traceback" not in result.output
    assert tweak_env["config"]().sounds_enabled is True


@pytest.mark.parametrize("args", [(), ("   ",), ("--yes",)])
def test_missing_request_is_a_usage_error_without_prompting(tweak_env, args):
    result = run(*args)
    assert result.exit_code == 2
    assert "Apply these changes?" not in result.output
    assert tweak_env["sessions"] == []


@pytest.mark.parametrize("args", [
    ("--undo", "turn off sounds"), ("--undo", "--yes"), ("--bogus", "x"),
])
def test_invalid_flag_combinations_are_usage_errors(tweak_env, args):
    result = run(*args)
    assert result.exit_code == 2
    assert tweak_env["sessions"] == []


def test_agent_errors_are_clean_messages(tweak_env, monkeypatch):
    def broken():
        agent = sa.SettingsAgent(
            config_path=tweak_env["undo"].parent / "config.toml",
            dictionary=DictionaryStore(tweak_env["undo"].parent / "d.json"),
            undo_path=tweak_env["undo"],
            session=FakeOllama(error=requests.ConnectionError("refused")),
        )
        return agent

    monkeypatch.setattr(cli_module, "_tweak_agent", broken)
    result = run("turn off sounds", "--yes")
    assert result.exit_code == 1
    assert "Error: Couldn't reach Ollama at http://localhost:11434" in result.output
    assert "Traceback" not in result.output

    nothing = run("--undo")
    assert nothing.exit_code == 1
    assert "There's no change to undo." in nothing.output


@pytest.mark.parametrize("error", [
    requests.ReadTimeout("read timed out"), requests.ConnectionError("reset by peer"),
])
def test_body_read_failures_are_clean_messages(tweak_env, monkeypatch, tmp_path, error):
    from test_settings_agent import FakeResponse

    def flaky():
        return sa.SettingsAgent(
            config_path=tmp_path / "config.toml",
            dictionary=DictionaryStore(tmp_path / "dictionary.json"),
            undo_path=tweak_env["undo"],
            session=FakeOllama(chat=FakeResponse(200, {"message": {}}, error=error)),
            platform="darwin",
            templates=lambda _c: ("general",),
        )

    monkeypatch.setattr(cli_module, "_tweak_agent", flaky)
    result = run("turn off sounds", "--yes")
    assert result.exit_code == 1
    assert result.output.startswith("Error: ")
    assert "Ollama" in result.output and "Traceback" not in result.output
    assert not isinstance(result.exception, requests.RequestException)
    assert tweak_env["config"]().sounds_enabled is True
    assert not tweak_env["undo"].exists()


@pytest.mark.parametrize(("typed", "page"), [
    ("settings", "Settings"), ("Settings", "Settings"), ("hot phrases", "Hot Phrases"),
    ("MODELS", "Models"), ("Home", "Home"),
])
def test_hub_page_is_case_insensitive_and_canonical(monkeypatch, typed, page):
    launched = []
    monkeypatch.setattr(cli_module.subprocess, "Popen", lambda argv: launched.append(argv))
    result = CliRunner().invoke(cli_module.cli, ["hub", "--page", typed])
    assert result.exit_code == 0, result.output
    assert launched[-1][-2:] == ["--page", page]


def test_hub_rejects_unknown_pages(monkeypatch):
    monkeypatch.setattr(cli_module.subprocess, "Popen", lambda argv: None)
    result = CliRunner().invoke(cli_module.cli, ["hub", "--page", "secrets"])
    assert result.exit_code == 2
