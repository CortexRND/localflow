"""`lf agents …` — launch and manage headless agent sessions.

Mutations go through the HTTP API; `list`, `show` and `logs` (and rule/profile
edits) fall back to the local store when the server is down. Agent output is
untrusted: it is echoed as plain text, never as Rich markup.
"""

from __future__ import annotations

import json
import re
import sys
import time
from pathlib import Path

import click

TERMINAL = ("completed", "failed", "cancelled", "timed_out")
_HINT = "server not running — open LocalFlow.app or run: lf serve"


class ServerDown(Exception):
    pass


class ApiError(Exception):
    def __init__(self, status: int, body: dict):
        self.status = status
        self.body = body
        super().__init__(str(body.get("detail") or f"HTTP {status}"))


def _base() -> str:
    from localflow.server_url import configured_base_url

    return configured_base_url()


def _headers() -> dict:
    from localflow.apitoken import read

    token = read()
    return {"X-Localflow-Token": token} if token else {}


def _request(method: str, path: str, *, body=None, params=None, timeout: float = 30) -> dict:
    import requests

    try:
        response = requests.request(method, _base() + path, json=body, params=params,
                                    headers=_headers(), timeout=timeout, allow_redirects=False)
    except requests.ConnectionError:
        raise ServerDown() from None
    try:
        data = response.json()
    except ValueError:
        data = {"detail": response.reason or f"HTTP {response.status_code}"}
    if not response.ok:
        raise ApiError(response.status_code, data if isinstance(data, dict) else {})
    return data


def _call(method: str, path: str, **kwargs) -> dict:
    try:
        return _request(method, path, **kwargs)
    except ServerDown:
        click.echo(f"error: {_HINT}", err=True)
        sys.exit(1)
    except ApiError as exc:
        click.echo(f"error: {exc}", err=True)
        sys.exit(1)


def _store():
    from localflow.agents.store import AgentSessionStore

    return AgentSessionStore()


def _echo_json(value) -> None:
    click.echo(json.dumps(value, indent=2, ensure_ascii=False))


_TTY_ESCAPES = re.compile(
    r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?"   # OSC … BEL / ST
    r"|\x1b[P^_X][^\x1b]*(?:\x1b\\)?"         # DCS / PM / APC / SOS … ST
    r"|\x1b\[[0-?]*[ -/]*[@-~]"                # CSI
    r"|\x1b[ -/]*[0-~]?"                        # other ESC sequences, lone ESC
    r"|\x9b[0-?]*[ -/]*[@-~]"                   # 8-bit CSI
    r"|[\x00-\x08\x0b-\x1f\x7f-\x9f]"           # C0 (but \t \n), DEL, C1
)


def _sanitize_tty(text: str) -> str:
    """Strip ANSI escape sequences and control characters (keeping \\n and \\t)."""
    return _TTY_ESCAPES.sub("", text)


def _safe(text, *, err: bool = False) -> str:
    """Untrusted (agent/provider) text for display: sanitized when it goes to a terminal,
    unchanged when piped."""
    text = str(text if text is not None else "")
    stream = sys.stderr if err else sys.stdout
    try:
        tty = stream.isatty()
    except (AttributeError, ValueError):
        tty = False
    return _sanitize_tty(text) if tty else text


def _one_line(text: str, limit: int = 60) -> str:
    text = " ".join(_safe(text).split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


def _all_session_ids() -> list[str]:
    try:
        ids, before = [], None
        while True:
            params = {"limit": 200}
            if before:
                params["before"] = before
            page = _request("GET", "/api/agents/sessions", params=params)
            ids += [s["id"] for s in page["sessions"]]
            before = page.get("next_cursor")
            if not before:
                return ids
    except ServerDown:
        return [s.id for s in _store().list()]
    except ApiError as exc:
        click.echo(f"error: {exc}", err=True)
        sys.exit(1)


def _resolve(prefix: str) -> str:
    """Full session id for a unique prefix, or exit 1."""
    prefix = prefix.strip().lower()
    matches = [sid for sid in _all_session_ids() if sid.startswith(prefix)] if prefix else []
    if len(matches) == 1:
        return matches[0]
    what = "no agent session" if not matches else "ambiguous agent session"
    click.echo(f"error: {what} {prefix!r} — list them with: lf agents list", err=True)
    sys.exit(1)


@click.group("agents")
def agents() -> None:
    """Launch and manage headless coding agents (Claude Code, Devin, HTTP)."""


@agents.command("status")
@click.option("--json", "as_json", is_flag=True, help="Print JSON.")
def status_command(as_json: bool) -> None:
    """Show whether agents are on, the limits and session counts."""
    data = _call("GET", "/api/agents/status")
    if as_json:
        _echo_json(data)
        return
    click.echo(f"enabled: {'yes' if data['enabled'] else 'no'}"
               f"{'  (sandbox)' if data['sandbox'] else ''}")
    click.echo(f"hot commands: {'on' if data['hot_commands'] else 'off'}")
    click.echo(f"default provider: {data['default_provider']}")
    click.echo(f"default repo: {data['default_repo'] or '-'}")
    for repo in data["repos"]:
        click.echo(f"  repo {repo['alias']}: {repo['path']}"
                   f"{'' if repo['exists'] else ' (missing)'}")
    counts = data["counts"]
    click.echo(f"sessions: {counts['active']} active, {counts['queued']} queued, "
               f"{counts['terminal']} finished")


@agents.command("providers")
@click.option("--json", "as_json", is_flag=True, help="Print JSON.")
@click.option("--check", is_flag=True, help="Run a deep readiness check for each provider.")
def providers_command(as_json: bool, check: bool) -> None:
    """List providers, their capabilities and readiness."""
    entries = _call("GET", "/api/agents/providers")["providers"]
    if check:
        entries = [_call("POST", f"/api/agents/providers/{e['id']}/check",
                         body={"profile": e["profile"]}, timeout=30)
                   if e["id"] != "http" or e["profile"] else e for e in entries]
    if as_json:
        _echo_json({"providers": entries})
        return
    for entry in entries:
        ready = entry["readiness"]
        name = entry["id"] + (f"/{entry['profile']}" if entry["profile"] else "")
        mark = "ready" if ready["ready"] else ready["code"]
        click.echo(f"{name:<20} {mark:<16} {_one_line(ready['detail'], 70)}")


def _read_prompt(prompt: str | None, file: str | None) -> str:
    if prompt and file:
        raise click.UsageError("give PROMPT or --file, not both")
    if file:
        return sys.stdin.read() if file == "-" else Path(file).read_text(encoding="utf-8")
    if not prompt:
        raise click.UsageError("give a PROMPT or --file")
    return prompt


def _follow(session_id: str, *, as_json: bool, after: int = 0) -> str:
    """Print events until the session ends; returns the final state."""
    while True:
        try:
            page = _request("GET", f"/api/agents/sessions/{session_id}/events",
                            params={"after": after, "wait": 25}, timeout=40)
        except ServerDown:
            click.echo(f"error: {_HINT}", err=True)
            sys.exit(1)
        except ApiError as exc:
            click.echo(f"error: {exc}", err=True)
            sys.exit(1)
        for event in page["events"]:
            _print_event(event, as_json=as_json)
        after = page["next_after"]
        if page["terminal"] and not page["has_more"]:
            return page["state"]
        if not page["events"] and not page["waited"]:
            time.sleep(2)


def _print_event(event: dict, *, as_json: bool) -> None:
    if as_json:
        click.echo(json.dumps(event, ensure_ascii=False))
    elif event["type"] == "output":
        err = event["stream"] == "stderr"
        text = _safe(event["text"], err=err)
        click.echo(text, nl=not text.endswith("\n"), err=err)
    elif event["type"] == "state":
        click.echo(f"[{event['ts']}] state: {_safe(event['state'], err=True)}", err=True)
    elif event["type"] in ("error", "truncated", "message"):
        click.echo(f"[{event['ts']}] {event['type']}: {_safe(event['text'], err=True)}",
                   err=True)


@agents.command("launch")
@click.argument("prompt", required=False)
@click.option("-p", "--provider", default="", help="Provider id (default: agents.default_provider).")
@click.option("--profile", default="", help="HTTP agent profile id (provider http).")
@click.option("-r", "--repo", default="", help="Repo alias from agents.repos.")
@click.option("--cwd", default="", help="Working directory inside a configured repo.")
@click.option("--resume", "resume", default="", help="Continue a finished session (new child).")
@click.option("--title", default="", help="Session title.")
@click.option("--timeout", "timeout", type=int, default=None, help="Timeout in seconds.")
@click.option("-F", "--file", "file", default=None, help="Read the prompt from a file (- = stdin).")
@click.option("-f", "--follow", is_flag=True, help="Stream events until the session ends.")
@click.option("--json", "as_json", is_flag=True, help="Print JSON.")
def launch_command(prompt, provider, profile, repo, cwd, resume, title, timeout, file, follow,
                   as_json) -> None:
    """Launch an agent session with PROMPT."""
    text = _read_prompt(prompt, file)
    body: dict = {"prompt": text, "origin": "cli"}
    for name, value in (("provider", provider), ("profile", profile), ("repo", repo),
                        ("cwd", str(Path(cwd).expanduser().resolve()) if cwd else ""),
                        ("title", title)):
        if value:
            body[name] = value
    if resume:
        body["resume_of"] = _resolve(resume)
    if timeout is not None:
        body["timeout_seconds"] = timeout
    session = _call("POST", "/api/agents/sessions", body=body)["session"]
    if as_json and not follow:
        _echo_json({"session": session})
        return
    click.echo(f"launched {session['id']} ({session['provider']}) — {session['state']}",
               err=follow)
    if follow:
        state = _follow(session["id"], as_json=as_json)
        final = _call("GET", f"/api/agents/sessions/{session['id']}")["session"]
        if as_json:
            _echo_json({"session": final})
        elif final.get("error"):
            click.echo(f"{state}: {final['error']['code']}: "
                       f"{_safe(final['error']['message'], err=True)}", err=True)
        else:
            click.echo(state, err=True)
        sys.exit(0 if state == "completed" else 1)


def _offline_list(states: set[str] | None, provider: str, limit: int) -> list[dict]:
    from localflow.agents.models import ACTIVE_STATES, TERMINAL_STATES

    sessions = sorted(_store().list(), key=lambda s: (s.created_at, s.id), reverse=True)
    if states:
        wanted = set()
        for state in states:
            wanted |= set(ACTIVE_STATES if state == "active" else TERMINAL_STATES
                          if state == "terminal" else {state})
        sessions = [s for s in sessions if s.state in wanted]
    if provider:
        sessions = [s for s in sessions if s.provider == provider]
    return [s.to_summary() for s in sessions[:limit]]


@agents.command("list")
@click.option("-s", "--state", "states", multiple=True,
              help="Only this state (repeatable; also active, terminal).")
@click.option("-p", "--provider", default="", help="Only this provider.")
@click.option("-n", "--limit", type=click.IntRange(1, 200), default=20)
@click.option("--json", "as_json", is_flag=True, help="Print JSON.")
def list_command(states, provider, limit, as_json) -> None:
    """List agent sessions, newest first."""
    params: dict = {"limit": limit}
    if states:
        params["state"] = ",".join(states)
    if provider:
        params["provider"] = provider
    try:
        sessions = _request("GET", "/api/agents/sessions", params=params)["sessions"]
    except ServerDown:
        sessions = _offline_list(set(states) or None, provider, limit)
    except ApiError as exc:
        click.echo(f"error: {exc}", err=True)
        sys.exit(1)
    if as_json:
        _echo_json({"sessions": sessions})
        return
    if not sessions:
        click.echo("no agent sessions")
        return
    for s in sessions:
        click.echo(f"{s['id']}  {s['state']:<13} {s['provider']:<8} {s['created_at']}  "
                   f"{_one_line(s['title'])}")


def _session(session_id: str) -> dict:
    try:
        return _request("GET", f"/api/agents/sessions/{session_id}")["session"]
    except ServerDown:
        session = _store().get(session_id)
        if session is None:
            click.echo(f"error: no agent session {session_id}", err=True)
            sys.exit(1)
        return session.to_api()
    except ApiError as exc:
        click.echo(f"error: {exc}", err=True)
        sys.exit(1)


@agents.command("show")
@click.argument("session")
@click.option("--json", "as_json", is_flag=True, help="Print JSON.")
def show_command(session, as_json) -> None:
    """Show one session."""
    data = _session(_resolve(session))
    if as_json:
        _echo_json({"session": data})
        return
    for name in ("id", "state", "provider", "profile", "origin", "repo", "cwd", "title",
                 "parent_id", "external_id", "external_url", "created_at", "finished_at"):
        if data.get(name):
            click.echo(f"{name}: {_safe(data[name])}")
    if data.get("error"):
        click.echo(f"error: {data['error']['code']}: {_safe(data['error']['message'])}")
    if data.get("usage"):
        click.echo("usage: " + ", ".join(f"{k}={v}" for k, v in data["usage"].items()
                                         if v is not None))
    if data.get("result_text"):
        click.echo("")
        click.echo(_safe(data["result_text"]))


@agents.command("logs")
@click.argument("session")
@click.option("-f", "--follow", is_flag=True, help="Keep waiting for new events.")
@click.option("--after", type=click.IntRange(min=0), default=0, help="Only events after SEQ.")
@click.option("--json", "as_json", is_flag=True, help="One JSON event per line.")
def logs_command(session, follow, after, as_json) -> None:
    """Print a session's event log."""
    session_id = _resolve(session)
    if follow:
        _follow(session_id, as_json=as_json, after=after)
        return
    try:
        while True:
            page = _request("GET", f"/api/agents/sessions/{session_id}/events",
                            params={"after": after, "limit": 500})
            for event in page["events"]:
                _print_event(event, as_json=as_json)
            after = page["next_after"]
            if not page["has_more"]:
                return
    except ServerDown:
        for event in _store().events(session_id, after=after, limit=10**9):
            _print_event(event.to_dict(), as_json=as_json)
    except ApiError as exc:
        click.echo(f"error: {exc}", err=True)
        sys.exit(1)


@agents.command("send")
@click.argument("session")
@click.argument("message")
def send_command(session, message) -> None:
    """Send a follow-up MESSAGE to a running session."""
    data = _call("POST", f"/api/agents/sessions/{_resolve(session)}/messages",
                 body={"text": message})
    click.echo(f"sent to {data['session']['id']} ({data['session']['state']})")


def _active_ids() -> list[str]:
    """Active session ids, queued ones first: cancelling a running session frees a slot
    and would start the next queued session before `--all` reached it."""
    params = {"state": "active", "limit": 200}
    sessions = _call("GET", "/api/agents/sessions", params=params)["sessions"]
    return [s["id"] for s in sorted(sessions, key=lambda s: s.get("state") != "queued")]


@agents.command("cancel")
@click.argument("sessions", nargs=-1)
@click.option("--all", "all_", is_flag=True, help="Cancel every active session.")
@click.option("-y", "--yes", is_flag=True, help="Do not ask.")
def cancel_command(sessions, all_, yes) -> None:
    """Cancel sessions."""
    if all_ == bool(sessions):
        raise click.UsageError("give SESSION ids or --all")
    ids = _active_ids() if all_ else [_resolve(s) for s in sessions]
    if all_:
        if not ids:
            click.echo("no active sessions")
            return
        if not yes and not click.confirm(f"Cancel {len(ids)} active session(s)?"):
            return
    for session_id in ids:
        data = _call("POST", f"/api/agents/sessions/{session_id}/cancel")
        click.echo(f"{session_id}: {data['session']['state']}")


@agents.command("rm")
@click.argument("sessions", nargs=-1)
@click.option("--finished", is_flag=True, help="Remove every finished session.")
@click.option("-y", "--yes", is_flag=True, help="Do not ask.")
def rm_command(sessions, finished, yes) -> None:
    """Delete finished sessions and their logs."""
    if finished == bool(sessions):
        raise click.UsageError("give SESSION ids or --finished")
    if finished:
        ids = []
        before = None
        while True:
            params = {"state": "terminal", "limit": 200}
            if before:
                params["before"] = before
            page = _call("GET", "/api/agents/sessions", params=params)
            ids += [s["id"] for s in page["sessions"]]
            before = page.get("next_cursor")
            if not before:
                break
        if not ids:
            click.echo("no finished sessions")
            return
        if not yes and not click.confirm(f"Delete {len(ids)} finished session(s)?"):
            return
    else:
        ids = [_resolve(s) for s in sessions]
    for session_id in ids:
        _call("DELETE", f"/api/agents/sessions/{session_id}")
        click.echo(f"deleted {session_id}")


# -- rules ----------------------------------------------------------------------


@agents.group("rules")
def rules_group() -> None:
    """Spoken hot commands that launch an agent."""


def _offline_rules():
    from localflow.agents.store import HotCommandStore

    return HotCommandStore()


def _offline_known(provider_id: str) -> bool:
    from localflow.agents.registry import list_agent_ids

    return provider_id in list_agent_ids()


def _offline(fn):
    """Run a store edit locally (server down); map launcher errors to exit 1."""
    from localflow.agents.models import AgentError

    try:
        return fn()
    except AgentError as exc:
        click.echo(f"error: {exc.detail}", err=True)
        sys.exit(1)


@rules_group.command("list")
@click.option("--json", "as_json", is_flag=True, help="Print JSON.")
def rules_list(as_json) -> None:
    """List hot-command rules."""
    try:
        rules = _request("GET", "/api/agents/hot-commands")["rules"]
    except ServerDown:
        rules = _offline_rules().list()
    except ApiError as exc:
        click.echo(f"error: {exc}", err=True)
        sys.exit(1)
    if as_json:
        _echo_json({"rules": rules})
        return
    if not rules:
        click.echo("no hot commands")
        return
    for rule in rules:
        click.echo(f"{rule['id']}  {'on ' if rule['enabled'] else 'off'}  "
                   f"{rule['trigger']!r} -> {rule['provider']}"
                   f"{' in ' + rule['repo'] if rule.get('repo') else ''}")


def _rule_write(method: str, path: str, body: dict | None, offline):
    try:
        return _request(method, path, body=body)
    except ServerDown:
        return _offline(offline)
    except ApiError as exc:
        click.echo(f"error: {exc}", err=True)
        sys.exit(1)


@rules_group.command("add")
@click.argument("trigger")
@click.option("-p", "--provider", required=True, help="Provider id.")
@click.option("--profile", default="", help="HTTP agent profile id.")
@click.option("-r", "--repo", default="", help="Repo alias.")
@click.option("--template", default=None, help="Prompt template ({prompt} = spoken words).")
@click.option("--min-words", type=click.IntRange(0, 50), default=None)
@click.option("--disabled", is_flag=True, help="Create the rule turned off.")
def rules_add(trigger, provider, profile, repo, template, min_words, disabled) -> None:
    """Add a hot command spoken as TRIGGER."""
    from localflow.agents import rules as rule_ops
    from localflow.config import load_config

    body: dict = {"trigger": trigger, "provider": provider, "enabled": not disabled}
    if profile:
        body["profile"] = profile
    if repo:
        body["repo"] = repo
    if template is not None:
        body["prompt_template"] = template
    if min_words is not None:
        body["min_prompt_words"] = min_words
    rule = _rule_write("POST", "/api/agents/hot-commands", body, lambda: rule_ops.add_rule(
        _offline_rules(), body, load_config(), known_provider=_offline_known))
    click.echo(f"added {rule['id']}: {rule['trigger']!r}")


def _set_enabled(rule_id: str, enabled: bool) -> None:
    from localflow.agents import rules as rule_ops
    from localflow.config import load_config

    body = {"enabled": enabled}
    rule = _rule_write("PUT", f"/api/agents/hot-commands/{rule_id}", body,
                       lambda: rule_ops.update_rule(_offline_rules(), rule_id, body, load_config(),
                                                    known_provider=_offline_known))
    click.echo(f"{rule['id']}: {'enabled' if rule['enabled'] else 'disabled'}")


@rules_group.command("enable")
@click.argument("rule")
def rules_enable(rule) -> None:
    """Turn a rule on."""
    _set_enabled(rule, True)


@rules_group.command("disable")
@click.argument("rule")
def rules_disable(rule) -> None:
    """Turn a rule off."""
    _set_enabled(rule, False)


@rules_group.command("rm")
@click.argument("rule")
def rules_rm(rule) -> None:
    """Delete a rule."""
    from localflow.agents import rules as rule_ops

    _rule_write("DELETE", f"/api/agents/hot-commands/{rule}", None,
                lambda: rule_ops.delete_rule(_offline_rules(), rule))
    click.echo(f"deleted {rule}")


@rules_group.command("test")
@click.argument("utterance")
@click.option("--json", "as_json", is_flag=True, help="Print JSON.")
def rules_test(utterance, as_json) -> None:
    """Show what UTTERANCE would launch (never launches anything)."""
    result = _call("POST", "/api/agents/hot-commands/test", body={"text": utterance})
    if as_json:
        _echo_json(result)
    elif result["matched"]:
        click.echo(f"match {result['rule_id']}: {result['provider']}"
                   f"{' in ' + result['repo'] if result['repo'] else ''}: {result['prompt']}")
    else:
        click.echo(f"no match ({result['reason']})")


# -- HTTP profiles ------------------------------------------------------------------


@agents.group("http")
def http_group() -> None:
    """Generic HTTP agent profiles."""


@http_group.command("list")
@click.option("--json", "as_json", is_flag=True, help="Print JSON.")
def http_list(as_json) -> None:
    """List HTTP agent profiles."""
    profiles = _call("GET", "/api/agents/http-profiles")["profiles"]
    if as_json:
        _echo_json({"profiles": profiles})
        return
    if not profiles:
        click.echo("no HTTP agent profiles")
    for profile in profiles:
        click.echo(f"{profile['id']}  {profile['label']}  {profile['base_url']}"
                   f"{'' if profile['enabled'] else '  (off)'}")


@http_group.command("add")
@click.argument("file", type=click.Path(exists=True, dir_okay=False))
def http_add(file) -> None:
    """Add a profile from a JSON FILE."""
    try:
        body = json.loads(Path(file).read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise click.ClickException(f"cannot read {file}: {type(exc).__name__}") from None
    profile = _call("POST", "/api/agents/http-profiles", body=body)
    click.echo(f"added {profile['id']}: {profile['label']}")


@http_group.command("rm")
@click.argument("profile")
def http_rm(profile) -> None:
    """Delete a profile (and its token)."""
    _call("DELETE", f"/api/agents/http-profiles/{profile}")
    click.echo(f"deleted {profile}")


# -- secrets --------------------------------------------------------------------------


def _secret_name(provider: str, profile: str) -> str:
    from localflow.agents.providers.http import secret_name
    from localflow.agents.registry import agent_provider_class

    if provider == "http":
        if not profile:
            raise click.UsageError("--profile is required for http")
        return secret_name(profile)
    try:
        names = agent_provider_class(provider).secret_names_for(None)
    except KeyError as exc:
        raise click.ClickException(str(exc)) from None
    if len(names) != 1:
        raise click.ClickException(f"{provider} has no key to set")
    return names[0]


@agents.group("secret")
def secret_group() -> None:
    """Store or remove a provider's API key in the keyring."""


@secret_group.command("set")
@click.argument("provider")
@click.option("--profile", default="", help="HTTP agent profile id.")
def secret_set(provider, profile) -> None:
    """Prompt for PROVIDER's key (hidden) and store it in the keyring."""
    from localflow.secrets import SecretsUnavailable, set_secret

    name = _secret_name(provider, profile)
    value = click.prompt(f"{name}", hide_input=True)
    try:
        set_secret(name, value)
    except SecretsUnavailable:
        raise click.ClickException("the keyring is unavailable; the key was not saved") from None
    if provider == "http":
        from localflow.agents.providers.http import bind_secret
        from localflow.agents.store import HttpProfileStore

        bind_secret(HttpProfileStore(), profile)
    click.echo(f"saved {name}")


@secret_group.command("unset")
@click.argument("provider")
@click.option("--profile", default="", help="HTTP agent profile id.")
def secret_unset(provider, profile) -> None:
    """Remove PROVIDER's key from the keyring."""
    from localflow.secrets import SecretsUnavailable, delete_secret

    name = _secret_name(provider, profile)
    try:
        delete_secret(name)
    except SecretsUnavailable:
        raise click.ClickException("the keyring is unavailable or had no key") from None
    click.echo(f"removed {name}")
