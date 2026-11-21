//! `lf-hub` on Linux opens LocalFlow Preview through the real binary: a fake
//! `lf-desktop` records the argv and environment it was exec'd with.
#![cfg(target_os = "linux")]

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LF_HUB: &str = env!("CARGO_BIN_EXE_lf-hub");

// `printf` formats (`\\n` becomes a newline in the fake's shell script).
const CURRENT_HELP: &str =
    "lf-desktop — LocalFlow native desktop app\\n    --page <PAGE>  x\\n    \
    --setup  y\\n\\nEXIT STATUS (launch-status: 1):\\n    0   closed or handed off\\n";
const OLD_HELP: &str = "lf-desktop — old build\\n    --page <PAGE>  x\\n";
// A Scope 3 preview build: every flag lf-hub sends and more, but no
// launch-status promise (it exited 0 even when no window could open).
const SCOPE3_HELP: &str = "lf-desktop — LocalFlow native desktop app (GPUI preview)\\n\\n\
    OPTIONS:\\n    --page <PAGE>      Open a page\\n    --setup            Setup wizard\\n    \
    --engine           Dictation\\n    --sandbox          Sandbox\\n    \
    --data-dir <DIR>   Sandbox directory\\n    --sandbox-reset    Re-seed\\n    \
    -h, --help         Help\\n    -V, --version      Version\\n";

fn fake_desktop(dir: &Path, record: &Path, help_text: &str) -> PathBuf {
    let path = dir.join("lf-desktop");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --help ]; then printf '{help_text}'; exit 0; fi\n\
         printf '%s\\n' \"argv:$#:$*\" \"home:$HOME\" \"config:$LOCALFLOW_CONFIG\" \
         \"sandbox:${{LOCALFLOW_SANDBOX-unset}}\" > '{}'\nexit 0\n",
        record.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn lf_hub(home: &Path, bin: Option<&Path>, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut command = Command::new(LF_HUB);
    command
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("RUST_LOG", "off");
    if let Some(bin) = bin {
        command.env("LOCALFLOW_DESKTOP_BIN", bin);
    }
    for (key, value) in extra {
        command.env(key, value);
    }
    command.output().unwrap()
}

#[test]
fn routes_parsed_options_to_the_override_and_keeps_the_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("dir with space $(touch pwned)");
    std::fs::create_dir(&dir).unwrap();
    let record = tmp.path().join("record");
    let exe = fake_desktop(&dir, &record, CURRENT_HELP);
    for (args, expected) in [
        (&[][..], "argv:0:"),
        (&["--page", "hot phrases"][..], "argv:2:--page Hot Phrases"),
        (&["--setup"][..], "argv:1:--setup"),
    ] {
        let out = lf_hub(
            tmp.path(),
            Some(&exe),
            args,
            &[
                ("LOCALFLOW_CONFIG", "/x/c.toml"),
                ("LOCALFLOW_SANDBOX", "1"),
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let seen = std::fs::read_to_string(&record).unwrap();
        let mut lines = seen.lines();
        assert_eq!(lines.next(), Some(expected));
        assert_eq!(
            lines.next(),
            Some(format!("home:{}", tmp.path().display()).as_str())
        );
        assert_eq!(lines.next(), Some("config:/x/c.toml"));
        // The sandbox request reaches lf-desktop, which honours it itself.
        assert_eq!(lines.next(), Some("sandbox:1"));
    }
    assert!(std::fs::read_dir(tmp.path()).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("pwned")));
}

#[test]
fn uses_the_owned_package_without_an_override() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".local/share/localflow-preview");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(
        root.join("MANIFEST.json"),
        "{\n  \"name\": \"localflow-preview\",\n}",
    )
    .unwrap();
    let record = home.path().join("record");
    fake_desktop(&root.join("bin"), &record, CURRENT_HELP);
    // Malformed manifest (trailing comma): not trusted.
    let out = lf_hub(home.path(), None, &["--page=history"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("didn't install"));
    std::fs::write(
        root.join("MANIFEST.json"),
        "{\"name\": \"localflow-preview\"}",
    )
    .unwrap();
    let out = lf_hub(home.path(), None, &["--page=history"], &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(std::fs::read_to_string(&record)
        .unwrap()
        .starts_with("argv:2:--page History\n"));
}

#[test]
fn failures_exit_1_with_an_install_hint_and_never_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let record = tmp.path().join("record");
    let cases: Vec<(Option<PathBuf>, &str)> = vec![
        (None, "isn't installed"),
        (
            Some(PathBuf::from("relative/lf-desktop")),
            "LOCALFLOW_DESKTOP_BIN must be",
        ),
        (
            Some(tmp.path().join("missing")),
            "LOCALFLOW_DESKTOP_BIN must be",
        ),
    ];
    for (bin, message) in cases {
        let out = lf_hub(tmp.path(), bin.as_deref(), &[], &[]);
        assert_eq!(out.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(message) && stderr.contains("install_gpui_preview.sh"),
            "{stderr}"
        );
    }
    let old_dir = tmp.path().join("old");
    std::fs::create_dir(&old_dir).unwrap();
    for (name, help) in [("old", OLD_HELP), ("scope3", SCOPE3_HELP)] {
        let dir = old_dir.join(name);
        std::fs::create_dir(&dir).unwrap();
        let stale = fake_desktop(&dir, &record, help);
        for args in [&["--setup"][..], &[][..], &["--page", "history"][..]] {
            let out = lf_hub(tmp.path(), Some(&stale), args, &[]);
            assert_eq!(out.status.code(), Some(1), "{name}");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("too old") && stderr.contains("install_gpui_preview.sh"),
                "{stderr}"
            );
            assert!(
                !record.exists(),
                "{name}: a stale binary must not be launched"
            );
        }
    }
    // An override pointing back at lf-hub (here through a symlink) is refused.
    let link = tmp.path().join("lf-desktop-link");
    symlink(LF_HUB, &link).unwrap();
    let out = lf_hub(tmp.path(), Some(&link), &[], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("lf-hub itself"));
}

#[test]
fn bad_arguments_still_exit_2_before_routing() {
    let tmp = tempfile::tempdir().unwrap();
    let out = lf_hub(tmp.path(), None, &["--bogus"], &[]);
    assert_eq!(out.status.code(), Some(2));
}
