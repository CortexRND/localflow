import os
import tomllib

import pytest

from localflow.config import Config, load_config, save_config


def test_save_config_merges_unknown_keys_and_keeps_existing_order(tmp_path):
    path = tmp_path / ".localflow.toml"
    path.write_text('custom = "kept"\nmodel_size = "small"\n')

    assert save_config({"hotkey": "alt_r"}, path) == path

    text = path.read_text()
    assert text.index("custom =") < text.index("model_size =") < text.index("hotkey =")
    assert tomllib.loads(text) == {
        "custom": "kept",
        "model_size": "small",
        "hotkey": "alt_r",
    }


def test_save_config_rejects_unknown_keys_and_wrong_types(tmp_path):
    with pytest.raises(ValueError, match="unknown config key"):
        save_config({"not_a_setting": "x"}, tmp_path / "config.toml")
    with pytest.raises(ValueError, match="cleanup_enabled must be bool"):
        save_config({"cleanup_enabled": 1}, tmp_path / "config.toml")
    with pytest.raises(ValueError, match="skills_dirs must be a list of strings"):
        save_config({"skills_dirs": [1]}, tmp_path / "config.toml")


def test_save_config_none_removes_a_key_and_escaping_round_trips(tmp_path):
    path = tmp_path / "config.toml"
    path.write_text('language = "en"\n')

    save_config({"language": None, "input_device": 'Mic "one"\\path\n'}, path)

    data = tomllib.loads(path.read_text())
    assert "language" not in data
    assert data["input_device"] == 'Mic "one"\\path\n'
    assert load_config(path).input_device == data["input_device"]


def test_save_config_keeps_backup_and_replaces_atomically(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    original = b'model_size = "tiny"\n'
    path.write_bytes(original)
    replaces = []
    real_replace = os.replace

    def replace(source, destination):
        replaces.append((source, destination))
        real_replace(source, destination)

    monkeypatch.setattr("localflow.config.os.replace", replace)

    save_config({"setup_version": 1}, path)

    assert (tmp_path / "config.toml.bak").read_bytes() == original
    assert len(replaces) == 1 and replaces[0][1] == path
    assert tomllib.loads(path.read_text()) == {"model_size": "tiny", "setup_version": 1}


def test_new_config_uses_private_permissions_and_defaults(tmp_path):
    path = tmp_path / "nested" / "config.toml"

    save_config({"setup_version": 1}, path)

    assert path.stat().st_mode & 0o777 == 0o600
    assert load_config(path) == Config(setup_version=1)
