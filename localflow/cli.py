"""`lf` — command-line control for localflow.

Talks to the running localflow server over HTTP; `lf serve` / `lf dictate`
run the components themselves.
"""

import json
import sys
import time
from datetime import datetime
from pathlib import Path

import click
import requests
from rich.console import Console
from rich.live import Live
from rich.panel import Panel
from rich.table import Table
from rich.text import Text

from localflow.config import (
    config_field,
    config_toml,
    legacy_config_path,
    load_config,
    migrate_legacy,
    save_config,
    valid_config_keys,
)
from localflow.config import config_path as resolved_config_path
from localflow.dispatch import (
    QUEUE_PATH,
    STATUSES,
    PromptQueue,
    StatusConflict,
    dispatch,
    orca_available,
)
from localflow.platform import current
from localflow.secrets import SecretsUnavailable, delete_secret, get_secret, set_secret

console = Console()
_config = load_config()
BASE = f"http://127.0.0.1:{_config.server_port}"


def _get(path: str) -> dict:
    try:
        resp = requests.get(BASE + path, timeout=5)
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
        _section, field_name = config_field(section_key)
    except KeyError:
        raise click.ClickException(
            f"unknown config key {section_key!r}; valid keys: "
            + ", ".join(valid_config_keys())
        )
    current = load_config()
    setattr(current, field_name, _coerce_config_value(getattr(current, field_name), raw_value))
    path = save_config(current)
    click.echo(f"saved {path}")


@config.command("migrate")
def config_migrate_command() -> None:
    """Force migration of the legacy flat config file."""
    migrated = migrate_legacy()
    if migrated is None:
        click.echo("no legacy config found")
    else:
        click.echo(f"migrated legacy config to {resolved_config_path()}")


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


@cli.command()
def status() -> None:
    """Server, watcher, and session state."""
    st = _get("/meeting/status")
    table = Table(show_header=False, box=None)
    table.add_row("server", "[green]up[/green]")
    watcher = "[green]watching[/green]" if st["watching"] else f"[red]off[/red] {st['watch_error']}"
    table.add_row("watcher", watcher)
    table.add_row("mic busy", "[yellow]yes[/yellow]" if st["mic_busy"] else "no")
    if st["detected"]:
        table.add_row("detected", f"[bold yellow]meeting ({st['platform'] or '?'})[/bold yellow]")
    session = st.get("session")
    if session:
        table.add_row(
            "session",
            f"[bold red]● {session['title']}[/bold red] ({session['category']}) "
            f"{session['seconds'] // 60}m{session['seconds'] % 60:02d}s, "
            f"{session['segment_count']} segments",
        )
    else:
        table.add_row("session", "none")
    if st.get("last_saved"):
        table.add_row("last saved", st["last_saved"]["notes_path"])
    console.print(table)


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
    current().open_path(BASE)


@cli.command()
def menubar() -> None:
    """Run the macOS menu bar app (mic icon, meeting control)."""
    from localflow.menubar import main as menubar_main
    menubar_main()


@cli.group()
def agent() -> None:
    """Run localflow on login via a launchd LaunchAgent (always on)."""


def _autostart_install() -> None:
    try:
        path = current().autostart_install()
    except RuntimeError as exc:
        console.print(f"[red]error:[/red] {exc}")
        sys.exit(1)
    console.print(f"[green]installed[/green] {path}")
    console.print("localflow starts on login and relaunches if it crashes; "
                  "a clean exit stays down until next login (KeepAlive).")


def _autostart_uninstall() -> None:
    try:
        current().autostart_uninstall()
    except RuntimeError as exc:
        console.print(f"[red]error:[/red] {exc}")
        sys.exit(1)
    console.print("[green]uninstalled[/green]")


def _autostart_status() -> None:
    try:
        result = current().autostart_status()
    except RuntimeError as exc:
        console.print(f"[red]error:[/red] {exc}")
        sys.exit(1)
    console.print(result)


@agent.command("install")
def agent_install() -> None:
    """Install and load the LaunchAgent so localflow starts on login."""
    _autostart_install()


@agent.command("uninstall")
def agent_uninstall() -> None:
    """Unload and remove the LaunchAgent."""
    _autostart_uninstall()


@agent.command("status")
def agent_status() -> None:
    """Show whether the LaunchAgent is loaded and running."""
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
