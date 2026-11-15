#!/bin/bash
# Build "Install LocalFlow.app": a double-clickable bundle that opens Terminal.app
# and runs the bundled install.sh.
#
# Output (in dist/):
#   Install LocalFlow.app
#   LocalFlow-Installer.dmg        (macOS: app + Read Me.txt)
#   LocalFlow-Installer-mac.zip    (other systems; keeps the executable bit)
#
# Environment:
#   LOCALFLOW_REF  Ref the installer downloads (default: current git commit, else main)
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$ROOT/dist"
APP="$DIST/Install LocalFlow.app"
DMG="$DIST/LocalFlow-Installer.dmg"
ZIP="$DIST/LocalFlow-Installer-mac.zip"

REF="${LOCALFLOW_REF:-}"
if [[ -z "$REF" ]]; then
    REF="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)"
fi
REF="${REF:-main}"
if [[ ! "$REF" =~ ^[A-Za-z0-9._/-]+$ ]]; then
    echo "Unsupported LOCALFLOW_REF: $REF" >&2
    exit 2
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
    <string>0.1.0</string>
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
    printf 'export LOCALFLOW_REF=%q\n' "$ref"
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
printf '%s\n' "${launcher_template//@@LOCALFLOW_REF@@/$REF}" \
    >"$APP/Contents/MacOS/install-localflow"
chmod 755 "$APP/Contents/MacOS/install-localflow"

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

The app is not signed by Apple, so macOS may block it the first time:
  - Right-click (or Control-click) "Install LocalFlow" and choose Open, then
    click Open again; or
  - open System Settings > Privacy & Security, scroll down, and click
    "Open Anyway" next to the Install LocalFlow message.

Nothing needs to be installed beforehand: Homebrew, git and Python are
optional. Audio and speech recognition stay on your Mac.
'

if [[ "$(uname -s)" == "Darwin" ]] && command -v hdiutil >/dev/null 2>&1; then
    staging="$DIST/.staging"
    mkdir -p "$staging"
    cp -R "$APP" "$staging/"
    printf '%s' "$README_TEXT" >"$staging/Read Me.txt"
    hdiutil create -volname "Install LocalFlow" -srcfolder "$staging" \
        -ov -format UDZO "$DMG"
    rm -rf "$staging"
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
