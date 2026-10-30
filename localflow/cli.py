"""`lf` — command-line control for localflow.

Talks to the running localflow server over HTTP; `lf serve` / `lf dictate`
run the components themselves.
"""

import json
import sys
import time
from dataclasses import asdict, replace
from datetime import datetime
from pathlib import Path
from urllib.parse import quote

import click
import requests
from rich.console import Console
from rich.live import Live
from rich.panel import Panel
from rich.progress import BarColumn, Progress, TaskProgressColumn, TextColumn, TimeRemainingColumn
from rich.table import Table
from rich.text import Text

from localflow import api as api_logic
from localflow.apitoken import read as read_api_token
from localflow.config import (
    apply_config_update,
    config_field,
    config_toml,
    legacy_config_path,
    load_config,
    migrate_legacy,
    save_config,
    valid_config_keys,
)
from localflow.config import config_path as resolved_config_path
from localflow.dictionary import DictionaryError, DictionaryStore
from localflow.dispatch import (
    QUEUE_PATH,
    STATUSES,
    PromptQueue,
    StatusConflict,
    dispatch,
    orca_available,
)
from localflow.platform import current
from localflow.platform.base import PASTE_METHODS
from localflow.providers import registry
from localflow.providers.factory import build_llm
from localflow.secrets import SecretsUnavailable, delete_secret, get_secret, set_secret

console = Console()
_config = load_config()
BASE = f"http://127.0.0.1:{_config.server_port}"


def _get(path: str, timeout: float = 5) -> dict:
    try:
        resp = requests.get(BASE + path, timeout=timeout)
        resp.raise_for_status()
        return resp.json()
    except requests.ConnectionError:
        console.print("[red]server not running[/red] — start it with: [bold]lf serve[/bold]")
        sys.exit(1)


def _post(path: str, json: dict | None = None) -> dict:
    try:
        resp = requests.post(BASE + path, json=json, timeout=600)
    except requests.ConnectionError:
        console.print("[red]server not running[/red] — start it with: [bold]lf serve[/bold]")
        sys.exit(1)
    if not resp.ok:
        try:
            detail = resp.json().get("detail", resp.reason)
        except Exception:
            detail = resp.reason
        console.print(f"[red]error:[/red] {detail}")
        sys.exit(1)
    return resp.json()


@click.group()
def cli() -> None:
    """localflow: local dictation + meeting transcription to Obsidian."""


@cli.group()
def config() -> None:
    """Inspect and edit localflow configuration."""


@config.command("path")
def config_path_command() -> None:
    """Print the resolved v2 path and legacy-file status."""
    path = resolved_config_path()
    legacy = legacy_config_path()
    click.echo(f"path: {path}")
    click.echo(f"legacy: {'exists' if legacy.exists() else 'missing'} ({legacy})")


@config.command("show")
def config_show() -> None:
    """Print the v2 TOML that would be written for the current config."""
    click.echo(config_toml(load_config()), nl=False)


@config.command("get")
@click.argument("section_key")
def config_get(section_key: str) -> None:
    """Print SECTION.KEY."""
    try:
        _section, field_name = config_field(section_key)
    except KeyError:
        raise click.ClickException(
            f"unknown config key {section_key!r}; valid keys: "
            + ", ".join(valid_config_keys())
        )
    value = getattr(load_config(), field_name)
    if isinstance(value, (list, bool)):
        output = json.dumps(value)
    else:
        output = "" if value is None else str(value)
    click.echo(output)


def _coerce_config_value(current: object, raw: str) -> object:
    if isinstance(current, bool):
        if raw.lower() not in ("true", "false"):
            raise click.ClickException("boolean values must be true or false")
        return raw.lower() == "true"
    if isinstance(current, int) and not isinstance(current, bool):
        try:
            return int(raw)
        except ValueError as exc:
            raise click.ClickException(f"invalid integer: {raw!r}") from exc
    if isinstance(current, float):
        try:
            return float(raw)
        except ValueError as exc:
            raise click.ClickException(f"invalid float: {raw!r}") from exc
    if isinstance(current, list):
        return [item.strip() for item in raw.split(",") if item.strip()]
    if current is None:
        return raw or None
    return raw


@config.command("set")
@click.argument("section_key")
@click.argument("raw_value")
def config_set(section_key: str, raw_value: str) -> None:
    """Set SECTION.KEY to VALUE and write the v2 config."""
    try:
        section, field_name = config_field(section_key)
    except KeyError:
        raise click.ClickException(
            f"unknown config key {section_key!r}; valid keys: "
            + ", ".join(valid_config_keys())
        )
    if section_key == "paste.method" and raw_value not in PASTE_METHODS:
        raise click.ClickException(
            f"invalid paste method {raw_value!r}; valid methods: "
            + ", ".join(PASTE_METHODS)
        )
    current = load_config()
    key = section_key.split(".", 1)[1]
    value = _coerce_config_value(getattr(current, field_name), raw_value)
    updated = apply_config_update(current, {section: {key: value}})
    path = save_config(updated)
    click.echo(f"saved {path}")


@config.command("migrate")
def config_migrate_command() -> None:
    """Force migration of the legacy flat config file."""
    migrated = migrate_legacy()
    if migrated is None:
        click.echo("no legacy config found")
    else:
        click.echo(f"migrated legacy config to {resolved_config_path()}")


@cli.group("dict")
def dictionary() -> None:
    """Manage personal vocabulary and transcription corrections."""


@dictionary.command("list")
def dictionary_list() -> None:
    entries = DictionaryStore().list()
    if not entries["words"] and not entries["replacements"]:
        click.echo("No dictionary entries.")
        return
    for word in entries["words"]:
        click.echo(f"word: {word}")
    for pair in entries["replacements"]:
        click.echo(f"{pair['from']} -> {pair['to']}")


@dictionary.command("add")
@click.argument("word", required=False)
@click.option("--from", "source")
@click.option("--to", "target")
def dictionary_add(word: str | None, source: str | None, target: str | None) -> None:
    if word is not None and (source is not None or target is not None):
        raise click.UsageError("use a word or --from/--to, not both")
    store = DictionaryStore()
    try:
        if word is not None and source is None and target is None:
            added = store.add_word(word)
            label = f"word: {word.strip()}"
        elif word is None and source is not None and target is not None:
            added = store.add_replacement(source, target)
            label = f"{source.strip()} -> {target.strip()}"
        else:
            raise click.UsageError("provide a word or both --from and --to")
    except DictionaryError as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(f"{'added' if added else 'already exists'}: {label}")


@dictionary.command("remove")
@click.argument("word_or_from")
def dictionary_remove(word_or_from: str) -> None:
    if not DictionaryStore().remove(word_or_from):
        raise click.ClickException(f"dictionary entry not found: {word_or_from}")
    click.echo(f"removed: {word_or_from}")


@dictionary.command("import")
@click.argument(
    "file_path",
    type=click.Path(exists=True, dir_okay=False, path_type=Path),
)
def dictionary_import(file_path: Path) -> None:
    try:
        counts = DictionaryStore().import_file(file_path)
    except (DictionaryError, OSError) as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(
        "added {added}; skipped_duplicate {skipped_duplicate}; "
        "skipped_invalid {skipped_invalid}".format(**counts)
    )


@cli.group()
def secret() -> None:
    """Manage provider API-key secrets."""


@secret.command("set")
@click.argument("name")
def secret_set(name: str) -> None:
    """Prompt for and store NAME in the keyring."""
    value = click.prompt("secret", hide_input=True)
    try:
        set_secret(name, value)
    except SecretsUnavailable as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(f"stored {name}")


@secret.command("unset")
@click.argument("name")
def secret_unset(name: str) -> None:
    """Delete NAME from the keyring."""
    try:
        delete_secret(name)
    except SecretsUnavailable as exc:
        raise click.ClickException(str(exc)) from exc
    click.echo(f"removed {name}")


@secret.command("status")
def secret_status() -> None:
    """Show whether each supported secret is configured."""
    for name in ("llm_api_key", "fireworks_api_key"):
        state = "configured" if get_secret(name) else "not configured"
        click.echo(f"{name}: {state}")


def _flatten_status(value: dict, prefix: str = ""):
    for key, item in value.items():
        name = f"{prefix}.{key}" if prefix else key
        if isinstance(item, dict):
            yield from _flatten_status(item, name)
        else:
            yield name, item


@cli.command()
@click.option("--json", "json_output", is_flag=True, help="Print JSON.")
def status(json_output: bool) -> None:
    """Show localflow status."""
    result = api_logic.status(load_config(), current(), False)
    try:
        result["meeting"] = _get("/meeting/status", timeout=2)
    except (SystemExit, requests.RequestException, ValueError):
        result["meeting"] = {"server": "down"}
    if json_output:
        click.echo(json.dumps(result))
        return
    for key, value in _flatten_status(result):
        click.echo(f"{key}: {value}")


@cli.group()
def stt() -> None:
    """Inspect and select speech-to-text providers."""


@stt.command("list")
@click.option("--json", "json_output", is_flag=True, help="Print JSON.")
def stt_list(json_output: bool) -> None:
    """List STT providers and models."""
    config = load_config()
    providers = api_logic.stt_providers()
    error = None
    selected = api_logic.resolve_stt_provider(config.stt_backend)
    try:
        models = [
            asdict(model)
            for model in registry.stt_provider_class(selected)().list_models()
        ]
    except Exception as exc:  # noqa: BLE001
        models = []
        error = str(exc)
    result = {"providers": providers, "models": models, "error": error}
    if json_output:
        click.echo(json.dumps(result))
        return
    for provider in providers:
        state = "available" if provider["available"] else "unavailable"
        click.echo(f"{provider['id']}: {state}")
    for model in models:
        click.echo(f"{model['id']}: {model.get('label', '')}")
    if error:
        console.print(f"[red]error:[/red] {error}")


@stt.command("use")
@click.argument("provider")
@click.argument("model", required=False)
@click.option("--device", type=click.Choice(("auto", "cpu", "cuda")), default="auto")
def stt_use(provider: str, model: str | None, device: str) -> None:
    """Select an STT provider, optional MODEL, and DEVICE."""
    selected = api_logic.resolve_stt_provider(provider)
    try:
        registry.stt_provider_class(selected)
    except KeyError as exc:
        raise click.ClickException(str(exc)) from exc
    config = load_config()
    provider_id = "mlx-whisper" if provider == "mlx" else provider
    updated = apply_config_update(
        config,
        {
            "stt": {
                "provider": provider_id,
                "model": model or config.model_size,
                "device": device,
            }
        },
    )
    click.echo(f"saved {save_config(updated)}")


@stt.command("download")
@click.argument("model_id")
def stt_download(model_id: str) -> None:
    """Download a catalog speech model without selecting it."""
    _download_stt_model(model_id)


@cli.group()
def llm() -> None:
    """Inspect and select language-model providers."""


@llm.command("list")
@click.option("--provider", default=None)
@click.option("--base-url", default=None)
@click.option("--json", "json_output", is_flag=True, help="Print JSON.")
def llm_list(
    provider: str | None, base_url: str | None, json_output: bool
) -> None:
    """List models from an LLM provider."""
    config = load_config()
    selected = provider or config.llm_provider
    selected_config = replace(
        config,
        llm_provider=selected,
        llm_base_url=base_url or config.llm_base_url,
    )
    try:
        models = [
            asdict(model) for model in build_llm(selected_config, timeout=10).list_models()
        ]
    except Exception as exc:  # noqa: BLE001
        if json_output:
            click.echo(json.dumps({"models": [], "error": str(exc)}))
        else:
            console.print(f"[red]error:[/red] {exc}")
        raise click.exceptions.Exit(1)
    result = {"models": models, "error": None}
    if json_output:
        click.echo(json.dumps(result))
    else:
        for model in models:
            click.echo(f"{model['id']}: {model.get('label', '')}")


@llm.command("use")
@click.argument("provider")
@click.option("--base-url", default=None)
@click.option("--model", "model_name", default=None)
def llm_use(provider: str, base_url: str | None, model_name: str | None) -> None:
    """Select an LLM provider and optional endpoint or model."""
    if provider not in registry.list_llm_ids():
        raise click.ClickException(f"unknown LLM provider: {provider}")
    config = load_config()
    values = {"provider": provider}
    if base_url is not None:
        values["base_url"] = base_url
    if model_name is not None:
        values["model"] = model_name
    updated = apply_config_update(config, {"llm": values})
    click.echo(f"saved {save_config(updated)}")


@llm.command("test")
@click.option("--provider", default=None)
@click.option("--base-url", default=None)
@click.option("--model", "model_name", default=None)
def llm_test(
    provider: str | None, base_url: str | None, model_name: str | None
) -> None:
    """Test connectivity to an LLM provider."""
    result = api_logic.test_llm(load_config(), provider, base_url, model_name)
    if result["ok"]:
        console.print(f"[green]ok[/green] {result['detail'] or 'reachable'}")
        return
    console.print(f"[red]{result['detail']}[/red]")
    raise click.exceptions.Exit(1)


@llm.command("pull")
@click.argument("tag")
def llm_pull(tag: str) -> None:
    """Pull an Ollama model tag."""
    _pull_ollama_model(tag)


@cli.group()
def meeting() -> None:
    """Control meeting transcription sessions."""


@meeting.command("start")
@click.option("--title", "-t", default="Meeting", help="Note title.")
@click.option("--category", "-c", default="", help="Zoom/Teams/... (default: auto-detected).")
def meeting_start(title: str, category: str) -> None:
    """Start transcribing a meeting."""
    out = _post("/meeting/start", {"title": title, "category": category})
    console.print(f"[green]●[/green] transcribing — category [bold]{out['category']}[/bold]. "
                  f"Stop with: [bold]lf meeting stop[/bold]")


@meeting.command("stop")
def meeting_stop() -> None:
    """Stop, summarize, and write notes to the vault."""
    with console.status("summarizing and writing notes…"):
        out = _post("/meeting/stop")
    console.print(f"[green]saved[/green] ({out['segments']} segments)")
    console.print(f"  notes: {out['notes_path']}")
    console.print(f"  log:   {out['log_path']}")


@meeting.command("dismiss")
def meeting_dismiss() -> None:
    """Dismiss the current meeting-detected banner."""
    _post("/meeting/dismiss")
    console.print("dismissed")


def _render(st: dict) -> Panel:
    body = Text()
    if st["detected"] and not st.get("session"):
        body.append(f"⚑ meeting detected ({st['platform'] or '?'}) — lf meeting start\n\n",
                    style="bold yellow")
    session = st.get("session")
    if session:
        body.append(f"● {session['title']}", style="bold red")
        body.append(f"  {session['seconds'] // 60}m{session['seconds'] % 60:02d}s · "
                    f"{session['segment_count']} segments\n\n", style="dim")
        for seg in session["segments"]:
            body.append(f"[{seg['stamp']}] ", style="cyan")
            body.append(seg["text"] + "\n")
        if not session["segments"]:
            body.append("(listening — first segment lands after ~30s)\n", style="dim")
    else:
        mic = "mic in use" if st["mic_busy"] else "idle"
        body.append(f"no session · {mic}\n", style="dim")
        if st.get("last_saved"):
            body.append(f"last saved: {st['last_saved']['notes_path']}\n", style="green")
    state = "watching" if st["watching"] else "watcher off"
    return Panel(body, title=f"localflow — {state}", border_style="grey50")


@cli.command()
def watch() -> None:
    """Live dashboard: detection state and rolling transcript. Ctrl+C exits."""
    with Live(_render(_get("/meeting/status")), refresh_per_second=1, console=console) as live:
        while True:
            time.sleep(3)
            try:
                live.update(_render(_get("/meeting/status")))
            except SystemExit:
                raise
            except Exception:
                pass


@cli.command()
@click.option("--limit", "-n", default=10, help="How many recent notes to list.")
def notes(limit: int) -> None:
    """List recent meeting notes in the vault."""
    root = Path(_config.vault_path).expanduser() / _config.notes_folder
    if not root.exists():
        console.print(f"[dim]no notes yet ({root})[/dim]")
        return
    files = sorted(root.rglob("*.md"), key=lambda p: p.stat().st_mtime, reverse=True)[:limit]
    if not files:
        console.print(f"[dim]no notes yet ({root})[/dim]")
        return
    table = Table(box=None, header_style="dim")
    table.add_column("modified")
    table.add_column("category")
    table.add_column("note")
    for f in files:
        mtime = datetime.fromtimestamp(f.stat().st_mtime).strftime("%m-%d %H:%M")
        table.add_row(mtime, f.parent.name, str(f))
    console.print(table)


@cli.command("open")
@click.argument("query", nargs=-1, required=True)
def open_note(query: tuple[str, ...]) -> None:
    """Open the newest note matching QUERY words in Obsidian/default app."""
    root = Path(_config.vault_path).expanduser() / _config.notes_folder
    words = [w.lower() for w in query]
    matches = [
        f for f in root.rglob("*.md")
        if all(w in f.name.lower() for w in words)
    ]
    if not matches:
        console.print("[red]no match[/red]")
        sys.exit(1)
    newest = max(matches, key=lambda p: p.stat().st_mtime)
    current().open_path(str(newest))
    console.print(f"opened {newest.name}")


_STATUS_STYLE = {
    "pending": "yellow",
    "approved": "cyan",
    "dispatched": "green",
    "rejected": "dim",
    "failed": "red",
}


def _status(status: str) -> str:
    style = _STATUS_STYLE.get(status, "white")
    return f"[{style}]{status}[/{style}]"


def _resolve(queue: PromptQueue, prompt_id: str) -> dict:
    """Queue entry for PROMPT_ID, or exit 1 with a message."""
    entry = queue.get(prompt_id)
    if entry is None:
        console.print(f"[red]unknown prompt[/red] {prompt_id} — list them with: [bold]lf prompts list[/bold]")
        sys.exit(1)
    return entry


@cli.group()
def prompts() -> None:
    """Review work prompts from meetings and dispatch them to Orca."""


@prompts.command("list")
@click.option("--status", "-s", default=None, type=click.Choice(STATUSES),
              help="Only show prompts with this status.")
def prompts_list(status: str | None) -> None:
    """List queued work prompts."""
    entries = PromptQueue().list(status=status)
    if not entries:
        what = f"no {status} prompts" if status else "no prompts queued"
        console.print(f"[dim]{what} ({QUEUE_PATH})[/dim]")
        return
    table = Table(box=None, header_style="dim")
    table.add_column("id")
    table.add_column("status")
    table.add_column("title")
    table.add_column("meeting")
    table.add_column("created")
    for e in entries:
        table.add_row(
            e["id"],
            _status(e["status"]),
            e["title"],
            e.get("meeting") or "",
            e.get("created") or "",
        )
    console.print(table)


@prompts.command("show")
@click.argument("prompt_id")
def prompts_show(prompt_id: str) -> None:
    """Print the full text of prompt PROMPT_ID."""
    entry = _resolve(PromptQueue(), prompt_id)
    path = Path(entry["path"]).expanduser()
    if not path.exists():
        console.print(f"[red]prompt file missing[/red] {path}")
        sys.exit(1)
    console.print(f"[dim]{path}[/dim]")
    if entry["status"] == "dispatched":
        console.print(f"[green]dispatched[/green] to worktree [bold]{entry.get('worktree') or '?'}[/bold]")
        handle = entry.get("handle") or ""
        if handle:
            console.print(f"  follow it with: [bold]orca terminal read --terminal {handle} --json[/bold]")
        else:
            console.print("  [dim]no terminal handle recorded[/dim]")
    console.print()
    console.print(path.read_text())


@prompts.command("approve")
@click.argument("ids", nargs=-1, required=True)
def prompts_approve(ids: tuple[str, ...]) -> None:
    """Approve prompts IDS for dispatch (retries a failed one, clearing its error)."""
    _set_reviewed(ids, "approved", ("pending", "failed"))


@prompts.command("reject")
@click.argument("ids", nargs=-1, required=True)
def prompts_reject(ids: tuple[str, ...]) -> None:
    """Reject prompts IDS so they are never dispatched."""
    _set_reviewed(ids, "rejected", ("pending",))


def _set_reviewed(ids: tuple[str, ...], status: str, from_statuses: tuple[str, ...]) -> None:
    """Move prompts in FROM_STATUSES to STATUS; anything else is refused.

    Re-approving a failed prompt clears the residue of the failed attempt so it
    dispatches clean.
    """
    queue = PromptQueue()
    approvable = " or ".join(from_statuses)
    refused = False
    for prompt_id in ids:
        entry = _resolve(queue, prompt_id)
        if entry["status"] not in from_statuses:
            console.print(f"[red]{prompt_id}[/red] is {_status(entry['status'])} — "
                          f"only {approvable} prompts can be {status}; skipped")
            refused = True
            continue
        # Compare-and-set on the status we just read: the server can approve or
        # dispatch the same entry between the read above and this write.
        clear = {"error": "", "worktree": "", "handle": ""} if entry["status"] == "failed" else {}
        try:
            queue.set_status(prompt_id, status, expect=from_statuses, **clear)
        except StatusConflict as conflict:
            console.print(f"[red]{prompt_id}[/red] changed to {_status(conflict.current)} "
                          f"while it was being {status} — not touched")
            refused = True
            continue
        console.print(f"{_status(status)} {prompt_id}  {entry['title']}")
    if refused:
        sys.exit(1)


@prompts.command("dispatch")
@click.argument("ids", nargs=-1)
@click.option("--all", "all_", is_flag=True, help="Dispatch every approved prompt.")
@click.option("--repo", default="", help="Orca repo selector (default: config.orca_repo).")
@click.option("--agent", default="", help="Agent to run (default: config.orca_agent).")
@click.option("--dry-run", is_flag=True, help="Show what would be dispatched; change nothing.")
def prompts_dispatch(ids: tuple[str, ...], all_: bool, repo: str, agent: str, dry_run: bool) -> None:
    """Dispatch approved prompts IDS (or --all) to Orca worktrees."""
    target_repo = repo or _config.orca_repo
    if not target_repo:
        console.print("[red]no repo configured[/red] — set [bold]orca_repo[/bold] in ~/.localflow.toml "
                      "or pass [bold]--repo[/bold]")
        console.print("  find the selector with: [bold]orca repo list --json[/bold]")
        sys.exit(1)
    target_agent = agent or _config.orca_agent

    queue = PromptQueue()
    if all_ and ids:
        console.print("[red]pass ids or --all, not both[/red]")
        sys.exit(1)
    if all_:
        entries = queue.list(status="approved")
    elif ids:
        entries = [_resolve(queue, i) for i in ids]
        unapproved = [e for e in entries if e["status"] != "approved"]
        if unapproved:
            for e in unapproved:
                console.print(f"[red]{e['id']}[/red] is {_status(e['status'])}, not approved — "
                              f"approve it with: [bold]lf prompts approve {e['id']}[/bold]")
            sys.exit(1)
    else:
        console.print("[red]nothing selected[/red] — pass ids or [bold]--all[/bold]")
        sys.exit(1)

    if not entries:
        console.print("[dim]no approved prompts to dispatch[/dim]")
        return

    if dry_run:
        table = Table(box=None, header_style="dim")
        table.add_column("id")
        table.add_column("title")
        table.add_column("repo")
        table.add_column("agent")
        for e in entries:
            table.add_row(e["id"], e["title"], target_repo, target_agent)
        console.print(table)
        console.print(f"[dim]dry run — {len(entries)} prompt(s) would be dispatched[/dim]")
        return

    ready, reason = orca_available()
    if not ready:
        console.print(f"[red]orca unavailable:[/red] {reason}")
        console.print("  start the app with: [bold]orca open[/bold], then retry — prompts stay approved")
        sys.exit(1)

    dispatched = 0
    conflicts = 0
    for e in entries:
        result = dispatch(e, target_repo, target_agent)
        # Each write compare-and-sets on "approved": if the server dispatched this
        # entry while orca was working, we must not overwrite its record.
        if result.get("ok"):
            worktree = result.get("worktree")
            try:
                queue.set_status(e["id"], "dispatched", expect=("approved",), worktree=worktree,
                                 handle=result.get("handle", ""), repo=target_repo)
            except StatusConflict as conflict:
                console.print(f"[yellow]dispatched but not recorded[/yellow] {e['id']}  {e['title']} → "
                              f"{worktree} (queue moved to {_status(conflict.current)} meanwhile)")
                conflicts += 1
                continue
            console.print(f"[green]dispatched[/green] {e['id']}  {e['title']} → {worktree}")
            dispatched += 1
        else:
            error = result.get("error", "unknown error")
            try:
                queue.set_status(e["id"], "failed", expect=("approved",), error=error)
            except StatusConflict as conflict:
                console.print(f"[red]failed[/red] {e['id']}  {e['title']}: {error} "
                              f"(not recorded — queue moved to {_status(conflict.current)})")
                conflicts += 1
                continue
            console.print(f"[red]failed[/red] {e['id']}  {e['title']}: {error}")
    failures = len(entries) - dispatched - conflicts
    summary = f"\n{dispatched} dispatched, {failures} failed"
    if conflicts:
        summary += f", {conflicts} raced (see above)"
    console.print(summary)
    if failures or conflicts:
        sys.exit(1)


@cli.command()
def serve() -> None:
    """Run the localflow server (UI at /, meeting API, transcription)."""
    from localflow.server import main as server_main
    server_main()


@cli.command()
def dictate() -> None:
    """Run push-to-talk dictation (same as the `localflow` command)."""
    from localflow.app import main as app_main
    app_main()


@cli.command()
def ui() -> None:
    """Open the web UI in the default browser."""
    token = read_api_token() or ""
    current().open_path(f"{BASE}/settings?token={quote(token)}")


@cli.group(invoke_without_command=True)
def phrases() -> None:
    """Manage hot phrases in the native window (no subcommand = open it)."""
    ctx = click.get_current_context()
    if ctx.invoked_subcommand is not None:
        return
    try:
        from localflow.phrases_window import main as window_main

        window_main()
    except ImportError as exc:
        if "tkinter" not in str(exc) and "_tkinter" not in str(exc):
            raise
        minor = sys.version_info[:2]
        console.print("[red]tkinter is not available[/red] in this Python — install it:")
        if sys.platform == "darwin":
            console.print(f"  brew install python-tk@{minor[0]}.{minor[1]}")
        else:
            console.print("  sudo apt install python3-tk")
        sys.exit(1)


@phrases.command("install-app")
def phrases_install_app() -> None:
    """Install a .app bundle so the window opens from Spotlight/Launchpad."""
    from localflow.appbundle import install_app
    path = install_app()
    console.print(f"[green]installed[/green] {path}")
    console.print("open it from Spotlight/Launchpad or `open " + str(path) + "`")


@phrases.command("uninstall-app")
def phrases_uninstall_app() -> None:
    """Remove the hot phrases .app bundle."""
    from localflow.appbundle import uninstall_app
    if uninstall_app():
        console.print("[green]uninstalled[/green]")
    else:
        console.print("[dim]not installed[/dim]")


def _stt_model(model_id: str):
    from localflow.models import STT_MODELS

    return next((model for model in STT_MODELS if model.id == model_id), None)


def _progress(title: str, operation):
    with Progress(
        TextColumn("{task.description}"),
        BarColumn(),
        TaskProgressColumn(),
        TextColumn("{task.fields[detail]}"),
        TimeRemainingColumn(),
        console=console,
    ) as progress:
        task = progress.add_task(title, total=100, detail="")

        def update(done, total, detail=""):
            if total:
                progress.update(task, completed=done, total=total, detail=detail)
            else:
                progress.update(task, detail=detail)

        return operation(update)


def _download_stt_cli(model) -> None:
    from localflow.models import download_stt, friendly_download_error

    try:
        _progress(
            f"Downloading {model.id}",
            lambda update: download_stt(
                model,
                lambda done, total: update(
                    done, total, f"{done / 1_000_000:.0f}/{total / 1_000_000:.0f} MB"
                ),
            ),
        )
    except Exception as exc:
        raise click.ClickException(friendly_download_error(exc)) from exc
    console.print(f"[green]downloaded[/green] {model.id}")


def _require_stt_runtime(model) -> None:
    from localflow.models import runtime_available

    extra = {"mlx-whisper": "mlx", "parakeet": "parakeet"}.get(model.backend)
    if extra and not runtime_available(model.backend):
        raise click.ClickException(
            f"install the runtime: pip install -e '.[{extra}]'"
        )


def _pull_llm_cli(tag: str, client) -> None:
    from localflow.models import friendly_download_error

    try:
        _progress(
            f"Downloading {tag}",
            lambda update: client.pull(
                tag, lambda done, total, status: update(done, total, status)
            ),
        )
    except Exception as exc:
        raise click.ClickException(friendly_download_error(exc)) from exc
    console.print(f"[green]downloaded[/green] {tag}")


def _download_stt_model(model_id: str):
    from localflow.models import detect_hardware

    model = _stt_model(model_id)
    if model is None:
        raise click.ClickException(f"unknown speech model: {model_id}")
    _require_stt_runtime(model)
    if model.apple_silicon_only and not detect_hardware().apple_silicon:
        raise click.ClickException(f"{model_id} is only available on Apple Silicon.")
    _download_stt_cli(model)
    return model


def _pull_ollama_model(tag: str, config=None) -> None:
    from localflow.models import DownloadError, OllamaClient, friendly_download_error

    config = config or load_config()
    if config.llm_provider != "ollama":
        raise click.ClickException(
            "model downloads are only available for the Ollama provider; change in Settings"
        )
    client = OllamaClient(config.llm_base_url)
    if not client.is_running():
        raise click.ClickException("Ollama is not running; start it with `ollama serve`.")
    try:
        _pull_llm_cli(tag, client)
    except DownloadError as exc:
        raise click.ClickException(friendly_download_error(exc)) from exc


def _use_stt_model(model_id: str, no_download: bool = False) -> Path:
    from localflow.models import detect_hardware, is_stt_downloaded, stt_config

    model = _stt_model(model_id)
    if model is None:
        raise click.ClickException(f"unknown speech model: {model_id}")
    _require_stt_runtime(model)
    if model.apple_silicon_only and not detect_hardware().apple_silicon:
        raise click.ClickException(f"{model_id} is only available on Apple Silicon.")
    if not no_download and not is_stt_downloaded(model):
        _download_stt_cli(model)
    updated = apply_config_update(load_config(), stt_config(model))
    path = save_config(updated)
    console.print(f"[green]selected[/green] {model.id}  ·  {path}")
    return path


def _use_ollama_model(model_id: str, no_download: bool = False) -> Path:
    from localflow.models import (
        LLM_MODELS,
        DownloadError,
        OllamaClient,
        friendly_download_error,
    )

    if model_id not in {entry.id for entry in LLM_MODELS}:
        raise click.ClickException(f"unknown model: {model_id}")
    config = load_config()
    if config.llm_provider != "ollama":
        raise click.ClickException(
            "model selection is only available for the Ollama provider; change in Settings"
        )
    if not no_download:
        client = OllamaClient(config.llm_base_url)
        if not client.is_running():
            raise click.ClickException("Ollama is not running; start it with `ollama serve`.")
        if model_id not in client.installed():
            try:
                _pull_llm_cli(model_id, client)
            except DownloadError as exc:
                raise click.ClickException(friendly_download_error(exc)) from exc
    updated = apply_config_update(
        config, {"llm": {"model": model_id, "cleanup_enabled": True}}
    )
    path = save_config(updated)
    console.print(f"[green]selected[/green] {model_id}  ·  {path}")
    return path


def _setup_cli(
    stt_id: str | None,
    llm_id: str | None,
    yes: bool,
    cleanup_level: str | None = None,
) -> None:
    from localflow.audio import list_input_devices
    from localflow.models import (
        LLM_MODELS,
        DownloadError,
        OllamaClient,
        available_stt_models,
        detect_hardware,
        is_stt_downloaded,
    )
    from localflow.setup_flow import (
        HOTKEY_OPTIONS,
        SetupChoices,
        apply,
        initial_choices,
        run_checks,
        try_dictation,
    )

    cfg = load_config()
    hardware = detect_hardware()
    defaults = initial_choices(cfg, hardware)
    available_models = available_stt_models(hardware)
    if yes:
        chosen_stt = stt_id or defaults.stt_id
        chosen_llm = (defaults.llm_id if llm_id is None else
                      None if llm_id.lower() == "none" else llm_id)
        if cfg.llm_provider != "ollama" and chosen_llm:
            raise click.ClickException(
                "LLM downloads are only available for Ollama; change the provider in Settings"
            )
        model = next((entry for entry in available_models if entry.id == chosen_stt), None)
        if model is None:
            raise click.ClickException(f"speech model is unavailable on this hardware: {chosen_stt}")
        if not is_stt_downloaded(model):
            _download_stt_cli(model)
        if chosen_llm and cfg.llm_provider == "ollama":
            client = OllamaClient(cfg.llm_base_url)
            if not client.is_running():
                raise click.ClickException("Ollama is not running; start it with `ollama serve`.")
            try:
                installed = client.installed()
            except DownloadError as exc:
                raise click.ClickException(str(exc)) from exc
            if chosen_llm not in installed:
                _pull_llm_cli(chosen_llm, client)
        choices = SetupChoices(
            chosen_stt,
            chosen_llm,
            defaults.input_device,
            defaults.hotkey,
            defaults.language,
            defaults.start_on_login,
            cfg.llm_provider,
            cleanup_level or defaults.cleanup_level,
        )
        path = apply(choices)
        console.print(f"[green]setup saved[/green] {path}")
        return

    console.print(f"[bold]Hardware:[/bold] {hardware.ram_gb:.0f} GB RAM · {hardware.free_disk_gb:.0f} GB free")
    console.print("[bold]Checks[/bold]")
    for check in run_checks(cfg):
        badge = "✓" if check.ok else ("?" if check.ok is None else "✗")
        console.print(f"  {badge} {check.label}: {check.detail}")
    stt_table = Table(title="Speech models", box=None)
    stt_table.add_column("id")
    stt_table.add_column("size")
    for model in available_models:
        stt_table.add_row(model.id, f"{model.size_mb:,} MB")
    console.print(stt_table)
    default_stt = stt_id if stt_id is not None else defaults.stt_id
    chosen_stt = click.prompt("Speech model", default=default_stt)
    model = next((entry for entry in available_models if entry.id == chosen_stt), None)
    if model is None:
        raise click.ClickException(f"speech model is unavailable on this hardware: {chosen_stt}")
    if not is_stt_downloaded(model) and click.confirm(f"Download {chosen_stt} now?", default=True):
        _download_stt_cli(model)

    if cfg.llm_provider == "ollama":
        llm_table = Table(title="Cleanup models (or none)", box=None)
        llm_table.add_column("id")
        llm_table.add_column("size")
        for llm_model in LLM_MODELS:
            llm_table.add_row(llm_model.id, f"{llm_model.size_mb:,} MB")
        llm_table.add_row("none", "raw transcript")
        console.print(llm_table)
        default_llm = (llm_id if llm_id is not None else defaults.llm_id) or "none"
        chosen_llm = click.prompt("Cleanup model", default=default_llm)
        if chosen_llm.lower() == "none":
            chosen_llm = None
        elif chosen_llm not in {item.id for item in LLM_MODELS}:
            raise click.ClickException(f"unknown cleanup model: {chosen_llm}")
        if chosen_llm:
            client = OllamaClient(cfg.llm_base_url)
            if not client.is_running():
                console.print("Ollama is not running; continue without cleanup or start `ollama serve` and rerun setup.")
                chosen_llm = None
            elif chosen_llm not in client.installed() and click.confirm(f"Download {chosen_llm} now?", default=True):
                _pull_llm_cli(chosen_llm, client)
            elif chosen_llm not in client.installed():
                chosen_llm = None
    else:
        console.print(
            f"LLM provider: {cfg.llm_provider}. Change provider or model in Settings."
        )
        chosen_llm = None

    chosen_cleanup_level = click.prompt(
        "Cleanup level",
        default=cleanup_level or defaults.cleanup_level,
        type=click.Choice(["light", "medium"]),
    )
    devices = ["System default", *list_input_devices()]
    device_default = defaults.input_device if defaults.input_device in devices else "System default"
    device = click.prompt("Microphone", default=device_default, type=click.Choice(devices))
    hotkey = click.prompt(
        "Hotkey",
        default=defaults.hotkey,
        type=click.Choice([value for value, _label in HOTKEY_OPTIONS]),
    )
    language = click.prompt("Language (blank = autodetect)", default=defaults.language or "")
    if click.confirm("Try a five-second dictation now?", default=False):
        from localflow.stt import Transcriber

        transcriber = Transcriber(
            model_size=model.model_size,
            language=language or None,
            backend=model.backend,
        )
        transcript = try_dictation(
            5, "" if device == "System default" else device, transcriber
        )
        console.print(Panel(transcript or "No speech detected.", title="Practice transcript"))
    start_on_login = (
        click.confirm("Start LocalFlow on login?", default=defaults.start_on_login)
        if hardware.system == "Darwin" else False
    )
    choices = SetupChoices(
        chosen_stt, chosen_llm, "" if device == "System default" else device,
        hotkey, language or None, start_on_login, cfg.llm_provider,
        chosen_cleanup_level,
    )
    path = apply(choices)
    console.print(f"[green]setup saved[/green] {path}")


@cli.group(invoke_without_command=True)
@click.option("--cli", "cli_mode", is_flag=True, help="Use the terminal wizard.")
@click.option("--yes", is_flag=True, help="Use recommendations without prompts.")
@click.option("--stt", "stt_id", default=None, help="Speech model id for --yes.")
@click.option("--llm", "llm_id", default=None, help="Cleanup model id or none for --yes.")
@click.option(
    "--cleanup-level",
    type=click.Choice(["light", "medium"]),
    default=None,
    help="Cleanup level (light or medium).",
)
@click.pass_context
def setup(
    ctx,
    cli_mode: bool,
    yes: bool,
    stt_id: str | None,
    llm_id: str | None,
    cleanup_level: str | None,
) -> None:
    """Open the setup window or configure LocalFlow in the terminal."""
    if ctx.invoked_subcommand is not None:
        return
    if yes and not cli_mode:
        raise click.UsageError("--yes requires --cli")
    if yes and stt_id and _stt_model(stt_id) is None:
        raise click.ClickException(f"unknown speech model: {stt_id}")
    if yes and llm_id and llm_id.lower() != "none":
        from localflow.models import LLM_MODELS

        if llm_id not in {model.id for model in LLM_MODELS}:
            raise click.ClickException(f"unknown cleanup model: {llm_id}")
    if cli_mode:
        _setup_cli(stt_id, llm_id, yes, cleanup_level)
        return
    from localflow.setup_window import main as window_main

    try:
        window_main()
    except ImportError as exc:
        if "tkinter" not in str(exc) and "_tkinter" not in str(exc):
            raise
        console.print("[red]tkinter is unavailable[/red] — install Python's Tk package and retry.")
        sys.exit(1)


@setup.command("install-app")
def setup_install_app() -> None:
    """Install the setup window as a LocalFlow Setup.app bundle."""
    from localflow.appbundle import install_app

    path = install_app(
        name="LocalFlow Setup.app",
        bundle_id="com.cortexrnd.localflow.setup",
        module="localflow.setup_window",
    )
    console.print(f"[green]installed[/green] {path}")


@setup.command("uninstall-app")
def setup_uninstall_app() -> None:
    """Remove the LocalFlow Setup.app bundle."""
    from localflow.appbundle import uninstall_app

    if uninstall_app(name="LocalFlow Setup.app"):
        console.print("[green]uninstalled[/green]")
    else:
        console.print("[dim]not installed[/dim]")


@cli.group()
def models() -> None:
    """List and manage local speech and cleanup models."""


@models.command("list")
@click.option("--kind", type=click.Choice(["stt", "llm"]), default=None)
def models_list(kind: str | None) -> None:
    from localflow.models import (
        LLM_MODELS,
        OllamaClient,
        available_stt_models,
        current_stt_id,
        detect_hardware,
        is_stt_downloaded,
        recommend,
    )

    cfg = load_config()
    hardware = detect_hardware()
    rec_stt, rec_llm = recommend(hardware)
    table = Table(box=None, header_style="bold")
    table.add_column("id")
    table.add_column("size")
    table.add_column("downloaded / installed")
    table.add_column("selected")
    table.add_column("recommended")
    if kind in (None, "stt"):
        for model in available_stt_models(hardware):
            table.add_row(
                model.id, f"{model.size_mb:,} MB",
                "downloaded" if is_stt_downloaded(model) else "not downloaded",
                "yes" if current_stt_id(cfg) == model.id else "",
                "yes" if rec_stt == model.id else "",
            )
    if kind in (None, "llm"):
        running = cfg.llm_provider == "ollama"
        installed = set()
        if running:
            client = OllamaClient(cfg.llm_base_url)
            running = client.is_running()
            installed = set(client.installed()) if running else set()
        for model in LLM_MODELS:
            table.add_row(
                model.id, f"{model.size_mb:,} MB",
                "managed by Settings" if cfg.llm_provider != "ollama" else
                "installed" if model.id in installed else
                "not installed" if running else "Ollama offline",
                "yes" if cfg.llm_provider == "ollama" and cfg.cleanup_enabled and cfg.llm_model == model.id else "",
                "yes" if rec_llm == model.id else "",
            )
    console.print(table)


@models.command("download")
@click.argument("model_id")
def models_download(model_id: str) -> None:
    model = _stt_model(model_id)
    if model is not None:
        _download_stt_model(model_id)
        return
    from localflow.models import LLM_MODELS

    if model_id not in {entry.id for entry in LLM_MODELS}:
        raise click.ClickException(f"unknown model: {model_id}")
    _pull_ollama_model(model_id)


@models.command("use")
@click.argument("model_id")
@click.option("--no-download", is_flag=True, help="Select the model without downloading it.")
def models_use(model_id: str, no_download: bool) -> None:
    model = _stt_model(model_id)
    if model is not None:
        _use_stt_model(model_id, no_download)
        return
    _use_ollama_model(model_id, no_download)


@cli.command()
def doctor() -> None:
    """Check LocalFlow's machine, audio, and Ollama prerequisites."""
    from localflow.setup_flow import run_checks

    table = Table(box=None)
    table.add_column("status")
    table.add_column("check")
    table.add_column("detail")
    for check in run_checks(load_config()):
        badge = "✓" if check.ok else ("?" if check.ok is None else "✗")
        table.add_row(badge, check.label, check.detail)
    console.print(table)


@cli.command()
def menubar() -> None:
    """Run the macOS menu bar app (mic icon, meeting control)."""
    from localflow.menubar import main as menubar_main
    menubar_main()


@cli.group()
def agent() -> None:
    """Alias of `lf autostart` (launchd LaunchAgent on macOS)."""


def _autostart_install() -> None:
    platform = current()
    try:
        path = platform.autostart_install()
    except (RuntimeError, OSError) as exc:
        console.print(f"[red]error:[/red] {exc}")
        sys.exit(1)
    if platform.name == "darwin":
        console.print(f"[green]installed[/green] {path}")
        console.print("localflow starts on login and relaunches if it crashes; "
                      "a clean exit stays down until next login (KeepAlive).")
    else:
        console.print(f"installed: {path}")
        console.print("localflow will start at login")


def _autostart_uninstall() -> None:
    try:
        current().autostart_uninstall()
    except (RuntimeError, OSError) as exc:
        console.print(f"[red]error:[/red] {exc}")
        sys.exit(1)
    console.print("[green]uninstalled[/green]")


def _autostart_status() -> None:
    try:
        result = current().autostart_status()
    except (RuntimeError, OSError) as exc:
        console.print(f"[red]error:[/red] {exc}")
        sys.exit(1)
    console.print(result)


@agent.command("install")
def agent_install() -> None:
    """Alias of `lf autostart install` (launchd LaunchAgent on macOS)."""
    _autostart_install()


@agent.command("uninstall")
def agent_uninstall() -> None:
    """Alias of `lf autostart uninstall` (launchd LaunchAgent on macOS)."""
    _autostart_uninstall()


@agent.command("status")
def agent_status() -> None:
    """Alias of `lf autostart status` (launchd LaunchAgent on macOS)."""
    _autostart_status()


@cli.group()
def autostart() -> None:
    """Run localflow on login."""


@autostart.command("install")
def autostart_install() -> None:
    """Install autostart for the current platform."""
    _autostart_install()


@autostart.command("uninstall")
def autostart_uninstall() -> None:
    """Remove autostart for the current platform."""
    _autostart_uninstall()


@autostart.command("status")
def autostart_status() -> None:
    """Show current autostart status."""
    _autostart_status()


def main() -> None:
    cli()


if __name__ == "__main__":
    main()
