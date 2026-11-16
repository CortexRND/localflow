# lf-audio

Standalone optional audio capture; not wired to the CLI or Python runtime.
`Recorder::new(configured_input_device)` accepts `Option<String>` from
`dictation.input_device`; None/empty selects the default, unknown names fail.
`start()` errors if already recording. `stop()` is fallible, joins the owner
thread, closes the device and returns the full 16 kHz mono f32 clip; idle stop
returns an empty clip. Drop also closes and joins. cpal's stream never leaves
its owner thread (including on macOS).

The native default input rate/format is used, channel zero is converted with
cpal/dasp, and rubato's anti-aliased FFT resampler runs at stop, outside the
real-time callback. Its delay is trimmed and tail flushed; output length is
floor(native frames * 16000 / native rate). Native samples are retained until
stop, so memory grows with clip length. Callback chunk boundaries do not
affect output. `level_dbfs()` reads the latest native mono block's f64 RMS
through an atomic without locking. Runtime device errors are logged; cpal
does not recover disconnected devices automatically.

Gates are explicit helpers (`is_silent`, `clip_too_short`) rather than silently
dropping recorder output. Defaults are -50 dBFS and 0.3 s; force bypasses only
the duration gate. No STT, push-to-talk controller, device hotplug recovery,
or max-duration cap is provided. Only device names/duration/peak/errors are
logged, never PCM.

Run pure tests with `cargo test -p lf-audio`. On a machine with a microphone:
`cargo test -p lf-audio -- --ignored`. Hardware tests are not run in CI.
