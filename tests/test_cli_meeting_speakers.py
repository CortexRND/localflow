import pytest
import requests
from click.testing import CliRunner

from localflow import cli as cli_module

MID = "0123456789abcdef0123456789abcdef"
VIEW = {"meeting_id": MID, "speakers": [
    {"id": "S1", "label": "Speaker 1", "name": None, "source": "others", "seconds": 6.0},
    {"id": "S2", "label": "Dana Lee", "name": "Dana Lee", "source": "others", "seconds": 7.0},
]}


@pytest.fixture
def calls(monkeypatch):
    log = []
    state = {"status": {"last_saved": {"meeting_id": MID}}}

    def get(path, timeout=5):
        log.append(("get", path, None))
        return state["status"] if path == "/meeting/status" else VIEW

    def post(path, json=None):
        log.append(("post", path, json))
        return VIEW

    def put(path, json=None):
        log.append(("put", path, json))
        return VIEW

    monkeypatch.setattr(cli_module, "_get", get)
    monkeypatch.setattr(cli_module, "_post", post)
    monkeypatch.setattr(cli_module, "_put", put)
    return log, state


def _run(*args):
    return CliRunner().invoke(cli_module.cli, ["meeting", *args])


def test_speakers_with_explicit_id(calls):
    log, _ = calls
    result = _run("speakers", MID)
    assert result.exit_code == 0, result.output
    assert log == [("get", f"/api/meetings/{MID}/speakers", None)]
    lines = result.output.strip().split("\n")
    assert lines[0] == f"meeting {MID}"
    assert len(lines) == 3
    assert "S1" in lines[1] and "Speaker 1" in lines[1] and "6.0s" in lines[1]
    assert "S2" in lines[2] and "Dana Lee" in lines[2] and "7.0s" in lines[2]


def test_speakers_defaults_to_last_saved_meeting(calls):
    log, _ = calls
    result = _run("speakers")
    assert result.exit_code == 0, result.output
    assert [c[1] for c in log] == ["/meeting/status", f"/api/meetings/{MID}/speakers"]


@pytest.mark.parametrize("status", [{"last_saved": None}, {"last_saved": {"meeting_id": None}}])
@pytest.mark.parametrize("args", [("speakers",), ("rename-speaker", "S2", "Dana"),
                                  ("merge-speakers", "S3", "S1")])
def test_no_saved_meeting_is_a_clear_error(calls, status, args):
    log, state = calls
    state["status"] = status
    result = _run(*args)
    assert result.exit_code == 1
    assert "no saved meeting with speaker labels" in result.output
    assert [c[0] for c in log] == ["get"]


def test_speakers_http_error_prints_detail(monkeypatch):
    class Resp:
        reason = "Not Found"

        def json(self):
            return {"detail": "no speaker data for this meeting"}

    def get(path, timeout=5):
        raise requests.HTTPError(response=Resp())

    monkeypatch.setattr(cli_module, "_get", get)
    result = _run("speakers", MID)
    assert result.exit_code == 1
    assert "no speaker data for this meeting" in result.output
    assert isinstance(result.exception, SystemExit)


@pytest.mark.parametrize("args", [("speakers",), ("rename-speaker", "S2", "Dana"),
                                  ("merge-speakers", "S3", "S1")])
def test_status_http_error_prints_detail(monkeypatch, args):
    class Resp:
        reason = "Unauthorized"

        def json(self):
            return {"detail": "missing or invalid API token"}

    sent = []

    def get(path, timeout=5):
        sent.append(path)
        raise requests.HTTPError(response=Resp())

    monkeypatch.setattr(cli_module, "_get", get)
    monkeypatch.setattr(cli_module, "_put", lambda *a, **k: sent.append("put"))
    monkeypatch.setattr(cli_module, "_post", lambda *a, **k: sent.append("post"))
    result = _run(*args)
    assert result.exit_code == 1
    assert "error: missing or invalid API token" in result.output
    assert isinstance(result.exception, SystemExit)
    assert sent == ["/meeting/status"]


def test_rename_speaker_with_and_without_id(calls):
    log, _ = calls
    result = _run("rename-speaker", MID, "S2", "Dana Lee")
    assert result.exit_code == 0, result.output
    assert log == [("put", f"/api/meetings/{MID}/speakers/S2", {"name": "Dana Lee"})]
    assert "Dana Lee" in result.output and "Speaker 1" in result.output
    log.clear()
    result = _run("rename-speaker", "S2", "Dana Lee")
    assert result.exit_code == 0, result.output
    assert log == [("get", "/meeting/status", None),
                   ("put", f"/api/meetings/{MID}/speakers/S2", {"name": "Dana Lee"})]


def test_merge_speakers_with_and_without_id(calls):
    log, _ = calls
    expected = ("post", f"/api/meetings/{MID}/speakers/merge", {"from": "S3", "into": "S1"})
    result = _run("merge-speakers", MID, "S3", "S1")
    assert result.exit_code == 0, result.output
    assert log == [expected]
    log.clear()
    result = _run("merge-speakers", "S3", "S1")
    assert result.exit_code == 0, result.output
    assert log == [("get", "/meeting/status", None), expected]


@pytest.mark.parametrize("args", [
    ("rename-speaker", "S2"), ("rename-speaker", MID, "S2", "Dana", "extra"),
    ("merge-speakers", "S3"), ("merge-speakers", MID, "S3", "S1", "S2"),
])
def test_wrong_argument_counts_are_usage_errors(calls, args):
    log, _ = calls
    result = _run(*args)
    assert result.exit_code == 2
    assert log == []


def test_ids_are_url_quoted(calls):
    log, _ = calls
    result = _run("rename-speaker", "a/b", "S 2", "n")
    assert result.exit_code == 0, result.output
    assert log == [("put", "/api/meetings/a%2Fb/speakers/S%202", {"name": "n"})]


def test_put_sends_token_and_reports_errors(monkeypatch):
    sent = []

    class Resp:
        def __init__(self, ok, status=200, data=None):
            self.ok = ok
            self.status_code = status
            self.reason = "Conflict"
            self.data = data or {}

        def json(self):
            return self.data

    responses = [Resp(True, data={"x": 1}), Resp(False, 409, {"detail": "x"})]

    def fake(url, headers=None, json=None, **kwargs):
        sent.append((url, headers, json))
        return responses.pop(0)

    monkeypatch.setattr(cli_module, "read_api_token", lambda: "tok")
    monkeypatch.setattr(cli_module.requests, "put", fake)
    assert cli_module._put("/api/x", {"name": "n"}) == {"x": 1}
    assert sent[0][1] == {"X-Localflow-Token": "tok"} and sent[0][2] == {"name": "n"}
    with pytest.raises(SystemExit) as exc:
        cli_module._put("/api/x", {})
    assert exc.value.code == 1

    def down(url, **kwargs):
        raise requests.ConnectionError("down")

    monkeypatch.setattr(cli_module.requests, "put", down)
    with pytest.raises(SystemExit) as exc:
        cli_module._put("/api/x", {})
    assert exc.value.code == 1


@pytest.mark.parametrize("name", ["speakers", "rename-speaker", "merge-speakers"])
def test_new_meeting_commands_are_hidden_but_registered(name):
    group = cli_module.cli.commands["meeting"]
    assert group.commands[name].hidden is True
    result = CliRunner().invoke(cli_module.cli, ["meeting", name, "--help"])
    assert result.exit_code == 0
