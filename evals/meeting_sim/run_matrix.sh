#!/bin/bash
R=$(cat ~/sim-runs/CURRENT); cd "$(dirname "$0")"
for k in 1 2 3; do for s in pricing-review eng-standup one-on-one incident-retro design-standup-nonotes vendor-call-micfallback; do
  timeout 1800 ~/work/venv-sys/bin/python driver.py --scenario $s --out ~/sim-runs/$R/r$k/results/$s.json >> ~/sim-runs/$R/matrix.log 2>&1 || echo "NOT_EXERCISED r$k $s exit $?" >> ~/sim-runs/$R/matrix.log
done; done; echo MATRIX_DONE >> ~/sim-runs/$R/matrix.log
