"""Render the AppKit hot-phrases window to PNGs (macOS only).

Used by the macOS UI workflow; writes artifacts/mac-ui/<state>-<light|dark>.png.
"""

import tempfile
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import numpy as np
from Foundation import NSMakeSize

from localflow import provider_keys
from localflow.config import Config
from localflow.dictionary import DictionaryStore
from localflow.dispatch import PromptQueue, WorkPrompt
from localflow.history import HistoryStore
from localflow.hotphrases import HotPhraseStore
from localflow.hub_mac import build_window as build_hub_window
from localflow.phrases_mac import build_window, snapshot_png
from localflow.phrases_model import PhrasesModel
from localflow.setup_flow import Check
from localflow.setup_mac import SetupWindowController
from localflow.ui_tokens import (
    HUB_DEFAULT_HEIGHT,
    HUB_DEFAULT_WIDTH,
    HUB_MIN_HEIGHT,
    HUB_MIN_WIDTH,
)

OUT = Path("artifacts/mac-ui")
WIDE_WIDTH = 1100
# Hub content sizes: the supported minimum and a wide window.
HUB_SIZES = (
    ("min", HUB_MIN_WIDTH, HUB_MIN_HEIGHT),
    ("wide", 1280, 820),
)


def _config(hot_phrases=True):
    return SimpleNamespace(
        hot_phrases=hot_phrases,
        spoken_symbols=True,
        spoken_punctuation=False,
        styles_enabled=False,
        style_other="formal",
        command_names=[],
        skills_dirs=[],
    )


def _controller(store_dir, hot_phrases=True):
    store = HotPhraseStore(store_dir / "hot_phrases.json")
    c = build_window(store, _config(hot_phrases))
    c.confirm_discard = lambda _err: True
    return c, store


def _shot(c, name):
    for dark in (False, True):
        snapshot_png(c, OUT / f"{name}-{'dark' if dark else 'light'}.png", dark=dark)


# (key, label, detail, fix_label, fix_url): the real checks run_checks reports on
# macOS. Fix labels are kept on every check, ready or not, so a regression that
# shows actions on ready rows is visible in the all-ready snapshot.
_CHECK_SPECS = (
    ("ffmpeg", "ffmpeg", "Installed", "Install ffmpeg",
     "https://formulae.brew.sh/formula/ffmpeg"),
    ("ollama-installed", "Ollama installed", "Installed", "Get Ollama",
     "https://ollama.com/download"),
    ("ollama-running", "Ollama running", "Ready", "Start Ollama",
     "https://ollama.com/download"),
    ("microphone-device", "Input device", "1 input device(s) found",
     "Check microphone", ""),
    ("microphone", "Microphone permission", "Unknown", "Open settings",
     "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"),
    ("accessibility", "Accessibility permission", "Not granted", "Open settings",
     "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"),
    ("input-monitoring", "Input Monitoring permission", "Not granted",
     "Open settings",
     "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"),
)
_NEEDS_ACTION = {"microphone": None, "accessibility": False, "input-monitoring": False}


def _seven_checks(all_ready=False):
    checks = []
    for key, label, detail, fix_label, fix_url in _CHECK_SPECS:
        ok = True if all_ready else _NEEDS_ACTION.get(key, True)
        if ok is True and key in _NEEDS_ACTION:
            detail = "Granted"
        checks.append(Check(key, label, ok, detail, fix_label, fix_url))
    return checks


def _setup_check_snapshots(setup):
    """Quick checkup states, including a wide window. Restores state after."""
    saved = (setup.step, setup.checks, setup.status_message, setup.error_message)
    window = setup.window
    size = window.contentView().frame().size
    setup.step = 1
    setup.status_message = setup.error_message = ""
    for name, all_ready in (
        ("setup-checks-needs-action", False),
        ("setup-checks-all-ready", True),
    ):
        setup.checks = _seven_checks(all_ready)
        setup._render()
        _shot(setup, name)
    setup.checks = _seven_checks()
    window.setContentSize_(NSMakeSize(WIDE_WIDTH, size.height))
    setup._render()  # rebuild at the current width; snapshot mode starts no workers
    _shot(setup, "setup-checks-needs-action-wide")
    window.setContentSize_(size)
    setup.step, setup.checks, setup.status_message, setup.error_message = saved
    setup._render()


def _hub_snapshots(base):
    for state in ("empty", "populated", "error"):
        history = HistoryStore(base / f"hub-{state}.sqlite", base / f"clips-{state}")
        dictionary = DictionaryStore(base / f"dictionary-{state}.json")
        config = Config()
        if state == "populated":
            history.add(
                kind="dictate", status="ok", app_name="Editor",
                raw_text="A raw transcript", final_text="A polished transcript.",
                duration_s=30,
                audio_path=history.write_clip(np.zeros(1600, dtype=np.float32), 16000),
            )
            history.add(
                kind="command", status="ok", app_name="Browser",
                raw_text="Make this concise", final_text="A concise selection.",
            )
            for index in range(10):
                history.add(
                    kind="dictate",
                    status="ok",
                    app_name="Editor",
                    raw_text=f"Raw history item {index + 1}",
                    final_text=f"History item {index + 1}.",
                    duration_s=12,
                )
            dictionary.add_word("LocalFlow")
            dictionary.add_replacement("teh", "the")
        controller = build_hub_window(history, dictionary, config)
        for page in ("Home", "History", "Dictionary", "Style", "Models"):
            controller.page_errors.clear()
            if state == "error":
                controller.page_errors[page] = "Unable to load local data."
            controller.show_page(page)
            _shot(controller, f"hub-{page.lower()}-{state}")

    class SnapshotMeetingClient:
        state = "idle"

        def templates(self):
            return {
                "default": "general",
                "templates": [
                    {"id": "general", "name": "General"},
                    {"id": "standup", "name": "Standup"},
                ],
            }

        def status(self):
            if self.state == "running":
                return {
                    "session": {
                        "title": "Planning", "template": "standup",
                        "user_notes": "Check launch date",
                        "system_audio": "error",
                        "system_audio_error": "System audio permission unavailable",
                        "seconds": 185, "segment_count": 2,
                        "segments": [
                            {"stamp": "00:04", "source": "me", "text": "I can take this"},
                            {"stamp": "01:12", "source": "others", "text": "Sounds good"},
                        ],
                    },
                    "last_saved": None, "chat_available": False,
                }
            if self.state == "saved":
                return {
                    "session": None,
                    "last_saved": {
                        "notes_path": "/Users/runner/Meeting notes/Planning.md",
                        "enhanced": True,
                    },
                    "chat_available": True,
                }
            return {
                "session": None, "last_saved": None, "chat_available": False,
            }

        def start(self, *_args):
            return {"ok": True}

        def save_notes(self, _notes):
            return {"ok": True}

        def stop(self):
            return {"notes_path": "/Users/runner/Meeting notes/Planning.md"}

        def chat(self, _question):
            return {"answer": "The launch date is March 3."}

        def dismiss(self):
            return None

    client = SnapshotMeetingClient()
    history = HistoryStore(base / "hub-meetings.sqlite", base / "hub-meetings-clips")
    dictionary = DictionaryStore(base / "hub-meetings-dictionary.json")
    controller = build_hub_window(
        history, dictionary, Config(), meeting_client=client
    )
    for state in ("idle", "running", "saved"):
        client.state = state
        if state == "saved":
            controller.meeting_notes_text = ""
            controller.meeting_notes_dirty = False
            controller._meeting_adopting_notes = True
            controller.meeting_notes.setString_("")
            controller._meeting_adopting_notes = False
        controller.show_page("Meetings")
        controller._meeting_event("templates", client.templates(), None)
        controller._meeting_event("status", client.status(), None)
        _shot(controller, f"hub-meetings-{state}")
        _hub_sized_shots(controller, f"hub-meetings-{state}")


def _hub_sized_shots(controller, name):
    """Snapshot the current page at the minimum and a wide window size.

    Each resize rebuilds the page at its new width, the same path a live
    resize takes. The window is put back afterwards.
    """
    for label, width, height in HUB_SIZES:
        controller.resize_content(width, height)
        _shot(controller, f"{name}-{label}")
    controller.resize_content(HUB_DEFAULT_WIDTH, HUB_DEFAULT_HEIGHT)


def _phrases_model(path):
    """A model over a temp store and a fixed config (no real skills dirs)."""
    return PhrasesModel(HotPhraseStore(path), _config())


def _hub_hot_phrases_snapshots(base):
    """The embedded Hot Phrases page: empty, populated + selected, invalid draft.

    Each state is built through the editor's own methods and shot at the
    minimum and a wide window size, light and dark. Stores and config are
    temp/injected, no runtime or main-app singleton is started, and no
    preview is typed (the preview would read the dictionary store).
    """
    history = HistoryStore(base / "hp-history.sqlite", base / "hp-clips")
    dictionary = DictionaryStore(base / "hp-dictionary.json")

    def hub(name, entries=()):
        model = _phrases_model(base / f"hp-{name}.json")
        for trigger, text, enabled in entries:
            model.store.add(trigger, text, enabled)
        return build_hub_window(
            history, dictionary, Config(), initial_page="Hot Phrases",
            phrases_model=model,
        )

    controller = hub("empty")
    _hub_sized_shots(controller, "hub-hot-phrases-empty")

    entries = (
        ("review checklist", "Review this PR:\n- tests\n- types\n- no churn", True),
        ("sign off", "Thanks — talk tomorrow.", False),
        ("standup", "yesterday:\ntoday:\nblockers:", True),
    )
    controller = hub("populated3", entries)
    editor = controller.phrases
    editor.select(next(e["id"] for e in editor.entries if e["trigger"] == "sign off"))
    controller._phrases_sync()
    _hub_sized_shots(controller, "hub-hot-phrases-populated3-selected")

    controller = hub("invalid", entries)
    editor = controller.phrases
    editor.new_phrase()
    # A trigger that collides with "review checklist" once normalised. fill()
    # types it for real; flush() validates through the model, writes nothing
    # and sets the inline error (so the settle loop cannot autosave it away).
    editor.fill("Review, checklist", "y")
    editor.flush()
    controller._phrases_sync()
    _hub_sized_shots(controller, "hub-hot-phrases-invalid-draft")

    # The Undo bar after a delete, and both notices at once. Only visible
    # notices take room, so "Try it" stays right below the enabled row in the
    # normal shots above and moves down only for these.
    controller = hub("undo", entries)
    editor = controller.phrases
    editor.select(next(e["id"] for e in editor.entries if e["trigger"] == "sign off"))
    editor.delete()
    controller._phrases_sync()
    _hub_sized_shots(controller, "hub-hot-phrases-undo")

    controller = hub("error-undo", entries)
    editor = controller.phrases
    editor.select(next(e["id"] for e in editor.entries if e["trigger"] == "sign off"))
    editor.delete()
    editor.new_phrase()
    editor.fill("Review, checklist", "y")
    editor.flush()
    controller._phrases_sync()
    _hub_sized_shots(controller, "hub-hot-phrases-error-and-undo")


def _hub_responsive_snapshots(base):
    """Home, Dictionary, Hot Phrases and Prompts at min and wide sizes.

    Everything uses temp stores and a temp queue: nothing touches the real
    dictionary, history, queue or config, and no network is involved.
    """
    history = HistoryStore(base / "resp-history.sqlite", base / "resp-clips")
    dictionary = DictionaryStore(base / "resp-dictionary.json")
    for index in range(4):
        history.add(
            kind="dictate", status="ok", app_name="Editor",
            raw_text=f"Raw item {index}", final_text=f"Dictation number {index}.",
            duration_s=20,
        )
    for word in ("LocalFlow", "Kubernetes", "Fireworks", "Orca", "PyObjC", "Obsidian",
                 "Whisper", "Ollama"):
        dictionary.add_word(word)
    for source, target in (("teh", "the"), ("my email", "me@example.com"),
                           ("lf", "LocalFlow"), ("k8s", "Kubernetes")):
        dictionary.add_replacement(source, target)
    empty_queue = PromptQueue(base / "queue-empty.json")
    queue = PromptQueue(base / "queue-two.json")
    _pending, failed = queue.add(
        [
            WorkPrompt("Migrate session storage to Redis", "Replace local files."),
            WorkPrompt("Update deploy docs for Redis", "Document the new dependency."),
        ],
        [base / "prompts" / "01.md", base / "prompts" / "02.md"],
        "Weekly planning",
    )
    queue.set_status(
        failed["id"], "failed", repo="localflow", error="Orca is not running",
    )

    # Hot Phrases reads the injected temp store, never ~/.localflow.
    controller = build_hub_window(
        history, dictionary, Config(), prompt_queue=empty_queue,
        phrases_model=_phrases_model(base / "resp-hot-phrases.json"),
    )
    controller.show_page("Home")
    _hub_sized_shots(controller, "hub-home-populated")
    from localflow.ask_model import Turn

    controller.show_page("Ask")
    ask_page = controller.ask_page
    def citation(number, source, title):
        return {
            "n": number, "source": source, "when": "2026-04-15T12:00", "title": title,
            "ref": str(base / "meeting.md") + "#01:20" if source == "meeting" else "1",
        }

    long_answer = (
        "We agreed to launch on Friday and finish the checklist first [1]. "
        "Marketing owns the announcement and engineering owns the rollout plan [2]. "
    ) * 4
    for state in (
        "empty", "conversation", "thinking", "error", "long-conversation", "citations",
    ):
        # Leave Ask first: show_page captures the previous state's views (draft,
        # scroll) on the way out, so the resets below are not overwritten.
        controller.show_page("Home")
        ask_page.model.clear()
        # Each state starts like a first visit: no remembered scroll offset.
        ask_page.scroll_y, ask_page.was_at_bottom, ask_page.draft = None, True, ""
        ask_page.busy = state == "thinking"
        ask_page.pending = (
            "And who owns the rollout checklist?" if ask_page.busy else ""
        )
        if state == "conversation":
            ask_page.model.turns.append(Turn(
                "What did we decide about the launch?",
                "We agreed to launch on Friday and finish the checklist first [1].",
                [citation(1, "meeting", "Launch planning")],
            ))
        elif state == "error":
            ask_page.model.turns.append(Turn(
                "What did we decide about the launch?",
                error="Couldn't reach the model or local sources. Check the Models page and try again.",
            ))
        elif state == "long-conversation":
            for number in range(1, 5):
                ask_page.model.turns.append(Turn(
                    f"Question {number}: what did we decide about the launch?",
                    long_answer, [citation(1, "meeting", "Launch planning")],
                ))
        elif state == "citations":
            ask_page.model.turns.append(Turn(
                "Where did we talk about the launch?", long_answer,
                [
                    citation(1, "meeting", "Launch planning"),
                    citation(2, "dictation", "A long dictation title about launch marketing"),
                    citation(3, "notes", "Launch checklist"),
                    citation(4, "meeting", "Rollout retrospective with engineering"),
                    citation(5, "notes", "Marketing announcement plan"),
                ],
            ))
        controller.show_page("Ask")
        _hub_sized_shots(controller, f"hub-ask-{state}")
    ask_page.busy = False
    controller.show_page("Hot Phrases")
    _hub_sized_shots(controller, "hub-hot-phrases")
    controller.show_page("Prompts")
    _hub_sized_shots(controller, "hub-prompts-empty")
    controller.prompt_queue = queue
    controller.show_page("Prompts")
    _hub_sized_shots(controller, "hub-prompts-two")

    controller.show_page("Dictionary")
    _hub_sized_shots(controller, "hub-dictionary-entries")
    # Real handlers: an empty word, then a half-filled replacement. The typed
    # text must survive and the error must sit inside the card it belongs to.
    controller.word_field.setStringValue_("")
    controller.addWord_(None)
    _hub_sized_shots(controller, "hub-dictionary-word-error")
    controller.from_field.setStringValue_("brb")
    controller.to_field.setStringValue_("")
    controller.addReplacement_(None)
    _hub_sized_shots(controller, "hub-dictionary-replacement-error")


def _hub_provider_snapshots(base):
    history = HistoryStore(base / "provider-history.sqlite", base / "provider-clips")
    dictionary = DictionaryStore(base / "provider-dictionary.json")
    config = Config()
    store = {}
    candidates = [
        provider_keys.ImportCandidate("fireworks", "environment: FIREWORKS_API_KEY", "fake-key-1234"),
        provider_keys.ImportCandidate("openai", "~/.zshrc: OPENAI_API_KEY", "fake-key-5678"),
    ]
    with (
        patch("localflow.secrets.get_secret", side_effect=lambda name: store.get(name, "")),
        patch("localflow.secrets.set_secret", side_effect=lambda name, value: store.__setitem__(name, value)),
        patch("localflow.secrets.delete_secret", side_effect=lambda name: store.pop(name, None)),
        patch("localflow.hub_mac.load_config", side_effect=lambda: config),
        patch("localflow.provider_keys.save_config"),
        patch("localflow.provider_keys.detect_imports", return_value=candidates),
        patch("localflow.provider_keys.fetch_models", return_value=[{"id": "example-model"}]),
        patch("localflow.provider_keys.test_llm", return_value=(True, "Provider connection successful")),
    ):
        controller = build_hub_window(history, dictionary, config)
        controller.provider_snapshot_mode = True
        controller.show_page("Models")
        # state: (keys saved + Fireworks active, expanded provider, import results)
        # Collapsed list; a single expanded active / inactive / keyless / custom
        # provider; import results with defaults ON (no saved key) and OFF
        # (already saved). Everything is the mocked store above: no Keychain,
        # no network.
        specs = {
            "no-keys": (False, None, False),
            "saved-active": (True, None, False),
            "import-results": (True, None, True),
            "import-defaults": (False, None, True),
            "test-success": (True, "ollama", False),
            "test-failure": (True, "ollama", False),
            "expanded-active": (True, "fireworks", False),
            "expanded-inactive": (True, "openai", False),
            "expanded-no-key": (False, "groq", False),
            "expanded-custom": (False, "custom", False),
        }
        for state, (saved, expanded, imports) in specs.items():
            # Leave Models first so show_page captures and tears down the
            # previous state's views before anything below is reset.
            controller.show_page("Home")
            store.clear()
            config = Config()
            controller.provider_candidates = []
            controller.provider_checked = {}
            controller.provider_message = ("", False)
            controller.provider_expanded = expanded
            if saved:
                provider_keys.save_imports(candidates)
                config = provider_keys.activate("fireworks", model="example-model", config=config)
            controller.config = config
            # Fresh per-provider state seeded from this state's config, so an
            # active provider really is active/unchanged (no stale drafts).
            controller.provider_state = {}
            for preset in provider_keys.PRESETS:
                controller._models_state(preset)
            if imports:
                controller.provider_candidates = provider_keys.detect_imports()
                controller.provider_message = ("Select the keys to save", True)
            if state in ("test-success", "test-failure"):
                result = (True, "Provider connection successful") if state == "test-success" else (
                    False, "Provider connection failed; check the key, base URL, and network"
                )
                with patch("localflow.provider_keys.test_llm", return_value=result):
                    ok, message = provider_keys.test_llm("ollama")
                    controller.provider_state["ollama"]["message"] = (message, ok)
            if expanded == "custom":
                controller.provider_state["custom"]["url"] = "https://llm.example.com/v1"
            for preset in provider_keys.PRESETS:
                controller.provider_state[preset.id]["models"] = [
                    item["id"] for item in provider_keys.fetch_models(preset.id)
                ]
            controller.show_page("Models")
            for label, width, height in HUB_SIZES:
                controller.resize_content(width, height)
                _shot(controller, f"hub-models-keys-{state}-{label}")


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    with (
        tempfile.TemporaryDirectory() as tmp,
        patch("localflow.secrets.get_secret", return_value=""),
        patch("localflow.secrets.set_secret"),
        patch("localflow.secrets.delete_secret"),
        # The Models page reads (and Use writes) the config: keep both off the
        # real ~/.localflow.
        patch("localflow.hub_mac.load_config", return_value=Config()),
        patch("localflow.provider_keys.save_config"),
        patch("localflow.provider_keys.fetch_models", return_value=[]),
        patch("localflow.provider_keys.test_llm", return_value=(False, "Snapshot: offline")),
    ):
        tmp = Path(tmp)

        c, _ = _controller(tmp / "empty")
        _shot(c, "empty")

        c, store = _controller(tmp / "list")
        store.add("review checklist", "Review this PR:\n- tests\n- types")
        store.add("sign off", "Thanks — talk tomorrow.", enabled=False)
        store.add("standup", "yesterday:\ntoday:\nblockers:")
        c.reload_sidebar()
        c.select_entry(store.list()[0]["id"])
        _shot(c, "list-editing")

        c, store = _controller(tmp / "validation")
        store.add("review checklist", "x")
        c.reload_sidebar()
        c.new_phrase()
        c.set_fields("Review, checklist", "y")
        c.flush_autosave()
        _shot(c, "validation-error")

        c, store = _controller(tmp / "test-drawer")
        store.add("review checklist", "CHECK: tests, lint, types")
        c.reload_sidebar()
        c.select_entry(store.list()[0]["id"])
        c.open_test_area(True)
        c.set_test_input("please review checklist now")
        _shot(c, "test-drawer-open")

        c, store = _controller(tmp / "undo")
        e = store.add("trig", "t")
        c.reload_sidebar()
        c.select_entry(e["id"])
        c.delete_current()
        _shot(c, "undo-bar")

        c, store = _controller(tmp / "search")
        store.add("review checklist", "Review this PR")
        store.add("sign off", "Thanks")
        store.add("standup", "yesterday:\ntoday:")
        c.reload_sidebar()
        c.set_search("stand")
        c.select_entry(store.list()[-1]["id"])
        _shot(c, "search")
        c.set_search("zzz")
        _shot(c, "search-no-matches")

        c, store = _controller(tmp / "off", hot_phrases=False)
        store.add("review checklist", "CHECK")  # banner needs the editor visible
        c.reload_sidebar()
        c.select_entry(store.list()[0]["id"])
        _shot(c, "hot-phrases-off-banner")

        setup = SetupWindowController.alloc().init()
        setup.snapshot_mode = True
        setup.checks = [
            Check("ffmpeg", "ffmpeg", True, "Installed", "", ""),
            Check("ollama-installed", "Ollama installed", True, "Installed", "", ""),
            Check("ollama-running", "Ollama running", True, "Ready", "", ""),
            Check("microphone-device", "Input device", True, "1 input device found", "", ""),
        ]
        setup.downloaded = {model.id: True for model in setup.models}
        setup.ollama_checked = True
        setup.ollama_running = True
        setup.ollama_models = ["llama3.2:1b"]
        setup.llm_id = "llama3.2:1b"
        setup.status_message = ""
        for step in range(7):
            setup.step = step
            setup._render()
            _shot(setup, f"setup-step-{step + 1}")
        _setup_check_snapshots(setup)
        setup.step = 6
        setup.saved = True
        setup._render()
        _shot(setup, "setup-complete")

        setup.saved = False
        setup.step = 3
        setup.cleanup_level = "medium"
        setup._render()
        _shot(setup, "setup-cleanup-level-medium")

        setup.choices = replace(setup.choices, llm_provider="openai-compatible")
        setup.step = 3
        setup.saved = False
        setup._render()
        _shot(setup, "setup-non-ollama-provider")

        setup.choices = replace(setup.choices, llm_provider="ollama")
        setup.step = 2
        setup.busy.add("stt-download")
        setup.status_message = "43 / 78 MB"
        setup.error_message = ""
        setup._render()
        _shot(setup, "setup-download-progress")

        setup.busy.clear()
        setup.status_message = "Download failed."
        setup.error_message = "Network connection interrupted."
        setup._render()
        _shot(setup, "setup-download-error")
        _hub_snapshots(tmp)
        _hub_responsive_snapshots(tmp)
        _hub_hot_phrases_snapshots(tmp)
        _hub_provider_snapshots(tmp)

    print(f"wrote snapshots to {OUT}")


if __name__ == "__main__":
    main()
