//! Final-review regressions on Settings → Agents: a row-scoped Check (M4),
//! accepted writes shown at once (M5), a stale load never undoing Remove
//! key, and keys trimmed (a blank one sends nothing). A child of the
//! Slice A tests, so it shares their helpers.

use super::*;
use crate::controllers::agents::AgentsBackend;
use crate::controllers::agents_settings::SecretValue;

/// M4: a deep Check busies only its row; the rest of the tab still writes.
#[gpui::test]
fn a_check_busies_only_its_row(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    let (id, profile) = agents.read_with(cx, |a, _| {
        let p = &a.providers()[0];
        (p.id.clone(), p.profile.clone())
    });
    agents.update(cx, |a, cx| {
        a.check(id.clone(), profile.clone(), cx);
        assert!(a.checking_for_test(&id, &profile));
        assert!(!a.writing(), "not the tab's write guard");
        // The switch still writes meanwhile.
        a.set_enabled(false, cx);
        assert!(a.writing());
    });
    cx.run_until_parked();
    assert_eq!(fake.calls().check, 1);
    assert_eq!(
        fake.config_bodies(),
        [json!({"agents": {"enabled": false}})]
    );
    // Done; the switch's reload (newer than the check) shows the row.
    agents.read_with(cx, |a, _| assert!(!a.checking_for_test(&id, &profile)));
}

/// M5: an accepted write is shown at once; the reload's failure doesn't
/// take it back.
#[gpui::test]
fn an_accepted_write_does_not_snap_back(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, _fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    agents.update(cx, |a, cx| a.choose_default_provider("devin", cx));
    cx.run_until_parked();
    // The reload after the write fails (nothing answering): the accepted
    // value stays on screen, under the banner.
    let generation = agents.update(cx, |a, _| a.start_stale_load());
    agents.update(cx, |a, cx| {
        a.land_load(generation, Err(AgentsError::Offline), cx)
    });
    agents.read_with(cx, |a, _| {
        assert!(a.service_down());
        assert_eq!(a.settings().unwrap().default_provider, "devin");
    });
}

/// A load started before Remove key lands after it: the removal stands.
#[gpui::test]
fn a_stale_load_never_undoes_remove_key(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, |_| {});
    let t = agents.read_with(cx, |a, _| {
        target(
            a.providers()
                .iter()
                .find(|p| p.secrets.iter().any(|s| s.required && s.set))
                .unwrap(),
        )
    });
    // What a load started now would answer: the key still set.
    let stale = (
        fake.settings().unwrap(),
        fake.providers().unwrap(),
        fake.rules().unwrap(),
    );
    let generation = agents.update(cx, |a, _| a.start_stale_load());
    agents.update(cx, |a, cx| a.remove_key_now(t.clone(), cx));
    cx.run_until_parked();
    // The old answer lands last.
    agents.update(cx, |a, cx| a.land_load(generation, Ok(stale), cx));
    let p = provider_of(&agents, &t, cx);
    assert!(!p.secrets[0].set, "the removal stands");
    assert_eq!(p.readiness, Readiness::NotConnected);
}

/// Keys are trimmed before they're saved; a blank one sends nothing.
#[gpui::test]
fn a_blank_key_sends_nothing(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    let t = missing_key(&agents, cx);
    open_key(&agents, &t, cx);
    agents.update(cx, |a, cx| {
        a.type_key("  \n\t", cx);
        a.save_key(cx);
    });
    cx.run_until_parked();
    assert_eq!(fake.calls().set_secret, 0);
    assert_eq!(
        SecretValue::trimmed(format!("{PLANTED}\n")).expose(),
        PLANTED,
        "a pasted newline is not part of the key"
    );
}

/// Agents writes are pending writes while they run, so closing the window
/// waits for them (config, a key, a trigger). The tracker is
/// process-wide; only "pending while ours runs" is asserted.
#[gpui::test]
fn agents_writes_hold_the_shutdown_tracker(cx: &mut TestAppContext) {
    use lf_ui::shutdown::wait_for_pending_writes;
    use std::time::Duration;
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    agents.update(cx, |a, cx| {
        a.set_enabled(false, cx);
        assert!(a.writing());
        assert!(
            !wait_for_pending_writes(Duration::ZERO),
            "config write pending"
        );
    });
    cx.run_until_parked();
    assert_eq!(fake.calls().set_config, 1);
    let t = missing_key(&agents, cx);
    open_key(&agents, &t, cx);
    agents.update(cx, |a, cx| {
        a.type_key(PLANTED, cx);
        a.save_key(cx);
        assert!(
            !wait_for_pending_writes(Duration::ZERO),
            "key write pending"
        );
    });
    cx.run_until_parked();
    assert_eq!(fake.calls().set_secret, 1);
    agents.update(cx, |a, cx| {
        a.toggle_rule(crate::sandbox::agents::RULE_ASK.into(), true, cx);
        assert!(
            !wait_for_pending_writes(Duration::ZERO),
            "trigger write pending"
        );
    });
    cx.run_until_parked();
    assert_eq!(fake.calls().update_rule, 1);
}
