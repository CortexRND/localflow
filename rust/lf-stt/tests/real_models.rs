#[cfg(any(feature = "whisper", feature = "parakeet"))]
#[path = "../examples/support/mod.rs"]
mod support;

#[cfg(any(feature = "whisper", feature = "parakeet"))]
fn exercise(mut engine: impl lf_stt::SpeechEngine, model: &str) {
    let _lease = lf_stt::stt_lease::record(engine.id(), model, None, true).unwrap();
    let clip = support::read_clip(None).unwrap();
    eprintln!("{} (source: {} Hz)", clip.name, clip.original_sample_rate);
    engine.download().unwrap();
    engine.load().unwrap();
    assert!(engine.loaded());
    assert!(engine.transcribe(&[], None).unwrap().text.is_empty());
    let transcript = engine
        .transcribe(&clip.pcm, Some("Speech recognition."))
        .unwrap();
    assert!(!transcript.text.trim().is_empty());
    let audio_seconds = clip.pcm.len() as f64 / 16000.0;
    assert!(!transcript.segments.is_empty());
    for span in &transcript.segments {
        assert!(0.0 <= span.start && span.start <= span.end && span.end <= audio_seconds);
        assert!(!span.text.trim().is_empty());
    }
    for pair in transcript.segments.windows(2) {
        assert!(pair[0].start <= pair[1].start);
    }
    assert!(engine.transcribe(&[f32::NAN; 512], None).is_err());
    engine.unload();
    assert!(!engine.loaded());
    assert!(engine.transcribe(&clip.pcm, None).is_err());
}

/// Downloads models: `cargo test -p lf-stt whisper_real_model -- --ignored`.
#[cfg(feature = "whisper")]
#[test]
#[ignore]
fn whisper_real_model() {
    exercise(
        lf_stt::engines::WhisperEngine::new("base.en", Some("en".into()), "cpu").unwrap(),
        "base.en",
    );
}

/// Downloads models: `cargo test -p lf-stt parakeet_real_model -- --ignored`.
#[cfg(feature = "parakeet")]
#[test]
#[ignore]
fn parakeet_real_model() {
    exercise(lf_stt::engines::ParakeetEngine::new("cpu").unwrap(), "base");
}
