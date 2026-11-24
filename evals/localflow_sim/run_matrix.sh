#!/bin/bash
# Full matrix: REPEATS x every cell of every suite, one fresh process per cell, each under `timeout`.
# Results: ~/sim-runs/$RUN_ID/rK/results/<cell>.json   (see PROTOCOL.md; SIM_RUNS overrides ~/sim-runs)
#   RUN_ID=<id> REPEATS=3 SUITES="dictation meetings recall_ask mcp settings_api" evals/localflow_sim/run_matrix.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
PY="${SIM_PYTHON:-$REPO/.venv-tk/bin/python}"
export SIM_SUT="${SIM_SUT:-$HOME/work/sim-sut}"
REPEATS="${REPEATS:-3}"
SUITES="${SUITES:-dictation meetings recall_ask mcp settings_api}"
CELL_TIMEOUT="${CELL_TIMEOUT:-900}"
MEETING_TIMEOUT="${MEETING_TIMEOUT:-1800}"
RUN_ID="${RUN_ID:-$(date +%Y%m%d-%H%M%S)-localflow-sim}"
RUN="${SIM_RUNS:-$HOME/sim-runs}/$RUN_ID"
LOG="$RUN/matrix.log"

if [ -z "${FIREWORKS_API_KEY:-}" ]; then
  echo "FIREWORKS_API_KEY is not set (the live LLM is never mocked)" >&2
  exit 2
fi
mkdir -p "$RUN"
for old in "$RUN"/r*; do
  [ -d "$old" ] || continue
  n="${old##*/r}"
  case "$n" in *[!0-9]*|"") continue ;; esac
  [ "$n" -gt "$REPEATS" ] && rm -rf "$old"
done
COMMIT="$(git -C "$SIM_SUT" rev-parse HEAD)" || exit 2
SPEC_COMMIT="$("$PY" -c "import json, sys; print(json.load(open(sys.argv[1]))['sut']['commit'])" "$HERE/scenarios.json")" || exit 2
if [ "$COMMIT" != "$SPEC_COMMIT" ] && [ "${SIM_ALLOW_COMMIT_OVERRIDE:-0}" != "1" ]; then
  echo "SIM_SUT HEAD ($COMMIT) != frozen scenarios.json pin ($SPEC_COMMIT); set SIM_ALLOW_COMMIT_OVERRIDE=1 to override" >&2
  exit 2
fi
HARNESS="$(git -C "$REPO" rev-parse HEAD 2>/dev/null || echo unknown)"
HARNESS_DIRTY="$(git -C "$REPO" status --porcelain -- evals/localflow_sim 2>/dev/null | head -n 1)"

# The manifest pins the SUT commit for gates.py; it holds no secrets.
RUN_ID="$RUN_ID" COMMIT="$COMMIT" HARNESS="$HARNESS" HARNESS_DIRTY="$HARNESS_DIRTY" REPEATS="$REPEATS" SUITES="$SUITES" \
MCP_RUST="${MCP_RUST_BIN:+yes}" "$PY" - "$RUN/manifest.json" "$HERE/scenarios.json" <<'PY'
import json, os, sys, time
spec = json.load(open(sys.argv[2]))
json.dump({
    "run_id": os.environ["RUN_ID"], "commit": os.environ["COMMIT"], "sim_sut": os.environ["SIM_SUT"],
    "harness_commit": os.environ["HARNESS"], "harness_dirty": bool(os.environ.get("HARNESS_DIRTY")),
    "repeats": int(os.environ["REPEATS"]),
    "suites": os.environ["SUITES"].split(), "llm": spec["llm"], "spec_version": spec["version"],
    "mcp_rust_bin": bool(os.environ.get("MCP_RUST")), "started": time.strftime("%Y-%m-%dT%H:%M:%S"),
}, open(sys.argv[1], "w"), indent=1)
PY

echo "MATRIX_START $(date +%FT%T) run=$RUN_ID commit=$COMMIT repeats=$REPEATS" >> "$LOG"
for k in $(seq 1 "$REPEATS"); do
  for suite in $SUITES; do
    limit="$CELL_TIMEOUT"
    [ "$suite" = meetings ] && limit="$MEETING_TIMEOUT"
    for cell in $("$PY" "$HERE/driver.py" --suite "$suite" --list); do
      # Outer timeout is the backstop; the driver enforces the same limit on the cell process.
      timeout "$((limit + 60))" "$PY" "$HERE/driver.py" --suite "$suite" --scenario "$cell" \
        --timeout "$limit" --out-dir "$RUN/r$k/results" >> "$LOG" 2>&1 \
        || echo "NOT_EXERCISED r$k $suite $cell exit $?" >> "$LOG"
    done
  done
done
echo "MATRIX_DONE $(date +%FT%T)" >> "$LOG"
echo "$RUN"
