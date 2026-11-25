"""Release pins and isolated, network-free macOS prerequisite provisioning."""

import hashlib
import os
import re
import shutil
import subprocess
import textwrap
import tomllib
from pathlib import Path

import pytest

from localflow import appbundle

ROOT = Path(__file__).resolve().parents[1]
BASH = shutil.which("bash") if os.name != "nt" else None


def test_release_versions_and_installer_hashes_are_consistent():
    version = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))["project"]["version"]
    assert version == "0.2.3"
    assert f'LOCALFLOW_RELEASE="v{version}"' in (ROOT / "install.sh").read_text(encoding="utf-8")
    assert f"$LocalflowRelease = 'v{version}'" in (ROOT / "install.ps1").read_text(encoding="utf-8")
    cmd = (ROOT / "packaging/windows/Install LocalFlow.cmd").read_bytes()
    assert f'set "LF_REF=v{version}"\r\n'.encode() in cmd
    assert b"\n" not in cmd.replace(b"\r\n", b""), "Install LocalFlow.cmd must keep CRLF line endings"
    assert tomllib.loads((ROOT / "rust/Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"] == version
    assert f"<string>{version}</string>" in (ROOT / "scripts/build_mac_installer.sh").read_text(encoding="utf-8")
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    for filename in ("install.sh", "install.ps1"):
        assert f"/v{version}/{filename}" in readme
    assert f"pinned to release `v{version}`" in readme
    assert f"`localflow-{version}.tar.gz`" in readme and f"`localflow-{version}.zip`" in readme


def test_python_package_metadata_and_lock_share_the_release_version(monkeypatch: pytest.MonkeyPatch):
    version = _release_version()
    assert appbundle._PLIST["CFBundleShortVersionString"] == version

    def missing_metadata(_name: str) -> str:
        raise appbundle.metadata.PackageNotFoundError("localflow")

    monkeypatch.setattr(appbundle.metadata, "version", missing_metadata)
    assert appbundle._version() == version
    lock = tomllib.loads((ROOT / "uv.lock").read_text(encoding="utf-8"))
    local = [p for p in lock["package"] if p["name"] == "localflow"]
    assert len(local) == 1 and local[0]["source"] == {"editable": "."}
    assert local[0]["version"] == version


def test_rust_workspace_crates_share_the_release_version():
    version = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))["project"]["version"]
    workspace = tomllib.loads((ROOT / "rust/Cargo.toml").read_text(encoding="utf-8"))
    local = set()
    for member in workspace["workspace"]["members"]:
        package = tomllib.loads((ROOT / "rust" / member / "Cargo.toml").read_text(encoding="utf-8"))["package"]
        assert package["version"] in ({"workspace": True}, version), member
        local.add(package["name"])
    lock = tomllib.loads((ROOT / "rust/Cargo.lock").read_text(encoding="utf-8"))
    locked = {p["name"]: p["version"] for p in lock["package"] if p["name"] in local and "source" not in p}
    assert locked == {name: version for name in local}


def _release_version() -> str:
    return tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))["project"]["version"]


def _branch_installer_sha256(filename: str) -> str:
    # The release is not published while it is being prepared, so README digests
    # are the SHA-256 of this branch's installer bytes (pinned to LF endings by
    # .gitattributes); these are what the tag will publish. Regenerate the README
    # digests after every installer change, before tagging (docs/releasing.md).
    return hashlib.sha256((ROOT / filename).read_bytes()).hexdigest()


def test_install_sh_pins_no_release_commit():
    # The release tag lands on a merge commit containing install.sh, so its hash
    # cannot be embedded; default installs trust the checksummed archive instead.
    script = (ROOT / "install.sh").read_text(encoding="utf-8")
    assert "LOCALFLOW_RELEASE_COMMIT" not in script
    assert "require_release_commit" not in script
    assert not re.search(r"\b[0-9a-f]{40}\b", script)
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    assert "pinned release commit" not in readme


# Number of README one-liners expected per installer (install.sh has a plain
# and a --with-ollama variant).
README_INSTALLER_COMMANDS = {"install.sh": 2, "install.ps1": 1}


def test_readme_installer_one_liners_pin_branch_installer_digests():
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    release = f"v{_release_version()}"
    url_re = re.compile(
        r"https://raw\.githubusercontent\.com/CortexRND/localflow/(?P<tag>[^/\s]+)/(?P<file>install\.(?:sh|ps1))\b"
    )
    digest_re = re.compile(r"\b[0-9a-fA-F]{64}\b")
    seen = {name: 0 for name in README_INSTALLER_COMMANDS}
    for lineno, line in enumerate(readme.splitlines(), 1):
        urls = list(url_re.finditer(line))
        if not urls:
            continue
        assert len(urls) == 1, f"README:{lineno}: expected one installer URL per command"
        tag, filename = urls[0]["tag"], urls[0]["file"]
        assert tag == release, f"README:{lineno}: installer URL must use the current release {release}, not {tag}"
        digests = [d.lower() for d in digest_re.findall(line)]
        assert digests == [_branch_installer_sha256(filename)], (
            f"README:{lineno}: {tag}/{filename} must be paired on the same line with the SHA-256 of this "
            f"branch's {filename}; regenerate it after installer changes (docs/releasing.md)"
        )
        seen[filename] += 1
    assert seen == README_INSTALLER_COMMANDS


def test_python_switch_back_example_requires_explicit_runtime():
    readme = (ROOT / "rust/README.md").read_text(encoding="utf-8")
    example = readme.split("To switch back:", 1)[1].split("If the active agent", 1)[0]
    assert "LOCALFLOW_RUNTIME=python ./install.sh --no-launch" in example
    assert "omit LOCALFLOW_RUNTIME" not in example


def test_tag_workflow_publishes_complete_release_and_checksums():
    workflow = (ROOT / ".github/workflows/mac-installer.yml").read_text(encoding="utf-8")
    assert "tags:" in workflow and "'v*'" in workflow
    assert 'scripts/release-assets.sh "$GITHUB_REF_NAME"' in workflow
    assert 'cp dist/LocalFlow-Installer.dmg dist/release/LocalFlow-Installer.dmg' in workflow
    # GitHub publishes "Install LocalFlow.cmd" as "Install.LocalFlow.cmd"; the
    # checksum must name the uploaded asset, not the local spaced file.
    assert 'cp "packaging/windows/Install LocalFlow.cmd" dist/release/Install.LocalFlow.cmd' in workflow
    assert 'shasum -a 256 LocalFlow-Installer.dmg Install.LocalFlow.cmd >> SHA256SUMS' in workflow
    assert '"Install LocalFlow.cmd" >> SHA256SUMS' not in workflow
    assert 'shasum -a 256 -c SHA256SUMS' in workflow
    assert 'gh release create "$GITHUB_REF_NAME" --verify-tag --generate-notes' in workflow
    assert 'gh release upload "$GITHUB_REF_NAME" dist/release/* --clobber' in workflow
    assert (ROOT / "packaging/windows/Install LocalFlow.cmd").is_file()


def _workflow_step_run(step_name: str) -> str:
    """Return the dedented `run: |` script of a step in mac-installer.yml."""
    lines = (ROOT / ".github/workflows/mac-installer.yml").read_text(encoding="utf-8").splitlines()
    indent = lambda line: len(line) - len(line.lstrip())
    start = next(i for i, line in enumerate(lines) if line.strip() == f"- name: {step_name}")
    step_indent = indent(lines[start])
    body: list[str] = []
    run_indent = None
    for line in lines[start + 1:]:
        if line.strip() and indent(line) <= step_indent:
            break
        if run_indent is None:
            if line.strip() == "run: |":
                run_indent = indent(line)
            continue
        if line.strip() and indent(line) <= run_indent:
            break
        body.append(line)
    assert body, f"step {step_name!r} has no run script"
    block_indent = min(indent(line) for line in body if line.strip())
    return "\n".join(line[block_indent:] for line in body) + "\n"


def _write_stub(path: Path, body: str) -> None:
    path.write_text(f"#!/bin/bash\n{body}\n", encoding="utf-8")
    path.chmod(0o755)


def _run_release_publisher(tmp_path: Path, fault: str | None = None):
    """Run the workflow's upload step against stubbed release-assets.sh and gh."""
    version = _release_version()
    tag = f"v{version}"
    work = tmp_path / "work"
    bin_dir = tmp_path / "bin"
    (work / "scripts").mkdir(parents=True)
    (work / "packaging/windows").mkdir(parents=True)
    bin_dir.mkdir()
    gh_log = tmp_path / "gh.log"
    if shutil.which("shasum") is None:
        _write_stub(bin_dir / "shasum", '[ "$1" = -a ] && shift 2\nexec sha256sum "$@"')
    # Every gh call appends its argv (one per line) and a terminator; `release
    # view` fails so the create path is exercised. No network is touched.
    _write_stub(bin_dir / "gh", 'printf \'%s\\n\' "$@" "<<end>>" >> "$GH_LOG"\n[ "$1 $2" != "release view" ]')
    assets = textwrap.dedent(f"""\
        set -euo pipefail
        [ "$1" = {tag} ]
        mkdir -p dist/release
        printf 'tarball' > dist/release/localflow-{version}.tar.gz
        printf 'zipball' > dist/release/localflow-{version}.zip
        (cd dist/release && shasum -a 256 localflow-{version}.tar.gz localflow-{version}.zip > SHA256SUMS)
    """)
    if fault == "tampered":
        assets += f"printf 'changed' > dist/release/localflow-{version}.zip\n"
    if fault == "unlisted":
        assets += "printf 'extra' > 'dist/release/ExtraAsset.txt'\n"
    _write_stub(work / "scripts/release-assets.sh", assets)
    if fault != "missing-dmg":
        (work / "dist").mkdir()
        (work / "dist/LocalFlow-Installer.dmg").write_bytes(b"fake dmg bytes")
    if fault != "missing-cmd":
        shutil.copyfile(ROOT / "packaging/windows/Install LocalFlow.cmd", work / "packaging/windows/Install LocalFlow.cmd")
    env = {**os.environ, "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
           "GITHUB_REF_NAME": tag, "GH_TOKEN": "unused", "GH_LOG": str(gh_log)}
    result = subprocess.run([BASH, "--noprofile", "--norc", "-eo", "pipefail", "-c", _workflow_step_run("Upload to release")],
                            cwd=work, env=env, capture_output=True, text=True, check=False)
    calls = []
    if gh_log.exists():
        current: list[str] = []
        for arg in gh_log.read_text(encoding="utf-8").splitlines():
            if arg == "<<end>>":
                calls.append(current)
                current = []
            else:
                current.append(arg)
    return result, work, calls


@pytest.mark.skipif(BASH is None, reason="requires Bash")
def test_release_publisher_checksums_every_asset_under_its_upload_name(tmp_path):
    version = _release_version()
    result, work, calls = _run_release_publisher(tmp_path)
    assert result.returncode == 0, result.stderr
    uploads = [c for c in calls if c[:2] == ["release", "upload"]]
    assert len(uploads) == 1, calls
    upload = uploads[0]
    assert upload[2] == f"v{version}" and upload[-1] == "--clobber"
    uploaded = {Path(p).name: work / p for p in upload[3:-1]}
    assert set(uploaded) == {
        f"localflow-{version}.tar.gz", f"localflow-{version}.zip",
        "LocalFlow-Installer.dmg", "Install.LocalFlow.cmd", "SHA256SUMS",
    }
    sums = {}
    for line in uploaded["SHA256SUMS"].read_text(encoding="utf-8").splitlines():
        digest, name = line.split(maxsplit=1)
        name = name.lstrip("*")
        assert name not in sums, f"duplicate checksum entry for {name}"
        sums[name] = digest
    assert set(sums) == set(uploaded) - {"SHA256SUMS"}
    for name, digest in sums.items():
        assert uploaded[name].is_file(), f"{name} is checksummed but not uploaded under that name"
        assert hashlib.sha256(uploaded[name].read_bytes()).hexdigest() == digest, name
    assert uploaded["Install.LocalFlow.cmd"].read_bytes() == (ROOT / "packaging/windows/Install LocalFlow.cmd").read_bytes()
    assert (work / "packaging/windows/Install LocalFlow.cmd").is_file()


@pytest.mark.skipif(BASH is None, reason="requires Bash")
@pytest.mark.parametrize("fault", ["missing-dmg", "missing-cmd", "tampered", "unlisted"])
def test_release_publisher_fails_closed_before_upload(tmp_path, fault):
    result, _, calls = _run_release_publisher(tmp_path, fault)
    assert result.returncode != 0, result.stdout
    assert not [c for c in calls if c[:2] in (["release", "upload"], ["release", "create"])], calls


def _local_manifest_version(manifest: Path) -> str:
    version = tomllib.loads(manifest.read_text(encoding="utf-8"))["package"]["version"]
    if version != {"workspace": True}:
        return version
    for directory in manifest.parent.parents:
        candidate = directory / "Cargo.toml"
        if candidate.is_file():
            data = tomllib.loads(candidate.read_text(encoding="utf-8"))
            if "workspace" in data:
                return data["workspace"]["package"]["version"]
    raise AssertionError(f"{manifest} inherits a version but has no workspace root")


def test_standalone_desktop_lock_matches_local_manifest_versions():
    # rust/lf-desktop is its own workspace with its own Cargo.lock, so bumping
    # rust/Cargo.toml (or any local manifest) leaves that lock stale until it is
    # refreshed (docs/releasing.md).
    desktop = ROOT / "rust/lf-desktop"
    local = {}
    for manifest in [*(ROOT / "rust").glob("*/Cargo.toml"), *(desktop / "vendor").glob("*/Cargo.toml")]:
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
        if "package" in data:
            name = data["package"]["name"]
            assert name not in local, f"duplicate local package {name}"
            local[name] = _local_manifest_version(manifest)
    manifest = tomllib.loads((desktop / "Cargo.toml").read_text(encoding="utf-8"))
    expected = {manifest["package"]["name"]}
    for table in (manifest["dependencies"], manifest["patch"]["crates-io"]):
        expected |= {name for name, spec in table.items() if isinstance(spec, dict) and "path" in spec}
    assert {"lf-desktop", "lf-core", "lf-ui", "lf-providers", "lf-audio", "xattr"} <= expected
    lock = tomllib.loads((desktop / "Cargo.lock").read_text(encoding="utf-8"))
    locked = {p["name"]: p["version"] for p in lock["package"] if "source" not in p}
    assert expected <= set(locked), f"path packages missing from rust/lf-desktop/Cargo.lock: {expected - set(locked)}"
    assert set(locked) <= set(local), f"unknown path packages in lock: {set(locked) - set(local)}"
    stale = {name: (version, local[name]) for name, version in locked.items() if version != local[name]}
    assert not stale, f"rust/lf-desktop/Cargo.lock is stale (locked, manifest): {stale}"
    # Every LocalFlow crate, including the standalone desktop app, carries the
    # release version; vendored third-party shims keep their upstream versions.
    vendored = {
        tomllib.loads(m.read_text(encoding="utf-8"))["package"]["name"]
        for m in (desktop / "vendor").glob("*/Cargo.toml")
    }
    release = _release_version()
    assert manifest["package"]["version"] == release
    off_release = {name: v for name, v in locked.items() if name not in vendored and v != release}
    assert not off_release, f"LocalFlow path packages not at {release}: {off_release}"


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
