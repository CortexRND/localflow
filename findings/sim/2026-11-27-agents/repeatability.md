# Repeatability - 2026-11-27-agents

SUT `acde5821b6d2` (commit invariant: ok; seen acde5821b6d2), repeats: r1, r2, r3.
A gate passes only if it passes in every repeat. `low power` marks pooled n < 10.
Suites gated: agents.

| gate | what | threshold | status | passes | r1 | r2 | r3 | spread | pooled k/n | Wilson 95% | low power |
|---|---|---|---|---|---|---|---|---|---|---|---|
| A1 | agents lifecycle & state machine | all | pass | 3/3 | 1.000 (58/58) | 1.000 (58/58) | 1.000 (58/58) | 0.000 | 174/174 | 0.978-1.000 |  |
| A2 | hot-command parsing & dictation seam | all | pass | 3/3 | 1.000 (186/186) | 1.000 (186/186) | 1.000 (186/186) | 0.000 | 558/558 | 0.993-1.000 |  |
| A3 | agents concurrency & bounds | all | pass | 3/3 | 1.000 (16/16) | 1.000 (16/16) | 1.000 (16/16) | 0.000 | 48/48 | 0.926-1.000 |  |
| A4 | agents API contract & auth | all | pass | 3/3 | 1.000 (165/165) | 1.000 (165/165) | 1.000 (165/165) | 0.000 | 495/495 | 0.992-1.000 |  |
| A5 | agents secrets & redaction | all | pass | 3/3 | 1.000 (20/20) | 1.000 (20/20) | 1.000 (20/20) | 0.000 | 60/60 | 0.940-1.000 |  |
| A6 | agent provider adapters (scripted boundary) | all | pass | 3/3 | 1.000 (106/106) | 1.000 (106/106) | 1.000 (106/106) | 0.000 | 318/318 | 0.988-1.000 |  |
| A7 | agents idle cost & efficiency | all | pass | 3/3 | 1.000 (8/8) | 1.000 (8/8) | 1.000 (8/8) | 0.000 | 24/24 | 0.862-1.000 |  |
| A8 | agents recovery & cross-process | all | pass | 3/3 | 1.000 (12/12) | 1.000 (12/12) | 1.000 (12/12) | 0.000 | 36/36 | 0.904-1.000 |  |

## Items that flipped between repeats

None.

## Not exercised

Every expected cell produced a result at the pinned commit.

- Scoped run: only the agents suite(s) declared in manifest.json are gated.
- Agents live cells L1/L2 not exercised (outside the gated matrix; never change the verdict).
