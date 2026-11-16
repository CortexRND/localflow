"""Settings agent: local-only model calls, strict validation, apply and undo.

Ollama is a fake requests-like session; config, dictionary and the undo
record live in tmp_path, so nothing here touches the real machine.
"""

import dataclasses
import json
import os
import stat

import pytest
import requests

from localflow import settings_agent as sa
from localflow.config import Config, _config_from_v2, _parse, save_config
from localflow.dictionary import DictionaryStore

# ------------------------------------------------------------------ fakes ---


class FakeResponse:
    def __init__(self, status=200, body=None, raw=None, error=None):
        self.status_code = status
        self._raw = raw if raw is not None else json.dumps(body).encode()
        self.closed = False
        self.error = error  # raised mid-body, after the headers arrived

    def iter_content(self, size):
        for start in range(0, len(self._raw), size):
            if self.error is not None and start:
                raise self.error
            yield self._raw[start:start + size]
        if self.error is not None:
            raise self.error

    def close(self):
        self.closed = True


def call(name, **arguments):
    return {"function": {"name": name, "arguments": arguments}}


class FakeOllama:
    """Records every request; answers /api/tags and /api/chat."""

    def __init__(self, calls=(), models=("gemma4:e4b",), chat=None, tags=None,
                 error=None):
        self.calls = calls if isinstance(calls, str) else list(calls)
        self.models = list(models)
        self.chat = chat
        self.tags = tags
        self.error = error
        self.requests = []

    def get(self, url, **kwargs):
        self.requests.append(("GET", url, None, kwargs))
        if self.error is not None:
            raise self.error
        if self.tags is not None:
            return self.tags
        return FakeResponse(200, {"models": [{"name": name} for name in self.models]})

    def post(self, url, json=None, **kwargs):
        self.requests.append(("POST", url, json, kwargs))
        if self.error is not None:
            raise self.error
        if self.chat is not None:
            return self.chat
        message = {"role": "assistant", "content": "", "tool_calls": self.calls}
        return FakeResponse(200, {"model": json["model"], "message": message})

    @property
    def chat_payload(self):
        return next(r[2] for r in self.requests if r[0] == "POST")


@pytest.fixture
def paths(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    config_path = tmp_path / "config.toml"
    save_config(Config(), config_path)
    return {
        "config": config_path,
        "dictionary": tmp_path / "dictionary.json",
        "undo": tmp_path / "data" / "settings-undo.json",
        "root": tmp_path,
    }


def make_agent(paths, session=None, **overrides):
    options = {
        "config_path": paths["config"],
        "dictionary": DictionaryStore(paths["dictionary"]),
        "undo_path": paths["undo"],
        "session": session or FakeOllama(),
        "platform": "darwin",
        "templates": lambda _config: ("general", "standup"),
    }
    options.update(overrides)
    return sa.SettingsAgent(**options)


def read_config(paths):
    return _config_from_v2(_parse(paths["config"]))


def tree(root):
    """Every file under root with its bytes (a 'did anything get written' probe)."""
    found = {}
    for folder, _dirs, files in os.walk(root):
        for name in files:
            path = os.path.join(folder, name)
            with open(path, "rb") as handle:
                found[path] = (handle.read(), os.stat(path).st_mtime_ns)
    return found


# ---------------------------------------------------------------- preview ---


def test_preview_turns_tool_calls_into_changes_without_writing(paths):
    ollama = FakeOllama([
        call("set_setting", key="sounds_enabled", value="off"),
        call("add_dictionary_word", word="Kubernetes"),
        call("add_replacement", spoken="local flow", written="LocalFlow"),
    ])
    agent = make_agent(paths, ollama)
    before = tree(paths["root"])

    proposal = agent.propose(
        "turn off sounds, learn Kubernetes and always write local flow as LocalFlow"
    )

    assert tree(paths["root"]) == before  # preview writes nothing at all
    assert [c.line() for c in proposal.changes] == [
        "Sounds: On → Off",
        "Dictionary: — → add “Kubernetes”",
        "Replacement: “local flow” → “LocalFlow”",
    ]
    assert proposal.model == agent.model == "gemma4:e4b"
    assert proposal.rejected == ()
    assert not paths["undo"].exists()


def test_propose_returns_changes_that_apply_takes_directly(paths):
    ollama = FakeOllama([
        call("set_setting", key="sounds_enabled", value="off"),
        call("add_dictionary_word", word="Kubernetes"),
    ])
    agent = make_agent(paths, ollama)
    before = tree(paths["root"])

    changes = agent.propose("sounds off, learn Kubernetes")

    assert tree(paths["root"]) == before  # nothing written until apply
    assert len(changes) == 2 and list(changes) == list(changes.changes)
    assert all(isinstance(change, sa.Change) for change in changes)
    assert changes[0].line() == "Sounds: On → Off"
    assert changes.model == "gemma4:e4b" and changes.rejected == ()

    result = agent.apply(changes)

    assert result.message == "Applied 2 changes."
    assert read_config(paths).sounds_enabled is False
    assert DictionaryStore(paths["dictionary"]).words() == ["Kubernetes"]


def test_chat_request_is_local_tool_use_with_only_allowlisted_settings(paths):
    save_config(
        Config(server_host="0.0.0.0", server_port=9999, vault_path="/Users/me/secret-vault",
               llm_model="gemma4:e4b", orca_repo="id:private"),
        paths["config"],
    )
    ollama = FakeOllama([call("set_setting", key="sounds_enabled", value="off")])
    make_agent(paths, ollama).propose("sounds off")

    method, url, payload, kwargs = ollama.requests[-1]
    assert (method, url) == ("POST", "http://localhost:11434/api/chat")
    assert payload["stream"] is False
    assert kwargs["timeout"] == pytest.approx(60)
    assert kwargs["allow_redirects"] is False
    assert [t["function"]["name"] for t in payload["tools"]] == list(sa.TOOLS)
    key_enum = payload["tools"][0]["function"]["parameters"]["properties"]["key"]["enum"]
    assert key_enum == list(sa.ALLOWLIST)
    text = json.dumps(payload)
    for leaked in ("server_host", "0.0.0.0", "9999", "secret-vault", "vault_path",
                   "llm_base_url", "api_key", "orca", "private", "prompts_dir",
                   "fireworks"):
        assert leaked not in text, leaked
    system = payload["messages"][0]["content"]
    for key in sa.ALLOWLIST:
        assert f"- {key} (" in system


@pytest.mark.parametrize(
    ("provider", "base_url", "expected"),
    [
        ("ollama", "http://localhost:11434", "http://localhost:11434"),
        ("ollama", "http://127.0.0.1:11500/", "http://127.0.0.1:11500"),
        ("ollama", "http://[::1]:11434", "http://[::1]:11434"),
        ("ollama", "http://10.0.0.5:11434", "http://localhost:11434"),
        ("ollama", "https://ollama.example.com", "http://localhost:11434"),
        ("ollama", "http://user:pw@localhost:11434", "http://localhost:11434"),
        ("ollama", "http://localhost:11434/proxy/v1", "http://localhost:11434"),
        ("ollama", "http://localhost.evil.com:11434", "http://localhost:11434"),
        ("openai-compatible", "http://localhost:11434", "http://localhost:11434"),
        ("openai-compatible", "https://api.openai.com/v1", "http://localhost:11434"),
        ("groq", "https://api.groq.com/openai/v1", "http://localhost:11434"),
    ],
)
def test_endpoint_is_always_loopback_ollama(paths, provider, base_url, expected):
    save_config(Config(llm_provider=provider, llm_base_url=base_url), paths["config"])
    ollama = FakeOllama([call("set_setting", key="sounds_enabled", value="off")])
    make_agent(paths, ollama).propose("sounds off")

    assert {r[1].rsplit("/api/", 1)[0] for r in ollama.requests} == {expected}


def test_default_session_ignores_proxy_environment():
    assert sa._session().trust_env is False


@pytest.mark.parametrize(
    ("installed", "preferred", "expected"),
    [
        (["llama3.2:latest", "gemma4:e4b"], "llama3.2", "llama3.2:latest"),
        (["llama3.2:3b", "gemma4:e4b"], "llama3.2:3b", "llama3.2:3b"),
        (["llama3.2:3b", "gemma4:e4b", "gemma4:e2b"], "gpt-4o", "gemma4:e2b"),
        (["gemma4:latest"], "gemma4", "gemma4:latest"),
        (["nomic-embed-text:latest", "qwen3:4b"], "missing", "nomic-embed-text:latest"),
        (["qwen3:4b", "llama3.1:8b"], None, "qwen3:4b"),
    ],
)
def test_model_choice(installed, preferred, expected):
    assert sa.choose_model(installed, preferred) == expected


def test_configured_model_is_used_when_installed(paths):
    save_config(Config(llm_model="qwen3:4b"), paths["config"])
    ollama = FakeOllama(
        [call("set_setting", key="sounds_enabled", value="off")],
        models=("gemma4:e4b", "qwen3:4b"),
    )
    agent = make_agent(paths, ollama)
    agent.propose("sounds off")
    assert ollama.chat_payload["model"] == "qwen3:4b" == agent.model


@pytest.mark.parametrize(
    ("session", "message"),
    [
        (FakeOllama(error=requests.ConnectionError("refused")), "Couldn't reach Ollama"),
        (FakeOllama(models=()), "no models installed"),
        (FakeOllama(tags=FakeResponse(200, raw=b"<html>")), "invalid model list"),
        (FakeOllama(chat=FakeResponse(200, raw=b"not json")), "invalid chat response"),
        (FakeOllama(calls=[]), "didn't suggest any setting change"),
        (
            FakeOllama(chat=FakeResponse(
                400, {"error": "registry.ollama.ai/library/x does not support tools"}
            )),
            "can't use tools",
        ),
        (FakeOllama(chat=FakeResponse(500, raw=b"boom")), "couldn't run"),
        (
            FakeOllama(chat=FakeResponse(200, raw=b"x" * (sa.MAX_RESPONSE_BYTES + 10))),
            "oversized",
        ),
        (FakeOllama(calls="nope"), "malformed tool calls"),
        (
            # The cap is MAX_TOOL_CALLS (10): one more is refused outright.
            FakeOllama([call("set_setting", key="sounds_enabled", value="off")]
                       * (sa.MAX_TOOL_CALLS + 1)),
            f"more than {sa.MAX_TOOL_CALLS}",
        ),
    ],
)
def test_ollama_failures_are_clear_errors(paths, session, message):
    with pytest.raises(sa.SettingsAgentError, match=message):
        make_agent(paths, session).propose("turn off sounds")
    assert not paths["undo"].exists()


@pytest.mark.parametrize("error", [
    requests.ReadTimeout("read timed out"),
    requests.ConnectionError("connection reset"),
    requests.exceptions.ChunkedEncodingError("broken chunk"),
])
@pytest.mark.parametrize("where", ["tags", "chat"])
def test_failures_while_streaming_the_body_are_friendly(paths, error, where):
    payload = {"models": [{"name": "gemma4:e4b"}]}
    broken = FakeResponse(200, payload, error=error)
    ollama = FakeOllama(
        [call("set_setting", key="sounds_enabled", value="off")],
        tags=broken if where == "tags" else None,
        chat=broken if where == "chat" else None,
    )
    before = tree(paths["root"])
    with pytest.raises(sa.SettingsAgentError, match="Ollama") as caught:
        make_agent(paths, ollama).propose("sounds off")
    assert not isinstance(caught.value, requests.RequestException)
    assert broken.closed  # the streamed response is always released
    assert tree(paths["root"]) == before


@pytest.mark.parametrize("request_text", ["", "   ", None, 42, "x" * 501])
def test_request_is_bounded_and_required(paths, request_text):
    ollama = FakeOllama([call("set_setting", key="sounds_enabled", value="off")])
    with pytest.raises(sa.SettingsAgentError):
        make_agent(paths, ollama).propose(request_text)
    assert ollama.requests == []  # rejected before any network call


def test_disallowed_and_invalid_calls_are_rejected_with_reasons(paths):
    # Two batches: one request may carry at most MAX_TOOL_CALLS (10) calls.
    first = FakeOllama([
        call("set_setting", key="sounds_enabled", value="off"),
        call("set_setting", key="server_port", value="80"),
        call("set_setting", key="cleanup_level", value="maximum"),
        call("set_setting", key="hotkey", value="a"),
        call("set_setting", key="polish_hotkey", value="<shift>+a"),
        call("set_setting", key="paste_method", value="wtype"),  # not on macOS
        call("set_setting", key="sounds_enabled", value="maybe"),
    ])
    second = FakeOllama([
        call("set_setting", key="sounds_enabled", value="off"),
        call("run_shell", command="rm -rf ~"),
        call("add_dictionary_word", word="ok", extra="x"),
        call("add_dictionary_word", word="bad\nword"),
        {"function": {"name": "set_setting", "arguments": "{not json"}},
        "garbage",
    ])
    # The requests name the change that should survive (unnamed ones are
    # refused now, see test_settings_security).
    proposal = make_agent(paths, first).propose("turn off sounds and do lots more")
    other = make_agent(paths, second).propose("turn off sounds and do lots more")

    assert [c.key for c in proposal.changes] == ["sounds_enabled"]
    assert [c.key for c in other.changes] == ["sounds_enabled"]
    reasons = " | ".join(proposal.rejected + other.rejected)
    for expected in (
        "'server_port' is not a setting",
        "Cleanup level: choose one of", "Dictation shortcut:", "Polish shortcut:",
        "Paste method: on this computer choose one of: auto, clipboard, osascript",
        "sounds_enabled was set twice", "unknown tool 'run_shell'",
        "wrong arguments for add_dictionary_word", "control characters",
        "malformed arguments for set_setting", "malformed tool call",
    ):
        assert expected in reasons, expected


def test_everything_rejected_is_an_error_not_an_empty_preview(paths):
    ollama = FakeOllama([call("set_setting", key="server_port", value="80")])
    with pytest.raises(sa.SettingsAgentError, match="Nothing to change"):
        make_agent(paths, ollama).propose("open port 80")


def test_string_arguments_and_value_normalisation(paths):
    ollama = FakeOllama([
        {"function": {"name": "set_setting",
                      "arguments": json.dumps({"key": "style_work", "value": "Very Casual"})}},
        call("set_setting", key="hotkey", value="Right Option"),
        call("set_setting", key="prompt_hotkey", value="ctrl+option+5"),
        call("set_setting", key="command_hotkey", value="off"),
        call("set_setting", key="meeting_default_template", value="Standup"),
    ])
    changes = {c.key: c for c in make_agent(paths, ollama).propose(
        "make the work style very casual, dictate with Right Option, set the prompt "
        "shortcut to ctrl+option+5, turn the command key off, use the Standup template"
    ).changes}

    assert changes["style_work"].value == "very_casual"
    assert changes["hotkey"].value == "alt_r"
    assert changes["hotkey"].new == "Hold Right Option"
    assert changes["prompt_hotkey"].value == "<ctrl>+<alt>+5"
    assert changes["command_hotkey"].value == "" and changes["command_hotkey"].new == "Off"
    assert changes["meeting_default_template"].value == "standup"
    assert changes["meeting_default_template"].note == "From the next meeting"


def test_shortcut_clash_is_refused(paths):
    ollama = FakeOllama([
        call("set_setting", key="prompt_hotkey", value="<ctrl>+<alt>+1"),  # = polish
        call("set_setting", key="sounds_enabled", value="off"),
    ])
    proposal = make_agent(paths, ollama).propose(
        "set the prompt shortcut to ctrl+option+1 and turn off sounds"
    )
    assert [c.key for c in proposal.changes] == ["sounds_enabled"]
    assert any("already the Polish shortcut" in r for r in proposal.rejected)


def test_unchanged_values_and_existing_entries_are_not_previewed(paths):
    store = DictionaryStore(paths["dictionary"])
    store.add_word("Kubernetes")
    store.add_replacement("teh", "the")
    ollama = FakeOllama([
        call("set_setting", key="sounds_enabled", value="on"),  # already on
        call("add_dictionary_word", word="kubernetes"),
        call("add_replacement", spoken="TEH", written="the"),
        call("add_dictionary_word", word="Terraform"),
    ])
    proposal = make_agent(paths, ollama).propose(
        "turn on sounds, learn kubernetes and Terraform, replace teh with the"
    )
    assert [c.value for c in proposal.changes] == ["Terraform"]
    assert len(proposal.rejected) == 3


@pytest.mark.parametrize("platform_name,allowed,refused", [
    ("darwin", "osascript", "xdotool"),
    ("linux", "xdotool", "osascript"),
    ("win32", "pynput", "wtype"),
])
def test_paste_methods_follow_the_platform(platform_name, allowed, refused):
    validator = sa.Validator(platform_name, templates=lambda _c: ())
    assert validator.value("paste_method", allowed, Config()) == allowed
    with pytest.raises(sa.SettingError):
        validator.value("paste_method", refused, Config())


@pytest.mark.parametrize("value,expected", [
    ("<ctrl>+<alt>+d", "<ctrl>+<alt>+d"),
    ("Cmd+Shift+Space", "<cmd>+<shift>+<space>"),
    ("ctrl+f5", "<ctrl>+<f5>"),
])
def test_combos_normalise(value, expected):
    assert sa.normalize_combo(value) == expected


@pytest.mark.parametrize("value", [
    "a", "<shift>+a", "<ctrl>+<alt>", "<ctrl>+a+b", "<ctrl>+<ctrl>+a", "<ctrl>+<esc>",
    "<ctrl>+<nope>", "", "<ctrl>++",
])
def test_junk_combos_are_refused(value):
    with pytest.raises(sa.SettingError):
        sa.normalize_combo(value)


@pytest.mark.parametrize("value", ["a", "space", "esc", "enter", "alt_r+a", "<ctrl>+<alt>+1"])
def test_junk_hold_keys_are_refused(value):
    with pytest.raises(sa.SettingError):
        sa.normalize_hold_key(value)


# ------------------------------------------------------------ apply / undo ---


def _request_naming(calls):
    """A request that names exactly the calls' changes (unnamed ones are
    refused), so these apply/undo tests keep previewing the same batch."""
    parts = []
    for item in calls:
        name, args = item["function"]["name"], item["function"]["arguments"]
        if name == "set_setting":
            parts.append(f"set {sa.LABELS[args['key']]} to {args['value']}")
        elif name == "add_dictionary_word":
            parts.append(f"learn {args['word']}")
        else:
            parts.append(f"replace {args['spoken']} with {args['written']}")
    return "; ".join(parts)


def _preview(paths, calls, **overrides):
    agent = make_agent(paths, FakeOllama(calls), **overrides)
    return agent, agent.propose(_request_naming(calls))


def test_apply_and_undo_round_trip_across_a_restart(paths):
    store = DictionaryStore(paths["dictionary"])
    store.add_word("Existing")
    store.add_replacement("lf", "LocalFlow")
    agent, proposal = _preview(paths, [
        call("set_setting", key="sounds_enabled", value="off"),
        call("set_setting", key="hotkey", value="f13"),
        call("add_dictionary_word", word="Kubernetes"),
        call("add_replacement", spoken="kay eight s", written="k8s"),
    ])

    result = agent.apply(proposal.changes)

    assert result.message == "Applied 4 changes."
    config = read_config(paths)
    assert config.sounds_enabled is False and config.hotkey == "f13"
    assert store.words() == ["Existing", "Kubernetes"]
    assert ("kay eight s", "k8s") in store.replacements()
    if os.name == "posix":  # Windows has no POSIX permission bits to check
        assert stat.S_IMODE(paths["undo"].stat().st_mode) == 0o600
    record = json.loads(paths["undo"].read_text())
    assert record["words"] == ["Kubernetes"]  # pre-existing entries never recorded
    assert len(paths["undo"].read_bytes()) < sa.MAX_UNDO_BYTES

    # "Restart": a brand-new agent finds the batch on disk and undoes it.
    fresh = make_agent(paths)
    assert fresh.pending_undo() == [
        "Sounds: Off → On",
        "Dictation shortcut: Hold F13 → Hold Left Option",
        "Remove word “Kubernetes”",
        "Remove replacement “kay eight s” → “k8s”",
    ]
    undone = fresh.undo()
    config = read_config(paths)
    assert config.sounds_enabled is True and config.hotkey == "alt_l"
    assert store.words() == ["Existing"]
    assert store.replacements() == [("lf", "LocalFlow")]
    assert undone.skipped == ()
    assert not paths["undo"].exists()
    with pytest.raises(sa.SettingsAgentError, match="no change to undo"):
        fresh.undo()


def test_apply_preserves_unrelated_changes_made_after_the_preview(paths):
    agent, proposal = _preview(paths, [call("set_setting", key="sounds_enabled", value="off")])
    save_config(Config(llm_model="qwen3:4b", vault_path="~/notes"), paths["config"])

    agent.apply(proposal.changes)

    config = read_config(paths)
    assert config.sounds_enabled is False
    assert (config.llm_model, config.vault_path) == ("qwen3:4b", "~/notes")


def test_stale_preview_is_refused_without_writing(paths):
    agent, proposal = _preview(paths, [
        call("set_setting", key="sounds_enabled", value="off"),
        call("add_dictionary_word", word="Kubernetes"),
    ])
    save_config(Config(sounds_enabled=False), paths["config"])  # changed elsewhere
    before = tree(paths["root"])

    with pytest.raises(sa.SettingsAgentError, match="changed to Off since the preview"):
        agent.apply(proposal.changes)
    assert tree(paths["root"]) == before


@pytest.mark.parametrize("tamper", [
    lambda c: dataclasses.replace(c, value=False, new="On"),       # display mismatch
    lambda c: dataclasses.replace(c, value="off"),                 # non-canonical value
    lambda c: dataclasses.replace(c, key="server_port", value=80),  # off the allowlist
    lambda c: dataclasses.replace(c, kind="shell", value="rm -rf ~"),
    lambda c: "sounds_enabled=false",                              # not a Change
])
def test_tampered_changes_are_refused(paths, tamper):
    agent, proposal = _preview(paths, [call("set_setting", key="sounds_enabled", value="off")])
    before = tree(paths["root"])
    with pytest.raises(sa.SettingsAgentError):
        agent.apply([tamper(proposal.changes[0])])
    assert tree(paths["root"]) == before


def test_constructed_change_matching_validation_is_still_stale_checked(paths):
    agent = make_agent(paths)
    forged = sa.setting_change("sounds_enabled", False, True)  # claims it was Off
    with pytest.raises(sa.SettingsAgentError, match="since the preview"):
        agent.apply([forged])


def test_failure_mid_batch_rolls_back_and_keeps_previous_undo(paths, monkeypatch):
    # An earlier batch is undoable...
    agent, proposal = _preview(paths, [call("add_dictionary_word", word="First")])
    agent.apply(proposal.changes)
    earlier = paths["undo"].read_bytes()
    # ...then a new batch fails on its last write.
    agent, proposal = _preview(paths, [
        call("set_setting", key="sounds_enabled", value="off"),
        call("add_dictionary_word", word="Second"),
        call("add_replacement", spoken="foo", written="bar"),
    ])
    monkeypatch.setattr(
        DictionaryStore, "add_replacement",
        lambda self, s, t: (_ for _ in ()).throw(OSError("disk full")),
    )
    with pytest.raises(sa.SettingsAgentError, match="Nothing was changed: couldn't save"):
        agent.apply(proposal.changes)

    assert read_config(paths).sounds_enabled is True
    assert DictionaryStore(paths["dictionary"]).words() == ["First"]
    assert paths["undo"].read_bytes() == earlier


def test_lost_undo_record_rolls_the_batch_back(paths, monkeypatch):
    agent, proposal = _preview(paths, [
        call("set_setting", key="sounds_enabled", value="off"),
        call("add_dictionary_word", word="Kubernetes"),
    ])
    monkeypatch.setattr(sa, "_write_private_json", lambda *_a: (_ for _ in ()).throw(
        OSError("read-only")))
    with pytest.raises(sa.SettingsAgentError, match="Nothing was changed"):
        agent.apply(proposal.changes)
    assert read_config(paths).sounds_enabled is True
    assert DictionaryStore(paths["dictionary"]).words() == []


def test_undo_never_overwrites_newer_changes(paths):
    store = DictionaryStore(paths["dictionary"])
    agent, proposal = _preview(paths, [
        call("set_setting", key="sounds_enabled", value="off"),
        call("set_setting", key="cleanup_level", value="medium"),
        call("add_dictionary_word", word="Kubernetes"),
        call("add_replacement", spoken="foo", written="bar"),
    ])
    agent.apply(proposal.changes)
    # Newer edits: cleanup level changed again, replacement retargeted.
    config = read_config(paths)
    save_config(dataclasses.replace(config, cleanup_level="none"), paths["config"])
    store.update_replacement("foo", "foo", "baz")

    result = make_agent(paths).undo()

    config = read_config(paths)
    assert config.sounds_enabled is True        # reverted
    assert config.cleanup_level == "none"       # newer change kept
    assert store.words() == []                  # exactly-added word removed
    assert store.replacements() == [("foo", "baz")]  # edited entry kept
    assert any("Cleanup level" in s for s in result.skipped)
    assert "Left alone" in result.message


def test_exact_removal_is_one_locked_compare_and_remove(paths):
    store = DictionaryStore(paths["dictionary"])
    store.add_word("kubernetes")
    store.add_replacement("foo", "baz")
    assert store.remove_exact_word("Kubernetes") is False      # casing differs
    assert store.remove_exact_replacement("foo", "bar") is False  # target differs
    assert store.words() == ["kubernetes"]
    assert store.replacements() == [("foo", "baz")]
    assert store.remove_exact_word("kubernetes") is True
    assert store.remove_exact_replacement("foo", "baz") is True
    assert store.list() == {"words": [], "replacements": []}
    # The existing case-insensitive remove keeps its default behaviour.
    store.add_word("Terraform")
    assert store.remove("terraform") is True


def test_edit_racing_undo_survives(paths, monkeypatch):
    agent, proposal = _preview(paths, [
        call("add_dictionary_word", word="Kubernetes"),
        call("add_replacement", spoken="foo", written="bar"),
    ])
    agent.apply(proposal.changes)
    rival = DictionaryStore(paths["dictionary"])
    exact_word = DictionaryStore.remove_exact_word
    exact_replacement = DictionaryStore.remove_exact_replacement

    def edit_then_remove_word(self, word):
        rival.update_word("Kubernetes", "kubernetes")  # lands right before removal
        return exact_word(self, word)

    def edit_then_remove_replacement(self, source, target):
        rival.update_replacement("foo", "foo", "baz")
        return exact_replacement(self, source, target)

    monkeypatch.setattr(DictionaryStore, "remove_exact_word", edit_then_remove_word)
    monkeypatch.setattr(DictionaryStore, "remove_exact_replacement",
                        edit_then_remove_replacement)
    # No separate pre-check may decide the removal outside the store's lock.
    for name in ("words", "replacements", "remove"):
        monkeypatch.setattr(DictionaryStore, name, lambda *a, **k: pytest.fail(
            "undo must not check-then-delete"))

    with pytest.raises(sa.SettingsAgentError, match="Nothing to undo"):
        make_agent(paths).undo()

    monkeypatch.undo()
    assert rival.words() == ["kubernetes"]
    assert rival.replacements() == [("foo", "baz")]


def test_undo_with_everything_changed_since_is_an_error(paths):
    agent, proposal = _preview(paths, [call("set_setting", key="sounds_enabled", value="off")])
    agent.apply(proposal.changes)
    save_config(Config(sounds_enabled=True), paths["config"])
    with pytest.raises(sa.SettingsAgentError, match="Nothing to undo"):
        make_agent(paths).undo()


@pytest.mark.parametrize("content", [
    b"not json",
    json.dumps({"version": 1, "settings": [{"key": "server_port", "before": 1, "after": 2}],
                "words": [], "replacements": []}).encode(),
    json.dumps({"version": 1, "settings": [{"key": "sounds_enabled", "before": "x",
                                            "after": True}], "words": [],
                "replacements": []}).encode(),
    json.dumps({"version": 9, "settings": [], "words": [], "replacements": []}).encode(),
    json.dumps({"version": 1, "settings": [], "words": ["x"] * 50,
                "replacements": []}).encode(),
    b"{" + b" " * (sa.MAX_UNDO_BYTES + 10) + b"}",
])
def test_tampered_or_oversized_undo_record_is_refused(paths, content):
    paths["undo"].parent.mkdir(parents=True, exist_ok=True)
    paths["undo"].write_bytes(content)
    before = read_config(paths)
    with pytest.raises(sa.SettingsAgentError, match="unreadable"):
        make_agent(paths).undo()
    assert read_config(paths) == before
    assert make_agent(paths).pending_undo() is None


def test_undo_record_holds_no_secrets_or_paths(paths):
    agent, proposal = _preview(paths, [call("set_setting", key="hot_phrases", value="off")])
    agent.apply(proposal.changes)
    record = json.loads(paths["undo"].read_text())
    assert set(record) == {"version", "created", "settings", "words", "replacements"}
    assert record["settings"] == [{"key": "hot_phrases", "before": True, "after": False}]


def test_current_settings_is_a_short_read_only_list(paths):
    rows = dict(make_agent(paths).current_settings())
    assert rows["Sounds"] == "On"
    assert rows["Dictation shortcut"] == "Hold Left Option"
    assert len(rows) == len(sa.CURRENT_KEYS)


@pytest.mark.parametrize("record_settings", [
    [{"key": "cleanup_level", "before": "maximum", "after": "light"}],     # bad choice
    [{"key": "style_work", "before": "formal", "after": "shouty"}],
    [{"key": "hotkey", "before": "a", "after": "alt_l"}],                   # junk shortcut
    [{"key": "polish_hotkey", "before": "<shift>+a", "after": "<ctrl>+<alt>+1"}],
    [{"key": "hotkey", "before": "", "after": "alt_l"}],                    # can't be off
    [{"key": "paste_method", "before": "xdotool", "after": "auto"}],        # not on macOS
    [{"key": "meeting_default_template", "before": "secret", "after": "general"}],
    [{"key": "hotkey", "before": "Right Option", "after": "alt_l"}],        # not canonical
    [{"key": "sounds_enabled", "before": True, "after": False},
     {"key": "sounds_enabled", "before": False, "after": True}],           # duplicate key
])
def test_undo_record_values_are_revalidated_like_apply(paths, record_settings):
    save_config(Config(cleanup_level="light"), paths["config"])
    paths["undo"].parent.mkdir(parents=True, exist_ok=True)
    paths["undo"].write_text(json.dumps({
        "version": 1, "settings": record_settings, "words": [], "replacements": [],
    }))
    before = tree(paths["root"])
    agent = make_agent(paths)
    with pytest.raises(sa.SettingsAgentError, match="unreadable|can't restore"):
        agent.undo()
    assert agent.pending_undo() is None
    assert tree(paths["root"]) == before  # nothing written, record kept


@pytest.mark.parametrize("extra", [
    {"words": ["Kubernetes", "kubernetes"]},
    {"replacements": [{"spoken": "foo", "written": "a"}, {"spoken": "FOO", "written": "b"}]},
])
def test_undo_record_with_duplicate_entries_is_refused(paths, extra):
    record = {"version": 1, "settings": [], "words": [], "replacements": []}
    record.update(extra)
    paths["undo"].parent.mkdir(parents=True, exist_ok=True)
    paths["undo"].write_text(json.dumps(record))
    with pytest.raises(sa.SettingsAgentError, match="unreadable"):
        make_agent(paths).undo()


def test_valid_hand_written_record_still_undoes(paths):
    save_config(Config(sounds_enabled=False), paths["config"])
    paths["undo"].parent.mkdir(parents=True, exist_ok=True)
    paths["undo"].write_text(json.dumps({
        "version": 1, "settings": [{"key": "sounds_enabled", "before": True, "after": False}],
        "words": [], "replacements": [],
    }))
    make_agent(paths).undo()
    assert read_config(paths).sounds_enabled is True


# ----------------------------------------------- shortcut grammar, no pynput ---


def test_shortcut_validation_never_imports_pynput_or_appkit(tmp_path):
    """Run validation in a fresh interpreter and list what got imported."""
    import subprocess
    import sys
    import textwrap

    script = textwrap.dedent("""
        import sys
        from localflow import settings_agent as sa
        from localflow.config import Config
        v = sa.Validator("darwin", templates=lambda c: ("general",))
        for key, value in [("hotkey", "alt_r"), ("hotkey", "<cmd>+<shift>+<space>"),
                           ("command_hotkey", "f13"), ("polish_hotkey", "ctrl+option+5")]:
            v.value(key, value, Config())
        sa.normalize_combo("<ctrl>+<alt>+d", "linux")
        loaded = [m for m in sys.modules
                  if m.split(".")[0] in ("pynput", "AppKit", "Quartz", "Foundation", "objc")]
        print(",".join(loaded))
    """)
    result = subprocess.run(
        [sys.executable, "-c", script], capture_output=True, text=True, timeout=60, check=False,
        env={**os.environ, "HOME": str(tmp_path)},
        cwd=os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == ""


def test_shortcut_names_match_the_platforms_pynput_backends():
    """The static key tables agree with pynput's per-platform Key classes,
    read as source text (importing the backends would load native code)."""
    import ast
    import importlib.util
    from pathlib import Path

    spec = importlib.util.find_spec("pynput")
    if spec is None or not spec.submodule_search_locations:
        pytest.skip("pynput not installed")
    folder = Path(next(iter(spec.submodule_search_locations))) / "keyboard"
    names = set(sa.HOLD_KEYS) | set(sa._COMBO_NAMED)
    for platform_name, filename in (("darwin", "_darwin.py"), ("win32", "_win32.py"),
                                    ("linux", "_xorg.py")):
        tree = ast.parse((folder / filename).read_text(encoding="utf-8"))
        key_class = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == "Key")
        defined = {
            target.id for node in key_class.body if isinstance(node, ast.Assign)
            for target in node.targets if isinstance(target, ast.Name)
        }
        unsupported = sa._UNSUPPORTED_KEYS.get(platform_name, frozenset())
        assert names - unsupported <= defined, (platform_name, names - unsupported - defined)
        assert unsupported.isdisjoint(defined), platform_name


def test_insert_is_refused_on_macos_only():
    with pytest.raises(sa.SettingError, match="not available"):
        sa.normalize_combo("<ctrl>+<alt>+<insert>", "darwin")
    assert sa.normalize_combo("<ctrl>+<alt>+<insert>", "linux") == "<ctrl>+<alt>+<insert>"


@pytest.mark.parametrize("value", [
    "<ctrl>+<alt>+d", "Cmd+Shift+Space", "ctrl+f5", "ctrl+option+5", "<cmd>+<ctrl>+v",
])
def test_normalized_combos_are_accepted_by_the_real_hotkey_parser(value, monkeypatch):
    """KeyboardHub stays the real parser: what we save, it accepts."""
    import enum
    import importlib.util

    if importlib.util.find_spec("pynput") is None:
        pytest.skip("pynput not installed")
    import pynput.keyboard as kb
    from pynput.keyboard import HotKey  # the test process, not the pure module

    if kb.Key.ctrl == kb.Key.alt:
        # PYNPUT_BACKEND=dummy (CI) gives every Key the same vk, so Enum
        # aliases them all and the real parser rejects any two named keys
        # as duplicates. Swap in the same names with distinct vks; HotKey.parse
        # itself (grammar, modifier handling, duplicate check) stays real.
        fake = enum.Enum("Key", {
            name: kb.KeyCode.from_vk(0x10000 + i) for i, name in enumerate(kb.Key.__members__)
        })
        groups = {"alt_gr": ("alt_gr",)} | {
            base: (base, f"{base}_l", f"{base}_r") for base in ("alt", "cmd", "ctrl", "shift")
        }
        monkeypatch.setattr(kb, "Key", fake)
        monkeypatch.setattr(kb, "_NORMAL_MODIFIERS", {
            fake[name].value: fake[base] for base, names in groups.items() for name in names
        })

    try:
        keys = HotKey.parse(sa.normalize_combo(value, "linux"))
    except ValueError as exc:  # pragma: no cover - the assertion below explains
        pytest.fail(f"HotKey.parse rejected {value!r}: {exc}")
    assert len(keys) == len(sa.normalize_combo(value, "linux").split("+"))


def test_allowlist_is_exactly_the_documented_fields():
    assert set(sa.ALLOWLIST) == {
        "dictation_enabled",
        "dictation_toggle_hotkey",
        "sounds_enabled",
        "cleanup_enabled",
        "cleanup_level",
        "styles_enabled",
        "style_work",
        "style_email",
        "style_personal",
        "style_other",
        "hotkey",
        "command_hotkey",
        "polish_hotkey",
        "prompt_hotkey",
        "paste_last_hotkey",
        "paste_method",
        "spoken_symbols",
        "spoken_punctuation",
        "hot_phrases",
        "meeting_watch",
        "meeting_auto_start",
        "capture_system_audio",
        "meeting_default_template",
        "work_prompts",
    }
    assert set(sa.LABELS) == set(sa.ALLOWLIST)


def test_turn_off_dictation(paths):
    agent = make_agent(
        paths,
        FakeOllama(
            [
                call("set_setting", key="dictation_enabled", value="off"),
            ]
        ),
    )
    proposal = agent.propose("turn off dictation")
    assert len(proposal.changes) == 1
    agent.apply(proposal.changes)
    assert read_config(paths).dictation_enabled is False
