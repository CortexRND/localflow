import threading
from datetime import datetime

import numpy as np
import pytest

from localflow import meetings, system_audio
from localflow.meeting_templates import (
    DEFAULT_TEMPLATE_ID,
    load_templates,
    parse_template,
    resolve_template,
)
from localflow.meetings import (
    CHAT_NO_ANSWER,
    CHAT_NOT_FOUND,
    ENHANCE_INSTRUCTION,
    MeetingSession,
    MeetingSummarizer,
    ObsidianWriter,
    Segment,
    answer_question,
    build_enhance_prompt,
    build_notes_prompt,
    format_transcript,
    latest_transcript_log,
    read_transcript_log,
)


class FakeLLM:
    id = "fake"
    offline = True

    def __init__(self, replies=None, fail=False):
        self.prompts: list[str] = []
        self.replies = list(replies or [])
        self.fail = fail

    def complete(self, system, user, *, max_tokens=None):
        self.prompts.append(user)
        if self.fail:
            raise RuntimeError("llm down")
        return self.replies.pop(0) if self.replies else f"reply {len(self.prompts)}"


# ------------------------------------------------------------- templates ---

def test_builtin_templates_ship_general_standup_and_one_on_one():
    templates = load_templates()
    assert {"general", "standup", "one-on-one"} <= set(templates)
    assert templates["one-on-one"].name == "1:1"
    assert templates["general"].section_names() == [
        "Summary", "Key Points", "Decisions", "Action Items",
    ]
    assert "Blockers" in templates["standup"].section_names()


def test_parse_template_reads_frontmatter_instructions_and_guidance():
    t = parse_template("retro", (
        "---\nname: Retro\ndescription: What went well.\n---\n"
        "Be blunt.\n\n## Went Well\nBullets.\n\n## To Improve\nBullets, with owner.\n"
    ))
    assert (t.id, t.name, t.description, t.instructions) == (
        "retro", "Retro", "What went well.", "Be blunt.",
    )
    assert t.section_names() == ["Went Well", "To Improve"]
    assert "## To Improve\nBullets, with owner." in t.sections_markdown()


def test_user_templates_add_and_override_builtins(tmp_path):
    (tmp_path / "general.md").write_text("## Only This\nOne line.\n")
    (tmp_path / "design-review.md").write_text("## Feedback\nBullets.\n")
    (tmp_path / "no-sections.md").write_text("just prose, no headers\n")
    (tmp_path / "notes.txt").write_text("## Ignored\n")
    templates = load_templates(tmp_path)
    assert templates["general"].section_names() == ["Only This"]
    assert templates["design-review"].section_names() == ["Feedback"]
    assert "no-sections" not in templates and "notes" not in templates
    assert "standup" in templates


def test_resolve_template_falls_back_to_general(tmp_path):
    assert resolve_template("nope", tmp_path).id == DEFAULT_TEMPLATE_ID
    assert resolve_template(None).id == DEFAULT_TEMPLATE_ID
    assert resolve_template("standup", tmp_path / "missing").id == "standup"


# --------------------------------------------------------------- prompts ---

def test_default_notes_prompt_matches_the_old_hardcoded_sections():
    prompt = build_notes_prompt()
    for header in ("## Summary", "## Key Points", "## Decisions", "## Action Items"):
        assert header in prompt
    assert prompt.rstrip().endswith("Transcript:")
    assert ENHANCE_INSTRUCTION not in prompt


def test_notes_prompt_is_built_from_the_chosen_template():
    prompt = build_notes_prompt(resolve_template("standup"))
    assert "## Yesterday" in prompt and "## Blockers" in prompt
    assert "Keep it short" in prompt          # template instructions
    assert "## Key Points" not in prompt


def test_enhance_prompt_carries_user_notes_and_the_verbatim_instruction():
    notes = "## Pricing\n- **raise to $20?**\n- ask Dana re: churn"
    prompt = build_enhance_prompt(notes, resolve_template("one-on-one"))
    assert (
        "The user wrote these rough notes during the meeting — expand and enhance "
        "them using the transcript, keeping the user's structure and emphasis."
    ) in prompt
    assert notes in prompt
    assert "## Commitments" in prompt
    assert prompt.rstrip().endswith("Transcript:")


# ------------------------------------------------------------ summarizer ---

def test_summarize_without_notes_uses_template_prompt():
    llm = FakeLLM(["## Summary\nok"])
    out = MeetingSummarizer(llm).summarize("we shipped it", template=resolve_template("standup"))
    assert out == "## Summary\nok"
    assert "## Yesterday" in llm.prompts[0]
    assert llm.prompts[0].endswith("we shipped it")
    assert ENHANCE_INSTRUCTION not in llm.prompts[0]


def test_summarize_with_notes_uses_enhance_prompt():
    llm = FakeLLM(["## Pricing\n- enhanced"])
    out = MeetingSummarizer(llm).summarize("price goes to twenty", user_notes="## Pricing\n- $20?")
    assert out == "## Pricing\n- enhanced"
    assert ENHANCE_INSTRUCTION in llm.prompts[0]
    assert "## Pricing\n- $20?" in llm.prompts[0]
    assert llm.prompts[0].endswith("price goes to twenty")


@pytest.mark.parametrize("notes", ["", "- my point"])
def test_summarize_long_transcript_chunks_then_merges(notes):
    llm = FakeLLM()
    words = " ".join(f"w{i}" for i in range(25))
    out = MeetingSummarizer(llm, words_per_chunk=10).summarize(words, user_notes=notes)
    assert len(llm.prompts) == 4  # 3 chunks + merge
    assert out == "reply 4"
    merge = llm.prompts[-1]
    assert "reply 1" in merge and "reply 3" in merge
    assert ("- my point" in merge) == bool(notes)
    assert "## Summary" in merge or "Summary" in merge


def test_summarize_empty_transcript_keeps_user_notes():
    llm = FakeLLM()
    assert MeetingSummarizer(llm).summarize("", user_notes="- todo") == "- todo\n"
    assert "No speech captured" in MeetingSummarizer(llm).summarize("")
    assert llm.prompts == []


def test_summarize_failure_keeps_user_notes():
    out = MeetingSummarizer(FakeLLM(fail=True)).summarize("words", user_notes="- keep me")
    assert "summarization failed" in out
    assert "- keep me" in out


# --------------------------------------------------------------- segments ---

def test_segment_source_defaults_to_me_and_labels_transcript():
    assert Segment(0, "hi").source == "me"
    mic_only = [Segment(0, "a"), Segment(5, "b")]
    assert format_transcript(mic_only) == "a b"
    mixed = [Segment(0, "hello", "me"), Segment(65, "hi back", "others")]
    assert format_transcript(mixed) == "[00:00] Me: hello\n[01:05] Others: hi back"


class FakeTranscriber:
    def __init__(self):
        self.calls = 0

    def transcribe(self, audio):
        self.calls += 1
        return f"text {self.calls}"


class FakeSystemSource:
    label = "fake"

    def __init__(self, fail=False):
        self.fail = fail
        self.callback = None
        self.stopped = False

    def start(self, callback):
        if self.fail:
            raise system_audio.SystemAudioUnavailable("no permission")
        self.callback = callback

    def stop(self):
        self.stopped = True


class FakeStream:
    def __init__(self, **kwargs):
        self.callback = kwargs["callback"]

    def start(self):
        pass

    def stop(self):
        pass

    def close(self):
        pass


@pytest.fixture
def fake_mic(monkeypatch):
    monkeypatch.setattr(meetings.sd, "InputStream", FakeStream)


def _speech(seconds, rate=16000):
    return (np.ones(int(seconds * rate), dtype=np.float32) * 0.1)


def test_session_transcribes_mic_and_system_audio_with_sources(fake_mic):
    source = FakeSystemSource()
    session = MeetingSession(FakeTranscriber(), threading.Lock(),
                             chunk_seconds=3600, system_audio=source)
    session.start()
    assert session.system_audio_status == "on"
    session._stream.callback(_speech(2)[:, None], 0, None, None)
    source.callback(_speech(2))
    session._drain_once()
    session._stream.callback(_speech(2)[:, None], 0, None, None)
    segments = session.stop()
    assert source.stopped
    assert [(s.offset_seconds, s.source) for s in segments] == [
        (0.0, "me"), (0.0, "others"), (2.0, "me"),
    ]


def test_session_falls_back_to_mic_when_system_audio_fails(fake_mic):
    session = MeetingSession(FakeTranscriber(), threading.Lock(),
                             chunk_seconds=3600, system_audio=FakeSystemSource(fail=True))
    session.start()
    assert session.active
    assert session.system_audio_status == "error"
    assert "no permission" in session.system_audio_error
    session._stream.callback(_speech(2)[:, None], 0, None, None)
    assert [s.source for s in session.stop()] == ["me"]


# ----------------------------------------------------------------- writer ---

def test_writer_labels_speakers_and_keeps_raw_notes(tmp_path):
    writer = ObsidianWriter(str(tmp_path), "notes", "logs")
    saved = writer.write(
        title="Sync", category="Zoom", started_at=datetime(2026, 11, 14, 9, 0),
        duration_seconds=120, notes_md="## Summary\nok",
        segments=[Segment(0, "hello", "me"), Segment(30, "hi", "others")],
        user_notes="- ask about Q3", template="standup",
    )
    log_text = saved.log_path.read_text()
    assert "**00:00** **Me:** hello" in log_text
    assert "**00:30** **Others:** hi" in log_text
    assert "## My raw notes\n\n- ask about Q3" in log_text
    assert "template: standup" in saved.notes_path.read_text()
    body = read_transcript_log(saved.log_path)
    assert not body.startswith("---") and "Notes: [[" not in body
    assert "**Others:** hi" in body
    assert latest_transcript_log(tmp_path / "logs") == saved.log_path


def test_writer_without_system_audio_is_unchanged(tmp_path):
    writer = ObsidianWriter(str(tmp_path), "notes", "logs")
    saved = writer.write(
        title="Solo", category="Other", started_at=datetime(2026, 11, 14, 9, 0),
        duration_seconds=60, notes_md="x", segments=[Segment(0, "hello")],
    )
    text = saved.log_path.read_text()
    assert "**00:00** hello" in text
    assert "Me:" not in text and "My raw notes" not in text
    assert "template:" not in saved.notes_path.read_text()


def test_latest_transcript_log_handles_missing_dir(tmp_path):
    assert latest_transcript_log(tmp_path / "nope") is None


# ------------------------------------------------------------ transcript Q&A ---

def test_answer_question_short_transcript_single_call():
    llm = FakeLLM(["  Launch is March 3 [02:10]. "])
    out = answer_question(llm, "[02:10] Others: we launch March 3", "When is launch?")
    assert out == "Launch is March 3 [02:10]."
    assert "never guess" in llm.prompts[0]
    assert "Question: When is launch?" in llm.prompts[0]
    assert "we launch March 3" in llm.prompts[0]


def test_answer_question_map_reduces_and_skips_irrelevant_parts():
    transcript = "\n".join(f"[00:{i:02d}] Me: " + "word " * 5 for i in range(6))
    llm = FakeLLM([CHAT_NOT_FOUND, "part two says yes", "Not in this part."])
    out = answer_question(llm, transcript, "q?", words_per_chunk=14)
    assert len(llm.prompts) == 3
    assert out == "part two says yes"  # one relevant part: no combine call needed
    assert all("part " in p for p in llm.prompts[:3])


def test_answer_question_combines_several_relevant_parts():
    transcript = "\n".join(f"[00:{i:02d}] Me: " + "word " * 5 for i in range(6))
    llm = FakeLLM(["a [00:00]", "b [00:02]", "c [00:04]", "combined"])
    assert answer_question(llm, transcript, "q?", words_per_chunk=14) == "combined"
    assert "a [00:00]" in llm.prompts[-1] and "c [00:04]" in llm.prompts[-1]


def test_answer_question_nothing_found():
    llm = FakeLLM([CHAT_NOT_FOUND, CHAT_NOT_FOUND])
    transcript = "a b c d\ne f g h"
    assert answer_question(llm, transcript, "q", words_per_chunk=4) == CHAT_NO_ANSWER
    assert answer_question(FakeLLM(), "   ", "q") == CHAT_NO_ANSWER


# ------------------------------------------------------------ system audio ---

def test_resample_and_to_mono():
    out = system_audio.resample(np.ones(48000, dtype=np.float32), 48000, 16000)
    assert out.shape == (16000,) and out.dtype == np.float32
    stereo = np.stack([np.ones(4), np.zeros(4)], axis=1).astype(np.float32)
    assert np.allclose(system_audio.to_mono(stereo), 0.5)


def test_find_loopback_device_prefers_configured_name(monkeypatch):
    devices = [
        {"name": "MacBook Mic", "max_input_channels": 1},
        {"name": "BlackHole 2ch", "max_input_channels": 2},
        {"name": "Speakers", "max_input_channels": 0},
        {"name": "Zoom Loopback", "max_input_channels": 2},
    ]
    monkeypatch.setattr(system_audio.sd, "query_devices", lambda *a: devices)
    assert system_audio.find_loopback_device() == 1
    assert system_audio.find_loopback_device("zoom") == 3
    assert system_audio.find_loopback_device("speakers") is None


def test_open_system_audio_explains_when_nothing_found(monkeypatch):
    monkeypatch.setattr(system_audio.sys, "platform", "linux")
    monkeypatch.setattr(system_audio.sd, "query_devices", lambda *a: [])
    with pytest.raises(system_audio.SystemAudioUnavailable, match="system_audio_device"):
        system_audio.open_system_audio(16000)
    with pytest.raises(system_audio.SystemAudioUnavailable, match="BlackHole"):
        system_audio.open_system_audio(16000, "BlackHole")
