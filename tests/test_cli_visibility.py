"""Which `lf` commands the help shows, and that hidden ones still work."""

import re
import time
import types

import pytest
import requests
from click.testing import CliRunner

from localflow import cli as cli_module
from localflow import dispatch as D
from localflow.dictionary import DictionaryStore
from localflow.dispatch import PromptQueue
from localflow.history import HistoryStore

VISIBLE_TOP = [
    "autostart",
    "config",
    "dict",
    "dictate",
    "dictation",
    "doctor",
    "history",
    "hub",
    "keys",
    "llm",
    "mcp",
    "meeting",
    "menubar",
    "open",
    "prompts",
    "serve",
    "setup",
    "status",
    "stt",
    "token",
    "tweak",
]
HIDDEN_TOP = [
    "phrases", "recall", "secret", "agent", "ui", "watch", "notes", "models", "stats",
]
VISIBLE_SUB = {
    "prompts": ["dispatch"],
    "dict": ["import"],
    "history": ["clear", "delete", "raw", "retry"],
    "llm": ["pull", "test"],
    "stt": ["download"],
}
HIDDEN_SUB = {
    "prompts": ["list", "show", "approve", "reject"],
    "dict": ["list", "add", "remove"],
    "history": ["list", "search", "copy"],
    "llm": ["list", "use"],
    "stt": ["list", "use"],
}


def _commands_section(output: str) -> list[str]:
    """Command names listed under `Commands:` in Click help output."""
    names, inside = [], False
    for line in output.splitlines():
        if line.strip() == "Commands:":
            inside = True
            continue
        if inside:
            if line and not line.startswith(" "):
                break  # next section heading
            match = re.match(r"^  (\S+)", line)
            if match:
                names.append(match.group(1))
    return names


def _help(*path):
    result = CliRunner().invoke(cli_module.cli, [*path, "--help"])
    assert result.exit_code == 0, result.output
    return result.output


def test_main_help_lists_exactly_the_visible_commands():
    output = _help()

    assert _commands_section(output) == VISIBLE_TOP
    for name in HIDDEN_TOP:
        assert name not in _commands_section(output)
    assert "Most features live in the LocalFlow app (`lf hub`)." in " ".join(output.split())


@pytest.mark.parametrize("group", sorted(VISIBLE_SUB))
def test_group_help_lists_exactly_the_visible_subcommands(group):
    listed = _commands_section(_help(group))

    assert listed == VISIBLE_SUB[group]
    assert not set(listed) & set(HIDDEN_SUB[group])


def test_visible_subcommands_of_other_groups_are_unchanged():
    assert _commands_section(_help("meeting")) == ["dismiss", "start", "stop"]
    assert _commands_section(_help("autostart")) == ["install", "status", "uninstall"]
    assert _commands_section(_help("keys")) == ["import", "list"]
    assert _commands_section(_help("setup")) == ["install-app", "uninstall-app"]


def test_every_hidden_command_is_still_registered_and_marked_hidden():
    for name in HIDDEN_TOP:
        command = cli_module.cli.commands[name]
        assert command.hidden is True, name
    for name in VISIBLE_TOP:
        assert cli_module.cli.commands[name].hidden is False, name
    for group, names in HIDDEN_SUB.items():
        commands = cli_module.cli.commands[group].commands
        for name in names:
            assert commands[name].hidden is True, (group, name)
        for name in VISIBLE_SUB[group]:
            assert commands[name].hidden is False, (group, name)


@pytest.mark.parametrize(
    "path",
    [[name] for name in HIDDEN_TOP]
    + [[group, name] for group, names in HIDDEN_SUB.items() for name in names],
)
def test_hidden_commands_still_answer_their_own_help(path):
    output = _help(*path)

    assert output.startswith("Usage: ")
    assert " ".join(path) in output.splitlines()[0]


def test_hidden_group_subcommands_stay_listed_in_their_own_help():
    assert _commands_section(_help("agent")) == ["install", "status", "uninstall"]
    assert _commands_section(_help("phrases")) == ["install-app", "uninstall-app"]
    assert _commands_section(_help("secret")) == ["set", "status", "unset"]
    assert _commands_section(_help("models")) == ["download", "list", "use"]


# --- hidden aliases still do their work ----------------------------------


def test_hidden_dict_aliases_edit_the_store(tmp_path, monkeypatch):
    path = tmp_path / "dictionary.json"
    monkeypatch.setattr(cli_module, "DictionaryStore", lambda: DictionaryStore(path))
    runner = CliRunner()

    added = runner.invoke(cli_module.cli, ["dict", "add", "Kubernetes"])
    replaced = runner.invoke(
        cli_module.cli, ["dict", "add", "--from", "cooper netties", "--to", "Kubernetes"]
    )
    listed = runner.invoke(cli_module.cli, ["dict", "list"])
    removed = runner.invoke(cli_module.cli, ["dict", "remove", "Kubernetes"])
    after = runner.invoke(cli_module.cli, ["dict", "list"])

    assert added.exit_code == 0 and "added: word: Kubernetes" in added.output
    assert replaced.exit_code == 0
    assert listed.exit_code == 0
    assert "word: Kubernetes" in listed.output
    assert "cooper netties -> Kubernetes" in listed.output
    assert removed.exit_code == 0
    assert "word: Kubernetes" not in after.output
    assert DictionaryStore(path).list()["words"] == []


def test_hidden_prompt_aliases_move_queue_entries(tmp_path, monkeypatch):
    queue_path = tmp_path / "queue.json"
    sample = "### First task\n> Do the first thing.\n\n### Second task\n> Do the second thing.\n"
    prompts = D.parse_work_prompts(sample)
    paths = D.write_prompts(prompts, "Sync", time.time(), prompts_dir=tmp_path)
    first, second = PromptQueue(queue_path).add(prompts, paths, "Sync")
    monkeypatch.setattr(cli_module, "PromptQueue", lambda: PromptQueue(queue_path))
    runner = CliRunner()

    listed = runner.invoke(cli_module.cli, ["prompts", "list"])
    shown = runner.invoke(cli_module.cli, ["prompts", "show", first["id"]])
    approved = runner.invoke(cli_module.cli, ["prompts", "approve", first["id"]])
    rejected = runner.invoke(cli_module.cli, ["prompts", "reject", second["id"]])

    assert listed.exit_code == 0 and first["id"] in listed.output
    assert shown.exit_code == 0 and "Do the first thing." in _flat(shown.output)
    assert approved.exit_code == 0, approved.output
    assert rejected.exit_code == 0, rejected.output
    queue = PromptQueue(queue_path)
    assert queue.get(first["id"])["status"] == "approved"
    assert queue.get(second["id"])["status"] == "rejected"


def test_hidden_history_and_stats_aliases_read_an_empty_store(tmp_path, monkeypatch):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    monkeypatch.setattr(cli_module, "_history_store", lambda: store)
    runner = CliRunner()

    listed = runner.invoke(cli_module.cli, ["history", "list"])
    searched = runner.invoke(cli_module.cli, ["history", "search", "nothing"])
    stats = runner.invoke(cli_module.cli, ["stats"])

    assert listed.exit_code == 0 and searched.exit_code == 0
    assert stats.exit_code == 0 and "Dictations: 0" in stats.output


def test_hidden_stt_use_alias_saves_config(tmp_path, monkeypatch):
    from localflow import config

    path = tmp_path / "config.toml"
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    monkeypatch.setattr(config, "legacy_config_path", lambda: tmp_path / "legacy.toml")
    config.save_config(config.Config())

    result = CliRunner().invoke(cli_module.cli, ["stt", "use", "faster-whisper", "small"])

    assert result.exit_code == 0, result.output
    loaded = config.load_config()
    assert (loaded.stt_backend, loaded.model_size) == ("faster-whisper", "small")


def test_hidden_ui_and_agent_aliases_reach_their_handlers(monkeypatch):
    opened, statuses = [], []
    monkeypatch.setattr(
        cli_module, "open_web_ui", lambda base, path, opener=None: opened.append((base, path))
    )
    monkeypatch.setattr(cli_module, "_autostart_status", lambda: statuses.append(1))
    monkeypatch.setattr(
        cli_module, "current", lambda: types.SimpleNamespace(open_path=lambda target: None)
    )
    runner = CliRunner()

    ui = runner.invoke(cli_module.cli, ["ui"])
    agent = runner.invoke(cli_module.cli, ["agent", "status"])

    assert ui.exit_code == 0 and opened == [(cli_module.BASE, "/settings")]
    assert agent.exit_code == 0 and statuses == [1]


# --- "server not running" hints ------------------------------------------


def _flat(text):
    return " ".join(text.split())


@pytest.mark.parametrize("method", ["get", "post"])
def test_server_down_hint_suggests_the_app_or_lf_serve(monkeypatch, capsys, method):
    def refused(*args, **kwargs):
        raise requests.ConnectionError("refused")

    monkeypatch.setattr(cli_module.requests, method, refused)
    monkeypatch.setattr(cli_module, "read_api_token", lambda: "")

    with pytest.raises(SystemExit) as exc:
        getattr(cli_module, f"_{method}")("/meeting/status")

    assert exc.value.code == 1
    out = _flat(capsys.readouterr().out)
    assert "server not running" in out
    assert "LocalFlow.app" in out and "lf serve" in out


def test_missing_token_hint_suggests_the_app_or_lf_serve(monkeypatch):
    monkeypatch.setattr(cli_module, "read_api_token", lambda: "")

    result = CliRunner().invoke(cli_module.cli, ["token"])

    assert result.exit_code == 1
    out = _flat(result.output)
    assert "no API token yet" in out
    assert "LocalFlow.app" in out and "lf serve" in out


def test_cli_requests_go_to_the_configured_server(monkeypatch):
    seen = []

    class Response:
        ok = True

        def raise_for_status(self):
            pass

        def json(self):
            return {"ok": True}

    def fake(url, **kwargs):
        seen.append(url)
        return Response()

    monkeypatch.setattr(cli_module, "BASE", "http://[::1]:9911")
    monkeypatch.setattr(cli_module.requests, "get", fake)
    monkeypatch.setattr(cli_module.requests, "post", fake)
    monkeypatch.setattr(cli_module, "read_api_token", lambda: "")

    assert cli_module._get("/meeting/status") == {"ok": True}
    assert cli_module._post("/meeting/stop") == {"ok": True}
    assert seen == ["http://[::1]:9911/meeting/status", "http://[::1]:9911/meeting/stop"]
