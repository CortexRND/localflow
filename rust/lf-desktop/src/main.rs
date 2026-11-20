//! `lf-desktop`: the LocalFlow native desktop app (GPUI).

use std::process::ExitCode;

use lf_desktop::app::{self, Launch};
use lf_desktop::args::{self, USAGE};
use lf_desktop::data::{default_sandbox_dir, DataRoot, Services};
use lf_desktop::page::Page;
use lf_desktop::sandbox;

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
    let root = if args.sandbox {
        let dir = args.data_dir.clone().unwrap_or_else(default_sandbox_dir);
        match sandbox::prepare(&dir, args.sandbox_reset) {
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
    let services = match Services::open(root) {
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
    app::run(Launch {
        services,
        page: args.page.unwrap_or(Page::Home),
        setup: args.setup,
        notice,
    });
    ExitCode::SUCCESS
}
