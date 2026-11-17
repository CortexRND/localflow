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
    NSButton, NSPopUpButton, NSSearchField, NSSwitch, NSTextField, NSTextView, NSView,
};
use objc2_foundation::{NSRange, NSString};

use lf_ui::backends::ask::FakeAskBackend;
use lf_ui::backends::meeting::FakeMeetingClient;
use lf_ui::backends::prompts::{UnavailablePromptQueue, WorkPrompt};
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

fn editable_field(view: &NSView, label: &str) -> Option<Retained<NSTextField>> {
    if let Ok(field) = view.retain().downcast::<NSTextField>() {
        if field.isEditable()
            && field
                .accessibilityLabel()
                .is_some_and(|value| value.to_string() == label)
        {
            return Some(field);
        }
    }
    view.subviews()
        .iter()
        .find_map(|child| editable_field(&child, label))
}

fn named_button(view: &NSView, label: &str) -> Option<Retained<NSButton>> {
    if let Ok(button) = view.retain().downcast::<NSButton>() {
        if button.title().to_string() == label
            || button
                .accessibilityLabel()
                .is_some_and(|value| value.to_string() == label)
        {
            return Some(button);
        }
    }
    view.subviews()
        .iter()
        .find_map(|child| named_button(&child, label))
}

fn settle_requests(hub: &Hub) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while has_text(&hub.content_view(), "Loading prompts…")
        || has_text(&hub.content_view(), "Loading settings…")
    {
        assert!(
            std::time::Instant::now() < deadline,
            "prompt worker did not finish"
        );
        spin_run_loop(0.02);
    }
}

fn assert_meetings_focus(mtm: MainThreadMarker) {
    fn notes(view: &NSView) -> Option<Retained<NSTextView>> {
        if let Ok(text) = view.retain().downcast::<NSTextView>() {
            if text
                .accessibilityLabel()
                .is_some_and(|label| label.to_string() == "Your notes")
            {
                return Some(text);
            }
        }
        view.subviews().iter().find_map(|child| notes(&child))
    }
    let root = tempfile::tempdir().unwrap();
    let hub = Hub::new(mtm, services(root.path()), "Meetings");
    hub.prepare_snapshot("running", root.path()).unwrap();
    hub.show_page("Meetings");
    hub.show();
    let before = notes(&hub.content_view()).unwrap();
    assert!(hub.window().makeFirstResponder(Some(&before)));
    before.setSelectedRange(NSRange::new(1, 3));
    let text = before.string().to_string();
    hub.show_page("Meetings");
    let after = notes(&hub.content_view()).unwrap();
    assert_eq!(after.string().to_string(), text);
    assert_eq!(after.selectedRange(), NSRange::new(1, 3));
    assert!(hub
        .window()
        .firstResponder()
        .is_some_and(|responder| std::ptr::eq(
            &*responder as *const objc2_app_kit::NSResponder as *const (),
            &*after as *const _ as *const ()
        )));
    for (preparation, label) in [
        ("idle", "Meeting title"),
        ("saved", "Ask about this meeting"),
    ] {
        hub.prepare_snapshot(preparation, root.path()).unwrap();
        hub.show_page("Meetings");
        let before = editable_field(&hub.content_view(), label).unwrap();
        before.setStringValue(&NSString::from_str("Unsent text with 🦀"));
        assert!(hub.window().makeFirstResponder(Some(&before)));
        before
            .currentEditor()
            .unwrap()
            .setSelectedRange(NSRange::new(4, 2));
        hub.show_page("Meetings");
        let after = editable_field(&hub.content_view(), label).unwrap();
        assert_eq!(after.stringValue().to_string(), "Unsent text with 🦀");
        assert_eq!(
            after
                .currentEditor()
                .expect("restored field editor")
                .selectedRange(),
            NSRange::new(4, 2)
        );
    }
    hub.shutdown();
    println!("Native Meetings focus audit: notes/title/question focus and selection survive status-style rebuilds");
}

fn assert_settings_writes(mtm: MainThreadMarker) {
    fn wait_for_sounds(hub: &Hub, services: &HubServices, enabled: bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while services.config().field_bool("sounds_enabled") != Some(enabled) {
            assert!(
                std::time::Instant::now() < deadline,
                "Settings write did not complete"
            );
            spin_run_loop(0.02);
        }
        settle_requests(hub);
    }
    fn request(view: &NSView) -> Option<Retained<NSTextView>> {
        if let Ok(text) = view.retain().downcast::<NSTextView>() {
            if text
                .accessibilityLabel()
                .is_some_and(|label| label.to_string() == lf_ui::hub_settings::HEADING)
            {
                return Some(text);
            }
        }
        view.subviews().iter().find_map(|child| request(&child))
    }
    let root = tempfile::tempdir().unwrap();
    let services = services(root.path());
    let hub = Hub::new(mtm, services.clone(), "Settings");
    hub.prepare_snapshot("preview", root.path()).unwrap();
    hub.show_page("Settings");
    let apply = named_button(&hub.content_view(), "Apply").expect("Apply preview");
    unsafe { apply.performClick(None) };
    wait_for_sounds(&hub, &services, false);
    assert_eq!(services.config().field_bool("sounds_enabled"), Some(false));
    assert_eq!(
        request(&hub.content_view()).unwrap().string().to_string(),
        ""
    );
    assert!(
        named_button(&hub.content_view(), lf_ui::hub_settings::UNDO_TITLE)
            .unwrap()
            .isEnabled()
    );
    hub.resize_content(1280.0, 820.0);
    spin_run_loop(0.1);
    settle_requests(&hub);
    assert_eq!(
        request(&hub.content_view()).unwrap().string().to_string(),
        ""
    );
    let undo = named_button(&hub.content_view(), lf_ui::hub_settings::UNDO_TITLE).unwrap();
    unsafe { undo.performClick(None) };
    wait_for_sounds(&hub, &services, true);
    assert_eq!(services.config().field_bool("sounds_enabled"), Some(true));
    hub.shutdown();
    println!("Native Settings audit: worker Apply/Undo, real undo record and cleared request survive rebuild");
}

fn assert_dictionary_submissions(mtm: MainThreadMarker) {
    let temp = tempfile::tempdir().unwrap();
    let services = services(temp.path());
    let hub = Hub::new(mtm, services.clone(), "Dictionary");
    let field = |label| editable_field(&hub.content_view(), label).expect("Dictionary field");
    let values =
        || ["Word", "When I say", "Type this"].map(|label| field(label).stringValue().to_string());
    let click = |label| {
        let button = named_button(&hub.content_view(), label).expect("Dictionary button");
        unsafe { button.performClick(None) };
        spin_run_loop(0.1);
    };

    field("Word").setStringValue(&NSString::from_str("Kubernetes"));
    field("When I say").setStringValue(&NSString::from_str("k8s"));
    field("Type this").setStringValue(&NSString::from_str("Kubernetes"));
    click("Add word");
    assert_eq!(services.dictionary.list().words, ["Kubernetes"]);
    assert_eq!(values(), ["", "k8s", "Kubernetes"]);
    assert!(services.page_error("Dictionary").is_none());

    click("Add word");
    assert_eq!(
        services.page_error("Dictionary").as_deref(),
        Some("Type a word first.")
    );
    assert_eq!(values(), ["", "k8s", "Kubernetes"]);
    click("Add replacement");
    assert_eq!(values(), ["", "", ""]);
    assert!(services.page_error("Dictionary").is_none());
    let data = services.dictionary.list();
    assert_eq!(data.replacements.len(), 1);
    assert_eq!(data.replacements[0].from, "k8s");
    assert_eq!(data.replacements[0].to, "Kubernetes");
    click("Add replacement");
    assert_eq!(
        services.page_error("Dictionary").as_deref(),
        Some("Type what you say first.")
    );
    assert_eq!(values(), ["", "", ""]);

    click("Edit Kubernetes");
    assert_eq!(values(), ["Kubernetes", "", ""]);
    field("Word").setStringValue(&NSString::from_str("Rust"));
    click("Save word");
    assert_eq!(services.dictionary.list().words, ["Rust"]);
    assert_eq!(values(), ["", "", ""]);
    assert!(named_button(&hub.content_view(), "Add word").is_some());

    click("Edit k8s → Kubernetes");
    field("Word").setStringValue(&NSString::from_str("unsaved word"));
    field("When I say").setStringValue(&NSString::from_str("rs"));
    field("Type this").setStringValue(&NSString::from_str("Rust"));
    click("Save replacement");
    assert_eq!(values(), ["unsaved word", "", ""]);
    let data = services.dictionary.list();
    assert_eq!(data.replacements.len(), 1);
    assert_eq!(data.replacements[0].from, "rs");
    assert_eq!(data.replacements[0].to, "Rust");
    assert!(named_button(&hub.content_view(), "Add replacement").is_some());

    field("Word").setStringValue(&NSString::from_str("Rust"));
    click("Add word");
    assert_eq!(values(), ["Rust", "", ""]);
    assert!(services
        .page_error("Dictionary")
        .unwrap()
        .contains("already in your dictionary"));
    field("When I say").setStringValue(&NSString::from_str("unfinished"));
    click("Add replacement");
    assert_eq!(values(), ["Rust", "unfinished", ""]);
    assert_eq!(
        services.page_error("Dictionary").as_deref(),
        Some("Type what it should become.")
    );
    hub.show_page("History");
    hub.show_page("Dictionary");
    assert_eq!(values(), ["Rust", "unfinished", ""]);
    hub.shutdown();
    println!("Native Dictionary submission audit: add/edit saves clear only the submitted card; empty and duplicate failures preserve drafts");
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
    assert_dictionary_submissions(mtm);
    assert_settings_writes(mtm);
    assert_meetings_focus(mtm);
    let temp = tempfile::tempdir().expect("temporary UI fixtures");
    let hub = Hub::new(mtm, services(temp.path()), "Home");
    let mut failures = Vec::new();
    for page in HUB_PAGES {
        hub.show_page(page);
        settle_requests(&hub);
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
    hub.services().dictionary.add_word("LocalFlow").unwrap();
    hub.services()
        .dictionary
        .add_replacement("teh", "the")
        .unwrap();
    let prompts = hub
        .services()
        .prompt_queue
        .add(
            &[
                WorkPrompt::new("Review the changes", "Check tests"),
                WorkPrompt::new("Update docs", "Document changes"),
            ],
            &["review.md".into(), "docs.md".into()],
            "Planning",
        )
        .unwrap();
    let id = prompts[1].get("id").unwrap().as_str().unwrap();
    hub.services()
        .prompt_queue
        .set_status(
            id,
            "failed",
            None,
            serde_json::json!({"error":"Orca is not running"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
    for page in HUB_PAGES {
        hub.show_page(page);
        settle_requests(&hub);
        walk(&hub.content_view(), "populated", page, &mut failures);
        assert_decorations_hidden(&hub.content_view(), page, &mut failures);
    }
    for scenario in lf_ui::snapshot_plan::SCENARIOS.iter().filter(|s| {
        matches!(
            s.page,
            "Ask" | "Meetings" | "Hot Phrases" | "Models" | "Settings"
        )
    }) {
        let fixture = tempfile::tempdir().unwrap();
        let fixture_hub = Hub::new(mtm, services(fixture.path()), scenario.page);
        fixture_hub
            .prepare_snapshot(scenario.prepare, fixture.path())
            .unwrap();
        fixture_hub.show_page(scenario.page);
        if scenario.page == "Meetings" && matches!(scenario.state, "saved" | "saved-generated") {
            assert!(
                named_button(&fixture_hub.content_view(), "Ask").is_some(),
                "meeting chat must include its Ask button"
            );
        }
        walk(
            &fixture_hub.content_view(),
            scenario.state,
            scenario.page,
            &mut failures,
        );
        assert_decorations_hidden(&fixture_hub.content_view(), scenario.page, &mut failures);
        fixture_hub.shutdown();
    }
    for unavailable in ["Setup", "not a page"] {
        hub.show_page("History");
        hub.show_page(unavailable);
        assert_eq!(hub.current_page(), "Home");
    }
    let production = HubServices::new(
        Config::defaults_for(true),
        hub.services().history.clone(),
        hub.services().dictionary.clone(),
        Arc::new(UnavailablePromptQueue),
        Arc::new(FakeMeetingClient::new()),
        Box::new(FakeAskBackend::default()),
    );
    let unavailable = Hub::new(mtm, production, "Prompts");
    assert!(has_text(&unavailable.content_view(), "Loading prompts…"));
    settle_requests(&unavailable);
    assert!(has_text(
        &unavailable.content_view(),
        "Can't read the queue"
    ));
    assert!(has_text(&unavailable.content_view(), "Toggle Dictation"));
    walk(
        &unavailable.content_view(),
        "unavailable",
        "Prompts",
        &mut failures,
    );
    unavailable.shutdown();
    hub.shutdown();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("Native accessibility audit: all ten ported pages and snapshot states passed");
}
