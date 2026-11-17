#!/usr/bin/env bash
set -euo pipefail

label=$1
output=$2
shift 2
mkdir -p "$(dirname "$output")"
command_name=$1
command_path=$(command -v "$command_name")
printf 'Measured executable: %s\n' "$command_path" | tee "${output%.txt}.diagnostics.txt"
if [[ "${LF_RSS_EXPECT_RELEASE:-0}" == 1 ]]; then
  case "$command_path" in
    */release/*) ;;
    *)
      printf 'Expected a release executable, got: %s\n' "$command_path" >&2
      exit 2
      ;;
  esac
  printf 'Build profile: release\n' | tee -a "${output%.txt}.diagnostics.txt"
fi
printf 'Sampling command:' | tee -a "${output%.txt}.diagnostics.txt"
printf ' %q' "$@" | tee -a "${output%.txt}.diagnostics.txt"
printf '\n' | tee -a "${output%.txt}.diagnostics.txt"

"$@" >"${output%.txt}.log" 2>&1 &
pid=$!
cleanup() {
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}
trap cleanup EXIT
sleep 10
printf '### %s Hub idle RSS (single process, KiB)\n\n' "$label" | tee "$output"
printf '| Seconds after launch | RSS (KiB) |\n| --- | --- |\n' | tee -a "$output"
for elapsed in 10 20 30; do
  if ! kill -0 "$pid" 2>/dev/null; then
    cat "${output%.txt}.log"
    echo "$label Hub exited before RSS measurement completed" >&2
    exit 1
  fi
  rss=$(ps -o rss= -p "$pid" | tr -d '[:space:]')
  [[ "$rss" =~ ^[0-9]+$ ]] && ((rss > 0))
  printf '| %s | %s |\n' "$elapsed" "$rss" | tee -a "$output"
  if [[ "${LF_RSS_REQUIRE_RUNTIME:-0}" == 1 ]]; then
    processes=$(ps -axo pid=,command= | awk '$2 ~ /(^|\/)(lf-hub|lf-rs|localflow)$/ || ($2 ~ /python/ && $0 ~ /-m localflow([. ]|$)/) {print $1}')
    if [[ "$processes" != "$pid" ]]; then
      printf 'Expected one LocalFlow process (%s), found: %s\n' "$pid" "$processes" >&2
      exit 1
    fi
    curl --noproxy '*' --connect-timeout 1 --max-time 2 --fail --silent --show-error \
      "${LF_RSS_HEALTH_URL:-http://127.0.0.1:8756}/healthz" > "${output%.txt}.healthz.json"
    grep -Eq '"ok"[[:space:]]*:[[:space:]]*true' "${output%.txt}.healthz.json"
    lsof -nP -a -p "$pid" -iTCP:"${LF_RSS_SERVER_PORT:-8756}" -sTCP:LISTEN > "${output%.txt}.server-owner.txt"
    python3 - "${LF_RSS_HEALTH_URL:-http://127.0.0.1:8756}" <<'PY' | tee -a "${output%.txt}.diagnostics.txt"
import json
import os
import sys
from pathlib import Path
from urllib.request import Request, urlopen

base = os.environ.get("XDG_CONFIG_HOME", "")
root = Path(base) if os.path.isabs(base) else Path.home() / "Library/Application Support"
token = (root / "localflow/api-token").read_text().strip()
request = Request(sys.argv[1] + "/api/status", headers={"X-Localflow-Token": token})
with urlopen(request, timeout=2) as response:
    assert json.load(response)["stt"]["loaded"] is False, "Idle runtime loaded a speech model"
print("Authenticated /api/status: speech model unloaded")
PY
    printf 'At %ss: one LocalFlow process; /healthz OK; server socket owned by Hub PID %s\n' "$elapsed" "$pid" | tee -a "${output%.txt}.diagnostics.txt"
  fi
  if [[ "$elapsed" != 30 ]]; then sleep 10; fi
done

# Keep the detailed native allocation map beside the RSS table. Both the file
# and job log matter: a single RSS number cannot explain where memory is held.
map_output="${output%.txt}.vmmap.txt"
if vmmap --summary "$pid" >"$map_output" 2>&1; then
  printf '\n### %s vmmap --summary\n\n' "$label" | tee -a "${output%.txt}.diagnostics.txt"
  cat "$map_output" | tee -a "${output%.txt}.diagnostics.txt"
else
  cat "$map_output" >&2
  map_output="${output%.txt}.footprint.txt"
  footprint "$pid" >"$map_output" 2>&1
  printf '\n### %s footprint\n\n' "$label" | tee -a "${output%.txt}.diagnostics.txt"
  cat "$map_output" | tee -a "${output%.txt}.diagnostics.txt"
fi

if [[ "${LF_RSS_REQUIRE_MENU:-0}" == 1 ]]; then
  if ! grep -F 'menu bar: enabled' "${output%.txt}.log"; then
    printf 'The measured process did not report an enabled menu bar.\n' >&2
    exit 1
  fi
  printf 'Menu bar: enabled\n' | tee -a "${output%.txt}.diagnostics.txt"
fi
cat "${output%.txt}.diagnostics.txt" >> "$output"
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  cat "$output" >> "$GITHUB_STEP_SUMMARY"
fi
