//! `lf-desktop`: the LocalFlow native desktop app (GPUI).

use std::process::ExitCode;

use lf_desktop::app::{self, Launch};
use lf_desktop::args::{self, USAGE};
use lf_desktop::controllers::capabilities::Capabilities;
use lf_desktop::controllers::instance::Route;
use lf_desktop::data::{default_sandbox_dir, DataRoot, Services};
use lf_desktop::launch::{self, Launched};
use lf_desktop::page::Page;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let env_sandbox = std::env::var("LOCALFLOW_SANDBOX").is_ok_and(|v| v == "1" || v == "true");
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
    let route = if args.setup {
        Route::Setup
    } else {
        Route::Page(args.page.unwrap_or(Page::Home))
    };
    let launched = if args.sandbox {
        let dir = args.data_dir.clone().unwrap_or_else(default_sandbox_dir);
        launch::sandbox(&dir, args.sandbox_reset, route)
    } else {
        match DataRoot::real() {
            Ok(root) => launch::real(root, route),
            Err(err) => Err(format!(
                "couldn't resolve LocalFlow's data locations: {err:#}"
            )),
        }
    };
    let (root, instance) = match launched {
        Ok(Launched::Window(root, instance)) => (*root, instance),
        Ok(Launched::HandedOff) => {
            eprintln!(
                "lf-desktop: LocalFlow is already open; sent it to {}.",
                route.id()
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
    let notice = args
        .unknown_page
        .as_ref()
        .map(|p| format!("There's no page called “{p}”, so Home is shown."));
    if let Some(n) = &notice {
        eprintln!("lf-desktop: {n}");
    }
    // First run (no setup marker, no explicit page) opens the wizard too,
    // with the same policy as the Hub (`hub_route::plan_initial_route`).
    let configured = lf_desktop::controllers::setup::configured(&services.root);
    let first_run = matches!(
        lf_ui::hub_route::plan_initial_route(configured, args.page.map(|p| p.route())),
        Ok(lf_ui::hub_route::InitialRoute::Setup { .. })
    );
    app::run(Launch {
        services,
        page: args.page.unwrap_or(Page::Home),
        setup: args.setup || first_run,
        notice,
        instance,
    });
    ExitCode::SUCCESS
}
