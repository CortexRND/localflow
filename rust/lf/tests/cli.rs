//! End-to-end `lf-rs` tests: the real binary in a temporary environment
//! (HOME, XDG/AppData, LOCALFLOW_CONFIG, empty secret overrides), so real
//! config, history and keychain entries are never touched and no test
//! mutates this process's environment.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use std::io::Write as _;

const SECRETS: &[&str] = &[
    "LLM_API_KEY",
    "FIREWORKS_API_KEY",
    "LLM_ACTIVATION_INDEX",
    "PROVIDER_KEY_FIREWORKS",
    "PROVIDER_KEY_OPENAI",
    "PROVIDER_KEY_OPENROUTER",
    "PROVIDER_KEY_GROQ",
    "PROVIDER_KEY_TOGETHER",
    "PROVIDER_KEY_CUSTOM",
];

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Sandbox {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    fn config(&self) -> PathBuf {
        self.root().join("config").join("config.toml")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lf-rs"));
        command.args(args);
        for (key, _) in std::env::vars_os() {
            // Windows names are case-insensitive, so match them upper-cased.
            let name = key.to_string_lossy().to_uppercase();
            if name.starts_with("LOCALFLOW_")
                || name.starts_with("XDG_")
                || name.ends_with("_API_KEY")
                || name == "DISPLAY"
                || name == "WAYLAND_DISPLAY"
            {
                command.env_remove(&key);
            }
        }
        let home = self.home();
        let dirs = [
            ("HOME", home.clone()),
            ("USERPROFILE", home.clone()),
            ("XDG_CONFIG_HOME", self.root().join("xdg-config")),
            ("XDG_DATA_HOME", self.root().join("xdg-data")),
            ("APPDATA", self.root().join("roaming")),
            ("LOCALAPPDATA", self.root().join("local")),
            ("WIN_PD_OVERRIDE_LOCAL_APPDATA", self.root().join("local")),
            (
                "WIN_PD_OVERRIDE_ROAMING_APPDATA",
                self.root().join("roaming"),
            ),
        ];
        for (key, path) in dirs {
            std::fs::create_dir_all(&path).expect("mkdir");
            command.env(key, path);
        }
        command.env("LOCALFLOW_CONFIG", self.config());
        command.env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", self.root().join("no-bus").display()),
        );
        for name in SECRETS {
            command.env(format!("LOCALFLOW_{name}"), "");
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[], "")
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)], input: &str) -> Output {
        let mut command = self.command(args);
        for (key, value) in env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn lf-rs");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait lf-rs")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

#[test]
fn help_lists_commands_and_usage_errors_exit_2() {
    let sandbox = Sandbox::new();
    let help = sandbox.run(&["--help"]);
    assert_eq!(code(&help), 0);
    for name in ["config", "dict", "history", "llm", "keys", "status"] {
        assert!(stdout(&help).contains(name), "{name}");
    }
    assert_eq!(code(&sandbox.run(&["history", "clear", "--help"])), 0);
    assert_eq!(code(&sandbox.run(&[])), 2);
    assert_eq!(code(&sandbox.run(&["bogus"])), 2);
    assert_eq!(code(&sandbox.run(&["history", "--limit", "0"])), 2);
    assert_eq!(
        code(&sandbox.run(&["history", "list", "--limit", "1001"])),
        2
    );
    assert_eq!(code(&sandbox.run(&["config", "set", "llm.model"])), 2);
    assert_eq!(code(&sandbox.run(&["dict", "import"])), 2);
}

#[test]
fn config_get_set_and_errors() {
    let sandbox = Sandbox::new();
    let model = sandbox.run(&["config", "get", "llm.model"]);
    assert_eq!(
        (code(&model), stdout(&model)),
        (0, "llama3.2:3b\n".to_string())
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "stt.language"])),
        "\n"
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "hotkey.sounds"])),
        "true\n"
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "dictation.skills_dirs"])),
        "[\"~/.claude/skills\", \"~/.agents/skills\"]\n"
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "dictation.silence_threshold_dbfs"])),
        "-50.0\n"
    );
    let full = sandbox.run(&["config", "get"]);
    assert!(stdout(&full).starts_with("config_version = 2\n"));
    assert_eq!(stdout(&full), stdout(&sandbox.run(&["config", "show"])));

    let unknown = sandbox.run(&["config", "get", "nope"]);
    assert_eq!(code(&unknown), 1);
    assert!(stderr(&unknown)
        .starts_with("Error: unknown config key 'nope'; valid keys: stt.provider, "));

    let saved = sandbox.run(&["config", "set", "dictation.silence_threshold_dbfs", "-42.5"]);
    assert_eq!(code(&saved), 0, "{}", stderr(&saved));
    assert_eq!(
        stdout(&saved),
        format!("saved {}\n", sandbox.config().display())
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "dictation.silence_threshold_dbfs"])),
        "-42.5\n"
    );
    assert_eq!(
        code(&sandbox.run(&["config", "set", "dictation.command_names", "a, b"])),
        0
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "dictation.command_names"])),
        "[\"a\", \"b\"]\n"
    );
    assert_eq!(
        code(&sandbox.run(&["config", "set", "stt.language", "de"])),
        0
    );
    assert_eq!(
        code(&sandbox.run(&["config", "set", "stt.language", ""])),
        0
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "stt.language"])),
        "\n"
    );

    let before = std::fs::read_to_string(sandbox.config()).unwrap();
    for (args, message) in [
        (["hotkey.sounds", "maybe"], "Error: boolean values must be true or false\n"),
        (["server.port", "80.5"], "Error: invalid integer: '80.5'\n"),
        (
            ["paste.method", "teleport"],
            "Error: invalid paste method 'teleport'; valid methods: auto, clipboard, wtype, xdotool, pynput, osascript\n",
        ),
    ] {
        let result = sandbox.run(&["config", "set", args[0], args[1]]);
        assert_eq!((code(&result), stderr(&result)), (1, message.to_string()));
    }
    let level = sandbox.run(&["config", "set", "cleanup.level", "loud"]);
    assert_eq!(code(&level), 1);
    assert!(stderr(&level).contains("cleanup.level"));
    assert_eq!(std::fs::read_to_string(sandbox.config()).unwrap(), before);
}

#[test]
fn config_path_and_migrate() {
    let sandbox = Sandbox::new();
    let path = sandbox.run(&["config", "path"]);
    let text = stdout(&path);
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap(),
        format!("path: {}", sandbox.config().display())
    );
    assert!(lines.next().unwrap().starts_with("legacy: missing ("));
    assert_eq!(
        stdout(&sandbox.run(&["config", "migrate"])),
        "no legacy config found\n"
    );
    std::fs::create_dir_all(sandbox.home()).unwrap();
    std::fs::write(
        sandbox.home().join(".localflow.toml"),
        "llm_model = \"legacy\"\nmystery = 1\n",
    )
    .unwrap();
    let migrated = sandbox.run(&["config", "migrate"]);
    assert_eq!(code(&migrated), 0, "{}", stderr(&migrated));
    assert_eq!(
        stdout(&migrated),
        format!("migrated legacy config to {}\n", sandbox.config().display())
    );
    // Core warnings reach stderr; INFO (the "migrated ..." log) does not.
    assert!(
        stderr(&migrated).contains("ignoring unknown v1 config key 'mystery'"),
        "{}",
        stderr(&migrated)
    );
    assert!(
        !stderr(&migrated).contains("the old file was left in place"),
        "{}",
        stderr(&migrated)
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "llm.model"])),
        "legacy\n"
    );
}

#[test]
fn invalid_config_value_warns_on_stderr_and_uses_default() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.config().parent().unwrap()).unwrap();
    std::fs::write(
        sandbox.config(),
        "config_version = 2\n\n[cleanup]\nlevel = \"loud\"\n",
    )
    .unwrap();
    let result = sandbox.run(&["config", "get", "cleanup.level"]);
    assert_eq!(code(&result), 0, "{}", stderr(&result));
    assert_eq!(stdout(&result), "light\n");
    assert_eq!(
        stderr(&result),
        "WARNING: invalid config value for cleanup.level: 'loud'; using default\n"
    );
}

#[test]
fn malformed_config_is_an_error_without_panic() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.config().parent().unwrap()).unwrap();
    std::fs::write(sandbox.config(), "[llm\n").unwrap();
    for args in [
        &["config", "get", "llm.model"][..],
        &["history"],
        &["status"],
    ] {
        let result = sandbox.run(args);
        assert_eq!(code(&result), 1, "{args:?}");
        assert!(stderr(&result).starts_with("Error: "), "{args:?}");
        assert!(!stderr(&result).contains("panicked"));
    }
}

/// Fails (before any `~/.localflow` write) unless lf-rs resolves `~` to the
/// sandbox home, like Python `Path.home()` does with HOME/USERPROFILE.
fn assert_home_is_sandboxed(sandbox: &Sandbox) {
    let path = sandbox.run(&["config", "path"]);
    let expected = format!(
        "legacy: missing ({})",
        sandbox.home().join(".localflow.toml").display()
    );
    assert!(
        stdout(&path).lines().any(|line| line == expected),
        "lf-rs home is not the sandbox: {}",
        stdout(&path)
    );
}

#[test]
fn dictionary_commands() {
    let sandbox = Sandbox::new();
    assert_home_is_sandboxed(&sandbox);
    assert_eq!(
        stdout(&sandbox.run(&["dict", "list"])),
        "No dictionary entries.\n"
    );
    assert_eq!(
        stdout(&sandbox.run(&["dict", "add", "Kubernetes"])),
        "added: word: Kubernetes\n"
    );
    assert_eq!(
        stdout(&sandbox.run(&["dict", "add", "--from", "gee pee tee", "--to", "GPT"])),
        "added: gee pee tee -> GPT\n"
    );
    assert_eq!(
        code(&sandbox.run(&["dict", "add", "w", "--from", "a", "--to", "b"])),
        2
    );
    assert_eq!(
        stdout(&sandbox.run(&["dict", "list"])),
        "word: Kubernetes\ngee pee tee -> GPT\n"
    );
    assert!(sandbox
        .home()
        .join(".localflow")
        .join("dictionary.json")
        .exists());
    assert_eq!(code(&sandbox.run(&["dict", "remove", "nope"])), 1);
}

#[test]
fn history_commands_on_an_empty_store() {
    let sandbox = Sandbox::new();
    assert_home_is_sandboxed(&sandbox);
    assert_eq!(stdout(&sandbox.run(&["history"])), "No history entries.\n");
    assert_eq!(
        stdout(&sandbox.run(&["history", "search", "x"])),
        "No history entries.\n"
    );
    let missing = sandbox.run(&["history", "delete", "5"]);
    assert_eq!(
        (code(&missing), stderr(&missing)),
        (1, "Error: history entry 5 not found\n".into())
    );
    let refused = sandbox.run(&["history", "clear"]);
    assert_eq!(code(&refused), 1);
    assert!(stderr(&refused).contains("Aborted!"));
    let cleared = sandbox.run_with(&["history", "clear"], &[], "y\n");
    assert_eq!(code(&cleared), 0);
    assert!(stdout(&cleared).ends_with("Cleared 0 history entries.\n"));
    assert!(sandbox
        .home()
        .join(".localflow")
        .join("history.sqlite")
        .exists());
    let stats = stdout(&sandbox.run(&["stats"]));
    assert!(stats.starts_with("Dictations: 0\nWords: 0\nAverage WPM: 0.0\n"));
    assert_eq!(stats, stdout(&sandbox.run(&["history", "stats"])));
}

#[test]
fn keys_list_only_shows_masked_values() {
    let sandbox = Sandbox::new();
    let result = sandbox.run_with(
        &["keys", "list"],
        &[("LOCALFLOW_PROVIDER_KEY_GROQ", "gsk-very-secret-4321")],
        "",
    );
    assert_eq!(code(&result), 0, "{}", stderr(&result));
    let text = stdout(&result);
    assert!(text.starts_with("Local (Ollama): No key needed\n"));
    assert!(text.contains("Groq: ••••4321\n"));
    assert!(text.contains("OpenAI: No key\n"));
    assert!(!text.contains("very-secret"));
}

// The keys-import cases below never confirm a key, so nothing reaches the
// real keychain.
#[test]
fn keys_import_reports_and_validates_without_saving() {
    let sandbox = Sandbox::new();
    let none = sandbox.run(&["keys", "import"]);
    assert_eq!((code(&none), stdout(&none)), (0, "No keys found\n".into()));

    let missing = sandbox.root().join("missing.env");
    let missing = missing.to_str().unwrap();
    let result = sandbox.run(&["keys", "import", "--file", missing]);
    assert_eq!(code(&result), 2);
    // click shows the path with Python's repr (backslashes doubled).
    let repr = |path: &str| path.replace('\\', "\\\\");
    assert!(stderr(&result).ends_with(&format!(
        "Error: Invalid value for '--file': File '{}' does not exist.\n",
        repr(missing)
    )));
    let dir = sandbox.root().to_str().unwrap();
    let result = sandbox.run(&["keys", "import", "--file", dir]);
    assert_eq!(code(&result), 2);
    assert!(stderr(&result).ends_with(&format!(
        "Error: Invalid value for '--file': File '{}' is a directory.\n",
        repr(dir)
    )));
    let binary = sandbox.root().join("binary.env");
    std::fs::write(&binary, b"OPENAI_API_KEY=\xff\xfe\n").unwrap();
    let result = sandbox.run(&["keys", "import", "--file", binary.to_str().unwrap()]);
    assert_eq!(
        (code(&result), stderr(&result)),
        (1, "Error: Unable to read the selected key file\n".into())
    );

    let env = [("GROQ_API_KEY", "gsk-env-secret-4321")];
    let declined = sandbox.run_with(&["keys", "import"], &env, "maybe\nn\n");
    assert_eq!(code(&declined), 0, "{}", stderr(&declined));
    assert_eq!(
        stdout(&declined),
        "Groq: ••••4321 (environment: GROQ_API_KEY)\nSave this key? [y/N]: \
         Error: invalid input\nSave this key? [y/N]: Saved 0 key(s)\n"
    );
    let aborted = sandbox.run_with(&["keys", "import"], &env, "");
    assert_eq!((code(&aborted), stderr(&aborted)), (1, "Aborted!\n".into()));
    for output in [&declined, &aborted] {
        assert!(!stdout(output).contains("env-secret") && !stderr(output).contains("env-secret"));
    }
}

/// Windows environment names are case-insensitive; Python sees them upper-cased.
#[cfg(windows)]
#[test]
fn keys_import_finds_mixed_case_environment_names() {
    let sandbox = Sandbox::new();
    let env = [("Groq_Api_Key", "gsk-env-secret-4321")];
    let declined = sandbox.run_with(&["keys", "import"], &env, "n\n");
    assert_eq!(code(&declined), 0, "{}", stderr(&declined));
    assert_eq!(
        stdout(&declined),
        "Groq: ••••4321 (environment: GROQ_API_KEY)\nSave this key? [y/N]: Saved 0 key(s)\n"
    );
}

#[test]
fn secret_status_uses_env_overrides_and_group_is_hidden() {
    let sandbox = Sandbox::new();
    let status = sandbox.run_with(
        &["secret", "status"],
        &[("LOCALFLOW_LLM_API_KEY", "sk-override")],
        "",
    );
    assert_eq!(code(&status), 0, "{}", stderr(&status));
    assert_eq!(
        stdout(&status),
        "llm_api_key: configured\nfireworks_api_key: not configured\n"
    );
    let help = stdout(&sandbox.run(&["--help"]));
    assert!(!help.lines().any(|l| l.trim_start().starts_with("secret")));
    assert_eq!(code(&sandbox.run(&["secret", "--help"])), 0);
    assert_eq!(code(&sandbox.run(&["secret"])), 2);
    assert_eq!(code(&sandbox.run(&["secret", "unset"])), 2);
}

#[test]
fn llm_use_and_unreachable_provider() {
    let sandbox = Sandbox::new();
    let unknown = sandbox.run(&["llm", "use", "bogus"]);
    assert_eq!(
        (code(&unknown), stderr(&unknown)),
        (1, "Error: unknown LLM provider: bogus\n".into())
    );
    let url = "http://127.0.0.1:0";
    let used = sandbox.run(&["llm", "use", "ollama", "--base-url", url, "--model", "m1"]);
    assert_eq!(code(&used), 0, "{}", stderr(&used));
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "llm.base_url"])),
        format!("{url}\n")
    );
    assert_eq!(
        stdout(&sandbox.run(&["config", "get", "llm.model"])),
        "m1\n"
    );
    let listed = sandbox.run(&["llm", "list", "--json"]);
    assert_eq!(code(&listed), 1);
    let payload: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(payload["models"], serde_json::json!([]));
    assert!(payload["error"].as_str().is_some_and(|e| !e.is_empty()));
    let tested = sandbox.run(&["llm", "test"]);
    assert_eq!(code(&tested), 1);
    assert!(!stdout(&tested).trim().is_empty());
}

#[test]
fn status_without_server_is_clear_and_never_creates_a_token() {
    let sandbox = Sandbox::new();
    let port = "0";
    assert_eq!(
        code(&sandbox.run(&["config", "set", "server.port", port])),
        0
    );
    let text = sandbox.run(&["status"]);
    assert_eq!(code(&text), 0, "{}", stderr(&text));
    let out = stdout(&text);
    assert!(out.contains("meeting.server: down\n"));
    assert!(out.contains("stt.loaded: False\n"));
    assert!(out.contains(&format!("server.port: {port}\n")));
    assert!(stderr(&text).contains("server not running"));
    let json = sandbox.run(&["status", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["meeting"], serde_json::json!({"server": "down"}));
    let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
    assert_eq!(keys[0], "platform");
    assert_eq!(keys.last().unwrap().as_str(), "meeting");
    // No api-token file anywhere in the sandbox.
    fn has_token(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                let path = entry.path();
                path.file_name().is_some_and(|name| name == "api-token")
                    || (path.is_dir() && has_token(&path))
            })
    }
    assert!(!has_token(sandbox.root()));
}

#[test]
fn meeting_prompts_and_recall_in_an_empty_sandbox() {
    let sandbox = Sandbox::new();
    assert_home_is_sandboxed(&sandbox);
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
        .to_string();
    assert_eq!(
        code(&sandbox.run(&["config", "set", "server.port", &port])),
        0
    );
    let env = [("COLUMNS", "200")];
    for args in [
        &["meeting", "start", "-t", "Standup"][..],
        &["meeting", "stop"],
        &["meeting", "dismiss"],
    ] {
        let down = sandbox.run_with(args, &env, "");
        assert_eq!(
            (code(&down), stdout(&down), stderr(&down)),
            (
                1,
                "server not running — open LocalFlow.app (it starts the server) or run: lf serve\n"
                    .to_string(),
                String::new()
            ),
            "{args:?}"
        );
    }

    let queue = sandbox.home().join(".localflow").join("queue.json");
    let listed = sandbox.run_with(&["prompts", "list"], &env, "");
    assert_eq!(
        stdout(&listed),
        format!("no prompts queued ({})\n", queue.display())
    );
    let unknown = sandbox.run_with(&["prompts", "approve", "abc123"], &env, "");
    assert_eq!(
        (code(&unknown), stdout(&unknown)),
        (
            1,
            "unknown prompt abc123 — list them with: lf prompts list\n".to_string()
        )
    );
    let dispatch = sandbox.run_with(&["prompts", "dispatch", "--all"], &env, "");
    assert_eq!(code(&dispatch), 1);
    assert!(stdout(&dispatch).starts_with("no repo configured — set orca_repo in "));
    let nothing = sandbox.run_with(
        &["prompts", "dispatch", "--all", "--repo", "id:r"],
        &env,
        "",
    );
    assert_eq!(
        (code(&nothing), stdout(&nothing)),
        (0, "no approved prompts to dispatch\n".to_string())
    );
    assert!(!queue.exists());

    let none = sandbox.run_with(&["recall", "launch pricing", "--no-llm"], &env, "");
    assert_eq!(
        (code(&none), stdout(&none)),
        (0, "I couldn't find that in your history.\n".to_string())
    );
    let json = sandbox.run_with(&["recall", "launch", "--no-llm", "--json"], &env, "");
    assert_eq!(
        stdout(&json),
        "{\n  \"answer\": null,\n  \"citations\": []\n}\n"
    );
    let empty = sandbox.run(&["recall", ""]);
    assert_eq!(code(&empty), 2);
    assert!(stderr(&empty).ends_with("Error: question is empty\n"));
}

#[test]
fn serve_rejects_a_saved_port_that_would_wrap() {
    let sandbox = Sandbox::new();
    assert_eq!(
        code(&sandbox.run(&["config", "set", "server.port", "65536"])),
        0
    );
    let output = sandbox.run(&["serve"]);
    assert_eq!(code(&output), 1);
    assert!(stderr(&output).contains("server_port must be between 0 and 65535"));
    assert!(stdout(&output).is_empty());
}

#[test]
fn serve_does_not_announce_readiness_when_binding_fails() {
    let sandbox = Sandbox::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let output = sandbox.run(&["serve", "--host", "127.0.0.1", "--port", &port]);
    assert_eq!(code(&output), 1);
    assert!(!stderr(&output).is_empty());
    assert!(stdout(&output).is_empty());
}

fn token_file(sandbox: &Sandbox) -> PathBuf {
    // platformdirs: XDG_CONFIG_HOME on Unix and macOS, local AppData on Windows.
    let base = if cfg!(windows) {
        sandbox.root().join("local")
    } else {
        sandbox.root().join("xdg-config")
    };
    base.join("localflow").join("api-token")
}

#[test]
fn token_prints_the_stripped_token_and_never_creates_one() {
    let sandbox = Sandbox::new();
    let token = token_file(&sandbox);
    let hint = "no API token yet — start the server once: open LocalFlow.app or run lf serve\n";
    let run = || sandbox.run_with(&["token"], &[("COLUMNS", "80"), ("NO_COLOR", "1")], "");
    let missing = run();
    assert_eq!(
        (code(&missing), stdout(&missing), stderr(&missing)),
        (1, hint.to_string(), String::new())
    );
    assert!(!token.exists());
    std::fs::create_dir_all(token.parent().unwrap()).unwrap();
    std::fs::write(&token, b" \n\t ").unwrap();
    let blank = run();
    assert_eq!((code(&blank), stdout(&blank)), (1, hint.to_string()));
    std::fs::write(&token, b"  tok-123 \r\n").unwrap();
    let found = run();
    assert_eq!(
        (code(&found), stdout(&found), stderr(&found)),
        (0, "tok-123\n".to_string(), String::new())
    );
}

#[test]
fn hub_usage_errors_and_sandbox_refusal() {
    let sandbox = Sandbox::new();
    let bad = sandbox.run(&["hub", "--page", "nope"]);
    assert_eq!(code(&bad), 2);
    assert!(stderr(&bad).ends_with(
        "Error: Invalid value for '--page': 'nope' is not one of 'home', 'ask', 'hot phrases', \
         'dictionary', 'prompts', 'meetings', 'history', 'style', 'models', 'settings'.\n"
    ));
    let both = sandbox.run(&["hub", "--gpui", "--legacy"]);
    assert_eq!(
        (code(&both), stderr(&both)),
        (
            2,
            "Error: --gpui and --legacy can't be used together\n".to_string()
        )
    );
    let refused = sandbox.run_with(
        &["hub", "--page", "ask"],
        &[("LOCALFLOW_UI", "gpui"), ("LOCALFLOW_SANDBOX", "true")],
        "",
    );
    assert_eq!(code(&refused), 1);
    assert!(stderr(&refused).starts_with("Error: LOCALFLOW_SANDBOX is set, and `lf` opens"));
    assert!(stdout(&refused).is_empty());
}

#[cfg(unix)]
mod hub_unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{Duration, Instant};

    fn script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A fake Python install whose `-m localflow.hub_window` records its argv.
    fn fake_python(dir: &Path, has_module: bool) -> PathBuf {
        let record = dir.join("legacy.txt");
        let python = dir.join("bin").join("python3");
        let found = if has_module { 0 } else { 1 };
        script(
            &python,
            &format!(
                "if [ \"$1\" = -c ]; then exit {found}; fi\necho \"$*\" >> '{}'",
                record.display()
            ),
        );
        record
    }

    fn lines(record: &Path, count: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(record).unwrap_or_default();
            let lines: Vec<String> = text.lines().map(String::from).collect();
            if lines.len() >= count || Instant::now() > deadline {
                return lines;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    const GOOD_HELP: &str =
        "lf-desktop\n    --page <PAGE>  x\n    --setup  x\nEXIT STATUS (launch-status: 1):\n";

    #[test]
    fn hub_opens_legacy_through_the_python_install_and_gpui_when_selected() {
        let sandbox = Sandbox::new();
        let empty = sandbox.root().join("empty-path");
        std::fs::create_dir_all(&empty).unwrap();
        let venv = sandbox.root().join("venv");
        let other = sandbox.root().join("other");
        let unused = fake_python(&venv, false);
        let record = fake_python(&other, true);
        let path = other.join("bin");
        let env = [
            ("VIRTUAL_ENV", venv.to_str().unwrap()),
            ("PATH", path.to_str().unwrap()),
        ];

        let nowhere = [
            ("VIRTUAL_ENV", empty.to_str().unwrap()),
            ("PATH", empty.to_str().unwrap()),
        ];
        let none = sandbox.run_with(&["hub"], &nowhere, "");
        assert_eq!(
            (code(&none), stderr(&none)),
            (
                1,
                "Error: no Python LocalFlow install found; activate its virtualenv or put \
                 its lf on PATH\n"
                    .to_string()
            )
        );

        let bare = sandbox.run_with(&["hub"], &env, "");
        assert_eq!(
            (code(&bare), stdout(&bare), stderr(&bare)),
            (0, String::new(), String::new())
        );
        let mut with_ui = env.to_vec();
        with_ui.push(("LOCALFLOW_UI", " Bogus "));
        let noticed = sandbox.run_with(&["hub", "--page", "hot phrases"], &with_ui, "");
        assert_eq!(
            (code(&noticed), stderr(&noticed)),
            (
                0,
                "lf: LOCALFLOW_UI must be gpui or legacy; opening the legacy window.\n".into()
            )
        );
        assert_eq!(
            lines(&record, 2),
            [
                "-m localflow.hub_window --page Home",
                "-m localflow.hub_window --page Hot Phrases"
            ]
        );
        assert!(!unused.exists(), "the venv without localflow is skipped");

        let desktop = sandbox.root().join("desktop").join("lf-desktop");
        let launched = sandbox.root().join("desktop.txt");
        script(
            &desktop,
            &format!(
                "echo \"$*\" >> '{}'\nif [ \"$1\" = --help ]; then printf '%s' '{GOOD_HELP}'; exit 0; fi",
                launched.display()
            ),
        );
        let mut gpui = env.to_vec();
        gpui.push(("LOCALFLOW_DESKTOP_BIN", desktop.to_str().unwrap()));
        // Not set up yet: probed, never started (no preview first-run wizard).
        for args in [&["hub", "--gpui"][..], &["hub", "--gpui", "--page", "home"]] {
            let run = sandbox.run_with(args, &gpui, "");
            assert_eq!(
                (code(&run), stderr(&run)),
                (
                    0,
                    "lf: LocalFlow isn't set up yet (run lf setup); opening the legacy window.\n"
                        .into()
                ),
                "{args:?}"
            );
        }
        assert_eq!(lines(&launched, 2), ["--help", "--help"]);
        assert_eq!(
            lines(&record, 4)[2..],
            [
                "-m localflow.hub_window --page Home",
                "-m localflow.hub_window --page Home"
            ]
        );
        std::fs::remove_file(&launched).unwrap();
        std::fs::create_dir_all(sandbox.config().parent().unwrap()).unwrap();
        std::fs::write(
            sandbox.config(),
            "config_version = 2\n\n[setup]\nversion = 1\n",
        )
        .unwrap();
        for args in [&["hub", "--gpui"][..], &["hub", "--gpui", "--page", "home"]] {
            let run = sandbox.run_with(args, &gpui, "");
            assert_eq!((code(&run), stderr(&run)), (0, String::new()), "{args:?}");
        }
        assert_eq!(lines(&launched, 4), ["--help", "", "--help", "--page Home"]);

        let mut relative = env.to_vec();
        relative.push(("LOCALFLOW_DESKTOP_BIN", "lf-desktop"));
        let bad = sandbox.run_with(&["hub", "--gpui", "--page", "Ask"], &relative, "");
        assert_eq!(
            (code(&bad), stderr(&bad)),
            (
                0,
                "lf: LOCALFLOW_DESKTOP_BIN must be an absolute path to an executable \
                 lf-desktop; opening the legacy window.\n"
                    .into()
            )
        );
        assert_eq!(lines(&record, 5)[4], "-m localflow.hub_window --page Ask");
    }
}
