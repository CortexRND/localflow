# Native GPUI app: integration status and cutover plan

`rust/lf-desktop` is a native GPUI front end over LocalFlow's existing stores and
local service. This file is for developers. It records what the app does today,
what it doesn't do yet, and the checks that must pass before it can become the
default. Build and run commands are in
[`rust/lf-desktop/README.md`](../rust/lf-desktop/README.md).

**Current state: preview, opt-in, not the default.** By default `lf hub`,
`lf phrases`, `lf setup`, `lf-hub` on macOS (the AppKit Hub), the Python runtime and
all existing installers are unchanged. PR 3 added a separate, opt-in preview package
and installer (`scripts/install_gpui_preview.sh`). Scope 4a (PR 66) lets the three
`lf` window commands open it only when asked (`--gpui` or `LOCALFLOW_UI=gpui`), with
a legacy fallback, and makes `lf-hub` on Linux (which had no UI) open it. Promoting
it to the default (4b) is pending the gates below and the owner's decision.

## PR boundaries

The migration is staged so that each PR can be reviewed and reverted on its own.
Each PR leaves every existing entry point working.

| PR | Scope | Status |
|---|---|---|
| **1. Parallel GPUI front end** | Standalone crate `rust/lf-desktop` (pinned `gpui = 0.2.2`, own `Cargo.lock`, outside the `rust/` workspace). One token and component layer. Library and config workflows connected to the real stores. Ask, Meetings and Prompts over the existing local HTTP service. Isolated persistent `--sandbox`. Controller, store and editing tests. Additive `gpui.yml` CI (Linux full, macOS type-check). `scripts/run_gpui.sh` | Done |
| **2. Runtime and global-input parity** | Native setup wizard over `lf_ui::setup_flow` / `setup_backend`. Speech-model storage. Settings assistant through `settings_worker`. Key import. Test LLM. A runtime status signal (listening, input device, warnings) to replace HTTP reachability. Linux global hotkeys in the Rust engine. Single instance and `page-request.json` routing. Optional engine mode behind `lf-ui --features runtime` | Implemented; verified by unit, sandbox and CLI subprocess tests and simulated CI. Interactive and macOS runtime QA not done (UI probing on the VM was waived by the owner) |
| **3. Packaging** (PR 65, merged) | Additive preview packaging: `scripts/build_gpui_package.sh` builds a release `lf-desktop` with `--features runtime` and its bundled Sherpa-ONNX/ONNX Runtime libraries for Linux x86_64 (`ubuntu-22.04`) and macOS arm64 (`macos-latest`) and x86_64 (`macos-15-intel`), natively in `gpui-package.yml`. `scripts/verify_gpui_package.sh` checks the extracted archive (library closure, relative search paths, glibc/libstdc++ floor, ad-hoc signatures). Separate `LocalFlow Preview.app` (`com.cortexrnd.localflow.desktop-preview`, no `LocalFlowRuntime` key, no login agent). Dedicated `scripts/install_gpui_preview.sh` (checksum-verified, transactional, owns only preview paths). Tag-only publishing with its own `SHA256SUMS-gpui-preview`. `lf-desktop --speech-helper` for the macOS same-executable speech helper. Per-distribution libraries in the README | Done. Linux package built and smoke-installed on the VM; fixture tests for builder/installer/publisher. All three `gpui-package.yml` legs (Linux x86_64, macOS arm64, macOS x86_64) build the release package, verify it, re-verify it outside the checkout and smoke-install/uninstall it in a temporary HOME, running `lf-desktop --version`; macOS relocation and signing are verified only by those CI legs. No interactive GUI, permission or Gatekeeper QA. `install.sh` and every launcher default unchanged |
| **4a. Opt-in launch** (PR 66, merged) | `localflow/desktop_launch.py`, called only by `lf hub`, bare `lf phrases` and the `lf setup` window: `--gpui` / `--legacy`, then `LOCALFLOW_UI=gpui\|legacy`, then `DEFAULT_UI = "legacy"` (no config key). Discovery only after a GPUI selection: absolute `LOCALFLOW_DESKTOP_BIN`, else only the preview installer's package (manifest name / bundle id checked). Bounded `--help` probe for the `--page`/`--setup` interface and the `EXIT STATUS (launch-status: 1):` line; `lf-desktop` now exits 1 when no window can open (it used to exit 0), so Scope 3 packages, which lack the line, must be updated before `lf` or `lf-hub` will open them. Closed argv, detached, 1.5 s startup watch; exit 0 is a hand-off. Any failure: one notice, then the legacy window once. `LOCALFLOW_SANDBOX` with a GPUI selection is refused (no fallback). `lf-hub` on Linux `exec`s the preview through the same rules (`lf_ui::desktop_launch`). Details: [`rust/lf-desktop/README.md`](../rust/lf-desktop/README.md#opening-it-from-lf-step-4a) | Implemented; unit and fake-executable subprocess tests (Python and Rust, temporary HOME, no GUI). Not exercised against a real window or on macOS hardware (GUI probing waived by the owner) |
| 4b. Default-launch promotion | `DEFAULT_UI` becomes `gpui` (per platform), keeping the 4a fallback and `--legacy` | Not started. Needs the open gates below (1 parity, 5 packaging QA, 8 UI verification) and the owner's decision |
| 5. Legacy retirement | Remove the launch paths that are no longer used and the controller code duplicated from `lf_ui::mac` (done on macOS). Bundle Figtree. Screen-reader support once GPUI has an accessibility tree | Not started |

## Feature matrix (after PR 2)

Legend:

- **Works**: implemented against the real store or service.
- **Honest-off**: shown with the reason it can't be used.
- **Missing**: not in this PR. The existing app is still the way to do it.

| Area | Capability | lf-desktop | Data path | Existing path still available |
|---|---|---|---|---|
| Home | Dictation on/off | Works | `[dictation] enabled` through `config_write` | `lf dictation on/off`, `lf-rs dictation on/off/status`, Hub |
| Home | Runtime state (not running, starting, shortcut listening, recording, transcribing, needs attention, not responding, running/status limited), shortcut session line, configured vs observed microphone, warnings, local service as a separate line | Works from the runtime's own `status.json` (protocol 2: listener, session, mic, warnings, 5 s heartbeat) plus PID liveness and proof the file is the current runtime's: this window's engine matches the file `instance` its live `RuntimeHandle` reports (any platform); an external runtime must hold `localflow.lock` (Linux `/proc/locks`, device + inode, read-only) with a matching lock-acquisition identity. External runtimes on macOS, where the lock owner can't be read, show as "status limited". A missing file is "not running" unless another process is proven to hold the lock ("another LocalFlow handles dictation"); an unreadable file says the status can't be read. `/healthz`, config `enabled`, a live PID alone or a spawned thread never mean "listening". Protocol-1 writers (Python, older Rust) show as "Running · status limited" | `status.json`, `localflow.lock` | Python menu bar, `lf-hub` |
| Home | Stats, recent dictations, copy | Works | `HistoryStore::stats/recent` | `lf stats`, Hub |
| Home | Setup checklist with links to the right page, **Run / Resume setup** | Works. The Shortcut step is done only when a runtime reports its listener | config, history, runtime status | `lf setup` |
| Home | In-window engine Start / Stop | Works in `--features runtime` builds launched with `--engine` (never in the sandbox, never automatically). Reuses `lf_ui::embedded::EmbeddedRuntime`; refuses only when another process is proven to hold `localflow.lock` (a recorded PID or an unreadable lock owner never blocks Start) and never stops it. Closing stops only this window's work behind a "Stopping LocalFlow…" cover that blocks input and routing, then re-checks downloads, deletions and pending writes right before closing | `EmbeddedRuntime` | `localflow`, `lf run` |
| Setup | 7-step wizard: checkup, microphone choice + level test, speech model choice/recommendation/inspection/download, cleanup, conflict-checked shortcut, bounded trial, review + apply | Works. Default build: mic test and all choices; inspection, download and trial say they need `--features runtime`. Runtime build: real. Apply writes only the setup sections under `config_write::with_lock`; start at login is left to `lf agent` | `setup_flow`, `FlowModelBackend`, `lf_audio` meter | `lf setup`, `lf-hub --setup` |
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
| Settings | Settings assistant (natural-language change): preview, explicit apply, undo that keeps newer edits, stale-preview and conflict handling | Works (native agent on explicit paths, loopback Ollama; writes through `settings_worker::dispatch`). Sandbox: native agent with a scripted offline chat | `hub_settings`, `settings_agent_native`, `settings_worker` | `lf tweak`, `lf-rs tweak` (the same `lf-agent`, default config/dictionary/undo paths), Hub |
| Settings | Run setup again; sandbox-only simulated runtime state selector | Works | — | — |
| Settings | Launch at login | Honest-off on Linux; macOS points to `lf agent` | — | `lf agent install`, `lf-rs agent install/uninstall/status` (`lf-rs agent install` never replaces a native install's agent) |
| Models | Speech engine and model size | Works (`models_layout::set_speech`; MLX refused off Apple silicon) | `[stt]` | `lf stt`, `lf-rs stt list/use`, Hub |
| Models | Speech models on this computer: installed / downloading (progress, cancel) / failed (retry) / in use; download, use, delete with confirmation | Works in `--features runtime` builds (default build explains the rebuild). Delete is refused for the selected model and for models leased by a running LocalFlow, re-checked at delete time; only managed, non-symlinked cache folders are removed | `lf_stt::model_storage`, `stt_lease`, `FlowModelBackend` | `lf setup`, `lf-rs stt download` (Python catalog), Hub |
| Models | Cleanup model: Ollama installed list, or a typed model ID | Works (background list, `cleanup_picker` states) | `[llm] model` | `lf llm`, Hub |
| Models | Provider keys: add or replace (masked, cleared after save), remove (with confirmation), use or activate | Works through `ProviderKeys` and the OS keyring. Key values are never shown or kept. Key presence is read on a worker and cached (render never touches the keyring); save, use, remove and import run on workers too | keyring + `[llm]` | `lf keys`, `lf llm use`, Hub |
| Models | Import keys from the environment or a `.env` file (selection, overwrite marker, counts) | Works. Values never shown or logged; active provider unchanged; providers overridden by `LOCALFLOW_PROVIDER_KEY_<ID>` can't be imported | `keys::detect_imports`, `ProviderKeys::save_imports` | `lf keys import`, Hub |
| Models | Test connection | Works on click only, background, 10 s timeout, model-list probe, no prompt sent, three fixed result sentences. The provider in use is probed as configured (`factory::build_llm` + `healthcheck`, keeping its endpoint and endpoint-bound key); other rows probe their preset with the saved key (`ProviderKeys::test_llm`) | provider HTTP | Hub |
| Ask | Question and answer, examples, time filter, citations to History or vault files, Stop | Works when the service is up. Offline banner and retry otherwise | `HttpAskBackend` → `/api/recall` | `lf recall`, `lf-rs recall` (local `lf_ask`, no service needed), Hub |
| Ask | Source filter | Honest-off (the adapter refuses restricted sets) | — | — |
| Meetings | Feature off: explanation and turn-on button | Works (says a restart is needed) | `[features] meetings` | `lf config` |
| Meetings | Start, notes autosave (800 ms, ordered), stop and save, chat, open saved note, recent notes | Works when the service is up | `HttpMeetingClient` → `/meeting/*`, `hub_model::recent_notes` | `lf meeting`, `lf-rs meeting start/stop/dismiss`, Hub |
| Meetings | Auto-detect on Linux | Honest-off | — | — |
| Prompts | List, filter by status, approve or reject with conflict messages, pending badge | Works when the service is up | `HttpPromptQueue` | `lf prompts`, `lf-rs prompts list/show/approve/reject`, Hub |
| Prompts | Dispatch to Orca | CLI only (the page says so) | — | `lf prompts dispatch`, `lf-rs prompts dispatch` |
| Shell | Sidebar with 10 destinations, light/dark from the system, keyboard map, toasts, confirmation dialogs, minimum window size 960 × 640 | Works | — | — |
| Shell | Narrow layout (below 1100 px): History list or detail with Back | Works (History) | — | — |
| Shell | Single instance and page routing (including Setup) | Works per mode and data folder: OS lock held for the process, `hub_route::RequestFile` hand-off, requests queued behind the overlay or a dialog; if the owner exits mid hand-off the new window consumes and routes the newest request. Own namespace (`~/.localflow/desktop/`, `<sandbox>/desktop-instance/`), so it still runs beside the legacy Hub without routing to it | `singleinstance`, `hub_route` | Hub |
| Shell | Screen-reader tree | Not possible with GPUI 0.2.2 (no accessibility API) | — | `lf-hub` on macOS |
| Dictation | Linux global hotkeys in the Rust engine | Works on X11 (listen-only XInput2 raw events via `x11rb`, no grab or extra permission; hold, toggle, chords, sides, repeat suppression, clean stop). Wayland: explicit "unsupported" with the reason. Listener readiness is published only after the platform accepted it. If the listener dies, held keys count as released: a hold recording ends and is transcribed; a toggle or command recording is discarded | `lf_input::linux` | Python runtime |
| Dictation | Windows global hotkeys in the Rust engine | Listen-only Win32 keyboard hook on the current desktop; supports hold-to-talk, chords, sided modifiers, repeat suppression, and clean stop. Printable shortcuts use virtual keys with US punctuation mappings | `lf_input::listener` | Python runtime |
| Dictation | Tray item | Works on Linux (StatusNotifierItem), Windows and macOS (menu bar item, not shown in `--sandbox`): status, Open, dictation switch, Paste last transcript (this window's engine only; on macOS refused unless, when the window handles the request, the app frontmost when it was chosen is still frontmost and isn't LocalFlow; as with the paste shortcut, focus can change after that check and before the keystroke), Quit. macOS: type-checked in CI and an earlier native probe exercised the status item and menu callbacks; this integration is not interactively verified (UI QA waived) | `lf_runtime::menubar::MenuModel`, `tray-icon` / `ksni` | Python runtime |

Wispr Flow features that LocalFlow doesn't plan (account, team sharing, sync, mobile,
cloud storage) have no placeholders in the UI. See `docs/wispr-parity.md`.

## Cutover gates (all required before 4b, the default promotion)

1. **Parity.** Every row marked Missing above that the Python Hub or `lf-hub` offers
   works in `lf-desktop`, or the owner explicitly waives it. PR 2 implemented the
   setup wizard, model storage, the settings assistant, key import, Test LLM, and
   single-instance and page routing; they still need interactive review on Linux
   (X11 and Wayland) and macOS before this gate is met, and the remaining Missing
   row (History retry) needs a decision.
2. **Data safety.** [`gpui_data_safety.py`](../scripts/gpui_data_safety.py) compares
   the real config file, `~/.localflow` and the keyring by hash before and after a
   scripted sandbox session. In real mode, every write goes through `config_write`
   or the existing store APIs. The unit tests in `data.rs`, `sandbox.rs` and
   `controllers/*` cover this already.
3. **Platforms.** The app builds and passes tests on Linux (X11 and Wayland) and
   macOS in CI. Today `gpui.yml` runs fmt, clippy, tests and the build for both the
   default and the `--features runtime` build on Linux, and type-checks both on
   macOS; `gpui-package.yml` builds, verifies and smoke-installs the release
   (runtime) package on Linux x86_64, macOS arm64 and macOS x86_64. The
   `lf-desktop` test suite does not run on macOS CI. Launching it on a machine with
   no GPU shows a clear error, not a crash.
4. **Launch integration.** `lf hub --page X` opens `lf-desktop` on page X when
   it's installed and falls back to the current UI otherwise. Two launches hand
   over the page instead of opening two windows. 4a implements this behind the
   opt-in (a hand-off exits 0 and is not treated as a failure); verified by
   fake-executable tests only, not against a real window. `lf-rs hub` follows
   the same rules with byte-identical notices (`rust/lf/src/desktop_launch.rs`,
   compared with `lf hub` in `tests/test_rust_parity.py`).
5. **Packaging.** The installers ship a release `lf-desktop` and its system-library
   requirements are documented per distribution. The `rust/` workspace CI matrix
   isn't weakened. PR 3 ships it as a separate preview package and installer;
   before PR 4 the package still needs interactive install/upgrade/uninstall
   and permission checks on each platform, and the default installers don't
   ship it yet.
6. **Honest runtime status.** Home and the sidebar report listening, input device
   and warnings from a runtime signal (PR 2: status-file protocol 2 from the Rust
   runtime). HTTP reachability alone is never shown as dictation readiness. Before
   cutover the Python runtime either writes protocol 2 or stays labelled
   "status limited".
7. **Accessibility.** Either GPUI exposes an accessibility tree and it's wired up,
   or the owner accepts keyboard-only support and `lf-hub` stays the recommended
   macOS app for screen-reader users. Taj accepted keyboard-only support for
   `lf-desktop`; the macOS screen-reader recommendation is unchanged.
8. **UI verification.** Light and dark screenshots of every page at 1280 × 850 and
   at 960 × 640 are captured from the sandbox with
   [`gpui_screenshots.py`](../scripts/gpui_screenshots.py) and reviewed.

### Run gates 2 and 8 on Linux

The Linux job in [GPUI desktop CI](../.github/workflows/gpui.yml) runs both
scripts and uploads `gpui-data-safety` and `gpui-screenshots`. CI uses an
isolated home with populated fixture stores; the local Gate 2 command below
uses your existing stores.

Build `lf-desktop` with the [GPUI build dependencies](../.github/workflows/gpui.yml).
The capture scripts use Xvfb, Openbox, ImageMagick, and the Linux Secret Service
and appearance portal. On Ubuntu/Debian, install `xvfb xauth x11-utils xdotool
openbox imagemagick dbus-x11 python3-dbus python3-gi mesa-vulkan-drivers` in addition
to the build dependencies. Use the system Python (3.10+) for the D-Bus bindings.

```bash
cargo build --manifest-path rust/lf-desktop/Cargo.toml --locked --bin lf-desktop
binary="$PWD/rust/lf-desktop/target/debug/lf-desktop"

# Gate 2: keep the current HOME, config overrides and D-Bus session.
xvfb-run -a -s '-screen 0 1600x1000x24 -dpi 96' /usr/bin/python3 \
  scripts/gpui_data_safety.py "$binary" artifacts/gpui-data-safety

# Gate 8: the private bus provides deterministic light/dark preferences.
# Disable Composite so the GPUI client pixels are capturable under Xvfb.
dbus-run-session -- xvfb-run -a -s '-screen 0 1600x1000x24 -dpi 96 -extension Composite' /usr/bin/python3 \
  scripts/gpui_screenshots.py "$binary" artifacts/gpui-screenshots
```

Use new artifact directories each run. Gate 2 requires a persistent GNOME
Keyring default collection in the current session; it fails unless the `default`
alias resolves to a collection whose `<name>.keyring` file is in the monitored
keyrings directory used by the running daemon, and it refuses to run while a legacy
`~/.gnome2/keyrings` directory exists. Run it with LocalFlow and other
writers to the monitored stores stopped. It adds a word to the **sandbox**
Dictionary and checks that the real config (including legacy fallback),
`~/.localflow`, and the GNOME Keyring backing files did not change. `before.json`
and `after.json` contain only per-store HMAC-SHA256 digests with a per-run key;
compare them only within the same run. Other keyring backends are not supported by
this script. Gate 8 writes 44 PNGs (10 pages and Setup × two themes × two sizes).
It launches `lf-desktop` with `GPUI_X11_SCALE_FACTOR=1` (which GPUI reads before
`Xft.dpi` and RandR) and fails if the process did not get it, so host HiDPI
settings cannot change the captured layout.
Review the PNGs for legibility and layout at both sizes; capture success alone
does not complete the review.

## Architecture notes for contributors

| Path | Contents |
|---|---|
| `src/data.rs` | `DataRoot` (explicit paths for every store) and `Services`. Nothing calls a default-path store constructor |
| `src/remote.rs` | `Remote::Http`, which uses the configured host and port and the token from the explicit token path, or `Remote::Sandbox` (fakes). All calls block, so they run on GPUI's background executor and results come back with a generation check |
| `src/controllers/` | Portable page logic with no GPUI types. Tested with `cargo test` and no display needed |
| `src/ui/` | The only styling layer: tokens (`theme.rs`), buttons, switches, segmented controls, selects, text input and banners. A test fails if a colour literal appears outside `theme.rs` |
| `src/pages/` | One module per destination, built only from `ui/`; `pages/setup.rs` is the setup overlay |
| `src/launch.rs`, `controllers/instance.rs` | Window lock first, then sandbox prepare/reset; page hand-off |
| `controllers/capabilities.rs` | The three launch facts (sandbox, runtime feature, `--engine`) that decide what may run |
| `controllers/runtime_state.rs`, `controllers/engine.rs` | Runtime observation → truthful status; the opt-in engine owner thread |
| `controllers/setup_flow.rs`, `jobs.rs`, `model_storage.rs`, `key_import.rs`, `assistant.rs` | Wizard state machine, shared download registry, model storage, key import / preset Test connection, settings assistant |
| `controllers/provider_ops.rs` | Models-page adapter over the existing provider APIs (no new shared API): import sessions, the one-mutation gate, editor lifetimes, Test connection targets (configured vs preset), sandbox Ollama listing and `.env` fixture |
| `rust/lf-input/src/linux.rs` | X11 raw-key source and the platform-independent translation/pump it feeds the hub with |
| `rust/lf-runtime/src/status.rs` | Status-file protocol 2 (`StatusReporter`, `StatusFile`) |
