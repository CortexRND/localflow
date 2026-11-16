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
SERVER_ARGV = [sys.executable, "-m", "localflow.server"]
_real_server_healthy = main_app.server_healthy
_real_watch_server = main_app._watch_server


class Result:
    def __init__(self, returncode=0, stderr=""):
        self.returncode = returncode
        self.stderr = stderr


class FakeChild:
    """Stands in for a spawned Popen; `returncode` None means still running."""

    _next_pid = 5000

    def __init__(self, argv, kwargs):
        FakeChild._next_pid += 1
        self.pid = FakeChild._next_pid
        self.argv = argv
        self.kwargs = kwargs
        self.returncode = None
        self.polls = 0

    def poll(self):
        self.polls += 1
        return self.returncode

    def wait(self):  # only called by the reaper, which tests replace
        return self.returncode


class Recorder:
    """Records Popen/run calls; `run_result` may be a Result or an exception."""

    def __init__(self, monkeypatch):
        self.popen = []
        self.children = []
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
        child = FakeChild(list(argv), kwargs)
        self.children.append(child)
        return child

    def server_spawns(self):
        return [a for a in self.popen if a == SERVER_ARGV]

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
    # No test touches the network or leaves a reaper thread behind: the
    # health probe answers from `probes` and the reaper only records.
    probes.clear()
    reaped.clear()
    healthy.clear()
    monkeypatch.setattr(main_app, "_server_child", None)
    monkeypatch.setattr(
        main_app,
        "server_healthy",
        lambda url, timeout=main_app.SERVER_PROBE_TIMEOUT: (
            probes.append((url, timeout)) or bool(healthy)
        ),
    )
    monkeypatch.setattr(main_app, "_watch_server", reaped.append)


probes = []  # (url, timeout) of every health probe
reaped = []  # children handed to the background reaper
healthy = []  # non-empty = the faked /healthz answers {"ok": true}


@pytest.fixture
def rec(monkeypatch):
    return Recorder(monkeypatch)


pages = []  # pages handed to the faked platform hub backend / hub builder
polls = []  # poll callbacks handed to the faked run_app


@pytest.fixture(autouse=True)
def _reset_pages():
    pages.clear()
    polls.clear()


def _fake_windows(monkeypatch):
    calls = []
    setup = types.ModuleType("localflow.setup_window")
    setup.main = lambda: calls.append("setup")
    monkeypatch.setitem(sys.modules, "localflow.setup_window", setup)
    hub = types.ModuleType("localflow.hub_window")
    # main_app's portable path must use the backend, never the dispatcher
    # (which routes back into main_app on macOS and would recurse).
    hub.run_backend = lambda page="Home": (
        calls.append("hub"), pages.append(page)
    )[0]
    hub.main = lambda *a, **k: calls.append("dispatcher")
    monkeypatch.setitem(sys.modules, "localflow.hub_window", hub)
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
    assert rec.server_spawns() == [SERVER_ARGV]


def test_running_runtime_is_not_started_twice(monkeypatch, rec):
    save_config(Config(setup_version=1))
    calls = _fake_windows(monkeypatch)
    monkeypatch.setattr(main_app, "_held", lambda name: True)
    healthy.append(True)  # the local server is part of the running runtime

    main_app.main()

    assert calls == ["hub"]
    assert rec.popen == [] and rec.run == []
    assert probes == [("http://127.0.0.1:8756", main_app.SERVER_PROBE_TIMEOUT)]


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
    assert "could not start the local server: no exec" in log


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


# --- local server -------------------------------------------------------


def test_healthy_server_is_not_spawned_again(rec):
    healthy.append(True)

    main_app.ensure_server()
    main_app.ensure_server()

    assert rec.popen == []
    assert [url for url, _ in probes] == ["http://127.0.0.1:8756"] * 2
    assert all(0 < timeout <= 1 for _, timeout in probes)


def test_unavailable_server_is_spawned_detached_once_while_starting(rec, tmp_path):
    main_app.ensure_server()
    # Reopen / page requests while the child is still starting (not healthy).
    main_app.ensure_server()
    main_app.ensure_server()

    assert rec.server_spawns() == [SERVER_ARGV]
    child = rec.children[0]
    assert child.kwargs["stdin"] is subprocess.DEVNULL
    assert child.kwargs["start_new_session"] is True
    assert child.kwargs["close_fds"] is True
    log_file = child.kwargs["stdout"]
    assert child.kwargs["stderr"] is log_file
    assert Path(log_file.name) == tmp_path / ".localflow" / "app.log"
    assert log_file.closed  # the parent keeps no handle open
    assert child.polls == 2  # later ensures checked the live child, no new probe
    assert len(probes) == 1
    assert reaped == [child]
    assert f"started local server (pid {child.pid})" in _log_text(tmp_path)


def test_server_child_that_exited_is_retried(rec):
    main_app.ensure_server()
    first = rec.children[0]
    first.returncode = 1  # e.g. crashed during startup

    main_app.ensure_server()

    assert rec.server_spawns() == [SERVER_ARGV, SERVER_ARGV]
    assert main_app._server_child is rec.children[1]
    assert len(probes) == 2


def test_server_child_that_became_healthy_is_left_alone(rec):
    main_app.ensure_server()
    healthy.append(True)
    rec.children[0].returncode = 0  # e.g. lost a bind race to `lf serve`

    main_app.ensure_server()

    assert rec.server_spawns() == [SERVER_ARGV]
    assert main_app._server_child is None


def test_server_reaper_waits_and_logs_a_failed_start(rec, tmp_path):
    import threading

    child = FakeChild(SERVER_ARGV, {})
    child.returncode = 3

    _real_watch_server(child)
    for thread in threading.enumerate():
        if thread.name == "localflow-server-reaper":
            thread.join(timeout=5)

    assert f"local server (pid {child.pid}) exited with status 3" in _log_text(tmp_path)


def test_server_spawn_failure_is_logged_and_retried_next_time(rec, tmp_path):
    rec.popen_error = OSError("no exec")

    main_app.ensure_server()

    assert main_app._server_child is None
    assert "could not start the local server: no exec" in _log_text(tmp_path)
    rec.popen_error = None
    main_app.ensure_server()
    assert rec.server_spawns() == [SERVER_ARGV]


def test_server_config_error_is_logged_without_spawning(monkeypatch, rec, tmp_path):
    from localflow import server_url
    from localflow.config import ConfigError

    def broken(config=None):
        raise ConfigError("config is broken")

    monkeypatch.setattr(server_url, "configured_base_url", broken)

    main_app.ensure_server()

    assert rec.popen == []
    assert "could not start the local server: config is broken" in _log_text(tmp_path)


def test_server_probe_error_is_logged_without_breaking_the_hub(monkeypatch, rec, tmp_path):
    save_config(Config(setup_version=1))
    calls = _fake_windows(monkeypatch)
    monkeypatch.setattr(launchagent, "status", lambda: "not installed")

    def exploding(url, timeout=None):
        raise RuntimeError("probe exploded")

    monkeypatch.setattr(main_app, "server_healthy", exploding)

    main_app.main()

    assert calls == ["hub"]
    assert rec.server_spawns() == []
    assert [sys.executable, "-m", "localflow.menubar"] in rec.popen
    assert "could not start the local server: probe exploded" in _log_text(tmp_path)


def test_unknowable_server_child_is_treated_as_gone(rec, tmp_path):
    main_app.ensure_server()

    def broken_poll():
        raise OSError("no such process")

    rec.children[0].poll = broken_poll
    main_app.ensure_server()

    assert rec.server_spawns() == [SERVER_ARGV, SERVER_ARGV]
    assert "could not check pid" in _log_text(tmp_path)


def test_server_starts_even_without_rumps(monkeypatch, rec):
    monkeypatch.setitem(sys.modules, "rumps", None)  # import rumps fails
    monkeypatch.setattr(launchagent, "status", lambda: "not installed")

    main_app.ensure_runtime()
    main_app.ensure_runtime()

    assert rec.server_spawns() == [SERVER_ARGV]
    assert [sys.executable, "-m", "localflow.menubar"] not in rec.popen


def test_router_reopens_share_one_server_child(monkeypatch, rec):
    save_config(Config(setup_version=1))
    monkeypatch.setattr(launchagent, "status", lambda: "not installed")
    opened = []
    router = main_app.WindowRouter(
        main_app.is_configured,
        main_app.ensure_runtime,
        lambda existing, page: opened.append(page) or "hub",
        lambda existing: "setup",
    )

    router.show()
    router.show("History")

    assert opened == [None, "History"]
    assert rec.server_spawns() == [SERVER_ARGV]


@pytest.mark.parametrize(
    ("host", "port", "url"),
    [
        ("127.0.0.1", 8756, "http://127.0.0.1:8756"),
        ("127.0.0.1", 9911, "http://127.0.0.1:9911"),
        ("0.0.0.0", 9911, "http://127.0.0.1:9911"),
        ("::", 9911, "http://[::1]:9911"),
        ("::1", 9911, "http://[::1]:9911"),
        ("localhost", 9911, "http://localhost:9911"),
    ],
)
def test_server_probe_uses_the_configured_host_and_port(rec, host, port, url):
    save_config(Config(setup_version=1, server_host=host, server_port=port))

    main_app.ensure_server()

    assert [u for u, _ in probes] == [url]


class _FakeResponse:
    def __init__(self, status, body):
        self.status = status
        self.body = body

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def read(self, limit=-1):
        return self.body


@pytest.mark.parametrize(
    ("answer", "expected"),
    [
        (_FakeResponse(200, b'{"ok": true}'), True),
        (_FakeResponse(200, b'{"ok": false}'), False),
        (_FakeResponse(200, b'{"ok": "true"}'), False),
        (_FakeResponse(200, b"[true]"), False),
        (_FakeResponse(200, b"<html>not localflow</html>"), False),
        (_FakeResponse(503, b'{"ok": true}'), False),
        (ConnectionRefusedError("refused"), False),
        (TimeoutError("timed out"), False),
    ],
)
def test_server_healthy_probe(monkeypatch, answer, expected):
    import urllib.error
    import urllib.request

    opened = []
    handlers = []

    class Opener:
        def open(self, url, timeout):
            opened.append((url, timeout))
            if isinstance(answer, Exception):
                raise urllib.error.URLError(answer)
            return answer

    def build_opener(*args):
        handlers.extend(args)
        return Opener()

    monkeypatch.setattr(urllib.request, "build_opener", build_opener)

    assert _real_server_healthy("http://[::1]:9911", timeout=0.25) is expected
    assert opened == [("http://[::1]:9911/healthz", 0.25)]
    # Loopback probes never go through an environment proxy.
    assert [h.proxies for h in handlers if isinstance(h, urllib.request.ProxyHandler)] == [{}]


def _imports(path, top_level_only=False):
    tree = ast.parse(path.read_text(encoding="utf-8"))
    nodes = tree.body if top_level_only else ast.walk(tree)
    names = set()
    for node in nodes:
        if isinstance(node, ast.Import):
            names.update(alias.name for alias in node.names)
        elif isinstance(node, ast.ImportFrom):
            names.add(node.module or "")
            names.update(f"{node.module}.{alias.name}" for alias in node.names)
    return names


def test_app_and_client_paths_never_import_the_server_module():
    for name in ("main_app.py", "meeting_client.py", "server_url.py"):
        assert "localflow.server" not in _imports(ROOT / "localflow" / name), name
    # `lf serve` imports it lazily inside the command; module import must not.
    assert "localflow.server" not in _imports(ROOT / "localflow" / "cli.py", True)
    assert "localflow.server" not in _imports(ROOT / "localflow" / "menubar.py", True)


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
    def __init__(self, label, page="Home"):
        self.label, self.window, self.page = label, FakeWindow(), page
        self.shown_pages = []

    def show_page(self, page):
        # Stands in for HubWindowController.show_page, the guarded entry point.
        self.shown_pages.append(page)
        self.page = page


def _native(monkeypatch):
    monkeypatch.setattr(sys, "platform", "darwin")
    built, shown = [], []
    mac = types.ModuleType("localflow.main_app_mac")
    mac.run_app = lambda show, poll=None: (
        shown.append(show), polls.append(poll)
    )[0]
    hub = types.ModuleType("localflow.hub_mac")
    hub.build_window = lambda initial_page="Home": (
        built.append("hub"), pages.append(initial_page)
    ) and FakeController("hub", initial_page)
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


# --- page routing (--page, request file, single Hub instance) -----------


@pytest.mark.parametrize(
    ("argv", "expected"),
    [
        ([], (False, None)),
        (["--reopened"], (True, None)),
        (["--page", "Hot Phrases"], (False, "Hot Phrases")),
        (["--page=Meetings", "--reopened"], (True, "Meetings")),
        (["-psn_0_12345", "--page", "History"], (False, "History")),
        (["--page", "Home", "--page", "Models"], (False, "Models")),
        (["--page", "Nope"], (False, None)),
        (["--page", "Home", "--page", "Nope"], (False, "Home")),
        (["--page"], (False, None)),
        (["--page", "hot phrases"], (False, None)),
        (["--page", "__import__('os')"], (False, None)),
    ],
)
def test_parse_args_validates_the_page_against_the_whitelist(argv, expected):
    assert main_app.parse_args(argv) == expected


def test_unknown_page_is_logged_not_fatal(tmp_path):
    main_app.parse_args(["--page", "Bogus"])

    assert "ignoring unknown page 'Bogus'" in _log_text(tmp_path)


def test_main_accepts_explicit_argv_and_still_strips_sys_argv(monkeypatch, rec):
    save_config(Config(setup_version=1))
    calls = _fake_windows(monkeypatch)
    monkeypatch.setattr(launchagent, "status", lambda: "not installed")

    main_app.main(["--page", "Hot Phrases"])

    assert calls == ["hub"] and pages == ["Hot Phrases"]
    assert sys.argv == ["main_app"]


def test_portable_path_uses_the_backend_never_the_dispatcher(monkeypatch, rec):
    save_config(Config(setup_version=1))
    calls = _fake_windows(monkeypatch)
    monkeypatch.setattr(launchagent, "status", lambda: "not installed")

    main_app.main()

    assert "dispatcher" not in calls and pages == ["Home"]


def test_running_app_gets_a_page_request_and_a_reopen(monkeypatch, rec):
    from localflow import hub_route

    monkeypatch.setattr(sys, "platform", "darwin")
    windows = _fake_windows(monkeypatch)
    _hold_lock(monkeypatch)

    main_app.main(["--page", "Hot Phrases"])

    assert windows == []
    assert rec.run == [["open", "-b", "com.cortexrnd.localflow.app", "--args", "--reopened"]]
    assert hub_route.consume_request() == "Hot Phrases"


def test_reopened_process_still_leaves_the_request_but_never_opens(monkeypatch, rec):
    from localflow import hub_route

    monkeypatch.setattr(sys, "platform", "darwin")
    _hold_lock(monkeypatch)

    main_app.main(["--reopened", "--page", "Models"])

    assert [cmd[0] for cmd in rec.run] == ["osascript"]
    assert hub_route.consume_request() == "Models"


def test_failed_request_write_is_logged_and_reopen_still_happens(
    monkeypatch, rec, tmp_path
):
    monkeypatch.setattr(sys, "platform", "darwin")
    _hold_lock(monkeypatch)
    monkeypatch.setattr(main_app, "write_request", lambda page: False)

    main_app.main(["--page", "History"])

    assert rec.run[0][0] == "open"
    assert "could not leave a request for page 'History'" in _log_text(tmp_path)


def test_no_page_leaves_no_request(monkeypatch, rec):
    from localflow import hub_route

    monkeypatch.setattr(sys, "platform", "darwin")
    _hold_lock(monkeypatch)

    main_app.main([])

    assert hub_route.consume_request() is None


def test_native_launch_with_page_opens_the_hub_there(monkeypatch, rec):
    save_config(Config(setup_version=1))
    built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)

    main_app.main(["--page", "Hot Phrases"])
    shown[0]()

    assert built == ["hub"] and pages == ["Hot Phrases"]
    assert polls == [shown[0].__self__.poll]


def test_native_launch_without_page_opens_home(monkeypatch, rec):
    save_config(Config(setup_version=1))
    _built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)

    main_app.main([])
    shown[0]()

    assert pages == ["Home"]


def _running_hub(monkeypatch):
    save_config(Config(setup_version=1))
    built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)
    main_app.main([])
    shown[0]()
    router = shown[0].__self__
    return built, router, polls[0]


def test_page_request_navigates_a_visible_hub_without_a_second_one(monkeypatch, rec):
    from localflow import hub_route

    built, router, poll = _running_hub(monkeypatch)
    hub = router.hub
    hub.window.events.clear()  # drop the startup "front" from the initial build
    hub_route.write_request("Hot Phrases")

    assert poll() is True

    assert built == ["hub"] and router.hub is hub
    assert hub.shown_pages == ["Hot Phrases"]
    assert hub.window.events == ["front"]
    assert poll() is False  # consumed: the request does not fire twice


def test_page_request_restores_a_minimized_hub_and_navigates(monkeypatch, rec):
    from localflow import hub_route

    built, router, poll = _running_hub(monkeypatch)
    hub = router.hub
    hub.window.events.clear()  # drop the startup "front" from the initial build
    hub.window.mini = True
    hub_route.write_request("Hot Phrases")

    assert poll() is True

    assert built == ["hub"] and router.hub is hub
    assert hub.window.events == ["deminiaturize"]
    assert hub.shown_pages == ["Hot Phrases"]


def test_page_request_after_the_window_was_closed_builds_one_fresh_hub(
    monkeypatch, rec
):
    from localflow import hub_route

    built, router, poll = _running_hub(monkeypatch)
    router.hub.window.visible = False
    hub_route.write_request("Meetings")

    assert poll() is True

    assert built == ["hub", "hub"] and pages == ["Home", "Meetings"]
    assert router.hub.page == "Meetings"


def test_request_for_the_page_already_shown_does_not_rebuild_it(monkeypatch, rec):
    from localflow import hub_route

    _built, router, poll = _running_hub(monkeypatch)
    hub_route.write_request("Home")

    assert poll() is True

    assert router.hub.shown_pages == []


def test_reopen_event_consumes_a_pending_request_even_for_a_visible_window(
    monkeypatch, rec
):
    from localflow import hub_route

    built, router, _poll = _running_hub(monkeypatch)
    hub_route.write_request("Prompts")

    router.show()  # what the LaunchServices reopen handler calls

    assert built == ["hub"] and router.hub.shown_pages == ["Prompts"]
    assert hub_route.consume_request() is None


def test_plain_reopen_keeps_the_current_page(monkeypatch, rec):
    built, router, _poll = _running_hub(monkeypatch)

    router.show()

    assert built == ["hub"] and router.hub.shown_pages == []


def test_unconfigured_routes_to_setup_and_keeps_the_page_until_configured(
    monkeypatch, rec
):
    from localflow import hub_route

    built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)

    main_app.main(["--page", "Hot Phrases"])
    shown[0]()
    hub_route.write_request("History")  # a later request replaces the wish
    polls[0]()

    assert built == ["setup"] and pages == []

    save_config(Config(setup_version=1))
    shown[0]()

    assert built == ["setup", "hub"] and pages == ["History"]


def test_unconfigured_launch_page_is_honoured_after_setup(monkeypatch, rec):
    built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)

    main_app.main(["--page", "Hot Phrases"])
    shown[0]()
    save_config(Config(setup_version=1))
    shown[0]()

    assert built == ["setup", "hub"] and pages == ["Hot Phrases"]


def test_stale_request_never_steers_an_unrelated_launch(monkeypatch, rec):
    import json
    import time

    from localflow import hub_route

    path = hub_route.request_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({"page": "Hot Phrases", "ts": time.time() - 3600}), encoding="utf-8"
    )
    save_config(Config(setup_version=1))
    _built, shown = _native(monkeypatch)
    monkeypatch.setattr(main_app, "ensure_runtime", lambda: None)

    main_app.main([])
    shown[0]()

    assert pages == ["Home"]
    assert not path.exists()


def test_poll_is_quiet_without_a_request(monkeypatch, rec):
    built, router, poll = _running_hub(monkeypatch)

    assert poll() is False
    assert built == ["hub"] and router.hub.shown_pages == []


def test_mac_poller_serves_requests_from_a_run_loop_timer():
    source = (ROOT / "localflow" / "main_app_mac.py").read_text(encoding="utf-8")
    tree = ast.parse(source)
    run_app = next(
        n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == "run_app"
    )
    assert [a.arg for a in run_app.args.args] == ["show", "poll"]
    assert "scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_" in source
    poller = next(
        n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "PageRequestPoller"
    )
    names = [m.name for m in poller.body if isinstance(m, ast.FunctionDef)]
    assert names == ["initWithPoll_", "tick_"]
