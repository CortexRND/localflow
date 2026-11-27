//! Settings → Agents → Voice triggers against the in-process fake (Slice
//! B): rows, the editor (create, edit, delete, toggle), client checks,
//! the service's refusals, Try it, the switch while Agents is off, the
//! limit, the fixture scene and layout. A child of the Slice A tests, so it
//! shares their helpers.

use gpui::{px, size};

use super::*;
use crate::controllers::agents::AgentsBackend;
use crate::controllers::agents_triggers::{self as triggers, TestShown};
use crate::sandbox::agents::{RULE_ASK, RULE_LAUNCH};

fn open_rule(agents: &Entity<AgentsSettings>, id: Option<&str>, cx: &mut VisualTestContext) {
    let id = id.map(str::to_owned);
    cx.update(|window, cx| agents.update(cx, |a, cx| a.open_rule(id, window, cx)));
    cx.run_until_parked();
}

fn editor_id(
    agents: &Entity<AgentsSettings>,
    cx: &mut VisualTestContext,
) -> Option<Option<String>> {
    agents.read_with(cx, |a, _| a.rule_editor().map(|e| e.id.clone()))
}

fn errors(
    agents: &Entity<AgentsSettings>,
    cx: &mut VisualTestContext,
) -> (Option<String>, Option<String>, Option<String>) {
    agents.read_with(cx, |a, _| {
        let (e, form) = a.rule_errors().expect("the editor is open");
        (e.trigger, e.task, form)
    })
}

fn result(agents: &Entity<AgentsSettings>, cx: &mut VisualTestContext) -> Option<TestShown> {
    agents.read_with(cx, |a, _| a.test_result().cloned())
}

fn try_sentence(agents: &Entity<AgentsSettings>, text: &str, cx: &mut VisualTestContext) {
    agents.update(cx, |a, cx| {
        a.type_test(text, cx);
        a.run_test(cx);
    });
    cx.run_until_parked();
}

/// The fixture's two triggers: titles, summaries, the switch on.
#[gpui::test]
fn the_section_lists_triggers_with_what_they_start(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    assert!(fake.calls().rules >= 1, "read with the tab");
    agents.read_with(cx, |a, _| {
        let s = a.settings().unwrap();
        assert!(s.hot_commands);
        let shown: Vec<(String, String, bool)> = a
            .rules_for_test()
            .iter()
            .map(|r| {
                (
                    triggers::title(&r.trigger),
                    triggers::rule_summary(r, a.providers(), s),
                    r.enabled,
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "\u{201c}launch claude\u{201d}".into(),
                    "Starts Claude in localflow".into(),
                    true
                ),
                (
                    "\u{201c}ask devin\u{201d}".into(),
                    "Starts Devin in the default workspace".into(),
                    false
                ),
            ]
        );
    });
}

/// Add trigger: client checks first (nothing sent), then exactly the seven
/// rule fields; the editor closes, the list reloads, "Trigger saved".
#[gpui::test]
fn a_new_trigger_is_checked_then_created(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    open_rule(&agents, None, cx);
    assert_eq!(editor_id(&agents, cx), Some(None));
    agents.update(cx, |a, cx| {
        a.type_rule("launch", "  ", cx);
        a.save_rule(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        errors(&agents, cx),
        (
            Some("Use 2 to 6 words, up to 60 characters.".into()),
            Some("Enter a task, or {prompt} to use what you say.".into()),
            None
        )
    );
    assert_eq!(fake.calls().create_rule, 0, "checked on Save, nothing sent");

    agents.update(cx, |a, cx| {
        a.type_rule("start codex", "{prompt}", cx);
        a.choose_rule_agent("codex", "", cx);
        a.choose_rule_repo("website", cx);
        a.save_rule(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        fake.rule_bodies(),
        [
            json!({"trigger": "start codex", "provider": "codex", "profile": "",
                "repo": "website", "prompt_template": "{prompt}", "min_prompt_words": 3,
                "enabled": true})
        ]
    );
    assert_eq!(editor_id(&agents, cx), None);
    assert_eq!(toast(&shell, cx).as_deref(), Some("Trigger saved"));
    assert_eq!(agents.read_with(cx, |a, _| a.rules_for_test().len()), 3);
}

/// 409 `rule_conflict` lands under Say with the service's sentence; the
/// draft stays as typed.
#[gpui::test]
fn a_conflict_shows_under_say_and_keeps_the_draft(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    open_rule(&agents, None, cx);
    agents.update(cx, |a, cx| {
        a.type_rule("launch claude now", "Fix: {prompt}", cx);
        a.save_rule(cx);
    });
    cx.run_until_parked();
    assert_eq!(fake.calls().create_rule, 1);
    assert_eq!(
        errors(&agents, cx).0.as_deref(),
        Some("Trigger overlaps the hot command 'launch claude'.")
    );
    assert_eq!(editor_id(&agents, cx), Some(None), "still open");
    let draft_kept = agents.read_with(cx, |a, cx| {
        a.unsaved_rule(cx) && a.rules_for_test().len() == 2
    });
    assert!(draft_kept);
}

/// Edit sends only what changed; an unchanged Save sends nothing.
#[gpui::test]
fn an_edit_sends_only_the_changed_fields(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    open_rule(&agents, Some(RULE_LAUNCH), cx);
    agents.update(cx, |a, cx| a.save_rule(cx));
    cx.run_until_parked();
    assert_eq!(fake.calls().update_rule, 0, "nothing changed, nothing sent");
    assert_eq!(editor_id(&agents, cx), None);

    open_rule(&agents, Some(RULE_LAUNCH), cx);
    agents.update(cx, |a, cx| {
        a.choose_rule_repo("", cx);
        a.type_rule("launch claude", "Please {prompt}", cx);
        a.save_rule(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        fake.rule_bodies(),
        [json!({"repo": "", "prompt_template": "Please {prompt}"})]
    );
    assert_eq!(toast(&shell, cx).as_deref(), Some("Trigger saved"));
    let rule = agents.read_with(cx, |a, _| a.rules_for_test()[0].clone());
    assert_eq!(
        (rule.repo.as_str(), rule.prompt_template.as_str()),
        ("", "Please {prompt}")
    );
}

/// A trigger removed elsewhere: the editor closes, the list reloads, and
/// the toast says so.
#[gpui::test]
fn an_edit_of_a_removed_trigger_closes_and_reloads(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    open_rule(&agents, Some(RULE_ASK), cx);
    fake.delete_rule(RULE_ASK).unwrap();
    let reads = fake.calls().rules;
    agents.update(cx, |a, cx| {
        a.type_rule("ask devin please", "{prompt}", cx);
        a.save_rule(cx);
    });
    cx.run_until_parked();
    assert_eq!(fake.calls().update_rule, 1);
    assert_eq!(editor_id(&agents, cx), None);
    assert_eq!(
        toast(&shell, cx).as_deref(),
        Some("That trigger no longer exists.")
    );
    assert_eq!(fake.calls().rules, reads + 1, "reloaded");
    assert_eq!(agents.read_with(cx, |a, _| a.rules_for_test().len()), 1);
}

/// 403 `local_only` and 400 `validation` land under the buttons; the
/// draft stays.
#[gpui::test]
fn refusals_land_under_the_buttons(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    fake.rules_local_only(true);
    open_rule(&agents, Some(RULE_LAUNCH), cx);
    agents.update(cx, |a, cx| {
        a.type_rule("start claude", "{prompt}", cx);
        a.save_rule(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        errors(&agents, cx).2.as_deref(),
        Some("Triggers can only be changed on this computer.")
    );
    fake.rules_local_only(false);
    // The workspace was removed at the service meanwhile.
    fake.set_config(&json!({"agents": {"repos": ["localflow=/home/you/src/localflow"]}}))
        .unwrap();
    agents.update(cx, |a, cx| {
        a.choose_rule_repo("website", cx);
        a.save_rule(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        errors(&agents, cx),
        (None, None, Some("Unknown repo 'website'.".into())),
        "400 validation: the service's sentence under the buttons"
    );
    assert_eq!(editor_id(&agents, cx), Some(Some(RULE_LAUNCH.into())));
    assert!(
        agents.read_with(cx, |a, cx| a.unsaved_rule(cx)),
        "draft kept"
    );
    assert_eq!(
        fake.rule_bodies(),
        [
            json!({"trigger": "start claude"}),
            json!({"trigger": "start claude", "repo": "website"}),
        ]
    );
}

/// A trigger's switch sends `{enabled}` alone, with no confirmation.
#[gpui::test]
fn a_trigger_switch_sends_only_enabled(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    agents.update(cx, |a, cx| a.toggle_rule(RULE_ASK.into(), true, cx));
    cx.run_until_parked();
    assert!(
        shell.read_with(cx, |s, _| s.confirm.is_none()),
        "no confirmation"
    );
    assert_eq!(fake.rule_bodies(), [json!({"enabled": true})]);
    assert!(agents.read_with(cx, |a, _| a.rules_for_test()[1].enabled));
    // Removed meanwhile: the toast, and the list reloads.
    fake.delete_rule(RULE_ASK).unwrap();
    agents.update(cx, |a, cx| a.toggle_rule(RULE_ASK.into(), false, cx));
    cx.run_until_parked();
    assert_eq!(
        toast(&shell, cx).as_deref(),
        Some("That trigger no longer exists.")
    );
    assert_eq!(agents.read_with(cx, |a, _| a.rules_for_test().len()), 1);
    // Local-only: the row says so.
    fake.rules_local_only(true);
    agents.update(cx, |a, cx| a.toggle_rule(RULE_LAUNCH.into(), false, cx));
    cx.run_until_parked();
    assert_eq!(
        agents.read_with(cx, |a, _| a
            .error(&Row::Rule(RULE_LAUNCH.into()))
            .map(str::to_owned)),
        Some("Triggers can only be changed on this computer.".into())
    );
}

/// Delete trigger… asks first (destructive), then deletes.
#[gpui::test]
fn deleting_a_trigger_needs_the_confirmation(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    open_rule(&agents, Some(RULE_LAUNCH), cx);
    agents.update(cx, |a, cx| a.confirm_delete_rule(cx));
    cx.run_until_parked();
    assert_eq!(
        fake.calls().delete_rule,
        0,
        "nothing before the confirmation"
    );
    let (title, body, verb, destructive) = confirm(&shell, cx);
    assert_eq!(title, "Delete this trigger?");
    assert_eq!(
        body,
        "Saying \u{201c}launch claude\u{201d} will be typed as normal dictation again. Sessions it already started are not affected."
    );
    assert_eq!((verb.as_str(), destructive), ("Delete", true));
    assert_eq!(fake.calls().delete_rule, 1);
    assert_eq!(toast(&shell, cx).as_deref(), Some("Trigger deleted"));
    assert_eq!(editor_id(&agents, cx), None);
    let left: Vec<String> = agents.read_with(cx, |a, _| {
        a.rules_for_test().iter().map(|r| r.id.clone()).collect()
    });
    assert_eq!(left, [RULE_ASK]);
}

/// Shortest task is disabled, showing No minimum, while the task has no
/// `{prompt}`; the body then carries 0.
#[gpui::test]
fn a_fixed_task_has_no_minimum(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    open_rule(&agents, None, cx);
    assert_eq!(
        agents.read_with(cx, |a, cx| a.min_select_disabled(cx)),
        (false, Some(2)),
        "3 words"
    );
    agents.update(cx, |a, cx| {
        a.type_rule("nightly checks", "Run the nightly checks", cx)
    });
    assert_eq!(
        agents.read_with(cx, |a, cx| a.min_select_disabled(cx)),
        (true, Some(0)),
        "No minimum"
    );
    agents.update(cx, |a, cx| a.save_rule(cx));
    cx.run_until_parked();
    assert_eq!(fake.rule_bodies()[0]["min_prompt_words"], 0);
}

/// Escape closes the trigger editor first. Below the limit, Add trigger is
/// drawn.
#[gpui::test]
fn escape_closes_the_trigger_editor(cx: &mut TestAppContext) {
    let (shell, cx, _dir, _fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("agents-rule-add-row").is_some());
    open_rule(&agents, Some(RULE_LAUNCH), cx);
    cx.dispatch_action(Escape);
    assert_eq!(editor_id(&agents, cx), None);
    assert_eq!(page(&shell, cx), Page::Settings);
}

/// Try it: explicit only, the exact copy for a match and each reason,
/// cleared when the sentence changes, and the unsaved-draft note.
#[gpui::test]
fn try_it_explains_each_outcome_and_starts_nothing(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    for text in ["l", "la", "launch claude"] {
        agents.update(cx, |a, cx| a.type_test(text, cx));
    }
    cx.run_until_parked();
    assert_eq!(fake.calls().test_rule, 0, "no call per keystroke");

    try_sentence(&agents, "launch claude in localflow to fix the tests", cx);
    assert_eq!(
        result(&agents, cx),
        Some(TestShown::Match {
            summary: "Starts Claude in localflow".into(),
            prompt: "fix the tests".into()
        })
    );
    assert_eq!(
        fake.rule_bodies(),
        [json!({"text": "launch claude in localflow to fix the tests"})]
    );
    agents.update(cx, |a, cx| a.type_test("launch claude in", cx));
    assert_eq!(result(&agents, cx), None, "cleared when the text changes");

    for (text, copy) in [
        (
            "hello there",
            "No trigger matches the start of this sentence. It would be typed as usual.",
        ),
        (
            "launch claude fix",
            "A trigger matches, but the task after it is too short. It would be typed as usual.",
        ),
        (
            "ask devin about the release plan",
            "That trigger is turned off.",
        ),
    ] {
        try_sentence(&agents, text, cx);
        assert_eq!(
            result(&agents, cx),
            Some(TestShown::Note(copy.into())),
            "{text}"
        );
    }
    // A saved change clears the result.
    agents.update(cx, |a, cx| a.toggle_rule(RULE_ASK.into(), true, cx));
    cx.run_until_parked();
    assert_eq!(result(&agents, cx), None, "cleared when a trigger is saved");

    // Unsaved editor changes: the note says tests use saved triggers.
    open_rule(&agents, Some(RULE_LAUNCH), cx);
    assert!(!agents.read_with(cx, |a, cx| a.unsaved_rule(cx)));
    agents.update(cx, |a, cx| {
        a.type_rule("launch claude code", "{prompt}", cx)
    });
    assert!(agents.read_with(cx, |a, cx| a.unsaved_rule(cx)));
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("agents-rule-try").is_some());

    fake.set_config(&json!({"agents": {"hot_commands": false}}))
        .unwrap();
    try_sentence(&agents, "launch claude to do the thing now", cx);
    assert_eq!(
        result(&agents, cx),
        Some(TestShown::Note("Voice triggers are off.".into()))
    );
    fake.set_config(&json!({"agents": {"enabled": false}}))
        .unwrap();
    try_sentence(&agents, "launch claude to do the thing", cx);
    assert_eq!(
        result(&agents, cx),
        Some(TestShown::Note("Turn on Agents to test triggers.".into()))
    );
    assert_eq!(fake.calls().launch, 0, "Try it never launches");
    assert!(fake
        .rule_bodies()
        .iter()
        .filter(|b| b.get("text").is_some())
        .all(|b| b.as_object().unwrap().len() == 1));
}

/// With Agents off the Voice triggers switch is disabled with its note;
/// triggers stay editable. With Agents on it writes `agents.hot_commands`.
#[gpui::test]
fn the_switch_needs_agents_on_and_writes_hot_commands(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, |_| {});
    assert!(!agents.read_with(cx, |a, _| a.settings().unwrap().enabled));
    agents.update(cx, |a, cx| a.set_hot_commands(true, cx));
    cx.run_until_parked();
    assert!(
        fake.config_bodies().is_empty(),
        "disabled while Agents is off"
    );
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("agents-triggers").is_some());
    // Rules stay editable while off.
    agents.update(cx, |a, cx| a.toggle_rule(RULE_LAUNCH.into(), false, cx));
    cx.run_until_parked();
    assert_eq!(fake.rule_bodies(), [json!({"enabled": false})]);

    agents.update(cx, |a, cx| a.set_enabled(true, cx));
    cx.run_until_parked();
    agents.update(cx, |a, cx| a.set_hot_commands(true, cx));
    cx.run_until_parked();
    assert_eq!(
        fake.config_bodies().last(),
        Some(&json!({"agents": {"hot_commands": true}}))
    );
    assert!(agents.read_with(cx, |a, _| a.settings().unwrap().hot_commands));
}

/// At 32 triggers Add trigger is replaced by the limit line; a workspace
/// removed at the service warns on the trigger that used it.
#[gpui::test]
fn the_limit_and_a_removed_workspace_are_shown(cx: &mut TestAppContext) {
    // GPUI 0.2 keeps `debug_bounds` across frames, so absence is checked in
    // a window whose first frame already has 32 triggers.
    let (_shell, cx, _dir, _fake, agents) = open_tab(cx, |f| {
        f.use_settings_fixture();
        f.set_config(
            &json!({"agents": {"repos": ["website=/home/you/src/missing/website"],
            "default_repo": ""}}),
        )
        .unwrap();
        f.fill_rules(32);
    });
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(agents.read_with(cx, |a, _| a.rules_for_test().len()), 32);
    assert!(
        cx.debug_bounds("agents-rule:1a2b3c4d").is_some(),
        "rows drawn"
    );
    assert!(
        cx.debug_bounds("agents-rule-add-row").is_none(),
        "hidden at 32"
    );
    agents.read_with(cx, |a, _| {
        let launch = a
            .rules_for_test()
            .iter()
            .find(|r| r.id == RULE_LAUNCH)
            .unwrap();
        assert_eq!(
            triggers::rule_warning(launch, a.providers(), a.settings().unwrap()).as_deref(),
            Some("Workspace \u{201c}localflow\u{201d} was removed, so this won't match.")
        );
        assert_eq!(a.rules_for_test().len(), 32);
    });
}

/// `--sandbox-fixture agents-settings-trigger` reaches its end state; the
/// editor and Try it lay out inside the tab at both sizes, both palettes.
#[gpui::test]
fn the_trigger_scene_and_its_layout(cx: &mut TestAppContext) {
    use crate::sandbox::Fixture;
    let fixture = Fixture::AgentsSettingsTrigger;
    assert!(fixture.is_agents() && fixture.is_agents_settings());
    assert_eq!(fixture.page(), Page::Settings);
    let (shell, cx, _dir) = open_with(
        Page::Home,
        |root| crate::sandbox::add_agents_fixture(root).unwrap(),
        cx,
    );
    fake(&shell, cx).use_settings_fixture();
    shell.update(cx, |s, cx| s.refresh_views(cx));
    shell.update(cx, |s, cx| s.run_fixture(fixture, cx));
    cx.run_until_parked();
    let agents = agents_tab(&shell, cx);
    assert_eq!(editor_id(&agents, cx), Some(Some(RULE_LAUNCH.into())));
    assert!(matches!(result(&agents, cx), Some(TestShown::Match { .. })));
    // The scene brings Voice triggers to the top of the Settings column
    // (the test window runs no next-frame callbacks: draw frames by hand).
    cx.simulate_resize(size(px(1280.), px(850.)));
    for _ in 0..8 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
    assert!(agents.read_with(cx, |a, _| a.reveal_done()));
    let scroll = settings_page(&shell, cx).read_with(cx, |p, _| p.page_scroll());
    assert!(scroll.offset().y < px(0.), "the column scrolled");
    let viewport = scroll.bounds();
    let section = cx.debug_bounds("agents-triggers").expect("section");
    assert!(
        section.top() >= viewport.top() && section.top() <= viewport.top() + px(40.),
        "Voice triggers starts at the top of the view: {section:?} in {viewport:?}"
    );
    let editor = cx.debug_bounds("agents-rule-editor").expect("editor");
    assert!(editor.top() < viewport.bottom(), "the editor is in view");
    // Say and Task fill the editor (a collapsed, percent-width input once
    // drew Say as a tiny square at 1280 px).
    let wide_inputs = |cx: &mut VisualTestContext, at: &str| {
        let editor = cx.debug_bounds("agents-rule-editor").expect("editor");
        for selector in ["text-input-launch claude", "text-input-{prompt}"] {
            let input = cx.debug_bounds(selector).expect(selector);
            assert!(
                input.size.width >= px(250.),
                "{selector} is usable at {at}: {input:?}"
            );
            assert!(
                input.left() >= editor.left() && input.right() <= editor.right(),
                "{selector} inside the editor at {at}: {input:?} in {editor:?}"
            );
        }
    };
    wide_inputs(cx, "1280x850");
    // The same window captured at the minimum size: brought back into view.
    cx.simulate_resize(size(px(960.), px(640.)));
    for _ in 0..8 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
    let viewport = scroll.bounds();
    let section = cx.debug_bounds("agents-triggers").expect("section");
    assert!(
        section.top() >= viewport.top() && section.top() <= viewport.top() + px(40.),
        "after a resize: {section:?} in {viewport:?}"
    );
    wide_inputs(cx, "960x640");
    for dark in [false, true] {
        cx.update(|_, cx| cx.set_global(if dark { Theme::dark() } else { Theme::light() }));
        for (w, h) in [(1200., 760.), (960., 640.)] {
            cx.simulate_resize(size(px(w), px(h)));
            cx.run_until_parked();
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            let body = cx.debug_bounds("agents-settings").expect("tab body");
            assert!(body.right() <= px(w), "body fits {w}x{h}");
            for selector in [
                "agents-triggers",
                "agents-rule-editor",
                "agents-rule-try",
                "agents-rule-result",
            ] {
                let b = cx.debug_bounds(selector).expect(selector);
                assert!(
                    b.left() >= body.left() && b.right() <= body.right(),
                    "{selector}: {b:?} isn't inside {body:?} at {w}x{h}"
                );
            }
        }
    }
}

/// The plain `agents-settings` scene stays at the top of the page.
#[gpui::test]
fn the_plain_settings_scene_is_not_scrolled(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = open_with(
        Page::Home,
        |root| crate::sandbox::add_agents_fixture(root).unwrap(),
        cx,
    );
    fake(&shell, cx).use_settings_fixture();
    shell.update(cx, |s, cx| s.refresh_views(cx));
    shell.update(cx, |s, cx| {
        s.run_fixture(crate::sandbox::Fixture::AgentsSettings, cx)
    });
    cx.simulate_resize(size(px(1280.), px(850.)));
    for _ in 0..4 {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }
    let scroll = settings_page(&shell, cx).read_with(cx, |p, _| p.page_scroll());
    assert_eq!(scroll.offset().y, px(0.));
}
