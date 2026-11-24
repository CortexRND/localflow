"""Launch of LocalFlow Preview (`lf-desktop`) from the `lf` CLI.

Only `lf hub`, bare `lf phrases` and the `lf setup` window call `launch`, after
their own validation. The direct window dispatchers (`python -m
localflow.hub_window`, `phrases_window`, `setup_window`) never come here, so
they stay the stable legacy escape paths and can't recurse.

Selection, highest first: `--gpui` / `--legacy`, then `LOCALFLOW_RUNTIME=python`
(legacy), then `LOCALFLOW_UI`, then `default_ui()`: gpui for the Hub routes
(`lf hub`, `lf phrases`) on Linux and Windows, legacy for the `lf setup` window
and on every other platform (macOS keeps the AppKit `lf-hub`). There is no config key.
Only a GPUI selection looks for a native binary: an absolute
`LOCALFLOW_DESKTOP_BIN`, else the owned package (Linux and macOS:
`scripts/install_gpui_preview.sh`; Windows: the `LocalFlow Preview` zip folder
unzipped into `%LOCALAPPDATA%\\Programs\\`). PATH and build trees are never
searched. Anything wrong prints one short notice and opens exactly today's
legacy window once, except that a default (not explicit) GPUI choice opens it
silently when the preview isn't installed. A sandbox request
(`LOCALFLOW_SANDBOX`) is refused for an explicit GPUI choice rather than turned
into a real-data launch; the default just opens the legacy window. The Hub
routes (`lf hub`, `lf phrases`) open the preview only once LocalFlow is set up
(the active config has the setup marker), for an explicit or a default choice,
so they never land in the preview's own first-run wizard; only `lf setup --gpui`
opens that.
"""

from __future__ import annotations

import os
import plistlib
import re
import stat
import subprocess
import sys
import tempfile
from collections.abc import Callable, Mapping
from pathlib import Path

import click

UI_CHOICES = ("gpui", "legacy")
UI_ENV = "LOCALFLOW_UI"
RUNTIME_ENV = "LOCALFLOW_RUNTIME"  # the installers validate it; lf only honours `python`
BIN_ENV = "LOCALFLOW_DESKTOP_BIN"
SANDBOX_ENV = "LOCALFLOW_SANDBOX"
# The interpreter running this `lf`, for History Retry's fallback to this
# install's speech model (`python -m localflow.cli history retry --json`).
PYTHON_ENV = "LOCALFLOW_PYTHON"

# Ownership metadata written by scripts/install_gpui_preview.sh (Linux, macOS)
# and by scripts/build_gpui_package.sh into the Windows zip's folder.
LINUX_ROOT = (".local", "share", "localflow-preview")
LINUX_MANIFEST_NAME = "localflow-preview"  # also the Windows MANIFEST.json name
WINDOWS_ROOT = ("Programs", "LocalFlow Preview")  # under %LOCALAPPDATA%
MAC_APP = ("Applications", "LocalFlow Preview.app")
MAC_BUNDLE_ID = "com.cortexrnd.localflow.desktop-preview"

# The `lf-desktop --help` line promising its exit status: 0 only when a window
# opened (then closed) or the request was handed to the open window, 1 when no
# window could open. Scope 3 preview builds (also 0.2.2, with --page/--setup)
# lack it and exit 0 even when no window opens, so they're treated as too old.
# Same marker as lf_ui::desktop_launch::LAUNCH_STATUS_MARKER and lf-desktop's USAGE.
LAUNCH_STATUS_MARKER = "EXIT STATUS (launch-status: 1):"

PROBE_TIMEOUT = 5.0  # seconds; only this `--help` probe is ever killed
STARTUP_WAIT = 1.5  # seconds a launched window is watched for an early exit
_MAX_METADATA_BYTES = 1 << 20
_MAX_HELP_BYTES = 64 * 1024

_PREFIX = "lf: "
_LEGACY = "; opening the legacy window."
NOTICE_INVALID_ENV = f"{_PREFIX}{UI_ENV} must be gpui or legacy{_LEGACY}"
NOTICE_PLATFORM = f"{_PREFIX}lf can't open LocalFlow Preview on this platform{_LEGACY}"
NOTICE_NOT_INSTALLED = (
    f"{_PREFIX}LocalFlow Preview isn't installed (see scripts/install_gpui_preview.sh){_LEGACY}"
)
NOTICE_NOT_INSTALLED_WINDOWS = (
    f"{_PREFIX}LocalFlow Preview isn't installed (unzip the LocalFlow Preview folder "
    f"into %LOCALAPPDATA%\\Programs){_LEGACY}"
)
NOTICE_NOT_OWNED = (
    f"{_PREFIX}the LocalFlow Preview install location holds something the preview "
    f"installer didn't install{_LEGACY}"
)
NOTICE_BAD_OVERRIDE = (
    f"{_PREFIX}{BIN_ENV} must be an absolute path to an executable lf-desktop{_LEGACY}"
)
NOTICE_PROBE = (
    f"{_PREFIX}that lf-desktop is too old (update LocalFlow Preview) or isn't "
    f"LocalFlow Preview{_LEGACY}"
)
NOTICE_SPAWN = f"{_PREFIX}couldn't start LocalFlow Preview{_LEGACY}"
NOTICE_NOT_SET_UP = f"{_PREFIX}LocalFlow isn't set up yet (run lf setup){_LEGACY}"
NOTICE_EARLY_EXIT = f"{_PREFIX}LocalFlow Preview exited during startup (code {{code}}){_LEGACY}"
SANDBOX_REFUSAL = (
    f"{SANDBOX_ENV} is set, and `lf` opens LocalFlow Preview only on your real data. "
    "Run the preview directly instead: lf-desktop --sandbox "
    '(Linux: localflow-preview --sandbox; macOS: open -a "LocalFlow Preview" --args --sandbox), '
    f"or use --legacy."
)

GPUI_HELP = (
    "Open LocalFlow Preview (native GPUI app, if installed; the default for lf hub "
    "and lf phrases on Linux and Windows) instead of the legacy window. No "
    "screen-reader support yet: use --legacy (or lf-hub on macOS)."
)
LEGACY_HELP = (
    "Open the legacy Python window (the default for lf setup, on macOS, when "
    "LocalFlow Preview isn't installed, or with LOCALFLOW_RUNTIME=python)."
)


def ui_options(func):
    """`--gpui` / `--legacy` for the three window commands (mutually exclusive)."""
    func = click.option("--legacy", "ui_legacy", is_flag=True, help=LEGACY_HELP)(func)
    return click.option("--gpui", "ui_gpui", is_flag=True, help=GPUI_HELP)(func)


def flag_choice(gpui: bool, legacy: bool) -> str | None:
    if gpui and legacy:
        raise click.UsageError("--gpui and --legacy can't be used together")
    return "gpui" if gpui else "legacy" if legacy else None


def default_ui(platform: str | None = None, *, setup: bool = False) -> str:
    """gpui for the Hub routes on Linux and Windows; legacy for the setup window,
    on macOS and on anything unknown."""
    # Preview's wizard fetches native (whisper.cpp) weights the Python runtime can't use.
    if setup:
        return "legacy"
    platform = sys.platform if platform is None else platform
    return "gpui" if platform.startswith("linux") or platform == "win32" else "legacy"


def choose_ui(
    flag: str | None,
    env: Mapping[str, str],
    platform: str | None = None,
    *,
    setup: bool = False,
) -> tuple[str, bool, str | None]:
    """(choice, explicit, notice): flag, `LOCALFLOW_RUNTIME=python`, `LOCALFLOW_UI`,
    then `default_ui()`. Only the platform default is not explicit."""
    if flag is not None:
        return flag, True, None
    # Any other runtime value is ignored here: the installers validate it.
    if env.get(RUNTIME_ENV, "").strip().lower() == "python":
        return "legacy", True, None
    value = env.get(UI_ENV, "").strip().lower()
    if not value:
        return default_ui(platform, setup=setup), False, None
    if value in UI_CHOICES:
        return value, True, None
    return "legacy", True, NOTICE_INVALID_ENV


def sandbox_requested(env: Mapping[str, str]) -> bool:
    """`lf-desktop`'s own rule: `LOCALFLOW_SANDBOX` is exactly `1` or `true`."""
    return env.get(SANDBOX_ENV) in ("1", "true")


def platform_key(platform: str | None = None) -> str | None:
    platform = sys.platform if platform is None else platform
    if platform.startswith("linux"):
        return "linux"
    if platform == "darwin":
        return "macos"
    if platform == "win32":
        return "windows"
    return None


def _real_dir(path: Path) -> bool:
    try:
        return stat.S_ISDIR(os.lstat(path).st_mode)
    except OSError:
        return False


def _small_regular_file(path: Path) -> bytes | None:
    """Contents of a non-symlink regular file of bounded size, else None."""
    try:
        info = os.lstat(path)
        if not stat.S_ISREG(info.st_mode) or info.st_size > _MAX_METADATA_BYTES:
            return None
        return path.read_bytes()[:_MAX_METADATA_BYTES]
    except OSError:
        return None


def _executable_file(path: Path, *, follow_symlinks: bool) -> bool:
    try:
        info = os.stat(path) if follow_symlinks else os.lstat(path)
    except OSError:
        return False
    return stat.S_ISREG(info.st_mode) and os.access(path, os.X_OK)


def _manifest_owned(root: Path) -> bool:
    import json

    raw = _small_regular_file(root / "MANIFEST.json")
    if raw is None:
        return False
    try:
        manifest = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return False
    return isinstance(manifest, dict) and manifest.get("name") == LINUX_MANIFEST_NAME


def _mac_owned(app: Path) -> bool:
    raw = _small_regular_file(app / "Contents" / "Info.plist")
    if raw is None:
        return False
    try:
        info = plistlib.loads(raw)
    except Exception:  # noqa: BLE001 - any malformed plist means "not ours"
        return False
    return isinstance(info, dict) and info.get("CFBundleIdentifier") == MAC_BUNDLE_ID


def local_appdata(env: Mapping[str, str], home: Path) -> Path:
    """`LOCALAPPDATA` when it's an absolute path, else `<home>/AppData/Local`."""
    value = env.get("LOCALAPPDATA", "")
    if value and Path(value).is_absolute():
        return Path(value)
    return home.joinpath("AppData", "Local")


def owned_install(
    key: str, home: Path, env: Mapping[str, str] | None = None
) -> tuple[Path | None, str | None]:
    """The preview package's `lf-desktop`, only if its metadata says it's ours."""
    if key == "linux":
        root = home.joinpath(*LINUX_ROOT)
        exe = root / "bin" / "lf-desktop"
        owned = _manifest_owned
    elif key == "windows":
        root = local_appdata({} if env is None else env, home).joinpath(*WINDOWS_ROOT)
        exe = root / "lf-desktop.exe"
        owned = _manifest_owned
    else:
        root = home.joinpath(*MAC_APP)
        exe = root / "Contents" / "MacOS" / "lf-desktop"
        owned = _mac_owned
    if not os.path.lexists(root):
        return None, not_installed_notice(key)
    if not _real_dir(root) or not owned(root):
        return None, NOTICE_NOT_OWNED
    if not _executable_file(exe, follow_symlinks=False):
        return None, not_installed_notice(key)
    return exe, None


def not_installed_notice(key: str | None) -> str:
    """Where to get the preview: Windows has no installer script, only the zip."""
    return NOTICE_NOT_INSTALLED_WINDOWS if key == "windows" else NOTICE_NOT_INSTALLED


def is_not_installed(notice: str | None) -> bool:
    """True for either "isn't installed" notice (the silent default fallback)."""
    return notice in (NOTICE_NOT_INSTALLED, NOTICE_NOT_INSTALLED_WINDOWS)


def find_desktop(
    env: Mapping[str, str], platform: str | None = None, home: Path | None = None
) -> tuple[Path | None, str | None]:
    """(executable, notice). An explicit override never falls through to the package."""
    key = platform_key(platform)
    if key is None:
        return None, NOTICE_PLATFORM
    override = env.get(BIN_ENV, "")
    if override:
        path = Path(override)
        if not path.is_absolute() or not _executable_file(path, follow_symlinks=True):
            return None, NOTICE_BAD_OVERRIDE
        return path, None
    home = home if home is not None else Path.home()
    if not home.is_absolute():  # same as the Rust helper: absolute paths only
        return None, not_installed_notice(key)
    return owned_install(key, home, env)


# Exact tokens, as in lf_ui::desktop_launch::help_has_interface: the identity
# can't continue with a word character or `-`, and each flag line is the exact
# token followed by whitespace or the end of the line.
_HELP_IDENTITY = re.compile(r"\A\s*lf-desktop(?![\w-])")
_HELP_PAGE = re.compile(r"^[ \t]*--page <PAGE>(?:[ \t]|$)", re.MULTILINE)
_HELP_SETUP = re.compile(r"^[ \t]*--setup(?:[ \t]|$)", re.MULTILINE)
_HELP_LAUNCH_STATUS = re.compile(rf"^[ \t]*{re.escape(LAUNCH_STATUS_MARKER)}[ \t]*$", re.MULTILINE)


def probe(exe: Path, timeout: float | None = None) -> bool:
    """True when `exe --help` is an lf-desktop with `--page`/`--setup` and the
    launch-status promise.

    A version number alone isn't enough: early unpackaged 0.2.2 builds have no
    Setup, and Scope 3 packages exit 0 when no window opens. Output goes to a
    temporary file (no pipe to fill); only this probe is killed when it hangs.
    """
    try:
        with tempfile.TemporaryFile() as out:
            proc = subprocess.Popen(
                [str(exe), "--help"],
                stdin=subprocess.DEVNULL,
                stdout=out,
                stderr=subprocess.DEVNULL,
                close_fds=True,
            )
            try:
                code = proc.wait(timeout=PROBE_TIMEOUT if timeout is None else timeout)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
                return False
            out.seek(0)
            text = out.read(_MAX_HELP_BYTES).decode("utf-8", errors="replace")
    except OSError:
        return False
    return (
        code == 0
        and bool(_HELP_IDENTITY.match(text))
        and bool(_HELP_PAGE.search(text))
        and bool(_HELP_SETUP.search(text))
        and bool(_HELP_LAUNCH_STATUS.search(text))
    )


def set_up() -> bool:
    """The active config file has the setup marker (`main_app.is_configured`).

    Only the active config is read: an unmigrated legacy flat file stays on the
    legacy window (which migrates it) and counts from the next launch.
    """
    from localflow.config import config_path
    from localflow.main_app import is_configured

    try:
        return config_path().exists() and is_configured()
    except OSError:  # e.g. an untraversable config directory
        return False


def desktop_argv(exe: Path, *, page: str | None = None, setup: bool = False) -> list[str]:
    """Closed argv: nothing from the user's command line, never --engine/--sandbox."""
    from localflow.hub_route import canonical_page

    if setup:
        return [str(exe), "--setup"]
    if page is None:
        return [str(exe)]  # lf-desktop keeps its own first-run wizard policy
    canonical = canonical_page(page)
    if canonical is None:
        raise ValueError(f"not a Hub page: {page!r}")
    return [str(exe), "--page", canonical]


def spawn(argv: list[str], wait: float | None = None) -> tuple[bool, str | None]:
    """Start the window detached; (started, notice). Never kills the window.

    Exit 0 within `wait` is a hand-off to an already open window. A crash after
    `wait` isn't seen here. stderr is inherited (nothing captured or logged).
    """
    # posix: its own session, so a hang-up of the terminal that ran `lf` doesn't
    # reach the window. Windows: its own process group, so a Ctrl+C typed in
    # that console doesn't reach it (Ctrl+Break and closing the console still do:
    # it isn't detached from the console).
    if os.name == "posix":
        popen_kwargs = {"start_new_session": True}
    else:
        popen_kwargs = {"creationflags": getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)}
    env = dict(os.environ)
    if sys.executable:
        env[PYTHON_ENV] = sys.executable
    try:
        proc = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            close_fds=True,
            env=env,
            **popen_kwargs,
        )
    except OSError:
        return False, NOTICE_SPAWN
    try:
        code = proc.wait(timeout=STARTUP_WAIT if wait is None else wait)
    except subprocess.TimeoutExpired:
        return True, None
    if code == 0:
        return True, None
    return False, NOTICE_EARLY_EXIT.format(code=code)


def _notice(message: str) -> None:
    click.echo(message, err=True)


def launch(
    flag: str | None,
    legacy: Callable[[], object],
    *,
    page: str | None = None,
    setup: bool = False,
    env: Mapping[str, str] | None = None,
) -> None:
    """Open the selected window: LocalFlow Preview, or `legacy()` exactly once."""
    env = os.environ if env is None else env
    choice, explicit, notice = choose_ui(flag, env, setup=setup)
    if notice:
        _notice(notice)
    if choice != "gpui":
        legacy()
        return
    if sandbox_requested(env):
        if explicit:
            raise click.ClickException(SANDBOX_REFUSAL)
        legacy()  # the default keeps the legacy sandbox semantics
        return
    exe, notice = find_desktop(env)
    if exe is None and is_not_installed(notice) and not explicit:
        legacy()  # the installers don't ship the preview yet: no notice by default
        return
    if exe is not None and not probe(exe):
        exe, notice = None, NOTICE_PROBE
    if exe is not None and not setup and not set_up():
        exe, notice = None, NOTICE_NOT_SET_UP  # never the preview's own wizard
    if exe is not None:
        started, notice = spawn(desktop_argv(exe, page=page, setup=setup))
        if started:
            return
    _notice(notice or NOTICE_SPAWN)
    legacy()
