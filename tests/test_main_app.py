import ast
import plistlib
import stat
import subprocess
import sys
import types
from pathlib import Path

import pytest

from localflow import appbundle, launchagent, main_app
from localflow.config import Config, save_config

ROOT = Path(__file__).parent.parent


class Result:
    def __init__(self, returncode=0, stderr=""):
        self.returncode = returncode
        self.stderr = stderr


class Recorder:
    """Records Popen/run calls; `run_result` may be a Result or an exception."""

    def __init__(self, monkeypatch):
        self.popen = []
        self.run = []
        self.run_result = Result()
        self.popen_error = None
        monkeypatch.setattr(main_app.subprocess, "Popen", self._popen)
        monkeypatch.setattr(main_app.subprocess, "run", self._run)
        monkeypatch.setitem(sys.modules, "rumps", types.ModuleType("rumps"))

    def _popen(self, argv, **kwargs):
        if self.popen_error is not None:
            raise self.popen_error
        self.popen.append(list(argv))

    def _run(self, argv, **kwargs):
        self.run.append(list(argv))
        if isinstance(self.run_result, Exception):
            raise self.run_result
        return self.run_result


@pytest.fixture(autouse=True)
def isolated(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))
    monkeypatch.setattr(sys, "argv", ["main_app", "--bogus"])
    monkeypatch.setattr(sys, "platform", "linux")
    monkeypatch.setattr(main_app, "_held", lambda name: False)


@pytest.fixture
def rec(monkeypatch):
    return Recorder(monkeypatch)


def _fake_windows(monkeypatch):
    calls = []
    for module, label in (("localflow.setup_window", "setup"), ("localflow.hub_window", "hub")):
        fake = types.ModuleType(module)
        fake.main = lambda label=label: calls.append(label)
        monkeypatch.setitem(sys.modules, module, fake)
    return calls


def _log_text(tmp_path):
    return (tmp_path / ".localflow" / "app.log").read_text()


# --- routing (portable path) --------------------------------------------


def test_unconfigured_opens_setup_only(monkeypatch, rec):
    calls = _fake_windows(monkeypatch)

    main_app.main()

    assert calls == ["setup"]
    assert rec.popen == [] and rec.run == []


def _write_config(tmp_path, text, legacy=False):
    path = tmp_path / (".localflow.toml" if legacy else "config.toml")
    path.write_text(text, encoding="utf-8")
    return path


LEGACY_DAILY_USER = """\
config_version = 2

[stt]
provider = "parakeet"
model = "base"

[llm]
provider = "ollama"
model = "llama3.2"

[setup]
version = 0
"""


@pytest.mark.parametrize(
    "text",
    [
        "",
        "config_version = 2\n",
        "config_version = 2\n[setup]\nversion = 0\n",
        'config_version = 2\n[stt]\nlanguage = "en"\n[setup]\nversion = 0\n',
        'config_version = 2\n[llm]\nmodel = "x"\n',
        'config_version = 2\n[stt]\nprovider = ""\nmodel = ""\n',
        "config_version = 2\n[stt]\nprovider = 3\n",
        "this is = = not toml [",
        "config_version = 99\n[stt]\nprovider = \"parakeet\"\n",
    ],
)
def test_config_without_chosen_models_is_unconfigured(monkeypatch, rec, tmp_path, text):
    _write_config(tmp_path, text)
    calls = _fake_windows(monkeypatch)

    assert main_app.is_configured() is False
    main_app.main()

    assert calls == ["setup"]
    assert rec.popen == []


@pytest.mark.parametrize("version", ['"bad"', "[2]", '{ a = 1 }'])
def test_wrongly_typed_config_version_is_unconfigured(monkeypatch, rec, tmp_path, version):
    _write_config(
        tmp_path,
        f'config_version = {version}\n[stt]\nprovider = "parakeet"\n[setup]\nversion = 1\n',
    )
    calls = _fake_windows(monkeypatch)

    assert main_app.is_configured() is False
    main_app.main()

    assert calls == ["setup"]
    assert rec.popen == []


def test_missing_config_is_unconfigured():
    assert main_app.is_configured() is False


def test_unreadable_config_is_unconfigured(monkeypatch, tmp_path):
    _write_config(tmp_path, LEGACY_DAILY_USER)

    def denied(path):
        raise PermissionError(path)

    monkeypatch.setattr(main_app, "_parse", denied)

    assert main_app.is_configured() is False


def test_non_utf8_config_is_unconfigured(tmp_path):
    (tmp_path / "config.toml").write_bytes(b"\xff\xfe\x00bad")

    assert main_app.is_configured() is False


def test_saved_default_config_counts_because_stt_keys_are_written(tmp_path):
    # save_config materializes [stt] provider/model, so the saved file is an
    # explicit choice even at setup_version 0; dataclass defaults alone are not.
    save_config(Config(setup_version=0))

    assert main_app.is_configured() is True


@pytest.mark.parametrize(
    "text",
    [
        'config_version = 2\n[stt]\nprovider = "parakeet"\n',
        'config_version = 2\n[stt]\nmodel = "small"\n',
        "config_version = 2\n[setup]\nversion = 1\n",
        LEGACY_DAILY_USER,
    ],
)
def test_explicit_provider_model_or_wizard_marker_is_configured(tmp_path, text):
    _write_config(tmp_path, text)

    assert main_app.is_configured() is True


def test_legacy_daily_user_routes_to_runtime_and_hub(monkeypatch, rec, tmp_path):
    _write_config(tmp_path, LEGACY_DAILY_USER)
    calls = _fake_windows(monkeypatch)
    monkeypatch.setattr(launchagent, "status", lambda: "not installed")

    main_app.main()

    assert calls == ["hub"]
    assert [a for a in rec.popen if a[0].endswith("/localflow")]
    assert [sys.executable, "-m", "localflow.menubar"] in rec.popen


def test_legacy_flat_file_is_used_when_active_config_is_missing(tmp_path):
    _write_config(tmp_path, 'stt_backend = "mlx"\nmodel_size = "small"\n', legacy=True)
    assert main_app.is_configured() is True
    _write_config(tmp_path, "ollama_model = \"x\"\n", legacy=True)
    assert main_app.is_configured() is False
    _write_config(tmp_path, "setup_version = 1\n", legacy=True)
    assert main_app.is_configured() is True


def test_active_config_takes_precedence_over_legacy_file(tmp_path):
    _write_config(tmp_path, 'stt_backend = "mlx"\n', legacy=True)
    _write_config(tmp_path, "config_version = 2\n")

    assert main_app.is_configured() is False


def test_malformed_active_config_does_not_fall_back_to_legacy(tmp_path):
    _write_config(tmp_path, 'stt_backend = "mlx"\n', legacy=True)
    _write_config(tmp_path, "not = = toml")

    assert main_app.is_configured() is False


def test_configured_starts_runtime_then_hub(monkeypatch, rec):
    save_config(Config(setup_version=1))
    calls = _fake_windows(monkeypatch)
    monkeypatch.setattr(launchagent, "status", lambda: "not installed")

    main_app.main()

    assert calls == ["hub"]
    assert sys.argv == ["main_app"]
    assert len([a for a in rec.popen if a[0].endswith("/localflow")]) == 1
    assert [sys.executable, "-m", "localflow.menubar"] in rec.popen


def test_running_runtime_is_not_started_twice(monkeypatch, rec):
    save_config(Config(setup_version=1))
    calls = _fake_windows(monkeypatch)
    monkeypatch.setattr(main_app, "_held", lambda name: True)

    main_app.main()

    assert calls == ["hub"]
    assert rec.popen == [] and rec.run == []


# --- login agent handling -----------------------------------------------


def test_loaded_agent_is_kickstarted(monkeypatch, rec):
    monkeypatch.setattr(launchagent, "status", lambda: "loaded, not running (x)")

    main_app._start_dictation()

    assert rec.run[0][:2] == ["launchctl", "kickstart"]
    assert not [a for a in rec.popen if a[0].endswith("/localflow")]


def test_not_loaded_agent_is_bootstrapped_again(monkeypatch, rec):
    installed = []
    monkeypatch.setattr(launchagent, "status", lambda: "not loaded (x)")
    monkeypatch.setattr(launchagent, "install", lambda: installed.append(1))

    main_app._start_dictation()

    assert installed == [1]
    assert rec.run == [] and rec.popen == []


def test_bootstrap_failure_falls_back_to_detached_runtime(monkeypatch, rec, tmp_path):
    def broken():
        raise RuntimeError("bootstrap denied")

    monkeypatch.setattr(launchagent, "status", lambda: "not loaded (x)")
    monkeypatch.setattr(launchagent, "install", broken)

    main_app._start_dictation()

    assert [a for a in rec.popen if a[0].endswith("/localflow")]
    assert "bootstrap denied" in _log_text(tmp_path)


def test_kickstart_failure_falls_back_to_detached_runtime(monkeypatch, rec, tmp_path):
    rec.run_result = Result(113, "Could not find service")
    monkeypatch.setattr(launchagent, "status", lambda: "loaded, not running (x)")

    main_app._start_dictation()

    assert [a for a in rec.popen if a[0].endswith("/localflow")]
    assert "Could not find service" in _log_text(tmp_path)


def test_kickstart_oserror_falls_back_to_detached_runtime(monkeypatch, rec):
    rec.run_result = FileNotFoundError("launchctl")
    monkeypatch.setattr(launchagent, "status", lambda: "running (x)")

    main_app._start_dictation()

    assert [a for a in rec.popen if a[0].endswith("/localflow")]


def test_runtime_start_failure_is_logged_and_hub_still_opens(monkeypatch, rec, tmp_path):
    save_config(Config(setup_version=1))
    calls = _fake_windows(monkeypatch)
    monkeypatch.setattr(launchagent, "status", lambda: "not installed")
    rec.popen_error = OSError("no exec")

    main_app.main()

    assert calls == ["hub"]
    log = _log_text(tmp_path)
    assert "could not start dictation" in log and "could not start the menu bar" in log


def test_detach_closes_log_when_popen_raises(monkeypatch, rec):
    class Log:
        closed = False

        def __enter__(self):
            return self

        def __exit__(self, *exc):
            self.closed = True

        def write(self, data):
            pass

    log = Log()
    monkeypatch.setattr(main_app, "_log_file", lambda: log)
    rec.popen_error = OSError("boom")

    with pytest.raises(OSError):
        main_app._detach(["x"])

    assert log.closed


# --- reopen -------------------------------------------------------------


def _hold_lock(monkeypatch, pid=4242):
    monkeypatch.setattr(main_app, "acquire", lambda name: None)
    monkeypatch.setattr(main_app, "read_holder", lambda name: (pid, "t"))


def test_second_launch_asks_launch_services_to_reopen(monkeypatch, rec):
    monkeypatch.setattr(sys, "platform", "darwin")
    windows = _fake_windows(monkeypatch)
    _hold_lock(monkeypatch)

    main_app.main()

    assert windows == []
    assert rec.run == [["open", "-b", "com.cortexrnd.localflow.app", "--args", "--reopened"]]


def test_reopen_failure_falls_back_to_activating_the_holder(monkeypatch, rec):
    monkeypatch.setattr(sys, "platform", "darwin")
    _hold_lock(monkeypatch)
    rec.run_result = Result(1)

    main_app.main()

    assert rec.run[0][0] == "open"
    assert rec.run[1][:2] == ["osascript", "-e"] and "4242" in rec.run[1][2]


def test_reopened_process_never_calls_open_again(monkeypatch, rec):
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setattr(sys, "argv", ["main_app", "--reopened"])
    _hold_lock(monkeypatch)

    main_app.main()

    assert [cmd[0] for cmd in rec.run] == ["osascript"]


# --- native (AppKit) path, with fakes -----------------------------------


class FakeWindow:
    def __init__(self, visible=True, mini=False):
        self.visible, self.mini, self.events = visible, mini, []

    def isVisible(self):
        return self.visible

    def isMiniaturized(self):
        return self.mini

    def makeKeyAndOrderFront_(self, sender):
        self.events.append("front")

    def deminiaturize_(self, sender):
        self.events.append("deminiaturize")


class FakeController:
    def __init__(self, label):
        self.label, self.window = label, FakeWindow()


def _native(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    built, shown = [], []
    mac = types.ModuleType("localflow.main_app_mac")
    mac.run_app = lambda show: shown.append(show)
    hub = types.ModuleType("localflow.hub_mac")
    hub.build_window = lambda initial_page="Home": built.append("hub") or FakeController("hub")
    setup = types.ModuleType("localflow.setup_mac")
    setup.build_window = lambda: built.append("setup") or FakeController("setup")
    for name, module in (
        ("localflow.main_app_mac", mac),
        ("localflow.hub_mac", hub),
        ("localflow.setup_mac", setup),
    ):
        monkeypatch.setitem(sys.modules, name, module)
    return built, shown


def test_native_unconfigured_shows_setup(monkeypatch, rec):
    built, shown = _native(monkeypatch)

    main_app.main()
    shown[0]()

    assert built == ["setup"]
    assert rec.popen == []


def test_native_configured_ensures_runtime_and_builds_hub_once(monkeypatch, rec):
    save_config(Config(setup_version=1))
    built, shown = _native(monkeypatch)
    runtime = []
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: runtime.append(1))

    main_app.main()
    shown[0]()
    shown[0]()  # reopen while the window is still visible: reuse it

    assert built == ["hub"] and runtime == [1, 1]


def test_native_reopen_after_close_builds_a_fresh_hub(monkeypatch, rec):
    save_config(Config(setup_version=1))
    built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)

    main_app.main()
    shown[0]()
    router_show = shown[0]
    router_show.__self__.hub.window.visible = False
    router_show()

    assert built == ["hub", "hub"]


def test_native_reopen_restores_miniaturized_window(monkeypatch, rec):
    save_config(Config(setup_version=1))
    built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)

    main_app.main()
    shown[0]()
    router = shown[0].__self__
    router.hub.window.mini = True
    shown[0]()

    assert built == ["hub"] and router.hub.window.events == ["front", "deminiaturize"]


def test_native_reopen_switches_from_setup_to_hub_once_configured(monkeypatch, rec):
    built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)

    main_app.main()
    shown[0]()
    save_config(Config(setup_version=1))
    shown[0]()

    assert built == ["setup", "hub"]


def test_missing_pyobjc_falls_back_to_portable_windows(monkeypatch, rec):
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setitem(sys.modules, "localflow.main_app_mac", None)  # import fails
    calls = _fake_windows(monkeypatch)

    main_app.main()

    assert calls == ["setup"]


# --- static checks for the AppKit delegate module -----------------------


def test_main_app_module_stays_importable_without_appkit():
    tree = ast.parse((ROOT / "localflow" / "main_app.py").read_text(encoding="utf-8"))
    top = {
        (node.module or "").split(".")[0] if isinstance(node, ast.ImportFrom) else alias.name
        for node in tree.body
        if isinstance(node, (ast.Import, ast.ImportFrom))
        for alias in (node.names if isinstance(node, ast.Import) else [None])
    }
    assert not top & {"AppKit", "objc", "Foundation", "PyObjCTools"}


def test_mac_delegate_has_valid_selectors_and_handles_reopen():
    tree = ast.parse((ROOT / "localflow" / "main_app_mac.py").read_text(encoding="utf-8"))
    cls = next(n for n in tree.body if isinstance(n, ast.ClassDef))
    methods = [n for n in cls.body if isinstance(n, ast.FunctionDef)]
    assert [m.name for m in methods] == [
        "initWithShow_",
        "applicationShouldHandleReopen_hasVisibleWindows_",
    ]
    for method in methods:
        assert method.name.count("_") == len(method.args.args) - 1
    source = (ROOT / "localflow" / "main_app_mac.py").read_text(encoding="utf-8")
    assert "NSApplicationActivationPolicyRegular" in source
    assert "setDelegate_" in source


# --- bundle -------------------------------------------------------------


def test_main_app_bundle_plist_and_launcher(tmp_path):
    bundle = appbundle.install_app(
        dest_dir=tmp_path / "Apps",
        python="/opt/lf venv/bin/python",
        name=appbundle.MAIN_APP_NAME,
        bundle_id=appbundle.MAIN_BUNDLE_ID,
        module=appbundle.MAIN_MODULE,
        icon=True,
        full_plist=True,
    )

    plist = plistlib.loads((bundle / "Contents" / "Info.plist").read_bytes())
    launcher = bundle / "Contents" / "MacOS" / "launcher"
    assert bundle.name == "LocalFlow.app"
    assert plist["CFBundleName"] == "LocalFlow"
    assert plist["LSUIElement"] is False
    assert plist["CFBundleVersion"]
    assert plist["CFBundleIdentifier"] == "com.cortexrnd.localflow.app"
    assert launcher.read_text() == (
        "#!/bin/bash\nexec '/opt/lf venv/bin/python' -m localflow.main_app \"$@\"\n"
    )
    assert launcher.stat().st_mode & stat.S_IXUSR
    assert ("CFBundleIconFile" in plist) == (
        (bundle / "Contents" / "Resources" / "LocalFlow.icns").exists()
    )


def test_png_renderer_makes_valid_png():
    data = appbundle._png(32)
    assert data.startswith(b"\x89PNG\r\n\x1a\n")
    assert data.endswith(b"IEND\xaeB`\x82")


def test_icon_generation_is_skipped_off_macos(tmp_path, monkeypatch):
    monkeypatch.setattr(sys, "platform", "linux")
    assert appbundle._make_icns(tmp_path / "x.icns") is False


def test_icon_generation_uses_iconutil_on_macos(tmp_path, monkeypatch):
    commands = []

    def fake_run(argv, **kwargs):
        commands.append(argv)
        Path(argv[-1]).write_bytes(b"icns")
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.setattr(appbundle.shutil, "which", lambda name: "/usr/bin/iconutil")
    monkeypatch.setattr(appbundle.subprocess, "run", fake_run)

    assert appbundle._make_icns(tmp_path / "x.icns") is True
    assert commands[0][:3] == ["iconutil", "-c", "icns"]


def test_setup_install_app_installs_both_bundles(tmp_path, monkeypatch):
    from click.testing import CliRunner

    from localflow import cli

    installed = []

    def fake_install(**kw):
        installed.append(kw)
        return tmp_path / kw["name"]

    monkeypatch.setattr(appbundle, "install_app", fake_install)
    result = CliRunner().invoke(cli.cli, ["setup", "install-app"])

    assert result.exit_code == 0, result.output
    assert [kw["name"] for kw in installed] == ["LocalFlow Setup.app", "LocalFlow.app"]
    assert installed[1]["module"] == "localflow.main_app"
    assert installed[1]["icon"] is True and installed[1]["full_plist"] is True
