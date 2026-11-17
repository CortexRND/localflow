"""Profile the unchanged Python Hub-only UI, with no speech engine or runtime."""
import csv
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import objc
from AppKit import NSApplication
from Foundation import NSDate, NSRunLoop

from localflow import hub_mac, mac_style

# Same launch compatibility shim as the existing Python RSS job.
hub_mac.super = mac_style.super = objc.super
output = Path(sys.argv[1]).resolve()
output.mkdir(parents=True, exist_ok=True)
app = NSApplication.sharedApplication()
app.finishLaunching()
controller = hub_mac.build_window(initial_page="Home")
controller.window.makeKeyAndOrderFront_(None)
app.activateIgnoringOtherApps_(True)
pages = ["Ask", "Hot Phrases", "Dictionary", "Prompts", "Meetings", "History", "Style", "Models", "Settings"]
stages = [("Home", "Home")]
for page in pages:
    stages += [(page, page), (f"Home_after_{page}", "Home")]
stages += [("Closed", None), ("Home_reopened", "Home")]
rows = []
for name, page in stages:
    with objc.autorelease_pool():
        if page is None:
            controller.window.performClose_(None)
        elif name == "Home_reopened":
            controller = hub_mac.build_window(initial_page="Home")
            controller.window.makeKeyAndOrderFront_(None)
        else:
            controller.show_page(page)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            NSRunLoop.currentRunLoop().runUntilDate_(NSDate.dateWithTimeIntervalSinceNow_(0.1))
            app.updateWindows()
    values = json.loads(subprocess.check_output([os.environ["LF_MEMORY_PROBE"], str(os.getpid())]))
    row = (name, values["rss"] // 1024, values["footprint"] // 1024)
    assert row[1] > 0 and row[2] > 0
    rows.append(row)
    for tool, args in [("vmmap", ["--summary"]), ("heap", ["-s"])]:
        result = subprocess.run([tool, *args, str(os.getpid())], capture_output=True)
        (output / f'{name.replace(" ", "_")}.{tool}.txt').write_bytes(result.stdout + result.stderr)
    print(row, flush=True)
with (output / "pages.csv").open("w") as destination:
    csv.writer(destination).writerows(rows)
table = "| Python Hub-only state | RSS KiB | Footprint KiB |\n| --- | ---: | ---: |\n"
table += "".join(f"| {name} | {rss} | {footprint} |\n" for name, rss, footprint in rows)
(output / "pages.md").write_text(table)
print(table)
if os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as summary:
        summary.write(table)
