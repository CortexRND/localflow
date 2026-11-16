"""Menu bar app: localflow status + meeting control in the macOS status bar.

Run with `lf menubar`. Talks to the localflow server over HTTP; the server
must be running (`lf serve`) for meeting features, but the menu bar app
stays up regardless and shows server state.
"""

import subprocess
import sys

import requests
import rumps

from localflow.apitoken import load
from localflow.config import load_config
from localflow.history import HistoryStore
from localflow.inject import paste_text
from localflow.live_config import LazyConfig
from localflow.menubar_poll import REQUEST_TIMEOUT, MeetingStatusPoller
from localflow.platform import current
from localflow.server_url import configured_base_url
from localflow.status import LiveStateCache, read_last_text, title_prefix
from localflow.weblaunch import open_web_ui

_config = load_config()
# Live settings (paste method, ...) edited elsewhere are picked up on use.
_live_config = LazyConfig(_config)
_history = HistoryStore(enabled=_config.history_enabled)
BASE = configured_base_url(_config)
TICK_SECONDS = 0.5  # recording indicator latency; each tick is one stat()


def _auth_headers() -> dict[str, str]:
    token = load()
    return {"X-Localflow-Token": token} if token else {}


def _fetch_status() -> dict:
    """Runs on the poller's background thread, never the main thread."""
    response = requests.get(
        BASE + "/meeting/status", headers=_auth_headers(), timeout=REQUEST_TIMEOUT
    )
    return response.json()


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
        self.meeting_notes_item = rumps.MenuItem(
            "Meeting notes…", callback=self.open_meeting_notes
        )
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
            self.meeting_notes_item,
            self.open_notes_item,
            self.paste_last_item,
        ]
        self._session_active = False
        self._was_detected = False
        self._base_icon = IDLE_ICON
        # Last values pushed to AppKit: unchanged ones are not re-set, so an
        # idle tick causes no native redraw.
        self._shown: dict[str, str] = {}
        self._live_state = LiveStateCache()
        self._poller = MeetingStatusPoller(_fetch_status)
        # One main-thread timer: a stat of status.json, plus applying any
        # finished background status request (see menubar_poll).
        self.timer = rumps.Timer(self.tick, TICK_SECONDS)
        self.timer.start()

    # ------------------------------------------------------------- polling ---

    def _show(self, key: str, target, text: str) -> None:
        if self._shown.get(key) != text:
            self._shown[key] = text
            target.title = text

    def tick(self, _timer=None) -> None:
        result = self._poller.tick()
        if result is not None:
            self._apply_status(*result)
        self.refresh_dictation_status()

    def refresh(self, _timer=None) -> None:
        """Ask for a fresh server status soon (after a user action)."""
        self._poller.poke()

    def _apply_status(self, kind: str, st: dict | None) -> None:
        if kind == "down":
            self._base_icon = DOWN_ICON
            self._show("status", self.status_item, "server: down (lf serve)")
            self._session_active = False
            return

        session = st.get("session")
        self._session_active = bool(session)
        if session:
            minutes, seconds = divmod(session["seconds"], 60)
            self._base_icon = LIVE_ICON
            self._show(
                "status", self.status_item,
                f"recording: {session['title']} — "
                f"{minutes}m{seconds:02d}s, {session['segment_count']} segments",
            )
            self._show("toggle", self.toggle_item, "Stop & save notes")
        else:
            self._show("toggle", self.toggle_item, "Start meeting transcription")
            if st.get("detected"):
                self._base_icon = DETECTED_ICON
                platform = st.get("platform") or "?"
                self._show("status", self.status_item, f"meeting detected ({platform})")
                if not self._was_detected:
                    rumps.notification(
                        "localflow", "Meeting detected",
                        f"{platform} — start transcription from the menu bar.",
                    )
            else:
                self._base_icon = IDLE_ICON
                busy = "mic in use" if st.get("mic_busy") else "idle"
                self._show("status", self.status_item, f"watching · {busy}")
        self._was_detected = bool(st.get("detected"))

    def refresh_dictation_status(self, _timer=None) -> None:
        self._show("title", self, f"{title_prefix(self._live_state.state())}{self._base_icon}")

    # ------------------------------------------------------------- actions ---

    def toggle_meeting(self, _item) -> None:
        try:
            if self._session_active:
                self._show("title", self, IDLE_ICON)
                self._show("status", self.status_item, "summarizing & saving…")
                resp = requests.post(
                    BASE + "/meeting/stop", headers=_auth_headers(), timeout=600
                )
                resp.raise_for_status()
                saved = resp.json()
                rumps.notification(
                    "localflow", "Notes saved", saved["notes_path"]
                )
            else:
                resp = requests.post(
                    BASE + "/meeting/start",
                    json={"title": "Meeting", "category": ""},
                    headers=_auth_headers(),
                    timeout=10,
                )
                resp.raise_for_status()
        except Exception as exc:  # noqa: BLE001 - surfaced as a notification
            rumps.notification("localflow", "Error", str(exc)[:120])
        self.refresh()

    def dismiss(self, _item) -> None:
        try:
            requests.post(BASE + "/meeting/dismiss", headers=_auth_headers(), timeout=3)
        except Exception:  # noqa: BLE001, S110 - best-effort dismiss
            pass
        self.refresh()

    def open_ui(self, _item) -> None:
        open_web_ui(BASE, "/")

    def open_phrases(self, _item) -> None:
        # A separate process: rumps owns this process's AppKit main loop. The
        # hub dispatcher hands the page to the single main-app Hub window.
        subprocess.Popen([
            sys.executable, "-m", "localflow.hub_window", "--page", "Hot Phrases"
        ])

    def open_hub(self, _item) -> None:
        subprocess.Popen([sys.executable, "-m", "localflow.hub_window"])

    def open_meeting_notes(self, _item) -> None:
        subprocess.Popen([
            sys.executable, "-m", "localflow.hub_window", "--page", "Meetings"
        ])

    def open_notes(self, _item) -> None:
        from pathlib import Path

        folder = Path(_config.vault_path).expanduser() / _config.notes_folder
        folder.mkdir(parents=True, exist_ok=True)
        current().open_path(str(folder))

    def paste_last(self, _item) -> None:
        text = _history.latest_text(read_last_text())
        if text:
            paste_text(text, _live_config.get().paste_method)


def main() -> None:
    from localflow.singleinstance import acquire

    lock = acquire("menubar")
    if lock is None:
        raise SystemExit(0)
    LocalflowMenuBar().run()


if __name__ == "__main__":
    main()
