"""Build the macOS installer bundle and validate its contents."""

from __future__ import annotations

import json
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
        script = archive.getinfo(f"{APP}/Contents/Resources/install-localflow.sh")
        assert script.external_attr >> 16 & stat.S_IXUSR
        same_launcher = archive.read(launcher) == archive.read(script)
        plist = plistlib.loads(archive.read(f"{APP}/Contents/Info.plist"))
        bundled = archive.read(f"{APP}/Contents/Resources/install.sh")
        readme = archive.read("Read Me.txt").decode()

    assert "Read Me.txt" in names
    # Off macOS no native launcher can be compiled: the script is the executable.
    assert same_launcher
    assert plist["CFBundleExecutable"] == "install-localflow"
    assert plist["CFBundlePackageType"] == "APPL"
    assert plist["CFBundleIdentifier"]
    assert plist["LSArchitecturePriority"] == ["arm64", "x86_64"]
    assert bundled == (ROOT / "install.sh").read_bytes()
    assert "System Settings > Privacy & Security" in readme
    assert "Open Anyway" in readme
    assert "macOS 15 and later, right-click > Open no longer" in readme
    assert "choose Open, then" not in readme  # the removed pre-15 bypass


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


@pytest.mark.parametrize("in_git", [False, True])
def test_default_build_pins_no_ref_so_the_release_is_verified(
    tmp_path: Path, in_git: bool
) -> None:
    if in_git:
        git = shutil.which("git")
        if git is None:
            pytest.skip("git is not installed")
        project = tmp_path / "project"
        project.mkdir()
        subprocess.run([git, "init", "-q", str(project)], check=True)
    project = _build(tmp_path, None)
    launcher = project / "dist" / APP / "Contents" / "MacOS" / "install-localflow"

    text = launcher.read_text(encoding="utf-8")
    assert 'PINNED_REF=""' in text
    assert 'PINNED_REF="main"' not in text


def _run_launcher(tmp_path: Path, launcher: Path, env: dict[str, str] | None = None) -> str:
    """Run a built launcher with `open` stubbed; return the .command it wrote."""
    stubs = tmp_path / "run-stubs"
    stubs.mkdir()
    (stubs / "open").write_text(f'#!/bin/sh\ncp "$3" "{tmp_path}/run.command"\n')
    (stubs / "open").chmod(0o755)
    tools = tmp_path / "run-tools"
    tools.mkdir()
    for tool in ("dirname", "mktemp", "chmod", "cat", "rm", "mkdir", "cp"):
        (tools / tool).symlink_to(shutil.which(tool))
    result = subprocess.run(
        [str(launcher)],
        env={"PATH": f"{stubs}:{tools}", "TMPDIR": str(tmp_path), **(env or {})},
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    command = tmp_path / "run.command"
    subprocess.run([BASH, "-n", str(command)], check=True)
    return command.read_text(encoding="utf-8")


def test_default_launcher_unsets_ref_and_custom_builds_keep_theirs(tmp_path: Path) -> None:
    default = _build(tmp_path / "default", None)
    launcher = default / "dist" / APP / "Contents" / "MacOS" / "install-localflow"
    captured = _run_launcher(tmp_path / "default", launcher)
    assert "unset LOCALFLOW_REF" in captured
    assert "export LOCALFLOW_REF" not in captured

    custom = _build(tmp_path / "custom", "feature/x-1.2")
    launcher = custom / "dist" / APP / "Contents" / "MacOS" / "install-localflow"
    captured = _run_launcher(tmp_path / "custom", launcher)
    assert "export LOCALFLOW_REF=feature/x-1.2" in captured
    assert "unset LOCALFLOW_REF" not in captured


DARWIN_TOOL = r"""
import json, os, shutil, sys
from pathlib import Path
name, args = Path(sys.argv[0]).name, sys.argv[1:]
with open(os.environ["BUILD_CALLS"], "a", encoding="utf-8") as log:
    log.write(json.dumps([name, *args]) + "\n")
if name == "uname":
    print("Darwin")
elif name == "clang":
    shutil.copy(args[-1], os.environ["BUILD_CALLS"] + ".c")
    Path(args[args.index("-o") + 1]).write_text("native launcher fixture")
elif name == "lipo":
    print(os.environ["LIPO_ARCHS"])
elif name == "hdiutil":
    staging = Path(args[args.index("-srcfolder") + 1])
    shutil.copy(staging / "Read Me.txt", os.environ["BUILD_CALLS"] + ".readme")
    Path(args[-1]).write_text("dmg fixture")
"""


def _darwin_build(
    tmp_path: Path, lipo_archs: str = "x86_64 arm64", clang: bool = True
) -> tuple[Path, subprocess.CompletedProcess[str], Path]:
    """Run the builder as on macOS, with stubbed Apple tools."""
    project = tmp_path / "project"
    (project / "scripts").mkdir(parents=True)
    shutil.copy(BUILD_SH, project / "scripts" / "build_mac_installer.sh")
    shutil.copy(ROOT / "install.sh", project / "install.sh")
    stubs = tmp_path / "stubs"
    stubs.mkdir()
    for tool in ("uname", "lipo", "codesign", "hdiutil", *(("clang",) if clang else ())):
        (stubs / tool).write_text(f"#!{sys.executable}\n" + DARWIN_TOOL)
        (stubs / tool).chmod(0o755)
    tools = tmp_path / "tools"
    tools.mkdir()
    for tool in ("dirname", "mktemp", "chmod", "cat", "rm", "mkdir", "cp"):
        (tools / tool).symlink_to(shutil.which(tool))
    calls = tmp_path / "calls"
    result = subprocess.run(
        [BASH, str(project / "scripts" / "build_mac_installer.sh")],
        env={
            "PATH": f"{stubs}:{tools}",
            "TMPDIR": str(tmp_path),
            "BUILD_CALLS": str(calls),
            "LIPO_ARCHS": lipo_archs,
        },
        capture_output=True,
        text=True,
        check=False,
    )
    return project, result, calls


def test_macos_build_uses_a_universal_native_launcher(tmp_path: Path) -> None:
    project, result, calls = _darwin_build(tmp_path)

    assert result.returncode == 0, result.stderr
    app = project / "dist" / APP
    logged = [json.loads(line) for line in calls.read_text().splitlines()]
    clang = next(call for call in logged if call[0] == "clang")
    assert clang[clang.index("-o") + 1] == str(app / "Contents/MacOS/install-localflow")
    arches = [clang[i + 1] for i, arg in enumerate(clang) if arg == "-arch"]
    assert sorted(arches) == ["arm64", "x86_64"]
    assert ["lipo", "-archs", str(app / "Contents/MacOS/install-localflow")] in logged
    source = Path(str(calls) + ".c").read_text(encoding="utf-8")
    assert '"/bin/bash"' in source
    assert "../Resources/install-localflow.sh" in source
    executable = app / "Contents" / "MacOS" / "install-localflow"
    assert executable.read_text() == "native launcher fixture"
    assert os.access(executable, os.X_OK)
    script = app / "Contents" / "Resources" / "install-localflow.sh"
    assert os.access(script, os.X_OK)
    subprocess.run([BASH, "-n", str(script)], check=True)
    assert BANNER in script.read_text(encoding="utf-8")
    plist = plistlib.loads((app / "Contents" / "Info.plist").read_bytes())
    assert plist["LSArchitecturePriority"] == ["arm64", "x86_64"]
    signed = [call for call in logged if call[0] == "codesign"]
    assert signed and signed[-1][-1] == str(app)
    assert logged.index(clang) < logged.index(signed[-1])
    assert (project / "dist" / "LocalFlow-Installer.dmg").read_text() == "dmg fixture"
    assert "Open Anyway" in Path(str(calls) + ".readme").read_text(encoding="utf-8")
    assert not (project / "dist" / "LocalFlow-Installer-mac.zip").exists()


@pytest.mark.parametrize("archs", ["x86_64", "arm64", ""])
def test_macos_build_fails_when_launcher_is_not_universal(tmp_path: Path, archs: str) -> None:
    project, result, calls = _darwin_build(tmp_path, lipo_archs=archs)

    assert result.returncode != 0
    assert "not universal" in result.stderr
    assert not (project / "dist" / "LocalFlow-Installer.dmg").exists()
    assert not any(json.loads(line)[0] == "hdiutil" for line in calls.read_text().splitlines())


def test_macos_build_requires_clang_instead_of_shipping_a_script_executable(
    tmp_path: Path,
) -> None:
    project, result, _calls = _darwin_build(tmp_path, clang=False)

    assert result.returncode != 0
    assert "clang is required" in result.stderr
    assert not (project / "dist" / APP / "Contents" / "MacOS" / "install-localflow").exists()
    assert not (project / "dist" / "LocalFlow-Installer.dmg").exists()


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
