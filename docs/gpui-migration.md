# Native GPUI app: integration status and cutover plan

`rust/lf-desktop` is a native GPUI front end over LocalFlow's existing stores and
local service. This file is for developers. It records what the app does today,
what it doesn't do yet, and the checks that must pass before it can become the
default. Build and run commands are in
[`rust/lf-desktop/README.md`](../rust/lf-desktop/README.md).

**Current state: preview, not the default.** `lf hub`, `lf phrases`, `lf setup`,
`lf-hub` (the macOS AppKit Hub), the Python runtime and all installers are unchanged.
Nothing launches `lf-desktop` automatically.

## PR boundaries

The migration is staged so that each PR can be reviewed and reverted on its own.
Each PR leaves every existing entry point working.

| PR | Scope | Status |
|---|---|---|
| **1. Parallel GPUI front end** (this one) | Standalone crate `rust/lf-desktop` (pinned `gpui = 0.2.2`, own `Cargo.lock`, outside the `rust/` workspace). One token and component layer. Library and config workflows connected to the real stores. Ask, Meetings and Prompts over the existing local HTTP service. Isolated persistent `--sandbox`. Controller, store and editing tests. Additive `gpui.yml` CI (Linux full, macOS type-check). `scripts/run_gpui.sh` | Done |
| 2. Runtime and global-input parity | Native setup wizard over `lf_ui::setup_flow` / `setup_backend`. Speech-model storage. Settings assistant through `settings_worker`. Key import. Test LLM. A runtime status signal (listening, input device, warnings) to replace HTTP reachability. Linux global hotkeys in the Rust engine. Single instance and `page-request.json` routing. Optional engine mode behind `lf-ui --features runtime` | Not started |
| 3. Packaging | Installers build and ship a release `lf-desktop`. Per-distribution system libraries. macOS bundle and signing | Not started |
| 4. Opt-in, then default-launch cutover | An opt-in flag first. Then `lf hub` / `lf phrases` / `lf setup` prefer `lf-desktop`, with a fallback. `lf-hub` on Linux points to it | Not started. Needs the gates below and the owner's agreement |
| 5. Legacy retirement | Remove the launch paths that are no longer used and the controller code duplicated from `lf_ui::mac` (done on macOS). Bundle Figtree. Screen-reader support once GPUI has an accessibility tree | Not started |

## Feature matrix (PR 1)

Legend:

- **Works**: implemented against the real store or service.
- **Honest-off**: shown with the reason it can't be used.
- **Missing**: not in this PR. The existing app is still the way to do it.

| Area | Capability | lf-desktop | Data path | Existing path still available |
|---|---|---|---|---|
| Home | Dictation on/off | Works | `[dictation] enabled` through `config_write` | `lf dictation on/off`, Hub |
| Home | Config state, service reachability and input capability as three separate lines | Works. Reachability comes from `/healthz`. Input capability is always "unverified": a reachable API never counts as dictation readiness, on any platform | config + HTTP | — |
| Home | Stats, recent dictations, copy | Works | `HistoryStore::stats/recent` | `lf stats`, Hub |
| Home | Setup checklist with links to the right page | Works (read-only checks) | config, history, health | `lf setup` |
| History | Search (debounced, background), paging, details | Works | `HistoryStore::recent(limit, query, offset)` | `lf history`, Hub |
| History | Copy final or original text | Works | clipboard | Hub |
| History | Delete one, clear all (with confirmation) | Works | `HistoryStore::delete/clear` | Hub |
| History | Retry transcription | Honest-off (`HISTORY_RETRY_UNAVAILABLE`) | — | Python Hub |
| Dictionary | Add, edit, remove (with Undo) words and replacements | Works | `hub_model::submit_*` → `dictionary.json` | `lf dict`, Hub |
| Dictionary | Import CSV/JSON | Works (file picker, or a typed path when no portal is available) | `DictionaryStore::import_file` | `lf dict import` |
| Snippets | Create, edit, enable, delete with Undo, autosave, external-change reconcile, try-it preview | Works | `HotPhrasesEditor<PhrasesModel>` → `hot_phrases.json` | `lf phrases`, Hub |
| Style | Cleanup level, cleanup on/off, styles on/off, four tones | Works | `hub_model::style_update` → `[cleanup]`, `[llm] cleanup_enabled` | Hub |
| Style | Per-app category detection on Linux | Honest-off (banner: everything uses "Other") | — | — |
| Settings | Every row in `controllers::settings::SPECS` | Works with validation | `apply_config_update`. Shortcuts are normalised and conflict-checked through `lf_core::shortcuts` | `lf config`, Hub |
| Settings | Sounds and meeting detection on Linux | Honest-off | — | — |
| Settings | Data locations, what leaves the device, clear history, read-only current config | Works | — | — |
| Settings | Settings assistant (natural-language change) | Missing (PR 2) | — | `lf tweak`, Hub |
| Settings | Launch at login | Honest-off on Linux; macOS points to `lf agent` | — | `lf agent install` |
| Models | Speech engine and model size | Works (`models_layout::set_speech`; MLX refused off Apple silicon) | `[stt]` | `lf stt`, Hub |
| Models | Speech model download and delete | Missing (PR 2); the page says so | — | `lf setup`, Hub |
| Models | Cleanup model: Ollama installed list, or a typed model ID | Works (background list, `cleanup_picker` states) | `[llm] model` | `lf llm`, Hub |
| Models | Provider keys: add or replace (masked, cleared after save), remove (with confirmation), use or activate | Works through `ProviderKeys` and the OS keyring. Key values are never shown or kept | keyring + `[llm]` | `lf keys`, `lf llm use`, Hub |
| Models | Import keys from `.env` / environment, Test LLM | Missing (PR 2) | — | `lf keys import`, Hub |
| Ask | Question and answer, examples, time filter, citations to History or vault files, Stop | Works when the service is up. Offline banner and retry otherwise | `HttpAskBackend` → `/api/recall` | `lf recall`, Hub |
| Ask | Source filter | Honest-off (the adapter refuses restricted sets) | — | — |
| Meetings | Feature off: explanation and turn-on button | Works (says a restart is needed) | `[features] meetings` | `lf config` |
| Meetings | Start, notes autosave (800 ms, ordered), stop and save, chat, open saved note, recent notes | Works when the service is up | `HttpMeetingClient` → `/meeting/*`, `hub_model::recent_notes` | `lf meeting`, Hub |
| Meetings | Auto-detect on Linux | Honest-off | — | — |
| Prompts | List, filter by status, approve or reject with conflict messages, pending badge | Works when the service is up | `HttpPromptQueue` | `lf prompts`, Hub |
| Prompts | Dispatch to Orca | CLI only (the page says so) | — | `lf prompts dispatch` |
| Shell | Sidebar with 10 destinations, light/dark from the system, keyboard map, toasts, confirmation dialogs, minimum window size 960 × 640 | Works | — | — |
| Shell | Narrow layout (below 1100 px): History list or detail with Back | Works (History) | — | — |
| Shell | Single instance and `page-request.json` routing | Missing (PR 2). It runs beside the Hub without conflict | — | Hub |
| Shell | Screen-reader tree | Not possible with GPUI 0.2.2 (no accessibility API) | — | `lf-hub` on macOS |
| Dictation | In-window "Dictate here", tray, Linux global hotkeys in the Rust engine | Missing (engine work) | — | Python runtime |

Wispr Flow features that LocalFlow doesn't plan (account, team sharing, sync, mobile,
cloud storage) have no placeholders in the UI. See `docs/wispr-parity.md`.

## Cutover gates (all required before PR 4)

1. **Parity.** Every row marked Missing above that the Python Hub or `lf-hub` offers
   works in `lf-desktop`, or the owner explicitly waives it: the setup wizard,
   model storage, the settings assistant, key import, Test LLM, and single-instance
   and page routing.
2. **Data safety.** A script compares the real config file, `~/.localflow` and the
   keyring by hash before and after a scripted sandbox session, and finds them
   unchanged. In real mode, every write goes through `config_write` or the
   existing store APIs. The unit tests in `data.rs`, `sandbox.rs` and
   `controllers/*` cover this already.
3. **Platforms.** The app builds and passes tests on Linux (X11 and Wayland) and
   macOS in CI. Today `gpui.yml` runs the full suite on Linux and type-checks macOS. Launching it on a machine with no GPU shows a clear error, not a
   crash.
4. **Launch integration.** `lf hub --page X` opens `lf-desktop` on page X when
   it's installed and falls back to the current UI otherwise. Two launches hand
   over the page instead of opening two windows.
5. **Packaging.** The installers ship a release `lf-desktop` and its system-library
   requirements are documented per distribution. The `rust/` workspace CI matrix
   isn't weakened.
6. **Honest runtime status.** Home and the sidebar report listening, input device
   and warnings from a runtime signal (PR 2). HTTP reachability alone is never
   shown as dictation readiness.
7. **Accessibility.** Either GPUI exposes an accessibility tree and it's wired up,
   or the owner accepts keyboard-only support and `lf-hub` stays the recommended
   macOS app for screen-reader users.
8. **UI verification.** Light and dark screenshots of every page at 1280 × 850 and
   at 960 × 640 are captured from the sandbox and reviewed.

## Architecture notes for contributors

| Path | Contents |
|---|---|
| `src/data.rs` | `DataRoot` (explicit paths for every store) and `Services`. Nothing calls a default-path store constructor |
| `src/remote.rs` | `Remote::Http`, which uses the configured host and port and the token from the explicit token path, or `Remote::Sandbox` (fakes). All calls block, so they run on GPUI's background executor and results come back with a generation check |
| `src/controllers/` | Portable page logic with no GPUI types. Tested with `cargo test` and no display needed |
| `src/ui/` | The only styling layer: tokens (`theme.rs`), buttons, switches, segmented controls, selects, text input and banners. A test fails if a colour literal appears outside `theme.rs` |
| `src/pages/` | One module per destination, built only from `ui/` |
