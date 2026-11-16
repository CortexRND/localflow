# lf-stt

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
Metal for Whisper and CoreML for Parakeet. Device overrides are auto/cpu/metal
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
