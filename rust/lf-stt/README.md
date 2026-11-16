# lf-stt (non-engine foundation)

No inference engines or downloads yet. `SpeechEngine` defines the load,
transcribe, unload and timing contract. `SttSlot::select(EngineSpec)` keeps
one unloaded engine, loads lazily on transcribe, and releases the previous
engine and lease immediately when the selection changes. The spec's key must
include every load-affecting option; constructors must not load weights.
Use one long-lived slot per process, matching Python's process lease. A scoped
selection lease protects loading/downloading; after successful load it is
replaced by a long-lived runtime lease. Lease-write failure refuses loading
or unloads the engine, rather than leaving unprotected weights in use.

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
