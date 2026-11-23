//! Ask page state: the conversation, the in-flight turn with its tool cards,
//! and the tools indicator. Plain data; the page applies [`AgentEvent`]s from
//! its worker here, guarded by a generation so a stopped run can't write.

use lf_ui::backends::ask::{Citation, Turn, ERROR_GENERIC};

use crate::controllers::ask_agent::{AgentEvent, Answer, Via};
use crate::controllers::mcp_client::McpStatus;
use crate::remote::Health;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardStatus {
    Running,
    Ok,
    Error,
}

impl CardStatus {
    /// Status word (never colour alone).
    pub fn word(self) -> &'static str {
        match self {
            CardStatus::Running => "Running",
            CardStatus::Ok => "Done",
            CardStatus::Error => "Error",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            CardStatus::Running => "loader-circle",
            CardStatus::Ok => "circle-check",
            CardStatus::Error => "circle-alert",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolCard {
    pub id: usize,
    pub name: String,
    pub args: String,
    pub status: CardStatus,
    /// Short result line ("3 results", the error).
    pub summary: String,
    /// Masked result text shown when expanded.
    pub preview: String,
    pub millis: Option<u64>,
    pub expanded: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentTurn {
    pub question: String,
    pub cards: Vec<ToolCard>,
    pub answer: String,
    pub citations: Vec<Citation>,
    pub error: String,
    pub note: Option<String>,
    pub via: Option<Via>,
    /// Three or more cards start collapsed under one "N tool calls" row.
    pub cards_open: bool,
}

/// Collapse cards into one summary row from this many.
pub const GROUP_CARDS_FROM: usize = 3;

impl AgentTurn {
    /// Recall matched sources but the model gave no answer (the same
    /// invariant as `Turn::sources_only`): show no answer prose.
    pub fn sources_only(&self) -> bool {
        self.answer.is_empty() && self.error.is_empty() && !self.citations.is_empty()
    }

    pub fn grouped(&self) -> bool {
        self.cards.len() >= GROUP_CARDS_FROM
    }

    pub fn cards_visible(&self) -> bool {
        !self.grouped() || self.cards_open
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Conversation {
    pub turns: Vec<AgentTurn>,
    /// The question being answered.
    pub pending: Option<AgentTurn>,
    pub generation: u64,
    pub status: McpStatus,
}

impl Default for Conversation {
    fn default() -> Self {
        Conversation {
            turns: Vec::new(),
            pending: None,
            generation: 0,
            status: McpStatus::Checking,
        }
    }
}

/// What the in-flight turn is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Thinking,
    /// A tool is running (its name).
    Tool(String),
}

impl Conversation {
    pub fn running(&self) -> bool {
        self.pending.is_some()
    }

    pub fn activity(&self) -> Activity {
        match &self.pending {
            None => Activity::Idle,
            Some(turn) => match turn
                .cards
                .iter()
                .rev()
                .find(|c| c.status == CardStatus::Running)
            {
                Some(card) => Activity::Tool(card.name.clone()),
                None => Activity::Thinking,
            },
        }
    }

    /// Earlier `(question, answer)` pairs for a follow-up.
    pub fn context(&self) -> Vec<(String, String)> {
        self.turns
            .iter()
            .filter(|t| t.error.is_empty())
            .map(|t| (t.question.clone(), t.answer.clone()))
            .collect()
    }

    /// Start a question; returns its generation.
    pub fn begin(&mut self, question: &str) -> u64 {
        self.generation += 1;
        self.pending = Some(AgentTurn {
            question: question.to_string(),
            ..AgentTurn::default()
        });
        self.generation
    }

    /// Apply a worker event; `false` (and no change) for a stale generation.
    pub fn apply(&mut self, generation: u64, event: AgentEvent) -> bool {
        if generation != self.generation {
            return false;
        }
        if let AgentEvent::Status(status) = event {
            self.status = status;
            return true;
        }
        let Some(turn) = self.pending.as_mut() else {
            return false;
        };
        match event {
            AgentEvent::Status(_) | AgentEvent::Thinking => {}
            AgentEvent::ToolStarted { id, name, args } => turn.cards.push(ToolCard {
                id,
                name,
                args,
                status: CardStatus::Running,
                summary: String::new(),
                preview: String::new(),
                millis: None,
                expanded: false,
            }),
            AgentEvent::ToolFinished {
                id,
                ok,
                summary,
                preview,
                millis,
            } => {
                if let Some(card) = turn.cards.iter_mut().find(|c| c.id == id) {
                    card.status = if ok {
                        CardStatus::Ok
                    } else {
                        CardStatus::Error
                    };
                    card.summary = summary;
                    card.preview = preview;
                    card.millis = Some(millis);
                }
            }
            AgentEvent::Finished(Answer {
                turn: done,
                note,
                via,
            }) => {
                let mut finished = self.pending.take().unwrap_or_default();
                // A card still running when the run ended did not finish.
                for card in &mut finished.cards {
                    if card.status == CardStatus::Running {
                        card.status = CardStatus::Error;
                        card.summary = "Didn't finish".into();
                    }
                }
                finished.answer = done.answer;
                finished.citations = done.citations;
                finished.error = done.error;
                finished.note = note;
                finished.via = Some(via);
                self.turns.push(finished);
            }
        }
        true
    }

    /// Stop the in-flight question (its result is discarded); returns the
    /// question so the composer can get it back.
    pub fn stop(&mut self) -> Option<String> {
        self.generation += 1;
        self.pending.take().map(|t| t.question)
    }

    pub fn clear(&mut self) {
        self.generation += 1;
        self.pending = None;
        self.turns.clear();
    }

    /// The worker's channel closed without a `Finished` for `generation`
    /// (it panicked or exited early): end the pending turn with an error so
    /// the page isn't stuck. `false` for a stopped or stale run.
    pub fn abandon(&mut self, generation: u64) -> bool {
        let Some(question) = self
            .pending
            .as_ref()
            .filter(|_| generation == self.generation)
            .map(|t| t.question.clone())
        else {
            return false;
        };
        let mut turn = Turn::new(question);
        turn.error = ERROR_GENERIC.into();
        self.apply(
            generation,
            AgentEvent::Finished(Answer {
                turn,
                note: None,
                via: Via::Tools,
            }),
        )
    }

    /// No run will report the tools' status, yet it still says "Checking"
    /// (a question started, then stopped or cleared before its status).
    pub fn needs_probe(&self) -> bool {
        !self.running() && self.status == McpStatus::Checking
    }
}

/// What the composer shows after Stop: the stopped question, unless the
/// user has already typed something new.
pub fn restored_text(current: &str, stopped: Option<String>) -> Option<String> {
    stopped.filter(|_| current.trim().is_empty())
}

impl Conversation {
    fn turn_mut(&mut self, turn: Option<usize>) -> Option<&mut AgentTurn> {
        match turn {
            Some(i) => self.turns.get_mut(i),
            None => self.pending.as_mut(),
        }
    }

    /// Expand or collapse one card (`turn` `None` = the in-flight turn).
    pub fn toggle_card(&mut self, turn: Option<usize>, card: usize) {
        if let Some(card) = self.turn_mut(turn).and_then(|t| t.cards.get_mut(card)) {
            card.expanded = !card.expanded;
        }
    }

    /// Show or hide a grouped turn's cards.
    pub fn toggle_group(&mut self, turn: Option<usize>) {
        if let Some(t) = self.turn_mut(turn) {
            t.cards_open = !t.cards_open;
        }
    }

    /// Indicator text and whether it is healthy.
    pub fn status_label(&self) -> (&'static str, StatusTone) {
        match &self.status {
            McpStatus::Checking => ("Checking tools…", StatusTone::Neutral),
            McpStatus::Connected { .. } => ("Tools connected", StatusTone::Good),
            McpStatus::Simulated => ("Simulated tools", StatusTone::Neutral),
            McpStatus::Missing(_) => ("Tools not installed · basic recall", StatusTone::Warn),
            McpStatus::Off => ("Tools off · basic recall", StatusTone::Neutral),
            McpStatus::Failed(_) => ("Tools unavailable · basic recall", StatusTone::Bad),
        }
    }

    /// Hover detail for the indicator.
    pub fn status_detail(&self) -> String {
        match &self.status {
            McpStatus::Checking => "Looking for LocalFlow's MCP server.".into(),
            McpStatus::Connected { tools } => {
                format!("LocalFlow MCP server: {tools} read-only tools. Click to check again.")
            }
            McpStatus::Simulated => {
                "Sandbox: in-process example tools, no server or network.".into()
            }
            McpStatus::Off => {
                "Turned off by LOCALFLOW_MCP_BIN=off. Questions use basic recall.".into()
            }
            McpStatus::Missing(why) | McpStatus::Failed(why) => {
                format!("{why}. Questions use basic recall. Click to check again.")
            }
        }
    }
}

/// The banner above the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    None,
    /// Sandbox: example services answer.
    Sandbox,
    /// Neither path can answer: the tools are missing, off or failed, and
    /// the service that basic recall needs reported Offline. The reason
    /// names what happened to the tools.
    Unreachable {
        tools: &'static str,
    },
}

impl Conversation {
    /// Warn only once both checks have settled on "nothing can answer". The
    /// tools (stdio, from this process) and the service (HTTP) are
    /// independent: connected tools answer with the service down, and the
    /// service answers by basic recall without tools. While either is still
    /// being checked nothing is shown, so the warning never flashes up
    /// before the tools check finishes.
    pub fn notice(&self, health: &Health) -> Notice {
        if *health == Health::Simulated {
            return Notice::Sandbox;
        }
        let tools = match &self.status {
            McpStatus::Checking | McpStatus::Connected { .. } | McpStatus::Simulated => {
                return Notice::None
            }
            McpStatus::Missing(_) => "aren't installed",
            McpStatus::Off => "are turned off",
            // Also reported when a running server stops responding, so
            // this doesn't say which.
            McpStatus::Failed(_) => "aren't available",
        };
        match health {
            Health::Offline => Notice::Unreachable { tools },
            Health::Unknown | Health::Online | Health::Simulated => Notice::None,
        }
    }
}

/// The `Notice::Unreachable` banner text. Only what was checked is named:
/// the model isn't checked until a question is sent.
pub fn unreachable_message(tools: &str) -> String {
    format!(
        "Ask can't reach your history right now. LocalFlow's tools {tools}, and the \
         LocalFlow service isn't running. Start LocalFlow, or run lf serve, then try again."
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusTone {
    Neutral,
    Good,
    Warn,
    Bad,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::ask_agent::tests::{ok, Harness};
    use crate::controllers::ask_agent::{answer, Question, LIMITS};
    use crate::controllers::mcp_client::McpError;
    use lf_ui::backends::ask::{Turn, ERROR_UNREACHABLE};
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    fn started(id: usize) -> AgentEvent {
        AgentEvent::ToolStarted {
            id,
            name: "search_dictations".into(),
            args: "query: \"x\"".into(),
        }
    }

    fn done(text: &str) -> AgentEvent {
        let mut turn = Turn::new("q");
        turn.answer = text.into();
        AgentEvent::Finished(Answer {
            turn,
            note: None,
            via: Via::Tools,
        })
    }

    #[test]
    fn events_build_cards_and_finish_the_turn() {
        let mut c = Conversation::default();
        assert_eq!(c.activity(), Activity::Idle);
        let g = c.begin("q");
        assert!(c.running());
        assert_eq!(c.activity(), Activity::Thinking);
        assert!(c.apply(g, AgentEvent::Status(McpStatus::Connected { tools: 4 })));
        assert_eq!(c.status_label().1, StatusTone::Good);
        c.apply(g, started(1));
        assert_eq!(c.activity(), Activity::Tool("search_dictations".into()));
        assert_eq!(
            c.pending.as_ref().unwrap().cards[0].status,
            CardStatus::Running
        );
        c.apply(
            g,
            AgentEvent::ToolFinished {
                id: 1,
                ok: true,
                summary: "2 results".into(),
                preview: "{…}".into(),
                millis: 12,
            },
        );
        assert_eq!(c.activity(), Activity::Thinking);
        c.toggle_card(None, 0);
        assert!(c.pending.as_ref().unwrap().cards[0].expanded);
        c.apply(g, started(2));
        c.apply(g, done("answer"));
        assert!(!c.running());
        let turn = &c.turns[0];
        assert_eq!(turn.answer, "answer");
        assert_eq!(turn.cards[0].status, CardStatus::Ok);
        assert_eq!(turn.cards[1].status, CardStatus::Error, "unfinished card");
        assert_eq!(c.context(), vec![("q".to_string(), "answer".to_string())]);
        c.toggle_card(Some(0), 1);
        assert!(c.turns[0].cards[1].expanded);
    }

    #[test]
    fn stale_generations_are_dropped_and_stop_restores_the_question() {
        let mut c = Conversation::default();
        let g = c.begin("first question");
        assert_eq!(c.stop().as_deref(), Some("first question"));
        assert!(!c.apply(g, started(1)));
        assert!(!c.apply(g, done("late")));
        assert!(!c.apply(g, AgentEvent::Status(McpStatus::Simulated)));
        assert!(c.turns.is_empty() && !c.running());
        assert_eq!(c.stop(), None);
        let g2 = c.begin("second");
        c.clear();
        assert!(!c.apply(g2, done("late")));
        assert!(c.turns.is_empty());
    }

    #[test]
    fn closed_channels_stopped_checks_and_typed_text_recover() {
        // Worker gone without `Finished`: the pending turn ends with an error.
        let mut c = Conversation::default();
        let g = c.begin("q");
        c.apply(g, started(1));
        assert!(c.abandon(g));
        assert!(!c.running());
        assert_eq!(c.turns[0].error, ERROR_GENERIC);
        assert_eq!(c.turns[0].cards[0].status, CardStatus::Error);
        assert!(!c.abandon(g), "already finished");
        // After Stop (or for a stale run) nothing changes.
        let g = c.begin("q2");
        c.stop();
        assert!(!c.abandon(g));
        assert_eq!(c.turns.len(), 1);
        // Stopped before the run reported its status: recheck.
        assert!(c.needs_probe());
        let g = c.begin("q3");
        assert!(!c.needs_probe(), "the run reports its own status");
        c.apply(g, AgentEvent::Status(McpStatus::Simulated));
        c.clear();
        assert!(!c.needs_probe());
        // Stop restores the question only into an empty composer.
        assert_eq!(restored_text("  ", Some("q".into())).as_deref(), Some("q"));
        assert_eq!(restored_text("new text", Some("q".into())), None);
        assert_eq!(restored_text("", None), None);
    }

    #[test]
    fn grouping_and_status_words() {
        let mut turn = AgentTurn::default();
        for id in 0..3 {
            turn.cards.push(ToolCard {
                id,
                name: "t".into(),
                args: String::new(),
                status: CardStatus::Ok,
                summary: String::new(),
                preview: String::new(),
                millis: None,
                expanded: false,
            });
        }
        assert!(turn.grouped() && !turn.cards_visible());
        let mut c = Conversation {
            turns: vec![turn],
            ..Conversation::default()
        };
        c.toggle_group(Some(0));
        assert!(c.turns[0].cards_visible());
        assert_eq!(CardStatus::Error.word(), "Error");
        for (status, tone) in [
            (McpStatus::Missing("x".into()), StatusTone::Warn),
            (McpStatus::Failed("x".into()), StatusTone::Bad),
            (McpStatus::Simulated, StatusTone::Neutral),
        ] {
            c.status = status;
            assert_eq!(c.status_label().1, tone);
        }
        assert_ne!(
            Conversation {
                status: McpStatus::Missing("a".into()),
                ..Conversation::default()
            }
            .status_label()
            .0,
            Conversation {
                status: McpStatus::Failed("a".into()),
                ..Conversation::default()
            }
            .status_label()
            .0,
        );
    }

    fn with_status(status: McpStatus) -> Conversation {
        Conversation {
            status,
            ..Conversation::default()
        }
    }

    /// Every tools status against every service health.
    #[test]
    fn notice_only_when_no_path_can_answer() {
        let unavailable = [
            (McpStatus::Missing("not found".into()), "aren't installed"),
            (McpStatus::Off, "are turned off"),
            // Failed covers both "didn't start" and "stopped responding".
            (McpStatus::Failed("no reply".into()), "aren't available"),
        ];
        for (status, tools) in unavailable {
            let c = with_status(status);
            assert_eq!(c.notice(&Health::Offline), Notice::Unreachable { tools });
            // Basic recall can still answer, or the service isn't checked yet.
            assert_eq!(c.notice(&Health::Online), Notice::None);
            assert_eq!(c.notice(&Health::Unknown), Notice::None);
            assert_eq!(c.notice(&Health::Simulated), Notice::Sandbox);
        }
        // Connected tools answer without the service.
        for health in [Health::Offline, Health::Unknown, Health::Online] {
            let c = with_status(McpStatus::Connected { tools: 4 });
            assert_eq!(c.notice(&health), Notice::None);
        }
        assert_eq!(
            with_status(McpStatus::Simulated).notice(&Health::Simulated),
            Notice::Sandbox
        );
    }

    #[test]
    fn notice_waits_for_both_checks() {
        // The page opens Checking with health Unknown; health usually
        // settles first (Offline), the tools check later.
        let mut c = Conversation::default();
        assert_eq!(c.status, McpStatus::Checking);
        for health in [Health::Unknown, Health::Offline, Health::Online] {
            assert_eq!(c.notice(&health), Notice::None, "{health:?}");
        }
        c.status = McpStatus::Missing("not found".into());
        assert!(matches!(
            c.notice(&Health::Offline),
            Notice::Unreachable { .. }
        ));
        // Try again puts the tools back to Checking: the warning goes until
        // the new result arrives.
        c.status = McpStatus::Checking;
        assert_eq!(c.notice(&Health::Offline), Notice::None);
        // A question reports its own status, which replaces the probe's.
        let g = c.begin("q");
        c.apply(g, AgentEvent::Status(McpStatus::Connected { tools: 4 }));
        assert_eq!(c.notice(&Health::Offline), Notice::None);
    }

    #[test]
    fn notice_copy_names_only_what_was_checked() {
        for tools in ["aren't installed", "are turned off", "aren't available"] {
            let text = unreachable_message(tools);
            assert!(text.contains(tools), "{text}");
            assert!(text.contains("Start LocalFlow") && text.contains("lf serve"));
            // Plain words: no address or code formatting in the banner.
            assert!(!text.contains("http") && !text.contains('`'), "{text}");
            // The model is never checked before a question.
            assert!(!text.to_lowercase().contains("model"), "{text}");
        }
    }

    /// With tools, a question never goes through the service (basic recall
    /// isn't called), so it answers with the service down and leaves no
    /// warning; a model failure is a turn error that points at the model,
    /// not the service.
    #[test]
    fn tools_answer_and_model_errors_without_the_service() {
        let question = Question {
            text: "q".into(),
            filter: "Any time".into(),
            context: Vec::new(),
        };
        let h = Harness::script(&["{\"answer\": \"Friday.\", \"sources\": []}"]);
        let mut c = Conversation::default();
        let g = c.begin("q");
        answer(&h, &question, &LIMITS, &AtomicBool::new(false), &mut |e| {
            c.apply(g, e);
        });
        assert!(h.recalls.lock().unwrap().is_empty());
        assert_eq!(c.turns[0].answer, "Friday.");
        assert_eq!(c.turns[0].via, Some(Via::Tools));
        assert_eq!(c.notice(&Health::Offline), Notice::None);

        let h = Harness::default();
        h.replies
            .lock()
            .unwrap()
            .push_back(Err("connection refused".into()));
        let g = c.begin("q");
        answer(&h, &question, &LIMITS, &AtomicBool::new(false), &mut |e| {
            c.apply(g, e);
        });
        assert!(h.recalls.lock().unwrap().is_empty());
        let error = &c.turns[1].error;
        assert_eq!(error, ERROR_UNREACHABLE);
        assert!(error.contains("model") && error.contains("Models page"));
        assert!(!error.contains("service"));
        assert_eq!(c.notice(&Health::Offline), Notice::None);
    }

    /// The visible mid-tool state, reproduced without sleeps: a worker
    /// event stream applied in order, observed before the tool returns.
    #[test]
    fn mid_tool_state_from_a_real_run() {
        let (release, gate) = std::sync::mpsc::channel();
        let h = Harness {
            gate: Some(std::sync::Arc::new(std::sync::Mutex::new(gate))),
            ..Harness::script(&[
                "{\"tool\": \"search_dictations\", \"arguments\": {\"query\": \"kubernetes\"}}",
                "{\"answer\": \"Friday.\", \"sources\": [12]}",
            ])
        };
        h.outputs.lock().unwrap().push_back(Ok(ok(json!({"count": 1, "results": [
            {"id": 12, "when": "2026-11-20T10:15:30", "app": "Slack", "snippet": "rollout Friday"}
        ]}))));
        let mut c = Conversation::default();
        let g = c.begin("When is the rollout?");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let question = Question {
                text: "When is the rollout?".into(),
                filter: "Any time".into(),
                context: Vec::new(),
            };
            answer(&h, &question, &LIMITS, &AtomicBool::new(false), &mut |e| {
                let _ = tx.send(e);
            });
        });
        loop {
            let event = rx.recv().unwrap();
            let is_start = matches!(event, AgentEvent::ToolStarted { .. });
            c.apply(g, event);
            if is_start {
                break;
            }
        }
        assert_eq!(c.activity(), Activity::Tool("search_dictations".into()));
        assert_eq!(
            c.pending.as_ref().unwrap().cards[0].args,
            "query: \"kubernetes\""
        );
        release.send(()).unwrap();
        for event in rx {
            c.apply(g, event);
        }
        worker.join().unwrap();
        assert_eq!(c.turns[0].answer, "Friday.");
        assert_eq!(c.turns[0].citations[0].reference, "12");
        assert_eq!(c.turns[0].cards[0].summary, "1 result");
    }

    #[test]
    fn recall_fallback_turn_keeps_note() {
        let h = Harness {
            connect_error: Some(McpError::Missing),
            ..Harness::default()
        };
        let mut c = Conversation::default();
        let g = c.begin("q");
        let question = Question {
            text: "q".into(),
            filter: "Any time".into(),
            context: Vec::new(),
        };
        answer(&h, &question, &LIMITS, &AtomicBool::new(false), &mut |e| {
            c.apply(g, e);
        });
        assert!(matches!(c.status, McpStatus::Missing(_)));
        assert_eq!(c.turns[0].via, Some(Via::Recall));
        assert!(c.turns[0].note.as_deref().unwrap().contains("basic recall"));
        assert_eq!(c.turns[0].citations.len(), 1);
        assert!(!c.turns[0].sources_only());
    }

    #[test]
    fn sources_only_recall_turn_has_no_prose_and_keeps_sources() {
        let mut sources = Turn::new("q");
        sources.citations = vec![Citation {
            n: 1,
            source: "dictation".into(),
            snippet: "Kubernetes rollout Friday".into(),
            reference: "12".into(),
            ..Citation::default()
        }];
        let h = Harness {
            connect_error: Some(McpError::Missing),
            recall_turn: std::sync::Mutex::new(Some(sources)),
            ..Harness::default()
        };
        let mut c = Conversation::default();
        let g = c.begin("q");
        let question = Question {
            text: "q".into(),
            filter: "Any time".into(),
            context: Vec::new(),
        };
        answer(&h, &question, &LIMITS, &AtomicBool::new(false), &mut |e| {
            c.apply(g, e);
        });
        let turn = &c.turns[0];
        assert!(turn.sources_only() && turn.answer.is_empty() && turn.error.is_empty());
        assert_eq!(turn.citations[0].snippet, "Kubernetes rollout Friday");
        let note = turn.note.as_deref().unwrap();
        assert!(!note.contains("Answered") && note.contains("couldn't answer"));
        // A follow-up never sends the empty answer as an earlier turn.
        let follow_up = Question {
            text: "and then?".into(),
            filter: "Any time".into(),
            context: c.context(),
        };
        let prompt = crate::controllers::ask_agent::user_prompt(
            &follow_up,
            &crate::controllers::ask_agent::Scope::default(),
            &[],
        );
        assert!(!prompt.contains("<earlier_turn>"));
    }
}
