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
            let key = key.to_string_lossy().into_owned();
            if key.starts_with("LOCALFLOW_")
                || key.starts_with("XDG_")
                || key == "FIREWORKS_API_KEY"
                || key == "DISPLAY"
                || key == "WAYLAND_DISPLAY"
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
