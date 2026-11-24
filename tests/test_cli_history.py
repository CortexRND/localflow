import json

import pyperclip
import pytest
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


def _retry_json_setup(tmp_path, monkeypatch, downloaded=True):
    """A data directory (not ~/.localflow) with one failed dictation and one
    older kept clip, and a stubbed model that hears "teh words here"."""
    from dataclasses import replace
    from unittest.mock import Mock

    import numpy as np
    from huggingface_hub.errors import LocalEntryNotFoundError

    from localflow import app
    from localflow.config import Config
    from localflow.dictionary import DictionaryStore

    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    data = tmp_path / "data dir"
    store = HistoryStore(data / "history.sqlite", data / "clips")
    audio = np.full(16000, 0.1, dtype=np.float32)
    kept = store.add(kind="dictate", status="ok", final_text="Older.", ts=1,
                     audio_path=store.write_clip(audio, 16000))
    entry_id = store.add(kind="dictate", status="failed", raw_text="previous raw",
                         audio_path=store.write_clip(audio, 16000), ts=2)
    DictionaryStore(data / "dictionary.json").add_replacement("teh", "the")
    config = replace(Config(), cleanup_enabled=False, keep_audio_clips=0,
                     stt_backend="faster-whisper", model_size="base")
    monkeypatch.setattr(cli_module, "load_config", lambda: config)
    built = []

    def build_stt(_config):
        built.append(1)
        if not downloaded:  # what an offline load of an uncached model raises
            raise LocalEntryNotFoundError("Cannot find an appropriate cached snapshot folder")
        return Mock(backend="faster-whisper", transcribe=Mock(return_value="teh words here"))

    monkeypatch.setattr(app, "build_stt", build_stt)
    monkeypatch.setattr(app, "build_llm", lambda *_a, **_k: Mock())
    monkeypatch.setattr(app, "build_fast_llm", lambda *_a, **_k: None)
    monkeypatch.setattr(app, "paste_text", lambda *_a: pytest.fail("retry must not paste"))
    monkeypatch.setattr(pyperclip, "copy", lambda _text: pytest.fail("retry must not copy"))
    return data, store, entry_id, kept, built


def test_history_retry_json_prints_the_result_and_changes_nothing(tmp_path, monkeypatch):
    data, store, entry_id, kept, built = _retry_json_setup(tmp_path, monkeypatch)
    before = store.recent()

    result = CliRunner().invoke(
        cli_module.cli, ["history", "retry", str(entry_id), "--json", "--data-dir", str(data)]
    )

    assert result.exit_code == 0, result.output
    reply = json.loads(result.stdout.splitlines()[-1])
    assert reply["id"] == entry_id and reply["ts"] == repr(store.get(entry_id)["ts"])
    assert reply["raw_text"] == "teh words here"
    assert "the words here" in reply["final_text"]  # the data dir's dictionary
    assert built == [1]
    # LocalFlow Preview commits it: no row, clip, last.txt or clipboard change.
    assert store.recent() == before
    assert store.get(kept)["audio_path"]
    assert not (tmp_path / "home" / ".localflow" / "last.txt").exists()


def test_history_retry_json_without_the_model_exits_3_and_changes_nothing(tmp_path, monkeypatch):
    data, store, entry_id, _kept, _built = _retry_json_setup(tmp_path, monkeypatch, downloaded=False)
    before = store.recent()

    result = CliRunner().invoke(
        cli_module.cli, ["history", "retry", str(entry_id), "--json", "--data-dir", str(data)]
    )

    assert result.exit_code == 3
    assert "the base speech model isn't downloaded" in result.output
    assert "{" not in result.stdout
    assert store.recent() == before
    missing = CliRunner().invoke(
        cli_module.cli, ["history", "retry", "999", "--json", "--data-dir", str(data)]
    )
    assert missing.exit_code == 1 and "history entry 999 not found" in missing.output
