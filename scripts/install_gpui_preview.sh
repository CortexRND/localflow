#!/bin/bash
# Install, reinstall or uninstall LocalFlow Preview: the native GPUI desktop
# app (lf-desktop), packaged by scripts/build_gpui_package.sh. No Rust
# toolchain is needed. This is separate from install.sh and changes nothing it
# installs: not lf, localflow, lf hub/phrases/setup, LocalFlow.app, its login
# agent or the localflow*.desktop entries.
#
# It owns only:
#   macOS  ~/Applications/LocalFlow Preview.app   (com.cortexrnd.localflow.desktop-preview)
#   Linux  ~/.local/share/localflow-preview/      (package tree with MANIFEST.json)
#          ~/.local/bin/localflow-preview         (symlink to its bin/lf-desktop)
#          ~/.local/share/applications/localflow-preview.desktop
# and refuses to replace or remove anything at those paths that it did not
# install. Your LocalFlow data (~/.localflow, config, keyring) is never touched.
set -euo pipefail

PREVIEW_RELEASE="v0.2.3"
PREVIEW_ID="com.cortexrnd.localflow.desktop-preview"
MAC_APP="LocalFlow Preview.app"
LINUX_ROOT="localflow-preview"
DESKTOP_MARKER="X-LocalFlow-Preview=true"
SUMS_NAME="SHA256SUMS-gpui-preview"

usage() {
    cat <<EOF
Usage: install_gpui_preview.sh [--release <tag>] [--no-launch]
       install_gpui_preview.sh --archive <file> --sha256 <file|digest> [--no-launch]
       install_gpui_preview.sh --uninstall

Installs LocalFlow Preview (native GPUI app) beside the existing LocalFlow.

  --release <tag>    Download that release's package and $SUMS_NAME
                     and verify it (default $PREVIEW_RELEASE). Needs curl.
  --archive <file>   Install a local LocalFlow-Preview-<ver>-<os>-<arch>.tar.gz
                     (for example a CI artifact) instead of downloading.
  --sha256 <value>   Required with --archive: its .sha256 file, a $SUMS_NAME
                     file, or the 64-hex-digit SHA-256 itself.
  --no-launch        Install without opening the app.
  --uninstall        Remove only what this script installed.
  -h, --help         Show this help.

The app opens with no arguments: a front end over your LocalFlow data that
never starts dictation by itself. To let a window dictate, start it with
--engine and press Start (macOS: open -a "LocalFlow Preview" --args --engine;
Linux: localflow-preview --engine). Nothing is added to login items.

Environment (testing only):
  LOCALFLOW_PREVIEW_BASE_URL   Download from this copy of the release assets.
EOF
}

die() {
    echo "install_gpui_preview: $*" >&2
    exit 1
}

usage_error() {
    echo "install_gpui_preview: $*" >&2
    echo "Run with --help for usage." >&2
    exit 2
}

release="$PREVIEW_RELEASE" archive="" sha="" no_launch=0 uninstall=0 release_set=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) [[ $# -ge 2 ]] || usage_error "--release needs a tag"; release="$2"; release_set=1; shift 2 ;;
        --archive) [[ $# -ge 2 ]] || usage_error "--archive needs a file"; archive="$2"; shift 2 ;;
        --sha256) [[ $# -ge 2 ]] || usage_error "--sha256 needs a file or digest"; sha="$2"; shift 2 ;;
        --no-launch) no_launch=1; shift ;;
        --uninstall) uninstall=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage_error "unknown argument: $1" ;;
    esac
done
if [[ -n "$archive" || -n "$sha" ]]; then
    [[ -n "$archive" && -n "$sha" ]] || usage_error "--archive and --sha256 go together"
    [[ "$release_set" -eq 0 ]] || usage_error "--release and --archive are alternatives"
fi
if [[ "$uninstall" -eq 1 && ( -n "$archive" || "$release_set" -eq 1 || "$no_launch" -eq 1 ) ]]; then
    usage_error "--uninstall takes no other option"
fi
[[ "$release" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage_error "not a release tag: $release"

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) os=linux platform=linux-x86_64 ;;
    Darwin-arm64) os=macos platform=macos-arm64 ;;
    Darwin-x86_64) os=macos platform=macos-x86_64 ;;
    *) die "LocalFlow Preview packages exist for Linux x86_64 and macOS (Apple silicon and Intel) only." ;;
esac
[[ "${HOME:-}" == /* && -d "$HOME" ]] || die "HOME must be an existing absolute directory."
[[ ! "$HOME" =~ [[:cntrl:]] ]] || die "HOME contains control characters."

if [[ "$os" == macos ]]; then
    parent="$HOME/Applications"
    dest="$parent/$MAC_APP"
else
    parent="$HOME/.local/share"
    dest="$parent/$LINUX_ROOT"
    bin_link="$HOME/.local/bin/localflow-preview"
    desktop="$HOME/.local/share/applications/localflow-preview.desktop"
fi

# 0: absent, 1: installed by this script; anything else is refused.
owned_dest() {
    [[ -e "$dest" || -L "$dest" ]] || return 0
    [[ -d "$dest" && ! -L "$dest" ]] || return 2
    if [[ "$os" == macos ]]; then
        [[ -f "$dest/Contents/Info.plist" ]] \
            && [[ "$(plutil -extract CFBundleIdentifier raw -o - "$dest/Contents/Info.plist" 2>/dev/null || true)" == "$PREVIEW_ID" ]] \
            && return 1
    else
        [[ -f "$dest/MANIFEST.json" && ! -L "$dest/MANIFEST.json" ]] \
            && grep -qx "  \"name\": \"$LINUX_ROOT\"," "$dest/MANIFEST.json" && return 1
    fi
    return 2
}
owned_link() {
    [[ -e "$bin_link" || -L "$bin_link" ]] || return 0
    [[ -L "$bin_link" && "$(readlink "$bin_link")" == "$dest/bin/lf-desktop" ]] && return 1
    return 2
}
owned_desktop() {
    [[ -e "$desktop" || -L "$desktop" ]] || return 0
    [[ -f "$desktop" && ! -L "$desktop" ]] && grep -qx "$DESKTOP_MARKER" "$desktop" && return 1
    return 2
}
state() { local s=0; "$1" || s=$?; echo "$s"; }

refuse_foreign() {
    [[ "$(state owned_dest)" != 2 ]] || die "$dest exists but is not a LocalFlow Preview install; move it away first (nothing was changed)."
    if [[ "$os" == linux ]]; then
        [[ "$(state owned_link)" != 2 ]] || die "$bin_link exists but is not LocalFlow Preview's link; move it away first (nothing was changed)."
        [[ "$(state owned_desktop)" != 2 ]] || die "$desktop exists but was not written by this installer; move it away first (nothing was changed)."
    fi
}

if [[ "$uninstall" -eq 1 ]]; then
    refuse_foreign
    removed=0
    if [[ "$os" == linux ]]; then
        if [[ "$(state owned_link)" == 1 ]]; then rm -f "$bin_link"; removed=1; fi
        if [[ "$(state owned_desktop)" == 1 ]]; then
            rm -f "$desktop"; removed=1
            if command -v update-desktop-database >/dev/null 2>&1; then
                update-desktop-database "${desktop%/*}" || true
            fi
        fi
    fi
    if [[ "$(state owned_dest)" == 1 ]]; then rm -rf "$dest"; removed=1; fi
    if [[ "$removed" -eq 1 ]]; then
        echo "Removed LocalFlow Preview. Your LocalFlow data and the main LocalFlow app were left in place."
    else
        echo "LocalFlow Preview is not installed."
    fi
    exit 0
fi

refuse_foreign

sha256_of() {
    local digest
    if command -v sha256sum >/dev/null 2>&1; then
        read -r digest _ < <(sha256sum "$1") || return 1
    elif command -v shasum >/dev/null 2>&1; then
        read -r digest _ < <(shasum -a 256 "$1") || return 1
    else
        die "Neither sha256sum nor shasum is available to verify the package."
    fi
    printf '%s\n' "$digest" | tr '[:upper:]' '[:lower:]'
}

# Expected digest of name $2 from a sha256sum-format file $1 (empty when absent).
listed_sha256() {
    local hash entry
    while read -r hash entry || [[ -n "$hash" ]]; do
        entry="${entry%$'\r'}"
        entry="${entry#\*}"
        if [[ "$entry" == "$2" ]]; then
            printf '%s\n' "$hash" | tr '[:upper:]' '[:lower:]'
            return 0
        fi
    done <"$1"
}

work="$(mktemp -d "${TMPDIR:-/tmp}/localflow-preview-install.XXXXXX")"
stage=""
phase=prepare
recovery=()
# Shell-quote $1 for a copy-pasteable recovery command.
q() { printf '%q' "$1"; }
rollback() {
    # Undo a partly applied replacement so the previous install stays usable.
    # Only armed after every backup below was taken and checked, and each
    # path is restored or removed only according to its recorded prior state.
    # A path that already matches its saved state is left alone. Returns 1,
    # with manual steps in $recovery, if anything could not be put back.
    local saved
    if [[ "$os" == linux ]]; then
        rm -f "$desktop.tmp-$$" 2>/dev/null || true
        if [[ -f "$stage/link-saved" ]]; then
            saved="$(cat "$stage/link-saved")"
            if ! [[ -L "$bin_link" && "$(readlink "$bin_link")" == "$saved" ]]; then
                if ! ln -sfn "$saved" "$bin_link" 2>/dev/null \
                    || [[ "$(readlink "$bin_link" 2>/dev/null)" != "$saved" ]]; then
                    recovery+=("ln -sfn $(q "$saved") $(q "$bin_link")")
                fi
            fi
        elif [[ -f "$stage/link-absent" ]] && [[ -e "$bin_link" || -L "$bin_link" ]]; then
            rm -f "$bin_link" 2>/dev/null || recovery+=("rm -f $(q "$bin_link")")
        fi
        if [[ -f "$stage/desktop-saved" ]]; then
            if ! cmp -s "$stage/previous.desktop" "$desktop" 2>/dev/null; then
                if ! { cp "$stage/previous.desktop" "$desktop.restore-$$" 2>/dev/null \
                    && mv -f "$desktop.restore-$$" "$desktop" 2>/dev/null; }; then
                    rm -f "$desktop.restore-$$" 2>/dev/null || true
                    recovery+=("cp $(q "$stage/previous.desktop") $(q "$desktop")")
                fi
            fi
        elif [[ -f "$stage/desktop-absent" && -e "$desktop" ]]; then
            rm -f "$desktop" 2>/dev/null || recovery+=("rm -f $(q "$desktop")")
        fi
    fi
    # $dest is ours to remove only once the new tree has been renamed onto it.
    if [[ -e "$stage/previous" ]]; then
        if [[ ! -e "$new" ]]; then rm -rf "$dest" 2>/dev/null || true; fi
        # Never mv onto an existing directory: that would nest it inside.
        if [[ -e "$dest" ]] || ! mv "$stage/previous" "$dest" 2>/dev/null; then
            recovery+=("rm -rf $(q "$dest") && mv $(q "$stage/previous") $(q "$dest")")
        fi
    elif [[ -e "$stage/moving-new" && ! -e "$new" && -e "$dest" ]]; then
        rm -rf "$dest" 2>/dev/null || recovery+=("rm -rf $(q "$dest")")
    fi
    [[ "${#recovery[@]}" -eq 0 ]]
}
cleanup() {
    local status=$? step
    if [[ "$phase" == swapping ]]; then
        if rollback; then
            echo "install_gpui_preview: installation failed; the previous state was restored." >&2
        else
            # Keep the staging directory: it holds the only copy of what
            # could not be put back.
            echo "install_gpui_preview: installation failed and the previous state could NOT be fully restored." >&2
            echo "install_gpui_preview: the backup is kept in $stage" >&2
            if [[ -f "$stage/previous.desktop" ]]; then
                echo "    previous desktop entry: $stage/previous.desktop" >&2
            fi
            if [[ -e "$stage/previous" ]]; then
                echo "    previous install: $stage/previous" >&2
            fi
            echo "install_gpui_preview: to finish restoring it, run:" >&2
            for step in "${recovery[@]}"; do echo "    $step" >&2; done
            stage=""
        fi
    fi
    [[ -z "$stage" ]] || rm -rf "$stage"
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT

expected_name() { printf 'LocalFlow-Preview-%s-%s.tar.gz' "$1" "$platform"; }

if [[ -n "$archive" ]]; then
    [[ -f "$archive" ]] || die "no such archive: $archive"
    name="${archive##*/}"
    [[ "$name" =~ ^LocalFlow-Preview-([0-9]+\.[0-9]+\.[0-9]+)-([a-z0-9_]+-[a-z0-9_]+)\.tar\.gz$ ]] \
        || die "$name is not a LocalFlow Preview archive (LocalFlow-Preview-<ver>-$platform.tar.gz)."
    version="${BASH_REMATCH[1]}"
    [[ "${BASH_REMATCH[2]}" == "$platform" ]] || die "$name is for ${BASH_REMATCH[2]}, not this $platform machine."
    if [[ "$sha" =~ ^[0-9a-fA-F]{64}$ ]]; then
        expected="$(printf '%s\n' "$sha" | tr '[:upper:]' '[:lower:]')"
    else
        [[ -f "$sha" ]] || die "--sha256 must be a checksum file or a 64-hex-digit digest: $sha"
        expected="$(listed_sha256 "$sha" "$name")"
    fi
    cp "$archive" "$work/package.tar.gz"
else
    command -v curl >/dev/null 2>&1 || die "curl is needed to download the package; or pass --archive and --sha256."
    version="${release#v}"
    name="$(expected_name "$version")"
    base="${LOCALFLOW_PREVIEW_BASE_URL:-https://github.com/CortexRND/localflow/releases/download/$release}"
    echo "Downloading LocalFlow Preview $release for $platform..."
    curl -fsSL "$base/$SUMS_NAME" -o "$work/$SUMS_NAME" \
        || die "Release $release has no $SUMS_NAME (it may predate the preview package, or the download failed)."
    curl -fsSL "$base/$name" -o "$work/package.tar.gz" \
        || die "Release $release has no $name (or the download failed)."
    expected="$(listed_sha256 "$work/$SUMS_NAME" "$name")"
fi
[[ "$expected" =~ ^[0-9a-f]{64}$ ]] || die "no valid SHA-256 for $name; refusing to install an unverified package."
actual="$(sha256_of "$work/package.tar.gz")"
[[ "$actual" == "$expected" ]] || die "SHA-256 mismatch for $name: expected $expected, got $actual."
echo "Verified SHA-256 of $name."

if [[ "$os" == macos ]]; then root="$MAC_APP"; else root="$LINUX_ROOT"; fi
# Check every member before extracting anything: under the one expected root,
# no absolute/parent/dot path, control character or backslash, and only
# regular files and directories (no symlink, hard link, device or FIFO).
names="$(tar -tzf "$work/package.tar.gz")" || die "$name is not a readable gzip tar archive."
verbose="$(tar -tvzf "$work/package.tar.gz")" || die "$name is not a readable gzip tar archive."
[[ -n "$names" ]] || die "$name is empty."
[[ "$(printf '%s\n' "$names" | wc -l)" -eq "$(printf '%s\n' "$verbose" | wc -l)" ]] || die "$name has member names spanning lines."
while IFS= read -r member; do
    member="${member%/}"
    [[ "$member" == "$root" || "$member" == "$root"/* ]] || die "$name has an unexpected member: $member"
    [[ "/$member/" != */../* && "/$member/" != */./* && "$member" != *//* ]] || die "$name has an unsafe member path: $member"
    [[ ! "$member" =~ [[:cntrl:]] && "$member" != *\\* ]] || die "$name has an unsafe member name: $member"
done <<<"$names"
while IFS= read -r line; do
    case "${line:0:1}" in
        -|d) ;;
        *) die "$name has a member that is not a regular file or directory: $line" ;;
    esac
done <<<"$verbose"

mkdir -p "$parent"
if [[ "$os" == linux ]]; then
    mkdir -p "${bin_link%/*}" "${desktop%/*}"
fi
# Stage beside the destination so the swap is a rename on one file system.
stage="$(mktemp -d "$parent/.localflow-preview-install.XXXXXX")"
mkdir "$stage/x"
tar -xzf "$work/package.tar.gz" -C "$stage/x" --no-same-owner
odd="$(find "$stage/x" \( ! -type f ! -type d \) -print -o \( -type f -links +1 \) -print)"
[[ -z "$odd" ]] || die "$name unpacked links or special files: $odd"
[[ "$(ls -A "$stage/x")" == "$root" ]] || die "$name does not unpack to exactly $root."
new="$stage/x/$root"

if [[ "$os" == macos ]]; then
    plist="$new/Contents/Info.plist"
    [[ -f "$plist" ]] || die "$name has no Info.plist."
    [[ "$(plutil -extract CFBundleIdentifier raw -o - "$plist" 2>/dev/null || true)" == "$PREVIEW_ID" ]] \
        || die "$name is not the LocalFlow Preview bundle ($PREVIEW_ID)."
    [[ "$(plutil -extract CFBundleExecutable raw -o - "$plist" 2>/dev/null || true)" == lf-desktop ]] \
        || die "$name does not run lf-desktop."
    [[ "$(plutil -extract CFBundleShortVersionString raw -o - "$plist" 2>/dev/null || true)" == "$version" ]] \
        || die "$name's bundle version does not match its name."
    if plutil -extract LocalFlowRuntime raw -o - "$plist" >/dev/null 2>&1; then
        die "$name carries LocalFlowRuntime, which marks the main LocalFlow app; refusing."
    fi
    codesign --verify --deep --strict "$new" || die "$name's code signature does not verify."
    exe="$new/Contents/MacOS/lf-desktop"
else
    manifest="$new/MANIFEST.json"
    field() { sed -n "s/^  \"$1\": \"\\([^\"]*\\)\",\$/\\1/p" "$manifest"; }
    [[ -f "$manifest" ]] || die "$name has no MANIFEST.json."
    [[ "$(field name)" == "$LINUX_ROOT" && "$(field version)" == "$version" \
        && "$(field os)" == linux && "$(field arch)" == x86_64 ]] \
        || die "$name's MANIFEST.json does not describe LocalFlow Preview $version for $platform."
    listed="$(sed -n 's/^    "\([^"]*\)": "\([0-9a-f]\{64\}\)",\{0,1\}$/\1 \2/p' "$manifest" | LC_ALL=C sort)"
    present="$(cd "$new" && find . -type f ! -path ./MANIFEST.json | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r rel; do
        printf '%s %s\n' "$rel" "$(sha256_of "$rel")"
    done)"
    [[ -n "$listed" && "$listed" == "$present" ]] || die "$name's files do not match its MANIFEST.json."
    exe="$new/bin/lf-desktop"
fi
[[ -f "$exe" && -x "$exe" ]] || die "$name has no executable lf-desktop."
# Proves the executable and its bundled libraries load here (Linux: also
# that the listed system libraries are installed). Prints only the version.
if ! reported="$(env -i PATH=/usr/bin:/bin HOME="$stage" LANG=C "$exe" --version 2>"$work/version.err")"; then
    cat "$work/version.err" >&2
    die "lf-desktop from $name does not start on this machine (see above; on Linux check the system libraries in its README.txt)."
fi
[[ "$reported" == "lf-desktop $version" ]] || die "lf-desktop reports '$reported', expected 'lf-desktop $version'."

desktop_exec_arg() {
    # Desktop Entry Exec quoting, then the general string escaping (spec 1.5).
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    value="${value//\`/\\\`}"
    value="${value//\$/\\\$}"
    value="\"$value\""
    value="${value//\\/\\\\}"
    printf '%s' "${value//%/%%}"
}

# Back up and record the prior state of every path we will change, without
# changing anything yet. A failure here exits with nothing to roll back.
if [[ "$os" == linux ]]; then
    if [[ -L "$bin_link" ]]; then
        target="$(readlink "$bin_link")" || die "could not read $bin_link; nothing was changed."
        printf '%s' "$target" >"$stage/link-state.tmp" || die "could not record $bin_link; nothing was changed."
        mv "$stage/link-state.tmp" "$stage/link-saved"
    else
        : >"$stage/link-absent"
    fi
    if [[ -f "$desktop" ]]; then
        if ! cp "$desktop" "$stage/previous.desktop" || ! cmp -s "$desktop" "$stage/previous.desktop"; then
            die "could not back up $desktop; nothing was changed."
        fi
        : >"$stage/desktop-saved"
    else
        : >"$stage/desktop-absent"
    fi
fi

# Apply: everything below is undone by the EXIT trap if any step fails.
phase=swapping
if [[ -e "$dest" ]]; then mv "$dest" "$stage/previous"; fi
touch "$stage/moving-new"
mv "$new" "$dest"
if [[ "$os" == linux ]]; then
    {
        printf '[Desktop Entry]\nType=Application\nName=LocalFlow Preview\n'
        printf 'Comment=LocalFlow native desktop app (GPUI preview)\n'
        printf 'Exec=%s\n' "$(desktop_exec_arg "$dest/bin/lf-desktop")"
        printf 'Icon=%s\n' "$(printf '%s' "$dest/share/icons/localflow-preview.png" | sed 's/\\/\\\\/g')"
        printf 'Terminal=false\nCategories=Utility;Accessibility;\n%s\n' "$DESKTOP_MARKER"
    } >"$stage/localflow-preview.desktop"
    cp "$stage/localflow-preview.desktop" "$desktop.tmp-$$"
    mv -f "$desktop.tmp-$$" "$desktop"
    ln -sfn "$dest/bin/lf-desktop" "$bin_link"
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "${desktop%/*}" || true
    fi
fi
phase=installed
echo "Installed LocalFlow Preview $version at $dest"
if [[ "$os" == linux ]]; then
    case ":${PATH}:" in
        *":$HOME/.local/bin:"*) ;;
        *) echo "Warning: add $HOME/.local/bin to PATH to use the localflow-preview command." >&2 ;;
    esac
fi

if [[ "$no_launch" -eq 1 ]]; then
    echo "Skipping launch."
    exit 0
fi
# Open with no arguments: no --engine, so nothing listens until you opt in.
if [[ "$os" == macos ]]; then
    echo "Opening LocalFlow Preview..."
    open "$dest" || echo "Warning: installed, but could not open it; open LocalFlow Preview from Applications in your home folder." >&2
elif [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]]; then
    echo "Opening LocalFlow Preview..."
    nohup "$dest/bin/lf-desktop" >/dev/null 2>&1 &
else
    echo "No display found; run localflow-preview from your desktop session."
fi
