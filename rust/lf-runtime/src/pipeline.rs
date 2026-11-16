//! The dictation text pipeline and job processing (`app._process_clip`,
//! `app._process_job`): silence gate → STT with the dictionary prompt →
//! hot phrases → replacements → spoken formatting → LLM cleanup → (park hot
//! phrases) → spoken symbols → style → restore.

use std::path::PathBuf;

use lf_core::config::Config;
use lf_core::dictionary::{apply_replacements, stt_prompt};
use lf_core::formatting::{apply_spoken_formatting, apply_style};
use lf_core::history::{HistoryStore, NewEntry};
use lf_core::hotphrases::{render_dictation, RenderSteps};
use lf_core::symbols::apply_spoken_symbols;
use lf_providers::cleanup::Cleaner;
use lf_providers::LlmProvider;

use crate::context::{category_for, AppInfo};
use crate::controller::{Job, JobKind};
use crate::engine::{rms_dbfs, SpeechEngine};
use crate::settings::{bool_field, f64_field, i64_field, str_field};
use crate::transform::{command_prompt, run_transform, transform_prompt};

/// Dictionary and hot-phrase data for one job.
#[derive(Clone, Debug, Default)]
pub struct Vocabulary {
    pub words: Vec<String>,
    pub replacements: Vec<(String, String)>,
    pub hot_phrases: Vec<(String, String)>,
    pub commands: Vec<String>,
}

/// The models one job runs with (snapshotted per job).
pub struct Models<'a> {
    pub speech: &'a dyn SpeechEngine,
    pub llm: Option<&'a dyn LlmProvider>,
}

fn style_for(config: &Config, app_info: Option<&AppInfo>) -> Option<String> {
    if !bool_field(config, "styles_enabled", true) {
        return None;
    }
    let overrides = config
        .field_str_list("app_category_overrides")
        .unwrap_or_default();
    let category = category_for(app_info, &overrides);
    let style = str_field(config, &format!("style_{category}"));
    (!style.is_empty()).then_some(style)
}

/// Text after the cleanup pipeline for an already transcribed utterance.
/// Cleanup only pays off on real sentences: fragments under five words skip it.
pub fn render_text(
    text: &str,
    config: &Config,
    vocab: &Vocabulary,
    llm: Option<&dyn LlmProvider>,
    app_info: Option<&AppInfo>,
) -> String {
    render_text_with(text, config, vocab, llm, app_info, 5)
}

/// `render_text` with the minimum word count for LLM cleanup (the server's
/// `/transcribe` cleans any length: 0).
pub fn render_text_with(
    text: &str,
    config: &Config,
    vocab: &Vocabulary,
    llm: Option<&dyn LlmProvider>,
    app_info: Option<&AppInfo>,
    min_clean_words: usize,
) -> String {
    if text.is_empty() {
        return String::new();
    }
    let phrases: &[(String, String)] = if bool_field(config, "hot_phrases", true) {
        &vocab.hot_phrases
    } else {
        &[]
    };
    let style_name = style_for(config, app_info);
    let level = if bool_field(config, "cleanup_enabled", true) {
        str_field(config, "cleanup_level")
    } else {
        "none".into()
    };
    let replace = |t: &str| apply_replacements(t, &vocab.replacements);
    let fmt = |t: &str| apply_spoken_formatting(t);
    let cleaner = llm.map(Cleaner::new);
    let clean = |t: &str| match &cleaner {
        Some(c) => c.clean(t, &level, style_name.as_deref(), &vocab.words),
        None => t.to_string(),
    };
    let symbols = |t: &str| apply_spoken_symbols(t, &vocab.commands);
    let style = |t: &str| apply_style(t, style_name.as_deref().unwrap_or_default(), &vocab.words);
    let use_clean = level != "none"
        && !level.is_empty()
        && llm.is_some()
        && text.split_whitespace().count() >= min_clean_words;
    let steps = RenderSteps {
        replace: Some(&replace),
        fmt: bool_field(config, "spoken_punctuation", true)
            .then_some(&fmt as &dyn Fn(&str) -> String),
        clean: use_clean.then_some(&clean as &dyn Fn(&str) -> String),
        symbols: bool_field(config, "spoken_symbols", true)
            .then_some(&symbols as &dyn Fn(&str) -> String),
        style: style_name
            .is_some()
            .then_some(&style as &dyn Fn(&str) -> String),
    };
    render_dictation(text, phrases, &steps)
}

/// `_process_clip` without the paste: (raw transcript, final text).
pub fn process_clip(
    audio: &[f32],
    config: &Config,
    models: &Models,
    vocab: &Vocabulary,
    app_info: Option<&AppInfo>,
) -> Result<(String, String), String> {
    let start = std::time::Instant::now();
    let threshold = f64_field(config, "silence_threshold_dbfs", -50.0);
    if rms_dbfs(audio) < threshold {
        log::info!("silence gate: audio below {threshold:.1} dBFS");
        return Ok((String::new(), String::new()));
    }
    let prompt = stt_prompt(&vocab.words);
    let raw = models.speech.transcribe(audio, prompt.as_deref())?;
    let text = render_text(&raw, config, vocab, models.llm, app_info);
    // Lengths and timings only: transcripts are never logged.
    log::info!(
        "transcribed {} chars in {}ms",
        text.chars().count(),
        start.elapsed().as_millis()
    );
    Ok((raw, text))
}

/// `_process_job`: (text to paste, error).
pub fn process_job(
    job: &Job,
    config: &Config,
    models: &Models,
    vocab: &Vocabulary,
    history: Option<&HistoryStore>,
) -> (Option<String>, bool) {
    let mut raw_text = String::new();
    let mut text: Option<String> = None;
    let mut status = "failed";
    let mut error = false;
    let rate = i64_field(config, "sample_rate", 16000);
    let duration_s = match (&job.audio, rate) {
        (Some(a), r) if r > 0 => a.len() as f64 / r as f64,
        _ => 0.0,
    };
    let history = history.filter(|h| h.enabled);
    let mut audio_path: Option<PathBuf> = None;
    if job.kind == JobKind::Dictate {
        if let (Some(h), Some(audio)) = (history, &job.audio) {
            match h.write_clip(audio, rate) {
                Ok(p) => audio_path = Some(p),
                Err(e) => log::warn!("could not store dictation clip: {e}"),
            }
        }
    }
    let result: Result<(), String> = (|| {
        match job.kind {
            JobKind::Dictate => {
                let audio = job.audio.as_deref().unwrap_or_default();
                let (raw, final_text) =
                    process_clip(audio, config, models, vocab, job.app_info.as_ref())?;
                raw_text = raw;
                status = if final_text.is_empty() {
                    "silent"
                } else {
                    "ok"
                };
                text = (!final_text.is_empty()).then_some(final_text);
            }
            JobKind::Command
                if str_field(config, "llm_provider") == "ollama"
                    && (!bool_field(config, "cleanup_enabled", true)
                        || str_field(config, "llm_model").is_empty()) =>
            {
                log::error!("command mode needs a local LLM — choose one in lf setup");
                error = true;
            }
            kind => {
                let Some(llm) = models.llm else {
                    error = true;
                    return Ok(());
                };
                if kind == JobKind::Command {
                    let audio = job.audio.as_deref().unwrap_or_default();
                    raw_text = models.speech.transcribe(audio, None)?;
                    if raw_text.trim().is_empty() {
                        log::error!("command mode received an empty instruction");
                        error = true;
                        status = "silent";
                    } else {
                        text = run_transform(llm, &command_prompt(&raw_text, &job.selection));
                    }
                } else {
                    raw_text = job.selection.clone();
                    let preset = job.preset.as_deref().unwrap_or_default();
                    text = run_transform(llm, &transform_prompt(preset, &job.selection)?);
                }
                if text.is_some() {
                    status = "ok";
                } else if status != "silent" {
                    status = "failed";
                    error = true;
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        log::error!("transcription pipeline failed: {e}");
        status = "failed";
        error = true;
    }
    if let Some(h) = history {
        if status == "silent" {
            h.delete_clip(audio_path.as_deref());
            audio_path = None;
        }
        let overrides = config
            .field_str_list("app_category_overrides")
            .unwrap_or_default();
        let info = job.app_info.as_ref();
        let mut entry = NewEntry::new(job.kind.as_str());
        entry.app_name = info.map(|i| i.name.clone());
        entry.bundle_id = info.map(|i| i.bundle_id.clone());
        entry.category = Some(category_for(info, &overrides));
        entry.duration_s = duration_s;
        entry.raw_text = raw_text;
        entry.final_text = text.clone().unwrap_or_default();
        entry.status = status.to_string();
        entry.audio_path = audio_path.map(|p| p.to_string_lossy().into_owned());
        match h.add(&entry) {
            Ok(Some(_)) if job.kind == JobKind::Dictate && status == "ok" => {
                if let Err(e) = h.prune_audio_clips(i64_field(config, "keep_audio_clips", 5)) {
                    log::warn!("could not prune audio clips: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => log::error!("could not save dictation history: {e}"),
        }
    }
    (text, error)
}
