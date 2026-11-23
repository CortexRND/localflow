//! Command-line grammar. Names, flags, defaults and hidden commands follow
//! `localflow/cli.py`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Optional Rust client for LocalFlow's local config, dictionary and history.
///
/// The Python `lf` command remains the default LocalFlow CLI; this binary
/// reads and writes the same local files.
#[derive(Debug, Parser)]
#[command(name = "lf-rs", version, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the Rust HTTP server for dictation and meetings.
    Serve {
        /// Bind address; defaults to the saved server.host.
        #[arg(long)]
        host: Option<String>,
        /// Bind port; defaults to the saved server.port (8756).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Inspect and edit localflow configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Manage personal vocabulary and transcription corrections.
    #[command(subcommand)]
    Dict(DictCommand),
    /// Enable, disable or inspect dictation without quitting LocalFlow.
    Dictation {
        #[arg(value_enum)]
        action: DictationAction,
    },
    /// Search and manage local dictation history.
    History(HistoryArgs),
    /// Show dictation statistics (same as `history stats`).
    #[command(hide = true)]
    Stats,
    /// Inspect and select language-model providers.
    #[command(subcommand)]
    Llm(LlmCommand),
    /// Inspect and select speech-to-text providers.
    #[command(subcommand)]
    Stt(SttCommand),
    /// Inspect inference provider keys in the system keychain.
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Show localflow status (asks the running server when it is up).
    Status {
        /// Print JSON.
        #[arg(long = "json")]
        json: bool,
    },
    /// Control meeting transcription sessions.
    #[command(subcommand)]
    Meeting(MeetingCommand),
    /// Review work prompts from meetings and dispatch them to Orca.
    #[command(subcommand)]
    Prompts(PromptsCommand),
    /// Ask a question across your dictations, meeting transcripts and notes.
    #[command(hide = true)]
    Recall(RecallArgs),
    /// Open the native history and settings hub.
    #[command(args_override_self = true)]
    Hub {
        /// Hub page to open.  [default: Home]
        #[arg(long, value_name = "PAGE", allow_hyphen_values = true)]
        page: Option<String>,
        /// Open LocalFlow Preview (native GPUI app, if installed) instead of
        /// the legacy window. No screen-reader support yet: use --legacy (or
        /// lf-hub on macOS).
        #[arg(long)]
        gpui: bool,
        /// Open the legacy window (the default).
        #[arg(long)]
        legacy: bool,
    },
    /// Print the API token (for a phone bookmark or an iOS Shortcut).
    Token,
}

// Required arguments, `--k` and `--status` are optional/plain strings here and
// validated by the commands, so their errors end with click's `Error: …` line.

#[derive(Debug, Subcommand)]
pub enum MeetingCommand {
    /// Start transcribing a meeting. Without --title the notes are named from
    /// the meeting when it stops; a given --title (even "Meeting") is kept.
    Start {
        /// Note title.
        #[arg(long, short = 't', allow_hyphen_values = true)]
        title: Option<String>,
        /// Zoom/Teams/... (default: auto-detected).
        #[arg(long, short = 'c', default_value = "", allow_hyphen_values = true)]
        category: String,
    },
    /// Stop, summarize, and write notes to the vault.
    Stop,
    /// Dismiss the current meeting-detected banner.
    Dismiss,
}

#[derive(Debug, Subcommand)]
pub enum PromptsCommand {
    /// List queued work prompts.
    #[command(hide = true)]
    List {
        /// Only show prompts with this status.
        #[arg(long, short = 's', allow_hyphen_values = true)]
        status: Option<String>,
    },
    /// Print the full text of prompt PROMPT_ID.
    #[command(hide = true)]
    Show {
        #[arg(value_name = "PROMPT_ID")]
        prompt_id: Option<String>,
    },
    /// Approve prompts IDS for dispatch (retries a failed one, clearing its error).
    #[command(hide = true)]
    Approve {
        #[arg(value_name = "IDS")]
        ids: Vec<String>,
    },
    /// Reject prompts IDS (pending or failed) so they are never dispatched.
    #[command(hide = true)]
    Reject {
        #[arg(value_name = "IDS")]
        ids: Vec<String>,
    },
    /// Dispatch approved prompts IDS (or --all) to Orca worktrees.
    Dispatch {
        #[arg(value_name = "IDS")]
        ids: Vec<String>,
        /// Dispatch every approved prompt.
        #[arg(long)]
        all: bool,
        /// Orca repo selector (default: config.orca_repo).
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        repo: String,
        /// Agent to run (default: config.orca_agent).
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        agent: String,
        /// Show what would be dispatched; change nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
pub struct RecallArgs {
    #[arg(value_name = "QUESTION")]
    pub question: Option<String>,
    /// today, yesterday, 7d, 30d or YYYY-MM-DD.
    #[arg(long, allow_hyphen_values = true)]
    pub since: Option<String>,
    /// Passages to retrieve (1-20).
    #[arg(long, default_value = "8", allow_hyphen_values = true)]
    pub k: String,
    /// Only show the ranked matching passages.
    #[arg(long = "no-llm")]
    pub no_llm: bool,
    /// Machine-readable output.
    #[arg(long = "json")]
    pub json: bool,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum DictationAction {
    On,
    Off,
    Status,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum SttDevice {
    Auto,
    Cpu,
    Cuda,
}

#[derive(Debug, Subcommand)]
pub enum SttCommand {
    /// List STT providers and models.
    #[command(hide = true)]
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Select an STT provider, optional MODEL, and DEVICE.
    #[command(hide = true)]
    Use {
        provider: String,
        model: Option<String>,
        #[arg(long, value_enum, default_value = "auto")]
        device: SttDevice,
    },
    /// Download a catalog speech model without selecting it.
    Download { model_id: String },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the resolved v2 path and legacy-file status.
    Path,
    /// Print the v2 TOML that would be written for the current config.
    Show,
    /// Print SECTION.KEY, or the whole v2 TOML when no key is given.
    Get {
        /// Dotted key such as llm.model.
        #[arg(value_name = "SECTION_KEY")]
        section_key: Option<String>,
    },
    /// Set SECTION.KEY to VALUE and write the v2 config.
    Set {
        /// Dotted key such as llm.model.
        #[arg(value_name = "SECTION_KEY")]
        section_key: String,
        /// New value, read as the type of the key's current value.
        #[arg(value_name = "RAW_VALUE", allow_hyphen_values = true)]
        raw_value: String,
    },
    /// Force migration of the legacy flat config file.
    Migrate,
}

#[derive(Debug, Subcommand)]
pub enum DictCommand {
    /// List dictionary words and replacements.
    List,
    /// Add a WORD, or a replacement with --from and --to.
    Add {
        /// Vocabulary word to add.
        word: Option<String>,
        /// Text to replace (with --to).
        #[arg(long = "from", value_name = "FROM", allow_hyphen_values = true)]
        source: Option<String>,
        /// Replacement text (with --from).
        #[arg(long = "to", value_name = "TO", allow_hyphen_values = true)]
        target: Option<String>,
    },
    /// Remove a word or the replacement whose source is WORD_OR_FROM.
    Remove {
        #[arg(value_name = "WORD_OR_FROM")]
        word_or_from: String,
    },
    /// Import words and replacements from a CSV or JSON file.
    Import {
        #[arg(value_name = "FILE_PATH")]
        file_path: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// Number of entries to show when no subcommand is given.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,
    /// Only show entries whose raw or final text contains this text.
    #[arg(long, allow_hyphen_values = true)]
    pub search: Option<String>,
    #[command(subcommand)]
    pub command: Option<HistoryCommand>,
}

#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// List the most recent entries.
    List {
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
    },
    /// List entries whose raw or final text contains QUERY.
    Search {
        #[arg(allow_hyphen_values = true)]
        query: String,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1000))]
        limit: u32,
    },
    /// Copy an entry's final text to the clipboard (ENTRY_ID or "last").
    Copy {
        #[arg(value_name = "ENTRY_ID")]
        entry_id: String,
    },
    /// Copy an entry's raw transcript to the clipboard (ENTRY_ID or "last").
    Raw {
        #[arg(value_name = "ENTRY_ID")]
        entry_id: String,
    },
    /// Delete one entry and its stored clip (ENTRY_ID or "last").
    Delete {
        #[arg(value_name = "ENTRY_ID")]
        entry_id: String,
    },
    /// Delete all history entries and stored clips.
    Clear {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Show dictation statistics.
    Stats,
}

#[derive(Debug, Subcommand)]
pub enum LlmCommand {
    /// List models from an LLM provider.
    List {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long = "base-url")]
        base_url: Option<String>,
        /// Print JSON.
        #[arg(long = "json")]
        json: bool,
    },
    /// Select an LLM provider and optional endpoint or model.
    Use {
        provider: String,
        #[arg(long = "base-url")]
        base_url: Option<String>,
        #[arg(long = "model")]
        model: Option<String>,
    },
    /// Test connectivity to an LLM provider.
    Test {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long = "base-url")]
        base_url: Option<String>,
        #[arg(long = "model")]
        model: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeysCommand {
    /// Show masked provider key status.
    List,
}
