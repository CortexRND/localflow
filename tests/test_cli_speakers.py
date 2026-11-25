"""`lf speakers status|download` on a fake cache; never touches the network."""

import hashlib
import json
import socket
from pathlib import Path

import pytest
from click.testing import CliRunner

from localflow import cli as cli_module
from localflow.config import Config
from localflow.diarization import models as dm
from localflow.diarization import registry as _registry  # noqa: F401 (before DIAR_MODELS stub)
from localflow.models import DownloadCancelled

PAYLOAD = bytes(range(256)) * 1200
MODEL = dm.DiarModel(
    id="seg", label="Speaker labels: seg", repo="test-org/seg-model", revision="c" * 40,
    filename="model.onnx", sha256=hashlib.sha256(PAYLOAD).hexdigest(),
    size_bytes=len(PAYLOAD), licence="MIT", source="test fixture",
)


class FakeResponse:
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False

    def raise_for_status(self):
        pass

    def iter_content(self, size):
        for start in range(0, len(PAYLOAD), size):
            yield PAYLOAD[start:start + size]


@pytest.fixture(autouse=True)
def isolated(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("HF_HUB_CACHE", str(tmp_path / "hub"))
    monkeypatch.delenv("HF_ENDPOINT", raising=False)
    monkeypatch.setattr(dm, "DIAR_MODELS", (MODEL,))
    monkeypatch.setattr(cli_module, "load_config", lambda: Config(speaker_diarization=True))

    def no_network(url):
        raise AssertionError(f"network: {url}")

    monkeypatch.setattr(dm, "_open_stream", no_network)


@pytest.fixture
def offline(monkeypatch):
    def refuse(*args, **kwargs):
        raise AssertionError("socket opened")

    monkeypatch.setattr(socket, "socket", refuse)
    monkeypatch.setattr(socket, "create_connection", refuse)


def run(*args):
    return CliRunner().invoke(cli_module.cli, ["speakers", *args])


def test_status_on_an_empty_cache_is_offline_and_reports_each_part(offline, tmp_path):
    result = run("status")
    assert result.exit_code == 0, result.output
    lines = result.output.splitlines()
    assert lines[0].startswith("models: not ready")
    assert "lf speakers download" in lines[0]
    assert any(line.startswith("seg: ") for line in lines)
    assert any(line.startswith("runtime (sherpa-onnx): ") for line in lines)
    assert "speaker_diarization: true" in lines
    assert "diarize_mic: false" in lines
    assert "speaker_recognition: false" in lines
    assert "pip install" not in result.output
    assert "[diarization]" not in result.output
    assert not (tmp_path / "hub").exists()


@pytest.mark.parametrize("installed", [True, False])
def test_status_reports_the_runtime_accurately(monkeypatch, installed):
    monkeypatch.setattr(dm, "runtime_installed", lambda: installed)
    result = run("status")
    expected = "installed" if installed else "not installed"
    assert f"runtime (sherpa-onnx): {expected}" in result.output.splitlines()


def test_status_json(offline):
    result = run("status", "--json")
    assert result.exit_code == 0, result.output
    info = json.loads(result.output)
    assert info["models_ready"] is False
    assert isinstance(info["extra_installed"], bool)
    assert [model["id"] for model in info["models"]] == ["seg"]
    assert info["speaker_diarization"] is True
    assert info["diarize_mic"] is False
    assert info["speaker_recognition"] is False


def test_download_success_makes_models_ready(monkeypatch):
    urls = []

    def fake_stream(url):
        urls.append(url)
        return FakeResponse()

    monkeypatch.setattr(dm, "_open_stream", fake_stream)
    result = run("download")
    assert result.exit_code == 0, result.output
    assert "downloaded" in result.output
    assert urls == [f"https://huggingface.co/test-org/seg-model/resolve/{'c' * 40}/model.onnx"]
    assert dm.models_ready() == (True, "")
    status = run("status")
    assert status.output.splitlines()[0] == "models: ready"
    assert "seg: verified" in status.output.splitlines()


def test_download_failure_is_a_friendly_click_error(monkeypatch):
    def fail(*args, **kwargs):
        raise RuntimeError("connection timed out while fetching")

    monkeypatch.setattr(dm, "download_diarization_models", fail)
    result = run("download")
    assert result.exit_code == 1
    assert "Network problem while downloading" in result.output


def test_download_cancelled_aborts(monkeypatch):
    def cancelled(*args, **kwargs):
        raise DownloadCancelled("Download cancelled")

    monkeypatch.setattr(dm, "download_diarization_models", cancelled)
    result = run("download")
    assert result.exit_code == 1
    assert "Aborted" in result.output


def test_speakers_is_hidden_from_top_level_help_but_usable():
    top = CliRunner().invoke(cli_module.cli, ["--help"])
    assert top.exit_code == 0
    assert not any(line.split()[:1] == ["speakers"] for line in top.output.splitlines())
    assert cli_module.cli.commands["speakers"].hidden is True
    group = run("--help")
    assert group.exit_code == 0
    listed = [line.split()[0] for line in group.output.split("Commands:")[1].splitlines()
              if line.strip()]
    assert listed == ["download", "enroll", "export", "forget", "list", "rename", "status"]


# ------------------------------------------------- enrolled voices (slice B9) ---

VP_ID = "a" * 32
LISTED = {"speakers": [
    {"id": VP_ID, "name": "Dana Lee", "created": "2026-11-14T10:00:00Z", "samples": 2,
     "model_id": "mock"},
    {"id": "b" * 32, "name": "Lee", "created": "2026-11-15T10:00:00Z", "samples": 1,
     "model_id": "3dspeaker-campplus-zh-en-16k-advanced"},
]}


@pytest.fixture
def http(monkeypatch):
    calls = []

    def fake(name, reply):
        def call(path, json=None, timeout=None):
            calls.append((name, path, json))
            return reply(path) if callable(reply) else reply
        return call

    monkeypatch.setattr(cli_module, "_get", fake("get", LISTED))
    monkeypatch.setattr(cli_module, "_post", fake("post", {**LISTED["speakers"][0]}))
    monkeypatch.setattr(cli_module, "_patch", fake("patch", {**LISTED["speakers"][0],
                                                             "name": "Dana"}))
    monkeypatch.setattr(cli_module, "_delete", fake("delete", {"forgotten": 2}))
    return calls


class Sent:
    def __init__(self, content=b"", status=200, data=None):
        self.content, self.status_code, self._data = content, status, data
        self.ok = 200 <= status < 300
        self.reason = "error"

    def json(self):
        return self._data


@pytest.fixture
def local(monkeypatch):
    """Captures _local_send traffic at the requests.Session level."""
    sent = {"reply": Sent(data=LISTED["speakers"][0]), "calls": []}

    def request(self, method, url, **kwargs):
        sent["calls"].append((method, url, kwargs, self.trust_env))
        return sent["reply"]

    monkeypatch.setattr(cli_module.requests.Session, "request", request)
    monkeypatch.setattr(cli_module, "read_api_token", lambda: "tok")
    return sent


def _config(monkeypatch, **fields):
    monkeypatch.setattr(cli_module, "load_config",
                        lambda: Config(speaker_diarization=True, **fields))


def test_list_marks_voices_from_another_model(http, monkeypatch):
    _config(monkeypatch, diarization_backend="mock")
    result = run("list")
    assert result.exit_code == 0, result.output
    assert result.output.splitlines() == [
        "Dana Lee\t2\t2026-11-14T10:00:00Z\tmock",
        ("Lee\t1\t2026-11-15T10:00:00Z\t3dspeaker-campplus-zh-en-16k-advanced"
         "\tneeds re-enrollment (different voice model)"),
    ]
    assert VP_ID not in result.output
    _config(monkeypatch, diarization_backend="sherpa-onnx")
    lines = run("list").output.splitlines()
    assert lines[0].endswith("needs re-enrollment (different voice model)")
    assert not lines[1].endswith("re-enrollment (different voice model)")
    assert http == [("get", "/api/speakers", None)] * 2


def test_list_json_and_empty(http, monkeypatch):
    assert json.loads(run("list", "--json").output) == LISTED
    monkeypatch.setattr(cli_module, "_get", lambda path, timeout=5: {"speakers": []})
    assert run("list").output == "no enrolled speakers\n"


def test_list_reports_the_gate(monkeypatch):
    import requests

    resp = Sent(status=403, data={"detail": "speaker recognition is off"})

    def forbidden(path, timeout=5):
        raise requests.HTTPError(response=resp)

    monkeypatch.setattr(cli_module, "_get", forbidden)
    result = run("list")
    assert result.exit_code == 1 and "speaker recognition is off" in result.output


def test_enroll_from_meeting(http):
    result = run("enroll", "Dana Lee", "--from-meeting", "f" * 32, "--speaker", "S2")
    assert result.exit_code == 0, result.output
    assert result.output == "enrolled Dana Lee\n"
    assert http == [("post", "/api/speakers",
                     {"name": "Dana Lee", "meeting_id": "f" * 32, "speaker_id": "S2"})]


@pytest.mark.parametrize("args", [
    [], ["--from-meeting", "x"], ["--speaker", "S1"], ["--record", "5", "--file", __file__],
    ["--from-meeting", "x", "--speaker", "S1", "--record", "5"], ["--record", "0"],
    ["--record", "301"],
])
def test_enroll_needs_exactly_one_source(http, local, args):
    result = run("enroll", "Dana", *args)
    assert result.exit_code == 2, result.output
    assert http == [] and local["calls"] == []


def test_enroll_from_file_goes_only_to_the_local_server(local, tmp_path, monkeypatch):
    _config(monkeypatch, server_host="0.0.0.0", server_port=8756)
    clip = tmp_path / "clip.m4a"
    clip.write_bytes(b"audio-bytes")
    result = run("enroll", "Dana Lee", "--file", str(clip))
    assert result.exit_code == 0, result.output
    [(method, url, kwargs, trust_env)] = local["calls"]
    assert (method, url) == ("post", "http://127.0.0.1:8756/api/speakers")
    assert trust_env is False and kwargs["allow_redirects"] is False
    assert kwargs["files"] == {"file": ("clip.m4a", b"audio-bytes", "application/octet-stream")}
    assert kwargs["data"] == {"name": "Dana Lee"}
    assert kwargs["headers"] == {"X-Localflow-Token": "tok"}


@pytest.mark.parametrize("host", ["192.168.1.5", "laptop.tail1234.ts.net", "example.com"])
def test_audio_and_export_refuse_a_remote_server(local, tmp_path, monkeypatch, host):
    _config(monkeypatch, server_host=host)
    clip = tmp_path / "clip.wav"
    clip.write_bytes(b"x")
    recorded = []
    monkeypatch.setattr("localflow.audio.Recorder", lambda **kw: recorded.append(kw))
    for args in (["enroll", "Dana", "--file", str(clip)], ["enroll", "Dana", "--record", "5"],
                 ["export", str(tmp_path / "out")]):
        result = run(*args)
        assert result.exit_code == 1 and "on this computer" in result.output
    assert local["calls"] == [] and recorded == []
    assert not (tmp_path / "out").exists()


def test_enroll_refuses_oversized_files(local, tmp_path, monkeypatch):
    clip = tmp_path / "big.wav"
    with clip.open("wb") as fh:
        fh.truncate(cli_module.MAX_ENROLL_UPLOAD_BYTES + 1)
    result = run("enroll", "Dana", "--file", str(clip))
    assert result.exit_code == 1 and "25 MB" in result.output
    assert local["calls"] == []


def test_enroll_record_uploads_an_in_memory_wav(local, tmp_path, monkeypatch):
    import io
    import wave

    import numpy as np

    _config(monkeypatch, sample_rate=16000, input_device="USB Mic")
    made = []

    class FakeRecorder:
        def __init__(self, sample_rate, device):
            made.append((sample_rate, device))

        def start(self):
            made.append("start")

        def stop(self):
            made.append("stop")
            return np.full(16000 * 2, 0.5, dtype=np.float32)

    monkeypatch.setattr("localflow.audio.Recorder", FakeRecorder)
    monkeypatch.setattr(cli_module.time, "sleep", lambda s: made.append(("sleep", s)))
    runner = CliRunner()
    with runner.isolated_filesystem(temp_dir=tmp_path) as cwd:
        result = runner.invoke(cli_module.cli, ["speakers", "enroll", "Dana", "--record", "7"])
        assert result.exit_code == 0, result.output
        assert list(Path(cwd).iterdir()) == []  # nothing written to disk
    assert made == [(16000, "USB Mic"), "start", ("sleep", 7), "stop"]
    [(_, _, kwargs, _)] = local["calls"]
    name, data, _ = kwargs["files"]["file"]
    assert name == "recording.wav"
    with wave.open(io.BytesIO(data)) as wav:
        assert (wav.getnchannels(), wav.getsampwidth(), wav.getframerate(),
                wav.getnframes()) == (1, 2, 16000, 32000)
        frames = np.frombuffer(wav.readframes(10), dtype="<i2")
    assert list(frames) == [16383] * 10


def test_rename_and_forget(http):
    assert run("rename", "Dana Lee/..", "Dana").output == "renamed to Dana\n"
    assert run("forget", "Dana Lee").output == "forgot 2 speaker(s)\n"
    assert run("forget", "--all").output == "forgot 2 speaker(s)\n"
    assert http == [("patch", "/api/speakers/Dana%20Lee%2F..", {"name": "Dana"}),
                    ("delete", "/api/speakers/Dana%20Lee", None),
                    ("delete", "/api/speakers", None)]


@pytest.mark.parametrize("args", [[], ["Dana", "--all"]])
def test_forget_needs_a_name_or_all(http, args):
    assert run("forget", *args).exit_code == 2
    assert http == []


def _export_zip(tmp_path):
    import numpy as np

    from localflow.diarization.registry import FileSpeakerRegistry

    registry = FileSpeakerRegistry(tmp_path / "src")
    vector = np.zeros(64, dtype=np.float32)
    vector[3] = 1.0
    vp = registry.enroll("Dana", [vector], "mock")
    return registry.export_zip(), vp


def test_export_writes_the_bundle_and_prints_no_vectors(local, tmp_path, monkeypatch):
    _config(monkeypatch, vault_path=str(tmp_path / "vault"))
    data, vp = _export_zip(tmp_path)
    local["reply"] = Sent(content=data)
    out = tmp_path / "backup"
    result = run("export", str(out))
    assert result.exit_code == 0, result.output
    assert result.output.splitlines() == [
        f"exported 1 speaker(s) to {out}", "These files are voiceprints. Keep them private."]
    assert sorted(p.name for p in out.iterdir()) == sorted(["registry.json", f"{vp.id}.npy"])
    [(method, url, _, trust_env)] = local["calls"]
    assert (method, url.endswith("/api/speakers/export"), trust_env) == ("get", True, False)
    assert "1.0" not in result.output and vp.id not in result.output


def test_export_refuses_non_empty_folders_and_bad_bundles(local, tmp_path, monkeypatch):
    _config(monkeypatch, vault_path=str(tmp_path / "vault"))
    full = tmp_path / "full"
    full.mkdir()
    (full / "x").write_text("x")
    assert run("export", str(full)).exit_code == 1
    assert local["calls"] == []
    local["reply"] = Sent(content=b"not a zip")
    result = run("export", str(tmp_path / "new"))
    assert result.exit_code == 1 and "not a speaker export" in result.output
    assert not (tmp_path / "new").exists()


def test_export_refuses_the_obsidian_vault_even_through_a_symlink(local, tmp_path, monkeypatch):
    vault = tmp_path / "vault"
    (vault / "People").mkdir(parents=True)
    _config(monkeypatch, vault_path=str(vault))
    targets = [vault, vault / "People" / "voices"]
    link = tmp_path / "innocent"
    try:
        link.symlink_to(vault / "People", target_is_directory=True)
        targets.append(link / "voices")
    except (OSError, NotImplementedError):
        pass
    for target in targets:
        result = run("export", str(target))
        assert result.exit_code == 1 and "vault" in result.output
    assert local["calls"] == []
    assert not (vault / "People" / "voices").exists()


@pytest.mark.parametrize("host", ["localhost", "LOCALHOST", "[localhost]"])
def test_configured_localhost_uses_a_literal_loopback_address(local, tmp_path, monkeypatch,
                                                              host):
    _config(monkeypatch, server_host=host, server_port=8756)
    clip = tmp_path / "clip.wav"
    clip.write_bytes(b"x")
    assert run("enroll", "Dana", "--file", str(clip)).exit_code == 0
    [(_, url, _, _)] = local["calls"]
    assert url == "http://127.0.0.1:8756/api/speakers"


@pytest.mark.parametrize("status", [301, 302, 307, 308])
def test_redirects_are_refused_for_audio_and_export(local, tmp_path, monkeypatch, status):
    _config(monkeypatch, vault_path=str(tmp_path / "vault"))
    data, _ = _export_zip(tmp_path)
    local["reply"] = Sent(content=data, status=status, data={"name": "Dana"})
    clip = tmp_path / "clip.wav"
    clip.write_bytes(b"x")
    enroll = run("enroll", "Dana", "--file", str(clip))
    assert enroll.exit_code == 1 and "redirect" in enroll.output
    assert "enrolled" not in enroll.output
    export = run("export", str(tmp_path / "out"))
    assert export.exit_code == 1 and "redirect" in export.output
    assert not (tmp_path / "out").exists()
    assert all(kwargs["allow_redirects"] is False for _, _, kwargs, _ in local["calls"])


@pytest.mark.parametrize("status", [199, 403, 500])
def test_export_writes_nothing_unless_the_server_answered_2xx(local, tmp_path, monkeypatch,
                                                              status):
    _config(monkeypatch, vault_path=str(tmp_path / "vault"))
    data, _ = _export_zip(tmp_path)  # even a valid bundle body is ignored on non-2xx
    local["reply"] = Sent(content=data, status=status, data={"detail": "export refused"})
    result = run("export", str(tmp_path / "out"))
    assert result.exit_code == 1 and "export refused" in result.output
    assert not (tmp_path / "out").exists()
