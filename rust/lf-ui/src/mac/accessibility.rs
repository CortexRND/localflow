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
    NSButton, NSPopUpButton, NSScrollView, NSScrollerStyle, NSSearchField, NSSwitch, NSTextField,
    NSTextView, NSView,
};
use objc2_foundation::{NSRange, NSString};

use lf_ui::backends::ask::FakeAskBackend;
use lf_ui::backends::meeting::FakeMeetingClient;
use lf_ui::backends::prompts::{FakePromptQueue, PromptQueue, UnavailablePromptQueue, WorkPrompt};
use lf_ui::hub_route::HUB_PAGES;
use lf_ui::mac::hub::Hub;
use lf_ui::mac::page::HubServices;
use lf_ui::mac::setup::{snapshot_fixture, SetupSnapshotState};
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

fn assert_setup_workers_and_links(mtm: MainThreadMarker) {
    use lf_ui::mac::setup::{ChecksRunner, SetupContext, SetupServices, SetupWindow};
    use lf_ui::setup_flow::{Check, FakeLaunchAgent, FakeSetupPlatform, HardwareInfo};
    struct Checks;
    impl ChecksRunner for Checks {
        fn run(&self, _config: &Config) -> Result<Vec<Check>, String> {
            assert!(
                MainThreadMarker::new().is_none(),
                "checks must run off main"
            );
            Ok(vec![Check::new(
                "worker",
                "Background check completed",
                Some(true),
                "Worker result",
                "",
                "",
            )])
        }
    }
    let platform = FakeSetupPlatform {
        runtimes: vec!["faster-whisper".into()],
        ..FakeSetupPlatform::default()
    };
    let context = SetupContext::detect(
        Config::defaults_for(true),
        HardwareInfo::new("Darwin", "arm64", 16.0, true, 256.0),
        &platform,
        &FakeLaunchAgent::new("not installed"),
    )
    .unwrap();
    let setup = SetupWindow::new(
        mtm,
        context,
        SetupServices {
            checks: Arc::new(Checks),
            ..SetupServices::default()
        },
    );
    setup.seed_snapshot(SetupSnapshotState::ChecksNeedsAction);
    setup.set_snapshot_mode(false);
    unsafe {
        named_button(&setup.content_view(), "Recheck")
            .unwrap()
            .performClick(None)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while setup.drain_requests() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "setup worker did not finish"
        );
        spin_run_loop(0.01);
    }
    assert!(has_text(
        &setup.content_view(),
        "Background check completed"
    ));
    setup.seed_snapshot(SetupSnapshotState::Step(2));
    assert!(has_text(
        &setup.content_view(),
        if cfg!(feature = "runtime") {
            "GGML"
        } else {
            "Not built"
        }
    ));
    setup.close();
    assert_eq!(setup.drain_requests(), 0);

    let root = tempfile::tempdir().unwrap();
    let services = services(root.path());
    let count = Rc::new(Cell::new(0));
    let opened = count.clone();
    *services.setup_opener.borrow_mut() = Some(Rc::new(move || opened.set(opened.get() + 1)));
    let hub = Hub::new(mtm, services, "Home");
    for page in ["Home", "History"] {
        hub.show_page(page);
        unsafe {
            named_button(&hub.content_view(), "Open setup")
                .unwrap()
                .performClick(None)
        };
        hub.drain_requests();
    }
    assert_eq!(count.get(), 2);
    hub.shutdown();
    println!(
        "Native setup audit: background checks, drained completion and Home/History links passed"
    );
}

fn assert_setup_model_workers(mtm: MainThreadMarker) {
    use lf_ui::mac::setup::{SetupContext, SetupServices, SetupWindow};
    use lf_ui::setup_backend::{ProgressCallback, SetupModelBackend};
    use lf_ui::setup_flow::{FakeLaunchAgent, FakeSetupPlatform, HardwareInfo, SttModel};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Mutex,
    };
    use std::time::{Duration, Instant};
    struct Models {
        entered: mpsc::Sender<Arc<AtomicBool>>,
        release: Mutex<mpsc::Receiver<()>>,
    }
    impl SetupModelBackend for Models {
        fn speech_downloads(
            &self,
            _: &[SttModel],
        ) -> Result<std::collections::HashMap<String, bool>, String> {
            Err("unused".into())
        }
        fn download_speech(
            &self,
            _: &SttModel,
            _: ProgressCallback,
            _: Arc<AtomicBool>,
        ) -> Result<(), String> {
            Err("unused".into())
        }
        fn ollama(&self, _: &str) -> Result<(bool, Vec<String>), String> {
            Err("unused".into())
        }
        fn pull_cleanup(
            &self,
            _: &str,
            _: &str,
            _: ProgressCallback,
            _: Arc<AtomicBool>,
        ) -> Result<(), String> {
            Err("unused".into())
        }
        fn try_dictation(
            &self,
            _: &SttModel,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<String, String> {
            panic!("trial must receive cancellation token")
        }
        fn try_dictation_cancellable(
            &self,
            _: &SttModel,
            _: Option<&str>,
            _: Option<&str>,
            cancel: Arc<AtomicBool>,
        ) -> Result<String, String> {
            assert!(MainThreadMarker::new().is_none(), "trial must run off main");
            self.entered.send(cancel).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            Ok("native worker transcript".into())
        }
    }
    for action in ["complete", "cancel", "skip", "close"] {
        let (entered, ready) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let platform = FakeSetupPlatform {
            runtimes: vec!["faster-whisper".into()],
            ..FakeSetupPlatform::default()
        };
        let context = SetupContext::detect(
            Config::defaults_for(true),
            HardwareInfo::new("Darwin", "arm64", 16.0, true, 256.0),
            &platform,
            &FakeLaunchAgent::new("not installed"),
        )
        .unwrap();
        let setup = SetupWindow::new(
            mtm,
            context,
            SetupServices {
                models: Arc::new(Models {
                    entered,
                    release: Mutex::new(released),
                }),
                ..SetupServices::default()
            },
        );
        setup.seed_snapshot(SetupSnapshotState::Step(5));
        setup.set_snapshot_mode(false);
        unsafe {
            named_button(&setup.content_view(), "Record 5 seconds")
                .unwrap()
                .performClick(None);
        }
        setup.drain_requests();
        let cancel = ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!named_button(&setup.content_view(), "Cancel recording")
            .unwrap()
            .isHidden());
        match action {
            "cancel" => unsafe {
                named_button(&setup.content_view(), "Cancel recording")
                    .unwrap()
                    .performClick(None);
            },
            "skip" => unsafe {
                named_button(&setup.content_view(), "Skip dictation trial")
                    .unwrap()
                    .performClick(None);
            },
            "close" => setup.close(),
            _ => {}
        }
        assert_eq!(cancel.load(Ordering::Acquire), action != "complete");
        release.send(()).unwrap();
        if action == "complete" {
            let deadline = Instant::now() + Duration::from_secs(5);
            while setup.drain_requests() != 0 {
                assert!(Instant::now() < deadline, "trial completion not applied");
                spin_run_loop(0.01);
            }
            assert!(has_text(&setup.content_view(), "native worker transcript"));
        }
        setup.close();
    }
    println!("Native setup model audit: format labels, worker transcript and Cancel/Skip/Close tokens passed");
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
    loop {
        let pending = hub.drain_requests();
        if pending == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{}: {pending} page requests did not finish",
            hub.current_page()
        );
        spin_run_loop(0.02);
    }
    assert!(!has_text(&hub.content_view(), "Loading prompts…"));
    assert!(!has_text(&hub.content_view(), "Loading settings…"));
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

fn assert_prompt_rebuild(mtm: MainThreadMarker) {
    for populated in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let base = services(root.path());
        let queue = Arc::new(FakePromptQueue::new());
        if populated {
            queue
                .add(
                    &[WorkPrompt::new("Completed queue result", "Review this")],
                    &["review.md".into()],
                    "Meeting",
                )
                .unwrap();
        }
        let services = HubServices::new(
            Config::defaults_for(true),
            base.history.clone(),
            base.dictionary.clone(),
            queue.clone(),
            Arc::new(FakeMeetingClient::new()),
            Box::new(FakeAskBackend::default()),
        );
        services.memory_only_config.set(true);
        let hub = Hub::new(mtm, services, "Prompts");
        settle_requests(&hub);
        let expected = if populated {
            "Completed queue result"
        } else {
            "No prompts to review yet"
        };
        assert!(has_text(&hub.content_view(), expected));
        queue.set_read_failure(Some("Rebuild must not reload the queue"));
        hub.rebuild_current_page();
        settle_requests(&hub);
        assert_eq!(hub.drain_requests(), 0);
        assert!(has_text(&hub.content_view(), expected));
        assert!(!has_text(&hub.content_view(), "Can't read the queue"));
        hub.shutdown();
    }
    println!("Native Prompts rebuild audit: completed empty/populated loads survive rebuild without rereading the queue");
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
        settle_requests(hub);
        assert_eq!(
            search_field(&hub.content_view())
                .unwrap()
                .stringValue()
                .to_string(),
            query
        );
        assert!(
            has_text(&hub.content_view(), expected),
            "History query {query:?} did not render {expected:?}"
        );
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
        if page.starts_with("Setup") {
            if let Some(scroll) = subview.downcast_ref::<NSScrollView>() {
                assert_eq!(scroll.scrollerStyle(), NSScrollerStyle::Legacy);
                if scroll.documentView().is_some_and(|document| {
                    document.frame().size.height > scroll.contentSize().height + 1.0
                }) {
                    let scroller = scroll.verticalScroller().expect("overflow scroller");
                    assert!(!scroller.isHidden());
                    assert!(scroller.isEnabled());
                    assert!(scroller.frame().size.width > 0.0);
                    assert!(scroller.alphaValue() > 0.0);
                    assert!(scroller.knobProportion() < 1.0);
                }
            }
        }
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
    assert_prompt_rebuild(mtm);
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
    assert_setup_workers_and_links(mtm);
    assert_setup_model_workers(mtm);
    let setup = snapshot_fixture(mtm);
    for state in (0..7).map(SetupSnapshotState::Step).chain([
        SetupSnapshotState::ChecksNeedsAction,
        SetupSnapshotState::ChecksAllReady,
        SetupSnapshotState::ChecksNeedsActionWide,
        SetupSnapshotState::Complete,
        SetupSnapshotState::CleanupLevelMedium,
        SetupSnapshotState::NonOllamaProvider,
        SetupSnapshotState::DownloadProgress,
        SetupSnapshotState::DownloadError,
    ]) {
        setup.seed_snapshot(state);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while setup.drain_requests() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "setup requests did not finish"
            );
            spin_run_loop(0.01);
        }
        let page = format!("Setup ({})", state.file_stem());
        walk(&setup.content_view(), "root", &page, &mut failures);
        assert_decorations_hidden(&setup.content_view(), &page, &mut failures);
    }
    setup.close();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("Native accessibility audit: all ten Hub pages and setup states passed");
}
