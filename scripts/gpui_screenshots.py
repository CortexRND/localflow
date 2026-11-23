"""Capture the Linux GPUI sandbox window in both themes and at both gate sizes."""

import argparse
import multiprocessing
import os
import subprocess
import time
from contextlib import contextmanager
from pathlib import Path
from tempfile import TemporaryDirectory

PAGES = (
    "home", "history", "dictionary", "snippets", "style", "ask",
    "meetings", "prompts", "models", "settings", "setup",
)
SIZES = ((1280, 850), (960, 640))


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


@contextmanager
def sandbox_window(binary, sandbox, *route):
    env = os.environ.copy()
    env.pop("WAYLAND_DISPLAY", None)
    if not env.get("XDG_RUNTIME_DIR"):
        runtime = sandbox.parent / "runtime"
        runtime.mkdir(mode=0o700, exist_ok=True)
        env["XDG_RUNTIME_DIR"] = str(runtime)
    process = subprocess.Popen([binary, "--sandbox", "--data-dir", sandbox, *route], env=env)

    def mapped_window():
        if process.poll() is not None:
            raise RuntimeError(f"Sandbox exited before opening a window ({process.returncode})")
        try:
            windows = xdotool(
                "search", "--all", "--onlyvisible", "--pid", str(process.pid),
                "--name", r"^LocalFlow \(Sandbox\)$",
            )
        except subprocess.CalledProcessError:
            return None
        return windows.splitlines()[0] if windows else None

    try:
        window = wait_for(mapped_window, "the GPUI sandbox window")
        yield window
        xdotool("windowactivate", "--sync", window)
        xdotool("key", "ctrl+q")
        if process.wait(timeout=10):
            raise RuntimeError(f"Sandbox failed to close cleanly ({process.returncode})")
    finally:
        stop(process)


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


def main():
    args = arguments(__doc__)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    with TemporaryDirectory(prefix="gpui-screenshots-", dir=output.parent) as scratch, window_manager():
        for theme in ("light", "dark"):
            with appearance(theme):
                for page in PAGES:
                    route = ["--setup"] if page == "setup" else ["--page", page]
                    with sandbox_window(args.binary, Path(scratch) / "sandbox", *route) as window:
                        previous = None
                        for width, height in SIZES:
                            path = output / f"{page}-{theme}-{width}x{height}.png"
                            capture(window, path, theme, width, height, previous)
                            previous = path
    print(f"Gate 8 captured {len(PAGES) * len(SIZES) * 2} PNGs for review ({output})")


if __name__ == "__main__":
    main()
