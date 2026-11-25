//! People: a read-only list of the voices LocalFlow remembers
//! (`GET /api/speakers`).
//!
//! Both servers answer that route only while `speaker_diarization` and
//! `speaker_recognition` are on (403 otherwise), so [`enabled`] is the one gate
//! for the sidebar entry, routing, the page and the request. Nothing here
//! writes: no toggle, consent, enrollment, rename or forget, and the export
//! route (the only one with voice vectors) is never called. The reply is
//! read through an allow-list: only a name, the date added and the sample
//! count are kept. Ids, model ids, the server's embedder and anything else
//! (voice-data health, any vector-like field) are dropped at parse time.

use lf_core::config::Config;
use serde_json::Value;

/// The single gate: both flags on in the config this window uses.
pub fn enabled(config: &Config) -> bool {
    config.field_bool("speaker_diarization").unwrap_or(false)
        && config.field_bool("speaker_recognition").unwrap_or(false)
}

/// One remembered voice, as shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    /// `YYYY-MM-DD`, or empty when the server sent no usable date.
    pub added: String,
    pub samples: u64,
}

/// What `GET /api/speakers` answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum People {
    List(Vec<Person>),
    /// 403: the server has speaker recognition off.
    Off,
    /// Nothing answered at the address.
    Offline,
    Error(String),
}

/// Longest name drawn (the servers cap names well below this).
const NAME_CHARS: usize = 60;

/// The allow-listed people of a reply, in server order.
pub fn parse(reply: &Value) -> Vec<Person> {
    reply
        .get("speakers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name: String = item
                .get("name")?
                .as_str()?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(NAME_CHARS)
                .collect();
            if name.is_empty() {
                return None;
            }
            let added = item
                .get("created")
                .and_then(Value::as_str)
                .and_then(|text| text.get(..10))
                .filter(|day| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok())
                .unwrap_or("")
                .to_string();
            let samples = item.get("samples").and_then(Value::as_u64).unwrap_or(0);
            Some(Person {
                name,
                added,
                samples,
            })
        })
        .collect()
}

/// The supporting line of one row.
pub fn detail(person: &Person) -> String {
    let samples = match person.samples {
        0 => String::new(),
        1 => "1 sample".to_string(),
        n => format!("{n} samples"),
    };
    match (person.added.is_empty(), samples.is_empty()) {
        (false, false) => format!("Added {} · {samples}", person.added),
        (false, true) => format!("Added {}", person.added),
        (true, false) => samples,
        (true, true) => String::new(),
    }
}

/// Page state: the last answer, and a generation so a late reply (or one
/// that lands after the gate closed) never replaces newer state.
#[derive(Clone, Debug, Default)]
pub struct PeopleState {
    pub people: Option<People>,
    pub loading: bool,
    generation: u64,
}

impl PeopleState {
    /// Start a fetch; `None` (and the list cleared) when the gate is off,
    /// so nothing is requested.
    pub fn begin(&mut self, enabled: bool) -> Option<u64> {
        self.generation = self.generation.wrapping_add(1);
        if !enabled {
            self.people = None;
            self.loading = false;
            return None;
        }
        self.loading = true;
        Some(self.generation)
    }

    /// A fetch answered. Dropped unless it is the newest; cleared when the
    /// gate went off meanwhile.
    pub fn finish(&mut self, generation: u64, result: People, enabled: bool) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.people = enabled.then_some(result);
        true
    }

    /// Forget everything (the gate closed); replies in flight are stale.
    pub fn clear(&mut self) {
        let _ = self.begin(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_gate_needs_both_flags() {
        let mut config = Config::defaults_for(false);
        assert!(!enabled(&config), "both off by default");
        config.set_field("speaker_recognition", json!(true));
        assert!(!enabled(&config), "recognition alone is a 403");
        config.set_field("speaker_diarization", json!(true));
        assert!(enabled(&config));
        config.set_field("speaker_recognition", json!(false));
        assert!(!enabled(&config));
    }

    #[test]
    fn parse_keeps_only_name_date_and_samples() {
        let reply = json!({
            "embedder": "wespeaker-resnet34",
            "speakers": [
                {"id": "vp_1", "name": "  Priya   Raman ", "created": "2026-11-20T09:30:00Z",
                 "samples": 3, "model_id": "wespeaker-resnet34", "centroid": [0.1, 0.2]},
                {"id": "vp_2", "name": "Sam", "created": "yesterday", "samples": 1,
                 "model_id": "other-model"},
                {"id": "vp_3", "name": "", "created": "2026-11-20"},
                {"id": "vp_4"},
            ]
        });
        let people = parse(&reply);
        assert_eq!(
            people,
            vec![
                Person {
                    name: "Priya Raman".into(),
                    added: "2026-11-20".into(),
                    samples: 3
                },
                Person {
                    name: "Sam".into(),
                    added: String::new(),
                    samples: 1
                },
            ]
        );
        // Nothing but the allow-listed fields can reach the page.
        let shown = format!("{people:?}");
        for hidden in ["vp_1", "wespeaker", "other-model", "0.1", "centroid"] {
            assert!(!shown.contains(hidden), "{hidden} leaked: {shown}");
        }
        assert_eq!(detail(&people[0]), "Added 2026-11-20 · 3 samples");
        assert_eq!(detail(&people[1]), "1 sample");
        assert!(parse(&json!({"speakers": "nope"})).is_empty());
    }

    #[test]
    fn late_and_gated_replies_never_show() {
        let mut state = PeopleState::default();
        assert_eq!(state.begin(false), None, "gate off: no request");
        let first = state.begin(true).unwrap();
        let second = state.begin(true).unwrap();
        assert!(
            !state.finish(first, People::Off, true),
            "older reply dropped"
        );
        assert!(state.loading);
        assert!(state.finish(second, People::List(vec![]), true));
        assert_eq!(state.people, Some(People::List(vec![])));
        let third = state.begin(true).unwrap();
        assert!(state.finish(third, People::Off, false));
        assert_eq!(state.people, None, "gate closed while loading");
        let fourth = state.begin(true).unwrap();
        state.clear();
        assert!(!state.finish(fourth, People::List(vec![]), true));
        assert_eq!(state.people, None);
    }
}
