# localflow

Local push-to-talk dictation for macOS. Fully offline STT using Whisper (faster-whisper), optional LLM cleanup, and phone access via Tailscale.

## What it is

An open-source WisprFlow-style dictation app:
- **Hold left Option** (default) to record — release to transcribe and paste; combo hotkeys toggle instead
- **Offline transcription** via faster-whisper (no cloud calls)
- **Optional cleanup** with Ollama (fix punctuation, casing, remove filler words)
- **Instant paste** into the frontmost app
- **Server mode** for phone access: record in browser over HTTPS (Tailscale), get transcript, copy to clipboard

## Requirements

- **macOS** (10.13+)
- **Python 3.11+**
- **ffmpeg** (via Homebrew: `brew install ffmpeg`)
- **Ollama** (optional, for cleanup): `ollama pull llama3.2:3b`

## Install

```bash
git clone <repo> && cd localflow
./setup.sh
```

Or manually:
```bash
python3 -m venv .venv
.venv/bin/pip install -e .
```

## Usage

### Desktop app (local dictation)

```bash
localflow
```

- **Hold left Option**, speak, release — transcript pastes. Customize in `~/.localflow.toml`
- Text pastes into your frontmost app
- Run from terminal; watch for model download (~1s first run) and transcription timing

### macOS permissions

Grant on first run:
1. **Microphone**: System Settings → Privacy & Security → Microphone → Terminal (or your app launcher)
2. **Accessibility**: System Settings → Privacy & Security → Accessibility → Terminal (for paste injection)

### Meetings → work prompts → Orca

Meetings flow from detection to dispatched work without manual steps in between:

1. **Detect** — the watcher sees sustained mic use and (with `meeting_auto_start`) starts a session
2. **Transcribe** — audio is transcribed in chunks while the meeting runs
3. **Notes** — on stop, notes and a transcript log are written to the vault
4. **Prompts** — work prompts derived from the notes are written to `prompts_dir` and queued

Then review and dispatch them:

```bash
lf prompts list                  # queued prompts and their status
lf prompts list --status pending
lf prompts show <id>             # full prompt text
lf prompts approve <id>...       # pending -> approved
lf prompts reject <id>...        # pending -> rejected
lf prompts dispatch <id>...      # approved -> an Orca worktree per prompt
lf prompts dispatch --all --dry-run
```

Only `approved` prompts are ever dispatched — approval is the gate. Dispatch needs
`orca_repo` set and the Orca app running (`orca open`); if it isn't, nothing is
dispatched and prompts stay approved for a retry. Once dispatched,
`lf prompts show <id>` prints the worktree and the `orca terminal read` command for
following the agent.

A prompt whose dispatch failed can be retried: `lf prompts approve <id>` puts a
`failed` prompt back to `approved` and clears the previous attempt's error and
worktree. `dispatched` and `rejected` prompts cannot be re-approved.

`orca open` launches a GUI desktop app, so dispatch only works from a logged-in
desktop session — not over SSH and not from a headless cron.

The repo selector accepts `path:/abs/repo`, `name:<name>`, or `id:<repoId>`;
`path:` is the easiest to write by hand. List registered repos with
`orca repo list --json`.

### Configuration

Create `~/.localflow.toml`:

```toml
model_size = "base"               # whisper model: tiny, base, small, medium, large-v3
stt_backend = "auto"              # auto | mlx | faster-whisper
# language = "en"                 # omit for autodetect
cleanup_enabled = true
ollama_url = "http://localhost:11434"
ollama_model = "llama3.2:3b"
hotkey = "alt_l"                  # bare key name = hold-to-talk; "<cmd>+<shift>+<space>" = toggle
sample_rate = 16000
server_host = "0.0.0.0"
server_port = 8756

meeting_auto_start = true              # start transcribing when a meeting is detected
prompts_dir = "~/projs/prompts/meetings"  # where derived work prompts are written
orca_repo = "path:/Users/you/projs/myrepo"  # Orca repo selector; required for `lf prompts dispatch`
orca_agent = "claude"                  # agent to run in each dispatched worktree
```

All keys are optional; defaults shown above apply.

### Phone access (Tailscale)

#### Setup

1. On **Mac**:
   ```bash
   localflow-server
   ```
   Server starts on http://0.0.0.0:8756 (config.server_port).

2. Install **Tailscale** on Mac and phone:
   - macOS: `brew install tailscale` → `tailscale up`
   - iOS: App Store → sign in with same account

3. On **Mac**, expose the server over HTTPS:
   ```bash
   tailscale serve --bg 8756
   ```
   Tailscale prints an HTTPS URL (e.g., `https://myhost.example.ts.net`).

4. On **phone**, open that URL in browser:
   - Large record/stop button
   - Speak, hit stop
   - Transcript appears
   - Tap copy-to-clipboard

#### iOS Shortcut (alternative)

Create a Shortcut that POSTs audio to `/transcribe`:

```
1. Ask for audio
2. POST [HTTPS URL]/transcribe (multipart form: file parameter)
3. Parse JSON response: {"text": ..., "ms": ...}
4. Show text
```

## Troubleshooting

### Microphone blocked on HTTP

- **Symptom**: "NotAllowedError: Permission denied" in browser on phone
- **Fix**: Use Tailscale (HTTPS required; see Setup above). Localhost works on desktop but not from phone.

### First run is slow

- **Symptom**: 5–30s delay before recording starts
- **Cause**: faster-whisper downloads the model (~1–3 GB depending on model_size)
- **Fix**: Check console; subsequent runs are fast (~100–500ms transcription per 10s audio)

### Ollama cleanup unavailable

- **Symptom**: Text doesn't get cleaned; appears in raw transcript
- **Cause**: Ollama service not running or model not pulled
- **Fix**: 
  - `ollama serve` in another terminal
  - `ollama pull llama3.2:3b` (or your configured model)
  - If unavailable, raw transcript is returned (no error; graceful fallback)

### Paste doesn't work

- **Symptom**: Text recorded and transcribed but not pasted
- **Cause**: Missing Accessibility permission
- **Fix**: System Settings → Privacy & Security → Accessibility → add Terminal/your launcher

## Architecture

- **localflow/config.py**: Load ~/.localflow.toml (TOML parsing)
- **localflow/audio.py**: Record mono float32 via sounddevice
- **localflow/stt.py**: Transcribe via faster-whisper (CPU, int8 quantization)
- **localflow/cleanup.py**: Optional Ollama LLM polish (timeout 15s, graceful fallback)
- **localflow/inject.py**: Paste text via osascript and System Events
- **localflow/hotkey.py**: Global hotkey listener (pynput)
- **localflow/app.py**: Desktop CLI entrypoint
- **localflow/server.py**: FastAPI web server for phone access
- **localflow/static/index.html**: Single-page mobile app (no external assets)
