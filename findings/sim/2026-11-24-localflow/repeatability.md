# Repeatability - 59fddde2-c9cf-45c3-b312-08b4d569dd50

SUT `3ee9ce30eaea` (commit invariant: ok; seen 3ee9ce30eaea), repeats: r1, r2, r3.
A gate passes only if it passes in every repeat. `low power` marks pooled n < 10.

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
| M7 | unsupported numbers (max) | <= 0.05 | pass | 3/3 | 0.000 (0/16) | 0.000 (0/16) | 0.000 (0/16) | 0.000 | 0/48 | 0.000-0.074 |  |
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

## Items that flipped between repeats

| gate | item | outcome per repeat |
|---|---|---|
| M10 | pricing-review:What was the annual contract discount we agreed? | r1=FAIL r2=ok r3=FAIL |
| M10 | incident-retro:Which cloud region was affected? | r1=ok r2=ok r3=FAIL |

## Not exercised

Every expected cell produced a result at the pinned commit.

