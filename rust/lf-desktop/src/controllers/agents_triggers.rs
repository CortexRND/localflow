//! Settings → Agents → Voice triggers: the model behind the hot-command
//! rules (`agents-api v1` `/hot-commands`, `/hot-commands/test`).
//!
//! GPUI-free. User-facing, a rule is a **trigger** (not a "command": that
//! is Command Mode); code keeps `hot_command` to match the API. Everything
//! the section decides is here so it is unit-tested: reading rules and test
//! replies through an allow-list, the client checks run on Save, the exact
//! request bodies (a `PUT` carries only the fields that changed), every
//! line of copy, and the warnings derived from the loaded providers and
//! workspaces. Providers are data: a rule's agent is a `(provider,
//! profile)` pair from `/providers`, and nothing here branches on an id.

use serde_json::{json, Map, Value};

use crate::controllers::agents::{self, AgentsError, Provider};
use crate::controllers::agents_launcher::provider_name;
use crate::controllers::agents_settings::{AgentSettings, Choices};

// ----------------------------------------------------------------- rules ---

/// The only fields a rule body may carry (the service refuses others).
pub const RULE_FIELDS: [&str; 7] = [
    "trigger",
    "provider",
    "profile",
    "repo",
    "prompt_template",
    "min_prompt_words",
    "enabled",
];

/// The service keeps at most this many rules.
pub const MAX_RULES: usize = 32;
pub const MIN_TRIGGER_WORDS: usize = 2;
pub const MAX_TRIGGER_WORDS: usize = 6;
pub const MAX_TRIGGER_CHARS: usize = 60;
pub const MAX_TEMPLATE_CHARS: usize = 20_000;
/// The placeholder a task template carries for what is said.
pub const PROMPT_PLACEHOLDER: &str = "{prompt}";
/// A new trigger's shortest task.
pub const DEFAULT_MIN_WORDS: u32 = 3;

/// One `HotCommandRule` as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// 8 lowercase hex characters.
    pub id: String,
    pub trigger: String,
    pub provider: String,
    pub profile: String,
    pub repo: String,
    pub prompt_template: String,
    pub min_prompt_words: u32,
    pub enabled: bool,
}

/// An id is safe to put in a path only when it is the service's 8 hex.
pub fn is_rule_id(id: &str) -> bool {
    id.len() == 8
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn field(value: Option<&Value>, max: usize) -> Option<String> {
    let s = value?.as_str()?;
    (s.chars().count() <= max).then(|| s.to_string())
}

/// One rule object, or `None` when a required field is missing or out of
/// bounds. Only the allow-listed fields are kept.
pub fn parse_rule(item: &Value) -> Option<Rule> {
    let id = field(item.get("id"), 8).filter(|id| is_rule_id(id))?;
    let trigger = field(item.get("trigger"), MAX_TRIGGER_CHARS)?;
    let provider = field(item.get("provider"), 40).filter(|p| !p.is_empty())?;
    Some(Rule {
        id,
        trigger,
        provider,
        profile: field(item.get("profile"), 40).unwrap_or_default(),
        repo: field(item.get("repo"), 40).unwrap_or_default(),
        prompt_template: field(item.get("prompt_template"), MAX_TEMPLATE_CHARS)
            .unwrap_or_else(|| PROMPT_PLACEHOLDER.into()),
        min_prompt_words: item
            .get("min_prompt_words")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_MIN_WORDS, |n| n.min(50) as u32),
        enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
    })
}

/// `GET /api/agents/hot-commands`.
pub fn parse_rules(reply: &Value) -> Vec<Rule> {
    reply
        .get("rules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_rule)
        .take(MAX_RULES)
        .collect()
}

// ----------------------------------------------------------------- draft ---

/// What the editor holds (no id: a new rule has none yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleDraft {
    pub trigger: String,
    pub provider: String,
    pub profile: String,
    pub repo: String,
    pub prompt_template: String,
    pub min_prompt_words: u32,
}

impl RuleDraft {
    /// A new trigger: the service's default provider (else the first one
    /// listed), the default workspace, `{prompt}`, 3 words.
    pub fn new(providers: &[Provider], settings: &AgentSettings) -> Self {
        let (provider, profile) = providers
            .iter()
            .find(|p| p.id == settings.default_provider && p.profile.is_empty())
            .or_else(|| providers.first())
            .map(|p| (p.id.clone(), p.profile.clone()))
            .unwrap_or_else(|| (settings.default_provider.clone(), String::new()));
        RuleDraft {
            trigger: String::new(),
            provider,
            profile,
            repo: String::new(),
            prompt_template: PROMPT_PLACEHOLDER.into(),
            min_prompt_words: DEFAULT_MIN_WORDS,
        }
    }

    pub fn from_rule(rule: &Rule) -> Self {
        RuleDraft {
            trigger: rule.trigger.clone(),
            provider: rule.provider.clone(),
            profile: rule.profile.clone(),
            repo: rule.repo.clone(),
            prompt_template: rule.prompt_template.clone(),
            min_prompt_words: rule.min_prompt_words,
        }
    }

    /// The task uses what is said (else the same task every time, and the
    /// shortest-task choice doesn't apply).
    pub fn uses_prompt(&self) -> bool {
        self.prompt_template.contains(PROMPT_PLACEHOLDER)
    }

    /// The shortest task actually sent: none for a fixed task.
    pub fn effective_min_words(&self) -> u32 {
        if self.uses_prompt() {
            self.min_prompt_words
        } else {
            0
        }
    }
}

/// The words of a trigger as the service counts them: runs of letters and
/// digits (`_` and punctuation separate), lower-cased.
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Client checks, on Save only (the service stays the authority).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftErrors {
    /// Under **Say**.
    pub trigger: Option<String>,
    /// Under **Task**.
    pub task: Option<String>,
}

impl DraftErrors {
    pub fn is_empty(&self) -> bool {
        self.trigger.is_none() && self.task.is_none()
    }
}

pub fn validate_draft(draft: &RuleDraft) -> DraftErrors {
    let trigger = draft.trigger.trim();
    let n = words(trigger).len();
    DraftErrors {
        trigger: (!(MIN_TRIGGER_WORDS..=MAX_TRIGGER_WORDS).contains(&n)
            || trigger.chars().count() > MAX_TRIGGER_CHARS)
            .then(|| BAD_TRIGGER.to_string()),
        task: draft
            .prompt_template
            .trim()
            .is_empty()
            .then(|| EMPTY_TASK.to_string()),
    }
}

/// `POST /api/agents/hot-commands`: every rule field, a new trigger on.
pub fn create_body(draft: &RuleDraft) -> Value {
    json!({
        "trigger": draft.trigger.trim(),
        "provider": draft.provider,
        "profile": draft.profile,
        "repo": draft.repo,
        "prompt_template": draft.prompt_template,
        "min_prompt_words": draft.effective_min_words(),
        "enabled": true,
    })
}

/// `PUT /api/agents/hot-commands/{id}`: only the fields that changed (an
/// empty object when nothing did).
pub fn rule_patch(old: &Rule, draft: &RuleDraft) -> Value {
    edit_patch(&RuleDraft::from_rule(old), draft)
}

/// The fields the user changed in the editor since it was opened
/// (`baseline`), and only those: a field edited elsewhere meanwhile (the
/// CLI) and left alone here is never sent back with its old value.
pub fn edit_patch(baseline: &RuleDraft, draft: &RuleDraft) -> Value {
    let old = baseline;
    let mut out = Map::new();
    let trigger = draft.trigger.trim();
    if trigger != old.trigger.trim() {
        out.insert("trigger".into(), json!(trigger));
    }
    if draft.provider != old.provider {
        out.insert("provider".into(), json!(draft.provider));
    }
    if draft.profile != old.profile {
        out.insert("profile".into(), json!(draft.profile));
    }
    if draft.repo != old.repo {
        out.insert("repo".into(), json!(draft.repo));
    }
    if draft.prompt_template != old.prompt_template {
        out.insert("prompt_template".into(), json!(draft.prompt_template));
    }
    // A fixed task (no `{prompt}`) has no minimum to change: the field is
    // left as stored (the service ignores it then), so an untouched Save
    // sends nothing (review M6).
    if draft.uses_prompt() && draft.min_prompt_words != old.min_prompt_words {
        out.insert("min_prompt_words".into(), json!(draft.min_prompt_words));
    }
    Value::Object(out)
}

/// `PUT /api/agents/hot-commands/{id}` from a rule's switch.
pub fn enabled_patch(on: bool) -> Value {
    json!({ "enabled": on })
}

/// `PUT /api/config` for the Voice triggers switch.
pub fn hot_commands_patch(on: bool) -> Value {
    json!({"agents": {"hot_commands": on}})
}

// ------------------------------------------------------------------ copy ---

pub const HEADING: &str = "Voice triggers";
pub const SUPPORT: &str = "Say a trigger at the start of dictation to start an agent instead of typing. Anything that doesn't match is typed as usual.";
pub const SWITCH_LABEL: &str = "Voice triggers";
pub const SWITCH_DESCRIPTION: &str = "Listen for your triggers while dictating.";
pub const AGENTS_OFF: &str = "Turn on Agents first.";
pub const ADD_TRIGGER: &str = "Add trigger";
pub const TOO_MANY: &str = "You can have up to 32 triggers.";
pub const EDIT: &str = "Edit";
pub const SAY_LABEL: &str = "Say";
pub const SAY_PLACEHOLDER: &str = "launch claude";
pub const SAY_HELP: &str = "2 to 6 words. It only counts at the very start of what you say.";
pub const AGENT_LABEL: &str = "Agent";
pub const WORKSPACE_LABEL: &str = "Workspace";
pub const DEFAULT_WORKSPACE: &str = "Default workspace";
pub const TASK_LABEL: &str = "Task";
pub const TASK_HELP: &str = "{prompt} is replaced by what you say after the trigger. Without it, the same task is sent every time.";
pub const MIN_LABEL: &str = "Shortest task";
pub const MIN_HELP: &str = "Shorter requests are typed as normal dictation.";
pub const NO_MINIMUM: &str = "No minimum";
pub const SAVE: &str = "Save";
pub const CANCEL: &str = "Cancel";
pub const DELETE: &str = "Delete trigger…";
pub const BAD_TRIGGER: &str = "Use 2 to 6 words, up to 60 characters.";
pub const EMPTY_TASK: &str = "Enter a task, or {prompt} to use what you say.";
pub const GONE: &str = "That trigger no longer exists.";
pub const LOCAL_ONLY: &str = "Triggers can only be changed on this computer.";
pub const SAVED: &str = "Trigger saved";
pub const DELETED: &str = "Trigger deleted";
pub const TRY_LABEL: &str = "Try a sentence";
pub const TRY_PLACEHOLDER: &str = "launch claude in localflow to fix the tests";
pub const TEST: &str = "Test";
pub const MATCHES: &str = "Matches";
pub const NOTHING_STARTED: &str = "Nothing was started.";
pub const UNSAVED_NOTE: &str = "Tests use your saved triggers.";

/// Shortest task: `(words, label)`; 0 is no minimum.
pub const MIN_WORD_CHOICES: [(u32, &str); 5] = [
    (0, NO_MINIMUM),
    (2, "2 words"),
    (3, "3 words"),
    (5, "5 words"),
    (8, "8 words"),
];

/// A rule row's title.
pub fn title(trigger: &str) -> String {
    format!("\u{201c}{trigger}\u{201d}")
}

/// `(title, body, verb)` of the Delete confirmation.
pub fn delete_confirm(trigger: &str) -> (String, String, String) {
    (
        "Delete this trigger?".into(),
        format!(
            "Saying \u{201c}{trigger}\u{201d} will be typed as normal dictation again. Sessions it already started are not affected."
        ),
        "Delete".into(),
    )
}

/// The help under Workspace (none without workspaces).
pub fn workspace_help(settings: &AgentSettings) -> Option<String> {
    settings.repos.first().map(|r| {
        format!(
            "You can also say \u{201c}in {}\u{201d} right after the trigger.",
            r.alias
        )
    })
}

/// A provider's name for `(id, profile)`: its listed label (with the
/// profile when another provider shares the label), else its id.
pub fn agent_name(providers: &[Provider], id: &str, profile: &str) -> String {
    providers
        .iter()
        .find(|p| p.id == id && p.profile == profile)
        .map(|p| provider_name(p, providers))
        .unwrap_or_else(|| id.to_string())
}

/// What a rule starts: `Starts {P} in {repo}`, `… in the default
/// workspace` (no repo, a default exists), else `Starts {P}`.
pub fn rule_summary(rule: &Rule, providers: &[Provider], settings: &AgentSettings) -> String {
    let name = agent_name(providers, &rule.provider, &rule.profile);
    if !rule.repo.is_empty() {
        format!("Starts {name} in {}", rule.repo)
    } else if !settings.default_repo.is_empty() {
        format!("Starts {name} in the default workspace")
    } else {
        format!("Starts {name}")
    }
}

/// Why a rule can't match, from what is loaded: its agent isn't listed, or
/// its workspace was removed.
pub fn rule_warning(
    rule: &Rule,
    providers: &[Provider],
    settings: &AgentSettings,
) -> Option<String> {
    if !providers
        .iter()
        .any(|p| p.id == rule.provider && p.profile == rule.profile)
    {
        return Some(format!(
            "{} isn't available, so this won't match.",
            rule.provider
        ));
    }
    if !rule.repo.is_empty() && !settings.repos.iter().any(|r| r.alias == rule.repo) {
        return Some(format!(
            "Workspace \u{201c}{}\u{201d} was removed, so this won't match.",
            rule.repo
        ));
    }
    None
}

// --------------------------------------------------------------- choices ---

/// The Agent select: every listed provider (profiles included), labelled
/// as in the launcher; values are `(provider, profile)`. A stored agent no
/// longer listed is kept as "Current: {id}".
pub fn agent_choices(
    providers: &[Provider],
    current: (&str, &str),
) -> (Vec<String>, Vec<(String, String)>, Option<usize>) {
    let mut labels: Vec<String> = providers
        .iter()
        .map(|p| provider_name(p, providers))
        .collect();
    let mut values: Vec<(String, String)> = providers
        .iter()
        .map(|p| (p.id.clone(), p.profile.clone()))
        .collect();
    let wanted = (current.0.to_string(), current.1.to_string());
    if !current.0.is_empty() && !values.contains(&wanted) {
        labels.push(format!("Current: {}", current.0));
        values.push(wanted.clone());
    }
    let selected = values.iter().position(|v| *v == wanted);
    (labels, values, selected)
}

/// The Workspace select: Default workspace (`""`), then each alias; a
/// removed one is kept as "Current: {repo}".
pub fn workspace_choices(settings: &AgentSettings, current: &str) -> Choices {
    let mut labels = vec![DEFAULT_WORKSPACE.to_string()];
    let mut values = vec![String::new()];
    for r in &settings.repos {
        labels.push(r.alias.clone());
        values.push(r.alias.clone());
    }
    if !current.is_empty() && !values.iter().any(|v| v == current) {
        labels.push(format!("Current: {current}"));
        values.push(current.to_string());
    }
    let selected = values.iter().position(|v| v == current);
    Choices {
        labels,
        values,
        selected,
    }
}

/// The Shortest task select; a stored count outside the list is kept as
/// "{n} words".
pub fn min_word_choices(current: u32) -> (Vec<String>, Vec<u32>, Option<usize>) {
    let mut labels: Vec<String> = MIN_WORD_CHOICES
        .iter()
        .map(|(_, l)| l.to_string())
        .collect();
    let mut values: Vec<u32> = MIN_WORD_CHOICES.iter().map(|(n, _)| *n).collect();
    if !values.contains(&current) {
        labels.push(if current == 1 {
            "1 word".into()
        } else {
            format!("{current} words")
        });
        values.push(current);
    }
    let selected = values.iter().position(|v| *v == current);
    (labels, values, selected)
}

// ---------------------------------------------------------------- errors ---

/// Where a refused rule write is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleFailure {
    /// `409 rule_conflict`: under **Say**, the service's sentence (it names
    /// the other trigger).
    Trigger(String),
    /// Under the button row (validation, local-only, anything else).
    Form(String),
    /// `404`: the trigger was removed elsewhere; close and reload.
    Gone,
}

pub fn rule_failure(error: &AgentsError) -> RuleFailure {
    match (error, error.code()) {
        (_, Some("rule_conflict")) => RuleFailure::Trigger(error.reason()),
        (AgentsError::Rejected(e), _) if e.status == 404 => RuleFailure::Gone,
        (_, Some("not_found")) => RuleFailure::Gone,
        (_, Some("local_only")) => RuleFailure::Form(LOCAL_ONLY.into()),
        _ => RuleFailure::Form(error.reason()),
    }
}

// ------------------------------------------------------------------ test ---

/// `POST /api/agents/hot-commands/test`, allow-listed. `prompt` is masked
/// for credential-like strings (the service redacts it too).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleTest {
    pub matched: bool,
    pub rule_id: String,
    pub provider: String,
    pub profile: String,
    pub repo: String,
    pub prompt: String,
    pub reason: String,
}

pub fn parse_rule_test(reply: &Value) -> Option<RuleTest> {
    let text = |name: &str, max: usize| field(reply.get(name), max).unwrap_or_default();
    Some(RuleTest {
        matched: reply.get("matched")?.as_bool()?,
        rule_id: text("rule_id", 8),
        provider: text("provider", 40),
        profile: text("profile", 40),
        repo: text("repo", 40),
        prompt: reply
            .get("prompt")
            .and_then(Value::as_str)
            .map(|p| {
                agents::clean(
                    &crate::controllers::assistant::mask_secrets(p),
                    MAX_TEMPLATE_CHARS,
                )
            })
            .unwrap_or_default(),
        reason: agents::clean(&text("reason", 40), 40),
    })
}

/// `{"text": …}`, exactly.
pub fn test_body(text: &str) -> Value {
    json!({ "text": text })
}

/// What Try it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestShown {
    /// A match: the summary line and the task that would be sent.
    Match { summary: String, prompt: String },
    /// No match, in words.
    Note(String),
    /// The request failed: the service's sentence.
    Error(String),
}

/// A reply in words (Fable §3 "Try it").
pub fn test_copy(reply: &RuleTest, providers: &[Provider]) -> TestShown {
    if reply.matched {
        let name = agent_name(providers, &reply.provider, &reply.profile);
        let summary = if reply.repo.is_empty() {
            format!("Starts {name}")
        } else {
            format!("Starts {name} in {}", reply.repo)
        };
        return TestShown::Match {
            summary,
            prompt: reply.prompt.clone(),
        };
    }
    TestShown::Note(
        match reply.reason.as_str() {
            "too_few_words" => {
                "A trigger matches, but the task after it is too short. It would be typed as usual."
            }
            "disabled" => "That trigger is turned off.",
            "unknown_repo" => "That trigger's workspace is no longer in your list.",
            "unknown_provider" => "That trigger's agent isn't available.",
            "feature_off" => "Voice triggers are off.",
            // `no_trigger`, and anything this window doesn't know yet.
            _ => "No trigger matches the start of this sentence. It would be typed as usual.",
        }
        .into(),
    )
}

/// A failed test request in words.
pub fn test_error(error: &AgentsError) -> TestShown {
    match error.code() {
        Some("agents_disabled") => TestShown::Note("Turn on Agents to test triggers.".into()),
        _ => TestShown::Error(error.reason()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controllers::agents::parse_providers;
    use crate::controllers::agents_settings::parse_settings;

    fn providers() -> Vec<Provider> {
        parse_providers(&json!({"providers": [
            {"id": "claude", "profile": "", "label": "Claude"},
            {"id": "devin", "profile": "", "label": "Devin"},
            {"id": "http", "profile": "0a1b2c3d", "label": "Agent"},
            {"id": "http", "profile": "4e5f6a7b", "label": "Agent"},
        ]}))
    }

    fn settings(default_repo: &str) -> AgentSettings {
        parse_settings(&json!({
            "api_version": 1, "enabled": true, "default_provider": "devin",
            "default_repo": default_repo,
            "repos": [{"alias": "localflow", "path": "/src/lf", "exists": true},
                      {"alias": "web", "path": "/src/web", "exists": true}],
        }))
    }

    fn rule(json: Value) -> Rule {
        let mut base = json!({"id": "1a2b3c4d", "trigger": "launch claude", "provider": "claude",
            "profile": "", "repo": "", "prompt_template": "{prompt}", "min_prompt_words": 3,
            "enabled": true, "created_at": "2026-11-20T12:00:00Z",
            "updated_at": "2026-11-20T12:00:00Z"});
        for (k, v) in json.as_object().unwrap() {
            base[k] = v.clone();
        }
        parse_rule(&base).unwrap()
    }

    #[test]
    fn rules_parse_allow_listed_with_bounded_ids() {
        let rules = parse_rules(&json!({"rules": [
            {"id": "1a2b3c4d", "trigger": "launch claude", "provider": "claude",
             "profile": "", "repo": "web", "prompt_template": "Fix: {prompt}",
             "min_prompt_words": 5, "enabled": false, "extra": "dropped"},
            {"id": "../etc", "trigger": "x y", "provider": "claude"},
            {"id": "ABCDEF12", "trigger": "x y", "provider": "claude"},
            {"id": "2b3c4d5e", "trigger": "x y"},
            {"id": "3c4d5e6f", "trigger": "ask devin", "provider": "devin"},
        ]}));
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules[0],
            Rule {
                id: "1a2b3c4d".into(),
                trigger: "launch claude".into(),
                provider: "claude".into(),
                profile: String::new(),
                repo: "web".into(),
                prompt_template: "Fix: {prompt}".into(),
                min_prompt_words: 5,
                enabled: false,
            }
        );
        assert_eq!(rules[1].prompt_template, "{prompt}", "defaults");
        assert_eq!(rules[1].min_prompt_words, 3);
        assert!(rules[1].enabled);
        assert!(is_rule_id("0a1b2c3d") && !is_rule_id("0a1b2c3") && !is_rule_id("0A1B2C3D"));
    }

    #[test]
    fn the_say_and_task_checks_have_exact_bounds_and_copy() {
        let ok = |trigger: &str| {
            let mut d = RuleDraft::new(&providers(), &settings(""));
            d.trigger = trigger.into();
            validate_draft(&d).trigger
        };
        assert_eq!(ok("launch"), Some(BAD_TRIGGER.into()), "1 word");
        assert_eq!(ok("launch claude"), None, "2 words");
        assert_eq!(ok("a b c d e f"), None, "6 words");
        assert_eq!(ok("a b c d e f g"), Some(BAD_TRIGGER.into()), "7 words");
        assert_eq!(ok("hey-there_you"), None, "punctuation separates words");
        let sixty = format!("{} {}", "a".repeat(29), "b".repeat(30));
        assert_eq!(sixty.chars().count(), 60);
        assert_eq!(ok(&sixty), None, "60 characters");
        assert_eq!(ok(&format!("{sixty}c")), Some(BAD_TRIGGER.into()), "61");
        assert_eq!(ok("  "), Some(BAD_TRIGGER.into()));
        assert_eq!(BAD_TRIGGER, "Use 2 to 6 words, up to 60 characters.");
        let mut d = RuleDraft::new(&providers(), &settings(""));
        d.trigger = "ask devin".into();
        d.prompt_template = " \n ".into();
        let errors = validate_draft(&d);
        assert_eq!(
            errors.task.as_deref(),
            Some("Enter a task, or {prompt} to use what you say.")
        );
        assert!(errors.trigger.is_none() && !errors.is_empty());
    }

    #[test]
    fn a_new_draft_uses_the_service_default_then_the_first_provider() {
        let d = RuleDraft::new(&providers(), &settings("web"));
        assert_eq!((d.provider.as_str(), d.profile.as_str()), ("devin", ""));
        assert_eq!(d.repo, "", "Default workspace");
        assert_eq!(d.prompt_template, "{prompt}");
        assert_eq!(d.min_prompt_words, 3);
        let mut s = settings("");
        s.default_provider = "gone".into();
        let d = RuleDraft::new(&providers(), &s);
        assert_eq!(d.provider, "claude");
    }

    #[test]
    fn create_sends_every_rule_field_and_a_fixed_task_has_no_minimum() {
        let mut d = RuleDraft::new(&providers(), &settings(""));
        d.trigger = "  ask the agent ".into();
        d.provider = "http".into();
        d.profile = "0a1b2c3d".into();
        d.repo = "web".into();
        let body = create_body(&d);
        assert_eq!(
            body,
            json!({"trigger": "ask the agent", "provider": "http", "profile": "0a1b2c3d",
                   "repo": "web", "prompt_template": "{prompt}", "min_prompt_words": 3,
                   "enabled": true})
        );
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(keys.iter().all(|k| RULE_FIELDS.contains(k)));
        assert_eq!(keys.len(), RULE_FIELDS.len());
        d.prompt_template = "Run the nightly checks".into();
        assert_eq!(create_body(&d)["min_prompt_words"], 0);
    }

    #[test]
    fn a_patch_carries_only_what_changed() {
        let old = rule(json!({"repo": "web", "min_prompt_words": 3}));
        let same = RuleDraft::from_rule(&old);
        assert_eq!(rule_patch(&old, &same), json!({}));
        let mut d = same.clone();
        d.trigger = " launch claude ".into();
        assert_eq!(rule_patch(&old, &d), json!({}), "trimmed is unchanged");
        d.trigger = "start claude".into();
        assert_eq!(rule_patch(&old, &d), json!({"trigger": "start claude"}));
        let mut d = same.clone();
        d.provider = "http".into();
        d.profile = "4e5f6a7b".into();
        d.repo = String::new();
        d.min_prompt_words = 8;
        assert_eq!(
            rule_patch(&old, &d),
            json!({"provider": "http", "profile": "4e5f6a7b", "repo": "", "min_prompt_words": 8})
        );
        let mut d = same;
        d.prompt_template = "Same every time".into();
        assert_eq!(
            rule_patch(&old, &d),
            json!({"prompt_template": "Same every time"}),
            "a fixed task leaves the minimum as stored"
        );
        // M6: a fixed-task rule opened and saved unchanged sends nothing,
        // even though its stored minimum isn't 0.
        let fixed = rule(json!({"prompt_template": "Run the nightly checks",
            "min_prompt_words": 3}));
        assert_eq!(rule_patch(&fixed, &RuleDraft::from_rule(&fixed)), json!({}));
        // Dirty fields only: opened, then edited elsewhere (the CLI changed
        // the task); the user changes only the agent and workspace here.
        let opened = RuleDraft::from_rule(&old);
        let mut mine = opened.clone();
        mine.provider = "devin".into();
        mine.repo = "localflow".into();
        let mut cli = old.clone();
        cli.prompt_template = "Changed elsewhere: {prompt}".into();
        assert_eq!(
            edit_patch(&opened, &mine),
            json!({"provider": "devin", "repo": "localflow"}),
            "the task edited elsewhere isn't sent back"
        );
        assert!(
            rule_patch(&cli, &mine).get("prompt_template").is_some(),
            "a diff against the reload would have overwritten it"
        );
        assert_eq!(edit_patch(&opened, &opened), json!({}), "no-op save");
        let mut touched = RuleDraft::from_rule(&fixed);
        touched.min_prompt_words = 8;
        assert_eq!(
            rule_patch(&fixed, &touched),
            json!({}),
            "the minimum can't change while the task is fixed"
        );
        assert_eq!(enabled_patch(false), json!({"enabled": false}));
        assert_eq!(
            hot_commands_patch(true),
            json!({"agents": {"hot_commands": true}})
        );
    }

    #[test]
    fn summaries_and_warnings_come_from_what_is_loaded() {
        let p = providers();
        let r = rule(json!({"repo": "web"}));
        assert_eq!(rule_summary(&r, &p, &settings("")), "Starts Claude in web");
        let r = rule(json!({}));
        assert_eq!(
            rule_summary(&r, &p, &settings("localflow")),
            "Starts Claude in the default workspace"
        );
        assert_eq!(rule_summary(&r, &p, &settings("")), "Starts Claude");
        let http = rule(json!({"provider": "http", "profile": "4e5f6a7b"}));
        assert_eq!(
            rule_summary(&http, &p, &settings("")),
            "Starts Agent \u{00b7} 4e5f6a7b"
        );
        assert_eq!(rule_warning(&r, &p, &settings("")), None);
        let gone = rule(json!({"provider": "codex"}));
        assert_eq!(
            rule_warning(&gone, &p, &settings("")).as_deref(),
            Some("codex isn't available, so this won't match.")
        );
        assert_eq!(rule_summary(&gone, &p, &settings("")), "Starts codex");
        let profile_gone = rule(json!({"provider": "http", "profile": "99999999"}));
        assert!(rule_warning(&profile_gone, &p, &settings("")).is_some());
        let removed = rule(json!({"repo": "docs"}));
        assert_eq!(
            rule_warning(&removed, &p, &settings("")).as_deref(),
            Some("Workspace \u{201c}docs\u{201d} was removed, so this won't match.")
        );
    }

    #[test]
    fn choices_carry_provider_and_profile_and_keep_unlisted_values() {
        let p = providers();
        let (labels, values, selected) = agent_choices(&p, ("http", "4e5f6a7b"));
        assert_eq!(
            labels,
            [
                "Claude",
                "Devin",
                "Agent \u{00b7} 0a1b2c3d",
                "Agent \u{00b7} 4e5f6a7b"
            ]
        );
        assert_eq!(values[3], ("http".into(), "4e5f6a7b".into()));
        assert_eq!(selected, Some(3));
        let (labels, _, selected) = agent_choices(&p, ("codex", ""));
        assert_eq!(labels.last().unwrap(), "Current: codex");
        assert_eq!(selected, Some(4));
        let w = workspace_choices(&settings(""), "");
        assert_eq!(w.labels, ["Default workspace", "localflow", "web"]);
        assert_eq!(w.values, ["", "localflow", "web"]);
        assert_eq!(w.selected, Some(0));
        let w = workspace_choices(&settings(""), "docs");
        assert_eq!(w.labels.last().unwrap(), "Current: docs");
        let (labels, values, selected) = min_word_choices(3);
        assert_eq!(
            labels,
            ["No minimum", "2 words", "3 words", "5 words", "8 words"]
        );
        assert_eq!(values, [0, 2, 3, 5, 8]);
        assert_eq!(selected, Some(2));
        let (labels, _, selected) = min_word_choices(4);
        assert_eq!(
            (labels.last().unwrap().as_str(), selected),
            ("4 words", Some(5))
        );
        assert_eq!(min_word_choices(1).0.last().unwrap(), "1 word");
    }

    #[test]
    fn refusals_land_beside_the_right_field() {
        let conflict = AgentsError::rejected(
            409,
            "rule_conflict",
            "trigger overlaps the hot command 'launch claude'",
        );
        assert_eq!(
            rule_failure(&conflict),
            RuleFailure::Trigger("Trigger overlaps the hot command 'launch claude'.".into())
        );
        let invalid = AgentsError::rejected(400, "validation", "unknown repo 'docs'");
        assert_eq!(
            rule_failure(&invalid),
            RuleFailure::Form("Unknown repo 'docs'.".into())
        );
        let gone = AgentsError::rejected(404, "not_found", "no hot command 1a2b3c4d");
        assert_eq!(rule_failure(&gone), RuleFailure::Gone);
        let local = AgentsError::rejected(403, "local_only", "This change is only allowed");
        assert_eq!(
            rule_failure(&local),
            RuleFailure::Form("Triggers can only be changed on this computer.".into())
        );
        assert_eq!(
            rule_failure(&AgentsError::Offline),
            RuleFailure::Form("LocalFlow's agent service isn't answering.".into())
        );
    }

    #[test]
    fn every_test_reply_has_exact_copy() {
        let p = providers();
        let reply = |v: Value| parse_rule_test(&v).unwrap();
        let matched = reply(
            json!({"matched": true, "rule_id": "1a2b3c4d", "provider": "claude",
            "profile": "", "repo": "localflow", "prompt": "fix   the\ntests", "reason": ""}),
        );
        assert_eq!(
            test_copy(&matched, &p),
            TestShown::Match {
                summary: "Starts Claude in localflow".into(),
                prompt: "fix the tests".into()
            }
        );
        let no_repo = reply(json!({"matched": true, "provider": "devin", "profile": "",
            "repo": "", "prompt": "x", "reason": ""}));
        assert!(matches!(test_copy(&no_repo, &p),
            TestShown::Match { summary, .. } if summary == "Starts Devin"));
        for (reason, copy) in [
            ("no_trigger", "No trigger matches the start of this sentence. It would be typed as usual."),
            ("too_few_words", "A trigger matches, but the task after it is too short. It would be typed as usual."),
            ("disabled", "That trigger is turned off."),
            ("unknown_repo", "That trigger's workspace is no longer in your list."),
            ("unknown_provider", "That trigger's agent isn't available."),
            ("feature_off", "Voice triggers are off."),
        ] {
            let r = reply(json!({"matched": false, "rule_id": "", "provider": "", "profile": "",
                "repo": "", "prompt": "", "reason": reason}));
            assert_eq!(test_copy(&r, &p), TestShown::Note(copy.into()), "{reason}");
        }
        assert_eq!(
            test_error(&AgentsError::rejected(
                409,
                "agents_disabled",
                "agents are off"
            )),
            TestShown::Note("Turn on Agents to test triggers.".into())
        );
        assert_eq!(
            test_error(&AgentsError::rejected(
                400,
                "validation",
                "body must be text"
            )),
            TestShown::Error("Body must be text.".into())
        );
        assert_eq!(test_body("launch claude"), json!({"text": "launch claude"}));
        let secret = reply(json!({"matched": true, "provider": "claude", "profile": "",
            "repo": "", "prompt": "use api_key=sk-abcdefghijklmnopqrstuvwxyz0123", "reason": ""}));
        assert!(
            !secret.prompt.contains("abcdefghijklmnop"),
            "the prompt is redacted"
        );
        assert!(parse_rule_test(&json!({"reason": "x"})).is_none());
    }

    #[test]
    fn section_copy_is_pinned() {
        assert_eq!(HEADING, "Voice triggers");
        assert_eq!(
            SUPPORT,
            "Say a trigger at the start of dictation to start an agent instead of typing. Anything that doesn't match is typed as usual."
        );
        assert_eq!(
            SWITCH_DESCRIPTION,
            "Listen for your triggers while dictating."
        );
        assert_eq!(AGENTS_OFF, "Turn on Agents first.");
        assert_eq!(TOO_MANY, "You can have up to 32 triggers.");
        assert_eq!(
            SAY_HELP,
            "2 to 6 words. It only counts at the very start of what you say."
        );
        assert_eq!(
            TASK_HELP,
            "{prompt} is replaced by what you say after the trigger. Without it, the same task is sent every time."
        );
        assert_eq!(MIN_HELP, "Shorter requests are typed as normal dictation.");
        assert_eq!(UNSAVED_NOTE, "Tests use your saved triggers.");
        assert_eq!(title("launch claude"), "\u{201c}launch claude\u{201d}");
        assert_eq!(
            delete_confirm("launch claude"),
            (
                "Delete this trigger?".into(),
                "Saying \u{201c}launch claude\u{201d} will be typed as normal dictation again. Sessions it already started are not affected.".into(),
                "Delete".into()
            )
        );
        assert_eq!(
            workspace_help(&settings("")).as_deref(),
            Some("You can also say \u{201c}in localflow\u{201d} right after the trigger.")
        );
        let mut none = settings("");
        none.repos.clear();
        assert_eq!(workspace_help(&none), None);
    }
}
