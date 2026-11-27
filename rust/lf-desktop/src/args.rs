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
                       exit without a window (only with --sandbox; refused while
                       that sandbox is open in another window)
    --sandbox-fixture <SCENE>
                       Screenshot scene over example data: speakers-saved,
                       speakers-recent, speakers-settings, speakers-people,
                       agents-page, agents-states, agents-detail,
                       agents-launcher, agents-launcher-typed,
                       agents-launcher-secret, agents-launcher-empty,
                       agents-settings, agents-settings-key or
                       agents-settings-trigger (only with
                       --sandbox and --data-dir; seeds a new or reset
                       directory only; agents-launcher* open the launcher)
    --agents-launcher  Open the agent launcher (its own small window; pressed
                       again while open, it closes). Works with the main
                       window closed; never with --page, --setup or --engine.
    --agent-task <TEXT>, --agent-provider <ID>, --agent-workspace <ALIAS>
                       Prefill the launcher (only with --agents-launcher)
    --agent-session <ID>
                       With --page agents: select this session
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
This app runs beside the existing LocalFlow windows. On Linux and macOS,
`lf hub` and `lf phrases` open it by default once it is installed and
LocalFlow is set up; `lf setup` opens it only with `--gpui` or
LOCALFLOW_UI=gpui, and nothing opens it from `lf hub` / `lf phrases` before
LocalFlow is set up (`--legacy` or LOCALFLOW_UI=legacy opens the legacy window).
On Linux, `lf-hub` opens it. It does not replace the Python runtime.";

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
    pub sandbox_fixture: Option<crate::sandbox::Fixture>,
    /// `--agents-launcher` (also set by a launcher scene).
    pub agents_launcher: bool,
    /// Internal (Run again): show the draft file's prefill, and if a
    /// launcher is open ask it to show it rather than close.
    pub agent_prefill: bool,
    pub agent_task: Option<String>,
    pub agent_provider: Option<String>,
    pub agent_workspace: Option<String>,
    pub agent_session: Option<String>,
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
                // The last page wins as one value: a known page drops an
                // earlier unknown name, and an unknown name an earlier page.
                match Page::parse(&name) {
                    Some(page) => {
                        out.page = Some(page);
                        out.unknown_page = None;
                    }
                    None => {
                        out.page = None;
                        out.unknown_page = Some(name);
                    }
                }
            }
            "--data-dir" => out.data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--sandbox-fixture" => {
                let name = value("--sandbox-fixture")?;
                out.sandbox_fixture =
                    Some(crate::sandbox::Fixture::parse(&name).ok_or_else(|| {
                        ArgError(format!("unknown --sandbox-fixture scene: {name}"))
                    })?);
            }
            "--agents-launcher" => out.agents_launcher = true,
            "--agent-prefill" => out.agent_prefill = true,
            "--agent-task" => out.agent_task = Some(value("--agent-task")?),
            "--agent-provider" => out.agent_provider = Some(value("--agent-provider")?),
            "--agent-workspace" => out.agent_workspace = Some(value("--agent-workspace")?),
            "--agent-session" => out.agent_session = Some(value("--agent-session")?),
            other => return Err(ArgError(format!("unknown argument: {other}"))),
        }
    }
    check_agents(&mut out)?;
    // Never the default sandbox directory: the fixture seeds only where asked.
    if out.sandbox_fixture.is_some()
        && (!out.sandbox || out.data_dir.is_none() || out.sandbox_exercise)
    {
        return Err(ArgError(
            "--sandbox-fixture needs --sandbox and --data-dir (and no --sandbox-exercise)".into(),
        ));
    }
    if (out.data_dir.is_some() || out.sandbox_reset || out.sandbox_exercise) && !out.sandbox {
        return Err(ArgError(
            "--data-dir, --sandbox-reset and --sandbox-exercise only apply with --sandbox".into(),
        ));
    }
    Ok(out)
}

/// The launcher's combinations: a launcher scene implies the launcher;
/// prefill flags need it; it opens no Hub page, setup, engine, reset or
/// exercise; `--agent-session` needs `--page agents` and a v1 id.
fn check_agents(out: &mut Args) -> Result<(), ArgError> {
    if let Some(f) = out.sandbox_fixture {
        if f.is_launcher() {
            out.agents_launcher = true;
        } else if out.agents_launcher {
            return Err(ArgError(format!(
                "--agents-launcher can't open the {} scene",
                f.name()
            )));
        }
    }
    let prefill = out.agent_task.is_some()
        || out.agent_provider.is_some()
        || out.agent_workspace.is_some()
        || out.agent_prefill;
    if prefill && !out.agents_launcher {
        return Err(ArgError(
            "--agent-task, --agent-provider, --agent-workspace and --agent-prefill \
             only apply with --agents-launcher"
                .into(),
        ));
    }
    if out.agents_launcher
        && (out.page.is_some()
            || out.unknown_page.is_some()
            || out.setup
            || out.engine
            || out.sandbox_reset
            || out.sandbox_exercise
            || out.agent_session.is_some())
    {
        return Err(ArgError(
            "--agents-launcher opens only the launcher: drop --page, --setup, --engine, \
             --agent-session, --sandbox-reset and --sandbox-exercise"
                .into(),
        ));
    }
    if let Some(id) = &out.agent_session {
        let agents = out
            .unknown_page
            .as_deref()
            .is_some_and(crate::page::Page::is_agents_name);
        if !agents || !crate::controllers::agents::is_session_id(id) {
            return Err(ArgError(
                "--agent-session needs --page agents and a 12-character session id".into(),
            ));
        }
    }
    Ok(())
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
    fn the_last_page_wins_with_its_unknown_name() {
        let known = p(&["--page", "typo", "--page", "home"]).unwrap();
        assert_eq!(known.page, Some(Page::Home));
        assert_eq!(known.unknown_page, None);
        let unknown = p(&["--page", "history", "--page", "typo"]).unwrap();
        assert_eq!(unknown.page, None);
        assert_eq!(unknown.unknown_page.as_deref(), Some("typo"));
        // Setup still wins over any page, in either order.
        for args in [
            ["--page", "typo", "--page", "setup"],
            ["--page", "setup", "--page", "typo"],
        ] {
            assert!(p(&args).unwrap().setup);
        }
        assert!(p(&["--setup", "--page", "typo"]).unwrap().setup);
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
    fn the_speaker_fixture_needs_an_explicit_sandbox_directory() {
        use crate::sandbox::Fixture;
        let a = p(&[
            "--sandbox",
            "--data-dir",
            "/tmp/x",
            "--sandbox-fixture",
            "speakers-people",
        ])
        .unwrap();
        assert_eq!(a.sandbox_fixture, Some(Fixture::SpeakersPeople));
        assert!(p(&["--sandbox", "--sandbox-fixture", "speakers-saved"]).is_err());
        assert!(p(&["--sandbox-fixture=speakers-saved", "--data-dir", "/tmp/x"]).is_err());
        assert!(p(&["--sandbox", "--data-dir=/tmp/x", "--sandbox-fixture=nope"]).is_err());
        assert!(p(&[
            "--sandbox",
            "--data-dir=/tmp/x",
            "--sandbox-exercise",
            "--sandbox-fixture=speakers-saved"
        ])
        .is_err());
        assert_eq!(p(&["--sandbox"]).unwrap().sandbox_fixture, None);
    }

    #[test]
    fn the_launcher_flags_combine_only_as_designed() {
        use crate::sandbox::Fixture;
        let a = p(&[
            "--agents-launcher",
            "--agent-task",
            "Fix it",
            "--agent-provider",
            "devin",
        ])
        .unwrap();
        assert!(a.agents_launcher);
        assert_eq!(a.agent_task.as_deref(), Some("Fix it"));
        assert_eq!(a.agent_provider.as_deref(), Some("devin"));
        assert!(
            p(&["--agents-launcher", "--agent-prefill"])
                .unwrap()
                .agent_prefill
        );
        for bad in [
            &["--agent-task", "x"][..],
            &["--agent-prefill"],
            &["--agents-launcher", "--page", "history"],
            &["--agents-launcher", "--page", "agents"],
            &["--agents-launcher", "--setup"],
            &["--agents-launcher", "--engine"],
            &["--agents-launcher", "--agent-task", ""],
            &["--sandbox", "--agents-launcher", "--sandbox-reset"],
            &["--sandbox", "--agents-launcher", "--sandbox-exercise"],
            &["--agent-session", "0123456789ab"],
            &["--page", "agents", "--agent-session", "nope"],
            &[
                "--sandbox",
                "--data-dir",
                "/tmp/x",
                "--sandbox-fixture",
                "agents-page",
                "--agents-launcher",
            ],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
        let a = p(&["--page", "agents", "--agent-session", "0123456789ab"]).unwrap();
        assert_eq!(a.agent_session.as_deref(), Some("0123456789ab"));
        let scene = p(&[
            "--sandbox",
            "--data-dir",
            "/tmp/x",
            "--sandbox-fixture",
            "agents-launcher-typed",
        ])
        .unwrap();
        assert!(scene.agents_launcher, "a launcher scene opens the launcher");
        assert_eq!(scene.sandbox_fixture, Some(Fixture::AgentsLauncherTyped));
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
