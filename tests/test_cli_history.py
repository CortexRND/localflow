import pyperclip
from click.testing import CliRunner

from localflow import cli as cli_module
from localflow.history import HistoryStore


def test_history_cli_lists_searches_copies_and_deletes(tmp_path, monkeypatch):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    entry_id = store.add(
        kind="dictate",
        raw_text="Raw words",
        final_text="Clean words",
        status="ok",
        app_name="Editor",
        ts=1,
    )
    monkeypatch.setattr(cli_module, "_history_store", lambda: store)
    copied = []
    monkeypatch.setattr(pyperclip, "copy", copied.append)
    runner = CliRunner()

    listed = runner.invoke(cli_module.cli, ["history", "list"])
    searched = runner.invoke(cli_module.cli, ["history", "search", "raw"])
    filtered = runner.invoke(
        cli_module.cli, ["history", "--limit", "1", "--search", "raw"]
    )
    copied_final = runner.invoke(
        cli_module.cli, ["history", "copy", str(entry_id)]
    )
    copied_last = runner.invoke(cli_module.cli, ["history", "copy", "last"])
    copied_raw = runner.invoke(cli_module.cli, ["history", "raw", str(entry_id)])
    deleted = runner.invoke(
        cli_module.cli, ["history", "delete", str(entry_id)]
    )

    assert listed.exit_code == 0
    assert "Clean words" in listed.output
    assert searched.exit_code == 0
    assert "Editor" in searched.output
    assert filtered.exit_code == 0
    assert "Editor" in filtered.output
    assert copied_final.exit_code == 0
    assert copied_last.exit_code == 0
    assert copied_raw.exit_code == 0
    assert copied == ["Clean words", "Clean words", "Raw words"]
    assert deleted.exit_code == 0
    assert store.get(entry_id) is None


def test_history_clear_requires_yes_and_stats_are_reported(tmp_path, monkeypatch):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    store.add(kind="dictate", final_text="one two", status="ok", duration_s=1)
    monkeypatch.setattr(cli_module, "_history_store", lambda: store)
    runner = CliRunner()

    stats = runner.invoke(cli_module.cli, ["stats"])
    refused = runner.invoke(cli_module.cli, ["history", "clear", "--yes"])

    assert stats.exit_code == 0
    assert "Dictations: 1" in stats.output
    assert "Words: 2" in stats.output
    assert refused.exit_code == 0
    assert "Cleared 1 history entries." in refused.output
    assert store.recent() == []


def test_history_clear_can_be_cancelled(tmp_path, monkeypatch):
    store = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    store.add(kind="dictate", final_text="keep me", status="ok")
    monkeypatch.setattr(cli_module, "HistoryStore", lambda: store)
    result = CliRunner().invoke(cli_module.cli, ["history", "clear"], input="n\n")

    assert result.exit_code != 0
    assert store.recent()
