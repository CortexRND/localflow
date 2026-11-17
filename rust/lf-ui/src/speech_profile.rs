//! Explicit, isolated macOS CI probe of the real embedded speech path.

use crate::speech_memory::SpeechMemory;
use lf_runtime::engine::SpeechEngine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn helper_pid() -> anyhow::Result<Option<u32>> {
    let children = Command::new("pgrep")
        .args(["-P", &std::process::id().to_string()])
        .output()?;
    for pid in String::from_utf8_lossy(&children.stdout).split_whitespace() {
        let args = Command::new("ps")
            .args(["-p", pid, "-o", "args="])
            .output()?;
        if String::from_utf8_lossy(&args.stdout).contains("--speech-helper") {
            return Ok(Some(pid.parse()?));
        }
    }
    Ok(None)
}

fn metrics(pid: u32) -> anyhow::Result<Value> {
    let output = Command::new(
        std::env::var_os("LF_MEMORY_PROBE")
            .ok_or_else(|| anyhow::anyhow!("memory probe missing"))?,
    )
    .arg(pid.to_string())
    .output()?;
    anyhow::ensure!(output.status.success(), "memory probe failed for {pid}");
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn sample(rows: &mut Vec<Value>, stage: &str) -> anyhow::Result<()> {
    let pid = helper_pid()?;
    let main = metrics(std::process::id())?;
    let child = pid.map(metrics).transpose()?;
    rows.push(json!({"stage":stage,"main":main,"helper_pid":pid,"helper":child,
        "total_rss":main["rss"].as_u64().unwrap()+child.as_ref().and_then(|v| v["rss"].as_u64()).unwrap_or(0),
        "total_footprint":main["footprint"].as_u64().unwrap()+child.as_ref().and_then(|v| v["footprint"].as_u64()).unwrap_or(0)}));
    Ok(())
}

fn assert_gone(pid: Option<u32>) -> anyhow::Result<()> {
    if let Some(pid) = pid {
        anyhow::ensure!(
            !Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "pid="])
                .output()?
                .status
                .success(),
            "helper pid {pid} survived unload"
        );
    }
    Ok(())
}

fn run(output: &Path, speech: Arc<dyn SpeechEngine>, memory: SpeechMemory) -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var_os("LOCALFLOW_CONFIG").is_some(),
        "profile requires isolated config"
    );
    let bytes = std::fs::read(
        std::env::var_os("LF_HUB_SPEECH_PCM")
            .ok_or_else(|| anyhow::anyhow!("profile PCM missing"))?,
    )?;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len().is_multiple_of(4),
        "invalid profile PCM"
    );
    let pcm: Vec<_> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|v| f32::from_le_bytes(*v))
        .collect();
    std::thread::sleep(Duration::from_secs(3));
    let mut rows = Vec::new();
    sample(&mut rows, "idle")?;
    let start = Instant::now();
    speech.load().map_err(anyhow::Error::msg)?;
    let cold_load_ms = start.elapsed().as_secs_f64() * 1000.0;
    sample(&mut rows, "loaded")?;
    let start = Instant::now();
    let transcript = speech.transcribe(&pcm, None).map_err(anyhow::Error::msg)?;
    let cold_inference_ms = start.elapsed().as_secs_f64() * 1000.0;
    sample(&mut rows, "transcribed")?;
    let start = Instant::now();
    anyhow::ensure!(
        speech.transcribe(&pcm, None).map_err(anyhow::Error::msg)? == transcript,
        "warm transcript changed"
    );
    let warm_inference_ms = start.elapsed().as_secs_f64() * 1000.0;
    let old_pid = helper_pid()?;
    anyhow::ensure!(
        memory.poll(false, Duration::ZERO, Instant::now()),
        "explicit eviction did not unload model"
    );
    anyhow::ensure!(
        !speech.loaded() && helper_pid()?.is_none(),
        "helper survived eviction"
    );
    assert_gone(old_pid)?;
    sample(&mut rows, "unloaded")?;
    let start = Instant::now();
    speech.load().map_err(anyhow::Error::msg)?;
    let reload_ms = start.elapsed().as_secs_f64() * 1000.0;
    if let Some(pid) = old_pid {
        anyhow::ensure!(helper_pid()? != Some(pid), "old helper reused");
    }
    let start = Instant::now();
    anyhow::ensure!(
        speech.transcribe(&pcm, None).map_err(anyhow::Error::msg)? == transcript,
        "reload transcript changed"
    );
    let reload_inference_ms = start.elapsed().as_secs_f64() * 1000.0;
    sample(&mut rows, "reloaded")?;
    let reload_pid = helper_pid()?;
    crate::config_write::update(None, |config| {
        let mut config = config.clone();
        config.set_field("dictation_enabled", json!(false));
        Ok(config)
    })?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while speech.loaded() || helper_pid()?.is_some() {
        anyhow::ensure!(
            Instant::now() < deadline,
            "dictation-off watcher did not evict speech"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    sample(&mut rows, "dictation_off")?;
    assert_gone(reload_pid)?;
    let pending = output.with_extension("pending");
    std::fs::write(
        &pending,
        serde_json::to_vec_pretty(&json!({"rows":rows,"transcript":transcript,
        "cold_load_ms":cold_load_ms,"cold_inference_ms":cold_inference_ms,"warm_inference_ms":warm_inference_ms,
        "reload_ms":reload_ms,"reload_inference_ms":reload_inference_ms}))?,
    )?;
    std::fs::rename(pending, output)?;
    Ok(())
}

pub(crate) fn start(speech: Arc<dyn SpeechEngine>, memory: SpeechMemory) {
    let Some(output) = std::env::var_os("LF_HUB_SPEECH_PROFILE").map(PathBuf::from) else {
        return;
    };
    std::thread::spawn(move || {
        if let Err(error) = run(&output, speech, memory) {
            let _ = std::fs::write(output.with_extension("error"), format!("{error:#}"));
        }
    });
}
