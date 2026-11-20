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
