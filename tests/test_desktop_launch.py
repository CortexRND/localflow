"""Opt-in LocalFlow Preview launch from `lf hub`, `lf phrases` and `lf setup`.

Process-boundary tests use real fake `lf-desktop` executables in a temporary
HOME; nothing opens a GUI. Policy tests run on every platform; tests that need
a POSIX executable fixture are skipped elsewhere.
"""

import json
import os
import plistlib
import signal
import sys
import textwrap
import time
import types
from pathlib import Path

import click
import pytest
from click.testing import CliRunner

from localflow import cli, desktop_launch, phrases_window, setup_window

ROOT = Path(__file__).parent.parent
posix_only = pytest.mark.skipif(os.name != "posix", reason="needs a POSIX executable fixture")

GOOD_HELP = """\
lf-desktop — LocalFlow native desktop app (GPUI preview)

USAGE:
    lf-desktop [OPTIONS]

OPTIONS:
    --page <PAGE>      Open a page
    --setup            Open the setup wizard (or hand it to the open window)
    --engine           Allow this window to run dictation itself

EXIT STATUS (launch-status: 1):
    0   The window closed, or the request was handed to the open window
    1   No window could be opened (display, lock or data error)
"""
# A Scope 3 preview package: the same name, version and every flag, but it
# exits 0 even when no window opens, so its exit 0 can't mean a hand-off.
SCOPE3_HELP = """\
lf-desktop — LocalFlow native desktop app (GPUI preview)

OPTIONS:
    --page <PAGE>      Open a page
    --setup            Open the setup wizard (or hand it to the open window)
    --engine           Allow this window to run dictation itself
    --sandbox          Use an isolated example data directory
    --data-dir <DIR>   Sandbox directory
    --sandbox-reset    Re-seed the sandbox directory
    -h, --help         Print this help
    -V, --version      Print the version
"""
# Near misses: a different program whose name starts with lf-desktop, and a
# build whose flags only start with the right tokens.
# Each carries the launch-status marker, so only the near miss is wrong.
_MARK = "EXIT STATUS (launch-status: 1):\n"
OTHER_IDENTITY_HELP = "lf-desktop-other\n    --page <PAGE> x\n    --setup x\n" + _MARK
NEAR_FLAGS_HELPS = (
    "lf-desktop\n    --page <PAGE> x\n    --setup-extra x\n" + _MARK,
    "lf-desktop\n    --page <PAGE>S x\n    --setup x\n" + _MARK,
    "lf-desktop_x\n    --page <PAGE> x\n    --setup x\n" + _MARK,
    "lf-desktop\n    --page <PAGE> x\n    --setup x\nEXIT STATUS (launch-status: 0):\n",
    "lf-desktop\n    --page <PAGE> x\n    --setup x\nEXIT STATUS (launch-status: 1): later\n",
)
# An early unpackaged build: same identity and version, but no Setup.
OLD_HELP = GOOD_HELP.replace(
    "    --setup            Open the setup wizard (or hand it to the open window)\n", ""
)
UI_ENVS = ("LOCALFLOW_UI", "LOCALFLOW_DESKTOP_BIN", "LOCALFLOW_SANDBOX", "LOCALFLOW_CONFIG",
           "LOCALFLOW_RUNTIME")
COMMANDS = ("hub", "phrases", "setup")
# The real discovery: tests/conftest.py stubs it out for every other test file.
REAL_FIND_DESKTOP = desktop_launch.find_desktop


@pytest.fixture(autouse=True)
def home(tmp_path, monkeypatch):
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("USERPROFILE", str(home))
    # Windows discovery reads LOCALAPPDATA: never the developer's real one.
    monkeypatch.setenv("LOCALAPPDATA", str(home.joinpath("AppData", "Local")))
    for name in UI_ENVS:
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setattr(desktop_launch, "find_desktop", REAL_FIND_DESKTOP)
    return home


class NoProcess:
    """Stands in for probe/spawn when nothing may be probed or launched."""

    def __init__(self, monkeypatch, probe_ok=True):
        self.probed = []
        self.spawned = []
        monkeypatch.setattr(desktop_launch, "probe", lambda exe: self.probed.append(exe) or probe_ok)
        monkeypatch.setattr(desktop_launch, "spawn",
                            lambda argv: self.spawned.append(argv) or (True, None))


@pytest.fixture
def no_process(monkeypatch):
    return NoProcess(monkeypatch)


@pytest.fixture
def set_platform(monkeypatch):
    def set_to(name):
        monkeypatch.setattr(desktop_launch.sys, "platform", name)

    return set_to


class Legacy:
    """Records every legacy window the CLI opens instead of opening it."""

    def __init__(self, monkeypatch):
        self.calls = []
        fake = types.SimpleNamespace(Popen=lambda argv, **kw: self.calls.append(("hub", argv)))
        monkeypatch.setattr(cli, "subprocess", fake)
        monkeypatch.setattr(phrases_window, "main", lambda path=None: self.calls.append(("phrases", path)))
        monkeypatch.setattr(setup_window, "main", lambda: self.calls.append(("setup", None)))


@pytest.fixture
def legacy(monkeypatch):
    return Legacy(monkeypatch)


def _hub_legacy(page="Home"):
    return ("hub", [sys.executable, "-m", "localflow.hub_window", "--page", page])


LEGACY_FOR = {
    "hub": _hub_legacy(),
    "phrases": ("phrases", None),
    "setup": ("setup", None),
}


def _invoke(args, env=None):
    return CliRunner().invoke(cli.cli, args, env=env)


class FakeDesktop:
    """A real executable that records how it was run."""

    def __init__(self, path: Path, record: Path, *, help_text=GOOD_HELP, help_code=0,
                 help_hang=False, vanish=False, launch="exit0", raw_help=None):
        self.path = path
        self.record = record
        path.parent.mkdir(parents=True, exist_ok=True)
        script = textwrap.dedent(f"""\
            #!{sys.executable}
            import json, os, sys, time
            record = {str(record)!r}
            entry = {{"argv": sys.argv[1:], "pid": os.getpid(),
                      "env": {{k: os.environ.get(k) for k in
                              ("HOME", "LOCALFLOW_CONFIG", "LOCALFLOW_SANDBOX", "LOCALFLOW_UI",
                               "LOCALFLOW_PYTHON")}}}}
            with open(record, "a") as f:
                f.write(json.dumps(entry) + "\\n")
            if sys.argv[1:] == ["--help"]:
                if {help_hang!r}:
                    time.sleep(60)
                if {raw_help!r} is not None:
                    sys.stdout.buffer.write({raw_help!r})
                else:
                    sys.stdout.write({help_text!r})
                if {vanish!r}:
                    os.remove(sys.argv[0])
                sys.exit({help_code!r})
            print("SECRET-native-stdout")
            print("SECRET-native-stderr", file=sys.stderr)
            mode = {launch!r}
            if mode == "stay":
                time.sleep(60)
            sys.exit({{"exit0": 0, "exit1": 1, "exit2": 2}}[mode])
            """)
        path.write_text(script)
        path.chmod(0o755)

    def runs(self):
        if not self.record.exists():
            return []
        return [json.loads(line) for line in self.record.read_text().splitlines()]

    def argvs(self):
        return [run["argv"] for run in self.runs()]


@pytest.fixture
def fake(tmp_path):
    made = []

    def make(name="lf-desktop", directory=None, **kwargs):
        directory = directory or tmp_path / "bin"
        desktop = FakeDesktop(Path(directory) / name, tmp_path / f"record-{len(made)}.jsonl", **kwargs)
        made.append(desktop)
        return desktop

    return make


def _wait_for_runs(desktop, count, timeout=10.0):
    deadline = time.monotonic() + timeout
    while len(desktop.runs()) < count and time.monotonic() < deadline:
        time.sleep(0.05)
    return desktop.runs()


def _gpui_env(desktop, **extra):
    return {"LOCALFLOW_DESKTOP_BIN": str(desktop.path), **extra}


SET_UP = "config_version = 2\n\n[setup]\nversion = 1\n"


@pytest.fixture
def configured(tmp_path, monkeypatch):
    """LocalFlow is set up: the active config carries the setup marker."""
    config = tmp_path / "config" / "config.toml"
    config.parent.mkdir()
    config.write_text(SET_UP)
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(config))
    return config


# --------------------------------------------------------------- selection ---


def test_platform_default_is_gpui_on_linux_and_windows_only():
    for name in ("linux", "linux2", "win32"):
        assert desktop_launch.default_ui(name) == "gpui", name
    for name in ("darwin", "cygwin", "freebsd14", "aix", ""):
        assert desktop_launch.default_ui(name) == "legacy", name
    assert not hasattr(desktop_launch, "DEFAULT_UI")
    assert desktop_launch.RUNTIME_ENV == "LOCALFLOW_RUNTIME"


@pytest.mark.parametrize(("name", "default"), [("linux", "gpui"), ("win32", "gpui"),
                                               ("darwin", "legacy"), ("freebsd14", "legacy")])
def test_selection_precedence_is_flag_then_runtime_then_env_then_platform_default(name, default):
    def choose(flag, env):
        return desktop_launch.choose_ui(flag, env, name)

    assert choose(None, {}) == (default, False, None)
    assert choose(None, {"LOCALFLOW_UI": ""}) == (default, False, None)
    assert choose(None, {"LOCALFLOW_UI": "gpui"}) == ("gpui", True, None)
    assert choose(None, {"LOCALFLOW_UI": " GPUI "}) == ("gpui", True, None)
    assert choose(None, {"LOCALFLOW_UI": "legacy"}) == ("legacy", True, None)
    assert choose("legacy", {"LOCALFLOW_UI": "gpui"}) == ("legacy", True, None)
    assert choose("gpui", {"LOCALFLOW_UI": "legacy"}) == ("gpui", True, None)
    assert choose("gpui", {"LOCALFLOW_UI": "bogus"}) == ("gpui", True, None)
    assert choose(None, {"LOCALFLOW_UI": "bogus"}) == ("legacy", True, desktop_launch.NOTICE_INVALID_ENV)
    # A binary override alone is not an opt-in: it never changes the default.
    assert choose(None, {"LOCALFLOW_DESKTOP_BIN": "/x/lf-desktop"}) == (default, False, None)
    # LOCALFLOW_RUNTIME=python beats LOCALFLOW_UI but not a flag.
    for value in ("python", " Python ", "PYTHON"):
        assert choose(None, {"LOCALFLOW_RUNTIME": value}) == ("legacy", True, None)
        assert choose(None, {"LOCALFLOW_RUNTIME": value, "LOCALFLOW_UI": "gpui"}) == ("legacy", True, None)
        assert choose(None, {"LOCALFLOW_RUNTIME": value, "LOCALFLOW_UI": "bogus"}) == ("legacy", True, None)
        assert choose("gpui", {"LOCALFLOW_RUNTIME": value}) == ("gpui", True, None)
    # Any other runtime value is ignored, silently.
    for value in ("", "rust", " Rust ", "pythonx", "secret-garbage"):
        assert choose(None, {"LOCALFLOW_RUNTIME": value}) == choose(None, {}), value
        assert choose(None, {"LOCALFLOW_RUNTIME": value, "LOCALFLOW_UI": "gpui"}) == ("gpui", True, None)


@pytest.mark.parametrize("name", ["linux", "win32", "darwin", "freebsd14"])
def test_setup_window_defaults_to_legacy_but_explicit_choices_are_unchanged(name):
    assert desktop_launch.default_ui(name, setup=True) == "legacy"

    def choose(flag, env):
        return desktop_launch.choose_ui(flag, env, name, setup=True)

    for env in ({}, {"LOCALFLOW_UI": ""}, {"LOCALFLOW_RUNTIME": "rust"},
                {"LOCALFLOW_DESKTOP_BIN": "/x/lf-desktop"}):
        assert choose(None, env) == ("legacy", False, None), env
    assert choose("gpui", {}) == ("gpui", True, None)
    assert choose(None, {"LOCALFLOW_UI": "gpui"}) == ("gpui", True, None)
    assert choose("gpui", {"LOCALFLOW_RUNTIME": "python"}) == ("gpui", True, None)
    assert choose("legacy", {"LOCALFLOW_UI": "gpui"}) == ("legacy", True, None)
    assert choose(None, {"LOCALFLOW_UI": "legacy"}) == ("legacy", True, None)
    assert choose(None, {"LOCALFLOW_RUNTIME": "python", "LOCALFLOW_UI": "gpui"}) == ("legacy", True, None)
    assert choose(None, {"LOCALFLOW_UI": "bogus"}) == ("legacy", True, desktop_launch.NOTICE_INVALID_ENV)


def test_flags_are_mutually_exclusive():
    assert desktop_launch.flag_choice(False, False) is None
    assert desktop_launch.flag_choice(True, False) == "gpui"
    assert desktop_launch.flag_choice(False, True) == "legacy"
    with pytest.raises(click.UsageError):
        desktop_launch.flag_choice(True, True)


@pytest.mark.parametrize("args", [["hub", "--gpui", "--legacy"], ["phrases", "--gpui", "--legacy"],
                                  ["setup", "--legacy", "--gpui"]])
def test_both_flags_are_a_usage_error(args, legacy):
    result = _invoke(args)
    assert result.exit_code == 2 and "can't be used together" in result.output
    assert legacy.calls == []


def test_sandbox_rule_matches_lf_desktop_exactly():
    for value in ("1", "true"):
        assert desktop_launch.sandbox_requested({"LOCALFLOW_SANDBOX": value})
    for value in ("", "0", "yes", "TRUE", "True", " 1"):
        assert not desktop_launch.sandbox_requested({"LOCALFLOW_SANDBOX": value})
    assert not desktop_launch.sandbox_requested({})


def test_argv_is_closed_and_canonical():
    exe = Path("/opt/lf desktop/lf-desktop")
    assert desktop_launch.desktop_argv(exe) == [str(exe)]
    assert desktop_launch.desktop_argv(exe, page="hot phrases") == [str(exe), "--page", "Hot Phrases"]
    assert desktop_launch.desktop_argv(exe, page="SETTINGS") == [str(exe), "--page", "Settings"]
    assert desktop_launch.desktop_argv(exe, setup=True) == [str(exe), "--setup"]
    with pytest.raises(ValueError):
        desktop_launch.desktop_argv(exe, page="Home; rm -rf /")


# --------------------------------------------------------------- discovery ---


@pytest.mark.parametrize("name", ["cygwin", "freebsd14", "aix", "win64"])
def test_unsupported_platforms_never_discover(name, home):
    exe = home / "lf-desktop"
    exe.write_text("x")
    env = {"LOCALFLOW_DESKTOP_BIN": str(exe)}
    assert desktop_launch.find_desktop(env, name, home) == (None, desktop_launch.NOTICE_PLATFORM)


def test_platform_keys():
    assert desktop_launch.platform_key("linux") == "linux"
    assert desktop_launch.platform_key("darwin") == "macos"
    assert desktop_launch.platform_key("win32") == "windows"
    assert desktop_launch.platform_key("cygwin") is None


def _linux_install(home, manifest=b'{\n  "name": "localflow-preview",\n  "version": "0.2.2"\n}\n',
                   exe=True):
    root = home / ".local/share/localflow-preview"
    (root / "bin").mkdir(parents=True)
    if manifest is not None:
        (root / "MANIFEST.json").write_bytes(manifest)
    if exe:
        (root / "bin/lf-desktop").write_text("#!/bin/sh\n")
        (root / "bin/lf-desktop").chmod(0o755)
    return root, root / "bin/lf-desktop"


def _mac_install(home, bundle_id=desktop_launch.MAC_BUNDLE_ID, raw=None):
    app = home / "Applications/LocalFlow Preview.app"
    (app / "Contents/MacOS").mkdir(parents=True)
    plist = raw if raw is not None else plistlib.dumps({"CFBundleIdentifier": bundle_id})
    (app / "Contents/Info.plist").write_bytes(plist)
    exe = app / "Contents/MacOS/lf-desktop"
    exe.write_text("#!/bin/sh\n")
    exe.chmod(0o755)
    return app, exe


def test_owned_linux_package_is_found(home):
    _, exe = _linux_install(home)
    assert desktop_launch.find_desktop({}, "linux", home) == (exe, None)


def test_owned_mac_bundle_is_found(home):
    _, exe = _mac_install(home)
    assert desktop_launch.find_desktop({}, "darwin", home) == (exe, None)


def test_missing_install_says_not_installed(home):
    for platform in ("linux", "darwin"):
        assert desktop_launch.find_desktop({}, platform, home) == (
            None, desktop_launch.NOTICE_NOT_INSTALLED)
    assert desktop_launch.find_desktop({}, "win32", home) == (
        None, desktop_launch.NOTICE_NOT_INSTALLED_WINDOWS)


def test_not_installed_notices():
    linux, windows = desktop_launch.NOTICE_NOT_INSTALLED, desktop_launch.NOTICE_NOT_INSTALLED_WINDOWS
    assert "scripts/install_gpui_preview.sh" in linux  # Linux/macOS text unchanged
    assert "install_gpui_preview" not in windows
    assert windows == ("lf: LocalFlow Preview isn't installed (unzip the LocalFlow Preview folder "
                       "into %LOCALAPPDATA%\\Programs); opening the legacy window.")
    for key in ("linux", "macos", None):
        assert desktop_launch.not_installed_notice(key) == linux
    assert desktop_launch.not_installed_notice("windows") == windows
    assert desktop_launch.is_not_installed(linux) and desktop_launch.is_not_installed(windows)
    for other in (None, "", desktop_launch.NOTICE_NOT_OWNED, desktop_launch.NOTICE_BAD_OVERRIDE,
                  desktop_launch.NOTICE_PLATFORM, desktop_launch.NOTICE_PROBE):
        assert not desktop_launch.is_not_installed(other)


def test_relative_home_is_never_searched_but_an_absolute_override_still_works(
        tmp_path, monkeypatch):
    # A valid-looking package under a *relative* HOME is never trusted.
    monkeypatch.chdir(tmp_path)
    _linux_install(Path("relhome"))
    _mac_install(Path("relhome"))
    for platform in ("linux", "darwin"):
        assert desktop_launch.find_desktop({}, platform, Path("relhome")) == (
            None, desktop_launch.NOTICE_NOT_INSTALLED)
    monkeypatch.setenv("HOME", "relhome")
    assert desktop_launch.find_desktop({}, "linux") == (None, desktop_launch.NOTICE_NOT_INSTALLED)
    exe = tmp_path / "abs" / "lf-desktop"
    exe.parent.mkdir()
    exe.write_text("#!/bin/sh\n")
    exe.chmod(0o755)
    env = {"LOCALFLOW_DESKTOP_BIN": str(exe)}
    assert desktop_launch.find_desktop(env, "linux", Path("relhome")) == (exe, None)


@pytest.mark.parametrize("manifest", [
    None,
    b'{"name": "localflow"}',
    b'{"name": "localflow-preview"',
    b'["localflow-preview"]',
    b"\xff\xfe not utf-8",
])
def test_linux_package_without_our_manifest_is_refused(home, manifest):
    _linux_install(home, manifest=manifest)
    assert desktop_launch.find_desktop({}, "linux", home) == (None, desktop_launch.NOTICE_NOT_OWNED)


@pytest.mark.parametrize("kwargs", [
    {"bundle_id": "com.cortexrnd.localflow"},
    {"bundle_id": "com.cortexrnd.localflow.desktop-preview.evil"},
    {"raw": b"<plist><dict><key>CFBundleIdentifier"},
    {"raw": b""},
])
def test_mac_bundle_with_wrong_or_malformed_identity_is_refused(home, kwargs):
    _mac_install(home, **kwargs)
    assert desktop_launch.find_desktop({}, "darwin", home) == (None, desktop_launch.NOTICE_NOT_OWNED)


def test_owned_package_without_executable_is_not_installed(home):
    _linux_install(home, exe=False)
    assert desktop_launch.find_desktop({}, "linux", home) == (None, desktop_launch.NOTICE_NOT_INSTALLED)


@posix_only
def test_symlinked_or_non_executable_package_parts_are_refused(home, tmp_path):
    root, exe = _linux_install(home)
    exe.chmod(0o644)
    assert desktop_launch.find_desktop({}, "linux", home)[0] is None
    exe.chmod(0o755)
    manifest = root / "MANIFEST.json"
    real = tmp_path / "elsewhere.json"
    manifest.rename(real)
    manifest.symlink_to(real)
    assert desktop_launch.find_desktop({}, "linux", home) == (None, desktop_launch.NOTICE_NOT_OWNED)
    manifest.unlink()
    real.rename(manifest)
    exe.unlink()
    target = tmp_path / "other-lf-desktop"
    target.write_text("#!/bin/sh\n")
    target.chmod(0o755)
    exe.symlink_to(target)
    assert desktop_launch.find_desktop({}, "linux", home)[0] is None
    # The package root itself must be a real directory.
    moved = tmp_path / "moved-root"
    root.rename(moved)
    root.symlink_to(moved)
    assert desktop_launch.find_desktop({}, "linux", home) == (None, desktop_launch.NOTICE_NOT_OWNED)


@pytest.mark.parametrize("platform", ["linux", "darwin"])
def test_invalid_override_never_falls_through_to_the_package(home, tmp_path, platform):
    _linux_install(home)
    _mac_install(home)
    for override in ("lf-desktop", "relative/lf-desktop", str(tmp_path / "missing"), str(tmp_path)):
        env = {"LOCALFLOW_DESKTOP_BIN": override}
        assert desktop_launch.find_desktop(env, platform, home) == (
            None, desktop_launch.NOTICE_BAD_OVERRIDE), override


@posix_only
def test_non_executable_override_is_refused(tmp_path, home):
    exe = tmp_path / "lf-desktop"
    exe.write_text("#!/bin/sh\n")
    exe.chmod(0o644)
    env = {"LOCALFLOW_DESKTOP_BIN": str(exe)}
    assert desktop_launch.find_desktop(env, "linux", home) == (None, desktop_launch.NOTICE_BAD_OVERRIDE)


_WIN_MANIFEST = b'{\n  "name": "localflow-preview",\n  "os": "windows",\n  "version": "0.2.2"\n}\n'


def _windows_install(local_appdata, manifest=_WIN_MANIFEST, exe=True):
    """The Windows zip's `LocalFlow Preview` folder unzipped into Programs."""
    root = local_appdata.joinpath("Programs", "LocalFlow Preview")
    root.mkdir(parents=True)
    if manifest is not None:
        root.joinpath("MANIFEST.json").write_bytes(manifest)
    exe_path = root.joinpath("lf-desktop.exe")
    if exe:
        exe_path.write_text("x")
        exe_path.chmod(0o755)
    return root, exe_path


def test_owned_windows_folder_is_found_under_localappdata(tmp_path, home):
    local = tmp_path.joinpath("Local AppData")
    _, exe = _windows_install(local)
    env = {"LOCALAPPDATA": str(local)}
    assert desktop_launch.find_desktop(env, "win32", home) == (exe, None)
    assert exe.parent.name == "LocalFlow Preview" and exe.name == "lf-desktop.exe"
    # LOCALAPPDATA is honoured: the same folder isn't found from another one.
    assert desktop_launch.find_desktop({"LOCALAPPDATA": str(home)}, "win32", home) == (
        None, desktop_launch.NOTICE_NOT_INSTALLED_WINDOWS)


@pytest.mark.parametrize("env", [{}, {"LOCALAPPDATA": ""}, {"LOCALAPPDATA": "relative/AppData"}])
def test_windows_falls_back_to_home_appdata_local(env, home):
    _, exe = _windows_install(home.joinpath("AppData", "Local"))
    assert desktop_launch.find_desktop(env, "win32", home) == (exe, None)


def test_missing_windows_folder_says_not_installed(tmp_path, home):
    notice = desktop_launch.NOTICE_NOT_INSTALLED_WINDOWS
    env = {"LOCALAPPDATA": str(tmp_path.joinpath("empty"))}
    assert desktop_launch.find_desktop(env, "win32", home) == (None, notice)
    _windows_install(tmp_path.joinpath("noexe"), exe=False)
    env = {"LOCALAPPDATA": str(tmp_path.joinpath("noexe"))}
    assert desktop_launch.find_desktop(env, "win32", home) == (None, notice)
    # A relative HOME is never searched, and still gets the Windows wording.
    assert desktop_launch.find_desktop({}, "win32", Path("relhome")) == (None, notice)


@pytest.mark.parametrize("manifest", [
    None,
    b'{"name": "localflow"}',
    b'{"name": "localflow-preview"',
    b'["localflow-preview"]',
    b"\xff\xfe not utf-8",
])
def test_windows_folder_without_our_manifest_is_refused(tmp_path, home, manifest):
    _windows_install(tmp_path.joinpath("lad"), manifest=manifest)
    env = {"LOCALAPPDATA": str(tmp_path.joinpath("lad"))}
    assert desktop_launch.find_desktop(env, "win32", home) == (None, desktop_launch.NOTICE_NOT_OWNED)


def test_windows_override_still_works_and_never_falls_through(tmp_path, home):
    _windows_install(home.joinpath("AppData", "Local"))
    exe = tmp_path.joinpath("elsewhere", "lf-desktop.exe")
    exe.parent.mkdir()
    exe.write_text("x")
    exe.chmod(0o755)
    assert desktop_launch.find_desktop({"LOCALFLOW_DESKTOP_BIN": str(exe)}, "win32", home) == (exe, None)
    for override in ("lf-desktop.exe", str(tmp_path.joinpath("missing.exe"))):
        env = {"LOCALFLOW_DESKTOP_BIN": override}
        assert desktop_launch.find_desktop(env, "win32", home) == (
            None, desktop_launch.NOTICE_BAD_OVERRIDE), override


# ------------------------------------------------------------------- probe ---


@posix_only
def test_probe_accepts_only_lf_desktop_with_page_and_setup(fake):
    assert desktop_launch.probe(fake(help_text=GOOD_HELP).path)
    assert desktop_launch.probe(fake(name="tabs", help_text="lf-desktop\n\t--page <PAGE>\n\t--setup\n" + _MARK).path)
    assert desktop_launch.LAUNCH_STATUS_MARKER + "\n" == _MARK
    desktop_names = []
    for kwargs in ({"help_text": OLD_HELP}, {"help_text": "usage: something-else --page <PAGE> --setup\n"},
                   {"help_text": ""}, {"raw_help": b"\x00\xff\xfe" * 100},
                   {"help_code": 1}, {"help_code": 2, "help_text": "lf-desktop: unknown argument"},
                   {"help_text": OTHER_IDENTITY_HELP}, {"help_text": SCOPE3_HELP},
                   *({"help_text": text} for text in NEAR_FLAGS_HELPS)):
        desktop = fake(name=f"lf-desktop-{len(desktop_names)}", **kwargs)
        desktop_names.append(desktop.path)
        assert not desktop_launch.probe(desktop.path), kwargs
        assert desktop.argvs() == [["--help"]]  # probed once, never launched


@posix_only
def test_hanging_probe_is_killed_and_only_the_probe(fake, monkeypatch):
    desktop = fake(help_hang=True)
    monkeypatch.setattr(desktop_launch, "PROBE_TIMEOUT", 2.0)
    started = time.monotonic()
    assert not desktop_launch.probe(desktop.path)
    assert time.monotonic() - started < 10
    (run,) = desktop.runs()  # recorded before it hung
    with pytest.raises(ProcessLookupError):
        os.kill(run["pid"], 0)


# ------------------------------------------------------------------ routes ---


ROUTES = [
    (["hub"], []),
    (["hub", "--page", "history"], ["--page", "History"]),
    (["hub", "--page", "Home"], ["--page", "Home"]),
    (["hub", "--page", "hot phrases"], ["--page", "Hot Phrases"]),
    (["phrases"], ["--page", "Hot Phrases"]),
    (["setup"], ["--setup"]),
]


@posix_only
@pytest.mark.parametrize(("args", "child_args"), ROUTES)
@pytest.mark.parametrize("opt_in", ["flag", "env"])
def test_gpui_routes_send_a_closed_argv(args, child_args, opt_in, fake, legacy, home, tmp_path):
    desktop = fake()
    config = tmp_path / "my config.toml"
    config.write_text(SET_UP)
    env = _gpui_env(desktop, LOCALFLOW_CONFIG=str(config))
    if opt_in == "flag":
        args = [args[0], "--gpui", *args[1:]]
    else:
        env["LOCALFLOW_UI"] = "gpui"
    result = _invoke(args, env=env)

    assert result.exit_code == 0, result.output
    assert legacy.calls == []
    assert desktop.argvs() == [["--help"], child_args]
    launched = desktop.runs()[1]["env"]
    assert launched["HOME"] == str(home) and launched["LOCALFLOW_CONFIG"] == str(config)
    assert launched["LOCALFLOW_SANDBOX"] is None
    # History Retry can use this install's speech model through it.
    assert launched["LOCALFLOW_PYTHON"] == sys.executable
    assert result.output == ""  # nothing from the native child is relayed
    assert not (home / ".localflow").exists() and config.read_text() == SET_UP  # no launcher writes


DEFAULT_HUB_ROUTES = [route for route in ROUTES if route[0][0] != "setup"]


@posix_only
@pytest.mark.parametrize(("args", "child_args"), DEFAULT_HUB_ROUTES)
def test_linux_default_prefers_the_owned_install(args, child_args, fake, legacy, home, set_platform,
                                                 configured):
    set_platform("linux")
    root, _ = _linux_install(home)
    desktop = fake(directory=root / "bin")
    result = _invoke(args)
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"], child_args]
    assert legacy.calls == []
    assert result.output == ""


@posix_only
@pytest.mark.parametrize(("args", "child_args"), DEFAULT_HUB_ROUTES)
def test_linux_default_before_setup_gives_the_not_set_up_notice(
        args, child_args, fake, legacy, home, set_platform, active_config):
    # PR 103's rule applies to the default too: probed, never started.
    set_platform("linux")
    root, _ = _linux_install(home)
    desktop = fake(directory=root / "bin")
    result = _invoke(args)
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"]]
    assert result.output == desktop_launch.NOTICE_NOT_SET_UP + "\n"
    page = child_args[-1] if child_args else "Home"
    expected = ("phrases", None) if args[0] == "phrases" else _hub_legacy(page)
    assert legacy.calls == [expected]
    assert not active_config.exists()


@posix_only
@pytest.mark.parametrize("kwargs", [{}, {"help_text": OLD_HELP}, {"launch": "exit1"}])
def test_linux_setup_window_stays_legacy_by_default_even_with_an_owned_install(
        kwargs, fake, legacy, home, set_platform):
    # Preview's wizard would fetch native weights the Python runtime can't use.
    set_platform("linux")
    root, _ = _linux_install(home)
    desktop = fake(directory=root / "bin", **kwargs)
    for env in ({}, {"LOCALFLOW_UI": ""}, {"LOCALFLOW_RUNTIME": "rust"}):
        legacy.calls.clear()
        result = _invoke(["setup"], env=env)
        assert result.exit_code == 0, result.output
        assert legacy.calls == [("setup", None)]
        assert result.output == ""
    assert desktop.argvs() == []  # nothing discovered or probed


@posix_only
@pytest.mark.parametrize("opt_in", ["flag", "env"])
def test_linux_explicit_setup_still_opens_the_preview_wizard(opt_in, fake, legacy, home, set_platform):
    set_platform("linux")
    root, _ = _linux_install(home)
    desktop = fake(directory=root / "bin")
    args, env = (["setup", "--gpui"], {}) if opt_in == "flag" else (["setup"], {"LOCALFLOW_UI": "gpui"})
    result = _invoke(args, env=env)
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"], ["--setup"]]
    assert legacy.calls == [] and result.output == ""


@pytest.mark.parametrize("name", ["linux", "win32"])
@pytest.mark.parametrize("command", COMMANDS)
def test_default_without_an_install_opens_legacy_silently(command, name, legacy, no_process,
                                                          set_platform):
    set_platform(name)
    for env in ({}, {"LOCALFLOW_UI": ""}, {"LOCALFLOW_RUNTIME": "rust"},
                {"LOCALFLOW_RUNTIME": ""}, {"LOCALFLOW_RUNTIME": "secret-garbage"}):
        legacy.calls.clear()
        result = _invoke([command], env=env)
        assert result.exit_code == 0, result.output
        assert legacy.calls == [LEGACY_FOR[command]]
        assert result.output == "", env
    assert no_process.probed == [] and no_process.spawned == []


@pytest.mark.parametrize("name", ["darwin", "freebsd14"])
@pytest.mark.parametrize("command", COMMANDS)
def test_mac_and_unknown_default_stay_legacy_and_override_alone_does_not_opt_in(
        command, name, legacy, no_process, home, tmp_path, set_platform):
    set_platform(name)
    _mac_install(home)
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    for env in ({}, {"LOCALFLOW_DESKTOP_BIN": str(desktop.path)}, {"LOCALFLOW_UI": ""},
                {"LOCALFLOW_RUNTIME": "rust"}):
        legacy.calls.clear()
        result = _invoke([command], env=env)
        assert result.exit_code == 0, result.output
        assert legacy.calls == [LEGACY_FOR[command]]
        assert result.output == ""
    assert desktop.runs() == []  # never probed
    assert no_process.probed == [] and no_process.spawned == []


@pytest.mark.parametrize("name", ["linux", "win32", "darwin"])
@pytest.mark.parametrize("value", ["python", " Python ", "PYTHON"])
@pytest.mark.parametrize("command", COMMANDS)
def test_runtime_python_opens_legacy_without_probing(command, value, name, legacy, no_process,
                                                     home, tmp_path, set_platform):
    set_platform(name)
    _linux_install(home)
    _windows_install(home.joinpath("AppData", "Local"))
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    for extra in ({}, {"LOCALFLOW_UI": "gpui"}, {"LOCALFLOW_UI": "bogus"},
                  {"LOCALFLOW_DESKTOP_BIN": str(desktop.path)}, {"LOCALFLOW_SANDBOX": "1"}):
        legacy.calls.clear()
        result = _invoke([command], env={"LOCALFLOW_RUNTIME": value, **extra})
        assert result.exit_code == 0, result.output
        assert legacy.calls == [LEGACY_FOR[command]]
        assert result.output == ""
    assert desktop.runs() == []
    assert no_process.probed == [] and no_process.spawned == []


@posix_only
@pytest.mark.parametrize("command", COMMANDS)
def test_gpui_flag_beats_runtime_python(command, fake, legacy, configured):
    desktop = fake()
    result = _invoke([command, "--gpui"], env=_gpui_env(desktop, LOCALFLOW_RUNTIME="python"))
    assert result.exit_code == 0, result.output
    assert legacy.calls == [] and len(desktop.argvs()) == 2


@posix_only
@pytest.mark.parametrize("value", ["", "rust", "secret-garbage"])
def test_other_runtime_values_keep_the_linux_default(value, fake, legacy, home, set_platform,
                                                     configured):
    set_platform("linux")
    root, _ = _linux_install(home)
    desktop = fake(directory=root / "bin")
    result = _invoke(["hub"], env={"LOCALFLOW_RUNTIME": value})
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"], []]
    assert legacy.calls == [] and result.output == ""


def test_windows_default_launches_the_owned_folder(legacy, no_process, home, tmp_path, set_platform,
                                                   configured):
    set_platform("win32")
    local = tmp_path.joinpath("Local AppData")
    _, exe = _windows_install(local)
    assert exe.parts[-3:] == ("Programs", "LocalFlow Preview", "lf-desktop.exe")
    result = _invoke(["hub"], env={"LOCALAPPDATA": str(local)})
    assert result.exit_code == 0, result.output
    assert no_process.probed == [exe]
    assert no_process.spawned == [[str(exe)]]
    assert legacy.calls == [] and result.output == ""
    result = _invoke(["phrases"], env={"LOCALAPPDATA": str(local)})
    assert result.exit_code == 0 and no_process.spawned[-1] == [str(exe), "--page", "Hot Phrases"]
    assert legacy.calls == []

    # The setup window stays on the Python wizard by default: nothing is probed.
    no_process.probed.clear()
    no_process.spawned.clear()
    result = _invoke(["setup"], env={"LOCALAPPDATA": str(local)})
    assert result.exit_code == 0 and result.output == ""
    assert legacy.calls == [("setup", None)]
    assert no_process.probed == [] and no_process.spawned == []
    # ...but an explicit choice still opens Preview's --setup.
    legacy.calls.clear()
    for args, env in ((["setup", "--gpui"], {}), (["setup"], {"LOCALFLOW_UI": "gpui"})):
        result = _invoke(args, env={"LOCALAPPDATA": str(local), **env})
        assert result.exit_code == 0 and result.output == ""
        assert no_process.spawned[-1] == [str(exe), "--setup"]
    assert no_process.probed == [exe, exe] and legacy.calls == []

    no_process.probed.clear()
    no_process.spawned.clear()
    result = _invoke(["hub"], env={"LOCALAPPDATA": str(local), "LOCALFLOW_RUNTIME": "python"})
    assert result.exit_code == 0 and result.output == ""
    assert legacy.calls == [_hub_legacy()]
    assert no_process.probed == [] and no_process.spawned == []


@pytest.mark.parametrize(("command", "expected"), [("hub", _hub_legacy()), ("phrases", ("phrases", None))])
def test_windows_default_before_setup_gives_the_not_set_up_notice(
        command, expected, legacy, no_process, tmp_path, set_platform, active_config):
    set_platform("win32")
    local = tmp_path.joinpath("Local AppData")
    _, exe = _windows_install(local)
    result = _invoke([command], env={"LOCALAPPDATA": str(local)})
    assert result.exit_code == 0, result.output
    assert no_process.probed == [exe] and no_process.spawned == []  # probed, never started
    assert result.output == desktop_launch.NOTICE_NOT_SET_UP + "\n"
    assert legacy.calls == [expected]


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_explicit_legacy_never_probes_even_when_env_opts_in(command, legacy, tmp_path):
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    result = _invoke([command, "--legacy"], env=_gpui_env(desktop, LOCALFLOW_UI="gpui"))
    assert result.exit_code == 0, result.output
    assert legacy.calls == [LEGACY_FOR[command]]
    assert desktop.runs() == []


def test_legacy_hub_keeps_the_canonical_page(legacy):
    for args, page in ((["hub"], "Home"), (["hub", "--legacy", "--page", "settings"], "Settings")):
        legacy.calls.clear()
        assert _invoke(args).exit_code == 0
        assert legacy.calls == [_hub_legacy(page)]
    assert _invoke(["hub", "--gpui", "--page", "Nope"]).exit_code == 2
    assert legacy.calls == [_hub_legacy("Settings")]


def test_invalid_env_value_uses_legacy_with_a_static_notice(legacy):
    result = _invoke(["hub"], env={"LOCALFLOW_UI": "secret-value-xyz"})
    assert result.exit_code == 0
    assert legacy.calls == [_hub_legacy()]
    assert result.output.strip() == desktop_launch.NOTICE_INVALID_ENV
    assert "secret-value-xyz" not in result.output


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_unsupported_platform_falls_back_once(command, legacy, monkeypatch, tmp_path):
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    monkeypatch.setattr(desktop_launch.sys, "platform", "freebsd14")
    result = _invoke([command, "--gpui"], env=_gpui_env(desktop))
    assert result.exit_code == 0
    assert legacy.calls == [LEGACY_FOR[command]]
    assert desktop_launch.NOTICE_PLATFORM in result.output
    assert desktop.runs() == []


def test_not_installed_falls_back_once(legacy):
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    result = _invoke(["phrases", "--gpui"])
    assert result.exit_code == 0
    assert legacy.calls == [("phrases", None)]
    expected = desktop_launch.not_installed_notice(desktop_launch.platform_key())
    assert result.output.strip() == expected


@pytest.mark.parametrize("opt_in", [["--gpui"], []])
@pytest.mark.parametrize("command", COMMANDS)
def test_explicit_gpui_on_windows_without_an_install_gives_the_windows_notice(
        command, opt_in, legacy, no_process, set_platform):
    set_platform("win32")
    env = {} if opt_in else {"LOCALFLOW_UI": "gpui"}
    result = _invoke([command, *opt_in], env=env)
    assert result.exit_code == 0
    assert legacy.calls == [LEGACY_FOR[command]]
    assert result.output == desktop_launch.NOTICE_NOT_INSTALLED_WINDOWS + "\n"
    assert no_process.probed == [] and no_process.spawned == []


@posix_only
def test_owned_install_is_used_without_an_override(home, fake, legacy, configured):
    if sys.platform == "darwin":
        app, _ = _mac_install(home)
        directory = app / "Contents/MacOS"
    else:
        root, _ = _linux_install(home)
        directory = root / "bin"
    desktop = fake(directory=directory)
    result = _invoke(["hub", "--gpui"])
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"], []]
    assert legacy.calls == []


# --------------------------------------------- not set up: no preview wizard ---

HUB_ROUTES = [
    (["hub", "--gpui"], {}, _hub_legacy(), []),
    (["hub", "--page", "Home", "--gpui"], {}, _hub_legacy(), ["--page", "Home"]),
    (["hub"], {"LOCALFLOW_UI": "gpui"}, _hub_legacy(), []),
    (["phrases", "--gpui"], {}, ("phrases", None), ["--page", "Hot Phrases"]),
]


@pytest.fixture
def active_config(tmp_path, monkeypatch):
    """The active config path, not yet written (an empty home has no config)."""
    config = tmp_path / "config" / "config.toml"
    config.parent.mkdir()
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(config))
    return config


@posix_only
@pytest.mark.parametrize(("args", "extra", "expected", "_child"), HUB_ROUTES)
@pytest.mark.parametrize("state", ["missing", "empty", "legacy-flat-file-only", "setup-0", "malformed"])
def test_hub_routes_never_open_the_preview_wizard_before_setup(
        args, extra, expected, _child, state, fake, legacy, home, active_config):
    if state == "empty":
        active_config.write_text("")
    elif state == "legacy-flat-file-only":
        (home / ".localflow.toml").write_text('model_size = "small"\n')  # not migrated yet
    elif state == "setup-0":
        active_config.write_text("[setup]\nversion = 0\n")
    elif state == "malformed":
        active_config.write_text("[setup\nversion = 1\n")
    desktop = fake()
    result = _invoke(args, env=_gpui_env(desktop, **extra))
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"]]  # probed, never started
    assert result.output == desktop_launch.NOTICE_NOT_SET_UP + "\n"
    assert legacy.calls == [expected]


@posix_only
def test_unreadable_config_location_is_not_set_up(fake, legacy, monkeypatch, tmp_path):
    from localflow import config

    class Untraversable(type(tmp_path)):
        def exists(self, *args, **kwargs):
            raise PermissionError(13, "Permission denied", str(self))

    monkeypatch.setattr(config, "config_path", lambda: Untraversable(tmp_path / "locked" / "config.toml"))
    desktop = fake()
    result = _invoke(["hub", "--gpui"], env=_gpui_env(desktop))
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"]]
    assert result.output == desktop_launch.NOTICE_NOT_SET_UP + "\n"
    assert legacy.calls == [_hub_legacy()]


@posix_only
@pytest.mark.parametrize(("args", "extra", "_expected", "child_args"), HUB_ROUTES)
@pytest.mark.parametrize("marker", [SET_UP, 'config_version = 2\n\n[stt]\nmodel = "base"\n'])
def test_hub_routes_open_the_preview_once_set_up(
        args, extra, _expected, child_args, marker, fake, legacy, active_config):
    active_config.write_text(marker)
    desktop = fake()
    result = _invoke(args, env=_gpui_env(desktop, **extra))
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"], child_args]
    assert result.output == "" and legacy.calls == []


@posix_only
def test_setup_route_still_opens_the_preview_wizard_before_setup(fake, legacy, active_config):
    desktop = fake()
    result = _invoke(["setup", "--gpui"], env=_gpui_env(desktop))
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"], ["--setup"]]
    assert result.output == "" and legacy.calls == []
    assert not active_config.exists()


@pytest.mark.parametrize(("args", "extra", "expected", "_child"), HUB_ROUTES)
def test_missing_preview_before_setup_keeps_the_not_installed_notice(
        args, extra, expected, _child, legacy, active_config):
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    result = _invoke(args, env=extra)
    assert result.exit_code == 0
    expected_notice = desktop_launch.not_installed_notice(desktop_launch.platform_key())
    assert result.output == expected_notice + "\n"
    assert legacy.calls == [expected]


# ------------------------------------------------------- sandbox boundary ---


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
@pytest.mark.parametrize("opt_in", [["--gpui"], []])
def test_sandbox_request_is_refused_not_downgraded(command, opt_in, legacy, home, tmp_path):
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    for value in ("1", "true"):
        env = _gpui_env(desktop, LOCALFLOW_SANDBOX=value)
        if not opt_in:
            env["LOCALFLOW_UI"] = "gpui"
        result = _invoke([command, *opt_in], env=env)
        assert result.exit_code == 1
        assert "lf-desktop --sandbox" in result.output and "LOCALFLOW_SANDBOX" in result.output
    assert legacy.calls == []  # no un-sandboxed fallback
    assert desktop.runs() == []  # nothing probed or launched
    assert list(home.iterdir()) == []  # no launcher logs or data


@pytest.mark.parametrize("name", ["linux", "win32", "darwin"])
@pytest.mark.parametrize("command", COMMANDS)
def test_sandbox_env_keeps_its_legacy_semantics(command, name, legacy, no_process, home, tmp_path,
                                                set_platform):
    # The platform default never refuses and never probes: even with an owned
    # install or an override present, the sandbox gets today's legacy window.
    set_platform(name)
    _linux_install(home)
    _windows_install(home.joinpath("AppData", "Local"))
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    for env in ({"LOCALFLOW_SANDBOX": "1"}, {"LOCALFLOW_SANDBOX": "true"},
                {"LOCALFLOW_SANDBOX": "1", "LOCALFLOW_DESKTOP_BIN": str(desktop.path)}):
        legacy.calls.clear()
        result = _invoke([command], env=env)
        assert result.exit_code == 0, result.output
        assert legacy.calls == [LEGACY_FOR[command]] and result.output == ""
        result = _invoke([command, "--legacy"], env=env)
        assert result.exit_code == 0 and legacy.calls[-1] == LEGACY_FOR[command]
    assert desktop.runs() == []
    assert no_process.probed == [] and no_process.spawned == []
    # An explicit GPUI choice with the same sandbox request is still refused.
    legacy.calls.clear()
    result = _invoke([command, "--gpui"], env={"LOCALFLOW_SANDBOX": "1"})
    assert result.exit_code == 1 and "lf-desktop --sandbox" in result.output
    assert legacy.calls == [] and no_process.probed == []


@posix_only
def test_non_native_sandbox_values_are_not_a_sandbox(fake, legacy, configured):
    desktop = fake()
    result = _invoke(["hub", "--gpui"], env=_gpui_env(desktop, LOCALFLOW_SANDBOX="0"))
    assert result.exit_code == 0 and legacy.calls == []
    assert desktop.runs()[1]["env"]["LOCALFLOW_SANDBOX"] == "0"  # passed through untouched


# ---------------------------------------------------- terminal & subcommands ---


def test_terminal_setup_and_its_errors_ignore_ui_flags(monkeypatch, legacy, tmp_path):
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    env = _gpui_env(desktop, LOCALFLOW_UI="gpui")
    ran = []
    monkeypatch.setattr(cli, "_setup_cli", lambda *a: ran.append(a))

    assert _invoke(["setup", "--cli", "--gpui"], env=env).exit_code == 0
    assert ran == [(None, None, False, None)]
    yes = _invoke(["setup", "--yes", "--gpui"], env=env)
    assert yes.exit_code == 2 and "--yes requires --cli" in yes.output
    unknown = _invoke(["setup", "--cli", "--yes", "--stt", "nope", "--gpui"], env=env)
    assert unknown.exit_code == 1 and "unknown speech model" in unknown.output
    assert legacy.calls == [] and desktop.runs() == []


@pytest.mark.parametrize("args", [["phrases", "--gpui", "install-app"], ["setup", "--gpui", "install-app"],
                                  ["phrases", "--gpui", "uninstall-app"], ["setup", "--gpui", "uninstall-app"]])
def test_subcommands_never_reach_the_selector(args, monkeypatch, legacy, tmp_path):
    from localflow import appbundle

    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    bundles = []
    monkeypatch.setattr(appbundle, "install_app", lambda **kw: bundles.append(kw) or tmp_path / "x.app")
    monkeypatch.setattr(appbundle, "uninstall_app", lambda **kw: bundles.append(kw) or True)
    result = _invoke(args, env=_gpui_env(desktop, LOCALFLOW_UI="gpui"))
    assert result.exit_code == 0, result.output
    assert bundles and legacy.calls == [] and desktop.runs() == []


# ------------------------------------------------------- process fallback ---


@pytest.mark.parametrize("name", ["linux", "win32"])
def test_default_with_a_foreign_install_location_notices_and_falls_back_once(
        name, legacy, no_process, home, set_platform):
    set_platform(name)
    _linux_install(home, manifest=b'{"name": "localflow"}')
    _windows_install(home.joinpath("AppData", "Local"), manifest=b'{"name": "localflow"}')
    result = _invoke(["hub"])
    assert result.exit_code == 0
    assert legacy.calls == [_hub_legacy()]
    assert result.output.strip() == desktop_launch.NOTICE_NOT_OWNED
    assert no_process.probed == []


@pytest.mark.parametrize("name", ["linux", "win32"])
def test_default_with_a_bad_override_notices_and_falls_back_once(name, legacy, no_process, tmp_path,
                                                                 set_platform):
    set_platform(name)
    for override in ("secret-relative/lf-desktop", str(tmp_path / "secret-missing")):
        legacy.calls.clear()
        result = _invoke(["phrases"], env={"LOCALFLOW_DESKTOP_BIN": override})
        assert result.exit_code == 0
        assert legacy.calls == [("phrases", None)]
        assert result.output.strip() == desktop_launch.NOTICE_BAD_OVERRIDE
        assert "secret" not in result.output
    assert no_process.probed == []


@pytest.mark.parametrize("manifest", [b'{"name": "localflow"}', _WIN_MANIFEST])
@pytest.mark.parametrize("name", ["linux", "win32"])
def test_default_setup_window_ignores_a_broken_or_valid_install_silently(
        name, manifest, legacy, no_process, home, tmp_path, set_platform):
    # The setup window's default is legacy, so nothing is discovered: no notice
    # for a foreign install or a bad override, and a valid one isn't probed.
    set_platform(name)
    _linux_install(home, manifest=manifest)
    _windows_install(home.joinpath("AppData", "Local"), manifest=manifest)
    for env in ({}, {"LOCALFLOW_DESKTOP_BIN": "secret-relative/lf-desktop"},
                {"LOCALFLOW_DESKTOP_BIN": str(tmp_path / "secret-missing")}):
        legacy.calls.clear()
        result = _invoke(["setup"], env=env)
        assert result.exit_code == 0
        assert legacy.calls == [("setup", None)] and result.output == ""
    assert no_process.probed == [] and no_process.spawned == []


@posix_only
@pytest.mark.parametrize(("kwargs", "notice"), [
    ({"help_text": OLD_HELP}, desktop_launch.NOTICE_PROBE),
    ({"help_text": SCOPE3_HELP}, desktop_launch.NOTICE_PROBE),
    ({"help_text": OTHER_IDENTITY_HELP}, desktop_launch.NOTICE_PROBE),
    ({"help_code": 3}, desktop_launch.NOTICE_PROBE),
    ({"vanish": True}, desktop_launch.NOTICE_SPAWN),
    ({"launch": "exit1"}, desktop_launch.NOTICE_EARLY_EXIT.format(code=1)),
    ({"launch": "exit2"}, desktop_launch.NOTICE_EARLY_EXIT.format(code=2)),
])
@pytest.mark.parametrize("command", ["hub", "phrases"])
def test_default_with_a_stale_or_failing_install_notices_and_falls_back_once(
        command, kwargs, notice, fake, legacy, home, set_platform, configured):
    set_platform("linux")
    root, _ = _linux_install(home)
    fake(directory=root / "bin", **kwargs)
    result = _invoke([command])
    assert result.exit_code == 0
    assert legacy.calls == [LEGACY_FOR[command]]
    assert result.output.strip() == notice
    assert "SECRET" not in result.output


@posix_only
@pytest.mark.parametrize("kwargs", [{"help_text": OLD_HELP}, {"help_text": "nope"},
                                    {"raw_help": b"\xff" * 10}, {"help_code": 3},
                                    {"help_text": OTHER_IDENTITY_HELP},
                                    *({"help_text": text} for text in NEAR_FLAGS_HELPS)])
def test_stale_or_wrong_binary_falls_back_once(kwargs, fake, legacy):
    desktop = fake(**kwargs)
    result = _invoke(["setup", "--gpui"], env=_gpui_env(desktop))
    assert result.exit_code == 0
    assert legacy.calls == [("setup", None)]
    assert result.output.strip() == desktop_launch.NOTICE_PROBE
    assert desktop.argvs() == [["--help"]]


@posix_only
@pytest.mark.parametrize(("command", "expected"), [
    (["hub", "--gpui"], _hub_legacy()),
    (["hub", "--gpui", "--page", "history"], _hub_legacy("History")),
    (["phrases", "--gpui"], ("phrases", None)),
    (["setup", "--gpui"], ("setup", None)),
])
def test_scope3_preview_is_never_launched_because_its_exit_0_proves_nothing(
        command, expected, fake, legacy):
    # Every flag lf sends is there, but a Scope 3 build exits 0 even when its
    # window can't open: launching it could look like a hand-off and show nothing.
    desktop = fake(help_text=SCOPE3_HELP, launch="exit0")
    result = _invoke(command, env=_gpui_env(desktop))
    assert result.exit_code == 0
    assert desktop.argvs() == [["--help"]]  # probed, never started as a window
    assert legacy.calls == [expected]  # legacy exactly once
    assert result.output.strip() == desktop_launch.NOTICE_PROBE
    assert "update LocalFlow Preview" in result.output


@posix_only
def test_window_open_failure_status_falls_back_once(fake, legacy, configured):
    # A current build that couldn't open a window now exits 1 (not 0).
    desktop = fake(launch="exit1")
    result = _invoke(["hub", "--gpui"], env=_gpui_env(desktop))
    assert legacy.calls == [_hub_legacy()]
    assert result.output.strip() == desktop_launch.NOTICE_EARLY_EXIT.format(code=1)


@posix_only
def test_hanging_probe_falls_back_once(fake, legacy, monkeypatch):
    desktop = fake(help_hang=True)
    monkeypatch.setattr(desktop_launch, "PROBE_TIMEOUT", 1.0)
    result = _invoke(["hub", "--gpui"], env=_gpui_env(desktop))
    assert result.exit_code == 0
    assert legacy.calls == [_hub_legacy()]
    assert result.output.strip() == desktop_launch.NOTICE_PROBE


@posix_only
def test_spawn_failure_falls_back_once(fake, legacy, configured):
    desktop = fake(vanish=True)  # passes the probe, then is gone
    result = _invoke(["phrases", "--gpui"], env=_gpui_env(desktop))
    assert result.exit_code == 0
    assert legacy.calls == [("phrases", None)]
    assert result.output.strip() == desktop_launch.NOTICE_SPAWN


@posix_only
@pytest.mark.parametrize("code", [1, 2])
def test_early_exit_falls_back_once_without_relaying_output(code, fake, legacy, configured):
    desktop = fake(launch=f"exit{code}")
    result = _invoke(["hub", "--gpui", "--page", "models"], env=_gpui_env(desktop))
    assert result.exit_code == 0
    assert legacy.calls == [_hub_legacy("Models")]
    assert result.output.strip() == desktop_launch.NOTICE_EARLY_EXIT.format(code=code)
    assert "SECRET" not in result.output


@posix_only
@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_exit_zero_is_a_handoff_not_a_fallback(command, fake, legacy, configured):
    desktop = fake(launch="exit0")
    result = _invoke([command, "--gpui"], env=_gpui_env(desktop))
    assert result.exit_code == 0 and result.output == ""
    assert legacy.calls == []
    assert len(desktop.argvs()) == 2


@posix_only
def test_healthy_window_is_left_running_after_the_startup_wait(fake, legacy, monkeypatch):
    desktop = fake(launch="stay")
    monkeypatch.setattr(desktop_launch, "STARTUP_WAIT", 0.5)
    started = time.monotonic()
    result = _invoke(["setup", "--gpui"], env=_gpui_env(desktop))
    assert result.exit_code == 0 and legacy.calls == []
    assert time.monotonic() - started < 10  # returns after startup, not on close
    runs = _wait_for_runs(desktop, 2)
    assert [run["argv"] for run in runs] == [["--help"], ["--setup"]]
    pid = runs[1]["pid"]
    try:
        assert os.waitpid(pid, os.WNOHANG) == (0, 0)  # still running, never killed
        # Its own session: a terminal hang-up for `lf` doesn't reach the window.
        assert os.getsid(pid) == pid
    finally:
        os.kill(pid, signal.SIGKILL)
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:  # already reaped by subprocess's own cleanup
            pass


@posix_only
def test_override_path_with_spaces_and_metacharacters_is_not_shell_interpreted(
        fake, legacy, tmp_path, configured):
    directory = tmp_path / "dir with space $(touch pwned) ; `touch pwned2` & 'q'"
    desktop = fake(directory=directory)
    result = _invoke(["hub", "--gpui", "--page", "dictionary"], env=_gpui_env(desktop))
    assert result.exit_code == 0, result.output
    assert desktop.argvs() == [["--help"], ["--page", "Dictionary"]]
    assert legacy.calls == []
    assert not list(tmp_path.rglob("pwned*")) and not list(Path.cwd().glob("pwned*"))


# ---------------------------------------------------------------- wiring ---


def test_suite_guard_matches_the_launcher():
    import conftest

    assert conftest._LAUNCHER_ENVS == (desktop_launch.UI_ENV, desktop_launch.BIN_ENV)
    # Captured at collection, before any fixture stubs it: the real discovery.
    assert REAL_FIND_DESKTOP.__module__ == "localflow.desktop_launch"
    assert desktop_launch.find_desktop is REAL_FIND_DESKTOP


def test_only_the_cli_uses_the_launcher_and_dispatchers_stay_legacy():
    users = sorted(
        path.name for path in (ROOT / "localflow").rglob("*.py")
        if "desktop_launch" in path.read_text(encoding="utf-8") and path.name != "desktop_launch.py"
    )
    assert users == ["cli.py"]


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_help_documents_the_platform_default(command):
    result = _invoke([command, "--help"])
    assert "--gpui" in result.output and "--legacy" in result.output
    text = " ".join(result.output.split())
    # One shared text, true for all three commands.
    assert "the default for lf hub and lf phrases on Linux and Windows" in text
    assert "the default for lf setup, on macOS" in text and "LOCALFLOW_RUNTIME=python" in text
    assert "(the default)" not in text and "the default on Linux and Windows" not in text


def test_spawn_uses_its_own_session_on_posix_and_process_group_on_windows(monkeypatch):
    # posix: a hang-up of lf's terminal doesn't reach the window. Windows: a
    # Ctrl+C in lf's console doesn't (it stays on that console).
    seen = []

    class Proc:
        def wait(self, timeout):
            raise fake_subprocess.TimeoutExpired("lf-desktop", timeout)

    real = desktop_launch.subprocess
    fake_subprocess = types.SimpleNamespace(
        Popen=lambda argv, **kw: seen.append(kw) or Proc(), DEVNULL=real.DEVNULL,
        TimeoutExpired=real.TimeoutExpired, CREATE_NEW_PROCESS_GROUP=0x200)
    monkeypatch.setattr(desktop_launch, "subprocess", fake_subprocess)
    environ = desktop_launch.os.environ
    for name in ("posix", "nt"):
        monkeypatch.setattr(desktop_launch, "os", types.SimpleNamespace(name=name, environ=environ))
        assert desktop_launch.spawn(["lf-desktop"], wait=0) == (True, None)
    assert seen[0]["start_new_session"] is True and "creationflags" not in seen[0]
    assert seen[1]["creationflags"] == 0x200 and "start_new_session" not in seen[1]
