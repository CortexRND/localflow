"""Adapter: packaged artifact sizes (``gpui-package.yml`` and installers).

Walks a directory of downloaded artifacts (``LocalFlow-Preview-<version>-
<platform>.tar.gz`` / ``.zip`` today; ``.dmg``, ``.AppImage``, ``.deb``,
``.rpm``, ``.exe``, ``.msi``, ``.pkg`` from the installer workflows) and
emits one case per artifact:

* ``package_size_bytes``: size of the file as downloaded.
* ``installed_size_bytes``: for ``.tar.gz``/``.tgz``/``.zip``, the sum of
  the uncompressed member sizes (what unpacking writes to disk). Archives are
  only listed, never extracted.

The version is stripped from the case name (``package/LocalFlow-Preview-
linux-x86_64.tar.gz``) so baseline and candidate builds line up; the full
file name, version and ``.sha256`` sidecar digest go into ``evidence``.

Stdlib only.
"""

from __future__ import annotations

import re
import tarfile
import zipfile
from pathlib import Path

from ..schema import make_case
from ._common import SUITE, bytes_metric, failed, read_text

#: Longest first so ``.tar.gz`` wins over ``.gz``.
PACKAGE_SUFFIXES = (
    ".tar.gz",
    ".tgz",
    ".AppImage",
    ".dmg",
    ".deb",
    ".rpm",
    ".exe",
    ".msi",
    ".pkg",
    ".zip",
)
_VERSION = re.compile(r"[-_]v?\d+\.\d+(?:\.\d+)*(?:[-+.~][0-9A-Za-z]+)*?(?=[-_.](?:[A-Za-z]|$))")


def package_suffix(name: str) -> str | None:
    lower = name.lower()
    for suffix in PACKAGE_SUFFIXES:
        if lower.endswith(suffix.lower()):
            return suffix
    return None


def split_version(name: str) -> tuple[str, str | None]:
    """``(name without version, version)`` for ``LocalFlow-Preview-1.2.3-linux.tar.gz``."""
    suffix = package_suffix(name) or ""
    stem = name[: len(name) - len(suffix)] if suffix else name
    match = _VERSION.search(stem + ".")
    if not match:
        return name, None
    version = match.group(0).lstrip("-_").lstrip("v")
    stripped = stem[: match.start()] + stem[match.end():]
    return stripped + suffix, version


def installed_size(path: Path) -> int | None:
    """Uncompressed size of a ``.tar.gz``/``.tgz``/``.zip``; ``None`` otherwise."""
    suffix = package_suffix(path.name)
    if suffix in (".tar.gz", ".tgz"):
        with tarfile.open(path, "r:gz") as archive:
            return sum(member.size for member in archive.getmembers() if member.isfile())
    if suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            return sum(info.file_size for info in archive.infolist() if not info.is_dir())
    return None


def _sidecar_digest(path: Path) -> str | None:
    sidecar = path.with_name(path.name + ".sha256")
    if not sidecar.is_file():
        return None
    try:
        token = read_text(sidecar).split()
    except OSError:
        return None
    return token[0].lower() if token and re.fullmatch(r"[0-9a-fA-F]{64}", token[0]) else None


def find_packages(directory: Path) -> list[Path]:
    return sorted(
        p for p in Path(directory).rglob("*") if p.is_file() and package_suffix(p.name) is not None
    )


def measure_package(path: Path, root: Path | None = None) -> dict:
    """One case for one artifact file."""
    path = Path(path)
    plain, version = split_version(path.name)
    rel_parent = ""
    if root is not None:
        parent = path.parent.relative_to(root).as_posix()
        rel_parent = "" if parent == "." else parent + "/"
    case = f"package/{rel_parent}{plain}"
    suffix = package_suffix(path.name) or path.suffix
    dims = {"component": "package", "artifact": plain, "format": suffix.lstrip(".").lower()}
    evidence: dict = {"file": path.name}
    if version:
        evidence["version"] = version
    digest = _sidecar_digest(path)
    if digest:
        evidence["sha256"] = digest
    try:
        metrics = {"package_size_bytes": bytes_metric(path.stat().st_size)}
    except OSError as exc:
        return failed(case, dims, f"cannot stat {path.name}: {exc}")
    try:
        unpacked = installed_size(path)
    except (OSError, tarfile.TarError, zipfile.BadZipFile, EOFError) as exc:
        return failed(case, dims, f"cannot list archive {path.name}: {type(exc).__name__}: {exc}")
    if unpacked is not None:
        metrics["installed_size_bytes"] = bytes_metric(unpacked)
    return make_case(suite=SUITE, case=case, dims=dims, metrics=metrics, evidence=evidence)


def measure_packages(directory: Path | str) -> list[dict]:
    """One case per package artifact under ``directory`` (recursive)."""
    directory = Path(directory)
    if not directory.is_dir():
        return [failed("package/*", {"component": "package"}, f"package directory not found: {directory.name}")]
    paths = find_packages(directory)
    if not paths:
        return [
            failed(
                "package/*",
                {"component": "package"},
                f"no package artifacts ({', '.join(PACKAGE_SUFFIXES)}) found",
                status="not_exercised",
            )
        ]
    cases: list[dict] = []
    seen: set[str] = set()
    for path in paths:
        case = measure_package(path, directory)
        name, n = case["case"], 2
        while case["case"] in seen:
            case["case"] = f"{name}#{n}"
            n += 1
        seen.add(case["case"])
        cases.append(case)
    return cases
