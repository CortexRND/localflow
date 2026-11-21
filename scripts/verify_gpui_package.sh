#!/bin/bash
# Verify a LocalFlow Preview (native GPUI lf-desktop) archive built by
# scripts/build_gpui_package.sh, on the platform it was built for.
#
# Usage:
#   scripts/verify_gpui_package.sh <LocalFlow-Preview-<ver>-<os>-<arch>.tar.gz>
#       Check the member list (paths, types, links), extract into a fresh
#       temporary directory and verify the extracted copy: identity, file
#       manifest, native-library closure and relative search paths (Linux:
#       readelf/ldd, glibc/libstdc++ symbol floors; macOS: otool, ad-hoc
#       signatures), then non-GUI CLI smoke checks with an empty environment.
#   scripts/verify_gpui_package.sh --linux-abi <package-tree>
#       Print the tree's ABI requirements (used by the builder's MANIFEST.json).
#   scripts/verify_gpui_package.sh --macos-minos <LocalFlow Preview.app>
#       Print the bundle's minimum macOS: the highest Mach-O minimum and the
#       introduction version of each strongly linked framework it knows.
#
# It never opens a window, a microphone, a keyboard hook or the keyring, and
# downloads nothing: --version/--help/the sandbox refusal and the speech
# helper's stdin-EOF exit all happen before any of those.
#
# What this does NOT prove: that the GPU driver, Vulkan loader, Wayland,
# fontconfig or other libraries loaded at run time (dlopen) exist on another
# machine, or (macOS) Developer ID / notarisation / Gatekeeper approval or
# privacy permission prompts. Those are documented, not verified.
set -euo pipefail

PREVIEW_ID="com.cortexrnd.localflow.desktop-preview"
MAC_APP="LocalFlow Preview.app"
LINUX_ROOT="localflow-preview"
# The documented Linux ABI claim (rust/lf-desktop/README.md). A build whose
# binary or bundled libraries need newer symbol versions fails here instead
# of shipping a package that would not load where the README says it does.
LINUX_GLIBC_FLOOR="2.35"
LINUX_GLIBCXX_FLOOR="3.4.30"
LINUX_CXXABI_FLOOR="1.3.13"
# Explicit system-library allowlist for DT_NEEDED (rust/lf-desktop/README.md
# lists the distribution packages). Everything else must be bundled in lib/.
LINUX_SYSTEM_LIBS=(
    ld-linux-x86-64.so.2 libc.so.6 libm.so.6 libdl.so.2 librt.so.1 libpthread.so.0
    libgcc_s.so.1 libstdc++.so.6
    libasound.so.2 libxcb.so.1 libxkbcommon.so.0 libxkbcommon-x11.so.0
)
# shellcheck disable=SC2016 # literal loader tokens, not shell expansions
LINUX_BIN_RUNPATH='$ORIGIN:$ORIGIN/../lib'
# shellcheck disable=SC2016
LINUX_LIB_RUNPATH='$ORIGIN'

die() {
    echo "verify_gpui_package: $*" >&2
    exit 1
}

# True when dotted version $1 is greater than $2 (numeric per component).
version_gt() {
    local IFS=. i
    local -a a b
    read -ra a <<<"$1"
    read -ra b <<<"$2"
    for ((i = 0; i < ${#a[@]} || i < ${#b[@]}; i++)); do
        if ((10#${a[i]:-0} > 10#${b[i]:-0})); then return 0; fi
        if ((10#${a[i]:-0} < 10#${b[i]:-0})); then return 1; fi
    done
    return 1
}

# Highest dotted version among the arguments (empty when none).
version_max() {
    local best="" v
    for v in "$@"; do
        if [[ -z "$best" ]] || version_gt "$v" "$best"; then best="$v"; fi
    done
    printf '%s' "$best"
}

sha256_of() {
    local digest
    if command -v sha256sum >/dev/null 2>&1; then
        read -r digest _ < <(sha256sum "$1") || return 1
    else
        read -r digest _ < <(shasum -a 256 "$1") || return 1
    fi
    printf '%s\n' "$digest" | tr '[:upper:]' '[:lower:]'
}

is_system_lib() {
    local lib
    for lib in "${LINUX_SYSTEM_LIBS[@]}"; do
        [[ "$1" == "$lib" ]] && return 0
    done
    return 1
}

elf_files() {
    printf '%s\n' "$1/bin/lf-desktop"
    find "$1/lib" -type f -name 'lib*.so*' | LC_ALL=C sort
}

# DT_NEEDED entries of ELF $1, one per line.
elf_needed() {
    readelf -d "$1" | sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\].*/\1/p'
}

# RUNPATH or RPATH of ELF $1 (empty when none; fails if it has both).
elf_runpath() {
    local paths
    paths="$(readelf -d "$1" | sed -n 's/.*(\(RUNPATH\|RPATH\)).*path: \[\(.*\)\].*/\2/p')"
    [[ "$(printf '%s' "$paths" | grep -c '^' || true)" -le 1 ]] || die "$1 has both RPATH and RUNPATH"
    printf '%s' "$paths"
}

# Symbol versions named in the version-needs section of ELF $1.
elf_version_needs() {
    readelf -V --wide "$1" | awk '
        /^Version needs section/ { n = 1; next }
        /^Version (definition|symbols) section/ { n = 0 }
        n { for (i = 1; i < NF; i++) if ($i == "Name:") print $(i + 1) }'
}

# Print key=value ABI requirements of Linux package tree $1.
linux_abi() {
    local tree="$1" file need
    local -a glibc=() glibcxx=() cxxabi=() system=()
    while IFS= read -r file; do
        while IFS= read -r need; do
            case "$need" in
                GLIBC_[0-9]*) glibc+=("${need#GLIBC_}") ;;
                GLIBCXX_[0-9]*) glibcxx+=("${need#GLIBCXX_}") ;;
                CXXABI_[0-9]*) cxxabi+=("${need#CXXABI_}") ;;
            esac
        done < <(elf_version_needs "$file")
        while IFS= read -r need; do
            if is_system_lib "$need"; then system+=("$need"); fi
        done < <(elf_needed "$file")
    done < <(elf_files "$tree")
    printf 'glibc=%s\n' "$(version_max "${glibc[@]}")"
    printf 'glibcxx=%s\n' "$(version_max "${glibcxx[@]}")"
    printf 'cxxabi=%s\n' "$(version_max "${cxxabi[@]}")"
    printf 'system_libraries=%s\n' "$(printf '%s\n' "${system[@]}" | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ $//')"
}

# Mach-O files of bundle $1: the executable, then the bundled dylibs.
macho_files() {
    printf '%s\n' "$1/Contents/MacOS/lf-desktop"
    find "$1/Contents/Frameworks" -type f -name '*.dylib' | LC_ALL=C sort
}

# Minimum macOS recorded in Mach-O $1 (LC_BUILD_VERSION or LC_VERSION_MIN_MACOSX).
macho_minos() {
    otool -l "$1" | awk '
        /cmd LC_BUILD_VERSION/ { b = 1; next }
        b && $1 == "minos" { print $2; b = 0 }
        /cmd LC_VERSION_MIN_MACOSX/ { m = 1; next }
        m && $1 == "version" { print $2; m = 0 }'
}

# System frameworks Mach-O $1 links strongly (LC_LOAD_DYLIB / LC_REEXPORT_DYLIB;
# not LC_LOAD_WEAK_DYLIB): dyld refuses to launch it where one is missing.
macho_strong_frameworks() {
    otool -l "$1" | awk '
        /cmd LC_(LOAD|REEXPORT)_DYLIB$/ { d = 1; next }
        /^[[:space:]]*cmd / { d = 0 }
        d && $1 == "name" { print $2; d = 0 }' \
        | sed -n 's|^/System/Library/Frameworks/\([^/]*\)\.framework/.*|\1|p'
}

# The macOS version that introduced framework $1, when it is newer than every
# minimum the pinned toolchain and native libraries could otherwise yield
# (empty for frameworks available since before macOS 11). LC_BUILD_VERSION
# does not encode this: a binary built for 11.0 can still link a newer
# framework strongly and then fail to launch on the older system. These are
# framework introduction versions only: newer APIs inside a framework (such as
# ScreenCaptureKit audio capture, macOS 13.0) are covered by the builder's
# MACOSX_DEPLOYMENT_TARGET floor, not by this table.
MACOS_FRAMEWORK_FLOORS=(ScreenCaptureKit=12.3 MetalFX=13.0)
# The executable's own minimum must cover the newest API it calls without an
# availability check: SCStreamConfiguration audio capture (capturesAudio and
# friends, macOS 13.0) in lf-meetings' ScreenCaptureKitSource. Same value as
# build_gpui_package.sh's MACOS_DEPLOYMENT_FLOOR.
MACOS_API_FLOOR="13.0"
framework_floor() {
    local entry
    for entry in "${MACOS_FRAMEWORK_FLOORS[@]}"; do
        if [[ "${entry%%=*}" == "$1" ]]; then
            printf '%s' "${entry#*=}"
            return 0
        fi
    done
    return 0
}

# The bundle's minimum macOS: the highest of every Mach-O minimum and the
# introduction version of every strongly linked framework in the table above.
macos_minos() {
    local file v framework
    local -a versions=()
    while IFS= read -r file; do
        while IFS= read -r v; do versions+=("$v"); done < <(macho_minos "$file")
        while IFS= read -r framework; do
            v="$(framework_floor "$framework")"
            [[ -z "$v" ]] || versions+=("$v")
        done < <(macho_strong_frameworks "$file")
    done < <(macho_files "$1")
    [[ "${#versions[@]}" -gt 0 ]] || die "no minimum macOS version recorded in $1"
    version_max "${versions[@]}"
}

# Fail when an image strongly links a framework newer than its own recorded
# minimum, i.e. the image itself claims an OS it cannot launch on.
check_framework_floors() {
    local file own framework floor v
    while IFS= read -r file; do
        own=""
        while IFS= read -r v; do own="$(version_max "$v" "$own")"; done < <(macho_minos "$file")
        while IFS= read -r framework; do
            floor="$(framework_floor "$framework")"
            [[ -z "$floor" ]] && continue
            ! version_gt "$floor" "${own:-0}" \
                || die "${file##*/} links $framework (macOS $floor+) strongly but records minimum macOS ${own:-none}; build with MACOSX_DEPLOYMENT_TARGET>=$floor"
        done < <(macho_strong_frameworks "$file")
    done < <(macho_files "$1")
}

# LC_RPATH entries of Mach-O $1, one per line.
macho_rpaths() {
    otool -l "$1" | awk '/cmd LC_RPATH/{r=1;next} r && /^[[:space:]]*path /{sub(/^[[:space:]]*path /, "");sub(/ \(offset.*$/, "");print;r=0}'
}

# Validate archive $1's member list before anything is extracted: every
# member sits under root $2, has no absolute/parent/dot component, control
# character or backslash, and is a regular file or directory (no symlink,
# hard link, device or FIFO).
check_members() {
    local archive="$1" root="$2" member line names verbose
    names="$(tar -tzf "$archive")" || die "cannot list $archive"
    verbose="$(tar -tvzf "$archive")" || die "cannot list $archive"
    [[ -n "$names" ]] || die "$archive is empty"
    [[ "$(printf '%s\n' "$names" | wc -l)" -eq "$(printf '%s\n' "$verbose" | wc -l)" ]] \
        || die "$archive has members whose names span lines"
    while IFS= read -r member; do
        member="${member%/}"
        [[ "$member" == "$root" || "$member" == "$root"/* ]] || die "unexpected archive member: $member"
        [[ "/$member/" != */../* && "/$member/" != */./* && "$member" != *//* ]] \
            || die "unsafe archive member path: $member"
        [[ ! "$member" =~ [[:cntrl:]] && "$member" != *\\* ]] || die "unsafe archive member name: $member"
    done <<<"$names"
    while IFS= read -r line; do
        case "${line:0:1}" in
            -|d) ;;
            *) die "archive member is not a regular file or directory: $line" ;;
        esac
    done <<<"$verbose"
}

# After extraction: nothing but regular single-link files and directories.
check_extracted() {
    local odd
    odd="$(find "$1" \( ! -type f ! -type d \) -print -o \( -type f -links +1 \) -print)"
    [[ -z "$odd" ]] || die "extracted links or special files: $odd"
}

# Run lf-desktop $1 with an empty environment and a throwaway HOME; never a
# display, a model, a device or real data.
smoke() {
    local exe="$1" version="$2" smoke_home out code
    smoke_home="$(mktemp -d "$work/smoke-home.XXXXXX")"
    run() { env -i PATH=/usr/bin:/bin HOME="$smoke_home" LANG=C "$exe" "$@"; }
    out="$(run --version)" || die "lf-desktop --version failed"
    [[ "$out" == "lf-desktop $version" ]] || die "lf-desktop --version printed '$out', expected 'lf-desktop $version'"
    out="$(run --help)" || die "lf-desktop --help failed"
    [[ "$out" == *--engine* && "$out" == *--sandbox* ]] || die "lf-desktop --help is missing --engine/--sandbox"
    code=0
    run --sandbox --engine --data-dir "$smoke_home/sandbox" >/dev/null 2>&1 || code=$?
    [[ "$code" -eq 2 && ! -e "$smoke_home/sandbox" ]] || die "--sandbox --engine was not refused before starting (exit $code)"
    code=0
    run --speech-helper </dev/null >/dev/null 2>&1 || code=$?
    [[ "$code" -eq 0 ]] || die "the runtime speech helper did not exit cleanly on stdin EOF (exit $code)"
    code=0
    run --speech-helper extra </dev/null >/dev/null 2>&1 || code=$?
    [[ "$code" -eq 2 ]] || die "the speech helper accepted an extra argument (exit $code)"
    [[ -z "$(ls -A "$smoke_home")" ]] || die "CLI smoke checks wrote into HOME: $(ls -A "$smoke_home")"
}

# The MANIFEST.json value of top-level string key $2 in file $1.
manifest_value() {
    sed -n "s/^  \"$2\": \"\\([^\"]*\\)\",\\{0,1\\}\$/\\1/p" "$1"
}

verify_linux() {
    local tree="$1" version="$2" manifest="$1/MANIFEST.json" file rel listed actual need runpath
    local -a bundled=()
    [[ -f "$manifest" ]] || die "MANIFEST.json is missing"
    [[ "$(manifest_value "$manifest" name)" == "$LINUX_ROOT" ]] || die "MANIFEST.json does not name $LINUX_ROOT"
    [[ "$(manifest_value "$manifest" version)" == "$version" ]] || die "MANIFEST.json version does not match the archive name"
    [[ "$(manifest_value "$manifest" os)" == linux && "$(manifest_value "$manifest" arch)" == x86_64 ]] \
        || die "MANIFEST.json platform is not linux-x86_64"
    for rel in bin/lf-desktop lib/libsherpa-onnx-c-api.so lib/libonnxruntime.so \
        share/applications/localflow-preview.desktop share/icons/localflow-preview.png README.txt; do
        [[ -f "$tree/$rel" ]] || die "package is missing $rel"
    done
    [[ -x "$tree/bin/lf-desktop" ]] || die "bin/lf-desktop is not executable"
    # Every file (except the manifest) is listed once with its hash, and nothing else is.
    listed="$(sed -n 's/^    "\([^"]*\)": "\([0-9a-f]\{64\}\)",\{0,1\}$/\1 \2/p' "$manifest" | LC_ALL=C sort)"
    actual="$(cd "$tree" && find . -type f ! -path ./MANIFEST.json | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r rel; do
        printf '%s %s\n' "$rel" "$(sha256_of "$rel")"
    done)"
    [[ -n "$listed" && "$listed" == "$actual" ]] || die "package files do not match MANIFEST.json"
    while IFS= read -r file; do bundled+=("${file##*/}"); done < <(find "$tree/lib" -type f -name 'lib*.so*')
    while IFS= read -r file; do
        runpath="$(elf_runpath "$file")"
        if [[ "$file" == "$tree/bin/lf-desktop" ]]; then
            [[ "$runpath" == "$LINUX_BIN_RUNPATH" ]] || die "bin/lf-desktop search path is '$runpath', expected '$LINUX_BIN_RUNPATH'"
        else
            [[ "$runpath" == "$LINUX_LIB_RUNPATH" ]] || die "${file#"$tree"/} search path is '$runpath', expected '$LINUX_LIB_RUNPATH'"
        fi
        while IFS= read -r need; do
            if [[ " ${bundled[*]} " == *" $need "* ]] || is_system_lib "$need"; then continue; fi
            die "${file#"$tree"/} needs $need, which is neither bundled nor an allowed system library"
        done < <(elf_needed "$file")
        # The loader must resolve bundled libraries inside the package and the
        # rest from the system, with no inherited search path or preload.
        local ldd_out name path
        ldd_out="$(env -u LD_LIBRARY_PATH -u LD_PRELOAD -u LD_AUDIT ldd "$file")" || die "ldd failed for ${file#"$tree"/}"
        if grep -q 'not found' <<<"$ldd_out"; then die "unresolved libraries for ${file#"$tree"/}: $ldd_out"; fi
        while read -r name _ path _; do
            [[ -n "$path" && "$path" == /* ]] || continue
            if [[ " ${bundled[*]} " == *" $name "* ]]; then
                [[ "$path" -ef "$tree/lib/$name" ]] || die "$name resolves to $path, not the bundled copy"
            elif [[ "$path" != /lib/* && "$path" != /lib64/* && "$path" != /usr/lib/* && "$path" != /usr/lib64/* ]]; then
                die "$name resolves outside the system library directories: $path"
            fi
        done < <(grep '=>' <<<"$ldd_out")
    done < <(elf_files "$tree")
    local abi key value
    abi="$(linux_abi "$tree")"
    while IFS='=' read -r key value; do
        [[ "$(manifest_value "$manifest" "$key")" == "$value" ]] || die "MANIFEST.json $key is not the measured '$value'"
    done <<<"$abi"
    value="$(sed -n 's/^glibc=//p' <<<"$abi")"
    [[ -n "$value" ]] || die "no glibc symbol versions found; cannot state the glibc requirement"
    ! version_gt "$value" "$LINUX_GLIBC_FLOOR" || die "needs glibc $value, newer than the documented $LINUX_GLIBC_FLOOR"
    value="$(sed -n 's/^glibcxx=//p' <<<"$abi")"
    [[ -z "$value" ]] || ! version_gt "$value" "$LINUX_GLIBCXX_FLOOR" || die "needs GLIBCXX_$value, newer than the documented $LINUX_GLIBCXX_FLOOR"
    value="$(sed -n 's/^cxxabi=//p' <<<"$abi")"
    [[ -z "$value" ]] || ! version_gt "$value" "$LINUX_CXXABI_FLOOR" || die "needs CXXABI_$value, newer than the documented $LINUX_CXXABI_FLOOR"
    local entry="$tree/share/applications/localflow-preview.desktop"
    desktop-file-validate "$entry" || die "invalid desktop entry"
    grep -qx 'Exec=localflow-preview' "$entry" || die "the packaged desktop entry must run localflow-preview with no arguments"
    if grep -q -- '--engine' "$entry"; then die "the desktop entry must not pass --engine"; fi
    smoke "$tree/bin/lf-desktop" "$version"
    echo "Verified Linux package: $abi" | tr '\n' ' '
    echo
}

verify_macos() {
    local app="$1" version="$2" plist="$1/Contents/Info.plist" file dep name rpaths
    plutil -lint "$plist" >/dev/null || die "Info.plist is invalid"
    [[ "$(plutil -extract CFBundleIdentifier raw -o - "$plist")" == "$PREVIEW_ID" ]] || die "bundle identifier is not $PREVIEW_ID"
    [[ "$(plutil -extract CFBundleExecutable raw -o - "$plist")" == lf-desktop ]] || die "CFBundleExecutable is not lf-desktop"
    [[ "$(plutil -extract CFBundleShortVersionString raw -o - "$plist")" == "$version" ]] || die "bundle version does not match the archive name"
    if plutil -extract LocalFlowRuntime raw -o - "$plist" >/dev/null 2>&1; then
        die "the preview must not carry LocalFlowRuntime (the legacy installer deletes apps that do)"
    fi
    for name in NSMicrophoneUsageDescription NSAccessibilityUsageDescription NSInputMonitoringUsageDescription; do
        [[ -n "$(plutil -extract "$name" raw -o - "$plist")" ]] || die "Info.plist is missing $name"
    done
    check_framework_floors "$app"
    local exe_minos="" v
    while IFS= read -r v; do exe_minos="$(version_max "$v" "$exe_minos")"; done \
        < <(macho_minos "$app/Contents/MacOS/lf-desktop")
    ! version_gt "$MACOS_API_FLOOR" "${exe_minos:-0}" \
        || die "lf-desktop records minimum macOS ${exe_minos:-none}, below the $MACOS_API_FLOOR its ScreenCaptureKit audio capture needs; build with MACOSX_DEPLOYMENT_TARGET>=$MACOS_API_FLOOR"
    [[ "$(plutil -extract LSMinimumSystemVersion raw -o - "$plist")" == "$(macos_minos "$app")" ]] \
        || die "LSMinimumSystemVersion does not match the bundled Mach-O and strongly linked framework minimum"
    echo "Strongly linked system frameworks: $(macho_strong_frameworks "$app/Contents/MacOS/lf-desktop" | LC_ALL=C sort -u | tr '\n' ' ')"
    # Only the executable, the bundled dylibs, the icon, the plist and the seal.
    local unexpected
    unexpected="$( (cd "$app" && find . -type f) | sed 's|^\./||' \
        | { grep -Ev '^Contents/(Info\.plist|MacOS/lf-desktop|Frameworks/[^/]+\.dylib|Resources/LocalFlow\.icns|_CodeSignature/CodeResources)$' || true; })"
    [[ -z "$unexpected" ]] || die "unexpected files in the bundle (no launch agent or helper belongs here): $unexpected"
    [[ -f "$app/Contents/Frameworks/libsherpa-onnx-c-api.dylib" && -f "$app/Contents/Frameworks/libonnxruntime.dylib" ]] \
        || die "Sherpa/ORT dylibs are missing"
    while IFS= read -r file; do
        rpaths="$(macho_rpaths "$file")"
        if [[ "$file" == */MacOS/lf-desktop ]]; then
            [[ "$rpaths" == '@executable_path/../Frameworks' ]] || die "lf-desktop search paths are '$rpaths'"
        else
            [[ "$rpaths" == '@loader_path' ]] || die "${file##*/} search paths are '$rpaths'"
            [[ "$(otool -D "$file" | tail -n +2)" == "@rpath/${file##*/}" ]] || die "${file##*/} has a non-relocatable install name"
        fi
        while IFS= read -r dep; do
            name="${dep##*/}"
            if [[ "$dep" == "@rpath/$name" && -f "$app/Contents/Frameworks/$name" ]]; then continue; fi
            [[ "$dep" == /usr/lib/* || "$dep" == /System/Library/* ]] || die "unbundled dependency $dep in ${file##*/}"
        done < <(otool -L "$file" | tail -n +2 | sed -E 's/^[[:space:]]*//;s/ \(compatibility version.*$//')
        [[ "$file" == */MacOS/lf-desktop ]] || codesign --verify --strict "$file" || die "invalid signature on ${file##*/}"
    done < <(macho_files "$app")
    codesign --verify --deep --strict --verbose=2 "$app" || die "bundle signature does not verify"
    codesign -d -r- "$app" 2>&1 | grep -F "designated => identifier \"$PREVIEW_ID\"" >/dev/null \
        || die "designated requirement is not the stable preview identifier"
    smoke "$app/Contents/MacOS/lf-desktop" "$version"
    echo "Verified macOS bundle (ad-hoc signed; minimum macOS $(macos_minos "$app"))"
}

case "${1:-}" in
    --linux-abi)
        [[ $# -eq 2 && -d "$2" ]] || die "usage: $0 --linux-abi <package-tree>"
        linux_abi "$2"
        exit 0
        ;;
    --macos-minos)
        [[ $# -eq 2 && -d "$2" ]] || die "usage: $0 --macos-minos <app>"
        macos_minos "$2"
        echo
        exit 0
        ;;
    --version-gt)
        # Exit 0 when dotted version $2 is greater than $3 (used by the builder).
        [[ $# -eq 3 ]] || die "usage: $0 --version-gt <a> <b>"
        if version_gt "$2" "$3"; then exit 0; fi
        exit 1
        ;;
    ""|-*)
        echo "Usage: $0 <LocalFlow-Preview-<ver>-<os>-<arch>.tar.gz>" >&2
        exit 2
        ;;
esac
[[ $# -eq 1 ]] || die "usage: $0 <archive>"
archive="$1"
[[ -f "$archive" ]] || die "no such archive: $archive"
base="${archive##*/}"
[[ "$base" =~ ^LocalFlow-Preview-([0-9]+\.[0-9]+\.[0-9]+)-(linux-x86_64|macos-arm64|macos-x86_64)\.tar\.gz$ ]] \
    || die "not a LocalFlow Preview archive name: $base"
version="${BASH_REMATCH[1]}"
platform="${BASH_REMATCH[2]}"
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) here=linux-x86_64 ;;
    Darwin-arm64) here=macos-arm64 ;;
    Darwin-x86_64) here=macos-x86_64 ;;
    *) here="unsupported" ;;
esac
[[ "$platform" == "$here" ]] || die "$base is for $platform; verify it on that platform (this is $here)"
work="$(mktemp -d "${TMPDIR:-/tmp}/localflow-preview-verify.XXXXXX")"
trap 'rm -rf "$work"' EXIT
if [[ "$platform" == linux-* ]]; then root="$LINUX_ROOT"; else root="$MAC_APP"; fi
check_members "$archive" "$root"
mkdir "$work/x"
tar -xzf "$archive" -C "$work/x" --no-same-owner
check_extracted "$work/x"
[[ "$(ls -A "$work/x")" == "$root" ]] || die "archive does not unpack to exactly $root"
if [[ "$platform" == linux-* ]]; then
    verify_linux "$work/x/$root" "$version"
else
    verify_macos "$work/x/$root" "$version"
fi
echo "Verified $base"
