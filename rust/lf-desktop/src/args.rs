//! Command-line arguments (mirrors `lf-hub`: unknown page falls back to Home,
//! unknown argument exits 2).

use std::path::PathBuf;

use crate::page::Page;

pub const USAGE: &str = "\
lf-desktop — LocalFlow native desktop app (GPUI preview)

USAGE:
    lf-desktop [OPTIONS]

OPTIONS:
    --page <PAGE>      Open a page: home, history, dictionary, snippets (\"Hot Phrases\"),
                       style, ask, meetings, prompts, models, settings
    --setup            Open the setup wizard (or hand it to the open window)
    --engine           Allow this window to run dictation itself (Start on Home).
                       Needs a build with `--features runtime`; never with --sandbox.
                       Nothing starts until you press Start.
    --sandbox          Use an isolated example data directory and in-process fake
                       services (never touches your real LocalFlow data or keyring)
    --data-dir <DIR>   Sandbox directory (only with --sandbox; default
                       $XDG_DATA_HOME/localflow-desktop-sandbox). Kept between runs.
    --sandbox-reset    Re-seed the sandbox directory with fresh example data
                       (refused while that sandbox is open in another window)
    --sandbox-exercise Write once to each sandbox store (Settings, a provider key,
                       Hot Phrases, History, Prompts), print what was written and
                       exit without a window (only with --sandbox)
    -h, --help         Print this help
    -V, --version      Print the version

ENVIRONMENT:
    LOCALFLOW_SANDBOX=1   Same as --sandbox
    LOCALFLOW_CONFIG      Real config file (honoured in real mode, as everywhere in LocalFlow)

EXIT STATUS (launch-status: 1):
    0   The window closed, or the request was handed to the open window
    1   No window could be opened (display, lock or data error)
    2   Invalid arguments

One window runs per data folder: a second launch sends its page to it.
This app runs beside the existing LocalFlow windows. `lf hub`, `lf phrases`
and `lf setup` open it only when asked (`--gpui` or LOCALFLOW_UI=gpui); on
Linux, `lf-hub` opens it. It does not replace the Python runtime.";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub page: Option<Page>,
    pub unknown_page: Option<String>,
    pub setup: bool,
    pub engine: bool,
    pub sandbox: bool,
    pub sandbox_reset: bool,
    pub sandbox_exercise: bool,
    pub data_dir: Option<PathBuf>,
    pub help: bool,
    pub version: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgError(pub String);

pub fn parse(args: impl IntoIterator<Item = String>, env_sandbox: bool) -> Result<Args, ArgError> {
    let mut out = Args {
        sandbox: env_sandbox,
        ..Args::default()
    };
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, ArgError> {
            inline
                .clone()
                .or_else(|| iter.next())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| ArgError(format!("{name} needs a value")))
        };
        match flag.as_str() {
            "-h" | "--help" => out.help = true,
            "-V" | "--version" => out.version = true,
            "--sandbox" => out.sandbox = true,
            "--sandbox-reset" => out.sandbox_reset = true,
            "--sandbox-exercise" => out.sandbox_exercise = true,
            "--setup" => out.setup = true,
            "--engine" => out.engine = true,
            "--page" => {
                let name = value("--page")?;
                if lf_ui::hub_route::canonical_page(&name) == Some("Setup") {
                    out.setup = true;
                    continue;
                }
                match Page::parse(&name) {
                    Some(page) => out.page = Some(page),
                    None => out.unknown_page = Some(name),
                }
            }
            "--data-dir" => out.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            other => return Err(ArgError(format!("unknown argument: {other}"))),
        }
    }
    if (out.data_dir.is_some() || out.sandbox_reset || out.sandbox_exercise) && !out.sandbox {
        return Err(ArgError(
            "--data-dir, --sandbox-reset and --sandbox-exercise only apply with --sandbox".into(),
        ));
    }
    Ok(out)
}

/// Refuse `--engine` where it can't be honoured, instead of pretending.
pub fn check_engine(args: &Args, runtime_compiled: bool) -> Result<(), ArgError> {
    if !args.engine {
        return Ok(());
    }
    if args.sandbox {
        return Err(ArgError(
            "--engine can't be used with --sandbox: the sandbox never starts a microphone, \
             keyboard listener or engine"
                .into(),
        ));
    }
    if !runtime_compiled {
        return Err(ArgError(format!(
            "--engine needs a build with the dictation engine. Rebuild with:\n    {}",
            crate::controllers::capabilities::REBUILD
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, ArgError> {
        parse(args.iter().map(|s| s.to_string()), false)
    }

    #[test]
    fn parses_pages_and_flags() {
        let a = p(&["--sandbox", "--page", "history"]).unwrap();
        assert!(a.sandbox);
        assert_eq!(a.page, Some(Page::History));
        assert_eq!(
            p(&["--page=Hot Phrases"]).unwrap().page,
            Some(Page::Snippets)
        );
        assert_eq!(
            p(&["--page", "snippets"]).unwrap().page,
            Some(Page::Snippets)
        );
        assert_eq!(p(&["--page", "Models"]).unwrap().page, Some(Page::Models));
        let unknown = p(&["--page", "nowhere"]).unwrap();
        assert_eq!(unknown.page, None);
        assert_eq!(unknown.unknown_page.as_deref(), Some("nowhere"));
        assert!(p(&["--help"]).unwrap().help);
        assert!(p(&["--setup"]).unwrap().setup);
        let setup_page = p(&["--page", "SETUP"]).unwrap();
        assert!(setup_page.setup && setup_page.page.is_none());
    }

    #[test]
    fn rejects_unknown_and_misplaced_flags() {
        assert!(p(&["--frobnicate"]).is_err());
        assert!(p(&["--page"]).is_err());
        assert!(p(&["--data-dir", "/tmp/x"]).is_err());
        assert!(p(&["--sandbox-reset"]).is_err());
        assert!(p(&["--sandbox-exercise"]).is_err());
        assert!(
            p(&["--sandbox", "--sandbox-exercise"])
                .unwrap()
                .sandbox_exercise
        );
        let a = p(&["--sandbox", "--data-dir", "/tmp/x", "--sandbox-reset"]).unwrap();
        assert_eq!(a.data_dir, Some(PathBuf::from("/tmp/x")));
        assert!(parse(Vec::new(), true).unwrap().sandbox);
    }

    #[test]
    fn engine_is_refused_in_the_sandbox_and_without_the_runtime_feature() {
        let engine = p(&["--engine"]).unwrap();
        assert!(engine.engine);
        assert!(check_engine(&engine, true).is_ok());
        let err = check_engine(&engine, false).unwrap_err();
        assert!(err.0.contains("--features runtime"), "{}", err.0);
        let sandboxed = p(&["--sandbox", "--engine"]).unwrap();
        assert!(check_engine(&sandboxed, true)
            .unwrap_err()
            .0
            .contains("sandbox"));
        // LOCALFLOW_SANDBOX counts too.
        let env = parse(vec!["--engine".to_string()], true).unwrap();
        assert!(check_engine(&env, true).is_err());
        assert!(check_engine(&p(&[]).unwrap(), false).is_ok());
    }

    #[test]
    fn help_promises_the_launch_status_the_launchers_probe_for() {
        // `lf --gpui` and Linux `lf-hub` only trust exit 0 from a build whose
        // --help passes this probe; Scope 3 builds (no marker) are refused.
        assert!(lf_ui::desktop_launch::help_has_interface(USAGE));
        let scope3 = USAGE.replace(lf_ui::desktop_launch::LAUNCH_STATUS_MARKER, "EXIT STATUS:");
        assert!(!lf_ui::desktop_launch::help_has_interface(&scope3));
    }
}
