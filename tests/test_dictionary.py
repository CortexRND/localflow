import json
import logging

import pytest
from click.testing import CliRunner

from localflow import dictionary
from localflow.cli import cli
from localflow.dictionary import (
    DictionaryError,
    DictionaryStore,
    apply_replacements,
    stt_prompt,
)


@pytest.fixture
def store(tmp_path):
    return DictionaryStore(tmp_path / "dictionary.json")


def test_store_round_trip_and_cross_instance_mtime_reload(tmp_path):
    path = tmp_path / "dictionary.json"
    first = DictionaryStore(path)
    second = DictionaryStore(path)

    assert first.add_word("Ada Lovelace")
    assert first.add_replacement("teh", "the")
    assert second.words() == ["Ada Lovelace"]
    assert second.replacements() == [("teh", "the")]
    assert not second.add_word("ADA LOVELACE")
    assert second.remove("TEH")
    assert second.replacements() == []


def test_store_treats_missing_and_corrupt_files_as_empty(store, caplog):
    assert store.list() == {"words": [], "replacements": []}
    store.path.write_text("{broken", encoding="utf-8")

    with caplog.at_level(logging.ERROR):
        assert store.list() == {"words": [], "replacements": []}

    assert "dictionary unreadable" in caplog.text


def test_replacements_are_longest_first_case_insensitive_and_literal():
    assert apply_replacements(
        "CATALOG, cat and c++",
        [("cat", "dog"), ("catalog", "map"), ("c++", "C sharp")],
    ) == "map, dog and C sharp"
    assert apply_replacements("concatenate", [("cat", "dog")]) == "concatenate"
    assert apply_replacements("ı İ", [("i", "I")]) == "I I"


def test_windows_lock_calls_msvcrt_lock_and_unlock(store, monkeypatch):
    class FakeMsvcrt:
        LK_LOCK = 1
        LK_UNLCK = 2

        def __init__(self):
            self.calls = []

        def locking(self, fd, mode, count):
            self.calls.append((mode, count))

    fake = FakeMsvcrt()
    monkeypatch.setattr(dictionary, "fcntl", None)
    monkeypatch.setattr(dictionary, "msvcrt", fake)

    with store._exclusive():
        pass

    assert fake.calls == [(fake.LK_LOCK, 1), (fake.LK_UNLCK, 1)]


def test_stt_prompt_keeps_order_and_drops_overflow():
    prompt = stt_prompt(["Ada", "   ", "Lovelace", "x" * 800])

    assert prompt == "Vocabulary: Ada, Lovelace."
    assert len(prompt) <= 800
    assert stt_prompt([]) is None


def test_csv_import_supports_word_header_duplicates_and_invalid_rows(store, tmp_path):
    source = tmp_path / "words.csv"
    source.write_text("word\n Ada \nada\n\n,invalid\n", encoding="utf-8")

    counts = store.import_file(source)

    assert counts == {
        "added": 1,
        "skipped_duplicate": 1,
        "skipped_invalid": 2,
    }
    assert store.words() == ["Ada"]


def test_csv_import_supports_replacement_header_and_two_columns(store, tmp_path):
    source = tmp_path / "replacements.csv"
    source.write_text(
        "from,to\n teh , the \nrecieve,receive\n", encoding="utf-8"
    )

    assert store.import_file(source) == {
        "added": 2,
        "skipped_duplicate": 0,
        "skipped_invalid": 0,
    }
    assert store.replacements() == [("teh", "the"), ("recieve", "receive")]


def test_json_import_uses_dictionary_shape_and_reports_counts(store, tmp_path):
    source = tmp_path / "import.json"
    source.write_text(
        json.dumps({
            "words": [" Ada ", "ada", "", 12],
            "replacements": [{"from": "teh", "to": "the"}, {"from": "", "to": "x"}],
        }),
        encoding="utf-8",
    )

    assert store.import_file(source) == {
        "added": 2,
        "skipped_duplicate": 1,
        "skipped_invalid": 3,
    }


def test_import_enforces_size_and_entry_limits(store, tmp_path, monkeypatch):
    source = tmp_path / "large.csv"
    source.write_text("word\nlong\n", encoding="utf-8")
    monkeypatch.setattr(dictionary, "MAX_IMPORT_BYTES", 3)
    with pytest.raises(DictionaryError, match="exceeds 3 MB"):
        store.import_file(source)

    monkeypatch.setattr(dictionary, "MAX_IMPORT_BYTES", 3 * 1024 * 1024)
    source = tmp_path / "large.json"
    source.write_text(json.dumps({"words": [f"word-{n}" for n in range(1001)]}))
    with pytest.raises(DictionaryError, match="more than 1000"):
        store.import_file(source)
    assert store.words() == []


def test_dict_cli_lists_adds_removes_and_imports(monkeypatch, tmp_path):
    monkeypatch.setattr(dictionary, "DICTIONARY_PATH", tmp_path / "dictionary.json")
    runner = CliRunner()

    assert runner.invoke(cli, ["dict", "add", "Ada"]).exit_code == 0
    assert runner.invoke(cli, ["dict", "add", "--from", "teh", "--to", "the"]).exit_code == 0
    listed = runner.invoke(cli, ["dict", "list"])
    assert listed.exit_code == 0
    assert "word: Ada" in listed.output
    assert "teh -> the" in listed.output
    assert runner.invoke(cli, ["dict", "remove", "ada"]).exit_code == 0
    assert runner.invoke(cli, ["dict", "remove", "teh"]).exit_code == 0
    assert "No dictionary entries." in runner.invoke(cli, ["dict", "list"]).output

    source = tmp_path / "import.csv"
    source.write_text("word\nLovelace\n", encoding="utf-8")
    imported = runner.invoke(cli, ["dict", "import", str(source)])
    assert imported.exit_code == 0
    assert "added 1" in imported.output


def test_dict_cli_rejects_incomplete_replacements():
    result = CliRunner().invoke(cli, ["dict", "add", "--from", "teh"])
    assert result.exit_code != 0
    assert "both --from and --to" in result.output


def test_stt_prompt_is_none_without_words():
    assert stt_prompt([]) is None
