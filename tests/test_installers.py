from __future__ import annotations

import os
import re
import shutil
import subprocess
import tomllib
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
INSTALL_SH = ROOT / "install.sh"


def _find_bash() -> str | None:
    if os.name != "nt":
        return shutil.which("bash")

    git_executable = shutil.which("git")
    if git_executable is None:
        return None
    for git_directory in Path(git_executable).resolve().parents:
        for relative_path in (("bin", "bash.exe"), ("usr", "bin", "bash.exe")):
            candidate = git_directory.joinpath(*relative_path)
            if candidate.is_file():
                return str(candidate)
    return None


BASH = _find_bash()


@pytest.mark.skipif(BASH is None, reason="Bash is not installed")
def test_install_sh_passes_bash_syntax_check() -> None:
    subprocess.run([BASH, "-n", INSTALL_SH.name], check=True, cwd=ROOT)


@pytest.mark.skipif(BASH is None, reason="Bash is not installed")
def test_install_sh_help_lists_supported_options() -> None:
    result = subprocess.run(
        [BASH, INSTALL_SH.name, "--help"],
        check=True,
        capture_output=True,
        text=True,
        cwd=ROOT,
    )

    assert "--with-ollama" in result.stdout
    assert "--no-launch" in result.stdout
    assert "--no-system-deps" in result.stdout


@pytest.mark.skipif(BASH is None, reason="Bash is not installed")
def test_install_sh_unknown_option_exits_two() -> None:
    result = subprocess.run(
        [BASH, INSTALL_SH.name, "--not-an-installer-option"],
        capture_output=True,
        text=True,
        cwd=ROOT,
    )

    assert result.returncode == 2


def test_install_ps1_exposes_piped_install_options() -> None:
    script = (ROOT / "install.ps1").read_text(encoding="utf-8")

    assert "param(" in script
    assert "WithOllama" in script
    assert "NoLaunch" in script
    assert "LOCALFLOW_REF" in script


def test_install_ps1_does_not_call_exit() -> None:
    script = (ROOT / "install.ps1").read_text(encoding="utf-8")

    assert re.search(r"^\s*exit\b", script, re.MULTILINE) is None


def test_install_ps1_rejects_non_localflow_install_destination() -> None:
    script = (ROOT / "install.ps1").read_text(encoding="utf-8")

    assert "is not a LocalFlow install" in script


def test_macos_permissions_extra_has_no_iokit_package() -> None:
    extras = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))[
        "project"
    ]["optional-dependencies"]["macos-permissions"]

    assert not any("iokit" in requirement.lower() for requirement in extras)
    assert any(r.startswith("pyobjc-framework-AVFoundation") for r in extras)
    assert any(r.startswith("pyobjc-framework-ApplicationServices") for r in extras)


def test_windows_cmd_installer_downloads_and_runs_install_ps1() -> None:
    raw = (ROOT / "packaging" / "windows" / "Install LocalFlow.cmd").read_bytes()
    script = raw.decode("ascii")

    assert b"\r\n" in raw and b"\n" not in raw.replace(b"\r\n", b"")
    assert "powershell -NoProfile -ExecutionPolicy Bypass" in script
    assert "install.ps1" in script
    assert "pause" in script
    assert "exit /b 1" in script


def test_mac_installer_workflow_builds_validates_and_publishes() -> None:
    workflow = (ROOT / ".github" / "workflows" / "mac-installer.yml").read_text(
        encoding="utf-8"
    )

    assert "runs-on: macos-latest" in workflow
    assert "macos-14" not in workflow
    assert "contents: write" in workflow
    assert "plutil -lint" in workflow
    assert "hdiutil verify" in workflow
    assert "test -x" in workflow
    assert "LocalFlow-Installer.dmg" in workflow
    assert "Install LocalFlow.cmd" in workflow
    # Published under the dotted name GitHub gives uploaded assets with spaces.
    assert "dist/release/Install.LocalFlow.cmd" in workflow
    assert "'v*'" in workflow
    assert "gh release upload" in workflow
    assert "secrets.GITHUB_TOKEN" in workflow


def test_mit_license_is_declared_in_project_metadata() -> None:
    license_text = (ROOT / "LICENSE").read_text(encoding="utf-8")
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))[
        "project"
    ]

    assert license_text.startswith("MIT License")
    assert project["license"] == "MIT"
    assert project["license-files"] == ["LICENSE"]
