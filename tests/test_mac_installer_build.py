"""Build the macOS installer bundle and validate its contents."""

from __future__ import annotations

import os
import plistlib
import shutil
import stat
import subprocess
import sys
import zipfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
BUILD_SH = ROOT / "scripts" / "build_mac_installer.sh"
BASH = shutil.which("bash") if os.name != "nt" else None

pytestmark = pytest.mark.skipif(
    BASH is None or sys.platform == "darwin",
    reason="needs bash; macOS builds a DMG instead of the zip",
)

APP = "Install LocalFlow.app"
BANNER = "Installing LocalFlow — this takes a few minutes; LocalFlow will open when done."
BANNER_HINT = "Afterwards, find it in Applications → LocalFlow."


def _build(tmp_path: Path, ref: str | None) -> Path:
    project = tmp_path / "project"
    (project / "scripts").mkdir(parents=True)
    shutil.copy(BUILD_SH, project / "scripts" / "build_mac_installer.sh")
    shutil.copy(ROOT / "install.sh", project / "install.sh")
    env = {k: v for k, v in os.environ.items() if k != "LOCALFLOW_REF"}
    if ref is not None:
        env["LOCALFLOW_REF"] = ref
    subprocess.run(
        [BASH, str(project / "scripts" / "build_mac_installer.sh")],
        check=True,
        capture_output=True,
        env=env,
    )
    return project


def _extract(zip_path: Path, target: Path) -> None:
    with zipfile.ZipFile(zip_path) as archive:
        for info in archive.infolist():
            archive.extract(info, target)
            mode = info.external_attr >> 16
            if mode:
                (target / info.filename).chmod(mode)


def test_build_creates_zip_with_valid_bundle(tmp_path: Path) -> None:
    project = _build(tmp_path, "v1.2.3")
    zip_path = project / "dist" / "LocalFlow-Installer-mac.zip"

    with zipfile.ZipFile(zip_path) as archive:
        names = archive.namelist()
        launcher = archive.getinfo(f"{APP}/Contents/MacOS/install-localflow")
        assert launcher.external_attr >> 16 & stat.S_IXUSR
        plist = plistlib.loads(archive.read(f"{APP}/Contents/Info.plist"))
        bundled = archive.read(f"{APP}/Contents/Resources/install.sh")
        readme = archive.read("Read Me.txt").decode()

    assert "Read Me.txt" in names
    assert plist["CFBundleExecutable"] == "install-localflow"
    assert plist["CFBundlePackageType"] == "APPL"
    assert plist["CFBundleIdentifier"]
    assert bundled == (ROOT / "install.sh").read_bytes()
    assert "Open Anyway" in readme
    assert "Privacy & Security" in readme
    assert "Right-click" in readme


def test_launcher_is_valid_bash_with_pinned_ref(tmp_path: Path) -> None:
    project = _build(tmp_path, "v1.2.3")
    launcher = project / "dist" / APP / "Contents" / "MacOS" / "install-localflow"

    subprocess.run([BASH, "-n", str(launcher)], check=True)
    text = launcher.read_text(encoding="utf-8")
    assert os.access(launcher, os.X_OK)
    assert text.startswith("#!/bin/bash")
    assert 'PINNED_REF="v1.2.3"' in text
    assert BANNER in text


def test_icon_is_omitted_off_macos_and_script_reuses_appbundle_generator(
    tmp_path: Path,
) -> None:
    project = _build(tmp_path, "v1.2.3")
    app = project / "dist" / APP
    plist = plistlib.loads((app / "Contents" / "Info.plist").read_bytes())

    assert "CFBundleIconFile" not in plist
    assert not list((app / "Contents" / "Resources").glob("*.icns"))
    script = BUILD_SH.read_text(encoding="utf-8")
    assert "_make_icns" in script
    assert "CFBundleIconFile" in script


def test_default_ref_is_main_outside_git(tmp_path: Path) -> None:
    project = _build(tmp_path, None)
    launcher = project / "dist" / APP / "Contents" / "MacOS" / "install-localflow"

    assert 'PINNED_REF="main"' in launcher.read_text(encoding="utf-8")


def test_build_rejects_unsafe_ref(tmp_path: Path) -> None:
    project = tmp_path / "project"
    (project / "scripts").mkdir(parents=True)
    shutil.copy(BUILD_SH, project / "scripts" / "build_mac_installer.sh")
    shutil.copy(ROOT / "install.sh", project / "install.sh")

    result = subprocess.run(
        [BASH, str(project / "scripts" / "build_mac_installer.sh")],
        env={**os.environ, "LOCALFLOW_REF": 'x"; rm -rf ~'},
        capture_output=True,
        check=False,
    )

    assert result.returncode != 0
    assert not (project / "dist").exists()


def test_launcher_generates_command_in_temp_dir_from_readonly_spaced_path(
    tmp_path: Path,
) -> None:
    project = _build(tmp_path, "v1.2.3")
    unpacked = tmp_path / "read only dir"
    _extract(project / "dist" / "LocalFlow-Installer-mac.zip", unpacked)
    launcher = unpacked / APP / "Contents" / "MacOS" / "install-localflow"
    assert os.access(launcher, os.X_OK)

    stubs = tmp_path / "stubs"
    stubs.mkdir()
    opened = tmp_path / "opened.txt"
    (stubs / "open").write_text(
        f'#!/bin/sh\nprintf "%s\\n" "$@" >"{opened}"\ncp "$3" "{tmp_path}/captured.command"\n'
    )
    (stubs / "open").chmod(0o755)
    tools = tmp_path / "tools"
    tools.mkdir()
    for tool in ("dirname", "mktemp", "chmod", "cat", "rm", "mkdir", "cp"):
        found = shutil.which(tool)
        assert found
        (tools / tool).symlink_to(found)
    scratch = tmp_path / "scratch"
    scratch.mkdir()
    for path in unpacked.rglob("*"):
        path.chmod(path.stat().st_mode & ~0o222)

    try:
        result = subprocess.run(
            [str(launcher)],
            env={"PATH": f"{stubs}:{tools}", "TMPDIR": str(scratch)},
            capture_output=True,
            text=True,
            check=False,
        )
    finally:
        for path in [unpacked, *unpacked.rglob("*")]:
            path.chmod(path.stat().st_mode | 0o200)

    assert result.returncode == 0, result.stderr
    args = opened.read_text().splitlines()
    assert args[0] == "-a" and args[1] == "Terminal"
    command_path = Path(args[2])
    assert command_path.name == "Install LocalFlow.command"
    assert command_path.parent.parent == scratch
    captured = (tmp_path / "captured.command").read_text(encoding="utf-8")
    subprocess.run([BASH, "-n", str(tmp_path / "captured.command")], check=True)
    bundled = unpacked / APP / "Contents" / "Resources" / "install.sh"
    assert bundled.read_bytes() == (ROOT / "install.sh").read_bytes()
    assert "export LOCALFLOW_REF=v1.2.3" in captured
    assert "read\\ only\\ dir" in captured  # path with spaces is quoted
    assert BANNER in captured
    assert BANNER_HINT in captured
    assert 'bash "$INSTALLER"' in captured
