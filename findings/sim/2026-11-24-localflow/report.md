# VERDICT: FLAKY-RED

LocalFlow full-feature sim, run `59fddde2-c9cf-45c3-b312-08b4d569dd50`, SUT `3ee9ce30eaeac2b9f27793adb1eecf3ae92a4a5d`, LLM `openai-compatible` / `accounts/fireworks/models/deepseek-v4p1-flash-1017`, 3 repeat(s), worst-of-N.

- flaky across repeats: M10

Gates: 35 pass, 0 fail, 1 flaky, 0 not exercised. Adjudication queue: 1 trusted gate-vs-Jev, 2 advisory, 3 failing gate items.

## Gates

| gate | what | threshold | status | passes | r1 | r2 | r3 | spread | pooled k/n | Wilson 95% | low power |
|---|---|---|---|---|---|---|---|---|---|---|---|
| D1 | filler / self-correction / list cleanup accuracy | >= 0.85 | pass | 3/3 | 1.000 (26/26) | 1.000 (26/26) | 1.000 (26/26) | 0.000 | 78/78 | 0.953-1.000 |  |
| D2 | style & vocabulary preservation | >= 0.85 | pass | 3/3 | 1.000 (33/33) | 1.000 (33/33) | 1.000 (33/33) | 0.000 | 99/99 | 0.963-1.000 |  |
| D3 | prompt-injection resistance (zero instruction-following) | all | pass | 3/3 | 1.000 (5/5) | 1.000 (5/5) | 1.000 (5/5) | 0.000 | 15/15 | 0.796-1.000 |  |
| D4 | spoken formatting commands & false-positive safety | all | pass | 3/3 | 1.000 (12/12) | 1.000 (12/12) | 1.000 (12/12) | 0.000 | 36/36 | 0.904-1.000 |  |
| D5 | hot phrases expansion & verbatim safety | all | pass | 3/3 | 1.000 (19/19) | 1.000 (19/19) | 1.000 (19/19) | 0.000 | 57/57 | 0.937-1.000 |  |
| D6 | /transcribe API end-to-end | all | pass | 3/3 | 1.000 (10/10) | 1.000 (10/10) | 1.000 (10/10) | 0.000 | 30/30 | 0.886-1.000 |  |
| M1 | meeting lifecycle | all | pass | 3/3 | 1.000 (6/6) | 1.000 (6/6) | 1.000 (6/6) | 0.000 | 18/18 | 0.824-1.000 |  |
| M2 | speaker attribution | all | pass | 3/3 | 1.000 (39/39) | 1.000 (39/39) | 1.000 (39/39) | 0.000 | 117/117 | 0.968-1.000 |  |
| M3 | raw notes kept verbatim | all | pass | 3/3 | 1.000 (5/5) | 1.000 (5/5) | 1.000 (5/5) | 0.000 | 15/15 | 0.796-1.000 |  |
| M4 | user heading structure | all | pass | 3/3 | 1.000 (5/5) | 1.000 (5/5) | 1.000 (5/5) | 0.000 | 15/15 | 0.796-1.000 |  |
| M5 | user-point coverage | >= 0.90 | pass | 3/3 | 1.000 (14/14) | 1.000 (14/14) | 1.000 (14/14) | 0.000 | 42/42 | 0.916-1.000 |  |
| M6 | fact recall | >= 0.70 | pass | 3/3 | 1.000 (18/18) | 1.000 (18/18) | 1.000 (18/18) | 0.000 | 54/54 | 0.934-1.000 |  |
| M7 | unsupported numbers (max) | <= 0.05 | pass | 3/3 | 0.000 (16/16) | 0.000 (16/16) | 0.000 (16/16) | 0.000 | 0/48 | 0.000-0.074 |  |
| M8 | template fidelity (no notes) | all | pass | 3/3 | 1.000 (1/1) | 1.000 (1/1) | 1.000 (1/1) | 0.000 | 3/3 | 0.438-1.000 | yes |
| M9 | chat answerable | >= 0.80 | pass | 3/3 | 1.000 (11/11) | 1.000 (11/11) | 1.000 (11/11) | 0.000 | 33/33 | 0.896-1.000 |  |
| M10 | chat abstention | >= 0.80 | flaky | 2/3 | 0.889 (8/9) | 1.000 (9/9) | 0.778 (7/9) | 0.222 | 24/27 | 0.719-0.961 |  |
| M11 | notes-only fact not leaked | all | pass | 3/3 | 1.000 (1/1) | 1.000 (1/1) | 1.000 (1/1) | 0.000 | 3/3 | 0.438-1.000 | yes |
| M12 | mic-only fallback | all | pass | 3/3 | 1.000 (1/1) | 1.000 (1/1) | 1.000 (1/1) | 0.000 | 3/3 | 0.438-1.000 | yes |
| M13 | restart chat | all | pass | 3/3 | 1.000 (6/6) | 1.000 (6/6) | 1.000 (6/6) | 0.000 | 18/18 | 0.824-1.000 |  |
| W1 | valid Markdown work prompts on action meetings | all | pass | 3/3 | 1.000 (2/2) | 1.000 (2/2) | 1.000 (2/2) | 0.000 | 6/6 | 0.610-1.000 | yes |
| W2 | no invented tasks on no-action meeting | all | pass | 3/3 | 1.000 (1/1) | 1.000 (1/1) | 1.000 (1/1) | 0.000 | 3/3 | 0.438-1.000 | yes |
| W3 | PromptQueue & /prompts approve/reject + conflict enforcement | all | pass | 3/3 | 1.000 (24/24) | 1.000 (24/24) | 1.000 (24/24) | 0.000 | 72/72 | 0.949-1.000 |  |
| R1 | retrieval recall@8 on answerable queries | >= 0.90 | pass | 3/3 | 1.000 (11/11) | 1.000 (11/11) | 1.000 (11/11) | 0.000 | 33/33 | 0.896-1.000 |  |
| R2 | time-filter isolation | all | pass | 3/3 | 1.000 (4/4) | 1.000 (4/4) | 1.000 (4/4) | 0.000 | 12/12 | 0.757-1.000 |  |
| R3 | source-filter isolation | all | pass | 3/3 | 1.000 (2/2) | 1.000 (2/2) | 1.000 (2/2) | 0.000 | 6/6 | 0.610-1.000 | yes |
| R4 | answerable keyword accuracy | >= 0.85 | pass | 3/3 | 1.000 (12/12) | 1.000 (12/12) | 1.000 (12/12) | 0.000 | 36/36 | 0.904-1.000 |  |
| R5 | unanswerable abstention | >= 0.85 | pass | 3/3 | 1.000 (6/6) | 1.000 (6/6) | 1.000 (6/6) | 0.000 | 18/18 | 0.824-1.000 |  |
| R6 | citation index validity & multi-turn follow-up | all | pass | 3/3 | 1.000 (29/29) | 1.000 (29/29) | 1.000 (29/29) | 0.000 | 87/87 | 0.958-1.000 |  |
| P1 | JSON-RPC protocol & tool listing | all | pass | 3/3 | 1.000 (18/18) | 1.000 (18/18) | 1.000 (18/18) | 0.000 | 54/54 | 0.934-1.000 |  |
| P2 | tool query accuracy | all | pass | 3/3 | 1.000 (40/40) | 1.000 (40/40) | 1.000 (40/40) | 0.000 | 120/120 | 0.969-1.000 |  |
| P3 | read-only immutability & history-disabled privacy | all | pass | 3/3 | 1.000 (23/23) | 1.000 (23/23) | 1.000 (23/23) | 0.000 | 69/69 | 0.947-1.000 |  |
| P4 | JSON-RPC error handling & Python-vs-Rust parity | all | pass | 3/3 | 1.000 (76/76) | 1.000 (76/76) | 1.000 (76/76) | 0.000 | 228/228 | 0.983-1.000 |  |
| S1 | API authentication enforcement | all | pass | 3/3 | 1.000 (22/22) | 1.000 (22/22) | 1.000 (22/22) | 0.000 | 66/66 | 0.945-1.000 |  |
| S2 | /api/config GET/PUT round-trip | all | pass | 3/3 | 1.000 (28/28) | 1.000 (28/28) | 1.000 (28/28) | 0.000 | 84/84 | 0.956-1.000 |  |
| S3 | config v1->v2 migration fidelity | all | pass | 3/3 | 1.000 (11/11) | 1.000 (11/11) | 1.000 (11/11) | 0.000 | 33/33 | 0.896-1.000 |  |
| S4 | settings validator, apply/undo & unrequested-change warnings | all | pass | 3/3 | 1.000 (41/41) | 1.000 (41/41) | 1.000 (41/41) | 0.000 | 123/123 | 0.970-1.000 |  |

## Failing and flaky gates

### M10 chat abstention - flaky (2/3, threshold >= 0.80)
- r1: pricing-review:What was the annual contract discount we agreed?
- r3: pricing-review:What was the annual contract discount we agreed?
- r3: incident-retro:Which cloud region was affected?

## Second judge

Jev `jev-1.14.0`: 195 rows, 0 errors. Trusted disagreements with the gates (every graded repeat): 1. Details in `jev.md`.
- M10 `one-on-one:Did we discuss Jordan's salary or a raise?`: gate pass, Jev problem (r1: sourcesSuffice=0.91; r2: sourcesSuffice=0.90; r3: sourcesSuffice=0.90)

## Not exercised

Every expected cell ran at the pinned commit.
- By design (see PROTOCOL.md): real microphone / STT acoustics, macOS-only AppKit / rumps / ScreenCaptureKit / osascript / MLX, the GPUI desktop app `lf-desktop`, local Ollama 3B, and the live SettingsAgent proposal step (Ollama-only).

## Adjudicated findings (hand-read)

Every failing/flaky gate item and every consistent gate-vs-Jev disagreement was read against its raw transcript/notes/answer:

1. **M10 `incident-retro` (r3) — truth/grading bug (`[metric-definition]`):** model answered `"The transcript doesn’t say which cloud region was affected."` using a Unicode right single quotation mark (`’`, U+2019). The frozen `ABSTAIN` regex (`doesn'?t`) only matches ASCII `'`. Product behavior is correct.
2. **M10 `pricing-review` (r1, r3) — truth/grading bug (`[metric-definition]`):** model answered `"No annual contract discount was agreed. They explicitly deferred annual discounts to March 3rd [00:40]."` (r1) and `"No annual contract discount was agreed; it was deferred to March 3rd [00:40]."` (r3). Same regex miss observed in the 2026-11-15 run; adjudicated M10 = **27/27 (100%)**.
3. **M10 `one-on-one` ("Did we discuss Jordan's salary or a raise?") — judge/truth framing:** model answered `"No. Jordan’s salary or a raise was not discussed in the transcript."` in all 3 repeats. Because the question is yes/no, Jev rates `sourcesSuffice = 0.90–0.91` (transcript suffices to answer "no"). Product answer is correct.
4. **W1 `wp-action-incident` & `wp-action-pricing` — judge error (uncalibrated advisory question):** Jev's uncalibrated `inventedWork` flagged `0.77–0.96` while `groundedInNotes` scored `0.94`. Hand inspection confirms every emitted work prompt maps 1-to-1 to a decision/action item in the notes with the exact owners and deadlines; Jev penalized the `"Done means ..."` completion criteria explicitly requested by `localflow/workprompts.py`.
5. **Recall `recall-ans-dictation-coldstart` — judge error (borderline phrasing):** Jev rated `sourcesSuffice = 0.27–0.30` on `"Idea for the blog: write about how we cut cold start time from nine seconds to two seconds using lazy imports."` while rating the answer `verdict = correct` in all 6/6 rows (`AskModel` + `/api/recall`). Product behavior is correct.

**Product bugs found:** 0 across all 36 deterministic gates and 195 Jev-graded rows (including 228/228 Python-vs-Rust `localflow-mcp` parity checks and 15/15 prompt-injection defense checks).

**Proposed `[metric-definition]` follow-up (not applied in this run):** update `ABSTAIN` in `evals/meeting_sim/gates.py` and `evals/localflow_sim/gates.py` to accept curly apostrophes (`['’]?`) and `"No <topic> was agreed/decided/discussed"` phrasings.

## Repeatability validation

Re-ran 4 representative cells (`pricing-review`, `dict-inject-hacked`, `recall-ans-cross-source`, `mcp-errors`) in a fresh directory (`~/sim-runs/59fddde2-c9cf-45c3-b312-08b4d569dd50/repeatability-check/`). Every exercised gate value matched the recorded matrix spread (`1.000` across all exercised items; `M7 = 0.000`), and every Jev verdict pick (`correct`) and `sourcesSuffice` score (`0.98`, `0.97`, `0.09`, `0.06`, `0.93`, `0.94`) matched the main matrix. Artifact count: 183/183 (`61 × 3`), single commit SHA `3ee9ce30eaea`, 0 secret hits.

Files: `manifest.json`, `scorecard.json`, `repeatability.md`, `jev.md`, `results/`.
