# lf-agent

Optional Rust port of `localflow/settings_agent.py`. Python remains the default.

`SettingsAgent<C: ChatClient>` takes explicit config, dictionary and undo paths.
Use `OllamaClient::new()` for bounded HTTP, or implement `ChatClient` for tests.
The production client disables environment proxies and redirects and checks the
loopback origin again on every request. `propose` selects a local model via tags
and returns `Proposal { request, model, changes, rejected }` without writes.
`apply(&proposal.changes)` revalidates and stale-checks the complete batch before
writes. `undo` only restores still-matching values and uses lf-core's locked exact
dictionary removals. `pending_undo`, `current_settings`, `manual_settings` and
`set_setting` support consumers without model calls.

`Validator` exposes platform-specific shortcut/paste validation and safe template
IDs. `validator.templates = Some(ids)` injects choices; the default discovers
valid custom templates from config alongside `general`, `one-on-one`, `standup`.
`choose_model`, `local_ollama_url`,
`is_loopback_url`, `request_message`, and display helpers are also public.

Undo uses Python's version-1 `settings-undo.json` structure at
`default_undo_path()` (`~/.localflow/settings-undo.json`). Writes use an atomic,
fsynced private temporary file (0600 on Unix). Config and dictionary serialization
are delegated to lf-core. There are no lf-core/lf-providers additions, secret
reads, token creation, provider changes, or CLI integration.

## Compatibility notes

* `dictation_enabled` and `dictation_toggle_hotkey` validate and persist through
  lf-core as `[dictation] enabled` and `[hotkey] dictation_toggle`. Tests cover
  preview/apply/restart/undo for both directions and an empty toggle shortcut.
* Rust uses typed `Change` variants rather than Python display-bearing dataclasses;
  display fields cannot be forged independently. Errors use `anyhow::Result`,
  warnings use `Option<String>`, and native OS/network error details differ.
  `UndoResult.lines` corresponds to Python's `reverted`; `skipped` is separate.
* Prompt wording, per-setting descriptions, tool schemas, request envelope,
  previews, warnings and manual group ordering follow the dictation-toggle branch.
  `_clean_request` replaces Unicode C-category characters only, retaining other
  whitespace, exactly as Python does. JSON envelopes escape all non-ASCII text.
* Rollback/undo are best effort, like Python: config comparison is not a cross-file
  transaction. Dictionary removals are atomic compare-and-remove operations.
* macOS/Windows runtime behavior and private Windows ACLs have not been tested
  here. Unix-only permission code is cfg-gated; no desktop frameworks are loaded.

Tests use injected Ollama responses and a loopback mock HTTP server, never a
real model. They cover injection, malformed calls, request/response/tool bounds,
local endpoint/model selection, validation, previews, rollback and undo records.
Frozen fixtures in `tests/python-reference.json` were computed by running the
branch's Python modules in isolation with `tests/python_reference.py`; no
checked-out Python files were modified. They assert the complete default prompt
against lf-core's defaults, schemas, preview/warning strings,
validation failures, rejected calls, URL checks and Unicode display behavior.
