# lf-meetings

Optional, non-capture Rust port of `localflow/meetings.py`,
`meeting_templates.py`, and `dispatch.py`. Python remains the default. No
capture, server, CLI, or UI integration is installed by this crate.

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

All completions go through `lf_providers::LlmProvider`. Tests use fake
providers/runners, temporary files and child processes, never the network.
Queue writes use an in-process mutex, POSIX sidecar flock (degrading to
unlocked writes if unavailable, as Python does), and per-process atomic
replacement. Windows, like Python without `fcntl`, uses the in-process mutex.

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

No `lf-core` or `lf-providers` additions are required. macOS/Windows runtime
verification must run on those platforms; no UI verification is applicable.
