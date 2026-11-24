use std::sync::Arc;

use anyhow::Result;
use lf_core::config::{load_config, Config};
use lf_core::perf_ledger::{self, Kind, PerfEvent, PerfLedger, Status};
use lf_meetings::{system_audio, AudioSource, Transcriber};
use lf_meetings_backend::{CaptureFactory, MeetingsBackend};
use lf_runtime::engine::SpeechEngine;
use lf_server::{Server, ServerState};
use lf_stt::{runtime::LazySpeechAdapter, server::SttBackend};

use crate::{outln, Io};

struct MeetingSpeech(LazySpeechAdapter);

impl Transcriber for MeetingSpeech {
    fn transcribe(&self, audio: &[f32], prompt: Option<&str>) -> Result<String> {
        self.0.transcribe(audio, prompt).map_err(anyhow::Error::msg)
    }
}

struct MeetingCapture;

impl CaptureFactory for MeetingCapture {
    fn transcriber(&self, config: &Config) -> Result<Arc<dyn Transcriber>> {
        // Meeting capture resamples to 16 kHz itself; the dictation
        // `sample_rate` setting only governs the dictation recorder.
        let speech = LazySpeechAdapter::for_16khz_pcm(config.clone());
        speech.load().map_err(anyhow::Error::msg)?;
        Ok(Arc::new(MeetingSpeech(speech)))
    }

    fn microphone(&self, config: &Config) -> Result<Box<dyn AudioSource>> {
        Ok(Box::new(system_audio::CpalSource::microphone_named(
            config.field_str("input_device"),
        )?))
    }

    fn system_audio(&self, config: &Config) -> Result<Option<Box<dyn AudioSource>>> {
        Ok(Some(system_audio::open_system_audio(
            system_audio::SAMPLE_RATE,
            config.field_str("system_audio_device").unwrap_or_default(),
        )?))
    }
}

pub(crate) fn run(host: Option<String>, port: Option<u16>, io: &mut Io<'_>) -> Result<()> {
    let started = std::time::Instant::now();
    let config = load_config()?;
    let host = host
        .as_deref()
        .unwrap_or_else(|| config.field_str("server_host").unwrap_or("127.0.0.1"));
    let port = match port {
        Some(port) => port,
        None => u16::try_from(config.field_i64("server_port").unwrap_or(8756))
            .map_err(|_| anyhow::anyhow!("server_port must be between 0 and 65535"))?,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(16)
        .enable_all()
        .build()?;
    let ledger = PerfLedger::default();
    let meetings = MeetingsBackend::new()?
        .with_capture(Arc::new(MeetingCapture))
        .with_perf_ledger(ledger.clone());
    let backend = SttBackend::new(meetings);
    let state = Arc::new(ServerState::load_with_backend(Arc::new(backend))?);
    let server = runtime.block_on(Server::bind_with_state(host, port, state.clone()))?;
    // Ready to accept requests: config, backends and the bound socket.
    let elapsed = perf_ledger::ms(started.elapsed());
    ledger.record_if(
        &config,
        &PerfEvent {
            total_ms: Some(elapsed),
            ..PerfEvent::new(Kind::Startup, Status::Ok)
        },
    );
    outln!(
        io.out,
        "LocalFlow Rust server listening on {}",
        lf_server::server_url::base_url(host, server.local_addr()?.port())
    )?;
    io.out.flush()?;
    let served = runtime.block_on(server.run());
    let shutdown = state.shutdown();
    served.and(shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn meeting_transcriber_loads_up_front_ignoring_the_dictation_sample_rate() {
        // An unknown provider fails before any model download; reaching that
        // error from the factory proves the model is loaded before capture
        // and that the 22,050 Hz dictation setting was not checked.
        let mut config = Config::default();
        config.set_field("sample_rate", json!(22050));
        config.set_field("stt_backend", json!("test-unknown"));
        let error = MeetingCapture.transcriber(&config).err().unwrap();
        assert_eq!(
            error.to_string(),
            "unsupported speech provider: test-unknown"
        );
    }
}
