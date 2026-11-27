#!/bin/bash
# Build the release assets for a tag, deterministically, into dist/release/:
#   localflow-<ver>.tar.gz  git archive | gzip -n (no name or timestamp)
#   localflow-<ver>.zip     git archive --format=zip
#   SHA256SUMS              sha256sum format, covering both archives
# <ver> is the tag without its leading "v" and must match pyproject.toml at
# that tag. Both archives unpack into localflow-<ver>/.
#
# Usage: scripts/release-assets.sh <tag>        e.g. scripts/release-assets.sh v0.2.4
# Then:  gh release create <tag> dist/release/*
set -euo pipefail

if [[ $# -ne 1 || -z "$1" || "$1" == -* ]]; then
    echo "Usage: $0 <tag>" >&2
    exit 2
fi

tag="$1"
version="${tag#v}"
root="$(git rev-parse --show-toplevel)"
cd "$root"

if ! git rev-parse --verify --quiet "refs/tags/$tag^{commit}" >/dev/null; then
    echo "Tag not found: $tag" >&2
    exit 1
fi

tag_version="$(git show "refs/tags/$tag:pyproject.toml" \
    | sed -n 's/^version *= *"\([^"]*\)".*/\1/p' | head -n 1)"
if [[ "$tag_version" != "$version" ]]; then
    echo "Tag $tag does not match pyproject.toml version '$tag_version' at that tag." >&2
    exit 1
fi

out="dist/release"
name="localflow-$version"
mkdir -p "$out"
rm -f "$out/$name.tar.gz" "$out/$name.zip" "$out/SHA256SUMS"

# Pin settings that would otherwise make the archive bytes depend on the
# machine building them.
archive=(git -c core.autocrlf=false -c tar.umask=0022 archive --prefix="$name/")
"${archive[@]}" --format=tar "refs/tags/$tag" | gzip -n -9 >"$out/$name.tar.gz"
"${archive[@]}" --format=zip -o "$out/$name.zip" "refs/tags/$tag"

if command -v sha256sum >/dev/null 2>&1; then
    sha256=(sha256sum)
else
    sha256=(shasum -a 256)
fi
(cd "$out" && "${sha256[@]}" "$name.tar.gz" "$name.zip" >SHA256SUMS)

echo "Release assets for $tag in $out:"
cat "$out/SHA256SUMS"
