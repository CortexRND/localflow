"""Launch of LocalFlow Preview (`lf-desktop`) from the `lf` CLI.

Only `lf hub`, bare `lf phrases` and the `lf setup` window call `launch`, after
their own validation. The direct window dispatchers (`python -m
localflow.hub_window`, `phrases_window`, `setup_window`) never come here, so
they stay the stable legacy escape paths and can't recurse.

Selection, highest first: `--gpui` / `--legacy`, then `LOCALFLOW_UI`, then
`DEFAULT_UI` (gpui). There is no config key. Only a GPUI selection looks for
a native binary: an absolute `LOCALFLOW_DESKTOP_BIN`, else the package that
`scripts/install_gpui_preview.sh` owns. PATH and build trees are never
searched. Anything wrong prints one short notice and opens exactly today's
legacy window once. A sandbox request (`LOCALFLOW_SANDBOX`) is refused rather
than turned into a real-data launch.

The default is conditional. When GPUI comes only from `DEFAULT_UI` (no flag,
blank or absent `LOCALFLOW_UI`), an unsupported platform, or no override and
nothing at the package root, opens the legacy window quietly, as before.
Anything at the package root, or a non-empty override, is a candidate and
goes through the same checks and notices as `--gpui`.
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

DEFAULT_UI = "gpui"  # Scope 4b. Rollback: "legacy" here and in lf-rs restores 4a.
UI_CHOICES = ("gpui", "legacy")
UI_ENV = "LOCALFLOW_UI"
BIN_ENV = "LOCALFLOW_DESKTOP_BIN"
SANDBOX_ENV = "LOCALFLOW_SANDBOX"

# Ownership metadata written by scripts/install_gpui_preview.sh.
LINUX_ROOT = (".local", "share", "localflow-preview")
LINUX_MANIFEST_NAME = "localflow-preview"
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
NOTICE_PLATFORM = f"{_PREFIX}LocalFlow Preview has no package for this platform{_LEGACY}"
NOTICE_NOT_INSTALLED = (
    f"{_PREFIX}LocalFlow Preview isn't installed (see scripts/install_gpui_preview.sh){_LEGACY}"
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
NOTICE_EARLY_EXIT = f"{_PREFIX}LocalFlow Preview exited during startup (code {{code}}){_LEGACY}"
SANDBOX_REFUSAL = (
    f"{SANDBOX_ENV} is set, and `lf` opens LocalFlow Preview only on your real data. "
    "Run the preview directly instead: lf-desktop --sandbox "
    '(Linux: localflow-preview --sandbox; macOS: open -a "LocalFlow Preview" --args --sandbox), '
    f"or use --legacy."
)

GPUI_HELP = (
    "Open LocalFlow Preview (native GPUI app) instead of the legacy window. "
    "The default on Linux and macOS when it is installed. No screen-reader "
    "support yet: use --legacy (or lf-hub on macOS)."
)
LEGACY_HELP = (
    "Open the legacy window (also used when LocalFlow Preview isn't installed)."
)


def ui_options(func):
    """`--gpui` / `--legacy` for the three window commands (mutually exclusive)."""
    func = click.option("--legacy", "ui_legacy", is_flag=True, help=LEGACY_HELP)(func)
    return click.option("--gpui", "ui_gpui", is_flag=True, help=GPUI_HELP)(func)


def flag_choice(gpui: bool, legacy: bool) -> str | None:
    if gpui and legacy:
        raise click.UsageError("--gpui and --legacy can't be used together")
    return "gpui" if gpui else "legacy" if legacy else None


def choose_ui(flag: str | None, env: Mapping[str, str]) -> tuple[str, str | None]:
    """(choice, notice): flag, then `LOCALFLOW_UI`, then `DEFAULT_UI`."""
    if flag is not None:
        return flag, None
    value = env.get(UI_ENV, "").strip().lower()
    if not value:
        return DEFAULT_UI, None
    if value in UI_CHOICES:
        return value, None
    return "legacy", NOTICE_INVALID_ENV


def implicit_choice(flag: str | None, env: Mapping[str, str]) -> bool:
    """True when the choice comes only from `DEFAULT_UI` (no flag, blank env)."""
    return flag is None and not env.get(UI_ENV, "").strip()


def sandbox_requested(env: Mapping[str, str]) -> bool:
    """`lf-desktop`'s own rule: `LOCALFLOW_SANDBOX` is exactly `1` or `true`."""
    return env.get(SANDBOX_ENV) in ("1", "true")


def platform_key(platform: str | None = None) -> str | None:
    platform = sys.platform if platform is None else platform
    if platform.startswith("linux"):
        return "linux"
    if platform == "darwin":
        return "macos"
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


def _linux_owned(root: Path) -> bool:
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


def owned_install(key: str, home: Path) -> tuple[Path | None, str | None]:
    """The preview installer's `lf-desktop`, only if its metadata says it's ours."""
    if key == "linux":
        root = home.joinpath(*LINUX_ROOT)
        exe = root / "bin" / "lf-desktop"
        owned = _linux_owned
    else:
        root = home.joinpath(*MAC_APP)
        exe = root / "Contents" / "MacOS" / "lf-desktop"
        owned = _mac_owned
    if not os.path.lexists(root):
        return None, NOTICE_NOT_INSTALLED
    if not _real_dir(root) or not owned(root):
        return None, NOTICE_NOT_OWNED
    if not _executable_file(exe, follow_symlinks=False):
        return None, NOTICE_NOT_INSTALLED
    return exe, None


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
        return None, NOTICE_NOT_INSTALLED
    return owned_install(key, home)


def candidate_absent(
    env: Mapping[str, str], platform: str | None = None, home: Path | None = None
) -> bool:
    """For the implicit default only: nothing to try, so stay quiet.

    True on an unsupported platform, or with no override and nothing at the
    package root (or no absolute HOME). Anything at the root, owned or not,
    is a candidate that `find_desktop` then checks and reports.
    """
    key = platform_key(platform)
    if key is None:
        return True
    if env.get(BIN_ENV, ""):
        return False
    home = home if home is not None else Path.home()
    if not home.is_absolute():
        return True
    return not os.path.lexists(home.joinpath(*(LINUX_ROOT if key == "linux" else MAC_APP)))


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
    popen_kwargs = {"start_new_session": True} if os.name == "posix" else {}
    try:
        proc = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            close_fds=True,
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
    choice, notice = choose_ui(flag, env)
    if notice:
        _notice(notice)
    if choice != "gpui":
        legacy()
        return
    if implicit_choice(flag, env) and candidate_absent(env):
        legacy()  # the conditional default: nothing installed, as before
        return
    if sandbox_requested(env):
        raise click.ClickException(SANDBOX_REFUSAL)
    exe, notice = find_desktop(env)
    if exe is not None and not probe(exe):
        exe, notice = None, NOTICE_PROBE
    if exe is not None:
        started, notice = spawn(desktop_argv(exe, page=page, setup=setup))
        if started:
            return
    _notice(notice or NOTICE_SPAWN)
    legacy()
