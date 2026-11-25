"""Static + node-driven tests for the U7 web UI speakers block (index.html)."""

from __future__ import annotations

import json
import re
import shutil
import subprocess
from html.parser import HTMLParser
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from localflow import server

HTML_PATH = Path(__file__).resolve().parent.parent / "localflow" / "static" / "index.html"
HTML = HTML_PATH.read_text(encoding="utf-8")
NODE = shutil.which("node")
needs_node = pytest.mark.skipif(not NODE, reason="node not installed")

START_MARK = "// ---- meeting speakers (U7) ----"
END_MARK = "// ---- end meeting speakers (U7) ----"
FALLBACK = "return s.source === 'others' ? 'Others: ' : (s.source === 'me' ? 'Me: ' : '');"
RENAME_URL = (
    "`/api/meetings/${encodeURIComponent(spkMeetingId)}"
    "/speakers/${encodeURIComponent(sp.id)}`"
)
MERGE_URL = "`/api/meetings/${encodeURIComponent(spkMeetingId)}/speakers/merge`"


def speakers_js() -> str:
    assert HTML.count(START_MARK) == 1
    assert HTML.count(END_MARK) == 1
    start = HTML.index(START_MARK)
    end = HTML.index(END_MARK) + len(END_MARK)
    return HTML[start:end]


def source_label_js() -> str:
    m = re.search(r"function sourceLabel\(s\) \{.*?\n  \}", HTML, re.DOTALL)
    assert m, "sourceLabel not found"
    return m.group(0)


def run_node(script: str, tmp_path: Path) -> str:
    path = tmp_path / "harness.js"
    path.write_text(script, encoding="utf-8")
    proc = subprocess.run(
        [NODE, str(path)],
        check=False, capture_output=True, text=True, encoding="utf-8",
        timeout=30, cwd=tmp_path,
    )
    assert proc.returncode == 0, proc.stderr
    return proc.stdout


# --- sourceLabel ---------------------------------------------------------------

SOURCE_CASES = [
    ({"speaker": "Alice", "source": "me"}, "Alice: "),
    ({"speaker": "", "source": "me"}, "Me: "),
    ({"speaker": "   ", "source": "others"}, "Others: "),
    ({"speaker": None, "source": "others"}, "Others: "),
    ({"speaker": 42, "source": "me"}, "Me: "),
    ({"source": "me"}, "Me: "),
    ({"source": "others"}, "Others: "),
    ({}, ""),
    ({"source": "system"}, ""),
]


@needs_node
@pytest.mark.parametrize(("segment", "expected"), SOURCE_CASES)
def test_source_label(segment, expected, tmp_path):
    script = (
        source_label_js()
        + f"\nprocess.stdout.write(JSON.stringify(sourceLabel({json.dumps(segment)})));\n"
    )
    assert json.loads(run_node(script, tmp_path)) == expected


def test_source_label_fallback_kept():
    body = source_label_js()
    assert FALLBACK in body
    assert "typeof s.speaker === 'string'" in body
    assert "return s.speaker + ': ';" in body


# --- markup + CSS --------------------------------------------------------------


def test_speakers_hidden_by_default():
    assert re.search(r"#meetingSpeakers\s*\{[^}]*display:\s*none", HTML)
    tag = re.search(r"<div id=\"meetingSpeakers\"[^>]*>", HTML)
    assert tag and "style=" not in tag.group(0)
    block = speakers_js()
    shows = [m.start() for m in re.finditer(r"meetingSpeakers\.style\.display = 'block'", HTML)]
    assert shows, "the block is never shown"
    start = HTML.index(START_MARK)
    end = HTML.index(END_MARK)
    assert all(start < i < end for i in shows)
    # shown only after a non-empty filtered list
    sync = block[block.index("function syncSpeakers"):]
    sync = sync[: sync.index("\n  }\n")]
    assert "if (!list.length)" in sync
    assert sync.index("if (!list.length)") < sync.index("style.display = 'block'")


def test_primary_button_matches_card_surface():
    rule = re.search(r"#meetingSpeakers \.dismissBtn\.primary\s*\{([^}]*)\}", HTML)
    assert rule
    assert "color: var(--paper)" in rule.group(1)
    assert "var(--bg)" not in rule.group(1)


def test_speakers_markup_order():
    for ident in ("meetingSaved", "meetingSpeakers", "meetingChat"):
        assert HTML.count(f'id="{ident}"') == 1, ident
    assert (
        HTML.index('id="meetingSaved"')
        < HTML.index('id="meetingSpeakers"')
        < HTML.index('id="meetingChat"')
    )


def test_speakers_block_is_safe():
    block = speakers_js()
    for bad in ("innerHTML", "outerHTML", "insertAdjacentHTML", "document.write"):
        assert bad not in block, bad
    assert "createElement" in block
    assert "textContent" in block


def test_speakers_endpoints():
    block = speakers_js()
    assert RENAME_URL in block
    assert MERGE_URL in block
    assert "method: 'PUT'" in block
    assert "method: 'POST'" in block
    assert "JSON.stringify({ name" in block
    assert "from:" in block and "into:" in block
    assert "meetingFetch(" in block
    assert not re.search(r"(?<![\w.])fetch\(", block)
    assert "authFetch(" not in block


def test_start_hides_speakers():
    m = re.search(
        r"meetingSaved\.style\.display = 'none';\s*"
        r"meetingChat\.style\.display = 'none';\s*"
        r"resetSpeakers\(\);",
        HTML,
    )
    assert m


def test_poll_resets_speakers_when_live():
    poll = HTML[HTML.index("async function pollMeeting()"):]
    poll = poll[: poll.index("\n  }\n")]
    live_branch = poll[poll.index("if (live) {"):]
    assert "if (meetingSpeakers.style.display === 'block') resetSpeakers();" in live_branch, (
        "pollMeeting must reset the speakers block while a meeting is live"
    )


def test_sync_checks_freshness_before_empty_list():
    block = speakers_js()
    sync = block[block.index("function syncSpeakers"):]
    sync = sync[: sync.index("\n  }\n")]
    assert sync.index("epoch !== spkEpoch") < sync.index("if (!list.length)")
    assert sync.index("if (spkBusy) return;") < sync.index("if (!list.length)")


def test_show_saved_wires_speakers():
    assert "showSaved(st.last_saved, epoch)" in HTML
    assert "syncSpeakers(saved, epoch);" in HTML
    # the /meeting/stop path still calls showSaved without an epoch
    assert re.search(r"showSaved\(saved\)", HTML)


# --- behaviour in node against a fake DOM -------------------------------------

FAKE_DOM = r"""
class El {
  constructor(tag) {
    this.tagName = tag.toUpperCase();
    this.children = [];
    this._text = '';
    this.style = {};
    this.dataset = {};
    this.attrs = {};
    this.listeners = {};
    this.className = '';
    this.value = '';
    this.disabled = false;
    this.type = '';
    const self = this;
    this.classList = {
      toggle(c, on) {
        const set = new Set(self.className.split(' ').filter(Boolean));
        if (on === undefined) on = !set.has(c);
        if (on) set.add(c); else set.delete(c);
        self.className = [...set].join(' ');
      },
      contains(c) { return self.className.split(' ').includes(c); },
    };
  }
  get textContent() { return this._text + this.children.map(c => c.textContent).join(''); }
  set textContent(v) {
    for (const c of this.children) c.parent = null;
    this.children = [];
    this._text = String(v);
  }
  appendChild(c) { c.parent = this; this.children.push(c); return c; }
  contains(el) {
    for (let n = el; n; n = n.parent) if (n === this) return true;
    return false;
  }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k]; }
  addEventListener(t, fn) { (this.listeners[t] = this.listeners[t] || []).push(fn); }
  fire(t, ev) { for (const fn of this.listeners[t] || []) fn(ev || { preventDefault() {}, key: '' }); }
  focus() { document._active = this; }
  select() {}
  find(pred) {
    for (const c of this.children) {
      if (pred(c)) return c;
      const hit = c.find(pred);
      if (hit) return hit;
    }
    return null;
  }
  findAll(pred, out) {
    out = out || [];
    for (const c of this.children) { if (pred(c)) out.push(c); c.findAll(pred, out); }
    return out;
  }
}
const body = new El('body');
const byId = {};
for (const id of ['meetingSpeakers', 'chatQuestion']) byId[id] = body.appendChild(new El('div'));
byId.speakersStatus = byId.meetingSpeakers.appendChild(new El('span'));
byId.speakersList = byId.meetingSpeakers.appendChild(new El('ul'));
// Like a browser: a focused node that is removed from the tree yields focus to body.
const document = {
  body,
  _active: null,
  get activeElement() {
    const a = this._active;
    return a && body.contains(a) ? a : body;
  },
  getElementById(id) { return byId[id]; },
  createElement(tag) { return new El(tag); },
};
function fmtSeconds(s) {
  const m = Math.floor(s / 60);
  return m + 'm' + String(s % 60).padStart(2, '0') + 's';
}
const calls = [];
let nextResponse = null;
async function meetingFetch(url, init) {
  calls.push({ url, method: init.method, body: JSON.parse(init.body) });
  const r = nextResponse;
  return { ok: r.status < 400, status: r.status, statusText: 'x', json: async () => r.body };
}
"""

SCENARIO = r"""
const box = byId.meetingSpeakers, list = byId.speakersList, status = byId.speakersStatus;
const rows = () => list.children;
const btns = li => li.findAll(c => c.tagName === 'BUTTON').map(b => b.textContent);
const btn = (li, text) => li.find(c => c.tagName === 'BUTTON' && c.textContent === text);
const out = {};
const A = { id: 'S1', label: 'Alice', name: 'Alice', source: 'me', seconds: 65.2 };
const B = { id: 'S 2/x', label: 'Speaker 2', name: '', source: 'others', seconds: 30 };

(async () => {
  syncSpeakers({});
  syncSpeakers({ speakers: [] });
  syncSpeakers({ speakers: null });
  out.empty = { display: box.style.display || null, rows: rows().length };

  syncSpeakers({ meeting_id: 'm1', speakers: [A, B] });
  out.two = {
    display: box.style.display,
    rows: rows().length,
    buttons: rows().map(btns),
    name: rows()[0].children[0].textContent,
    time: rows()[0].children[1].textContent,
  };

  resetSpeakers();
  syncSpeakers({ speakers: [A, B] });
  out.readonly = rows().map(btns);
  resetSpeakers();
  syncSpeakers({ meeting_id: 'm1', speakers: [A] });
  out.single = rows().map(btns);

  // editor survives a poll with a changed payload for the same meeting
  resetSpeakers();
  syncSpeakers({ meeting_id: 'm1', speakers: [A, B] });
  btn(rows()[1], 'Rename').fire('click');
  const input = rows()[1].find(c => c.tagName === 'INPUT');
  out.renameFocused = document.activeElement === input;
  out.renamePrefill = input.value;
  input.value = 'Bob!';
  input.fire('input');
  syncSpeakers({ meeting_id: 'm1', speakers: [A, { ...B, seconds: 31 }] }, spkEpoch);
  out.inputKept = rows()[1].find(c => c.tagName === 'INPUT') === input;
  out.inputValue = rows()[1].find(c => c.tagName === 'INPUT').value;

  // successful rename: server normalises the label
  const staleEpoch = spkEpoch;
  nextResponse = { status: 200, body: { meeting_id: 'm1', speakers: [A, { ...B, label: 'Bob', name: 'Bob' }] } };
  const form = rows()[1].children[0];
  form.fire('submit', { preventDefault() {} });
  await new Promise(r => setTimeout(r, 0));
  await new Promise(r => setTimeout(r, 0));
  out.renameCall = calls[calls.length - 1];
  out.afterRename = {
    name: rows()[1].children[0].textContent,
    status: status.textContent,
    focusOnRename: document.activeElement === spkButtons['S 2/x'].rename,
  };

  // stale poll carrying old labels is ignored
  syncSpeakers({ meeting_id: 'm1', speakers: [A, B] }, staleEpoch);
  out.afterStale = rows()[1].children[0].textContent;
  // a stale poll with *empty* speakers must not clear the panel either
  syncSpeakers({ meeting_id: 'm1', speakers: [] }, staleEpoch);
  out.afterStaleEmpty = { display: box.style.display, rows: rows().length };

  // a re-render from a changed poll keeps focus on the same row button
  spkButtons['S1'].merge.focus();
  const oldBtn = spkButtons['S1'].merge;
  syncSpeakers({ meeting_id: 'm1', speakers: [{ ...A, seconds: 70 }, { ...B, label: 'Bob', name: 'Bob' }] });
  out.syncFocus = {
    rerendered: spkButtons['S1'].merge !== oldBtn,
    focused: document.activeElement === spkButtons['S1'].merge,
  };

  // a failed edit doesn't pull focus back if the user moved to chat meanwhile
  btn(rows()[1], 'Rename').fire('click');
  rows()[1].find(c => c.tagName === 'INPUT').value = 'Carol';
  nextResponse = { status: 400, body: { detail: 'invalid speaker name' } };
  rows()[1].children[0].fire('submit', { preventDefault() {} });
  byId.chatQuestion.focus();   // while the request is on the wire
  await new Promise(r => setTimeout(r, 0));
  await new Promise(r => setTimeout(r, 0));
  out.noSteal = {
    chatFocused: document.activeElement === byId.chatQuestion,
    open: !!rows()[1].find(c => c.tagName === 'INPUT'),
    status: status.textContent,
  };
  rows()[1].children[0].fire('keydown', { key: 'Escape', preventDefault() {} });

  // 409 keeps the editor open with an error status
  btn(rows()[0], 'Merge').fire('click');
  const select = rows()[0].find(c => c.tagName === 'SELECT');
  out.mergeOptions = select.children.map(o => [o.value, o.textContent]);
  select.value = 'S 2/x';
  nextResponse = { status: 409, body: { detail: 'transcript changed' } };
  rows()[0].children[0].fire('submit', { preventDefault() {} });
  await new Promise(r => setTimeout(r, 0));
  await new Promise(r => setTimeout(r, 0));
  out.mergeCall = calls[calls.length - 1];
  out.mergeErr = {
    status: status.textContent,
    err: status.classList.contains('err'),
    open: !!rows()[0].find(c => c.tagName === 'SELECT'),
    disabled: rows()[0].find(c => c.tagName === 'SELECT').disabled,
  };

  // Escape closes the editor and returns focus to the Merge button
  rows()[0].children[0].fire('keydown', { key: 'Escape', preventDefault() {} });
  out.afterEscape = {
    open: !!rows()[0].find(c => c.tagName === 'SELECT'),
    focus: document.activeElement && document.activeElement.textContent,
  };

  // successful merge: S1 goes away, focus lands on the survivor's Rename button
  btn(rows()[0], 'Merge').fire('click');
  rows()[0].find(c => c.tagName === 'SELECT').value = 'S 2/x';
  nextResponse = { status: 200, body: { meeting_id: 'm1', speakers: [{ ...B, label: 'Bob', seconds: 100 }] } };
  rows()[0].children[0].fire('submit', { preventDefault() {} });
  await new Promise(r => setTimeout(r, 0));
  await new Promise(r => setTimeout(r, 0));
  out.afterMerge = {
    rows: rows().map(li => li.dataset.id),
    status: status.textContent,
    focusOnSurvivor: document.activeElement === spkButtons['S 2/x'].rename,
  };

  // empty rename is rejected without a request
  const before = calls.length;
  btn(rows()[0], 'Rename').fire('click');
  rows()[0].find(c => c.tagName === 'INPUT').value = '   ';
  rows()[0].children[0].fire('submit', { preventDefault() {} });
  out.emptyRename = { calls: calls.length - before, status: status.textContent };

  // duplicate labels: merge targets get talk time + id, unique ones stay clean
  resetSpeakers();
  syncSpeakers({ meeting_id: 'm2', speakers: [
    A,
    { id: 'S3', label: 'Alice', name: 'Alice', source: 'others', seconds: 12 },
    { id: 'S4', label: 'Dan', name: 'Dan', source: 'others', seconds: 5 },
  ] });
  btn(rows()[2], 'Merge').fire('click');
  out.dupOptions = rows()[2].find(c => c.tagName === 'SELECT').children.map(o => [o.value, o.textContent]);
  rows()[2].children[0].fire('keydown', { key: 'Escape', preventDefault() {} });
  btn(rows()[0], 'Merge').fire('click');
  out.dupOptionsFromAlice = rows()[0].find(c => c.tagName === 'SELECT').children.map(o => o.textContent);

  resetSpeakers();
  out.reset = { display: box.style.display, rows: rows().length };
  process.stdout.write(JSON.stringify(out));
})().catch(e => { console.error(e); process.exit(1); });
"""


@needs_node
def test_speakers_render_behaviour(tmp_path):
    out = json.loads(run_node(FAKE_DOM + speakers_js() + SCENARIO, tmp_path))
    assert out["empty"] == {"display": None, "rows": 0}
    assert out["two"] == {
        "display": "block",
        "rows": 2,
        "buttons": [["Rename", "Merge"], ["Rename", "Merge"]],
        "name": "Alice",
        "time": "1m05s",
    }
    assert out["readonly"] == [[], []]
    assert out["single"] == [["Rename"]]
    assert out["renameFocused"] is True
    assert out["renamePrefill"] == "Speaker 2"
    assert out["inputKept"] is True
    assert out["inputValue"] == "Bob!"
    assert out["renameCall"] == {
        "url": "/api/meetings/m1/speakers/S%202%2Fx",
        "method": "PUT",
        "body": {"name": "Bob!"},
    }
    assert out["afterRename"] == {"name": "Bob", "status": "Renamed", "focusOnRename": True}
    assert out["afterStale"] == "Bob"
    assert out["afterStaleEmpty"] == {"display": "block", "rows": 2}
    assert out["syncFocus"] == {"rerendered": True, "focused": True}
    assert out["noSteal"] == {
        "chatFocused": True,
        "open": True,
        "status": "Not saved — invalid speaker name",
    }
    assert out["mergeOptions"] == [["S 2/x", "Bob"]]
    assert out["mergeCall"] == {
        "url": "/api/meetings/m1/speakers/merge",
        "method": "POST",
        "body": {"from": "S1", "into": "S 2/x"},
    }
    assert "transcript changed" in out["mergeErr"]["status"]
    assert out["mergeErr"]["err"] is True
    assert out["mergeErr"]["open"] is True
    assert out["mergeErr"]["disabled"] is False
    assert out["afterEscape"] == {"open": False, "focus": "Merge"}
    assert out["afterMerge"] == {
        "rows": ["S 2/x"],
        "status": "Merged",
        "focusOnSurvivor": True,
    }
    assert out["emptyRename"] == {"calls": 0, "status": "Enter a name"}
    assert out["dupOptions"] == [
        ["S1", "Alice (1m05s · S1)"],
        ["S3", "Alice (0m12s · S3)"],
    ]
    assert out["dupOptionsFromAlice"] == ["Alice (0m12s · S3)", "Dan"]
    assert out["reset"] == {"display": "none", "rows": 0}


# --- served page ---------------------------------------------------------------


class _Balance(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.stack: list[str] = []
        self.errors: list[str] = []
        self.seen: set[str] = set()

    def handle_starttag(self, tag, attrs):
        ident = dict(attrs).get("id")
        if ident:
            self.seen.add(ident)
        if tag in ("div", "ul"):
            self.stack.append(tag)

    def handle_endtag(self, tag):
        if tag in ("div", "ul"):
            if not self.stack or self.stack[-1] != tag:
                self.errors.append(tag)
            else:
                self.stack.pop()


def test_root_serves_speakers_element(monkeypatch):
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1)
    ) as client:
        resp = client.get("/")
    assert resp.status_code == 200
    assert resp.headers["content-type"].startswith("text/html")
    body = resp.text
    assert body.lstrip().lower().startswith("<!doctype html>")
    assert 'id="meetingSpeakers"' in body
    start = body.index('id="meetingSpeakers"')
    snippet = "<div " + body[start: body.index("</ul>", start) + len("</ul>")] + "</div>"
    parser = _Balance()
    parser.feed(snippet)
    parser.close()
    assert not parser.errors and not parser.stack
    assert {"meetingSpeakers", "speakersStatus", "speakersList"} <= parser.seen
