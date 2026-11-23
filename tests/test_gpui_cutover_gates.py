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
