import logging
import queue
import sys
import threading
import time
from collections.abc import Sequence

import numpy as np

from localflow.audio import Recorder
from localflow.cleanup import Cleaner
from localflow.commands import discover_commands
from localflow.config import load_config
from localflow.hotkey import HoldKeyListener, HotkeyListener, is_hold_key
from localflow.inject import paste_text
from localflow.log import setup_logging
from localflow.providers.factory import build_llm, build_stt
from localflow.pushtotalk import PushToTalkController
from localflow.singleinstance import acquire, read_holder
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
    print(f"  llm model:    {config.llm_model}")
    print("-" * 60)
    print("macOS permissions required for this terminal app:")
    print("  System Settings -> Privacy & Security -> Microphone")
    print("  System Settings -> Privacy & Security -> Accessibility")
    print("=" * 60)


def main() -> None:
    """Desktop entrypoint (console script `localflow`)."""
    # Acquire the single-instance lock before anything else — in particular
    # before the (slow) model load — so a second launch fails fast instead
    # of silently sharing the hotkey and mic with an already-running
    # instance (see incident: transcriptions degraded to 0 chars).
    lock = acquire("localflow")
    if lock is None:
        pid, since = read_holder("localflow")
        pid_desc = pid if pid is not None else "unknown"
        since_desc = since if since is not None else "unknown time"
        print(
            f"another localflow instance is already running (pid {pid_desc}, "
            f"since {since_desc}); if it is the login agent, stop it with "
            f"`launchctl bootout gui/$UID/com.cortexrnd.localflow` (or `lf agent "
            f"uninstall` to also stop starting on login) — plain `kill` makes "
            f"launchd relaunch it; otherwise kill {pid_desc}",
            file=sys.stderr,
        )
        # Exit 0: launchd KeepAlive={SuccessfulExit: false} treats nonzero
        # as a crash and would relaunch-loop while a manual instance holds
        # the lock.
        raise SystemExit(0)

    log_path = setup_logging()
    log.info("localflow starting")
    config = load_config()

    print("loading model…")
    transcriber = build_stt(config)
    print(f"  stt backend:  {transcriber.backend}")
    commands = discover_commands(config.command_names, config.skills_dirs)
    print(f"  commands:     {len(commands)} registered")
    recorder = Recorder(sample_rate=config.sample_rate)
    cleaner = Cleaner(build_llm(config))

    work_queue: queue.Queue[np.ndarray] = queue.Queue()

    def worker() -> None:
        while True:
            audio = work_queue.get()
            if audio is None:
                break
            try:
                _process_clip(audio, config, transcriber, cleaner, commands)
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


def _process_clip(
    audio: np.ndarray,
    config,
    transcriber: Transcriber,
    cleaner: Cleaner,
    commands: Sequence[str] = (),
) -> None:
    start = time.monotonic()
    text = transcriber.transcribe(audio)
    # Cleanup only pays off on real sentences; short fragments have nothing to fix.
    if config.cleanup_enabled and text and len(text.split()) >= 5:
        text = cleaner.clean(text)
    if config.spoken_symbols and text:
        text = apply_spoken_symbols(text, commands)
    elapsed_ms = int((time.monotonic() - start) * 1000)
    log.info("transcribed %d chars in %dms", len(text or ""), elapsed_ms)
    if text:
        paste_text(text, config.paste_method)
        print(f"{text}  ({elapsed_ms}ms)")


if __name__ == "__main__":
    main()
