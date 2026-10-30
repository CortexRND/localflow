# localflow

Local push-to-talk dictation for macOS. Fully offline STT using Whisper (faster-whisper), optional LLM cleanup, and phone access via Tailscale.

## What it is

An open-source WisprFlow-style dictation app:
- **Hold left Option** (default) to record — release to transcribe and paste; combo hotkeys toggle instead
- **Offline transcription** via faster-whisper (no cloud calls)
- **Optional cleanup** with Ollama (fix punctuation, casing, remove filler words)
- **Instant paste** into the frontmost app
- **Server mode** for phone access: record in browser over HTTPS (Tailscale), get transcript, copy to clipboard

## Getting started (launcher)

LocalFlow's native setup wizard checks your machine, helps select and download
speech and cleanup models, and configures your microphone and hotkey. Audio and
speech recognition stay local.

### Platform support

| Platform | Dictation, paste, autostart, notifications | Meetings |
| --- | --- | --- |
| macOS | Full support | Supported |
| Linux | Best-effort support | Disabled |
| Windows | Best-effort support | Disabled |

On macOS, use Python 3.11+ and Homebrew to run the installer from a checkout:

```bash
git clone https://github.com/CortexRND/localflow.git
cd localflow
./install.sh
```

Or run `install.sh --with-ollama` to install and start Ollama for optional
transcript cleanup. The installer also works from a downloaded script and
clones LocalFlow into `~/.localflow/app` (override with `LOCALFLOW_HOME`).

After installation, reopen setup to switch models at any time:

```bash
lf setup
```

For terminal-only or automated setup, use `lf setup --cli` or
`lf setup --cli --yes --stt fw-base --llm none`. `lf models list` shows model
download status, and `lf doctor` checks local prerequisites.

## Choosing models

The speech model controls transcription speed, accuracy, memory use, and
download size. The wizard recommends a model for your hardware, but you can
switch later with `lf setup`. `lf models list --kind stt` lists the catalog;
`lf models download fw-small` downloads a model and `lf models use fw-small`
selects it.

The same catalog is available through the provider commands: `lf stt list`,
`lf stt download fw-tiny`, and `lf stt use faster-whisper small`. For Ollama,
`lf llm list`, `lf llm pull llama3.2:1b`, and `lf llm use ollama --model
llama3.2:1b` manage the configured cleanup model. Non-Ollama providers are
configured and tested with `lf llm use` and `lf llm test`; their models are
managed by that provider rather than downloaded by LocalFlow.

Transcript cleanup is optional. Select an Ollama model in setup or use
`lf models list --kind llm`; cleanup models require Ollama and additional disk
space and memory. Choose `none` to keep the raw transcript and avoid the extra
model.

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

### Run on login (always on)

Install a launchd LaunchAgent so localflow starts automatically:

```bash
lf agent install
lf agent status      # running / loaded, not running / not loaded
lf agent uninstall
```

Once installed, localflow runs on every login and relaunches if it crashes;
a clean exit stays down until next login (`KeepAlive` = `SuccessfulExit:
false`) — so a manually-started instance holding the single-instance lock
can exit 0 without triggering a relaunch loop.

Grant **Microphone** and **Accessibility** permissions to the `.venv` Python
binary (the interpreter the LaunchAgent actually runs), not just Terminal —
System Settings → Privacy & Security → Microphone / Accessibility → add
`.venv/bin/python3` (or use the `+` button and navigate to it).

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

### Hot phrases

Hot phrases let you paste a saved long prompt by speaking a short trigger
mid-dictation: say "review checklist" and the saved review prompt is pasted
instead. Entries live in `~/.localflow/hot_phrases.json`; the desktop app
re-reads the file when it changes, so edits apply without a restart.

Manage them in the hot phrases window:

```bash
lf phrases                  # open the window
lf phrases install-app      # also available from the menu bar ("Hot Phrases…")
lf phrases uninstall-app
```

On macOS this is a native AppKit window: a sidebar lists your phrases
(searchable, one row each with an on/off state), edits autosave as you
type, the switch toggles a phrase without deleting it, Delete shows an
Undo bar, and a "Test a phrase" section previews what dictation would
paste — including which triggers matched. `install-app` writes
`~/Applications/LocalFlow Hot Phrases.app` so the window can be opened
from Spotlight/Launchpad too.

Elsewhere (or if PyObjC is missing on macOS) a simpler Tk window opens
as a fallback. It keeps the classic layout — an entry list plus explicit
Save/Revert buttons and a confirm dialog on delete; the sidebar,
autosave and undo design above is macOS-only. The fallback needs
tkinter (`sudo apt install python3-tk` on Linux, or
`brew install python-tk@3.X` for a Homebrew Python).

Matching is on normalized words (case and punctuation are ignored), whole
words only — "reviews checklist" does not trigger "review checklist". A
trigger spoken on its own ("Review checklist.") is pasted verbatim —
Ollama cleanup and spoken symbols are skipped so the prompt is pasted
exactly as saved. Mid-sentence, the trigger is expanded in place after
cleanup, and the expansion itself is never symbol-converted ("slash"
inside a saved prompt stays literal). Set `hot_phrases = false` to
disable.

### Dictionary

The local dictionary supplies vocabulary to speech recognition and can
correct commonly misheard words before cleanup. It lives at
`~/.localflow/dictionary.json` and is separate from the hot-phrase store.

```bash
lf dict list
lf dict add LocalFlow
lf dict add --from "local flow" --to "LocalFlow"
lf dict remove LocalFlow
lf dict import vocabulary.csv
```

Imports accept CSV words or `from,to` replacements (with optional headers)
and JSON in the dictionary file's shape. Entries are trimmed, and
case-insensitive duplicates are skipped.

### Spoken formatting
When `[dictation].spoken_punctuation` is enabled, say “new line” or “new
paragraph” to add line breaks, or dictate punctuation such as “comma,”
“question mark,” and “period.” “Scratch that” removes the preceding
sentence or clause. The setting defaults to `true`.

## Shortcuts

The default hold key records dictation. Double-tap it to lock recording;
press Esc to cancel. The shortcut card in setup lists the configured paste-last,
Command Mode, Polish, and prompt-engineering keys. On macOS, the defaults are
Right Option for Command Mode, Ctrl+Option+1 for Polish, Ctrl+Option+2 for
prompt engineering, and Ctrl+Cmd+V to paste the last transcript. On other
platforms, paste-last defaults to Ctrl+Shift+V. Set a shortcut to an empty
string in `[hotkey]` to disable it. `max_recording_seconds` defaults to 1200;
set it to 0 for no duration limit.

## Command Mode & transforms

Hold the configured Command Mode key, speak an instruction, and release it.
LocalFlow transcribes the instruction and applies it to the selected text, or
writes the requested text when there is no selection. Polish rewrites selected
text without changing its meaning; prompt engineering turns selected notes
into a structured prompt. These actions require a configured local or
OpenAI-compatible LLM. Audio, selection capture, and paste actions remain
local to the machine.

### Cleanup levels & styles

Cleanup level controls how much the optional local language model changes a
transcript: `light` fixes dictation errors without rewording, while `medium`
may lightly rephrase awkward sentences. The setup wizard and
`lf setup --cli --cleanup-level light|medium` let you choose; the default is
`light`. Choosing No cleanup disables the LLM step.

Style is selected by the frontmost app category (work, email, personal, or
other) and can be configured in `[cleanup]`:

```toml
[cleanup]
level = "light"
styles_enabled = true
style_work = "formal"
style_email = "formal"
style_personal = "casual"
style_other = "formal"
app_overrides = ["com.example.editor=work"]
```

### Configuration

Configuration is stored at `platformdirs`' user config location
(`~/.config/localflow/config.toml` on Linux and
`~/Library/Application Support/localflow/config.toml` on macOS). An existing
`~/.localflow.toml` is migrated automatically on first load and left in place.
Use `lf config` to inspect or edit it, and `lf secret` to manage API keys
without putting them in the TOML file:

```console
lf config path
lf config show
lf config get llm.model
lf config set hotkey.key alt_l
lf secret set llm_api_key
lf secret status
```

The v2 file uses nested sections:

```toml
 config_version = 2

[stt]
provider = "auto"                  # auto | mlx-whisper | faster-whisper | parakeet
model = "base"                     # tiny, base, small, medium, large-v3
# language = "en"                  # omit for autodetect (parakeet is English-only)
device = "auto"

[llm]
provider = "ollama"                # ollama | openai-compatible
base_url = "http://localhost:11434"
model = "llama3.2:3b"
cleanup_enabled = true
cleanup_timeout = 120
num_ctx = 8192

[hotkey]
key = "alt_l"                      # bare key = hold-to-talk; combo = toggle
sounds = true

[paste]
method = "auto"

[dictation]
spoken_symbols = true              # "slash"/"dash"/"underscore" -> / - _
command_names = []
skills_dirs = ["~/.claude/skills", "~/.agents/skills"]
sample_rate = 16000
input_device = ""                  # empty = system default

[setup]
version = 0

[server]
host = "127.0.0.1"
port = 8756

[features]
meetings = true                    # macOS only; forced false elsewhere
hot_phrases = true                 # spoken trigger phrases expand into saved prompts

[meetings]
watch = true
chunk_seconds = 30
min_busy_seconds = 12
auto_start = true
work_prompts = true
fireworks_model = "accounts/fireworks/models/kimi-k2p6"
prompts_dir = "~/projs/prompts/meetings"
vault_path = "~/projs"
notes_folder = "notes/meetings"
logs_folder = "notes/transcripts"
orca_repo = "path:/Users/you/projs/myrepo"
orca_agent = "claude"
```

All keys are optional; defaults shown above apply.

Open the local settings UI at
`http://127.0.0.1:8756/settings`, or run `lf ui` to open it in your browser.
The `lf status`, `lf stt`, and `lf llm` commands show status and manage the
configured speech-to-text and language-model providers.

`stt.provider = "parakeet"` uses NVIDIA Parakeet TDT 0.6B v2 via
[parakeet-mlx](https://github.com/senstella/parakeet-mlx) (Apple Silicon).
Install with `pip install -e '.[parakeet]'`. It is a fixed checkpoint
(`mlx-community/parakeet-tdt-0.6b-v2`), so `stt.model` is ignored, and it is
English-only, so `stt.language` autodetect does not apply.

#### Spoken slash-commands

Dictation turns "slash" into `/` and, when the words after it exactly match a
registered command name, snaps them to that name's registered spelling:

| Dictated | Pasted (with `skill_part`, `candidate-solutioning` registered) |
| --- | --- |
| `slash skill part.` | `/skill_part` |
| `run slash candidate solutioning with these args` | `run /candidate-solutioning with these args` |
| `slash unknown words` | `/unknown words` |
| `we discussed candidate solutioning today` | unchanged |

Registered names are `command_names` plus the subdirectory names of each
`skills_dirs` entry (the `<skills>/<name>/SKILL.md` layout). Only exact word
sequences match — no fuzzy matching, so `candidate solution` is never rewritten
to `/candidate-solutioning` — the separator comes verbatim from the registered
name (`-` vs `_`), the longest registered name wins on overlap, and only
sentence punctuation trailing a command at the end of the dictation is dropped.
Meeting transcripts are never touched.

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

- **localflow/config.py**: Load nested v2 TOML (and migrate legacy
  `~/.localflow.toml` files)
- **localflow/audio.py**: Record mono float32 via sounddevice
- **localflow/stt.py**: Transcribe via mlx-whisper (Apple GPU), faster-whisper (CPU, int8), or NVIDIA Parakeet TDT via parakeet-mlx (English-only; arrays fed straight to the log-mel front-end, no temp files/ffmpeg)
- **localflow/cleanup.py**: Optional Ollama LLM polish (timeout 15s, graceful fallback)
- **localflow/inject.py**: Paste text via osascript and System Events
- **localflow/hotkey.py**: Global hotkey listener (pynput)
- **localflow/launchagent.py**: launchd LaunchAgent — run on login, stay always on
- **localflow/app.py**: Desktop CLI entrypoint
- **localflow/server.py**: FastAPI web server for phone access
- **localflow/static/index.html**: Single-page mobile app (no external assets)
