import pytest

pytest.importorskip("fastapi")
pytest.importorskip("sounddevice")
pytest.importorskip("pynput")
httpx = pytest.importorskip("httpx")

from fastapi.testclient import TestClient  # noqa: E402

from localflow import server  # noqa: E402
from localflow.hotphrases import HotPhraseStore  # noqa: E402


@pytest.fixture
def client(tmp_path, monkeypatch):
    store = HotPhraseStore(tmp_path / "hot_phrases.json")
    monkeypatch.setattr(server, "_hot_phrases", store)
    monkeypatch.setattr(server._config, "meeting_watch", False)
    # Not a context manager: no startup/shutdown events, so the meeting
    # watcher never starts on CI.
    return TestClient(server.app)


def test_crud_roundtrip(client):
    assert client.get("/hot-phrases").json() == {"entries": []}

    r = client.post("/hot-phrases", json={"trigger": "review checklist",
                                          "text": "big prompt"})
    assert r.status_code == 200
    entry = r.json()
    assert entry["trigger"] == "review checklist"
    assert entry["enabled"] is True

    assert client.get("/hot-phrases").json()["entries"] == [entry]

    r = client.patch(f"/hot-phrases/{entry['id']}", json={"enabled": False})
    assert r.status_code == 200
    assert r.json()["enabled"] is False

    r = client.delete(f"/hot-phrases/{entry['id']}")
    assert r.status_code == 200 and r.json() == {"ok": True}
    assert client.get("/hot-phrases").json() == {"entries": []}


def test_validation_error_is_400(client):
    r = client.post("/hot-phrases", json={"trigger": "", "text": "x"})
    assert r.status_code == 400
    assert "detail" in r.json()
    r = client.post("/hot-phrases", json={"trigger": "ok", "text": "  "})
    assert r.status_code == 400


def test_duplicate_is_409(client):
    client.post("/hot-phrases", json={"trigger": "Review Checklist", "text": "x"})
    r = client.post("/hot-phrases", json={"trigger": "review, checklist", "text": "y"})
    assert r.status_code == 409


def test_unknown_id_is_404(client):
    assert client.patch("/hot-phrases/nosuchid", json={"text": "x"}).status_code == 404
    assert client.delete("/hot-phrases/nosuchid").status_code == 404


def test_preview(client):
    client.post("/hot-phrases",
                json={"trigger": "review checklist", "text": "CHECK: all"})
    client.post("/hot-phrases",
                json={"trigger": "off", "text": "no", "enabled": False})
    r = client.post("/hot-phrases/preview",
                    json={"text": "please review checklist now off"})
    assert r.status_code == 200
    assert r.json() == {"text": "please CHECK: all now off"}
