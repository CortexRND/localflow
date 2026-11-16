# lf-meetings-backend

Optional `lf_server::backend::Backend` implementation for meeting templates,
status/start/notes/chat/stop/dismiss, Ask (`/api/recall`) and prompt review.
Compose with `MeetingsBackend::with_inner(Arc<dyn Backend>)`: every other method
and route, STT availability/loaded state and autostart status pass through
unchanged. The default inner is `UnavailableBackend`.

```rust,no_run
use std::sync::Arc;
use lf_meetings_backend::MeetingsBackend;
use lf_server::{router, ServerState};
# fn main() -> anyhow::Result<()> {
let backend = MeetingsBackend::new()?;
let app = router(Arc::new(ServerState::load_with_backend(Arc::new(backend))?));
# let _ = app;
# Ok(())
# }
```

Inject `CaptureFactory` with `with_capture` to supply a transcriber, microphone
and optional system audio. Without one, start returns HTTP 500 with
`{"detail":"mic open failed: transcriber unavailable"}` (Python's mic-open
failure boundary). Device factories and their session run on one dedicated
thread: CPAL/native devices need not be `Send`. HTTP threads communicate through
channels. A shared transcription mutex can be injected with
`with_transcribe_lock` for coordination with dictation. Only one session may be
starting, recording or saving at once; slow work never holds the lifecycle lock.

`ProviderFactory` defaults to lf-providers using each request's coherent config
snapshot and `CoreSecrets`; title requests get their own bounded-time client.
`with_providers` enables device/network-free tests. Summary, title, transcript,
notes, prompt files and queue formats use lf-meetings directly. Work-prompt
generation uses the unchanged Python prompt text, Fireworks configuration and
nonfatal failure behavior. No request bodies or headers are logged.

`with_queue` and `with_history_path` isolate storage in tests. Normal defaults
are Python's `~/.localflow/queue.json` and history SQLite path. An optional
`with_watcher` attaches an application-owned `MeetingWatcher` for status,
pause/resume and dismiss. The application manages watching/detection callbacks,
audio adapters, notifications and sound playback; without a watcher, enabled
watching reports `meeting watcher unavailable` rather than fabricated activity.

This is an opt-in library, not automatically installed into `lf-rs serve` or
the Python server. FastAPI validation envelopes/statuses are mirrored; malformed
JSON parser wording/offsets and native I/O errors can differ. Hardware is never
opened by tests, and no speech-to-text implementation is bundled.
