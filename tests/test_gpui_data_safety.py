import importlib
import sqlite3
from contextlib import closing
from pathlib import Path

import pytest


@pytest.fixture
def safety(monkeypatch):
    monkeypatch.syspath_prepend(str(Path(__file__).resolve().parents[1] / "scripts"))
    return importlib.import_module("gpui_data_safety")


def snapshot(safety, stores):
    return {name: safety.fingerprint(path, b"key") for name, path in stores.items()}


@pytest.fixture
def real_home(tmp_path):
    home = tmp_path / "home"
    config_dir = home / ".config" / "localflow"
    (config_dir / "launch").mkdir(parents=True)
    (config_dir / "config.toml").write_text("# fixture\n")
    (config_dir / "api-token").write_text("old-token\n")
    (home / ".localflow").mkdir()
    return home


def test_whole_config_dir_is_monitored(safety, real_home):
    stores = safety.real_store_paths({"HOME": str(real_home)}, real_home)
    assert stores["config_dir"] == real_home / ".config" / "localflow"
    assert stores["config"] == stores["config_dir"] / "config.toml"
    custom = real_home / "xdg"
    moved = safety.real_store_paths({"XDG_CONFIG_HOME": str(custom)}, real_home)
    assert moved["config_dir"] == custom / "localflow"


@pytest.mark.parametrize(
    "change",
    [
        lambda d: (d / "api-token").write_text("rotated-token\n"),
        lambda d: (d / "api-token").unlink(),
        lambda d: (d / "launch" / "settings.html").write_text("<html>"),
        lambda d: (d / "new-store.json").write_text("{}"),
    ],
    ids=["api-token-rotated", "api-token-removed", "launch-file-added", "file-added"],
)
def test_changes_beside_config_toml_fail_the_gate(safety, real_home, change):
    stores = safety.real_store_paths({}, real_home)
    before = snapshot(safety, stores)
    assert safety.changed_stores(before, snapshot(safety, stores)) == []
    change(stores["config_dir"])
    after = snapshot(safety, stores)
    assert safety.changed_stores(before, after) == ["config_dir"]


def fake_sandbox(root, rows=3, phrases="[]", enabled="true"):
    (root / "config").mkdir(parents=True, exist_ok=True)
    (root / "localflow").mkdir(exist_ok=True)
    (root / "config" / "config.toml").write_text(f"[dictation]\nenabled = {enabled}\n")
    (root / "localflow" / "hot_phrases.json").write_text(phrases)
    with closing(sqlite3.connect(root / "localflow" / "history.sqlite")) as db:
        db.execute("DROP TABLE IF EXISTS dictations")
        db.execute("CREATE TABLE dictations (id INTEGER PRIMARY KEY)")
        db.executemany("INSERT INTO dictations VALUES (?)", [(i,) for i in range(rows)])
        db.commit()


REPORT = """settings: dictation enabled = false
models: groq key saved to the in-memory sandbox keyring
hot phrases: added "cutover gate phrase"
history: deleted entry 14
prompts: approved abc
"""


def test_sandbox_writes_that_happened_pass(safety, tmp_path):
    fake_sandbox(tmp_path)
    before = safety.sandbox_state(tmp_path)
    assert before["history"] == 3
    phrases = f'[{{"trigger": "{safety.EXERCISE_PHRASE}"}}]'
    fake_sandbox(tmp_path, rows=2, phrases=phrases, enabled="false")
    assert safety.missing_writes(REPORT, before, safety.sandbox_state(tmp_path)) == []


def test_sandbox_writes_that_did_not_happen_fail_the_gate(safety, tmp_path):
    fake_sandbox(tmp_path)
    before = safety.sandbox_state(tmp_path)
    after = safety.sandbox_state(tmp_path)
    missing = safety.missing_writes("prompts: approved abc", before, after)
    assert missing[:4] == ["settings", "models", "hot phrases", "history"]
    assert len(missing) == 7
    assert safety.missing_writes(REPORT, before, after)[0].startswith("settings (")
