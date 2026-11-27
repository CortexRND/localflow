"""Install a minimal .app bundle that launches a LocalFlow module."""

import plistlib
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib
from importlib import metadata
from pathlib import Path

APP_NAME = "LocalFlow Hot Phrases.app"
MAIN_APP_NAME = "LocalFlow.app"
MAIN_BUNDLE_ID = "com.cortexrnd.localflow.app"
MAIN_MODULE = "localflow.main_app"
BUNDLE_ID = "com.cortexrnd.localflow.hotphrases"

_PLIST = {
    "CFBundleName": "LocalFlow Hot Phrases",
    "CFBundleDisplayName": "LocalFlow Hot Phrases",
    "CFBundleIdentifier": BUNDLE_ID,
    "CFBundleExecutable": "launcher",
    "CFBundlePackageType": "APPL",
    "CFBundleShortVersionString": "0.2.4",
    "NSHighResolutionCapable": True,
}


def _version() -> str:
    try:
        return metadata.version("localflow")
    except metadata.PackageNotFoundError:
        return "0.2.4"


def _png(size: int) -> bytes:
    """Render a rounded blue square with a white microphone as PNG bytes."""
    half = size / 2
    radius = size * 0.22
    rows = bytearray()
    for y in range(size):
        rows.append(0)
        for x in range(size):
            dx, dy = abs(x + 0.5 - half), abs(y + 0.5 - half)
            inner = half - radius
            corner = max(dx - inner, 0) ** 2 + max(dy - inner, 0) ** 2
            if corner > radius**2:
                rows += b"\x00\x00\x00\x00"
                continue
            u, v = (x + 0.5) / size - 0.5, (y + 0.5) / size - 0.5
            capsule = abs(u) < 0.11 and (
                -0.26 < v < 0.06
                or (abs(u) ** 2 + (v + 0.26) ** 2) < 0.11**2
                or (abs(u) ** 2 + (v - 0.06) ** 2) < 0.11**2
            )
            ring = (u * u + (v - 0.06) ** 2) ** 0.5
            holder = (
                (0.18 < ring < 0.21 and v > 0.06)
                or (abs(u) < 0.015 and 0.25 < v < 0.32)
                or (abs(u) < 0.1 and 0.31 < v < 0.34)
            )
            if capsule or holder:
                rows += b"\xff\xff\xff\xff"
            else:
                shade = int(70 + 90 * (y / size))
                rows += bytes((30, shade, 220, 255))

    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(rows), 9))
        + chunk(b"IEND", b"")
    )


def _make_icns(target: Path) -> bool:
    """Create an .icns with iconutil. macOS only; returns False when skipped."""
    if sys.platform != "darwin" or shutil.which("iconutil") is None:
        return False
    try:
        with tempfile.TemporaryDirectory() as tmp:
            iconset = Path(tmp) / "LocalFlow.iconset"
            iconset.mkdir()
            for base in (16, 32, 128, 256, 512):
                (iconset / f"icon_{base}x{base}.png").write_bytes(_png(base))
                (iconset / f"icon_{base}x{base}@2x.png").write_bytes(_png(base * 2))
            result = subprocess.run(
                ["iconutil", "-c", "icns", str(iconset), "-o", str(target)],
                capture_output=True,
                check=False,
            )
            return result.returncode == 0 and target.exists()
    except (OSError, subprocess.SubprocessError):
        return False


def install_app(
    dest_dir: Path = Path("~/Applications").expanduser(),
    python: str = sys.executable,
    name: str = APP_NAME,
    bundle_id: str = BUNDLE_ID,
    module: str = "localflow.phrases_window",
    *,
    icon: bool = False,
    full_plist: bool = False,
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
    if full_plist:
        version = _version()
        plist["CFBundleVersion"] = version
        plist["CFBundleShortVersionString"] = version
        plist["LSUIElement"] = False
    if icon:
        resources = contents / "Resources"
        resources.mkdir(parents=True, exist_ok=True)
        if _make_icns(resources / "LocalFlow.icns"):
            plist["CFBundleIconFile"] = "LocalFlow"
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
