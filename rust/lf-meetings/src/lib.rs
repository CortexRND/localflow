//! Meeting capture, notes, transcript Q&A, title generation and work dispatch.
pub mod capture;
pub mod dispatch;
#[cfg(target_os = "macos")]
mod macos_audio;
mod media;
pub mod prompts;
pub mod system_audio;
pub mod templates;
pub mod titles;
pub mod watcher;
pub mod writer;

pub use capture::{MeetingSession, Transcriber};
use caseless::Caseless;
use lf_core::symbols::pycompat as py;
pub use lf_core::transcript::TimedText;
use lf_providers::LlmProvider;
pub use media::strip_remote_media;
pub use prompts::ENHANCE_INSTRUCTION;
use regex::Regex;
use std::{
    borrow::Cow,
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};
pub use system_audio::{AudioCallback, AudioSource, SystemAudioUnavailable};
pub use templates::{MeetingTemplate, Section};
pub use titles::{generate_title, title_client};
pub use watcher::{MeetingWatcher, MicUsageProbe};
pub use writer::{ObsidianWriter, SavedNote, WriteNote};

pub(crate) fn trim(s: &str) -> &str {
    s.trim_matches(py::is_space)
}
pub(crate) fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(py::is_space).filter(|s| !s.is_empty())
}
pub(crate) fn lines(s: &str) -> impl Iterator<Item = &str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if [
            '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
            '\u{2029}',
        ]
        .contains(&c)
        {
            out.push(&s[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && chars.peek().is_some_and(|(_, c)| *c == '\n') {
                let (i, _) = chars.next().unwrap();
                start = i + 1;
            }
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out.into_iter()
}
pub(crate) fn expand(p: &Path) -> PathBuf {
    lf_core::home::expand_user(p).unwrap_or_else(|_| p.to_owned())
}
pub(crate) fn read_text(p: &Path) -> std::io::Result<String> {
    fs::read_to_string(p).map(|s| s.replace("\r\n", "\n").replace('\r', "\n"))
}
pub(crate) fn write_text(p: &Path, s: &str) -> std::io::Result<()> {
    if cfg!(windows) {
        fs::write(p, s.replace('\n', "\r\n"))
    } else {
        fs::write(p, s)
    }
}

pub const SOURCE_ME: &str = "me";
pub const SOURCE_OTHERS: &str = "others";
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    pub offset_seconds: f64,
    pub text: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_seconds: Option<f64>,
    /// Meeting-local diarization id; never rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_label: Option<String>,
}
impl Segment {
    pub fn new(offset_seconds: f64, text: impl Into<String>) -> Self {
        Self {
            offset_seconds,
            text: text.into(),
            source: SOURCE_ME.into(),
            end_seconds: None,
            speaker_id: None,
            speaker_label: None,
        }
    }
    pub fn stamp(&self) -> String {
        let seconds = self.offset_seconds as i64;
        format!(
            "{:02}:{:02}",
            seconds.div_euclid(60),
            seconds.rem_euclid(60)
        )
    }
    /// The cleaned display label when there is one, else the source label.
    pub fn speaker(&self) -> Cow<'_, str> {
        if let Some(label) = self.speaker_label.as_deref().and_then(clean_speaker_name) {
            return Cow::Owned(label);
        }
        Cow::Borrowed(match self.source.as_str() {
            SOURCE_ME => "Me",
            SOURCE_OTHERS => "Others",
            other => other,
        })
    }
}
pub const MAX_SPEAKER_NAME_CHARS: usize = 40;
/// Python's `_is_alnum`: letters (`L*`) and decimal digits (`Nd`) only.
fn is_name_char(c: char) -> bool {
    use unicode_general_category::{get_general_category as cat, GeneralCategory as G};
    matches!(
        cat(c),
        G::UppercaseLetter
            | G::LowercaseLetter
            | G::TitlecaseLetter
            | G::ModifierLetter
            | G::OtherLetter
            | G::DecimalNumber
    )
}
/// A speaker label safe for transcript lines and prompts, or `None`: NFKC,
/// then only letters, decimal digits and `-` `'` `.` survive (everything else
/// becomes a space), whitespace collapsed, capped at
/// [`MAX_SPEAKER_NAME_CHARS`]; a name with no letter or digit is refused.
pub fn clean_speaker_name(name: &str) -> Option<String> {
    use unicode_normalization::UnicodeNormalization;
    let kept: String = name
        .nfkc()
        .map(|c| {
            if is_name_char(c) || "-'.".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let joined = words(&kept).collect::<Vec<_>>().join(" ");
    let label: String = joined.chars().take(MAX_SPEAKER_NAME_CHARS).collect();
    let label = label.trim_end_matches(' ');
    label.chars().any(is_name_char).then(|| label.to_string())
}
pub fn has_speakers(segments: &[Segment]) -> bool {
    segments.iter().any(|s| s.source != SOURCE_ME)
}
/// True when diarization attributed any segment to a speaker id.
pub fn is_diarized(segments: &[Segment]) -> bool {
    segments.iter().any(|s| s.speaker_id.is_some())
}
/// Transcript text plus whether it came from diarized segments. Plain `&str`
/// converts with `diarized = false`; the flag is only ever set explicitly
/// from segment data, never inferred from the text.
#[derive(Clone, Copy, Debug)]
pub struct Transcript<'a> {
    text: &'a str,
    diarized: bool,
}
impl<'a> Transcript<'a> {
    pub fn new(text: &'a str, diarized: bool) -> Self {
        Self { text, diarized }
    }
    pub fn text(&self) -> &'a str {
        self.text
    }
    pub fn diarized(&self) -> bool {
        self.diarized
    }
}
impl<'a> From<&'a str> for Transcript<'a> {
    fn from(text: &'a str) -> Self {
        Self::new(text, false)
    }
}
impl<'a> From<&'a String> for Transcript<'a> {
    fn from(text: &'a String) -> Self {
        Self::new(text, false)
    }
}
fn speaker_note(diarized: bool) -> &'static str {
    if diarized {
        prompts::DIARIZED_SPEAKER_NOTE
    } else {
        prompts::SPEAKER_NOTE
    }
}
pub fn format_transcript(segments: &[Segment]) -> String {
    if has_speakers(segments) || is_diarized(segments) {
        segments
            .iter()
            .map(|s| format!("[{}] {}: {}", s.stamp(), s.speaker(), s.text))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}
static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<{3,}|>{3,}").unwrap());
pub fn neutralize(text: &str) -> String {
    FENCE
        .replace_all(text, |c: &regex::Captures<'_>| c[0][..2].to_string())
        .into()
}
pub fn as_data(text: &str) -> String {
    format!("<<<\n{}\n>>>\n", neutralize(trim(text)))
}
static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^#{1,6}[\s\x1c-\x1f]+(.+?)[\s\x1c-\x1f]*$").unwrap());
fn heading_lines(md: &str) -> Vec<&str> {
    lines(md).filter(|s| HEADING.is_match(s)).collect()
}
pub fn user_headings(md: &str) -> Vec<String> {
    lines(md)
        .filter_map(|s| HEADING.captures(s).map(|c| c[1].into()))
        .collect()
}
fn heading_key(s: &str) -> String {
    let folded: String = s.chars().default_case_fold().collect();
    let normalized: String = folded
        .chars()
        .filter(|c| py::is_word(*c) || py::is_space(*c))
        .map(|c| if c == '_' { ' ' } else { c })
        .collect();
    let normalized = words(&normalized).collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        trim(s).into()
    } else {
        normalized
    }
}
pub fn missing_user_headings(output: &str, user_notes: &str) -> Vec<String> {
    let headings: Vec<_> = user_headings(output)
        .iter()
        .map(|s| heading_key(s))
        .collect();
    let mut next = 0;
    user_headings(user_notes)
        .into_iter()
        .filter(|h| {
            if let Some(i) = headings[next..].iter().position(|s| s == &heading_key(h)) {
                next += i + 1;
                false
            } else {
                true
            }
        })
        .collect()
}
fn render(pattern: &str, fields: &[(&str, String)]) -> String {
    static FIELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([a-z_]+)\}").unwrap());
    FIELD
        .replace_all(pattern, |c: &regex::Captures<'_>| {
            fields
                .iter()
                .find(|(k, _)| *k == &c[1])
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| c[0].into())
        })
        .into()
}
fn template_fields(t: &MeetingTemplate, diarized: bool) -> Vec<(&'static str, String)> {
    let instructions = trim(&t.instructions);
    vec![
        ("sections", t.sections_markdown()),
        (
            "section_list",
            t.section_names()
                .iter()
                .map(|s| format!("## {s}"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "instructions",
            if instructions.is_empty() {
                String::new()
            } else {
                format!("{instructions}\n\n")
            },
        ),
        ("speaker_note", speaker_note(diarized).into()),
        ("data_note", prompts::DATA_NOTE.into()),
        ("enhance", ENHANCE_INSTRUCTION.into()),
    ]
}
pub fn build_notes_prompt(template: Option<&MeetingTemplate>) -> String {
    notes_prompt(template, false)
}
fn notes_prompt(template: Option<&MeetingTemplate>, diarized: bool) -> String {
    let default = templates::resolve_template(None, None);
    render(
        prompts::NOTES_PROMPT,
        &template_fields(template.unwrap_or(&default), diarized),
    )
}
pub fn build_enhance_prompt(user_notes: &str, template: Option<&MeetingTemplate>) -> String {
    enhance_prompt(user_notes, template, false)
}
fn enhance_prompt(user_notes: &str, template: Option<&MeetingTemplate>, diarized: bool) -> String {
    let default = templates::resolve_template(None, None);
    let mut fields = template_fields(template.unwrap_or(&default), diarized);
    let headings = heading_lines(user_notes);
    fields.push(("heading_skeleton",if headings.is_empty(){String::new()}else{format!("Your output MUST start with the user's headings, written exactly as below and in this order, each followed by its expanded points:\n{}\n\n",headings.join("\n"))}));
    fields.push(("user_notes", neutralize(trim(user_notes))));
    render(prompts::ENHANCE_PROMPT, &fields)
}

pub struct MeetingSummarizer<'a> {
    pub llm: &'a dyn LlmProvider,
    pub words_per_chunk: usize,
}
impl<'a> MeetingSummarizer<'a> {
    pub fn new(llm: &'a dyn LlmProvider) -> Self {
        Self {
            llm,
            words_per_chunk: 2500,
        }
    }
    pub fn chunks(&self, text: &str) -> Vec<String> {
        words(text)
            .collect::<Vec<_>>()
            .chunks(self.words_per_chunk)
            .map(|s| s.join(" "))
            .collect()
    }
    /// A plain `&str` transcript gets the channel speaker note; pass
    /// `Transcript::new(text, is_diarized(segments))` for diarized segments.
    pub fn summarize<'t>(
        &self,
        transcript: impl Into<Transcript<'t>>,
        user_notes: &str,
        template: Option<&MeetingTemplate>,
    ) -> String {
        let transcript = transcript.into();
        let (transcript, diarized) = (transcript.text(), transcript.diarized());
        let default = templates::resolve_template(None, None);
        let template = template.unwrap_or(&default);
        let user_notes = trim(user_notes);
        let chunks = self.chunks(transcript);
        if chunks.is_empty() {
            return if user_notes.is_empty() {
                "## Summary\n(No speech captured.)\n".into()
            } else {
                format!("{user_notes}\n")
            };
        }
        let enhanced = !user_notes.is_empty();
        let generate = |prompt: &str| self.llm.complete("", prompt, None);
        let attempt = (|| {
            let prefix = if enhanced {
                enhance_prompt(user_notes, Some(template), diarized)
            } else {
                notes_prompt(Some(template), diarized)
            };
            let mut parts = Vec::new();
            for chunk in chunks {
                parts.push(strip_remote_media(&generate(
                    &(prefix.clone() + &as_data(&chunk)),
                )?));
            }
            if parts.len() == 1 {
                return Ok(parts.remove(0));
            }
            let mut fields = template_fields(template, diarized);
            fields.push(("user_notes", neutralize(user_notes)));
            let merge = render(
                if enhanced {
                    prompts::ENHANCE_MERGE_PROMPT
                } else {
                    prompts::MERGE_PROMPT
                },
                &fields,
            );
            generate(
                &(merge
                    + &parts
                        .iter()
                        .map(|s| as_data(s))
                        .collect::<Vec<_>>()
                        .join("\n")),
            )
        })();
        let mut result = match attempt {
            Ok(s) => s,
            Err(_) => {
                return format!(
                    "## Summary\n(LLM summarization failed — see the linked transcript log.)\n{}",
                    if enhanced {
                        format!("\n## My Notes\n\n{user_notes}\n")
                    } else {
                        String::new()
                    }
                )
            }
        };
        if enhanced {
            let missing = missing_user_headings(&result, user_notes);
            if !missing.is_empty() {
                let prompt = render(
                    prompts::ENHANCE_RETRY_PROMPT,
                    &[
                        (
                            "missing_headings",
                            missing
                                .iter()
                                .map(|h| format!("- {h}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        ("skeleton", heading_lines(user_notes).join("\n")),
                        ("draft", neutralize(&result)),
                    ],
                );
                if let Ok(retry) = generate(&prompt) {
                    let length = |s: &str| words(s).map(|s| s.chars().count()).sum::<usize>();
                    if missing_user_headings(&retry, user_notes).len() < missing.len()
                        && length(&retry) as f64 >= 0.6 * length(&result) as f64
                    {
                        result = retry;
                    }
                }
            }
        }
        strip_remote_media(&result)
    }
}
pub const CHAT_NOT_FOUND: &str = "NOT IN THIS PART";
pub const CHAT_NO_ANSWER: &str = "The transcript doesn't mention that.";
/// A plain `&str` transcript (such as saved-log text) gets the channel
/// speaker note; pass `Transcript::new(text, true)` for diarized segments.
pub fn answer_question<'t>(
    llm: &dyn LlmProvider,
    transcript: impl Into<Transcript<'t>>,
    question: &str,
    words_per_chunk: usize,
) -> Result<String, lf_providers::ProviderError> {
    let transcript = transcript.into();
    let (transcript, diarized) = (transcript.text(), transcript.diarized());
    let count = words(transcript).count();
    if count == 0 {
        return Ok(CHAT_NO_ANSWER.into());
    }
    let speaker = format!("{}\n{}", speaker_note(diarized), prompts::DATA_NOTE);
    let call = |pattern: &str, mut fields: Vec<(&str, String)>| {
        fields.extend([
            ("question", question.into()),
            ("speaker_note", speaker.clone()),
        ]);
        llm.complete("", &render(pattern, &fields), None)
            .map(|s| trim(&s).to_string())
    };
    if count <= words_per_chunk {
        return call(
            prompts::CHAT_PROMPT,
            vec![("transcript", neutralize(trim(transcript)))],
        );
    }
    let mut parts = Vec::new();
    let mut current = Vec::new();
    let mut count = 0;
    for line in lines(transcript) {
        let n = words(line).count();
        if !current.is_empty() && count + n > words_per_chunk {
            parts.push(current.join("\n"));
            current.clear();
            count = 0;
        }
        current.push(line);
        count += n;
    }
    if !current.is_empty() {
        parts.push(current.join("\n"));
    }
    let total = parts.len();
    let mut answers = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let reply = call(
            prompts::CHAT_PART_PROMPT,
            vec![
                ("index", (i + 1).to_string()),
                ("total", total.to_string()),
                ("not_found", CHAT_NOT_FOUND.into()),
                ("transcript", neutralize(part)),
            ],
        )?;
        if !reply.is_empty() && !reply.to_uppercase().contains(CHAT_NOT_FOUND) {
            answers.push(reply);
        }
    }
    match answers.len() {
        0 => Ok(CHAT_NO_ANSWER.into()),
        1 => Ok(answers.remove(0)),
        _ => call(
            prompts::CHAT_COMBINE_PROMPT,
            vec![("answers", answers.join("\n\n---\n\n"))],
        ),
    }
}
pub fn read_transcript_log(path: &Path) -> std::io::Result<String> {
    let mut text = read_text(path)?;
    if text.starts_with("---\n") {
        if let Some(end) = text[4..].find("\n---\n") {
            text = text[end + 9..].into();
        }
    }
    let text = text.split("\n## My raw notes\n").next().unwrap_or("");
    Ok(trim(
        &lines(trim(text))
            .filter(|l| !l.starts_with("Notes: [["))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .into())
}
pub fn latest_transcript_log(root: &Path) -> std::io::Result<Option<PathBuf>> {
    fn visit(root: &Path, paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path.is_file()
                && path.file_name().is_some_and(|s| {
                    if cfg!(windows) {
                        s.to_string_lossy()
                            .to_lowercase()
                            .ends_with(" (transcript).md")
                    } else {
                        s.to_string_lossy().ends_with(" (transcript).md")
                    }
                })
            {
                paths.push(path);
            } else if path.is_dir() && !path.is_symlink() {
                visit(&path, paths)?;
            }
        }
        Ok(())
    }
    let root = expand(root);
    if !root.is_dir() {
        return Ok(None);
    }
    let mut paths = Vec::new();
    visit(&root, &mut paths)?;
    let mut best = None;
    let mut stamp = None;
    for path in paths {
        let ts = path.metadata()?.modified()?;
        if stamp.is_none_or(|old| ts > old) {
            stamp = Some(ts);
            best = Some(path);
        }
    }
    Ok(best)
}
