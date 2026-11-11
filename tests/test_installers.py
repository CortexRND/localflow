from __future__ import annotations

import re
import shutil
import subprocess
import tomllib
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
INSTALL_SH = ROOT / "install.sh"


@pytest.mark.skipif(shutil.which("bash") is None, reason="bash is not installed")
def test_install_sh_passes_bash_syntax_check() -> None:
    subprocess.run(["bash", "-n", INSTALL_SH.name], check=True, cwd=ROOT)


@pytest.mark.skipif(shutil.which("bash") is None, reason="bash is not installed")
def test_install_sh_help_lists_supported_options() -> None:
    result = subprocess.run(
        ["bash", INSTALL_SH.name, "--help"],
        check=True,
        capture_output=True,
        text=True,
        cwd=ROOT,
    )

    assert "--with-ollama" in result.stdout
    assert "--no-launch" in result.stdout
    assert "--no-system-deps" in result.stdout


@pytest.mark.skipif(shutil.which("bash") is None, reason="bash is not installed")
def test_install_sh_unknown_option_exits_two() -> None:
    result = subprocess.run(
        ["bash", INSTALL_SH.name, "--not-an-installer-option"],
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


def test_mit_license_is_declared_in_project_metadata() -> None:
    license_text = (ROOT / "LICENSE").read_text(encoding="utf-8")
    project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))[
        "project"
    ]

    assert license_text.startswith("MIT License")
    assert project["license"] == "MIT"
    assert project["license-files"] == ["LICENSE"]
