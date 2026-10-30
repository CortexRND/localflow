"""Menu bar app: localflow status + meeting control in the macOS status bar.

Run with `lf menubar`. Talks to the localflow server over HTTP; the server
must be running (`lf serve`) for meeting features, but the menu bar app
stays up regardless and shows server state.
"""

import subprocess
import sys
import webbrowser

import requests
import rumps

from localflow.config import load_config
from localflow.history import HistoryStore
from localflow.inject import paste_text
from localflow.platform import current
from localflow.status import live_state, read_last_text, title_prefix

_config = load_config()
_history = HistoryStore(enabled=_config.history_enabled)
BASE = f"http://127.0.0.1:{_config.server_port}"

IDLE_ICON = "🎙"
DETECTED_ICON = "🎙❗"
LIVE_ICON = "🔴"
DOWN_ICON = "🎙⚫"


class LocalflowMenuBar(rumps.App):
    def __init__(self):
        super().__init__(IDLE_ICON, quit_button="Quit localflow menu bar")
        self.status_item = rumps.MenuItem("server: …")
        self.toggle_item = rumps.MenuItem(
            "Start meeting transcription", callback=self.toggle_meeting
        )
        self.dismiss_item = rumps.MenuItem(
            "Dismiss detection", callback=self.dismiss
        )
        self.open_ui_item = rumps.MenuItem("Open web UI", callback=self.open_ui)
        self.phrases_item = rumps.MenuItem("Hot Phrases…", callback=self.open_phrases)
        self.hub_item = rumps.MenuItem("LocalFlow Hub…", callback=self.open_hub)
        self.open_notes_item = rumps.MenuItem(
            "Open notes folder", callback=self.open_notes
        )
        self.paste_last_item = rumps.MenuItem(
            "Paste last transcript", callback=self.paste_last
        )
        self.menu = [
            self.status_item,
            None,
            self.toggle_item,
            self.dismiss_item,
            None,
            self.open_ui_item,
            self.phrases_item,
            self.hub_item,
            self.open_notes_item,
            self.paste_last_item,
        ]
        self._session_active = False
        self._was_detected = False
        self._base_icon = IDLE_ICON
        self.timer = rumps.Timer(self.refresh, 3)
        self.timer.start()
        self.status_timer = rumps.Timer(self.refresh_dictation_status, 0.5)
        self.status_timer.start()

    # ------------------------------------------------------------- polling ---

    def refresh(self, _timer=None) -> None:
        try:
            st = requests.get(BASE + "/meeting/status", timeout=3).json()
        except Exception:
            self._base_icon = DOWN_ICON
            self.refresh_dictation_status()
            self.status_item.title = "server: down (lf serve)"
            self._session_active = False
            return

        session = st.get("session")
        self._session_active = bool(session)
        if session:
            minutes, seconds = divmod(session["seconds"], 60)
            self._base_icon = LIVE_ICON
            self.status_item.title = (
                f"recording: {session['title']} — "
                f"{minutes}m{seconds:02d}s, {session['segment_count']} segments"
            )
            self.toggle_item.title = "Stop & save notes"
        else:
            self.toggle_item.title = "Start meeting transcription"
            if st.get("detected"):
                self._base_icon = DETECTED_ICON
                platform = st.get("platform") or "?"
                self.status_item.title = f"meeting detected ({platform})"
                if not self._was_detected:
                    rumps.notification(
                        "localflow", "Meeting detected",
                        f"{platform} — start transcription from the menu bar.",
                    )
            else:
                self._base_icon = IDLE_ICON
                busy = "mic in use" if st.get("mic_busy") else "idle"
                self.status_item.title = f"watching · {busy}"
        self._was_detected = bool(st.get("detected"))
        self.refresh_dictation_status()

    def refresh_dictation_status(self, _timer=None) -> None:
        self.title = f"{title_prefix(live_state())}{self._base_icon}"

    # ------------------------------------------------------------- actions ---

    def toggle_meeting(self, _item) -> None:
        try:
            if self._session_active:
                self.title = IDLE_ICON
                self.status_item.title = "summarizing & saving…"
                resp = requests.post(BASE + "/meeting/stop", timeout=600)
                resp.raise_for_status()
                saved = resp.json()
                rumps.notification(
                    "localflow", "Notes saved", saved["notes_path"]
                )
            else:
                resp = requests.post(
                    BASE + "/meeting/start",
                    json={"title": "Meeting", "category": ""},
                    timeout=10,
                )
                resp.raise_for_status()
        except Exception as exc:
            rumps.notification("localflow", "Error", str(exc)[:120])
        self.refresh()

    def dismiss(self, _item) -> None:
        try:
            requests.post(BASE + "/meeting/dismiss", timeout=3)
        except Exception:
            pass
        self.refresh()

    def open_ui(self, _item) -> None:
        webbrowser.open(BASE)

    def open_phrases(self, _item) -> None:
        # A separate process: rumps owns this process's AppKit main loop, so
        # the Tk window can't run in-process.
        subprocess.Popen([sys.executable, "-m", "localflow.phrases_window"])

    def open_hub(self, _item) -> None:
        subprocess.Popen([sys.executable, "-m", "localflow.hub_window"])

    def open_notes(self, _item) -> None:
        from pathlib import Path

        folder = Path(_config.vault_path).expanduser() / _config.notes_folder
        folder.mkdir(parents=True, exist_ok=True)
        current().open_path(str(folder))

    def paste_last(self, _item) -> None:
        text = _history.latest_text(read_last_text())
        if text:
            paste_text(text, _config.paste_method)


def main() -> None:
    LocalflowMenuBar().run()


if __name__ == "__main__":
    main()
