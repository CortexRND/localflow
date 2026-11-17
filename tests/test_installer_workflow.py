import os
import shutil
import struct
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
BASH = shutil.which("bash") if os.name != "nt" else None


def test_rust_bundle_uses_stable_designated_requirement_and_icon() -> None:
    script = (ROOT / "scripts/build_rust_app.sh").read_text(encoding="utf-8")
    assert 'bundle_id="$(plutil -extract CFBundleIdentifier raw -o - "$plist")"' in script
    assert r'requirement="designated => identifier \"$bundle_id\""' in script
    assert '--identifier "$bundle_id" --requirements "=$requirement" "$bundle"' in script
    assert 'codesign --verify --deep --strict' in script
    assert '"$ROOT/packaging/macos/LocalFlow.icns" "$resources/LocalFlow.icns"' in script
    assert 'plutil -insert CFBundleIconFile -string LocalFlow.icns "$plist"' in script


def test_committed_icon_has_png_source_and_multiple_icns_sizes() -> None:
    source = (ROOT / "packaging/macos/LocalFlow.png").read_bytes()
    assert source[:8] == b"\x89PNG\r\n\x1a\n"
    assert struct.unpack(">II", source[16:24]) == (1024, 1024)
    icon = (ROOT / "packaging/macos/LocalFlow.icns").read_bytes()
    assert icon[:4] == b"icns"
    assert struct.unpack(">I", icon[4:8])[0] == len(icon)
    chunks = set()
    offset = 8
    while offset < len(icon):
        kind, length = struct.unpack(">4sI", icon[offset:offset + 8])
        assert length > 8
        assert offset + length <= len(icon)
        chunks.add(kind)
        offset += length
    assert offset == len(icon)
    assert {b"icp4", b"icp5", b"icp6", b"ic07", b"ic08", b"ic09", b"ic10"} <= chunks


def test_piped_installer_home_is_defined_at_step_scope() -> None:
    workflow = (ROOT / ".github" / "workflows" / "installers.yml").read_text(
        encoding="utf-8"
    )
    piped_job = workflow.split("\n  piped:\n", 1)[1]
    job_env = piped_job.split("\n    env:\n", 1)[1].split("\n    steps:\n", 1)[0]
    assert "runner." not in job_env

    for name in ("Install from piped shell script", "Install from piped PowerShell script"):
        step = piped_job.split(f"      - name: {name}\n", 1)[1].split("\n      - ", 1)[0]
        assert "\n        env:\n          LOCALFLOW_HOME: ${{ runner.temp }}/lfhome\n" in step


def test_linux_installer_jobs_prepare_headless_audio() -> None:
    workflow = (ROOT / ".github" / "workflows" / "installers.yml").read_text(
        encoding="utf-8"
    )
    for name in ("Prepare Linux audio", "Install Linux system dependencies"):
        step = workflow.split(f"      - name: {name}\n", 1)[1].split("\n      - ", 1)[0]
        assert "if: runner.os == 'Linux'" in step
        packages = next(
            line.split("sudo apt-get install -y ", 1)[1].split()
            for line in step.splitlines()
            if "sudo apt-get install -y " in line
        )
        assert "pulseaudio" in packages
        assert "pulseaudio --start --exit-idle-time=-1" in step


def test_rust_opt_in_job_checks_signature_agent_and_migration_round_trip() -> None:
    workflow = (ROOT / ".github/workflows/installers.yml").read_text(encoding="utf-8")
    job = workflow.split("\n  rust-macos:\n", 1)[1].split("\n  #", 1)[0]
    assert "runs-on: macos-latest" in job
    assert "LOCALFLOW_SKIP_LAUNCHCTL: '1'" in job
    assert job.count("          ./install.sh --no-launch") == 2
    assert "LOCALFLOW_RUNTIME=python ./install.sh --no-launch" in job
    assert "codesign --verify --deep --strict" in job
    assert "plutil -extract ProgramArguments.0" in job
    assert 'cmp "$RUNNER_TEMP/python-agent.plist" "$agent"' in job
    assert 'cmp "$RUNNER_TEMP/python-agent.plist" "$agent.python.bak"' in job
    assert "DYLD_PRINT_LIBRARIES=1" in job
    assert 'otool -L "$app/Contents/MacOS/lf-hub"' in job
    for marker in (
        "PYTHON_INITIAL_AGENT",
        "RUST_FIRST_AGENT",
        "RUST_REPEAT_AGENT",
        "PYTHON_RESTORED_AGENT",
        "PYTHON_REPEAT_AGENT",
        "RUST_REINSTATED_AGENT",
    ):
        assert f"echo {marker}\n" in job
    assert "./install.sh --uninstall" in job


def test_installer_runtime_defaults_by_platform_and_rejects_unknown_values() -> None:
    script = (ROOT / "install.sh").read_text(encoding="utf-8")
    assert 'RUNTIME="${LOCALFLOW_RUNTIME-python}"' in script
    assert 'if [[ "${LOCALFLOW_RUNTIME+x}" != x && "$OS" == Darwin ]]; then RUNTIME=rust; fi' in script
    assert "Invalid LOCALFLOW_RUNTIME" in script
    assert "python|rust)" in script
    assert 'if [[ "$RUNTIME" == python ]]; then' in script
    assert "bash scripts/build_rust_app.sh" in script
    assert "rust (macOS default)" in script


@pytest.mark.skipif(BASH is None, reason="Bash is not installed")
@pytest.mark.parametrize(
    ("have_cmake", "have_brew", "brew_status"),
    [(True, False, 0), (True, True, 0), (False, True, 0), (False, False, 0), (False, True, 1)],
)
def test_rust_macos_cmake_preflight(
    tmp_path: Path, have_cmake: bool, have_brew: bool, brew_status: int
) -> None:
    tools = tmp_path / "bin"
    tools.mkdir()
    brew_log = tmp_path / "brew.log"
    stubs = {"uname": "echo Darwin", "cargo": "exit 0", "xcode-select": "exit 0"}
    if have_cmake:
        stubs["cmake"] = "exit 0"
    if have_brew:
        stubs["brew"] = f'printf "%s\\n" "$*" > "$BREW_LOG"\nexit {brew_status}'
    for name, body in stubs.items():
        executable = tools / name
        executable.write_text(f"#!/bin/sh\n{body}\n", encoding="utf-8")
        executable.chmod(0o755)

    script = (ROOT / "install.sh").read_text(encoding="utf-8")
    preflight = script.split("\nSCRIPT_SOURCE=", 1)[0]
    result = subprocess.run(
        [BASH, "-c", preflight],
        env={
            "PATH": str(tools),
            "HOME": str(tmp_path),
            "LOCALFLOW_RUNTIME": "rust",
            "BREW_LOG": str(brew_log),
        },
        capture_output=True,
        text=True,
        check=False,
    )

    expected_success = have_cmake or (have_brew and brew_status == 0)
    assert result.returncode == (0 if expected_success else 1), result.stderr
    if not have_cmake and have_brew:
        assert brew_log.read_text(encoding="utf-8") == "install cmake\n"
    else:
        assert not brew_log.exists()
    if not have_cmake and not have_brew:
        assert "requires CMake; install cmake" in result.stderr
