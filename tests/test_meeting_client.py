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
        match="LocalFlow server not running — start it with `lf serve`",
    ):
        client.status()


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
