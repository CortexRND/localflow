"""Attribute first-Ask growth without confusing shared mapping totals with RSS."""
import csv
import os
from pathlib import Path
import re
import sys


def size(value):
    match = re.fullmatch(r"(\d+(?:\.\d+)?)([BKMGT]?)", value)
    if not match:
        raise ValueError(f"Invalid memory size: {value!r}")
    return round(float(match[1]) * 1024 ** "BKMGT".index(match[2] or "B"))


def regions(text):
    result = {}
    for line in text.splitlines():
        match = re.match(r"^(.*?)\s{2,}(\d+(?:\.\d+)?[BKMGT]?)\s+(\d+(?:\.\d+)?[BKMGT]?)\s+(\d+(?:\.\d+)?[BKMGT]?)\s", line)
        if match:
            result[match[1]] = (size(match[3]), size(match[4]))
    return result


def heap_types(text):
    result = {}
    for line in text.splitlines():
        match = re.match(r"\s*(\d+)\s+(\d+)\s+[\d.]+\s+(.+)", line)
        if match:
            result[match[3].strip()] = int(match[2])
    return result


def main(root):
    samples = {row[0]: list(map(int, row[1:])) for row in csv.reader((root / "pages.csv").open())}
    before, after = samples["Home"], samples["Ask"]
    lines = ["# First Ask attribution", "", "| KiB | Home | First Ask | Delta |", "| --- | ---: | ---: | ---: |"]
    for index, metric in enumerate(["RSS", "phys_footprint"]):
        lines.append(f"| {metric} | {before[index]} | {after[index]} | {after[index]-before[index]:+d} |")
    old = regions((root / "Home.vmmap.txt").read_text())
    new = regions((root / "Ask.vmmap.txt").read_text())
    lines += ["", "| vmmap region | Resident delta KiB | Dirty delta KiB |", "| --- | ---: | ---: |"]
    for name in ["MALLOC_SMALL", "MALLOC_TINY", "VM_ALLOCATE", "Stack", "CG raster data", "IOSurface", "__DATA", "__DATA_DIRTY", "__TEXT"]:
        if name in old and name in new:
            delta = [(b-a)/1024 for a,b in zip(old[name], new[name])]
            lines.append(f"| {name} | {delta[0]:+.1f} | {delta[1]:+.1f} |")
    old_heap = heap_types((root / "Home.heap.txt").read_text())
    new_heap = heap_types((root / "Ask.heap.txt").read_text())
    lines += ["", "| Heap type | Allocated byte delta |", "| --- | ---: |"]
    for name, delta in sorted(((name, value-old_heap.get(name, 0)) for name,value in new_heap.items()), key=lambda row: -row[1])[:15]:
        lines.append(f"| {name} | {delta:+d} |")
    lines += ["", "Shared mapping resident totals are not process RSS and must not be summed as private memory.",
              "Ask's initial render reads the existing backend's turns; it does not submit recall or initialize a new HTTP transport.",
              "HttpTransport is constructed with HubServices before the first Home sample. Deferring it cannot explain this first-Ask delta.",
              "Typed heap deltas identify native cache growth; non-object allocations remain unattributed without stack logging."]
    text = "\n".join(lines)
    (root.parent / "ask-attribution.md").write_text(text)
    print(text)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
            summary.write(text)


if __name__ == "__main__":
    main(Path(sys.argv[1]))
