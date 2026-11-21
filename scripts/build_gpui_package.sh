#!/bin/bash
# Build the ready-to-run LocalFlow Preview package: a release lf-desktop (the
# native GPUI app) with the runtime feature and its bundled Sherpa-ONNX/ONNX
# Runtime libraries, for the platform this runs on (native builds only):
#
#   LocalFlow-Preview-<ver>-linux-x86_64.tar.gz   localflow-preview/{bin,lib,share}, MANIFEST.json
#   LocalFlow-Preview-<ver>-macos-arm64.tar.gz    LocalFlow Preview.app (ad-hoc signed)
#   LocalFlow-Preview-<ver>-macos-x86_64.tar.gz   LocalFlow Preview.app (ad-hoc signed)
#
# plus <archive>.sha256. <ver> is rust/lf-desktop/Cargo.toml's version. The
# archive is verified (scripts/verify_gpui_package.sh) after extraction into a
# fresh directory before it replaces anything in the output directory.
#
# Usage: scripts/build_gpui_package.sh [<absolute output dir>]   (default dist/gpui-preview)
#
# This is additive: it does not touch scripts/build_rust_app.sh, the legacy
# LocalFlow.app (com.cortexrnd.localflow.app), its login agent or install.sh.
# The preview bundle has its own identity and never carries LocalFlowRuntime.
set -euo pipefail

PREVIEW_ID="com.cortexrnd.localflow.desktop-preview"
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/rust/lf-desktop/Cargo.toml"

command -v cargo >/dev/null || { echo "Install Rust/Cargo to build the LocalFlow Preview package." >&2; exit 1; }
[[ -z "${CARGO_BUILD_TARGET:-}" ]] || { echo "Unset CARGO_BUILD_TARGET; this builds a native package only." >&2; exit 1; }
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) os=linux arch=x86_64 ext=so ;;
    Darwin-arm64) os=macos arch=arm64 ext=dylib ;;
    Darwin-x86_64) os=macos arch=x86_64 ext=dylib ;;
    *) echo "LocalFlow Preview packages are built for Linux x86_64 and macOS arm64/x86_64 only." >&2; exit 1 ;;
esac
OUT="${1:-$ROOT/dist/gpui-preview}"
[[ $# -le 1 && "$OUT" == /* ]] || { echo "Supply an absolute output directory." >&2; exit 2; }
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$MANIFEST" | head -n 1)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Cannot read the lf-desktop version from $MANIFEST." >&2; exit 1; }
asset="LocalFlow-Preview-$version-$os-$arch.tar.gz"

# macOS: build for at least 13.0, the API minimum of what the binary uses, so
# the executable's own recorded minimum is honest. lf-desktop strongly links
# ScreenCaptureKit (GPUI's screen capture and lf-meetings' system audio); the
# framework itself first shipped in macOS 12.3, but lf-meetings'
# ScreenCaptureKitSource (rust/lf-meetings/src/macos_audio.rs) unconditionally
# configures SCStreamConfiguration audio capture (capturesAudio,
# excludesCurrentProcessAudio, sampleRate, channelCount), which Apple
# introduced in macOS 13.0. The verifier additionally folds every bundled
# library's minimum and strongly linked framework's introduction version into
# LSMinimumSystemVersion. A higher explicit target is respected.
MACOS_DEPLOYMENT_FLOOR="13.0"
if [[ "$os" == macos ]]; then
    target="${MACOSX_DEPLOYMENT_TARGET:-$MACOS_DEPLOYMENT_FLOOR}"
    if [[ ! "$target" =~ ^[0-9]+(\.[0-9]+)*$ ]] || bash "$ROOT/scripts/verify_gpui_package.sh" --version-gt "$MACOS_DEPLOYMENT_FLOOR" "$target"; then
        echo "MACOSX_DEPLOYMENT_TARGET=$target is below the package floor $MACOS_DEPLOYMENT_FLOOR (ScreenCaptureKit audio capture API)." >&2
        exit 1
    fi
    export MACOSX_DEPLOYMENT_TARGET="$target"
fi

# The desktop app is its own Cargo workspace; keep its canonical target dir
# (as cargo would) unless the caller chose one explicitly.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/rust/lf-desktop/target}"
mkdir -p "$CARGO_TARGET_DIR" "$OUT"
CARGO_TARGET_DIR="$(cd "$CARGO_TARGET_DIR" && pwd)"
export CARGO_TARGET_DIR
release="$CARGO_TARGET_DIR/release"
required=("libsherpa-onnx-c-api.$ext" "libonnxruntime.$ext")

# Rust caches may keep fingerprints while pruning the native libraries the
# sherpa-onnx-sys build script copies next to the binary, or leave a partly
# extracted SDK. Remove only an incomplete extracted SDK directory inside the
# target dir, and rebuild the sys crate only when a copied library is missing.
for sdk in "$CARGO_TARGET_DIR"/sherpa-onnx-prebuilt/*; do
    [[ -d "$sdk" && ! -L "$sdk" ]] || continue
    for lib in "${required[@]}"; do
        if [[ ! -f "$sdk/lib/$lib" ]]; then
            echo "Removing incomplete Sherpa SDK: $sdk"
            rm -rf -- "$sdk"
            break
        fi
    done
done
for lib in "${required[@]}"; do
    if [[ ! -f "$release/$lib" ]]; then
        cargo clean --manifest-path "$MANIFEST" --release -p sherpa-onnx-sys
        break
    fi
done
cargo build --manifest-path "$MANIFEST" --locked --release --features runtime --bin lf-desktop
for lib in "${required[@]}"; do
    [[ -f "$release/$lib" ]] || { echo "Sherpa/ORT runtime library $lib is missing from $release." >&2; exit 1; }
done

stage="$(mktemp -d "$OUT/.localflow-preview-build.XXXXXX")"
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/pkg"
shopt -s nullglob

sha256_line() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi
}

build_linux() {
    local tree="$stage/pkg/localflow-preview" lib rel digest
    mkdir -p "$tree/bin" "$tree/lib" "$tree/share/applications" "$tree/share/icons"
    cp "$release/lf-desktop" "$tree/bin/lf-desktop"
    chmod 755 "$tree/bin/lf-desktop"
    # Every Sherpa/ORT library and alias, as regular files (no links).
    for lib in "$release"/lib*.so "$release"/lib*.so.*; do
        cp -L "$lib" "$tree/lib/"
        chmod 644 "$tree/lib/${lib##*/}"
    done
    cp "$ROOT/packaging/macos/LocalFlow.png" "$tree/share/icons/localflow-preview.png"
    cat >"$tree/share/applications/localflow-preview.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=LocalFlow Preview
Comment=LocalFlow native desktop app (GPUI preview)
Exec=localflow-preview
Icon=localflow-preview
Terminal=false
Categories=Utility;Accessibility;
EOF
    cat >"$tree/README.txt" <<EOF
LocalFlow Preview $version (native GPUI desktop app, Linux x86_64)

This is a preview beside the existing LocalFlow launchers; it does not replace
lf, lf hub, lf setup or the Python runtime, and it starts nothing on login.

Install (no Rust needed), from a LocalFlow checkout:
    scripts/install_gpui_preview.sh --archive LocalFlow-Preview-$version-linux-x86_64.tar.gz \\
        --sha256 LocalFlow-Preview-$version-linux-x86_64.tar.gz.sha256
Run:
    localflow-preview            (front end over your LocalFlow data)
    localflow-preview --engine   (also allow dictation in this window; press Start)
    localflow-preview --sandbox  (example data only)

Requires glibc and libstdc++ at least as new as MANIFEST.json records, plus
the system libraries it lists and a working Vulkan driver, Wayland or X11,
and fontconfig. See rust/lf-desktop/README.md for distribution packages.
EOF
    local key value abi
    abi="$(bash "$ROOT/scripts/verify_gpui_package.sh" --linux-abi "$tree")"
    {
        printf '{\n'
        printf '  "name": "localflow-preview",\n'
        printf '  "app": "lf-desktop",\n'
        printf '  "version": "%s",\n' "$version"
        printf '  "os": "linux",\n'
        printf '  "arch": "x86_64",\n'
        while IFS='=' read -r key value; do
            printf '  "%s": "%s",\n' "$key" "$value"
        done <<<"$abi"
        printf '  "files": {\n'
        (cd "$tree" && find . -type f | sed 's|^\./||' | LC_ALL=C sort) | {
            first=1
            while IFS= read -r rel; do
                read -r digest _ < <(cd "$tree" && sha256_line "$rel")
                [[ "$first" -eq 1 ]] || printf ',\n'
                printf '    "%s": "%s"' "$rel" "$digest"
                first=0
            done
            printf '\n'
        }
        printf '  }\n}\n'
    } >"$stage/MANIFEST.json"
    mv "$stage/MANIFEST.json" "$tree/MANIFEST.json"
    tar -C "$stage/pkg" --owner=0 --group=0 --numeric-owner -czf "$stage/$asset" localflow-preview
}

build_macos() {
    local bundle="$stage/pkg/LocalFlow Preview.app"
    local macos="$bundle/Contents/MacOS" frameworks="$bundle/Contents/Frameworks" resources="$bundle/Contents/Resources"
    local image lib load_commands rpath dependencies dependency name
    mkdir -p "$macos" "$frameworks" "$resources"
    cp "$release/lf-desktop" "$macos/lf-desktop"
    cp "$ROOT/packaging/macos/LocalFlow.icns" "$resources/LocalFlow.icns"
    chmod +x "$macos/lf-desktop"
    for lib in "$release"/*.dylib; do cp -L "$lib" "$frameworks/"; done
    [[ -f "$frameworks/libsherpa-onnx-c-api.dylib" && -f "$frameworks/libonnxruntime.dylib" ]] || { echo "Sherpa/ORT dylibs are missing." >&2; exit 1; }

    # Same relocation as scripts/build_rust_app.sh: remove build-machine search
    # paths; every non-system dependency must be bundled.
    for image in "$macos/lf-desktop" "$frameworks"/*.dylib; do
        load_commands="$(otool -l "$image")"
        while IFS= read -r rpath; do
            install_name_tool -delete_rpath "$rpath" "$image"
        done < <(printf '%s\n' "$load_commands" | awk '/cmd LC_RPATH/{r=1;next} r && /^[[:space:]]*path /{sub(/^[[:space:]]*path /, "");sub(/ \(offset.*$/, "");print;r=0}')
        if [[ "$image" == "$macos/lf-desktop" ]]; then
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

    local plist="$bundle/Contents/Info.plist" minos
    minos="$(bash "$ROOT/scripts/verify_gpui_package.sh" --macos-minos "$bundle")"
    plutil -create xml1 "$plist"
    plutil -insert CFBundleIdentifier -string "$PREVIEW_ID" "$plist"
    plutil -insert CFBundleName -string 'LocalFlow Preview' "$plist"
    plutil -insert CFBundleDisplayName -string 'LocalFlow Preview' "$plist"
    plutil -insert CFBundleExecutable -string lf-desktop "$plist"
    plutil -insert CFBundleIconFile -string LocalFlow.icns "$plist"
    plutil -insert CFBundlePackageType -string APPL "$plist"
    plutil -insert CFBundleVersion -string "$version" "$plist"
    plutil -insert CFBundleShortVersionString -string "$version" "$plist"
    plutil -insert LSMinimumSystemVersion -string "$minos" "$plist"
    plutil -insert LSUIElement -bool NO "$plist"
    plutil -insert NSHighResolutionCapable -bool YES "$plist"
    plutil -insert NSMicrophoneUsageDescription -string 'LocalFlow uses the microphone for dictation and meeting recording.' "$plist"
    plutil -insert NSAccessibilityUsageDescription -string 'LocalFlow uses Accessibility to paste dictated text into the active application.' "$plist"
    plutil -insert NSInputMonitoringUsageDescription -string 'LocalFlow monitors the keyboard for the dictation hotkey.' "$plist"
    plutil -lint "$plist"
    # Signatures must not depend on extended attributes the archive drops.
    xattr -cr "$bundle"
    local requirement="designated => identifier \"$PREVIEW_ID\""
    for lib in "$frameworks"/*.dylib; do codesign --force -s - "$lib"; done
    codesign --force -s - --identifier "$PREVIEW_ID" --requirements "=$requirement" "$bundle"
    codesign --verify --deep --strict --verbose=2 "$bundle"
    COPYFILE_DISABLE=1 tar -C "$stage/pkg" -czf "$stage/$asset" "LocalFlow Preview.app"
}

"build_$os"
(cd "$stage" && sha256_line "$asset" >"$asset.sha256")
bash "$ROOT/scripts/verify_gpui_package.sh" "$stage/$asset"
mv -f "$stage/$asset" "$OUT/$asset"
mv -f "$stage/$asset.sha256" "$OUT/$asset.sha256"
echo "Built and verified: $OUT/$asset"
