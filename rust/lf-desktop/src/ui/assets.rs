//! Embedded assets: Lucide outline icons (ISC, see assets/icons/LICENSE-lucide.txt)
//! and the LocalFlow brand mark.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        pub const ICONS: &[(&str, &[u8])] = &[
            $(($name, include_bytes!(concat!("../../assets/icons/", $name, ".svg")))),*
        ];
    };
}

icons!(
    "house",
    "history",
    "book-a",
    "zap",
    "pen-line",
    "message-circle-question",
    "users",
    "list-checks",
    "cpu",
    "settings",
    "copy",
    "trash-2",
    "rotate-ccw",
    "search",
    "plus",
    "upload",
    "x",
    "chevron-right",
    "chevron-down",
    "check",
    "undo-2",
    "external-link",
    "folder-open",
    "mic",
    "square",
    "download",
    "key-round",
    "eye",
    "eye-off",
    "circle-check",
    "triangle-alert",
    "circle-alert",
    "info",
    "loader-circle",
    "pencil",
    "refresh-cw",
    "send",
    "arrow-left",
    "keyboard",
    "shield",
    "hard-drive",
    "server",
    "file-text",
    "play",
    "sparkles",
    "circle",
    "user-round",
    "bot",
    "message-square-dot",
    "ban",
    "cloud-off",
    "circle-help",
    "ellipsis",
    "rotate-cw",
);

pub const BRAND: &[u8] = include_bytes!("../../assets/brand.svg");

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == "brand.svg" {
            return Ok(Some(Cow::Borrowed(BRAND)));
        }
        let name = path
            .strip_prefix("icons/")
            .and_then(|p| p.strip_suffix(".svg"));
        Ok(name
            .and_then(|n| ICONS.iter().find(|(k, _)| *k == n))
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .map(|(n, _)| SharedString::from(format!("icons/{n}.svg")))
            .filter(|p| p.starts_with(path))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_used_by_pages_is_embedded() {
        for page in crate::page::Page::ALL
            .into_iter()
            .chain([crate::page::Page::People, crate::page::Page::Agents])
        {
            assert!(
                ICONS.iter().any(|(n, _)| *n == page.icon()),
                "{}",
                page.icon()
            );
        }
        for (_, bytes) in ICONS {
            assert!(std::str::from_utf8(bytes).unwrap().starts_with("<svg"));
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }
}
