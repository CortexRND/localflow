# Optional Rust HTTP server

`cargo run -p lf -- serve` binds the configured host/port (defaults
`127.0.0.1:8756`). Override with `--host` / `--port`; Python remains the shipped
default. No installer, LaunchAgent or Python application behavior changes.

The token comes from `lf_core::apitoken::load()`. Startup never creates or
rotates it: provision it with Python's token tooling first. With no existing
token, only `/`, `/healthz` and `/favicon.ico` are public; restart after
provisioning the token. Pass `X-Localflow-Token` or the `token` query parameter;
a nonempty header takes precedence. Auth uses the decoded request path,
independent of `Host` (including hostile hosts containing `/?` or `/#`).
Rejections mirror Python: 401 on loopback Host, 403 otherwise; a valid token
works regardless of Host. There are no CORS exemptions.

The two web UI HTML files are embedded unchanged from `localflow/static/`
using `include_dir`; no runtime source-tree dependency or directory traversal.
Pages have `Cache-Control: no-store`. Browser launch codes have the same
60-second lifetime, bounded queues and explicit rotate-on-replay recovery.
The token embedded in a page is the one proved at authentication time.
`weblaunch::launch_target` writes a private, reload-safe auto-POST file; tokens
and launch codes never enter browser argv or URLs. Call it on a blocking thread.

## Core routes

| Routes | Implementation |
| --- | --- |
| `/healthz`, `/`, `/settings`, `/api/launch-code` | Health, embedded pages, browser launch codes |
| `/api/config` GET/PUT | Same config JSON, validation, atomic TOML save and restart-required field order |
| `/api/status` | Same shape; loaded/autostart fields supplied by runtime backend |
| `/api/providers/llm`, `/api/providers/stt` | Built-in metadata; STT availability supplied by runtime backend |
| `/api/models`, `/api/test/llm` | STT catalogs/aliases and real Rust LLM clients, including endpoint-bound key protection |
| `/api/secrets` GET, `/api/secrets/{name}` PUT | Boolean status only, OS keychain writes, same endpoint-bound activation slots |
| `/api/autostart` GET/POST | Status via backend; POST validates boolean then delegates |
| `/docs`, `/redoc`, `/docs/oauth2-redirect`, `/openapi.json` | Embedded token-gated FastAPI documentation/schema snapshots from the Python API at the base revision |

No changes to `lf-core` or `lf-providers` are needed.

## Runtime integration and gaps

Implement `backend::Backend`, then pass it to
`ServerState::new(config, token, Arc::new(backend))` and serve `router(state)`
with axum. Blocking storage/provider/network operations run in Tokio's blocking
pool; slow LLM calls do not hold the config lock or block health/auth requests.
The CLI uses two async workers and at most sixteen blocking workers.

The backend receives a coherent config snapshot plus the original method,
path, query, headers and body (including multipart boundaries). It owns runtime
validation/lifecycle. TODO: connect the independently ported meeting,
transcription, recall, prompts and platform crates. Registered backend routes:

* `/transcribe` (including the 25 MiB Content-Length rejection)
* `/meeting/templates`, `/meeting/status`, `/meeting/start`, `/meeting/notes`,
  `/meeting/chat`, `/meeting/stop`, `/meeting/dismiss`
* `/api/recall`, `/prompts`, `/prompts/{entry_id}/approve` and `/reject`
* `/api/autostart` POST

The default `UnavailableBackend` returns HTTP 503 with Python's HTTPException
JSON **shape**, e.g. `{"detail":"meeting backend unavailable"}`. Python has no
single universal unavailable-backend message/status: these are explicit migration
errors, not simulated successful responses. STT `available` is false, `loaded`
is false and autostart is `unavailable`. The settings agent is native-only in
this Python revision (no HTTP route); future routes should accompany its backend.

Not implemented: Python provider entry-point plugins, external config-file
watching/runtime lease eviction, or runtime route validation without a linked
backend. API PUT changes apply to subsequent core HTTP requests but do not reload
a backend's cached objects; use the current config snapshot on each request.
Documentation snapshots describe Python runtime routes without implying their
backends are linked. Malformed JSON returns HTTP 422 and a FastAPI validation
envelope, but parser offset/wording can differ. Non-upload bodies have an 8 MiB
ceiling; chunked multipart bodies have a 26 MiB ceiling (audio plus framing)
before they reach the backend.

## Verification

Run workspace fmt/clippy/tests and build `lf-rs`, then from the repo root:

```sh
python -m pytest -q -s tests/test_rust_parity.py tests/test_rust_server.py
```

The contract suite runs actual Python/uvicorn and Rust HTTP hosts on free ports
against an isolated home/config/keychain environment. It compares core responses,
page bodies, config updates/errors, provider catalogs, secret validation and auth
rejections (including hostile Host headers), and checks launch replay and startup
token stability. Backend availability differences are explicit assertions.
On Linux it prints `/proc/PID/status` idle RSS for both hosts: unloaded HTTP
servers, not the whole Python desktop or a loaded Rust STT model.
