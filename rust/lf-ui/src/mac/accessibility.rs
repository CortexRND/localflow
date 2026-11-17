//! Native AppKit accessibility audits, run by `cargo test -p lf-ui` on macOS.

use std::rc::Rc;
use std::sync::Arc;

use lf_core::config::Config;
use lf_core::dictionary::DictionaryStore;
use lf_core::history::HistoryStore;
use lf_runtime::menubar::{MenuActions, MenuModel};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSButton, NSPopUpButton,
    NSSwitch, NSTextField, NSView,
};
use objc2_foundation::NSString;

use lf_ui::backends::meeting::FakeMeetingClient;
use lf_ui::backends::prompts::FakePromptQueue;
use lf_ui::hub_route::HUB_PAGES;
use lf_ui::mac::hub::Hub;
use lf_ui::mac::page::HubServices;
use lf_ui::mac::setup::{snapshot_fixture, SetupSnapshotState};
use lf_ui::mac::style::HalftoneView;

fn nonempty(text: Option<Retained<NSString>>) -> bool {
    text.is_some_and(|text| !text.to_string().trim().is_empty())
}

fn walk(view: &NSView, path: &str, page: &str, failures: &mut Vec<String>) {
    for (index, subview) in view.subviews().iter().enumerate() {
        let child_path = format!("{path}/{index}");
        if let Ok(button) = subview.clone().downcast::<NSButton>() {
            if !nonempty(Some(button.title())) && !nonempty(button.accessibilityLabel()) {
                failures.push(format!(
                    "{page}: button at {child_path} has no title or label"
                ));
            }
        }
        if let Ok(popup) = subview.clone().downcast::<NSPopUpButton>() {
            if !nonempty(Some(popup.title())) && !nonempty(popup.accessibilityLabel()) {
                failures.push(format!(
                    "{page}: popup at {child_path} has no title or label"
                ));
            }
        }
        if let Ok(switch) = subview.clone().downcast::<NSSwitch>() {
            if !nonempty(switch.accessibilityLabel()) {
                failures.push(format!("{page}: switch at {child_path} has no label"));
            }
        }
        if let Ok(field) = subview.clone().downcast::<NSTextField>() {
            if field.isEditable()
                && !nonempty(field.accessibilityLabel())
                && !nonempty(field.accessibilityTitle())
            {
                failures.push(format!(
                    "{page}: editable text field at {child_path} has no accessibility label or title"
                ));
            }
        }
        walk(&subview, &child_path, page, failures);
    }
}

fn assert_decorations_hidden(view: &NSView, page: &str, failures: &mut Vec<String>) {
    if let Ok(halftone) = view.retain().downcast::<HalftoneView>() {
        if halftone.isAccessibilityElement() {
            failures.push(format!("{page}: HalftoneView is accessibility-visible"));
        }
    }
    for subview in view.subviews().iter() {
        assert_decorations_hidden(&subview, page, failures);
    }
}

fn services(root: &std::path::Path) -> Rc<HubServices> {
    HubServices::new_isolated(
        Config::defaults_for(true),
        Rc::new(
            HistoryStore::open(
                Some(root.join("history.sqlite")),
                Some(root.join("clips")),
                true,
            )
            .expect("temporary history"),
        ),
        Rc::new(DictionaryStore::new(Some(root.join("dictionary.json")))),
        Arc::new(FakePromptQueue::default()),
        Arc::new(FakeMeetingClient::default()),
    )
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("AppKit audit must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    let actions = MenuActions {
        toggle_dictation: Arc::new(|| {}),
        paste_last: Arc::new(|| {}),
        quit: Arc::new(|| {}),
    };
    lf_runtime::macos::install_menu_bar(MenuModel::default(), actions.clone()).unwrap();
    assert_eq!(app.activationPolicy(), NSApplicationActivationPolicy::Accessory);
    lf_ui::mac::app::install_hub_menu_bar(MenuModel::default(), actions).unwrap();
    assert_eq!(app.activationPolicy(), NSApplicationActivationPolicy::Regular);
    println!("Native Hub menu audit: runtime default Accessory, Hub retains Regular/Dock policy");
    let temp = tempfile::tempdir().expect("temporary UI fixtures");
    let hub = Hub::new(mtm, services(temp.path()), "Home");
    let mut failures = Vec::new();
    for page in HUB_PAGES {
        hub.show_page(page);
        assert_eq!(hub.current_page(), page);
        walk(&hub.content_view(), "root", page, &mut failures);
        assert_decorations_hidden(&hub.content_view(), page, &mut failures);
    }
    hub.shutdown();

    let setup = snapshot_fixture(mtm);
    let states = (0..7).map(SetupSnapshotState::Step).chain([
        SetupSnapshotState::ChecksNeedsAction,
        SetupSnapshotState::ChecksAllReady,
        SetupSnapshotState::Complete,
        SetupSnapshotState::CleanupLevelMedium,
        SetupSnapshotState::NonOllamaProvider,
        SetupSnapshotState::DownloadProgress,
        SetupSnapshotState::DownloadError,
    ]);
    for state in states {
        setup.seed_snapshot(state);
        let page = format!("Setup ({})", state.file_stem());
        walk(&setup.content_view(), "root", &page, &mut failures);
        assert_decorations_hidden(&setup.content_view(), &page, &mut failures);
    }
    setup.close();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("Native accessibility audit: all 10 Hub pages and setup states passed");
}
