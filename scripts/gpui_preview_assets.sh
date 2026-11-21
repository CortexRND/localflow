#!/bin/bash
# Stage the LocalFlow Preview release assets for a tag, failing closed:
#   LocalFlow-Preview-<ver>-linux-x86_64.tar.gz
#   LocalFlow-Preview-<ver>-macos-arm64.tar.gz
#   LocalFlow-Preview-<ver>-macos-x86_64.tar.gz
#   SHA256SUMS-gpui-preview   sha256sum format, covering exactly those three
#
# <artifacts-dir> is what actions/download-artifact writes for the
# gpui-package.yml matrix: one gpui-preview-<platform>/ directory per native
# platform, each holding only that platform's archive and its .sha256.
# <ver> is the tag without "v" and must equal rust/lf-desktop/Cargo.toml's
# version. A missing, extra, misnamed or tampered asset, a changed platform
# set or a version mismatch stops here, before anything is uploaded.
#
# This never touches SHA256SUMS (install.sh's trust file) or the other
# release assets (scripts/release-assets.sh).
#
# Usage: scripts/gpui_preview_assets.sh <tag> <artifacts-dir> <out-dir>
set -euo pipefail

PLATFORMS=(linux-x86_64 macos-arm64 macos-x86_64)
SUMS_NAME="SHA256SUMS-gpui-preview"

die() {
    echo "gpui_preview_assets: $*" >&2
    exit 1
}

if [[ $# -ne 3 || "$1" == -* ]]; then
    echo "Usage: $0 <tag> <artifacts-dir> <out-dir>" >&2
    exit 2
fi
tag="$1" artifacts="$2" out="$3"
[[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]] || die "not a release tag: $tag"
version="${BASH_REMATCH[1]}"
ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
desktop_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/rust/lf-desktop/Cargo.toml" | head -n 1)"
[[ "$desktop_version" == "$version" ]] || die "tag $tag does not match lf-desktop version '$desktop_version'"
[[ -d "$artifacts" && ! -L "$artifacts" ]] || die "no artifacts directory: $artifacts"
if [[ -e "$out" ]]; then
    [[ -d "$out" && -z "$(ls -A "$out")" ]] || die "output directory must be new or empty: $out"
fi

# Entry names directly inside directory $1, sorted (a name containing a
# newline cannot equal an expected name, so it fails closed).
entries() {
    find "$1" -mindepth 1 -maxdepth 1 | sed 's|.*/||' | LC_ALL=C sort
}

sha256_line() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}
digest_of() {
    local digest
    read -r digest _ < <(sha256_line "$1")
    printf '%s\n' "$digest" | tr '[:upper:]' '[:lower:]'
}

# Exactly one artifact directory per supported platform, and nothing else.
expected_dirs="$(printf 'gpui-preview-%s\n' "${PLATFORMS[@]}" | LC_ALL=C sort)"
found_dirs="$(entries "$artifacts")"
[[ "$found_dirs" == "$expected_dirs" ]] || die "artifacts are not exactly one per platform (${PLATFORMS[*]}); found: $(echo "$found_dirs" | tr '\n' ' ')"

mkdir -p "$out"
names=()
for platform in "${PLATFORMS[@]}"; do
    dir="$artifacts/gpui-preview-$platform"
    name="LocalFlow-Preview-$version-$platform.tar.gz"
    [[ -d "$dir" && ! -L "$dir" ]] || die "$dir is not a directory"
    [[ "$(entries "$dir")" == "$(printf '%s\n' "$name" "$name.sha256" | LC_ALL=C sort)" ]] \
        || die "$dir must hold only $name and $name.sha256; found: $(entries "$dir" | tr '\n' ' ')"
    for file in "$dir/$name" "$dir/$name.sha256"; do
        [[ -f "$file" && ! -L "$file" ]] || die "$file is not a regular file"
    done
    # The build leg's own checksum must name this asset and match it.
    [[ "$(wc -l <"$dir/$name.sha256")" -eq 1 ]] || die "$name.sha256 must have exactly one line"
    read -r listed listed_name <"$dir/$name.sha256"
    listed_name="${listed_name#\*}"
    [[ "$listed" =~ ^[0-9a-fA-F]{64}$ && "$listed_name" == "$name" ]] || die "$name.sha256 does not list $name"
    [[ "$(printf '%s\n' "$listed" | tr '[:upper:]' '[:lower:]')" == "$(digest_of "$dir/$name")" ]] \
        || die "$name does not match its build checksum"
    # The archive holds the layout its platform name promises.
    if [[ "$platform" == linux-* ]]; then root="localflow-preview"; else root="LocalFlow Preview.app"; fi
    members="$(tar -tzf "$dir/$name")" || die "$name is not a gzip tar archive"
    while IFS= read -r member; do
        member="${member%/}"
        [[ "$member" == "$root" || "$member" == "$root"/* ]] || die "$name is not a $platform package (member $member)"
    done <<<"$members"
    cp "$dir/$name" "$out/$name"
    names+=("$name")
done

(cd "$out" && sha256_line "${names[@]}" >"$SUMS_NAME")
# Fail closed: every listed file must exist and match, and every staged asset
# must be listed under the exact (space-free) name it uploads as.
(cd "$out" && sha256_line -c "$SUMS_NAME" >/dev/null) || die "$SUMS_NAME does not verify"
count=0
for asset in "$out"/*; do
    asset_name="${asset##*/}"
    [[ "$asset_name" == "$SUMS_NAME" ]] && continue
    if [[ "$asset_name" == *" "* ]] || ! awk -v n="$asset_name" '$2 == n { found = 1 } END { exit !found }' "$out/$SUMS_NAME"; then
        die "release asset not checksummed under its upload name: $asset_name"
    fi
    count=$((count + 1))
done
[[ "$count" -eq "${#PLATFORMS[@]}" && "$(wc -l <"$out/$SUMS_NAME")" -eq "${#PLATFORMS[@]}" ]] \
    || die "expected exactly ${#PLATFORMS[@]} preview assets"
echo "Staged LocalFlow Preview $tag assets in $out"
