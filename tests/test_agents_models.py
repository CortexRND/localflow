import pytest

from localflow.agents import models as m


def test_transitions_are_the_single_source_of_truth():
    assert set(m.TRANSITIONS) == set(m.STATES)
    for state in m.TERMINAL_STATES:
        assert m.TRANSITIONS[state] == frozenset(), state
    for source, targets in m.TRANSITIONS.items():
        assert targets <= set(m.STATES), source
    assert m.TRANSITIONS["queued"] == {"starting", "cancelled", "failed"}
    assert m.TRANSITIONS["cancelling"] == {"cancelled", "failed"}
    assert "running" in m.TRANSITIONS["waiting_input"]
    assert "waiting_input" in m.TRANSITIONS["running"]
    # A follow-up never revives a terminal session.
    assert not any("running" in m.TRANSITIONS[s] for s in m.TERMINAL_STATES)


def test_launch_request_rejects_unknown_fields_and_wrong_types():
    assert m.LaunchRequest.from_body({"prompt": "x"}).prompt == "x"
    with pytest.raises(m.ValidationError, match="unknown field"):
        m.LaunchRequest.from_body({"prompt": "x", "shell": True})
    with pytest.raises(m.ValidationError, match="prompt is required"):
        m.LaunchRequest.from_body({})
    with pytest.raises(m.ValidationError, match="timeout_seconds"):
        m.LaunchRequest.from_body({"prompt": "x", "timeout_seconds": "10"})
    with pytest.raises(m.ValidationError, match="timeout_seconds"):
        m.LaunchRequest.from_body({"prompt": "x", "timeout_seconds": True})
    with pytest.raises(m.ValidationError):
        m.LaunchRequest.from_body(["prompt"])


def test_session_api_views_hide_internal_state():
    session = m.AgentSession(id="a" * 12, provider="claude", prompt="p", result_text="r" * 500,
                             internal={"digest": "secret-ish"})
    api = session.to_api()
    assert "internal" not in api and set(api) == set(m.SESSION_FIELDS)
    summary = session.to_summary()
    assert "prompt" not in summary and "result_text" not in summary
    assert summary["result_preview"] == "r" * 200
    assert m.AgentSession.from_record(session.to_record()) == session


def test_error_vocabulary_maps_to_http():
    conflict = m.SessionConflict("abc", "completed", ("running",))
    assert (conflict.status, conflict.code) == (409, "session_conflict")
    assert conflict.extra == {"state": "completed", "expected": ["running"]}
    assert (m.QueueFull().status, m.QueueFull().code) == (429, "queue_full")
    assert (m.PromptTooLong().status, m.ValidationError().status) == (413, 400)
    ready = m.ProviderNotReady(m.Readiness(False, "missing_secret", "Add a key."))
    assert ready.extra["readiness"]["code"] == "missing_secret"
    assert m.ProviderError("nonsense", "x").code == "internal"


def test_ids_and_timestamps():
    assert len(m.new_session_id()) == 12 and len(m.new_short_id()) == 8
    assert m.utc_now().endswith("Z") and "T" in m.utc_now()
    assert m.one_line_title("fix\nthe   tests " * 20).startswith("fix the tests fix")
    assert len(m.one_line_title("x" * 300)) == 80
