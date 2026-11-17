//! Native AppKit accessibility audits, run by `cargo test -p lf-ui` on macOS.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use lf_core::config::Config;
use lf_core::dictionary::DictionaryStore;
use lf_core::history::{HistoryStore, NewEntry};
use lf_runtime::menubar::{MenuActions, MenuModel};
use objc2::rc::Retained;
use objc2::{msg_send, MainThreadMarker, Message};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSApplicationTerminateReply,
    NSButton, NSPopUpButton, NSSearchField, NSSwitch, NSTextField, NSView,
};
use objc2_foundation::NSString;

use lf_ui::hub_route::HUB_PAGES;
use lf_ui::mac::hub::Hub;
use lf_ui::mac::page::HubServices;
use lf_ui::mac::snapshot::spin_run_loop;
use lf_ui::mac::style::HalftoneView;

fn search_field(view: &NSView) -> Option<Retained<NSSearchField>> {
    if let Ok(field) = view.retain().downcast::<NSSearchField>() {
        return Some(field);
    }
    view.subviews()
        .iter()
        .find_map(|child| search_field(&child))
}

fn has_text(view: &NSView, text: &str) -> bool {
    if let Ok(field) = view.retain().downcast::<NSTextField>() {
        if field.stringValue().to_string() == text {
            return true;
        }
    }
    view.subviews().iter().any(|child| has_text(&child, text))
}

fn assert_history_search(hub: &Hub) {
    hub.show_page("History");
    for (query, expected) in [
        ("polished", "A polished transcript."),
        ("not in the transcript", "No matching entries"),
        ("", "A polished transcript."),
    ] {
        let field = search_field(&hub.content_view()).expect("History search field");
        assert!(
            field.sendsWholeSearchString(),
            "typing must not rebuild the field"
        );
        field.setStringValue(&NSString::from_str(query));
        assert!(unsafe { field.sendAction_to(field.action(), field.target().as_deref()) });
        spin_run_loop(0.1);
        assert_eq!(
            search_field(&hub.content_view())
                .unwrap()
                .stringValue()
                .to_string(),
            query
        );
        assert!(has_text(&hub.content_view(), expected));
    }
    println!(
        "Native History search audit: explicit submit, matching, empty and cleared queries passed"
    );
}

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
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Accessory
    );
    lf_ui::mac::app::install_hub_menu_bar(MenuModel::default(), actions).unwrap();
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Regular
    );
    println!("Native Hub menu audit: runtime default Accessory, Hub retains Regular/Dock policy");
    let terminate_calls = Rc::new(Cell::new(0));
    let count = terminate_calls.clone();
    let delegate = lf_ui::mac::app::AppDelegate::new_with_termination(
        mtm,
        Rc::new(|| {}),
        Some(Rc::new(move || count.set(count.get() + 1))),
    );
    for _ in 0..2 {
        let reply: NSApplicationTerminateReply =
            unsafe { msg_send![&*delegate, applicationShouldTerminate: &*app] };
        assert_eq!(reply, NSApplicationTerminateReply::TerminateNow);
    }
    assert_eq!(terminate_calls.get(), 1);
    println!("Native Hub termination audit: AppKit quit invokes the flush callback once");
    let temp = tempfile::tempdir().expect("temporary UI fixtures");
    let hub = Hub::new(mtm, services(temp.path()), "Home");
    let mut failures = Vec::new();
    for page in HUB_PAGES {
        hub.show_page(page);
        assert_eq!(hub.current_page(), page);
        walk(&hub.content_view(), "root", page, &mut failures);
        assert_decorations_hidden(&hub.content_view(), page, &mut failures);
    }
    let mut entry = NewEntry::new("dictate");
    entry.raw_text = "A raw transcript".into();
    entry.final_text = "A polished transcript.".into();
    entry.audio_path = Some(
        hub.services()
            .history
            .write_clip(&[0.0; 1600], 16000)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    hub.services().history.add(&entry).unwrap();
    assert_history_search(&hub);
    for page in HUB_PAGES {
        hub.show_page(page);
        walk(&hub.content_view(), "populated", page, &mut failures);
        assert_decorations_hidden(&hub.content_view(), page, &mut failures);
    }
    for unavailable in [
        "Ask",
        "Dictionary",
        "Prompts",
        "Meetings",
        "Style",
        "Models",
        "Settings",
        "Hot Phrases",
    ] {
        hub.show_page("History");
        hub.show_page(unavailable);
        assert_eq!(hub.current_page(), "Home");
    }
    hub.shutdown();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("Native accessibility audit: Home, History and shell passed");
}
