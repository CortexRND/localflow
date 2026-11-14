# Granola feature parity

This comparison uses the public Granola Help Center (docs.granola.ai) and LocalFlow's
code. “Have” means there is a working local equivalent, not that the implementation is
identical. Citations in the notes identify the Granola Help Center article used for each
comparison. Everything LocalFlow does here stays on the machine: audio is transcribed by
the local STT provider and every LLM call goes through `localflow/providers/`
(`build_llm`), so with the default Ollama provider nothing leaves the box.

| Granola feature | localflow status (Have / Partial / Not planned) | how to use it (UI / endpoint / config) | notes |
| --- | --- | --- | --- |
| Meeting detection and auto-start | Have | `[meetings] watch = true`, `auto_start = true` (macOS). | Sustained mic use starts a session; `localflow/meetings.py` (`MeetingWatcher`). Granola source: Taking notes → “How transcription works”. |
| Real-time transcription | Partial | Meeting panel → Start, or auto-start. | Chunked every `chunk_seconds` (default 30 s) rather than streaming word-by-word; the live log polls `/meeting/status`. `MeetingSession`. Granola source: “How transcription works”. |
| Transcribe the other side of the call (system audio) | Have | `[meetings] capture_system_audio = true`; macOS: `pip install -e '.[system-audio]'` + Screen Recording permission, or a BlackHole multi-output device via `system_audio_device`. | ScreenCaptureKit (macOS 13+) or any loopback input; mic and system audio are transcribed separately. Falls back to mic-only with a visible reason. `localflow/system_audio.py`. Granola source: “How transcription works” (system audio capture). |
| “Me” vs “Them” transcript attribution | Have | Automatic once system audio is on. | `Segment.source` is `me` (mic) or `others` (system audio); shown as **Me** / **Others** in the live log, the transcript log and the LLM prompts. Granola source: “How transcription works” (green = your mic, grey = system audio). |
| Named speaker tags (who among the others spoke) | Not planned | — | Needs diarization or call-app integrations (Granola uses Meet/Zoom/Teams speaker tags). LocalFlow only separates you from everyone else. Granola source: Taking notes → “Speaker tags”. |
| Echo handling when on speakers | Partial | Wear headphones. | No echo cancellation: on speakers the mic also hears remote audio, so it can be transcribed twice. Granola source: “How transcription works”. |
| Live notepad during the meeting | Have | Meeting panel → **Your notes** (autosaves; `POST /meeting/notes`). Notes typed before Start are sent with `/meeting/start`. | Plain Markdown textarea, not a rich-text editor. Stored in session state (`user_notes`) in `localflow/server.py`. Granola source: Taking notes → “Writing your own notes”. |
| AI-enhanced notes built on your notes | Have | Write notes, then Stop. | `MeetingSummarizer.summarize(transcript, user_notes=..., template=...)` expands the user's notes from the transcript, keeping their structure and emphasis, then adds uncovered template sections. Written to the vault by `ObsidianWriter`; raw notes are kept in the transcript log. Granola source: Taking notes → “AI-enhanced notes”. |
| Distinguishing your words from AI additions in the note | Not planned | — | Granola renders raw notes in black and AI text in grey; LocalFlow writes plain Markdown, with your raw notes preserved verbatim at the end of the transcript log instead. Granola source: “AI-enhanced notes”. |
| Templates (built-in) | Have | Meeting panel → template select, or `template` on `POST /meeting/start`; default `[meetings] default_template`. | Ships General, Standup and 1:1 (`localflow/templates/meetings/`). Granola source: Taking notes → “Customize notes with templates”. |
| Custom templates | Have | Drop `*.md` files in `[meetings] templates_dir` (`~/.localflow/meeting_templates/`). | Optional frontmatter (`name`, `description`), free-text guidance, then `## Section` headers with per-section guidance; a file named like a built-in overrides it. `localflow/meeting_templates.py`. Granola source: “Customize notes with templates” (template best practice). |
| Switch template and regenerate after the meeting | Not planned | — | The template is fixed at start. Re-running a saved transcript through another template is not exposed. Granola source: “Customize notes with templates”. |
| Chat with a meeting | Have | Meeting panel → **Ask the transcript** after a meeting is saved; `POST /meeting/chat {"question": ...}`. | Answers from the last saved transcript (or the newest transcript log in the vault after a restart) via the configured LLM; long transcripts are answered part-by-part and combined. `answer_question` in `localflow/meetings.py`. Granola source: Getting more from your notes → “Chatting with your meetings”. |
| Chat across all meetings | Not planned | — | Chat covers one transcript; there is no cross-meeting index. Granola source: “Chatting with your meetings”. |
| Recipes (saved chat prompts) | Not planned | — | Granola source: Getting more from your notes → “Recipes”. |
| Follow-ups from the meeting | Partial | `[meetings] work_prompts = true`; review with `lf prompts`. | LocalFlow turns notes into coding-agent work prompts dispatched to Orca, not follow-up emails. See README “Meetings → work prompts → Orca”. Granola source: Taking notes → “Follow-up emails”. |
| Notes stored where you control them | Have | `[meetings] vault_path`, `notes_folder`, `logs_folder`. | Markdown notes + transcript logs in an Obsidian vault on disk; no account or cloud sync. Granola source: Sharing → “Exporting notes”. |
| Calendar integration, pre-meeting briefs | Not planned | — | No calendar access; the title comes from the panel or the detected platform. Granola source: Getting started → “Syncing your calendars”; Taking notes → “Pre-meeting briefs”. |
| Sharing, workspaces, Slack/Notion/CRM integrations | Not planned | — | LocalFlow is single-user and local; share the Markdown files yourself. Granola source: Sharing → “Sharing notes”, “Integrations with Granola”. |
| Custom vocabulary | Partial | `lf dict add <word>`. | The dictation dictionary is used as the STT prompt for dictation; meeting chunks do not use it yet. Granola source: Customising Granola → “Customizing transcription”. |
| Multi-language meetings | Partial | Choose an STT model that supports the language. | Model-dependent. Granola source: Customising Granola → “Multi-language support”. |

## Notes on the implementation

- **Prompts.** With notes, the enhance prompt starts with: “The user wrote these rough
  notes during the meeting — expand and enhance them using the transcript, keeping the
  user's structure and emphasis.” Without notes, the notes prompt is built from the
  template's sections (the General template reproduces the previous hard-coded prompt).
  Long transcripts are summarized in chunks and merged with a template-aware merge prompt.
- **Privacy.** `/meeting/*` endpoints, like the rest of the web UI, are served on the
  configured `server_host`; keep it on `127.0.0.1` (or behind Tailscale) since
  `/meeting/chat` can read the last transcript.
- **Not verified on Linux CI.** ScreenCaptureKit capture needs macOS 13+ with Screen
  Recording permission; the tests cover the source-tagging, fallback and API paths with
  fakes.
