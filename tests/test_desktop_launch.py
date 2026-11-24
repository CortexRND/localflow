"""LocalFlow Preview launch from `lf hub`, `lf phrases` and `lf setup`. By
default, a set-up Hub / phrases window uses an installed package; setup, first
run and everything else open the legacy window.

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
UI_ENVS = ("LOCALFLOW_UI", "LOCALFLOW_DESKTOP_BIN", "LOCALFLOW_SANDBOX", "LOCALFLOW_CONFIG")


@pytest.fixture(autouse=True)
def home(tmp_path, monkeypatch):
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("USERPROFILE", str(home))
    for name in UI_ENVS:
        monkeypatch.delenv(name, raising=False)
    return home


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
                              ("HOME", "LOCALFLOW_CONFIG", "LOCALFLOW_SANDBOX", "LOCALFLOW_UI")}}}}
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


def test_selection_precedence_is_flag_then_env_then_gpui_default():
    assert desktop_launch.DEFAULT_UI == "gpui"
    assert desktop_launch.choose_ui(None, {}) == ("gpui", None)
    assert desktop_launch.choose_ui(None, {"LOCALFLOW_UI": " \t"}) == ("gpui", None)
    assert desktop_launch.choose_ui(None, {"LOCALFLOW_UI": "gpui"}) == ("gpui", None)
    assert desktop_launch.choose_ui(None, {"LOCALFLOW_UI": " GPUI "}) == ("gpui", None)
    assert desktop_launch.choose_ui(None, {"LOCALFLOW_UI": "legacy"}) == ("legacy", None)
    assert desktop_launch.choose_ui("legacy", {"LOCALFLOW_UI": "gpui"}) == ("legacy", None)
    assert desktop_launch.choose_ui("gpui", {"LOCALFLOW_UI": "legacy"}) == ("gpui", None)
    assert desktop_launch.choose_ui("gpui", {"LOCALFLOW_UI": "bogus"}) == ("gpui", None)
    assert desktop_launch.choose_ui(None, {"LOCALFLOW_UI": "bogus"}) == (
        "legacy", desktop_launch.NOTICE_INVALID_ENV)
    # Only the default itself is implicit (quiet when nothing is installed).
    implicit = desktop_launch.implicit_choice
    assert implicit(None, {}) and implicit(None, {"LOCALFLOW_UI": " \n"})
    assert implicit(None, {"LOCALFLOW_DESKTOP_BIN": "/x/lf-desktop"})
    for env in ({"LOCALFLOW_UI": "gpui"}, {"LOCALFLOW_UI": " Legacy "}, {"LOCALFLOW_UI": "bogus"}):
        assert not implicit(None, env), env
    assert not implicit("gpui", {}) and not implicit("legacy", {})


def test_candidate_absent_only_for_nothing_to_try(home, tmp_path, monkeypatch):
    absent = desktop_launch.candidate_absent
    override = {"LOCALFLOW_DESKTOP_BIN": "anything, even invalid"}
    for platform in ("win32", "cygwin", "freebsd14"):
        assert absent({}, platform, home) and absent(override, platform, home)
    for platform in ("linux", "darwin"):
        assert absent({}, platform, home)
        assert absent({"LOCALFLOW_DESKTOP_BIN": ""}, platform, home)
        assert absent({}, platform, Path("relhome"))
        assert not absent(override, platform, home)
        assert not absent(override, platform, Path("relhome"))
    monkeypatch.setenv("HOME", "relhome")
    assert absent({}, "linux")
    # Anything at the package root is a candidate, however malformed.
    root = home / ".local/share/localflow-preview"
    root.parent.mkdir(parents=True)
    root.write_text("not a package")
    assert not absent({}, "linux", home) and absent({}, "darwin", home)
    root.unlink()
    if os.name == "posix":
        root.symlink_to(tmp_path / "dangling")
        assert not absent({}, "linux", home)
        root.unlink()
    app = home / "Applications/LocalFlow Preview.app"
    app.mkdir(parents=True)
    assert not absent({}, "darwin", home) and absent({}, "linux", home)


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


@pytest.mark.parametrize("platform", ["win32", "cygwin", "freebsd14", "aix"])
def test_unsupported_platforms_never_discover(platform, home):
    exe = home / "lf-desktop"
    exe.write_text("x")
    env = {"LOCALFLOW_DESKTOP_BIN": str(exe)}
    assert desktop_launch.find_desktop(env, platform, home) == (None, desktop_launch.NOTICE_PLATFORM)


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
@pytest.mark.parametrize("opt_in", ["flag", "env", "default"])
def test_gpui_routes_send_a_closed_argv(args, child_args, opt_in, fake, legacy, home, tmp_path,
                                        monkeypatch):
    if opt_in == "default" and desktop_launch.platform_key() is None:
        pytest.skip("the default never looks for a preview package here")
    desktop = fake()
    config = tmp_path / "my config.toml"
    config.write_text(SET_UP)
    env = _gpui_env(desktop, LOCALFLOW_CONFIG=str(config))
    if opt_in == "flag":
        args = [args[0], "--gpui", *args[1:]]
    elif opt_in == "env":
        env["LOCALFLOW_UI"] = "gpui"
    if opt_in != "default":
        monkeypatch.setattr(desktop_launch, "configured", _boom)  # explicit: no early marker read
    result = _invoke(args, env=env)

    assert result.exit_code == 0, result.output
    assert result.output == ""  # nothing from the native child is relayed
    if opt_in == "default" and args[0] == "setup":
        assert legacy.calls == [("setup", None)]  # setup stays on the legacy wizard
        assert desktop.runs() == []
        assert config.read_text() == SET_UP
        return
    assert legacy.calls == []
    assert desktop.argvs() == [["--help"], child_args]
    launched = desktop.runs()[1]["env"]
    assert launched["HOME"] == str(home) and launched["LOCALFLOW_CONFIG"] == str(config)
    assert launched["LOCALFLOW_SANDBOX"] is None
    assert not (home / ".localflow").exists() and config.read_text() == SET_UP  # no launcher writes


CHILD_ARGS_FOR = {"hub": [], "phrases": ["--page", "Hot Phrases"], "setup": ["--setup"]}


def _boom(*args, **kwargs):
    raise AssertionError("looked for LocalFlow Preview or read the setup marker")


def _never_discovered(monkeypatch):
    """Fail the test if the launcher reads the setup marker, or looks for,
    probes or starts lf-desktop."""
    for name in ("find_desktop", "owned_install", "probe", "spawn", "configured"):
        monkeypatch.setattr(desktop_launch, name, _boom)


def _use_config(monkeypatch, tmp_path, text=None):
    """Point LOCALFLOW_CONFIG at a temporary file holding `text` (absent when None)."""
    config = tmp_path / "config" / "config.toml"
    config.parent.mkdir(exist_ok=True)
    if text is None:
        config.unlink(missing_ok=True)
    else:
        config.write_text(text, encoding="utf-8")
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(config))
    return config


@pytest.fixture
def set_up(monkeypatch, tmp_path):
    """A set-up LocalFlow: the implicit default may open LocalFlow Preview."""
    return _use_config(monkeypatch, tmp_path, SET_UP)


def _owned_package(home, fake, **kwargs):
    """The preview installer's package for this platform, running a fake."""
    if sys.platform == "darwin":
        app, _ = _mac_install(home)
        directory = app / "Contents/MacOS"
    else:
        root, _ = _linux_install(home)
        directory = root / "bin"
    return fake(directory=directory, **kwargs)


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_default_without_a_candidate_is_quiet_legacy_once(command, legacy, monkeypatch):
    _never_discovered(monkeypatch)
    for env in ({}, {"LOCALFLOW_UI": ""}, {"LOCALFLOW_UI": "  "}, {"LOCALFLOW_DESKTOP_BIN": ""},
                {"LOCALFLOW_SANDBOX": "1"}):
        legacy.calls.clear()
        result = _invoke([command], env=env)
        assert result.exit_code == 0, result.output
        assert legacy.calls == [LEGACY_FOR[command]], env
        assert result.output == "", env


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_default_on_unsupported_platforms_is_quiet_even_with_an_override(
        command, legacy, monkeypatch, tmp_path, home):
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    _linux_install(home)
    _mac_install(home)
    monkeypatch.setattr(desktop_launch.sys, "platform", "win32")
    for env in (_gpui_env(desktop), {"LOCALFLOW_DESKTOP_BIN": "relative"}):
        legacy.calls.clear()
        result = _invoke([command], env=env)
        assert result.exit_code == 0 and result.output == ""
        assert legacy.calls == [LEGACY_FOR[command]]
    assert desktop.runs() == []


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_default_with_a_relative_home_is_quiet(command, legacy, monkeypatch, tmp_path):
    monkeypatch.chdir(tmp_path)
    _linux_install(Path("relhome"))
    _mac_install(Path("relhome"))
    monkeypatch.setenv("HOME", "relhome")
    _never_discovered(monkeypatch)
    result = _invoke([command])
    assert result.exit_code == 0 and result.output == ""
    assert legacy.calls == [LEGACY_FOR[command]]


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_default_with_an_unresolvable_home_is_quiet(command, legacy, monkeypatch):
    # No HOME and no passwd entry: Path.home() raises, as lf-rs's home lookup
    # fails; the implicit default has nothing to find, so legacy opens quietly.
    def no_home(cls):
        raise RuntimeError("Could not determine home directory.")

    monkeypatch.setattr(desktop_launch.Path, "home", classmethod(no_home))
    for platform in ("linux", "darwin"):
        assert desktop_launch.candidate_absent({}, platform)
    _never_discovered(monkeypatch)
    result = _invoke([command])
    assert result.exit_code == 0 and result.output == ""
    assert legacy.calls == [LEGACY_FOR[command]]


@posix_only
@pytest.mark.parametrize("command", ["hub", "phrases"])
def test_default_opens_the_owned_package_with_a_closed_argv(command, home, fake, legacy, set_up):
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    desktop = _owned_package(home, fake)
    result = _invoke([command])
    assert result.exit_code == 0, result.output
    assert result.output == "" and legacy.calls == []  # a hand-off, no legacy window
    assert desktop.argvs() == [["--help"], CHILD_ARGS_FOR[command]]
    assert desktop.runs()[1]["env"]["LOCALFLOW_SANDBOX"] is None
    assert set_up.read_text() == SET_UP


@posix_only
@pytest.mark.parametrize("env", [{}, {"LOCALFLOW_SANDBOX": "1"}, {"LOCALFLOW_UI": " "}])
def test_default_setup_stays_on_the_legacy_wizard_and_inspects_nothing(
        env, home, fake, legacy, set_up, monkeypatch):
    # Native setup writes native model choices the Python runtime may not run,
    # so the default never sends setup there, even with a good package.
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    desktop = _owned_package(home, fake)
    override = fake(name="override")
    _never_discovered(monkeypatch)
    monkeypatch.setattr(desktop_launch, "candidate_absent", _boom)
    for extra in ({}, _gpui_env(override)):
        legacy.calls.clear()
        result = _invoke(["setup"], env={**env, **extra})
        assert result.exit_code == 0 and result.output == ""
        assert legacy.calls == [("setup", None)]
    assert desktop.runs() == [] and override.runs() == []
    # The Setup page is the same request.
    assert desktop_launch.setup_route("Setup", False) and desktop_launch.setup_route(" Setup ", False)
    assert desktop_launch.setup_route(None, True)
    assert not desktop_launch.setup_route("Home", False) and not desktop_launch.setup_route(None, False)
    calls = []
    desktop_launch.launch(None, lambda: calls.append("legacy"), page="Setup", env={})
    assert calls == ["legacy"]


FIRST_RUN_CONFIGS = {
    "no config": None,
    "malformed": "[setup\nversion = 1\n",
    "unsupported version": "config_version = 99\n\n[setup]\nversion = 1\n",
    "marker zero": "config_version = 2\n\n[setup]\nversion = 0\n",
    "marker not an int": "config_version = 2\n\n[setup]\nversion = true\n",
    "blank speech choice": 'config_version = 2\n\n[stt]\nprovider = " "\nmodel = ""\n',
    # Missing config_version warns in load_config; the launcher stays quiet.
    "only other settings": '[llm]\nmodel = "llama3"\n',
}


@posix_only
@pytest.mark.parametrize("config", FIRST_RUN_CONFIGS.values(), ids=FIRST_RUN_CONFIGS.keys())
@pytest.mark.parametrize(("args", "expected"), [
    (["hub"], _hub_legacy()),
    (["hub", "--page", "history"], _hub_legacy("History")),
    (["phrases"], ("phrases", None)),
])
def test_default_first_run_stays_on_the_legacy_window(
        config, args, expected, home, fake, legacy, monkeypatch, tmp_path):
    # Without the setup marker LocalFlow Preview would open its own wizard,
    # even for a page, so the default opens the legacy window quietly.
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    path = _use_config(monkeypatch, tmp_path, config)
    desktop = _owned_package(home, fake)
    before = sorted(p.relative_to(home) for p in home.rglob("*"))
    for env in ({}, {"LOCALFLOW_SANDBOX": "1"}, _gpui_env(fake(name="override"))):
        legacy.calls.clear()
        result = _invoke(args, env=env)
        assert result.exit_code == 0 and result.output == ""
        assert legacy.calls == [expected]
    assert desktop.runs() == []
    assert (path.read_text() if path.exists() else None) == config  # nothing written
    assert sorted(p.relative_to(home) for p in home.rglob("*")) == before


@posix_only
def test_default_first_run_ignores_an_unreadable_legacy_file(home, fake, legacy, monkeypatch, tmp_path):
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    _use_config(monkeypatch, tmp_path)
    (home / ".localflow.toml").write_bytes(b"\xff\xfe not toml")
    desktop = _owned_package(home, fake)
    result = _invoke(["hub"])
    assert result.exit_code == 0 and result.output == ""
    assert legacy.calls == [_hub_legacy()] and desktop.runs() == []


@posix_only
@pytest.mark.parametrize("flat", [
    'model_size = "base"\n',
    # Its migration fails (non-string key), so the active file stays absent.
    'model_size = "base"\nfireworks_api_key = 123\n',
])
def test_default_legacy_flat_only_stays_on_the_legacy_window(
        flat, home, fake, legacy, monkeypatch, tmp_path):
    # Native checks only the active file, and a legacy migration can be
    # deferred or fail, so a legacy-flat-only config is a first run here even
    # though main_app.is_configured accepts it. The launcher never migrates.
    from localflow import main_app

    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    for name in ("FIREWORKS_API_KEY", "LOCALFLOW_FIREWORKS_API_KEY"):
        monkeypatch.delenv(name, raising=False)
    path = _use_config(monkeypatch, tmp_path)
    (home / ".localflow.toml").write_text(flat)
    assert main_app.is_configured() and not desktop_launch.configured()
    desktop = _owned_package(home, fake)
    before = sorted(p.relative_to(home) for p in home.rglob("*"))
    for args, expected in ((["hub"], _hub_legacy()), (["phrases"], ("phrases", None))):
        legacy.calls.clear()
        result = _invoke(args)
        assert result.exit_code == 0 and result.output == ""
        assert legacy.calls == [expected]
    assert desktop.runs() == []
    assert not path.exists()  # nothing migrated or written
    assert (home / ".localflow.toml").read_text() == flat
    assert sorted(p.relative_to(home) for p in home.rglob("*")) == before
    for name in ("FIREWORKS_API_KEY", "LOCALFLOW_FIREWORKS_API_KEY"):
        assert name not in os.environ


@posix_only
@pytest.mark.parametrize("config", [
    "config_version = 2\n\n[setup]\nversion = 2\n",
    'config_version = 2\n\n[stt]\nprovider = "faster-whisper"\n',
    'config_version = 2\n\n[stt]\nmodel = "base"\n',
])
def test_default_uses_the_same_configured_marker_as_the_app(
        config, home, fake, legacy, monkeypatch, tmp_path):
    from localflow import main_app

    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    _use_config(monkeypatch, tmp_path, config)
    assert main_app.is_configured() and desktop_launch.configured()
    desktop = _owned_package(home, fake)
    result = _invoke(["phrases"])
    assert result.exit_code == 0 and result.output == ""
    assert legacy.calls == [] and desktop.argvs() == [["--help"], ["--page", "Hot Phrases"]]


def test_configured_is_main_app_is_configured_quietly(monkeypatch, tmp_path, caplog):
    import logging

    from localflow import main_app

    for text in [*FIRST_RUN_CONFIGS.values(), SET_UP]:
        _use_config(monkeypatch, tmp_path, text)
        assert desktop_launch.configured() is main_app.is_configured(), text
    logger = logging.getLogger("localflow.config")
    assert logger.disabled is False  # restored afterwards
    _use_config(monkeypatch, tmp_path, '[llm]\nmodel = "llama3"\n')  # warns when loaded
    caplog.clear()
    with caplog.at_level(logging.WARNING, logger="localflow.config"):
        assert not desktop_launch.configured()
    assert caplog.records == []

    def no_home(cls):
        raise RuntimeError("Could not determine home directory.")

    _use_config(monkeypatch, tmp_path)
    monkeypatch.setattr(main_app.Path, "home", classmethod(no_home))
    assert desktop_launch.configured() is False
    assert logger.disabled is False


def _bare_root(home):
    (home / ".local/share/localflow-preview").mkdir(parents=True)
    (home / "Applications/LocalFlow Preview.app").mkdir(parents=True)


def _root_is_a_file(home):
    for root in (home / ".local/share/localflow-preview", home / "Applications/LocalFlow Preview.app"):
        root.parent.mkdir(parents=True, exist_ok=True)
        root.write_text("not a package")


def _symlinked_root(home):
    root, _ = _linux_install(home / "real")
    app, _ = _mac_install(home / "real")
    (home / ".local/share").mkdir(parents=True)
    (home / ".local/share/localflow-preview").symlink_to(root)
    (home / "Applications").mkdir()
    (home / "Applications/LocalFlow Preview.app").symlink_to(app)


def _owned_without_executable(home):
    _linux_install(home, exe=False)
    _mac_install(home)[1].unlink()


@pytest.mark.parametrize(("arrange", "notice"), [
    (_bare_root, desktop_launch.NOTICE_NOT_OWNED),
    (_root_is_a_file, desktop_launch.NOTICE_NOT_OWNED),
    pytest.param(_symlinked_root, desktop_launch.NOTICE_NOT_OWNED, marks=posix_only),
    (_owned_without_executable, desktop_launch.NOTICE_NOT_INSTALLED),
])
@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_default_with_a_malformed_install_notices_once_then_legacy_once(
        command, arrange, notice, home, legacy, set_up):
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    arrange(home)
    result = _invoke([command])
    assert result.exit_code == 0
    assert legacy.calls == [LEGACY_FOR[command]]
    # Setup never goes to the preview by default, so it has nothing to report.
    assert result.output == ("" if command == "setup" else notice + "\n")


@pytest.mark.parametrize("override", ["lf-desktop", "relative/lf-desktop", "/nonexistent/lf-desktop"])
@pytest.mark.parametrize("command", ["hub", "phrases"])
def test_default_with_a_bad_override_notices_once(override, command, legacy, set_up):
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    result = _invoke([command], env={"LOCALFLOW_DESKTOP_BIN": override})
    assert result.exit_code == 0
    assert legacy.calls == [LEGACY_FOR[command]]
    assert result.output == desktop_launch.NOTICE_BAD_OVERRIDE + "\n"


@posix_only
@pytest.mark.parametrize(("kwargs", "notice", "argvs"), [
    ({"help_text": OLD_HELP}, desktop_launch.NOTICE_PROBE, [["--help"]]),
    ({"help_text": SCOPE3_HELP}, desktop_launch.NOTICE_PROBE, [["--help"]]),
    ({"help_hang": True}, desktop_launch.NOTICE_PROBE, [["--help"]]),
    ({"vanish": True}, desktop_launch.NOTICE_SPAWN, [["--help"]]),
    ({"launch": "exit1"}, desktop_launch.NOTICE_EARLY_EXIT.format(code=1),
     [["--help"], ["--page", "Hot Phrases"]]),
    ({"launch": "exit2"}, desktop_launch.NOTICE_EARLY_EXIT.format(code=2),
     [["--help"], ["--page", "Hot Phrases"]]),
])
@pytest.mark.parametrize("where", ["package", "override"])
def test_default_candidate_failures_notice_once_then_legacy_once(
        kwargs, notice, argvs, where, home, fake, legacy, monkeypatch, set_up):
    # On a set-up machine (setup itself never goes to the preview by default).
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    monkeypatch.setattr(desktop_launch, "PROBE_TIMEOUT", 1.0)
    if where == "package":
        desktop, env = _owned_package(home, fake, **kwargs), {}
    else:
        desktop = fake(**kwargs)
        env = _gpui_env(desktop)
    result = _invoke(["phrases"], env=env)
    assert result.exit_code == 0
    assert legacy.calls == [("phrases", None)]
    assert result.output.strip() == notice
    assert "SECRET" not in result.output
    assert desktop.argvs() == argvs


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
@pytest.mark.parametrize("rollback", [["--legacy"], "LOCALFLOW_UI=legacy", "LOCALFLOW_UI= Legacy "])
def test_explicit_legacy_never_probes_even_with_a_candidate(
        command, rollback, legacy, home, tmp_path, monkeypatch):
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    _linux_install(home)  # an owned package, and a bad override, both ignored
    _mac_install(home)
    args, env = [command], {"LOCALFLOW_SANDBOX": "1"}
    if isinstance(rollback, list):
        args += rollback
        env["LOCALFLOW_UI"] = "gpui"
    else:
        env["LOCALFLOW_UI"] = rollback.split("=", 1)[1]
    for override in (str(desktop.path), "relative"):
        legacy.calls.clear()
        with monkeypatch.context() as patched:
            _never_discovered(patched)
            result = _invoke(args, env={**env, "LOCALFLOW_DESKTOP_BIN": override})
        assert result.exit_code == 0, result.output
        assert result.output == ""
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
    monkeypatch.setattr(desktop_launch.sys, "platform", "win32")
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
    assert result.output.strip() == desktop_launch.NOTICE_NOT_INSTALLED


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
    assert result.output == desktop_launch.NOTICE_NOT_INSTALLED + "\n"
    assert legacy.calls == [expected]


# ------------------------------------------------------- sandbox boundary ---


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
@pytest.mark.parametrize("opt_in", [["--gpui"], "env", "default"])
def test_sandbox_request_is_refused_not_downgraded(command, opt_in, legacy, home, tmp_path,
                                                   monkeypatch):
    if opt_in == "default" and desktop_launch.platform_key() is None:
        pytest.skip("the default never looks for a preview package here")
    if opt_in == "default":
        _use_config(monkeypatch, tmp_path, SET_UP)  # outside HOME
    desktop = FakeDesktop(tmp_path / "bin/lf-desktop", tmp_path / "record.jsonl")
    for value in ("1", "true"):
        # Under the default, the override alone makes this a candidate.
        env = _gpui_env(desktop, LOCALFLOW_SANDBOX=value)
        if opt_in == "env":
            env["LOCALFLOW_UI"] = "gpui"
        result = _invoke([command, *(opt_in if isinstance(opt_in, list) else [])], env=env)
        if opt_in == "default" and command == "setup":
            # Setup isn't a GPUI route by default: the legacy wizard, as before.
            assert result.exit_code == 0 and result.output == ""
            continue
        assert result.exit_code == 1
        assert "lf-desktop --sandbox" in result.output and "LOCALFLOW_SANDBOX" in result.output
    if opt_in == "default" and command == "setup":
        assert legacy.calls == [("setup", None)] * 2
        legacy.calls.clear()
    assert legacy.calls == []  # no un-sandboxed fallback
    assert desktop.runs() == []  # nothing probed or launched
    assert list(home.iterdir()) == []  # no launcher logs or data


@posix_only
@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_sandbox_request_with_an_installed_package_is_refused_under_the_default(
        command, home, fake, legacy, set_up):
    if desktop_launch.platform_key() is None:
        pytest.skip("no preview package for this platform")
    desktop = _owned_package(home, fake)
    result = _invoke([command], env={"LOCALFLOW_SANDBOX": "1"})
    if command == "setup":  # not a GPUI route by default: the legacy wizard
        assert result.exit_code == 0 and result.output == ""
        assert legacy.calls == [("setup", None)] and desktop.runs() == []
        return
    assert result.exit_code == 1
    assert "lf-desktop --sandbox" in result.output
    assert legacy.calls == [] and desktop.runs() == []


def test_sandbox_env_keeps_its_legacy_semantics(legacy):
    result = _invoke(["hub"], env={"LOCALFLOW_SANDBOX": "1"})
    assert result.exit_code == 0 and legacy.calls == [_hub_legacy()]
    result = _invoke(["setup", "--legacy"], env={"LOCALFLOW_SANDBOX": "1"})
    assert result.exit_code == 0 and legacy.calls[-1] == ("setup", None)


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
    # The GPUI default with a candidate doesn't touch the terminal path either.
    assert _invoke(["setup", "--cli"], env=_gpui_env(desktop)).exit_code == 0
    assert ran == [(None, None, False, None)] * 2
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


def test_only_the_cli_uses_the_launcher_and_dispatchers_stay_legacy():
    users = sorted(
        path.name for path in (ROOT / "localflow").rglob("*.py")
        if "desktop_launch" in path.read_text(encoding="utf-8") and path.name != "desktop_launch.py"
    )
    assert users == ["cli.py"]


@pytest.mark.parametrize("command", ["hub", "phrases", "setup"])
def test_help_documents_the_conditional_default(command):
    result = _invoke([command, "--help"])
    assert "--gpui" in result.output and "--legacy" in result.output
    text = " ".join(result.output.split())
    assert "only lf hub and lf phrases use it" in text
    assert "once LocalFlow is set up and the preview is installed" in text
    assert "setup and first run stay legacy" in text
    assert "No screen-reader support yet" in text
    assert "(the default)" not in text
