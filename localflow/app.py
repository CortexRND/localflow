import logging
import queue
import threading
import time

import numpy as np

from localflow.audio import Recorder
from localflow.cleanup import Cleaner
from localflow.config import load_config
from localflow.hotkey import HoldKeyListener, HotkeyListener, is_hold_key
from localflow.inject import paste_text
from localflow.log import setup_logging
from localflow.pushtotalk import PushToTalkController
from localflow.stt import Transcriber
from localflow.symbols import apply_spoken_symbols

log = logging.getLogger("localflow.app")


def _print_banner(config) -> None:
    print("=" * 60)
    print("localflow — local push-to-talk dictation")
    print("=" * 60)
    mode = "hold to talk" if is_hold_key(config.hotkey) else "toggle"
    print(f"  hotkey:       {config.hotkey} ({mode})")
    print(f"  model:        {config.model_size}")
    print(f"  ollama model: {config.ollama_model}")
    print("-" * 60)
    print("macOS permissions required for this terminal app:")
    print("  System Settings -> Privacy & Security -> Microphone")
    print("  System Settings -> Privacy & Security -> Accessibility")
    print("=" * 60)


def main() -> None:
    """Desktop entrypoint (console script `localflow`)."""
    log_path = setup_logging()
    log.info("localflow starting")
    config = load_config()

    print("loading model…")
    transcriber = Transcriber(
        model_size=config.model_size,
        language=config.language,
        backend=config.stt_backend,
    )
    print(f"  stt backend:  {transcriber.backend}")
    recorder = Recorder(sample_rate=config.sample_rate)
    cleaner = Cleaner(
        config.ollama_url,
        config.ollama_model,
        timeout=config.cleanup_timeout,
        num_ctx=config.cleanup_num_ctx,
    )

    work_queue: "queue.Queue[np.ndarray]" = queue.Queue()

    def worker() -> None:
        while True:
            audio = work_queue.get()
            if audio is None:
                break
            try:
                _process_clip(audio, config, transcriber, cleaner)
            except Exception:
                log.exception("transcription pipeline failed")
            work_queue.task_done()

    worker_thread = threading.Thread(target=worker, daemon=True)
    worker_thread.start()

    # Owns the real recorder.start()/stop() pipeline on its own daemon
    # thread; note_start/note_stop/note_toggle below are enqueue-only and
    # safe to call directly from the pynput event-tap callback thread.
    controller = PushToTalkController(recorder, config, work_queue)

    _print_banner(config)
    print(f"  log file:     {log_path}")

    if is_hold_key(config.hotkey):
        listener = HoldKeyListener(config.hotkey, controller.note_start, controller.note_stop)
    else:
        listener = HotkeyListener(config.hotkey, controller.note_toggle)
    try:
        listener.run_forever()
    except KeyboardInterrupt:
        print("\nexiting…")
        return

    # run_forever() returned on its own — the listener died (macOS disabled
    # the event tap after a timeout, or pynput crashed). Exiting 0 here
    # would look like a clean shutdown to launchd's KeepAlive={SuccessfulExit:
    # false} and the always-on agent would never be relaunched, so exit
    # non-zero instead.
    log.error("hotkey listener exited unexpectedly — exiting non-zero for launchd to restart")
    raise SystemExit(1)


def _process_clip(audio: np.ndarray, config, transcriber: Transcriber, cleaner: Cleaner) -> None:
    start = time.monotonic()
    text = transcriber.transcribe(audio)
    # Cleanup only pays off on real sentences; short fragments have nothing to fix.
    if config.cleanup_enabled and text and len(text.split()) >= 5:
        text = cleaner.clean(text)
    if config.spoken_symbols and text:
        text = apply_spoken_symbols(text)
    elapsed_ms = int((time.monotonic() - start) * 1000)
    log.info("transcribed %d chars in %dms", len(text or ""), elapsed_ms)
    if text:
        paste_text(text)
        print(f"{text}  ({elapsed_ms}ms)")


if __name__ == "__main__":
    main()
