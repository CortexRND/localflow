# lf-meetings

Optional Rust port of `localflow/meetings.py`, `system_audio.py`,
`meeting_templates.py`, and `dispatch.py`. Python remains the default. No
server, CLI, or UI integration is installed by this crate.

## Public API

- `Segment`, `format_transcript`, `has_speakers`, `neutralize`, `as_data`,
  `strip_remote_media`.
- `templates::{MeetingTemplate, Section, parse_template, load_templates,
  resolve_template}` embeds the three Python templates and overlays user files.
- `build_notes_prompt`, `build_enhance_prompt`, `user_headings`,
  `missing_user_headings`, `MeetingSummarizer::{new, chunks, summarize}`;
  exact prompt constants are in `prompts`.
- `answer_question`, `CHAT_NO_ANSWER`, `CHAT_NOT_FOUND`,
  `read_transcript_log`, `latest_transcript_log`.
- `titles::{is_default_title, fallback_title, clean_generated_title,
  title_client, generate_title, TitleProvider, safe_title, heading_text}` and
  the Python title/name limits. Use `Duration::from_secs_f64(
  TITLE_TIMEOUT_SECONDS)` for the default watchdog. The title client snapshots
  the concrete provider without altering the summarizer's client; the title
  request runs on a thread and provider errors/timeouts use the session date.
- `writer::{ObsidianWriter, WriteNote, SavedNote, reserve}` reserves both paths
  exclusively, never replacing existing notes or logs. The caller selects
  explicit versus generated titles; explicit titles retain Python sanitizing.
- `dispatch::{WorkPrompt, parse_work_prompts, slugify, write_prompts,
  header_value, PromptQueue, StatusConflict, transition_prompt,
  approve_prompt, reject_prompt, orca_available, dispatch}`. `write_prompts`
  accepts a local `NaiveDateTime` or floating-point Unix timestamp.
  `default_prompts_dir` and `queue_path` expose Python's default locations;
  `PromptQueue::at_default_path` uses `~/.localflow/queue.json`.
- `dispatch::{CommandRunner, ProcessRunner, CommandOutput, CommandError,
  orca_available_with, dispatch_with}` provides injectable Orca subprocesses.
  Dispatch returns results without changing the queue.

All LLM completions go through `lf_providers::LlmProvider`. Tests use fake
providers/runners, temporary files and child processes, never the network.
Queue writes use an in-process mutex, POSIX sidecar flock (degrading to
unlocked writes if unavailable, as Python does), and per-process atomic
replacement. Windows, like Python without `fcntl`, uses the in-process mutex.

## Capture

- `Transcriber: Send + Sync` accepts `&[f32]` (16 kHz, mono) and an optional
  prompt, returning `anyhow::Result<String>`. No dependency on `lf-stt`.
- `AudioSource::{start, stop, label}` receives an owned-sample `AudioCallback`.
  Callbacks must deliver mono float32 samples at the session's 16 kHz rate.
- `MeetingSession::new(transcriber, shared_lock, microphone, system_audio,
  chunk_interval)` injects both sources. `start`, `drain_once`, `segments`,
  `elapsed_seconds`, `system_audio_status`, and `stop` expose the capture
  lifecycle. Each source advances its own offsets even for skipped silence,
  short chunks and failed transcriptions. `stop` drains the remainder and
  returns an ordered `Vec<Segment>`. Snapshots and buffers are mutex-protected.
- Real microphone/loopback capture is opt-in on **all** platforms:
  `cargo test -p lf-meetings --features capture`. CPAL requires ALSA development
  headers on Linux. Default workspace builds do not depend on CPAL or ALSA.
  `system_audio::CpalSource::microphone()` opens the default input;
  `open_system_audio(16000, device)` finds a configured/known loopback input.
  Device-native integer/float samples are normalized, downmixed and resampled.
- On macOS, `system_audio::ScreenCaptureKitSource` is compiled by default
  through objc2 bindings. It requests mono audio (supported rate, otherwise
  48 kHz), excludes this process, and leaves a 2×2, 1 fps video stream unread.
  Asynchronous operations have Python's 10-second timeout. Screen Recording
  permission is required. A source start failure falls back to mic-only in
  `MeetingSession`; explicit devices bypass ScreenCaptureKit.
- `MeetingWatcher` defaults to 12 seconds of busy input and 3-second polling.
  `with_probe_factory`, `with_platform_detector`, `with_notifier` and the
  detection callback allow device-free tests and caller-supplied notifications.
  The default probe is CoreAudio on macOS; other platforms record Python's
  unavailable error. `start/stop/pause/resume/dismiss` preserve watcher state
  semantics, including keeping a single poller through rapid stop/start.
  Process detection uses Python's ordered `pgrep` commands; unavailable `pgrep`
  produces `Other`. A notification transport is supplied by the application,
  not installed by this library.

Capture tests use synthetic sources and mock transcribers/probes only, never
audio hardware, real transcription or network services. macOS-only adapter
tests exercise rate selection and async wait/timeout without device capture.

## Parity checks

`tests/python_reference.py` freezes reference outputs by executing the current
Python non-capture implementation without loading audio dependencies:

```sh
.venv-tk/bin/python rust/lf-meetings/tests/python_reference.py > rust/lf-meetings/tests/python-reference.json
cd rust
cargo test -p lf-meetings
```

The fixtures check exact prompts, provider calls, templates, sanitized media,
title text, note/log/frontmatter bytes, prompt files and queue bytes. Additional
tests cover fallback, timeout, retries, collisions, thread/process locking,
compare-and-set transitions and Orca failures. Text-file newline translation
follows Python's platform behavior, including CRLF on Windows.

## Intentional API differences

- Rust uses typed inputs/results (`NaiveDateTime`, `Duration`, `WriteNote`,
  `Option`, `Result`) instead of Python's dynamic objects/exceptions. Provider
  snapshots use `Clone`/`TitleProvider`; arbitrary Python plug-ins cannot be
  loaded. Native filesystem/process/JSON parser error details differ.
- Unicode behavior follows the existing Rust compatibility helpers and Rust
  Unicode tables, whose version may differ from the running Python.
- Session transcription is fixed to 16 kHz; source adapters resample. Rust
  snapshots are owned copies. `Drop` stops live capture/watchers, and stopping
  the microphone logs rather than propagating device shutdown errors. Watcher
  notification delivery is injected. The CoreAudio probe preserves Python's
  `_RUNNING_SOMEWHERE` selector (`goin`, named `DeviceIsRunning` in the SDK).

No `lf-core` or `lf-providers` additions are required. macOS/Windows runtime
verification must run on those platforms; no UI verification is applicable.
