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
    /// Import and inspect inference provider keys in the system keychain.
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Manage provider API-key secrets.
    #[command(subcommand, hide = true)]
    Secret(SecretCommand),
    /// Show localflow status (asks the running server when it is up).
    Status {
        /// Print JSON.
        #[arg(long = "json")]
        json: bool,
    },
    /// Control meeting transcription sessions.
    #[command(subcommand)]
    Meeting(MeetingCommand),
    /// Speaker labels for meetings: models, status and enrolled voices.
    #[command(subcommand, hide = true)]
    Speakers(SpeakersCommand),
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
        /// Open LocalFlow Preview (native GPUI app) instead of the legacy
        /// window. Without this flag, only lf hub and lf phrases use it, on
        /// Linux and macOS, once LocalFlow is set up and the preview is
        /// installed; setup and first run stay legacy. No screen-reader
        /// support yet: use --legacy (or lf-hub on macOS).
        #[arg(long)]
        gpui: bool,
        /// Open the legacy window (also used when LocalFlow Preview isn't
        /// installed).
        #[arg(long)]
        legacy: bool,
    },
    /// Print the API token (for a phone bookmark or an iOS Shortcut).
    Token,
    /// Change settings in plain words with a local model, e.g. lf tweak "turn off sounds".
    ///
    /// Shows a preview first; nothing changes until you confirm (or pass --yes).
    /// Runs on this computer through Ollama, whatever cloud provider is active.
    Tweak(TweakArgs),
    // `agent` and `setup` docs are verbatim click docstrings (periods kept):
    // `clickfmt` renders click's usage errors from them.
    /// Alias of `lf autostart` (launchd LaunchAgent on macOS).
    #[command(subcommand, hide = true, verbatim_doc_comment)]
    Agent(AgentCommand),
    /// Open the setup window or configure LocalFlow in the terminal.
    #[command(subcommand, verbatim_doc_comment)]
    Setup(SetupCommand),
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Alias of `lf autostart install` (launchd LaunchAgent on macOS).
    #[command(verbatim_doc_comment)]
    Install,
    /// Alias of `lf autostart uninstall` (launchd LaunchAgent on macOS).
    #[command(verbatim_doc_comment)]
    Uninstall,
    /// Alias of `lf autostart status` (launchd LaunchAgent on macOS).
    #[command(verbatim_doc_comment)]
    Status,
}

#[derive(Debug, Subcommand)]
pub enum SetupCommand {
    /// Install the setup window as a LocalFlow Setup.app bundle.
    #[command(verbatim_doc_comment)]
    InstallApp,
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
    /// Show a saved meeting's speakers (default: the last saved meeting).
    #[command(hide = true)]
    Speakers {
        #[arg(value_name = "MEETING_ID")]
        meeting_id: Option<String>,
    },
    /// Rename a speaker in a saved meeting's transcript.
    #[command(hide = true)]
    RenameSpeaker {
        #[arg(
            value_name = "[ID] SPEAKER NAME",
            required = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
    },
    /// Merge one speaker of a saved meeting into another.
    #[command(hide = true)]
    MergeSpeakers {
        #[arg(
            value_name = "[ID] FROM INTO",
            required = true,
            allow_hyphen_values = true
        )]
        args: Vec<String>,
    },
}

/// Speaker labels for meetings: models, status and enrolled voices.
///
/// Enrolled voices need [meetings] speaker_diarization and speaker_recognition on.
#[derive(Debug, Subcommand)]
pub enum SpeakersCommand {
    /// Show whether speaker-label models and the runtime are ready (offline).
    Status {
        /// Print JSON.
        #[arg(long = "json")]
        json: bool,
    },
    /// List enrolled voices: name, samples, enrolled at, voice model.
    List {
        /// Print the server JSON (includes ids).
        #[arg(long = "json")]
        json: bool,
    },
    /// Remember a voice under NAME, from one source: a meeting speaker, a file or the mic.
    ///
    /// Audio is sent only to the local server and is not retained there; some M4A/MP4 files
    /// need a short-lived private temporary copy while they are decoded.
    Enroll {
        #[arg(value_name = "NAME", allow_hyphen_values = true)]
        name: String,
        /// Meeting id (see `lf meeting speakers`).
        #[arg(long = "from-meeting", value_name = "MEETING_ID")]
        meeting_id: Option<String>,
        /// Speaker id in that meeting, e.g. S2.
        #[arg(long = "speaker", value_name = "SPEAKER_ID")]
        speaker_id: Option<String>,
        /// An audio clip of only this person speaking.
        #[arg(long = "file", value_name = "FILE")]
        file: Option<PathBuf>,
        /// Record this many seconds from the microphone now.
        #[arg(long = "record", value_name = "INTEGER RANGE", value_parser = clap::value_parser!(u32).range(1..=300))]
        record: Option<u32>,
    },
    /// Rename an enrolled voice (by name or id); applies to later meetings.
    Rename {
        #[arg(value_name = "OLD", allow_hyphen_values = true)]
        old: String,
        #[arg(value_name = "NEW", allow_hyphen_values = true)]
        new: String,
    },
    /// Forget an enrolled voice (or --all) and erase stored voice data from every saved
    /// meeting. Names already written into transcripts are not changed.
    Forget {
        #[arg(value_name = "NAME_OR_ID")]
        name_or_id: Option<String>,
        /// Forget every enrolled voice.
        #[arg(long = "all")]
        all: bool,
    },
    /// Copy enrolled voices (registry.json + .npy voiceprints) into a new or empty DIRECTORY
    /// on this computer. Not allowed inside the Obsidian vault (vaults get synced).
    Export {
        #[arg(value_name = "DIRECTORY")]
        directory: PathBuf,
    },
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

/// The missing or blank REQUEST and the `--undo` combinations are checked by
/// the command, like click's in-body `UsageError`s.
#[derive(Debug, Args)]
pub struct TweakArgs {
    #[arg(value_name = "REQUEST")]
    pub request: Option<String>,
    /// Apply without asking for confirmation.
    #[arg(long, short = 'y', overrides_with = "yes")]
    pub yes: bool,
    /// Undo the last applied tweak.
    #[arg(long, overrides_with = "undo")]
    pub undo: bool,
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
    /// Scan environment and an optional .env or shell profile; confirm before saving.
    Import {
        /// A .env file or shell profile to scan as well.
        #[arg(long = "file", value_name = "FILE_PATH")]
        file: Option<PathBuf>,
        /// Save all discovered keys without prompting.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Prompt for and store NAME in the keyring.
    Set {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Delete NAME from the keyring.
    Unset {
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Show whether each supported secret is configured.
    Status,
}
