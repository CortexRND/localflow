//! Final-review regressions on the Agents page and the shell: a reply never
//! skips unread Activity (H1), Activity catches up after a full read and
//! events errors retry (M3), and status is asked on transitions only,
//! never again after "no agents API" until health changes (M1).

use std::sync::Arc;

use gpui::{Entity, TestAppContext, VisualTestContext};

use super::focus_tests::open_with;
use super::*;
use crate::controllers::agents::AgentsError;
use crate::pages::agents::AgentsPage;
use crate::sandbox::agents::{FakeAgents, CLAUDE_RUNNING};

fn fake(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Arc<FakeAgents> {
    shell.read_with(cx, |shell, _| {
        shell.cx.services.remote.sandbox_agents().expect("sandbox")
    })
}

fn tick(cx: &mut VisualTestContext, secs: u64) {
    cx.executor().advance_clock(Duration::from_secs(secs));
    cx.run_until_parked();
}

/// Agents shown (the fake on), on the running Claude session.
fn on_running(
    cx: &mut TestAppContext,
) -> (
    Entity<Shell>,
    Entity<AgentsPage>,
    &mut VisualTestContext,
    tempfile::TempDir,
    Arc<FakeAgents>,
) {
    let (shell, cx, dir) = open_with(Page::Home, |_| {}, cx);
    let fake = fake(&shell, cx);
    fake.set_enabled(true);
    shell.update(cx, |s, cx| s.refresh_views(cx));
    shell.update(cx, |s, cx| s.navigate(Page::Agents, None, cx));
    cx.run_until_parked();
    let page = shell.read_with(cx, |s, _| match s.find(Page::Agents) {
        Some(View::Agents(v)) => v.clone(),
        _ => panic!("Agents is open"),
    });
    page.update(cx, |p, cx| p.select_for_test(CLAUDE_RUNNING, cx));
    cx.run_until_parked();
    assert!(page.read_with(cx, |p, _| p.activity().is_some_and(|a| a.loaded)));
    (shell, page, cx, dir, fake)
}

fn texts(page: &Entity<AgentsPage>, cx: &mut VisualTestContext) -> Vec<String> {
    page.read_with(cx, |p, _| {
        p.activity()
            .map(|a| a.newest_first().map(|e| e.text.clone()).collect())
            .unwrap_or_default()
    })
}

/// H1: output written before a reply, unread yet, is still read after the
/// reply comes back (the reply doesn't move the read cursor).
#[gpui::test]
fn a_reply_never_skips_unread_activity(cx: &mut TestAppContext) {
    let (_shell, page, cx, _dir, fake) = on_running(cx);
    fake.emit(CLAUDE_RUNNING, "Unread line one");
    fake.emit(CLAUDE_RUNNING, "Unread line two");
    let reply = page.read_with(cx, |p, _| p.reply_input());
    reply.update(cx, |r, cx| r.set_text("Go ahead", cx));
    page.update(cx, |p, cx| p.send_reply_for_test(cx));
    cx.run_until_parked();
    let shown = texts(&page, cx);
    for line in ["Unread line one", "Unread line two", "Go ahead"] {
        assert!(shown.iter().any(|t| t == line), "{line} in {shown:?}");
    }
    let after = page.read_with(cx, |p, _| p.activity().unwrap().after);
    let last = page.read_with(cx, |p, _| {
        p.store().session(CLAUDE_RUNNING).unwrap().last_seq
    });
    assert!(
        after >= last.saturating_sub(1),
        "caught up: {after} of {last}"
    );
}

/// M3: back on Agents after the selected session moved meanwhile: its
/// Activity catches up with the full read (no 25 s wait).
#[gpui::test]
fn activity_catches_up_after_a_full_read(cx: &mut TestAppContext) {
    let (shell, page, cx, _dir, fake) = on_running(cx);
    shell.update(cx, |s, cx| s.navigate(Page::Home, None, cx));
    cx.run_until_parked();
    fake.emit(CLAUDE_RUNNING, "Finished while you were away");
    assert!(fake.set_state(CLAUDE_RUNNING, "completed"));
    shell.update(cx, |s, cx| s.navigate(Page::Agents, None, cx));
    cx.run_until_parked();
    assert!(texts(&page, cx)
        .iter()
        .any(|t| t == "Finished while you were away"));
}

/// M3: a failed events read is tried again, backing off, until it works.
#[gpui::test]
fn a_failed_events_read_is_retried(cx: &mut TestAppContext) {
    let (_shell, page, cx, _dir, fake) = on_running(cx);
    fake.emit(CLAUDE_RUNNING, "After the outage");
    fake.fail(Some(AgentsError::Offline));
    let reads = fake.calls().events;
    page.update(cx, |p, cx| p.read_events_for_test(cx));
    cx.run_until_parked();
    assert_eq!(fake.calls().events, reads + 1);
    tick(cx, 2);
    assert_eq!(fake.calls().events, reads + 2, "retried after 2 s");
    fake.fail(None);
    tick(cx, 4);
    assert!(fake.calls().events >= reads + 3, "retried after 4 s");
    assert!(texts(&page, cx).iter().any(|t| t == "After the outage"));
}

/// M1: status is asked on window activation (a transition), not per
/// health tick; after "no agents API" it isn't asked again until health
/// changes.
#[gpui::test]
fn status_is_asked_on_transitions_and_not_after_absent(cx: &mut TestAppContext) {
    let (shell, cx, _dir) = open_with(Page::Home, |_| {}, cx);
    let fake = fake(&shell, cx);
    cx.run_until_parked();
    let at_rest = fake.calls().status;
    for _ in 0..4 {
        tick(cx, 5);
    }
    assert_eq!(fake.calls().status, at_rest, "no status read per tick");
    shell.update(cx, |s, cx| s.window_activation_changed(true, cx));
    cx.run_until_parked();
    assert_eq!(fake.calls().status, at_rest + 1, "asked on activation");
    fake.fail(Some(AgentsError::Absent));
    shell.update(cx, |s, cx| s.window_activation_changed(true, cx));
    cx.run_until_parked();
    assert_eq!(fake.calls().status, at_rest + 2);
    for _ in 0..3 {
        shell.update(cx, |s, cx| s.window_activation_changed(true, cx));
        tick(cx, 5);
    }
    assert_eq!(
        fake.calls().status,
        at_rest + 2,
        "not asked again after 404"
    );
    assert!(!shell.read_with(cx, |s, _| s.cx.agents_supported()));
}

/// Failed events reads retry only while Agents is shown; hidden, they
/// stop, and resume (once) when the page is shown again.
#[gpui::test]
fn events_retries_stop_while_hidden_and_resume(cx: &mut TestAppContext) {
    let (shell, page, cx, _dir, fake) = on_running(cx);
    fake.fail(Some(AgentsError::Offline));
    let reads = fake.calls().events;
    page.update(cx, |p, cx| p.read_events_for_test(cx));
    cx.run_until_parked();
    assert_eq!(fake.calls().events, reads + 1);
    shell.update(cx, |s, cx| s.navigate(Page::Home, None, cx));
    cx.run_until_parked();
    for secs in [2, 4, 8, 10] {
        tick(cx, secs);
    }
    assert_eq!(fake.calls().events, reads + 1, "no retries while hidden");
    fake.fail(None);
    fake.emit(CLAUDE_RUNNING, "Read once back on Agents");
    shell.update(cx, |s, cx| s.navigate(Page::Agents, None, cx));
    cx.run_until_parked();
    assert!(texts(&page, cx)
        .iter()
        .any(|t| t == "Read once back on Agents"));
    assert!(!page.read_with(cx, |p, _| p.events_stalled_for_test()));
}

/// A session's events 404 (it went away): no retry.
#[gpui::test]
fn a_gone_session_stops_its_events_reader(cx: &mut TestAppContext) {
    let (_shell, page, cx, _dir, fake) = on_running(cx);
    fake.fail(Some(AgentsError::Absent));
    let reads = fake.calls().events;
    page.update(cx, |p, cx| p.read_events_for_test(cx));
    cx.run_until_parked();
    for secs in [2, 4, 8] {
        tick(cx, secs);
    }
    fake.fail(None);
    assert_eq!(fake.calls().events, reads + 1, "no retry after a 404");
}

