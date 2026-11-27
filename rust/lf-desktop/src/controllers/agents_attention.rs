//! Agents: which sessions need the user's input, for the sidebar badge and
//! the one in-Hub toast (Slice C).
//!
//! GPUI-free. Ids only: the shell's watcher never keeps session bodies.
//! Primed from `GET /api/agents/sessions?state=waiting_input`, then moved
//! by `/changes`. Only `waiting_input` counts (running or queued never
//! badges). A session that *newly* enters it after priming is reported once
//! (the toast); priming, and re-priming after a `reset`, report nothing.

use std::collections::BTreeSet;

use crate::controllers::agents::{Changes, Provider, RuntimeState, Session, SessionPage};
use crate::controllers::agents_launcher::provider_name;

/// A session that just started waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewlyWaiting {
    pub id: String,
    pub provider: String,
    pub profile: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attention {
    ids: BTreeSet<String>,
    revision: u64,
    primed: bool,
}

fn waiting(session: &Session) -> bool {
    session.runtime == RuntimeState::WaitingInput
}

impl Attention {
    /// The waiting list as read now (no "new" sessions on a prime).
    pub fn prime(&mut self, page: &SessionPage) {
        self.ids = page
            .sessions
            .iter()
            .filter(|s| waiting(s))
            .map(|s| s.id.clone())
            .collect();
        self.revision = page.revision;
        self.primed = true;
    }

    /// Forget everything; the next read primes again.
    pub fn reset(&mut self) {
        *self = Attention::default();
    }

    pub fn primed(&self) -> bool {
        self.primed
    }

    /// The `/changes` cursor (`since`).
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Sessions needing input now.
    pub fn count(&self) -> usize {
        self.ids.len()
    }

    /// Apply one `/changes` answer. Returns the sessions that newly entered
    /// `waiting_input`. A `reset` clears and asks for a re-prime (nothing
    /// is reported); an answer older than what is held is ignored; nothing
    /// applies before the first prime.
    pub fn apply(&mut self, changes: &Changes) -> Vec<NewlyWaiting> {
        if !self.primed {
            return Vec::new();
        }
        if changes.reset {
            self.reset();
            return Vec::new();
        }
        if changes.revision < self.revision {
            return Vec::new();
        }
        let mut newly = Vec::new();
        for session in &changes.sessions {
            if waiting(session) {
                if self.ids.insert(session.id.clone()) {
                    newly.push(NewlyWaiting {
                        id: session.id.clone(),
                        provider: session.provider.clone(),
                        profile: session.profile.clone(),
                    });
                }
            } else {
                self.ids.remove(&session.id);
            }
        }
        for id in &changes.deleted {
            self.ids.remove(id);
        }
        self.revision = changes.revision;
        newly
    }
}

/// The sidebar row's accessible name.
pub fn nav_label(count: usize) -> String {
    if count == 0 {
        "Agents".into()
    } else {
        format!("Agents, {count} needs input")
    }
}

/// The toast for a session that newly needs input: the provider's name
/// only (never a title, task or output).
pub fn toast(session: &NewlyWaiting, providers: &[Provider]) -> String {
    let name = providers
        .iter()
        .find(|p| p.id == session.provider && p.profile == session.profile)
        .map(|p| provider_name(p, providers))
        .unwrap_or_else(|| session.provider.clone());
    format!("{name} needs your input")
}

/// The toast's action.
pub const OPEN: &str = "Open";

/// The waiting list's page size (everything the service retains).
pub const PRIME_LIMIT: u32 = 200;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::agents::{parse_changes, parse_providers, parse_session_page};
    use serde_json::{json, Value};

    fn s(id: &str, state: &str) -> Value {
        json!({"id": id, "provider": "devin", "profile": "", "state": state})
    }

    fn primed(ids: &[&str], revision: u64) -> Attention {
        let mut a = Attention::default();
        let sessions: Vec<Value> = ids.iter().map(|id| s(id, "waiting_input")).collect();
        a.prime(&parse_session_page(
            &json!({"sessions": sessions, "revision": revision, "counts": {}}),
        ));
        a
    }

    fn changes(revision: u64, sessions: Vec<Value>, deleted: &[&str], reset: bool) -> Changes {
        parse_changes(&json!({"revision": revision, "sessions": sessions,
            "deleted": deleted, "reset": reset, "counts": {}, "waited": false}))
    }

    #[test]
    fn a_prime_counts_waiting_only_and_reports_nothing() {
        let mut a = Attention::default();
        assert!(!a.primed());
        assert!(a
            .apply(&changes(
                5,
                vec![s("aaaaaaaaaaa1", "waiting_input")],
                &[],
                false
            ))
            .is_empty());
        assert_eq!(a.count(), 0, "nothing applies before a prime");
        a.prime(&parse_session_page(&json!({"sessions": [
            s("aaaaaaaaaaa1", "waiting_input"), s("aaaaaaaaaaa2", "running"),
            s("aaaaaaaaaaa3", "queued"),
        ], "revision": 7, "counts": {}})));
        assert!(a.primed());
        assert_eq!((a.count(), a.revision()), (1, 7));
    }

    #[test]
    fn changes_move_sessions_in_and_out_and_report_new_ones_once() {
        let mut a = primed(&["aaaaaaaaaaa1"], 3);
        let newly = a.apply(&changes(
            4,
            vec![
                s("aaaaaaaaaaa1", "waiting_input"),
                s("aaaaaaaaaaa2", "waiting_input"),
                s("aaaaaaaaaaa3", "running"),
            ],
            &[],
            false,
        ));
        assert_eq!(
            newly,
            [NewlyWaiting {
                id: "aaaaaaaaaaa2".into(),
                provider: "devin".into(),
                profile: String::new()
            }],
            "the one already waiting isn't new"
        );
        assert_eq!((a.count(), a.revision()), (2, 4));
        assert!(a
            .apply(&changes(
                5,
                vec![s("aaaaaaaaaaa2", "waiting_input")],
                &[],
                false
            ))
            .is_empty());
        a.apply(&changes(6, vec![s("aaaaaaaaaaa1", "running")], &[], false));
        assert_eq!(a.count(), 1, "answered: out");
        a.apply(&changes(7, vec![], &["aaaaaaaaaaa2"], false));
        assert_eq!(a.count(), 0, "deleted: out");
        // Back to waiting later: new again.
        assert_eq!(
            a.apply(&changes(
                8,
                vec![s("aaaaaaaaaaa1", "waiting_input")],
                &[],
                false
            ))
            .len(),
            1
        );
    }

    #[test]
    fn an_older_answer_never_goes_back() {
        let mut a = primed(&["aaaaaaaaaaa1"], 10);
        assert!(a
            .apply(&changes(9, vec![s("aaaaaaaaaaa1", "running")], &[], false))
            .is_empty());
        assert_eq!((a.count(), a.revision()), (1, 10));
    }

    #[test]
    fn a_reset_clears_and_asks_for_a_quiet_re_prime() {
        let mut a = primed(&["aaaaaaaaaaa1"], 10);
        assert!(a
            .apply(&changes(
                12,
                vec![s("aaaaaaaaaaa2", "waiting_input")],
                &[],
                true
            ))
            .is_empty());
        assert!(!a.primed());
        assert_eq!((a.count(), a.revision()), (0, 0));
        let mut again = a.clone();
        again.prime(&parse_session_page(&json!({"sessions": [
            s("aaaaaaaaaaa1", "waiting_input"), s("aaaaaaaaaaa2", "waiting_input"),
        ], "revision": 12, "counts": {}})));
        assert_eq!(again.count(), 2, "re-primed, nothing reported");
    }

    #[test]
    fn labels_and_toasts_name_the_provider_only() {
        assert_eq!(nav_label(0), "Agents");
        assert_eq!(nav_label(2), "Agents, 2 needs input");
        let providers = parse_providers(&json!({"providers": [
            {"id": "devin", "profile": "", "label": "Devin"},
        ]}));
        let devin = NewlyWaiting {
            id: "aaaaaaaaaaa1".into(),
            provider: "devin".into(),
            profile: String::new(),
        };
        assert_eq!(toast(&devin, &providers), "Devin needs your input");
        let gone = NewlyWaiting {
            provider: "other".into(),
            ..devin
        };
        assert_eq!(toast(&gone, &providers), "other needs your input");
    }
}
