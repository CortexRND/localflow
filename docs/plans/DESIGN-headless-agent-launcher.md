# LocalFlow headless agent launcher — architecture & UI interface contract

Status: design only, nothing built. Written against `CortexRND/localflow` main at `c0b12f3`.
Audience: the Bot implementing PR 1/PR 2, and the UX/UI Lead Bot (reads §6 only — it is self-contained).
Contract version: `agents-api v1`. Anything marked **VERIFY** must be checked against the vendor's current docs/`--help` during implementation; the design isolates each such item in one table or constant.

## 1. Goals & non-goals

**Goals**
1. Launch and manage headless agent sessions (`claude`, `devin`, generic `http`, `mock`, plus entry-point providers) from three front doors: a spoken hot command, `lf agents …`, and `/api/agents/*`.
2. One provider-neutral session model, state machine, event log and error vocabulary, so a UI never branches on provider id — only on `capabilities`.
3. Zero cost when off or idle: no import of `localflow.agents` in the dictation app, no threads, no provider objects, no timers, no polling.
4. Bounded everything: concurrency, queue, per-session output, retained sessions, long-poll waiters.
5. Secrets only through `localflow/secrets.py`; never in `config.toml`, responses, event logs or log files.
6. Fully testable without network or vendor CLIs (`mock`, a fake `claude` executable, a stub HTTP server), under the existing `!sim_testing` protocol with Jev as independent judge.

**Non-goals**
- **No UI code in PR 1 or PR 2.** No change to `static/`, `templates/`, `hub_*`, `*_mac.py`, `*_tk.py`, `menubar*`, `rust/lf-ui`, `rust/lf-desktop`. The UX/UI Lead Bot builds PR 3 against §6.
- No replacement of the Orca path (`dispatch.py`, `lf prompts dispatch`); the two are independent. Feeding approved work prompts into the launcher is a later, separate change.
- No agent orchestration (chaining, fan-out, scheduling), no MCP tools for agents, no history schema change, no new sound assets.
- No LLM in the hot-command path: matching and prompt extraction are deterministic.

## 2. Module layout

New package `localflow/agents/` (never imported at module import time by `app.py`, `server.py` or `cli.py` beyond the thin pieces noted):

| File | Contents | Import weight |
|---|---|---|
| `__init__.py` | Empty apart from `API_VERSION = 1`. | stdlib-free |
| `models.py` | `AgentSession`, `AgentEvent`, `LaunchRequest`, `HotCommandRule`, `ProviderCapabilities`, `Readiness`, `STATES`, `TRANSITIONS`, `AgentError` hierarchy (`SessionConflict`, `HotCommandConflict`, `QueueFull`, `ProviderNotReady`, `AgentsDisabled`, `ValidationError`). | stdlib only |
| `store.py` | `AgentSessionStore` (sessions index + per-session event log), `HotCommandStore`, `HttpProfileStore`. All use the `HotPhraseStore` storage pattern. | stdlib only |
| `hotcommands.py` | Pure functions: `validate_rule`, `compile_rules`, `match_hot_command(text, rules, repos) -> HotCommandMatch \| None`. | `re` only |
| `handoff.py` | Dictation-side seam: `HotCommandLauncher` (lazy daemon thread + bounded queue → loopback `POST /api/agents/sessions`). | `requests` (already a dep) |
| `redact.py` | `Redactor` (known-secret values + patterns), `scrub_env`. | stdlib only |
| `registry.py` | `AGENT_BUILTINS` as **dotted-path strings**, `agent_provider_class(id)`, `list_agent_ids()`; entry-point group `localflow.agent_providers`. Mirrors `providers/registry.py` but resolves lazily with `importlib.import_module` so listing ids imports no provider. | stdlib only |
| `manager.py` | `AgentManager`: admission, concurrency, supervisor thread, timeouts, recovery, change notification. | stdlib only |
| `providers/base.py` | `AgentProvider` ABC, `EventSink`, `ProviderContext`. | stdlib only |
| `providers/claude.py`, `devin.py`, `http.py`, `mock.py` | Built-in providers. | `subprocess` / `requests` |
| `api.py` | `fastapi.APIRouter(prefix="/api/agents")` + `AgentApiError` → JSON handler. Builds the manager on first use. | fastapi + `models` |
| `cli.py` | Click group `agents`. | click/rich |

Modified existing files (PR 1):

| File | Change |
|---|---|
| `localflow/config.py` | `_SECTIONS["agents"]`, `Config` fields, `_ENUM_VALUES`, `_FIELD_TYPES`, range checks in `_valid_field_value` (§8). |
| `localflow/api.py` | `SECRET_NAMES += ("agent_devin_api_key", "agent_claude_api_key")`. `restart_required` unchanged (`agents` is deliberately **not** a restart section). |
| `localflow/secrets.py` | `_LEGACY_ENV["agent_devin_api_key"] = "DEVIN_API_KEY"`. |
| `localflow/live_config.py` | Add `agents_enabled`, `agent_hot_commands`, `agent_repos`, `agent_default_repo`, `agent_default_provider` to `ALLOWLIST` so the dictation app adopts them without restart. |
| `localflow/app.py` | `_process_clip(..., intercept=None)` hook; `_process_job` wires it for live `dictate` jobs only; `main()` builds the launcher lazily (§5). |
| `localflow/server.py` | `app.include_router(agents_api.router)`; register the `AgentApiError` handler; `api_config_update` gains the local-only guard for three keys (§8); shutdown hook calls `manager.shutdown()` if a manager exists. |
| `localflow/cli.py` | `cli.add_command(agents)` (import of `localflow.agents.cli` only). Note the existing hidden `lf agent` group is the autostart alias; the new group is `lf agents` (plural) and the two never share subcommand names. |
| `pyproject.toml` | `[project.entry-points."localflow.agent_providers"]` with the four built-ins. |
| `docs/contracts/agents-api.v1.schema.json` | **New.** JSON Schema for every object in §6; the A4 gate and PR 3 validate against it. |

Tests (PR 1): `tests/test_agents_{models,store,hotcommands,manager,provider_claude,provider_devin,provider_http,provider_mock,api,cli,config,dictation_seam,idle,redact}.py`, plus `tests/fixtures/agents/fake_claude.py` (argv/stdin/cwd/env recorder emitting canned JSON) and `stub_agent_server.py` (scripted HTTP stub).

## 3. Core data model & state machine

All timestamps are UTC ISO-8601 with `Z` (`2026-11-27T10:15:02Z`). Ids: session `secrets.token_hex(6)` (12 hex), rule/profile `token_hex(4)`.

### 3.1 `LaunchRequest` (input)

| Field | Type | Default | Rules |
|---|---|---|---|
| `id` | str | generated | Optional client-chosen `^[0-9a-f]{12}$`; makes the POST idempotent (§6.3). |
| `provider` | str | `agents.default_provider` | Must be in `list_agent_ids()`. |
| `profile` | str | `""` | Required iff `provider == "http"`; an `HttpProfile.id`. |
| `prompt` | str | — | Non-blank, ≤ `MAX_PROMPT_CHARS = 20000` (same bound as hot phrases). |
| `repo` | str | `agents.default_repo` | An alias from `agents.repos`. Mutually exclusive with `cwd`. |
| `cwd` | str | `""` | Absolute path; after `Path.resolve()` must equal or sit under a configured repo path. |
| `resume_of` | str | `""` | Id of a terminal session of the same provider with `capabilities.resume` and a non-empty `external_id`. |
| `title` | str | first 80 chars of prompt, one line | ≤ 120 chars. |
| `timeout_seconds` | int | `agents.timeout_seconds` | 10…86400. |
| `origin` | str | `"api"` | `api` \| `cli` \| `hot_command`. |
| `rule_id` | str | `""` | Set by the hot-command path. |
| `options` | object | `{}` | Provider-allowlisted scalars only: `claude`: `model`; `mock`: `script`. Unknown key → 400. |

### 3.2 `AgentSession` (persisted, returned by the API)

`id, provider, profile, state, origin, rule_id, title, prompt, repo, cwd, parent_id, root_id, external_id, external_url, sandbox(bool), created_at, started_at, updated_at, finished_at, timeout_seconds, last_seq(int), revision(int), result_text, result_truncated(bool), error({code, message} | null), usage({cost_usd, duration_ms, turns} with nullable members | null), cancel_requested(bool)`.

- `prompt` and every text field are stored **after redaction** (§8.3). `result_text` ≤ 16 KiB; the full output lives in the event log.
- `root_id` = own id, or the root of the `resume_of` chain; lets a UI thread a conversation without server joins.
- `revision` = value of the store-wide monotonic counter at this session's last write.

### 3.3 `AgentEvent` (append-only, per session)

`{seq:int, ts, type, stream, text, state, data}`
- `seq` starts at 1, strictly increasing, **contiguous** (events are never dropped from the head).
- `type`: `state` (carries `state`) · `output` · `message` (user follow-up, `stream="user"`) · `error` · `usage` · `truncated`.
- `stream`: `agent` \| `stderr` \| `user` \| `system`. `text` ≤ 8 KiB per event (longer chunks are split). `data` is a small provider-neutral object (may be `{}`).

### 3.4 States & transitions

States: `queued, starting, running, waiting_input, cancelling` (active) and `completed, failed, cancelled, timed_out` (terminal, never left).

| From | To | Trigger |
|---|---|---|
| — | `queued` | accepted launch |
| `queued` | `starting` | admission (a slot is free) |
| `queued` | `cancelled` | cancel |
| `queued` | `failed` | provider not ready at admission, or recovery after restart (`interrupted`) |
| `starting` | `running` | provider `start()` returned / process spawned |
| `starting` | `failed` \| `cancelled` | spawn/API error \| cancel |
| `running` | `waiting_input` | provider reports blocked (needs `capabilities.live_message`) |
| `waiting_input` | `running` | follow-up message sent or provider resumed |
| `running`, `waiting_input` | `completed` \| `failed` \| `timed_out` | provider result \| error / non-zero exit \| deadline |
| `running`, `waiting_input`, `starting` | `cancelling` | cancel on a provider that needs an async stop |
| `cancelling` | `cancelled` \| `failed` | stop confirmed \| stop failed (`cancel_failed`; local processes are SIGKILLed first, so this is cloud-only) |

`TRANSITIONS: dict[str, frozenset[str]]` in `models.py` is the single source of truth. Every write goes through `AgentSessionStore.transition(id, to, expect=<tuple>, **fields)` — a compare-and-set exactly like `PromptQueue.set_status(expect=…)`. A lost race or illegal move raises `SessionConflict(session_id, current, expected)` (same shape as `StatusConflict`) → HTTP 409.

Follow-ups never revive a terminal session: a resume is a **new** session with `parent_id` set (`resume_of`). Only `running`/`waiting_input` sessions of `live_message` providers accept `POST …/messages`.

Error codes (`error.code`, also the `code` of HTTP errors where applicable): `provider_not_ready, spawn_failed, exit_nonzero, bad_output, auth_failed, rate_limited, upstream_error, network_error, timeout, interrupted, cancel_failed, internal`.

### 3.5 Persistence & locking

```
~/.localflow/agents/            (0700; files 0600)
  sessions.json                 {"version":1,"revision":N,"sessions":[AgentSession…]}   newest last
  sessions.json.lock            flock sidecar
  events/<session_id>.jsonl     one AgentEvent per line, append-only
  hot_commands.json             [HotCommandRule…]            (+ .lock)
  http_providers.json           [HttpProfile…]               (+ .lock)
```

- Same pattern as `HotPhraseStore`/`PromptQueue`: `threading.Lock` + `fcntl.flock` on the sidecar around each read-modify-write, per-pid temp file + `os.replace`, unreadable/missing file reads as empty, degrade to in-process locking where `fcntl` is missing or the lock cannot be taken.
- **Writer roles.** The server process is the only writer of `sessions.json` and `events/`. The CLI and the dictation app mutate sessions only through HTTP. `hot_commands.json` and `http_providers.json` are written by the server (and by `lf agents rules|http` when the server is down) and read by the dictation app via a `(st_mtime_ns, st_size)` cache identical to `HotPhraseStore.enabled()`. The locks exist for those readers/offline writers and for the future Rust runtime.
- Event appends: `open(..., "a")`, one `write` per line, under the store's in-process lock; no flock (single writer), no fsync per line (fsync once on terminal transition).
- **Bounds.** Per-session log ≤ `agents.max_output_kb`. When the next `output` event would exceed it, the store appends exactly one `truncated` event and drops all later `output` events for that session; `state`, `error`, `usage` and `message` events are always written (reserve: 32 KiB beyond the limit). Retention: at most `agents.max_sessions` sessions; on each terminal transition the oldest terminal sessions beyond the limit are removed with their event files. Active sessions are never pruned.
- **Recovery** (first manager construction after a server start): `queued`, `starting`, `cancelling` and any active session of a provider without `capabilities.reattach` → `failed` / `interrupted`; active sessions of `reattach` providers are handed back to the supervisor.

## 4. Provider-neutral interface & built-in providers

### 4.1 Interface (`providers/base.py`)

```python
@dataclass(frozen=True)
class ProviderCapabilities:
    kind: Literal["local_process", "cloud"]
    resume: bool          # LaunchRequest.resume_of supported
    live_message: bool    # send_message while running / waiting_input
    cancel: bool
    streaming: bool       # incremental output events (else final result only)
    needs_cwd: bool       # a repo/cwd is required
    reattach: bool        # active sessions survive a server restart
    secret_names: tuple[str, ...]   # names only

@dataclass(frozen=True)
class Readiness:
    ready: bool
    code: str      # ok | disabled | missing_binary | missing_wrapper | missing_secret | missing_profile | unreachable | auth_failed
    detail: str    # human sentence, already redacted

class AgentProvider(ABC):
    id: ClassVar[str]; label: ClassVar[str]; capabilities: ClassVar[ProviderCapabilities]
    def __init__(self, config: Config, ctx: ProviderContext): ...
    def readiness(self, *, deep: bool = False) -> Readiness      # deep=False: no subprocess, no network
    def start(self, session: AgentSession, sink: EventSink) -> None   # must return promptly
    def poll(self, session: AgentSession, sink: EventSink) -> float | None   # cloud only; seconds until next poll
    def send_message(self, session: AgentSession, text: str, sink: EventSink) -> None
    def cancel(self, session: AgentSession, sink: EventSink) -> None
    def close(self) -> None
```

- `EventSink`: `output(stream, text)`, `external(id, url)`, `state(to)`, `usage(...)`, `finish(state, result_text="", error=None)`. The sink redacts, persists and notifies; providers never touch the store.
- `ProviderContext`: `get_secret`, `redactor`, `clock`, `popen`, `http` (a `requests.Session` factory with `allow_redirects=False`), all injectable for tests.
- **Laziness.** `AgentManager` holds `dict[str, AgentProvider]`, filled on first `start`/deep check of that id. `GET /api/agents/providers` uses class attributes plus a shallow `readiness()` on a throwaway instance — no network, no subprocess.
- **Execution.** `local_process`: `start()` spawns `Popen` and one reader thread that blocks on the pipe (no polling) and calls `sink.finish` at EOF. `cloud`: `start()` makes the create call; the manager's single supervisor thread calls `poll()` when due.
- **Supervisor.** One thread `lf-agents-supervisor`, created on the first admission, sleeping on a `threading.Condition` until `min(next cloud poll, nearest timeout deadline)`; it exits as soon as no session is active. Cloud polls back off ×1.5 up to 60 s while a session's state is unchanged, reset on change.
- **Admission.** FIFO over `queued`; a session is admitted when `count(starting|running|waiting_input|cancelling) < agents.max_concurrent`. A launch arriving with `count(queued) >= agents.max_queued` is refused (`QueueFull` → 429). Admission runs on launch, on every terminal transition, and on recovery — never on a timer.

Registry: `AGENT_BUILTINS = {"claude": "localflow.agents.providers.claude:ClaudeProvider", "devin": …, "http": …, "mock": …}`; unknown id → `KeyError("unknown agent provider 'x'; valid ids: …")`, entry points in group `localflow.agent_providers` resolved by name exactly as `llm_provider_class` does. An entry-point provider that fails to load is listed with `ready=false, code="disabled"` and never raises into a route.

### 4.2 `claude` (local process)

Capabilities: `local_process, resume=True, live_message=False, cancel=True, streaming=agents.claude_stream, needs_cwd=True, reattach=False, secret_names=("agent_claude_api_key",)`.

- argv (list, `shell=False`), built only from validated config and fixed tokens:
  `[*wrapper, claude_bin, "-p", "--output-format", "json"]` + `["--resume", external_id]` (resume) + `["--permission-mode", mode]` (if not `default`) + `["--model", m]` (if set). With `claude_stream = true`: `--output-format stream-json --verbose`. **VERIFY** flags against `claude --help`.
- The prompt is written to **stdin**, never argv: no option injection from a prompt starting with `-`, no leak through `ps`, no arg-length limit.
- `cwd = session.cwd`; `start_new_session=True` (POSIX) / `CREATE_NEW_PROCESS_GROUP` (Windows); env = `scrub_env(os.environ)` (drops every `LOCALFLOW_*`, `FIREWORKS_API_KEY`, `DEVIN_API_KEY`) plus `ANTHROPIC_API_KEY` only when `agent_claude_api_key` is set (empty = the CLI's own login).
- Result: final JSON object → `result` → `result_text`, `session_id` → `external_id`, `is_error` → `failed/exit_nonzero`, `total_cost_usd`/`duration_ms`/`num_turns` → `usage`. Unparseable stdout → `failed/bad_output` with the last non-blank stderr line (≤ 200 chars, as `dispatch._last_line`).
- Cancel/timeout: SIGTERM to the process group, SIGKILL after 5 s.
- Headroom: `agents.claude_wrapper = "headroom"` selects a **fixed** argv prefix from a table in `claude.py` (`("headroom", "wrap")`, giving `headroom wrap claude -p …`; **VERIFY** exact form against the installed Headroom CLI). The wrapper is an enum, never a free-form string. Missing binary → readiness `missing_wrapper`; there is no silent fallback to unwrapped.
- Readiness: shallow = `shutil.which(claude_bin)` (+ wrapper); deep = `claude --version` with a 10 s timeout.

### 4.3 `devin` (cloud)

Capabilities: `cloud, resume=False, live_message=True, cancel=True, streaming=False, needs_cwd=False, reattach=True, secret_names=("agent_devin_api_key",)`.

All vendor specifics live in one table in `devin.py` (**VERIFY** against current Devin API docs; v1 shown):

| Operation | Request | Mapping |
|---|---|---|
| start | `POST {base}/v1/sessions` `{"prompt", "title", "idempotent": true}` | `session_id` → `external_id`, `url` → `external_url` |
| poll | `GET {base}/v1/sessions/{id}` | `status_enum`: `working`→`running`, `blocked`→`waiting_input`, `finished`→`completed`, `expired`→`timed_out`; unknown → unchanged (raw value in `data.raw_status`); new messages → `output` events |
| message | `POST {base}/v1/sessions/{id}/message` `{"message"}` | 2xx → `message` event |
| cancel | `DELETE {base}/v1/sessions/{id}` | 2xx/404 → `cancelled` |

`base` is the constant `https://api.devin.ai` (test-only override `LOCALFLOW_DEVIN_BASE_URL`); it is deliberately **not** a config key, so a config write can never redirect the key to another host. `Authorization: Bearer <secret>`. HTTP 401/403 → `auth_failed`, 429 → `rate_limited` (honour `Retry-After` for the next poll), 5xx/timeouts → retried on the poll schedule, `failed/upstream_error` after 5 consecutive failures. Repo context for Devin is conveyed in the prompt (`repo` alias is recorded, `cwd` ignored).

### 4.4 `http` (generic cloud/webhook, config-only)

Capabilities are derived per profile: `cloud`, `live_message = "message" in profile`, `cancel = "cancel" in profile`, `reattach = "status" in profile`, `resume=False`, `needs_cwd=False`.

`HttpProfile` (in `http_providers.json`):

```json
{"id":"a1b2c3d4","label":"My agent","base_url":"https://agents.example.com","enabled":true,
 "headers":{"Authorization":"Bearer {secret}","X-Source":"localflow"},
 "start":{"method":"POST","path":"/v1/runs","body":{"input":"{prompt}","name":"{title}","ref":"{session_id}"},
          "id_path":"id","url_path":"links.web"},
 "status":{"method":"GET","path":"/v1/runs/{external_id}","state_path":"status",
           "state_map":{"queued":"running","running":"running","needs_input":"waiting_input","done":"completed","error":"failed"},
           "result_path":"output.text","poll_seconds":10},
 "message":{"method":"POST","path":"/v1/runs/{external_id}/messages","body":{"text":"{message}"}},
 "cancel":{"method":"POST","path":"/v1/runs/{external_id}/cancel"}}
```

- Only `start` is required. Without `status` the profile is a fire-and-forget webhook: 2xx → `completed` with the first 16 KiB of the response as `result_text`.
- Placeholders: `{prompt} {title} {session_id} {external_id} {repo} {message} {secret}`. Substitution happens on **JSON string leaves** of `body` (the body is never built by string concatenation, so a prompt cannot inject JSON) and on `path` with percent-encoding. `{secret}` is legal in header values only.
- Validation (`ValidationError` → 400): `base_url` must be `https://` (or `http://` on a loopback host); methods ∈ `GET/POST/PUT/PATCH/DELETE`; paths start with `/`; `*_path` selectors are dotted keys/indices; `state_map` values ∈ `STATES`; a header named `authorization`, `cookie`, or matching `(?i)(key|token|secret)` must contain `{secret}` (no literal credentials on disk).
- Secret: `agent_http_<profile_id>_token`, **bound to the profile's origin** (scheme+host+port): changing the origin deletes the stored secret (same intent as `llm_activation` endpoint binding).
- Transport: no redirects, 30 s timeout, response capped at 1 MiB, error mapping as §4.3.

### 4.5 `mock` (deterministic)

Capabilities: all `True`, `kind="cloud"`, no secrets. Scripts selected by `options.script` (default `ok`), advanced one step per `poll()` with an injected clock, so tests are time-independent:

`ok` (2 output chunks → completed) · `fail` · `needs_input` (waits for one message, then completes echoing it) · `slow` (N polls) · `hang` (never finishes → timeout) · `flood` (output > `max_output_kb`) · `secret_echo` (emits a planted secret → must be redacted) · `flaky_cancel` (cancel → `cancel_failed`).

`agents.sandbox = true` routes **every** launch to `mock` (the session keeps the requested `provider`, with `sandbox: true`). Launching `provider="mock"` directly is allowed only when `agents.sandbox` is true (else 400), so mock sessions cannot be mistaken for real work.

## 5. Hot commands & the dictation seam

### 5.1 Rule & grammar

`HotCommandRule`: `{id, trigger, provider, profile, repo, prompt_template, min_prompt_words, enabled, created_at, updated_at}`. Defaults: `prompt_template="{prompt}"`, `min_prompt_words=3`. At most 32 rules. No rules are seeded.

```
utterance := TRIGGER [ ("in" | "on") REPO_ALIAS ] [ "to" ] PROMPT
```

Example: rule `launch claude` → “Launch Claude in localflow to fix the flaky hot phrase test” ⇒ `provider=claude, repo=localflow, prompt="fix the flaky hot phrase test"`.

Matching (`match_hot_command`, pure, one precompiled regex per rules-file signature):
1. Tokenise with the `normalize_trigger` rule (`[^\W_]+`, case-folded), keeping character offsets into the raw transcript.
2. The trigger must match **at token 0** on whole-word boundaries (anchored at utterance start — a trigger mid-sentence never fires). Rules are tried longest first (word count, then length), as `_trigger_regex` does.
3. If the next token is `in`/`on` and the following tokens equal a configured repo alias (longest alias first), consume them; otherwise `in …` stays part of the prompt and the rule's `repo` (or `agents.default_repo`) applies.
4. Drop one leading `to`. The prompt is the **raw transcript slice** from that offset, stripped of leading punctuation/whitespace, then `apply_replacements` (dictionary) and `apply_spoken_formatting` — no LLM cleanup, no spoken-symbol pass, no style.
5. `prompt_template.replace("{prompt}", prompt)`. A template without `{prompt}` is a fixed prompt (then `min_prompt_words` is forced to 0 and extra words are ignored).

False-positive safety — a non-match always falls through to **unchanged normal dictation**:
- Trigger: 2–6 words, ≤ 60 chars; normalised duplicates → `HotCommandConflict` (409); a trigger that equals, or is a token-prefix of, an enabled hot-phrase trigger (or vice versa) is refused at creation. At runtime hot commands are evaluated before hot phrases.
- Fewer than `min_prompt_words` prompt words → no match (“launch claude” alone is typed as text).
- Only `Job.kind == "dictate"` clips from the live worker. Never: command/transform jobs, `/transcribe`, meeting transcripts, `retry_history_entry` / `transcribe_history_entry`.
- Disabled rule, `agents.enabled = false` or `agents.hot_commands = false` → no match. Provider unknown/repo alias removed since the rule was written → no match, one `log.warning`.
- Cooldown: a second hot launch within 3 s of the previous one is ignored (falls through), guarding against duplicate clips.

### 5.2 `app.py` integration

- `_process_clip` gains `intercept: Callable[[str], bool] | None = None`, called once with the raw STT text right after `raw_text_callback`. If it returns `True` the function records timings and returns `""` — no render, no cleanup LLM call, no paste, no print of the text.
- `_process_job` (dictate branch) passes `intercept` only when `config.agents_enabled and config.agent_hot_commands`. On interception it returns `(None, False)`, so `_worker_loop` neither writes last-text nor pastes; it then skips the `silent` branch and writes history as `kind="command"`, `raw_text=<transcript>`, `final_text=""`, `status="ok"`, no audio clip. (`last_ok()` requires `final_text != ''`, so paste-last is unaffected; no schema change.) The perf ledger row is recorded as usual.
- `main()` creates nothing up front. The first clip processed with the feature on does `from localflow.agents import handoff, hotcommands, store` and builds a `HotCommandStore` + `HotCommandLauncher`; with the feature off `localflow.agents` is never imported. Per-clip cost when on: one `stat()` and one anchored regex match.
- `HotCommandLauncher.submit(match)` generates the session id, enqueues to a `queue.Queue(maxsize=8)` and returns immediately (full queue → drop + warning). Its daemon thread (started on first submit) POSTs the `LaunchRequest` (`origin="hot_command"`, `rule_id`, client `id`) to the loopback server with `X-Localflow-Token` (`apitoken.read()`), `trust_env=False`, no redirects, 5 s timeout, one retry on connection error. Outcome → `log.info`/`log.warning` with the session id, provider and prompt **length** only, and an existing audio cue through `sounds.play` when `sounds_enabled` (start cue on 202/200, error cue otherwise). The dictation worker never waits on the network.

## 6. HTTP API & UI interface contract (`agents-api v1`)

**This section is the complete contract for PR 3.** Machine-readable twin: `docs/contracts/agents-api.v1.schema.json`.

### 6.1 Conventions

- Base: the LocalFlow server (`[server] host/port`). Every route sits behind the existing middleware: send `X-Localflow-Token: <token>` (or `?token=`). Missing/wrong token → `401 {"detail":"invalid API token"}`; with a non-loopback `Host` → `403 {"detail":"invalid host"}`.
- Bodies are JSON objects, UTF-8. Unknown body fields → 400 (never silently ignored). Validation errors are **400**, never 422 (handlers parse the raw object, as `_json_object` does).
- Error envelope for every non-2xx produced by these routes: `{"detail": "<human sentence, safe to display>", "code": "<slug>"}` plus optional fields named below. `detail` never contains a secret or a prompt.
- **Local-only writes.** Every non-GET route requires a direct loopback request (`server._request_is_local`: loopback peer, loopback `Host`, no proxy headers) unless `agents.allow_remote = true`; otherwise `403 code="local_only"`. GET routes work wherever the token works.
- Feature off (`agents.enabled = false`): GET routes work (old sessions stay readable); launching, messaging and hot-command testing return `409 code="agents_disabled"`. Cancel and delete still work.
- A server without this feature (older build, or the Rust server before PR 4) answers `404` on `GET /api/agents/status`; the UI must treat that as “agents unsupported” and hide the feature.
- Responses never include secret values. `prompt`, `result_text`, `title`, event `text` and error `detail` are redacted server-side; the UI must render them as plain text (untrusted agent output — no HTML/Markdown execution).

Codes: `validation` 400 · `local_only` 403 · `not_found` 404 · `agents_disabled` 409 · `session_conflict` 409 (+`state`, `expected[]`) · `id_conflict` 409 · `rule_conflict` 409 · `provider_not_ready` 409 (+`readiness`) · `unsupported` 409 (capability missing) · `prompt_too_long` 413 · `queue_full` 429 (+`Retry-After: 5`) · `secrets_unavailable` 503 · `internal` 500.

### 6.2 Status, providers, secrets

`GET /api/agents/status` → 200
```json
{"api_version":1,"enabled":true,"hot_commands":false,"sandbox":false,"allow_remote":false,
 "default_provider":"claude","default_repo":"localflow",
 "repos":[{"alias":"localflow","path":"/home/u/repos/localflow","exists":true}],
 "limits":{"max_concurrent":2,"max_queued":8,"max_prompt_chars":20000,"max_wait_seconds":25},
 "counts":{"active":1,"queued":0,"terminal":14},"revision":57}
```

`GET /api/agents/providers` → 200 `{"providers":[Provider…]}` (sorted by `id`; one entry per `http` profile, `id="http"`, distinguished by `profile`)
```json
{"id":"devin","profile":"","label":"Devin","builtin":true,
 "capabilities":{"kind":"cloud","resume":false,"live_message":true,"cancel":true,"streaming":false,"needs_cwd":false,"reattach":true},
 "secrets":[{"name":"agent_devin_api_key","set":false,"required":true}],
 "readiness":{"ready":false,"code":"missing_secret","detail":"Add a Devin API key.","checked":"shallow"}}
```
Shallow readiness is computed per request and is free (no network/subprocess).

`POST /api/agents/providers/{id}/check` body `{"profile":""}` → 200 `Provider` with `readiness.checked="deep"` (runs `claude --version` / an authenticated vendor call; ≤ 15 s). 404 unknown provider/profile.

`PUT /api/agents/providers/{id}/secret` body `{"name":"agent_devin_api_key","value":"…"}` (`name` optional when the provider has exactly one secret; for `http` pass `"profile"`). `value:""` clears. → 200 `Provider`. 400 name not owned by that provider; 503 keyring unavailable. `GET /api/secrets` also reports the two static names as booleans.

HTTP profiles: `GET /api/agents/http-profiles` → `{"profiles":[HttpProfile…]}` · `POST` → 201 `HttpProfile` · `PUT /api/agents/http-profiles/{id}` → 200 · `DELETE …/{id}` → 200 `{"deleted":true}` (409 `session_conflict` while an active session uses it; also deletes its secret). Shape as §4.4 plus `created_at`, `updated_at`, `secret_set: bool`.

### 6.3 Sessions

`POST /api/agents/sessions` body = `LaunchRequest` (§3.1) → **202** `{"session": Session}` with `state="queued"` (admission is asynchronous; the session may already be `starting` in a later read).
- Replay with the same client `id` and an identical body → **200** with the existing session, nothing launched. Same `id`, different body → 409 `id_conflict`.
- 400 `validation` (unknown provider/profile/repo/option, `cwd` outside configured repos, `repo` with `cwd`, missing repo for a `needs_cwd` provider, bad `resume_of`) · 409 `provider_not_ready` (shallow readiness false; body carries `readiness`) · 409 `unsupported` (`resume_of` on a provider without `resume`) · 413 · 429 `queue_full`.

`Session` (full): every field of §3.2. `SessionSummary` = `Session` without `prompt` and `result_text`, plus `result_preview` (first 200 chars).

`GET /api/agents/sessions?state=&provider=&origin=&root_id=&q=&limit=&before=` → 200
```json
{"sessions":[SessionSummary…],"next_cursor":"<opaque>|null","revision":57,"counts":{"active":1,"queued":0,"terminal":14}}
```
- Order: `created_at` desc, then `id` desc. `limit` 1…200, default 50. `before` = a `next_cursor` from a previous page.
- `state`: comma list of states, or the aliases `active` / `terminal`. `q`: case-insensitive substring over `title`. Bad value → 400.

`GET /api/agents/sessions/{id}` → 200 `{"session": Session}` · 404.

`POST /api/agents/sessions/{id}/cancel` → 200 `{"session": Session}` (state `cancelled`, or `cancelling` when the provider stops asynchronously). Already `cancelling`/`cancelled` → 200 unchanged (idempotent). Other terminal state → 409 `session_conflict`.

`POST /api/agents/sessions/{id}/messages` body `{"text": "…"}` (≤ 20000 chars) → 202 `{"session": Session, "event": Event}`. Requires `capabilities.live_message` (else 409 `unsupported`) and state `running`/`waiting_input` (else 409 `session_conflict`). To continue a finished `resume`-capable session, POST a new session with `resume_of`.

`DELETE /api/agents/sessions/{id}` → 200 `{"deleted": true}`; terminal sessions only (active → 409 `session_conflict`); removes the event log.

### 6.4 Events & change feed (cursor polling with optional long-poll)

PR 1 ships cursor polling with a long-poll `wait`; there is no SSE/WebSocket in v1 (`/api/agents/stream` is reserved for a later additive version).

`GET /api/agents/sessions/{id}/events?after=0&limit=200&wait=0` → 200
```json
{"events":[{"seq":3,"ts":"2026-11-27T10:15:04Z","type":"output","stream":"agent","text":"…","state":null,"data":{}}],
 "next_after":3,"last_seq":3,"has_more":false,"state":"running","terminal":false,"waited":false}
```
- Returns events with `seq > after`, ascending, at most `limit` (1…500). Pass `next_after` as the next `after`. Sequences are contiguous from 1, so a client can detect nothing missing by `events[0].seq == after + 1`.
- `wait` (0…25 s): if there is nothing new **and** the session is not terminal, the server holds the request until an event arrives or the time elapses, then answers 200 (possibly with `events: []`). `waited` says whether the hold was honoured; when more than 16 long-polls are open server-wide it is `false` and the response is immediate — the client must then sleep ≥ 2 s before retrying.
- Stop polling when `terminal` is true and `has_more` is false.
- An `events` item with `type="truncated"` means later `output` was discarded (bounded log); state/error/usage events still follow.

`GET /api/agents/changes?since=<revision>&wait=0` → 200
```json
{"revision":58,"sessions":[SessionSummary…],"deleted":["9f2c…"],"reset":false,"counts":{"active":1,"queued":0,"terminal":14},"waited":true}
```
- `sessions` = every session whose `revision > since`. `deleted` = ids removed since `since`. `reset: true` means the server cannot answer incrementally (tombstone window passed or server restarted): refetch `GET /api/agents/sessions`.
- Same `wait` semantics. This is the only request a list view needs to keep open; an idle UI holds one `changes?wait=25` call and the server holds no thread for it (asyncio waiter). The UI must not poll `…/sessions` on a timer, and must not poll anything while its view is hidden.

### 6.5 Hot-command rules

`GET /api/agents/hot-commands` → `{"rules":[HotCommandRule…]}` · `POST` body `{trigger, provider, profile?, repo?, prompt_template?, min_prompt_words?, enabled?}` → 201 rule · `PUT /api/agents/hot-commands/{id}` (partial) → 200 rule · `DELETE …/{id}` → 200 `{"deleted":true}`. 400 `validation`, 409 `rule_conflict` (duplicate/overlapping trigger; `detail` names the other trigger), 404.

`POST /api/agents/hot-commands/test` body `{"text":"launch claude in localflow to fix the tests"}` → 200
`{"matched":true,"rule_id":"…","provider":"claude","profile":"","repo":"localflow","prompt":"fix the tests","reason":""}` or `{"matched":false,…,"reason":"too_few_words|no_trigger|disabled|unknown_repo|feature_off"}`. Never launches anything. This is the same pure function the dictation app runs.

## 7. CLI — `lf agents …`

Mutating commands use the HTTP API (`_post`/`_put`/`_delete` helpers, token from `apitoken.read()`); `list`, `show`, `logs` fall back to reading the store directly when the server is down (as `lf prompts list` does). Session ids accept a unique prefix (same resolution as `cli._resolve`). Every read command has `--json`.

| Command | Flags |
|---|---|
| `lf agents status` | `--json` |
| `lf agents providers` | `--json`, `--check` (deep readiness for each) |
| `lf agents launch [PROMPT]` | `-p/--provider ID`, `--profile ID`, `-r/--repo ALIAS`, `--cwd PATH`, `--resume SESSION`, `--title TEXT`, `--timeout SECONDS`, `-F/--file PATH` (`-` = stdin; exclusive with PROMPT), `-f/--follow`, `--json` |
| `lf agents list` | `-s/--state STATE` (repeatable; `active`, `terminal`), `-p/--provider ID`, `-n/--limit N` (default 20), `--json` |
| `lf agents show SESSION` | `--json` |
| `lf agents logs SESSION` | `-f/--follow` (long-poll), `--after SEQ`, `--json` (one event per line) |
| `lf agents send SESSION MESSAGE` | — |
| `lf agents cancel [SESSION…]` | `--all` (every active session; asks unless `-y/--yes`) |
| `lf agents rm [SESSION…]` | `--finished` (all terminal), `-y/--yes` |
| `lf agents rules list` | `--json` |
| `lf agents rules add TRIGGER` | `-p/--provider ID` (required), `--profile ID`, `-r/--repo ALIAS`, `--template TEXT`, `--min-words N`, `--disabled` |
| `lf agents rules enable\|disable\|rm RULE` | — |
| `lf agents rules test UTTERANCE` | `--json` |
| `lf agents http list\|add FILE\|rm PROFILE` | `add` reads an `HttpProfile` JSON file; `--json` on `list` |
| `lf agents secret set\|unset PROVIDER` | `--profile ID`; `set` prompts with hidden input and writes the keyring directly (like `lf secret set`); never echoes |

Exit codes: 0 success; 1 error (message from the envelope's `detail`); `launch --follow` exits 0 only if the session ends `completed`.

## 8. Config & security

### 8.1 `[agents]` keys

| File key | `Config` field | Type / default | Validation |
|---|---|---|---|
| `enabled` | `agents_enabled` | bool `false` | |
| `hot_commands` | `agent_hot_commands` | bool `false` | effective only with `enabled` |
| `default_provider` | `agent_default_provider` | str `"claude"` | non-empty; resolved at launch (entry-point ids allowed) |
| `default_repo` | `agent_default_repo` | str `""` | `""` or an alias in `repos` |
| `repos` | `agent_repos` | list[str] `[]` | each `alias=/absolute/path`; alias `^[a-z0-9][a-z0-9 _-]{0,39}$`, unique after `normalize_trigger`; ≤ 32 |
| `max_concurrent` | `agent_max_concurrent` | int `2` | 1…8 |
| `max_queued` | `agent_max_queued` | int `8` | 0…64 |
| `timeout_seconds` | `agent_timeout_seconds` | int `1800` | 10…86400 |
| `max_output_kb` | `agent_max_output_kb` | int `512` | 16…8192 |
| `max_sessions` | `agent_max_sessions` | int `200` | 10…2000 |
| `poll_seconds` | `agent_poll_seconds` | int `10` | 2…300 |
| `allow_remote` | `agent_allow_remote` | bool `false` | |
| `sandbox` | `agent_sandbox` | bool `false` | |
| `claude_bin` | `agent_claude_bin` | str `"claude"` | bare command name or absolute path; no whitespace-split, no shell |
| `claude_wrapper` | `agent_claude_wrapper` | enum `"none"` | `none` \| `headroom` |
| `claude_permission_mode` | `agent_claude_permission_mode` | enum `"default"` | `default` \| `acceptEdits` \| `plan` |
| `claude_model` | `agent_claude_model` | str `""` | `^[A-Za-z0-9._:\[\]-]{0,80}$` |
| `claude_stream` | `agent_claude_stream` | bool `false` | |

All go through the existing machinery: `_SECTIONS`, `_FIELD_TYPES`, `_ENUM_VALUES`, `_valid_field_value` (load: warn and use the default; `apply_config_update`: `ConfigError` → 400), and appear in `valid_config_keys()` / `lf config set`.

`restart_required`: **no `agents.*` key requires a restart.** The server reads `_config` at each launch/admission (limits apply to the next admission; running sessions keep the timeout they started with); the dictation app adopts the allowlisted fields through `live_config`. `api.restart_required` therefore returns nothing for the section.

Local-only config keys: `PUT /api/config` rejects a body that changes `agents.claude_bin`, `agents.repos` or `agents.allow_remote` unless the request is local (`_request_is_local`) → 403; these three decide what binary runs, where it runs, and who may launch.

### 8.2 Secrets

| Name | Env | Legacy env | Used by |
|---|---|---|---|
| `agent_devin_api_key` | `LOCALFLOW_AGENT_DEVIN_API_KEY` | `DEVIN_API_KEY` | `devin` |
| `agent_claude_api_key` | `LOCALFLOW_AGENT_CLAUDE_API_KEY` | — | `claude` (optional) |
| `agent_http_<profile_id>_token` | `LOCALFLOW_AGENT_HTTP_<ID>_TOKEN` | — | `http` profile, origin-bound |

Resolved only via `secrets.get_secret` at call time (never cached on the session, never written to any store). Secret names accepted by the agents routes must match `^agent_[a-z0-9_]{1,60}$` and be declared by the addressed provider — the routes cannot read or write any other LocalFlow secret.

### 8.3 Redaction & logging

- `Redactor` = exact-value replacement of every currently resolvable agent secret of length ≥ 8 (`[redacted:<name>]`) + patterns (`sk-ant-…`, `Bearer <token>`, `Authorization:`/`x-api-key:` header values, `apk_…`-style Devin keys). Rebuilt when a secret changes.
- Applied by `EventSink` and the store to: prompts and titles before persistence, every event `text`, `result_text`, `error.message`, readiness `detail`, and HTTP error `detail`.
- Logs carry ids, provider, state, byte/char counts, durations and error codes — never prompt text, output text, header values or request bodies. Vendor URLs are logged without query strings.
- Files under `~/.localflow/agents/` are 0600 (dir 0700).

### 8.4 Command-injection & abuse prevention

- `subprocess` with list argv and `shell=False` everywhere; argv is assembled from fixed tokens + validated enum/regex config values; the prompt travels on stdin.
- `cwd` only inside a configured repo path (after `resolve()`, symlinks included); must exist and be a directory at admission.
- Child env is scrubbed of LocalFlow secrets (§4.2).
- `http` templating cannot change the host (only `path` is templated, values percent-encoded), cannot inject JSON structure, follows no redirects.
- Launch surface: token + Host guard + local-only writes; bounded queue (429) and concurrency; hot commands additionally need two opt-in flags, an explicit user-created rule, start-anchoring, a minimum prompt length and a cooldown.
- `claude_permission_mode` excludes any bypass mode from the settable enum.

## 9. Sim-testing & Jev judge plan

Extends `evals/localflow_sim` as a new suite **`agents`** (cells `agents_*` in `scenarios.json`, checks in `gates.py`, suite added to `driver.py --suite`), under the unchanged protocol rules: pinned SUT commit asserted per cell, one fresh process per cell with its own `HOME`/`XDG_*`/`LOCALFLOW_CONFIG`, keyring forced to `keyring.backends.fail.Keyring`, N=3 repeats, worst-of-N, `flaky` when repeats disagree, Wilson intervals, same verdict logic. PR 2 appends an “Agents” section to `PROTOCOL.md` and **freezes it before the first run**.

**Cell isolation & boundaries.** Each cell gets its own `~/.localflow/agents/`, a temp git repo as the only configured `repos` entry, a private `PATH` whose `claude` (and `headroom`) are `fake_claude.py` recorders, `LOCALFLOW_DEVIN_BASE_URL` pointing at the in-cell stub server, and planted fake secrets (`LOCALFLOW_AGENT_DEVIN_API_KEY=sim-secret-…`). No cell touches the network or a real vendor CLI. Scripted transcriber as today.

### 9.1 Deterministic gates (thresholds frozen; “all” = every assertion in every repeat)

- **A1 lifecycle & state machine** — mock scripts `ok`, `fail`, `needs_input`, `hang` (timeout), cancel-while-queued, cancel-while-running, `flaky_cancel`: exact state sequence per script; event `seq` contiguous from 1; terminal states never change; every illegal transition raises `SessionConflict` / returns 409 with `state` and `expected`; resume creates a child with correct `parent_id`/`root_id` — all.
- **A2 hot-command parsing & dictation seam** — through `_process_job` with a scripted transcriber: ≥ 20 positive utterances → exact `(rule_id, provider, repo, prompt)`, no paste text, history row `kind=command`/`final_text=""`/`status=ok`; ≥ 20 negatives (trigger mid-sentence, partial word, too few words, disabled rule, either feature flag off, unknown alias, cooldown repeat, command/transform jobs, history retry) → no launch request and byte-identical normal dictation output; duplicate/overlapping trigger → `HotCommandConflict`; `/hot-commands/test` agrees with the app on every utterance — all (precision and recall both 1.0).
- **A3 concurrency & bounds** — `max_concurrent=2`, `max_queued=3`, six `slow` launches: never more than 2 active at any observed instant, FIFO admission order, the 6th → 429 `queue_full`; `flood`: event file ≤ `max_output_kb` + 32 KiB, exactly one `truncated` event, final `state` event present, `result_text` ≤ 16 KiB; retention prunes oldest terminal sessions and their logs, never an active one — all.
- **A4 API contract & auth** — every `/api/agents/*` route: no/wrong token → 401, foreign `Host` → 403, and nothing changed; non-local write → 403 `local_only`; every 2xx and error body validates against `agents-api.v1.schema.json`; pagination walks all sessions exactly once; filters; idempotent replay (200, one session) and `id_conflict`; events cursor never skips or repeats a `seq`; long-poll returns < 1 s after an event and within `wait + 1 s` otherwise; `changes` reports updates, deletions and `reset` — all.
- **A5 secrets & redaction** — after a full cell run, no planted secret value appears in any file of the cell sandbox (stores, event logs, `config.toml`, log files), any HTTP response body, or CLI stdout/stderr (byte search); `secret_echo` output shows `[redacted:…]`; a profile with a literal credential header is rejected; changing a profile origin clears its secret; `PUT /api/config` cannot set the three local-only keys remotely — all.
- **A6 provider adapters (scripted boundary)** — `claude`: recorded argv equals the expected list exactly (base, `--resume`, wrapper prefix, permission mode, model), prompt present on stdin and absent from argv, `cwd` correct, env contains no `LOCALFLOW_*`; prompts containing `; rm -rf`, `$(…)`, backticks, newlines and a leading `--dangerously…` arrive byte-identical on stdin and alter no argv element; canned success/`is_error`/garbage/non-zero exit map to the right state and `error.code`. `devin` and `http`: exact request sequence (method, path, JSON body, auth header) against the stub; status mapping incl. unknown status; message; cancel; 401/429/5xx/timeout → specified codes and retry behaviour; webhook-only profile completes on 2xx — all.
- **A7 idle & efficiency** — feature off: after a dictation job and after server requests to non-agent routes, `localflow.agents` is absent from `sys.modules` in the app process and no `AgentManager` exists in the server; feature on, no sessions: zero threads named `lf-agents-*`, zero provider instances; supervisor thread exits ≤ 1 s after the last session ends; `match_hot_command` p95 ≤ 1 ms over 1 000 utterances with 32 rules (recorded; gated at ≤ 5 ms); `HotCommandLauncher.submit` returns ≤ 5 ms with the server down — all.
- **A8 recovery & cross-process** — server killed mid-run: on restart local/queued sessions are `failed/interrupted`, a `devin` session is re-attached and completes; two processes issuing 200 interleaved rule writes lose none and never produce an unreadable file; hot command with the server down: no paste, history row written, warning logged, no exception in the worker — all.

Bookkeeping: a missing/crashed/off-commit agents cell fails A1 (the suite's lifecycle gate) and the gate it feeds.

### 9.2 Live-optional boundary

`AGENTS_LIVE=claude,devin` (plus real credentials in the environment) adds cells outside the gated matrix: **L1** real `claude -p` in the temp repo with a trivial verifiable task (create a named file with given content → file exists, session `completed`, `usage` present); **L2** real Devin create → observe `running` → cancel → `cancelled`. Results appear in a separate “live” table, are listed under `not_exercised` when unset, and never change the verdict. They exist to catch drift in the **VERIFY** items.

### 9.3 Jev (independent judge)

Jev `jev-1.14.0` via cortex `evals/sim/judge.ts`, driven by `jev_inputs.py` → `jev.ts` exactly as today. Gates stay the verdict; Jev is the second opinion on the things a regex cannot see.

| Set | Source given to Jev | Item judged | Question | Signal |
|---|---|---|---|---|
| **JA1 prompt fidelity** (A2 positives) | the spoken transcript | the extracted launch prompt | Does the prompt state the same task as the transcript, with nothing added and nothing material dropped (trigger/repo words excluded)? | `verdict.pick` correct / partially ok vs contradicts / unrelated |
| **JA2 launch intent** (A2 positives + negatives) | the transcript + the rule list | launched? yes/no | Is this utterance a request to start an agent under these rules? | disagreement with the gate's label → adjudication (guards the corpus labels themselves) |
| **JA3 error helpfulness** (A1/A6 failure cells) | the scripted cause (e.g. “API key missing”, “binary not found”) | user-facing `detail` + `error.message` | Does the message state the actual cause and a correct next step, and reveal no credential? | advisory flag at p crossing 0.5 |
| **JA4 result faithfulness** (A6 canned outputs; L1/L2 when live) | task prompt + full event log | `result_text` / `result_preview` | Is the reported result supported by the log, with nothing invented by truncation? | `sourcesSuffice.p >= 0.8` with a wrong answer = suspect |

Calibration plan (before any JA set is called *trusted*):
1. Hand-label a calibration set per question: 40 items, half positive, half negative, including adversarial ones (prompt with an added instruction, dropped constraint, wrong repo; an error message naming the wrong cause; a preview cut mid-number). Labels by two independent reviewers; disagreements resolved before the run.
2. Run Jev live 3× on the set. Promote a question to **trusted** only at accuracy ≥ 0.95 and run-to-run agreement ≥ 0.95; record the figure and date in the `PROTOCOL.md` header (same form as `answer 1.000 [calibration 2026-11-24]`).
3. Until then every JA set is **advisory** — it cannot turn the verdict RED; it can only populate the adjudication queue (items where gate and Jev disagree in every graded repeat) and make the run `PARTIAL` if the Jev pass is missing.
4. Re-calibrate on a Jev version change or any edit to a question's wording.

## 10. PR breakdown

### PR 1 — non-UI foundation (this design)

Scope: everything in §2–§8; no UI files; `PROTOCOL.md` untouched.

Acceptance criteria:
1. With `agents.enabled = false` (default) behaviour is unchanged: full existing suite green; `tests/test_agents_idle.py` proves `localflow.agents` is not imported by a dictation job or by non-agent server routes, and that no agent thread exists.
2. `mock` end-to-end over HTTP and CLI: launch → events → completed; cancel; message; resume-as-child; queue overflow → 429; timeout → `timed_out`; illegal transitions → 409 with `SessionConflict` semantics.
3. `claude` against `fake_claude.py`: exact argv (incl. `--resume`, headroom prefix), prompt on stdin, cwd, scrubbed env, result/usage/error mapping, process-group kill on cancel and timeout.
4. `devin` and `http` against the stub server: create/poll/message/cancel, state mapping, error mapping, origin-bound secret, profile validation.
5. Hot commands: grammar, every false-positive rule in §5.1, conflict detection, `_process_job` seam (no paste, history row, non-blocking handoff with the server up and down).
6. Every route in §6 implemented with the stated codes; all responses validate against `docs/contracts/agents-api.v1.schema.json`; auth/Host/local-only enforced; long-poll and `changes` semantics as specified.
7. No planted secret in any response, store, event log, or captured log (`tests/test_agents_redact.py`); `config.toml` never contains a key.
8. Config: all `[agents]` keys round-trip through load/save/`apply_config_update`; invalid values rejected; `restart_required == []` for agents keys; the three local-only keys guarded.
9. Bounds: output cap + single `truncated` marker, retention pruning, concurrency never exceeded under a threaded stress test.
10. No new dependency; `ruff check` clean.

Test commands:
```
pip install -e . ruff pytest
ruff check
pytest -q tests/test_agents_models.py tests/test_agents_store.py tests/test_agents_hotcommands.py \
          tests/test_agents_manager.py tests/test_agents_provider_mock.py tests/test_agents_provider_claude.py \
          tests/test_agents_provider_devin.py tests/test_agents_provider_http.py tests/test_agents_api.py \
          tests/test_agents_cli.py tests/test_agents_config.py tests/test_agents_dictation_seam.py \
          tests/test_agents_idle.py tests/test_agents_redact.py
pytest -q                                   # whole suite, as CI (ci.yml) runs it
# manual smoke, sandboxed (no vendor calls):
lf config set agents.enabled true && lf config set agents.sandbox true
lf serve &                                   # then:
lf agents launch "say hello" -p claude --follow && lf agents list --json
```

### PR 2 — sim-testing + Jev suite

`evals/localflow_sim`: `agents` suite cells, gates A1–A8, JA1–JA4 inputs in `jev_inputs.py`, question handling in `jev.ts`, calibration set + results under `evals/localflow_sim/calibration/agents/`, `report.py` section, frozen “Agents” section in `PROTOCOL.md`. Accepted when a 3-repeat run is GREEN on A1–A8 at the pinned commit and the Jev pass completes (advisory or calibrated, as recorded).

### PR 3 — UI (UX/UI Lead Bot)

Built solely against §6 and the JSON Schema. Expected surfaces: sessions list + detail/log view, launch form, provider readiness & key entry, hot-command rule editor with the `/hot-commands/test` preview, settings toggles via `PUT /api/config`. Any contract gap is raised as a change to this document and the schema (additive, `api_version` unchanged) rather than worked around in the UI.

### PR 4 (optional) — Rust parity

`rust/lf-server` routes + a Rust store/manager reading the same files and locks; contract fixtures shared with `tests/test_rust_parity.py`; the `agents` sim suite gains a Rust column. Until then the Rust server returns 404 on `/api/agents/status` (§6.1).

## 11. Items to verify during implementation

1. `claude` flags: `-p` with stdin prompt, `--output-format json|stream-json` (+`--verbose`), `--resume`, `--permission-mode`, `--model`, and the JSON result field names.
2. Headroom wrapper invocation form and whether it forwards stdin unchanged.
3. Devin API version, paths, status enum values and auth header (table in §4.3).
4. Which existing `sounds` cues suit launch-accepted / launch-failed (no new assets in PR 1).
5. `live_config.ALLOWLIST` semantics for list-typed fields (`agent_repos`).
