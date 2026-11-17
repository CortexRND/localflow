#!/bin/bash
set -euo pipefail

[[ "$(uname -s)" == Darwin ]] || { echo "Rust app packaging requires macOS." >&2; exit 1; }
command -v cargo >/dev/null || { echo "Install Rust/Cargo before selecting LOCALFLOW_RUNTIME=rust." >&2; exit 1; }
[[ -z "${CARGO_BUILD_TARGET:-}" ]] || { echo "Unset CARGO_BUILD_TARGET; this builds a native macOS app." >&2; exit 1; }
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
APP="${1:-$ROOT/dist/LocalFlow.app}"
[[ "$APP" == /* && "$APP" == *.app ]] || { echo "Supply an absolute .app output path." >&2; exit 2; }
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/rust/target}"
mkdir -p "$CARGO_TARGET_DIR" "$(dirname "$APP")"
CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
export CARGO_TARGET_DIR

# rust-cache may retain fingerprints while omitting copied native dylibs.
cargo clean --manifest-path "$ROOT/rust/Cargo.toml" --release -p sherpa-onnx-sys
cargo build --manifest-path "$ROOT/rust/Cargo.toml" --locked --release -p lf-ui --bin lf-hub --features runtime
stage="$(mktemp -d "$(dirname "$APP")/.localflow-bundle.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
bundle="$stage/LocalFlow.app"
macos="$bundle/Contents/MacOS"
frameworks="$bundle/Contents/Frameworks"
resources="$bundle/Contents/Resources"
mkdir -p "$macos" "$frameworks" "$resources"
cp "$CARGO_TARGET_DIR/release/lf-hub" "$macos/lf-hub"
cp "$ROOT/scripts/macos_launchagent.sh" "$resources/macos_launchagent.sh"
cp "$ROOT/packaging/macos/LocalFlow.icns" "$resources/LocalFlow.icns"
chmod +x "$macos/lf-hub" "$resources/macos_launchagent.sh"
shopt -s nullglob
libs=("$CARGO_TARGET_DIR"/release/*.dylib)
[[ "${#libs[@]}" -ge 2 ]] || { echo "Sherpa/ORT runtime dylibs are missing." >&2; exit 1; }
for lib in "${libs[@]}"; do cp -L "$lib" "$frameworks/"; done
[[ -f "$frameworks/libsherpa-onnx-c-api.dylib" && -f "$frameworks/libonnxruntime.dylib" ]] || { echo "Sherpa/ORT dylibs are missing." >&2; exit 1; }

# Remove build-machine search paths; every non-system dependency must be bundled.
for image in "$macos/lf-hub" "$frameworks"/*.dylib; do
    load_commands="$(otool -l "$image")"
    while IFS= read -r rpath; do
        install_name_tool -delete_rpath "$rpath" "$image"
    done < <(printf '%s\n' "$load_commands" | awk '/cmd LC_RPATH/{r=1;next} r && /^[[:space:]]*path /{sub(/^[[:space:]]*path /, "");sub(/ \(offset.*$/, "");print;r=0}')
    if [[ "$image" == "$macos/lf-hub" ]]; then
        install_name_tool -add_rpath '@executable_path/../Frameworks' "$image"
    else
        install_name_tool -id "@rpath/$(basename "$image")" "$image"
        install_name_tool -add_rpath '@loader_path' "$image"
    fi
    dependencies="$(otool -L "$image")"
    while IFS= read -r dependency; do
        name="$(basename "$dependency")"
        if [[ -f "$frameworks/$name" ]]; then
            if [[ "$dependency" != "@rpath/$name" ]]; then
                install_name_tool -change "$dependency" "@rpath/$name" "$image"
            fi
        elif [[ "$dependency" != /usr/lib/* && "$dependency" != /System/Library/* ]]; then
            echo "Unbundled dependency $dependency in $image" >&2
            exit 1
        fi
    done < <(printf '%s\n' "$dependencies" | tail -n +2 | sed -E 's/^[[:space:]]*//;s/ \(compatibility version.*$//')
done

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/rust/Cargo.toml" | head -n 1)"
plist="$bundle/Contents/Info.plist"
plutil -create xml1 "$plist"
plutil -insert CFBundleIdentifier -string com.cortexrnd.localflow.app "$plist"
plutil -insert CFBundleName -string LocalFlow "$plist"
plutil -insert CFBundleDisplayName -string LocalFlow "$plist"
plutil -insert CFBundleExecutable -string lf-hub "$plist"
plutil -insert CFBundleIconFile -string LocalFlow.icns "$plist"
plutil -insert CFBundlePackageType -string APPL "$plist"
plutil -insert CFBundleVersion -string "$version" "$plist"
plutil -insert CFBundleShortVersionString -string "$version" "$plist"
plutil -insert LSUIElement -bool NO "$plist"
plutil -insert NSHighResolutionCapable -bool YES "$plist"
plutil -insert LocalFlowRuntime -string rust "$plist"
plutil -insert NSMicrophoneUsageDescription -string 'LocalFlow uses the microphone for dictation and meeting recording.' "$plist"
plutil -insert NSAccessibilityUsageDescription -string 'LocalFlow uses Accessibility to paste dictated text into the active application.' "$plist"
plutil -insert NSInputMonitoringUsageDescription -string 'LocalFlow monitors the keyboard for the dictation hotkey.' "$plist"
plutil -lint "$plist"
bundle_id="$(plutil -extract CFBundleIdentifier raw -o - "$plist")"
requirement="designated => identifier \"$bundle_id\""
for lib in "$frameworks"/*.dylib; do codesign --force -s - "$lib"; done
codesign --force -s - --identifier "$bundle_id" --requirements "=$requirement" "$bundle"
codesign --verify --deep --strict --verbose=2 "$bundle"
rm -rf "$APP"
mv "$bundle" "$APP"
echo "Built and ad-hoc signed: $APP"
