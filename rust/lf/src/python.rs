//! The Python installation `lf-rs` runs next to. `stt` asks it about speech
//! runtimes; `agent install` and `setup install-app` point launchers at a
//! Python LocalFlow install the way the Python CLI uses `sys.executable`.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Interpreter file name the discovery looks for.
pub(crate) const PYTHON: &str = if cfg!(windows) {
    "python.exe"
} else {
    "python3"
};

/// The `localflow` console script of a Python LocalFlow install.
const SCRIPT: &str = if cfg!(windows) {
    "localflow.exe"
} else {
    "localflow"
};

/// Why `agent install` and `setup install-app` refuse to run.
pub(crate) const NO_INSTALL: &str =
    "no Python LocalFlow install found; activate its virtualenv or put its lf on PATH";

/// Interpreters to try, best first: `$VIRTUAL_ENV`'s, the one next to the
/// `lf` launcher found on PATH (symlinks resolved), then bare `python3` and
/// `python` for the OS to look up on PATH. Only the launcher candidate is
/// checked to exist.
pub(crate) fn candidates() -> Vec<PathBuf> {
    candidates_in(
        env::var_os("VIRTUAL_ENV").as_deref(),
        &env::var_os("PATH").unwrap_or_default(),
    )
}

fn candidates_in(venv: Option<&OsStr>, path: &OsStr) -> Vec<PathBuf> {
    venv_python(venv)
        .into_iter()
        .chain(beside_launchers(path).next())
        .chain([PathBuf::from(PYTHON), PathBuf::from("python")])
        .collect()
}

fn venv_python(venv: Option<&OsStr>) -> Option<PathBuf> {
    venv.map(|root| {
        Path::new(root)
            .join(if cfg!(windows) { "Scripts" } else { "bin" })
            .join(PYTHON)
    })
}

/// The interpreter next to each `lf` launcher on PATH (symlinks resolved).
fn beside_launchers(path: &OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    let launcher = if cfg!(windows) { "lf.exe" } else { "lf" };
    env::split_paths(path).filter_map(move |dir| {
        let binary = dir.join(launcher).canonicalize().ok()?;
        let python = binary.with_file_name(PYTHON);
        python.is_file().then_some(python)
    })
}

/// The `localflow` script installed for `python`: next to it, or in
/// `Scripts\` below a Windows interpreter that is not in a virtualenv.
fn script_of(python: &Path) -> Option<PathBuf> {
    let beside = python.with_file_name(SCRIPT);
    let scripts = python.with_file_name("Scripts").join(SCRIPT);
    if beside.is_file() {
        Some(beside)
    } else {
        (cfg!(windows) && scripts.is_file()).then_some(scripts)
    }
}

/// Interpreters that exist, best first: the venv's, the one next to any
/// `lf` on PATH (another program may be called `lf` too), then `python3`
/// and `python` in each PATH directory.
fn interpreters_in<'a>(
    venv: Option<&OsStr>,
    path: &'a OsStr,
) -> impl Iterator<Item = PathBuf> + 'a {
    let bare = [PYTHON, "python"]
        .into_iter()
        .flat_map(move |name| env::split_paths(path).map(move |dir| dir.join(name)));
    venv_python(venv)
        .into_iter()
        .chain(beside_launchers(path))
        .chain(bare)
        .filter(|python| python.is_file())
}

/// The `localflow` script of the Python LocalFlow install for this
/// environment, which `agent install` starts at login. Nothing is run.
pub(crate) fn find_script() -> Option<PathBuf> {
    find_script_in(
        env::var_os("VIRTUAL_ENV").as_deref(),
        &env::var_os("PATH").unwrap_or_default(),
    )
}

fn find_script_in(venv: Option<&OsStr>, path: &OsStr) -> Option<PathBuf> {
    interpreters_in(venv, path).find_map(|python| script_of(&python))
}

/// The interpreter of the Python LocalFlow install for this environment,
/// whose modules `setup install-app` launches: the first with the
/// `localflow` script or, as in a `pip install --user` whose scripts live
/// elsewhere, able to import `localflow`.
pub(crate) fn find_interpreter() -> Option<PathBuf> {
    find_interpreter_in(
        env::var_os("VIRTUAL_ENV").as_deref(),
        &env::var_os("PATH").unwrap_or_default(),
    )
}

fn find_interpreter_in(venv: Option<&OsStr>, path: &OsStr) -> Option<PathBuf> {
    interpreters_in(venv, path)
        .find(|python| script_of(python).is_some() || imports_localflow(python))
}

/// The working directory is left off `sys.path`: a source checkout there is
/// not an install.
const IMPORT_PROBE: &str = "import importlib.util, sys\n\
sys.path[:] = [entry for entry in sys.path if entry]\n\
sys.exit(importlib.util.find_spec('localflow') is None)";

fn imports_localflow(python: &Path) -> bool {
    Command::new(python)
        .args(["-c", IMPORT_PROBE])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn bare_names_come_last() {
        let candidates = candidates();
        let names: Vec<_> = candidates[candidates.len() - 2..]
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [PYTHON, "python"]);
    }

    /// `root/name/<bin>/` holding `files`; returns the venv root and bin dir.
    fn tree(root: &Path, name: &str, files: &[&str]) -> (PathBuf, PathBuf) {
        let venv = root.join(name);
        let bin = venv.join(if cfg!(windows) { "Scripts" } else { "bin" });
        fs::create_dir_all(&bin).unwrap();
        for file in files {
            fs::write(bin.join(file), "").unwrap();
        }
        (venv, bin)
    }

    #[test]
    fn install_needs_the_localflow_script() {
        let root = tempfile::tempdir().unwrap();
        let (other, _) = tree(root.path(), "other", &[PYTHON]);
        let (venv, venv_bin) = tree(root.path(), "venv", &[PYTHON, SCRIPT]);
        let (_, bare) = tree(root.path(), "bare", &[PYTHON]);
        let (_, install) = tree(root.path(), "install", &["python", SCRIPT]);
        let path = env::join_paths([&bare, &install]).unwrap();

        // An unrelated venv is skipped for the install on PATH.
        assert_eq!(
            find_script_in(Some(other.as_os_str()), &path),
            Some(install.join(SCRIPT))
        );
        // A venv with the script wins.
        assert_eq!(
            find_script_in(Some(venv.as_os_str()), &path),
            Some(venv_bin.join(SCRIPT))
        );
        // Nothing found.
        let bare_only = env::join_paths([&bare]).unwrap();
        assert_eq!(find_script_in(Some(other.as_os_str()), &bare_only), None);
        assert_eq!(find_script_in(None, OsStr::new("")), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_script_may_live_in_scripts() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("Python312");
        fs::create_dir_all(home.join("Scripts")).unwrap();
        fs::write(home.join(PYTHON), "").unwrap();
        fs::write(home.join("Scripts").join(SCRIPT), "").unwrap();
        assert_eq!(
            find_script_in(None, home.as_os_str()),
            Some(home.join("Scripts").join(SCRIPT))
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_is_found_behind_a_later_lf_on_path() {
        let root = tempfile::tempdir().unwrap();
        // Another program called `lf` next to an unrelated python3 comes first.
        let (_, other) = tree(root.path(), "other", &["lf", PYTHON]);
        let (_, install) = tree(root.path(), "install", &["lf", PYTHON, SCRIPT]);
        let (_, links) = tree(root.path(), "links", &[]);
        std::os::unix::fs::symlink(install.join("lf"), links.join("lf")).unwrap();
        let path = env::join_paths([&other, &links]).unwrap();
        assert_eq!(
            find_script_in(None, &path),
            Some(install.canonicalize().unwrap().join(SCRIPT))
        );
    }

    #[cfg(unix)]
    #[test]
    fn interpreter_without_the_script_must_import_localflow() {
        let root = tempfile::tempdir().unwrap();
        // Stand-ins that fail and pass the import probe.
        let stub = |name: &str, program: &str| {
            let (_, bin) = tree(root.path(), name, &[]);
            let target = ["/bin", "/usr/bin"]
                .map(|dir| Path::new(dir).join(program))
                .into_iter()
                .find(|path| path.is_file())
                .unwrap();
            std::os::unix::fs::symlink(target, bin.join(PYTHON)).unwrap();
            bin
        };
        let (unrelated, user_install) = (stub("unrelated", "false"), stub("user", "true"));
        let path = env::join_paths([&unrelated, &user_install]).unwrap();
        assert_eq!(
            find_interpreter_in(None, &path),
            Some(user_install.join(PYTHON))
        );
        assert_eq!(find_interpreter_in(None, unrelated.as_os_str()), None);
        assert_eq!(find_script_in(None, &path), None);
    }
}
