//! `lf-rs`: optional Rust command-line client for LocalFlow's local data.
//!
//! The Python `lf` stays the default CLI. This crate ports the subset of its
//! commands that work on local files (config, dictionary, history) plus the
//! provider-key/LLM commands and a read-only `status` that asks the running
//! Python server. All storage logic lives in `lf-core`/`lf-providers`; this
//! crate only parses arguments and renders output like `localflow/cli.py`.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, Write};

use clap::Parser;

pub mod args;
pub mod clipboard;
mod cmd_config;
mod cmd_dict;
mod cmd_history;
mod cmd_keys;
mod cmd_llm;
mod cmd_status;
pub mod pyfmt;

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

/// Run the CLI against the process stdio. Returns the exit status.
pub fn run_main<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let stdout = io::stdout();
    let stderr = io::stderr();
    let stdin = io::stdin();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut input = stdin.lock();
    let mut clipboard = SystemClipboard;
    let mut io = Io {
        out: &mut out,
        err: &mut err,
        input: &mut input,
        clipboard: &mut clipboard,
        server_url: None,
    };
    run(args, &mut io)
}

/// Parse `args` (including the program name) and run the command.
pub fn run<I, T>(args: I, io: &mut Io<'_>) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
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
        Command::Serve { host, port } => {
            let config = lf_core::config::load_config()?;
            let host = host.unwrap_or_else(|| {
                config
                    .field_str("server_host")
                    .unwrap_or("127.0.0.1")
                    .into()
            });
            let port = match port {
                Some(port) => port,
                None => u16::try_from(config.field_i64("server_port").unwrap_or(8756))
                    .map_err(|_| anyhow::anyhow!("server_port must be between 0 and 65535"))?,
            };
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(16)
                .enable_all()
                .build()?;
            let server = runtime.block_on(lf_server::Server::bind(&host, port))?;
            outln!(
                io.out,
                "LocalFlow Rust server listening on {}",
                lf_server::server_url::base_url(&host, server.local_addr()?.port())
            )?;
            io.out.flush()?;
            runtime.block_on(server.run())
        }
        Command::Config(command) => cmd_config::run(command, io),
        Command::Dict(command) => cmd_dict::run(command, io),
        Command::History(args) => cmd_history::run(args, io),
        Command::Stats => cmd_history::stats(io),
        Command::Llm(command) => cmd_llm::run(command, io),
        Command::Keys(command) => cmd_keys::run(command, io),
        Command::Status { json } => cmd_status::run(json, io),
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
        for name in ["config", "dict", "history", "llm", "keys", "status"] {
            assert!(help.out.contains(name), "{name}");
        }
        assert!(help.err.is_empty());
        let sub = run_cli(&["history", "clear", "--help"]);
        assert_eq!(sub.code, 0);
        assert!(sub.out.contains("--yes"));
        assert_eq!(run_cli(&["--version"]).code, 0);
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
        ] {
            let outcome = run_cli(args);
            assert_eq!(outcome.code, 2, "{args:?}");
            assert!(!outcome.err.is_empty(), "{args:?}");
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
}
