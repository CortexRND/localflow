"""Speaker-label models: pinned catalog, offline checks, download, Free up space.

Everything runs on a fake Hugging Face cache under tmp_path with two tiny fake
models (real SHA-256 of their payloads). The network seam
`diar_models._open_stream` fails the test unless a test installs a fake.
"""

import hashlib
import importlib.abc
import importlib.machinery
import importlib.util
import os
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest
import requests

from localflow import model_storage as ms
from localflow.config import Config
from localflow.diarization import models as dm
from localflow.models import DownloadCancelled, DownloadError, friendly_download_error

REAL_MODELS = dm.DIAR_MODELS
posix_only = pytest.mark.skipif(os.name == "nt", reason="needs POSIX symlinks")

SEG_BYTES = bytes(range(256)) * 2400            # 614400 bytes: three chunks
EMB_BYTES = bytes(reversed(range(256))) * 1300  # 332800 bytes: two chunks


def _fake_model(model_id, repo, revision, filename, payload):
    return dm.DiarModel(
        id=model_id, label=f"Speaker labels: {model_id}", repo=repo, revision=revision,
        filename=filename, sha256=hashlib.sha256(payload).hexdigest(),
        size_bytes=len(payload), licence="MIT", source="test fixture",
    )


SEG = _fake_model("seg", "test-org/seg-model", "a" * 40, "model.onnx", SEG_BYTES)
EMB = _fake_model("emb", "test-org/emb-model", "b" * 40, "voice embed.onnx", EMB_BYTES)
PAYLOADS = {SEG.id: SEG_BYTES, EMB.id: EMB_BYTES}


def url_for(model, endpoint="https://huggingface.co"):
    filename = model.filename.replace(" ", "%20")
    return f"{endpoint}/{model.repo}/resolve/{model.revision}/{filename}"


SIGNED_URL = "https://cdn.example/blob?X-Amz-Signature=SECRET-SIGNATURE"


class FakeResponse:
    def __init__(self, payload, status_code=200):
        self.payload = payload
        self.status_code = status_code
        self.closed = False

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.closed = True
        return False

    def raise_for_status(self):
        if self.status_code >= 400:  # like requests: the message names the final URL
            raise requests.HTTPError(
                f"{self.status_code} Client Error: Forbidden for url: {SIGNED_URL}",
                response=self,
            )

    def iter_content(self, size):
        for start in range(0, len(self.payload), size):
            yield self.payload[start:start + size]


class FakeNetwork:
    def __init__(self):
        self.urls: list[str] = []
        self.payloads: dict[str, bytes] = {}
        self.error: Exception | None = None
        self.status_code = 200

    def serve(self, model, payload=None, endpoint="https://huggingface.co"):
        self.payloads[url_for(model, endpoint)] = (
            PAYLOADS[model.id] if payload is None else payload
        )

    def open(self, url):
        self.urls.append(url)
        if self.error is not None:
            raise self.error
        return FakeResponse(self.payloads[url], self.status_code)


@pytest.fixture(autouse=True)
def isolated(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    monkeypatch.setenv("HF_HUB_CACHE", str(tmp_path / "hub"))
    monkeypatch.delenv("HF_ENDPOINT", raising=False)
    monkeypatch.setattr(dm, "DIAR_MODELS", (SEG, EMB))

    def no_network(url):
        raise AssertionError(f"network: {url}")

    monkeypatch.setattr(dm, "_open_stream", no_network)


@pytest.fixture
def cache(tmp_path):
    return tmp_path / "hub"


@pytest.fixture
def net(monkeypatch):
    network = FakeNetwork()
    network.serve(SEG)
    network.serve(EMB)
    monkeypatch.setattr(dm, "_open_stream", network.open)
    return network


def folder(cache, model):
    return cache / ms.repo_folder_name(model.repo)


def pointer(cache, model):
    return folder(cache, model) / "snapshots" / model.revision / model.filename


def blob(cache, model):
    return folder(cache, model) / "blobs" / model.sha256


def assert_no_partials(root):
    for directory, _dirs, files in os.walk(root):  # never follows links
        leftovers = [name for name in files if name.endswith(".lf-part")]
        assert not leftovers, (directory, leftovers)


def download(cache, cancel=None):
    progress = []
    paths = dm.download_diarization_models(
        lambda done, total: progress.append((done, total)), cancel, cache=cache
    )
    return paths, progress


def no_symlinks(monkeypatch):
    def refuse(*args, **kwargs):
        raise OSError("symlinks unavailable")

    monkeypatch.setattr(os, "symlink", refuse)


# ------------------------------------------------------------------ catalog ---


def test_pinned_catalog_is_exact():
    seg, emb = REAL_MODELS
    assert seg == dm.DiarModel(
        id="pyannote-segmentation-3.0",
        label="Speaker labels: segmentation",
        repo="csukuangfj/sherpa-onnx-pyannote-segmentation-3-0",
        revision="9403a6902bb58e3d5ae8c7e77c3422de279db2e0",
        filename="model.onnx",
        sha256="220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
        size_bytes=5992913,
        licence="MIT",
        source=seg.source,
    )
    assert emb == dm.DiarModel(
        id="3dspeaker-campplus-zh-en-16k-advanced",
        label="Speaker labels: voice embedding",
        repo="csukuangfj/speaker-embedding-models",
        revision="a8055d6daef723a6a4ecf4de233708575bf4994f",
        filename="3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx",
        sha256="53cab20ba8f2eb2f6e2d89ecc18e88ef3222169b61b4419cbc13666744a4574b",
        size_bytes=28281165,
        licence="Apache-2.0",
        source=emb.source,
    )
    hexdigits = set("0123456789abcdef")
    for model in REAL_MODELS:
        assert len(model.revision) == 40 and set(model.revision) <= hexdigits
        assert len(model.sha256) == 64 and set(model.sha256) <= hexdigits
    assert "MIT" in seg.source
    assert "iic/speech_campplus_sv_zh_en_16k-common_advanced" in emb.source
    assert "Apache" in emb.source


def test_diarization_repos_follow_the_catalog_at_call_time():
    assert dm.diarization_repos() == {SEG.repo: SEG.label, EMB.repo: EMB.label}
    assert list(ms.catalog())[-2:] == [SEG.repo, EMB.repo]


# ------------------------------------------------------------ offline check ---


def test_empty_cache_is_not_ready_and_creates_nothing(cache):
    ready, reason = dm.models_ready()
    assert ready is False
    assert "lf speakers download" in reason
    assert not cache.exists()
    with pytest.raises(dm.DiarizationModelsUnavailable, match="lf speakers download"):
        dm.verified_model_paths()
    info = dm.status()
    assert info["models_ready"] is False
    assert [item["ready"] for item in info["models"]] == [False, False]
    assert not cache.exists()


def test_model_path_is_the_snapshot_pointer(cache):
    assert dm.model_path(SEG) == pointer(cache, SEG)
    assert dm.model_path(SEG, cache) == pointer(cache, SEG)


# ----------------------------------------------------------------- download ---


def test_download_fetches_only_the_pinned_files(cache, net):
    download(cache)
    assert net.urls == [url_for(SEG), url_for(EMB)]


def test_download_honours_hf_endpoint(cache, net, monkeypatch):
    monkeypatch.setenv("HF_ENDPOINT", "https://mirror.example/")
    net.serve(SEG, endpoint="https://mirror.example")
    net.serve(EMB, endpoint="https://mirror.example")
    download(cache)
    assert net.urls == [
        url_for(SEG, "https://mirror.example"), url_for(EMB, "https://mirror.example"),
    ]


@posix_only
def test_download_publishes_the_hub_layout_and_is_ready(cache, net):
    paths, progress = download(cache)
    total = len(SEG_BYTES) + len(EMB_BYTES)
    assert progress[0] == (0, total)
    assert progress[-1] == (total, total)
    dones = [done for done, _ in progress]
    assert dones == sorted(dones)
    assert all(t == total for _, t in progress)
    assert len(progress) > 4  # per-chunk progress
    for model in (SEG, EMB):
        link = pointer(cache, model)
        assert paths[model.id] == link
        assert link.is_symlink()
        assert os.readlink(link) == os.path.join("..", "..", "blobs", model.sha256)
        assert blob(cache, model).read_bytes() == PAYLOADS[model.id]
        assert not (folder(cache, model) / "refs").exists()
    assert dm.models_ready() == (True, "")
    assert dm.verified_model_paths() == paths
    assert_no_partials(cache)
    net.urls.clear()
    again, _ = download(cache)
    assert again == paths
    assert net.urls == []


def test_copy_layout_without_symlinks(cache, net, monkeypatch):
    no_symlinks(monkeypatch)
    paths, _ = download(cache)
    for model in (SEG, EMB):
        assert not pointer(cache, model).is_symlink()
        assert pointer(cache, model).read_bytes() == PAYLOADS[model.id]
        assert not blob(cache, model).exists()
        assert paths[model.id] == pointer(cache, model)
    assert dm.models_ready() == (True, "")
    assert_no_partials(cache)


@pytest.mark.parametrize("payload", [
    bytes(len(SEG_BYTES)),     # right length, wrong bytes
    SEG_BYTES + b"extra",      # more than the pinned size
])
def test_download_rejects_bad_bytes_before_publishing(cache, net, payload):
    net.serve(SEG, payload)
    with pytest.raises(DownloadError, match="failed its checksum") as info:
        download(cache)
    assert not isinstance(info.value, DownloadCancelled)
    assert not os.path.lexists(pointer(cache, SEG))
    assert not os.path.lexists(blob(cache, SEG))
    assert net.urls == [url_for(SEG)]
    assert_no_partials(cache)
    assert dm.models_ready()[0] is False


def assert_sanitized(error, caplog):
    assert error.__cause__ is None and error.__suppress_context__ is True
    with caplog.at_level("WARNING"):
        friendly = friendly_download_error(error)
    for text in (str(error), friendly, caplog.text):
        assert "SECRET" not in text and "cdn.example" not in text
    return friendly


def test_http_error_reports_only_the_status_code(cache, net, caplog):
    net.status_code = 403
    with pytest.raises(DownloadError) as info:
        download(cache)
    assert str(info.value) == "Could not download model.onnx: HTTP 403"
    assert assert_sanitized(info.value, caplog) == "Could not download model.onnx: HTTP 403"
    assert_no_partials(cache)


def test_network_error_maps_to_the_friendly_network_message(cache, net, caplog):
    net.error = requests.ConnectionError(f"Max retries exceeded with url: {SIGNED_URL}")
    with pytest.raises(DownloadError) as info:
        download(cache)
    friendly = assert_sanitized(info.value, caplog)
    assert friendly.startswith("Network problem while downloading")
    assert_no_partials(cache)
    assert not os.path.lexists(pointer(cache, SEG))


def test_progress_callback_errors_propagate_and_clean_up(cache, net):
    def explode(done, total):
        if done:
            raise RuntimeError("boom")

    with pytest.raises(RuntimeError, match="boom"):
        dm.download_diarization_models(explode, cache=cache)
    assert_no_partials(cache)
    assert not os.path.lexists(pointer(cache, SEG))


# --------------------------------------------------- eviction and recovery ---


@pytest.mark.parametrize("layout", [
    pytest.param("symlink", marks=posix_only),
    "copy",
])
def test_checksum_mismatch_is_evicted_then_recovered(cache, net, monkeypatch, layout):
    if layout == "copy":
        no_symlinks(monkeypatch)
    download(cache)
    stored = pointer(cache, SEG) if layout == "copy" else blob(cache, SEG)
    stored.write_bytes(bytes(len(SEG_BYTES)))  # same length, wrong bytes
    ready, reason = dm.models_ready()
    assert ready is False
    assert reason == "model.onnx failed its checksum and was removed. Run `lf speakers download`."
    assert not os.path.lexists(pointer(cache, SEG))
    assert not os.path.lexists(blob(cache, SEG))
    assert dm.check_model(EMB)[0] == pointer(cache, EMB)  # the other file is kept
    net.urls.clear()
    download(cache)
    assert net.urls == [url_for(SEG)]
    assert dm.models_ready() == (True, "")
    assert_no_partials(cache)


@posix_only
def test_truncated_file_is_rejected_by_size(cache, net):
    download(cache)
    blob(cache, EMB).write_bytes(EMB_BYTES[:-1])
    path, reason = dm.check_model(EMB)
    assert path is None
    assert "failed its checksum" in reason
    assert not os.path.lexists(blob(cache, EMB))
    assert not os.path.lexists(pointer(cache, EMB))


# ----------------------------------------------------------- symlink safety ---


@posix_only
def test_pointer_linking_outside_the_repo_removes_only_the_link(cache, net, tmp_path):
    download(cache)
    outside = tmp_path / "outside.onnx"
    outside.write_bytes(b"precious")
    link = pointer(cache, SEG)
    link.unlink()
    link.symlink_to(outside)
    ready, reason = dm.models_ready()
    assert ready is False
    assert "didn't point to its pinned download" in reason
    assert not os.path.lexists(link)
    assert outside.read_bytes() == b"precious"


@posix_only
def test_pointer_linking_another_blob_keeps_that_blob(cache, net):
    download(cache)
    other = folder(cache, SEG) / "blobs" / "other"
    other.write_bytes(SEG_BYTES)  # even good bytes: only blobs/<sha256> counts
    link = pointer(cache, SEG)
    link.unlink()
    link.symlink_to(os.path.join("..", "..", "blobs", "other"))
    path, _ = dm.check_model(SEG)
    assert path is None
    assert not os.path.lexists(link)
    assert other.read_bytes() == SEG_BYTES
    assert blob(cache, SEG).read_bytes() == SEG_BYTES


@posix_only
def test_link_through_a_linked_alias_and_dotdot_is_not_trusted(cache, net, tmp_path):
    download(cache)
    outside = tmp_path / "outside"
    (outside / "subdir").mkdir(parents=True)
    other = outside / SEG.sha256
    other.write_bytes(bytes(len(SEG_BYTES)))  # other bytes under the pinned name
    blobs = folder(cache, SEG) / "blobs"
    (blobs / "alias").symlink_to(outside / "subdir", target_is_directory=True)
    link = pointer(cache, SEG)
    link.unlink()
    link.symlink_to(os.path.join("..", "..", "blobs", "alias", "..", SEG.sha256))
    assert os.path.normpath(os.path.join(link.parent, os.readlink(link))) == str(
        blob(cache, SEG)
    )  # lexically it looks like the pinned blob
    path, reason = dm.check_model(SEG)
    assert path is None
    assert "didn't point to its pinned download" in reason
    assert not os.path.lexists(link)
    assert other.read_bytes() == bytes(len(SEG_BYTES))
    assert blob(cache, SEG).read_bytes() == SEG_BYTES
    assert dm.models_ready()[0] is False


@posix_only
def test_link_loop_is_rejected_safely(cache, net):
    download(cache)
    link = pointer(cache, SEG)
    link.unlink()
    link.symlink_to(link.name)  # points at itself
    path, reason = dm.check_model(SEG)
    assert path is None
    assert "didn't point to its pinned download" in reason
    assert not os.path.lexists(link)
    assert blob(cache, SEG).read_bytes() == SEG_BYTES


class _JunctionStat:
    def __init__(self, real):
        self.st_mode = real.st_mode
        self.st_reparse_tag = dm._MOUNT_POINT_TAG


def test_windows_junction_is_not_a_plain_folder(cache, tmp_path, monkeypatch):
    repo = folder(cache, SEG)
    (repo / "snapshots" / SEG.revision).mkdir(parents=True)
    junction = repo / "snapshots"
    real_lstat = os.lstat

    def lstat(path, *args, **kwargs):
        info = real_lstat(path, *args, **kwargs)
        if isinstance(path, (str, os.PathLike)) and Path(path) == junction:
            return _JunctionStat(info)
        return info

    monkeypatch.setattr(os, "lstat", lstat)
    assert dm._is_plain_dir(repo) is True
    assert dm._is_plain_dir(junction) is False
    path, reason = dm.check_model(SEG)
    assert path is None
    assert "plain folder" in reason


@posix_only
def test_absolute_link_to_the_pinned_blob_is_accepted(cache, net):
    download(cache)
    link = pointer(cache, SEG)
    link.unlink()
    link.symlink_to(blob(cache, SEG))
    assert dm.check_model(SEG) == (link, "")


@posix_only
def test_linked_snapshots_directory_is_never_read_or_evicted(cache, tmp_path):
    outside = tmp_path / "outside-snapshots"
    (outside / SEG.revision).mkdir(parents=True)
    bad = outside / SEG.revision / SEG.filename
    bad.write_bytes(bytes(len(SEG_BYTES)))
    repo = folder(cache, SEG)
    (repo / "blobs").mkdir(parents=True)
    (repo / "snapshots").symlink_to(outside, target_is_directory=True)
    path, reason = dm.check_model(SEG)
    assert path is None
    assert "plain folder" in reason
    assert bad.read_bytes() == bytes(len(SEG_BYTES))


@posix_only
def test_linked_revision_directory_is_never_created_through(cache, net, tmp_path):
    outside = tmp_path / "outside-revision"
    outside.mkdir()
    repo = folder(cache, SEG)
    (repo / "blobs").mkdir(parents=True)
    (repo / "snapshots").mkdir()
    (repo / "snapshots" / SEG.revision).symlink_to(outside, target_is_directory=True)
    assert dm.check_model(SEG)[0] is None
    with pytest.raises(DownloadError, match="plain folder"):
        download(cache)
    assert list(outside.iterdir()) == []
    assert net.urls == []
    assert_no_partials(cache)


@posix_only
def test_linked_blobs_directory_is_never_read_evicted_or_written(cache, net, tmp_path):
    outside = tmp_path / "outside-blobs"
    outside.mkdir()
    bad = outside / SEG.sha256
    bad.write_bytes(bytes(len(SEG_BYTES)))
    repo = folder(cache, SEG)
    repo.mkdir(parents=True)
    (repo / "blobs").symlink_to(outside, target_is_directory=True)
    (repo / "snapshots" / SEG.revision).mkdir(parents=True)
    pointer(cache, SEG).symlink_to(os.path.join("..", "..", "blobs", SEG.sha256))
    assert dm.check_model(SEG)[0] is None
    assert bad.read_bytes() == bytes(len(SEG_BYTES))
    with pytest.raises(DownloadError, match="plain folder"):
        download(cache)
    assert sorted(p.name for p in outside.iterdir()) == [SEG.sha256]
    assert bad.read_bytes() == bytes(len(SEG_BYTES))
    assert net.urls == []


@posix_only
def test_linked_repo_folder_is_not_ready_and_never_written(cache, net, tmp_path):
    outside = tmp_path / "outside-repo"
    outside.mkdir()
    cache.mkdir()
    folder(cache, SEG).symlink_to(outside, target_is_directory=True)
    path, reason = dm.check_model(SEG)
    assert path is None
    assert "plain folder" in reason
    with pytest.raises(DownloadError, match="plain folder"):
        download(cache)
    assert list(outside.iterdir()) == []
    assert net.urls == []


# ------------------------------------------------------------- cancellation ---


def test_cancel_before_the_call_makes_no_request(cache, net):
    cancel = threading.Event()
    cancel.set()
    with pytest.raises(DownloadCancelled):
        download(cache, cancel)
    assert net.urls == []
    assert not cache.exists() or not any(cache.iterdir())


def test_cancel_mid_stream_discards_the_partial_file(cache, net):
    cancel = threading.Event()

    def on_progress(done, total):
        if 0 < done < len(SEG_BYTES):
            cancel.set()

    with pytest.raises(DownloadCancelled):
        dm.download_diarization_models(on_progress, cancel, cache=cache)
    assert net.urls == [url_for(SEG)]
    assert not os.path.lexists(pointer(cache, SEG))
    assert not os.path.lexists(blob(cache, SEG))
    assert_no_partials(cache)


def test_cancel_on_the_final_chunk_of_the_final_file_publishes_nothing_more(cache, net):
    cancel = threading.Event()
    total = len(SEG_BYTES) + len(EMB_BYTES)

    def on_progress(done, _total):
        if done == total:
            cancel.set()  # first report of the total: the last chunk's

    with pytest.raises(DownloadCancelled):
        dm.download_diarization_models(on_progress, cancel, cache=cache)
    assert dm.check_model(SEG)[0] is not None
    assert not os.path.lexists(pointer(cache, EMB))
    assert_no_partials(cache)


def test_cancel_after_the_last_file_still_raises_and_keeps_verified_files(cache, net):
    cancel = threading.Event()
    total = len(SEG_BYTES) + len(EMB_BYTES)
    seen = []

    def on_progress(done, _total):
        if done == total:
            seen.append(done)
            if len(seen) == 2:  # the boundary report after the last file
                cancel.set()

    with pytest.raises(DownloadCancelled):
        dm.download_diarization_models(on_progress, cancel, cache=cache)
    assert dm.models_ready() == (True, "")
    assert_no_partials(cache)


def test_cancel_between_files_skips_the_second(cache, net, monkeypatch):
    cancel = threading.Event()
    publish = dm._publish

    def publish_then_cancel(*args):
        path = publish(*args)
        cancel.set()
        return path

    monkeypatch.setattr(dm, "_publish", publish_then_cancel)
    with pytest.raises(DownloadCancelled):
        download(cache, cancel)
    assert net.urls == [url_for(SEG)]
    assert dm.check_model(SEG)[0] is not None
    assert not os.path.lexists(pointer(cache, EMB))
    assert_no_partials(cache)


# ----------------------------------------------------- stale partial files ---


@posix_only
def test_explicit_download_removes_only_old_matching_regular_partials(cache, net, tmp_path):
    blobs = folder(cache, SEG) / "blobs"
    blobs.mkdir(parents=True)
    old = time.time() - 2 * 60 * 60
    prefix = SEG.sha256[:16]

    def aged(path):
        os.utime(path, (old, old), follow_symlinks=False)
        return path

    stale = blobs / f"{prefix}.abc123{dm._PART_SUFFIX}"
    stale.write_bytes(b"left by a killed process")
    aged(stale)
    fresh = blobs / f"{prefix}.live99{dm._PART_SUFFIX}"
    fresh.write_bytes(b"another process is downloading")
    other_model = blobs / f"{EMB.sha256[:16]}.abc{dm._PART_SUFFIX}"
    other_model.write_bytes(b"x")
    aged(other_model)
    other_suffix = blobs / f"{prefix}.abc.tmp"
    other_suffix.write_bytes(b"x")
    aged(other_suffix)
    directory = blobs / f"{prefix}.dir{dm._PART_SUFFIX}"
    directory.mkdir()
    (directory / "inside").write_bytes(b"x")
    aged(directory)
    outside = tmp_path / "outside-part"
    outside.write_bytes(b"precious")
    aged(outside)
    link = blobs / f"{prefix}.link{dm._PART_SUFFIX}"
    link.symlink_to(outside)
    aged(link)

    assert dm.models_ready()[0] is False  # checks never clean up
    assert dm.status()["models_ready"] is False
    assert stale.exists()

    download(cache)
    assert not os.path.lexists(stale)
    for kept in (fresh, other_model, other_suffix, directory / "inside", link):
        assert os.path.lexists(kept), kept
    assert outside.read_bytes() == b"precious"
    assert dm.models_ready() == (True, "")


# ------------------------------------------------- offline, no runtime import ---


class _ExplodingLoader(importlib.abc.Loader):
    def create_module(self, spec):
        return None

    def exec_module(self, module):
        raise AssertionError("sherpa_onnx was imported")


class _FakeSherpaFinder(importlib.abc.MetaPathFinder):
    def find_spec(self, name, path=None, target=None):
        if name == "sherpa_onnx":
            return importlib.machinery.ModuleSpec(name, _ExplodingLoader())
        return None


@pytest.fixture
def offline(monkeypatch):
    def refuse(*args, **kwargs):
        raise AssertionError("socket opened")

    monkeypatch.setattr(socket, "socket", refuse)
    monkeypatch.setattr(socket, "create_connection", refuse)


def test_checks_are_offline_and_never_import_the_runtime(cache, net, offline, monkeypatch):
    monkeypatch.delitem(sys.modules, "sherpa_onnx", raising=False)
    monkeypatch.setattr(sys, "meta_path", [_FakeSherpaFinder(), *sys.meta_path])
    assert dm.runtime_installed() is True
    assert dm.models_ready()[0] is False
    info = dm.status()
    assert info["extra_installed"] is True
    assert info["models_ready"] is False
    assert "sherpa_onnx" not in sys.modules


@pytest.mark.skipif(
    importlib.util.find_spec("sherpa_onnx") is not None, reason="sherpa-onnx is installed"
)
def test_runtime_not_installed_is_reported(offline):
    assert dm.runtime_installed() is False
    assert dm.status()["extra_installed"] is False


@pytest.mark.parametrize("installed", [True, False])
def test_models_and_runtime_are_reported_independently(cache, net, monkeypatch, installed):
    monkeypatch.setattr(dm, "runtime_installed", lambda: installed)
    before = dm.status()
    assert (before["models_ready"], before["extra_installed"]) == (False, installed)
    download(cache)
    after = dm.status()
    assert (after["models_ready"], after["extra_installed"]) == (True, installed)
    assert after["reason"] == ""
    assert [item["ready"] for item in after["models"]] == [True, True]


def test_importing_storage_and_catalog_loads_no_runtime_or_hub_client():
    code = (
        "import sys, localflow.model_storage, localflow.diarization.models\n"
        "heavy = ['sherpa_onnx', 'faster_whisper', 'huggingface_hub', 'localflow.models']\n"
        "print([name for name in heavy if name in sys.modules])\n"
    )
    result = subprocess.run(
        [sys.executable, "-c", code], capture_output=True, text=True, check=True,
        cwd=Path(__file__).resolve().parents[1],
    )
    assert result.stdout.strip() == "[]"


def test_flags_report_the_three_speaker_settings():
    config = Config(speaker_diarization=True, diarize_mic=False, speaker_recognition=True)
    assert dm.flags(config) == {
        "speaker_diarization": True, "diarize_mic": False, "speaker_recognition": True,
    }


# ------------------------------------------------------------ Free up space ---


def _hub_repo(cache, repo, size):
    """A downloaded repo laid out like huggingface_hub."""
    repo_dir = cache / ms.repo_folder_name(repo)
    (repo_dir / "blobs").mkdir(parents=True)
    (repo_dir / "blobs" / ("f" * 64)).write_bytes(b"x" * size)
    (repo_dir / "snapshots" / "rev").mkdir(parents=True)
    (repo_dir / "snapshots" / "rev" / "model.onnx").symlink_to(
        os.path.join("..", "..", "blobs", "f" * 64)
    )
    return repo_dir


def _pinned_repo(cache, model, size, *, copy=False):
    """The pinned file of `model` (stat-only checks: the bytes need not hash)."""
    repo_dir = cache / ms.repo_folder_name(model.repo)
    revision_dir = repo_dir / "snapshots" / model.revision
    revision_dir.mkdir(parents=True, exist_ok=True)
    (repo_dir / "blobs").mkdir(exist_ok=True)
    if copy:
        (revision_dir / model.filename).write_bytes(b"c" * size)
    else:
        (repo_dir / "blobs" / model.sha256).write_bytes(b"p" * size)
        (revision_dir / model.filename).symlink_to(
            os.path.join("..", "..", "blobs", model.sha256)
        )
    return repo_dir


def _unrelated(repo_dir, revision, name, blob_name, size):
    """Another file in a shared repo that LocalFlow never fetched."""
    (repo_dir / "blobs" / blob_name).write_bytes(b"u" * size)
    (repo_dir / "snapshots" / revision).mkdir(parents=True, exist_ok=True)
    link = repo_dir / "snapshots" / revision / name
    link.symlink_to(os.path.join("..", "..", "blobs", blob_name))
    return link


@pytest.fixture
def storage(cache, monkeypatch):
    monkeypatch.setattr(dm, "DIAR_MODELS", REAL_MODELS)
    cache.mkdir()
    sizes = {}
    for index, model in enumerate(REAL_MODELS):
        sizes[model.repo] = 1000 * (index + 1)
        _pinned_repo(cache, model, sizes[model.repo])
    _hub_repo(cache, "Systran/faster-whisper-base", 200)
    return sizes


EMB_REAL = REAL_MODELS[1]  # csukuangfj/speaker-embedding-models: a shared repo
OFF = Config(speaker_diarization=False)


def _listed(cache, config=OFF):
    return {m.repo: m for m in ms.inventory(config, cache, no_mlx, leases=[])}


def no_mlx():
    return False


@posix_only
def test_free_up_space_lists_speaker_models_with_labels_and_sizes(cache, storage):
    listed = {m.repo: m for m in ms.inventory(Config(), cache, no_mlx, leases=[])}
    for model in REAL_MODELS:
        assert ms.catalog()[model.repo] == model.label
        assert listed[model.repo].label == model.label
        assert listed[model.repo].size_bytes == storage[model.repo]
        assert listed[model.repo].status == ""


@posix_only
@pytest.mark.parametrize("backend", ["faster-whisper", "auto", "parakeet"])
def test_speaker_models_are_protected_while_the_flag_is_on(cache, storage, backend):
    config = Config(stt_backend=backend, speaker_diarization=True)
    listed = {m.repo: m for m in ms.inventory(config, cache, no_mlx, leases=[])}
    for model in REAL_MODELS:
        assert listed[model.repo].status == "Selected"
        with pytest.raises(ms.ModelStorageError, match="speaker labels"):
            ms.delete_unused(model.repo, config, cache, no_mlx, leases=[])
        assert (cache / ms.repo_folder_name(model.repo)).is_dir()


@posix_only
def test_speaker_models_are_deletable_while_the_flag_is_off(cache, storage):
    config = Config(speaker_diarization=False)
    for model in REAL_MODELS:
        freed = ms.delete_unused(model.repo, config, cache, no_mlx, leases=[])
        assert freed == storage[model.repo]
        repo_dir = cache / ms.repo_folder_name(model.repo)
        assert not os.path.lexists(repo_dir / "blobs" / model.sha256)
        assert not (repo_dir / "snapshots").exists()  # empty snapshot folders go too
        assert model.repo not in _listed(cache)
    assert "Systran/faster-whisper-base" in _listed(cache)


@posix_only
def test_shared_repo_counts_and_deletes_only_the_pinned_file(cache, storage):
    repo_dir = cache / ms.repo_folder_name(EMB_REAL.repo)
    same_rev = _unrelated(repo_dir, EMB_REAL.revision, "other_embedder.onnx", "1" * 64, 70000)
    other_rev = _unrelated(repo_dir, "e" * 40, "wespeaker.onnx", "2" * 64, 50000)
    (repo_dir / "refs").mkdir()
    (repo_dir / "refs" / "main").write_text("e" * 40)
    (repo_dir / "README.md").write_text("not ours")
    assert _listed(cache)[EMB_REAL.repo].size_bytes == storage[EMB_REAL.repo]

    freed = ms.delete_unused(EMB_REAL.repo, OFF, cache, no_mlx, leases=[])
    assert freed == storage[EMB_REAL.repo]
    assert not os.path.lexists(repo_dir / "snapshots" / EMB_REAL.revision / EMB_REAL.filename)
    assert not os.path.lexists(repo_dir / "blobs" / EMB_REAL.sha256)
    assert same_rev.read_bytes() == b"u" * 70000
    assert other_rev.read_bytes() == b"u" * 50000
    assert (repo_dir / "refs" / "main").read_text() == "e" * 40
    assert (repo_dir / "README.md").read_text() == "not ours"
    assert EMB_REAL.repo not in _listed(cache)  # only unrelated files remain


@posix_only
def test_shared_repo_with_only_unrelated_files_is_not_listed_or_deleted(cache, monkeypatch):
    monkeypatch.setattr(dm, "DIAR_MODELS", REAL_MODELS)
    repo_dir = cache / ms.repo_folder_name(EMB_REAL.repo)
    (repo_dir / "blobs").mkdir(parents=True)
    link = _unrelated(repo_dir, EMB_REAL.revision, "other_embedder.onnx", "1" * 64, 9000)
    assert EMB_REAL.repo not in _listed(cache)
    with pytest.raises(ms.ModelStorageError, match="isn't downloaded"):
        ms.delete_unused(EMB_REAL.repo, OFF, cache, no_mlx, leases=[])
    assert link.read_bytes() == b"u" * 9000


@posix_only
def test_pinned_blob_shared_with_another_revision_is_kept(cache, storage):
    repo_dir = cache / ms.repo_folder_name(EMB_REAL.repo)
    blob_path = repo_dir / "blobs" / EMB_REAL.sha256
    other = repo_dir / "snapshots" / ("e" * 40) / EMB_REAL.filename
    other.parent.mkdir(parents=True)
    other.symlink_to(os.path.join("..", "..", "blobs", EMB_REAL.sha256))
    assert _listed(cache)[EMB_REAL.repo].size_bytes == storage[EMB_REAL.repo]

    freed = ms.delete_unused(EMB_REAL.repo, OFF, cache, no_mlx, leases=[])
    assert freed == 0
    assert not os.path.lexists(repo_dir / "snapshots" / EMB_REAL.revision)
    assert blob_path.read_bytes() == b"p" * storage[EMB_REAL.repo]
    assert other.read_bytes() == b"p" * storage[EMB_REAL.repo]


@posix_only
def test_pinned_blob_is_kept_when_a_snapshot_folder_cannot_be_inspected(
    cache, storage, tmp_path
):
    repo_dir = cache / ms.repo_folder_name(EMB_REAL.repo)
    outside = tmp_path / "outside-rev"
    outside.mkdir()
    (repo_dir / "snapshots" / ("e" * 40)).symlink_to(outside, target_is_directory=True)
    ms.delete_unused(EMB_REAL.repo, OFF, cache, no_mlx, leases=[])
    assert (repo_dir / "blobs" / EMB_REAL.sha256).exists()
    assert not os.path.lexists(repo_dir / "snapshots" / EMB_REAL.revision / EMB_REAL.filename)
    assert outside.is_dir()


@posix_only
def test_hard_linked_pinned_blob_is_kept(cache, storage, tmp_path):
    repo_dir = cache / ms.repo_folder_name(EMB_REAL.repo)
    os.link(repo_dir / "blobs" / EMB_REAL.sha256, tmp_path / "hardlink")
    assert ms.delete_unused(EMB_REAL.repo, OFF, cache, no_mlx, leases=[]) == 0
    assert (repo_dir / "blobs" / EMB_REAL.sha256).exists()


@posix_only
def test_malformed_pinned_link_is_never_counted_resolved_or_deleted(
    cache, monkeypatch, tmp_path
):
    monkeypatch.setattr(dm, "DIAR_MODELS", REAL_MODELS)
    outside = tmp_path / "outside.onnx"
    outside.write_bytes(b"o" * 4000)
    repo_dir = cache / ms.repo_folder_name(EMB_REAL.repo)
    revision_dir = repo_dir / "snapshots" / EMB_REAL.revision
    revision_dir.mkdir(parents=True)
    (repo_dir / "blobs").mkdir()
    (repo_dir / "blobs" / EMB_REAL.sha256).write_bytes(b"p" * 3000)
    link = revision_dir / EMB_REAL.filename
    link.symlink_to(outside)  # an absolute link outside the repo: not ours
    assert EMB_REAL.repo not in _listed(cache)
    with pytest.raises(ms.ModelStorageError, match="isn't downloaded"):
        ms.delete_unused(EMB_REAL.repo, OFF, cache, no_mlx, leases=[])
    assert os.path.islink(link)
    assert outside.read_bytes() == b"o" * 4000
    assert (repo_dir / "blobs" / EMB_REAL.sha256).exists()


@posix_only
def test_backslash_link_text_is_not_a_pinned_pointer_on_posix(cache, monkeypatch):
    monkeypatch.setattr(dm, "DIAR_MODELS", REAL_MODELS)
    repo_dir = cache / ms.repo_folder_name(EMB_REAL.repo)
    revision_dir = repo_dir / "snapshots" / EMB_REAL.revision
    revision_dir.mkdir(parents=True)
    (repo_dir / "blobs").mkdir()
    blob_path = repo_dir / "blobs" / EMB_REAL.sha256
    blob_path.write_bytes(b"p" * 3000)
    link = revision_dir / EMB_REAL.filename
    link.symlink_to("..\\..\\blobs\\" + EMB_REAL.sha256)  # one odd file name on POSIX
    assert not ms._is_pinned_link(link, EMB_REAL.sha256)
    assert EMB_REAL.repo not in _listed(cache)
    with pytest.raises(ms.ModelStorageError, match="isn't downloaded"):
        ms.delete_unused(EMB_REAL.repo, OFF, cache, no_mlx, leases=[])
    assert os.path.islink(link)
    assert blob_path.read_bytes() == b"p" * 3000


def test_plain_copy_layout_counts_and_deletes_only_the_pinned_copy(cache, monkeypatch):
    monkeypatch.setattr(dm, "DIAR_MODELS", REAL_MODELS)
    repo_dir = _pinned_repo(cache, EMB_REAL, 6000, copy=True)
    other_copy = repo_dir / "snapshots" / EMB_REAL.revision / "other_embedder.onnx"
    other_copy.write_bytes(b"u" * 8000)  # an unrelated Windows-style copy
    unrelated_blob = repo_dir / "blobs" / ("3" * 64)
    unrelated_blob.write_bytes(b"u" * 500)
    assert _listed(cache)[EMB_REAL.repo].size_bytes == 6000

    freed = ms.delete_unused(EMB_REAL.repo, OFF, cache, no_mlx, leases=[])
    assert freed == 6000
    assert not os.path.lexists(repo_dir / "snapshots" / EMB_REAL.revision / EMB_REAL.filename)
    assert other_copy.read_bytes() == b"u" * 8000
    assert unrelated_blob.read_bytes() == b"u" * 500
    assert EMB_REAL.repo not in _listed(cache)


@posix_only
def test_plugin_backend_still_fails_closed_with_the_flag_on(cache, storage):
    config = Config(stt_backend="my-plugin", speaker_diarization=True)
    assert ms.selected_repos(config, no_mlx) is ms.UNKNOWN
    listed = ms.inventory(config, cache, no_mlx, leases=[])
    assert listed and all(m.status == "Unknown use" for m in listed)


@pytest.mark.parametrize("backend", ["auto", "mlx", "faster-whisper", "parakeet", "plugin"])
@pytest.mark.parametrize("size", ["base", "small", "large-v3-turbo", "org/custom", "/local"])
@pytest.mark.parametrize("mlx", [False, True])
def test_flag_off_selection_is_unchanged(monkeypatch, backend, size, mlx):
    monkeypatch.setattr(dm, "DIAR_MODELS", REAL_MODELS)
    off = Config(stt_backend=backend, model_size=size)
    on = Config(stt_backend=backend, model_size=size, speaker_diarization=True)
    expected = ms._selected_stt_repos(off, lambda: mlx)
    if expected is ms.UNKNOWN:
        assert ms.selected_repos(off, lambda: mlx) is ms.UNKNOWN
        assert ms.selected_repos(on, lambda: mlx) is ms.UNKNOWN
    else:
        assert ms.selected_repos(off, lambda: mlx) == expected
        assert ms.selected_repos(on, lambda: mlx) == (
            expected | {model.repo for model in REAL_MODELS}
        )
