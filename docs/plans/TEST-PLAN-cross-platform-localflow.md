# localflow — cross-platform test plan

Companion to `PLAN-cross-platform-localflow.md`. Organised by layer (unit → contract →
integration → OS-level end-to-end → release), then by phase gate.

Guiding rule: **anything that touches a mic, a keyboard hook, a clipboard, or a GPU gets a
fake for CI and a real check on real hardware before each release.** CI proves the logic;
the per-OS smoke matrix proves the OS integration.

---

## 1. Test infrastructure

### 1.1 Fakes (in `tests/fakes/`)
- `FakeSTTProvider` — returns canned text; records the audio array it received; can be told to raise, sleep, or return empty.
- `FakeLLMProvider` — echo / canned / failing / slow; records prompts.
- `FakePlatform` — implements the platform protocol; records `paste()`, `notify()`, autostart calls; `mic_in_use` scripted.
- `FakeRecorder` — yields a fixture WAV instead of opening PortAudio.
- `FakeHotkey` — exposes `press()`/`release()` to drive `pushtotalk` without pynput.
- HTTP fakes for providers via `respx`/`responses`: Ollama `/api/tags` + `/api/generate`, OpenAI `/v1/models` + `/v1/chat/completions`, LM Studio, Fireworks (each with a recorded real response as fixture).

### 1.2 Fixtures
- `tests/fixtures/audio/`: 3 short WAVs (1 s silence, 5 s clean English, 12 s with filler words), 16 kHz mono float32, plus one webm and one m4a for the upload path. Include expected transcripts for the `tiny` model with a tolerance (WER ≤ 0.2) so real-model tests are deterministic enough.
- Config fixtures: legacy `~/.localflow.toml` (v1), v2 minimal, v2 full, malformed.

### 1.3 CI matrix (GitHub Actions)
```
os:      [ubuntu-22.04, ubuntu-24.04, windows-latest, macos-14]
python:  [3.11, 3.12, 3.13]
```
- Unit + contract tests run on every OS/Python with fakes only (no models, no audio device).
- A `real-model` job (Linux + macOS only, cached HF download of `tiny`) runs `@pytest.mark.model` tests.
- A `gpu` job is self-hosted or skipped (label `needs-gpu`); it runs weekly on Taj's machines if registered.
- Lint/type: `ruff`, `mypy --strict` on `providers/`, `platform/`, `config.py` (the new seams must be typed).
- Coverage floor 80 % on `localflow/` excluding `platform/darwin.py`/`win32.py`/`linux.py` (covered by the OS smoke matrix instead).

---

## 2. Unit tests (all OSes, fakes only)

| Area | Cases |
|---|---|
| **config** | v1→v2 migration maps every old key; unknown keys ignored with warning; `platformdirs` path per OS (monkeypatch `sys.platform`); legacy `~/.localflow.toml` read once and migration message emitted; `save_config` round-trips; env var overrides; keys never written to TOML (assert file contents); `features.meetings` forced false off darwin; malformed TOML → clear error, defaults used |
| **provider registry** | built-ins discovered; entry-point provider discovered from a test package; unknown id → error listing valid ids; `auto` resolution per platform/hardware (parametrise `sys.platform`, `platform.machine()`, fake CUDA presence) |
| **STT providers** | each provider's `transcribe` on fixture WAVs (faster-whisper `tiny` in `@model` tests; mlx/parakeet skipped off darwin/arm64 with explicit `skip` reason); empty/short audio returns `""` not exception (existing Parakeet `n_fft` guard becomes a shared test); `list_models` shape |
| **LLM providers** | OpenAI-compatible provider against recorded fixtures for Ollama, LM Studio, OpenAI, Fireworks; timeout → returns input unchanged (existing Cleaner contract); non-2xx → unchanged; empty response → unchanged; `num_ctx` passed on native Ollama path; API key read from keyring not config; healthcheck states |
| **cleanup / symbols / dispatch** | existing `test_process_clip.py`, `test_symbols.py`, `test_dispatch.py` keep passing unchanged (regression guard for the P0 move) |
| **pushtotalk** | existing tests + `FakeHotkey` driven hold/toggle; sub-0.3 s clip dropped; STT on worker thread (assert listener thread never blocks > 50 ms); provider hot-swap mid-idle takes effect on next clip; hot-swap mid-recording is deferred |
| **platform selection** | `platform.current()` returns darwin/win32/linux by `sys.platform`; each module exposes the full protocol (structural check so a missing method fails at import in tests) |
| **paste (logic only)** | clipboard saved/restored even when the keystroke fails; `clipboard-only` method never sends keys; unicode/emoji/newline round-trip through the clipboard |
| **autostart (logic only)** | generated launchd plist / systemd unit / schtasks XML match golden files; install is idempotent; uninstall of absent entry is a no-op; status parses each tool's output |
| **single instance** | second start exits non-zero with the right message; lock released on exit; stale lock from dead PID reclaimed (Windows semantics tested with `msvcrt` on the windows job) |
| **CLI** | `click.testing.CliRunner` for every `lf` command; `--json` output validates against a schema; commands fall back to direct calls when no server is running and to the API when one is (fake both); `lf doctor` reports each check pass/fail from fakes |
| **local API** | FastAPI `TestClient`: every route in §2.4 of the plan; `PUT /api/config` rejects invalid values with field-level errors and does not partially write; token required on non-loopback bind, not required on loopback; `/api/models/download` progress lifecycle (queued → downloading → done/failed) |

---

## 3. Contract tests for providers (run against real services, opt-in)

Marked `@pytest.mark.live`; each skips unless its env var / service is present. Run nightly
and before release, not on every PR.

- Ollama (`ollama serve` in CI service container, pulls `qwen2.5:0.5b`): list models, cleanup a filler-word fixture, assert no filler words remain and word count within ±20 %.
- LM Studio / llama.cpp server: same suite via OpenAI-compatible provider against a container running `llama-server` with a tiny GGUF.
- OpenAI / Fireworks: list models + one 20-token completion (uses `FIREWORKS_API_KEY` secret; skipped if absent).
- Remote STT (OpenAI Whisper API, Deepgram if included): transcribe the 5 s fixture, WER ≤ 0.2.

These catch API drift that the recorded fixtures would hide.

---

## 4. OS-level end-to-end smoke matrix (manual or semi-automated, per release)

Run on real machines / VMs, not CI runners, because global hotkeys, TCC prompts, clipboards
and tray icons don't work headless.

**Targets**

| OS | Variants |
|---|---|
| macOS | Apple Silicon (Sonoma or later), Intel if still supported |
| Windows | 11 with NVIDIA GPU; 11 CPU-only; 10 CPU-only |
| Linux | Ubuntu 22.04 X11 (GNOME); Ubuntu 24.04 Wayland (GNOME); Fedora KDE Wayland; Arch/Hyprland (community) |

**Script (identical on every target, results recorded in `docs/qa/<version>/<target>.md`)**

1. **Install** from the release artefact (dmg / installer exe / AppImage / pipx). Record: time to first launch, any OS warning (Gatekeeper, SmartScreen), whether it launched.
2. **First run**: no config present → tray appears, settings window opens, default STT is `tiny`, download progress shown, completes.
3. **Permissions** (macOS): Mic + Accessibility + Input Monitoring prompts appear once, granted, hints in UI disappear. (Linux Wayland: hotkey limitation banner shown.)
4. **Core loop**: hold hotkey in a text editor, say the fixture sentence, release → text appears in the editor within 3 s (`tiny`, CPU). Repeat 5×; record latency and any drops. Clipboard content from before the dictation is restored.
5. **Toggle mode**: set a combo hotkey in UI → same test.
6. **Provider switch (STT)**: UI → pick a bigger model → download → dictate; `lf stt use faster-whisper small` from terminal → UI reflects it within 2 s; on GPU boxes pick `cuda` → `lf status` shows device; on Apple Silicon pick mlx and parakeet.
7. **Provider switch (LLM)**: with Ollama running, enable cleanup → filler words removed; stop Ollama → dictation still pastes raw text within timeout and UI shows LLM "unreachable"; switch to LM Studio / OpenAI-compatible URL → works; enter an API key → confirm it is in the OS keyring and absent from the TOML.
8. **Paste edge cases**: dictate into a browser URL bar, a terminal, a password field (should not paste? decide expected), an Electron app (VS Code, Slack); text with emoji and newlines.
9. **Server/phone**: enable non-loopback bind → red warning shown; from phone over Tailscale record and transcribe; copy to clipboard works; disable → warning gone.
10. **Autostart**: enable → reboot → tray present, hotkey works without opening UI; disable → reboot → not running. Crash the process (`kill -9`) → relaunched (mac/linux KeepAlive/Restart=on-failure; Windows Task Scheduler restart setting).
11. **Single instance**: launch twice → second exits with message; `lf toggle` controls the first.
12. **Config edit while running**: change hotkey via `lf config set` → takes effect without restart; write invalid TOML by hand → app keeps running, UI shows validation error.
13. **Uninstall**: leaves no autostart entry, keyring entries removed on request, config dir left (documented).
14. **Offline**: disconnect network, local providers → everything works; remote provider selected → clear error, no hang > timeout.
15. **Resource check**: idle CPU < 2 %, idle RSS recorded per model; no mic indicator lit when idle (macOS orange dot, Windows mic icon).

Pass criterion per target: steps 1–7, 10, 11, 14 all pass; the rest logged as known issues
with severity.

### 4.1 Semi-automation
- Steps 4–7, 12 can be scripted with `xdotool`/`pyautogui` on X11 and Windows to press the
  hotkey and play the fixture WAV into a virtual mic (`pactl load-module module-null-sink` +
  `paplay` on Linux; VB-Cable on Windows; BlackHole on macOS). Ship as `scripts/e2e/` so a
  release engineer can run it in a VM.
- Windows and Linux VMs via GitHub Actions `windows-latest`/`ubuntu` with a virtual audio
  device can cover the **non-hotkey** path: `lf dictate --from-file fixture.wav --paste-to
  stdout`, plus `lf paste-test` into a Tk window the test opens itself. This gives a CI
  approximation of steps 4 and 8.

---

## 5. Packaging / release tests

Run in the release workflow on each OS runner after building the artefact:

- Artefact launches with `--version` and exits 0 (PyInstaller import-hook regressions show up here).
- `lf doctor --json` from the bundled binary: ffmpeg/PyAV present, PortAudio loads, at least one input device enumerated (virtual), faster-whisper imports, CUDA libs present in the `cuda` build.
- Size budget: dmg/exe/AppImage each under an agreed cap (e.g. 300 MB CPU build, 1 GB CUDA build); fail the build if exceeded.
- Signature check: `codesign --verify --deep --strict` + `spctl -a` on macOS; `signtool verify /pa` on Windows; fail if unsigned.
- `pipx install ./dist/*.whl` on each OS then `lf --version`; `pip install localflow[mlx]` on Linux is a no-op (marker test).
- Install → run → uninstall in a clean VM snapshot leaves the expected files only (golden list).
- Homebrew cask / winget manifest validate (`brew audit --cask`, `winget validate`).

---

## 6. Security tests

- Server bound to `0.0.0.0` without token → `PUT /api/config` rejected (401). Loopback without token → allowed.
- Path traversal on `/api/models/download` and `/api/logs`.
- Transcript text never appears in logs at default level (grep the log after a dictation with a sentinel phrase).
- API key never appears in: TOML, logs, `lf status`, `lf config get` (masked), `/api/config` response.
- `pip-audit` / `uv audit` in CI; Dependabot enabled.
- Static: `bandit` on `platform/` (subprocess usage is where injection risk lives; assert argv-list calls only, no `shell=True`).

---

## 7. Performance and regression benchmarks

- `bench/` script: transcribe the 12 s fixture 10× per provider/model/device, report p50/p95;
  results committed to `docs/bench/<host>.md`. Guards against a provider refactor silently
  dropping mlx to CPU (the existing 951 ms vs 3202 ms number is the reference).
- Cold start to tray icon, cold start to model loaded, per OS.
- Memory after 100 dictations vs after 1 (leak check on the worker-thread path).

---

## 8. Phase gates (what must be green to move on)

| Phase | Gate |
|---|---|
| P0 Seams | All pre-existing tests pass unchanged; new registry/config/platform unit tests pass on all 4 OS runners; Taj's daily dictation on Mac unchanged for 2 days |
| P1 Win/Linux core | Smoke script steps 1–5, 11 pass on Windows 11 and Ubuntu 22.04 X11 |
| P2 Providers | Contract suite (§3) green against Ollama + one OpenAI-compatible server + Fireworks; hot-swap tests pass |
| P3 UI | Smoke steps 2, 6, 7, 9, 12 pass on all three OSes; API tests 100 % route coverage |
| P4 CLI | Every `/api/*` route has a CLI test; `--json` schema tests pass |
| P5 Packaging | §5 green on all runners; signed artefacts; fresh-VM install on each OS by someone other than the author |
| P6 Hardening | Full §4 matrix including Wayland targets; §6 and §7 green; no P0/P1 bugs open |

---

## 9. What we cannot test in CI and how we cover it

| Gap | Coverage |
|---|---|
| macOS TCC permission prompts | Manual on Taj's Mac + one fresh user account per release |
| Global hotkey on Wayland | Manual on GNOME + KDE Wayland VMs; documented limitations |
| GPU paths (CUDA, Metal) | Self-hosted weekly job or manual bench run before release |
| Real microphones (echo, gain, USB devices) | Beta group of 5–10 external users with a feedback form and `lf doctor --report` output |
| Code-signing / notarisation end-to-end | Release dry run on a tag `v0.x.0-rc1` at least one week before release |
