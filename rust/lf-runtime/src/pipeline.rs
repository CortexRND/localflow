//! The dictation text pipeline and job processing (`app._process_clip`,
//! `app._process_job`): silence gate → STT with the dictionary prompt →
//! hot phrases → replacements → spoken formatting → LLM cleanup → (park hot
//! phrases) → spoken symbols → style → restore.

use std::cell::Cell;
use std::path::PathBuf;
use std::time::Instant;

use lf_core::config::Config;
use lf_core::dictionary::{apply_replacements, stt_prompt};
use lf_core::formatting::{apply_spoken_formatting, apply_style};
use lf_core::history::{HistoryStore, NewEntry};
use lf_core::hotphrases::{render_dictation, RenderSteps};
use lf_core::perf_ledger::{self, Kind, LlmLabels, PerfEvent, PerfLedger, Status, SttLabels};
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

/// Per-stage wall times of one job, in milliseconds (`None`: stage not run).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StageTimings {
    pub stt_ms: Option<f64>,
    pub llm_ms: Option<f64>,
    /// Text pipeline time excluding the LLM call.
    pub render_ms: Option<f64>,
    /// Whether an LLM call was made (cleanup, command or transform).
    pub llm_used: bool,
}

/// Where and how a worker job's timings are recorded.
pub(crate) struct PerfContext<'a> {
    pub ledger: &'a PerfLedger,
    pub queued_at: Option<Instant>,
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
    render_text_timed(text, config, vocab, llm, app_info, min_clean_words, None)
}

/// `render_text_with`, adding the cleanup LLM's wall time to `llm_ms`.
fn render_text_timed(
    text: &str,
    config: &Config,
    vocab: &Vocabulary,
    llm: Option<&dyn LlmProvider>,
    app_info: Option<&AppInfo>,
    min_clean_words: usize,
    llm_ms: Option<&Cell<Option<f64>>>,
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
        Some(c) => {
            let started = Instant::now();
            let cleaned = c.clean(t, &level, style_name.as_deref(), &vocab.words);
            if let Some(total) = llm_ms {
                total.set(Some(
                    total.get().unwrap_or(0.0) + perf_ledger::ms(started.elapsed()),
                ));
            }
            cleaned
        }
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
    process_clip_timed(
        audio,
        config,
        models,
        vocab,
        app_info,
        &mut StageTimings::default(),
    )
}

/// [`process_clip`], filling in the stage timings.
pub fn process_clip_timed(
    audio: &[f32],
    config: &Config,
    models: &Models,
    vocab: &Vocabulary,
    app_info: Option<&AppInfo>,
    timings: &mut StageTimings,
) -> Result<(String, String), String> {
    let start = std::time::Instant::now();
    let threshold = f64_field(config, "silence_threshold_dbfs", -50.0);
    if rms_dbfs(audio) < threshold {
        log::info!("silence gate: audio below {threshold:.1} dBFS");
        return Ok((String::new(), String::new()));
    }
    let prompt = stt_prompt(&vocab.words);
    let stt_started = Instant::now();
    let raw = models.speech.transcribe(audio, prompt.as_deref());
    timings.stt_ms = Some(perf_ledger::ms(stt_started.elapsed()));
    let raw = raw?;
    let render_started = Instant::now();
    let llm_ms = Cell::new(None);
    let text = render_text_timed(&raw, config, vocab, models.llm, app_info, 5, Some(&llm_ms));
    let llm_ms = llm_ms.get();
    timings.llm_used = llm_ms.is_some();
    timings.llm_ms = Some(llm_ms.unwrap_or(0.0));
    timings.render_ms =
        Some((perf_ledger::ms(render_started.elapsed()) - llm_ms.unwrap_or(0.0)).max(0.0));
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
    process_job_guarded(job, config, models, vocab, history, None, || Some(())).0
}

/// The LLM a job's event is labelled with: the fast model for light dictation
/// cleanup when one is set (the worker uses it then), else the main model.
fn llm_labels(job: &Job, config: &Config) -> LlmLabels {
    match config.fast_model_config() {
        Some(fast) if job.kind == JobKind::Dictate && config.uses_light_cleanup() => {
            LlmLabels::from_config(&fast)
        }
        _ => LlmLabels::from_config(config),
    }
}

/// One job's ledger event: numbers and model labels only, never text.
#[allow(clippy::too_many_arguments)]
fn job_event(
    job: &Job,
    config: &Config,
    ok: bool,
    duration_s: f64,
    queue_ms: Option<f64>,
    timings: &StageTimings,
    total_ms: f64,
    chars: Option<usize>,
) -> PerfEvent {
    let kind = match job.kind {
        JobKind::Dictate => Kind::Dictate,
        JobKind::Command => Kind::Command,
        JobKind::Transform => Kind::Transform,
    };
    PerfEvent {
        audio_s: job.audio.is_some().then_some(duration_s),
        queue_ms,
        stt_ms: timings.stt_ms,
        llm_ms: timings.llm_ms,
        render_ms: timings.render_ms,
        total_ms: Some(total_ms),
        stt: job.audio.is_some().then(|| {
            SttLabels::new(
                &str_field(config, "stt_backend"),
                &str_field(config, "model_size"),
                &str_field(config, "stt_device"),
            )
        }),
        llm: timings.llm_used.then(|| llm_labels(job, config)),
        chars: chars.map(|n| n as u64),
        ..PerfEvent::new(kind, Status::from_ok(ok))
    }
}

pub(crate) fn process_job_guarded<G>(
    job: &Job,
    config: &Config,
    models: &Models,
    vocab: &Vocabulary,
    history: Option<&HistoryStore>,
    perf: Option<PerfContext<'_>>,
    begin_commit: impl FnOnce() -> Option<G>,
) -> ((Option<String>, bool), Option<G>) {
    let started = Instant::now();
    let queue_ms = perf
        .as_ref()
        .and_then(|p| p.queued_at)
        .map(|at| perf_ledger::ms(started.saturating_duration_since(at)));
    let mut timings = StageTimings::default();
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
                let (raw, final_text) = process_clip_timed(
                    audio,
                    config,
                    models,
                    vocab,
                    job.app_info.as_ref(),
                    &mut timings,
                )?;
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
                let timed_transform = |timings: &mut StageTimings, prompt: &str| {
                    let llm_started = Instant::now();
                    let result = run_transform(llm, prompt);
                    timings.llm_ms = Some(perf_ledger::ms(llm_started.elapsed()));
                    timings.llm_used = true;
                    result
                };
                if kind == JobKind::Command {
                    let audio = job.audio.as_deref().unwrap_or_default();
                    let stt_started = Instant::now();
                    let raw = models.speech.transcribe(audio, None);
                    timings.stt_ms = Some(perf_ledger::ms(stt_started.elapsed()));
                    raw_text = raw?;
                    if raw_text.trim().is_empty() {
                        log::error!("command mode received an empty instruction");
                        error = true;
                        status = "silent";
                    } else {
                        text = timed_transform(
                            &mut timings,
                            &command_prompt(&raw_text, &job.selection),
                        );
                    }
                } else {
                    raw_text = job.selection.clone();
                    let preset = job.preset.as_deref().unwrap_or_default();
                    text =
                        timed_transform(&mut timings, &transform_prompt(preset, &job.selection)?);
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
    // Timed before history and paste: the pipeline itself, not bookkeeping.
    let total_ms = perf_ledger::ms(started.elapsed());
    let commit = begin_commit();
    if let (Some(perf), Some(_)) = (&perf, &commit) {
        let chars = text.as_ref().map_or(0, |t| t.chars().count());
        let event = job_event(
            job,
            config,
            !error,
            duration_s,
            queue_ms,
            &timings,
            total_ms,
            Some(chars),
        );
        perf.ledger.record_if(config, &event);
    }
    if commit.is_none() {
        if let Some(history) = history {
            history.delete_clip(audio_path.as_deref());
        }
        return ((None, false), None);
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
    ((text, error), commit)
}
