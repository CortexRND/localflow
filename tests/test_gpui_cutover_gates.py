import importlib
import os
from pathlib import Path

import pytest


@pytest.fixture
def gates(monkeypatch):
    monkeypatch.syspath_prepend(str(Path(__file__).resolve().parents[1] / "scripts"))
    return importlib.import_module("gpui_data_safety"), importlib.import_module("gpui_screenshots")


def test_store_fingerprints_detect_creates_edits_and_deletes(gates, tmp_path):
    safety, _ = gates
    target = tmp_path / "store"
    key = b"test key"
    missing = safety.fingerprint(target, key)
    target.mkdir()
    empty = safety.fingerprint(target, key)
    (target / "config").write_text("before")
    original = safety.fingerprint(target, key)
    (target / "config").write_text("after!")
    edited = safety.fingerprint(target, key)
    assert len({missing, empty, original, edited}) == 4
    (target / "config").unlink()
    assert safety.fingerprint(target, key) == empty
    assert safety.fingerprint(target, b"another key") != empty


@pytest.mark.skipif(os.name == "nt", reason="Gate 2 runs on Linux; Windows symlinks need elevation")
def test_store_fingerprint_includes_symlink_target_contents(gates, tmp_path):
    safety, _ = gates
    config = tmp_path / "config.toml"
    actual = tmp_path / "redirected.toml"
    actual.write_text("old")
    config.symlink_to(actual)
    before = safety.fingerprint(config, b"key")
    actual.write_text("new")
    assert safety.fingerprint(config, b"key") != before


def test_capture_rejects_blank_wrong_size_and_wrong_theme(gates):
    _, screenshots = gates
    check = screenshots.matches_capture
    assert check("1280 850 941 0.85", "light", 1280, 850)
    assert check("1280 850 721 0.15", "dark", 1280, 850)
    assert not check("1280 850 1 0.01", "dark", 1280, 850)
    assert not check("1280 850 941 0.85", "dark", 1280, 850)
    assert not check("1280 850 941 0.85", "light", 960, 640)


def test_collection_file_decodes_gnome_keyring_object_paths(gates, tmp_path):
    safety, _ = gates
    prefix = "/org/freedesktop/secrets/collection/"
    assert safety.collection_file(prefix + "login", tmp_path) == tmp_path / "login.keyring"
    assert safety.collection_file(prefix + "my_5fkeys_20x", tmp_path) == tmp_path / "my_keys x.keyring"
    assert safety.collection_file("/", tmp_path) is None
    assert safety.collection_file(prefix + "login/1", tmp_path) is None


def test_keyring_directory_follows_the_daemon_environment(gates, tmp_path):
    safety, _ = gates
    data = tmp_path / "data"
    assert safety.keyring_directory({"HOME": str(tmp_path), "XDG_DATA_HOME": str(data)}) == data / "keyrings"
    assert safety.keyring_directory({"HOME": str(tmp_path)}) == tmp_path / ".local/share/keyrings"
    (tmp_path / ".gnome2/keyrings").mkdir(parents=True)
    assert safety.keyring_directory({"HOME": str(tmp_path)}) == tmp_path / ".gnome2/keyrings"
