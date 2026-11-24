"""Fetch, SHA-256 verify and cache the pinned STT corpus (stdlib only).

Clips are never committed. Each one resolves to ``<cache_dir>/<id>.wav`` plus
a ``<cache_dir>/<id>.sha256`` marker holding the pinned digest it was verified
against, so a cache hit never touches the network.

Two source kinds (see ``stt.json``):

* ``url``: a WAV downloaded directly; the digest is of the WAV bytes.
* ``archive_url`` + ``archive_member``: a FLAC member of a ``.tar.gz``. The
  archive is streamed once per URL (stopping as soon as every wanted member
  was seen), the digest is of the raw FLAC bytes, and ``ffmpeg`` decodes it to
  16 kHz mono 16-bit PCM WAV.

CLI: ``python -m evals.perf.corpus.download --cache-dir DIR [--smoke]``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
from collections.abc import Callable
from pathlib import Path
from typing import Any

DEFAULT_MANIFEST_PATH = Path(__file__).resolve().parent / "stt.json"
USER_AGENT = "localflow-perf-evals/1"
TIMEOUT_S = 120
_CHUNK = 1 << 16
_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")

Opener = Callable[..., Any]


def load_stt_manifest(path: Path = DEFAULT_MANIFEST_PATH) -> dict:
    """Read and sanity-check the corpus manifest."""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    clips = data.get("clips") if isinstance(data, dict) else None
    if not isinstance(clips, list) or not clips:
        raise ValueError(f"{path}: manifest needs a non-empty 'clips' list")
    seen: set[str] = set()
    for clip in clips:
        cid = clip.get("id") if isinstance(clip, dict) else None
        if not isinstance(cid, str) or not _ID_RE.match(cid):
            raise ValueError(f"{path}: bad clip id {cid!r}")
        if cid in seen:
            raise ValueError(f"{path}: duplicate clip id {cid!r}")
        seen.add(cid)
        if not _SHA256_RE.match(str(clip.get("sha256", ""))):
            raise ValueError(f"{path}: clip {cid}: sha256 must be 64 lowercase hex chars")
        has_url = bool(clip.get("url"))
        has_archive = bool(clip.get("archive_url")) and bool(clip.get("archive_member"))
        if has_url == has_archive:
            raise ValueError(f"{path}: clip {cid}: needs exactly one of url or archive_url+archive_member")
        if not isinstance(clip.get("reference"), str) or not clip["reference"].strip():
            raise ValueError(f"{path}: clip {cid}: reference must be non-empty")
    return data


def _select(manifest: dict, smoke_only: bool, clip_ids: list[str] | None) -> list[dict]:
    clips = list(manifest["clips"])
    if clip_ids is not None:
        by_id = {c["id"]: c for c in clips}
        unknown = [cid for cid in clip_ids if cid not in by_id]
        if unknown:
            raise ValueError(f"unknown clip ids: {', '.join(unknown)}")
        clips = [by_id[cid] for cid in clip_ids]
    if smoke_only:
        clips = [c for c in clips if c.get("smoke")]
    return clips


def _paths(cache_dir: Path, clip: dict) -> tuple[Path, Path]:
    return cache_dir / f"{clip['id']}.wav", cache_dir / f"{clip['id']}.sha256"


def _is_cached(cache_dir: Path, clip: dict) -> bool:
    wav, marker = _paths(cache_dir, clip)
    try:
        return wav.is_file() and marker.read_text(encoding="utf-8").strip() == clip["sha256"]
    except OSError:
        return False


def _request(url: str) -> urllib.request.Request:
    return urllib.request.Request(url, headers={"User-Agent": USER_AGENT})


def _mark(cache_dir: Path, clip: dict) -> None:
    _, marker = _paths(cache_dir, clip)
    tmp = marker.with_suffix(".sha256.tmp")
    tmp.write_text(clip["sha256"] + "\n", encoding="utf-8")
    os.replace(tmp, marker)


def _mismatch(clip: dict, got: str) -> ValueError:
    return ValueError(f"sha256 mismatch for clip {clip['id']}: expected {clip['sha256']}, got {got}")


def _download_direct(clip: dict, cache_dir: Path, urlopen: Opener) -> None:
    wav, _ = _paths(cache_dir, clip)
    fd, tmp_name = tempfile.mkstemp(prefix=f".{clip['id']}.", suffix=".part", dir=cache_dir)
    tmp = Path(tmp_name)
    try:
        digest = hashlib.sha256()
        with os.fdopen(fd, "wb") as out, urlopen(_request(clip["url"]), timeout=TIMEOUT_S) as resp:
            while chunk := resp.read(_CHUNK):
                digest.update(chunk)
                out.write(chunk)
        got = digest.hexdigest()
        if got != clip["sha256"]:
            raise _mismatch(clip, got)
        os.replace(tmp, wav)
    finally:
        tmp.unlink(missing_ok=True)
    _mark(cache_dir, clip)


def flac_to_wav(data: bytes, wav: Path, *, ffmpeg_bin: str = "ffmpeg") -> None:
    """Decode audio bytes to 16 kHz mono s16 WAV at ``wav`` (atomic rename)."""
    wav.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=wav.parent, prefix=".decode-") as tmp_dir:
        src = Path(tmp_dir) / "in.flac"
        dst = Path(tmp_dir) / "out.wav"
        src.write_bytes(data)
        cmd = [
            ffmpeg_bin, "-nostdin", "-hide_banner", "-loglevel", "error", "-y",
            "-i", str(src), "-map_metadata", "-1", "-bitexact",
            "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le", "-f", "wav", str(dst),
        ]
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, check=False, timeout=120)
        except FileNotFoundError as exc:
            raise RuntimeError(
                f"ffmpeg not found ({ffmpeg_bin!r}); install ffmpeg to decode the FLAC corpus"
            ) from exc
        if proc.returncode != 0 or not dst.is_file():
            raise RuntimeError(f"ffmpeg failed to decode {wav.name}: {proc.stderr.strip()[-500:]}")
        os.replace(dst, wav)


def _download_archive(
    url: str, clips: list[dict], cache_dir: Path, urlopen: Opener, ffmpeg_bin: str
) -> None:
    wanted = {c["archive_member"]: c for c in clips}
    found: set[str] = set()
    errors: list[str] = []
    with urlopen(_request(url), timeout=TIMEOUT_S) as resp, tarfile.open(fileobj=resp, mode="r|gz") as tar:
        for member in tar:
            clip = wanted.get(member.name)
            if clip is None or not member.isfile():
                continue
            fh = tar.extractfile(member)
            if fh is None:
                continue
            data = fh.read()
            found.add(member.name)
            got = hashlib.sha256(data).hexdigest()
            if got != clip["sha256"]:
                errors.append(str(_mismatch(clip, got)))
            else:
                wav, _ = _paths(cache_dir, clip)
                flac_to_wav(data, wav, ffmpeg_bin=ffmpeg_bin)
                _mark(cache_dir, clip)
            if len(found) == len(wanted):
                break
    missing = sorted(set(wanted) - found)
    if missing:
        errors.append(f"{url}: archive members not found: {', '.join(missing)}")
    if errors:
        raise ValueError("; ".join(errors))


def ensure_stt_clips(
    manifest: dict,
    cache_dir: Path,
    *,
    smoke_only: bool = False,
    clip_ids: list[str] | None = None,
    urlopen: Opener | None = None,
    ffmpeg_bin: str = "ffmpeg",
) -> list[dict]:
    """Make every selected clip available as a verified WAV in ``cache_dir``.

    Returns ``[{"id", "path", "reference", "duration_s", "smoke"}]`` in
    manifest order. Raises ``ValueError`` on a digest mismatch or a missing
    archive member (nothing unverified is ever left at ``<id>.wav``).
    """
    opener = urlopen or urllib.request.urlopen
    cache_dir = Path(cache_dir)
    cache_dir.mkdir(parents=True, exist_ok=True)
    clips = _select(manifest, smoke_only, clip_ids)
    archives: dict[str, list[dict]] = {}
    for clip in clips:
        if _is_cached(cache_dir, clip):
            continue
        if clip.get("url"):
            _download_direct(clip, cache_dir, opener)
        else:
            archives.setdefault(clip["archive_url"], []).append(clip)
    for url, group in archives.items():
        _download_archive(url, group, cache_dir, opener, ffmpeg_bin)
    return [
        {
            "id": clip["id"],
            "path": str(_paths(cache_dir, clip)[0]),
            "reference": clip["reference"],
            "duration_s": clip.get("duration_s"),
            "smoke": bool(clip.get("smoke")),
        }
        for clip in clips
    ]


def default_cache_dir() -> Path:
    """``LF_PERF_CACHE_DIR``/stt, else ``$XDG_CACHE_HOME`` or ``~/.cache``."""
    root = os.environ.get("LF_PERF_CACHE_DIR")
    if root:
        return Path(root) / "stt"
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "localflow-perf" / "stt"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Download and verify the pinned STT corpus.")
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST_PATH)
    parser.add_argument("--cache-dir", type=Path, default=None)
    parser.add_argument("--smoke", action="store_true", help="only the smoke clips")
    args = parser.parse_args(argv)
    try:
        clips = ensure_stt_clips(
            load_stt_manifest(args.manifest), args.cache_dir or default_cache_dir(), smoke_only=args.smoke
        )
    except (OSError, RuntimeError, ValueError, tarfile.TarError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    for clip in clips:
        print(f"{clip['id']}\t{clip['path']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
