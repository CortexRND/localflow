//! `lf-rs`: optional Rust command-line client for LocalFlow's local data.
//!
//! The Python `lf` stays the default CLI. This crate ports the subset of its
//! commands that work on local files (config, dictionary, history) plus the
//! provider-key/LLM commands and a read-only `status` that asks the running
//! server. The `serve` command hosts the Rust dictation and meeting services.
//! `meeting start|stop|dismiss` POST to the running server, `prompts` reviews
//! and dispatches the shared work-prompt queue, and `recall` searches local
//! history, transcripts and notes; their Rich console output is reproduced by
//! [`richfmt`]. `tweak` previews, applies and undoes plain-language settings
//! changes through the local `lf-agent` settings agent. `perf` summarizes the
//! local perf ledger (no server needed).
//! `agent` installs launch at login and `setup install-app` the macOS app
//! bundles, both pointing at the Python installation.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, IsTerminal, Write};

use clap::Parser;

pub mod args;
mod clickfmt;
pub mod clipboard;
mod cmd_agent;
mod cmd_config;
mod cmd_dict;
mod cmd_history;
mod cmd_hub;
mod cmd_keys;
mod cmd_llm;
mod cmd_meeting;
mod cmd_perf;
mod cmd_prompts;
mod cmd_recall;
mod cmd_secret;
mod cmd_serve;
mod cmd_setup;
mod cmd_status;
mod cmd_stt;
mod cmd_token;
mod cmd_tweak;
pub mod desktop_launch;
mod native;
mod prompt;
pub mod pyfmt;
mod pyfs;
mod python;
pub mod richfmt;

pub use clipboard::{Clipboard, SystemClipboard};

use args::{Cli, Command};

/// Streams and side-effect seams a command runs against. The binary passes
/// the process stdio and the system clipboard; tests pass buffers and fakes.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub input: &'a mut dyn BufRead,
    pub clipboard: &'a mut dyn Clipboard,
    /// Base URL override for `status` (tests); `None` = from config.
    pub server_url: Option<String>,
    /// Stdin is a terminal: hidden prompts read from it without echo instead
    /// of reading a line of `input`.
    pub terminal: bool,
}

/// A user-facing failure. `Failed` is click's `ClickException` (prints
/// `Error: …`, exit 1); `Usage` is click's `UsageError` (exit 2); `Exit`
/// ends with a status after the command already printed its own message.
#[derive(Debug)]
pub enum CliError {
    Failed(String),
    Usage(String),
    Exit(i32),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Failed(message) | CliError::Usage(message) => f.write_str(message),
            CliError::Exit(code) => write!(f, "exit status {code}"),
        }
    }
}

impl std::error::Error for CliError {}

/// `Err(CliError::Failed(..))` as an `anyhow::Error`.
pub(crate) fn fail<T>(message: impl Into<String>) -> anyhow::Result<T> {
    Err(CliError::Failed(message.into()).into())
}

pub(crate) fn usage<T>(message: impl Into<String>) -> anyhow::Result<T> {
    Err(CliError::Usage(message.into()).into())
}

/// click's `Abort`: `Aborted!` on stderr, exit 1.
pub(crate) fn abort<T>(io: &mut Io<'_>) -> anyhow::Result<T> {
    writeln!(io.err, "Aborted!")?;
    Err(CliError::Exit(1).into())
}

/// `click.confirm(text, default=False)` on stdout: the answer is stripped and
/// lowercased; y/yes or n/no, empty means no; anything else prints
/// `Error: invalid input` (stdout too) and asks again. End of input is click's
/// `Abort`, with nothing more on stdout.
pub(crate) fn confirm(text: &str, io: &mut Io<'_>) -> anyhow::Result<bool> {
    loop {
        write!(io.out, "{text} [y/N]: ")?;
        io.out.flush()?;
        let mut line = String::new();
        if io.input.read_line(&mut line)? == 0 {
            return abort(io);
        }
        match pyfmt::py_strip(&line).to_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" | "" => return Ok(false),
            _ => writeln!(io.out, "Error: invalid input")?,
        }
    }
}

/// Write one line to a stream; a closed pipe is not worth a panic.
macro_rules! outln {
    ($dst:expr) => {
        writeln!($dst).map_err(anyhow::Error::from)
    };
    ($dst:expr, $($arg:tt)*) => {
        writeln!($dst, $($arg)*).map_err(anyhow::Error::from)
    };
}
pub(crate) use outln;

/// Writes WARN and ERROR records from the `log` facade (lf-core's warnings)
/// to stderr as `WARNING: …`/`ERROR: …`; INFO and below are dropped.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) || config_log_quieted(record.target()) {
            return;
        }
        let level = match record.level() {
            log::Level::Error => "ERROR",
            _ => "WARNING",
        };
        // Lock per record so concurrent records don't interleave; a failed
        // write (closed stderr) is ignored.
        let _ = writeln!(io::stderr().lock(), "{level}: {}", record.args());
    }

    fn flush(&self) {
        let _ = io::stderr().flush();
    }
}

static STDERR_LOGGER: StderrLogger = StderrLogger;

thread_local! {
    /// Depth of active [`quiet_config_log`] scopes on this thread.
    static QUIET_CONFIG_LOG: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// True when `target` is the config target and this thread is inside a
/// [`quiet_config_log`] scope. Other targets and threads are never quieted.
fn config_log_quieted(target: &str) -> bool {
    target == lf_core::config::LOG_TARGET
        && QUIET_CONFIG_LOG
            .try_with(|depth| depth.get() > 0)
            .unwrap_or(false)
}

/// Leaves a [`quiet_config_log`] scope on drop, so a panic unwinding out of
/// the scope (or a nested one) restores the depth.
struct QuietConfigLog;

impl Drop for QuietConfigLog {
    fn drop(&mut self) {
        let _ = QUIET_CONFIG_LOG.try_with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Run `f` with [`StderrLogger`] dropping only `localflow.config` records on
/// this thread, as Python's launcher guard silences that logger around its
/// read-only configured check. The global level, stderr and every other
/// target and thread are unchanged; scopes nest.
pub(crate) fn quiet_config_log<R>(f: impl FnOnce() -> R) -> R {
    QUIET_CONFIG_LOG.with(|depth| depth.set(depth.get() + 1));
    let _scope = QuietConfigLog;
    f()
}

/// Install [`StderrLogger`] unless a logger is already installed (then that
/// one and its level filter are kept).
fn init_stderr_logger() {
    if log::set_logger(&STDERR_LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
}

/// Run the CLI against the process stdio. Returns the exit status.
pub fn run_main<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    init_stderr_logger();
    let stdout = io::stdout();
    let stdin = io::stdin();
    let mut out = stdout.lock();
    // Unlocked (stderr is unbuffered): holding the lock for the whole run
    // would block log records from other threads until exit.
    let mut err = io::stderr();
    let terminal = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut clipboard = SystemClipboard;
    let mut io = Io {
        out: &mut out,
        err: &mut err,
        input: &mut input,
        clipboard: &mut clipboard,
        server_url: None,
        terminal,
    };
    run(args, &mut io)
}

/// Parse `args` (including the program name) and run the command.
pub fn run<I, T>(args: I, io: &mut Io<'_>) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            if let Some(text) = clickfmt::usage_error(&args, &error) {
                let _ = write!(io.err, "{text}");
                let _ = io.err.flush();
                return 2;
            }
            let rendered = error.render().to_string();
            let stream: &mut dyn Write = if error.use_stderr() {
                &mut *io.err
            } else {
                &mut *io.out
            };
            let _ = write!(stream, "{rendered}");
            let _ = stream.flush();
            return error.exit_code();
        }
    };
    let result = dispatch(cli.command, io);
    report(result, io)
}

/// Print a command's error the way click does and return the exit status.
pub(crate) fn report(result: anyhow::Result<()>, io: &mut Io<'_>) -> i32 {
    let code = match result {
        Ok(()) => 0,
        Err(error) => match error.downcast_ref::<CliError>() {
            Some(CliError::Exit(code)) => *code,
            Some(CliError::Usage(message)) => {
                let _ = writeln!(io.err, "Error: {message}");
                2
            }
            Some(CliError::Failed(message)) => {
                let _ = writeln!(io.err, "Error: {message}");
                1
            }
            None => {
                let _ = writeln!(io.err, "Error: {error:#}");
                1
            }
        },
    };
    let _ = io.out.flush();
    let _ = io.err.flush();
    code
}

fn dispatch(command: Command, io: &mut Io<'_>) -> anyhow::Result<()> {
    match command {
        Command::Serve { host, port } => cmd_serve::run(host, port, io),
        Command::Config(command) => cmd_config::run(command, io),
        Command::Dict(command) => cmd_dict::run(command, io),
        Command::Dictation { action } => {
            use args::DictationAction;
            use lf_core::config::{load_config, save_config};
            let mut config = load_config()?;
            if !matches!(action, DictationAction::Status) {
                config.set_field(
                    "dictation_enabled",
                    serde_json::json!(matches!(action, DictationAction::On)),
                );
                save_config(&config, None)?;
            }
            let enabled = config.field_bool("dictation_enabled").unwrap_or(true);
            outln!(io.out, "Dictation: {}", if enabled { "on" } else { "off" })
        }
        Command::History(args) => cmd_history::run(args, io),
        Command::Stats => cmd_history::stats(io),
        Command::Perf(args) => cmd_perf::run(args, io),
        Command::Llm(command) => cmd_llm::run(command, io),
        Command::Stt(command) => cmd_stt::run(command, io),
        Command::Keys(command) => cmd_keys::run(command, io),
        Command::Secret(command) => cmd_secret::run(command, io),
        Command::Status { json } => cmd_status::run(json, io),
        Command::Meeting(command) => cmd_meeting::run(command, io),
        Command::Prompts(command) => cmd_prompts::run(command, io),
        Command::Recall(args) => cmd_recall::run(args, io),
        Command::Hub { page, gpui, legacy } => cmd_hub::run(page, gpui, legacy, io),
        Command::Token => cmd_token::run(io),
        Command::Tweak(args) => cmd_tweak::run(args, io),
        Command::Agent(command) => cmd_agent::run(command, io),
        Command::Setup(command) => cmd_setup::run(command, io),
    }
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests {
    use crate::testing::{run_cli, run_with, FakeClipboard};

    #[test]
    fn help_and_version_exit_zero_on_stdout() {
        let help = run_cli(&["--help"]);
        assert_eq!(help.code, 0);
        assert!(help.out.contains("Usage: lf-rs"));
        for name in [
            "config", "dict", "history", "llm", "keys", "status", "meeting", "prompts", "hub",
            "token", "tweak",
        ] {
            assert!(help.out.contains(name), "{name}");
        }
        // Hidden like their click counterparts, but still runnable.
        assert!(!help.out.contains("recall"));
        let prompts = run_cli(&["prompts", "--help"]);
        assert!(prompts.out.contains("  dispatch") && !prompts.out.contains("  approve"));
        assert_eq!(run_cli(&["recall", "--help"]).code, 0);
        let start = run_cli(&["meeting", "start", "--help"]);
        assert!(start.out.contains("--title") && start.out.contains("--category"));
        assert!(help.err.is_empty());
        // `secret` is hidden like Python's `@cli.group(hidden=True)`.
        assert!(!help
            .out
            .lines()
            .any(|l| l.trim_start().starts_with("secret")));
        let secret = run_cli(&["secret", "--help"]);
        assert_eq!(secret.code, 0);
        for name in ["set", "unset", "status"] {
            assert!(secret.out.contains(name), "{name}");
        }
        let sub = run_cli(&["history", "clear", "--help"]);
        assert_eq!(sub.code, 0);
        assert!(sub.out.contains("--yes"));
        assert_eq!(run_cli(&["--version"]).code, 0);
        // `agent` is hidden like click's; `setup` is listed.
        assert!(!help.out.contains("agent") && help.out.contains("setup"));
        let agent = run_cli(&["agent", "--help"]);
        assert!(agent.out.contains("install") && agent.out.contains("uninstall"));
        assert!(run_cli(&["setup", "--help"]).out.contains("install-app"));
    }

    #[test]
    fn new_groups_report_click_usage_errors() {
        for (args, last) in [
            (
                &["agent"][..],
                "  uninstall  Alias of `lf autostart uninstall`",
            ),
            (&["agent", "bogus"], "Error: No such command 'bogus'."),
            (
                &["setup", "install-app", "extra"],
                "Error: Got unexpected extra argument (extra)",
            ),
            (
                &["agent", "status", "a", "b"],
                "Error: Got unexpected extra arguments (a b)",
            ),
        ] {
            let outcome = run_cli(args);
            assert_eq!((outcome.code, outcome.out.as_str()), (2, ""), "{args:?}");
            let line = outcome.err.lines().last().unwrap_or_default();
            assert!(line.starts_with(last), "{args:?}: {line}");
        }
        assert_eq!(run_cli(&["setup"]).code, 2);
    }

    #[test]
    fn usage_errors_exit_two_on_stderr() {
        for args in [
            &[][..],
            &["bogus"],
            &["history", "--limit", "0"],
            &["history", "list", "--limit", "1001"],
            &["history", "search"],
            &["config", "set", "llm.model"],
            &["dict", "import"],
            &["llm", "use"],
            &["keys"],
            &["keys", "import", "--file"],
            &["secret"],
            &["secret", "set"],
            &["secret", "unset"],
            &["meeting"],
            &["meeting", "bogus"],
            &["prompts"],
            &["prompts", "dispatch", "--dry-run", "--repo"],
        ] {
            let outcome = run_cli(args);
            assert_eq!(outcome.code, 2, "{args:?}");
            assert!(!outcome.err.is_empty(), "{args:?}");
        }
    }

    #[test]
    fn new_commands_report_click_usage_errors_before_touching_files() {
        for (args, message) in [
            (&["recall"][..], "Missing argument 'QUESTION'."),
            (&["recall", " \t"], "question is empty"),
            (
                &["recall", "x", "--k", "0"],
                "Invalid value for '--k': 0 is not in the range 1<=x<=20.",
            ),
            (
                &["recall", "--k=x"],
                "Invalid value for '--k': 'x' is not a valid integer range.",
            ),
            (
                &["recall", "x", "--since", "Soon"],
                "Invalid value for --since: invalid since value 'Soon'; \
                 use today, yesterday, 7d, 30d or YYYY-MM-DD",
            ),
            (
                &["hub", "--page", "x", "--gpui", "--legacy"],
                "Invalid value for '--page': 'x' is not one of 'home', 'ask', 'hot phrases', \
                 'dictionary', 'prompts', 'meetings', 'history', 'style', 'models', 'settings'.",
            ),
            (
                &["hub", "--page", "SETTINGS", "--gpui", "--legacy"],
                "--gpui and --legacy can't be used together",
            ),
            (&["prompts", "show"], "Missing argument 'PROMPT_ID'."),
            (&["prompts", "approve"], "Missing argument 'IDS...'."),
            (&["prompts", "reject"], "Missing argument 'IDS...'."),
            (
                &["prompts", "list", "-s", "done"],
                "Invalid value for '--status' / '-s': 'done' is not one of \
                 'pending', 'approved', 'rejected', 'dispatched', 'failed'.",
            ),
        ] {
            let outcome = run_cli(args);
            assert_eq!(
                (outcome.code, outcome.out.as_str(), outcome.err.as_str()),
                (2, "", format!("Error: {message}\n").as_str()),
                "{args:?}"
            );
        }
    }

    #[test]
    fn unknown_config_key_fails_before_touching_config() {
        let outcome = run_cli(&["config", "get", "nope.key"]);
        assert_eq!(outcome.code, 1);
        assert!(outcome.out.is_empty());
        assert!(outcome.err.starts_with(
            "Error: unknown config key 'nope.key'; valid keys: stt.provider, stt.model"
        ));
        let set = run_with(
            &["config", "set", "llm", "x"],
            "",
            &mut FakeClipboard::default(),
        );
        assert_eq!(set.code, 1);
        assert!(set.err.starts_with("Error: unknown config key 'llm'; "));
    }

    #[test]
    fn negative_values_are_values_not_flags() {
        // `config set KEY -5` must reach validation, not be parsed as a flag.
        let outcome = run_cli(&["config", "set", "nope.key", "-5"]);
        assert_eq!(outcome.code, 1);
        assert!(outcome.err.starts_with("Error: unknown config key"));
    }

    #[test]
    fn stderr_logger_filters_below_warn_and_init_is_idempotent() {
        use log::Log as _;
        let enabled =
            |level| super::STDERR_LOGGER.enabled(&log::Metadata::builder().level(level).build());
        assert!(enabled(log::Level::Error));
        assert!(enabled(log::Level::Warn));
        assert!(!enabled(log::Level::Info));
        assert!(!enabled(log::Level::Debug));
        // A second install (or one after another logger) must not panic.
        super::init_stderr_logger();
        super::init_stderr_logger();
        assert!(log::max_level() >= log::LevelFilter::Warn);
    }

    #[test]
    fn quiet_config_log_drops_only_config_records_in_scope() {
        use super::{config_log_quieted, quiet_config_log};
        let config = lf_core::config::LOG_TARGET;
        assert!(!config_log_quieted(config));
        quiet_config_log(|| {
            assert!(config_log_quieted(config));
            assert!(!config_log_quieted("localflow.secrets"));
            assert!(!config_log_quieted("lf"));
            // Another thread is never quieted.
            assert!(!std::thread::spawn(move || config_log_quieted(config))
                .join()
                .unwrap());
            // Nested scopes restore the outer one.
            assert!(quiet_config_log(|| config_log_quieted(config)));
            assert!(config_log_quieted(config));
        });
        assert!(!config_log_quieted(config));
        // A panic inside (nested) scopes restores the depth on unwind.
        let caught = std::panic::catch_unwind(|| {
            quiet_config_log(|| quiet_config_log(|| panic!("boom")));
        });
        assert!(caught.is_err());
        assert!(!config_log_quieted(config));
        assert_eq!(quiet_config_log(|| 7), 7);
    }
}
