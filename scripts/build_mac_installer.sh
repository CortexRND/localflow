#!/bin/bash
# Build "Install LocalFlow.app": a double-clickable bundle that opens Terminal.app
# and runs the bundled install.sh.
#
# Output (in dist/):
#   Install LocalFlow.app
#   LocalFlow-Installer.dmg        (macOS: app + Read Me.txt)
#   LocalFlow-Installer-mac.zip    (other systems; keeps the executable bit)
#
# On macOS the bundle executable is a small universal (arm64 + x86_64) Mach-O
# launcher, so Apple Silicon never asks to install Rosetta; it runs the shell
# launcher in Contents/Resources with the system /bin/bash. Other systems cannot
# compile it and ship the shell launcher itself as the executable.
# On macOS the Xcode Command Line Tools (clang, python3) are required; python3
# parses `hdiutil info -plist` to detach a stale image left at the DMG path.
#
# Environment:
#   LOCALFLOW_REF  Custom ref the installer downloads (not checksum-verified).
#                  Default: unset, so install.sh installs its pinned release
#                  and verifies it against the release's SHA256SUMS.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/dist"
APP="$DIST/Install LocalFlow.app"
DMG="$DIST/LocalFlow-Installer.dmg"
ZIP="$DIST/LocalFlow-Installer-mac.zip"

# Never pin the build commit: an exported LOCALFLOW_REF turns off install.sh's
# checksum-verified release download.
REF="${LOCALFLOW_REF:-}"
if [[ -n "$REF" && ! "$REF" =~ ^[A-Za-z0-9._/-]+$ ]]; then
    echo "Unsupported LOCALFLOW_REF: $REF" >&2
    exit 2
fi

# A failed `hdiutil create` can leave the output image attached ("Resource
# busy" on macOS 26), which breaks every later build. Only images whose backing
# file is exactly $DMG are detached (never another volume with the same name),
# and $DMG is never deleted while still attached.
DMG_ATTACHED_PY='
import os, plistlib, re, sys
target = os.path.realpath(sys.argv[1])
for image in plistlib.loads(sys.stdin.buffer.read()).get("images", []):
    path = image.get("image-path")
    if not path or os.path.realpath(path) != target:
        continue
    disks = {entity.get("dev-entry", "") for entity in image.get("system-entities", [])}
    disks = sorted(d for d in disks if re.fullmatch(r"/dev/disk[0-9]+", d))
    if not disks:
        sys.exit("attached image " + path + " has no whole-disk device")
    print("\n".join(disks))
'
attached_dmg_devices() {
    hdiutil info -plist | python3 -c "$DMG_ATTACHED_PY" "$DMG"
}

detach_stale_dmg() {
    local devices device
    if ! devices="$(attached_dmg_devices)"; then
        echo "Could not list attached disk images; refusing to replace $DMG." >&2
        return 1
    fi
    [[ -n "$devices" ]] || return 0
    while IFS= read -r device; do
        echo "Detaching stale disk image $device ($DMG)." >&2
        hdiutil detach "$device" >/dev/null \
            || hdiutil detach -force "$device" >/dev/null \
            || true
    done <<<"$devices"
    if ! devices="$(attached_dmg_devices)" || [[ -n "$devices" ]]; then
        echo "Could not detach the disk image at $DMG (${devices//$'\n'/ }); refusing to replace it. Detach it with 'hdiutil detach' and retry." >&2
        return 1
    fi
}

if [[ "$(uname -s)" == "Darwin" && -e "$DMG" ]] && command -v hdiutil >/dev/null 2>&1; then
    detach_stale_dmg || exit 1
fi
rm -rf "$APP" "$DMG" "$ZIP" "$DIST/.staging"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/install.sh" "$APP/Contents/Resources/install.sh"

# Optional icon: reuse the generator in localflow/appbundle.py (stdlib only, so
# no LocalFlow dependencies are needed). macOS only; silently omitted elsewhere.
ICON_XML=""
if [[ "$(uname -s)" == "Darwin" && -f "$ROOT/localflow/appbundle.py" ]]; then
    icon_python="$(command -v python3 || true)"
    # /usr/bin/python3 shows an install dialog without the Command Line Tools.
    if [[ -n "$icon_python" ]] \
        && { [[ "$icon_python" != /usr/bin/* ]] || xcode-select -p >/dev/null 2>&1; } \
        && "$icon_python" - "$ROOT/localflow/appbundle.py" \
            "$APP/Contents/Resources/InstallLocalFlow.icns" >/dev/null 2>&1 <<'PY'
import importlib.util
import sys
from pathlib import Path

spec = importlib.util.spec_from_file_location("lf_appbundle", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
raise SystemExit(0 if module._make_icns(Path(sys.argv[2])) else 1)
PY
    then
        ICON_XML="    <key>CFBundleIconFile</key>
    <string>InstallLocalFlow</string>"
    else
        rm -f "$APP/Contents/Resources/InstallLocalFlow.icns"
        echo "Note: building without an icon." >&2
    fi
fi

cat >"$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Install LocalFlow</string>
    <key>CFBundleDisplayName</key>
    <string>Install LocalFlow</string>
    <key>CFBundleIdentifier</key>
    <string>com.cortexrnd.localflow.installer</string>
    <key>CFBundleExecutable</key>
    <string>install-localflow</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.2.0</string>
    <key>LSArchitecturePriority</key>
    <array>
        <string>arm64</string>
        <string>x86_64</string>
    </array>
    <key>NSHighResolutionCapable</key>
    <true/>
$ICON_XML
</dict>
</plist>
EOF

launcher_template="$(cat <<'EOF'
#!/bin/bash
# Opens Terminal.app running the bundled install.sh. The .command file is
# generated in a temporary writable directory, so this also works when the app
# runs from a read-only disk image or from a path with spaces.
set -u
# Empty: install the release pinned in install.sh (checksum-verified).
PINNED_REF="@@LOCALFLOW_REF@@"

here="$(cd "$(dirname "$0")" && pwd)"
installer="$here/../Resources/install.sh"
if [[ ! -r "$installer" ]]; then
    osascript -e 'display alert "Install LocalFlow" message "The bundled installer script is missing."' >/dev/null 2>&1
    exit 1
fi
installer="$(cd "$(dirname "$installer")" && pwd)/install.sh"

workdir="$(mktemp -d "${TMPDIR:-/tmp}/localflow-installer.XXXXXX")" || exit 1
command_file="$workdir/Install LocalFlow.command"
ref="${LOCALFLOW_REF:-$PINNED_REF}"

{
    printf '#!/bin/bash\n'
    printf 'WORKDIR=%q\n' "$workdir"
    printf 'INSTALLER=%q\n' "$installer"
    if [[ -n "$ref" ]]; then
        printf 'export LOCALFLOW_REF=%q\n' "$ref"
    else
        printf 'unset LOCALFLOW_REF\n'
    fi
    cat <<'INNER'
trap 'rm -rf "$WORKDIR"' EXIT
clear
echo "Installing LocalFlow — this takes a few minutes; LocalFlow will open when done."
echo "Afterwards, find it in Applications → LocalFlow."
echo
bash "$INSTALLER"
status=$?
if [ "$status" -ne 0 ]; then
    echo
    echo "LocalFlow installation failed (exit code $status). Scroll up for details."
    echo "Press Return to close this window."
    read -r _
fi
exit "$status"
INNER
} >"$command_file"
chmod +x "$command_file"

if ! open -a Terminal "$command_file"; then
    osascript \
        -e 'on run argv' \
        -e 'tell application "Terminal"' \
        -e 'activate' \
        -e 'do script (quoted form of item 1 of argv)' \
        -e 'end tell' \
        -e 'end run' "$command_file"
fi
EOF
)"
LAUNCHER_SCRIPT="$APP/Contents/Resources/install-localflow.sh"
printf '%s\n' "${launcher_template//@@LOCALFLOW_REF@@/$REF}" >"$LAUNCHER_SCRIPT"
chmod 755 "$LAUNCHER_SCRIPT"

EXECUTABLE="$APP/Contents/MacOS/install-localflow"
if [[ "$(uname -s)" == "Darwin" ]]; then
    # A script CFBundleExecutable has no architecture, which makes Apple Silicon
    # offer to install Rosetta. Build a universal native launcher instead.
    command -v clang >/dev/null 2>&1 || {
        echo "clang is required to build the native installer launcher; install the Xcode Command Line Tools (xcode-select --install)." >&2
        exit 1
    }
    stub_dir="$(mktemp -d "${TMPDIR:-/tmp}/localflow-launcher.XXXXXX")"
    cat >"$stub_dir/launcher.c" <<'C'
#include <limits.h>
#include <mach-o/dyld.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    char exe[PATH_MAX], real[PATH_MAX], script[PATH_MAX];
    uint32_t size = sizeof exe;
    if (_NSGetExecutablePath(exe, &size) != 0 || realpath(exe, real) == NULL) {
        fprintf(stderr, "install-localflow: cannot locate the app bundle\n");
        return 1;
    }
    char *slash = strrchr(real, '/');
    if (slash == NULL) return 1;
    *slash = '\0';
    if (snprintf(script, sizeof script, "%s/../Resources/install-localflow.sh", real)
        >= (int)sizeof script) return 1;
    char *const args[] = {"/bin/bash", script, NULL};
    execv("/bin/bash", args);
    perror("install-localflow: /bin/bash");
    return 127;
}
C
    if ! clang -Os -arch arm64 -arch x86_64 -mmacosx-version-min=10.13 \
        -o "$EXECUTABLE" "$stub_dir/launcher.c"; then
        rm -rf "$stub_dir"
        echo "Could not compile the universal installer launcher." >&2
        exit 1
    fi
    rm -rf "$stub_dir"
    # Verified here rather than in CI so every build path is covered.
    archs="$(lipo -archs "$EXECUTABLE" 2>/dev/null || true)"
    if [[ " $archs " != *" arm64 "* || " $archs " != *" x86_64 "* ]]; then
        echo "Installer launcher is not universal (architectures: ${archs:-unknown})." >&2
        exit 1
    fi
else
    cp "$LAUNCHER_SCRIPT" "$EXECUTABLE"
fi
chmod 755 "$EXECUTABLE"

if [[ "$(uname -s)" == "Darwin" ]] && command -v codesign >/dev/null 2>&1; then
    codesign --force --sign - "$APP" \
        || echo "Warning: ad-hoc codesign failed; continuing unsigned." >&2
fi

README_TEXT='Install LocalFlow
=================

1. Double-click "Install LocalFlow".
2. A Terminal window opens and installs LocalFlow. This takes a few minutes.
   LocalFlow opens when it is done. Afterwards, find it in Applications >
   LocalFlow (the Applications folder inside your home folder).

The app is not signed or notarized by Apple, so macOS blocks it the first
time you open it. To allow it:
  1. Double-click "Install LocalFlow" and close the warning (Done or OK).
  2. Open System Settings > Privacy & Security and scroll down to Security.
  3. Click "Open Anyway" next to the message about Install LocalFlow, then
     confirm with Open Anyway and your password or Touch ID.
On macOS 15 and later, right-click > Open no longer offers this bypass; use
System Settings as above.

The default app uses Rust, not Python. Rust is provisioned automatically;
accept the Apple Command Line Tools dialog if prompted. Missing CMake is
installed using Homebrew; without Homebrew, install CMake first from cmake.org.
Audio and speech recognition stay on your Mac.
'

if [[ "$(uname -s)" == "Darwin" ]] && command -v hdiutil >/dev/null 2>&1; then
    staging="$DIST/.staging"
    mkdir -p "$staging"
    cp -R "$APP" "$staging/"
    printf '%s' "$README_TEXT" >"$staging/Read Me.txt"
    created=0
    for attempt in 1 2 3; do
        # Detach an image leaked by an earlier attempt before replacing it.
        if ! detach_stale_dmg; then
            rm -rf "$staging"
            exit 1
        fi
        rm -f "$DMG"
        if hdiutil create -volname "Install LocalFlow" -srcfolder "$staging" \
            -ov -format UDZO "$DMG"; then
            created=1
            break
        fi
        echo "hdiutil create failed (attempt $attempt of 3)." >&2
        if ((attempt < 3)); then
            sleep "$attempt"
        fi
    done
    rm -rf "$staging"
    if ((!created)); then
        if detach_stale_dmg; then
            rm -f "$DMG"
        fi
        echo "Could not create $DMG after 3 attempts." >&2
        exit 1
    fi
    echo "Built $DMG"
else
    staging="$(mktemp -d "${TMPDIR:-/tmp}/localflow-zip.XXXXXX")"
    cp -R "$APP" "$staging/"
    printf '%s' "$README_TEXT" >"$staging/Read Me.txt"
    if command -v zip >/dev/null 2>&1; then
        (cd "$staging" && zip -qry "$ZIP" "Install LocalFlow.app" "Read Me.txt")
    else
        python3 - "$staging" "$ZIP" <<'PY'
import sys
import zipfile
from pathlib import Path

root, target = Path(sys.argv[1]), Path(sys.argv[2])
with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as archive:
    for path in sorted(root.rglob("*")):
        info = zipfile.ZipInfo.from_file(path, path.relative_to(root).as_posix())
        info.compress_type = zipfile.ZIP_DEFLATED
        data = b"" if path.is_dir() else path.read_bytes()
        archive.writestr(info, data)
PY
    fi
    rm -rf "$staging"
    echo "Built $ZIP"
fi
