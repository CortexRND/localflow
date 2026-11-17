//! Native LocalFlow Hub shell (macOS only).

use lf_ui::hub_route::{canonical_page, ported_page};
use std::process::ExitCode;

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<&'static str>, String> {
    let mut page = None;
    while let Some(arg) = args.next() {
        let value = if arg == "--page" {
            args.next().ok_or("--page needs a page name")?
        } else if let Some(value) = arg.strip_prefix("--page=") {
            value.to_string()
        } else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        page = Some(ported_page(canonical_page(&value)));
    }
    Ok(page)
}

#[cfg(target_os = "macos")]
fn run(page: Option<&str>) -> ExitCode {
    match lf_ui::mac::app::run_hub(page) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lf-hub: {error:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn run(_page: Option<&str>) -> ExitCode {
    eprintln!("lf-hub: the native Hub is only supported on macOS");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)) {
        Ok(page) => run(page),
        Err(message) => {
            eprintln!("lf-hub: {message}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_routes_only_to_ported_pages() {
        let parse = |args: &[&str]| parse_args(args.iter().map(|arg| (*arg).to_string()));
        assert_eq!(parse(&[]), Ok(None));
        assert_eq!(parse(&["--page", "history"]), Ok(Some("History")));
        assert_eq!(parse(&["--page=Home"]), Ok(Some("Home")));
        assert_eq!(parse(&["--page=dictionary"]), Ok(Some("Dictionary")));
        assert_eq!(parse(&["--page=style"]), Ok(Some("Style")));
        assert_eq!(parse(&["--page=prompts"]), Ok(Some("Prompts")));
        assert_eq!(parse(&["--page", "hot phrases"]), Ok(Some("Home")));
        assert_eq!(parse(&["--page=nope"]), Ok(Some("Home")));
        assert!(parse(&["--page"]).is_err());
        assert!(parse(&["--setup"]).is_err());
        assert!(parse(&["unknown"]).is_err());
    }
}
