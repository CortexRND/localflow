"""Release pins and isolated, network-free macOS prerequisite provisioning."""

import hashlib
import os
import shutil
import subprocess
import tomllib
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
BASH = shutil.which("bash") if os.name != "nt" else None


def test_release_versions_and_installer_hashes_are_consistent():
    version = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))["project"]["version"]
    assert version == "0.2.0"
    assert f'LOCALFLOW_RELEASE="v{version}"' in (ROOT / "install.sh").read_text(encoding="utf-8")
    assert f"$LocalflowRelease = 'v{version}'" in (ROOT / "install.ps1").read_text(encoding="utf-8")
    assert f'LF_REF=v{version}' in (ROOT / "packaging/windows/Install LocalFlow.cmd").read_text(encoding="utf-8")
    assert tomllib.loads((ROOT / "rust/Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"] == version
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    for filename in ("install.sh", "install.ps1"):
        digest = hashlib.sha256((ROOT / filename).read_bytes()).hexdigest()
        assert digest in readme.lower()
        assert f"/v{version}/{filename}" in readme


def test_python_switch_back_example_requires_explicit_runtime():
    readme = (ROOT / "rust/README.md").read_text(encoding="utf-8")
    example = readme.split("To switch back:", 1)[1].split("If the active agent", 1)[0]
    assert "LOCALFLOW_RUNTIME=python ./install.sh --no-launch" in example
    assert "omit LOCALFLOW_RUNTIME" not in example


def test_tag_workflow_publishes_complete_release_and_checksums():
    workflow = (ROOT / ".github/workflows/mac-installer.yml").read_text(encoding="utf-8")
    assert "tags:" in workflow and "'v*'" in workflow
    assert 'scripts/release-assets.sh "$GITHUB_REF_NAME"' in workflow
    assert 'cp dist/LocalFlow-Installer.dmg dist/release/' in workflow
    assert 'cp "packaging/windows/Install LocalFlow.cmd" dist/release/' in workflow
    assert 'shasum -a 256 LocalFlow-Installer.dmg "Install LocalFlow.cmd" >> SHA256SUMS' in workflow
    assert 'gh release create "$GITHUB_REF_NAME" --verify-tag --generate-notes' in workflow
    assert 'gh release upload "$GITHUB_REF_NAME" dist/release/* --clobber' in workflow


@pytest.mark.skipif(BASH is None, reason="requires Bash")
@pytest.mark.parametrize("system,runtime,expected", [
    ("Darwin", None, "rust"), ("Linux", None, "python"),
    ("Darwin", "python", "python"), ("Darwin", "rust", "rust"),
])
def test_platform_runtime_selection(tmp_path, system, runtime, expected):
    uname = tmp_path / "uname"
    uname.write_text(f"#!/bin/sh\necho {system}\n")
    uname.chmod(0o755)
    script = (ROOT / "install.sh").read_text(encoding="utf-8").split('\nPYTHON=""', 1)[0]
    env = {"PATH": str(tmp_path)}
    if runtime is not None:
        env["LOCALFLOW_RUNTIME"] = runtime
    result = subprocess.run([BASH, "-c", script + '\nprintf "%s" "$RUNTIME"'], env=env, capture_output=True, text=True, check=False)
    assert result.returncode == 0, result.stderr
    assert result.stdout == expected


@pytest.mark.skipif(BASH is None, reason="requires Bash")
@pytest.mark.parametrize("clt,rust,expected", [
    ("ready", "ready", None), ("install", "install", None),
    ("fail", "ready", "Could not start Xcode"),
    ("timeout", "ready", "Timed out waiting"),
    ("ready", "download-fail", "Rust installation failed"),
    ("ready", "install-fail", "Rust installation failed"),
    ("ready", "missing", "Cargo is unavailable"),
])
def test_rust_preflight_provisions_or_reports_failure(tmp_path, clt, rust, expected):
    tools = tmp_path / "bin"
    tools.mkdir()
    for name in ("sh", "mktemp", "rm", "mkdir", "chmod"):
        (tools / name).symlink_to(shutil.which(name))
    stubs = {
        "uname": "echo Darwin",
        "sleep": 'echo wait >> "$HOME/waits"',
        "cmake": "exit 0",
        "uv": "exit 99",
        "xcode-select": '''if [ "$1" = --install ]; then
echo install > "$HOME/clt-request"
[ "$CLT" != fail ] || exit 1
[ "$CLT" != install ] || echo ready > "$HOME/clt-ready"
exit 0
fi
[ "$CLT" = ready ] || [ -f "$HOME/clt-ready" ]''',
        "curl": '''[ "$RUST" != download-fail ] || exit 22
while [ "$1" != -o ]; do shift; done
printf '%s\\n' 'echo "$*" > "$HOME/rustup-args"' '[ "$RUST" != install-fail ] || exit 1' '[ "$RUST" != missing ] || exit 0' 'mkdir -p "$HOME/.cargo/bin"' 'printf "#!/bin/sh\\nexit 0\\n" > "$HOME/.cargo/bin/cargo"' 'chmod +x "$HOME/.cargo/bin/cargo"' > "$2"''',
    }
    if rust == "ready":
        stubs["cargo"] = "exit 0"
    for name, body in stubs.items():
        path = tools / name
        path.write_text(f"#!/bin/sh\n{body}\n")
        path.chmod(0o755)
    script = (ROOT / "install.sh").read_text(encoding="utf-8").split("\nSCRIPT_SOURCE=", 1)[0]
    script = script.replace(
        "for brew_dir in /opt/homebrew/bin /usr/local/bin; do",
        "for brew_dir in; do",
    )
    result = subprocess.run([BASH, "-c", script], env={"PATH": str(tools), "HOME": str(tmp_path), "CLT": clt, "RUST": rust}, capture_output=True, text=True, check=False)
    assert result.returncode == (1 if expected else 0), result.stderr
    if expected:
        assert expected in result.stderr
    if clt == "install":
        assert (tmp_path / "clt-request").exists()
    if rust == "install":
        assert (tmp_path / "rustup-args").read_text(encoding="utf-8").strip() == "-y --profile minimal --default-toolchain stable"
        assert (tmp_path / ".cargo/bin/cargo").exists()
    if clt == "timeout":
        assert len((tmp_path / "waits").read_text(encoding="utf-8").splitlines()) == 180
