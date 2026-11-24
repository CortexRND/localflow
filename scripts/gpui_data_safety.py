"""Compare the real Linux LocalFlow stores around a sandbox session that writes every store."""

import hashlib
import hmac
import json
import os
import re
import sqlite3
import stat
import subprocess
from contextlib import closing
from pathlib import Path
from tempfile import TemporaryDirectory

from gpui_screenshots import (
    arguments,
    parse_environ,
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


def xdg_path(environ, variable, fallback):
    path = Path(environ.get(variable, ""))
    return path if path.is_absolute() else fallback


def real_store_paths(environ, home):
    """Every real location a sandbox session must leave untouched.

    ``config_dir`` is the whole LocalFlow config directory (``config.toml``,
    ``api-token``, ``launch/`` and anything added later), hashed recursively.
    """
    config_dir = xdg_path(environ, "XDG_CONFIG_HOME", home / ".config") / "localflow"
    return {
        "config_dir": config_dir,
        "config": Path(environ.get("LOCALFLOW_CONFIG") or config_dir / "config.toml"),
        "legacy_config": home / ".localflow.toml",
        "localflow_dir": home / ".localflow",
        "keyring": xdg_path(environ, "XDG_DATA_HOME", home / ".local/share") / "keyrings",
    }


def changed_stores(before, after):
    return [name for name in before if before[name] != after.get(name)]


COLLECTION_PREFIX = "/org/freedesktop/secrets/collection/"


def collection_keyring(keyrings, collection):
    """Map a gnome-keyring collection object path to its backing file.

    gnome-keyring names collections after their file (``login.keyring`` is
    ``.../collection/login``) and escapes other bytes in the path as ``_xx`` hex.
    """
    collection = str(collection)
    name = collection.removeprefix(COLLECTION_PREFIX)
    if name == collection or not name or "/" in name:
        return None
    raw = re.sub(rb"_([0-9a-fA-F]{2})", lambda match: bytes.fromhex(match[1].decode()), name.encode())
    filename = os.fsdecode(raw)
    if "/" in filename or filename in (".", ".."):
        return None
    return keyrings / f"{filename}.keyring"


def daemon_keyrings(environ, home):
    """Return the keyrings directory gnome-keyring-daemon uses for its environment."""
    data = Path(environ.get("XDG_DATA_HOME", ""))
    if not data.is_absolute():
        data = Path(environ.get("HOME") or home) / ".local/share"
    return data / "keyrings"


def legacy_keyrings(environ, home):
    """Return the pre-XDG keyrings directory gnome-keyring-daemon may pick at startup."""
    return Path(environ.get("HOME") or home) / ".gnome2/keyrings"


def monitored_stores():
    import dbus

    home = Path.home()
    if not home.is_absolute():
        raise RuntimeError("HOME must be an absolute path")
    stores = real_store_paths(os.environ, home)
    keyrings = stores["keyring"]
    bus = dbus.SessionBus()
    service = "org.freedesktop.secrets"
    secrets = bus.get_object(service, "/org/freedesktop/secrets")
    collection = secrets.ReadAlias("default", dbus_interface="org.freedesktop.Secret.Service")
    daemon = bus.get_object("org.freedesktop.DBus", "/org/freedesktop/DBus")
    pid = daemon.GetConnectionUnixProcessID(service, dbus_interface="org.freedesktop.DBus")
    if collection == "/" or Path(os.readlink(f"/proc/{pid}/exe")).name != "gnome-keyring-daemon":
        raise RuntimeError("An active, persistent GNOME Keyring default collection is required")
    label = bus.get_object(service, collection).Get(
        "org.freedesktop.Secret.Collection", "Label",
        dbus_interface="org.freedesktop.DBus.Properties",
    )
    backing = collection_keyring(keyrings, collection)
    if backing is None or not backing.is_file():
        raise RuntimeError(
            f"The default keyring collection {str(label)!r} ({collection}) is not backed "
            f"by a file in {keyrings}; Gate 2 cannot detect writes to it"
        )
    with open(f"/proc/{pid}/environ", "rb") as source:
        daemon_env = parse_environ(source.read())
    daemon_dir = daemon_keyrings(daemon_env, home)
    if daemon_dir.resolve() != keyrings.resolve():
        raise RuntimeError(
            f"gnome-keyring-daemon stores keyrings in {daemon_dir}, not the monitored {keyrings}"
        )
    legacy = legacy_keyrings(daemon_env, home)
    if legacy.exists():
        raise RuntimeError(
            f"Legacy keyrings directory {legacy} exists; gnome-keyring-daemon may use it "
            f"instead of {keyrings}, so Gate 2 cannot tell which keyring is live"
        )
    return stores


def exercise_dictionary(binary, sandbox):
    word = "LocalFlowCutoverGate"
    with sandbox_window(binary, sandbox, "--page", "dictionary") as window:
        xdotool("windowactivate", "--sync", window)
        xdotool("key", "--delay", "100", "ctrl+n")
        xdotool("type", word)
        xdotool("key", "Return")
        dictionary = sandbox / "localflow" / "dictionary.json"
        wait_for(lambda: word in dictionary.read_text(), "sandbox Dictionary write")


EXERCISED = ("settings", "models", "hot phrases", "history", "prompts")
EXERCISE_PHRASE = "cutover gate phrase"
NOT_DRIVEN = (
    "API token: lf-desktop never creates or rotates it; the whole real config "
    "directory (api-token, launch/) is still hashed"
)


def sandbox_state(sandbox):
    """Read the file-backed sandbox stores the exercise writes."""
    localflow = sandbox / "localflow"
    uri = f"{(localflow / 'history.sqlite').as_uri()}?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as db:
        history = db.execute("SELECT COUNT(*) FROM dictations").fetchone()[0]
    return {
        "config": (sandbox / "config" / "config.toml").read_bytes(),
        "hot_phrases": (localflow / "hot_phrases.json").read_text(),
        "history": history,
    }


def missing_writes(report, before, after):
    """Return the sandbox writes that did not demonstrably happen."""
    reported = {line.split(":", 1)[0] for line in report.splitlines()}
    missing = [store for store in EXERCISED if store not in reported]
    if after["config"] == before["config"]:
        missing.append("settings (sandbox config.toml unchanged)")
    if EXERCISE_PHRASE in before["hot_phrases"] or EXERCISE_PHRASE not in after["hot_phrases"]:
        missing.append("hot phrases (phrase not added to the sandbox store)")
    if after["history"] != before["history"] - 1:
        missing.append("history (no sandbox entry deleted)")
    return missing


def exercise_stores(binary, sandbox):
    """Write once to every other sandbox store with ``--sandbox-exercise``."""
    before = sandbox_state(sandbox)
    result = subprocess.run(
        [binary, "--sandbox", "--data-dir", sandbox, "--sandbox-exercise"],
        check=False, capture_output=True, text=True, timeout=60,
    )
    if result.returncode:
        raise RuntimeError(f"Sandbox exercise failed ({result.returncode}): {result.stderr.strip()}")
    missing = missing_writes(result.stdout, before, sandbox_state(sandbox))
    if missing:
        raise RuntimeError(f"Sandbox exercise did not write: {', '.join(missing)}")
    return result.stdout.splitlines()


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
        with TemporaryDirectory(prefix="gpui-data-safety-", dir=output.parent) as scratch:
            sandbox = Path(scratch) / "sandbox"
            with window_manager():
                exercise_dictionary(args.binary, sandbox)
            written = ["dictionary: added a word (keyboard, in the window)"]
            written += exercise_stores(args.binary, sandbox)
    finally:
        after = snapshot("after")
        changed = changed_stores(before, after)
        if changed:
            raise SystemExit(f"Gate 2 failed: changed stores: {', '.join(changed)}")
    print("Sandbox writes:", *written, sep="\n  ")
    print(f"Not driven:\n  {NOT_DRIVEN}")
    print(f"Gate 2 passed: real stores unchanged ({output})")


if __name__ == "__main__":
    main()
