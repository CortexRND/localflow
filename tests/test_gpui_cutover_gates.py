import importlib
import json
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


def test_collection_paths_map_to_gnome_keyring_files(gates, tmp_path):
    safety, _ = gates
    keyrings = tmp_path / "keyrings"
    mapped = safety.collection_keyring
    prefix = "/org/freedesktop/secrets/collection/"
    assert mapped(keyrings, prefix + "login") == keyrings / "login.keyring"
    assert mapped(keyrings, prefix + "my_2dkeys") == keyrings / "my-keys.keyring"
    assert mapped(keyrings, "/") is None
    assert mapped(keyrings, prefix) is None
    assert mapped(keyrings, "/org/freedesktop/secrets/aliases/default") is None
    assert mapped(keyrings, prefix + "_2e_2e") is None
    assert mapped(keyrings, prefix + "a_2fb") is None


def test_daemon_keyrings_follow_its_environment(gates, tmp_path):
    safety, _ = gates
    home = tmp_path / "home"
    data = tmp_path / "data"
    other = tmp_path / "other"
    expected = home / ".local/share/keyrings"
    assert safety.daemon_keyrings({}, home) == expected
    assert safety.daemon_keyrings({"XDG_DATA_HOME": "relative"}, home) == expected
    assert safety.daemon_keyrings({"XDG_DATA_HOME": str(data)}, home) == data / "keyrings"
    assert safety.daemon_keyrings({"HOME": str(other)}, home) == other / ".local/share/keyrings"
    assert safety.legacy_keyrings({}, home) == home / ".gnome2/keyrings"
    assert safety.legacy_keyrings({"XDG_DATA_HOME": str(data)}, home) == home / ".gnome2/keyrings"
    assert safety.legacy_keyrings({"HOME": str(other)}, home) == other / ".gnome2/keyrings"


def test_capture_requires_unit_gpui_scale(gates):
    _, screenshots = gates
    environ = screenshots.parse_environ(b"GPUI_X11_SCALE_FACTOR=1\0GDK_SCALE=2\0junk\0")
    assert environ == {"GPUI_X11_SCALE_FACTOR": "1", "GDK_SCALE": "2"}
    screenshots.require_unit_scale(environ)
    screenshots.require_unit_scale({"GPUI_X11_SCALE_FACTOR": "1.0"})
    for bad in ("", "2", "randr", "0.5"):
        with pytest.raises(RuntimeError):
            screenshots.require_unit_scale({"GPUI_X11_SCALE_FACTOR": bad})
    with pytest.raises(RuntimeError):
        screenshots.require_unit_scale({})


def test_unknown_page_notice_is_detected_in_stderr(gates):
    _, screenshots = gates
    log = (
        "[2026-11-24T10:00:00Z INFO  blade_graphics::hal::init] Adapter: llvmpipe\n"
        "lf-desktop: There's no page called “nowhere”, so Home is shown.\n"
        "[2026-11-24T10:00:01Z INFO  blade_graphics::hal::surface] Surface 1280x850\n"
    )
    assert screenshots.rejected_page(log) == "nowhere"
    ordinary = "[2026-11-24T10:00:00Z INFO  blade_graphics::hal::init] Adapter: llvmpipe\n"
    assert screenshots.rejected_page(ordinary) is None
    assert screenshots.rejected_page("") is None
    quoted = "note: lf-desktop: There's no page called “nowhere”, so Home is shown.\n"
    assert screenshots.rejected_page(quoted) is None
    unprefixed = "There's no page called “nowhere”, so Home is shown.\n"
    assert screenshots.rejected_page(unprefixed) is None


def capture_digests(screenshots):
    return {
        f"{page}-{theme}-{width}x{height}.png": f"{page}/{theme}/{width}"
        for page in screenshots.PAGES
        for theme in ("light", "dark")
        for width, height in screenshots.SIZES
    }


def test_distinct_captures_have_no_duplicates(gates):
    _, screenshots = gates
    digests = capture_digests(screenshots)
    assert len(digests) == 44
    assert screenshots.duplicate_captures(digests) == []


def test_page_rendering_home_is_a_duplicate(gates):
    _, screenshots = gates
    digests = capture_digests(screenshots)
    digests["ask-dark-960x640.png"] = digests["home-dark-960x640.png"]
    assert screenshots.duplicate_captures(digests) == [
        ("ask-dark-960x640.png", "home-dark-960x640.png"),
    ]


def test_unswitched_theme_is_a_duplicate_and_groups_are_sorted(gates):
    _, screenshots = gates
    digests = capture_digests(screenshots)
    digests["style-dark-1280x850.png"] = digests["style-light-1280x850.png"]
    digests["setup-light-960x640.png"] = "same"
    digests["models-light-960x640.png"] = "same"
    digests["meetings-dark-960x640.png"] = "same"
    assert screenshots.duplicate_captures(digests) == [
        ("meetings-dark-960x640.png", "models-light-960x640.png", "setup-light-960x640.png"),
        ("style-dark-1280x850.png", "style-light-1280x850.png"),
    ]


def fake_signatures(monkeypatch, screenshots, signatures):
    def run(*command):
        assert command[:3] == ("identify", "-format", "%#")
        return signatures[Path(command[-1]).name]

    monkeypatch.setattr(screenshots, "run", run)


def test_record_writes_manifest_before_rejecting_identical_pixels(gates, monkeypatch, tmp_path):
    _, screenshots = gates
    names = ("home-dark-960x640.png", "ask-dark-960x640.png")
    fake_signatures(monkeypatch, screenshots, dict.fromkeys(names, "shared"))
    with pytest.raises(RuntimeError, match=r"ask-dark-960x640\.png, home-dark-960x640\.png"):
        screenshots.record(tmp_path, [tmp_path / name for name in names])
    captures = json.loads((tmp_path / "manifest.json").read_text())["captures"]
    assert [entry["file"] for entry in captures] == sorted(names)
    assert {entry["signature"] for entry in captures} == {"shared"}


def test_record_writes_sorted_manifest_for_distinct_pixels(gates, monkeypatch, tmp_path):
    _, screenshots = gates
    signatures = {"style-light-1280x850.png": "c", "home-light-1280x850.png": "a",
                  "home-dark-1280x850.png": "b"}
    fake_signatures(monkeypatch, screenshots, signatures)
    screenshots.record(tmp_path, [tmp_path / name for name in signatures])
    text = (tmp_path / "manifest.json").read_text()
    assert text.endswith("}\n")
    captures = json.loads(text)["captures"]
    assert [entry["file"] for entry in captures] == sorted(signatures)
    assert [entry["signature"] for entry in captures] == ["b", "a", "c"]


def test_manifest_parses_names_in_sorted_order(gates):
    _, screenshots = gates
    digests = {"setup-dark-960x640.png": "b", "home-light-1280x850.png": "a"}
    assert screenshots.manifest(digests) == {"captures": [
        {"file": "home-light-1280x850.png", "page": "home", "theme": "light",
         "width": 1280, "height": 850, "signature": "a"},
        {"file": "setup-dark-960x640.png", "page": "setup", "theme": "dark",
         "width": 960, "height": 640, "signature": "b"},
    ]}
    names = [entry["file"] for entry in screenshots.manifest(capture_digests(screenshots))["captures"]]
    assert names == sorted(names) and len(names) == 44
