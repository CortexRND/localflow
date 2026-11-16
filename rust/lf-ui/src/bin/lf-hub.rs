//! `lf-hub`: the native LocalFlow Hub (macOS only).
//!
//! `lf-hub [--page NAME]` opens the Hub on a page (default `Home`; the name is
//! matched case-insensitively against the ten Hub pages). Other platforms print
//! a message and exit with status 1.

use std::process::ExitCode;

use lf_ui::hub_route::{canonical_page, DEFAULT_PAGE, HUB_PAGES};

fn parse_page(args: impl Iterator<Item = String>) -> Result<&'static str, String> {
    let mut page = DEFAULT_PAGE;
    let mut args = args;
    while let Some(arg) = args.next() {
        let value = if arg == "--page" {
            args.next().ok_or("--page needs a page name")?
        } else if let Some(value) = arg.strip_prefix("--page=") {
            value.to_string()
        } else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        page = canonical_page(&value).ok_or_else(|| {
            format!(
                "unknown page {value:?} (choose from: {})",
                HUB_PAGES.join(", ")
            )
        })?;
    }
    Ok(page)
}

#[cfg(target_os = "macos")]
fn run(page: &str) -> ExitCode {
    match lf_ui::mac::app::run_hub(page) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lf-hub: {error:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn run(_page: &str) -> ExitCode {
    eprintln!("lf-hub: the native Hub is only supported on macOS");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    match parse_page(std::env::args().skip(1)) {
        Ok(page) => run(page),
        Err(message) => {
            eprintln!("lf-hub: {message}");
            ExitCode::from(2)
        }
    }
}
