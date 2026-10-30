import logging
import queue
import sys
import threading
import time
from collections.abc import Callable, Sequence

import numpy as np

from localflow import sounds
from localflow.audio import Recorder, rms_dbfs
from localflow.cleanup import Cleaner
from localflow.commands import discover_commands
from localflow.config import load_config
from localflow.context import AppInfo, category_for
from localflow.dictionary import (
    DictionaryStore,
    apply_replacements,
    stt_prompt,
)
from localflow.formatting import apply_spoken_formatting, apply_style
from localflow.history import HistoryStore
from localflow.hotkey import KeyboardHub, is_hold_key
from localflow.hotphrases import HotPhraseStore, render_dictation
from localflow.inject import paste_text
from localflow.jobs import Job
from localflow.log import setup_logging
from localflow.providers.factory import build_llm, build_stt
from localflow.pushtotalk import PushToTalkController
from localflow.singleinstance import acquire, read_holder
from localflow.status import write_last_text, write_state
from localflow.stt import Transcriber
from localflow.symbols import apply_spoken_symbols
from localflow.transform import Transformer, command_prompt, transform_prompt

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
    hot_phrases = HotPhraseStore()
    if config.hot_phrases:
        print(f"  hot phrases:  {len(hot_phrases.enabled())} enabled")
    recorder = Recorder(
        sample_rate=config.sample_rate,
        device=config.input_device or None,
    )
    llm = build_llm(config)
    cleaner = Cleaner(llm)
    transformer = Transformer(llm)

    work_queue: queue.Queue[Job | None] = queue.Queue()
    dictionary_store = DictionaryStore()
    history_store = HistoryStore(enabled=config.history_enabled)
    write_state("idle")
    controller = PushToTalkController(
        recorder, config, work_queue, on_state=write_state,
        history_store=history_store,
    )

    def worker() -> None:
        while True:
            item = work_queue.get()
            if item is None:
                break
            text, error = _process_job(
                item,
                config,
                transcriber,
                cleaner,
                transformer,
                commands,
                hot_phrases,
                dictionary_store,
                history_store,
            )
            if text:
                write_last_text(text)
            controller.note_job_result(
                text,
                item.clipboard_before,
                error=error,
                clipboard_expected=item.selection,
            )
            work_queue.task_done()

    worker_thread = threading.Thread(target=worker, daemon=True)
    worker_thread.start()

    _print_banner(config)
    print(f"  log file:     {log_path}")

    combos = [
        (shortcut, callback)
        for shortcut, callback in (
            (config.paste_last_hotkey, controller.note_paste_last),
            (config.polish_hotkey, lambda: controller.note_transform("polish")),
            (config.prompt_hotkey, lambda: controller.note_transform("prompt")),
        )
        if shortcut
    ]
    listener = KeyboardHub(
        hold_key=config.hotkey if is_hold_key(config.hotkey) else None,
        toggle_combo=None if is_hold_key(config.hotkey) else config.hotkey,
        combos=combos,
        on_down=controller.note_down,
        on_up=controller.note_up,
        on_toggle=controller.note_toggle,
        on_cancel=controller.note_cancel,
        command_key=config.command_hotkey,
        on_command_down=controller.note_command_down,
        on_command_up=controller.note_command_up,
        on_command_cancel=controller.note_command_cancel,
    )
    try:
        listener.run_forever()
    except KeyboardInterrupt:
        print("\nexiting…")
    finally:
        controller.shutdown()
        work_queue.put(None)
        worker_thread.join(timeout=1)

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
    hot_phrases: HotPhraseStore | None = None,
    app_info: AppInfo | None = None,
    dictionary_store: DictionaryStore | None = None,
    paste: bool = True,
    raw_text_callback: Callable[[str], None] | None = None,
) -> str:
    start = time.monotonic()
    if rms_dbfs(audio) < config.silence_threshold_dbfs:
        log.info("silence gate: audio below %.1f dBFS", config.silence_threshold_dbfs)
        return ""
    words = dictionary_store.words() if dictionary_store is not None else []
    pairs = dictionary_store.replacements() if dictionary_store is not None else []
    text = transcriber.transcribe(audio, prompt=stt_prompt(words))
    if raw_text_callback is not None:
        raw_text_callback(text)
    if text:
        phrases = (
            hot_phrases.enabled()
            if (hot_phrases is not None and config.hot_phrases)
            else []
        )
        # Cleanup only pays off on real sentences; short fragments have nothing to fix.
        category = category_for(app_info, config.app_category_overrides)
        style_name = (
            getattr(config, f"style_{category}") if config.styles_enabled else None
        )
        level = config.cleanup_level if config.cleanup_enabled else "none"
        clean = None
        if level != "none" and len(text.split()) >= 5:
            clean = lambda value: cleaner.clean(
                value,
                level=level,
                style=style_name,
                vocabulary=words,
            )
        symbols = (
            (lambda t: apply_spoken_symbols(t, commands))
            if config.spoken_symbols
            else None
        )
        style = (
            (lambda value: apply_style(value, style_name, words))
            if style_name
            else None
        )
        text = render_dictation(
            text,
            phrases,
            replace=lambda value: apply_replacements(value, pairs),
            fmt=apply_spoken_formatting if config.spoken_punctuation else None,
            clean=clean,
            symbols=symbols,
            style=style,
        )
    elapsed_ms = int((time.monotonic() - start) * 1000)
    log.info("transcribed %d chars in %dms", len(text or ""), elapsed_ms)
    if text:
        if paste:
            paste_text(text, config.paste_method)
        print(f"{text}  ({elapsed_ms}ms)")
    return text


def _process_job(
    item: Job,
    config,
    transcriber: Transcriber,
    cleaner: Cleaner,
    transformer: Transformer,
    commands: Sequence[str],
    hot_phrases: HotPhraseStore,
    dictionary_store: DictionaryStore,
    history_store: HistoryStore,
) -> tuple[str | None, bool]:
    raw_text = ""
    text = None
    status = "failed"
    error = False
    duration_s = (
        len(item.audio) / config.sample_rate
        if item.audio is not None and config.sample_rate
        else 0
    )
    audio_path = None
    if (
        item.kind == "dictate"
        and history_store.enabled
        and item.audio is not None
    ):
        try:
            audio_path = history_store.write_clip(item.audio, config.sample_rate)
        except Exception:
            log.warning("could not store dictation clip", exc_info=True)

    try:
        if item.kind == "dictate":
            captured = []
            text = _process_clip(
                item.audio,
                config,
                transcriber,
                cleaner,
                commands,
                hot_phrases,
                app_info=item.app_info,
                dictionary_store=dictionary_store,
                paste=False,
                raw_text_callback=captured.append,
            )
            raw_text = captured[0] if captured else ""
            status = "ok" if text else "silent"
        elif (
            item.kind == "command"
            and config.llm_provider == "ollama"
            and (not config.cleanup_enabled or not config.llm_model)
        ):
            message = "command mode needs a local LLM — choose one in lf setup"
            log.error(message)
            print(message)
            error = True
        else:
            if item.kind == "command":
                raw_text = transcriber.transcribe(item.audio, prompt=None)
                if not raw_text.strip():
                    log.error("command mode received an empty instruction")
                    error = True
                    status = "silent"
                else:
                    text = transformer.run(command_prompt(raw_text, item.selection))
            else:
                raw_text = item.selection
                text = transformer.run(transform_prompt(item.preset, item.selection))
            if text:
                status = "ok"
            elif status != "silent":
                status = "failed"
                error = True
    except Exception:
        log.exception("transcription pipeline failed")
        status = "failed"
        error = True

    if history_store.enabled:
        if status == "silent":
            history_store.delete_clip(audio_path)
            audio_path = None
        info = item.app_info
        entry_id = None
        try:
            entry_id = history_store.add(
                kind=item.kind,
                app_name=info.name if info else None,
                bundle_id=info.bundle_id if info else None,
                category=category_for(info, config.app_category_overrides),
                duration_s=duration_s,
                raw_text=raw_text,
                final_text=text or "",
                status=status,
                audio_path=audio_path,
            )
        except Exception:
            log.exception("could not save dictation history")
        if item.kind == "dictate" and status == "ok" and entry_id is not None:
            try:
                history_store.prune_audio_clips(
                    getattr(config, "keep_audio_clips", 5)
                )
                streak = history_store.stats()["current_streak_days"]
                if streak in {3, 7, 30, 100} and history_store.claim_milestone(streak):
                    sounds.play("achievement")
            except Exception:
                log.exception("could not update dictation insights")
    return text, error


def retry_history_entry(
    entry_id: int,
    history_store: HistoryStore,
    config=None,
    transcriber: Transcriber | None = None,
    cleaner: Cleaner | None = None,
    dictionary_store: DictionaryStore | None = None,
    hot_phrases: HotPhraseStore | None = None,
) -> str:
    entry = history_store.get(entry_id)
    if entry is None or entry["kind"] != "dictate" or not entry["audio_path"]:
        raise ValueError("history entry has no retryable audio clip")
    config = config or load_config()
    audio, _sample_rate = history_store.load_clip(entry["audio_path"])
    transcriber = transcriber or build_stt(config)
    cleaner = cleaner or Cleaner(build_llm(config))
    dictionary_store = dictionary_store or DictionaryStore()
    hot_phrases = hot_phrases or HotPhraseStore()
    app_info = (
        AppInfo(entry["app_name"] or "", entry["bundle_id"] or "")
        if entry["app_name"] or entry["bundle_id"]
        else None
    )
    captured = []
    try:
        text = _process_clip(
            audio,
            config,
            transcriber,
            cleaner,
            discover_commands(config.command_names, config.skills_dirs),
            hot_phrases=hot_phrases,
            app_info=app_info,
            dictionary_store=dictionary_store,
            paste=False,
            raw_text_callback=captured.append,
        )
    except Exception:
        history_store.update(
            entry_id,
            status="failed",
            raw_text=captured[0] if captured else entry["raw_text"],
            final_text="",
        )
        raise
    if not text:
        history_store.update(
            entry_id,
            status="failed",
            raw_text=captured[0] if captured else entry["raw_text"],
            final_text="",
        )
        return ""
    history_store.update(
        entry_id,
        status="ok",
        raw_text=captured[0] if captured else "",
        final_text=text,
    )
    write_last_text(text)
    history_store.prune_audio_clips(getattr(config, "keep_audio_clips", 5))
    return text


if __name__ == "__main__":
    main()
