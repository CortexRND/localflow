"""Adapter: ``scripts/ci_hub_pages.sh`` ``pages.csv`` → footprint cases.

The Hub (``rust/lf-ui/src/mac/memory_profile.rs``, enabled by
``LF_HUB_RSS_PROFILE``) appends one ``state,rss_kib,footprint_kib`` row per
visited page (``Home``, ``<Page>``, ``Home_after_<Page>``, ``Closed``,
``Home_reopened``). The script (unchanged) checks and tabulates them.

One case per row: ``hub_pages/<state>`` (or ``hub_pages/<label>/<state>``)
with ``rss_bytes`` and ``footprint_bytes`` (KiB × 1024). A header row is
tolerated; any other unparsable row becomes a ``failed`` case.

Stdlib only.
"""

from __future__ import annotations

import csv
import io
from pathlib import Path

from ..schema import make_case
from ._common import SUITE, failed, kib_metric, read_text


def _case(state: str, label: str | None) -> str:
    return f"hub_pages/{label}/{state}" if label else f"hub_pages/{state}"


def _dims(state: str, label: str | None) -> dict:
    dims = {"component": "hub_pages", "state": state}
    if label:
        dims["label"] = label
    return dims


def _int(text: str) -> int:
    value = int(text.strip())
    if value < 0:
        raise ValueError(f"negative KiB value {value}")
    return value


def convert_hub_pages(text: str, *, label: str | None = None) -> list[dict]:
    """Convert ``pages.csv`` content into one case per sampled state."""
    cases: list[dict] = []
    seen: dict[str, int] = {}
    for lineno, row in enumerate(csv.reader(io.StringIO(text)), start=1):
        cells = [c.strip() for c in row]
        if not any(cells):
            continue
        if lineno == 1 and len(cells) >= 2 and cells[0].lower() in ("state", "name", "stage"):
            continue  # header
        state = cells[0] or f"row-{lineno}"
        if state in seen:  # keep case names unique if a state is sampled twice
            seen[state] += 1
            state = f"{state}#{seen[state]}"
        else:
            seen[state] = 1
        if len(cells) != 3:
            cases.append(failed(_case(state, label), _dims(state, label), f"line {lineno}: expected 3 columns"))
            continue
        try:
            rss, footprint = _int(cells[1]), _int(cells[2])
        except ValueError as exc:
            cases.append(failed(_case(state, label), _dims(state, label), f"line {lineno}: {exc}"))
            continue
        cases.append(
            make_case(
                suite=SUITE,
                case=_case(state, label),
                dims=_dims(state, label),
                metrics={"rss_bytes": kib_metric(rss), "footprint_bytes": kib_metric(footprint)},
            )
        )
    if not cases:
        return [failed(_case("*", label), _dims("*", label), "pages.csv has no rows")]
    return cases


def load_hub_pages(path: Path | str, *, label: str | None = None) -> list[dict]:
    """Cases from ``pages.csv`` (or a ``ci_hub_pages.sh`` output dir)."""
    path = Path(path)
    if path.is_dir():
        path = path / "pages.csv"
    try:
        text = read_text(path)
    except OSError as exc:
        return [failed(_case("*", label), _dims("*", label), f"cannot read {path.name}: {exc}")]
    return convert_hub_pages(text, label=label)
