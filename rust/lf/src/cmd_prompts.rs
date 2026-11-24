//! `prompts list|show|approve|reject|dispatch`: the Python `lf prompts`
//! commands over the shared `~/.localflow/queue.json` (`lf_meetings::dispatch`).

use std::path::Path;

use anyhow::Result;
use lf_core::{config, home};
use lf_meetings::dispatch::{
    self, PromptQueue, StatusConflict, APPROVE_FROM, REJECT_FROM, STATUSES,
};
use serde_json::{json, Map, Value};

use crate::args::PromptsCommand;
use crate::pyfmt::{str_repr, value_str};
use crate::richfmt::Console;
use crate::{usage, CliError, Io};

pub(crate) fn run(command: PromptsCommand, io: &mut Io<'_>) -> Result<()> {
    let mut out = Out {
        io,
        console: Console::detect(),
    };
    match command {
        PromptsCommand::List { status } => list(status, &mut out),
        PromptsCommand::Show { prompt_id } => match prompt_id {
            Some(prompt_id) => show(&prompt_id, &mut out),
            None => usage("Missing argument 'PROMPT_ID'."),
        },
        PromptsCommand::Approve { ids } => set_reviewed(&ids, "approved", APPROVE_FROM, &mut out),
        PromptsCommand::Reject { ids } => set_reviewed(&ids, "rejected", REJECT_FROM, &mut out),
        PromptsCommand::Dispatch {
            ids,
            all,
            repo,
            agent,
            dry_run,
        } => dispatch_prompts(&ids, all, &repo, &agent, dry_run, &mut out),
    }
}

/// The command's stdout through a Rich console.
struct Out<'a, 'b> {
    io: &'a mut Io<'b>,
    console: Console,
}

impl Out<'_, '_> {
    /// `console.print(markup)`.
    fn print(&mut self, markup: &str) -> Result<()> {
        write!(self.io.out, "{}", self.console.markup(markup))?;
        Ok(())
    }

    fn table(&mut self, headers: &[&str], rows: &[Vec<String>]) -> Result<()> {
        write!(self.io.out, "{}", self.console.table(headers, rows))?;
        Ok(())
    }
}

fn exit1<T>() -> Result<T> {
    Err(CliError::Exit(1).into())
}

/// `entry[key]` as `str()` would format it.
fn text(entry: &Value, key: &str) -> String {
    entry.get(key).map(value_str).unwrap_or_default()
}

/// `entry.get(key) or ""`.
fn text_or_empty(entry: &Value, key: &str) -> String {
    match entry.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => String::new(),
        Some(Value::Array(a)) if a.is_empty() => String::new(),
        Some(Value::Object(o)) if o.is_empty() => String::new(),
        Some(other) => value_str(other),
    }
}

fn status_of(entry: &Value) -> &str {
    entry.get("status").and_then(Value::as_str).unwrap_or("")
}

/// `cli._status`: the status wrapped in its style tag.
fn styled(status: &str) -> String {
    let style = match status {
        "pending" => "yellow",
        "approved" => "cyan",
        "dispatched" => "green",
        "rejected" => "dim",
        "failed" => "red",
        _ => "white",
    };
    format!("[{style}]{status}[/{style}]")
}

/// `cli._resolve`: the entry for `prompt_id`, or exit 1 with a message.
fn resolve(queue: &PromptQueue, prompt_id: &str, out: &mut Out<'_, '_>) -> Result<Value> {
    match queue.get(prompt_id)? {
        Some(entry) => Ok(entry),
        None => {
            out.print(&format!(
                "[red]unknown prompt[/red] {prompt_id} — list them with: [bold]lf prompts list[/bold]"
            ))?;
            exit1()
        }
    }
}

fn list(status: Option<String>, out: &mut Out<'_, '_>) -> Result<()> {
    if let Some(status) = status.as_deref().filter(|s| !STATUSES.contains(s)) {
        let choices: Vec<String> = STATUSES.iter().map(|s| str_repr(s)).collect();
        return usage(format!(
            "Invalid value for '--status' / '-s': {} is not one of {}.",
            str_repr(status),
            choices.join(", ")
        ));
    }
    let queue = PromptQueue::at_default_path()?;
    let entries = queue.list(status.as_deref())?;
    if entries.is_empty() {
        let what = match &status {
            Some(status) => format!("no {status} prompts"),
            None => "no prompts queued".into(),
        };
        return out.print(&format!("[dim]{what} ({})[/dim]", queue.path.display()));
    }
    let rows: Vec<Vec<String>> = entries
        .iter()
        .map(|e| {
            vec![
                text(e, "id"),
                styled(&text(e, "status")),
                text(e, "title"),
                text_or_empty(e, "meeting"),
                text_or_empty(e, "created"),
            ]
        })
        .collect();
    out.table(&["id", "status", "title", "meeting", "created"], &rows)
}

fn show(prompt_id: &str, out: &mut Out<'_, '_>) -> Result<()> {
    let entry = resolve(&PromptQueue::at_default_path()?, prompt_id, out)?;
    let path = home::expand_user(Path::new(&text(&entry, "path")))?;
    if !path.exists() {
        out.print(&format!(
            "[red]prompt file missing[/red] {}",
            path.display()
        ))?;
        return exit1();
    }
    out.print(&format!("[dim]{}[/dim]", path.display()))?;
    if status_of(&entry) == "dispatched" {
        let worktree = text_or_empty(&entry, "worktree");
        let worktree = if worktree.is_empty() { "?" } else { &worktree };
        out.print(&format!(
            "[green]dispatched[/green] to worktree [bold]{worktree}[/bold]"
        ))?;
        let handle = text_or_empty(&entry, "handle");
        if handle.is_empty() {
            out.print("  [dim]no terminal handle recorded[/dim]")?;
        } else {
            out.print(&format!(
                "  follow it with: [bold]orca terminal read --terminal {handle} --json[/bold]"
            ))?;
        }
    }
    out.print("")?;
    // `Path.read_text()`: universal newlines.
    let bytes = std::fs::read(&path)?;
    let body = String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    out.print(&body)
}

/// `cli._set_reviewed`: move prompts in `from` to `status` with a
/// compare-and-set; anything else is refused (exit 1 after the loop).
fn set_reviewed(ids: &[String], status: &str, from: &[&str], out: &mut Out<'_, '_>) -> Result<()> {
    if ids.is_empty() {
        return usage("Missing argument 'IDS...'.");
    }
    let queue = PromptQueue::at_default_path()?;
    let approvable = from.join(" or ");
    let mut refused = false;
    for prompt_id in ids {
        let entry = resolve(&queue, prompt_id, out)?;
        let current = status_of(&entry);
        if !from.contains(&current) {
            out.print(&format!(
                "[red]{prompt_id}[/red] is {} — only {approvable} prompts can be {status}; skipped",
                styled(&text(&entry, "status"))
            ))?;
            refused = true;
            continue;
        }
        // Re-approving a failed prompt clears the failed attempt's residue; a
        // rejected failure keeps its error as the record of what went wrong.
        let mut clear = Map::new();
        if current == "failed" && status == "approved" {
            for key in ["error", "worktree", "handle"] {
                clear.insert(key.into(), json!(""));
            }
        }
        if let Err(error) = queue.set_status(prompt_id, status, Some(from), &clear) {
            let Some(conflict) = error.downcast_ref::<StatusConflict>() else {
                return Err(error);
            };
            out.print(&format!(
                "[red]{prompt_id}[/red] changed to {} while it was being {status} — not touched",
                styled(&conflict.current)
            ))?;
            refused = true;
            continue;
        }
        out.print(&format!(
            "{} {prompt_id}  {}",
            styled(status),
            text(&entry, "title")
        ))?;
    }
    if refused {
        return exit1();
    }
    Ok(())
}

fn dispatch_prompts(
    ids: &[String],
    all: bool,
    repo: &str,
    agent: &str,
    dry_run: bool,
    out: &mut Out<'_, '_>,
) -> Result<()> {
    let loaded = config::load_config()?;
    let target_repo = if repo.is_empty() {
        loaded.field_str("orca_repo").unwrap_or("").to_string()
    } else {
        repo.to_string()
    };
    if target_repo.is_empty() {
        out.print(
            "[red]no repo configured[/red] — set [bold]orca_repo[/bold] in ~/.localflow.toml \
             or pass [bold]--repo[/bold]",
        )?;
        out.print("  find the selector with: [bold]orca repo list --json[/bold]")?;
        return exit1();
    }
    let target_agent = if agent.is_empty() {
        loaded.field_str("orca_agent").unwrap_or("").to_string()
    } else {
        agent.to_string()
    };

    let queue = PromptQueue::at_default_path()?;
    if all && !ids.is_empty() {
        out.print("[red]pass ids or --all, not both[/red]")?;
        return exit1();
    }
    let entries = if all {
        queue.list(Some("approved"))?
    } else if !ids.is_empty() {
        let mut entries = Vec::new();
        for id in ids {
            entries.push(resolve(&queue, id, out)?);
        }
        let unapproved: Vec<&Value> = entries
            .iter()
            .filter(|e| status_of(e) != "approved")
            .collect();
        if !unapproved.is_empty() {
            for e in unapproved {
                let id = text(e, "id");
                out.print(&format!(
                    "[red]{id}[/red] is {}, not approved — approve it with: \
                     [bold]lf prompts approve {id}[/bold]",
                    styled(&text(e, "status"))
                ))?;
            }
            return exit1();
        }
        entries
    } else {
        out.print("[red]nothing selected[/red] — pass ids or [bold]--all[/bold]")?;
        return exit1();
    };

    if entries.is_empty() {
        return out.print("[dim]no approved prompts to dispatch[/dim]");
    }

    if dry_run {
        let rows: Vec<Vec<String>> = entries
            .iter()
            .map(|e| {
                vec![
                    text(e, "id"),
                    text(e, "title"),
                    target_repo.clone(),
                    target_agent.clone(),
                ]
            })
            .collect();
        out.table(&["id", "title", "repo", "agent"], &rows)?;
        return out.print(&format!(
            "[dim]dry run — {} prompt(s) would be dispatched[/dim]",
            entries.len()
        ));
    }

    let (ready, reason) = dispatch::orca_available();
    if !ready {
        out.print(&format!("[red]orca unavailable:[/red] {reason}"))?;
        out.print(
            "  start the app with: [bold]orca open[/bold], then retry — prompts stay approved",
        )?;
        return exit1();
    }

    let mut dispatched = 0;
    let mut conflicts = 0;
    for e in &entries {
        let (id, title) = (text(e, "id"), text(e, "title"));
        let result = dispatch::dispatch(e, &target_repo, &target_agent);
        // Each write compare-and-sets on "approved": if the server dispatched
        // this entry while orca was working, its record must not be overwritten.
        if result.get("ok") == Some(&Value::Bool(true)) {
            let worktree = text(&result, "worktree");
            let mut fields = Map::new();
            fields.insert(
                "worktree".into(),
                result.get("worktree").cloned().unwrap_or(Value::Null),
            );
            fields.insert(
                "handle".into(),
                result.get("handle").cloned().unwrap_or(json!("")),
            );
            fields.insert("repo".into(), json!(target_repo));
            match queue.set_status(&id, "dispatched", Some(&["approved"]), &fields) {
                Err(error) => {
                    let Some(conflict) = error.downcast_ref::<StatusConflict>() else {
                        return Err(error);
                    };
                    out.print(&format!(
                        "[yellow]dispatched but not recorded[/yellow] {id}  {title} → \
                         {worktree} (queue moved to {} meanwhile)",
                        styled(&conflict.current)
                    ))?;
                    conflicts += 1;
                    continue;
                }
                Ok(_) => {
                    out.print(&format!(
                        "[green]dispatched[/green] {id}  {title} → {worktree}"
                    ))?;
                    dispatched += 1;
                }
            }
        } else {
            let error = match result.get("error") {
                Some(error) => value_str(error),
                None => "unknown error".into(),
            };
            let mut fields = Map::new();
            fields.insert("error".into(), json!(error));
            if let Err(failure) = queue.set_status(&id, "failed", Some(&["approved"]), &fields) {
                let Some(conflict) = failure.downcast_ref::<StatusConflict>() else {
                    return Err(failure);
                };
                out.print(&format!(
                    "[red]failed[/red] {id}  {title}: {error} \
                     (not recorded — queue moved to {})",
                    styled(&conflict.current)
                ))?;
                conflicts += 1;
                continue;
            }
            out.print(&format!("[red]failed[/red] {id}  {title}: {error}"))?;
        }
    }
    let failures = entries.len() - dispatched - conflicts;
    let mut summary = format!("\n{dispatched} dispatched, {failures} failed");
    if conflicts > 0 {
        summary.push_str(&format!(", {conflicts} raced (see above)"));
    }
    out.print(&summary)?;
    if failures > 0 || conflicts > 0 {
        return exit1();
    }
    Ok(())
}
