#!/usr/bin/env bash
set -euo pipefail

label=$1
output=$2
shift 2
mkdir -p "$(dirname "$output")"
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
for elapsed in 10 15 20 25 30; do
  if ! kill -0 "$pid" 2>/dev/null; then
    cat "${output%.txt}.log"
    echo "$label Hub exited before RSS measurement completed" >&2
    exit 1
  fi
  rss=$(ps -o rss= -p "$pid" | tr -d '[:space:]')
  [[ "$rss" =~ ^[0-9]+$ ]] && ((rss > 0))
  printf '| %s | %s |\n' "$elapsed" "$rss" | tee -a "$output"
  if [[ "$elapsed" != 30 ]]; then sleep 5; fi
done
cat "$output" >> "$GITHUB_STEP_SUMMARY"
