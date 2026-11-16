# LocalFlow Rust port (Stage 1, optional)

This directory holds an **optional** Rust implementation of LocalFlow's local
data layer and a small command-line client, `lf-rs`. **The Python package
remains the default and the only shipped LocalFlow**: the app, server,
dictation runtime, hotkeys, audio, meetings and every UI are Python. Nothing in
the installers, LaunchAgents or Python CI depends on this directory.

| Crate | Contents |
| --- | --- |
| `lf-core` | config (v2 TOML + legacy v1 migration), API token, keyring secrets, dictionary, history (SQLite), hot phrases, spoken symbols/punctuation, styles |
| `lf-providers` | Ollama / OpenAI-compatible LLM clients, provider presets and keys, endpoint-bound key activation |
| `lf` | the `lf-rs` CLI (this README's focus) and the parity test driver (`lf/examples/parity.rs`) |
| `lf-audio` | standalone microphone recorder, level meter and dictation gates; see [crate README](lf-audio/README.md) |
| `lf-stt` | Whisper/Parakeet inference, downloads, model cache protection, Python-compatible leases and lazy engine slot; [API and gaps](lf-stt/README.md) |

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

## `lf-rs` commands

Names, flags, defaults and messages follow the Python `lf` (`localflow/cli.py`).

```
lf-rs config path | show | get [SECTION.KEY] | set SECTION.KEY VALUE | migrate
lf-rs dict list | add WORD | add --from X --to Y | remove WORD_OR_FROM | import FILE.csv|FILE.json
lf-rs history [--limit N] [--search TEXT]
lf-rs history list [--limit 20] | search QUERY [--limit 100]
lf-rs history copy ID|last | raw ID|last | delete ID|last | clear [--yes] | stats
lf-rs llm list [--provider P] [--base-url URL] [--json] | use PROVIDER [--base-url URL] [--model M] | test [...]
lf-rs keys list
lf-rs status [--json]
```

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
the above on Ubuntu, macOS and Windows.

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
counterparts without changing model size; the engines are not yet wired into
`lf-rs` or the Python app.

Benchmark (downloads models and the fixed Parakeet `test_wavs/0.wav`):

```sh
cd rust
cargo run --release -p lf-stt --example stt_bench -- \
  --engines whisper-cpp,parakeet-onnx --model base.en --provider cpu --runs 10 \
  --json bench.json --table bench.md
# macOS defaults: Whisper Metal, Parakeet CoreML (falls back to CPU on init error)
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
original rate (channel 0 and FFT resampling to 16 kHz when needed). Provider
names indicate initialization, not verified accelerator utilization: ONNX
Runtime can place unsupported operators on CPU. Separate benchmark processes
avoid retained allocator memory skew between engines. `rust-bench.yml` records
macOS base.en/small/Parakeet CoreML/CPU results and uploads JSON/tables.

Parity gaps: Whisper uses fast deterministic greedy best-of-1, temperature 0
without beam/temperature fallback or Python faster-whisper's VAD; independent
clips don't carry context. Optional language and initial prompts are supported.
Parakeet is the int8 ONNX TDT v2 model, not MLX, and ignores prompts like Python;
it skips clips below the verified 512-sample FFT frame and warms up on load.
Transcripts/timing need not be bit-identical across models/backends. See the
[STT README](lf-stt/README.md) for storage parity and lifecycle details.

## Known gaps (Stage 1)

This is not a full port and does not imply a Rust runtime or UI:

* no dictation runtime, integrated speech-to-text, hotkeys, paste
  injection, meetings, server, menu bar, Hub, setup or settings windows;
* audio: `lf-core`'s `HistoryStore` has the Python clip helpers —
  `write_clip` (linear resample to private 16 kHz mono 16-bit PCM WAV with
  Python `wave`'s header), `load_clip` (PCM / extensible-PCM WAV inside the
  clips dir only), `prune_audio_clips` and `delete_clip` — but nothing in the
  CLI records audio (`lf-audio` is standalone), `lf-rs` exposes no clip command, and there is no
  retry-from-audio (`lf history retry` needs speech-to-text); no other audio
  codecs;
* `lf-rs` implements only the commands listed above (no `serve`, `dictate`,
  `meeting`, `models`, `stt`, `setup`, `tweak`, `recall`, `prompts`,
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
