//! The sidebar's needs-input badge and its one toast (Slice C), against the
//! in-process fake: the shell's watcher primes and follows `/changes` while
//! Agents isn't shown; the page feeds the count while it is; at most one
//! `/changes` call is ever open.

use std::sync::Arc;

use gpui::{Entity, TestAppContext, VisualTestContext};

use super::focus_tests::open_with;
use super::*;
use crate::controllers::agents_attention::nav_label;
use crate::sandbox::agents::{FakeAgents, CLAUDE_RUNNING, CODEX_RUNNING, NEEDS_INPUT};

fn fake(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Arc<FakeAgents> {
    shell.read_with(cx, |shell, _| {
        shell.cx.services.remote.sandbox_agents().expect("sandbox")
    })
}

fn tick(cx: &mut VisualTestContext, secs: u64) {
    cx.executor().advance_clock(Duration::from_secs(secs));
    cx.run_until_parked();
}

fn count(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> usize {
    shell.read_with(cx, |s, _| s.agents_attention)
}

fn toast(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<(String, Option<String>)> {
    shell.read_with(cx, |s, _| {
        s.toast.as_ref().map(|t| {
            (
                t.message.to_string(),
                t.action.as_ref().map(|(label, _)| label.to_string()),
            )
        })
    })
}

/// Home, with the fake service's Agents turned on (the seed has one
/// session that needs input) and the watcher primed.
fn open_on(
    cx: &mut TestAppContext,
) -> (
    Entity<Shell>,
    &mut VisualTestContext,
    tempfile::TempDir,
    Arc<FakeAgents>,
) {
    let (shell, cx, dir) = open_with(Page::Home, |_| {}, cx);
    let fake = fake(&shell, cx);
    fake.set_enabled(true);
    shell.update(cx, |s, cx| s.refresh_views(cx));
    cx.run_until_parked();
    assert!(shell.read_with(cx, |s, _| s.agents_enabled()));
    (shell, cx, dir, fake)
}

/// Primed on another page: the badge counts needs-input sessions only, its
/// row is named for it, and priming shows no toast.
#[gpui::test]
fn the_badge_counts_needs_input_only_and_priming_is_quiet(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake) = open_on(cx);
    assert_eq!(fake.calls().waiting, 1, "primed once");
    assert_eq!(count(&shell, cx), 1, "two running sessions don't badge");
    assert_eq!(toast(&shell, cx), None, "no toast on prime");
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert!(cx.debug_bounds("nav-label:Agents, 1 needs input").is_some());
    assert_eq!(nav_label(0), "Agents");
}

/// A session that newly needs input, while Agents isn't shown: the badge
/// grows and one toast names its provider; Open goes to that session.
#[gpui::test]
fn a_newly_waiting_session_toasts_once_and_opens(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake) = open_on(cx);
    assert!(fake.set_state(CLAUDE_RUNNING, "waiting_input"));
    tick(cx, 2);
    assert_eq!(count(&shell, cx), 2);
    assert_eq!(
        toast(&shell, cx),
        Some(("Claude needs your input".into(), Some("Open".into())))
    );
    // The same state again: no second toast.
    shell.update(cx, |s, cx| {
        s.toast = None;
        cx.notify();
    });
    tick(cx, 2);
    assert_eq!(toast(&shell, cx), None);

    // Open: Agents, on that session.
    assert!(fake.set_state(CODEX_RUNNING, "waiting_input"));
    tick(cx, 2);
    let action = shell
        .read_with(cx, |s, _| s.toast.as_ref().and_then(|t| t.action.clone()))
        .expect("Open")
        .1;
    cx.update(|window, cx| action(window, cx));
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |s, _| s.page), Page::Agents);
    let selected = shell.read_with(cx, |s, cx| match s.find(Page::Agents) {
        Some(View::Agents(v)) => v.read(cx).selected(),
        _ => None,
    });
    assert_eq!(selected.as_deref(), Some(CODEX_RUNNING));
}

/// Answered sessions leave the count; it clears to no badge.
#[gpui::test]
fn the_badge_clears_when_nothing_needs_input(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake) = open_on(cx);
    assert!(fake.set_state(NEEDS_INPUT, "running"));
    tick(cx, 2);
    assert_eq!(count(&shell, cx), 0);
    assert_eq!(nav_label(count(&shell, cx)), "Agents");
    assert_eq!(toast(&shell, cx), None, "leaving needs-input never toasts");
}

/// `reset: true`: cleared, then primed again quietly, even though a
/// session started waiting meanwhile.
#[gpui::test]
fn a_reset_re_primes_without_a_toast(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake) = open_on(cx);
    let primes = fake.calls().waiting;
    fake.reset_next();
    assert!(fake.set_state(CLAUDE_RUNNING, "waiting_input"));
    tick(cx, 2);
    assert_eq!(fake.calls().waiting, primes, "the reset, not yet re-primed");
    assert_eq!(count(&shell, cx), 1, "no flash to 0 before the re-prime");
    tick(cx, 2);
    assert_eq!(fake.calls().waiting, primes + 1, "re-primed");
    assert_eq!(count(&shell, cx), 2);
    assert_eq!(toast(&shell, cx), None, "no toast on a re-prime");
}

/// While Agents is shown the watcher makes no call (the page is the one
/// `/changes` reader and feeds the count), and nothing toasts there.
#[gpui::test]
fn on_agents_the_page_feeds_the_count_with_one_reader(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake) = open_on(cx);
    shell.update(cx, |s, cx| s.navigate(Page::Agents, None, cx));
    cx.run_until_parked();
    tick(cx, 2);
    let (changes, primes) = (fake.calls().changes, fake.calls().waiting);
    for _ in 0..3 {
        tick(cx, 2);
    }
    assert_eq!(fake.calls().changes, changes + 3, "the page's reads only");
    assert_eq!(fake.calls().waiting, primes, "the watcher primes nothing");
    assert!(fake.set_state(CLAUDE_RUNNING, "waiting_input"));
    tick(cx, 2);
    assert_eq!(count(&shell, cx), 2, "fed by the page's list");
    assert_eq!(toast(&shell, cx), None, "no toast while Agents is shown");
    // Back on another page: the watcher primes again, quietly.
    shell.update(cx, |s, cx| s.navigate(Page::Home, None, cx));
    tick(cx, 2);
    tick(cx, 2);
    assert_eq!(fake.calls().waiting, primes + 1);
    assert_eq!(count(&shell, cx), 2);
    assert_eq!(toast(&shell, cx), None);
}

/// One `/changes` call at a time: while another reader holds one, the
/// watcher waits instead of opening a second.
#[gpui::test]
fn the_watcher_waits_while_another_changes_call_is_open(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake) = open_on(cx);
    let gate = shell.read_with(cx, |s, _| s.cx.agents_changes.clone());
    let held = gate.enter().expect("nothing open at rest");
    let changes = fake.calls().changes;
    for _ in 0..3 {
        tick(cx, 2);
    }
    assert_eq!(fake.calls().changes, changes, "no second call");
    drop(held);
    tick(cx, 2);
    assert!(fake.calls().changes > changes, "resumes once it closes");
    assert_eq!(gate.open(), 0);
}

/// The gate closing takes the badge with it.
#[gpui::test]
fn turning_agents_off_clears_the_badge(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake) = open_on(cx);
    assert_eq!(count(&shell, cx), 1);
    fake.set_enabled(false);
    shell.update(cx, |s, cx| s.refresh_views(cx));
    cx.run_until_parked();
    assert_eq!(count(&shell, cx), 0);
    let calls = fake.calls().changes;
    for _ in 0..3 {
        tick(cx, 2);
    }
    assert_eq!(fake.calls().changes, calls, "the watcher stopped");
}
