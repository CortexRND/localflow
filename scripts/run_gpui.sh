#!/usr/bin/env bash
# Build and launch the native GPUI desktop app (rust/lf-desktop).
#
#   scripts/run_gpui.sh --sandbox            # example data, fake services
#   scripts/run_gpui.sh --page history       # your real LocalFlow data
#   LF_DESKTOP_RELEASE=1 scripts/run_gpui.sh # optimised build
#
# All arguments are passed to lf-desktop (see `--help`). This does not change
# how LocalFlow is installed or which app `lf hub` / `lf-hub` open.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="$root/rust/lf-desktop/Cargo.toml"
profile=debug
build_args=()
if [[ "${LF_DESKTOP_RELEASE:-}" == "1" ]]; then
  profile=release
  build_args+=(--release)
fi

if [[ "$(uname -s)" == "Linux" && -z "${DISPLAY:-}" && -z "${WAYLAND_DISPLAY:-}" ]]; then
  echo "run_gpui.sh: no DISPLAY or WAYLAND_DISPLAY; lf-desktop needs a graphical session." >&2
  exit 1
fi

cargo build --manifest-path "$manifest" "${build_args[@]}"
exec "$root/rust/lf-desktop/target/$profile/lf-desktop" "$@"
