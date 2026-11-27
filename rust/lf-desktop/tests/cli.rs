//! The non-GUI command-line paths of the real `lf-desktop` binary.

use std::process::Command;

fn bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lf-desktop"));
    // Never let a test reach a display or real data.
    cmd.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
    cmd
}

#[test]
fn help_and_version_print_without_a_display() {
    let out = bin().arg("--help").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("--sandbox") && text.contains("--page") && text.contains("--data-dir"));
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("lf-desktop "));
}

#[test]
fn unknown_arguments_exit_2() {
    let out = bin().arg("--frobnicate").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown argument"));
    let out = bin().args(["--data-dir", "/tmp/x"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn engine_is_refused_where_it_cannot_run() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin()
        .args(["--sandbox", "--engine", "--data-dir"])
        .arg(dir.path().join("sb"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("sandbox never starts"));
    assert!(
        !dir.path().join("sb").exists(),
        "refused before touching anything"
    );
    if !cfg!(feature = "runtime") {
        let out = bin()
            .arg("--engine")
            .env("HOME", dir.path())
            .env("LOCALFLOW_CONFIG", dir.path().join("config.toml"))
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("--features runtime"));
        assert!(!dir.path().join(".localflow").exists());
    }
}

/// A child with a throwaway HOME/config and closed (or no) stdin, so the
/// helper paths can never reach real data, a model or a device.
fn isolated(dir: &std::path::Path) -> Command {
    let mut cmd = bin();
    cmd.env("HOME", dir)
        .env("XDG_DATA_HOME", dir.join("xdg"))
        .env("LOCALFLOW_CONFIG", dir.join("config.toml"))
        .env_remove("LOCALFLOW_SANDBOX")
        .env_remove("LOCALFLOW_SPEECH_IN_PROCESS")
        .stdin(std::process::Stdio::null());
    cmd
}

fn left_untouched(dir: &std::path::Path) {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(entries.is_empty(), "helper wrote into HOME: {entries:?}");
}

#[test]
fn speech_helper_mode_exists_only_in_the_runtime_build() {
    let dir = tempfile::tempdir().unwrap();
    let out = isolated(dir.path())
        .arg("--speech-helper")
        .output()
        .unwrap();
    if cfg!(feature = "runtime") {
        // Stdin EOF ends the helper cleanly before any config, model or device.
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "the protocol channel stays silent");
    } else {
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("unknown argument"));
    }
    left_untouched(dir.path());
}

#[test]
fn speech_helper_rejects_extra_arguments_and_the_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let cases: [(&[&str], Option<&str>); 5] = [
        (&["--speech-helper", "extra"], None),
        (&["--speech-helper", "--sandbox"], None),
        (&["--sandbox", "--speech-helper"], None),
        (&["--speech-helper"], Some("1")),
        (&["--speech-helper"], Some("true")),
    ];
    for (args, env_sandbox) in cases {
        let mut cmd = isolated(dir.path());
        cmd.args(args);
        if let Some(value) = env_sandbox {
            cmd.env("LOCALFLOW_SANDBOX", value);
        }
        let out = cmd.output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?} {env_sandbox:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        if cfg!(feature = "runtime") && env_sandbox.is_some() {
            assert!(stderr.contains("can't run in the sandbox"), "{stderr}");
        } else if cfg!(feature = "runtime") && args[0] == "--speech-helper" {
            assert!(stderr.contains("takes no other arguments"), "{stderr}");
        } else {
            assert!(stderr.contains("unknown argument"), "{stderr}");
        }
        assert!(out.stdout.is_empty());
    }
    left_untouched(dir.path());
}

#[test]
fn a_second_sandbox_launch_hands_its_page_to_the_open_window() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = dir.path().join("sb");
    let instance = sandbox.join("desktop-instance");
    let _window = lf_runtime::singleinstance::acquire_at(&instance.join("main-app.lock"))
        .unwrap()
        .unwrap();
    let out = bin()
        .args(["--sandbox", "--page", "models", "--data-dir"])
        .arg(&sandbox)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let request = std::fs::read_to_string(instance.join("page-request.json")).unwrap();
    assert!(request.contains("\"Models\""), "{request}");
    assert!(
        !sandbox.join("LOCALFLOW-DESKTOP-SANDBOX.txt").exists(),
        "the owner prepares data, not the hand-off"
    );

    let out = bin()
        .args(["--sandbox", "--setup", "--data-dir"])
        .arg(&sandbox)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let request = std::fs::read_to_string(instance.join("page-request.json")).unwrap();
    assert!(request.contains("\"Setup\""), "{request}");

    let out = bin()
        .args(["--sandbox", "--sandbox-reset", "--data-dir"])
        .arg(&sandbox)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("open in another LocalFlow window"));
}

#[test]
fn the_speaker_fixture_is_refused_without_its_own_new_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let out = isolated(dir.path())
        .args(["--sandbox", "--sandbox-fixture", "speakers-saved"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--data-dir"));
    left_untouched(dir.path());

    // A sandbox seeded without the fixture is never mixed with it.
    let sandbox = dir.path().join("sb");
    let out = isolated(dir.path())
        .args(["--sandbox", "--sandbox-exercise", "--data-dir"])
        .arg(&sandbox)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = isolated(dir.path())
        .args([
            "--sandbox",
            "--sandbox-fixture",
            "speakers-saved",
            "--data-dir",
        ])
        .arg(&sandbox)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("without the speaker fixture"));
    assert!(!sandbox.join("speaker-fixture").exists());
}

#[test]
fn sandbox_refuses_a_foreign_directory_before_opening_a_window() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("keep.txt"), "user data").unwrap();
    let out = bin()
        .args(["--sandbox", "--data-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a LocalFlow sandbox"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
        "user data"
    );
}
