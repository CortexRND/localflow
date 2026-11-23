"""Compare the real Linux LocalFlow stores around a sandbox Dictionary edit."""

import hashlib
import hmac
import json
import os
import re
import stat
from pathlib import Path
from tempfile import TemporaryDirectory

from gpui_screenshots import (
    arguments,
    sandbox_window,
    wait_for,
    window_manager,
    xdotool,
)


def fingerprint(path, key):
    """Hash names, entry types, permissions and bytes without exporting them."""
    digest = hmac.new(key, digestmod="sha256")

    def record(value):
        digest.update(len(value).to_bytes(8, "big"))
        digest.update(value)

    def visit(entry, ancestors):
        record(os.fsencode(entry.name))
        try:
            mode = entry.lstat().st_mode
        except FileNotFoundError:
            record(b"missing")
            return
        record(str(mode).encode())
        if stat.S_ISLNK(mode):
            target = entry.resolve(strict=False)
            record(os.fsencode(os.readlink(entry)))
            visit(target, ancestors)
        elif stat.S_ISREG(mode):
            file_hash = hashlib.sha256()
            with entry.open("rb") as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b""):
                    file_hash.update(chunk)
            record(file_hash.digest())
        elif stat.S_ISDIR(mode):
            canonical = entry.resolve()
            if canonical in ancestors:
                raise ValueError("A monitored tree contains a directory cycle")
            children = sorted(entry.iterdir())
            record(str(len(children)).encode())
            for child in children:
                visit(child, ancestors | {canonical})

    visit(path, set())
    return digest.hexdigest()


def xdg_path(variable, fallback, environ=os.environ):
    path = Path(environ.get(variable, ""))
    return path if path.is_absolute() else fallback


def keyring_directory(environ):
    """Mirror gnome-keyring's gkm_util_locate_keyrings_directory for a daemon environment."""
    home = Path(environ.get("HOME") or Path.home())
    current = xdg_path("XDG_DATA_HOME", home / ".local/share", environ) / "keyrings"
    legacy = home / ".gnome2" / "keyrings"
    return legacy if not current.is_dir() and legacy.is_dir() else current


def collection_file(collection, directory):
    """Map a gnome-keyring collection object path to its backing .keyring file."""
    prefix = "/org/freedesktop/secrets/collection/"
    if not collection.startswith(prefix) or "/" in collection[len(prefix):]:
        return None
    identifier = re.sub(r"_([0-9a-f]{2})", lambda m: chr(int(m[1], 16)), collection[len(prefix):])
    return directory / f"{identifier}.keyring"


def process_environment(pid):
    entries = Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
    return dict(os.fsdecode(entry).split("=", 1) for entry in entries if b"=" in entry)


def monitored_stores():
    import dbus

    home = Path.home()
    if not home.is_absolute():
        raise RuntimeError("HOME must be an absolute path")
    config = xdg_path("XDG_CONFIG_HOME", home / ".config") / "localflow" / "config.toml"
    bus = dbus.SessionBus()
    service = "org.freedesktop.secrets"
    secrets = bus.get_object(service, "/org/freedesktop/secrets")
    collection = secrets.ReadAlias("default", dbus_interface="org.freedesktop.Secret.Service")
    daemon = bus.get_object("org.freedesktop.DBus", "/org/freedesktop/DBus")
    pid = daemon.GetConnectionUnixProcessID(service, dbus_interface="org.freedesktop.DBus")
    if Path(os.readlink(f"/proc/{pid}/exe")).name != "gnome-keyring-daemon":
        raise RuntimeError("An active, persistent GNOME Keyring default collection is required")
    keyrings = keyring_directory(process_environment(pid))
    backing = collection_file(str(collection), keyrings)
    if backing is None or not backing.is_file():
        raise RuntimeError("An active, persistent GNOME Keyring default collection is required")
    return {
        "config": Path(os.environ.get("LOCALFLOW_CONFIG") or config),
        "legacy_config": home / ".localflow.toml",
        "localflow_dir": home / ".localflow",
        "keyring": keyrings,
    }


def exercise_dictionary(binary, sandbox):
    word = "LocalFlowCutoverGate"
    with sandbox_window(binary, sandbox, "--page", "dictionary") as window:
        xdotool("windowactivate", "--sync", window)
        xdotool("key", "--delay", "100", "ctrl+n")
        xdotool("type", word)
        xdotool("key", "Return")
        dictionary = sandbox / "localflow" / "dictionary.json"
        wait_for(lambda: word in dictionary.read_text(), "sandbox Dictionary write")


def main():
    args = arguments(__doc__)
    stores = monitored_stores()
    output = args.output.resolve()
    for name, path in stores.items():
        if output.is_relative_to(path.resolve()):
            raise ValueError(f"The artifact directory must be outside the {name} store")
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    key = os.urandom(32)

    def snapshot(name):
        state = {label: fingerprint(path, key) for label, path in stores.items()}
        (output / f"{name}.json").write_text(json.dumps(state, indent=2) + "\n")
        return state

    before = snapshot("before")
    try:
        with TemporaryDirectory(prefix="gpui-data-safety-", dir=output.parent) as scratch, window_manager():
            exercise_dictionary(args.binary, Path(scratch) / "sandbox")
    finally:
        after = snapshot("after")
        changed = [name for name in before if before[name] != after[name]]
        if changed:
            raise SystemExit(f"Gate 2 failed: changed stores: {', '.join(changed)}")
    print(f"Gate 2 passed: sandbox Dictionary edited; real stores unchanged ({output})")


if __name__ == "__main__":
    main()
