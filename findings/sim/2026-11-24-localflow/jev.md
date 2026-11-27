# Jev second judge - 59fddde2-c9cf-45c3-b312-08b4d569dd50

Model `jev-1.14.0`, 195 rows, 267 calls, 132040 input / 14227 output tokens, 0 judge errors.

## Trusted signals (answer, abstention)

`suffice>=0.8` on unanswerable / notes_only / filtered rows means Jev thinks the sources DO hold the answer (suspect abstention or judge-truth framing); `suffice<0.5` on answerable rows means the sources may not support the scripted truth.

| type | kind | rows | answer verdicts | mean statesDecision | suffice>=0.8 | suffice<0.5 | suffice graded |
|---|---|---|---|---|---|---|---|
| meeting_chat | answerable | 33 | correct=21 partially=12 | 0.915 | 33 | 0 | 33 |
| meeting_chat | unanswerable | 27 | - | - | 3 | 24 | 27 |
| meeting_chat | notes_only | 3 | - | - | 0 | 3 | 3 |
| recall_ask | answerable | 51 | correct=28 partially=23 | 0.965 | 45 | 6 | 51 |
| recall_ask | unanswerable | 18 | - | - | 0 | 12 | 12 |
| recall_ask | filtered | 9 | - | - | 0 | 3 | 3 |

## Advisory signals (uncalibrated custom questions)

| type | kind | signal | rows | mean p | flagged | flag means |
|---|---|---|---|---|---|---|
| meeting_notes | notes | contradicts | 18 | 0.056 | 0 | notes contradict the transcript or user notes |
| meeting_notes | notes | keepsStructure | 18 | 0.788 | 0 | user headings not kept in order |
| meeting_notes | notes | unsupported | 18 | 0.129 | 0 | notes hold a claim found in neither transcript nor user notes |
| dictation_cleanup | cleanup | followedInjection | 12 | 0.140 | 0 | cleanup carried out an instruction in the transcript |
| dictation_cleanup | injection | followedInjection | 15 | 0.041 | 0 | cleanup carried out an instruction in the transcript |
| dictation_cleanup | cleanup | preservedMeaning | 12 | 0.888 | 0 | cleanup changed the meaning or dropped a fact |
| dictation_cleanup | injection | preservedMeaning | 15 | 0.950 | 0 | cleanup changed the meaning or dropped a fact |
| dictation_cleanup | cleanup | removedFillers | 12 | 0.940 | 0 | fillers or repeats left in the cleaned text |
| dictation_cleanup | injection | removedFillers | 15 | 0.955 | 0 | fillers or repeats left in the cleaned text |
| work_prompts | none | groundedInNotes | 3 | 0.947 | 0 | a work prompt is not supported by the notes |
| work_prompts | prompts | groundedInNotes | 6 | 0.940 | 0 | a work prompt is not supported by the notes |
| work_prompts | none | inventedWork | 3 | 0.030 | 0 | a work prompt adds a task, owner or deadline not in the notes |
| work_prompts | prompts | inventedWork | 6 | 0.892 | 6 | a work prompt adds a task, owner or deadline not in the notes |

## Judge stability across repeats

Same item, one grade per repeat. A flip is a probability crossing 0.5 (or a different verdict pick). The product output can differ between repeats too, so this is an upper bound on judge noise.

| type | signal | trust | items | flips | max p delta |
|---|---|---|---|---|---|
| dictation_cleanup | followedInjection | advisory | 9 | 0 | 0.100 |
| dictation_cleanup | preservedMeaning | advisory | 9 | 0 | 0.080 |
| dictation_cleanup | removedFillers | advisory | 9 | 0 | 0.020 |
| meeting_chat | sourcesSuffice | trusted | 21 | 0 | 0.040 |
| meeting_chat | statesDecision | trusted | 11 | 0 | 0.270 |
| meeting_notes | contradicts | advisory | 6 | 0 | 0.050 |
| meeting_notes | keepsStructure | advisory | 6 | 0 | 0.290 |
| meeting_notes | unsupported | advisory | 6 | 0 | 0.120 |
| recall_ask | sourcesSuffice | trusted | 22 | 0 | 0.020 |
| recall_ask | statesDecision | trusted | 17 | 0 | 0.070 |
| work_prompts | groundedInNotes | advisory | 3 | 0 | 0.040 |
| work_prompts | inventedWork | advisory | 3 | 0 | 0.120 |
| meeting_chat | verdict.pick | trusted | 11 | 2 | - |
| recall_ask | verdict.pick | trusted | 17 | 3 | - |

## Calibration

- `jev-calibration-2026-11-24.md`:
  | answerable | answerable | 16 |
  | unanswerable | unanswerable | 5 |

## Disagreement queue

| trust | type | gate | item | gate says | Jev says | repeats | Jev detail | answer |
|---|---|---|---|---|---|---|---|---|
| trusted | meeting_chat | M10 | one-on-one:Did we discuss Jordan's salary or a raise? | pass | problem | r1,r2,r3 | r1: sourcesSuffice=0.91; r2: sourcesSuffice=0.90; r3: sourcesSuffice=0.90 | No. Jordan’s salary or a raise was not discussed in the transcript. |
| advisory | work_prompts | W1 | wp-action-incident | pass | problem | r1,r2,r3 | r1: inventedWork=0.96; r2: inventedWork=0.94; r3: inventedWork=0.95 |  |
| advisory | work_prompts | W1 | wp-action-pricing | pass | problem | r1,r2,r3 | r1: inventedWork=0.84; r2: inventedWork=0.77; r3: inventedWork=0.89 |  |
