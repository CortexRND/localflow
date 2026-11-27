# VERDICT: GREEN

LocalFlow full-feature sim, run `2026-11-27-agents`, SUT `acde5821b6d220d4d024332755dbd4f0889ddd2d`, LLM `openai-compatible` / `accounts/fireworks/models/deepseek-v4p1-flash-1017`, 3 repeat(s), worst-of-N.

- every gate passed in every repeat

Gates: 8 pass, 0 fail, 0 flaky, 0 not exercised. Adjudication queue: 0 trusted gate-vs-Jev, 9 advisory, 0 failing gate items.

Suites gated: agents.

## Gates

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

## Second judge

Jev `jev-1.14.0`: 84 rows, 0 errors. Trusted disagreements with the gates (every graded repeat): 0. Details in `jev.md`.

## Not exercised

Every expected cell ran at the pinned commit.
- Scoped run: only the agents suite(s) declared in manifest.json are gated.
- Agents live cells L1/L2 not exercised (outside the gated matrix; never change the verdict).
- By design (see PROTOCOL.md): real microphone / STT acoustics, macOS-only AppKit / rumps / ScreenCaptureKit / osascript / MLX, the GPUI desktop app `lf-desktop`, local Ollama 3B, and the live SettingsAgent proposal step (Ollama-only).

## Live (outside the gated matrix; never changes the verdict)

| cell | status | reason |
|---|---|---|
| L1-claude-live | not exercised | AGENTS_LIVE unset |
| L2-devin-live | not exercised | AGENTS_LIVE unset |

Files: `scorecard.json`, `repeatability.md`, `jev.md`.
