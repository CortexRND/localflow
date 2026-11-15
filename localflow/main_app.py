"""Entry point of ~/Applications/LocalFlow.app.

Unconfigured -> native setup window. Configured -> make sure the background
runtime (dictation + menu bar) is running, then show the native Hub. The app
keeps running after its window is closed; a later reopen (Dock click or
double-click, delivered by LaunchServices as an Apple event) restores the
right window through the delegate in `main_app_mac`.

`--page PAGE` (also reached through `lf hub`, `lf phrases`, the menu bar and
the optional Hot Phrases alias app) opens the Hub on that page. This is the
single Hub instance: a caller that finds the app running leaves an atomic
page request (see `hub_route`) that the running app serves from a timer.
"""

import os
import subprocess
import sys
from pathlib import Path

from localflow.appbundle import MAIN_BUNDLE_ID
from localflow.config import (
    ConfigError,
    _config_from_v2,
    _parse,
    config_path,
    legacy_config_path,
)
from localflow.config_migrate import migrate_v1
from localflow.hub_route import (
    DEFAULT_PAGE,
    consume_request,
    validate_page,
    write_request,
)
from localflow.singleinstance import acquire, read_holder

LOCK_NAME = "main-app"
REOPEN_FLAG = "--reopened"
PAGE_FLAG = "--page"


def parse_args(argv) -> tuple[bool, str | None]:
    """(reopened, page) from the launch arguments.

    Tolerant on purpose: LaunchServices adds its own arguments (`-psn_...`)
    and a bad `--page` must not stop the app from opening. An unknown page is
    logged and ignored; the last valid `--page` wins.
    """
    argv = list(argv)
    page = None
    for index, arg in enumerate(argv):
        if arg == PAGE_FLAG and index + 1 < len(argv):
            value = argv[index + 1]
        elif arg.startswith(PAGE_FLAG + "="):
            value = arg[len(PAGE_FLAG) + 1:]
        else:
            continue
        checked = validate_page(value)
        if checked is None:
            _log(f"ignoring unknown page {value!r}")
        else:
            page = checked
    return REOPEN_FLAG in argv, page


def _explicit(value) -> bool:
    return isinstance(value, str) and bool(value.strip())


def is_configured() -> bool:
    """True when the saved config shows the user already chose their setup.

    That is the wizard marker (`[setup] version > 0`) or an explicit STT
    `provider` or `model` in the file itself. The raw TOML is inspected, so
    dataclass defaults never imply "configured". The file consulted follows
    `load_config`: the active config path, else the legacy flat file. A
    missing, empty, unreadable, malformed or unsupported config is not
    configured.
    """
    try:
        target = config_path()
        if target.exists():
            data = _parse(target)
            _config_from_v2(data)  # same validation load_config applies
        else:
            legacy = legacy_config_path()
            if not legacy.exists():
                return False
            data, _ = migrate_v1(_parse(legacy))
    except (ConfigError, OSError, TypeError, ValueError):
        return False
    setup = data.get("setup")
    version = setup.get("version") if isinstance(setup, dict) else None
    if isinstance(version, int) and not isinstance(version, bool) and version > 0:
        return True
    stt = data.get("stt")
    return isinstance(stt, dict) and (
        _explicit(stt.get("provider")) or _explicit(stt.get("model"))
    )


def _held(name: str) -> bool:
    lock = acquire(name)
    if lock is None:
        return True
    lock.release()
    return False


def _log_file():
    log_dir = Path.home() / ".localflow"
    log_dir.mkdir(parents=True, exist_ok=True)
    return open(log_dir / "app.log", "ab")

def _log(message: str) -> None:
    try:
        with _log_file() as log:
            log.write(f"{message}\n".encode())
    except OSError:
        pass


def _detach(argv: list[str]) -> None:
    with _log_file() as log:
        subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=log,
            start_new_session=True,
            close_fds=True,
        )


def ensure_runtime() -> None:
    """Start dictation and the menu bar unless they are already running.

    A failure to start either one is logged but never blocks the window.
    """
    if not _held("localflow"):
        try:
            _start_dictation()
        except OSError as exc:
            _log(f"could not start dictation: {exc}")
    if not _held("menubar"):
        try:
            import rumps  # noqa: F401
        except ImportError:
            return
        try:
            _detach([sys.executable, "-m", "localflow.menubar"])
        except OSError as exc:
            _log(f"could not start the menu bar: {exc}")


def _start_agent(state: str) -> bool:
    """Try to get the installed login agent running. True when it was started."""
    from localflow import launchagent

    try:
        if state.startswith("not loaded"):
            # Plist exists but launchd dropped it: bootstrap it again.
            launchagent.install()
            return True
        result = subprocess.run(
            ["launchctl", "kickstart", launchagent._gui_target()],
            capture_output=True,
            text=True,
            check=False,
        )
    except (OSError, RuntimeError) as exc:
        _log(f"login agent could not be started ({exc}); starting dictation directly")
        return False
    if result.returncode != 0:
        _log(
            "launchctl kickstart failed "
            f"({(result.stderr or '').strip() or result.returncode}); "
            "starting dictation directly"
        )
        return False
    return True


def _start_dictation() -> None:
    from localflow import launchagent

    state = launchagent.status()
    if state != "not installed" and _start_agent(state):
        return
    # No agent, or it would not start: run the entry point ourselves. The
    # single-instance lock keeps a late-starting agent from duplicating it.
    _detach([str(Path(sys.executable).parent / "localflow")])


def activate_holder(name: str) -> None:
    """Last resort: bring the process that owns lock `name` to the front."""
    pid, _ = read_holder(name)
    if pid is None:
        return
    script = (
        'tell application "System Events" to set frontmost of '
        f"(first process whose unix id is {int(pid)}) to true"
    )
    subprocess.run(["osascript", "-e", script], capture_output=True, check=False)


def reopen_running_app(allow_open: bool, page: str | None = None) -> None:
    """Ask the running app to restore its window, on `page` when given.

    LaunchServices never delivers new `--args` to a running app, so a page is
    passed as an atomic request file that the app consumes from its run-loop
    timer (see hub_route). `open -b` then makes LaunchServices send the
    running app a reopen event, which its delegate answers by restoring a
    window (consuming the same request). `--args --reopened` marks a process
    launched this way: if that process again finds the lock held, it must not
    call `open` once more (that would loop when the lock holder is not
    registered with LaunchServices), so it only activates the holder.
    """
    if page is not None and not write_request(page):
        _log(f"could not leave a request for page {page!r}")
    if allow_open:
        result = subprocess.run(
            ["open", "-b", MAIN_BUNDLE_ID, "--args", REOPEN_FLAG],
            capture_output=True,
            check=False,
        )
        if result.returncode == 0:
            return
    activate_holder(LOCK_NAME)


def front_or_build(existing, build, navigate=None):
    """Bring `existing` controller's window forward, or build a new one.

    A closed window is never reused (AppKit may have released it). For a
    reused window `navigate(existing)` runs after it is shown, so an explicit
    page goes through the window's own navigation guard.
    """
    window = getattr(existing, "window", None)
    if window is not None:
        reuse = False
        if window.isMiniaturized():
            window.deminiaturize_(None)
            reuse = True
        elif window.isVisible():
            window.makeKeyAndOrderFront_(None)
            reuse = True
        if reuse:
            if navigate is not None:
                navigate(existing)
            return existing
    controller = build()
    controller.window.makeKeyAndOrderFront_(None)
    return controller


class WindowRouter:
    """Decides which window a launch, reopen or page request should show.

    `desired` is the page asked for but not yet shown; it is kept while setup
    is unfinished so first-run setup wins and the route is honoured after.
    """

    def __init__(self, configured, runtime, open_hub, open_setup) -> None:
        self._configured = configured
        self._runtime = runtime
        self._open_hub = open_hub
        self._open_setup = open_setup
        self.hub = None
        self.setup = None
        self.desired = None

    def show(self, page=None) -> None:
        requested = validate_page(page) or consume_request()
        if requested:
            self.desired = requested
        if self._configured():
            self._runtime()
            page, self.desired = self.desired, None
            self.hub = self._open_hub(self.hub, page)
        else:
            self.setup = self._open_setup(self.setup)

    def poll(self) -> bool:
        """Run-loop timer hook: serve a pending page request. True if served."""
        page = consume_request()
        if page is None:
            return False
        self.show(page)
        return True


def _native_hub(existing, page=None):
    from localflow import hub_mac

    def navigate(controller):
        # Same page: nothing to do (a rebuild would only reset scrolling).
        if page and getattr(controller, "page", None) != page:
            controller.show_page(page)

    return front_or_build(
        existing,
        lambda: hub_mac.build_window(initial_page=page or DEFAULT_PAGE),
        navigate,
    )


def _native_setup(existing):
    from localflow import setup_mac

    return front_or_build(existing, setup_mac.build_window)


def _run_native(page=None) -> bool:
    """Run the AppKit app with a reopen delegate. False when unavailable."""
    try:
        from localflow.main_app_mac import run_app
    except ImportError:
        return False
    router = WindowRouter(is_configured, ensure_runtime, _native_hub, _native_setup)
    router.desired = page
    run_app(router.show, router.poll)
    return True


def _run_portable(page=None) -> None:
    if not is_configured():
        from localflow.setup_window import main as setup_main

        setup_main()
        return
    ensure_runtime()
    # The platform backend, not hub_window.main: on macOS that dispatcher
    # routes back here, which would recurse when PyObjC is unavailable.
    from localflow.hub_window import run_backend

    run_backend(page or DEFAULT_PAGE)


def main(argv=None) -> None:
    reopened, page = parse_args(sys.argv[1:] if argv is None else argv)
    # The windows below parse sys.argv themselves; hand them none of ours.
    sys.argv = sys.argv[:1]
    lock = acquire(LOCK_NAME)
    if lock is None:
        reopen_running_app(
            allow_open=not reopened and sys.platform == "darwin", page=page
        )
        return
    try:
        if sys.platform == "darwin" and _run_native(page):
            return
        _run_portable(page)
    finally:
        lock.release()


if __name__ == "__main__":
    os.environ.setdefault("PYTHONUNBUFFERED", "1")
    main()
