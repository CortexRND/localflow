//! Frontmost-app metadata and cleanup-style selection (`localflow/context.py`).

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: String,
}

const BUNDLE_CATEGORIES: &[(&str, &str)] = &[
    ("com.tinyspeck.slackmacgap", "work"),
    ("com.microsoft.teams2", "work"),
    ("com.microsoft.teams", "work"),
    ("com.linear", "work"),
    ("notion.id", "work"),
    ("com.apple.mail", "email"),
    ("com.microsoft.outlook", "email"),
    ("com.superhuman.electron", "email"),
    ("com.readdle.sparkdesktop", "email"),
    ("com.apple.mobilesms", "personal"),
    ("net.whatsapp.whatsapp", "personal"),
    ("ru.keepcoder.telegram", "personal"),
    ("com.hnc.discord", "personal"),
];

const NAME_CATEGORIES: &[(&str, &str)] = &[
    ("slack", "work"),
    ("microsoft teams", "work"),
    ("linear", "work"),
    ("notion", "work"),
    ("mail", "email"),
    ("outlook", "email"),
    ("microsoft outlook", "email"),
    ("apple mail", "email"),
    ("superhuman", "email"),
    ("spark", "email"),
    ("messages", "personal"),
    ("whatsapp", "personal"),
    ("telegram", "personal"),
    ("discord", "personal"),
];

const CATEGORIES: &[&str] = &["work", "email", "personal", "other"];

fn fold(s: &str) -> String {
    caseless_fold(s)
}

fn caseless_fold(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// `category_for(app, overrides)`: "name=category" / "bundle=category"
/// overrides first, then the built-in tables, else "other".
pub fn category_for<S: AsRef<str>>(app: Option<&AppInfo>, overrides: &[S]) -> String {
    let Some(app) = app else {
        return "other".into();
    };
    let bundle_id = fold(&app.bundle_id);
    let name = fold(&app.name);
    for override_ in overrides {
        let Some((target, category)) = override_.as_ref().split_once('=') else {
            continue;
        };
        let category = fold(category.trim());
        if !CATEGORIES.contains(&category.as_str()) {
            continue;
        }
        let target = fold(target.trim());
        if !target.is_empty() && (target == bundle_id || target == name) {
            return category;
        }
    }
    BUNDLE_CATEGORIES
        .iter()
        .find(|(k, _)| *k == bundle_id)
        .or_else(|| NAME_CATEGORIES.iter().find(|(k, _)| *k == name))
        .map_or("other", |(_, c)| c)
        .to_string()
}

/// The focused app, captured when a recording starts.
pub fn frontmost_app() -> Option<AppInfo> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::frontmost_app()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, bundle: &str) -> AppInfo {
        AppInfo {
            name: name.into(),
            bundle_id: bundle.into(),
        }
    }

    #[test]
    fn categories() {
        let none: [&str; 0] = [];
        assert_eq!(category_for(None, &none), "other");
        assert_eq!(category_for(Some(&app("Slack", "com.tinyspeck.slackmacgap")), &none), "work");
        assert_eq!(category_for(Some(&app("Mail", "")), &none), "email");
        assert_eq!(category_for(Some(&app("Xcode", "com.apple.dt.Xcode")), &none), "other");
        assert_eq!(
            category_for(Some(&app("Xcode", "com.apple.dt.Xcode")), &["xcode = Personal"]),
            "personal"
        );
        assert_eq!(
            category_for(Some(&app("Slack", "")), &["slack=bogus", "nope"]),
            "work"
        );
    }
}
