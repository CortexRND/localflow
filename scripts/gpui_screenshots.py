"""Capture the Linux GPUI sandbox window in both themes and at both gate sizes."""

import argparse
import json
import multiprocessing
import os
import re
import subprocess
import sys
import time
from contextlib import contextmanager
from pathlib import Path
from tempfile import TemporaryDirectory

PAGES = (
    "home", "history", "dictionary", "snippets", "style", "ask",
    "meetings", "prompts", "models", "settings", "setup",
)
SIZES = ((1280, 850), (960, 640))
# Opt-in speaker-label scenes (`--sandbox-fixture`), captured after PAGES in their own
# sandbox directory so the default sandbox and its captures stay as they are. Each opens
# a real page and performs one real action there (rust/lf-desktop/src/sandbox.rs).
SPEAKER_SURFACES = ("speakers-saved", "speakers-recent", "speakers-settings", "speakers-people")
# Opt-in Agents scenes (`--sandbox-fixture agents-*`): the in-process fake agents service
# with its clock pinned, in their own sandbox directory (the agents fixture has its own
# marker, so the speaker scenes and the default sandbox don't gain the Agents page).
# `agents-settings*` open Settings → Agents over the same fake (providers, keys, workspaces).
AGENT_SURFACES = (
    "agents-page", "agents-states", "agents-detail", "agents-settings", "agents-settings-key",
    "agents-settings-trigger",
)
# Opt-in agent launcher scenes (mockups M1, M2, M3, M5): `--sandbox-fixture agents-launcher*`
# opens the launcher's own fixed-size popup window (not the Hub), in the same agents sandbox.
LAUNCHER_SURFACES = (
    "agents-launcher", "agents-launcher-typed", "agents-launcher-secret", "agents-launcher-empty",
)
# The launcher popup is not resizable: one capture per theme at its own size.
LAUNCHER_SIZES = ((640, 540),)
HUB_TITLE = r"^LocalFlow \(Sandbox\)$"
LAUNCHER_TITLE = r"^Agent launcher \(Sandbox\)$"
# lf-desktop prints this once a scene's real end state is rendered (fixture runs only).
SCENE_READY = "lf-desktop: scene ready: {}"
SCENE_TIMEOUT_SECONDS = 20
# GPUI 0.2 on X11 reads this before Xft.dpi and RandR, so it pins captures to scale 1.
SCALE_ENV = "GPUI_X11_SCALE_FACTOR"
# rust/lf-desktop/src/main.rs prints this before falling back to Home for an unknown --page.
UNKNOWN_PAGE = re.compile(r"^lf-desktop: There's no page called “(.*)”", re.MULTILINE)


def arguments(description):
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("binary", type=Path, help="Path to the built lf-desktop binary")
    parser.add_argument("output", type=Path, help="New artifact directory")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    if not os.access(args.binary, os.X_OK) or not args.binary.is_file():
        parser.error("binary must be an executable lf-desktop file")
    return args


def run(*command):
    return subprocess.check_output(command, text=True, timeout=10).strip()


def xdotool(*args):
    return run("xdotool", *args)


def wait_for(probe, label, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        time.sleep(0.15)
    raise RuntimeError(f"Timed out waiting for {label}")


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


@contextmanager
def window_manager():
    def running():
        return "window id #" in run("xprop", "-root", "_NET_SUPPORTING_WM_CHECK")

    if running():
        raise RuntimeError("Run on a dedicated Xvfb display without a window manager")
    manager = subprocess.Popen(["openbox"])
    try:
        wait_for(running, "Openbox")
        yield
    finally:
        stop(manager)


def capture_environment(base, sandbox):
    """The environment a capture runs lf-desktop in: X11 at scale 1, in UTC."""
    env = dict(base)
    env.pop("WAYLAND_DISPLAY", None)
    env[SCALE_ENV] = "1"
    # Fixture clocks are pinned, but times are drawn in local time: one zone for every
    # capture, so they don't change with the machine that takes them.
    env["TZ"] = "UTC"
    if not env.get("XDG_RUNTIME_DIR"):
        runtime = sandbox.parent / "runtime"
        runtime.mkdir(mode=0o700, exist_ok=True)
        env["XDG_RUNTIME_DIR"] = str(runtime)
    return env


@contextmanager
def sandbox_window(binary, sandbox, *route, scene=None, title=HUB_TITLE):
    """Open a sandbox window titled ``title``; with ``scene``, also wait until it reports that
    scene ready."""
    env = capture_environment(os.environ, sandbox)
    # A file, not a pipe: the app logs a lot and nothing drains a pipe while we wait.
    log = sandbox.parent / "lf-desktop.stderr"
    with open(log, "wb") as stderr:
        process = subprocess.Popen(
            [binary, "--sandbox", "--data-dir", sandbox, *route], env=env, stderr=stderr
        )

    def output():
        return log.read_text(encoding="utf-8", errors="replace")

    def require_known_page():
        name = rejected_page(output())
        if name is not None:
            raise RuntimeError(f"lf-desktop has no page called {name!r}; it showed Home instead")

    def started_environ():
        # Popen can return before execve has published the new environment,
        # so /proc/<pid>/environ is briefly empty; retry until it is populated.
        if process.poll() is not None:
            raise RuntimeError(f"Sandbox exited before starting ({process.returncode})")
        return process_environ(process.pid)

    def mapped_window():
        if process.poll() is not None:
            raise RuntimeError(f"Sandbox exited before opening a window ({process.returncode})")
        try:
            windows = xdotool(
                "search", "--all", "--onlyvisible", "--pid", str(process.pid),
                "--name", title,
            )
        except subprocess.CalledProcessError:
            return None
        return windows.splitlines()[0] if windows else None

    try:
        require_unit_scale(wait_for(started_environ, "the sandbox process environment", timeout=5))
        window = wait_for(mapped_window, "the GPUI sandbox window")
        require_known_page()
        if scene is not None:
            def ready():
                if process.poll() is not None:
                    raise RuntimeError(f"Sandbox exited before scene {scene} was ready ({process.returncode})")
                return scene_ready(output(), scene)

            wait_for(ready, f"scene {scene} to render its real view", timeout=SCENE_TIMEOUT_SECONDS)
        yield window
        xdotool("windowactivate", "--sync", window)
        xdotool("key", "ctrl+q")
        if process.wait(timeout=10):
            raise RuntimeError(f"Sandbox failed to close cleanly ({process.returncode})")
        require_known_page()
    finally:
        stop(process)
        sys.stderr.write(output())
        sys.stderr.flush()


def rejected_page(stderr_text):
    """Return the --page name lf-desktop said it does not know, or None."""
    match = UNKNOWN_PAGE.search(stderr_text)
    return match.group(1) if match else None


def scene_ready(stderr_text, scene):
    """True only when lf-desktop reported exactly this scene's real end state on its own line."""
    return SCENE_READY.format(scene) in stderr_text.splitlines()


def process_environ(pid):
    """Return the environment a running process was started with."""
    with open(f"/proc/{pid}/environ", "rb") as source:
        return parse_environ(source.read())


def parse_environ(raw):
    pairs = (entry.split(b"=", 1) for entry in raw.split(b"\0") if b"=" in entry)
    return {os.fsdecode(key): os.fsdecode(value) for key, value in pairs}


def require_unit_scale(environ):
    """Fail unless GPUI on X11 uses scale 1 regardless of host Xft.dpi or RandR."""
    value = environ.get(SCALE_ENV, "")
    try:
        scale = float(value)
    except ValueError:
        scale = None
    if scale != 1.0:
        raise RuntimeError(
            f"lf-desktop must run with {SCALE_ENV}=1 so captures are at scale 1 (got {value!r})"
        )


def serve_appearance(theme, ready):
    import dbus
    import dbus.service
    from dbus.mainloop.glib import DBusGMainLoop
    from gi.repository import GLib

    class Settings(dbus.service.Object):
        @dbus.service.method("org.freedesktop.portal.Settings", in_signature="ss", out_signature="v")
        def Read(self, namespace, key):
            if (namespace, key) != ("org.freedesktop.appearance", "color-scheme"):
                raise dbus.exceptions.DBusException(
                    "Setting not found", name="org.freedesktop.portal.Error.NotFound"
                )
            return dbus.UInt32(1 if theme == "dark" else 2, variant_level=2)

    DBusGMainLoop(set_as_default=True)
    bus = dbus.service.BusName(
        "org.freedesktop.portal.Desktop", dbus.SessionBus(), do_not_queue=True
    )
    portal = Settings(bus, "/org/freedesktop/portal/desktop")
    ready.set()
    GLib.MainLoop().run()
    portal.remove_from_connection()


@contextmanager
def appearance(theme):
    ready = multiprocessing.Event()
    portal = multiprocessing.Process(target=serve_appearance, args=(theme, ready))
    portal.start()
    try:
        if not ready.wait(timeout=10):
            raise RuntimeError("Could not start the appearance portal; run on a private D-Bus session")
        yield
    finally:
        portal.terminate()
        portal.join(timeout=5)
        if portal.is_alive():
            portal.kill()
            portal.join()


def capture(window, path, theme, width, height, previous):
    # Keep the pointer outside the resized window: GPUI 0.2.2 can re-enter its
    # X11 cursor-style RefCell and panic when a resize/capture happens under it.
    xdotool("mousemove", "1599", "999")
    xdotool("windowsize", window, str(width), str(height))
    stale_frame = None
    if previous is not None:
        stale_frame = run("convert", previous, "-crop", f"{width}x{height}+0+0", "-format", "%#", "info:")

    def rendered_frame():
        run("import", "-window", window, path)
        stats = run("identify", "-format", "%w %h %k %[fx:mean]", path)
        return matches_capture(stats, theme, width, height) and (
            stale_frame is None or run("identify", "-format", "%#", path) != stale_frame
        )

    try:
        time.sleep(0.4)
        wait_for(rendered_frame, f"nonblank {theme} capture at {width}x{height}")
    except Exception:
        path.unlink(missing_ok=True)
        raise


def matches_capture(stats, theme, width, height):
    w, h, colors, mean = stats.split()
    return (
        (int(w), int(h)) == (width, height)
        and int(colors) > 30
        and (float(mean) > 0.5) == (theme == "light")
    )


def duplicate_captures(digests):
    """Group captures with identical pixels, so two routes (or a page and a stale frame of another) cannot pass as distinct."""
    groups = {}
    for name, digest in digests.items():
        groups.setdefault(digest, []).append(name)
    return sorted(tuple(sorted(names)) for names in groups.values() if len(names) > 1)


def manifest(digests):
    captures = []
    for name in sorted(digests):
        page, theme, size = Path(name).stem.rsplit("-", 2)
        width, height = (int(side) for side in size.split("x"))
        captures.append({
            "file": name, "page": page, "theme": theme,
            "width": width, "height": height, "signature": digests[name],
        })
    return {"captures": captures}


def record(output, paths):
    """Write manifest.json before checking, so a failed run still leaves the evidence."""
    digests = {path.name: run("identify", "-format", "%#", path) for path in paths}
    (output / "manifest.json").write_text(json.dumps(manifest(digests), indent=2) + "\n")
    duplicates = duplicate_captures(digests)
    if duplicates:
        groups = "; ".join(", ".join(group) for group in duplicates)
        raise RuntimeError(f"Captures share identical pixels: {groups}")


def capture_sizes(window, output, name, theme, sizes=SIZES):
    paths = []
    previous = None
    for width, height in sizes:
        path = output / f"{name}-{theme}-{width}x{height}.png"
        capture(window, path, theme, width, height, previous)
        paths.append(path)
        previous = path
    return paths


def speaker_scenes():
    """(capture name, extra lf-desktop arguments) of each speaker-label scene."""
    return [(scene, ["--sandbox-fixture", scene]) for scene in SPEAKER_SURFACES]


def agent_scenes():
    """(capture name, extra lf-desktop arguments) of each Agents scene."""
    return [(scene, ["--sandbox-fixture", scene]) for scene in AGENT_SURFACES]


def launcher_scenes():
    """(capture name, extra lf-desktop arguments) of each agent launcher scene."""
    return [(scene, ["--sandbox-fixture", scene]) for scene in LAUNCHER_SURFACES]


def expected_captures():
    hub = (len(PAGES) + len(SPEAKER_SURFACES) + len(AGENT_SURFACES)) * len(SIZES) * 2
    return hub + len(LAUNCHER_SURFACES) * len(LAUNCHER_SIZES) * 2


def main():
    args = arguments(__doc__)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    paths = []
    with TemporaryDirectory(prefix="gpui-screenshots-", dir=output.parent) as scratch, window_manager():
        for theme in ("light", "dark"):
            with appearance(theme):
                for page in PAGES:
                    route = ["--setup"] if page == "setup" else ["--page", page]
                    with sandbox_window(args.binary, Path(scratch) / "sandbox", *route) as window:
                        paths += capture_sizes(window, output, page, theme)
                for name, route in speaker_scenes():
                    sandbox = Path(scratch) / "sandbox-speakers"
                    with sandbox_window(args.binary, sandbox, *route, scene=name) as window:
                        paths += capture_sizes(window, output, name, theme)
                for name, route in agent_scenes():
                    sandbox = Path(scratch) / "sandbox-agents"
                    with sandbox_window(args.binary, sandbox, *route, scene=name) as window:
                        paths += capture_sizes(window, output, name, theme)
                for name, route in launcher_scenes():
                    sandbox = Path(scratch) / "sandbox-agents"
                    with sandbox_window(
                        args.binary, sandbox, *route, scene=name, title=LAUNCHER_TITLE
                    ) as window:
                        paths += capture_sizes(window, output, name, theme, LAUNCHER_SIZES)
    record(output, paths)
    if len(paths) != expected_captures():
        raise RuntimeError(f"Gate 8 expected {expected_captures()} captures, made {len(paths)}")
    print(f"Gate 8 captured {len(paths)} distinct PNGs for review ({output}/manifest.json)")


if __name__ == "__main__":
    main()
