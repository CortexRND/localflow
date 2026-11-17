use std::process::{Command, Output};

fn launch(filter: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lf-hub"));
    command
        .args(args)
        .env_remove("RUST_LOG")
        .env("RUST_LOG_STYLE", "never");
    if let Some(filter) = filter {
        command.env("RUST_LOG", filter);
    }
    command.output().unwrap()
}

#[test]
fn startup_logs_to_stderr_with_timestamp_by_default() {
    let output = launch(None, &["--invalid-logging-test"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    let record = stderr
        .lines()
        .find(|line| line.contains("starting lf-hub"))
        .unwrap();
    assert!(record.starts_with('['), "{record}");
    assert!(
        record.contains("T") && record.contains("Z INFO"),
        "{record}"
    );
}

#[test]
fn rust_log_overrides_default_filter() {
    for filter in ["error", ""] {
        let output = launch(Some(filter), &["--invalid-logging-test"]);
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("starting lf-hub"), "{stderr}");
        assert!(stderr.contains("unexpected argument"), "{stderr}");
    }
}

#[cfg(feature = "runtime")]
#[test]
fn speech_helper_initializes_logging_before_dispatch() {
    let output = launch(None, &["--speech-helper", "--invalid-logging-test"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("starting speech helper"));
}
