# localflow — spec

> **Historical v0 spec.** Superseded by [README.md](README.md); hotkeys, config layout and dependencies below are out of date.

Local push-to-talk dictation for macOS (open-source WisprFlow clone).
Flow: global hotkey toggles recording → mic audio → faster-whisper transcribes →
optional Ollama LLM cleans punctuation/formatting → text pasted into frontmost app.
Also a FastAPI server so a phone (via Tailscale) can record in browser and get transcripts.

Python 3.11+. Package dir: `localflow/`. All modules implement EXACTLY these interfaces —
other modules are written concurrently against them.

## Dependencies (pyproject)
faster-whisper, sounddevice, numpy, pynput, pyperclip, requests, fastapi, uvicorn, python-multipart

## localflow/config.py
```python
from dataclasses import dataclass

@dataclass
class Config:
    model_size: str = "small"          # faster-whisper model
    language: str | None = None        # None = autodetect
    ollama_url: str = "http://localhost:11434"
    ollama_model: str = "llama3.2:3b"
    cleanup_enabled: bool = True
    hotkey: str = "<cmd>+<shift>+<space>"   # pynput GlobalHotKeys format
    sample_rate: int = 16000
    server_host: str = "0.0.0.0"
    server_port: int = 8756

def load_config() -> Config:
    """Read ~/.localflow.toml (tomllib) if present, override defaults; missing file OK."""
```

## localflow/audio.py
```python
import numpy as np

class Recorder:
    def __init__(self, sample_rate: int = 16000): ...
    def start(self) -> None:
        """Begin capturing mono float32 from default mic via sounddevice.InputStream."""
    def stop(self) -> np.ndarray:
        """Stop capture, return full clip as 1-D float32 np array at sample_rate."""
    @property
    def recording(self) -> bool: ...
```

## localflow/stt.py
```python
class Transcriber:
    def __init__(self, model_size: str = "small", language: str | None = None):
        """faster-whisper WhisperModel, device='cpu', compute_type='int8'. Load once."""
    def transcribe(self, audio: "np.ndarray") -> str:
        """audio: 1-D float32 16kHz. Use vad_filter=True. Return joined stripped text."""
```

## localflow/cleanup.py
```python
class Cleaner:
    def __init__(self, url: str, model: str): ...
    def clean(self, text: str) -> str:
        """POST {url}/api/generate (stream=False), prompt: fix punctuation/casing,
        remove filler words (um, uh), do NOT change wording/meaning, return only the
        cleaned text. Timeout 15s. On ANY error (conn refused, timeout, bad json):
        return input text unchanged."""
```

## localflow/inject.py
```python
def paste_text(text: str) -> None:
    """Save current clipboard (pyperclip), set text, osascript keystroke cmd+v
    (System Events), sleep 0.3, restore old clipboard. Never raise."""
```

## localflow/hotkey.py
```python
from collections.abc import Callable

class HotkeyListener:
    def __init__(self, combo: str, on_toggle: Callable[[], None]): ...
    def run_forever(self) -> None:
        """pynput.keyboard.GlobalHotKeys with {combo: on_toggle}; blocks."""
```

## localflow/app.py
```python
def main() -> None:
    """Desktop entrypoint (console script `localflow`).
    load_config; instantiate Transcriber (print 'loading model…' first), Recorder,
    Cleaner, HotkeyListener. on_toggle: if not recording -> start, print '● recording';
    else stop -> audio; skip if < 0.3s; transcribe; if cleanup_enabled clean;
    if text: paste_text; print transcript + elapsed ms. Do STT work in a worker
    thread so hotkey listener never blocks. Print startup banner with hotkey and
    macOS permission hints (mic + accessibility). Handle KeyboardInterrupt cleanly."""
```

## localflow/server.py
```python
app = FastAPI()
# GET /            -> serve localflow/static/index.html
# POST /transcribe -> multipart file upload (webm/ogg/wav/m4a). Decode to 16k mono
#                     float32 via ffmpeg subprocess (ffmpeg -i pipe:0 -f f32le -ac 1
#                     -ar 16000 pipe:1). Reuse ONE module-level Transcriber + Cleaner
#                     (lazy init from load_config). Optional query param clean=1.
#                     Return {"text": ..., "ms": int}.
def main() -> None: ...  # console script `localflow-server`, uvicorn on config host/port
```

## localflow/static/index.html
Single self-contained page, no external assets. Big record/stop button, MediaRecorder
capture, POST blob to /transcribe?clean=1, show text + copy-to-clipboard button,
running list of past transcripts. Mobile-friendly (large tap targets, viewport meta).
Note in small print: mic needs HTTPS or localhost (use `tailscale serve`).

## pyproject.toml
name localflow, version 0.1.0, requires-python >=3.11, deps above,
[project.scripts] localflow = "localflow.app:main", localflow-server = "localflow.server:main".
Build backend: hatchling.
