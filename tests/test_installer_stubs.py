"""Run install.sh against stubbed system tools to exercise macOS fallbacks."""

from __future__ import annotations

import hashlib
import os
import re
import shlex
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path
from urllib.parse import urlsplit

import pytest

ROOT = Path(__file__).resolve().parents[1]
INSTALL_SH = ROOT / "install.sh"
BASH = shutil.which("bash") if os.name != "nt" else None

pytestmark = pytest.mark.skipif(BASH is None, reason="Bash is not installed")

REAL_TOOLS = (
    "sh", "dirname", "mkdir", "mv", "rm", "cat", "touch", "tar", "gzip",
    "mktemp", "chmod", "cp", "ls", "ln", "env", "sleep", "tr",
)  # fmt: skip
HASH_TOOLS = ("sha256sum", "shasum")
RELEASE = re.search(
    r'^LOCALFLOW_RELEASE="(v[^"]+)"$', INSTALL_SH.read_text(encoding="utf-8"), re.M
).group(1)
VERSION = RELEASE.removeprefix("v")
ASSET = f"localflow-{VERSION}.tar.gz"
RELEASE_URL = f"https://github.com/CortexRND/localflow/releases/download/{RELEASE}"
REAL_CURL = shutil.which("curl")
EXTRAS_ARM = "[mlx,parakeet,menubar,macos-permissions]"
EXTRAS_INTEL = "[macos-permissions]"
HOST_BREW_PROBE = "for brew_dir in /opt/homebrew/bin /usr/local/bin; do"

STUB_TOOL = r'''
import os
import shutil
import sys
from pathlib import Path

name, extra, args = sys.argv[1], sys.argv[2], sys.argv[3:]
PY = sys.executable
SELF = os.path.abspath(__file__)


def record():
    with open(os.environ["STUB_LOG"], "a", encoding="utf-8") as log:
        log.write(" ".join([name, *args]) + "\n")


def wrapper(path, tool):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text('#!/bin/sh\nexec "%s" "%s" %s "" "$@"\n' % (PY, SELF, tool))
    path.chmod(0o755)


def make_venv(target):
    for tool in ("python", "pip", "lf"):
        wrapper(Path(target) / "bin" / tool, "venv-" + tool)


if name == "uname":
    system, arch = extra.split("|")
    print(system if args[:1] == ["-s"] else arch)
    sys.exit(0)
if name == "xcode-select":
    sys.exit(int(extra))

record()
if name.startswith("python"):
    if args[:1] == ["-c"]:
        prelude = (
            "import sys; sys.version_info = tuple(int(p) for p in %r.split('.'))"
            " + ('final', 0); exec(compile(sys.argv[1], '<stub>', 'exec'))" % extra
        )
        os.execv(PY, [PY, "-c", prelude, args[1]])
    if args[:2] == ["-m", "venv"]:
        make_venv(args[2])
elif name == "uv":
    if args[:1] == ["venv"]:
        make_venv(args[-1])
elif name == "curl":
    from urllib.parse import urlsplit

    url = next(a for a in args if a.startswith(("http", "file:")))
    if url.startswith("file:"):
        # Local release assets (LOCALFLOW_RELEASE_BASE_URL): use the real curl.
        os.execv(os.environ["STUB_REAL_CURL"], ["curl", *args])
    host = urlsplit(url).hostname
    if host == "astral.sh":
        wrapper(Path(os.environ["HOME"]) / ".local" / "bin" / "uv", "uv")
        print("exit 0")
    elif host == "codeload.github.com":
        if os.environ.get("STUB_DOWNLOAD_FAIL"):
            sys.exit(22)
        shutil.copy(os.environ["STUB_TARBALL"], args[args.index("-o") + 1])
    elif host == "github.com" and "/releases/download/" in url:
        if os.environ.get("STUB_DOWNLOAD_FAIL"):
            sys.exit(22)
        asset = Path(os.environ["STUB_RELEASE_DIR"]) / url.rsplit("/", 1)[1]
        if not asset.is_file():
            sys.exit(22)
        shutil.copy(asset, args[args.index("-o") + 1])
elif name == "git" and args[:1] == ["clone"]:
    dest = Path(args[-1])
    (dest / ".git").mkdir(parents=True)
    (dest / "pyproject.toml").write_text("[project]\nname = 'localflow'\n")
'''


class Sandbox:
    def __init__(
        self,
        tmp_path: Path,
        *,
        arch: str = "arm64",
        clt: bool | None = True,
        hash_tools: tuple[str, ...] = HASH_TOOLS,
    ):
        self.root = tmp_path
        self.home = tmp_path / "home"
        self.home.mkdir()
        self.bin = tmp_path / "stubs"
        self.bin.mkdir()
        self.tools = tmp_path / "tools"
        self.tools.mkdir()
        self.log = tmp_path / "calls.log"
        self.log.write_text("")
        self.stub_tool = tmp_path / "stubtool.py"
        self.stub_tool.write_text(STUB_TOOL, encoding="utf-8")
        self.script = tmp_path / "installer" / "install.sh"
        self.script.parent.mkdir()
        # install.sh appends the fixed Homebrew prefixes to PATH, so on a Mac
        # host the real /opt/homebrew/bin/python3.x would win over the stubs
        # and run a real pip. The sandbox copy probes local prefixes instead.
        self.brew_dirs = (tmp_path / "opt-homebrew" / "bin", tmp_path / "usr-local" / "bin")
        for directory in self.brew_dirs:
            directory.mkdir(parents=True)
        source = INSTALL_SH.read_text(encoding="utf-8")
        assert source.count(HOST_BREW_PROBE) == 1, "install.sh Homebrew probe changed"
        local_probe = "for brew_dir in {}; do".format(" ".join(f'"{d}"' for d in self.brew_dirs))
        self.script.write_text(source.replace(HOST_BREW_PROBE, local_probe), encoding="utf-8")
        for tool in (*REAL_TOOLS, *hash_tools):
            found = shutil.which(tool)
            if found:
                (self.tools / tool).symlink_to(found)
        self.dest = self.home / "lf"
        self.tarball = tmp_path / "source.tar.gz"
        self.release = tmp_path / "release"
        self.release.mkdir()
        self.extra_path: list[str] = []
        self.add("uname", f"Darwin|{arch}")
        self.add("curl")
        if clt is not None:
            self.add("xcode-select", "0" if clt else "1")
        self.make_tarball("v1")

    def add(self, name: str, extra: str = "", directory: Path | None = None) -> None:
        path = (directory or self.bin) / name
        path.write_text(
            f'#!/bin/sh\nexec "{sys.executable}" "{self.stub_tool}" {name} "{extra}" "$@"\n'
        )
        path.chmod(0o755)

    def make_tarball(self, marker: str) -> None:
        src = self.root / f"src-{marker}" / "localflow-main"
        src.mkdir(parents=True, exist_ok=True)
        (src / "pyproject.toml").write_text("[project]\nname = 'localflow'\n")
        (src / "version.txt").write_text(marker)
        with tarfile.open(self.tarball, "w:gz") as archive:
            archive.add(src, arcname="localflow-main")
        # The same bytes as the pinned release asset, with a matching SHA256SUMS.
        shutil.copy(self.tarball, self.release / ASSET)
        digest = hashlib.sha256(self.tarball.read_bytes()).hexdigest()
        (self.release / "SHA256SUMS").write_text(
            f"{'0' * 64}  localflow-{VERSION}.zip\n{digest}  {ASSET}\n"
        )

    def run(
        self,
        args: tuple[str, ...] = ("--no-launch",),
        env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        path = os.pathsep.join([str(self.bin), str(self.tools), *self.extra_path])
        environment = {
            "HOME": str(self.home),
            "PATH": path,
            "STUB_LOG": str(self.log),
            "STUB_TARBALL": str(self.tarball),
            "STUB_RELEASE_DIR": str(self.release),
            "STUB_REAL_CURL": str(REAL_CURL),
            "LOCALFLOW_HOME": str(self.dest),
            "LOCALFLOW_RUNTIME": "python",
            "LOCALFLOW_REPO_URL": "https://example.invalid/localflow.git",
            **(env or {}),
        }
        return subprocess.run(
            [BASH, str(self.script), *args],
            env=environment,
            cwd=self.root,
            capture_output=True,
            text=True,
            check=False,
        )

    def calls(self) -> list[str]:
        return self.log.read_text().splitlines()


def _urls(calls: list[str]) -> list[str]:
    """Every http(s) URL argument passed to a stubbed tool."""
    return [
        word for call in calls for word in shlex.split(call)
        if urlsplit(word).scheme in ("http", "https")
    ]


def _hosts(calls: list[str]) -> set[str | None]:
    return {urlsplit(url).hostname for url in _urls(calls)}


def _astral_downloads(calls: list[str]) -> int:
    return _urls(calls).count("https://astral.sh/uv/install.sh")


def test_macos_without_brew_python_or_git_uses_uv_and_tarball(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path, clt=False)

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert _astral_downloads(calls) == 1
    assert "uv python install 3.12" in calls
    assert "uv venv --allow-existing --python 3.12 .venv" in calls
    assert f"uv pip install --python .venv/bin/python -e .{EXTRAS_ARM}" in calls
    assert "venv-lf setup install-app" in calls
    assert not any(call.startswith("git ") for call in calls)
    assert (sandbox.dest / ".localflow-tarball").is_file()
    assert (sandbox.dest / "version.txt").read_text() == "v1"
    assert "ffmpeg was not installed" in result.stdout


def test_macos_final_step_opens_main_app(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("open")

    result = sandbox.run(args=())

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert "venv-lf setup install-app" in calls
    assert calls[-1] == f"open {sandbox.home}/Applications/LocalFlow.app"
    assert "venv-lf setup" not in calls


def test_macos_no_launch_does_not_open_app(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("open")

    result = sandbox.run(args=("--no-launch",))

    assert result.returncode == 0, result.stderr
    assert not any(call.startswith("open ") for call in sandbox.calls())


def test_macos_without_brew_ignores_supported_system_python(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("python3", "3.12.4")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert "uv python install 3.12" in calls
    assert not any(call.startswith("python3 -m venv") for call in calls)


@pytest.mark.parametrize("version", ["3.11.9", "3.12.4", "3.13.1", "3.14.0", "3.15.0"])
def test_macos_with_brew_and_supported_python_uses_pip(
    tmp_path: Path, version: str
) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3", version)

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert "brew install ffmpeg" in calls
    assert "python3 -m venv .venv" in calls
    assert f"venv-pip install -e .{EXTRAS_ARM}" in calls
    assert not any(call.startswith("uv ") for call in calls)
    assert "astral.sh" not in _hosts(calls)
    assert any(call.startswith("git clone https://example.invalid/") for call in calls)


def test_macos_too_old_python_falls_back_to_uv(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3", "3.10.14")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert "uv python install 3.12" in calls
    assert f"uv pip install --python .venv/bin/python -e .{EXTRAS_ARM}" in calls
    assert not any(call.startswith("python3 -m venv") for call in calls)
    # Homebrew and git are present, so those are still used.
    assert "brew install ffmpeg" in calls
    assert any(call.startswith("git clone ") for call in calls)


def test_macos_accepts_python_314(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3", "3.14.0")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert "python3 -m venv .venv" in calls
    assert not any(call.startswith("uv ") for call in calls)


def test_sandbox_installer_never_probes_host_homebrew(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)

    script = sandbox.script.read_text(encoding="utf-8")

    assert "/opt/homebrew" not in script and "/usr/local/bin" not in script
    assert INSTALL_SH.read_text(encoding="utf-8").count(HOST_BREW_PROBE) == 1


def test_brew_prefix_off_path_supplies_brew_and_newer_python(tmp_path: Path) -> None:
    # The collision seen on macOS CI: a Homebrew prefix that is not on PATH
    # gets appended by install.sh and its newer python3.x outranks python3.
    sandbox = Sandbox(tmp_path)
    sandbox.add("git")
    sandbox.add("python3", "3.12.4")
    prefix = sandbox.brew_dirs[0]
    sandbox.add("brew", directory=prefix)
    sandbox.add("python3.14", "3.14.0", directory=prefix)

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert "brew install ffmpeg" in calls
    assert "python3.14 -m venv .venv" in calls
    assert f"venv-pip install -e .{EXTRAS_ARM}" in calls
    assert not any(call.startswith(("python3 -m venv", "uv ")) for call in calls)


@pytest.mark.parametrize("use_uv", [False, True])
def test_macos_intel_installs_only_permissions_extra(
    tmp_path: Path, use_uv: bool
) -> None:
    sandbox = Sandbox(tmp_path, arch="x86_64")
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3", "3.10.14" if use_uv else "3.14.0")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    expected = (
        f"uv pip install --python .venv/bin/python -e .{EXTRAS_INTEL}"
        if use_uv
        else f"venv-pip install -e .{EXTRAS_INTEL}"
    )
    assert expected in calls


def test_macos_without_clt_never_runs_usr_bin_git(tmp_path: Path) -> None:
    real_git = Path("/usr/bin/git")
    if not real_git.exists():
        pytest.skip("/usr/bin/git is not present")
    sandbox = Sandbox(tmp_path, clt=False)
    sandbox.add("brew")
    sandbox.extra_path = ["/usr/bin"]

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert f"{RELEASE_URL}/{ASSET}" in _urls(calls)
    assert (sandbox.dest / ".localflow-tarball").is_file()
    assert not (sandbox.dest / ".git").exists()
    # /usr/bin/python3 (if any) must not have been used either.
    assert "uv python install 3.12" in calls


def test_macos_without_clt_still_uses_non_usr_bin_git(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path, clt=False)
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3.12", "3.12.4")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert any(call.startswith("git clone ") for call in calls)
    assert "codeload.github.com" not in _hosts(calls)


def test_tarball_uses_localflow_ref_but_ignores_repo_url(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)

    result = sandbox.run(env={"LOCALFLOW_REF": "v9.9.9"})

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert "https://codeload.github.com/CortexRND/localflow/tar.gz/v9.9.9" in _urls(calls)
    assert "example.invalid" not in _hosts(calls)
    assert "github.com" not in _hosts(calls)  # no release asset or SHA256SUMS
    assert "LOCALFLOW_REF=v9.9.9 is a custom ref; its source is not checksum-verified" in (
        result.stderr
    )


def test_tarball_reinstall_replaces_source_and_keeps_venv(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    assert sandbox.run().returncode == 0
    (sandbox.dest / "stale.txt").write_text("old")
    (sandbox.dest / ".venv" / "keep.txt").write_text("keep")
    sandbox.make_tarball("v2")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    assert (sandbox.dest / "version.txt").read_text() == "v2"
    assert not (sandbox.dest / "stale.txt").exists()
    assert (sandbox.dest / ".venv" / "keep.txt").read_text() == "keep"
    assert (sandbox.dest / ".localflow-tarball").is_file()
    assert _astral_downloads(sandbox.calls()) == 1  # uv reused the second time
    assert not [p for p in sandbox.home.iterdir() if p.name.startswith(".localflow-")]


def test_failed_download_leaves_existing_install_untouched(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    assert sandbox.run().returncode == 0
    (sandbox.dest / "stale.txt").write_text("old")

    result = sandbox.run(env={"STUB_DOWNLOAD_FAIL": "1"})

    assert result.returncode != 0
    assert "Could not download" in result.stderr
    assert (sandbox.dest / "stale.txt").read_text() == "old"
    assert (sandbox.dest / "version.txt").read_text() == "v1"
    assert (sandbox.dest / ".venv").is_dir()
    assert not [p for p in sandbox.home.iterdir() if p.name.startswith(".localflow-")]


def test_failed_first_download_creates_no_destination(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)

    result = sandbox.run(env={"STUB_DOWNLOAD_FAIL": "1"})

    assert result.returncode != 0
    assert not sandbox.dest.exists()


def test_unrelated_destination_is_refused_untouched(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("brew")
    sandbox.add("python3", "3.12.4")
    sandbox.dest.mkdir()
    (sandbox.dest / "notes.txt").write_text("mine")

    result = sandbox.run()

    assert result.returncode == 1
    assert "not a LocalFlow install" in result.stderr
    assert (sandbox.dest / "notes.txt").read_text() == "mine"
    assert "codeload.github.com" not in _hosts(sandbox.calls())


def test_existing_git_checkout_is_updated_with_git(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3", "3.12.4")
    (sandbox.dest / ".git").mkdir(parents=True)
    (sandbox.dest / "pyproject.toml").write_text("[project]\nname = 'localflow'\n")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    tag = f"refs/tags/{RELEASE}"
    assert f"git -C {sandbox.dest} fetch origin {tag}:{tag}" in calls
    assert f"git -C {sandbox.dest} -c advice.detachedHead=false checkout {tag}" in calls
    assert not any("pull" in call for call in calls)


def test_existing_git_checkout_without_usable_git_is_kept(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    (sandbox.dest / ".git").mkdir(parents=True)
    (sandbox.dest / "pyproject.toml").write_text("[project]\nname = 'localflow'\n")

    result = sandbox.run(env={"LOCALFLOW_REF": "abc123"})

    assert result.returncode == 0, result.stderr
    assert "keeping the existing checkout" in result.stdout
    assert (sandbox.dest / ".git").is_dir()


def test_existing_git_checkout_honours_localflow_ref(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3", "3.12.4")
    (sandbox.dest / ".git").mkdir(parents=True)
    (sandbox.dest / "pyproject.toml").write_text("[project]\nname = 'localflow'\n")

    result = sandbox.run(env={"LOCALFLOW_REF": "v1.0.0"})

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    assert f"git -C {sandbox.dest} fetch origin" in calls
    assert f"git -C {sandbox.dest} checkout v1.0.0" in calls


def test_install_sh_never_requires_tkinter_on_macos(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3", "3.12.4")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    assert not any("tkinter" in call for call in sandbox.calls())


def test_fresh_git_clone_checks_out_release_tag(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    sandbox.add("brew")
    sandbox.add("git")
    sandbox.add("python3", "3.12.4")

    result = sandbox.run()

    assert result.returncode == 0, result.stderr
    calls = sandbox.calls()
    tag = f"refs/tags/{RELEASE}"
    clone = calls.index(f"git clone https://example.invalid/localflow.git {sandbox.dest}")
    checkout = calls.index(f"git -C {sandbox.dest} -c advice.detachedHead=false checkout {tag}")
    assert clone < checkout
    assert "not checksum-verified" not in result.stderr


def test_help_names_pinned_release_not_main() -> None:
    result = subprocess.run(
        [BASH, str(INSTALL_SH), "--help"], capture_output=True, text=True, check=True
    )

    assert RELEASE in result.stdout
    assert "default: main" not in result.stdout
    assert "LOCALFLOW_RELEASE_BASE_URL" in result.stdout


def test_install_ps1_pins_the_same_release() -> None:
    script = (ROOT / "install.ps1").read_text(encoding="utf-8")

    assert f"$LocalflowRelease = '{RELEASE}'" in script
    assert "Get-FileHash" in script and "OrdinalIgnoreCase" in script
    assert "'main'" not in script


# Checksum verifier: release assets are served from a local directory through
# LOCALFLOW_RELEASE_BASE_URL (file://) and fetched by the real curl.


def _local_release_env(sandbox: Sandbox) -> dict[str, str]:
    assert REAL_CURL, "curl is required for the checksum tests"
    return {"LOCALFLOW_RELEASE_BASE_URL": sandbox.release.as_uri()}


def _no_leftover_downloads(sandbox: Sandbox) -> bool:
    return not [p for p in sandbox.home.iterdir() if p.name.startswith(".localflow-")]


@pytest.mark.parametrize("hash_tools", [("sha256sum",), ("shasum",)], ids=["sha256sum", "shasum"])
def test_checksum_good_hash_installs(tmp_path: Path, hash_tools: tuple[str, ...]) -> None:
    if not all(shutil.which(tool) for tool in hash_tools):
        pytest.skip(f"{hash_tools} not available")
    sandbox = Sandbox(tmp_path, hash_tools=hash_tools)

    result = sandbox.run(env=_local_release_env(sandbox))

    assert result.returncode == 0, result.stderr
    assert f"Verified SHA-256 of {ASSET}." in result.stdout
    assert (sandbox.dest / "version.txt").read_text() == "v1"
    assert "not checksum-verified" not in result.stderr
    assert _no_leftover_downloads(sandbox)


@pytest.mark.parametrize("hash_tools", [("sha256sum",), ("shasum",)], ids=["sha256sum", "shasum"])
def test_checksum_tampered_tarball_fails_closed(
    tmp_path: Path, hash_tools: tuple[str, ...]
) -> None:
    if not all(shutil.which(tool) for tool in hash_tools):
        pytest.skip(f"{hash_tools} not available")
    sandbox = Sandbox(tmp_path, hash_tools=hash_tools)
    with (sandbox.release / ASSET).open("ab") as asset:
        asset.write(b"tampered")

    result = sandbox.run(env=_local_release_env(sandbox))

    assert result.returncode != 0
    assert f"SHA-256 mismatch for {ASSET}" in result.stderr
    assert "failed checksum verification; the download was removed" in result.stderr
    assert not sandbox.dest.exists()
    assert _no_leftover_downloads(sandbox)
    assert not any(call.startswith(("uv venv", "uv pip")) for call in sandbox.calls())


def test_checksum_missing_entry_fails_closed(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    (sandbox.release / "SHA256SUMS").write_text(f"{'a' * 64}  localflow-{VERSION}.zip\n")

    result = sandbox.run(env=_local_release_env(sandbox))

    assert result.returncode != 0
    assert f"SHA256SUMS has no valid entry for {ASSET}" in result.stderr
    assert not sandbox.dest.exists()
    assert _no_leftover_downloads(sandbox)


def test_checksum_missing_sums_file_fails_closed(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    (sandbox.release / "SHA256SUMS").unlink()

    result = sandbox.run(env=_local_release_env(sandbox))

    assert result.returncode != 0
    assert "SHA256SUMS" in result.stderr
    assert not sandbox.dest.exists()
    assert _no_leftover_downloads(sandbox)


def test_checksum_accepts_uppercase_binary_mode_and_crlf(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    digest = hashlib.sha256((sandbox.release / ASSET).read_bytes()).hexdigest().upper()
    (sandbox.release / "SHA256SUMS").write_bytes(f"{digest} *{ASSET}\r\n".encode())

    result = sandbox.run(env=_local_release_env(sandbox))

    assert result.returncode == 0, result.stderr
    assert f"Verified SHA-256 of {ASSET}." in result.stdout


def test_checksum_tampered_update_leaves_existing_install_untouched(tmp_path: Path) -> None:
    sandbox = Sandbox(tmp_path)
    env = _local_release_env(sandbox)
    assert sandbox.run(env=env).returncode == 0
    (sandbox.dest / "stale.txt").write_text("old")
    with (sandbox.release / ASSET).open("ab") as asset:
        asset.write(b"tampered")

    result = sandbox.run(env=env)

    assert result.returncode != 0
    assert (sandbox.dest / "stale.txt").read_text() == "old"
    assert (sandbox.dest / "version.txt").read_text() == "v1"
    assert _no_leftover_downloads(sandbox)
