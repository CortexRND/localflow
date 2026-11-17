//! Native LocalFlow Hub shell (macOS only).

use lf_ui::hub_route::canonical_page;
use std::process::ExitCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LaunchOptions {
    page: Option<&'static str>,
    setup: bool,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<LaunchOptions, String> {
    let mut page = None;
    let mut setup = false;
    while let Some(arg) = args.next() {
        if arg == "--setup" {
            setup = true;
            continue;
        }
        let value = if arg == "--page" {
            args.next().ok_or("--page needs a page name")?
        } else if let Some(value) = arg.strip_prefix("--page=") {
            value.to_string()
        } else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        page = Some(canonical_page(&value).unwrap_or("Home"));
    }
    Ok(LaunchOptions { page, setup })
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
    #[cfg(feature = "runtime")]
    if std::env::args().nth(1).as_deref() == Some("--speech-helper") {
        if std::env::args().len() != 2 {
            return ExitCode::from(2);
        }
        return match lf_stt::helper::serve_stdio(|config| {
            lf_stt::runtime::SpeechAdapter::from_config(&config)
                .map(|engine| {
                    std::sync::Arc::new(engine)
                        as std::sync::Arc<dyn lf_runtime::engine::SpeechEngine>
                })
                .map_err(|error| error.to_string())
        }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("speech helper: {error}");
                ExitCode::from(1)
            }
        };
    }
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
        let parse_options = |args: &[&str]| parse_args(args.iter().map(|arg| (*arg).to_string()));
        let parse = |args: &[&str]| parse_options(args).map(|options| options.page);
        assert_eq!(parse(&[]), Ok(None));
        assert_eq!(parse(&["--page", "history"]), Ok(Some("History")));
        assert_eq!(parse(&["--page=Home"]), Ok(Some("Home")));
        assert_eq!(parse(&["--page=dictionary"]), Ok(Some("Dictionary")));
        assert_eq!(parse(&["--page=style"]), Ok(Some("Style")));
        assert_eq!(parse(&["--page=prompts"]), Ok(Some("Prompts")));
        assert_eq!(parse(&["--page", "hot phrases"]), Ok(Some("Hot Phrases")));
        assert_eq!(parse(&["--page=ask"]), Ok(Some("Ask")));
        assert_eq!(parse(&["--page=meetings"]), Ok(Some("Meetings")));
        assert_eq!(parse(&["--page=models"]), Ok(Some("Models")));
        assert_eq!(parse(&["--page=settings"]), Ok(Some("Settings")));
        assert_eq!(parse(&["--page=nope"]), Ok(Some("Home")));
        assert!(parse(&["--page"]).is_err());
        assert_eq!(
            parse_options(&["--setup"]),
            Ok(LaunchOptions {
                page: None,
                setup: true
            })
        );
        assert_eq!(
            parse_options(&["--setup", "--page=History"]),
            Ok(LaunchOptions {
                page: Some("History"),
                setup: true
            })
        );
        assert_eq!(parse(&["--page=setup"]), Ok(Some("Setup")));
        assert!(parse(&["unknown"]).is_err());
    }
}
