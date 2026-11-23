//! click's usage errors for the `agent` and `setup` groups: a bare `agent`
//! prints the group help (exit 2), an unknown subcommand is `No such command
//! 'x'.` and surplus arguments are `Got unexpected extra argument (x)`, as
//! `localflow/cli.py` reports them. Other usage errors keep clap's text.
//! Names and help come from the clap definitions in `args.rs`, whose doc
//! comments are verbatim click docstrings.

use std::env;
use std::ffi::OsString;

use clap::error::ErrorKind;
use clap::CommandFactory;

use crate::args::Cli;
use crate::pyfmt::str_repr;

/// The groups whose usage errors read like click's.
const GROUPS: [&str; 2] = ["agent", "setup"];

/// A group's help and its `(name, help)` commands, sorted by name as click
/// lists them.
fn group(name: &str) -> Option<(String, Vec<(String, String)>)> {
    let root = Cli::command();
    let group = root.find_subcommand(name)?;
    let about = |command: &clap::Command| {
        command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default()
    };
    let mut commands: Vec<(String, String)> = group
        .get_subcommands()
        .filter(|command| command.get_name() != "help")
        .map(|command| (command.get_name().to_string(), about(command)))
        .collect();
    commands.sort();
    Some((about(group), commands))
}

/// click's text for a failed parse of `args` (program name first), or `None`
/// when it is not one of the cases above.
pub(crate) fn usage_error(args: &[OsString], error: &clap::Error) -> Option<String> {
    if !matches!(
        error.kind(),
        ErrorKind::InvalidSubcommand
            | ErrorKind::UnknownArgument
            | ErrorKind::MissingSubcommand
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        return None;
    }
    let tokens: Vec<String> = args
        .iter()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let group_name = GROUPS
        .into_iter()
        .find(|name| tokens.first().map(String::as_str) == Some(*name))?;
    let (help, commands) = group(group_name)?;
    let prog = format!("lf-rs {group_name}");
    let Some(name) = tokens.get(1) else {
        // `setup` alone opens the setup window in Python; not ported.
        return (group_name == "agent").then(|| group_help(&prog, &help, &commands));
    };
    if name.starts_with('-') {
        return None;
    }
    if !commands.iter().any(|(command, _)| command == name) {
        return Some(format!(
            "Usage: {prog} [OPTIONS] COMMAND [ARGS]...\n\
             Try '{prog} --help' for help.\n\nError: No such command {}.\n",
            str_repr(name)
        ));
    }
    let extra = &tokens[2..];
    if extra.is_empty() || extra.iter().any(|arg| arg.starts_with('-')) {
        return None;
    }
    let plural = if extra.len() == 1 { "" } else { "s" };
    Some(format!(
        "Usage: {prog} {name} [OPTIONS]\nTry '{prog} {name} --help' for help.\n\n\
         Error: Got unexpected extra argument{plural} ({})\n",
        extra.join(" ")
    ))
}

/// click's `HelpFormatter` width: the terminal columns (`COLUMNS`, else 80)
/// capped at 80, minus 2, at least 50.
fn help_width() -> usize {
    let columns = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&columns| columns > 0)
        .unwrap_or(80);
    columns.min(80).saturating_sub(2).max(50)
}

/// `Group.get_help` for a group whose commands take no options.
fn group_help(prog: &str, help: &str, commands: &[(String, String)]) -> String {
    let width = help_width();
    let mut out = format!("Usage: {prog} [OPTIONS] COMMAND [ARGS]...\n\n");
    for line in wrap(help, width - 2) {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("\nOptions:\n  --help  Show this message and exit.\n\nCommands:\n");
    let longest = commands
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    let limit = width.saturating_sub(6 + longest);
    for (name, text) in commands {
        out.push_str(&format!(
            "  {name:<longest$}  {}\n",
            short_help(text, limit)
        ));
    }
    out
}

/// Greedy word wrap (`textwrap` for single-spaced text without long words).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
}

/// `click.utils.make_default_short_help(help, max_length)`.
fn short_help(help: &str, max_length: usize) -> String {
    let words: Vec<&str> = help.split_whitespace().collect();
    let mut total = 0;
    let mut index = 0;
    let mut truncated = false;
    for (i, word) in words.iter().enumerate() {
        index = i;
        total += word.chars().count() + usize::from(i > 0);
        if total > max_length {
            truncated = true;
            break;
        }
        if word.ends_with('.') {
            return words[..=i].join(" ");
        }
        if total == max_length && i + 1 != words.len() {
            truncated = true;
            break;
        }
    }
    if !truncated {
        return words.join(" ");
    }
    total += 3;
    while index > 0 {
        total -= words[index].chars().count() + 1;
        if total <= max_length {
            break;
        }
        index -= 1;
    }
    format!("{}...", words[..index].join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_help_truncates_like_click() {
        let text = "Alias of `lf autostart uninstall` (launchd LaunchAgent on macOS).";
        assert_eq!(
            short_help(text, 63),
            "Alias of `lf autostart uninstall` (launchd LaunchAgent on..."
        );
        assert_eq!(short_help(text, 65), text);
        assert_eq!(short_help("One. Two.", 3), "...");
        assert_eq!(short_help("First sentence. More", 45), "First sentence.");
    }

    #[test]
    fn group_help_lists_sorted_commands() {
        let (help, commands) = group(GROUPS[0]).unwrap();
        let text = group_help("lf-rs agent", &help, &commands);
        assert!(text.starts_with("Usage: lf-rs agent [OPTIONS] COMMAND [ARGS]...\n\n  Alias of"));
        assert!(text.contains("\n  install    Alias of `lf autostart install`"));
        // click's short help needs the docstrings' closing periods (verbatim).
        assert!(text.contains("(launchd LaunchAgent on macOS).\n\nOptions:"));
        let names: Vec<&str> = commands.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["install", "status", "uninstall"]);
        let (_, setup) = group(GROUPS[1]).unwrap();
        assert_eq!(
            setup,
            [(
                "install-app".to_string(),
                "Install the setup window as a LocalFlow Setup.app bundle.".to_string()
            )]
        );
    }
}
