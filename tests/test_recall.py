import json
import os
from datetime import datetime, timedelta

import pytest
from click.testing import CliRunner
from fastapi.testclient import TestClient

from localflow import cli as cli_module
from localflow import recall as recall_mod
from localflow import server
from localflow.config import Config
from localflow.history import HistoryStore
from localflow.meetings import MeetingSummarizer, ObsidianWriter, Segment
from localflow.providers.base import ProviderError
from localflow.recall import (
    NO_ANSWER,
    Passage,
    chunk_lines,
    clean_citations,
    parse_file_stamp,
    parse_since,
    rank,
    recall,
    search,
    stem,
    tokenize,
    transcript_passages,
)

NOW = datetime(2026, 11, 15, 12, 0)


class FakeLLM:
    id = "fake"
    offline = True

    def __init__(self, reply="The budget is $5k [1]."):
        self.reply = reply
        self.prompts: list[str] = []

    def complete(self, system, user, *, max_tokens=None):
        self.prompts.append(user)
        if isinstance(self.reply, Exception):
            raise self.reply
        return self.reply


def _p(text, days_ago=1.0, source="dictation", ref="1"):
    return Passage(
        text=text, source=source, title="", ref=ref,
        ts=(NOW - timedelta(days=days_ago)).timestamp(),
    )


def _vault(tmp_path):
    vault = tmp_path / "vault"
    writer = ObsidianWriter(str(vault), "notes", "logs")
    saved = writer.write(
        title="Launch sync", category="Zoom", started_at=datetime(2026, 11, 14, 10, 0),
        duration_seconds=600, notes_md="## Summary\nWe agreed to drop the Windows beta.",
        segments=[
            Segment(0, "we will drop the Windows beta next week", "me"),
            Segment(75, "agreed, Dana owns the announcement", "others"),
        ],
    )
    return vault, saved


@pytest.fixture
def store(tmp_path):
    s = HistoryStore(tmp_path / "history.sqlite", tmp_path / "clips")
    s.add(kind="dictate", final_text="Sarah the budget is five thousand dollars",
          app_name="Slack", ts=NOW.timestamp() - 3600)
    s.add(kind="dictate", final_text="remember to buy milk and eggs",
          app_name="Notes", ts=NOW.timestamp() - 7200)
    s.add(kind="dictate", final_text="failed one about budget", status="failed",
          ts=NOW.timestamp() - 100)
    s.add(kind="dictate", final_text="", status="ok", ts=NOW.timestamp() - 50)
    return s


# ------------------------------------------------------------- retrieval ---

def test_bm25_ranks_the_right_passage_first():
    passages = [
        _p("lunch plans for friday with the team", ref="a"),
        _p("the quarterly budget review is moved to thursday", ref="b"),
        _p("budget budget budget but also a lot of other unrelated filler words here "
           "that dilute the passage considerably so it should rank lower overall", ref="c"),
    ]
    top = rank("when is the budget review", passages, now=NOW)
    assert top[0].ref == "b"
    assert "a" not in [p.ref for p in top]


def test_stemming_matches_plurals_and_inflections():
    assert stem("budgets") == stem("budget") == "budget"
    assert stem("dropping") == stem("dropped") == stem("drop")
    assert stem("policies") == "policy"
    assert stem("business") == "business"
    top = rank("budget", [_p("we reviewed the budgets today")], now=NOW)
    assert len(top) == 1


def test_stopwords_are_ignored():
    assert tokenize("What did we say about the budget?") == ["budget"]
    passages = [_p("the and of to a is it"), _p("budget notes")]
    assert [p.text for p in rank("what is the", passages, now=NOW)] == []
    assert [p.text for p in rank("what is the budget", passages, now=NOW)] == ["budget notes"]


def test_quoted_phrase_scores_higher():
    scattered = _p("review the plan and then the budget later", ref="scattered")
    exact = _p("the budget review is on thursday", ref="exact")
    # Without the phrase the scattered passage (shorter match density) can win on its own;
    # with quotes the verbatim phrase must come first.
    top = rank('"budget review"', [scattered, exact], now=NOW)
    assert top[0].ref == "exact"
    assert {p.ref for p in top} == {"exact", "scattered"}


def test_recency_breaks_ties():
    old = _p("budget approved", days_ago=90, ref="old")
    new = _p("budget approved", days_ago=1, ref="new")
    assert rank("budget", [old, new], now=NOW)[0].ref == "new"


def test_since_filter_and_parsing():
    assert parse_since("today", NOW) == datetime(2026, 11, 15)
    assert parse_since("yesterday", NOW) == datetime(2026, 11, 14)
    assert parse_since("7d", NOW) == NOW - timedelta(days=7)
    assert parse_since("2026-11-01", NOW) == datetime(2026, 11, 1)
    assert parse_since(None) is None
    with pytest.raises(ValueError):
        parse_since("last week")
    passages = [_p("budget old", days_ago=40, ref="old"), _p("budget new", days_ago=2, ref="new")]
    assert [p.ref for p in rank("budget", passages, since="30d", now=NOW)] == ["new"]
    assert [p.ref for p in rank("budget", passages, now=NOW)] == ["new", "old"]


# ---------------------------------------------------- transcripts / dates ---

def test_filename_date_parsing(tmp_path):
    when, title = parse_file_stamp(tmp_path / "2026-11-14 1030 Launch sync (transcript).md")
    assert when == datetime(2026, 11, 14, 10, 30)
    assert title == "Launch sync"
    odd = tmp_path / "random notes.md"
    odd.write_text("x")
    os.utime(odd, (1_700_000_000, 1_700_000_000))
    when, title = parse_file_stamp(odd)
    assert when == datetime.fromtimestamp(1_700_000_000)
    assert title == "random notes"


def test_chunking_keeps_speaker_prefix_with_text():
    lines = [f"**{i:02d}:00** **Me:** " + " ".join(f"w{i}x{j}" for j in range(40))
             for i in range(10)]
    chunks = chunk_lines("\n\n".join(lines))
    assert len(chunks) > 1
    assert all(len(c.split()) <= 200 for c in chunks)
    for chunk in chunks:
        for line in chunk.splitlines():
            assert line.startswith("**") and "**Me:**" in line
    assert "\n".join(chunks).count("**Me:**") == 10


def test_long_single_line_is_split():
    chunks = chunk_lines("**00:00** **Me:** " + " ".join(["word"] * 450))
    assert len(chunks) == 3
    assert chunks[0].startswith("**00:00**")


def test_transcript_passages_have_ref_and_time(tmp_path):
    _, saved = _vault(tmp_path)
    passages = transcript_passages(saved.log_path)
    assert passages and passages[0].source == "meeting"
    assert passages[0].title == "Launch sync"
    assert passages[0].ref == f"{saved.log_path}#00:00"
    assert "Windows beta" in passages[0].text
    assert "Notes: [[" not in passages[0].text
    assert datetime.fromtimestamp(passages[0].ts) == datetime(2026, 11, 14, 10, 0)


# ------------------------------------------------------------ recall() ---

def test_search_covers_dictations_meetings_and_notes(tmp_path, store):
    vault, saved = _vault(tmp_path)
    result = search("windows beta", store=store, vault_path=vault,
                    notes_folder="notes", logs_folder="logs", now=NOW)
    assert result.answer is None
    assert {c["source"] for c in result.citations} == {"meeting", "notes"}
    meeting = next(c for c in result.citations if c["source"] == "meeting")
    assert meeting["ref"].startswith(str(saved.log_path))
    assert meeting["when"] == "2026-11-14T10:00"

    result = search("budget", store=store, now=NOW)
    assert [c["source"] for c in result.citations] == ["dictation"]  # failed/empty skipped
    assert result.citations[0]["app"] == "Slack"
    assert result.citations[0]["ref"] == "1"
    assert result.citations[0]["n"] == 1


def test_llm_answer_keeps_valid_citations_and_strips_invalid(store):
    llm = FakeLLM("The budget is five thousand [1]. Also [7] and [1, 9].")
    result = recall("what is the budget", llm=llm, store=store, now=NOW)
    assert result.answer == "The budget is five thousand [1]. Also and [1]."
    assert [c["n"] for c in result.citations] == [1]
    assert "[1]" in llm.prompts[0] and "ONLY" in llm.prompts[0]
    assert "Slack" in llm.prompts[0] and NO_ANSWER in llm.prompts[0]


def test_clean_citations_keeps_timestamps():
    assert clean_citations("said at [12:30] [3]", 2) == "said at [12:30]"


def test_sentinel_when_nothing_retrieved_and_llm_not_called(store):
    llm = FakeLLM()
    result = recall("zebra migration", llm=llm, store=store, now=NOW)
    assert result.answer == NO_ANSWER and result.citations == []
    assert llm.prompts == []


def test_sentinel_from_llm_drops_citations(store):
    result = recall("budget", llm=FakeLLM(NO_ANSWER), store=store, now=NOW)
    assert result.answer == NO_ANSWER and result.citations == []


@pytest.mark.parametrize("error", [ProviderError("down"), ConnectionError("refused")])
def test_llm_failure_falls_back_to_passages(store, error):
    result = recall("budget", llm=FakeLLM(error), store=store, now=NOW)
    assert result.answer is None
    assert len(result.citations) == 1


def test_history_disabled_skips_dictations(tmp_path):
    disabled = HistoryStore(tmp_path / "off.sqlite", enabled=False)
    assert search("budget", store=disabled, now=NOW).citations == []
    assert search("budget", store=None, now=NOW).citations == []


# ------------------------------------------------------------------ CLI ---

@pytest.fixture
def cli_env(tmp_path, store, monkeypatch):
    vault, _ = _vault(tmp_path)
    config = Config(vault_path=str(vault), notes_folder="notes", logs_folder="logs")
    monkeypatch.setattr(cli_module, "load_config", lambda: config)
    monkeypatch.setattr(cli_module, "_history_store", lambda: store)
    llm = FakeLLM("Budget is five thousand [1].")
    monkeypatch.setattr(cli_module, "build_llm", lambda cfg: llm)
    return config, llm


def test_cli_no_llm_lists_ranked_passages(cli_env):
    _, llm = cli_env
    result = CliRunner().invoke(cli_module.cli, ["recall", "budget", "--no-llm"])
    assert result.exit_code == 0, result.output
    assert "Sources" in result.output
    assert "[1]" in result.output and "dictation · Slack" in result.output
    assert llm.prompts == []


def test_cli_answer_and_json(cli_env):
    result = CliRunner().invoke(cli_module.cli, ["recall", "budget"])
    assert result.exit_code == 0, result.output
    assert "Budget is five thousand [1]." in result.output
    assert "Sources" in result.output

    as_json = CliRunner().invoke(cli_module.cli, ["recall", "budget", "--json", "--k", "3"])
    data = json.loads(as_json.output)
    assert data["answer"] == "Budget is five thousand [1]."
    assert data["citations"][0]["source"] == "dictation"

    raw = CliRunner().invoke(cli_module.cli, ["recall", "budget", "--no-llm", "--json"])
    assert json.loads(raw.output)["answer"] is None


def test_cli_since_validation_and_history_disabled(cli_env, monkeypatch):
    config, _ = cli_env
    bad = CliRunner().invoke(cli_module.cli, ["recall", "budget", "--since", "soon"])
    assert bad.exit_code != 0
    config.history_enabled = False
    result = CliRunner().invoke(cli_module.cli, ["recall", "budget", "--no-llm", "--json"])
    assert json.loads(result.output)["citations"] == []


# ------------------------------------------------------------------ API ---

@pytest.fixture
def api(tmp_path, store, monkeypatch):
    vault, _ = _vault(tmp_path)
    config = Config(
        meetings_enabled=False, meeting_watch=False, sounds_enabled=False,
        work_prompts=False, vault_path=str(vault), notes_folder="notes", logs_folder="logs",
    )
    llm = FakeLLM("Budget is five thousand [1].")
    monkeypatch.setattr(server, "_config", config)
    monkeypatch.setattr(server, "_history", store)
    monkeypatch.setattr(server, "_summarizer", MeetingSummarizer(llm))
    monkeypatch.setattr(server, "_api_token", "test-token")
    monkeypatch.setattr(server, "load_or_create", lambda: "test-token")
    headers = {"X-Localflow-Token": "test-token"}
    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as c:
        c.llm = llm
        c.headers.update(headers)
        yield c


def test_api_recall_answers_with_citations(api):
    resp = api.post("/api/recall", json={"question": "what is the budget?"})
    assert resp.status_code == 200
    body = resp.json()
    assert body["answer"] == "Budget is five thousand [1]."
    assert body["citations"][0]["app"] == "Slack"


def test_api_recall_llm_false_is_retrieval_only(api):
    body = api.post("/api/recall", json={"question": "budget", "llm": False}).json()
    assert body["answer"] is None and body["citations"]
    assert api.llm.prompts == []


def test_api_recall_validates_input(api):
    post = api.post
    assert post("/api/recall", json={"question": "  "}).status_code == 400
    assert post("/api/recall", json={"question": "x" * 1001}).status_code == 413
    assert post("/api/recall", json={"question": "q", "k": 0}).status_code == 400
    assert post("/api/recall", json={"question": "q", "k": 21}).status_code == 400
    assert post("/api/recall", json={"question": "q", "since": "soon"}).status_code == 400
    assert post("/api/recall", json={"question": "budget", "k": 20, "since": "7d"}).status_code == 200


def test_api_recall_requires_token_even_on_loopback(api):
    payload = {"question": "budget"}
    with TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    ) as anon:
        assert anon.post("/api/recall", json=payload).status_code == 401
        assert anon.post(
            "/api/recall", json=payload, headers={"X-Localflow-Token": "wrong"}
        ).status_code == 401
    with TestClient(
        server.app, base_url="http://10.0.0.5:8756", client=("10.0.0.5", 1234)
    ) as remote:
        assert remote.post("/api/recall", json=payload).status_code == 403
        assert remote.post(
            "/api/recall", json=payload, headers={"X-Localflow-Token": "test-token"}
        ).status_code == 200


def test_tokenize_non_latin_scripts():
    assert recall_mod.tokenize("Привет мир") == recall_mod.tokenize("привет мир")
    assert recall_mod.tokenize("Привет мир")


def test_long_dictation_is_chunked_so_late_facts_reach_prompt():
    filler = " ".join(f"filler{i}" for i in range(400))
    text = filler + " the gate code is 4417"
    chunks = recall_mod.chunk_lines(text)
    assert len(chunks) > 1
    assert any("4417" in c and len(c.split()) <= recall_mod.PROMPT_PASSAGE_MAX_WORDS for c in chunks)


def test_transcript_with_bad_encoding_is_skipped(tmp_path):
    bad = tmp_path / "2026-11-12 1400 Sync (transcript).md"
    bad.write_bytes(b"**00:01** **Me:** caf\xe9 \xff\xfe")
    assert recall_mod.transcript_passages(bad) == []


# ------------------------------------------------------- speaker citations ---

def _without_speaker(citations):
    return [{k: v for k, v in c.items() if k != "speaker"} for c in citations]


def _diarized_vault(tmp_path):
    # Mic-only segments carrying speaker ids: lines are labelled only because the
    # meeting is diarized (the writer's is_diarized branch, not channel attribution).
    vault = tmp_path / "dvault"
    ObsidianWriter(str(vault), "notes", "logs").write(
        title="Pricing review", category="Zoom", started_at=datetime(2026, 11, 13, 9, 0),
        duration_seconds=600, notes_md="## Summary\nPricing stays.",
        segments=[
            Segment(0, "welcome everyone to the call", "me",
                    speaker_id="spk0", speaker_label="Speaker 2"),
            Segment(20, "the launch pricing stays at ten dollars", "me",
                    speaker_id="spk1", speaker_label="Dana O'Neil"),
        ],
    )
    return vault


@pytest.mark.parametrize("text, speaker", [
    ("**00:00** **Me:** we drop the beta", "Me"),
    ("**01:15** **Others:** agreed", "Others"),
    ("**02:30** **Speaker 2:** sounds good", "Speaker 2"),
    ("**03:45** **Dana O'Neil:** I own it", "Dana O'Neil"),
    ("**04:00** **José:** listo", "José"),
    ("**[05:10]** **Speaker 3:** bracketed stamp", "Speaker 3"),
    ("**1:02:03**\t**Me:** hour stamp, tab separator", "Me"),
    ("**00:00** **Me:** hi\n**00:10** **Dana:** the budget", "Me"),  # first line wins
    ("**00:00** budget\n**00:05** **Dana:** budget", None),          # later line only
    ("**00:00** we drop the beta", None),                             # unlabelled log
    ("**00:00**\n**Me:** hello", None),                               # never crosses a newline
    ("**00:00** said **Me:** hello", None),                           # not right after stamp
    ("**Me:** no stamp at all", None),
    ("continuation of an over-long line **Me:** text", None),
    ("**00:00** **Dana  Smith:** needs rewriting", None),             # not a clean name
    ("**00:00** **!!!:** junk", None),
])
def test_meeting_citation_speaker_from_first_line(text, speaker):
    passage = _p(text, source="meeting", ref="m.md#00:00")
    [off] = recall_mod._citations([passage], "budget")
    [on] = recall_mod._citations([passage], "budget", cite_speakers=True)
    assert "speaker" not in off
    assert on.get("speaker") == speaker and ("speaker" in on) == (speaker is not None)
    assert _without_speaker([on]) == [off]


@pytest.mark.parametrize("source", ["dictation", "notes"])
def test_non_meeting_citations_never_get_speaker(source):
    passage = _p("**00:00** **Me:** budget", source=source)
    on = recall_mod._citations([passage], "budget", cite_speakers=True)
    assert on == recall_mod._citations([passage], "budget")


def test_search_and_recall_speaker_on_vs_off(tmp_path, store):
    vault = _diarized_vault(tmp_path)
    kwargs = {"store": store, "vault_path": vault, "notes_folder": "notes",
              "logs_folder": "logs", "now": NOW}
    off = search("launch pricing", **kwargs).to_dict()
    on = search("launch pricing", cite_speakers=True, **kwargs).to_dict()
    assert off["citations"] and all("speaker" not in c for c in off["citations"])
    meeting = next(c for c in on["citations"] if c["source"] == "meeting")
    # The chunk starts with Speaker 2 even though the match is on Dana's line.
    assert meeting["speaker"] == "Speaker 2"
    assert all("speaker" not in c for c in on["citations"] if c["source"] != "meeting")
    assert _without_speaker(on["citations"]) == off["citations"]
    assert on["answer"] == off["answer"]

    llm_off, llm_on = FakeLLM("Ten dollars [1]."), FakeLLM("Ten dollars [1].")
    r_off = recall("launch pricing", llm=llm_off, **kwargs).to_dict()
    r_on = recall("launch pricing", llm=llm_on, cite_speakers=True, **kwargs).to_dict()
    assert llm_on.prompts == llm_off.prompts
    assert r_on["answer"] == r_off["answer"]
    assert _without_speaker(r_on["citations"]) == r_off["citations"]
    assert all("speaker" not in c for c in r_off["citations"])
    assert any(c.get("speaker") == "Speaker 2" for c in r_on["citations"])


def _assert_speakers(body, *, use_llm, enabled):
    # _vault's channel-attributed meeting starts with "**00:00** **Me:**".
    assert body["answer"] == ("Budget is five thousand [1]." if use_llm else None)
    assert {c["source"] for c in body["citations"]} == {"meeting", "notes"}
    for c in body["citations"]:
        expected = "Me" if enabled and c["source"] == "meeting" else None
        assert c.get("speaker") == expected and ("speaker" in c) == (expected is not None)


@pytest.mark.parametrize("use_llm", [True, False])
@pytest.mark.parametrize("enabled", [False, True])
def test_api_recall_speaker_follows_setting(api, monkeypatch, enabled, use_llm):
    monkeypatch.setattr(server._config, "speaker_diarization", enabled)
    resp = api.post("/api/recall", json={"question": "windows beta", "llm": use_llm})
    assert resp.status_code == 200
    _assert_speakers(resp.json(), use_llm=use_llm, enabled=enabled)


@pytest.mark.parametrize("use_llm", [True, False])
@pytest.mark.parametrize("enabled", [False, True])
def test_cli_recall_json_speaker_follows_setting(cli_env, enabled, use_llm):
    config, _ = cli_env
    config.speaker_diarization = enabled
    args = ["recall", "windows beta", "--json"] + ([] if use_llm else ["--no-llm"])
    result = CliRunner().invoke(cli_module.cli, args)
    assert result.exit_code == 0, result.output
    _assert_speakers(json.loads(result.output), use_llm=use_llm, enabled=enabled)


@pytest.mark.parametrize("flags", [[], ["--no-llm"]])
def test_cli_recall_plain_text_unchanged_by_speaker_setting(cli_env, flags):
    config, _ = cli_env
    outputs = []
    for enabled in (False, True):
        config.speaker_diarization = enabled
        result = CliRunner().invoke(cli_module.cli, ["recall", "windows beta", *flags])
        assert result.exit_code == 0, result.output
        outputs.append(result.output)
    assert outputs[0] == outputs[1]
