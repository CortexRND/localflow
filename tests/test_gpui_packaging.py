"""LocalFlow Preview (native GPUI) packaging, installer and release publishing.

The builder and verifier run against fake native tools (cargo, readelf, ldd,
otool, install_name_tool, codesign, plutil) whose "binaries" are small shell
programs carrying their link metadata in a ``# FAKE {...}`` line, so search
paths, dependencies and symbol versions travel with the files through
staging, archiving and extraction. The installer and publisher use the real
``tar``. None of this is native macOS or Windows validation: the
gpui-package.yml macOS and Windows legs run the real tools (the Windows leg
also launches the window).
"""

import hashlib
import io
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tarfile
import textwrap
import tomllib
import zipfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
BASH = shutil.which("bash") if os.name != "nt" else None
pytestmark = pytest.mark.skipif(BASH is None, reason="requires Bash")
LINUX_ONLY = pytest.mark.skipif(not sys.platform.startswith("linux"), reason="the Linux builder uses GNU tar")
PREVIEW_ID = "com.cortexrnd.localflow.desktop-preview"
PLATFORMS = ["linux-x86_64", "macos-arm64", "macos-x86_64", "windows-x86_64"]
WINDOWS_SYSTEM_DLLS = ["kernel32.dll", "user32.dll", "gdi32.dll", "d3d11.dll", "dxgi.dll", "ole32.dll"]
VERSION = tomllib.loads((ROOT / "rust/lf-desktop/Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]

LINUX_BIN_NEEDED = [
    "libstdc++.so.6", "libsherpa-onnx-c-api.so", "libasound.so.2", "libxcb.so.1",
    "libxkbcommon.so.0", "libxkbcommon-x11.so.0", "libgcc_s.so.1", "libm.so.6",
    "libc.so.6", "ld-linux-x86-64.so.2",
]

TOOLS = r'''
import json, os, plistlib, sys
from pathlib import Path
name = Path(sys.argv[0]).name
a = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write(json.dumps([name, *a]) + "\n")
fault = set(os.environ.get("FAULTS", "").split(","))
SYSTEM = {"ld-linux-x86-64.so.2", "libc.so.6", "libm.so.6", "libdl.so.2", "librt.so.1",
          "libpthread.so.0", "libgcc_s.so.1", "libstdc++.so.6", "libasound.so.2",
          "libxcb.so.1", "libxkbcommon.so.0", "libxkbcommon-x11.so.0"}

def meta(path):
    for line in Path(path).read_text().splitlines():
        if line.startswith("# FAKE "):
            return json.loads(line[7:])
    sys.exit(f"{name}: not a fixture binary: {path}")

def write_meta(path, m):
    p = Path(path)
    lines = ["# FAKE " + json.dumps(m) if l.startswith("# FAKE ") else l for l in p.read_text().splitlines()]
    p.write_text("\n".join(lines) + "\n")

def program(m, version):
    return "\n".join([
        "#!/bin/sh", "# FAKE " + json.dumps(m),
        'if [ -n "${LAUNCH_LOG:-}" ]; then printf "%s\\n" "launched $#:$*" >> "$LAUNCH_LOG"; fi',
        'case "$1" in',
        f'  --version) echo "lf-desktop {version}" ;;',
        '  --help) echo "lf-desktop --page --engine --sandbox --data-dir" ;;',
        '  --speech-helper) [ "$#" -eq 1 ] || exit 2; [ -z "${LOCALFLOW_SANDBOX:-}" ] || exit 2 ;;',
        '  *) exit 2 ;;',
        "esac", ""])

if name == "uname":
    print(os.environ["FAKE_OS"] if a == ["-s"] else os.environ["FAKE_ARCH"])
elif name == "cargo":
    if a[0] == "build":
        release = Path(os.environ["CARGO_TARGET_DIR"]) / "release"
        release.mkdir(parents=True, exist_ok=True)
        cleaned = any(json.loads(l)[:2] == ["cargo", "clean"] for l in open(os.environ["CALLS"]))
        version = os.environ.get("FAKE_VERSION", os.environ["REAL_VERSION"])
        mac = os.environ["FAKE_OS"] == "Darwin"
        win = os.environ["FAKE_OS"].startswith("MINGW")
        ext = "dylib" if mac else "dll" if win else "so"
        if win:
            libs = ["sherpa-onnx-c-api", "sherpa-onnx-cxx-api", "onnxruntime", "onnxruntime_providers_shared"]
        else:
            libs = ["libsherpa-onnx-c-api", "libonnxruntime"] + ([] if mac else ["libsherpa-onnx-cxx-api"])
        if "MISSING_ORT" in fault:
            libs.remove("onnxruntime" if win else "libonnxruntime")
        if win:
            imports = ["KERNEL32.dll", "USER32.dll", "d3d11.dll", "sherpa-onnx-c-api.dll", "api-ms-win-core-synch-l1-2-0.dll"]
            if "VC_RUNTIME" in fault:
                imports.append("VCRUNTIME140.dll")
            if "BAD_IMPORT" in fault:
                imports.append("libomp140.x86_64.dll")
            exe = {"imports": imports, "format": "COFF-i386" if "WRONG_ARCH" in fault else "COFF-x86-64"}
        elif mac:
            sck = "/System/Library/Frameworks/ScreenCaptureKit.framework/Versions/A/ScreenCaptureKit"
            deps = ["/build/target/release/libsherpa-onnx-c-api.dylib", "/usr/lib/libSystem.B.dylib",
                    "/System/Library/Frameworks/Metal.framework/Versions/A/Metal"]
            weak = []
            (weak if "WEAK_SCK" in fault else deps).append(sck)
            if "BAD_DEP" in fault:
                deps.append("/opt/homebrew/lib/libomp.dylib")
            # rustc/cc record MACOSX_DEPLOYMENT_TARGET as the image minimum.
            minos = os.environ.get("MACOSX_DEPLOYMENT_TARGET", "11.0")
            if "IGNORE_DEPLOYMENT_TARGET" in fault:
                minos = "11.0"
            elif "FRAMEWORK_ONLY_TARGET" in fault:
                minos = "12.3"
            exe = {"rpaths": ["@loader_path", "/build/native libs"], "deps": deps, "weak": weak, "minos": minos}
        else:
            needed = list(os.environ["BIN_NEEDED"].split(","))
            if "BAD_NEEDED" in fault:
                needed.append("libunbundled.so.1")
            verneed = ["GLIBC_2.2.5", "GLIBC_2.35", "GLIBCXX_3.4.29", "CXXABI_1.3.9"]
            if "NEW_GLIBC" in fault:
                verneed.append("GLIBC_2.39")
            runpath = "/build/target/release" if "BAD_RUNPATH" in fault else "$ORIGIN:$ORIGIN/../lib"
            exe = {"needed": needed, "runpath": runpath, "verneed": verneed}
        binary = release / ("lf-desktop.exe" if win else "lf-desktop")
        binary.write_text(program(exe, version))
        binary.chmod(0o755)
        if "PRUNED" in fault and not cleaned:
            libs = []
        for lib in libs:
            path = release / f"{lib}.{ext}"
            if win:
                m = {"format": "COFF-x86-64",
                     "imports": ["KERNEL32.dll"] + (["onnxruntime.dll"] if lib.startswith("sherpa") else [])}
            elif mac:
                m = {"id": f"/build/sdk/{lib}.dylib", "rpaths": ["/build/sdk/lib"],
                     "deps": ["@rpath/libonnxruntime.dylib", "/usr/lib/libc++.1.dylib"],
                     "minos": os.environ.get("FAKE_ORT_MINOS", "13.3") if lib == "libonnxruntime" else "10.15"}
            else:
                deps = {"libsherpa-onnx-c-api": ["libonnxruntime.so"],
                        "libsherpa-onnx-cxx-api": ["libsherpa-onnx-c-api.so", "libonnxruntime.so"]}.get(lib, [])
                m = {"needed": deps + ["libpthread.so.0", "libstdc++.so.6", "libc.so.6"],
                     "runpath": "$ORIGIN", "verneed": ["GLIBC_2.17", "GLIBCXX_3.4.22"]}
            path.write_text("binary\n# FAKE " + json.dumps(m) + "\n")
elif name == "readelf":
    m = meta(a[-1])
    if a[0] == "-d":
        for n in m["needed"]:
            print(f" 0x0000000000000001 (NEEDED)             Shared library: [{n}]")
        if m.get("runpath"):
            print(f" 0x000000000000001d (RUNPATH)            Library runpath: [{m['runpath']}]")
    else:
        print("Version definition section '.gnu.version_d' contains 1 entry:")
        print("  0x0000: Rev: 1  Flags: BASE  Index: 1  Cnt: 1  Name: GLIBC_9.99")
        print("Version needs section '.gnu.version_r' contains 1 entry:")
        print(" 0x0000: Version: 1  File: libc.so.6  Cnt: 1")
        for v in m["verneed"]:
            print(f"  0x0010:   Name: {v}  Flags: none  Version: 3")
elif name == "ldd":
    target = Path(a[-1])
    seen, queue = set(), [(target, n) for n in meta(target)["needed"]]
    while queue:
        owner, lib = queue.pop(0)
        if lib in seen:
            continue
        seen.add(lib)
        found = None
        for entry in meta(owner)["runpath"].split(":"):
            candidate = Path(entry.replace("$ORIGIN", str(owner.parent))) / lib
            if candidate.is_file():
                found = candidate
                break
        if found and "LDD_BUILD_TREE" in fault:
            print(f"\t{lib} => /build/target/release/{lib} (0x0)")
        elif found and "LDD_SPACED_BUILD_TREE" in fault:
            print(f"\t{lib} => /build/spaced dir (x)/{lib} (0x00007f0000000000)")
        elif lib == "libasound.so.2" and "LDD_SPACED_SYSTEM" in fault:
            print(f"\t{lib} => /opt/odd dir (x)/{lib} (0x00007f0000000000)")
        elif found:
            print(f"\t{lib} => {found} (0x0)")
            queue += [(found, n) for n in meta(found)["needed"]]
        elif lib in SYSTEM:
            print(f"\t{lib} => /lib/x86_64-linux-gnu/{lib} (0x0)")
        else:
            print(f"\t{lib} => not found")
elif name == "llvm-readobj":
    m = meta(a[-1])
    print(f"File: {a[-1]}\nFormat: {m['format']}\nArch: x86_64\nAddressSize: 64bit")
    for dll in m["imports"]:
        print(f"Import {{\n  Name: {dll}\n  ImportLookupTableRVA: 0x1000\n  Symbol: Fn (0)\n}}")
elif name == "bsdtar":
    import zipfile
    mode = a[0] if a[0] != "--format" else a[2]
    if mode == "-cf":
        with zipfile.ZipFile(a[3] if a[0] == "--format" else a[1], "w") as z:
            for root in a[4 if a[0] == "--format" else 2:]:
                for p in sorted([Path(root), *Path(root).rglob("*")]):
                    z.write(p, str(p) + ("/" if p.is_dir() else ""))
    elif mode in ("-tf", "-tvf"):
        for info in zipfile.ZipFile(a[1]).infolist():
            kind = "d" if info.is_dir() else ("l" if (info.external_attr >> 28) == 0xA else "-")
            print(f"{kind}rw-r--r--  0 0      0         {info.file_size} Jan  1 00:00 {info.filename}" if mode == "-tvf"
                  else info.filename)
    elif mode == "-xf":
        with zipfile.ZipFile(a[1]) as z:
            z.extractall(a[a.index("-C") + 1])
            for info in z.infolist():
                if not info.is_dir() and (info.external_attr >> 16) & 0o111:
                    (Path(a[a.index("-C") + 1]) / info.filename).chmod(0o755)
elif name == "desktop-file-validate":
    sys.exit(0 if Path(a[-1]).read_text().startswith("[Desktop Entry]\n") else 1)
elif name == "otool":
    m = meta(a[-1])
    if a[0] == "-l":
        for r in m["rpaths"]:
            print(f"Load command 9\n          cmd LC_RPATH\n      cmdsize 32\n         path {r} (offset 12)")
        print(f"Load command 2\n      cmd LC_BUILD_VERSION\n  cmdsize 32\n platform 1\n    minos {m['minos']}\n      sdk 14.0")
        for kind, paths in (("LC_LOAD_DYLIB", m["deps"]), ("LC_LOAD_WEAK_DYLIB", m.get("weak", []))):
            for d in paths:
                print(f"Load command 12\n          cmd {kind}\n      cmdsize 56\n         name {d} (offset 24)")
    elif a[0] == "-D":
        print(a[-1] + ":\n" + m["id"])
    else:
        print(a[-1] + ":")
        for d in ([m["id"]] if "id" in m else []) + m["deps"] + m.get("weak", []):
            print(f"\t{d} (compatibility version 1.0.0, current version 1.0.0)")
elif name == "install_name_tool":
    m, flag, path = meta(a[-1]), a[0], a[-1]
    if flag == "-delete_rpath":
        m["rpaths"].remove(a[1])
    elif flag == "-add_rpath":
        m["rpaths"].append(a[1])
    elif flag == "-id":
        m["id"] = a[1]
    elif flag == "-change":
        m["deps"] = [a[2] if d == a[1] else d for d in m["deps"]]
    write_meta(path, m)
elif name == "codesign":
    if "--verify" in a and "FAIL_VERIFY" in fault:
        print("invalid signature", file=sys.stderr)
        sys.exit(1)
    if "--requirements" in a:
        seal = Path(a[-1]) / "Contents/_CodeSignature/CodeResources"
        seal.parent.mkdir(parents=True, exist_ok=True)
        seal.write_text(a[a.index("--requirements") + 1].lstrip("="))
    if a[:2] == ["-d", "-r-"]:
        print("Executable=" + a[-1], file=sys.stderr)
        print((Path(a[-1]) / "Contents/_CodeSignature/CodeResources").read_text())
elif name == "plutil":
    p = Path(a[-1])
    if a[0] == "-create":
        p.write_bytes(plistlib.dumps({}))
    elif a[0] == "-insert":
        v = plistlib.loads(p.read_bytes())
        v[a[1]] = (a[3] == "YES") if a[2] == "-bool" else a[3]
        p.write_bytes(plistlib.dumps(v))
    elif a[0] == "-lint":
        plistlib.loads(p.read_bytes())
    elif a[0] == "-extract":
        v = plistlib.loads(p.read_bytes())
        if a[1] not in v:
            sys.exit(1)
        print(v[a[1]])
'''
FAKE_TOOLS = ("uname", "cargo", "readelf", "ldd", "desktop-file-validate", "otool",
              "install_name_tool", "codesign", "plutil", "xattr", "open", "llvm-readobj", "bsdtar")


def _calls(env) -> list[list[str]]:
    path = Path(env["CALLS"])
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


@pytest.fixture
def fake(tmp_path: Path):
    """Return (home, env) with fake native tools first on PATH."""
    tools = tmp_path / "tools"
    tools.mkdir()
    for tool in FAKE_TOOLS:
        p = tools / tool
        p.write_text(f"#!{sys.executable}\n" + TOOLS)
        p.chmod(0o755)
    if shutil.which("sha256sum") is None:
        (tools / "sha256sum").write_text('#!/bin/bash\nexec shasum -a 256 "$@"\n')
        (tools / "sha256sum").chmod(0o755)
    home = tmp_path / "home with space"
    home.mkdir()
    env = dict(os.environ, HOME=str(home), CALLS=str(tmp_path / "calls"), FAKE_OS="Linux",
               FAKE_ARCH="x86_64", PATH=f"{tools}{os.pathsep}{os.environ['PATH']}",
               CARGO_TARGET_DIR=str(tmp_path / "target"), REAL_VERSION=VERSION,
               BIN_NEEDED=",".join(LINUX_BIN_NEEDED), TMPDIR=str(tmp_path))
    for key in ("CARGO_BUILD_TARGET", "MACOSX_DEPLOYMENT_TARGET", "LOCALFLOW_PREVIEW_BASE_URL", "DISPLAY", "WAYLAND_DISPLAY",
                "LOCALFLOW_SANDBOX", "FAULTS", "SYSTEMROOT", "WINDIR", "LLVM_READOBJ", "RUSTFLAGS"):
        env.pop(key, None)
    return home, env


def _run(script: str, env, *args, cwd=None):
    return subprocess.run([BASH, str(ROOT / "scripts" / script), *map(str, args)], env=env, cwd=cwd,
                          check=False, text=True, capture_output=True)


def _build(env, out: Path, *faults: str):
    env = dict(env, FAULTS=",".join(faults))
    return _run("build_gpui_package.sh", env, out)


def _asset(platform: str, version: str = VERSION) -> str:
    ext = "zip" if platform.startswith("windows") else "tar.gz"
    return f"LocalFlow-Preview-{version}-{platform}.{ext}"


# --- builder and verifier -------------------------------------------------


@LINUX_ONLY
def test_linux_builder_bundles_verifies_and_records_the_abi(fake, tmp_path):
    _, env = fake
    out = tmp_path / "out"
    result = _build(env, out)
    assert result.returncode == 0, result.stderr + result.stdout
    name = _asset("linux-x86_64")
    assert sorted(p.name for p in out.iterdir()) == [name, f"{name}.sha256"]
    digest = hashlib.sha256((out / name).read_bytes()).hexdigest()
    assert (out / f"{name}.sha256").read_text() == f"{digest}  {name}\n"
    with tarfile.open(out / name) as archive:
        members = {m.name: m for m in archive.getmembers()}
        assert all(n == "localflow-preview" or n.startswith("localflow-preview/") for n in members)
        assert all(m.isfile() or m.isdir() for m in members.values())
        assert {m.uid for m in members.values()} == {0}
        manifest = json.loads(archive.extractfile("localflow-preview/MANIFEST.json").read())
        entry = archive.extractfile("localflow-preview/share/applications/localflow-preview.desktop").read().decode()
        files = {n[len("localflow-preview/"):]: hashlib.sha256(archive.extractfile(m).read()).hexdigest()
                 for n, m in members.items() if m.isfile() and not n.endswith("/MANIFEST.json")}
    assert manifest["name"] == "localflow-preview" and manifest["version"] == VERSION
    assert (manifest["os"], manifest["arch"]) == ("linux", "x86_64")
    # The highest version needed across the binary AND the bundled libraries;
    # a version *definition* (GLIBC_9.99 in the fake) is not a requirement.
    assert (manifest["glibc"], manifest["glibcxx"], manifest["cxxabi"]) == ("2.35", "3.4.29", "1.3.9")
    assert manifest["system_libraries"].split() == sorted(
        (set(LINUX_BIN_NEEDED) | {"libpthread.so.0"}) - {"libsherpa-onnx-c-api.so"})
    assert manifest["files"] == files
    assert {"bin/lf-desktop", "lib/libsherpa-onnx-c-api.so", "lib/libonnxruntime.so",
            "lib/libsherpa-onnx-cxx-api.so", "share/icons/localflow-preview.png", "README.txt"} <= set(files)
    assert "Exec=localflow-preview\n" in entry and "--engine" not in entry
    calls = _calls(env)
    builds = [c for c in calls if c[:2] == ["cargo", "build"]]
    assert builds == [["cargo", "build", "--manifest-path", str(ROOT / "rust/lf-desktop/Cargo.toml"), "--locked",
                       "--release", "--features", "runtime", "--bin", "lf-desktop"]]
    # The extracted copy was smoke-tested: version, help, sandbox refusal, helper EOF.
    assert not [c for c in calls if c[0] == "patchelf"]


@LINUX_ONLY
@pytest.mark.parametrize("fault,message", [
    ("MISSING_ORT", "libonnxruntime.so is missing"),
    ("BAD_NEEDED", "needs libunbundled.so.1"),
    ("BAD_RUNPATH", "search path is '/build/target/release'"),
    ("NEW_GLIBC", "needs glibc 2.39, newer than the documented 2.35"),
    ("LDD_BUILD_TREE", "not the bundled copy"),
    # Full resolved paths, whitespace and parentheses included, are judged.
    ("LDD_SPACED_BUILD_TREE",
     "libsherpa-onnx-c-api.so resolves to /build/spaced dir (x)/libsherpa-onnx-c-api.so, not the bundled copy"),
    ("LDD_SPACED_SYSTEM",
     "libasound.so.2 resolves outside the system library directories: /opt/odd dir (x)/libasound.so.2"),
])
def test_linux_builder_fails_closed_and_keeps_the_previous_output(fake, tmp_path, fault, message):
    _, env = fake
    out = tmp_path / "out"
    out.mkdir()
    previous = out / _asset("linux-x86_64")
    previous.write_text("previous release")
    result = _build(env, out, fault)
    assert result.returncode != 0
    assert message in result.stderr, result.stderr
    assert previous.read_text() == "previous release"
    assert sorted(p.name for p in out.iterdir()) == [previous.name], "no staging left behind"


@pytest.fixture
def windows(fake, tmp_path):
    """The fake environment as Git Bash on Windows x64, with a fake System32."""
    home, env = fake
    system32 = tmp_path / "Windows" / "System32"
    system32.mkdir(parents=True)
    for dll in WINDOWS_SYSTEM_DLLS:
        (system32 / dll).write_text("system dll")
    env.update(FAKE_OS="MINGW64_NT-10.0-20348", SYSTEMROOT=str(tmp_path / "Windows"))
    return home, env


@LINUX_ONLY
def test_windows_builder_zips_a_flat_static_crt_folder_and_verifies_it(windows, tmp_path):
    _, env = windows
    out = tmp_path / "out"
    result = _build(env, out)
    assert result.returncode == 0, result.stderr + result.stdout
    name = _asset("windows-x86_64")
    assert name == f"LocalFlow-Preview-{VERSION}-windows-x86_64.zip"
    assert sorted(p.name for p in out.iterdir()) == [name, f"{name}.sha256"]
    digest = hashlib.sha256((out / name).read_bytes()).hexdigest()
    assert (out / f"{name}.sha256").read_text() == f"{digest}  {name}\n"
    with zipfile.ZipFile(out / name) as archive:
        names = archive.namelist()
        manifest = json.loads(archive.read("LocalFlow Preview/MANIFEST.json"))
        files = {n[len("LocalFlow Preview/"):]: hashlib.sha256(archive.read(n)).hexdigest()
                 for n in names if not n.endswith("/") and not n.endswith("/MANIFEST.json")}
        readme = archive.read("LocalFlow Preview/README.txt").decode()
    assert all(n == "LocalFlow Preview/" or n.startswith("LocalFlow Preview/") for n in names)
    assert sorted(files) == ["README.txt", "lf-desktop.exe", "onnxruntime.dll", "onnxruntime_providers_shared.dll",
                             "sherpa-onnx-c-api.dll", "sherpa-onnx-cxx-api.dll"]
    assert manifest["files"] == files
    assert (manifest["name"], manifest["version"], manifest["os"], manifest["arch"]) == (
        "localflow-preview", VERSION, "windows", "x86_64")
    assert manifest["crt"] == "static"
    # Bundled DLLs are not system libraries; api-ms-win-* sets resolve in the OS.
    assert manifest["system_libraries"].split() == [
        "api-ms-win-core-synch-l1-2-0.dll", "d3d11.dll", "kernel32.dll", "user32.dll"]
    assert "install.ps1" in readme and "Visual C++ Redistributable" in readme
    calls = _calls(env)
    assert [c for c in calls if c[:2] == ["cargo", "build"]] == [[
        "cargo", "build", "--manifest-path", str(ROOT / "rust/lf-desktop/Cargo.toml"), "--locked",
        "--release", "--features", "runtime", "--bin", "lf-desktop"]]
    # The extracted copy was inspected and smoke-tested, not the build tree.
    inspected = [Path(c[-1]) for c in calls if c[0] == "llvm-readobj"]
    assert {p.name for p in inspected} == set(files) - {"README.txt"}
    assert all("localflow-preview-verify." in str(p) for p in inspected[len(files) - 1:])


@LINUX_ONLY
def test_windows_builder_links_the_crt_statically(windows, tmp_path):
    _, env = windows
    env["RUSTFLAGS"] = "-D warnings"
    bin_dir = tmp_path / "rustflags-bin"
    bin_dir.mkdir()
    real_cargo = shutil.which("cargo", path=env["PATH"])
    (bin_dir / "cargo").write_text(f'#!/bin/bash\nprintf "%s\\n" "$RUSTFLAGS" >> "{tmp_path}/rustflags"\n'
                                   f'exec "{real_cargo}" "$@"\n')
    (bin_dir / "cargo").chmod(0o755)
    env["PATH"] = f"{bin_dir}{os.pathsep}{env['PATH']}"
    result = _build(env, tmp_path / "out")
    assert result.returncode == 0, result.stderr
    assert set((tmp_path / "rustflags").read_text().splitlines()) == {"-D warnings -C target-feature=+crt-static"}


@LINUX_ONLY
@pytest.mark.parametrize("fault,message", [
    ("MISSING_ORT", "onnxruntime.dll is missing"),
    ("VC_RUNTIME", "lf-desktop.exe needs vcruntime140.dll (the Visual C++ runtime)"),
    ("BAD_IMPORT", "lf-desktop.exe needs libomp140.x86_64.dll, which is neither bundled nor a Windows system DLL"),
    ("WRONG_ARCH", "lf-desktop.exe is not an x86-64 PE image"),
])
def test_windows_builder_fails_closed_and_keeps_the_previous_output(windows, tmp_path, fault, message):
    _, env = windows
    out = tmp_path / "out"
    out.mkdir()
    previous = out / _asset("windows-x86_64")
    previous.write_text("previous release")
    result = _build(env, out, fault)
    assert result.returncode != 0
    assert message in result.stderr, result.stderr
    assert previous.read_text() == "previous release"
    assert sorted(p.name for p in out.iterdir()) == [previous.name], "no staging left behind"


@LINUX_ONLY
def test_windows_verifier_rejects_extra_files_and_other_hosts(windows, tmp_path):
    _, env = windows
    out = tmp_path / "out"
    assert _build(env, out).returncode == 0
    name = _asset("windows-x86_64")
    with zipfile.ZipFile(out / name) as z:
        entries = {n: z.read(n) for n in z.namelist() if not n.endswith("/")}
    tampered = tmp_path / "tampered" / name
    tampered.parent.mkdir()
    with zipfile.ZipFile(tampered, "w") as z:
        for n, data in entries.items():
            z.writestr(n, data)
        z.writestr("LocalFlow Preview/plugins/extra.dll", "x")
    result = _run("verify_gpui_package.sh", env, tampered)
    assert result.returncode != 0 and "unexpected files in the Windows package" in result.stderr, result.stderr
    linux = dict(env, FAKE_OS="Linux")
    result = _run("verify_gpui_package.sh", linux, out / name)
    assert result.returncode != 0 and "verify it on that platform (this is linux-x86_64)" in result.stderr


@LINUX_ONLY
def test_linux_archive_verifies_from_a_temporary_directory_with_spaces(fake, tmp_path):
    # The verifier extracts into $TMPDIR; bundled libraries then resolve to
    # paths containing whitespace and parentheses, which must still match.
    _, env = fake
    spaced = tmp_path / "spaced temporary (dir)"
    spaced.mkdir()
    env["TMPDIR"] = str(spaced)
    out = tmp_path / "out"
    result = _build(env, out)
    assert result.returncode == 0, result.stderr + result.stdout
    archive = out / _asset("linux-x86_64")
    result = _run("verify_gpui_package.sh", env, archive)
    assert result.returncode == 0, result.stderr
    assert f"Verified {archive.name}" in result.stdout


@LINUX_ONLY
def test_builder_refuses_a_version_mismatch(fake, tmp_path):
    _, env = fake
    env["FAKE_VERSION"] = "9.9.9"
    result = _build(env, tmp_path / "out")
    assert result.returncode != 0
    assert f"expected 'lf-desktop {VERSION}'" in result.stderr


def test_builder_refuses_cross_targets_relative_output_and_other_platforms(fake, tmp_path):
    _, env = fake
    result = _run("build_gpui_package.sh", dict(env, CARGO_BUILD_TARGET="aarch64-unknown-linux-gnu"), tmp_path)
    assert result.returncode == 1 and "Unset CARGO_BUILD_TARGET" in result.stderr
    result = _run("build_gpui_package.sh", env, "relative/out")
    assert result.returncode == 2
    result = _run("build_gpui_package.sh", dict(env, FAKE_ARCH="aarch64"), tmp_path / "out")
    assert result.returncode == 1 and "Linux x86_64, macOS arm64/x86_64 and Windows x86_64 only" in result.stderr
    result = _run("build_gpui_package.sh", dict(env, FAKE_OS="MINGW64_NT-10.0-26100", FAKE_ARCH="aarch64"), tmp_path / "out")
    assert result.returncode == 1 and "Windows x86_64 only" in result.stderr
    assert not [c for c in _calls(env) if c[0] == "cargo"]


@LINUX_ONLY
def test_builder_restores_pruned_native_outputs_only_when_needed(fake, tmp_path):
    _, env = fake
    target = Path(env["CARGO_TARGET_DIR"])
    sdks = target / "sherpa-onnx-prebuilt"
    complete, partial = sdks / "sherpa-complete", sdks / "sherpa-partial"
    for sdk in (complete, partial):
        (sdk / "lib").mkdir(parents=True)
        (sdk / "lib/libsherpa-onnx-c-api.so").write_text("sdk")
    (complete / "lib/libonnxruntime.so").write_text("sdk")
    outside = tmp_path / "outside-sdk"
    (outside / "lib").mkdir(parents=True)
    (sdks / "linked").symlink_to(outside, target_is_directory=True)
    (sdks / "sherpa.tar.bz2").write_text("archive")
    # A cached build whose copied libraries were pruned: the sys crate is rebuilt.
    result = _build(env, tmp_path / "out", "PRUNED")
    assert result.returncode == 0, result.stderr
    assert complete.is_dir() and not partial.exists() and outside.is_dir() and (sdks / "sherpa.tar.bz2").exists()
    cleans = [c for c in _calls(env) if c[:2] == ["cargo", "clean"]]
    assert cleans == [["cargo", "clean", "--manifest-path", str(ROOT / "rust/lf-desktop/Cargo.toml"),
                       "--release", "-p", "sherpa-onnx-sys"]]
    # Libraries present: no clean, no rebuild of the sys crate.
    Path(env["CALLS"]).unlink()
    assert _build(env, tmp_path / "out").returncode == 0
    assert not [c for c in _calls(env) if c[:2] == ["cargo", "clean"]]


@LINUX_ONLY
def test_builder_defaults_to_the_desktop_target_dir_and_respects_an_explicit_one(fake, tmp_path):
    _, env = fake
    env.pop("CARGO_TARGET_DIR")
    script = (ROOT / "scripts/build_gpui_package.sh").read_text(encoding="utf-8")
    assert 'CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/rust/lf-desktop/target}"' in script
    explicit = tmp_path / "explicit target"
    assert _build(dict(env, CARGO_TARGET_DIR=str(explicit)), tmp_path / "out").returncode == 0
    assert (explicit / "release/lf-desktop").is_file()


def _mac_env(env, arch="arm64"):
    return dict(env, FAKE_OS="Darwin", FAKE_ARCH=arch)


@LINUX_ONLY
@pytest.mark.parametrize("arch", ["arm64", "x86_64"])
def test_macos_builder_relocates_signs_and_keeps_a_separate_identity(fake, tmp_path, arch):
    _, env = fake
    env = _mac_env(env, arch)
    out = tmp_path / "out"
    result = _build(env, out)
    assert result.returncode == 0, result.stderr + result.stdout
    name = _asset(f"macos-{arch}")
    assert sorted(p.name for p in out.iterdir()) == [name, f"{name}.sha256"]
    extracted = tmp_path / "x"
    with tarfile.open(out / name) as archive:
        assert {m.name.split("/")[0] for m in archive.getmembers()} == {"LocalFlow Preview.app"}
        archive.extractall(extracted, filter="data")
    app = extracted / "LocalFlow Preview.app"
    plist = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    assert plist["CFBundleIdentifier"] == PREVIEW_ID
    assert plist["CFBundleExecutable"] == "lf-desktop"
    assert plist["CFBundleName"] == "LocalFlow Preview"
    assert plist["CFBundleShortVersionString"] == VERSION
    assert plist["LSMinimumSystemVersion"] == "13.3", "highest minos across executable and bundled dylibs"
    assert "LocalFlowRuntime" not in plist
    for key in ("NSMicrophoneUsageDescription", "NSAccessibilityUsageDescription", "NSInputMonitoringUsageDescription"):
        assert plist[key]
    files = sorted(str(p.relative_to(app)) for p in app.rglob("*") if p.is_file())
    assert files == ["Contents/Frameworks/libonnxruntime.dylib", "Contents/Frameworks/libsherpa-onnx-c-api.dylib",
                     "Contents/Info.plist", "Contents/MacOS/lf-desktop", "Contents/Resources/LocalFlow.icns",
                     "Contents/_CodeSignature/CodeResources"]
    assert (app / "Contents/Resources/LocalFlow.icns").read_bytes() == (ROOT / "packaging/macos/LocalFlow.icns").read_bytes()
    exe = json.loads((app / "Contents/MacOS/lf-desktop").read_text().splitlines()[1][7:])
    assert exe["rpaths"] == ["@executable_path/../Frameworks"]
    assert "@rpath/libsherpa-onnx-c-api.dylib" in exe["deps"]
    lib = json.loads((app / "Contents/Frameworks/libsherpa-onnx-c-api.dylib").read_text().splitlines()[1][7:])
    assert lib["id"] == "@rpath/libsherpa-onnx-c-api.dylib" and lib["rpaths"] == ["@loader_path"]
    calls = _calls(env)
    signs = [c for c in calls if c[:5] == ["codesign", "--force", "-s", "-", "--identifier"]]
    assert len(signs) == 1 and signs[0][5:8] == [PREVIEW_ID, "--requirements", f'=designated => identifier "{PREVIEW_ID}"']
    lib_signs = [c for c in calls if c[:4] == ["codesign", "--force", "-s", "-"] and c[-1].endswith(".dylib")]
    assert len(lib_signs) == 2
    assert calls.index(lib_signs[-1]) < calls.index(signs[0]), "nested libraries are signed before the bundle"
    verifies = [c for c in calls if c[:4] == ["codesign", "--verify", "--deep", "--strict"]]
    assert any("/localflow-preview-verify." in c[-1] for c in verifies), "signature re-verified after extraction"
    assert ["xattr", "-cr"] in [c[:2] for c in calls]


@LINUX_ONLY
@pytest.mark.parametrize("faults,ort_minos,expected", [
    ((), "13.3", "13.3"),          # a bundled dylib's own minimum is the highest
    ((), "10.15", "13.0"),         # the deployment floor (ScreenCaptureKit audio capture API)
    (("WEAK_SCK",), "10.15", "13.0"),  # a weak link is not listed as strongly linked
])
def test_macos_minimum_covers_strongly_linked_frameworks(fake, tmp_path, faults, ort_minos, expected):
    _, env = fake
    env = dict(_mac_env(env), FAKE_ORT_MINOS=ort_minos)
    out = tmp_path / "out"
    result = _build(env, out, *faults)
    assert result.returncode == 0, result.stderr + result.stdout
    with tarfile.open(out / _asset("macos-arm64")) as archive:
        info = archive.extractfile("LocalFlow Preview.app/Contents/Info.plist").read()
        exe = archive.extractfile("LocalFlow Preview.app/Contents/MacOS/lf-desktop").read().decode()
    assert plistlib.loads(info)["LSMinimumSystemVersion"] == expected
    recorded = json.loads(exe.splitlines()[1][7:])["minos"]
    assert recorded == "13.0", "the builder exports MACOSX_DEPLOYMENT_TARGET=13.0"
    assert "Strongly linked system frameworks:" in result.stdout
    assert ("ScreenCaptureKit" in result.stdout.split("Strongly linked system frameworks:")[1].splitlines()[0]) \
        == (not faults)


@LINUX_ONLY
@pytest.mark.parametrize("weak,expected", [(False, "12.3"), (True, "11.0")])
def test_macos_minimum_counts_only_strongly_linked_framework_introductions(fake, tmp_path, weak, expected):
    # The minimum helper alone: an 11.0 image linking ScreenCaptureKit strongly
    # needs 12.3 (framework introduction); weakly linked, it does not.
    _, env = fake
    env = _mac_env(env)
    app = tmp_path / "LocalFlow Preview.app"
    (app / "Contents/MacOS").mkdir(parents=True)
    (app / "Contents/Frameworks").mkdir()
    sck = "/System/Library/Frameworks/ScreenCaptureKit.framework/Versions/A/ScreenCaptureKit"
    meta = {"rpaths": [], "deps": [] if weak else [sck], "weak": [sck] if weak else [], "minos": "11.0"}
    (app / "Contents/MacOS/lf-desktop").write_text("#!/bin/sh\n# FAKE " + json.dumps(meta) + "\n")
    result = _run("verify_gpui_package.sh", env, "--macos-minos", app)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == expected


@LINUX_ONLY
def test_macos_executable_built_only_for_the_framework_floor_fails_closed(fake, tmp_path):
    # 12.3 satisfies ScreenCaptureKit's introduction but not the audio capture
    # API (13.0) lf-meetings calls, so the verifier refuses the archive.
    _, env = fake
    out = tmp_path / "out"
    result = _build(dict(_mac_env(env), FAKE_ORT_MINOS="10.15"), out, "FRAMEWORK_ONLY_TARGET")
    assert result.returncode != 0
    assert "records minimum macOS 12.3, below the 13.0 its ScreenCaptureKit audio capture needs" in result.stderr
    assert not (out / _asset("macos-arm64")).exists()


@LINUX_ONLY
def test_macos_build_that_ignores_the_deployment_target_fails_closed(fake, tmp_path):
    # The image strongly links ScreenCaptureKit (12.3) but records 11.0: dyld
    # would refuse to launch it on 11.x/12.0-12.2, so the claim is rejected.
    _, env = fake
    out = tmp_path / "out"
    out.mkdir()
    (out / _asset("macos-arm64")).write_text("previous release")
    result = _build(dict(_mac_env(env), FAKE_ORT_MINOS="10.15"), out, "IGNORE_DEPLOYMENT_TARGET")
    assert result.returncode != 0
    assert "links ScreenCaptureKit (macOS 12.3+) strongly but records minimum macOS 11.0" in result.stderr
    assert (out / _asset("macos-arm64")).read_text() == "previous release"


@pytest.mark.parametrize("target,ok", [
    ("11.0", False), ("12.2", False), ("banana", False),
    # 12.3 introduced the ScreenCaptureKit framework but not the audio capture
    # API lf-meetings configures unconditionally, so it is not enough.
    ("12.3", False), ("12.9", False),
    ("13.0", True), ("13.1", True), ("14.0", True),
])
def test_macos_builder_refuses_a_deployment_target_below_the_api_floor(fake, tmp_path, target, ok):
    _, env = fake
    result = _build(dict(_mac_env(env), MACOSX_DEPLOYMENT_TARGET=target), tmp_path / "out")
    if ok:
        if not sys.platform.startswith("linux"):
            return
        assert result.returncode == 0, result.stderr
        assert any(c[:2] == ["cargo", "build"] for c in _calls(env))
        with tarfile.open(tmp_path / "out" / _asset("macos-arm64")) as archive:
            info = plistlib.loads(archive.extractfile("LocalFlow Preview.app/Contents/Info.plist").read())
        # The bundled ORT fixture records 13.3, so that or the explicit target wins.
        assert info["LSMinimumSystemVersion"] == ("14.0" if target == "14.0" else "13.3")
    else:
        assert result.returncode == 1 and "below the package floor 13.0" in result.stderr
        assert not [c for c in _calls(env) if c[0] == "cargo"]


def test_builder_deployment_floor_covers_the_screencapturekit_audio_api():
    build = (ROOT / "scripts/build_gpui_package.sh").read_text(encoding="utf-8")
    verify = (ROOT / "scripts/verify_gpui_package.sh").read_text(encoding="utf-8")
    floor = re.search(r'^MACOS_DEPLOYMENT_FLOOR="([0-9.]+)"$', build, re.MULTILINE).group(1)
    table = dict(e.split("=") for e in re.search(r"MACOS_FRAMEWORK_FLOORS=\((.*?)\)", verify).group(1).split())
    # The table holds framework introduction versions; the floor is the API minimum.
    assert table == {"ScreenCaptureKit": "12.3", "MetalFX": "13.0"}
    assert floor == "13.0" == re.search(r'^MACOS_API_FLOOR="([0-9.]+)"$', verify, re.MULTILINE).group(1)
    # SCStreamConfiguration audio capture (macOS 13.0) is configured unconditionally.
    audio = (ROOT / "rust/lf-meetings/src/macos_audio.rs").read_text(encoding="utf-8")
    for call in ("setCapturesAudio(true)", "setExcludesCurrentProcessAudio(true)", "setSampleRate(", "setChannelCount("):
        assert call in audio, call
    lock = (ROOT / "rust/lf-desktop/Cargo.lock").read_text(encoding="utf-8")
    assert 'name = "screencapturekit-sys"' in lock and 'name = "objc2-screen-capture-kit"' in lock


@LINUX_ONLY
@pytest.mark.parametrize("fault", ["MISSING_ORT", "BAD_DEP", "FAIL_VERIFY"])
def test_macos_builder_failures_keep_the_previous_output(fake, tmp_path, fault):
    _, env = fake
    out = tmp_path / "out"
    out.mkdir()
    previous = out / _asset("macos-arm64")
    previous.write_text("previous release")
    result = _build(_mac_env(env), out, fault)
    assert result.returncode != 0
    assert previous.read_text() == "previous release"
    assert sorted(p.name for p in out.iterdir()) == [previous.name]


def test_builder_leaves_the_legacy_app_builder_and_identity_alone():
    legacy = (ROOT / "scripts/build_rust_app.sh").read_text(encoding="utf-8")
    assert "plutil -insert LocalFlowRuntime -string rust" in legacy
    assert "com.cortexrnd.localflow.app" in legacy and "desktop-preview" not in legacy
    preview = (ROOT / "scripts/build_gpui_package.sh").read_text(encoding="utf-8")
    assert f'PREVIEW_ID="{PREVIEW_ID}"' in preview
    assert "LocalFlowRuntime" not in preview.replace("never carries LocalFlowRuntime", "")
    assert "macos_launchagent" not in preview and "LaunchAgents" not in preview
    # Only the READMEs mention --engine; no launcher, entry or default passes it.
    assert "--engine" not in preview.replace("localflow-preview --engine", "").replace("lf-desktop.exe --engine", "")


def test_desktop_build_script_sets_relative_runtime_search_paths():
    build = (ROOT / "rust/lf-desktop/build.rs").read_text(encoding="utf-8")
    assert 'var_os("CARGO_FEATURE_RUNTIME").is_none()' in build
    for arg in ("-Wl,-rpath,$ORIGIN\"", "-Wl,-rpath,$ORIGIN/../lib\"", "-Wl,-rpath,@loader_path\"",
                "-Wl,-rpath,@loader_path/../Frameworks\""):
        assert arg in build
    verify = (ROOT / "scripts/verify_gpui_package.sh").read_text(encoding="utf-8")
    assert "LINUX_BIN_RUNPATH='$ORIGIN:$ORIGIN/../lib'" in verify


def test_documented_linux_abi_matches_the_enforced_floor():
    verify = (ROOT / "scripts/verify_gpui_package.sh").read_text(encoding="utf-8")
    floors = dict(re.findall(r'^LINUX_(GLIBC|GLIBCXX|CXXABI)_FLOOR="([0-9.]+)"$', verify, re.MULTILINE))
    readme = (ROOT / "rust/lf-desktop/README.md").read_text(encoding="utf-8")
    assert f"glibc {floors['GLIBC']}" in readme
    assert f"GLIBCXX_{floors['GLIBCXX']}" in readme and f"CXXABI_{floors['CXXABI']}" in readme
    allow = re.search(r"LINUX_SYSTEM_LIBS=\((.*?)\)", verify, re.DOTALL).group(1).split()
    for lib in allow:
        assert f"`{lib}`" in readme, f"{lib} is allowed but not documented"


# --- installer ------------------------------------------------------------


def _fake_exe(version: str = VERSION) -> str:
    return textwrap.dedent(f"""\
        #!/bin/sh
        if [ -n "${{LAUNCH_LOG:-}}" ]; then printf '%s\\n' "launched $#:$*" >> "$LAUNCH_LOG"; fi
        [ "$1" = --version ] && echo "lf-desktop {version}"
        exit 0
        """)


def _linux_manifest(files: dict[str, bytes], version: str = VERSION, **override) -> bytes:
    fields = {"name": "localflow-preview", "app": "lf-desktop", "version": version, "os": "linux",
              "arch": "x86_64", "glibc": "2.35", **override}
    lines = ["{"] + [f'  "{k}": "{v}",' for k, v in fields.items()] + ['  "files": {']
    entries = [f'    "{n}": "{hashlib.sha256(d).hexdigest()}"' for n, d in sorted(files.items())]
    lines += [",\n".join(entries), "  }", "}"]
    return ("\n".join(lines) + "\n").encode()


def _linux_files(marker: str = "new", exe_version: str = VERSION) -> dict[str, bytes]:
    return {
        "bin/lf-desktop": _fake_exe(exe_version).encode(),
        "lib/libonnxruntime.so": b"ort",
        "lib/libsherpa-onnx-c-api.so": b"sherpa",
        "share/applications/localflow-preview.desktop": b"[Desktop Entry]\nExec=localflow-preview\n",
        "share/icons/localflow-preview.png": b"png",
        "README.txt": marker.encode(),
    }


def _write_tar(path: Path, entries: list[tuple[str, bytes | None, dict]]) -> Path:
    with tarfile.open(path, "w:gz") as archive:
        dirs = set()
        for name, _, _ in entries:
            parts = name.split("/")[:-1]
            for i in range(1, len(parts) + 1):
                dirs.add("/".join(parts[:i]))
        for d in sorted(dirs):
            info = tarfile.TarInfo(d)
            info.type, info.mode = tarfile.DIRTYPE, 0o755
            archive.addfile(info)
        for name, data, extra in entries:
            info = tarfile.TarInfo(name)
            for key, value in extra.items():
                setattr(info, key, value)
            if data is None:
                archive.addfile(info)
            else:
                info.size = len(data)
                info.mode = extra.get("mode", 0o755 if name.endswith("lf-desktop") else 0o644)
                archive.addfile(info, io.BytesIO(data))
    return path


def _linux_archive(tmp_path: Path, marker="new", version=VERSION, extra=(), manifest=None, name=None,
                   root="localflow-preview", exe_version=VERSION) -> Path:
    files = _linux_files(marker, exe_version)
    entries = [(f"{root}/{n}", d, {}) for n, d in files.items()]
    entries.append((f"{root}/MANIFEST.json", manifest if manifest is not None else _linux_manifest(files, version), {}))
    entries += list(extra)
    folder = tmp_path / f"archives-{marker}-{len(list(tmp_path.glob('archives-*')))}"
    folder.mkdir()
    return _write_tar(folder / (name or _asset("linux-x86_64", version)), entries)


def _sidecar(archive: Path, name: str | None = None) -> Path:
    path = archive.with_name(archive.name + ".sha256")
    path.write_text(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {name or archive.name}\n")
    return path


def _install(env, *args):
    return _run("install_gpui_preview.sh", env, *args)


def _linux_paths(home: Path):
    return (home / ".local/share/localflow-preview", home / ".local/bin/localflow-preview",
            home / ".local/share/applications/localflow-preview.desktop")


def _seed_legacy(home: Path) -> dict[Path, str]:
    legacy = {
        home / ".local/share/applications/localflow.desktop": "[Desktop Entry]\nName=LocalFlow\n",
        home / ".local/share/applications/localflow-setup.desktop": "[Desktop Entry]\nName=LocalFlow Setup\n",
        home / ".local/bin/lf": "legacy lf",
        home / ".local/bin/localflow": "legacy localflow",
        home / "Applications/LocalFlow.app/Contents/Info.plist": "legacy app",
        home / ".localflow/config.toml": "user data",
    }
    for path, text in legacy.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
    return legacy


def _unescape_desktop_exec(value: str) -> list[str]:
    """Desktop Entry spec: string unescaping, then Exec argument unquoting."""
    out, i = [], 0
    string = []
    while i < len(value):
        if value[i] == "\\" and i + 1 < len(value):
            string.append({"s": " ", "n": "\n", "t": "\t", "r": "\r", "\\": "\\"}.get(value[i + 1], "\\" + value[i + 1]))
            i += 2
        else:
            string.append(value[i])
            i += 1
    text, args, current, quoted, i = "".join(string), [], [], False, 0
    while i < len(text):
        c = text[i]
        if quoted and c == "\\" and i + 1 < len(text) and text[i + 1] in '"`$\\':
            current.append(text[i + 1])
            i += 2
            continue
        if c == '"':
            quoted = not quoted
        elif c == " " and not quoted:
            if current:
                args.append("".join(current))
            current = []
        elif c == "%" and i + 1 < len(text) and text[i + 1] == "%":
            current.append("%")
            i += 1
        else:
            current.append(c)
        i += 1
    if current:
        args.append("".join(current))
    out.extend(args)
    return out


def test_installer_offline_install_reinstall_and_uninstall_own_only_preview_paths(fake, tmp_path):
    home, env = fake
    legacy = _seed_legacy(home)
    archive = _linux_archive(tmp_path)
    tree, link, entry = _linux_paths(home)
    for _ in range(2):
        result = _install(env, "--archive", archive, "--sha256", _sidecar(archive), "--no-launch")
        assert result.returncode == 0, result.stderr
        assert "Skipping launch" in result.stdout
    assert (tree / "README.txt").read_text() == "new"
    assert link.is_symlink() and os.readlink(link) == str(tree / "bin/lf-desktop")
    text = entry.read_text()
    assert "X-LocalFlow-Preview=true" in text and "--engine" not in text
    exec_line = next(line[5:] for line in text.splitlines() if line.startswith("Exec="))
    assert _unescape_desktop_exec(exec_line) == [str(tree / "bin/lf-desktop")]
    assert not list(home.glob(".local/share/.localflow-preview-install.*")), "staging removed"
    result = _install(env, "--uninstall")
    assert result.returncode == 0 and "Removed LocalFlow Preview" in result.stdout
    assert not any(p.exists() or p.is_symlink() for p in (tree, link, entry))
    assert _install(env, "--uninstall").stdout.strip() == "LocalFlow Preview is not installed."
    for path, text in legacy.items():
        assert path.read_text() == text, path


def test_installer_escapes_awkward_home_paths_in_the_desktop_entry(fake, tmp_path):
    _, env = fake
    home = tmp_path / 'odd home $HOME `x` "q" 100% back\\slash'
    home.mkdir()
    env["HOME"] = str(home)
    archive = _linux_archive(tmp_path)
    result = _install(env, "--archive", archive, "--sha256", _sidecar(archive), "--no-launch")
    assert result.returncode == 0, result.stderr
    tree, link, entry = _linux_paths(home)
    exec_line = next(line[5:] for line in entry.read_text().splitlines() if line.startswith("Exec="))
    assert _unescape_desktop_exec(exec_line) == [str(tree / "bin/lf-desktop")]
    validate = shutil.which("desktop-file-validate", path=os.environ["PATH"])
    if validate:
        check = subprocess.run([validate, str(entry)], capture_output=True, text=True, check=False)
        assert check.returncode == 0, check.stdout + check.stderr
    run = subprocess.run([str(link), "--version"], capture_output=True, text=True, check=False)
    assert run.stdout.strip() == f"lf-desktop {VERSION}"


def test_installer_accepts_a_literal_digest_or_a_sums_file_but_never_unverified(fake, tmp_path):
    _, env = fake
    archive = _linux_archive(tmp_path)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    sums = tmp_path / "SHA256SUMS-gpui-preview"
    sums.write_text(f"{'0' * 64}  {_asset('macos-arm64')}\n{digest.upper()}  *{archive.name}\n")
    assert _install(env, "--archive", archive, "--sha256", sums, "--no-launch").returncode == 0
    assert _install(env, "--archive", archive, "--sha256", digest, "--no-launch").returncode == 0
    wrong = tmp_path / "wrong.sha256"
    wrong.write_text(f"{digest}  some-other-name.tar.gz\n")
    for sha in (wrong, "0" * 64, tmp_path / "missing.sha256", "not-a-digest"):
        result = _install(env, "--archive", archive, "--sha256", sha, "--no-launch")
        assert result.returncode == 1, sha
    assert _install(env, "--archive", archive, "--no-launch").returncode == 2
    assert _install(env, "--sha256", digest).returncode == 2
    assert _install(env, "--uninstall", "--no-launch").returncode == 2
    assert _install(env, "--bogus").returncode == 2


def _assert_previous_install_intact(home: Path):
    tree, link, entry = _linux_paths(home)
    assert (tree / "README.txt").read_text() == "old"
    assert os.readlink(link) == str(tree / "bin/lf-desktop")
    assert "X-LocalFlow-Preview=true" in entry.read_text()
    assert not list(home.glob(".local/share/.localflow-preview-install.*"))


def _install_old(env, tmp_path):
    old = _linux_archive(tmp_path, marker="old")
    assert _install(env, "--archive", old, "--sha256", _sidecar(old), "--no-launch").returncode == 0


REJECTIONS = {
    "tampered": "SHA-256 mismatch",
    "symlink": "not a regular file or directory",
    "hardlink": "not a regular file or directory",
    "fifo": "not a regular file or directory",
    "parent": "unsafe member path",
    "absolute": "unexpected member",
    "second-root": "unexpected member",
    "wrong-root": "unexpected member",
    "manifest-hash": "files do not match its MANIFEST.json",
    "unlisted-file": "files do not match its MANIFEST.json",
    "manifest-version": "does not describe LocalFlow Preview",
    "manifest-name": "does not describe LocalFlow Preview",
    "wrong-platform": "is for macos-arm64, not this linux-x86_64 machine",
    "binary-version": "reports 'lf-desktop 0.0.1'",
}


@pytest.mark.parametrize("case", sorted(REJECTIONS))
def test_installer_rejects_unsafe_or_inconsistent_archives_without_changes(fake, tmp_path, case):
    home, env = fake
    _install_old(env, tmp_path)
    outside = tmp_path / "outside"
    files = _linux_files()
    kwargs: dict = {}
    if case == "symlink":
        kwargs["extra"] = [("localflow-preview/lib/link.so", None, {"type": tarfile.SYMTYPE, "linkname": str(outside)})]
    elif case == "hardlink":
        kwargs["extra"] = [("localflow-preview/lib/hard.so", None, {"type": tarfile.LNKTYPE,
                                                                    "linkname": "localflow-preview/README.txt"})]
    elif case == "parent":
        kwargs["extra"] = [("localflow-preview/../outside", b"x", {})]
    elif case == "absolute":
        kwargs["extra"] = [(str(outside / "abs"), b"x", {})]
    elif case == "second-root":
        kwargs["extra"] = [("other/file", b"x", {})]
    elif case == "fifo":
        kwargs["extra"] = [("localflow-preview/fifo", None, {"type": tarfile.FIFOTYPE})]
    elif case == "manifest-hash":
        kwargs["manifest"] = _linux_manifest({**files, "README.txt": b"different"})
    elif case == "manifest-version":
        kwargs["manifest"] = _linux_manifest(files, version="0.0.1")
    elif case == "manifest-name":
        kwargs["manifest"] = _linux_manifest(files, name="localflow")
    elif case == "unlisted-file":
        kwargs["extra"] = [("localflow-preview/lib/extra.so", b"x", {})]
    elif case == "wrong-platform":
        kwargs["name"] = _asset("macos-arm64")
    elif case == "wrong-root":
        kwargs["root"] = "LocalFlow Preview.app"
    elif case == "binary-version":
        kwargs["exe_version"] = "0.0.1"
    archive = _linux_archive(tmp_path, **kwargs)
    sidecar = _sidecar(archive)
    if case == "tampered":
        archive.write_bytes(archive.read_bytes() + b"tamper")
    result = _install(env, "--archive", archive, "--sha256", sidecar, "--no-launch")
    assert result.returncode == 1, result.stdout
    assert REJECTIONS[case] in result.stderr, result.stderr
    _assert_previous_install_intact(home)
    assert not outside.exists() and not (tmp_path / "outside").exists()


def test_installer_refuses_foreign_destinations_and_never_removes_them(fake, tmp_path):
    home, env = fake
    archive = _linux_archive(tmp_path)
    sha = _sidecar(archive)
    tree, link, entry = _linux_paths(home)
    tree.mkdir(parents=True)
    (tree / "keep.txt").write_text("someone else's")
    for args in (["--archive", archive, "--sha256", sha, "--no-launch"], ["--uninstall"]):
        result = _install(env, *args)
        assert result.returncode == 1 and "not a LocalFlow Preview install" in result.stderr
    assert (tree / "keep.txt").read_text() == "someone else's"
    shutil.rmtree(tree)
    target = tmp_path / "elsewhere"
    target.mkdir()
    tree.parent.mkdir(parents=True, exist_ok=True)
    tree.symlink_to(target, target_is_directory=True)
    assert _install(env, "--archive", archive, "--sha256", sha, "--no-launch").returncode == 1
    tree.unlink()
    link.parent.mkdir(parents=True)
    link.write_text("a different localflow-preview command")
    result = _install(env, "--archive", archive, "--sha256", sha, "--no-launch")
    assert result.returncode == 1 and "not LocalFlow Preview's link" in result.stderr
    assert _install(env, "--uninstall").returncode == 1
    assert link.read_text() == "a different localflow-preview command"
    link.unlink()
    entry.parent.mkdir(parents=True)
    entry.write_text("[Desktop Entry]\nName=Someone else\n")
    result = _install(env, "--archive", archive, "--sha256", sha, "--no-launch")
    assert result.returncode == 1 and "not written by this installer" in result.stderr
    assert not tree.exists(), "refused before anything was written"
    assert entry.read_text() == "[Desktop Entry]\nName=Someone else\n"


@pytest.mark.skipif(hasattr(os, "geteuid") and os.geteuid() == 0, reason="root ignores directory permissions")
def test_installer_rolls_back_when_the_swap_fails(fake, tmp_path):
    home, env = fake
    _install_old(env, tmp_path)
    new = _linux_archive(tmp_path, marker="new")
    applications = home / ".local/share/applications"
    applications.chmod(0o555)
    try:
        result = _install(env, "--archive", new, "--sha256", _sidecar(new), "--no-launch")
    finally:
        applications.chmod(0o755)
    assert result.returncode == 1
    assert "previous state was restored" in result.stderr
    _assert_previous_install_intact(home)


def _shims(tmp_path: Path, env, ln_fails: bool, restore_copy_fails: bool) -> None:
    """Fail the install's localflow-preview symlink and/or rollback's restore copies."""
    shim = tmp_path / "shims"
    shim.mkdir()
    for tool, fails, pattern in (("ln", ln_fails, "*/.local/bin/localflow-preview"),
                                 ("cp", restore_copy_fails, "*.restore-*")):
        condition = f'[[ "$last" == {pattern} ]]' if fails else "false"
        (shim / tool).write_text(textwrap.dedent(f"""\
            #!/bin/bash
            last="${{!#}}"
            if {condition}; then
                echo "{tool}: simulated failure" >&2
                exit 1
            fi
            exec {shutil.which(tool)} "$@"
            """))
        (shim / tool).chmod(0o755)
    env["PATH"] = f"{shim}{os.pathsep}{env['PATH']}"


def _custom_old_entry(env, tmp_path, home: Path) -> bytes:
    """An existing owned install whose desktop entry carries a custom line."""
    _install_old(env, tmp_path)
    entry = _linux_paths(home)[2]
    entry.write_text(entry.read_text() + "X-Custom-Marker=keep-me\n")
    return entry.read_bytes()


def test_rollback_after_a_late_failure_restores_the_previous_entry(fake, tmp_path):
    # The new desktop entry is already in place when creating the symlink
    # fails; rollback puts the old tree and the custom entry back.
    home, env = fake
    before = _custom_old_entry(env, tmp_path, home)
    _shims(tmp_path, env, ln_fails=True, restore_copy_fails=False)
    new = _linux_archive(tmp_path, marker="new")
    result = _install(env, "--archive", new, "--sha256", _sidecar(new), "--no-launch")
    assert result.returncode == 1
    assert "previous state was restored" in result.stderr
    assert _linux_paths(home)[2].read_bytes() == before
    _assert_previous_install_intact(home)


def test_failed_recovery_keeps_the_backup_and_says_where_it_is(fake, tmp_path):
    # As above, but restoring the desktop entry also fails. The installer must
    # not claim success or delete the only copy of the previous entry.
    home, env = fake
    before = _custom_old_entry(env, tmp_path, home)
    _shims(tmp_path, env, ln_fails=True, restore_copy_fails=True)
    new = _linux_archive(tmp_path, marker="new")
    result = _install(env, "--archive", new, "--sha256", _sidecar(new), "--no-launch")
    assert result.returncode == 1
    assert "previous state was restored" not in result.stderr
    kept = list(home.glob(".local/share/.localflow-preview-install.*/previous.desktop"))
    assert len(kept) == 1, "the staging backup was deleted"
    assert kept[0].read_bytes() == before
    assert str(kept[0]) in result.stderr, result.stderr
    # What could be restored was: the old tree and the unchanged link.
    tree, link, _ = _linux_paths(home)
    assert (tree / "README.txt").read_text() == "old"
    assert os.readlink(link) == str(tree / "bin/lf-desktop")


@pytest.mark.parametrize("partial", [False, True])
def test_a_failed_backup_of_the_previous_state_changes_nothing(fake, tmp_path, partial):
    # Backing up the existing desktop entry fails (optionally after writing a
    # truncated copy). Nothing has been replaced yet, so the failure must not
    # trigger a rollback that deletes or overwrites the valid entry.
    home, env = fake
    _install_old(env, tmp_path)
    entry = _linux_paths(home)[2]
    before = entry.read_bytes()
    shim = tmp_path / "cp-shim"
    shim.mkdir()
    partial_write = 'printf "[Desk" > "$last"' if partial else ":"
    (shim / "cp").write_text(textwrap.dedent(f"""\
        #!/bin/bash
        last="${{!#}}"
        if [[ "$last" == */previous.desktop ]]; then
            {partial_write}
            exit 1
        fi
        exec {shutil.which("cp")} "$@"
        """))
    (shim / "cp").chmod(0o755)
    env["PATH"] = f"{shim}{os.pathsep}{env['PATH']}"
    new = _linux_archive(tmp_path, marker="new")
    result = _install(env, "--archive", new, "--sha256", _sidecar(new), "--no-launch")
    assert result.returncode == 1, result.stdout
    assert entry.exists() and entry.read_bytes() == before, "the valid desktop entry was lost"
    _assert_previous_install_intact(home)
    assert "previous state was restored" not in result.stderr, "nothing had been changed to restore"


@pytest.mark.skipif(hasattr(os, "geteuid") and os.geteuid() == 0, reason="root ignores directory permissions")
def test_failed_fresh_install_leaves_nothing_behind(fake, tmp_path):
    home, env = fake
    archive = _linux_archive(tmp_path)
    applications = home / ".local/share/applications"
    applications.mkdir(parents=True)
    applications.chmod(0o555)
    try:
        result = _install(env, "--archive", archive, "--sha256", _sidecar(archive), "--no-launch")
    finally:
        applications.chmod(0o755)
    assert result.returncode == 1
    tree, link, entry = _linux_paths(home)
    assert not any(p.exists() or p.is_symlink() for p in (tree, link, entry))
    assert not list(home.glob(".local/share/.localflow-preview-install.*"))


def test_installer_launch_passes_no_engine_and_only_with_a_display(fake, tmp_path):
    _, env = fake
    archive = _linux_archive(tmp_path)
    launches = tmp_path / "launches"
    env["LAUNCH_LOG"] = str(launches)
    result = _install(env, "--archive", archive, "--sha256", _sidecar(archive))
    assert result.returncode == 0 and "No display found" in result.stdout
    assert not launches.exists(), "the pre-install version check runs with an empty environment"
    env["DISPLAY"] = ":99"
    assert _install(env, "--archive", archive, "--sha256", _sidecar(archive)).returncode == 0
    for _ in range(100):
        if launches.exists():
            break
        subprocess.run(["sleep", "0.05"], check=True)
    assert launches.read_text().splitlines() == ["launched 0:"]


def test_installer_downloads_and_verifies_a_versioned_release(fake, tmp_path):
    home, env = fake
    archive = _linux_archive(tmp_path)
    release = tmp_path / "release"
    release.mkdir()
    shutil.copy(archive, release / archive.name)
    sums = release / "SHA256SUMS-gpui-preview"
    sums.write_text(f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n")
    env["LOCALFLOW_PREVIEW_BASE_URL"] = release.as_uri()
    result = _install(env, "--release", f"v{VERSION}", "--no-launch")
    assert result.returncode == 0, result.stderr
    assert (_linux_paths(home)[0] / "README.txt").read_text() == "new"
    (release / archive.name).write_bytes(archive.read_bytes() + b"x")
    result = _install(env, "--no-launch")
    assert result.returncode == 1 and "SHA-256 mismatch" in result.stderr
    sums.unlink()
    result = _install(env, "--no-launch")
    assert result.returncode == 1 and "has no SHA256SUMS-gpui-preview" in result.stderr
    result = _install(env, "--release", "v9.9.9", "--no-launch")
    assert result.returncode == 1
    assert _install(env, "--release", "main").returncode == 2
    assert (_linux_paths(home)[0] / "README.txt").read_text() == "new"


def test_installer_default_release_is_this_version():
    script = (ROOT / "scripts/install_gpui_preview.sh").read_text(encoding="utf-8")
    assert f'PREVIEW_RELEASE="v{VERSION}"' in script
    assert "https://github.com/CortexRND/localflow/releases/download/$release" in script
    launches = [line for line in script.splitlines() if re.search(r'\b(open|nohup) "\$dest', line)]
    assert len(launches) == 2 and not any("--engine" in line or "--args" in line for line in launches)
    assert "LaunchAgents" not in script and "launchctl" not in script


def _mac_archive(tmp_path: Path, bundle_id=PREVIEW_ID, runtime_key=False, version=VERSION) -> Path:
    info = {"CFBundleIdentifier": bundle_id, "CFBundleExecutable": "lf-desktop",
            "CFBundleShortVersionString": version}
    if runtime_key:
        info["LocalFlowRuntime"] = "rust"
    root = "LocalFlow Preview.app/Contents"
    entries = [(f"{root}/Info.plist", plistlib.dumps(info), {}),
               (f"{root}/MacOS/lf-desktop", _fake_exe().encode(), {}),
               (f"{root}/Frameworks/libonnxruntime.dylib", b"ort", {})]
    folder = tmp_path / f"mac-{len(list(tmp_path.glob('mac-*')))}"
    folder.mkdir()
    return _write_tar(folder / _asset("macos-arm64", version), entries)


def test_macos_installer_owns_only_the_preview_bundle(fake, tmp_path):
    home, env = fake
    env.update(FAKE_OS="Darwin", FAKE_ARCH="arm64")
    legacy = _seed_legacy(home)
    archive = _mac_archive(tmp_path)
    app = home / "Applications/LocalFlow Preview.app"
    for _ in range(2):
        result = _install(env, "--archive", archive, "--sha256", _sidecar(archive))
        assert result.returncode == 0, result.stderr
    assert plistlib.loads((app / "Contents/Info.plist").read_bytes())["CFBundleIdentifier"] == PREVIEW_ID
    opens = [c for c in _calls(env) if c[0] == "open"]
    assert opens == [["open", str(app)]] * 2, "opened with no --engine or other arguments"
    assert any(c[:4] == ["codesign", "--verify", "--deep", "--strict"] for c in _calls(env))
    assert not (home / ".local/share/localflow-preview").exists()
    assert _install(env, "--uninstall").returncode == 0
    assert not app.exists()
    for path, text in legacy.items():
        assert path.read_text() == text


@pytest.mark.parametrize("case", ["legacy-id", "runtime-key", "bad-signature", "intel-archive"])
def test_macos_installer_refuses_foreign_or_unsigned_bundles(fake, tmp_path, case):
    home, env = fake
    env.update(FAKE_OS="Darwin", FAKE_ARCH="arm64")
    if case == "legacy-id":
        archive = _mac_archive(tmp_path, bundle_id="com.cortexrnd.localflow.app")
    elif case == "runtime-key":
        archive = _mac_archive(tmp_path, runtime_key=True)
    else:
        archive = _mac_archive(tmp_path)
    if case == "bad-signature":
        env["FAULTS"] = "FAIL_VERIFY"
    if case == "intel-archive":
        env["FAKE_ARCH"] = "x86_64"
    result = _install(env, "--archive", archive, "--sha256", _sidecar(archive), "--no-launch")
    assert result.returncode == 1
    assert not (home / "Applications/LocalFlow Preview.app").exists()


def test_macos_installer_refuses_to_replace_or_remove_another_app(fake, tmp_path):
    home, env = fake
    env.update(FAKE_OS="Darwin", FAKE_ARCH="arm64")
    app = home / "Applications/LocalFlow Preview.app/Contents"
    app.mkdir(parents=True)
    (app / "Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": "com.example.other"}))
    archive = _mac_archive(tmp_path)
    assert _install(env, "--archive", archive, "--sha256", _sidecar(archive), "--no-launch").returncode == 1
    assert _install(env, "--uninstall").returncode == 1
    assert plistlib.loads((app / "Info.plist").read_bytes())["CFBundleIdentifier"] == "com.example.other"


@LINUX_ONLY
def test_builder_output_installs_with_the_preview_installer(fake, tmp_path):
    home, env = fake
    out = tmp_path / "out"
    assert _build(env, out).returncode == 0
    archive = out / _asset("linux-x86_64")
    result = _install(env, "--archive", archive, "--sha256", f"{archive}.sha256", "--no-launch")
    assert result.returncode == 0, result.stderr
    assert (home / ".local/share/localflow-preview/lib/libonnxruntime.so").is_file()


# --- release publishing ---------------------------------------------------


def _artifacts(tmp_path: Path, version=VERSION) -> Path:
    artifacts = tmp_path / "artifacts"
    for platform in PLATFORMS:
        folder = artifacts / f"gpui-preview-{platform}"
        folder.mkdir(parents=True)
        if platform.startswith("windows"):
            archive = folder / _asset(platform, version)
            with zipfile.ZipFile(archive, "w") as z:
                z.writestr(f"LocalFlow Preview/file-{platform}", platform)
        else:
            root = "localflow-preview" if platform.startswith("linux") else "LocalFlow Preview.app"
            archive = _write_tar(folder / _asset(platform, version), [(f"{root}/file-{platform}", platform.encode(), {})])
        _sidecar(archive)
    return artifacts


def _publish_tree(tmp_path: Path) -> Path:
    work = tmp_path / "work"
    (work / "scripts").mkdir(parents=True)
    (work / "rust/lf-desktop").mkdir(parents=True)
    shutil.copy(ROOT / "scripts/gpui_preview_assets.sh", work / "scripts/gpui_preview_assets.sh")
    shutil.copy(ROOT / "rust/lf-desktop/Cargo.toml", work / "rust/lf-desktop/Cargo.toml")
    return work


def _break(artifacts: Path, fault: str):
    linux = artifacts / "gpui-preview-linux-x86_64"
    name = _asset("linux-x86_64")
    if fault == "missing-platform":
        shutil.rmtree(artifacts / "gpui-preview-macos-x86_64")
    elif fault == "extra-platform":
        (artifacts / "gpui-preview-linux-aarch64").mkdir()
    elif fault == "extra-file":
        (linux / "notes.txt").write_text("x")
    elif fault == "tampered":
        (linux / name).write_bytes((linux / name).read_bytes() + b"x")
    elif fault == "misnamed":
        (linux / name).rename(linux / "LocalFlow-Preview.tar.gz")
    elif fault == "sidecar-name":
        (linux / f"{name}.sha256").write_text(f"{hashlib.sha256((linux / name).read_bytes()).hexdigest()}  other.tar.gz\n")
    elif fault == "wrong-content":
        mac = artifacts / "gpui-preview-macos-arm64"
        shutil.copy(linux / name, mac / _asset("macos-arm64"))
        _sidecar(mac / _asset("macos-arm64"))
    elif fault == "symlink":
        (linux / name).unlink()
        (linux / name).symlink_to(artifacts / "gpui-preview-macos-arm64" / _asset("macos-arm64"))


PUBLISH_FAULTS = {
    "missing-platform": "not exactly one per platform",
    "extra-platform": "not exactly one per platform",
    "extra-file": "must hold only",
    "misnamed": "must hold only",
    "tampered": "does not match its build checksum",
    "sidecar-name": "does not list",
    "wrong-content": "is not a macos-arm64 package",
    "symlink": "is not a regular file",
    "tag-mismatch": "does not match lf-desktop version",
    "dirty-out": "must be new or empty",
}


@pytest.mark.parametrize("fault", [None, *sorted(PUBLISH_FAULTS)])
def test_preview_assets_stage_exactly_the_native_set_or_fail_closed(fake, tmp_path, fault):
    _, env = fake
    work = _publish_tree(tmp_path)
    artifacts = _artifacts(tmp_path)
    if fault:
        _break(artifacts, fault)
    out = tmp_path / "release"
    if fault == "dirty-out":
        out.mkdir()
        (out / "SHA256SUMS").write_text("legacy trust file")
    tag = "v0.0.1" if fault == "tag-mismatch" else f"v{VERSION}"
    result = subprocess.run([BASH, str(work / "scripts/gpui_preview_assets.sh"), tag, str(artifacts), str(out)],
                            env=env, capture_output=True, text=True, check=False)
    if fault:
        assert result.returncode != 0, result.stdout
        assert PUBLISH_FAULTS[fault] in result.stderr, result.stderr
        if fault == "dirty-out":
            assert (out / "SHA256SUMS").read_text() == "legacy trust file"
        return
    assert result.returncode == 0, result.stderr
    names = sorted(p.name for p in out.iterdir())
    assert names == sorted([_asset(p) for p in PLATFORMS] + ["SHA256SUMS-gpui-preview"])
    sums = dict(reversed(line.split(maxsplit=1)) for line in (out / "SHA256SUMS-gpui-preview").read_text().splitlines())
    assert set(sums) == {_asset(p) for p in PLATFORMS}
    for name, digest in sums.items():
        assert hashlib.sha256((out / name).read_bytes()).hexdigest() == digest
    assert not (out / "SHA256SUMS").exists()


def _step_run(workflow: str, step_name: str) -> str:
    lines = (ROOT / ".github/workflows" / workflow).read_text(encoding="utf-8").splitlines()
    indent = lambda line: len(line) - len(line.lstrip())
    start = next(i for i, line in enumerate(lines) if line.strip() == f"- name: {step_name}")
    body, run_indent = [], None
    for line in lines[start + 1:]:
        if line.strip() and indent(line) <= indent(lines[start]):
            break
        if run_indent is None:
            if line.strip() == "run: |":
                run_indent = indent(line)
            continue
        if line.strip() and indent(line) <= run_indent:
            break
        body.append(line)
    block = min(indent(line) for line in body if line.strip())
    return "\n".join(line[block:] for line in body) + "\n"


@pytest.mark.parametrize("fault", [None, "no-release", "tampered"])
def test_release_job_uploads_only_preview_assets_to_the_existing_release(fake, tmp_path, fault):
    _, env = fake
    work = _publish_tree(tmp_path)
    artifacts = _artifacts(tmp_path)
    shutil.move(artifacts, work / "dist/gpui-preview-artifacts")
    if fault == "tampered":
        _break(work / "dist/gpui-preview-artifacts", "tampered")
    bin_dir = tmp_path / "gh-bin"
    bin_dir.mkdir()
    gh_log = tmp_path / "gh.log"
    gh = bin_dir / "gh"
    gh.write_text('#!/bin/bash\nprintf \'%s\\n\' "$@" "<<end>>" >> "$GH_LOG"\n'
                  '[ "$1 $2" != "release view" ] || [ -z "${NO_RELEASE:-}" ]\n')
    gh.chmod(0o755)
    env.update(PATH=f"{bin_dir}{os.pathsep}{env['PATH']}", GITHUB_REF_NAME=f"v{VERSION}", GH_LOG=str(gh_log))
    if fault == "no-release":
        env["NO_RELEASE"] = "1"
    result = subprocess.run([BASH, "--noprofile", "--norc", "-eo", "pipefail", "-c",
                             _step_run("mac-installer.yml", "Upload GPUI preview to release")],
                            cwd=work, env=env, capture_output=True, text=True, check=False)
    calls, current = [], []
    for arg in gh_log.read_text().splitlines() if gh_log.exists() else []:
        if arg == "<<end>>":
            calls.append(current)
            current = []
        else:
            current.append(arg)
    assert not [c for c in calls if c[:2] == ["release", "create"]]
    uploads = [c for c in calls if c[:2] == ["release", "upload"]]
    if fault:
        assert result.returncode != 0
        assert not uploads
        return
    assert result.returncode == 0, result.stderr
    assert len(uploads) == 1 and uploads[0][2] == f"v{VERSION}" and uploads[0][-1] == "--clobber"
    assert sorted(Path(p).name for p in uploads[0][3:-1]) == sorted(
        [_asset(p) for p in PLATFORMS] + ["SHA256SUMS-gpui-preview"])


# --- workflow wiring --------------------------------------------------------


def _job_block(workflow: str, job: str) -> str:
    text = (ROOT / ".github/workflows" / workflow).read_text(encoding="utf-8")
    match = re.search(rf"^  {re.escape(job)}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", text, re.DOTALL | re.MULTILINE)
    assert match, job
    return match.group(1)


def test_package_workflow_builds_every_native_platform_without_cross_targets():
    workflow = (ROOT / ".github/workflows/gpui-package.yml").read_text(encoding="utf-8")
    assert "workflow_call:" in workflow and "fail-fast: false" in workflow
    pairs = re.findall(r"- runner: (\S+)\n\s+platform: (\S+)", workflow)
    assert pairs == [("ubuntu-22.04", "linux-x86_64"), ("macos-latest", "macos-arm64"),
                     ("macos-15-intel", "macos-x86_64"), ("windows-latest", "windows-x86_64")]
    # Windows: no preview installer (install.ps1 is untouched); the unzipped
    # package is launched in sandbox mode and must open a window.
    assert "install.ps1" in workflow and "install.ps1 is unchanged" in workflow
    installer = workflow.split("- name: Installer smoke test in a temporary HOME (no launch)\n", 1)[1]
    assert installer.startswith("        if: runner.os != 'Windows'\n")
    launch = workflow.split("- name: Launch the packaged app (Windows)\n", 1)[1].split("\n      - ", 1)[0]
    assert "if: runner.os == 'Windows'" in launch and "--sandbox" in launch and "--engine" not in launch
    assert "MainWindowHandle" in launch and "Stop-Process" in launch
    assert "name: windows-launch-screenshot" in workflow, "never matched by mac-installer's gpui-preview-* download"
    assert "CARGO_BUILD_TARGET" not in workflow and "--target" not in workflow
    assert "name: gpui-preview-${{ matrix.platform }}" in workflow
    assert "if-no-files-found: error" in workflow
    assert "gh release" not in workflow and "contents: write" not in workflow
    assert "tags:" not in workflow, "tag builds come only from mac-installer.yml's call"
    # Pushes build only on main; PR heads are covered by the pull_request run,
    # so one branch update never builds the native legs twice.
    push = re.search(r"^  push:\n(.*?)^  pull_request:\n", workflow, re.DOTALL | re.MULTILINE).group(1)
    assert push.splitlines()[0].strip() == "branches: [main]"
    assert "workflow_dispatch:" in workflow and "\n  pull_request:\n    paths:" in workflow
    assets = (ROOT / "scripts/gpui_preview_assets.sh").read_text(encoding="utf-8")
    assert f"PLATFORMS=({' '.join(PLATFORMS)})" in assets
    for script in ("verify_gpui_package.sh", "install_gpui_preview.sh"):
        text = (ROOT / "scripts" / script).read_text(encoding="utf-8")
        for platform, uname in (("linux-x86_64", "Linux-x86_64"), ("macos-arm64", "Darwin-arm64"),
                                ("macos-x86_64", "Darwin-x86_64")):
            assert uname in text and platform in text, (script, platform)
    for path in ("scripts/build_gpui_package.sh", "scripts/verify_gpui_package.sh",
                 "scripts/install_gpui_preview.sh", "scripts/gpui_preview_assets.sh",
                 "tests/test_gpui_packaging.py", "rust/lf-desktop/**", "rust/lf-stt/**"):
        assert workflow.count(f'- "{path}"') == 2, path


def test_release_wiring_is_additive_and_tag_only():
    workflow = (ROOT / ".github/workflows/mac-installer.yml").read_text(encoding="utf-8")
    build = "\n".join(line for line in _job_block("mac-installer.yml", "build").splitlines()
                      if not line.strip().startswith("#"))
    assert "gpui" not in build.lower() and "SHA256SUMS-gpui-preview" not in build
    preview = [line.strip() for line in _job_block("mac-installer.yml", "gpui-preview").splitlines()
               if line.strip() and not line.strip().startswith("#")]
    # Not on pull requests: gpui-package.yml's own PR run already builds them.
    assert preview == ["if: startsWith(github.ref, 'refs/tags/v') || github.event_name == 'workflow_dispatch'",
                       "uses: ./.github/workflows/gpui-package.yml"]
    publish = _job_block("mac-installer.yml", "publish-gpui-preview")
    assert "needs: [build, gpui-preview]" in publish
    assert "if: startsWith(github.ref, 'refs/tags/v')" in publish
    assert "pattern: gpui-preview-*" in publish
    assert "gh release create" not in publish and "SHA256SUMS " not in publish and "SHA256SUMS\n" not in publish
    assert workflow.count("gh release create") == 1
    installers = (ROOT / ".github/workflows/installers.yml").read_text(encoding="utf-8")
    shellcheck = next(line for line in installers.splitlines() if "run: shellcheck" in line)
    for script in ("install.sh", "scripts/release-assets.sh", "scripts/build_rust_app.sh",
                   "scripts/macos_launchagent.sh", "scripts/build_gpui_package.sh",
                   "scripts/verify_gpui_package.sh", "scripts/install_gpui_preview.sh",
                   "scripts/gpui_preview_assets.sh"):
        assert f" {script}" in shellcheck, script
        if script.startswith("scripts/"):
            assert installers.count(f"- {script}\n") == 2, script
    assert installers.count("- tests/test_gpui_packaging.py\n") == 2
    ci = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")
    assert "tests/test_gpui_packaging.py" in ci
