//! Style page (port of `_page_style`, `_segment_group`, `_segment_button`,
//! `_update_segment_buttons`, `selectCleanupLevel_`, `selectStyle_` and
//! `_style_autosave` of `localflow/hub_mac.py`).
//!
//! A cleanup-level segmented control and one segmented control per writing
//! category. There is no Save button: every click restyles its segment in place
//! and is written straight away through the one config write path
//! ([`HubServices::update_config`]). Python has no debounce timer here either,
//! so none is added; instead [`HubPage::flush`] retries a selection whose save
//! failed (the pending selections stay visible and the next click saves them
//! all again, exactly like Python).
//!
//! A failure is stored as the page error (`services.page_errors["Style"]`),
//! which the shell shows as the callout under the title, and the page is
//! rebuilt with the pending selections still selected.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2_app_kit::{NSAccessibility, NSButton};
use objc2_foundation::NSString;

use crate::hub_model::{
    style_category_label, style_option_title, style_row_frames, style_save_error_text,
    style_update, STYLE_AUTOSAVE_HINT, STYLE_CATEGORIES, STYLE_CLEANUP_OPTIONS,
    STYLE_CLEANUP_TITLES, STYLE_OPTIONS,
};
use crate::mac::page::{HubPage, HubServices, PageCtx};
use crate::mac::style::{rect, segmented, FlippedView};
use crate::mac::widgets::{label, LabelStyle};
use crate::tokens::{Palette, Weight};

const PAGE: &str = "Style";

/// One segmented control: its group view, its buttons and the option values.
struct Segments {
    group: Retained<FlippedView>,
    buttons: Vec<Retained<NSButton>>,
}

#[derive(Default)]
struct State {
    /// Selected cleanup level (a [`STYLE_CLEANUP_OPTIONS`] value).
    cleanup: String,
    /// `(category, selected style)` in [`STYLE_CATEGORIES`] order.
    styles: Vec<(String, String)>,
    /// True while selections exist that were not saved (a failed write): they
    /// win over the config when the page is rebuilt.
    dirty: bool,
    cleanup_segments: Option<Segments>,
    style_segments: Vec<Segments>,
    palette: Option<Palette>,
    services: Option<Rc<HubServices>>,
}

type Shared = Rc<RefCell<State>>;

/// The Style page.
#[derive(Default)]
pub struct StylePage {
    state: Shared,
}

impl StylePage {
    pub fn new() -> Self {
        StylePage::default()
    }
}

/// Index of `value` in `options` (0 when unknown, which a validated config
/// cannot produce).
fn index_of(options: &[&str], value: &str) -> usize {
    options
        .iter()
        .position(|option| *option == value)
        .unwrap_or(0)
}

/// `_style_autosave`: write the selections. Returns whether it saved. On
/// failure the page error is set and `dirty` stays true; the caller rebuilds.
fn persist(state: &RefCell<State>, services: &HubServices) -> bool {
    let (cleanup, styles) = {
        let state = state.borrow();
        (state.cleanup.clone(), state.styles.clone())
    };
    let pairs: Vec<(&str, &str)> = styles
        .iter()
        .map(|(c, v)| (c.as_str(), v.as_str()))
        .collect();
    match services.update_config(&style_update(&cleanup, &pairs)) {
        Ok(_) => {
            state.borrow_mut().dirty = false;
            true
        }
        Err(error) => {
            services.set_page_error(PAGE, &style_save_error_text(&error));
            state.borrow_mut().dirty = true;
            false
        }
    }
}

/// A click on a segment: restyle in place, then save. A failure, or clearing
/// an earlier error, rebuilds the page (to show or drop the callout).
fn select(
    state: &Shared,
    services: &HubServices,
    rebuild: &dyn Fn(),
    category: Option<usize>,
    index: usize,
) {
    let restyle = {
        let mut state = state.borrow_mut();
        let Some(palette) = state.palette else { return };
        match category {
            None => {
                state.cleanup = STYLE_CLEANUP_OPTIONS[index].to_string();
                state
                    .cleanup_segments
                    .as_ref()
                    .map(|s| (s.group.clone(), s.buttons.clone(), palette))
            }
            Some(row) => {
                state.styles[row].1 = STYLE_OPTIONS[index].to_string();
                state
                    .style_segments
                    .get(row)
                    .map(|s| (s.group.clone(), s.buttons.clone(), palette))
            }
        }
    };
    if let Some((group, buttons, palette)) = restyle {
        segmented(&group, &buttons, index, &palette);
    }
    let saved = persist(state, services);
    // A failure shows its callout; only an earlier error forces a rebuild on
    // success (to drop its callout), a normal click keeps the in-place update.
    let cleared_error = saved && services.page_errors.borrow_mut().remove(PAGE).is_some();
    if !saved || cleared_error {
        rebuild();
    }
}

/// `_segment_group` + `_segment_button` + `_segmented`: one native button per
/// option inside a rounded group; returns the group.
#[allow(clippy::too_many_arguments)]
fn segment_group(
    ctx: &PageCtx,
    weak: &Weak<RefCell<State>>,
    y: f64,
    width: f64,
    titles: &[String],
    group_label: &str,
    selected: usize,
    category: Option<usize>,
) -> Segments {
    let group = FlippedView::new(ctx.mtm, rect(180.0, y, width, 36.0));
    ctx.body.addSubview(&group);
    let option_width = (width / titles.len() as f64).floor();
    let mut buttons = Vec::with_capacity(titles.len());
    for (index, title) in titles.iter().enumerate() {
        // SAFETY: no target/action yet; `on_action` installs them below.
        let button = unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, ctx.mtm)
        };
        button.setFrame(rect(index as f64 * option_width, 1.0, option_width, 34.0));
        button.setBordered(false);
        button.setAccessibilityLabel(Some(&NSString::from_str(&format!(
            "{group_label}: {title}"
        ))));
        let weak = weak.clone();
        let services = ctx.services.clone();
        let rebuild = ctx.rebuild.clone();
        ctx.target.on_action(&button, move || {
            if let Some(state) = weak.upgrade() {
                select(&state, &services, &*rebuild, category, index);
            }
        });
        group.addSubview(&button);
        buttons.push(button);
    }
    segmented(&group, &buttons, selected, &ctx.palette);
    Segments { group, buttons }
}

impl HubPage for StylePage {
    fn name(&self) -> &'static str {
        PAGE
    }

    // Nothing to capture: every click already updated the page state.

    fn flush(&mut self) {
        // Retry a selection whose save failed, so leaving the page or closing
        // the Hub does not silently drop it. Success clears the error.
        let (dirty, services) = {
            let state = self.state.borrow();
            (state.dirty, state.services.clone())
        };
        if let (true, Some(services)) = (dirty, services) {
            if persist(&self.state, &services) {
                services.clear_page_error(PAGE);
            }
        }
    }

    fn release_views(&mut self) {
        let mut state = self.state.borrow_mut();
        state.cleanup_segments = None;
        state.style_segments.clear();
    }

    fn shutdown(&mut self) {
        self.flush();
        self.release_views();
        self.state.borrow_mut().services = None;
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        let p = ctx.palette;
        let mut cursor = top;
        let weak: Weak<RefCell<State>> = Rc::downgrade(&self.state);

        // Selections: the pending (unsaved) ones when a save failed, else the
        // committed config (so a config change elsewhere shows up).
        {
            let config = ctx.services.config();
            let mut state = self.state.borrow_mut();
            state.palette = Some(p);
            state.services = Some(ctx.services.clone());
            if !state.dirty || state.styles.is_empty() {
                state.cleanup = config
                    .field_str("cleanup_level")
                    .unwrap_or("light")
                    .to_string();
                state.styles = STYLE_CATEGORIES
                    .iter()
                    .map(|(category, _)| {
                        let value = config
                            .field_str(&format!("style_{category}"))
                            .unwrap_or(STYLE_OPTIONS[0])
                            .to_string();
                        ((*category).to_string(), value)
                    })
                    .collect();
            }
        }
        let (cleanup, styles) = {
            let state = self.state.borrow();
            (state.cleanup.clone(), state.styles.clone())
        };

        ctx.body.addSubview(&label(
            ctx.mtm,
            "Cleanup level",
            0.0,
            cursor + 8.0,
            150.0,
            LabelStyle {
                size: 15.0,
                height: 22.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &p,
        ));
        // Segmented groups take the room left of the 180pt label column, capped
        // so wide windows do not stretch the controls.
        let group_width = 560.0_f64.min(ctx.width - 180.0);
        let cleanup_width = 330.0_f64.min(group_width);
        let cleanup_titles: Vec<String> = STYLE_CLEANUP_TITLES
            .iter()
            .map(|t| (*t).to_string())
            .collect();
        let cleanup_segments = segment_group(
            ctx,
            &weak,
            cursor,
            cleanup_width,
            &cleanup_titles,
            "Cleanup level",
            index_of(&STYLE_CLEANUP_OPTIONS, &cleanup),
            None,
        );
        cursor += 58.0;

        let option_titles: Vec<String> = STYLE_OPTIONS
            .iter()
            .map(|o| style_option_title(o))
            .collect();
        let mut style_segments = Vec::with_capacity(STYLE_CATEGORIES.len());
        for (row, (category, example)) in STYLE_CATEGORIES.iter().enumerate() {
            let frames = style_row_frames(cursor as i32, group_width as i32);
            let (lx, ly, lw, lh) = frames.label;
            let row_label = style_category_label(category);
            ctx.body.addSubview(&label(
                ctx.mtm,
                &row_label,
                f64::from(lx),
                f64::from(ly),
                f64::from(lw),
                LabelStyle {
                    size: 15.0,
                    height: f64::from(lh),
                    weight: Weight::Semibold,
                    ..LabelStyle::default()
                },
                &p,
            ));
            // The example wraps onto a second line inside the row instead of
            // being cut off in the 160pt label column.
            let (ex, ey, ew, eh) = frames.example;
            ctx.body.addSubview(&label(
                ctx.mtm,
                example,
                f64::from(ex),
                f64::from(ey),
                f64::from(ew),
                LabelStyle {
                    size: 11.0,
                    color: "muted",
                    height: f64::from(eh),
                    wrap: true,
                    ..LabelStyle::default()
                },
                &p,
            ));
            let selected = index_of(&STYLE_OPTIONS, &styles[row].1);
            style_segments.push(segment_group(
                ctx,
                &weak,
                cursor,
                group_width,
                &option_titles,
                &row_label,
                selected,
                Some(row),
            ));
            cursor += 86.0;
        }
        {
            let mut state = self.state.borrow_mut();
            state.cleanup_segments = Some(cleanup_segments);
            state.style_segments = style_segments;
        }
        // No Save button: every click is saved right away (see `select`); a
        // failure is shown in the callout under the title.
        ctx.body.addSubview(&label(
            ctx.mtm,
            STYLE_AUTOSAVE_HINT,
            0.0,
            cursor,
            ctx.width,
            LabelStyle {
                size: 12.0,
                color: "muted",
                height: 20.0,
                ..LabelStyle::default()
            },
            &p,
        ));
        cursor + 20.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_core::config::{load_from, save_config, Config};
    use lf_core::dictionary::DictionaryStore;
    use lf_core::history::HistoryStore;

    #[test]
    fn offscreen_failed_selection_retries_on_shutdown_after_storage_recovers() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let mut config = Config::defaults_for(true);
        config.set_field("cleanup_level", "light".into());
        config.set_field("style_work", "formal".into());
        save_config(&config, Some(&path)).unwrap();
        let services = HubServices::new_isolated(
            config,
            Rc::new(
                HistoryStore::open(
                    Some(root.path().join("history.sqlite")),
                    Some(root.path().join("clips")),
                    true,
                )
                .unwrap(),
            ),
            Rc::new(DictionaryStore::new(Some(
                root.path().join("dictionary.json"),
            ))),
        );
        services.memory_only_config.set(false);
        *services.config_path.borrow_mut() = Some(path.clone());
        let mut page = StylePage::new();
        {
            let mut state = page.state.borrow_mut();
            state.cleanup = "medium".into();
            state.styles = vec![("work".into(), "casual".into())];
            state.services = Some(services.clone());
        }
        let original_permissions = std::fs::metadata(root.path()).unwrap().permissions();
        let mut read_only = original_permissions.clone();
        read_only.set_readonly(true);
        std::fs::set_permissions(root.path(), read_only).unwrap();
        let saved = persist(&page.state, &services);
        page.flush();
        page.release_views();
        std::fs::set_permissions(root.path(), original_permissions).unwrap();
        assert!(!saved);
        assert!(page.state.borrow().dirty);
        assert!(services.page_error(PAGE).is_some());
        assert_eq!(
            load_from(&path).unwrap().field_str("cleanup_level"),
            Some("light")
        );
        assert_eq!(
            load_from(&path).unwrap().field_str("style_work"),
            Some("formal")
        );
        page.shutdown();
        assert!(!page.state.borrow().dirty);
        assert!(services.page_error(PAGE).is_none());
        let saved = load_from(&path).unwrap();
        assert_eq!(saved.field_str("cleanup_level"), Some("medium"));
        assert_eq!(saved.field_str("style_work"), Some("casual"));
    }
}
