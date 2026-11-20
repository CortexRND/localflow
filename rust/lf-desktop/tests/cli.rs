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
