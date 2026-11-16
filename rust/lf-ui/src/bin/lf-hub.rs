//! `lf-hub`: the native LocalFlow Hub (macOS only).
//!
//! `lf-hub [--page NAME] [--setup]` opens the native Hub or setup window (the
//! name is matched case-insensitively against the ten Hub pages). Other
//! platforms print a message and exit with status 1.

use std::process::ExitCode;

use lf_ui::hub_route::{canonical_page, HUB_PAGES};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LaunchOptions {
    page: Option<&'static str>,
    setup: bool,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<LaunchOptions, String> {
    let mut options = LaunchOptions {
        page: None,
        setup: false,
    };
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg == "--setup" {
            options.setup = true;
            continue;
        }
        let value = if arg == "--page" {
            args.next().ok_or("--page needs a page name")?
        } else if let Some(value) = arg.strip_prefix("--page=") {
            value.to_string()
        } else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        options.page = Some(canonical_page(&value).ok_or_else(|| {
            format!(
                "unknown page {value:?} (choose from: {})",
                HUB_PAGES.join(", ")
            )
        })?);
    }
    Ok(options)
}

#[cfg(target_os = "macos")]
fn run(options: LaunchOptions) -> ExitCode {
    match lf_ui::mac::app::run_hub(options.page, options.setup) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lf-hub: {error:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn run(_options: LaunchOptions) -> ExitCode {
    eprintln!("lf-hub: the native Hub is only supported on macOS");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)) {
        Ok(options) => run(options),
        Err(message) => {
            eprintln!("lf-hub: {message}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<LaunchOptions, String> {
        parse_args(args.iter().map(|arg| (*arg).to_string()))
    }

    #[test]
    fn command_line_supports_pages_setup_and_errors() {
        assert_eq!(
            parse(&[]),
            Ok(LaunchOptions {
                page: None,
                setup: false
            })
        );
        assert_eq!(
            parse(&["--page", "hot phrases", "--setup"]),
            Ok(LaunchOptions {
                page: Some("Hot Phrases"),
                setup: true
            })
        );
        assert!(parse(&["--page"]).is_err());
        assert!(parse(&["--page=nope"]).is_err());
        assert!(parse(&["unknown"]).is_err());
    }
}
