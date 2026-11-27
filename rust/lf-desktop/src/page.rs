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

    /// The sidebar as drawn (arrow-key order): People follows Meetings only
    /// while it is turned on.
    pub fn visible_order(people: bool) -> Vec<Page> {
        let mut pages = Page::sidebar_order();
        if people {
            if let Some(i) = pages.iter().position(|p| *p == Page::Meetings) {
                pages.insert(i + 1, Page::People);
            }
        }
        pages
    }

    /// `--page people`: parsed as an unknown name (People has no Hub route)
    /// and resolved once the config is known.
    pub fn is_people_name(name: &str) -> bool {
        name.trim().eq_ignore_ascii_case("people")
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
        assert_eq!(Page::visible_order(false), Page::sidebar_order());
        let shown = Page::visible_order(true);
        let meetings = shown.iter().position(|p| *p == Page::Meetings).unwrap();
        assert_eq!(shown[meetings + 1], Page::People);
        assert_eq!(shown.len(), 11);
        assert_eq!(
            Page::Models.label(),
            lf_ui::hub_model::sidebar_label("Models")
        );
    }
}
