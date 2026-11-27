//! Navigation destinations. Route ids are the existing Hub page names.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Home,
    History,
    Dictionary,
    Snippets,
    Style,
    Ask,
    Meetings,
    Prompts,
    Models,
    Settings,
    /// Read-only People (remembered voices). GPUI-only and gated by
    /// `controllers::people::enabled`: never in [`Page::ALL`], the sidebar
    /// order for Ctrl+1..0, or a hand-off request (it has no Hub route).
    People,
    /// Agents (sessions handed to cloud or local agents). GPUI-only like
    /// People, and shown only while the service reports the feature on
    /// (`controllers::agents::available`).
    Agents,
}

/// Sidebar groups: `(group label, pages)`; `None` = ungrouped.
pub const NAV_GROUPS: [(Option<&str>, &[Page]); 3] = [
    (None, &[Page::Home, Page::History]),
    (
        Some("Personalize"),
        &[Page::Dictionary, Page::Snippets, Page::Style],
    ),
    (
        Some("Workspace"),
        &[Page::Ask, Page::Meetings, Page::Prompts],
    ),
];
/// Pinned to the bottom of the sidebar.
pub const NAV_FOOTER: [Page; 2] = [Page::Models, Page::Settings];

impl Page {
    pub const ALL: [Page; 10] = [
        Page::Home,
        Page::History,
        Page::Dictionary,
        Page::Snippets,
        Page::Style,
        Page::Ask,
        Page::Meetings,
        Page::Prompts,
        Page::Models,
        Page::Settings,
    ];

    /// The existing route id (`hub_route::HUB_PAGES`).
    pub fn route(self) -> &'static str {
        match self {
            Page::Home => "Home",
            Page::History => "History",
            Page::Dictionary => "Dictionary",
            Page::Snippets => "Hot Phrases",
            Page::Style => "Style",
            Page::Ask => "Ask",
            Page::Meetings => "Meetings",
            Page::Prompts => "Prompts",
            Page::Models => "Models",
            Page::Settings => "Settings",
            Page::People => "People",
            Page::Agents => "Agents",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Page::Snippets => "Snippets",
            Page::Models => "Models & keys",
            other => other.route(),
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::Home => "house",
            Page::History => "history",
            Page::Dictionary => "book-a",
            Page::Snippets => "zap",
            Page::Style => "pen-line",
            Page::Ask => "message-circle-question",
            Page::Meetings => "users",
            Page::Prompts => "list-checks",
            Page::Models => "cpu",
            Page::Settings => "settings",
            Page::People => "user-round",
            Page::Agents => "bot",
        }
    }

    /// Typed input: route ids case-insensitively, plus "snippets".
    pub fn parse(text: &str) -> Option<Page> {
        let folded = text.trim().to_lowercase();
        if folded == "snippets" {
            return Some(Page::Snippets);
        }
        if folded == "setup" {
            return Some(Page::Home);
        }
        let route = lf_ui::hub_route::canonical_page(text)?;
        Page::ALL.into_iter().find(|p| p.route() == route)
    }

    /// Index in sidebar order (for Ctrl+1..0).
    pub fn sidebar_order() -> Vec<Page> {
        let mut pages: Vec<Page> = NAV_GROUPS
            .iter()
            .flat_map(|(_, p)| p.iter().copied())
            .collect();
        pages.extend(NAV_FOOTER);
        pages
    }

    /// The sidebar as drawn (arrow-key order): People follows Meetings and
    /// Agents follows Prompts, each only while it is turned on.
    pub fn visible_order(people: bool, agents: bool) -> Vec<Page> {
        let mut pages = Page::sidebar_order();
        for (on, after, page) in [
            (people, Page::Meetings, Page::People),
            (agents, Page::Prompts, Page::Agents),
        ] {
            if on {
                if let Some(i) = pages.iter().position(|p| *p == after) {
                    pages.insert(i + 1, page);
                }
            }
        }
        pages
    }

    /// `--page people`: parsed as an unknown name (People has no Hub route)
    /// and resolved once the config is known.
    pub fn is_people_name(name: &str) -> bool {
        name.trim().eq_ignore_ascii_case("people")
    }

    /// A hand-off for `agents`: an unknown name (no Hub route), resolved by
    /// the open window once it knows whether Agents is on.
    pub fn is_agents_name(name: &str) -> bool {
        let name = name.trim();
        name.eq_ignore_ascii_case("agents") || Page::agents_session(name).is_some()
    }

    /// `agents/<session id>`: the launcher's request to open Agents on one
    /// session (`--page agents --agent-session <id>`).
    pub fn agents_session(name: &str) -> Option<String> {
        let (page, id) = name.trim().split_once('/')?;
        (page.eq_ignore_ascii_case("agents") && crate::controllers::agents::is_session_id(id))
            .then(|| id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_match_the_hub() {
        for page in Page::ALL {
            assert!(lf_ui::hub_route::HUB_PAGES.contains(&page.route()));
            assert_eq!(Page::parse(page.route()), Some(page));
        }
        assert_eq!(Page::sidebar_order().len(), 10);
        assert!(!Page::ALL.contains(&Page::People));
        assert!(!Page::sidebar_order().contains(&Page::People));
        assert_eq!(Page::parse("people"), None, "no Hub route");
        assert!(Page::is_people_name(" People "));
        assert_eq!(Page::visible_order(false, false), Page::sidebar_order());
        let shown = Page::visible_order(true, false);
        let meetings = shown.iter().position(|p| *p == Page::Meetings).unwrap();
        assert_eq!(shown[meetings + 1], Page::People);
        assert_eq!(shown.len(), 11);
        // Agents: after Prompts, never in Ctrl+1..0 or the Hub routes.
        assert!(!Page::ALL.contains(&Page::Agents));
        assert!(!Page::sidebar_order().contains(&Page::Agents));
        assert_eq!(Page::parse("agents"), None, "no Hub route");
        assert!(Page::is_agents_name(" Agents "));
        let shown = Page::visible_order(false, true);
        let prompts = shown.iter().position(|p| *p == Page::Prompts).unwrap();
        assert_eq!(shown[prompts + 1], Page::Agents);
        assert_eq!(shown.len(), 11);
        let both = Page::visible_order(true, true);
        assert_eq!(both.len(), 12);
        let without: Vec<Page> = both
            .into_iter()
            .filter(|p| !matches!(p, Page::People | Page::Agents))
            .collect();
        assert_eq!(without, Page::sidebar_order(), "the ten keep their order");
        assert_eq!(
            Page::Models.label(),
            lf_ui::hub_model::sidebar_label("Models")
        );
    }
}
