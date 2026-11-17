# lf-stt

## Dictation runtime (optional)

Enable `runtime` to use `lf_stt::runtime` with the existing `lf-runtime` library:

```rust,ignore
let config = lf_runtime::settings::load_runtime_config(None)?;
let options = lf_stt::runtime::run_options(&config, std::sync::Arc::new(|_sound| {}))?;
lf_runtime::run::run(options)?;
```

The callback is the host's sound player; the example explicitly leaves sounds
off. This adds no binary and changes no runtime/controller behavior.
`RecorderSource` implements `lf_runtime::engine::AudioSource` using lf-audio's
`Recorder`, including the configured input device and stop/drop mic release.
Construction is idle. Since `AudioSource::stop` cannot return an error, capture
failures are logged and drop the clip; `take_stop_error()` retrieves the error.

`SpeechAdapter::{from_config, from_spec}` implements the runtime's shared
`SpeechEngine` using a mutex-protected `SttSlot`. Load really loads weights
without running inference, transcription passes the dictionary prompt through,
and unload releases weights/leases while permitting reloading the snapshot.
`stt_factory()` fits `Worker::build_stt`: each call creates a frozen config
snapshot, preserving the runtime's load-new-before-unload-old and
keep-old-on-failure behavior. Independent scoped leases protect both snapshots
during reload (and any shared HTTP users); dropping the last reference releases
the slot. Access to each native engine is serialized.

Native audio and models use **16 kHz**. Config-based constructors reject other
`sample_rate` values instead of letting the runtime miscalculate duration/gates.
Set `[dictation] sample_rate = 16000`; input-device changes require rebuilding
the audio source/restarting the runtime. No microphone or real-model tests run
automatically; fakes cover snapshots, prompts, reload/failure, sharing and leases.

### Composing HTTP backends

Both `lf_runtime::server::DictationBackend` and `SttBackend<B>` handle
`/transcribe`. They can be stacked, but **the outermost backend wins** for that
route, exactly once; neither falls through after a transcription error. Other
routes and autostart status continue through the chain. Tests cover both orders.
Prefer one transcription owner rather than two model caches: use `SpeechAdapter`
as `DictationBackend::speech` to share a runtime model snapshot, or put
`SttBackend` outermost for per-request config-based model selection.
`DictationBackend` retains its existing loaded-state reporting (`true`) and
delegates provider availability to its fallback. `SttBackend` reports its own
slot state and compiled providers. Wrapping a loaded DictationBackend with an
unloaded SttBackend therefore reports the outer slot as unloaded. A runtime
adapter is a frozen model snapshot, not an automatic per-request config reload.

`lf_meetings_backend::MeetingsBackend` owns meeting, Ask and prompt-review
routes, not `/transcribe`. Either `SttBackend::new(meetings_backend)` or
`meetings_backend.with_inner(Arc::new(stt_backend))` composes without a
transcription conflict. STT still owns only `/transcribe`; all other requests
reach the next backend unchanged. MeetingsBackend delegates STT availability,
loaded state and autostart status. Tests exercise both orders with real backend
wrappers, isolated queues and fake speech engines, without starting capture or
any status poller.

## HTTP backend (optional)

Enable `server` and pass `SttBackend::new(inner_backend)` from `lf_stt::server`
to `lf_server::ServerState::load_with_backend(Arc::new(backend))`. This is an
opt-in library integration, not a change to the `lf-rs serve` default backend.
Every other route and autostart status delegates unchanged to `inner_backend`.
Provider availability follows compiled engine features; loaded state comes
from a mutex-protected `SttSlot`. Requests sharing that slot serialize model
selection/inference so a switch cannot unload an in-flight model.

`POST /transcribe` accepts multipart `file` and integer query `clean`, uses
the Python server's 25 MiB ceiling and ffmpeg input-format/protocol whitelist,
120 s decode timeout, maximum configured duration, mono mixing and 16 kHz
f32 decoding. **ffmpeg must be installed** (same dependency as Python); no
request body, recording or transcript is logged. The request's coherent config
snapshot controls the silence gate, native model selection, dictionary prompt
and replacements, hot phrases, spoken punctuation/symbols, optional configured
LLM cleanup, and local style. The `other` category is faithful to Python's
`category_for(None)`. History records phone/dictate entries (silent/ok/failed),
best-effort; phone requests do not save audio clips, matching Python.

Success is `{"text": ..., "ms": ...}`; silent input returns empty text
without loading an engine. Upload/decode errors use Python's status/detail
messages and ffmpeg's truncated last diagnostic. Unhandled inference/pipeline
failures use the host's JSON `500 {"detail":"Internal Server Error"}` envelope
(Python's unhandled 500 has a plain-text body). Multipart-parser-specific
diagnostics and unusual integer-validation inputs can differ from FastAPI;
there is no Starlette/Pydantic dependency in Rust. Speech engines retain the
decoding parity gaps documented below; no missing runtime is simulated.

Tests use fake engines, isolated storage and real ffmpeg WAV decoding:
`cargo test -p lf-stt --all-features server::tests`. Workspace CI builds/tests
all features, including this default-off adapter. No change to `lf-server`
is required.

## Engines and lifecycle

`SpeechEngine` defines the download, load,
transcribe, unload and timing contract. `SttSlot::select(EngineSpec)` keeps
one unloaded engine, loads lazily on transcribe, and releases the previous
engine and lease immediately when the selection changes. The spec's key must
include every load-affecting option; constructors must not load weights.
Use one long-lived slot per process, matching Python's process lease. A scoped
selection lease protects loading/downloading; after successful load it is
replaced by a long-lived runtime lease. Lease-write failure refuses loading
or unloads the engine, rather than leaving unprotected weights in use.

Default features `whisper` and `parakeet` add `engines::{WhisperEngine,
ParakeetEngine}`. `factory::from_config(&Config, provider_override)` creates an
unloaded `EngineSpec`: `auto`/`faster-whisper`/`mlx`/`whisper-cpp` → whisper.cpp;
`parakeet`/`parakeet-onnx` → Parakeet ONNX. Unknown providers or disabled features
error. Keys include model, language and device; leases use the native backend,
not the aliased Python backend. CPU is the non-macOS default; macOS defaults to
Metal for Whisper and CPU for Parakeet (CoreML measured slower to load and
transcribe, with higher RSS, on Apple Silicon). Device overrides are auto/cpu/metal
for Whisper and auto/cpu/coreml for Parakeet (accelerators require macOS).

Whisper uses `whisper-rs 0.16` with `log_backend` and once-installed whisper/GGML
logging hooks; print/progress/timestamp output is off. Decode defaults are
greedy best-of-1, temperature 0, no temperature fallback, up to 4 threads, no
cross-clip context: fast repeatable dictation, not Python's beam/VAD behavior.
Language None is auto (English-only `.en` weights use English, matching Python
and avoiding meaningless language detection), initial prompts are passed through, and trimmed segments
join with a space then trim, including Python's interior empty-segment behavior.
Non-finite PCM, invalid language/model/provider and NUL prompts return errors.

Parakeet uses **first-party `sherpa-onnx 2.0.1`**, whose offline API documents
Parakeet and ships matching prebuilt C libraries for Linux/macOS/Windows. This
keeps the wrapper and runtime versioned together under the actively maintained,
widely used sherpa-onnx project, rather than the independent `sherpa-rs` wrapper
with separately pinned binary releases. This is a maintenance/API choice, not
a claim that the new Rust wrapper itself has more production usage. No local
sherpa build is needed. It uses 128-bin features, greedy transducer decoding,
up to 4 CPU threads, and a 1 s silence warm-up inside load. The source MLX model
config's `preprocessor.n_fft` is 512; shorter inputs return empty without even
requiring a loaded recognizer. Prompts are ignored. CoreML creation failure
logs and retries CPU, reported as `cpu (fallback)`. Successful CoreML requests
are reported as `coreml requested (unverified)`: the sherpa C API exposes neither
session provider assignment nor silent native fallback. This is deliberately
not a hardware-utilization claim; unsupported ONNX operators may run on CPU.

Linux uses the **shared** sherpa/ONNX prebuilt libraries: linking its static
ORT with whisper.cpp produced a native regex/allocator abort, while the same
Parakeet run without Whisper linked succeeded. Shared libraries isolate those
C++ dependencies. The upstream build copies `.so` files beside Cargo outputs;
this package sets relative rpaths for its benchmark/tests. Downstream Linux
executables must likewise ship the `.so` files and set an appropriate rpath
or `LD_LIBRARY_PATH`. Windows also uses shared libraries to avoid mixing sherpa's
static MSVC runtime with whisper-rs's dynamic runtime; upstream copies the DLLs
beside Cargo executables. macOS uses upstream's **shared** prebuilt too, with
`@loader_path`/`@loader_path/..` rpaths for benchmark/tests. Ship the `.dylib`
files beside downstream executables and set the corresponding rpaths.

The v2.0.1 static macOS builds explicitly set `SHERPA_ONNX_DISABLE_COREML` in
[`cmake/onnxruntime-osx-arm64-static.cmake`](https://github.com/k2-fsa/sherpa-onnx/blob/v2.0.1/cmake/onnxruntime-osx-arm64-static.cmake#L60-L61)
(also x86_64/universal). In [`session.cc`](https://github.com/k2-fsa/sherpa-onnx/blob/v2.0.1/sherpa-onnx/csrc/session.cc#L462-L472),
CoreML requires `__APPLE__`, `ORT_API_VERSION >= 15`, and that disable macro to
be absent. Otherwise it logs the misleading "CoreML is for Apple only ..."
message and silently returns CPU session options, even on Apple Silicon.
The `shared` crate feature selects `sherpa-onnx-v2.0.1-osx-arm64-shared-lib`
(x64 on Intel), whose CMake path uses shared ONNX Runtime 1.28.2 without that
disable macro. No custom runtime build or wrapper fork is needed. The macOS
benchmark records linked libraries, the CoreML EP entry-point export and
native fallback diagnostics. Those establish capability, not per-operator
execution; provider reporting remains conservative even with this build.

`download()` resolves HF assets separately so benchmark load excludes download
and cache-lookup time; `load()` also calls it if needed and reports that time
separately in `LoadInfo`. `hf-hub 0.4.3`'s established synchronous API writes
standard HF cache layout using `model_storage::hf_hub_cache(None)` explicitly.
Keep a lease during download/load/use (automatic with `SttSlot`, scoped in the
benchmark). Models and audio are never logged by these adapters; benchmark
output intentionally includes transcript text.

For benchmark commands, metrics and parity gaps see the workspace
[README](../README.md#standalone-audio-and-stt). Real-model integration tests
are ignored by default: `cargo test -p lf-stt --test real_models -- --ignored`
downloads the models and fixed speech WAV, verifies nonempty inference and
unload. `cargo test -p lf-stt --all-targets` covers factory/short-input/segment
joining, lifecycle failures, storage, WAV/resampling and benchmark statistics.

`stt_lease::{record, live_leases, runtime_repo, selection_backend}` use the
unchanged Python JSON schema in `~/.localflow/stt-leases`. Keep the returned
`LeaseGuard` alive with the model. Drop removes it, including during unwind;
`process::exit`, aborts and SIGKILL skip destructors, but dead-pid leases are
ignored. Do not retain multiple long-lived guards: as in Python, recording
again replaces `<pid>.json`, and release checks only the pid. Scoped guards
are independent. Unix uses signal-zero probes; Windows matches Python's
successful-OpenProcess/access-denied rule (an exit-code query does not weaken
that conservative protection).

One intentional safety difference: a lease missing `repo` is unknown in Rust.
Python currently raises `KeyError` after its validation accepts the missing
key; the parity test records that bug without changing Python. Rust must keep
junk leases conservative instead of crashing or treating them as known/local.

`model_storage` ports catalog ordering, selection/active/unknown protection,
cache environment precedence, inode-deduplicated sizes and catalog-only
deletion. Functions take `lf_core::config::Config`, an injectable MLX closure,
and optional explicit leases/cache roots. Use `default_mlx_available` for
normal calls: conservatively true on macOS aarch64, false elsewhere. Rust
cannot probe Python's `mlx_whisper` import spec; true protects both MLX and
faster-whisper fallback models. Always pass a freshly read Config to deletion.

All whisper.cpp ggml sizes share `ggerganov/whisper.cpp`; a selected/active
size protects the **whole repository**, not just one file. The mapping covers
tiny/base/small/medium, their `.en` variants, large-v3 and large-v3-turbo.
There are no large-v3 `.en` models upstream. Catalog has one repository row
("Whisper Base (whisper.cpp)") because inventory/deletion are repository-level.

Parakeet ONNX is the **int8** repo
`csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8`, verified with the HF
model API. Files: `.gitattributes`, `README.md`, `encoder.int8.onnx`,
`decoder.int8.onnx`, `joiner.int8.onnx`, `tokens.txt`, `test_wavs/0.wav`.
`PARAKEET_ONNX_FILES` lists the four engine assets; no fp32 files exist there.

`examples/parity_stt.rs` accepts one JSON request array per stdin line and
flushes one response array per line. It holds lease guards until release or
EOF so Python tests can inspect live Rust leases. The Python parity fixture
builds it itself; no CI prebuild step is needed. It also exposes `lf-audio`
RMS through a dev-dependency. Run `cargo test -p lf-stt` and
`LF_RUST_PARITY_REQUIRED=1 .venv-tk/bin/python -m pytest -q tests/test_rust_parity.py`
(from the repo root after building `lf-rs`).
