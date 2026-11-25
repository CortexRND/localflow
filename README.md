# localflow

Local push-to-talk dictation for macOS, with best-effort Linux and Windows support. Fully offline STT using Whisper or Parakeet, optional LLM cleanup, and phone access via Tailscale.

See the [Wispr Flow feature parity comparison](docs/wispr-parity.md) for a feature-by-feature overview.

Release maintainers: [v0.2.3 asset build and publishing](docs/releasing.md).

## Easiest install (no Terminal needed)

**macOS:** download `LocalFlow-Installer.dmg` from the
[latest release](https://github.com/CortexRND/localflow/releases/latest) (or from
the artifacts of a "Mac installer" run on the Actions tab).

The DMG is unsigned and not notarized. On macOS 15 and later (including macOS
26), opening the downloaded DMG may show a dialog saying it "is damaged and
can't be opened", with only **Cancel** and **Move to Trash**. There is no
**Open Anyway** for this dialog, and Privacy & Security does not fix it. Click
**Cancel**, not **Move to Trash**, then either:

- use the verified one-line Terminal install under
  [macOS and Linux](#macos-and-linux) (recommended; it does not need the DMG), or
- remove the download quarantine from the DMG you got from the official
  release:

  ```sh
  xattr -d com.apple.quarantine ~/Downloads/LocalFlow-Installer.dmg
  ```

  Then open the DMG again and double-click **Install LocalFlow**. A window
  installs everything, which takes several minutes, and LocalFlow opens when it
  is done.

Whichever installation method you use (Terminal or DMG), since v0.2.0 macOS
installs the native **Rust LocalFlow.app** by default. Rust
is installed automatically; accept Apple's Command Line Tools installation
dialog if prompted. Python and
its virtual environment are not needed. Missing CMake is installed with Homebrew;
without Homebrew, install CMake first (https://cmake.org/download/).

Linux and Windows continue to use Python. To choose the Python app on macOS:

```sh
LOCALFLOW_RUNTIME=python bash install.sh
```

The Python fallback provisions Python with uv when needed. Both runtimes share
settings, history and notes. Speech models are downloaded by the selected app's
setup wizard on first launch, not redundantly by the Rust installer. Ollama is
optional (`--with-ollama` requires Homebrew on macOS).

The installer puts **LocalFlow** in `~/Applications` (Applications → LocalFlow).
Double-click it: the first time it opens the native setup window; after setup
it starts dictation and the menu bar in the background (reusing the login agent
if installed, never twice) and opens the native Hub. Opening it again just
brings the Hub to the front.

Once the DMG opens, **Install LocalFlow** and the app are still unsigned, so
Gatekeeper blocks each the first time you open it. Close the warning, go to System
Settings > Privacy & Security, scroll down to Security, click **Open Anyway**
next to the message about the app, and confirm with **Open Anyway** and your
password or Touch ID. (On macOS 15 and later, right-click > **Open** no longer
offers this bypass.) The DMG's `Read Me.txt` gives the same steps.

**Windows:** download `Install LocalFlow.cmd` from the same release (or the
Actions artifact) and double-click it.

## What it is

An open-source WisprFlow-style dictation app:
- **Hold left Option** (default) to record — release to transcribe and paste; combo hotkeys toggle instead
- **Offline transcription** via Whisper or Parakeet (no cloud calls)
- **Optional cleanup** with Ollama or an OpenAI-compatible provider (fix punctuation, casing, remove filler words)
- **Instant paste** into the frontmost app
- **Local history and recall**: search past dictations, ask questions across them and your meetings, and let agents search them through the [MCP server](#mcp-server-for-agents)
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

### macOS and Linux

Download the installer for the pinned release, check its SHA-256, then run it:

```sh
curl -fsSLO https://raw.githubusercontent.com/CortexRND/localflow/v0.2.3/install.sh && echo "bbb213cea04a223ac290b5e19eb0fa2b11adf05eb9c8479e9cac6db0639f846c  install.sh" | shasum -a 256 -c - && bash install.sh
```

(`sha256sum -c -` works too.) To install and start Ollama for optional
transcript cleanup, use the same chain with `bash install.sh --with-ollama` as
the final command:

```sh
curl -fsSLO https://raw.githubusercontent.com/CortexRND/localflow/v0.2.3/install.sh && echo "bbb213cea04a223ac290b5e19eb0fa2b11adf05eb9c8479e9cac6db0639f846c  install.sh" | shasum -a 256 -c - && bash install.sh --with-ollama
```

The installer is pinned to release `v0.2.3`. Whether or not git is installed,
it downloads `localflow-0.2.3.tar.gz` from that release and verifies it against
the release's `SHA256SUMS` before unpacking; it stops and deletes the download
if the checksum file or hash is missing or does not match. An existing install
in `~/.localflow/app` (including an older git checkout made by a previous
installer) is replaced by the verified archive only after verification
succeeds, keeping its `.venv`; a destination that is not a LocalFlow install is
never overwritten. A replaced git checkout is never deleted, even a clean one:
it is moved as is (uncommitted, staged and untracked files, unpushed commits,
stashes and all git metadata) to a sibling backup such as
`~/.localflow/app.git-backup-20261120-101500-4242`, and the installer prints
the exact path. Delete the backup yourself once you have recovered anything
you need from it. A linked git worktree or submodule (whose `.git` is a file)
is never replaced: moving it would break its repository's bookkeeping, so the
installer stops with an explanation and changes nothing. The installer does not pin a git commit: the release tag
points at the merge commit that contains `install.sh`, and a file cannot embed
the hash of a commit that includes it.

Or run from a checkout:

```sh
git clone https://github.com/CortexRND/localflow.git
cd localflow
./install.sh
```

The installer creates a Python environment and installs LocalFlow. On macOS,
Homebrew, git and a supported Python are optional (the installer falls back to
a uv-managed Python 3.12, and default installs use the release archive rather
than git); its setup app is placed in
`~/Applications`. Linux installs
system dependencies and creates `~/.local/bin/lf` plus desktop launchers. When
run outside a checkout, the app is installed to `~/.localflow/app`; override
it with `LOCALFLOW_HOME`.

`LOCALFLOW_REF` overrides the pinned release with any branch, tag or commit
(for example `LOCALFLOW_REF=main bash install.sh`). When git is usable, a
branch (`main`, `refs/heads/main` or `origin/main`), tag or full commit SHA
works: a fresh install clones `LOCALFLOW_REPO_URL` (default: this repository),
and every install, fresh or existing, then fetches the ref from origin and
checks out the fetched commit detached, so a rerun always picks up the latest
commit of a branch rather than a stale local copy (`origin/main` and
`refs/remotes/origin/main` are fetched as origin's branch `main`). An existing archive
install is switched to git too: the clone, fetch and checkout happen in a
temporary directory next to it, and the archive install (and its `.venv`, which
is kept) is replaced only once they succeed. If any git step fails the existing
install is left as is and the installer stops; it does not fall back to an
archive. Only without a usable git does the installer download GitHub's
archive for the ref, with a warning (such archives cannot resolve names like
`origin/main`). This is an unverified
override: the source for a custom ref is not checksum-verified, and the
installer prints a warning saying so.

### Windows

Download the installer for the pinned release, check its SHA-256, then run it:

```powershell
Invoke-WebRequest -UseBasicParsing https://raw.githubusercontent.com/CortexRND/localflow/v0.2.3/install.ps1 -OutFile install.ps1; if ((Get-FileHash install.ps1 -Algorithm SHA256).Hash -eq 'E6F3C35E4A6637249F71FA2ABD92DECE3BB7207C10E9CE769F436B9AD2C5B407') { powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 } else { Write-Error 'install.ps1 checksum mismatch; not running it' }
```

To install and start Ollama for optional transcript cleanup, add
`-WithOllama` to the `powershell` command inside the success branch.

Without git, the installer downloads `localflow-0.2.3.zip` from the `v0.2.3`
release and verifies it against the release's `SHA256SUMS`
(`Get-FileHash -Algorithm SHA256`); with git it checks out the `v0.2.3` tag.
As on macOS and Linux, `LOCALFLOW_REF` is an unverified override.

Or run from a checkout:

```powershell
git clone https://github.com/CortexRND/localflow.git
cd localflow
.\install.ps1
```

The installer creates a Python environment, Start Menu shortcuts, and the
`lf` command in `.venv\Scripts`. Run outside a checkout, it installs to
`$HOME\.localflow\app` by default; override it with `LOCALFLOW_HOME`.

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

### Set up a fast local model for dictation

On a machine with around 16 GB of RAM or more, start with `qwen2.5:7b` for
general text tasks and `llama3.2:3b` for light cleanup and automatic meeting
titles. `llama3.1:8b` is an alternative general model; try `qwen2.5:3b` as the
fast model for multilingual dictation. These are starting points: leave memory
for speech recognition and the OS rather than putting a 27B model on the
dictation path.

```bash
ollama pull qwen2.5:7b
ollama pull llama3.2:3b
```

Edit the existing sections in your [LocalFlow config file](#configuration):

```toml
[llm]
provider = "ollama"
base_url = "http://localhost:11434"
model = "qwen2.5:7b"
fast_model = "llama3.2:3b"
cleanup_enabled = true

[cleanup]
level = "light"
```

`fast_model` is optional and uses the same provider and URL as `model`. Leave it
unset or empty to use `model` for everything, as before. If you use the CLI,
you can also set it with `lf config set llm.fast_model llama3.2:3b`. Medium cleanup, voice
commands, selection transforms, meeting notes, transcript Q&A and Recall
answers still use `model`. Whole-utterance hot phrases expand without LLM
generation, dictionary replacements are deterministic, and hotkey dictation skips
cleanup for transcripts under five words (HTTP uploads can clean shorter text).
Switching providers or endpoints through LocalFlow clears the saved `fast_model`,
so set it again afterward if you want to use one.

On an 8 GB machine, try a single model: run `ollama pull llama3.2:1b`, set
`model = "llama3.2:1b"` and leave `fast_model` empty. Or set
`[cleanup].level = "none"` to skip dictation cleanup altogether. A local Ollama
configuration does not change the separate Fireworks meeting work-prompt
feature: if you have a Fireworks key configured and want all meeting processing
offline, set `work_prompts = false` under `[meetings]`.

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

### Meeting notes: notepad, templates, chat

The web UI's Meeting panel works like Granola's notepad (see
[docs/granola-parity.md](docs/granola-parity.md)):

- **Your notes** — jot rough notes before or during the meeting; they autosave to
  `POST /meeting/notes`. On stop the LLM *enhances* them from the transcript, keeping
  your headings, order and emphasis, then adds any template sections you didn't cover.
  Your raw notes are also kept at the bottom of the transcript log.
- **Templates** — pick General, Standup or 1:1 before starting (`template` on
  `POST /meeting/start`; default `[meetings] default_template`). Add your own by dropping
  Markdown files into `templates_dir` (`~/.localflow/meeting_templates/`); a file with the
  same name as a built-in replaces it:

  ```markdown
  ---
  name: Customer call
  description: Discovery call with a prospect.
  ---
  Focus on what the customer said, not what I said. Quote numbers exactly.

  ## Pain Points
  Bulleted list of problems the customer described, in their words.

  ## Next Steps
  Bulleted list with owner and date.
  ```

  Text before the first `## ` header is general guidance; each `## ` header is a section
  and the text under it tells the LLM what goes there.
- **Ask the transcript** — after a meeting is saved, ask questions about it
  (`POST /meeting/chat {"question": ...}`). Answers come from the last saved transcript
  log (or the newest one in the vault after a restart) via the configured LLM provider.

**Remote participants (system audio).** By default only the mic is recorded, so the
transcript is mostly you. Set `[meetings] capture_system_audio = true` to also
transcribe what the machine plays; segments are then tagged **Me** (mic) or **Others**
(system audio) in the live log, the transcript log and the LLM prompts.

- **macOS 13+ (ScreenCaptureKit):** `pip install -e '.[system-audio]'`, then grant
  *Screen Recording* to the Python binary running LocalFlow (System Settings → Privacy &
  Security). No virtual device needed.
- **Fallback (BlackHole):** `brew install blackhole-2ch`, create a *Multi-Output Device*
  in Audio MIDI Setup with your speakers/headphones + BlackHole, make it the system
  output, and set `system_audio_device = "BlackHole 2ch"`. On Linux use a PulseAudio /
  PipeWire `.monitor` source; on Windows "Stereo Mix".

If system audio can't be opened the meeting still records the mic and the panel shows
"mic only — system audio unavailable" (hover for the reason). Wear headphones: on
speakers the mic also hears the remote side, which then appears twice. To tell remote
participants apart, see [Speaker labels](#speaker-labels-and-recognition)
below.

The Hub's Meetings page offers the same features (`lf hub` or menu bar → Meeting notes…).

### Speaker labels and recognition

Off by default. With `[meetings] speaker_diarization = true`, LocalFlow tells the voices
in a meeting apart and writes **Speaker 1**, **Speaker 2**, … instead of **Others**. The
default macOS install is the Rust app: it serves the speaker routes below
but does not label meetings. Labels come from the Python runtime (`lf`, `localflow`) or
from an `lf-rs serve` built with `--features diarize`; for Rust see
[rust/README.md](rust/README.md#speaker-labels). Python setup: use a Python install
(`LOCALFLOW_RUNTIME=python bash install.sh`), then, from the LocalFlow source folder with
its virtualenv active (`~/.localflow/app` for the installer, or your own checkout):

```bash
cd ~/.localflow/app && source .venv/bin/activate
pip install -e '.[diarization]'                  # sherpa-onnx runtime; not installed by default
lf speakers download                             # two ONNX models (~34 MB), checksum-verified
lf config set meetings.speaker_diarization true
lf speakers status                               # models, runtime and flags (offline)
```

- **What gets labelled.** Enable `capture_system_audio = true` to label remote voices
  on the system-audio stream. Enable `diarize_mic = true` to also label voices on the
  microphone (several people in one room).
  A stream where fewer than two voices are found keeps **Me** / **Others**.
  `max_speakers` caps the count per stream (0 = automatic, at most 10).
- **When.** Labels are applied when the meeting stops; the live log stays Me/Others.
  Settings are read when a meeting starts. A running Python server picks up file edits to
  `speaker_diarization` and `speaker_recognition` for the next meeting; edits to
  `diarize_mic`, `max_speakers` and `diarization_backend` need a server restart. A Rust
  server reads all five keys from the file at each meeting start.
- **If it can't run.** Models are never downloaded on their own. Without the extra or the
  models, or if labelling fails or takes longer than 20 s at stop, the meeting is saved
  with Me/Others as before.
- **Fixing labels.** With the server running:

  ```bash
  lf meeting speakers                       # ids, labels and talk time of the last saved meeting
  lf meeting rename-speaker S2 "Sarah"      # rewrites that label in the transcript log
  lf meeting merge-speakers S3 S1           # S3 was the same person as S1
  ```

  Each takes an optional meeting id first (the `meeting_id` in the transcript log's
  frontmatter). The notes file is not rewritten.
- **Speaker file.** Each saved meeting with speaker labels gets
  `~/.localflow/meetings/<meeting_id>.json` with speaker ids, labels, names, talk time,
  segment timings and file paths, but no transcript text or audio. It is written whether
  or not recognition is on, is what rename and merge use, and is kept until you delete it.

**Recognising people (opt-in, biometric).** Without recognition no voice embedding is
persisted: embeddings stay in memory during the meeting and are dropped at stop.
`speaker_recognition = true` (needs `speaker_diarization`) is a separate switch that keeps
*voiceprints*. At stop, each relabelled speaker (a **Speaker N**) is compared with the
enrolled voiceprints and takes the enrolled name only on a confident match; a stream left
as **Me** / **Others** is never matched. With both switches on and the server running:

```bash
lf config set meetings.speaker_recognition true
MEETING_ID=paste-meeting-id-here     # printed by `lf meeting speakers`; saved with recognition on
lf speakers enroll "Sarah" --from-meeting "$MEETING_ID" --speaker S2
lf speakers enroll "Alex" --record 30      # or: --file clip.wav (up to 25 MB and 300 s)
lf speakers list
lf speakers rename "Sarah" "Sarah K"
```

- **Where it is stored.** Voiceprints live in `~/.localflow/speakers/` (`registry.json`
  plus one `.npy` per person). While recognition is on, each labelled meeting's speaker
  file also keeps one voiceprint per speaker, named or not; that is what `--from-meeting`
  reads. Files are owner-only (0600 in 0700 folders on macOS and Linux), not encrypted,
  and never expire on their own. Voiceprints are never written to the vault; speaker labels
  and names are, as part of the transcript log and notes. Enrollment audio is not kept: it
  is decoded in memory, except that an M4A/MP4 that needs seeking is copied briefly into
  `~/.localflow/speakers/` and deleted after decoding. A voiceprint identifies a person;
  only enroll people who agreed to it.
- **Turning it off** stops matching and stops voiceprints being stored from that moment,
  including for a meeting already running. It deletes nothing: earlier voiceprints and
  speaker files stay on disk.
- **Forgetting** needs both `speaker_diarization` and `speaker_recognition` on (every voice
  command is refused otherwise), so forget before switching either off:

  ```bash
  lf speakers forget "Sarah"            # one person
  lf speakers forget --all              # everyone
  ```

  It removes the voiceprint and erases the stored voiceprints from every saved meeting, so
  earlier meetings can no longer be used with `--from-meeting`. Names already written in
  transcripts and the speaker files' other details stay.
- **Export.** `lf speakers export ~/speakers-backup` copies the voiceprints to a new or
  empty folder outside the vault; there is no import. Enrolling from audio and exporting
  only work against a server on this computer.
- **What leaves the machine.** Analysis is local and offline, and voiceprints are never
  sent to an LLM or exposed over MCP. Speaker labels and names are transcript text, so the
  configured LLM provider gets them for notes, titles, meeting chat and Recall; that is a
  cloud service if you chose one. Separately, with `work_prompts = true` (the default) and
  a Fireworks API key set, the notes, which can contain those names, are sent to Fireworks
  to generate work prompts, even when the LLM provider is local.

`lf-rs` has the same `meeting` and `speakers` commands, except `speakers download`.

HTTP, all with the API token (`X-Localflow-Token`): `GET /api/diarization/status`,
`POST /api/diarization/download` (Python only); `GET /api/meetings/{meeting_id}/speakers`,
`PUT /api/meetings/{meeting_id}/speakers/{speaker_id} {"name": ...}`,
`POST /api/meetings/{meeting_id}/speakers/merge {"from": ..., "into": ...}`. With
recognition on (`{ref}` is a voiceprint id or an enrolled name):

| Method and path | What it does |
| --- | --- |
| `GET /api/speakers` | List enrolled people |
| `POST /api/speakers {"name": ..., "meeting_id": ..., "speaker_id": ...}` | Enroll from a meeting; or multipart `file` + `name` from audio (this computer only) |
| `PATCH /api/speakers/{ref} {"name": ...}` | Rename (later meetings only) |
| `DELETE /api/speakers/{ref}`, `DELETE /api/speakers` | Forget one person, or everyone |
| `GET /api/speakers/export` | ZIP of the voiceprints (this computer only) |

### Recall: ask across everything you've said

Ask "what did I tell Sarah about the budget?" or "when did we decide to drop the Windows
beta?" and get a short answer with numbered citations, drawn from your own dictation
history, meeting transcript logs and meeting notes:

```bash
lf recall "when did we decide to drop the Windows beta?" --since 30d
lf recall '"budget review"' --no-llm      # ranked passages only, no LLM call
lf recall "what did I tell Sarah?" --json --k 5
```

- `--since` takes `today`, `yesterday`, `7d`, `30d` or `YYYY-MM-DD`; `--k` (1–20) sets how
  many passages are retrieved. Put a phrase in quotes to rank exact matches higher.
- Retrieval is a built-in BM25 search (no extra dependencies) rebuilt on each query; the
  index never leaves your machine. Only the top passages go to your configured LLM, which
  is told to answer from them alone and to say "I couldn't find that in your history."
  otherwise. If the LLM is down you still get the matching passages. With a cloud LLM
  provider those passages are sent to it, so use a local one if that matters.
- With `[history] enabled = false` dictations are skipped; meetings and notes still count.
- With `[meetings] speaker_diarization = true` (Python runtime only), a citation from a
  meeting transcript may include `speaker` in `--json` output and the HTTP response: the
  label on the first line of the passage if that line has a timestamp and a label (Me,
  Others, Speaker 2 or a name). It is read from the transcript text, independent of
  recognition and without any voiceprint lookup. Ranking and the text output are unchanged.
- HTTP: `POST /api/recall {"question": ..., "since"?, "k"?, "llm"?}` returns
  `{answer, citations}`. It requires the API token (`X-Localflow-Token`), even from
  loopback. The web UI has a Recall panel; open the page with `?token=…` to use it.

### MCP server for agents

`localflow-mcp` is a local [MCP](https://modelcontextprotocol.io) server that lets an
agent (Claude Code, Claude Desktop, Cursor, …) search what you have dictated and the work
prompts generated from your meetings. It speaks MCP over stdio only (no network port, no
token), is read-only (it never writes under `~/.localflow`), never calls an LLM itself, and
never returns audio clips.

| Tool | What it does |
| --- | --- |
| `search_dictations` | Search or list recent dictations, spoken commands and transforms; filter by `since`/`until` (`today`, `yesterday`, `7d`, `YYYY-MM-DD`), `app` and `kind`. |
| `get_dictation` | Full final and raw text of one dictation by id. |
| `search_prompts` | Search or list meeting work prompts (title, meeting, body); filter by `status`. |
| `get_prompt` | Full body and status of one work prompt by id. |

Whatever the tools return is sent to the agent's model provider, so only connect clients
you are happy to share that history with. With `[history] enabled = false` the dictation
tools return an error and never open the history database; the prompt tools still work.

The server is part of the Python package. The default macOS install is the Rust app and has
no `lf` on `PATH`; run a Python install (`LOCALFLOW_RUNTIME=python bash install.sh`) and use
the absolute path `<repo>/.venv/bin/localflow-mcp`, or run it without installing via
`uvx --from <repo> localflow-mcp`.

Claude Code:

```bash
claude mcp add localflow -- localflow-mcp
```

Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`, on
Windows `%APPDATA%\Claude\claude_desktop_config.json`) and Cursor (`~/.cursor/mcp.json` or a
project's `.cursor/mcp.json`) use the same JSON. GUI clients do not see your shell `PATH`,
so give the absolute path:

```json
{ "mcpServers": { "localflow": { "command": "/abs/path/to/.venv/bin/localflow-mcp" } } }
```

Then ask, for example, "find what I dictated about the pricing page last week" or "show
pending work prompts from the Acme sync". `lf mcp` and `python -m localflow.mcp_server`
run the same server.

### Hot phrases

Hot phrases let you paste a saved long prompt by speaking a short trigger
mid-dictation: say "review checklist" and the saved review prompt is pasted
instead. Entries live in `~/.localflow/hot_phrases.json`; the desktop app
re-reads the file when it changes, so edits apply without a restart.

Manage them on the Hot Phrases page of the LocalFlow Hub:

```bash
lf phrases                  # open the Hub on Hot Phrases
lf hub --page "Hot Phrases" # the same, through the Hub command
```

On macOS the Hub is one native window owned by the main LocalFlow app. The
menu bar's "Hot Phrases…" always opens that window. `lf phrases` and `lf hub`
reach it too (bringing it forward if it is already open), unless the separate
LocalFlow Preview is installed and LocalFlow is set up; then they open the
preview instead, and `--legacy` opens the Hub window (see
[History & insights](#history--insights)). The Hot Phrases page has a searchable list on the left (one row per
phrase with an on/off dot) and an editor on the right: "When I say", "Type
this", an Enabled switch, Delete with Undo, and a "Try it" preview that shows
what dictation would paste and which triggers matched. Edits autosave as you
type; an invalid edit stays in the editor with an inline message and is
never lost when you navigate.

`lf phrases install-app` is optional: it writes a `LocalFlow Hot Phrases.app`
compatibility alias in `~/Applications` that simply opens the Hub on that
page (`lf phrases uninstall-app` removes it). The installer does not create it.

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

### History & insights

LocalFlow stores dictations, commands, and transforms in
`~/.localflow/history.sqlite`. Successful dictations include word counts,
WPM, daily totals, streaks, and top-app summaries. Raw transcripts remain
available alongside the final text. Failed dictations can be retried from a
private local audio clip; configure `[history].keep_audio_clips` to choose how
many successful clips to retain. History is enabled by default. Agents can
search it read-only through the [MCP server](#mcp-server-for-agents).

Open the native hub with `lf hub` to browse history, edit the dictionary and
cleanup styles, or review the selected models. If you installed the separate
LocalFlow Preview (native GPUI app, [`rust/lf-desktop`](rust/lf-desktop/README.md)),
`lf hub` and `lf phrases` open it instead on Linux and macOS once LocalFlow is
set up; if something is wrong with it, they say so once and open the usual
window. `lf setup`, and any window on a machine that hasn't been set up yet,
keep the usual setup wizard and windows even with the preview installed.
Without the preview (it isn't part of the standard install) and on Windows,
nothing changes. `--legacy` or `LOCALFLOW_UI=legacy` always opens the usual
window. `--gpui` asks for the preview explicitly. `lf setup --gpui` opens the
preview's own setup wizard; that wizard offers the native app's speech models, which the Python
runtime that handles dictation may not be able to run. This needs `lf` v0.2.3 or later
(the standard install or a source install); v0.2.2 and earlier don't have this
choice and always open the usual window. The CLI can search and manage
history without opening the hub:

```bash
lf history list
lf history search "project update"
lf history --limit 20 --search "project update"
lf history copy 12
lf history raw last
lf history raw 12
lf history retry 12
lf history delete 12
lf history clear --yes
lf stats
```

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
# fast_model = "llama3.2:1b"       # only light cleanup and automatic meeting titles
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

[history]
enabled = true
keep_audio_clips = 5

[meetings]
watch = true
chunk_seconds = 30
min_busy_seconds = 12
auto_start = true
work_prompts = true
fireworks_model = "accounts/fireworks/models/kimi-k3"
prompts_dir = "~/projs/prompts/meetings"
vault_path = "~/projs"
notes_folder = "notes/meetings"
logs_folder = "notes/transcripts"
orca_repo = "path:/Users/you/projs/myrepo"
orca_agent = "claude"
capture_system_audio = false       # also transcribe remote participants (see below)
system_audio_device = ""           # loopback input, e.g. "BlackHole 2ch"; empty = auto
default_template = "general"       # general | standup | one-on-one | your own
templates_dir = "~/.localflow/meeting_templates"
speaker_diarization = false        # Speaker 1..N instead of Others (see Speaker labels)
diarization_backend = "sherpa-onnx"
diarize_mic = false                # also tell apart voices on the microphone
max_speakers = 0                   # 0 = automatic; at most 10
speaker_recognition = false        # opt-in voiceprints; needs speaker_diarization
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

4. Every route except the page itself needs the API token, including from
   the phone. Print it on the Mac with `lf token` (it changes each time the
   server starts), then on the **phone** open the URL once with the token:
   `https://myhost.example.ts.net/?token=<token>`. The page remembers the token
   on that device and drops it from the address bar; open it again with the
   new token after a server restart.
   - Large record/stop button
   - Speak, hit stop
   - Transcript appears
   - Tap copy-to-clipboard

#### iOS Shortcut (alternative)

Create a Shortcut that POSTs audio to `/transcribe`:

```
1. Ask for audio
2. POST [HTTPS URL]/transcribe (multipart form: file parameter)
   with header  X-Localflow-Token: <output of `lf token`>
3. Parse JSON response: {"text": ..., "ms": ...}
4. Show text
```

Uploads are capped at 25 MB and decoded audio at `max_recording_seconds`
(default 20 minutes). The token rotates on every server start, so update the
Shortcut's header after restarting `localflow-server`. Requests without a valid
token get `401`.

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
- **localflow/stt.py**: Speech-to-text front door; delegates to the providers below
- **localflow/providers/**: Pluggable STT (mlx-whisper, faster-whisper, Parakeet) and LLM (Ollama, OpenAI-compatible) providers, registered as entry points
- **localflow/cleanup.py**: Optional LLM polish through the configured LLM provider (graceful fallback to the raw transcript)
- **localflow/inject.py**: Paste text through the platform layer
- **localflow/platform/**: Per-OS paste, autostart, notifications and locking (`darwin.py`, `linux.py`, `win32.py`)
- **localflow/hotkey.py**: Global hotkey listener (pynput)
- **localflow/launchagent.py**: launchd LaunchAgent — run on login, stay always on
- **localflow/app.py**: Dictation entrypoint (`localflow`)
- **localflow/cli.py**: The `lf` command
- **localflow/history.py**: Local SQLite dictation history and retry clips
- **localflow/recall.py**: BM25 search and recall across history, transcripts and notes
- **localflow/meetings.py**: Meeting detection, transcription and notes
- **localflow/diarization/**: Optional meeting speaker labels and the opt-in voiceprint registry
- **localflow/dispatch.py**: Work-prompt files, review queue and Orca dispatch
- **localflow/mcp_server.py**: Read-only MCP server over stdio for agents
- **localflow/server.py**: FastAPI web server for phone access
- **localflow/static/index.html**: Single-page mobile app (no external assets)

## License

LocalFlow is licensed under the MIT License; see [LICENSE](LICENSE).
Dependencies retain their own licenses (for example, pynput is LGPL-3.0 and is
installed from PyPI rather than bundled).
