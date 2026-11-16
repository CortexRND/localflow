import logging
import queue
import sys
import threading
import time
from collections.abc import Callable, Sequence

import numpy as np

from localflow import sounds, stt_lease
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
from localflow.live_config import (
    HOTKEY_FIELDS,
    RUNTIME_FIELDS,
    RUNTIME_GROUPS,
    ConfigWatcher,
    adopt_groups,
    merge_live,
    runtime_changes,
)
from localflow.log import setup_logging
from localflow.logfiles import prepare_process_logs
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


def keyboard_bindings(config, controller) -> dict:
    """KeyboardHub binding arguments for `config` (construction and rebinds)."""
    hold = is_hold_key(config.hotkey)
    combos = [
        (shortcut, callback)
        for shortcut, callback in (
            (config.paste_last_hotkey, controller.note_paste_last),
            (config.polish_hotkey, lambda: controller.note_transform("polish")),
            (config.prompt_hotkey, lambda: controller.note_transform("prompt")),
        )
        if shortcut
    ]
    return {
        "hold_key": config.hotkey if hold else None,
        "toggle_combo": None if hold else config.hotkey,
        "combos": combos,
        "command_key": config.command_hotkey,
    }


def _speech_selection(config) -> tuple:
    """The speech-group fields that pick which speech model loads."""
    return tuple(getattr(config, name) for name in RUNTIME_GROUPS["stt"])


class RuntimeModels:
    """The speech model and LLM clients the dictation worker uses, swappable.

    Each job takes one `snapshot()` and keeps it to the end, so a reload never
    changes a model under a job in progress. LLM clients cost nothing to build
    (no network) and are swapped at once. A new speech model loads (and may
    download) on its own background thread, never the UI, hotkey or watcher
    thread; it is swapped in only when `can_swap()` (nothing recording, no job
    queued or running), else on a later `retry()`. Until the swap the old model
    keeps working and keeps its long-lived STT lease; the loaded one holds a
    scoped lease, so Free up space can delete neither. A failed load keeps the
    old model and is remembered (unless a newer choice superseded it) so that
    `retry_failed_stt` - the user picking the same model again - loads it once
    more; nothing retries it on its own.
    """

    def __init__(
        self, transcriber, cleaner, transformer, *, can_swap=lambda: True,
        build_stt_fn=None, build_llm_fn=None, lease=stt_lease, start_thread=None,
    ):
        self._lock = threading.Lock()
        self.transcriber = transcriber
        self.cleaner = cleaner
        self.transformer = transformer
        self.can_swap = can_swap
        self._build_stt = build_stt_fn or build_stt
        self._build_llm = build_llm_fn or build_llm
        self._lease = lease
        self._start_thread = start_thread or (
            lambda target: threading.Thread(
                target=target, name="localflow-stt-reload", daemon=True
            ).start()
        )
        self._wanted = None      # newest config whose speech model should load
        self._loading = False
        self._ready = None       # (transcriber, config, scoped lease) awaiting a swap
        self._failed = None      # speech selection of the newest choice, if its load failed

    @property
    def stt_pending(self) -> bool:
        with self._lock:
            return self._loading or self._ready is not None

    def snapshot(self):
        with self._lock:
            return self.transcriber, self.cleaner, self.transformer

    def reload_llm(self, config) -> None:
        llm = self._build_llm(config)
        cleaner, transformer = Cleaner(llm), Transformer(llm)
        with self._lock:
            self.cleaner, self.transformer = cleaner, transformer
        log.info("cleanup and command model now %s / %s", config.llm_provider, config.llm_model)

    def reload_stt(self, config) -> None:
        self._request_stt(config, only_if_failed=False)

    def retry_failed_stt(self, config) -> bool:
        """Load `config`'s speech model again if it is the newest choice and
        its load failed; True when a load was requested. A model that is
        serving, loading or loaded and waiting for a swap is left alone."""
        return self._request_stt(config, only_if_failed=True)

    def _request_stt(self, config, *, only_if_failed) -> bool:
        with self._lock:
            if only_if_failed and self._failed != _speech_selection(config):
                return False
            self._failed = None
            self._wanted = config
            if self._loading:
                return True  # the running loader picks up the newest choice next
            self._loading = True
        self._start_thread(self._load_loop)
        return True

    def _load_loop(self) -> None:
        while True:
            with self._lock:
                config, self._wanted = self._wanted, None
                if config is None:
                    self._loading = False
                    return
            # Protect the chosen model before it downloads/loads, even if the
            # saved choice changes meanwhile (auto/plug-in: every cached model).
            selection = self._lease.record(
                stt_lease.selection_backend(config.stt_backend), config.model_size,
                scoped=True,
            )
            try:
                transcriber = self._build_stt(config)
            except Exception:
                log.exception("could not load the new speech model; keeping the current one")
                if selection is not None:
                    self._lease.release(selection)
                with self._lock:
                    if self._wanted is None:  # not superseded by a newer choice
                        self._failed = _speech_selection(config)
                continue
            scoped = self._lease.record(
                str(getattr(transcriber, "backend", "unknown")), config.model_size,
                scoped=True,
            )
            # Handed over to the lease naming what actually loaded.
            if selection is not None:
                self._lease.release(selection)
            with self._lock:
                superseded, self._ready = self._ready, (transcriber, config, scoped)
            if superseded is not None and superseded[2] is not None:
                self._lease.release(superseded[2])
            self.retry()

    def retry(self) -> bool:
        """Swap in a loaded speech model if nothing is in progress."""
        with self._lock:
            ready = self._ready
        if ready is None or not self.can_swap():
            return False
        transcriber, config, scoped = ready
        with self._lock:
            if self._ready is not ready:
                return False
            self._ready = None
            self.transcriber = transcriber
        self._lease.record(str(getattr(transcriber, "backend", "unknown")), config.model_size)
        if scoped is not None:
            self._lease.release(scoped)
        log.info("speech model now %s / %s", config.stt_backend, config.model_size)
        return True


class LiveSettings:
    """The running dictation app's config, refreshed when the file changes.

    Jobs read `config` per item; the controller gets the new config on its
    own consumer thread; shortcut changes are rebound in place on the one
    KeyboardHub (deferred while a recording or key is in progress). The
    allowlisted live fields change via live_config.merge_live; the model
    groups (LLM provider/endpoint/model/key ref, speech provider/model) are
    adopted whole and their clients rebuilt by `models` (RuntimeModels). The
    recorder and listener are never rebuilt here.
    """

    def __init__(self, config, controller, hub=None, models=None):
        self.config = config
        self.controller = controller
        self.hub = hub
        self.models = models
        self._lock = threading.Lock()

    def apply(self, fresh) -> frozenset:
        with self._lock:
            merged, changed = merge_live(self.config, fresh, reloaded=RUNTIME_FIELDS)
            groups = runtime_changes(merged, fresh)
            if not changed and not groups:
                # The file was saved with nothing different, as when the Hub
                # re-saves a re-picked speech model: the one signal to retry
                # that model if its load failed (never on watcher ticks).
                if self.models is not None and self.models.retry_failed_stt(merged):
                    log.info("retrying the speech model that failed to load")
                return changed
            if groups:
                runtime = frozenset(
                    name for group in groups for name in RUNTIME_GROUPS[group]
                    if getattr(merged, name) != getattr(fresh, name)
                )
                merged = adopt_groups(merged, fresh, groups)
                changed = changed | runtime
            self.config = merged
            self.controller.update_config(merged)
            if changed & HOTKEY_FIELDS and self.hub is not None:
                now = self.hub.reconfigure(**keyboard_bindings(merged, self.controller))
                if not now:
                    log.info("shortcut change waits until dictation is idle")
            if self.models is not None:
                if "llm" in groups:
                    try:
                        self.models.reload_llm(merged)
                    except Exception:
                        log.exception("could not rebuild the cleanup model client")
                if "stt" in groups:
                    self.models.reload_stt(merged)
            log.info("settings updated without restart: %s", ", ".join(sorted(changed)))
            return changed

    def retry(self) -> None:
        hub = self.hub
        if hub is not None and hub.reconfigure_pending:
            hub.apply_pending()
        if self.models is not None:
            self.models.retry()


def _worker_loop(work_queue, process_job, deliver_result) -> None:
    while True:
        item = work_queue.get()
        try:
            if item is None:
                return
            try:
                text, error = process_job(item)
            except Exception:
                log.exception("worker failed to process job")
                text, error = "", True
            if text:
                try:
                    write_last_text(text)
                except Exception:
                    log.exception("could not write last text")
            try:
                deliver_result(item, text, error)
            except Exception:
                log.exception("could not deliver worker result")
        except Exception:
            log.exception("worker iteration failed")
        finally:
            work_queue.task_done()


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

    # Before logging opens localflow.log: cap oversized logs (including the
    # launchd stdout/stderr files this process may be writing to).
    prepare_process_logs()
    log_path = setup_logging()
    log.info("localflow starting")
    config = load_config()
    live = None
    # Created now so an edit made while the model loads is still noticed;
    # started once the runtime below exists.
    watcher = ConfigWatcher(
        lambda fresh: live.apply(fresh) if live is not None else None,
        on_tick=lambda: live.retry() if live is not None else None,
    )

    print("loading model…")
    transcriber = build_stt(config)
    # Free up space reads this lease so the loaded model is never deleted
    # under us; RuntimeModels rewrites it only once a reloaded model is in use.
    stt_lease.record(transcriber.backend, config.model_size)
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
    models = RuntimeModels(
        transcriber, cleaner, transformer,
        # Swap the speech model only between dictations: nothing recording,
        # nothing queued or being transcribed.
        can_swap=lambda: controller.is_idle() and work_queue.unfinished_tasks == 0,
    )
    del transcriber
    live = LiveSettings(config, controller, models=models)

    def process_item(item):
        # One consistent set of models per job, whatever a reload does meanwhile.
        job_transcriber, job_cleaner, job_transformer = models.snapshot()
        return _process_job(
            item,
            live.config,
            job_transcriber,
            job_cleaner,
            job_transformer,
            commands,
            hot_phrases,
            dictionary_store,
            history_store,
        )

    def deliver_result(item, text, error):
        controller.note_job_result(
            text,
            item.clipboard_before,
            error=error,
            clipboard_expected=item.selection,
        )

    worker_thread = threading.Thread(
        target=_worker_loop,
        args=(work_queue, process_item, deliver_result),
        daemon=True,
    )
    worker_thread.start()

    _print_banner(config)
    print(f"  log file:     {log_path}")

    listener = KeyboardHub(
        **keyboard_bindings(config, controller),
        on_down=controller.note_down,
        on_up=controller.note_up,
        on_toggle=controller.note_toggle,
        on_cancel=controller.note_cancel,
        on_command_down=controller.note_command_down,
        on_command_up=controller.note_command_up,
        on_command_cancel=controller.note_command_cancel,
        can_reconfigure=controller.is_idle,
    )
    live.hub = listener
    watcher.start()
    try:
        listener.run_forever()
    except KeyboardInterrupt:
        print("\nexiting…")
    finally:
        watcher.stop()
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
                milestone_claimed = (
                    streak in {3, 7, 30, 100}
                    and history_store.claim_milestone(streak)
                )
                if milestone_claimed and getattr(config, "sounds_enabled", True):
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
    captured = []
    lease = None
    try:
        config = config or load_config()
        audio, _sample_rate = history_store.load_clip(entry["audio_path"])
        if transcriber is None:
            transcriber = build_stt(config)
            # A temporary model in this (Hub/CLI) process: lease it while the
            # retry runs so Free up space never deletes it mid-transcribe.
            # Scoped, so a long-lived lease of this pid is left untouched.
            lease = stt_lease.record(
                str(getattr(transcriber, "backend", "unknown")),
                config.model_size,
                scoped=True,
            )
        cleaner = cleaner or Cleaner(build_llm(config))
        dictionary_store = dictionary_store or DictionaryStore()
        hot_phrases = hot_phrases or HotPhraseStore()
        app_info = (
            AppInfo(entry["app_name"] or "", entry["bundle_id"] or "")
            if entry["app_name"] or entry["bundle_id"]
            else None
        )
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
        log.exception("retry failed for history entry %s", entry_id)
        raise
    finally:
        if lease is not None:
            stt_lease.release(lease)
    if not text:
        log.warning("retry produced no text for history entry %s", entry_id)
        raise RuntimeError("retry produced no text")
    history_store.update(
        entry_id,
        status="ok",
        raw_text=captured[0] if captured else "",
        final_text=text,
    )
    try:
        write_last_text(text)
    except Exception:
        log.exception("could not write retried text to last.txt")
    history_store.prune_audio_clips(getattr(config, "keep_audio_clips", 5))
    return text


if __name__ == "__main__":
    main()
