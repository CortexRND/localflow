//! The shared alignment golden table (`tests/fixtures/diarization_align.json`),
//! also read by `tests/test_diarization_align.py`.

use lf_diarize::{assign_speakers, merge_runs, SpeakerTurn, TimedText, GAP_TOLERANCE_S};
use serde_json::Value;

fn golden() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/diarization_align.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn timed(value: &Value) -> TimedText {
    TimedText {
        start: value["start"].as_f64().unwrap(),
        end: value["end"].as_f64().unwrap(),
        text: value["text"].as_str().unwrap().into(),
    }
}

#[test]
fn fixture_header_matches_constants() {
    let golden = golden();
    assert_eq!(golden["version"], 1);
    assert_eq!(golden["gap_tolerance_s"].as_f64(), Some(GAP_TOLERANCE_S));
    assert_eq!(golden["tie_eps"].as_f64(), Some(1e-9));
    assert!(golden["assign_cases"].as_array().unwrap().len() >= 20);
    assert!(golden["merge_cases"].as_array().unwrap().len() >= 10);
}

#[test]
fn every_assign_case_matches_python() {
    for case in golden()["assign_cases"].as_array().unwrap() {
        let texts: Vec<TimedText> = case["timed"]
            .as_array()
            .unwrap()
            .iter()
            .map(timed)
            .collect();
        let turns: Vec<SpeakerTurn> = case["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| SpeakerTurn {
                start: t["start"].as_f64().unwrap(),
                end: t["end"].as_f64().unwrap(),
                local_speaker: t["local_speaker"].as_u64().unwrap() as usize,
            })
            .collect();
        let got: Vec<Value> = assign_speakers(&texts, &turns)
            .into_iter()
            .map(|(_, s)| s.map_or(Value::Null, Value::from))
            .collect();
        assert_eq!(Value::from(got), case["expected"], "{}", case["name"]);
    }
}

#[test]
fn every_merge_case_matches_python_with_int_null_and_string_keys() {
    for case in golden()["merge_cases"].as_array().unwrap() {
        let items: Vec<(TimedText, Value)> = case["assigned"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| (timed(a), a["speaker"].clone()))
            .collect();
        let assigned: Vec<(&TimedText, Value)> =
            items.iter().map(|(t, k)| (t, k.clone())).collect();
        let runs = merge_runs(&assigned, case["chunk_offset"].as_f64().unwrap());
        let got: Vec<Value> = runs
            .into_iter()
            .map(|r| serde_json::json!({"start": r.start, "end": r.end, "text": r.text, "speaker": r.speaker}))
            .collect();
        let expected: Vec<Value> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| serde_json::json!({"start": e["start"].as_f64(), "end": e["end"].as_f64(), "text": e["text"], "speaker": e["speaker"]}))
            .collect();
        assert_eq!(got, expected, "{}", case["name"]);
    }
}

#[test]
fn merge_uses_python_whitespace_strip() {
    let items = [TimedText {
        start: 0.0,
        end: 1.0,
        text: "\u{1c} hi\u{3000}".into(),
    }];
    let assigned = [(&items[0], 0)];
    assert_eq!(merge_runs(&assigned, 0.0)[0].text, "hi");
}
