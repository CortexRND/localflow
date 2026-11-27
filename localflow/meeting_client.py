"""HTTP client for the local meeting endpoints."""

import logging
import threading
from pathlib import Path
from urllib.parse import quote

import requests

from localflow.apitoken import load
from localflow.config import load_config
from localflow.server_url import configured_base_url

log = logging.getLogger("localflow.meeting_client")

SERVER_DOWN = (
    "LocalFlow server not running — open LocalFlow.app (it starts the server) "
    "or run `lf serve`"
)


def vault_notes_path(notes_path, vault_path) -> str | None:
    """Return the resolved notes file if it is an existing ``.md`` file inside
    the vault, else None. The path comes from whatever answers on the local
    port, so it must never reach the OS opener unchecked."""
    if not isinstance(notes_path, str) or not notes_path.strip() or not vault_path:
        return None
    try:
        vault = Path(vault_path).expanduser().resolve(strict=True)
        target = Path(notes_path).expanduser().resolve(strict=True)
    except (OSError, RuntimeError, ValueError):
        return None
    if target.suffix.lower() != ".md" or not target.is_file():
        return None
    if not target.is_relative_to(vault):
        return None
    return str(target)


class MeetingClientError(RuntimeError):
    """A meeting request failed or the local server is unavailable.

    `status_code` is the HTTP status of a non-2xx answer; None when the server
    could not be reached (or the error was raised without one).
    """

    def __init__(self, message="", status_code: int | None = None):
        super().__init__(message)
        self.status_code = status_code


def _raise_for_status(response) -> None:
    if not 200 <= response.status_code < 300:
        try:
            detail = response.json().get("detail")
        except (ValueError, AttributeError):
            detail = None
        raise MeetingClientError(
            str(detail or getattr(response, "reason", "request failed")),
            status_code=response.status_code,
        )


class MeetingClient:
    def __init__(
        self,
        base_url: str | None = None,
        token: str | None = None,
        http=requests,
        session_factory=None,
    ):
        if base_url is None:
            base_url = configured_base_url(load_config())
        self.base_url = base_url.rstrip("/")
        self._fixed_token = token
        self.token = token if token is not None else load()
        self.http = http
        # Only voice audio and voiceprint exports use a session (see _local_request).
        self.session_factory = session_factory or requests.Session

    def _refresh_token(self) -> None:
        if self._fixed_token is None:
            # The server issues a new token at every start; read the current one.
            self.token = load()

    def _request(self, method: str, path: str, *, timeout=3, json=None) -> dict:
        self._refresh_token()
        options = {"timeout": timeout}
        if self.token:
            options["headers"] = {"X-Localflow-Token": self.token}
        try:
            response = getattr(self.http, method)(
                f"{self.base_url}{path}",
                **options,
                **({"json": json} if json is not None else {}),
            )
        except requests.RequestException as exc:
            raise MeetingClientError(SERVER_DOWN) from exc
        except Exception as exc:
            # Injected HTTP adapters can expose their own connection exception.
            if isinstance(exc, OSError):
                raise MeetingClientError(SERVER_DOWN) from exc
            raise
        _raise_for_status(response)
        if response.status_code == 204:
            return {}
        return response.json()

    def _local_request(self, method: str, path: str, *, timeout=600, **kwargs):
        """A request that carries voice audio or voiceprints: this computer only.

        The URL host must be a loopback literal (``localhost`` becomes
        127.0.0.1); otherwise nothing is sent. No environment proxy, no
        redirect followed or accepted. Returns the response.
        """
        from localflow.people_model import ENROLL_REMOTE, local_base_url

        base = local_base_url(self.base_url)
        if base is None:
            raise MeetingClientError(ENROLL_REMOTE)
        self._refresh_token()
        headers = {"X-Localflow-Token": self.token} if self.token else {}
        session = self.session_factory()
        try:
            session.trust_env = False  # never route voice data through an environment proxy
            response = session.request(
                method.upper(), f"{base}{path}", headers=headers, timeout=timeout,
                allow_redirects=False, **kwargs,
            )
        except requests.RequestException as exc:
            raise MeetingClientError(SERVER_DOWN) from exc
        except OSError as exc:
            raise MeetingClientError(SERVER_DOWN) from exc
        finally:
            session.close()
        if 300 <= response.status_code < 400:
            raise MeetingClientError(
                "the server answered with a redirect", status_code=response.status_code,
            )
        _raise_for_status(response)
        return response

    def status(self) -> dict:
        return self._request("get", "/meeting/status", timeout=3)

    def diarization_status(self) -> dict:
        """Whether speaker labels can run (runtime extra installed, models ready)."""
        return self._request("get", "/api/diarization/status", timeout=3)

    def templates(self) -> dict:
        return self._request("get", "/meeting/templates")

    def start(self, title, category, template, user_notes, auto_title=None) -> dict:
        """`auto_title`: True = the title is a placeholder to name on save,
        False = the user typed it (kept, even "Meeting"), None = let the
        server decide from the title (older callers)."""
        body = {
            "title": title,
            "category": category,
            "template": template,
            "user_notes": user_notes,
        }
        if auto_title is not None:
            body["auto_title"] = bool(auto_title)
        return self._request("post", "/meeting/start", timeout=10, json=body)

    def save_notes(self, notes: str) -> dict:
        return self._request("post", "/meeting/notes", json={"notes": notes})

    def stop(self) -> dict:
        return self._request("post", "/meeting/stop", timeout=600)

    def chat(self, question: str) -> dict:
        return self._request(
            "post", "/meeting/chat", timeout=300, json={"question": question}
        )

    def dismiss(self) -> None:
        self._request("post", "/meeting/dismiss")

    def speakers(self, meeting_id: str) -> dict:
        return self._request("get", f"/api/meetings/{quote(str(meeting_id), safe='')}/speakers")

    def rename_speaker(self, meeting_id: str, speaker_id: str, name: str) -> dict:
        return self._request(
            "put",
            f"/api/meetings/{quote(str(meeting_id), safe='')}"
            f"/speakers/{quote(str(speaker_id), safe='')}",
            timeout=10, json={"name": name},
        )

    def merge_speakers(self, meeting_id: str, from_id: str, into_id: str) -> dict:
        return self._request(
            "post", f"/api/meetings/{quote(str(meeting_id), safe='')}/speakers/merge",
            timeout=10, json={"from": from_id, "into": into_id},
        )

    # Enrolled voices (speaker recognition). Voice audio uploads and voiceprint exports
    # go through _local_request: only to a loopback server, never through a proxy or a
    # redirect, and the audio is only ever held in memory.

    def enrolled(self) -> dict:
        return self._request("get", "/api/speakers")

    def enroll(self, name: str, meeting_id: str, speaker_id: str) -> dict:
        """Remember a speaker of a meeting saved with speaker recognition on."""
        return self._request(
            "post", "/api/speakers", timeout=10,
            json={"name": name, "meeting_id": meeting_id, "speaker_id": speaker_id},
        )

    def forget(self, voiceprint_id: str | None = None) -> dict:
        """Forget one enrolled voice (id or name), or every voice with None."""
        path = "/api/speakers"
        if voiceprint_id is not None:
            path += f"/{quote(str(voiceprint_id), safe='')}"
        return self._request("delete", path, timeout=10)

    def rename_enrolled(self, voiceprint_id: str, name: str) -> dict:
        """Rename an enrolled voice for later meetings; past transcripts stay."""
        return self._request(
            "patch", f"/api/speakers/{quote(str(voiceprint_id), safe='')}",
            timeout=10, json={"name": name},
        )

    def enroll_audio(self, name: str, wav: bytes) -> dict:
        """Enroll `name` from a WAV clip held in memory (local server only)."""
        response = self._local_request(
            "post", "/api/speakers", data={"name": name},
            files={"file": ("recording.wav", wav, "application/octet-stream")},
        )
        return response.json()

    def export_enrolled(self, target) -> int:
        """Write the enrolled voices into the new or empty folder `target`; the count."""
        response = self._local_request("get", "/api/speakers/export")
        from localflow.diarization.registry import RegistryError, write_bundle

        try:
            _folder, count = write_bundle(response.content, target)
        except RegistryError as exc:
            raise MeetingClientError(str(exc)) from None
        except OSError:
            raise MeetingClientError("could not write the export folder") from None
        return count


def wav_bytes(audio, sample_rate: int) -> bytes:
    """16-bit PCM mono WAV in memory (as `lf speakers enroll --record` sends it)."""
    import io
    import wave

    import numpy as np

    pcm = (np.clip(np.asarray(audio, dtype=np.float32), -1.0, 1.0) * 32767).astype("<i2")
    buffer = io.BytesIO()
    with wave.open(buffer, "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(int(sample_rate))
        wav.writeframes(pcm.tobytes())
    return buffer.getvalue()


class VoiceRecording:
    """One "Enroll my voice" capture. The audio is only ever in memory.

    `start` opens the microphone (the Recorder is imported only then);
    `finish` stops it and uploads the clip as WAV to the local server;
    `discard` stops it and drops the audio. Each runs off the UI thread; a
    `discard` that lands while `start` is still opening the microphone closes
    it as soon as it opened.
    """

    def __init__(self, config=None, recorder_factory=None):
        self.config = config
        self._recorder_factory = recorder_factory
        self._lock = threading.Lock()
        self._recorder = None
        self._discarded = False

    @property
    def sample_rate(self) -> int:
        return int(getattr(self.config, "sample_rate", 16000) or 16000)

    def start(self) -> None:
        factory = self._recorder_factory
        if factory is None:
            from localflow.audio import Recorder

            factory = Recorder
        device = getattr(self.config, "input_device", "") or None
        recorder = factory(sample_rate=self.sample_rate, device=device)
        recorder.start()
        with self._lock:
            if not self._discarded:
                self._recorder = recorder
                return
        recorder.stop()  # cancelled while the microphone was opening

    def _take(self):
        with self._lock:
            recorder, self._recorder = self._recorder, None
        return recorder

    def finish(self, client, name: str) -> dict:
        recorder = self._take()
        if recorder is None:
            raise MeetingClientError("nothing was recorded")
        audio = recorder.stop()
        wav = None
        try:
            if audio is None or len(audio) == 0:
                raise MeetingClientError("nothing was recorded")
            max_samples = 30 * self.sample_rate
            if len(audio) > max_samples:
                audio = audio[:max_samples]
            wav = wav_bytes(audio, self.sample_rate)
            return client.enroll_audio(name, wav)
        finally:
            del audio, wav

    def discard(self) -> None:
        with self._lock:
            self._discarded = True
            recorder, self._recorder = self._recorder, None
        if recorder is not None:
            try:
                recorder.stop()
            except Exception as exc:  # noqa: BLE001 - the audio is dropped either way
                log.warning("closing the enrollment microphone failed (%s)", type(exc).__name__)
