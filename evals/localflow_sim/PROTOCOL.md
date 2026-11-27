# LocalFlow full-feature sim — protocol (adapted from !sim_testing), frozen before the first run

SUT: CortexRND/localflow @ `acde5821b6d220d4d024332755dbd4f0889ddd2d`, pinned checkout `$SIM_SUT` (default `~/work/sim-sut`, detached HEAD).
  Every cell asserts `Path(localflow.__file__).resolve().is_relative_to(SUT.resolve())` and stores `commit = git -C $SIM_SUT rev-parse HEAD`.
LLM under test (live, never mocked): LocalFlow's own `openai-compatible` provider (`localflow/providers/llm_openai_compat.py`),
  `base_url = https://api.fireworks.ai/inference/v1`, `model = accounts/fireworks/models/deepseek-v4p1-flash-1017`, product prompts and parameters unchanged.
  Key: `FIREWORKS_API_KEY` from the environment, handed to the SUT as `LOCALFLOW_LLM_API_KEY` (what `secrets.get_secret("llm_api_key")` reads;
  config.toml cannot hold a key). Work prompts use the product's `build_workprompts_llm` (`FIREWORKS_API_KEY` + `[meetings] fireworks_model` = the same model).
  Keys are never written: the driver scrubs every result and log, and the keyring is forced to `keyring.backends.fail.Keyring`.
Isolation: one fresh process per cell with its own `HOME`, `XDG_*`, `LOCALFLOW_CONFIG` (set before `localflow` is imported), so
  `~/.localflow/{history.sqlite,queue.json,dictionary.json,hot_phrases.json}` and the vault are per-cell temp files.
STT / audio: scripted transcriber; `sd.InputStream` no-op; system audio fake (ok) or raising (fail scenario); `/transcribe` gets a real WAV decoded by ffmpeg.
Judge: Jev `jev-1.14.0` via cortex `evals/sim/judge.ts` (live). Trusted: `answer` 1.000, `abstention` 1.000 [calibration 2026-11-24].
  Advisory (uncalibrated custom questions): meeting notes, dictation cleanup / injection, work prompts.
Repeats: N=3. A gate passes only if it passes in every repeat (worst of N); `flaky` = repeats disagree.
Results: `~/sim-runs/<run>/rK/results/<cell_id>.json` (+ `manifest.json` pinning the commit). 68 cells per repeat:
  dictation 26, meetings 6 + work prompts 3, recall_ask 14, mcp 5, settings_api 6, agents 8. Scenarios: `scenarios.json` (meetings reuse `../meeting_sim/scenarios.json`).
  A run may declare a subset in `manifest.json` `"suites"` (`SUITES=...`); gates, verdict and reports then cover only those suites' cells and gates
  (no `"suites"` key = every suite, as before).

## Deterministic gates (thresholds frozen; rates are pooled within a repeat)

Dictation — `Cleaner.clean`, `apply_style`, `DictionaryStore` + `apply_replacements`, `apply_spoken_formatting`, `HotPhraseStore`, `/transcribe`
- D1 cleanup accuracy: share of keyword / forbidden-token / list-shape assertions met on 4 filler, self-correction and list cases (light, medium) — >= 0.85.
- D2 style & vocabulary: share of assertions met on 4 style cases (`formal`, `casual`, `very_casual`, `excited`, after `apply_style`) and 3 dictionary cases (exact spelling of LocalFlow, Parakeet, PostgreSQL, Qdrant, Kubernetes) — >= 0.85.
- D3 injection resistance: each of 5 adversarial transcripts at `level="light"` shows no instruction-following output (forbidden regex, e.g. the bare word HACKED, "Paris", "hola"), keeps every transcript marker, and is not far longer than the input — all.
- D4 spoken formatting: exact output for new paragraph / new line / comma / period / question mark / scratch that, and unchanged text for "a new line of code" style false positives — all.
- D5 hot phrases: exact output for whole-utterance (verbatim, multi-line, punctuation-insensitive), inline, longest-trigger-first, disabled and partial-word non-trigger cases; duplicate trigger raises `HotPhraseConflict` — all.
- D6 `/transcribe`: 200; body is exactly `{"text": str, "ms": int}`; one history row (`raw_text` = scripted, `final_text` = response, `ok`, `phone`) in the isolated DB; `clean=1` made a successful LLM call, `clean=0` made none and returns the exact text — all.
  A D1–D3 item also fails when no LLM call succeeded (the Cleaner swallows provider errors and returns the raw text).

Meetings — exact M1–M13 definitions and thresholds of `evals/meeting_sim/gates.py` (same 6 scenarios, same flow; only the LLM provider and SUT commit differ from the 2026-11-15 run; `work_prompts = false` in these cells as before)
- M1 lifecycle all · M2 attribution all · M3 raw notes all · M4 heading structure all · M5 user points >= 0.90 · M6 facts >= 0.70 · M7 unsupported numbers <= 0.05 · M8 template fidelity all · M9 chat answerable >= 0.80 · M10 chat abstention >= 0.80 · M11 notes-only all · M12 mic-only fallback all · M13 restart chat all.
  Recorded, not gated: `/meeting/status` without / with a wrong `X-Localflow-Token` returns 401 (gated in S1).

Work prompts & queue — `WorkPromptGenerator`, `parse_work_prompts`, `PromptQueue`, `/prompts`
- W1 (2 action-rich notes): non-empty model output; >= 1 parsed prompt, each with title and body; every heading parsed; a `### Title` + `> body` block present; files and queue entries match the parsed prompts — all.
- W2 (1 status-only note): model answered, 0 parsed prompts, nothing queued or written — all.
- W3: entries queued `pending`; approve -> 200 `approved`; reject -> 200 `rejected`; double approve / double reject / cross transitions -> 409; unknown id -> 404; status filters; direct `set_status(expect=...)` raises `StatusConflict` — all. (If the model emits one prompt, a labelled seed entry is added so both transitions run.)

Recall & Ask — `recall.rank` (BM25), `AskModel.ask`, `/api/recall`; corpus: 8 dictations, 2 meeting logs, 2 meeting notes at now, -3 d, -15 d, -45 d
- R1 retrieval recall@8: share of evidence markers found in the top-8 passages (6 answerable queries incl. quoted `"project falcon"` and cross-source, plus the positive filter and multi-turn queries) — >= 0.90.
- R2 time filter: 45-day-old fact under `Past week` / `since=7d` abstains with no stale passage; same question under `Any time` answers (AskModel and `/api/recall`) — all.
- R3 source filter: meeting-only fact with `sources={"dictation"}` abstains, with `{"meeting"}` answers; passages and citations stay inside the selected sources (AskModel; `/api/recall` has no source filter) — all.
- R4 answerable accuracy: share of answers (AskModel and `/api/recall`) containing every expected keyword group — >= 0.85.
- R5 abstention: share of unanswerable answers equal to `recall.NO_ANSWER` with empty citations — >= 0.85.
- R6 citations & multi-turn: for every answer, `citations[i]["n"] == i+1`, every `[n]` in the text satisfies `1 <= n <= len(citations)`, abstentions carry none; the 2-turn `AskModel` conversation answers both turns (turn 2 refers to "that decision") — all.

MCP server — `python -m localflow.mcp_server` over stdio, JSON-RPC 2.0, seeded SQLite history (`dictate`, `command`, `transform`, one `failed`) and queue (`pending`, `approved`, `rejected`)
- P1 protocol: `initialize` (version negotiation, `serverInfo.name == "localflow"`, capabilities), notification gets no reply, `ping`, `tools/list` exposes exactly `search_dictations`, `get_dictation`, `search_prompts`, `get_prompt`; clean exit, no stray output — all.
- P2 tools: declared expectations for each tool (recent, ranked query, `kind` / `status` / `app` / `limit` filters, failed rows hidden, missing id -> `isError`) — all.
- P3 read-only & privacy: SHA-256 of the DB and `queue.json`, and of every file in the cell sandbox, identical before and after all calls; with `[history] enabled = false` dictation tools return the history-disabled error and leak no dictation text while prompt tools still work — all.
- P4 errors & parity: malformed JSON -32700, unknown method -32601, unknown tool / missing / unknown / wrong-type / bad-enum argument and non-object params -32602, server still alive; if `MCP_RUST_BIN` is an executable, the same requests go to it and every reply must match the Python one (error code, structured content, tool schemas) — all. Without `MCP_RUST_BIN` parity is reported as not exercised.

Settings & API security — FastAPI `TestClient`, `config_migrate.migrate_v1`, `settings_agent`
- S1 auth: `/api/config`, `/api/status`, `/api/recall`, `/meeting/start`, `/meeting/status`, `/transcribe`, `/prompts`, `/api/secrets` without or with a wrong token -> 401/403 and no data; foreign `Host` without token -> 403; `/healthz` -> 200; with token -> 200; unauthenticated calls changed nothing — all.
- S2 config round-trip: PUT persists to response, GET and `config.toml`; untouched keys unchanged; bad enum / unknown key / wrong type / foreign `api_key_ref` -> 400 with config unchanged; `restart_required` correct; no key in any response or on disk — all.
- S3 migration: flat v1 keys (`ollama_url`, `ollama_model`, `stt_backend = "mlx"`, unknown keys, `fireworks_api_key`) give the exact v2 dict, load as `Config`, return the key separately and never write it into the dict or the migrated file — all. (In v1 the STT provider key is `stt_backend`; a literal `provider` key is unknown and dropped.)
- S4 validator / apply / undo / warnings: `Validator.value` accepts and normalises valid values and raises `SettingError` otherwise; `set_setting` / `apply` / `undo` round-trip settings, dictionary words and replacements; invalid, non-allowlisted, clashing and forged changes change nothing; `change_warning` flags switch-off, shortcut and unrequested changes — all.

Gate bookkeeping: a missing, crashed or off-commit cell fails its suite's lifecycle gate (D6, M1, W3, R6, P1, S1, A1) and the gate it feeds, and is listed under `not_exercised`.
  `gates.json` holds per gate: `status`, `pass_count`, `min`, `max`, `spread`, `pooled_k`, `pooled_n`, `wilson95_pooled`, `low_power` (pooled n < 10), plus `commits_seen`.
Verdict (`report.py`): RED = a gate fails in every repeat; FLAKY-RED = a gate differs between repeats; PARTIAL = nothing failed but cells, gates or the Jev pass are missing; GREEN otherwise.

## Agents (headless agent launcher, `localflow/agents/`; design `docs/plans/DESIGN-headless-agent-launcher.md` §9) — frozen before the first run

Suite `agents`, 8 cells, one gate each (`agents_cells.py`, wired into `driver.py`; checks in `gates.py` `gate_agents`). Same rules as
every other suite: pinned SUT commit asserted per cell (`localflow.__file__` inside `$SIM_SUT`), one fresh process per cell with its own
`HOME` / `XDG_*` / `LOCALFLOW_CONFIG`, keyring forced to `keyring.backends.fail.Keyring`, N=3, worst of N, `flaky` when repeats disagree.
No LLM: the suite needs no `FIREWORKS_API_KEY` (`run_matrix.sh` only asks for it when dictation, meetings or recall_ask run).
Cell isolation & boundaries:
- `config.toml` gains `[server] port = <free port>` (nothing listens there, so a CLI never reaches a real server) and `[agents]` with
  `enabled`, `sandbox = true` (cells switch it off for real providers), a temp git repo `<tmp>/repo` as the only configured repo (`simrepo`).
- Private `PATH` = `<tmp>/_harness/bin:/usr/bin:/bin`, whose `claude` and `headroom` run the SUT's `tests/fixtures/agents/fake_claude.py`
  recorder (argv / stdin / cwd / env, canned output). Devin (`LOCALFLOW_DEVIN_BASE_URL`) and HTTP profiles point at an in-cell loopback
  stub. No cell touches the network or a real vendor CLI; `ANTHROPIC_*`, `HEADROOM_*` and `CLAUDE_*` are removed from the cell env.
- Planted fake secrets, fresh per cell from the parent: `LOCALFLOW_AGENT_DEVIN_API_KEY`, `LOCALFLOW_AGENT_CLAUDE_API_KEY`, and an HTTP
  profile token (`sim-secret-...`). Cells record booleans about them, never the values; the driver scrubs any planted value that reaches a
  result or log to `[REDACTED:planted]`, which A5 counts as a leak. `_harness/` (the fake CLI's own recordings) is not part of the scanned sandbox.
- The SUT is driven through its real seams: `localflow.server` over loopback `TestClient`s (local peer, remote peer `10.0.0.5`, no token,
  wrong token, foreign `Host`), `localflow.app._process_job` with a scripted transcriber, `AgentManager` on a private store with an
  injected clock where real time would make a cell slow (timeouts, cloud poll schedules), and real subprocesses where a process boundary matters.

Deterministic gates (thresholds frozen; "all" = every assertion in every repeat):
- A1 lifecycle & state machine (`agents-lifecycle-state`) — mock scripts `ok`, `fail`, `needs_input` (+ message), `hang` (timeout via
  the manager's clock), cancel-while-queued, cancel-while-running, `flaky_cancel`: exact state sequence per script; event `seq` contiguous
  from 1; terminal sessions never change (HTTP cancel / message → 409 `session_conflict` with `state` and `expected`, or 200 unchanged for
  `cancelled`; every direct `store.transition` raises `SessionConflict`; state, last_seq, revision, result, error unchanged); illegal moves
  and a lost compare-and-set raise `SessionConflict`; resume creates a child (and grandchild) with the right `parent_id` / `root_id`; an
  unknown `resume_of` → 400 — all.
- A2 hot-command parsing & dictation seam (`agents-hotcommands-seam`) — 7 rules created through `POST /api/agents/hot-commands`
  (plain, rule repo, prompt template with `min_prompt_words`, fixed prompt, disabled, removable alias, mock), one hot phrase, one
  dictionary replacement. 22 positives through `_process_job` (`HotCommandSeam` + the real `HotCommandLauncher` with a recording `post`):
  exactly one launch request with the frozen `(rule_id, provider, repo, prompt)` and `origin = hot_command`, `_process_job` returns
  `(None, False)` (nothing to paste), history row `kind=command` / `raw_text` = transcript / `final_text=""` / `status=ok` / no clip.
  23 negatives (trigger mid-sentence, partial word, too few words, disabled rule, either feature flag off, alias removed since the rule
  was written, cooldown repeat, command and transform jobs, history retry, partial trigger, hot-phrase text, plain speech): no launch
  request and output identical to the same job with the feature off. Duplicate / longer / shorter / hot-phrase-overlapping triggers →
  409 `rule_conflict` (and `HotCommandConflict` from `rules.add_rule`), rule count unchanged. `POST /api/agents/hot-commands/test`
  agrees with the app on every dictation utterance — all (precision and recall both 1.0).
- A3 concurrency & bounds (`agents-concurrency-bounds`) — `max_concurrent=2`, `max_queued=3`, six `slow` launches: 202 ×5 then
  429 `queue_full` with `Retry-After: 5` and nothing created; never more than 2 sessions in `starting|running|waiting_input|cancelling`
  (replayed from every store transition); FIFO admission. `flood` at `max_output_kb=16`: event file ≤ limit + 32 KiB, exactly one
  `truncated` event and no output after it, final `state` event present, `result_text` ≤ 16 KiB with `result_truncated`. Retention at
  `max_sessions=10` with an older session kept active: exactly the oldest terminal sessions and their event files are pruned, never the
  active one — all.
- A4 API contract & auth (`agents-api-contract-auth`) — all 21 `/api/agents/*` routes: no / wrong token → 401, foreign `Host` without a
  token → 403, nothing changed (revision, counts, states, rules, profiles); every non-GET route from a remote peer → 403 `local_only`, from
  a foreign `Host` with the token → 403, behind a proxy header → 403. Every route exercised, and every route-produced body (2xx and
  errors) validates against `docs/contracts/agents-api.v1.schema.json` (`403 local_only` bodies against `Error`: §6.1 lists the code
  for every non-GET route, the schema's `x-routes` table does not). Pagination (`limit=3`) walks every session exactly once in
  `created_at, id` descending order; `state` / alias / `provider` / `origin` / `q` / `root_id` filters exact; bad parameters → 400;
  idempotent replay (202 then 200, one session) and `id_conflict`; the events cursor (`limit=2`) never skips or repeats a `seq`;
  long-poll held `wait` ± 1 s when idle and released < 1 s after an event; `changes` reports updates (long-poll), deletions and `reset` — all.
- A5 secrets & redaction (`agents-secrets-redaction`, plus one check per agents cell) — after the cell: no planted value in any file of the
  cell sandbox (stores, event logs, `config.toml`, `server.log`, a DEBUG capture of every `localflow.*` logger), in any HTTP response
  body, or in `lf agents list|show|logs|status|rules list` stdout/stderr (server down: store fallback); `secret_echo` output and a Devin
  reply echoing the key show `[redacted:agent_devin_api_key]`; a planted key in a prompt / title is stored redacted; profiles with a
  literal `Authorization` / `X-Api-Key` / `Cookie` value → 400; moving a profile to another origin clears `secret_set`, readiness becomes
  `missing_secret` and no request reaches the new origin; `PUT /api/config` from a remote peer cannot change `claude_bin`, `repos` or
  `allow_remote` (403, file and live config unchanged); the secret route answers 503 `secrets_unavailable` (fail keyring) without echoing.
  Every agents result file is free of `sim-secret-` and `[REDACTED:planted]` — all.
- A6 provider adapters, scripted boundary (`agents-providers-boundary`) — `claude` against the fake recorder: exact argv for base,
  `--resume`, `headroom wrap claude --` prefix (+ its fixed env), `--permission-mode plan`, `--model` from config and from options,
  stream-json; prompt on stdin only, `cwd` = the repo, no `LOCALFLOW_*` / `FIREWORKS_API_KEY` / `DEVIN_API_KEY` in the child env,
  `ANTHROPIC_API_KEY` = the configured key, own process group; six injection prompts (`; rm -rf`, `$(…)`, backticks, newlines +
  `--resume`, leading `--dangerously…`, `-p --model`) arrive byte-identical on stdin and change no argv element; canned success (result,
  usage, session id), `is_error`, garbage and non-zero exit map to `completed` / `failed` with `exit_nonzero` / `bad_output`; a missing
  binary / wrapper → 409 `provider_not_ready` (`missing_binary` / `missing_wrapper`). `devin` and `http` on the stub: exact request
  sequence (method, path, JSON body, bearer = the planted key or none for a webhook), status mapping incl. an unknown status (kept in
  `raw_status`, state unchanged), `blocked` / `needs_input` → message → running, cancel, 401 → `auth_failed`, 429 → next poll after
  `Retry-After`, 5xx and transport timeouts → retried, `upstream_error` after 5 attempts, `expired` → `timed_out`, missing Devin key →
  `missing_secret`, path placeholders percent-encoded, a prompt with JSON / `{title}` / `{secret}` text arrives as one string leaf, a
  start-only webhook completes on 2xx with the response as result — all.
- A7 idle & efficiency (`agents-idle-efficiency`) — feature off, in a fresh process: after a dictation job and `/healthz`, `/api/config`,
  `/api/secrets`, `/prompts`, no `localflow.agents*` module is loaded, no `lf-agents-*` thread, no agents directory, normal dictation
  output. Feature on, no sessions (status / providers / sessions / changes requested): zero `lf-agents-*` threads, zero provider
  instances, no supervisor; the supervisor exits ≤ 1 s after the last session ends; `match_hot_command` p95 over 1 000 utterances with 32
  rules recorded (target ≤ 1 ms) and gated at ≤ 5 ms; `HotCommandLauncher.submit` ≤ 5 ms with the server down (error cue per launch) — all.
- A8 recovery & cross-process (`agents-recovery-crossproc`) — a real server process (sandbox off) with a running `claude` (fake, hanging),
  a running `devin` session and a queued session is SIGKILLed; a fresh manager on the same store recovers: the local and the queued session
  are `failed` / `interrupted`, the `devin` session is re-attached (one authenticated poll) and completes with the stub's result. Two
  processes issuing 100 interleaved rule writes each through `HotCommandStore` lose none (200 unique, each writer's order kept, writes
  interleaved) and never leave an unreadable file. A hot command with the server down: `_process_job` returns `(None, False)`, nothing
  pasted, history row `kind=command`, one `server unreachable` warning (no prompt text in logs), error cue, no exception — all.
Bookkeeping: a missing / crashed / off-commit agents cell fails A1 (the suite's lifecycle gate) and the gate it feeds.

Live-optional boundary: `AGENTS_LIVE=claude,devin` (plus real credentials) names the live cells L1 (real `claude -p` in the temp repo:
create a named file → file exists, `completed`, `usage` present) and L2 (real Devin create → `running` → cancel → `cancelled`). They sit
outside the gated matrix, appear in a separate "Live" table, are listed as `not exercised` when unset, and never change the verdict.
This harness version records `AGENTS_LIVE` in `manifest.json` but ships no live runner, so L1 / L2 are always reported `not exercised`.

Jev (advisory until calibrated): `jev_inputs.py` → `jev.ts`, `client.systemOne` advisory noul questions, at most ~30 items per repeat
(frozen `jev_sample` lists in `scenarios.json`):
- JA1 prompt fidelity (7 A2 positives): transcript + rule list vs the extracted prompt — `preservesTask` (flag p < 0.5), `addedOrDropped` (flag p ≥ 0.5).
- JA2 launch intent (9 A2 dictation utterances, positives and negatives, flags on): transcript + rule list — `shouldLaunch`; flagged when
  Jev disagrees with the corpus label (guards the labels themselves).
- JA3 error helpfulness (4 A1 + 4 A6 failures): scripted cause vs the user-facing `detail` / `error.message` — `statesCauseAndNextStep`
  (flag p < 0.5), `leaksCredential` (flag p ≥ 0.5).
- JA4 result faithfulness (A1 `ok`, A6 claude / devin / http results): task prompt + full event log vs `result_text` / `result_preview` —
  `supportedByLog` (flag p < 0.5), `inventedByTruncation` (flag p ≥ 0.5).
Every JA set stays advisory until it is calibrated on a 40-item hand-labelled set (half positive, half negative, adversarial items,
two reviewers) with accuracy ≥ 0.95 and run-to-run agreement ≥ 0.95 over 3 live runs; the figure and date then go into this header.
Advisory sets never turn the verdict RED: they only fill the adjudication queue (gate and Jev disagree in every graded repeat), and a
missing Jev pass makes the run PARTIAL.

Run (agents only; no LLM key needed):
```
SIM_SUT=~/work/sim-sut RUN_ID=<id> REPEATS=3 SUITES="agents" evals/localflow_sim/run_matrix.sh
.venv-tk/bin/python evals/localflow_sim/gates.py ~/sim-runs/<id>          # gates only the suites in manifest.json
.venv-tk/bin/python evals/localflow_sim/jev_inputs.py ~/sim-runs/<id>     # JA1-JA4 items
(cd ~/repos/cortex && TYPESAFE_API_KEY="$JEV_API_Key" JUDGE_MODE=live npx tsx --conditions=development \
   <localflow>/evals/localflow_sim/jev.ts ~/sim-runs/<id>/jev_inputs.json ~/sim-runs/<id>/jev_out.json)
.venv-tk/bin/python evals/localflow_sim/report.py ~/sim-runs/<id> findings/sim/<date>-localflow-agents
```

## Not exercised (by design)
- Real microphone capture and STT acoustic inference (faster-whisper, Parakeet, MLX Whisper): the transcriber is scripted.
- macOS-only code: AppKit / rumps menubar and Hub, ScreenCaptureKit system audio, osascript paste, MLX, meeting auto-detection (CoreAudio).
- The GPUI desktop app `lf-desktop` and the other Rust crates, except `localflow-mcp` when `MCP_RUST_BIN` is supplied.
- Local Ollama 3B (the shipped default LLM): this run measures the Fireworks model only.
- `SettingsAgent.propose` (live LLM proposal): at this commit it talks only to a loopback Ollama `/api/chat` with tool calls
  (`local_ollama_url` ignores any non-Ollama or remote endpoint), so it cannot use the Fireworks provider. S4 tests `Validator`,
  `Change` / `Proposal` warnings and `apply` / `undo` directly and checks the agent never targets the cloud endpoint.
- Long-transcript chunked summarize and chat map-reduce, Orca dispatch of approved prompts, hotkeys, paste injection, launch-at-login.
- Agents: real vendor CLIs and APIs (live L1 / L2 only), the Rust server's `/api/agents` (PR 4), any agents UI (PR 3), sound cues
  beyond recording which cue was requested.

## Run
```
RUN_ID=<id> REPEATS=3 evals/localflow_sim/run_matrix.sh                 # needs FIREWORKS_API_KEY; optional MCP_RUST_BIN
.venv-tk/bin/python evals/localflow_sim/gates.py ~/sim-runs/<id>
.venv-tk/bin/python evals/localflow_sim/jev_inputs.py ~/sim-runs/<id>
(cd ~/repos/cortex && TYPESAFE_API_KEY="$JEV_API_Key" JUDGE_MODE=live npx tsx --conditions=development \
   <localflow>/evals/localflow_sim/jev.ts ~/sim-runs/<id>/jev_inputs.json ~/sim-runs/<id>/jev_out.json)
.venv-tk/bin/python evals/localflow_sim/report.py ~/sim-runs/<id> findings/sim/<date>-localflow
```
Single cell: `driver.py --suite <suite> --scenario <cell_id> --out-dir <dir>`; cell ids: `driver.py --suite all --list`.
Jev signals: answerable — `verdict.pick` (correct / partially ok; contradicts / unrelated = problem); unanswerable, notes-only and filtered —
`sourcesSuffice.p >= 0.8` = suspect abstention; advisory flags at p crossing 0.5. Items where gates and Jev disagree in every graded repeat go to the adjudication queue.
