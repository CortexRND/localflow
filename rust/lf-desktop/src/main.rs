//! `lf-desktop`: the LocalFlow native desktop app (GPUI).

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::process::ExitCode;

use lf_desktop::app::{self, Launch};
use lf_desktop::args::{self, USAGE};
use lf_desktop::controllers::capabilities::Capabilities;
use lf_desktop::controllers::instance::{Request, Route};
use lf_desktop::data::{default_sandbox_dir, DataRoot, Services};
use lf_desktop::launch::{self, Launched};
use lf_desktop::page::Page;

fn main() -> ExitCode {
    lf_desktop::console::attach_parent();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let env_sandbox = std::env::var("LOCALFLOW_SANDBOX").is_ok_and(|v| v == "1" || v == "true");
    #[cfg(feature = "runtime")]
    if let Some(code) = speech_helper(env_sandbox) {
        return code;
    }
    let args = match args::parse(std::env::args().skip(1), env_sandbox) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("lf-desktop: {}\n\n{USAGE}", err.0);
            return ExitCode::from(2);
        }
    };
    if args.help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("lf-desktop {}", lf_desktop::VERSION);
        return ExitCode::SUCCESS;
    }
    if let Err(err) = args::check_engine(&args, cfg!(feature = "runtime")) {
        eprintln!("lf-desktop: {}", err.0);
        return ExitCode::from(2);
    }
    if args.agents_launcher {
        return agents_launcher(&args);
    }
    let fixture = args.sandbox_fixture;
    // A fixture scene opens its own page; `people` resolves once the config
    // is known (it has no Hub route, so a hand-off carries the unknown name).
    let (page, unknown_page) = match fixture {
        Some(f) if f.page() == Page::People => (None, Some("people".to_string())),
        // Agents has no Hub route either: start on Home; the scene's action
        // opens Agents once the window knows the (fake) service has it on.
        Some(f) if f.page() == Page::Agents => (Some(Page::Home), None),
        Some(f) => (Some(f.page()), None),
        // `--page agents --agent-session <id>` hands off as `agents/<id>`.
        None => match &args.agent_session {
            Some(id) => (None, Some(format!("agents/{id}"))),
            None => (args.page, args.unknown_page.clone()),
        },
    };
    let request = Request {
        route: if args.setup && fixture.is_none() {
            Route::Setup
        } else {
            Route::Page(page.unwrap_or(Page::Home))
        },
        unknown_page,
    };
    let people_request = request.is_unknown_home()
        && request
            .unknown_page
            .as_deref()
            .is_some_and(Page::is_people_name);
    // `--page agents` resolves once the window has asked the service
    // whether Agents is on, so no notice is decided here.
    let agents_settings = request.is_unknown_home()
        && request
            .unknown_page
            .as_deref()
            .is_some_and(Page::is_agents_settings_name);
    let agents_request = agents_settings
        || request.is_unknown_home()
            && request
                .unknown_page
                .as_deref()
                .is_some_and(Page::is_agents_name);
    let mut notice = request.notice();
    let launched = if args.sandbox {
        let dir = args.data_dir.clone().unwrap_or_else(default_sandbox_dir);
        launch::sandbox_with(
            &dir,
            args.sandbox_reset,
            fixture,
            (!args.sandbox_exercise).then(|| request.clone()),
        )
    } else {
        match DataRoot::real() {
            Ok(root) => launch::real(root, request.clone()),
            Err(err) => Err(format!(
                "couldn't resolve LocalFlow's data locations: {err:#}"
            )),
        }
    };
    let (root, instance) = match launched {
        Ok(Launched::Window(root, instance)) => (*root, instance),
        Ok(Launched::HandedOff) => {
            if people_request {
                // The open window decides: People while speaker recognition
                // is on there, otherwise Home with the notice.
                eprintln!(
                    "lf-desktop: LocalFlow is already open; sent it to People \
                     (Home if speaker recognition is off)."
                );
                return ExitCode::SUCCESS;
            }
            if agents_settings {
                eprintln!("lf-desktop: LocalFlow is already open; sent it to Settings → Agents.");
                return ExitCode::SUCCESS;
            }
            if agents_request {
                eprintln!(
                    "lf-desktop: LocalFlow is already open; sent it to Agents \
                     (Home if its agent service doesn't have Agents on)."
                );
                return ExitCode::SUCCESS;
            }
            if let Some(n) = &notice {
                eprintln!("lf-desktop: {n}");
            }
            eprintln!(
                "lf-desktop: LocalFlow is already open; sent it to {}.",
                request.route.id()
            );
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("lf-desktop: {err}");
            return ExitCode::from(1);
        }
    };
    let caps = Capabilities::new(root.is_sandbox(), args.engine);
    let services = match Services::open_with(root, caps) {
        Ok(services) => services,
        Err(err) => {
            eprintln!("lf-desktop: {err:#}");
            return ExitCode::from(1);
        }
    };
    if args.sandbox_exercise {
        return match lf_desktop::sandbox::exercise(&services) {
            Ok(done) => {
                for line in done {
                    println!("{line}");
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("lf-desktop: sandbox exercise failed: {err:#}");
                ExitCode::from(1)
            }
        };
    }
    // `--page people` opens People only while it is turned on; otherwise it
    // is an unknown page like any other (Home and the notice).
    let start_page =
        if people_request && lf_desktop::controllers::people::enabled(&services.config()) {
            notice = None;
            Page::People
        } else {
            page.unwrap_or(Page::Home)
        };
    if agents_request {
        // The window shows the notice itself if Agents turns out to be off.
        notice = None;
    }
    if let Some(n) = &notice {
        eprintln!("lf-desktop: {n}");
    }
    let configured = lf_desktop::controllers::setup::configured(&services.root);
    let status = app::run(Launch {
        services,
        page: start_page,
        setup: fixture.is_none() && opens_setup(&request, configured),
        notice,
        instance,
        fixture,
        agents_request,
        agent_session: args.agent_session.clone(),
        agents_settings,
    });
    // Non-zero when no window opened, so `lf --gpui` falls back instead of
    // mistaking it for a hand-off (see USAGE's EXIT STATUS).
    ExitCode::from(status.exit_code())
}

/// `--agents-launcher`: the launcher's own process. It takes the
/// launcher's lock (never the Hub's); a second launch hands the open one a
/// toggle (close) or, for Run again, a "show the prefill", and exits 0.
fn agents_launcher(args: &args::Args) -> ExitCode {
    use lf_desktop::controllers::agents_launcher::{self as model, LauncherClaim, Signal};
    use lf_desktop::launcher;
    let root = if args.sandbox {
        let dir = args.data_dir.clone().unwrap_or_else(default_sandbox_dir);
        // An existing sandbox is used as is (never reset from here).
        match lf_desktop::sandbox::check_dir(&dir)
            .and_then(|_| lf_desktop::sandbox::prepare_with(&dir, false, args.sandbox_fixture))
        {
            Ok(root) => root,
            Err(err) => {
                eprintln!("lf-desktop: sandbox: {err:#}");
                return ExitCode::from(1);
            }
        }
    } else {
        match DataRoot::real() {
            Ok(root) => root,
            Err(err) => {
                eprintln!("lf-desktop: couldn't resolve LocalFlow's data locations: {err:#}");
                return ExitCode::from(1);
            }
        }
    };
    let signal = if args.agent_prefill {
        Signal::Show
    } else {
        Signal::Toggle
    };
    let instance = match model::claim(&model::launcher_dir(&root), signal) {
        Ok(LauncherClaim::Owner(instance)) => instance,
        Ok(LauncherClaim::HandedOff) => {
            eprintln!("lf-desktop: the agent launcher is already open; sent it the request.");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!("lf-desktop: {err}");
            return ExitCode::from(1);
        }
    };
    let fixture = args.sandbox_fixture;
    let backend = match fixture {
        Some(f) => {
            lf_desktop::sandbox::set_scene(f);
            launcher::scene_backend(&root, f)
        }
        None => launcher::backend_for(&root),
    };
    let prefill = match fixture {
        Some(f) => launcher::scene_prefill(f),
        None => args.agent_task.clone().map(|text| model::Draft {
            text,
            provider: args.agent_provider.clone().map(|p| (p, String::new())),
            workspace: args.agent_workspace.clone(),
            saved_at: lf_desktop::controllers::agents::now(),
        }),
    };
    match launcher::run(launcher::LauncherLaunch {
        root,
        backend,
        instance: Some(instance),
        prefill,
        fixture,
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lf-desktop: {err}");
            ExitCode::from(1)
        }
    }
}

/// The window starts in setup by the same policy as the Hub
/// (`hub_route::plan_initial_route`): for an explicit Setup request, or for
/// any page before LocalFlow is set up (no setup marker). The exception is an
/// unknown `--page`, which shows Home with its notice.
fn opens_setup(request: &Request, configured: bool) -> bool {
    !request.is_unknown_home()
        && matches!(
            lf_ui::hub_route::plan_initial_route(configured, Some(request.route.id())),
            Ok(lf_ui::hub_route::InitialRoute::Setup { .. })
        )
}

/// The same-executable speech helper (`lf-desktop --speech-helper`).
///
/// On macOS `lf_stt::runtime::hub_speech` runs the speech model in a child
/// process spawned as `current_exe() --speech-helper`, exactly as `lf-hub`
/// does, so the packaged app keeps one signed identity. It is dispatched
/// before argument parsing, data resolution or any window, takes no other
/// argument and is never part of the sandbox (which never starts a speech
/// engine). Returns `None` when this process is not the helper. Not listed in
/// `USAGE`: it is an internal protocol endpoint, not a user command.
#[cfg(feature = "runtime")]
fn speech_helper(env_sandbox: bool) -> Option<ExitCode> {
    let mut argv = std::env::args_os().skip(1);
    if argv.next().as_deref() != Some(std::ffi::OsStr::new("--speech-helper")) {
        return None;
    }
    if argv.next().is_some() {
        eprintln!("lf-desktop: --speech-helper takes no other arguments");
        return Some(ExitCode::from(2));
    }
    if env_sandbox {
        eprintln!(
            "lf-desktop: the speech helper can't run in the sandbox: \
             the sandbox never starts a speech engine (LOCALFLOW_SANDBOX is set)"
        );
        return Some(ExitCode::from(2));
    }
    log::info!("starting speech helper");
    Some(
        // A cache-only request (History retry) never downloads a model.
        match lf_stt::helper::serve_stdio_with(|config, cache_only| {
            if cache_only {
                lf_stt::runtime::SpeechAdapter::from_config_cached(&config)
            } else {
                lf_stt::runtime::SpeechAdapter::from_config(&config)
            }
            .map(|engine| {
                std::sync::Arc::new(engine) as std::sync::Arc<dyn lf_runtime::engine::SpeechEngine>
            })
            .map_err(|error| error.to_string())
        }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("speech helper: {error}");
                ExitCode::from(1)
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown(route: Route) -> Request {
        Request {
            route,
            unknown_page: Some("NoSuchPage".into()),
        }
    }

    #[test]
    fn setup_opens_on_first_run_or_when_asked_but_not_for_an_unknown_page() {
        let home = Request::from(Route::Page(Page::Home));
        assert!(opens_setup(&home, false), "bare first run");
        assert!(!opens_setup(&home, true), "configured Home");
        assert!(
            !opens_setup(&unknown(Route::Page(Page::Home)), false),
            "an unknown page shows Home"
        );
        for configured in [false, true] {
            assert!(opens_setup(&unknown(Route::Setup), configured));
            assert!(opens_setup(&Route::Setup.into(), configured));
        }
    }
}
