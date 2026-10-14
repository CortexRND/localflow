# localflow — world-facing, cross-platform release plan

Target: a version of localflow anyone can install on **Windows, Linux, and macOS**, with a
**pluggable inference backend** (local or remote) that the user chooses from an **on-machine
UI** and can drive entirely from the **CLI**.

This plan is grounded in the current repo (`localflow/`, ~3.2k lines of Python, macOS-only).
Every section names the existing module it touches so work can be parallelised.

---

## 0. Where we are today

| Concern | Current implementation | Portable? |
|---|---|---|
| Config | `config.py` — single `~/.localflow.toml`, flat dataclass | Mostly. Path is hard-coded, no XDG/AppData; Mac-specific defaults (`~/projs` vault, `alt_l` hotkey) |
| Audio capture | `audio.py` — `sounddevice` | Yes (PortAudio ships on all three) |
| STT | `stt.py` — faster-whisper (CPU), mlx-whisper, parakeet-mlx | faster-whisper yes; mlx / parakeet-mlx are **Apple Silicon only** |
| Cleanup LLM | `cleanup.py` — Ollama `/api/generate` only | Yes, but only one provider |
| Meeting notes / work prompts | `meetings.py` (Ollama), `workprompts.py` (Fireworks, hard-coded) | Provider hard-coded per feature |
| Paste | `inject.py` — `osascript` Cmd+V | **macOS only** |
| Hotkey | `hotkey.py` — `pynput` | Library is cross-platform; `alt_l` default and permission text are Mac-specific; Wayland is unsupported by pynput |
| Meeting detection | `meetings.py` — mic-in-use polling via macOS tooling, `osascript` notifications | **macOS only** |
| Always-on | `launchagent.py` — launchd plist | **macOS only** |
| Tray UI | `menubar.py` — `rumps` | **macOS only** |
| Web UI | `server.py` + `static/index.html` — phone recording page | Yes, but no settings/model UI |
| CLI | `cli.py` — `lf` (click) | Yes |
| Install | `setup.sh`, `pip install -e .`, `uv.lock` | Unix only; no binaries |
| Single instance | `singleinstance.py` | Needs checking on Windows (file-lock semantics) |

The Mac-specific pieces are all thin (each < 150 lines), so the shape of the work is
"introduce a platform seam, then fill in two more implementations", not a rewrite.

---

## 1. Goals and non-goals

**Goals**
1. One-command install on each OS; a signed/notarised binary where the OS requires it.
2. User picks *where inference runs* — STT and LLM independently — from a UI or CLI, without
   editing TOML by hand.
3. Same feature set on all three OSes for the **core loop** (hotkey → record → STT → cleanup →
   paste) and the **server/phone** path.
4. Everything the UI can do, `lf` can do (the UI is a client of the same local API).
5. Safe defaults for strangers: loopback bind, no telemetry, no keys in the repo, cloud
   backends opt-in with a clear "this leaves your machine" label.

**Non-goals for v1**
- Meeting detection / notes / Orca dispatch on Windows and Linux. These stay macOS-only
  behind a feature flag (see §2.5). They are Taj's personal workflow, not the product.
- Mobile native apps. The existing web page over Tailscale remains the phone story.
- Real-time streaming STT.

---

## 2. Architecture changes

### 2.1 Inference provider abstraction (the "plug in where ideas queue from" piece)

Introduce `localflow/providers/` with two small protocols and a registry:

```python
class STTProvider(Protocol):
    id: str                                   # "faster-whisper", "whisper-cpp", "mlx-whisper", "parakeet-mlx", "openai", "deepgram", "custom-http"
    def list_models(self) -> list[ModelInfo]  # downloadable / available models
    def load(self, model: str, language: str | None) -> None
    def transcribe(self, audio: np.ndarray) -> str
    def capabilities(self) -> Caps            # gpu, languages, streaming, offline

class LLMProvider(Protocol):
    id: str                                   # "ollama", "lmstudio", "llamacpp-server", "openai-compatible", "fireworks", "anthropic", "none"
    def list_models(self) -> list[ModelInfo]
    def complete(self, system: str, user: str, *, max_tokens: int | None) -> str
    def healthcheck(self) -> Health
```

- `stt.Transcriber` becomes a thin facade over the selected `STTProvider`; existing
  faster-whisper / mlx / parakeet code moves into one provider each (behaviour-preserving move,
  existing tests keep passing).
- `cleanup.Cleaner`, `meetings.MeetingSummarizer`, `workprompts.WorkPromptGenerator` all take
  an `LLMProvider` instead of URL/model/API-key triples. **One** OpenAI-compatible HTTP provider
  covers Ollama (`/v1`), LM Studio, llama.cpp server, vLLM, Fireworks, OpenAI, OpenRouter,
  Groq, Together — differentiated only by base URL + key. Keep the native Ollama
  `/api/generate` path for `num_ctx` control.
- Providers declare `offline: bool`; the UI and `lf status` show a lock icon / "local" badge
  from that flag.
- Registry via `importlib.metadata` entry points (`localflow.stt_providers`,
  `localflow.llm_providers`) so third parties can ship a provider as a pip package.

**Recommended default STT per platform** (auto backend):
- macOS Apple Silicon → mlx-whisper (existing) ; Parakeet optional
- Windows / Linux with NVIDIA → faster-whisper CUDA (`device="cuda"`, `compute_type="float16"`), fall back to CPU int8
- Everything else → faster-whisper CPU int8
- Add `whisper.cpp` (`pywhispercpp`) as an optional provider: Vulkan/Metal/CUDA, small binary, good for AMD/Intel GPUs where CTranslate2 has no acceleration.

### 2.2 Config: layered, machine-writable, versioned

- Location via `platformdirs`: `%APPDATA%\localflow\config.toml`, `~/.config/localflow/config.toml`,
  `~/Library/Application Support/localflow/config.toml`. Keep reading `~/.localflow.toml` as a
  legacy fallback with a one-time migration message.
- Add `config_version`; a `migrations.py` that rewrites old keys (`ollama_url`+`ollama_model` →
  `[llm] provider="ollama" base_url=... model=...`).
- Structure:
  ```toml
  config_version = 2
  [stt]     provider = "auto"  model = "base"  language = ""  device = "auto"
  [llm]     provider = "ollama" base_url = "http://localhost:11434" model = "llama3.2:3b"  cleanup_enabled = true
  [hotkey]  key = "auto"          # auto → alt_l on mac, ctrl_r on win/linux
  [paste]   method = "auto"       # auto | clipboard-only | type
  [server]  host = "127.0.0.1"  port = 8756
  [features] meetings = false     # macOS only for now
  ```
- Secrets (API keys) go to the OS keyring via `keyring`, never into the TOML. Env vars still
  override (`LOCALFLOW_LLM_API_KEY`).
- `config.py` gains `save_config()`; the UI and `lf config set` write through it; the running
  app reloads on change (watch file mtime, or `POST /config` triggers hot-swap of providers).

### 2.3 Platform seam

`localflow/platform/` with `base.py` protocol + `darwin.py`, `win32.py`, `linux.py`, selected
once at import by `sys.platform`:

| Capability | macOS | Windows | Linux |
|---|---|---|---|
| `paste(text)` | osascript Cmd+V (existing) | `pyperclip` + `pynput` Ctrl+V; fallback SendInput unicode typing via `pywin32`/`ctypes` | X11: `pyperclip` + `pynput` Ctrl+V or `xdotool type`; Wayland: `wl-copy` + `wtype`/`ydotool` |
| `hotkey listener` | pynput (existing) | pynput | X11: pynput; Wayland: `evdev` (needs `input` group) or GNOME/KDE global-shortcut portal → fallback to toggle via `lf toggle`/tray click |
| `notify(title, body)` | osascript | `win10toast`/`plyer` or WinRT toast | `notify-send` (libnotify) |
| `autostart install/uninstall/status` | launchd (existing) | Task Scheduler (`schtasks`) or `HKCU\...\Run` entry | systemd `--user` unit (`~/.config/systemd/user/localflow.service`) + XDG autostart `.desktop` |
| `permissions_hint()` | Mic + Accessibility + Input Monitoring text | none needed (maybe SmartScreen note) | `input` group / portal note on Wayland |
| `mic_in_use_by_other_app()` | existing | `NotImplemented` (feature off) | `NotImplemented` |
| `single_instance_lock()` | fcntl (existing) | `msvcrt.locking` or named mutex | fcntl |

Every call site in `app.py`, `pushtotalk.py`, `server.py`, `cli.py`, `menubar.py` swaps its
direct `subprocess`/`osascript` call for `platform.current().<capability>()`.

### 2.4 On-machine UI

Decision: **one UI, two hosts.** Extend the existing FastAPI server + `static/index.html`
into a settings/status SPA, then wrap it in a tray app that opens it in a native window.

Why not a native toolkit per OS: we already have a web UI, the server already exists as the
single source of truth, and `lf` must expose the same operations — so everything routes
through a local HTTP API anyway.

**Tray / desktop shell** — replace `rumps` with `pystray` (cross-platform: Win32, AppKit,
GTK/AppIndicator). Menu: status line, Start/Stop dictation, Open Settings, Toggle cleanup,
Quit. On macOS keep rumps-quality behaviour by checking pystray's Darwin backend against the
existing `menubar.py` features (menu bar icon states: idle / recording / server down).

**Settings window** — `pywebview` pointing at `http://127.0.0.1:<port>/settings`. Uses the
OS's own web engine (WebView2 / WebKit / WebKitGTK), so no Electron; adds ~1 MB. Fallback:
open in default browser.

**Settings SPA pages** (vanilla JS or Preact via a single `<script>` — keep the no-build-step
property of `static/index.html`):
1. **Status** — STT/LLM provider + model in use, local/remote badge, last transcription
   latency, hotkey, server bind, log tail.
2. **Speech-to-text** — provider dropdown (filtered to what's installed + what the hardware
   supports), model dropdown with size/VRAM/download-state, device (auto/cpu/cuda/metal),
   language, "Download" button with progress, "Test with mic" button.
3. **Language model** — provider dropdown, base URL, model dropdown populated from the
   provider's `list_models()` (Ollama `/api/tags`, OpenAI `/v1/models`), API key field (writes to
   keyring), "Test connection", cleanup on/off, cleanup prompt editor.
4. **Hotkey & paste** — key picker (records next keypress), hold vs toggle, paste method,
   sounds on/off.
5. **Server & remote access** — bind address with a red warning when non-loopback, port,
   Tailscale hint, QR code of the URL for phone.
6. **Startup** — run on login toggle (calls platform autostart), single-instance status.
7. **Advanced** — raw TOML editor with validation, "Open config folder", "Open logs".

**Local API** (added to `server.py`, all loopback-only by default, plus a per-boot random
token in the config dir that CLI/UI send as a header so a stray LAN bind can't reconfigure
the app):
```
GET  /api/status
GET  /api/config            PUT /api/config           (validated, saved, hot-reloaded)
GET  /api/providers/stt     GET /api/providers/llm    (installed, available, capabilities)
GET  /api/models?provider=  POST /api/models/download  GET /api/models/download/{id}/progress
POST /api/test/stt          POST /api/test/llm
POST /api/dictate/start     POST /api/dictate/stop    POST /api/dictate/toggle
GET  /api/logs?tail=200
```
The desktop app (`app.py`) and the server merge into **one process** (`lf run`) so the UI can
control the live dictation loop; the phone `/transcribe` endpoint remains.

### 2.5 Feature flags and macOS-only features

Meetings, notes, work prompts and Orca dispatch move behind `[features] meetings = true`,
which the config loader forces to `false` off macOS and the UI hides. `cli.py` still
registers the `meeting`/`prompts` groups but they exit with "not available on this platform".
This keeps Taj's workflow intact without blocking the release.

### 2.6 CLI parity

`lf` grows to cover everything in the UI. Proposed surface:

```
lf run                      # tray + server + dictation (the thing autostart launches)
lf dictate                  # headless dictation only (SSH / no tray)
lf serve                    # server only (phone / API)
lf toggle | start | stop    # control a running instance (for WMs without global hotkeys)
lf status [--json]
lf config get <key> | set <key> <value> | edit | path | migrate
lf stt list [--all] | use <provider> [<model>] | download <model> | test [file.wav]
lf llm  list | use <provider> [--base-url --model] | key set <provider> | test
lf paste-test               # types "localflow ok" into the focused window
lf autostart install | uninstall | status     (replaces `lf agent`)
lf doctor                   # audio devices, ffmpeg, GPU, permissions, provider health
lf ui                       # open settings window / browser
```
All commands print human output by default and `--json` for scripting; they talk to the
running instance over the local API when present and fall back to direct calls otherwise.

### 2.7 Packaging and distribution

| Channel | macOS | Windows | Linux |
|---|---|---|---|
| Python users | `pipx install localflow[mlx]` / `uv tool install` | `pipx install localflow[cuda]` | `pipx install localflow` |
| Binary | PyInstaller `.app` in a `.dmg`, **signed + notarised** (Developer ID) — required or Gatekeeper blocks it and TCC permission prompts misbehave | PyInstaller one-dir `.exe` + Inno Setup or MSIX installer, **Authenticode signed** (otherwise SmartScreen warning) | PyInstaller AppImage; also `.deb` via `fpm`; Flatpak later |
| Package managers | Homebrew cask | winget manifest, Scoop | AUR, Flathub (later) |

- Bundling: ffmpeg is only needed for the phone upload path (`/transcribe`). Replace with
  `av` (PyAV wheels, bundles ffmpeg libs) or `soundfile` + browser-side WAV so no system ffmpeg
  dependency remains.
- Models are **not** bundled; downloaded on first use to the platform cache dir with the
  progress endpoint above. Ship `tiny` pre-selected so first run works in seconds.
- Optional-extras matrix in `pyproject.toml`: `mlx`, `parakeet` (darwin/arm64 markers),
  `cuda` (faster-whisper + `nvidia-cublas-cu12`, `nvidia-cudnn-cu12`), `whispercpp`, `tray`,
  `ui` (pywebview). Use PEP 508 environment markers so `pip install localflow[mlx]` on Linux
  is a no-op, not an error.
- CI matrix builds all artefacts on tag (`macos-14`, `windows-latest`, `ubuntu-22.04`) and
  attaches them to a GitHub Release; a `version` bump is the only release step.

### 2.8 Security and privacy for a public audience
- Loopback default already in place; add the API token (§2.4) and a warning banner in UI/CLI on non-loopback bind.
- Never log transcripts at INFO; add `[privacy] log_transcripts = false`.
- Remote providers show "audio/text leaves this machine" in the picker and in `lf stt use`.
- `SECURITY.md`, `LICENSE` (confirm MIT/Apache), `CONTRIBUTING.md`, third-party licence notices in the binary (faster-whisper MIT, model licences vary — Parakeet is CC-BY-4.0, whisper MIT).
- Dependency pinning via `uv.lock` in CI; Dependabot.

---

## 3. Delivery phases

Estimates are in Devin sessions (one session ≈ a focused day of work); Mac-specific
verification needs Taj's machine or a macOS runner.

| Phase | Scope | Est. | Exit criterion |
|---|---|---|---|
| **P0 — Seams** | `providers/` + `platform/` packages; move existing code behind them with zero behaviour change; config v2 + migration + `platformdirs`; `keyring` | 1–2 | All existing tests pass; `lf dictate` on Mac identical to today |
| **P1 — Linux + Windows core loop** | `win32.py`, `linux.py` (X11 first, Wayland best-effort); pystray tray; autostart for both; faster-whisper CUDA device selection; `lf doctor` | 2 | Hold hotkey → paste works in a Windows VM and an Ubuntu X11 VM |
| **P2 — Provider plurality** | OpenAI-compatible LLM provider (covers LM Studio/llama.cpp/OpenAI/Fireworks); `whisper.cpp` and remote STT providers; model listing/download API; entry-point registry | 1–2 | Switch STT and LLM at runtime via `lf stt use` / `lf llm use` without restart |
| **P3 — Settings UI** | Local API; settings SPA (7 pages); pywebview window; merge app+server into `lf run` | 2 | Fresh user can pick a model, test it, set hotkey, enable autostart with no terminal |
| **P4 — CLI parity + docs** | Full `lf` surface with `--json`; README rewrite per OS; `lf doctor` polish | 1 | Every UI action has a documented CLI equivalent |
| **P5 — Packaging** | PyInstaller specs ×3; signing/notarisation (needs Apple Developer ID + Windows code-signing cert from Taj); installers; Homebrew/winget/AppImage; release CI | 2 + external waits | Tagged release produces installable artefacts on all three OSes |
| **P6 — Hardening** | Wayland path, AMD/Intel GPU via whisper.cpp, first-run wizard, crash reporting opt-in | 1–2 | Test plan §4/§5 green |

Critical path: P0 → P1 → P3 → P5. P2 and P4 can run in parallel with P3.

**External dependencies to line up early (they gate P5, not the code):**
- Apple Developer ID certificate + notarisation credentials
- Windows Authenticode certificate (or Azure Trusted Signing)
- Decide product name / licence (is "localflow" the public name? check PyPI availability)

---

## 4. Open decisions (need Taj)

1. **Tray + pywebview vs. a native Tauri shell** — pywebview keeps everything in Python and
   the repo small; Tauri gives a nicer installer and smaller binary but adds a Rust toolchain.
   Recommendation: pywebview for v1.
2. **Wayland support level** — best-effort (tray toggle + `lf toggle`) vs. full evdev hotkey
   (requires the user to join the `input` group). Recommendation: best-effort in v1, documented.
3. **Public name / repo** — release from `CortexRND/localflow` or fork to a product repo.
4. **Cloud STT in the default picker** — include OpenAI/Deepgram out of the box (opt-in,
   labelled) or local-only in v1. Recommendation: include, since "plug in where inference
   runs" is the headline feature.
5. **Meetings on other OSes** — confirm they stay macOS-only for v1.
