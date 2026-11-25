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
