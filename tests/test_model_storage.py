"""Free up space: speech model inventory and safe deletion on a fake HF cache.

Every test builds its own cache and lease folder under tmp_path (HOME is
redirected too); the real Hugging Face cache and real leases are never
listed or touched.
"""

import json
import os

import pytest

from localflow import model_storage as ms
from localflow import stt_lease
from localflow.config import Config
from localflow.stt_lease import Lease

pytestmark = pytest.mark.skipif(os.name == "nt", reason="needs POSIX symlinks")

FW_SMALL = "Systran/faster-whisper-small"
FW_BASE = "Systran/faster-whisper-base"
FW_TINY = "Systran/faster-whisper-tiny"
MLX_BASE = "mlx-community/whisper-base-mlx"
QWEN = "Qwen/Qwen2.5-14B-Instruct"
NONE = []  # no live STT leases


@pytest.fixture(autouse=True)
def isolated_home(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))


def _repo(cache, repo, blobs, *, snapshots=("rev1",)):
    """A folder laid out like huggingface_hub: blobs + snapshot symlinks."""
    folder = cache / ms.repo_folder_name(repo)
    (folder / "blobs").mkdir(parents=True)
    (folder / "refs").mkdir()
    (folder / "refs" / "main").write_text("rev1")
    for name, size in blobs.items():
        (folder / "blobs" / name).write_bytes(b"x" * size)
    for rev in snapshots:
        snap = folder / "snapshots" / rev
        snap.mkdir(parents=True)
        for index, name in enumerate(blobs):
            (snap / f"file{index}").symlink_to(os.path.join("..", "..", "blobs", name))
    return folder


@pytest.fixture
def cache(tmp_path):
    cache = tmp_path / "hub"
    cache.mkdir()
    _repo(cache, FW_SMALL, {"a" * 40: 1000, "b" * 40: 500}, snapshots=("rev1", "rev2"))
    _repo(cache, FW_BASE, {"e" * 40: 200})
    _repo(cache, MLX_BASE, {"c" * 40: 300})
    _repo(cache, QWEN, {"d" * 40: 5000})  # unrelated 15 GB-style LLM
    outside = tmp_path / "external-model"
    (outside / "blobs").mkdir(parents=True)
    (outside / "blobs" / "big").write_bytes(b"y" * 7000)
    (cache / ms.repo_folder_name(FW_TINY)).symlink_to(outside, target_is_directory=True)
    # A snapshot link that escapes its model folder is neither counted nor followed.
    (cache / ms.repo_folder_name(MLX_BASE) / "snapshots" / "rev1" / "escape").symlink_to(
        outside / "blobs" / "big"
    )
    return cache


def no_mlx():
    return False


def has_mlx():
    return True


def _exists(cache, repo):
    return (cache / ms.repo_folder_name(repo)).is_dir()


# ---------------------------------------------------------------- inventory ---


def test_inventory_lists_only_catalog_models_with_deduplicated_sizes(cache):
    found = ms.inventory(Config(stt_backend="faster-whisper", model_size="small"),
                         cache=cache, mlx_available=no_mlx, leases=NONE)
    by_repo = {model.repo: model for model in found}

    assert set(by_repo) == {FW_SMALL, FW_BASE, MLX_BASE}  # no Qwen, no symlinked folder
    # blobs once each (two snapshots point at them) + the 4-byte ref
    assert by_repo[FW_SMALL].size_bytes == 1000 + 500 + 4
    assert by_repo[MLX_BASE].size_bytes == 300 + 4  # escaping link not counted
    assert by_repo[FW_SMALL].status == "Selected"  # config only: not claimed as running
    assert by_repo[MLX_BASE].status == "" and not by_repo[MLX_BASE].protected
    assert by_repo[FW_SMALL].label == "Whisper Small"
    assert found[0].repo == FW_SMALL  # biggest first


def test_running_model_is_marked_in_use_even_after_the_settings_changed(cache):
    # Dictation still runs Small (loaded at start); the user since chose Base.
    running = [Lease(4242, "faster-whisper", FW_SMALL, True)]
    found = {m.repo: m for m in ms.inventory(
        Config(stt_backend="faster-whisper", model_size="base"),
        cache=cache, mlx_available=no_mlx, leases=running,
    )}
    assert found[FW_SMALL].status == "In use"
    assert found[FW_BASE].status == "Selected"
    with pytest.raises(ms.ModelStorageError, match="loaded by a running LocalFlow process"):
        ms.delete_unused(FW_SMALL, Config(stt_backend="faster-whisper", model_size="base"),
                         cache=cache, mlx_available=no_mlx, leases=running)
    assert _exists(cache, FW_SMALL)


def test_auto_fallback_protects_what_actually_loaded(cache):
    # mlx_whisper's spec exists but its import failed at load time, so the
    # running app fell back to faster-whisper base.
    running = [Lease(4242, "faster-whisper", FW_BASE, True)]
    config = Config(stt_backend="auto", model_size="base")
    found = {m.repo: m for m in ms.inventory(config, cache=cache, mlx_available=has_mlx,
                                             leases=running)}
    assert found[FW_BASE].status == "In use"
    assert found[MLX_BASE].status == "Selected"  # could load: not proven either way
    for repo in (FW_BASE, MLX_BASE):
        with pytest.raises(ms.ModelStorageError):
            ms.delete_unused(repo, config, cache=cache, mlx_available=has_mlx, leases=running)
        assert _exists(cache, repo)


@pytest.mark.parametrize(("config", "mlx", "expected"), [
    # spec present: MLX may load, or fall back: both are protected
    (Config(stt_backend="auto", model_size="base"), True, {MLX_BASE, FW_BASE}),
    # spec missing: the MLX import cannot succeed
    (Config(stt_backend="auto", model_size="base"), False, {FW_BASE}),
    (Config(stt_backend="auto", model_size="base", stt_device="cpu"), True, {FW_BASE}),
    (Config(stt_backend="mlx-whisper", model_size="distil-large-v3"), True,
     {"Systran/faster-distil-whisper-large-v3"}),
    (Config(stt_backend="faster-whisper", model_size="large"), True,
     {"Systran/faster-whisper-large-v3"}),
    (Config(stt_backend="faster-whisper", model_size="org/custom-ct2"), True,
     {"org/custom-ct2"}),
    (Config(stt_backend="faster-whisper", model_size="/models/local"), True, set()),
    (Config(stt_backend="parakeet", model_size="large-v3"), True, {ms.PARAKEET_REPO}),
])
def test_selected_repos_mirror_build_stt_conservatively(config, mlx, expected):
    assert ms.selected_repos(config, lambda: mlx) == frozenset(expected)


def test_plugin_backend_is_unknown_and_blocks_deletion(cache):
    config = Config(stt_backend="my-plugin")
    assert ms.selected_repos(config, no_mlx) is ms.UNKNOWN
    assert all(m.status == "Unknown use" for m in ms.inventory(
        config, cache=cache, mlx_available=no_mlx, leases=NONE))
    with pytest.raises(ms.ModelStorageError, match="can't tell"):
        ms.delete_unused(MLX_BASE, config, cache=cache, mlx_available=no_mlx, leases=NONE)
    assert _exists(cache, MLX_BASE)


def test_unknown_running_model_blocks_deletion(cache):
    unknown = [Lease(4242, "unknown", None, False)]
    with pytest.raises(ms.ModelStorageError, match="can't identify"):
        ms.delete_unused(MLX_BASE, Config(stt_backend="parakeet"), cache=cache,
                         mlx_available=no_mlx, leases=unknown)
    assert _exists(cache, MLX_BASE)


def test_deleting_an_unused_model_frees_its_bytes_only(cache, tmp_path):
    config = Config(stt_backend="faster-whisper", model_size="small")
    freed = ms.delete_unused(MLX_BASE, config, cache=cache, mlx_available=no_mlx,
                             leases=NONE)

    assert freed == 304
    assert not _exists(cache, MLX_BASE)
    assert _exists(cache, FW_SMALL) and _exists(cache, QWEN)
    assert (tmp_path / "external-model" / "blobs" / "big").stat().st_size == 7000


def test_selection_is_rechecked_at_click_time(cache):
    listed = ms.inventory(Config(stt_backend="faster-whisper", model_size="small"),
                          cache=cache, mlx_available=no_mlx, leases=NONE)
    assert not next(m for m in listed if m.repo == MLX_BASE).protected
    # The user switched to MLX base before clicking Delete.
    now = Config(stt_backend="auto", model_size="base")
    with pytest.raises(ms.ModelStorageError, match="in your settings"):
        ms.delete_unused(MLX_BASE, now, cache=cache, mlx_available=has_mlx, leases=NONE)
    assert _exists(cache, MLX_BASE)


def test_default_leases_are_read_from_the_lease_folder(cache, tmp_path):
    folder = tmp_path / "home" / ".localflow" / "stt-leases"
    assert stt_lease.lease_dir() == folder
    stt_lease.record("faster-whisper", "base")  # this (live) test process
    with pytest.raises(ms.ModelStorageError, match="running"):
        ms.delete_unused(FW_BASE, Config(stt_backend="parakeet"), cache=cache,
                         mlx_available=no_mlx)
    stt_lease.release(folder / f"{os.getpid()}.json")
    assert ms.delete_unused(FW_BASE, Config(stt_backend="parakeet"), cache=cache,
                            mlx_available=no_mlx) == 204


@pytest.mark.parametrize("repo", [QWEN, "../../etc", "/", "", "llama3.2:3b"])
def test_unknown_models_and_paths_are_refused(cache, repo):
    with pytest.raises(ms.ModelStorageError, match="isn't a speech model"):
        ms.delete_unused(repo, Config(), cache=cache, mlx_available=no_mlx, leases=NONE)
    assert _exists(cache, QWEN)


def test_symlinked_model_folder_is_never_deleted(cache, tmp_path):
    with pytest.raises(ms.ModelStorageError, match="isn't downloaded"):
        ms.delete_unused(FW_TINY, Config(stt_backend="parakeet"), cache=cache,
                         mlx_available=no_mlx, leases=NONE)
    assert (tmp_path / "external-model" / "blobs" / "big").exists()


def test_missing_cache_lists_nothing(tmp_path):
    assert ms.inventory(Config(), cache=tmp_path / "nope", mlx_available=no_mlx,
                        leases=NONE) == []


# ------------------------------------------------------------------ leases ---


@pytest.mark.parametrize(("backend", "size", "expected"), [
    ("mlx", "base", (MLX_BASE, True)),
    ("faster-whisper", "base", (FW_BASE, True)),       # auto that fell back
    ("faster-whisper", "/models/local", (None, True)),  # nothing in the cache
    ("parakeet", "large-v3", (ms.PARAKEET_REPO, True)),
    ("my-plugin", "x", (None, False)),
])
def test_lease_records_what_actually_loaded(backend, size, expected):
    assert stt_lease.runtime_repo(backend, size) == expected


def test_leases_are_private_live_only_and_tolerate_junk(tmp_path):
    folder = tmp_path / "leases"
    path = stt_lease.record("mlx", "base", root=folder)
    assert path == folder / f"{os.getpid()}.json"
    assert oct(path.stat().st_mode & 0o777) == oct(0o600)
    data = json.loads(path.read_text())
    assert set(data) == {"version", "pid", "backend", "repo", "known", "created"}
    (folder / "999999.json").write_text(json.dumps(  # a dead process
        {"version": 1, "pid": 999999, "backend": "mlx", "repo": FW_SMALL, "known": True}))
    (folder / "4242.json").write_text("{broken")  # live but unreadable
    (folder / "notes.txt").write_text("ignored")
    alive = {os.getpid(), 4242}

    leases = stt_lease.live_leases(folder, alive=lambda pid: pid in alive)

    assert Lease(os.getpid(), "mlx", MLX_BASE, True) in leases
    assert Lease(4242, "unknown", None, False) in leases
    assert all(lease.pid != 999999 for lease in leases)  # stale: ignored
    assert stt_lease.live_leases(tmp_path / "missing") == []
    stt_lease.release(path)
    assert not path.exists()


def test_scoped_lease_coexists_with_the_long_lived_one(tmp_path):
    folder = tmp_path / "leases"
    long_lived = stt_lease.record("faster-whisper", "base", root=folder)
    scoped = stt_lease.record("mlx", "base", root=folder, scoped=True)
    other = stt_lease.record("parakeet", "large-v3", root=folder, scoped=True)

    assert long_lived == folder / f"{os.getpid()}.json"
    assert len({long_lived, scoped, other}) == 3  # unique names, nothing replaced
    assert scoped.name.startswith(f"{os.getpid()}.") and scoped.name != long_lived.name
    leases = stt_lease.live_leases(folder, alive=lambda pid: pid == os.getpid())
    assert {lease.repo for lease in leases} == {FW_BASE, MLX_BASE, ms.PARAKEET_REPO}

    stt_lease.release(scoped)
    assert not scoped.exists() and long_lived.exists() and other.exists()
    stt_lease.release(other)
    assert [lease.repo for lease in stt_lease.live_leases(
        folder, alive=lambda pid: pid == os.getpid())] == [FW_BASE]
    stt_lease.release(long_lived)


@pytest.mark.parametrize("name", [
    "12.json.bak", "12.ABCDEF0123456789.json", "12.short.json", "-1.json", "12..json",
    "x12.json", "12.0123456789abcdef.extra.json",
])
def test_lease_names_are_parsed_strictly(tmp_path, name):
    folder = tmp_path / "leases"
    folder.mkdir()
    (folder / name).write_text("{}")
    assert stt_lease.live_leases(folder, alive=lambda _pid: True) == []


def test_lease_with_mismatched_pid_is_unknown(tmp_path):
    folder = tmp_path / "leases"
    folder.mkdir()
    (folder / "4242.json").write_text(json.dumps(
        {"version": 1, "pid": 1, "backend": "mlx", "repo": MLX_BASE, "known": True}))
    assert stt_lease.live_leases(folder, alive=lambda _pid: True) == [
        Lease(4242, "unknown", None, False)
    ]


def test_runtime_paths_record_leases():
    from pathlib import Path

    root = Path(ms.__file__).parent
    assert "stt_lease.record(transcriber.backend, config.model_size)" in (
        root / "app.py").read_text(encoding="utf-8")
    assert "stt_lease.record(_transcriber.backend, _config.model_size)" in (
        root / "server.py").read_text(encoding="utf-8")


# ------------------------------------------------------------------- misc ---


def test_hf_cache_location_follows_huggingface_variables():
    home = os.path.expanduser("~")
    assert ms.hf_hub_cache({}) == ms.Path(home, ".cache", "huggingface", "hub")
    assert ms.hf_hub_cache({"XDG_CACHE_HOME": "/x"}) == ms.Path("/x/huggingface/hub")
    assert ms.hf_hub_cache({"HF_HOME": "/h", "XDG_CACHE_HOME": "/x"}) == ms.Path("/h/hub")
    assert ms.hf_hub_cache(
        {"HUGGINGFACE_HUB_CACHE": "/old", "HF_HOME": "/h"}
    ) == ms.Path("/old")
    assert ms.hf_hub_cache(
        {"HF_HUB_CACHE": "/new", "HUGGINGFACE_HUB_CACHE": "/old"}
    ) == ms.Path("/new")


def test_catalog_matches_the_download_catalog_and_faster_whisper():
    from faster_whisper.utils import _MODELS

    from localflow.models import STT_MODELS

    assert set(ms.catalog()) == {model.repo for model in STT_MODELS}
    for size, repo in ms.FASTER_WHISPER_REPOS.items():
        assert _MODELS.get(size) == repo, size


def test_format_bytes():
    assert ms.format_bytes(512) == "512 bytes"
    assert ms.format_bytes(486_000_000) == "486 MB"
    assert ms.format_bytes(1_622_000_000) == "1.6 GB"
