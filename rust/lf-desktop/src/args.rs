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
    --setup            Open Home with the setup checklist expanded
    --sandbox          Use an isolated example data directory and in-process fake
                       services (never touches your real LocalFlow data or keyring)
    --data-dir <DIR>   Sandbox directory (only with --sandbox; default
                       $XDG_DATA_HOME/localflow-desktop-sandbox). Kept between runs.
    --sandbox-reset    Re-seed the sandbox directory with fresh example data
    -h, --help         Print this help
    -V, --version      Print the version

ENVIRONMENT:
    LOCALFLOW_SANDBOX=1   Same as --sandbox
    LOCALFLOW_CONFIG      Real config file (honoured in real mode, as everywhere in LocalFlow)

This app runs beside the existing LocalFlow launchers; it does not replace
`lf hub`, `lf-hub` or the Python runtime.";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub page: Option<Page>,
    pub unknown_page: Option<String>,
    pub setup: bool,
    pub sandbox: bool,
    pub sandbox_reset: bool,
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
            "--setup" => out.setup = true,
            "--page" => {
                let name = value("--page")?;
                match Page::parse(&name) {
                    Some(page) => out.page = Some(page),
                    None => out.unknown_page = Some(name),
                }
            }
            "--data-dir" => out.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            other => return Err(ArgError(format!("unknown argument: {other}"))),
        }
    }
    if (out.data_dir.is_some() || out.sandbox_reset) && !out.sandbox {
        return Err(ArgError(
            "--data-dir and --sandbox-reset only apply with --sandbox".into(),
        ));
    }
    Ok(out)
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
    }

    #[test]
    fn rejects_unknown_and_misplaced_flags() {
        assert!(p(&["--frobnicate"]).is_err());
        assert!(p(&["--page"]).is_err());
        assert!(p(&["--data-dir", "/tmp/x"]).is_err());
        assert!(p(&["--sandbox-reset"]).is_err());
        let a = p(&["--sandbox", "--data-dir", "/tmp/x", "--sandbox-reset"]).unwrap();
        assert_eq!(a.data_dir, Some(PathBuf::from("/tmp/x")));
        assert!(parse(Vec::new(), true).unwrap().sandbox);
    }
}
