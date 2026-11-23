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


@pytest.mark.skipif(not Path("/proc/self/environ").exists(), reason="needs Linux /proc")
def test_keyring_directory_comes_from_the_daemon_environment(gates):
    safety, _ = gates
    keyrings = safety.daemon_keyrings(os.getpid())
    data = os.environ.get("XDG_DATA_HOME", "")
    expected = Path(data) if Path(data).is_absolute() else Path(os.environ["HOME"]) / ".local/share"
    assert keyrings == expected / "keyrings"


def test_collection_path_maps_to_its_keyring_file(gates, tmp_path):
    safety, _ = gates
    prefix = "/org/freedesktop/secrets/collection/"
    assert safety.collection_file(tmp_path, prefix + "login") == tmp_path / "login.keyring"
    assert safety.collection_file(tmp_path, prefix + "my_20keys") == tmp_path / "my keys.keyring"
    assert safety.collection_file(tmp_path, prefix + "caf_c3_a9") == tmp_path / "caf\u00e9.keyring"
    for invalid in ("/", prefix, prefix + "_2e_2e", prefix + "a_2fb", "/org/other/login"):
        assert safety.collection_file(tmp_path, invalid) is None
