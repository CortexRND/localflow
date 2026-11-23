# Rust MCP server (opt-in)

Run `cargo build --locked --manifest-path rust/Cargo.toml --bin localflow-mcp`
from the repository root, then point your MCP client's stdio command at the
absolute path to `rust/target/debug/localflow-mcp` (add `.exe` on Windows).
It exposes the same four read-only dictation and work-prompt tools as the
Python server, on the same local files, without a network or model call.
The Python `localflow-mcp` entry point, `lf mcp`, and installers are unchanged.

To compare both servers on isolated fixtures after building the binary, run
`python -m pytest -q tests/test_rust_mcp.py` from the repository root.

# Speech helper (Rust macOS Hub)

The Rust macOS Hub defaults to a same-executable speech child:
`lf-hub --speech-helper`. This hidden mode runs before AppKit, runtime locks,
hotkeys or server startup. Dictation, HTTP transcription and meetings share the
managed proxy; the Setup dictation trial uses its own short-lived proxy. Rust is
the default macOS runtime since v0.2.0; the Python runtime is unchanged and
remains the default on Linux and Windows. Other Rust binaries and
Linux/Windows retain their in-process engines.

Set `LOCALFLOW_SPEECH_IN_PROCESS=1` when launching the Rust Hub to restore the
in-process fallback. No shared Python config keys were added. The existing
Rust `runtime.toml` idle timeout and dictation-off eviction apply to the child.

IPC uses stdin/stdout only: a little-endian u32 JSON-header length, the header,
then the announced number of little-endian f32 mono 16 kHz samples. Headers are
bounded to 1 MiB and requests to one hour of PCM. Only speech configuration and
per-request vocabulary prompts cross this channel. Requests are serialized.
Native stdout is redirected to stderr on macOS; the protocol has a dedicated
stdout descriptor. Load waits for ready or a real model error. Failed requests
return an error with an 8 KiB stderr tail and discard the child; the next request
or load respawns it. No transcription is invented or retried behind the caller.

Eviction sends unload and reaps the child, with a two-second grace period before
killing a stuck child. Stdin stays open during graceful teardown so model leases
can be released; forced exits are cleaned by the supervisor and dead leases are
pruned during discovery. A dedicated child input reader exits on EOF even while
native loading/inference is busy, including when the parent dies. Quit/shutdown
drains meeting/server work before evicting retained engines. Speech admission
closes before shutdown waits for active calls; quit retains its 60-second cap.
Closing UI writes are acknowledged before HTTP admission stops. Ordinary HTTP
requests get four seconds before their connections and async tasks are aborted;
meeting persistence runs concurrently, so a stalled chat cannot consume its budget.
Already-running blocking providers are detached during runtime teardown rather
than allowed to delay quitting.
The existing recording/meeting/inference guards
still defer ordinary idle/off eviction; existing cancellation epochs still
discard stale results. Setup cancellation releases its proxy when the trial
returns, including failed or cancelled loads.

`cargo test -p lf-stt --features runtime --test helper_process` exercises the
same-executable protocol with a fake engine, including crash recovery and pid
reaping. macOS CI measures the actual AppKit Hub/runtime/server in helper and
in-process modes, using the fixed Parakeet int8 clip from the 6c harness.
`scripts/ci_speech_helper.py` validates the reference transcript, pid disappearance,
main/helper RSS and kernel footprint, and cold/warm/reload latency. Its explicit
`LF_HUB_SPEECH_PROFILE` / `LF_HUB_SPEECH_PCM` instrumentation requires an isolated
`LOCALFLOW_CONFIG`; the probe toggles dictation off in that isolated file.
Results and the historical 6c comparison are in `memory-measurements` artifacts.
The same CI driver also runs `--whisper`: backend `auto`, ggml `tiny.en` on CPU,
real load/transcription/reload, nonempty stable text, and PID-gone assertions
after unload and dictation-off. Its transcript is recorded independently rather
than required to match Parakeet's punctuation and spelling.

# Default macOS app packaging (v0.2.2)

Rust is the macOS installer default; Linux and Windows retain Python. From a source checkout on macOS:

```sh
./install.sh --no-launch
```

This **builds from source**, not a prebuilt download. The installer provisions
stable Rust/Cargo with rustup's minimal profile and opens Apple's Xcode Command
Line Tools installer, waiting up to 30 minutes for you to complete it. Missing
CMake is installed through Homebrew; without Homebrew, install CMake first.
Only the current Mac's
architecture is built; unset `CARGO_BUILD_TARGET`. Network access is needed for
Cargo and sherpa's native runtime download. Rust installation does not create a
Python venv or install Python dependencies. Ollama remains optional. To build
without installing or changing autostart:

```sh
bash scripts/build_rust_app.sh  # dist/LocalFlow.app
```

Like Python's `lf setup install-app`, installation uses
`~/Applications/LocalFlow.app` and `com.cortexrnd.localflow.app`. The Rust bundle
contains `Contents/MacOS/lf-hub`, sherpa/ONNX Runtime dylibs in
`Contents/Frameworks`, and the login-agent helper in `Contents/Resources`.
Build-machine rpaths are removed, dependencies are rewritten to `@rpath`, and
the executable searches `@executable_path/../Frameworks`. The builder rejects
unbundled non-system dependencies, signs dylibs and the app ad-hoc, and verifies
the signature. This is **not** Developer ID signing or notarization. The app is
a regular application (`LSUIElement=false`, matching Python); the runtime manages
its menu-bar-only state when the Hub closes. Purpose strings cover microphone,
Accessibility/paste, and keyboard/input monitoring; Python's current launcher
plist does not provide those strings. Grant the Rust app its own permissions in
System Settings → Privacy & Security; ad-hoc rebuilds may require reapproval.

## Login-agent migration and removal

Installing Rust installs/loads the shared `com.cortexrnd.localflow` LaunchAgent,
replacing a Python agent rather than installing a second label. The Rust Hub's
start-on-login control uses this same bundled helper, never a PATH Python runtime.
Each migration unloads the old agent before replacing and loading its plist.
Reinstalling an already loaded Rust target leaves that service running (and does
not kill the Hub enabling its own login agent); restart the app to use a new build.
Loading falls back from `bootstrap` to `load -w`; if both fail, installation
reports both errors and restores the prior plist, attempting to reload it.

Backups live alongside the plist in `~/Library/LaunchAgents`:
`com.cortexrnd.localflow.plist.python.bak` and `.plist.rust.bak`. Repeated Rust
installs do not overwrite the Python backup with a Rust plist. To switch back:

```sh
LOCALFLOW_RUNTIME=python ./install.sh --no-launch
```

If the active agent is Rust, this restores the previous Python plist (including
custom launchd options) when its executable still exists; otherwise it generates
one pointing at the newly installed Python venv. Rust-only bundle files and its
old signature are removed before rebuilding the Python launcher. A normal Python
installation with no Rust agent still leaves autostart unchanged. Any runtime
value other than `python` or `rust` is rejected (including an explicitly empty
value). `--no-launch` suppresses opening Setup/Hub, not restarting a migrated
login agent. Manually started copies still obey the existing runtime lock; the
installer does not kill them.

```sh
./install.sh --uninstall
```

This macOS-only removal works for either runtime, unloads/removes the shared
agent, and removes LocalFlow/LocalFlow Setup apps. It retains the checkout/venv,
settings, models, logs and agent backups. Python's existing bundle-removal CLI
is unchanged; use the command above to remove autostart as well.

Installer CI uses `LOCALFLOW_SKIP_LAUNCHCTL=1` **only for testing** so a headless
runner can verify signed bundles and the Python → Rust → Python plist round-trip
without launching TCC-protected services. Shell-helper tests separately exercise
unload/load ordering, fallback errors and rollback with a fake `launchctl`.

# Rust runtime memory settings

The Rust Hub reads `runtime.toml` beside its resolved `config.toml` (including
`LOCALFLOW_CONFIG` overrides). For a normal macOS installation this is
`~/Library/Application Support/localflow/runtime.toml`; use the same directory
as your existing config on other platforms.

```toml
[stt]
idle_timeout_seconds = 0
```

Zero disables idle eviction, matching Python, which has no STT idle-unload
setting. A positive integer unloads the speech model after that many inactive
seconds. Dictation off unloads it independently of the timeout; enabling
dictation does not load weights until speech is requested. Recording, loading,
transcription and meeting startup/recording/finalization prevent eviction.
The sidecar is checked every two seconds off the UI thread. Invalid edits retain
the previous setting; deleting it restores zero. Unknown sidecar keys are ignored
and the file is never rewritten. Python is unchanged and never reads or writes
this sidecar: Python's normal config saves discard unknown keys, so putting this
setting in the shared config would lose it.

The macOS `memory` CI job compares the pre-6c Hub against the current build on
the same runner, injecting only the identical opt-in profiling driver into the
baseline. It samples every page, Home after each page, the closed window, and a
reopened window, with runtime/server running and speech unloaded. The artifact
contains CSV/Markdown tables and per-state `vmmap --summary` / `heap -s` output.
`LF_HUB_RSS_PROFILE` is measurement-only instrumentation, unset in normal use.

Parakeet measurements use the cached native int8 and fp32 repositories and their
fixed `test_wavs/0.wav` clip. Each configuration runs in a fresh process. Exact
match is measured against the default native int8 CPU transcript, not a
human-labeled WER. Arena, memory-pattern, graph and intra/inter-op experiments
use a measurement-only C interposer compiled against the installed ONNX
Runtime's exact headers; each must verify interception on all three sessions.
It is **not** linked into production. Native production session defaults remain
unchanged; no unverified accuracy or provider claims are made. The results
artifact includes load/transcription RSS, latency, transcripts, exact matches,
unload RSS and any download/provider/diagnostic-tool errors.

Memory rows include both RSS and the kernel `phys_footprint` ledger, sampled
with the SDK's `proc_pid_rusage` API. This is the same footprint accounting used
for Activity Monitor's Memory column; shared clean framework mappings inflate
RSS without equivalently increasing this ledger. The Python comparison is the
unchanged standalone Hub UI, **without** the runtime/server included in Rust.
`scripts/ci_memory_analysis.py` reports first-Ask map and typed heap deltas.

After safe model eviction on macOS, Rust asks all malloc zones to return unused
pages with `malloc_zone_pressure_relief(NULL, 0)`. It never frees live allocations.
The benchmark measures recognizer unload, complete engine drop, and allocator
relief separately, retaining their maps/heap summaries. Sherpa's recognizer and
stream wrappers own their native handles and destroy them on drop; the native
NeMo model owns all three sessions and its environment. The measurement-only
session shim resolves the original ORT export in its loaded Mach-O image to
avoid recursive dyld interposition. A failed repaired attempt disables further
shim cases rather than repeatedly crashing or claiming options were measured.

# LocalFlow Rust workspace

This directory holds the Rust implementation of LocalFlow: the `lf-hub` macOS
app that the installer ships by default (see the packaging section above), a
small command-line client, `lf-rs`, and the supporting crates. **On macOS the
Rust app is the shipped default; the Python package remains the shipped
default on Linux and Windows**, where this workspace's CLI and libraries build
but no application installer consumes them.

| Crate | Contents |
| --- | --- |
| `lf-core` | config (v2 TOML + legacy v1 migration), API token, keyring secrets, dictionary, history (SQLite), hot phrases, spoken symbols/punctuation, styles |
| `lf-providers` | Ollama / OpenAI-compatible LLM clients, provider presets and keys, endpoint-bound key activation |
| `lf-agent` | local Ollama settings proposals, validation, preview/apply/undo and dictionary tools |
| `lf-ask` | history/vault retrieval, ranking, citations and Ask prompts |
| `lf-meetings` | meeting capture/session abstractions, notes, titles, transcript Q&A and prompt dispatch |
| `lf-meetings-backend` | optional composable meeting/Ask/prompt-review backend for `lf-server` |
| `lf` | the `lf-rs` CLI (this README's focus) and the parity test driver (`lf/examples/parity.rs`) |
| `lf-audio` | standalone microphone recorder, level meter and dictation gates; see [crate README](lf-audio/README.md) |
| `lf-stt` | Whisper/Parakeet inference, downloads, model cache protection, Python-compatible leases and lazy engine slot; [API and gaps](lf-stt/README.md) |
| `lf-input` | global hotkeys (`KeyboardHub`, pynput-compatible key names, order-insensitive chords, macOS CGEventTap with the Input Monitoring checks) and paste/clipboard (`Injector`) |
| `lf-runtime` | the dictation controller state machine, worker and text pipeline, live config reload, the `dictation_enabled` switch, menu bar model + macOS NSStatusItem, single-instance lock; parity driver `lf-runtime/examples/runtime_parity.rs` |
| `lf-server` | optional axum HTTP server, embedded web UI, auth, config/provider/key APIs and runtime backend interface (see `lf-server/README.md`) |
| `lf-ui` | native Hub pages, settings/setup flows and shared UI primitives; builds the `lf-hub` binary (AppKit on macOS) that the macOS installer ships |

### Dictation runtime (`lf-input`, `lf-runtime`)

A library port of `hotkey.py`, `pushtotalk.py`, `inject.py`, `app.py`
(worker, `LiveSettings`, `_process_clip`/`_process_job`), `live_config.py`,
`menubar.py`/`menubar_poll.py`, `transform.py` and `singleinstance.py`.
Audio and speech-to-text come in through the `AudioSource` and
`SpeechEngine` traits (`lf-runtime/src/engine.rs`). `lf-stt`'s optional `runtime`
feature supplies real lf-audio capture and SttSlot adapters, a worker factory
and `run_options` ([integration and HTTP stacking](lf-stt/README.md#dictation-runtime-optional));
`lf_runtime::run::run` wires everything, and
`lf-hub` runs it on macOS.

Everything is event-driven. The controller blocks on its channel and only
sets a deadline for the double-tap window and the max-duration timer. The
hotkey timer, the config watcher (`notify`: FSEvents/inotify/
ReadDirectoryChangesW) and the meeting poller all sleep on condvars or
channels, and the menu gets state pushed to it. `cargo run --release -p
lf-runtime --example idle -- 10` reports the idle footprint on Linux.

`dictation_enabled` (`[dictation] enabled`, default true) and
`dictation_toggle_hotkey` (`[hotkey] dictation_toggle`) match the Python
keys. While dictation is off, every gesture except paste-last is ignored and
the microphone is never opened. The toggle shortcut and the menu item flip
the key in the file in place (`toml_edit`) and reload it live.

`lf_runtime::server::DictationBackend` implements `lf_server::backend::Backend`
for `POST /transcribe` (Python's `server.transcribe`): multipart `file` and
`clean` query, ffmpeg decode to 16 kHz mono f32, the same silence gate,
pipeline and `phone` history row. Other backend routes go to its `fallback`
(`UnavailableBackend` by default). Pass it to
`ServerState::load_with_backend` together with a loaded `SpeechEngine`.

## Build

Requires a stable Rust toolchain (`rustup`, edition 2021).

```sh
cd rust
cargo build --bin lf-rs          # target/debug/lf-rs
cargo build --release --bin lf-rs
```

On Linux, install `pkg-config` and `libasound2-dev` for `lf-audio`/cpal, plus
`cmake`, a C/C++ compiler and `libclang-dev` for `lf-stt`/whisper-rs. CMake and
libclang are also needed on macOS/Windows (included on GitHub runners). The Secret Service
client vendors libdbus, the clipboard uses pure-Rust X11, and HTTPS uses
rustls.

The native GPUI desktop app preview lives in `lf-desktop/`. It's a standalone crate
outside this workspace, so the commands above don't build it. Build it with
`cargo build --manifest-path rust/lf-desktop/Cargo.toml`, or run
`scripts/run_gpui.sh --sandbox`. See `lf-desktop/README.md` and
`docs/gpui-migration.md`. It doesn't replace `lf-hub` or any launcher.

## `lf-rs` commands

Names, flags, defaults and messages follow the Python `lf` (`localflow/cli.py`).

```
lf-rs config path | show | get [SECTION.KEY] | set SECTION.KEY VALUE | migrate
lf-rs dict list | add WORD | add --from X --to Y | remove WORD_OR_FROM | import FILE.csv|FILE.json
lf-rs dictation on | off | status
lf-rs stt list [--json] | use PROVIDER [MODEL] [--device auto|cpu|cuda] | download MODEL_ID
lf-rs history [--limit N] [--search TEXT]
lf-rs history list [--limit 20] | search QUERY [--limit 100]
lf-rs history copy ID|last | raw ID|last | delete ID|last | clear [--yes] | stats
lf-rs llm list [--provider P] [--base-url URL] [--json] | use PROVIDER [--base-url URL] [--model M] | test [...]
lf-rs keys list
lf-rs status [--json]
lf-rs serve [--host HOST] [--port PORT]
lf-rs meeting start [-t TITLE] [-c CATEGORY] | stop | dismiss
lf-rs prompts list [-s STATUS] | show ID | approve ID... | reject ID...
lf-rs prompts dispatch [ID...] [--all] [--repo R] [--agent A] [--dry-run]
lf-rs recall QUESTION [--since today|yesterday|7d|YYYY-MM-DD] [--k 1-20] [--no-llm] [--json]
lf-rs agent install | uninstall | status
lf-rs setup install-app
```

* `agent` (hidden, as in Python) manages launch at login for
  `LOCALFLOW_PLATFORM` or the real OS: the freedesktop autostart entry on
  Linux, the launchd LaunchAgent on macOS (via `launchctl`, trimming its
  oversized logs) and the HKCU `Run` value on Windows. `setup install-app`
  writes `~/Applications/LocalFlow Setup.app` and `LocalFlow.app`. Both need
  the Python LocalFlow install, as Python uses `sys.executable`: the first
  interpreter (`VIRTUAL_ENV`, next to an `lf` on PATH, then `python3` and
  `python` on PATH) that has the `localflow` script next to it, or in
  `Scripts\` on Windows. `agent install` starts that script at login, also
  where Python would name a missing `localflow.exe` next to a Windows
  interpreter outside a virtualenv. `setup install-app` writes that
  interpreter where Python writes its own `sys.executable`, and also accepts
  one that only imports `localflow` (a `pip install --user`, whose scripts
  live elsewhere). Without an install they exit 1 and change nothing, so
  they never touch a native install's app bundle or login job. Bare `setup`
  and `setup uninstall-app` are not ported.

* `meeting` posts to the running server (API token read, never created);
  `prompts` works on the shared `~/.localflow/queue.json` and dispatches
  through the `orca` CLI; `recall` searches history, transcripts and notes
  locally and asks the configured LLM unless `--no-llm`. Their Rich console
  output (wrapping at `COLUMNS`, tables) is reproduced by `lf/src/richfmt.rs`;
  `prompts list/show/approve/reject` and `recall` are hidden, as in Python.

* `config get` without a key prints the full v2 TOML (`config show`).
  `config set` reads VALUE as the type of the key's current value, exactly
  like the Python CLI: `true`/`false` for booleans, Python `int()`/`float()`
  syntax for numbers, comma-separated items for lists, empty text = unset for
  the nullable `stt.language`; then the shared validation runs and the file is
  written atomically.
* `history clear` asks for confirmation unless `--yes` is given; anything but
  `y`/`yes` (including end of input) aborts with status 1.
* `history copy`/`raw` really copy to the system clipboard and fail with
  status 1 if no clipboard is available; they never claim a copy that did not
  happen.
* `status` asks the running Python server (`/api/status`, `/meeting/status`)
  with the existing API token. It only *reads* the token file and never
  creates or rotates it. With no server it reports the local config and
  `meeting.server: down`; a rejected/missing token is reported as
  `meeting.server: unauthorized`.
* `keys list` only prints masked values.
* `dictation` uses the shared enabled setting. `stt use` updates the shared STT
  settings without loading a model; `stt download` fills the Hugging Face cache
  without selecting the model. `stt list` checks Python runtime availability
  and plug-ins in the active virtual environment or the Python installation
  associated with `lf` on PATH, falling back to Python on PATH. If no Python
  installation is found, the built-in providers are listed as unavailable.
* Errors print `Error: …` to stderr and exit 1; usage errors exit 2.

## Local data compatibility and safety

`lf-rs` reads and writes the same files as Python LocalFlow, in the same
formats:

* config: `config.toml` in the platform config dir (`LOCALFLOW_CONFIG`
  overrides), legacy `~/.localflow.toml` migrated on demand, left in place;
* history: `~/.localflow/history.sqlite` (same schema/`user_version`) and
  `~/.localflow/clips/`;
* dictionary and hot phrases: `~/.localflow/dictionary.json`,
  `~/.localflow/hot_phrases.json`, with the same lock files and atomic
  replace;
* secrets: keyring service `localflow`, same account names; environment
  `LOCALFLOW_<NAME>` overrides keep priority (see *Keyring backends* below).

`~` is resolved like Python's `Path.home()`/`Path.expanduser()` on every OS
(`lf_core::home`): `HOME` (else the passwd entry) on Unix, `USERPROFILE`
(else `HOMEDRIVE`+`HOMEPATH`) on Windows, never the Windows profile known
folder; `~user`, `~/x` and, on Windows, `~\x` are expanded the same way. This
applies to `~/.localflow`, the legacy `~/.localflow.toml`, `dictation.skills_dirs`
entries and `keys` import files. The config file location follows
`LOCALFLOW_CONFIG`, then platformdirs (`~/Library/Application Support/localflow`
on macOS, `$XDG_CONFIG_HOME`/`~/.config` elsewhere, local AppData on Windows).

### Keyring backends

* macOS: the `keyring` crate's native store, generic password with
  service `localflow` and the secret name as account, which is where Python
  `keyring`'s macOS backend keeps them.
* Windows: Credential Manager generic credentials written with Python
  `WinVaultKeyring`'s target scheme (target = service, a second account for
  the same service under `account@service`, UTF-16 blobs). This layout is
  implemented from the Python source but **has not been verified against a
  real Windows credential store written by Python** here.
* Linux/BSD: the Secret Service default collection over D-Bus directly
  (`dbus-secret-service`), with Python keyring's attributes and label, because
  the `keyring` crate's default Secret Service attributes/layout differ from
  Python's and the two would not see each other's items.
* Not ported: KWallet and other Python keyring backends, and Python keyring's
  backend selection via its own config file / `PYTHON_KEYRING_BACKEND`.

Back up `~/.localflow` and your config before experimenting. Both
implementations can run against the same files, but `lf-rs` is new code.

## Tests

```sh
cd rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --bin lf-rs
cd ..
python -m pytest -q tests/test_rust_parity.py
```

`tests/test_rust_parity.py` compares the Python modules with the Rust crates
on shared fixtures (config defaults, TOML save/load, legacy migration,
dictionary, history databases written by either side, spoken symbols and
punctuation, styles, hot phrases, API token handling) and exercises `lf-rs`
end to end. It skips unless `cargo` is on `PATH` **and** a prebuilt
`target/debug/lf-rs` exists (`CARGO_TARGET_DIR` is honoured); with
`LF_RUST_PARITY_REQUIRED=1` (set in CI) a missing build is a failure. All
runs use temporary `HOME`/`USERPROFILE`/XDG/AppData directories and
`LOCALFLOW_CONFIG`, and secrets are supplied via `LOCALFLOW_*` environment
overrides, so real config, history and keychain entries are not touched.

The `rust` GitHub Actions workflow (`.github/workflows/rust.yml`) runs all of
the above on Ubuntu, macOS and Windows. The additive `lf-meetings` parity
example also compares settings-agent prompts/validation, Ask retrieval,
meeting formatting/writer bytes and Python/Rust prompt-queue round trips.

## Meetings / Ask / Settings agent

Both `lf-rs serve` and the embedded macOS `lf-hub` server wire these routes
through `lf-meetings-backend`. The headless CLI supports explicit meeting
start/stop with microphone capture and configured system audio, but does not
run the desktop meeting watcher or manage platform autostart. For HTTP-upload
dictation, `POST /transcribe` uses ffmpeg and loads the configured native speech model on
demand.
They preserve Python prompts and on-disk formats (including history SQLite,
meeting Markdown, `queue.json` and `settings-undo.json`). Providers,
transcription, audio sources and watcher notifications are injectable;
`lf-meetings` does not depend on a particular speech-to-text implementation.

Real CPAL audio capture and macOS ScreenCaptureKit/CoreAudio adapters are
available to `lf-rs serve`, which enables `lf-meetings/capture`. Building
`lf-rs` on Linux requires ALSA development headers.
macOS/Windows CI compiles the hardware paths with `capture` and runs only
device-free tests. Real microphone, loopback, ScreenCaptureKit permissions
and CoreAudio hardware behavior are not run in CI.

Intentional differences: Rust exposes typed inputs/results rather than Python
objects; native OS/process/JSON error details and Unicode-table versions may
differ. Ask's optional source filter is additive (off by default); its result
count is unsigned. Watcher notifications are delivered through callbacks.
See each crate's README for the public API and remaining platform caveats.

For a prepared synthetic history/vault, measure release retrieval with:

```sh
cargo build --release -p lf-ask --example bench_search
/usr/bin/time -v target/release/examples/bench_search HISTORY.sqlite VAULT
```

The example reports search-only milliseconds and citations; `time` adds
whole-process wall time and peak RSS, including startup.

## Standalone audio and STT

`lf-audio` ports microphone capture (native sample formats/rates, channel 0,
16 kHz f32 output), RMS metering and the Python silence/minimum-clip gates.
The stream lives and is dropped on its capture thread; stop/drop joins it.
The hardware test is opt-in: `cargo test -p lf-audio -- --ignored`.

`lf-stt` ports cache inventory/deletion protections and lease formats, and adds
native `whisper-cpp` and `parakeet-onnx` engines. Both features are **on by
default**; `--no-default-features` builds storage/lifecycle only. Whisper uses
`whisper-rs` (Metal on macOS only); Parakeet uses the first-party `sherpa-onnx`
offline transducer API and downloaded prebuilt libraries, not a local
sherpa build. HF downloads use the standard shared blobs/snapshots cache and
the same `HF_HUB_CACHE`/`HUGGINGFACE_HUB_CACHE`/`HF_HOME`/`XDG_CACHE_HOME`
precedence as Python. The factory maps Python provider IDs to the native
counterparts without changing model size. `lf-hub` uses them on macOS, and
`lf-rs serve` uses them in-process on every platform for HTTP uploads
(`POST /transcribe`) and for meeting transcription, loading the configured
model on first use. They are not wired into the Python app, and `lf-rs serve`
does not start a global (hotkey) dictation runtime.

Benchmark (downloads models and the fixed Parakeet `test_wavs/0.wav`):

```sh
cd rust
cargo run --release -p lf-stt --example stt_bench -- \
  --engines whisper-cpp,parakeet-onnx --model base.en --provider cpu --runs 10 \
  --json bench.json --table bench.md
# macOS defaults: Whisper Metal, Parakeet CPU; CoreML (unverified) is opt-in
cargo run --release -p lf-stt --example stt_bench -- --engines parakeet-onnx --provider coreml
# Optional local PCM/float WAV, language/prompt, alternate Whisper model:
cargo run --release -p lf-stt --example stt_bench -- \
  --engines whisper-cpp --model small --clip speech.wav --language en --runs 10
# Real-model tests download weights; ordinary workspace tests stay offline:
cargo test -p lf-stt --test real_models -- --ignored
```

JSON is printed on stdout; the Markdown table and transcript on stderr.
Downloads/cache lookup are measured separately from load; Parakeet load
includes its one-second-silence warm-up. RSS is process-wide before/after
load; first/median/p95 cover N full transcription calls, with median RTF.
The benchmark rejects empty transcripts and reports clip path, duration and
original rate (channel 0 and FFT resampling to 16 kHz when needed). JSON's
`effective_provider` and the table report `cpu (fallback)` after an explicit
CoreML initialization failure. A successful request is labeled
`coreml requested (unverified)` because sherpa's C API cannot reveal silent
native fallback or per-operator assignment. macOS uses upstream shared sherpa
and ONNX Runtime libraries; its static build disables CoreML entirely
([details](lf-stt/README.md)). Even a CoreML-capable runtime can place
unsupported operators on CPU. Separate benchmark processes
avoid retained allocator memory skew between engines. `rust-bench.yml` records
macOS base.en/small/Parakeet CoreML-request/CPU results and uploads JSON/tables
plus native logs. The summary distinguishes observed CPU fallback diagnostics
from their absence; absence alone is not proof of CoreML execution.

Parity gaps: Whisper uses fast deterministic greedy best-of-1, temperature 0
without beam/temperature fallback or Python faster-whisper's VAD; independent
clips don't carry context. Optional language and initial prompts are supported.
Parakeet is the int8 ONNX TDT v2 model, not MLX, and ignores prompts like Python;
it skips clips below the verified 512-sample FFT frame and warms up on load.
Transcripts/timing need not be bit-identical across models/backends. See the
[STT README](lf-stt/README.md) for storage parity and lifecycle details.

## Known gaps

Outside the shipped macOS `lf-hub` app this is not a full application port:

* `lf-hub` ships on macOS only; on Linux and Windows there is no Hub, setup or
  native settings window. `serve` hosts the web UI, core APIs, HTTP-upload
  transcription (`POST /transcribe`, via ffmpeg and the native engines;
  [integration](lf-stt/README.md#http-backend-optional)) and the meetings,
  Ask and prompt routes, including explicit meeting start/stop. It does not
  run the meeting watcher (no auto-detect or auto-start) or manage
  autostart: `POST /api/autostart` returns an unavailable-backend error and
  `GET` reports it as unavailable;
* `lf-rs` has no `dictate` command, and `lf-rs serve` does not start a
  global hotkey dictation runtime; HTTP uploads are its only dictation path;
* hotkeys and the menu bar are implemented on macOS only: on Linux and
  Windows `lf_input::listener::run_forever` returns `Unsupported`, and no
  tray icon is drawn. Paste works everywhere (CGEvent Cmd+V or `osascript`
  on macOS, `wtype`/`xdotool` on Linux, `SendInput` on Windows; Linux
  `pynput` paste is not ported). Keys are mapped by US-layout keycode, plus
  the character Shift types (as pynput reports it), so `<ctrl>++` works;
  Option never changes the key;
* not ported from the runtime: pausing media while dictating, the
  achievement cue and explicit speech retry requests,
  and the frontmost-app AppleScript fallback;
* audio: `lf-core`'s `HistoryStore` has the Python clip helpers —
  `write_clip` (linear resample to private 16 kHz mono 16-bit PCM WAV with
  Python `wave`'s header), `load_clip` (PCM / extensible-PCM WAV inside the
  clips dir only), `prune_audio_clips` and `delete_clip` — but nothing in the
  CLI records dictation clips (`serve` captures only meeting audio), `lf-rs`
  exposes no clip command, and there is no retry-from-audio
  (`lf history retry` needs speech-to-text); no other audio codecs;
* `lf-rs` implements only the commands listed above (no `dictate`,
  `watch`, `notes`, `open`, `models`, `stt`, bare `setup`, `tweak`,
  `history retry`, `keys import`, `secret`, `token`, …);
* `status` without a server cannot read the autostart state (the platform
  layer is not ported) and reports it as unknown;
* `config set` refuses non-finite floats and integers outside the 64-bit
  range (Python would write a config TOML cannot read back), and accepts
  only ASCII digits;
* `history` tables are plain text, not Rich tables;
* on X11 a copied entry survives `lf-rs` exiting only if a clipboard manager
  is running (Wayland sessions go through XWayland).

### Intentional narrow differences

These are deliberate and covered by tests; upstream error-message wording
is otherwise matched where the parity suite checks it, not exhaustively.

* invalid JSON in a dictionary import is reported with `serde_json`'s parser
  message after the same `invalid JSON: ` prefix, not CPython's `json`
  wording (other JSON parse errors likewise carry `serde_json` text);
* Unicode character classes (`\w`, `\d`, `\s`, case mapping, casefold) come
  from the Rust standard library / `regex` / `caseless` Unicode tables, whose
  version can differ from the running CPython's `unicodedata`;
* where Python crashes on text it never produces itself, Rust keeps the text:
  an out-of-range `\x00N\x00`/`\x01N\x01` placeholder injected into dictated
  text (Python `IndexError`) is left as-is, and IGNORECASE-only spellings of
  symbol words such as `daſh` (Python `KeyError`) map to the word they matched;
* a `dictation.skills_dirs` entry with an unresolvable `~user` is skipped
  (Python's `discover_commands` raises `RuntimeError`);
* LLM provider plugins registered through the `localflow.llm_providers`
  entry-point group are unavailable; only the built-in providers exist;
* config integers are limited to the signed 64-bit range (see above);
* HTTP timeouts are a connect timeout plus a whole-response timeout of the
  same length, whereas `requests` applies its timeout per socket read, so a
  slow but steadily streaming response can time out in Rust and not in Python.

The pattern matcher behind the Python `re` ports (`pycompat::re`) is a
compiled backtracking VM with an explicit heap stack, so long patterns (e.g.
command names with thousands of separators) and long inputs cannot overflow
the Rust stack. The dictionary CSV importer is a port of CPython's `_csv`
reader state machine (excel dialect), including its blank-row handling and
default 131072-character `field_size_limit`.

See the `lf-core`/`lf-providers` sources for module-level gaps.
