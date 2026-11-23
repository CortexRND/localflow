//! The native (Rust) macOS install that `setup install-app` and the darwin
//! `agent install` must leave alone: the `LocalFlow.app` bundle written by
//! `scripts/build_rust_app.sh` and the login agent written by
//! `scripts/macos_launchagent.sh`, which share their paths and label with the
//! Python install's.

use std::fs;
use std::path::Path;

/// Why `setup install-app` refuses to replace a native `LocalFlow.app`.
pub(crate) const NATIVE_APP: &str = "~/Applications/LocalFlow.app is the native LocalFlow app; \
     lf-rs will not replace it. To switch back to Python run: LOCALFLOW_RUNTIME=python ./install.sh";

/// Why `agent install` refuses to replace the native app's login agent.
pub(crate) const NATIVE_AGENT: &str =
    "the LocalFlow login agent belongs to the native LocalFlow app; \
     lf-rs will not replace it. To switch back to Python run: LOCALFLOW_RUNTIME=python ./install.sh";

/// A native bundle: its `Info.plist` has `LocalFlowRuntime` = `rust` (the
/// marker `install.sh` checks); otherwise its `CFBundleExecutable` is
/// `lf-hub`; only without a readable `CFBundleExecutable` does a
/// `Contents/MacOS/lf-hub` file decide, so a Python bundle written over a
/// native one (leaving `lf-hub` behind) is not native.
pub(crate) fn is_native_app(app: &Path) -> bool {
    let contents = app.join("Contents");
    let info = plist::Value::from_file(contents.join("Info.plist"))
        .ok()
        .and_then(plist::Value::into_dictionary)
        .unwrap_or_default();
    let string = |key| info.get(key).and_then(plist::Value::as_string);
    if string("LocalFlowRuntime") == Some("rust") {
        return true;
    }
    match string("CFBundleExecutable") {
        Some(executable) => executable == "lf-hub",
        None => fs::symlink_metadata(contents.join("MacOS").join("lf-hub")).is_ok(),
    }
}

/// A native login agent: its `ProgramArguments[0]` ends with
/// `/Contents/MacOS/lf-hub` (`is_rust` in `scripts/macos_launchagent.sh`).
pub(crate) fn is_native_agent(plist: &Path) -> bool {
    plist::Value::from_file(plist)
        .ok()
        .and_then(plist::Value::into_dictionary)
        .and_then(|agent| agent.get("ProgramArguments")?.as_array()?.first().cloned())
        .and_then(plist::Value::into_string)
        .is_some_and(|program| program.ends_with("/Contents/MacOS/lf-hub"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Info.plist` as `plutil -create xml1` + `plutil -insert` write it.
    fn info_plist(executable: Option<&str>, runtime: Option<&str>) -> String {
        let entry = |key: &str, value: Option<&str>| {
            value
                .map(|value| format!("\t<key>{key}</key>\n\t<string>{value}</string>\n"))
                .unwrap_or_default()
        };
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\
             {}{}</dict>\n</plist>\n",
            entry("CFBundleExecutable", executable),
            entry("LocalFlowRuntime", runtime)
        )
    }

    #[test]
    fn native_app_by_marker_then_executable_then_file() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("LocalFlow.app");
        assert!(!is_native_app(&app));
        let contents = app.join("Contents");
        let hub = contents.join("MacOS").join("lf-hub");
        fs::create_dir_all(contents.join("MacOS")).unwrap();
        // (CFBundleExecutable, LocalFlowRuntime, lf-hub file) -> native
        for (executable, runtime, file, native) in [
            (Some("launcher"), None, false, false),
            (Some("launcher"), Some("python"), false, false),
            (Some("launcher"), Some("rust"), false, true),
            (Some("lf-hub"), None, false, true),
            (Some("lf-hub"), Some("python"), false, true),
            // A Python bundle written over a native one, `lf-hub` left behind.
            (Some("launcher"), None, true, false),
            (None, None, true, true),
            (None, None, false, false),
        ] {
            fs::write(contents.join("Info.plist"), info_plist(executable, runtime)).unwrap();
            assert!(plist::Value::from_file(contents.join("Info.plist")).is_ok());
            let _ = fs::remove_file(&hub);
            if file {
                fs::write(&hub, "").unwrap();
            }
            assert_eq!(
                is_native_app(&app),
                native,
                "{executable:?} {runtime:?} {file}"
            );
        }
        // A binary Info.plist (`plutil -convert binary1`) is read too.
        for (runtime, native) in [("python", false), ("rust", true)] {
            let mut info = plist::Dictionary::new();
            info.insert("CFBundleExecutable".into(), "launcher".into());
            info.insert("LocalFlowRuntime".into(), runtime.into());
            plist::to_file_binary(contents.join("Info.plist"), &info).unwrap();
            assert_eq!(is_native_app(&app), native, "binary {runtime}");
        }
        // No Info.plist at all: the `lf-hub` file decides.
        fs::remove_file(contents.join("Info.plist")).unwrap();
        assert!(!is_native_app(&app));
        fs::write(&hub, "").unwrap();
        assert!(is_native_app(&app));
    }

    #[test]
    fn native_agent_by_program() {
        let root = tempfile::tempdir().unwrap();
        let plist = root.path().join("com.cortexrnd.localflow.plist");
        assert!(!is_native_agent(&plist));
        // scripts/macos_launchagent.sh's single-line heredoc.
        let heredoc = |program: &str| {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n\
                 <key>Label</key><string>com.cortexrnd.localflow</string>\n\
                 <key>ProgramArguments</key><array><string>{program}</string></array>\n\
                 <key>StandardOutPath</key><string>/Users/a/.localflow/agent.out.log</string>\n\
                 </dict></plist>\n"
            )
        };
        // plistlib's multi-line form.
        let plistlib = |program: &str| {
            format!(
                "<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\
                 \t<string>com.cortexrnd.localflow</string>\n\
                 \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{program}</string>\n\
                 \t</array>\n</dict>\n</plist>\n"
            )
        };
        for (xml, native) in [
            (
                heredoc("/Users/a/Applications/LocalFlow.app/Contents/MacOS/lf-hub"),
                true,
            ),
            (
                heredoc("/Users/a &amp; b/LocalFlow.app/Contents/MacOS/lf-hub"),
                true,
            ),
            (
                heredoc("/Users/a/LocalFlow.app/Contents/MacOS/lf-hub &lt;x&gt;"),
                false,
            ),
            (heredoc("/Users/a/.venv/bin/localflow"), false),
            (
                plistlib("/Users/a/Applications/LocalFlow.app/Contents/MacOS/lf-hub"),
                true,
            ),
            (plistlib("/Users/a/.venv/bin/localflow"), false),
            (
                "<plist><dict><key>ProgramArguments</key><array/></dict></plist>".to_string(),
                false,
            ),
        ] {
            fs::write(&plist, &xml).unwrap();
            assert!(plist::Value::from_file(&plist).is_ok(), "{xml}");
            assert_eq!(is_native_agent(&plist), native, "{xml}");
        }
        // A binary plist (`plutil -convert binary1`).
        for (program, native) in [
            (
                "/Users/a/Applications/LocalFlow.app/Contents/MacOS/lf-hub",
                true,
            ),
            ("/Users/a/.venv/bin/localflow", false),
        ] {
            let mut agent = plist::Dictionary::new();
            agent.insert("Label".into(), "com.cortexrnd.localflow".into());
            agent.insert(
                "ProgramArguments".into(),
                plist::Value::Array(vec![program.into()]),
            );
            plist::to_file_binary(&plist, &agent).unwrap();
            assert_eq!(is_native_agent(&plist), native, "binary {program}");
        }
    }
}
