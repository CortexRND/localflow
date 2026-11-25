#!/bin/bash
# Run driver.py for every scenario x repeat into $OUT/rK/results/<scenario>.json.
#
# Settings (flag wins over env; env wins over default):
#   --out DIR      / OUT        run dir (default: ~/sim-runs/$(cat ~/sim-runs/CURRENT) if that file exists)
#   --sut DIR      / SIM_SUT    LocalFlow checkout under test (default: this repo)
#   --repeats N    / REPEATS    repeats per scenario (default 3)
#   --python BIN   / PYTHON     interpreter with the product deps (default: python3)
#   --scenarios "a b" / SCENARIOS  scenario ids (default: the seven in scenarios.json)
#   SIM_LLM_URL, SIM_LLM_MODEL are passed through to driver.py.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SIM_SUT="${SIM_SUT:-$(cd "$HERE/../.." && pwd)}"
REPEATS="${REPEATS:-3}"
PYTHON="${PYTHON:-python3}"
SCENARIOS="${SCENARIOS:-pricing-review eng-standup one-on-one incident-retro design-standup-nonotes vendor-call-micfallback launch-sync-diarized}"
OUT="${OUT:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --out) OUT="$2"; shift 2 ;;
    --sut) SIM_SUT="$2"; shift 2 ;;
    --repeats) REPEATS="$2"; shift 2 ;;
    --python) PYTHON="$2"; shift 2 ;;
    --scenarios) SCENARIOS="$2"; shift 2 ;;
    *) echo "run_matrix.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done
if [ -z "$OUT" ] && [ -f "$HOME/sim-runs/CURRENT" ]; then
  OUT="$HOME/sim-runs/$(cat "$HOME/sim-runs/CURRENT")"
fi
if [ -z "$OUT" ]; then
  echo "run_matrix.sh: set OUT or pass --out DIR" >&2; exit 2
fi
case "$OUT" in "~") OUT="${HOME:?}" ;; "~/"*) OUT="${HOME:?}/${OUT#~/}" ;; esac
case "$SIM_SUT" in "~") SIM_SUT="${HOME:?}" ;; "~/"*) SIM_SUT="${HOME:?}/${SIM_SUT#~/}" ;; esac
mkdir -p "$OUT" || exit 2
OUT="$(cd "$OUT" && pwd)" || exit 2
SIM_SUT="$(cd "$SIM_SUT" && pwd)" || exit 2
export SIM_SUT
cd "$HERE" || exit 2
for k in $(seq 1 "$REPEATS"); do for s in $SCENARIOS; do
  timeout 1800 "$PYTHON" driver.py --sut "$SIM_SUT" --scenario "$s" --out "$OUT/r$k/results/$s.json" >> "$OUT/matrix.log" 2>&1 || echo "NOT_EXERCISED r$k $s exit $?" >> "$OUT/matrix.log"
done; done; echo MATRIX_DONE >> "$OUT/matrix.log"
