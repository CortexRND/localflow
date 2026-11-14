import pytest
import requests

from localflow.meeting_client import MeetingClient, MeetingClientError


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
