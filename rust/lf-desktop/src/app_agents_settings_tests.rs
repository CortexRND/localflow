//! Settings → Agents against the in-process fake service (Slice A): tab
//! visibility, the switch and the gate, keys, workspaces, defaults, the
//! `agents/settings` deep link and the two fixture scenes. A child module
//! of `app`, so it can read the shell's dialog and toast.

use std::sync::Arc;

use gpui::{Entity, TestAppContext, VisualTestContext};
use serde_json::json;

use super::focus_tests::open_with;
use super::*;
use crate::controllers::agents::{AgentsError, Provider, Readiness};
use crate::controllers::agents_settings as model;
use crate::controllers::settings::Tab;
use crate::pages::settings_agents::{AgentsSettings, KeyTarget, Row};
use crate::sandbox::agents::FakeAgents;

const PLANTED: &str = "sk-planted-settings-secret-0123456789";

fn fake(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Arc<FakeAgents> {
    shell.read_with(cx, |shell, _| {
        shell.cx.services.remote.sandbox_agents().expect("sandbox")
    })
}

fn settings_page(
    shell: &Entity<Shell>,
    cx: &mut VisualTestContext,
) -> Entity<pages::settings::SettingsPage> {
    shell.read_with(cx, |shell, _| match shell.view(Page::Settings) {
        Some(View::Settings(v)) => v.clone(),
        _ => panic!("Settings is open"),
    })
}

fn agents_tab(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Entity<AgentsSettings> {
    let page = settings_page(shell, cx);
    page.read_with(cx, |p, _| {
        p.agents_settings().expect("the Agents tab was built")
    })
}

fn page(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Page {
    shell.read_with(cx, |shell, _| shell.page)
}

fn toast(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> Option<String> {
    shell.read_with(cx, |shell, _| {
        shell.toast.as_ref().map(|t| t.message.to_string())
    })
}

type Opened<'a> = (
    Entity<Shell>,
    &'a mut VisualTestContext,
    tempfile::TempDir,
    Arc<FakeAgents>,
    Entity<AgentsSettings>,
);

/// Settings → Agents open and read; `prepare` sets the fake up first.
fn open_tab(cx: &mut TestAppContext, prepare: impl FnOnce(&FakeAgents)) -> Opened<'_> {
    let (shell, cx, dir) = open_with(Page::Home, |_| {}, cx);
    let fake = fake(&shell, cx);
    prepare(&fake);
    shell.update(cx, |s, cx| s.refresh_views(cx));
    shell.update(cx, |s, cx| s.navigate(Page::Settings, Some("agents"), cx));
    cx.run_until_parked();
    let agents = agents_tab(&shell, cx);
    assert!(agents.read_with(cx, |a, _| a.settings().is_some()), "read");
    (shell, cx, dir, fake, agents)
}

/// The open dialog's copy; then its action runs (as Confirm would).
fn confirm(shell: &Entity<Shell>, cx: &mut VisualTestContext) -> (String, String, String, bool) {
    let confirm = shell
        .update(cx, |s, _| s.confirm.take())
        .expect("a confirmation");
    let action = confirm.on_confirm.clone();
    cx.update(|window, cx| action(window, cx));
    cx.run_until_parked();
    (
        confirm.title.to_string(),
        confirm.body.to_string(),
        confirm.verb.to_string(),
        confirm.destructive,
    )
}

fn target(provider: &Provider) -> KeyTarget {
    KeyTarget {
        id: provider.id.clone(),
        profile: provider.profile.clone(),
        name: provider.secrets[0].name.clone(),
    }
}

/// The first provider missing a required key (the fixture's cloud one).
fn missing_key(agents: &Entity<AgentsSettings>, cx: &mut VisualTestContext) -> KeyTarget {
    agents.read_with(cx, |a, _| {
        target(
            a.providers()
                .iter()
                .find(|p| p.secrets.iter().any(|s| s.required && !s.set))
                .expect("a provider without its key"),
        )
    })
}

fn provider_of(
    agents: &Entity<AgentsSettings>,
    t: &KeyTarget,
    cx: &mut VisualTestContext,
) -> Provider {
    agents.read_with(cx, |a, _| {
        a.providers()
            .iter()
            .find(|p| p.id == t.id && p.profile == t.profile)
            .cloned()
            .unwrap()
    })
}

fn open_key(agents: &Entity<AgentsSettings>, t: &KeyTarget, cx: &mut VisualTestContext) {
    let t = t.clone();
    cx.update(|window, cx| agents.update(cx, |a, cx| a.open_key(t, window, cx)));
    cx.run_until_parked();
}

/// With a v1 service that has Agents off, the tab is shown (the only place
/// to turn it on); turning it on opens the gate at once.
#[gpui::test]
fn the_tab_shows_while_off_and_turning_agents_on_opens_the_gate(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, |_| {});
    assert!(!shell.read_with(cx, |s, _| s.agents_enabled()));
    assert!(shell.read_with(cx, |s, _| s.cx.agents_supported()));
    let settings = settings_page(&shell, cx);
    assert_eq!(settings.read_with(cx, |p, _| p.tab()), Tab::Agents);
    assert!(!agents.read_with(cx, |a, _| a.settings().unwrap().enabled));

    agents.update(cx, |a, cx| a.set_enabled(true, cx));
    cx.run_until_parked();
    assert_eq!(fake.config_bodies(), [json!({"agents": {"enabled": true}})]);
    assert!(
        shell.read_with(cx, |s, _| s.agents_enabled()),
        "the sidebar row appears without a restart"
    );
    assert!(agents.read_with(cx, |a, _| a.settings().unwrap().enabled));
    shell.update(cx, |s, cx| s.navigate(Page::Agents, None, cx));
    cx.run_until_parked();
    assert_eq!(page(&shell, cx), Page::Agents);
}

/// A 404 (no agents API) hides the tab; a transient failure keeps it, with
/// every control disabled.
#[gpui::test]
fn the_tab_is_hidden_without_an_agents_api(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, |_| {});
    fake.fail(Some(AgentsError::Offline));
    shell.update(cx, |s, cx| s.refresh_views(cx));
    cx.run_until_parked();
    assert!(shell.read_with(cx, |s, _| s.cx.agents_supported()), "kept");
    assert!(agents.read_with(cx, |a, _| a.service_down()));
    assert!(
        agents.read_with(cx, |a, _| a.settings().is_some()),
        "rows stay"
    );
    agents.update(cx, |a, cx| a.set_enabled(true, cx));
    cx.run_until_parked();
    assert_eq!(fake.calls().set_config, 0, "nothing is written meanwhile");

    fake.fail(Some(AgentsError::Absent));
    shell.update(cx, |s, cx| s.refresh_views(cx));
    cx.run_until_parked();
    assert!(!shell.read_with(cx, |s, _| s.cx.agents_supported()));
    let settings = settings_page(&shell, cx);
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
    assert_eq!(
        settings.read_with(cx, |p, _| p.tab()),
        Tab::General,
        "the open tab falls back"
    );
    settings.update(cx, |p, cx| p.open_tab("agents", cx));
    assert_eq!(settings.read_with(cx, |p, _| p.tab()), Tab::General);
}

/// Save: the field is cleared before the request runs; the row is
/// replaced, a deep check follows, and the value is nowhere.
#[gpui::test]
fn a_saved_key_is_cleared_at_once_and_never_kept(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    let t = missing_key(&agents, cx);
    open_key(&agents, &t, cx);
    assert_eq!(
        agents.read_with(cx, |a, _| a.editor().cloned()),
        Some(t.clone())
    );
    agents.update(cx, |a, cx| {
        a.type_key(PLANTED, cx);
        a.save_key(cx);
        assert_eq!(a.key_text(cx), "", "cleared before the request");
        assert!(a.writing());
    });
    assert_eq!(fake.calls().set_secret, 0, "the request hasn't run yet");
    cx.run_until_parked();
    assert_eq!(fake.calls().set_secret, 1);
    assert_eq!(fake.calls().check, 1, "a deep check follows");
    let provider = provider_of(&agents, &t, cx);
    assert_eq!(provider.readiness, Readiness::Connected);
    assert!(provider.deep && provider.secrets[0].set);
    assert!(agents.read_with(cx, |a, _| a.editor().is_none()));
    assert_eq!(toast(&shell, cx).as_deref(), Some("Key saved"));
    assert!(!fake.stored().contains("planted"));
    assert!(!format!("{:?}", fake.received()).contains("planted"));
    agents.read_with(cx, |a, cx| {
        assert!(!format!("{:?}", a.providers()).contains("planted"));
        assert!(a.key_error().is_none());
        assert_eq!(a.key_text(cx), "");
    });
}

/// 503 `secrets_unavailable`: the exact keyring copy; the editor stays open
/// with an empty field; nothing changes.
#[gpui::test]
fn an_unavailable_keyring_keeps_the_editor_open(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, |f| {
        f.use_settings_fixture();
        f.secrets_unavailable(true);
    });
    let t = missing_key(&agents, cx);
    open_key(&agents, &t, cx);
    agents.update(cx, |a, cx| {
        a.type_key(PLANTED, cx);
        a.save_key(cx);
    });
    cx.run_until_parked();
    agents.read_with(cx, |a, cx| {
        assert_eq!(
            a.key_error(),
            Some("Your system keyring isn't available, so the key wasn't saved.")
        );
        assert_eq!(a.editor(), Some(&t));
        assert_eq!(a.key_text(cx), "");
    });
    assert_eq!(
        provider_of(&agents, &t, cx).readiness,
        Readiness::NotConnected
    );
    assert_eq!(fake.calls().check, 0);
    assert!(toast(&shell, cx).is_none());
    assert!(!fake.stored().contains("planted"));
}

/// Escape closes the open editor first; leaving Settings drops a
/// half-entered key.
#[gpui::test]
fn escape_and_leaving_close_the_editor(cx: &mut TestAppContext) {
    let (shell, cx, _dir, _fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    let t = missing_key(&agents, cx);
    open_key(&agents, &t, cx);
    cx.dispatch_action(Escape);
    assert!(agents.read_with(cx, |a, _| a.editor().is_none()));
    assert_eq!(page(&shell, cx), Page::Settings);

    open_key(&agents, &t, cx);
    agents.update(cx, |a, cx| a.type_key("half", cx));
    shell.update(cx, |s, cx| s.navigate(Page::Home, None, cx));
    cx.run_until_parked();
    agents.read_with(cx, |a, cx| {
        assert!(a.editor().is_none());
        assert_eq!(a.key_text(cx), "");
    });
}

/// Remove key… asks first (destructive) and then sends an empty value.
#[gpui::test]
fn removing_a_key_needs_the_confirmation(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, |_| {});
    let (t, name) = agents.read_with(cx, |a, _| {
        let p = a
            .providers()
            .iter()
            .find(|p| p.secrets.iter().any(|s| s.required && s.set))
            .unwrap();
        (target(p), p.label.clone())
    });
    agents.update(cx, |a, cx| a.confirm_remove_key(t.clone(), cx));
    cx.run_until_parked();
    assert_eq!(
        fake.calls().set_secret,
        0,
        "nothing before the confirmation"
    );
    let (title, body, verb, destructive) = confirm(&shell, cx);
    assert_eq!(title, "Remove this key?");
    assert_eq!(
        body,
        format!(
            "The key is deleted from your system keyring. {name} can't start tasks until you add one."
        )
    );
    assert_eq!((verb.as_str(), destructive), ("Remove key", true));
    assert_eq!(fake.calls().set_secret, 1);
    let p = provider_of(&agents, &t, cx);
    assert!(!p.secrets[0].set);
    assert_eq!(p.readiness, Readiness::NotConnected);
}

/// Removing the default workspace: one body with both keys, after the
/// confirmation.
#[gpui::test]
fn removing_the_default_workspace_sends_one_combined_patch(cx: &mut TestAppContext) {
    let (shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    let default = agents.read_with(cx, |a, _| a.settings().unwrap().default_repo.clone());
    assert_eq!(default, "localflow");
    agents.update(cx, |a, cx| a.confirm_remove_workspace(default.clone(), cx));
    cx.run_until_parked();
    assert!(
        fake.config_bodies().is_empty(),
        "nothing before the confirmation"
    );
    let (title, body, verb, destructive) = confirm(&shell, cx);
    assert_eq!(title, "Remove this workspace?");
    assert_eq!(
        body,
        "\u{201c}localflow\u{201d} is removed from LocalFlow only. The folder and its files are not touched. Voice triggers that use it stop matching."
    );
    assert_eq!((verb.as_str(), destructive), ("Remove", true));
    assert_eq!(
        fake.config_bodies(),
        [
            json!({"agents": {"repos": ["website=/home/you/src/missing/website"], "default_repo": ""}})
        ]
    );
    agents.read_with(cx, |a, _| {
        let s = a.settings().unwrap();
        assert_eq!(s.default_repo, "");
        assert_eq!(s.repos.len(), 1);
        assert!(!s.repos[0].exists, "its folder is still not found");
    });
}

/// Add: the client checks first (nothing sent), then the whole list goes
/// in one body and the fields clear.
#[gpui::test]
fn adding_a_workspace_checks_then_sends_the_whole_list(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    agents.update(cx, |a, cx| {
        a.type_workspace("Docs", "/srv/docs", cx);
        a.add_workspace(cx);
        assert_eq!(a.error(&Row::AddWorkspace), Some(model::BAD_NAME));
        a.type_workspace("docs", "relative/docs", cx);
        a.add_workspace(cx);
        assert_eq!(a.error(&Row::AddWorkspace), Some(model::NOT_ABSOLUTE));
        a.type_workspace("website", "/srv/site", cx);
        a.add_workspace(cx);
        assert_eq!(
            a.error(&Row::AddWorkspace),
            Some("There's already a workspace called \u{201c}website\u{201d}.")
        );
    });
    cx.run_until_parked();
    assert_eq!(fake.calls().set_config, 0, "client checks only so far");
    agents.update(cx, |a, cx| {
        a.type_workspace("docs", "/srv/docs", cx);
        a.add_workspace(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        fake.config_bodies(),
        [json!({"agents": {"repos": [
            "localflow=/home/you/src/localflow",
            "website=/home/you/src/missing/website",
            "docs=/srv/docs",
        ]}})]
    );
    agents.read_with(cx, |a, _| {
        assert_eq!(a.settings().unwrap().repos.len(), 3);
        assert!(a.error(&Row::AddWorkspace).is_none());
    });
}

/// A refused write shows the service's detail on its row and the control
/// snaps back to the stored value; a later one goes through.
#[gpui::test]
fn a_refused_default_shows_the_detail_and_snaps_back(cx: &mut TestAppContext) {
    let (_shell, cx, _dir, fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    fake.fail_writes(Some(AgentsError::rejected(
        400,
        "validation",
        "agents.default_repo must be one of the aliases in agents.repos",
    )));
    agents.update(cx, |a, cx| a.choose_default_repo("website", cx));
    cx.run_until_parked();
    agents.read_with(cx, |a, _| {
        assert_eq!(
            a.error(&Row::DefaultRepo),
            Some("Agents.default_repo must be one of the aliases in agents.repos.")
        );
        assert_eq!(a.settings().unwrap().default_repo, "localflow");
    });
    fake.fail_writes(None);
    agents.update(cx, |a, cx| a.choose_default_provider("devin", cx));
    cx.run_until_parked();
    assert_eq!(
        fake.config_bodies().last(),
        Some(&json!({"agents": {"default_provider": "devin"}}))
    );
    assert_eq!(
        agents.read_with(cx, |a, _| a.settings().unwrap().default_provider.clone()),
        "devin"
    );
}

/// `agents/settings` (launcher, empty states) lands on the tab with Agents
/// off, without an unknown-page notice.
#[gpui::test]
fn the_agents_settings_deep_link_works_with_agents_off(cx: &mut TestAppContext) {
    let (shell, cx, dir) = open_with(Page::History, |_| {}, cx);
    assert!(!shell.read_with(cx, |s, _| s.agents_enabled()));
    let request = crate::controllers::instance::Request {
        route: Route::Page(Page::Home),
        unknown_page: Some(Page::AGENTS_SETTINGS.into()),
    };
    let launched =
        crate::launch::sandbox(&dir.path().join("sandbox"), false, Some(request)).unwrap();
    assert!(matches!(launched, crate::launch::Launched::HandedOff));
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.poll_instance(window, cx)));
    cx.run_until_parked();
    assert_eq!(page(&shell, cx), Page::Settings);
    let settings = settings_page(&shell, cx);
    assert_eq!(settings.read_with(cx, |p, _| p.tab()), Tab::Agents);
    assert!(toast(&shell, cx).is_none(), "no unknown-page notice");
}

/// The tab body at the design size and the minimum window, in both
/// palettes, with the key editor open: every row, the editor and the add
/// row stay inside the body, and the body inside the window.
#[gpui::test]
fn the_tab_lays_out_at_both_sizes_in_both_palettes(cx: &mut TestAppContext) {
    use gpui::{px, size};
    let (_shell, cx, _dir, _fake, agents) = open_tab(cx, FakeAgents::use_settings_fixture);
    let t = missing_key(&agents, cx);
    open_key(&agents, &t, cx);
    let rows: Vec<String> = agents.read_with(cx, |a, _| {
        let mut rows: Vec<String> = a
            .providers()
            .iter()
            .map(|p| format!("agents-provider:{}:{}", p.id, p.profile))
            .collect();
        rows.extend(
            a.settings()
                .unwrap()
                .repos
                .iter()
                .map(|r| format!("agents-workspace:{}", r.alias)),
        );
        rows
    });
    let inside =
        |inner: gpui::Bounds<gpui::Pixels>, outer: gpui::Bounds<gpui::Pixels>, what: &str| {
            assert!(
                inner.left() >= outer.left() && inner.right() <= outer.right(),
                "{what}: {inner:?} isn't inside {outer:?}"
            );
        };
    for dark in [false, true] {
        cx.update(|_, cx| cx.set_global(if dark { Theme::dark() } else { Theme::light() }));
        for (w, h) in [(1200., 760.), (960., 640.)] {
            cx.simulate_resize(size(px(w), px(h)));
            cx.run_until_parked();
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            let body = cx.debug_bounds("agents-settings").expect("tab body");
            assert!(body.left() >= px(crate::theme::size::SIDEBAR));
            assert!(body.right() <= px(w), "body fits {w}x{h}");
            for row in &rows {
                let selector: &'static str = Box::leak(row.clone().into_boxed_str());
                let bounds = cx.debug_bounds(selector).expect("row laid out");
                inside(bounds, body, row);
            }
            for selector in ["agents-key-editor", "agents-workspace-add"] {
                let bounds = cx.debug_bounds(selector).expect("laid out");
                inside(bounds, body, selector);
            }
            let input = cx
                .debug_bounds("text-input-Paste the API key")
                .expect("key input laid out");
            assert!(
                input.size.width >= px(250.),
                "key input is usable at {w}x{h}: {input:?}"
            );
            inside(input, body, "key input");
        }
    }
}

/// `--sandbox-fixture agents-settings|agents-settings-key` reach their end
/// state over the settings fixture.
#[gpui::test]
fn settings_fixture_scenes_reach_their_end_state(cx: &mut TestAppContext) {
    use crate::sandbox::Fixture;
    for fixture in [Fixture::AgentsSettings, Fixture::AgentsSettingsKey] {
        assert!(fixture.is_agents() && fixture.is_agents_settings());
        assert_eq!(fixture.page(), Page::Settings);
        let (shell, cx, _dir) = open_with(
            Page::Home,
            |root| crate::sandbox::add_agents_fixture(root).unwrap(),
            cx,
        );
        // What a `--sandbox-fixture` launch does first (Shell::new).
        fake(&shell, cx).use_settings_fixture();
        shell.update(cx, |s, cx| s.refresh_views(cx));
        shell.update(cx, |s, cx| s.run_fixture(fixture, cx));
        cx.run_until_parked();
        assert_eq!(page(&shell, cx), Page::Settings);
        let agents = agents_tab(&shell, cx);
        agents.read_with(cx, |a, cx| {
            let words: Vec<&str> = a
                .providers()
                .iter()
                .map(|p| model::readiness_word(p.readiness).0)
                .collect();
            assert_eq!(words, ["Connected", "Not connected", "Offline"]);
            assert_eq!(a.settings().unwrap().repos.len(), 2);
            match fixture {
                Fixture::AgentsSettingsKey => {
                    let editor = a.editor().expect("the key editor is open");
                    let p = a.providers().iter().find(|p| p.id == editor.id).unwrap();
                    assert_eq!(p.caps.kind, crate::controllers::agents::Kind::Cloud);
                    assert_eq!(p.readiness, Readiness::NotConnected);
                    assert_eq!(a.key_text(cx), "");
                }
                _ => assert!(a.editor().is_none()),
            }
        });
    }
}

#[path = "app_agents_triggers_tests.rs"]
mod triggers;

#[path = "app_agents_review_settings_tests.rs"]
mod review;
