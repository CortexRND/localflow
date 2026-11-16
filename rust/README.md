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
| `lf-input` | global hotkeys (`KeyboardHub`, pynput-compatible key names, order-insensitive chords, macOS CGEventTap with the Input Monitoring checks) and paste/clipboard (`Injector`) |
| `lf-runtime` | the dictation controller state machine, worker and text pipeline, live config reload, the `dictation_enabled` switch, menu bar model + macOS NSStatusItem, single-instance lock; parity driver `lf-runtime/examples/runtime_parity.rs` |

### Dictation runtime (`lf-input`, `lf-runtime`)

A library port of `hotkey.py`, `pushtotalk.py`, `inject.py`, `app.py`
(worker, `LiveSettings`, `_process_clip`/`_process_job`), `live_config.py`,
`menubar.py`/`menubar_poll.py`, `transform.py` and `singleinstance.py`.
Audio and speech-to-text come in through the `AudioSource` and
`SpeechEngine` traits (`lf-runtime/src/engine.rs`) so the `lf-audio` and
`lf-stt` crates can be plugged in; `lf_runtime::run::run` wires everything,
and no binary ships it yet.

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

## Build

Requires a stable Rust toolchain (`rustup`, edition 2021).

```sh
cd rust
cargo build --bin lf-rs          # target/debug/lf-rs
cargo build --release --bin lf-rs
```

No system development packages are needed: on Linux the Secret Service
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

## Known gaps (Stage 1)

This is not a full port and does not imply a Rust runtime or UI:

* no audio capture, speech-to-text, meetings, server, Hub, setup or
  settings windows; the dictation runtime (`lf-runtime`) is a library with
  no `dictate` binary yet;
* hotkeys and the menu bar are implemented on macOS only: on Linux and
  Windows `lf_input::listener::run_forever` returns `Unsupported`, and no
  tray icon is drawn. Paste works everywhere (CGEvent Cmd+V or `osascript`
  on macOS, `wtype`/`xdotool` on Linux, `SendInput` on Windows; Linux
  `pynput` paste is not ported). Keys are mapped by US-layout keycode;
* not ported from the runtime: pausing media while dictating, the
  achievement cue, speech-model leases and explicit speech retry requests,
  and the frontmost-app AppleScript fallback;
* audio: `lf-core`'s `HistoryStore` has the Python clip helpers —
  `write_clip` (linear resample to private 16 kHz mono 16-bit PCM WAV with
  Python `wave`'s header), `load_clip` (PCM / extensible-PCM WAV inside the
  clips dir only), `prune_audio_clips` and `delete_clip` — but nothing in the
  port records audio, `lf-rs` exposes no clip command, and there is no
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
