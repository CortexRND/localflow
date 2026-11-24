# Native GPUI app: integration status and cutover plan

`rust/lf-desktop` is a native GPUI front end over LocalFlow's existing stores and
local service. This file is for developers. It records what the app does today,
what it doesn't do yet, and the checks that must pass before it can become the
default. Build and run commands are in
[`rust/lf-desktop/README.md`](../rust/lf-desktop/README.md).

**Current state: preview package, preferred by `lf` when installed (4b,
conditional default).** PR 3 added a separate, opt-in preview package and
installer (`scripts/install_gpui_preview.sh`); no default installer ships it.
Scope 4a (PR 66) let the three `lf` window commands open it when asked, with a legacy
fallback, and made `lf-hub` on Linux (which had no UI) open it. Scope 4b (this
PR) makes GPUI the CLI default on Linux and macOS for configured `lf hub`,
bare `lf phrases` and `lf-rs hub` windows only, for users who have already
installed the preview package themselves (or set `LOCALFLOW_DESKTOP_BIN`) and
whose active config file carries the setup marker. `lf setup`, the Setup page and first
run stay on the legacy wizard and windows by default, even with the preview
installed, because the native wizard configures the native engines while the
Python runtime owns dictation (see the 4b row). Everyone else sees no change:
with nothing installed, on Windows, and on other platforms, those commands
open the legacy window quietly, as before. `lf-hub` on macOS (the AppKit Hub, the screen-reader
escape), `lf-hub` on Linux, the direct Python window dispatchers, the Python
runtime and the stores are unchanged, every installer's package and bundle
identities are unchanged, and no installer ships the preview. The pinned default
installer installs v0.2.3, the first release whose `lf` has this selector, as
source installs (`install.sh` from a checkout or with `LOCALFLOW_REF`, and macOS
`setup.sh`) do; their bare `lf setup`, and the Linux "LocalFlow Setup" menu entry, are setup
requests and so keep the legacy wizard. This is not a default-installer cutover and does not
retire the legacy windows.

## PR boundaries

The migration is staged so that each PR can be reviewed and reverted on its own.
Each PR leaves every existing entry point working.

| PR | Scope | Status |
|---|---|---|
| **1. Parallel GPUI front end** | Standalone crate `rust/lf-desktop` (pinned `gpui = 0.2.2`, own `Cargo.lock`, outside the `rust/` workspace). One token and component layer. Library and config workflows connected to the real stores. Ask, Meetings and Prompts over the existing local HTTP service. Isolated persistent `--sandbox`. Controller, store and editing tests. Additive `gpui.yml` CI (Linux full, macOS type-check). `scripts/run_gpui.sh` | Done |
| **2. Runtime and global-input parity** | Native setup wizard over `lf_ui::setup_flow` / `setup_backend`. Speech-model storage. Settings assistant through `settings_worker`. Key import. Test LLM. A runtime status signal (listening, input device, warnings) to replace HTTP reachability. Linux global hotkeys in the Rust engine. Single instance and `page-request.json` routing. Optional engine mode behind `lf-ui --features runtime` | Implemented; verified by unit, sandbox and CLI subprocess tests and simulated CI. Interactive and macOS runtime QA not done (UI probing on the VM was waived by the owner) |
| **3. Packaging** (PR 65, merged) | Additive preview packaging: `scripts/build_gpui_package.sh` builds a release `lf-desktop` with `--features runtime` and its bundled Sherpa-ONNX/ONNX Runtime libraries for Linux x86_64 (`ubuntu-22.04`) and macOS arm64 (`macos-latest`) and x86_64 (`macos-15-intel`), natively in `gpui-package.yml`. `scripts/verify_gpui_package.sh` checks the extracted archive (library closure, relative search paths, glibc/libstdc++ floor, ad-hoc signatures). Separate `LocalFlow Preview.app` (`com.cortexrnd.localflow.desktop-preview`, no `LocalFlowRuntime` key, no login agent). Dedicated `scripts/install_gpui_preview.sh` (checksum-verified, transactional, owns only preview paths). Tag-only publishing with its own `SHA256SUMS-gpui-preview`. `lf-desktop --speech-helper` for the macOS same-executable speech helper. Per-distribution libraries in the README | Done. Linux package built and smoke-installed on the VM; fixture tests for builder/installer/publisher. All three `gpui-package.yml` legs (Linux x86_64, macOS arm64, macOS x86_64) build the release package, verify it, re-verify it outside the checkout and smoke-install/uninstall it in a temporary HOME, running `lf-desktop --version`; macOS relocation and signing are verified only by those CI legs. The Linux x86_64 package was installed, launched, reinstalled and uninstalled by hand (gate 5), and the Windows zip was unzipped and run by hand (gate 5); macOS has no interactive, permission or Gatekeeper QA. `install.sh` and every launcher default unchanged |
| **4a. Opt-in launch** (PR 66, merged) | `localflow/desktop_launch.py`, called only by `lf hub`, bare `lf phrases` and the `lf setup` window: `--gpui` / `--legacy`, then `LOCALFLOW_UI=gpui\|legacy`, then `DEFAULT_UI = "legacy"` (no config key). Discovery only after a GPUI selection: absolute `LOCALFLOW_DESKTOP_BIN`, else only the preview installer's package (manifest name / bundle id checked). Bounded `--help` probe for the `--page`/`--setup` interface and the `EXIT STATUS (launch-status: 1):` line; `lf-desktop` now exits 1 when no window can open (it used to exit 0), so Scope 3 packages, which lack the line, must be updated before `lf` or `lf-hub` will open them. Closed argv, detached, 1.5 s startup watch; exit 0 is a hand-off. `lf hub` / `lf phrases` open the preview only once LocalFlow is set up (the active config has the setup marker), so they never show its first-run wizard; only `lf setup --gpui` does. Any failure (including not set up yet): one notice, then the legacy window once. `LOCALFLOW_SANDBOX` with a GPUI selection is refused (no fallback). `lf-hub` on Linux `exec`s the preview through the same rules (`lf_ui::desktop_launch`). Details: [`rust/lf-desktop/README.md`](../rust/lf-desktop/README.md#opening-it-from-lf-step-4a) | Implemented; unit and fake-executable subprocess tests (Python and Rust, temporary HOME, no GUI). Not exercised against a real window or on macOS hardware (GUI probing waived by the owner) |
| **4b. Default-launch promotion** (this PR, conditional default only) | `DEFAULT_UI = "gpui"` in `localflow/desktop_launch.py` and `rust/lf/src/desktop_launch.rs`; selection order unchanged (`--gpui`/`--legacy`, then `LOCALFLOW_UI`, then the default). When GPUI comes only from the default (no flag, blank or unset `LOCALFLOW_UI`), the legacy window opens quietly with no discovery, probe or sandbox check, in this order: for a setup request (`lf setup`, the Setup page; nothing is read); on an unsupported platform, or with no `LOCALFLOW_DESKTOP_BIN` and nothing at the preview package root (or no usable HOME); and on first run, i.e. no active config file with the setup marker of `main_app.is_configured` / `lf_ui::hub_route::configuration_marker` (`[setup] version > 0` or an explicit `stt.provider`/`stt.model`; only the active config file, which is all `lf-desktop` checks, not the legacy flat fallback, so a `~/.localflow.toml` the Python CLI's own config load already migrated is an active config like any other, while one whose migration was deferred or failed leaves no active file and stays on the legacy window, which handles that migration as before; missing, unreadable or malformed counts as not set up; read-only, the launcher never migrates or writes; `LOCALFLOW_CONFIG` honoured; the launcher's checks are quiet, but the Python CLI still loads config when it starts, as before, so its existing load errors and warnings come before route selection: a malformed config can error before any launch and an unversioned one can already warn), because `lf-desktop` opens its own first-run wizard even for a page. The native wizard offers and writes models the native engines can run, not necessarily the Python runtime (for example Parakeet on a source macOS install without `parakeet-mlx`), and its downloads and trial prepare whisper.cpp files rather than Python's faster-whisper model, so a passing native trial says nothing about Python dictation. Only a configured Hub / phrases window remains: anything else is a candidate and takes exactly the 4a path: a not-owned or malformed root, a missing executable, a bad override, an old (Scope 3) package, a failed probe, spawn or early exit each print the existing notice once, then open the legacy window once; exit 0 is a hand-off. `LOCALFLOW_DESKTOP_BIN` alone now takes effect on Linux and macOS. `LOCALFLOW_SANDBOX` with a candidate on a configured Hub / phrases window is refused before any probe; for setup, first run or with nothing installed it keeps its legacy behaviour. Explicit `--gpui` / `LOCALFLOW_UI=gpui` keep the exact 4a path, including its set-up check: an explicit Hub / phrases window before setup is probed, prints the not-set-up notice and opens the legacy window; only `lf setup --gpui` opens the native wizard, with the model compatibility limits above. Explicit `--gpui`, `--legacy`, `LOCALFLOW_UI` and every notice are unchanged. Not changed: `lf-hub` (macOS AppKit, Linux), the direct dispatchers, package and bundle identities, trust files, stores, engine ownership and every installer's bytes (source installs run this CLI; their bare `lf setup` is a setup request and stays legacy). Rollback: `--legacy` or `LOCALFLOW_UI=legacy` per user; setting both `DEFAULT_UI` constants back to legacy restores 4a exactly | Implemented; unit and fake-executable subprocess tests in Python and Rust, with Python/Rust parity for implicit and explicit cases, temporary HOME, no GUI. The owner's acceptance for this PR is Devin's review plus a simulated GitHub CI run; native interactive QA is waived. Gates 1, 5 and 8 are not verified (see below). Not exercised against a real window or on macOS hardware. Default installers still don't ship the preview, so this changes nothing for users who haven't installed it |
| Default-installer integration | The default installers ship and install `lf-desktop`, so the default reaches users who haven't run the preview installer | Not started. Needs gate 5 and the owner's decision on installer identities. Until then, `install.sh` (Linux) ends with `exec … lf setup`; the pinned one-liner installs the v0.2.3 release, whose `lf` has the 4b selector, as source installs (`install.sh` from a checkout or with `LOCALFLOW_REF`, macOS `setup.sh`) do, and the installer's bytes are pinned by the README checksum. None ships the preview, so without a separately installed preview `lf hub` and bare `lf phrases` open the legacy window; the bare `lf setup` and the Linux "LocalFlow Setup" menu entry `install.sh` writes (`Exec=… setup`) are setup requests, so 4b keeps them on the legacy wizard for preview owners too; no installer pin or guard is needed for that. Shipping native setup by default needs the native wizard to respect the runtime that owns dictation (its models, model files and login registration) |
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
| History | Retry transcription | Works in `--features runtime` builds for dictations with a saved 16 kHz clip: the shared `process_clip` pipeline (current config, this data root's dictionary/snippets/commands, the row's app context, fail-soft cleanup) with only the installed configured model (cache-only; never downloads; macOS keeps the speech helper via a cache-only load request old helpers reject). When that native model isn't downloaded (a Python-only install, e.g. faster-whisper in the Hugging Face cache) or whisper.cpp has no such size (e.g. `distil-large-v3`), a real window (never the sandbox) instead runs the Python `lf history retry <id> --json` of this install (`LOCALFLOW_PYTHON` from the `lf` launcher, else an `lf` on `PATH`, else `~/.local/bin/lf` on Unix; never `lf-rs`) offline (`HF_HUB_OFFLINE=1`) with this window's config and data directory; it prints the result without changing anything and the window commits it the same way. It times out, is killed and reaped on close, and with neither model the error names both ways out (Models & keys, or `lf models download`). Updates only that row with an atomic conditional update (refuses a deleted, cleared-and-reused or edited row); no paste, clipboard, `last.txt`, pruning or config write. One job per process; a close waits briefly for it and can't interrupt it. Default build explains the rebuild; the sandbox runs a labelled simulation (no model, provider or key). Native interaction and macOS are unverified | `lf_runtime::pipeline::process_clip`, `HistoryStore::update_if_unchanged`, `lf_stt::runtime::hub_speech_cached` | Python Hub |
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

**Status at 4b.** The owner chose to promote the conditional default (4b)
after closing the tray, History retry and accessibility items, with Devin's
review and a simulated GitHub CI run as the acceptance check and native
interactive QA waived. That decision does not mark the gates below met: gate 1
(interactive parity review), gate 5 (interactive install/upgrade/uninstall,
permission and Gatekeeper checks; default installers don't ship the preview)
and gate 8 (screenshot review) remain unverified. Gate 8's captures are
produced by the scripted Linux CI job, not by new interactive QA. Gate 7 is met
by the owner's acceptance of keyboard-only support. Default-installer
integration and legacy retirement still require these gates.

1. **Parity.** Every row marked Missing above that the Python Hub or `lf-hub` offers
   works in `lf-desktop`, or the owner explicitly waives it. PR 2 implemented the
   setup wizard, model storage, the settings assistant, key import, Test LLM, and
   single-instance and page routing; they need interactive review on Linux
   (X11 and Wayland) and macOS before this gate is met. No rows remain marked
   Missing: the tray item (Linux, Windows and macOS) and History retry (runtime
   builds) are implemented and reviewed as below.

   Reviewed interactively on 2026-11-24. Still open, so this gate isn't met:
   macOS has had no interactive review.
   - Linux X11: the `v0.2.3` release package (`lf-desktop 0.2.3`) on Ubuntu
     22.04 with Mesa's software Vulkan driver, in nested X11 (Xephyr, Openbox,
     1280 × 850), real mode with throwaway homes. All of these worked:
     - Setup: all seven steps and Apply on a fresh home. Four unrelated config
       values seeded beforehand, `features.meetings = true` among them, were
       unchanged. With no microphone the trial stays open and says so.
     - Model storage: Whisper Tiny download with progress, cancel, download
       again, Use, a refused delete of the model in use, and a confirmed delete.
       The stored-file card and the Delete dialog name Tiny.
     - Settings assistant, against a local stand-in for the model: "turn off
       dictation" previews "Dictation: On → Off", Apply changes only that key,
       and Undo restores the file byte for byte.
     - Key import from a `.env` by typed path (2 keys; values never shown), and
       Test connection, which sends a request only on click.
     - Single instance and page routing: a second launch exits 0 and the open
       window changes page; an unknown page shows its notice in the open
       window.
     - Tray on a KDE Plasma host: status, Open, the dictation switch, Quit.
     - History Retry with native whisper.cpp weights, with only the Python
       app's faster-whisper model (through the `v0.2.3` Python app), and with
       neither, where the row is left alone with a message. The dictation
       count follows Retry and Delete.
     - Closing from the title bar, three times: exit 0, no panic, no process
       left.
   - Linux Wayland, same package:
     - sway 1.7 (headless): first paint, three page handoffs (the unknown-page
       notice among them) and a keyboard action in setup each repainted
       without a resize. Mouse input wasn't exercised there.
     - Cage 0.1.4 (nested on X11): setup through Apply, the tray's status,
       switch and Quit, and first paint work, but new content appeared only
       after the Cage window was resized, and tray Open didn't restore a
       minimized Cage window.
     - Weston 9.0.0: `lf-desktop` panics on start with `UnsupportedVersion`
       (exit 101).
   - Windows (Server 2022), on packages built from the fixes before the
     release: setup's seven steps and Apply, Models (Whisper Tiny download,
     select, delete) and History Retry through the Python app's model, with
     native weights, and with neither. The `v0.2.3` zip's pages, close and
     second-launch hand-off are under gate 5.
   - Found and not fixed:
     - A second launch with an unknown page while setup is open leaves the
       wizard covering Home and the notice until the wizard is closed.
     - After a cancelled download, Models keeps a "0 bytes" stored-file card.
   - Not reviewed: macOS; a real microphone (the trial, dictation); a real
     model for the settings assistant; the native file picker; mouse input
     on Wayland outside Cage; an X11 desktop with a real GPU.

   Known limit, accepted by the owner (Taj, 2026-11-24, answer: "Accept as known
   limit"): on a remote X11 desktop with software rendering, `lf-desktop` can open
   with a title bar and an unpainted (blank) window body. It was seen on
   TigerVNC's X server (Xtigervnc 1.12) with KWin and Mesa's software Vulkan
   driver (llvmpipe), with compositing on or suspended. The same build renders
   under nested X11 (Xephyr), Xvfb and Wayland (Cage), and `vkcube` renders on
   that same desktop. The cause is not pinned down; GPUI picks a 32-bit ARGB
   visual there. An X11 desktop with a real GPU was not tested. This limit does
   not by itself keep gate 1 open.
2. **Data safety.** [`gpui_data_safety.py`](../scripts/gpui_data_safety.py) compares
   the whole real config directory (including `api-token` and `launch/`),
   `~/.localflow` and the keyring by hash before and after a scripted sandbox
   session that writes every sandbox store, and fails if any of those writes did not
   happen. In real mode, every write goes through `config_write`
   or the existing store APIs. The unit tests in `data.rs`, `sandbox.rs` and
   `controllers/*` cover this already.
3. **Platforms.** The app builds and passes tests on Linux (X11; Wayland is not
   exercised) and macOS in CI. Today `gpui.yml` runs fmt, clippy, tests and the
   build for both the default and the `--features runtime` build on Linux, and
   type-checks both on macOS; `gpui-package.yml` builds, verifies and
   smoke-installs the release (runtime) package on Linux x86_64, macOS arm64 and
   macOS x86_64. On macOS and Windows CI only the `lf-desktop` library unit tests
   of the default build run. Launching it on a machine with no GPU shows a clear
   error, not a crash.
4. **Launch integration.** `lf hub --page X` opens `lf-desktop` on page X when
   it's installed and falls back to the current UI otherwise. Two launches hand
   over the page instead of opening two windows. 4a implements this behind the
   opt-in (a hand-off exits 0 and is not treated as a failure); 4b applies it
   by default on Linux and macOS to a set-up Hub / phrases window when a
   candidate is installed, quietly opening the legacy window when nothing is,
   for setup and on first run. Verified by fake-executable tests only,
   not against a real window. `lf-rs hub` follows the same rules with
   byte-identical notices (`rust/lf/src/desktop_launch.rs`, compared with
   `lf hub` in `tests/test_rust_parity.py`).
5. **Packaging.** The installers ship a release `lf-desktop` and its system-library
   requirements are documented per distribution. The `rust/` workspace CI matrix
   isn't weakened. PR 3 ships it as a separate preview package and installer.

   Checked by hand on Linux x86_64 on 2026-11-24: Ubuntu, X11 under Xvfb, the
   `gpui-preview-linux-x86_64` CI artifact built from `main` at `c74fc9d` (which
   includes #108's generic-CPU whisper.cpp build), installed with
   `scripts/install_gpui_preview.sh --archive … --sha256 …` into a fresh HOME
   that already had `~/.localflow` and `config.toml`.
   - Install creates only the three documented paths:
     `~/.local/share/localflow-preview/`, the `~/.local/bin/localflow-preview`
     link and `localflow-preview.desktop`. The desktop entry passes
     `desktop-file-validate`.
   - The installed files are owned by the user. No file is setuid, setgid or
     world-writable. `ldd` resolves every library and finds Sherpa-ONNX and ONNX
     Runtime inside the package.
   - The install step opens the window on Home. The desktop entry's `Exec` opens
     it too, and with `--page History` it opens on History. Both close with
     Ctrl+Q.
   - Installing again over the existing copy works. A wrong checksum is refused,
     and so is a foreign file at an owned path. Nothing changes in either case.
   - Uninstall removes the three paths. After a `--no-launch` install and after
     its uninstall, every file under `~/.localflow` and `~/.config` has the same
     SHA-256 as before. Opening the app is not covered by that comparison: the
     launched window left `config.toml` as it was but wrote
     `~/.localflow/desktop/main-app.lock` and, in a home without one,
     `~/.localflow/history.sqlite`. Uninstall leaves both in place.

   Checked against the published release on 2026-11-24. `v0.2.3` (tag at
   `eb8b88a`) is the first release that carries the preview package, as the
   owner decided (Taj, 2026-11-24, "Tag next release").
   - The release has the four archives (`linux-x86_64`, `macos-arm64`,
     `macos-x86_64`, `windows-x86_64`) and `SHA256SUMS-gpui-preview`, and every
     archive matches its line in that file.
   - On Linux x86_64 (Ubuntu, X11 under Xvfb), in a fresh HOME that already had
     `~/.localflow` and `config.toml`, the tag's
     `scripts/install_gpui_preview.sh` run with no arguments downloaded
     `v0.2.3`, verified it and installed 0.2.3 over a 0.2.2 CI package: a real
     version-to-version upgrade. The installed `lf-desktop` has the same
     SHA-256 as the one in the release archive. The window opened on Home and
     closed with Ctrl+Q.
   - With no display, the same no-argument run installs without opening the
     app. After it and after its uninstall, every file under `~/.localflow`
     and `~/.config` has the same SHA-256 as before. Opening the window changed
     two header fields of an existing `history.sqlite` (SQLite's file change
     counter and version-valid-for number); its dump is identical.
   - `--release v0.2.2` is refused with "Release v0.2.2 has no
     SHA256SUMS-gpui-preview" and leaves the installed 0.2.3 in place. A copy
     of the installer from before `v0.2.3` stays pinned to `v0.2.2` and needs
     `--release v0.2.3`.
   - Not checked: the default download on macOS.

   Checked by hand on Windows on 2026-11-24: Windows Server 2022 (build 20348),
   `LocalFlow-Preview-0.2.3-windows-x86_64.zip` from the `v0.2.3` release,
   downloaded in Chrome and extracted with Explorer.
   - The download matches `SHA256SUMS-gpui-preview`. The folder holds
     `lf-desktop.exe`, four DLLs, `README.txt` and `MANIFEST.json`.
   - `lf-desktop.exe` is unsigned. The first double-click stops at SmartScreen's
     "Windows protected your PC"; **More info**, then **Run anyway** (publisher
     "Unknown publisher") starts it, as `README.txt` warns. The second launch
     didn't prompt.
   - It opens with no console window, on setup's Quick checkup in a profile
     with no LocalFlow config. Home, History, Settings and Models & keys open.
     The title-bar X leaves no process, tray icon or crash event.
     `lf-desktop.exe --version` prints `lf-desktop 0.2.3`.
   - A second double-click while the window is open brings that window forward
     on Home, still as one process.
   - Deleting the folder removes the app. Nothing was added to the Start Menu,
     Desktop, Startup, the Run keys, `%LOCALAPPDATA%` or `%APPDATA%`. The app
     wrote only `history.sqlite` (no rows) and two lock files in `desktop\`
     under `%USERPROFILE%\.localflow`, which stay.
   - Not checked: setup's Apply, model downloads, dictation, and other Windows
     versions or SmartScreen policies.

   Still open, so this gate isn't met:
   - macOS: CI smoke-install only, with no Gatekeeper or permission check, and
     the installer's default download wasn't run there.

   Waived by the owner for the launcher switch (Taj, 2026-11-24, "Waive for
   launcher"): the default installers (`install.sh`, `install.ps1`) still don't
   ship the preview and need not for this gate, because the launcher falls back
   to the legacy window when no preview is installed.
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
Dictionary with the keyboard in the window, then runs `lf-desktop --sandbox
--sandbox-exercise`. That makes one write through the page controllers to each other
sandbox store (a Settings change, a provider key saved to the in-memory keyring, a Hot
Phrase, a History delete and approving a pending Prompt) and reads each one back. It is
refused while that sandbox is open in a window. The script
also checks the file-backed sandbox stores itself, and fails if any write is missing.
It then checks that the whole real config directory (`config.toml`, `api-token`,
`launch/` and any other file), `LOCALFLOW_CONFIG` and the legacy config,
`~/.localflow`, and the GNOME Keyring backing files did not change. `lf-desktop`
never creates or rotates the API token, so no token write is scripted. `before.json`
and `after.json` contain only per-store HMAC-SHA256 digests with a per-run key;
compare them only within the same run. Other keyring backends are not supported by
this script. Gate 8 writes 44 PNGs (10 pages and Setup × two themes × two sizes).
It launches `lf-desktop` with `GPUI_X11_SCALE_FACTOR=1` (which GPUI reads before
`Xft.dpi` and RandR) and fails if the process did not get it, so host HiDPI
settings cannot change the captured layout. It also fails if `lf-desktop`
reports a page name it does not know (it would show Home instead) or if two
captures have identical pixels, and it writes `manifest.json` with each PNG's
page, theme, size and ImageMagick pixel signature.
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
