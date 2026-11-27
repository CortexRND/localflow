# Jev second judge - 2026-11-27-agents

Model `jev-1.14.0`, 84 rows, 84 calls, 45355 input / 3225 output tokens, 0 judge errors.

## Trusted signals (answer, abstention)

`suffice>=0.8` on unanswerable / notes_only / filtered rows means Jev thinks the sources DO hold the answer (suspect abstention or judge-truth framing); `suffice<0.5` on answerable rows means the sources may not support the scripted truth.

No rows.

## Advisory signals (uncalibrated custom questions)

| type | kind | signal | rows | mean p | flagged | flag means |
|---|---|---|---|---|---|---|
| agent_prompt_fidelity | positive | addedOrDropped | 21 | 0.201 | 0 | launch prompt adds or drops part of the task |
| agent_prompt_fidelity | positive | preservesTask | 21 | 0.906 | 0 | extracted launch prompt does not state the spoken task |
| agent_launch_intent | expect_launch | shouldLaunch | 9 | 0.832 | 0 | Jev disagrees with the corpus label (adjudicate the label) |
| agent_launch_intent | expect_no_launch | shouldLaunch | 18 | 0.346 | 8 | Jev disagrees with the corpus label (adjudicate the label) |
| agent_error_helpfulness | error | leaksCredential | 24 | 0.047 | 0 | error message appears to reveal a credential |
| agent_error_helpfulness | error | statesCauseAndNextStep | 24 | 0.268 | 18 | error message misses the cause or the next step |
| agent_result_faithfulness | result | inventedByTruncation | 12 | 0.039 | 0 | result preview says something the log does not, by truncation |
| agent_result_faithfulness | result | supportedByLog | 12 | 0.707 | 4 | agent result not supported by the event log |

## Judge stability across repeats

Same item, one grade per repeat. A flip is a probability crossing 0.5 (or a different verdict pick). The product output can differ between repeats too, so this is an upper bound on judge noise.

| type | signal | trust | items | flips | max p delta |
|---|---|---|---|---|---|
| agent_error_helpfulness | leaksCredential | advisory | 8 | 0 | 0.030 |
| agent_error_helpfulness | statesCauseAndNextStep | advisory | 8 | 0 | 0.060 |
| agent_launch_intent | shouldLaunch | advisory | 9 | 1 | 0.090 |
| agent_prompt_fidelity | addedOrDropped | advisory | 7 | 0 | 0.070 |
| agent_prompt_fidelity | preservesTask | advisory | 7 | 0 | 0.040 |
| agent_result_faithfulness | inventedByTruncation | advisory | 4 | 0 | 0.010 |
| agent_result_faithfulness | supportedByLog | advisory | 4 | 1 | 0.160 |

## Calibration

No `calibration/` directory in the run: trust levels are the protocol defaults (answer 1.000, abstention 1.000; custom questions and agents JA1-JA4 uncalibrated, advisory).

## Disagreement queue

| trust | type | gate | item | gate says | Jev says | repeats | Jev detail | answer |
|---|---|---|---|---|---|---|---|---|
| advisory | agent_error_helpfulness | A1 | agents-lifecycle-state:fail | pass | problem | r1,r2,r3 | r1: statesCauseAndNextStep=0.06; r2: statesCauseAndNextStep=0.05; r3: statesCauseAndNextStep=0.06 | mock run failed |
| advisory | agent_error_helpfulness | A1 | agents-lifecycle-state:flaky_cancel | pass | problem | r1,r2,r3 | r1: statesCauseAndNextStep=0.05; r2: statesCauseAndNextStep=0.04; r3: statesCauseAndNextStep=0.04 | the mock refused to stop |
| advisory | agent_error_helpfulness | A1 | agents-lifecycle-state:hang | pass | problem | r1,r2,r3 | r1: statesCauseAndNextStep=0.06; r2: statesCauseAndNextStep=0.07; r3: statesCauseAndNextStep=0.07 | no result within 10 s |
| advisory | agent_error_helpfulness | A1 | agents-lifecycle-state:terminal:ok | pass | problem | r1,r2,r3 | r1: statesCauseAndNextStep=0.37; r2: statesCauseAndNextStep=0.35; r3: statesCauseAndNextStep=0.36 | cannot change session 2f1115aee341 in state 'completed'; expected one of queued, starting, running, waiting_input |
| advisory | agent_error_helpfulness | A6 | agents-providers-boundary:claude:mode_garbage | pass | problem | r1,r2,r3 | r1: statesCauseAndNextStep=0.04; r2: statesCauseAndNextStep=0.04; r3: statesCauseAndNextStep=0.04 | unreadable claude output: oops: unexpected token |
| advisory | agent_error_helpfulness | A6 | agents-providers-boundary:devin:auth_failed | pass | problem | r1,r2,r3 | r1: statesCauseAndNextStep=0.05; r2: statesCauseAndNextStep=0.05; r3: statesCauseAndNextStep=0.05 | the service rejected the credentials (HTTP 401) |
| advisory | agent_launch_intent | A2 | agents-hotcommands-seam:n01 | pass | problem | r1,r2,r3 | r1: shouldLaunch=0.80 launched=False; r2: shouldLaunch=0.79 launched=False; r3: shouldLaunch=0.83 launched=False | please launch claude to fix the flaky test |
| advisory | agent_launch_intent | A2 | agents-hotcommands-seam:n17 | pass | problem | r1,r2,r3 | r1: shouldLaunch=0.61 launched=False; r2: shouldLaunch=0.70 launched=False; r3: shouldLaunch=0.62 launched=False | claude launch to fix the flaky test |
| advisory | agent_result_faithfulness | A1 | agents-lifecycle-state:ok | pass | problem | r1,r2,r3 | r1: supportedByLog=0.40; r2: supportedByLog=0.31; r3: supportedByLog=0.40 | done: Summarise the README of the sim repo in one sentence |
