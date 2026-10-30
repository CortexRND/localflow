"""Install a minimal .app bundle that launches the hot-phrases Tk window,
so it can be opened from Spotlight/Launchpad like a native app."""

import plistlib
import shlex
import shutil
import sys
from pathlib import Path

APP_NAME = "LocalFlow Hot Phrases.app"
BUNDLE_ID = "com.cortexrnd.localflow.hotphrases"

_PLIST = {
    "CFBundleName": "LocalFlow Hot Phrases",
    "CFBundleDisplayName": "LocalFlow Hot Phrases",
    "CFBundleIdentifier": BUNDLE_ID,
    "CFBundleExecutable": "launcher",
    "CFBundlePackageType": "APPL",
    "CFBundleShortVersionString": "0.1.0",
    "NSHighResolutionCapable": True,
}


def install_app(
    dest_dir: Path = Path("~/Applications").expanduser(),
    python: str = sys.executable,
    name: str = APP_NAME,
    bundle_id: str = BUNDLE_ID,
    module: str = "localflow.phrases_window",
) -> Path:
    """Write (idempotently) the .app bundle into dest_dir; return its path."""
    display_name = name.removesuffix(".app")
    bundle = Path(dest_dir) / name
    contents = bundle / "Contents"
    macos = contents / "MacOS"
    macos.mkdir(parents=True, exist_ok=True)

    plist = {
        **_PLIST,
        "CFBundleName": display_name,
        "CFBundleDisplayName": display_name,
        "CFBundleIdentifier": bundle_id,
    }
    with (contents / "Info.plist").open("wb") as f:
        plistlib.dump(plist, f)

    launcher = macos / "launcher"
    launcher.write_text(
        f"#!/bin/bash\nexec {shlex.quote(python)} -m {shlex.quote(module)} \"$@\"\n",
        encoding="utf-8",
    )
    launcher.chmod(0o755)
    return bundle


def uninstall_app(
    dest_dir: Path = Path("~/Applications").expanduser(),
    name: str = APP_NAME,
) -> bool:
    """Remove the bundle. Returns False when it wasn't installed."""
    bundle = Path(dest_dir) / name
    if not bundle.exists():
        return False
    shutil.rmtree(bundle)
    return True
