"""Adapter: ``scripts/ci_hub_rss.sh`` idle table → footprint cases.

The script (unchanged) writes, per measured build::

    ### <label> Hub idle RSS (single process, KiB)

    | Seconds after launch | RSS (KiB) |
    | --- | --- |
    | 10 | 51234 |
    | 20 | 51240 |
    | 30 | 51260 |

followed by diagnostics (vmmap/footprint dumps, which may contain other
tables). Several outputs may be concatenated (``$GITHUB_STEP_SUMMARY``).

One case per row: ``hub_rss/<label>/<seconds>s`` with ``rss_bytes``
(KiB × 1024). Only rows directly under the idle-RSS header are read.

Stdlib only.
"""

from __future__ import annotations

import re
from pathlib import Path

from ..schema import make_case
from ._common import SUITE, failed, kib_metric, read_text

DEFAULT_LABEL = "hub"
_HEADING = re.compile(r"^#{1,6}\s+(?P<label>.*?)\s+Hub idle RSS\b", re.IGNORECASE)
_HEADER = re.compile(r"^\|\s*Seconds after launch\s*\|\s*RSS \(KiB\)\s*\|\s*$", re.IGNORECASE)
_SEPARATOR = re.compile(r"^\|\s*:?-{3,}:?\s*\|\s*:?-{3,}:?\s*\|\s*$")
_ROW = re.compile(r"^\|\s*(?P<seconds>\d+(?:\.\d+)?)\s*\|\s*(?P<rss>\d+)\s*\|\s*$")


def _slug(label: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9._-]+", "-", label.strip()).strip("-").lower()
    return slug or DEFAULT_LABEL


def _seconds_key(raw: str) -> str:
    value = float(raw)
    return f"{int(value)}" if value.is_integer() else f"{value:g}"


def parse_hub_rss(text: str) -> list[tuple[str, str, int]]:
    """``[(label, seconds, rss_kib), ...]`` from every idle-RSS table in ``text``."""
    rows: list[tuple[str, str, int]] = []
    label = DEFAULT_LABEL
    in_table = False
    for raw in text.splitlines():
        line = raw.strip()
        heading = _HEADING.match(line)
        if heading:
            label = heading.group("label") or DEFAULT_LABEL
            in_table = False
            continue
        if _HEADER.match(line):
            in_table = True
            continue
        if not in_table:
            continue
        if _SEPARATOR.match(line):
            continue
        match = _ROW.match(line)
        if match:
            rows.append((label, _seconds_key(match.group("seconds")), int(match.group("rss"))))
        else:
            in_table = False  # table ended (blank line, diagnostics, ...)
    return rows


def convert_hub_rss(text: str, *, label: str | None = None) -> list[dict]:
    """Convert ``ci_hub_rss.sh`` output into one case per sample.

    ``label`` overrides the label parsed from the ``###`` heading.
    """
    parsed = parse_hub_rss(text)
    if not parsed:
        slug = _slug(label or DEFAULT_LABEL)
        return [
            failed(
                f"hub_rss/{slug}/*",
                {"component": "hub_rss", "label": slug},
                "no '| Seconds after launch | RSS (KiB) |' table found",
            )
        ]
    cases: list[dict] = []
    seen: set[str] = set()
    for found_label, seconds, rss_kib in parsed:
        slug = _slug(label or found_label)
        name = f"hub_rss/{slug}/{seconds}s"
        suffix = 2
        while name in seen:  # same label measured twice in one file
            name = f"hub_rss/{slug}#{suffix}/{seconds}s"
            suffix += 1
        seen.add(name)
        cases.append(
            make_case(
                suite=SUITE,
                case=name,
                dims={"component": "hub_rss", "label": slug, "seconds": int(float(seconds))},
                metrics={"rss_bytes": kib_metric(rss_kib)},
            )
        )
    return cases


def load_hub_rss(path: Path | str, *, label: str | None = None) -> list[dict]:
    """Cases from a ``ci_hub_rss.sh`` output file."""
    path = Path(path)
    try:
        text = read_text(path)
    except OSError as exc:
        slug = _slug(label or DEFAULT_LABEL)
        return [failed(f"hub_rss/{slug}/*", {"component": "hub_rss", "label": slug}, f"cannot read {path.name}: {exc}")]
    return convert_hub_rss(text, label=label)
