//! `hub`: the Python `lf hub` (`cli.hub_command`). The legacy window is
//! `<python> -m localflow.hub_window --page <Page>` from the Python install,
//! started without waiting, like Python's `subprocess.Popen`.

use std::process::Command;

use anyhow::Result;

use crate::desktop_launch::{flag_choice, Launcher};
use crate::pyfmt::str_repr;
use crate::{fail, python, usage, Io};

/// `localflow.hub_route.HUB_PAGES`.
pub const HUB_PAGES: [&str; 10] = [
    "Home",
    "Ask",
    "Hot Phrases",
    "Dictionary",
    "Prompts",
    "Meetings",
    "History",
    "Style",
    "Models",
    "Settings",
];
pub const DEFAULT_PAGE: &str = "Home";
const LEGACY_MODULE: &str = "localflow.hub_window";

/// click's `Choice(HUB_PAGES, case_sensitive=False)`: the canonical name.
pub fn page_choice(value: &str) -> Result<&'static str> {
    let fold = caseless::default_case_fold_str;
    let folded = fold(value);
    if let Some(page) = HUB_PAGES.iter().find(|page| fold(page) == folded) {
        return Ok(page);
    }
    let choices: Vec<String> = HUB_PAGES.iter().map(|page| str_repr(&fold(page))).collect();
    usage(format!(
        "Invalid value for '--page': {} is not one of {}.",
        str_repr(value),
        choices.join(", ")
    ))
}

pub(crate) fn run(page: Option<String>, gpui: bool, legacy: bool, io: &mut Io<'_>) -> Result<()> {
    let explicit = page.as_deref().map(page_choice).transpose()?;
    let flag = flag_choice(gpui, legacy)?;
    let page = explicit.unwrap_or(DEFAULT_PAGE);
    // Bare `hub` sends no page to LocalFlow Preview (opened only once LocalFlow
    // is set up, so never its first-run wizard); the legacy Hub always gets
    // the canonical page.
    Launcher::system().launch(flag, explicit, &mut |_| open_legacy(page), io)
}

fn open_legacy(page: &str) -> Result<()> {
    let Some(python) = python::find_interpreter() else {
        return fail(python::NO_INSTALL);
    };
    match Command::new(python)
        .args(["-m", LEGACY_MODULE, "--page", page])
        .spawn()
    {
        Ok(_) => Ok(()),
        Err(_) => fail(python::NO_INSTALL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CliError;

    #[test]
    fn pages_are_case_insensitive_and_canonical() {
        assert_eq!(page_choice("hot phrases").unwrap(), "Hot Phrases");
        assert_eq!(page_choice("SETTINGS").unwrap(), "Settings");
        assert_eq!(page_choice("ſtyle").unwrap(), "Style");
        for bad in ["hot  phrases", " Home", "", "x"] {
            let err = page_choice(bad).unwrap_err();
            assert!(
                matches!(err.downcast_ref(), Some(CliError::Usage(_))),
                "{bad:?}"
            );
        }
        assert_eq!(
            page_choice("x").unwrap_err().to_string(),
            "Invalid value for '--page': 'x' is not one of 'home', 'ask', 'hot phrases', \
             'dictionary', 'prompts', 'meetings', 'history', 'style', 'models', 'settings'."
        );
    }

    #[test]
    fn pages_match_lf_ui() {
        // A Windows checkout may have CRLF line endings.
        let source = include_str!("../../lf-ui/src/hub_route.rs").replace("\r\n", "\n");
        let quoted: Vec<String> = HUB_PAGES
            .iter()
            .map(|page| format!("    \"{page}\",\n"))
            .collect();
        assert!(source.contains(&format!(
            "pub const HUB_PAGES: [&str; 10] = [\n{}];",
            quoted.concat()
        )));
        assert!(source.contains(&format!(
            "pub const DEFAULT_PAGE: &str = \"{DEFAULT_PAGE}\";"
        )));
    }
}
