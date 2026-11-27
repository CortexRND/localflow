from typing import ClassVar

import pytest
import requests

from localflow.meeting_client import (
    MeetingClient,
    MeetingClientError,
    vault_notes_path,
)


class FakeResponse:
    def __init__(self, status_code=200, data=None, reason="bad request"):
        self.status_code = status_code
        self.data = data or {}
        self.reason = reason

    def json(self):
        return self.data


class FakeHTTP:
    def __init__(self):
        self.calls = []
        self.response = FakeResponse(data={"ok": True})
        self.error = None

    def get(self, url, **kwargs):
        return self._call("get", url, **kwargs)

    def post(self, url, **kwargs):
        return self._call("post", url, **kwargs)

    def put(self, url, **kwargs):
        return self._call("put", url, **kwargs)

    def _call(self, method, url, **kwargs):
        self.calls.append((method, url, kwargs))
        if self.error:
            raise self.error
        return self.response


def test_client_uses_token_urls_payloads_and_timeouts():
    http = FakeHTTP()
    client = MeetingClient("http://localhost:9876/", "secret-token", http)

    client.status()
    client.templates()
    client.start("Daily", "Zoom", "standup", "- blocker")
    client.save_notes("notes")
    client.stop()
    client.chat("What is next?")
    client.dismiss()

    assert [call[0:2] for call in http.calls] == [
        ("get", "http://localhost:9876/meeting/status"),
        ("get", "http://localhost:9876/meeting/templates"),
        ("post", "http://localhost:9876/meeting/start"),
        ("post", "http://localhost:9876/meeting/notes"),
        ("post", "http://localhost:9876/meeting/stop"),
        ("post", "http://localhost:9876/meeting/chat"),
        ("post", "http://localhost:9876/meeting/dismiss"),
    ]
    assert all(
        call[2]["headers"] == {"X-Localflow-Token": "secret-token"}
        for call in http.calls
    )
    assert [call[2]["timeout"] for call in http.calls] == [
        3, 3, 10, 3, 600, 300, 3,
    ]
    assert http.calls[2][2]["json"] == {
        "title": "Daily", "category": "Zoom", "template": "standup",
        "user_notes": "- blocker",
    }
    assert http.calls[3][2]["json"] == {"notes": "notes"}
    assert http.calls[5][2]["json"] == {"question": "What is next?"}


def test_client_maps_server_detail_and_connection_errors():
    http = FakeHTTP()
    client = MeetingClient("http://localhost:9876", "token", http)
    http.response = FakeResponse(409, {"detail": "meeting already running"})
    with pytest.raises(MeetingClientError, match="meeting already running"):
        client.status()

    http.error = requests.ConnectionError("connection refused")
    with pytest.raises(
        MeetingClientError,
        match=r"LocalFlow server not running — open LocalFlow\.app .*or run `lf serve`",
    ):
        client.status()

    http.error = ConnectionRefusedError("adapter-level refusal")
    with pytest.raises(MeetingClientError, match="LocalFlow.app"):
        client.status()


def _config_file(tmp_path, monkeypatch, server_section):
    path = tmp_path / "config.toml"
    path.write_text(f"config_version = 2\n[server]\n{server_section}", encoding="utf-8")
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(path))
    monkeypatch.setenv("HOME", str(tmp_path))


@pytest.mark.parametrize(
    ("server_section", "expected"),
    [
        ("", "http://127.0.0.1:8756"),
        ('host = "127.0.0.1"\nport = 9911\n', "http://127.0.0.1:9911"),
        ('host = "0.0.0.0"\nport = 9911\n', "http://127.0.0.1:9911"),
        ('host = "::"\nport = 9911\n', "http://[::1]:9911"),
        ('host = "::1"\nport = 9911\n', "http://[::1]:9911"),
        ('host = "localhost"\nport = 9911\n', "http://localhost:9911"),
        ('host = "192.168.1.20"\nport = 9911\n', "http://192.168.1.20:9911"),
    ],
)
def test_default_base_url_follows_configured_host_and_port(
    tmp_path, monkeypatch, server_section, expected
):
    _config_file(tmp_path, monkeypatch, server_section)
    http = FakeHTTP()

    client = MeetingClient(token="t", http=http)
    client.status()

    assert client.base_url == expected
    assert http.calls[0][1] == f"{expected}/meeting/status"


def test_client_defaults_to_existing_token_and_omits_empty_header(monkeypatch):
    http = FakeHTTP()
    monkeypatch.setattr("localflow.meeting_client.load", lambda: "issued-token")

    client = MeetingClient("http://localhost:9876", http=http)
    client.status()
    assert http.calls[0][2]["headers"] == {
        "X-Localflow-Token": "issued-token"
    }

    monkeypatch.setattr("localflow.meeting_client.load", lambda: "")
    client = MeetingClient("http://localhost:9876", http=http)
    client.status()
    assert "headers" not in http.calls[1][2]


def test_client_picks_up_the_token_issued_after_it_was_created(monkeypatch):
    http = FakeHTTP()
    tokens = iter(["", "pre-start", "after-restart"])
    monkeypatch.setattr("localflow.meeting_client.load", lambda: next(tokens))

    client = MeetingClient("http://localhost:9876", http=http)
    client.status()
    client.status()
    assert [c[2]["headers"] for c in http.calls] == [
        {"X-Localflow-Token": "pre-start"},
        {"X-Localflow-Token": "after-restart"},
    ]


def test_client_keeps_an_explicit_token(monkeypatch):
    http = FakeHTTP()
    monkeypatch.setattr("localflow.meeting_client.load", lambda: "file-token")
    client = MeetingClient("http://localhost:9876", token="given", http=http)
    client.status()
    assert http.calls[0][2]["headers"] == {"X-Localflow-Token": "given"}


def _vault_with_note(tmp_path):
    vault = tmp_path / "vault"
    note = vault / "Meetings" / "2026-11-14 Daily.md"
    note.parent.mkdir(parents=True)
    note.write_text("# Daily\n")
    return vault, note


def test_vault_notes_path_allows_existing_markdown_inside_vault(tmp_path):
    vault, note = _vault_with_note(tmp_path)

    assert vault_notes_path(str(note), str(vault)) == str(note.resolve())
    upper = note.with_name("Upper.MD")
    upper.write_text("x")
    assert vault_notes_path(str(upper), str(vault)) == str(upper.resolve())


@pytest.mark.parametrize(
    "candidate",
    [
        None,
        "",
        42,
        "/usr/bin/xterm",
        "C:\\Windows\\System32\\calc.exe",
        "https://example.com/notes.md",
    ],
)
def test_vault_notes_path_rejects_non_vault_targets(tmp_path, candidate):
    vault, _ = _vault_with_note(tmp_path)

    assert vault_notes_path(candidate, str(vault)) is None


def test_vault_notes_path_rejects_wrong_type_missing_and_escaping_paths(tmp_path):
    vault, note = _vault_with_note(tmp_path)
    program = vault / "run.exe"
    program.write_text("x")
    folder = vault / "folder.md"
    folder.mkdir()
    outside = tmp_path / "shared" / "evil.md"
    outside.parent.mkdir()
    outside.write_text("x")

    assert vault_notes_path(str(program), str(vault)) is None
    assert vault_notes_path(str(folder), str(vault)) is None
    assert vault_notes_path(str(vault / "missing.md"), str(vault)) is None
    assert vault_notes_path(str(outside), str(vault)) is None
    assert vault_notes_path(str(vault / ".." / "shared" / "evil.md"), str(vault)) is None
    assert vault_notes_path(str(note), str(tmp_path / "no-vault")) is None
    assert vault_notes_path(str(note), "") is None


def test_vault_notes_path_rejects_symlink_that_leaves_the_vault(tmp_path):
    vault, _ = _vault_with_note(tmp_path)
    program = tmp_path / "shared" / "payload.app"
    program.parent.mkdir()
    program.write_text("x")
    outside_md = tmp_path / "shared" / "evil.md"
    outside_md.write_text("x")
    try:
        (vault / "link-to-program.md").symlink_to(program)
        (vault / "link-to-outside.md").symlink_to(outside_md)
    except (OSError, NotImplementedError) as exc:
        pytest.skip(f"symlinks unavailable: {exc}")

    assert vault_notes_path(str(vault / "link-to-program.md"), str(vault)) is None
    assert vault_notes_path(str(vault / "link-to-outside.md"), str(vault)) is None


MID = "0123456789abcdef0123456789abcdef"


def test_client_speaker_methods_use_api_routes():
    http = FakeHTTP()
    http.response = FakeResponse(data={"meeting_id": MID, "speakers": []})
    client = MeetingClient("http://localhost:9876", "secret-token", http)

    results = [
        client.speakers(MID),
        client.rename_speaker(MID, "S2", "Dana"),
        client.merge_speakers(MID, "S3", "S1"),
    ]

    base = f"http://localhost:9876/api/meetings/{MID}/speakers"
    assert [call[0:2] for call in http.calls] == [
        ("get", base), ("put", f"{base}/S2"), ("post", f"{base}/merge"),
    ]
    assert "json" not in http.calls[0][2]
    assert http.calls[1][2]["json"] == {"name": "Dana"}
    assert http.calls[2][2]["json"] == {"from": "S3", "into": "S1"}
    assert all(call[2]["headers"] == {"X-Localflow-Token": "secret-token"}
               for call in http.calls)
    assert [call[2]["timeout"] for call in http.calls] == [3, 10, 10]
    assert results == [{"meeting_id": MID, "speakers": []}] * 3


def test_client_speaker_ids_are_url_quoted():
    http = FakeHTTP()
    MeetingClient("http://localhost:9876", "t", http).rename_speaker("../x", "S 2/..", "n")
    assert http.calls[0][1].endswith("/api/meetings/..%2Fx/speakers/S%202%2F..")


def test_client_speaker_errors_carry_the_server_detail():
    http = FakeHTTP()
    http.response = FakeResponse(404, {"detail": "no speaker data for this meeting"})
    client = MeetingClient("http://localhost:9876", "t", http)
    for call in (lambda: client.speakers(MID),
                 lambda: client.rename_speaker(MID, "S1", "x"),
                 lambda: client.merge_speakers(MID, "S1", "S2")):
        with pytest.raises(MeetingClientError, match="no speaker data for this meeting"):
            call()


def test_client_errors_carry_the_http_status_code():
    http = FakeHTTP()
    client = MeetingClient("http://localhost:9876", "t", http)
    http.response = FakeResponse(404, {"detail": "no speaker data for this meeting"})
    with pytest.raises(MeetingClientError) as caught:
        client.speakers("x")
    assert caught.value.status_code == 404
    assert str(caught.value) == "no speaker data for this meeting"

    http.response = FakeResponse(409, {"detail": "meeting already running"})
    with pytest.raises(MeetingClientError) as caught:
        client.status()
    assert caught.value.status_code == 409

    for error in (requests.ConnectionError("down"), ConnectionRefusedError("refused")):
        http.error = error
        with pytest.raises(MeetingClientError) as caught:
            client.speakers("x")
        assert caught.value.status_code is None


def test_client_error_stays_backwards_compatible():
    error = MeetingClientError("boom")
    assert error.status_code is None
    assert str(error) == "boom"
    assert isinstance(error, RuntimeError)
    assert MeetingClientError("gone", 404).status_code == 404


class FakeHTTPWithDelete(FakeHTTP):
    def delete(self, url, **kwargs):
        return self._call("delete", url, **kwargs)


def test_client_voiceprint_methods_use_api_routes():
    http = FakeHTTPWithDelete()
    http.response = FakeResponse(data={"ok": True})
    client = MeetingClient("http://localhost:9876", "secret-token", http)
    client.enrolled()
    client.enroll("Dana Lee", MID, "S2")
    client.forget("Dana Lee/..")
    client.forget()
    base = "http://localhost:9876/api/speakers"
    assert [call[0:2] for call in http.calls] == [
        ("get", base), ("post", base), ("delete", f"{base}/Dana%20Lee%2F.."), ("delete", base),
    ]
    assert http.calls[1][2]["json"] == {"name": "Dana Lee", "meeting_id": MID,
                                        "speaker_id": "S2"}
    assert all("json" not in call[2] for i, call in enumerate(http.calls) if i != 1)
    assert [call[2]["timeout"] for call in http.calls] == [3, 10, 10, 10]
    assert all(call[2]["headers"] == {"X-Localflow-Token": "secret-token"}
               for call in http.calls)


def test_client_voiceprint_errors_carry_the_server_detail():
    http = FakeHTTPWithDelete()
    http.response = FakeResponse(403, {"detail": "speaker recognition is off"})
    client = MeetingClient("http://localhost:9876", "t", http)
    for call in (client.enrolled, lambda: client.enroll("x", MID, "S1"), client.forget):
        with pytest.raises(MeetingClientError, match="speaker recognition is off"):
            call()


class FakeHTTPWithPatch(FakeHTTP):
    def patch(self, url, **kwargs):
        return self._call("patch", url, **kwargs)


def test_rename_enrolled_patches_the_quoted_id():
    http = FakeHTTPWithPatch()
    client = MeetingClient("http://localhost:9876", "secret-token", http)
    client.rename_enrolled("Dana Lee/..", "Dana")
    method, url, kwargs = http.calls[0]
    assert (method, url) == ("patch", "http://localhost:9876/api/speakers/Dana%20Lee%2F..")
    assert kwargs["json"] == {"name": "Dana"} and kwargs["timeout"] == 10


# ------------------------------------------------- local-only voice requests ---

class FakeSessionResponse:
    def __init__(self, status_code=200, data=None, content=b""):
        self.status_code = status_code
        self.data = data or {}
        self.content = content
        self.reason = "reason"

    def json(self):
        return self.data


class FakeSession:
    def __init__(self, response=None, error=None):
        self.trust_env = True
        self.calls = []
        self.closed = False
        self.response = response or FakeSessionResponse(201, {"id": "a" * 32, "name": "Me"})
        self.error = error

    def request(self, method, url, **kwargs):
        self.calls.append((method, url, kwargs, self.trust_env))
        if self.error:
            raise self.error
        return self.response

    def close(self):
        self.closed = True


def _local_client(base_url, session):
    made = []

    def factory():
        made.append(session)
        return session

    client = MeetingClient(base_url, "secret-token", FakeHTTP(), session_factory=factory)
    return client, made


@pytest.mark.parametrize("base_url", [
    "http://192.168.1.10:8765", "http://0.0.0.0:8765", "https://127.0.0.1:8765",
    "http://lf.example.com:8765",
])
def test_voice_requests_refuse_a_remote_server_before_any_session(base_url):
    from localflow.people_model import ENROLL_REMOTE

    client, made = _local_client(base_url, FakeSession())
    with pytest.raises(MeetingClientError, match=ENROLL_REMOTE):
        client.enroll_audio("Me", b"RIFF")
    with pytest.raises(MeetingClientError, match=ENROLL_REMOTE):
        client.export_enrolled("/tmp/x")
    assert made == []


def test_enroll_audio_goes_to_a_literal_loopback_host_without_proxy_or_redirects():
    session = FakeSession()
    client, made = _local_client("http://localhost:9876/", session)
    out = client.enroll_audio("Me", b"RIFFdata")
    assert out == {"id": "a" * 32, "name": "Me"} and made == [session]
    method, url, kwargs, trust_env = session.calls[0]
    assert (method, url) == ("POST", "http://127.0.0.1:9876/api/speakers")
    assert trust_env is False and session.closed
    assert kwargs["allow_redirects"] is False
    assert kwargs["headers"] == {"X-Localflow-Token": "secret-token"}
    assert kwargs["data"] == {"name": "Me"}
    assert kwargs["files"] == {"file": ("recording.wav", b"RIFFdata", "application/octet-stream")}


def test_local_requests_refuse_redirects_and_keep_error_details():
    session = FakeSession(FakeSessionResponse(302))
    client, _made = _local_client("http://127.0.0.1:9876", session)
    with pytest.raises(MeetingClientError, match="redirect") as caught:
        client.enroll_audio("Me", b"x")
    assert caught.value.status_code == 302 and session.closed
    session = FakeSession(FakeSessionResponse(400, {"detail": "no speech detected"}))
    client, _made = _local_client("http://[::1]:9876", session)
    with pytest.raises(MeetingClientError, match="no speech detected") as caught:
        client.enroll_audio("Me", b"x")
    assert caught.value.status_code == 400
    assert session.calls[0][1] == "http://[::1]:9876/api/speakers"
    session = FakeSession(error=requests.ConnectionError("refused"))
    client, _made = _local_client("http://127.0.0.1:9876", session)
    with pytest.raises(MeetingClientError, match="server not running"):
        client.enroll_audio("Me", b"x")
    assert session.closed


def test_export_enrolled_writes_the_bundle(monkeypatch, tmp_path):
    from localflow.diarization import registry

    written = []

    def write_bundle(data, target):
        written.append((data, target))
        return tmp_path, 2

    monkeypatch.setattr(registry, "write_bundle", write_bundle)
    session = FakeSession(FakeSessionResponse(200, content=b"PK-zip"))
    client, _made = _local_client("http://127.0.0.1:9876", session)
    assert client.export_enrolled(tmp_path / "out") == 2
    assert written == [(b"PK-zip", tmp_path / "out")]
    method, url, kwargs, trust_env = session.calls[0]
    assert (method, url) == ("GET", "http://127.0.0.1:9876/api/speakers/export")
    assert kwargs["allow_redirects"] is False and trust_env is False


@pytest.mark.parametrize("error,message", [
    ("registry", "the export folder must be new or empty"),
    ("os", "could not write the export folder"),
])
def test_export_enrolled_maps_write_errors(monkeypatch, tmp_path, error, message):
    from localflow.diarization import registry

    def write_bundle(_data, target):
        if error == "registry":
            raise registry.RegistryError("the export folder must be new or empty")
        raise OSError(f"permission denied: {target}")

    monkeypatch.setattr(registry, "write_bundle", write_bundle)
    session = FakeSession(FakeSessionResponse(200, content=b"PK"))
    client, _made = _local_client("http://127.0.0.1:9876", session)
    with pytest.raises(MeetingClientError) as caught:
        client.export_enrolled(tmp_path / "out")
    assert str(caught.value) == message


class FakeRecorder:
    instances: ClassVar[list] = []

    def __init__(self, sample_rate=16000, device=None):
        self.sample_rate = sample_rate
        self.device = device
        self.started = self.stopped = False
        self.audio = None
        FakeRecorder.instances.append(self)

    def start(self):
        self.started = True

    def stop(self):
        import numpy as np

        self.stopped = True
        return self.audio if self.audio is not None else np.zeros(1600, dtype=np.float32)


class FakeUploader:
    def __init__(self):
        self.uploads = []

    def enroll_audio(self, name, wav):
        self.uploads.append((name, wav))
        return {"id": "a" * 32, "name": name}


def test_voice_recording_uploads_wav_from_memory(monkeypatch, tmp_path):
    import tempfile

    from localflow.config import Config
    from localflow.meeting_client import VoiceRecording

    def no_temp_files(*_args, **_kwargs):
        raise AssertionError("audio must stay in memory")

    monkeypatch.setattr(tempfile, "NamedTemporaryFile", no_temp_files)
    monkeypatch.setattr(tempfile, "mkstemp", no_temp_files)
    monkeypatch.chdir(tmp_path)
    capture = VoiceRecording(Config(sample_rate=16000, input_device="USB mic"),
                             recorder_factory=FakeRecorder)
    capture.start()
    recorder = FakeRecorder.instances[-1]
    assert recorder.started and recorder.device == "USB mic"
    uploader = FakeUploader()
    assert capture.finish(uploader, "Me") == {"id": "a" * 32, "name": "Me"}
    name, wav = uploader.uploads[0]
    assert name == "Me" and wav[:4] == b"RIFF" and wav[8:12] == b"WAVE"
    assert recorder.stopped
    assert list(tmp_path.iterdir()) == []
    with pytest.raises(MeetingClientError, match="nothing was recorded"):
        capture.finish(uploader, "Me")  # the recorder was handed over once


def test_voice_recording_with_no_audio_uploads_nothing():
    import numpy as np

    from localflow.meeting_client import VoiceRecording

    capture = VoiceRecording(None, recorder_factory=FakeRecorder)
    capture.start()
    FakeRecorder.instances[-1].audio = np.array([], dtype=np.float32)
    uploader = FakeUploader()
    with pytest.raises(MeetingClientError, match="nothing was recorded"):
        capture.finish(uploader, "Me")
    assert uploader.uploads == []


def test_voice_recording_discard_drops_the_audio():
    from localflow.meeting_client import VoiceRecording

    capture = VoiceRecording(None, recorder_factory=FakeRecorder)
    capture.start()
    recorder = FakeRecorder.instances[-1]
    capture.discard()
    assert recorder.stopped
    uploader = FakeUploader()
    with pytest.raises(MeetingClientError):
        capture.finish(uploader, "Me")
    assert uploader.uploads == []
    # A discard that lands while the microphone is still opening closes it.
    late = VoiceRecording(None, recorder_factory=FakeRecorder)
    late.discard()
    late.start()
    assert FakeRecorder.instances[-1].stopped


def test_voice_modules_do_not_import_audio_or_diarization_at_load():
    import ast
    from pathlib import Path

    root = Path(__file__).parent.parent / "localflow"
    for name in ("meeting_client.py", "people_model.py", "hub_mac.py", "hub_tk.py"):
        tree = ast.parse((root / name).read_text(encoding="utf-8"))
        for node in tree.body:
            if isinstance(node, ast.ImportFrom):
                modules = [node.module or ""]
            elif isinstance(node, ast.Import):
                modules = [alias.name for alias in node.names]
            else:
                continue
            for module in modules:
                assert not module.startswith(("localflow.audio", "localflow.diarization")), (
                    name, module,
                )
                assert module not in ("sounddevice", "numpy"), (name, module)
