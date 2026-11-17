//! The Hub window shell (port of the shell half of `HubWindowController` in
//! `localflow/hub_mac.py`): window, sidebar with the ported pages, the scrolling
//! page viewport, `show_page`, full rebuilds on resize / appearance change,
//! the error callout and the close hooks.
//!
//! Ownership: the [`Hub`] owns the window, every view reference it needs, the
//! pages and the delegate / action targets. AppKit keeps targets and delegates
//! *unretained*, so the `Hub` must outlive its window; closures registered on
//! controls and the window delegate only hold `Weak<Hub>`, so no retain cycle
//! exists. Work requested from inside a control action (navigate / rebuild) is
//! deferred to the next run-loop turn so it never re-enters a page that is
//! still borrowed.
#![allow(deprecated)]

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSButton,
    NSScrollView, NSScrollerStyle, NSTextAlignment, NSTextField, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSPoint, NSSize, NSString};

use super::actions::{ActionTarget, Timer};
use super::page::{HubPage, HubServices, Measure, PageCtx, PageHandle};
use super::pages::{
    ask::AskPage, dictionary::DictionaryPage, history::HistoryPage, home::HomePage,
    hot_phrases::HotPhrasesPage, meetings::MeetingsPage, models::ModelsPage, prompts::PromptsPage,
    settings::SettingsPage, style::StylePage,
};
use super::style::{
    boxed_measure, color, hide_from_accessibility, page_title, rect, style_flat_button,
    style_surface, text_measure, tokens_for_window, FlippedView, SurfaceStyle,
};
use super::widgets::{content_bottom, label, remove_subviews, LabelStyle};
use crate::hub_model::{sidebar_label, sidebar_title, SIDEBAR_BUTTON_WIDTH};
use crate::hub_route::{ported_page, validate_page, HUB_PAGES};
use crate::tokens::{
    Palette, Weight, HUB_CONTENT_GUTTER, HUB_CONTENT_TOP, HUB_CONTENT_X, HUB_DEFAULT_HEIGHT,
    HUB_DEFAULT_WIDTH, HUB_HEADER_OFFSET, HUB_MIN_HEIGHT, HUB_MIN_WIDTH, HUB_PAGE_BOTTOM,
    HUB_SIDEBAR_WIDTH,
};

/// Height of the error callout card, and the space it takes (card + gap).
pub const CALLOUT_HEIGHT: f64 = 58.0;
pub const CALLOUT_ADVANCE: f64 = 70.0;
/// Width reserved at the right of the History title for its action button.
pub const HISTORY_TITLE_RESERVE: f64 = 140.0;
/// Pages that show their friendly errors inline instead of the callout.
pub const INLINE_ERROR_PAGES: [&str; 2] = ["Dictionary", "Prompts"];

// ------------------------------------------------------------ pure helpers ---

/// Width pages lay out in: the scroll view minus the right gutter, never less
/// than the width the minimum window leaves (`_content_width`).
pub fn content_width_for(scroll_width: f64) -> f64 {
    (scroll_width - f64::from(HUB_CONTENT_GUTTER)).max(f64::from(
        HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER,
    ))
}

/// Document height: the content plus the bottom margin, at least the viewport
/// (`_fit_body_height`).
pub fn fitted_height(content_bottom: f64, viewport_height: f64) -> f64 {
    (content_bottom + f64::from(HUB_PAGE_BOTTOM)).max(viewport_height)
}

/// The scroll offset to restore after a rebuild: the old one, clamped to what
/// the new document can scroll (`_rebuild_current_page`).
pub fn restored_scroll(old_y: f64, document_height: f64, viewport_height: f64) -> f64 {
    old_y.min((document_height - viewport_height).max(0.0))
}

/// Title width: History leaves room for its right-aligned action.
pub fn title_width(page: &str, width: f64) -> f64 {
    if page == "History" {
        width - HISTORY_TITLE_RESERVE
    } else {
        width
    }
}

/// Whether the shell draws the error callout for `page`.
pub fn shows_callout(page: &str) -> bool {
    !INLINE_ERROR_PAGES.contains(&page)
}

// ----------------------------------------------------------------- delegate ---

struct DelegateState {
    hub: Weak<Hub>,
}

define_class!(
    /// The window delegate. Holds only a `Weak<Hub>`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DelegateState]
    #[name = "LFHubWindowDelegate"]
    struct HubDelegate;

    unsafe impl NSObjectProtocol for HubDelegate {}

    unsafe impl NSWindowDelegate for HubDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            // A valid draft is saved; an invalid one keeps the window open so
            // the user sees why instead of silently losing it.
            self.ivars().hub.upgrade().is_none_or(|hub| hub.can_close())
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            if let Some(hub) = self.ivars().hub.upgrade() {
                hub.shutdown();
            }
        }

        #[unsafe(method(windowDidEndLiveResize:))]
        fn window_did_end_live_resize(&self, _notification: &NSNotification) {
            if let Some(hub) = self.ivars().hub.upgrade() {
                hub.rebuild_current_page();
            }
        }
    }

    impl HubDelegate {
        // Not part of the `NSWindowDelegate` protocol in objc2: AppKit sends it
        // to delegates that respond to it, so it is declared on the class.
        #[unsafe(method(windowDidChangeEffectiveAppearance:))]
        fn window_did_change_effective_appearance(&self, _notification: &NSNotification) {
            if let Some(hub) = self.ivars().hub.upgrade() {
                hub.rebuild_current_page();
            }
        }
    }
);

impl HubDelegate {
    fn new(mtm: MainThreadMarker, hub: Weak<Hub>) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(DelegateState { hub });
        // SAFETY: plain `init` on NSObject.
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------- hub ---

struct HubState {
    page: String,
    palette: Palette,
    /// A page has been rendered at least once (so there is state to capture).
    rendered: bool,
    closed: bool,
}

/// Resets the busy flag even if a page panics while rendering.
struct BusyGuard<'a>(&'a Cell<bool>);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// The Hub window.
pub struct Hub {
    mtm: MainThreadMarker,
    services: Rc<HubServices>,
    window: Retained<NSWindow>,
    root: Retained<FlippedView>,
    sidebar: Retained<FlippedView>,
    brand: Retained<NSTextField>,
    mark: Retained<NSView>,
    sidebar_buttons: Vec<Retained<NSButton>>,
    scroll: Retained<NSScrollView>,
    body: Retained<FlippedView>,
    /// Target of the page controls; cleared on every rebuild.
    page_target: Retained<ActionTarget>,
    /// Target of the sidebar buttons; lives as long as the Hub.
    _shell_target: Retained<ActionTarget>,
    delegate: Retained<HubDelegate>,
    pages: Vec<(&'static str, PageHandle)>,
    measure: Measure,
    state: RefCell<HubState>,
    busy: Cell<bool>,
    /// Pending zero-delay timers (navigate / rebuild requested by pages).
    deferred: RefCell<Vec<Timer>>,
    me: Weak<Hub>,
}

fn set_layer_background(view: &NSView, token: &str, palette: &Palette) {
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        let tint = color(token, palette);
        layer.setBackgroundColor(Some(&tint.CGColor()));
    }
}

fn make_pages() -> Vec<(&'static str, PageHandle)> {
    HUB_PAGES
        .iter()
        .map(|&name| {
            let page: PageHandle = match name {
                "Ask" => Rc::new(RefCell::new(AskPage::new())),
                "Meetings" => Rc::new(RefCell::new(MeetingsPage::new())),
                "Hot Phrases" => Rc::new(RefCell::new(HotPhrasesPage::new())),
                "Home" => Rc::new(RefCell::new(HomePage::new())),
                "History" => Rc::new(RefCell::new(HistoryPage::new())),
                "Dictionary" => Rc::new(RefCell::new(DictionaryPage::new())),
                "Prompts" => Rc::new(RefCell::new(PromptsPage::new())),
                "Style" => Rc::new(RefCell::new(StylePage::new())),
                "Models" => Rc::new(RefCell::new(ModelsPage::new())),
                "Settings" => Rc::new(RefCell::new(SettingsPage::new())),
                _ => unreachable!("HUB_PAGES contains only implemented pages"),
            };
            (name, page)
        })
        .collect()
}

impl Hub {
    /// Build the window with all ten sidebar pages and show `initial_page`
    /// (an unknown name falls back to Home). The window is created but not
    /// ordered front: call [`Hub::show`].
    pub fn new(mtm: MainThreadMarker, services: Rc<HubServices>, initial_page: &str) -> Rc<Hub> {
        let hub = Rc::new_cyclic(|me: &Weak<Hub>| Hub::build(mtm, services, me.clone()));
        hub.show_page(ported_page(Some(initial_page)));
        hub
    }

    fn build(mtm: MainThreadMarker, services: Rc<HubServices>, me: Weak<Hub>) -> Hub {
        let measure: Measure = Rc::from(boxed_measure());
        let delegate = HubDelegate::new(mtm, me.clone());

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // SAFETY: a plain window init on the main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                rect(
                    0.0,
                    0.0,
                    f64::from(HUB_DEFAULT_WIDTH),
                    f64::from(HUB_DEFAULT_HEIGHT),
                ),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the `Hub` owns the window through `Retained`; AppKit must not
        // also release it when it closes.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("LocalFlow Hub"));
        window.setContentMinSize(NSSize::new(
            f64::from(HUB_MIN_WIDTH),
            f64::from(HUB_MIN_HEIGHT),
        ));
        window.center();
        let palette = tokens_for_window(&window);

        let root = FlippedView::new(
            mtm,
            rect(
                0.0,
                0.0,
                f64::from(HUB_DEFAULT_WIDTH),
                f64::from(HUB_DEFAULT_HEIGHT),
            ),
        );
        set_layer_background(&root, "paper", &palette);
        let root_view: &NSView = &root;
        window.setContentView(Some(root_view));

        // The sidebar keeps its width and follows the window height.
        let sidebar = FlippedView::new(
            mtm,
            rect(
                0.0,
                0.0,
                f64::from(HUB_SIDEBAR_WIDTH),
                f64::from(HUB_DEFAULT_HEIGHT),
            ),
        );
        sidebar.setAutoresizingMask(NSAutoresizingMaskOptions::ViewHeightSizable);
        set_layer_background(&sidebar, "sidebar", &palette);
        root.addSubview(&sidebar);

        let brand = label(
            mtm,
            "localflow",
            42.0,
            25.0,
            145.0,
            LabelStyle {
                size: 17.0,
                color: "sidebar_text",
                height: 26.0,
                ..LabelStyle::default()
            },
            &palette,
        );
        sidebar.addSubview(&brand);
        // The sun mark is decoration only.
        let mark = NSView::initWithFrame(mtm.alloc::<NSView>(), rect(22.0, 31.0, 10.0, 10.0));
        set_layer_background(&mark, "sun", &palette);
        if let Some(layer) = mark.layer() {
            layer.setCornerRadius(5.0);
        }
        hide_from_accessibility(&mark);
        sidebar.addSubview(&mark);

        // Ten real NSButtons; titles use the real 13pt semibold measure and
        // fall back to the page id when a label would not fit.
        let shell_target = ActionTarget::new(mtm);
        let mut sidebar_buttons = Vec::with_capacity(HUB_PAGES.len());
        let measure_fn = text_measure();
        for (index, page) in HUB_PAGES.iter().enumerate() {
            // SAFETY: no target/action yet; `on_action` installs them below.
            let button = unsafe {
                NSButton::buttonWithTitle_target_action(&NSString::from_str(page), None, None, mtm)
            };
            button.setFrame(rect(16.0, 76.0 + index as f64 * 44.0, 168.0, 36.0));
            button.setTitle(&NSString::from_str(&sidebar_title(
                page,
                Some(&measure_fn),
                SIDEBAR_BUTTON_WIDTH,
            )));
            button.setAccessibilityLabel(Some(&NSString::from_str(sidebar_label(page))));
            button.setAlignment(NSTextAlignment::Left);
            button.setTag(index as isize);
            style_flat_button(&button, false, &palette);
            let weak = me.clone();
            let name: &'static str = page;
            shell_target.on_action(&button, move || {
                if let Some(hub) = weak.upgrade() {
                    hub.sidebar_clicked(name);
                }
            });
            sidebar.addSubview(&button);
            sidebar_buttons.push(button);
        }

        // Pages are laid out in `body`, the document view of a scroll view that
        // tracks the window: pages use the current body width, and anything
        // taller than the window scrolls.
        let scroll = NSScrollView::initWithFrame(
            mtm.alloc::<NSScrollView>(),
            rect(
                f64::from(HUB_CONTENT_X),
                f64::from(HUB_CONTENT_TOP),
                f64::from(HUB_DEFAULT_WIDTH - HUB_CONTENT_X - HUB_CONTENT_GUTTER),
                f64::from(HUB_DEFAULT_HEIGHT - HUB_CONTENT_TOP),
            ),
        );
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setScrollerStyle(NSScrollerStyle::Legacy);
        scroll.setDrawsBackground(false);
        let scroll_frame = scroll.frame();
        let body = FlippedView::new(
            mtm,
            rect(
                0.0,
                0.0,
                content_width_for(scroll_frame.size.width),
                scroll_frame.size.height,
            ),
        );
        body.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        let body_view: &NSView = &body;
        scroll.setDocumentView(Some(body_view));
        root.addSubview(&scroll);
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

        Hub {
            mtm,
            services,
            window,
            root,
            sidebar,
            brand,
            mark,
            sidebar_buttons,
            scroll,
            body,
            page_target: ActionTarget::new(mtm),
            _shell_target: shell_target,
            delegate,
            pages: make_pages(),
            measure,
            state: RefCell::new(HubState {
                page: "Home".to_string(),
                palette,
                rendered: false,
                closed: false,
            }),
            busy: Cell::new(false),
            deferred: RefCell::new(Vec::new()),
            me,
        }
    }

    // ------------------------------------------------------------ accessors ---

    pub fn window(&self) -> &NSWindow {
        &self.window
    }

    /// The window's content (root) view.
    pub fn content_view(&self) -> Retained<NSView> {
        Retained::into_super(self.root.clone())
    }

    /// The page currently shown.
    pub fn current_page(&self) -> String {
        self.state.borrow().page.clone()
    }

    /// The palette the current render used.
    pub fn palette(&self) -> Palette {
        self.state.borrow().palette
    }

    pub fn services(&self) -> &Rc<HubServices> {
        &self.services
    }

    /// The page canvas (document view of the scroll viewport).
    pub fn body(&self) -> &FlippedView {
        &self.body
    }

    pub fn is_closed(&self) -> bool {
        self.state.borrow().closed
    }

    /// Prepare the current page through its real state API (used by snapshots).
    pub fn prepare_snapshot(
        &self,
        preparation: &str,
        root: &std::path::Path,
    ) -> anyhow::Result<()> {
        let page = self.current_page();
        self.with_page(&page, |page| page.prepare_snapshot(preparation, root))
            .unwrap_or_else(|| anyhow::bail!("page {page} is already borrowed"))
    }

    /// The window delegate object (kept alive by the Hub).
    pub fn delegate_object(&self) -> &NSObject {
        &self.delegate
    }

    /// Order the window front, make it key and activate the app.
    pub fn show(&self) {
        self.window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(self.mtm).activateIgnoringOtherApps(true);
    }

    // ----------------------------------------------------------- geometry ---

    fn content_width(&self) -> f64 {
        content_width_for(self.scroll.frame().size.width)
    }

    fn viewport_height(&self) -> f64 {
        self.scroll.frame().size.height
    }

    fn fit_body_height(&self, page_bottom: f64) {
        let bottom = content_bottom(&self.body).max(page_bottom);
        let frame = self.body.frame();
        self.body.setFrame(rect(
            0.0,
            0.0,
            frame.size.width,
            fitted_height(bottom, self.viewport_height()),
        ));
    }

    // -------------------------------------------------------------- pages ---

    fn page_handle(&self, name: &str) -> Option<PageHandle> {
        self.pages
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, p)| p.clone())
    }

    /// Run `f` on the page unless it is already borrowed (a re-entrant call).
    fn with_page<R>(&self, name: &str, f: impl FnOnce(&mut dyn HubPage) -> R) -> Option<R> {
        let handle = self.page_handle(name)?;
        let mut page = handle.try_borrow_mut().ok()?;
        Some(f(&mut *page))
    }

    /// Whether the window may close: the current page's veto.
    pub fn can_close(&self) -> bool {
        let page = self.current_page();
        self.with_page(&page, |p| p.can_leave()).unwrap_or(true)
    }

    /// Write whatever every page was still holding (debounced saves).
    pub fn pages_flush(&self) {
        for (name, _) in &self.pages {
            let _ = catch_unwind(AssertUnwindSafe(|| self.with_page(name, |p| p.flush())));
        }
    }

    /// Run `work` on the next run-loop turn, from a `Weak<Hub>`.
    fn defer(&self, work: impl Fn(&Hub) + 'static) {
        let weak = self.me.clone();
        let mut timers = self.deferred.borrow_mut();
        timers.retain(Timer::is_valid);
        timers.push(Timer::schedule(0.0, false, move || {
            if let Some(hub) = weak.upgrade() {
                work(&hub);
            }
        }));
    }

    fn sidebar_clicked(&self, page: &'static str) {
        // Navigating away and back starts clean: a stale error from an earlier
        // visit must not greet the user again.
        self.services.clear_page_error(page);
        self.services.page_notices.borrow_mut().remove(page);
        self.show_page(page);
    }

    // --------------------------------------------------------- navigation ---

    /// The single navigation entry point (sidebar, menu bar, CLI, page
    /// requests). Leaving a page whose draft is invalid keeps the user there;
    /// re-showing the current page (resize, appearance) is a rebuild, never a
    /// navigation commit. Unavailable page names fall back to Home.
    pub fn show_page(&self, page: &str) {
        let requested = ported_page(Some(page));
        if self.busy.replace(true) {
            // Called while a render is running: do it right after.
            let page = requested.to_string();
            self.defer(move |hub| hub.show_page(&page));
            return;
        }
        let _guard = BusyGuard(&self.busy);
        self.render_page(requested);
    }

    fn render_page(&self, requested: &'static str) {
        let (current, rendered) = {
            let state = self.state.borrow();
            (state.page.clone(), state.rendered)
        };
        let mut page = requested;
        if rendered && current != requested {
            let allowed = self.with_page(&current, |p| p.can_leave()).unwrap_or(true);
            if !allowed {
                page = validate_page(&current).unwrap_or(requested);
            }
        }
        if rendered {
            // Read live controls into the page state before they are removed;
            // leaving the page also writes its debounced saves.
            self.with_page(&current, |p| p.capture());
            if current != page {
                self.with_page(&current, |p| p.flush());
            }
        }

        // The palette is re-read on every render (dark mode).
        let palette = tokens_for_window(&self.window);
        {
            let mut state = self.state.borrow_mut();
            state.page = page.to_string();
            state.palette = palette;
            state.rendered = true;
        }
        self.restyle_shell(&palette, page);

        self.page_target.clear();
        remove_subviews(&self.body);
        let width = self.content_width();
        self.body
            .setFrame(rect(0.0, 0.0, width, self.viewport_height()));
        // Every page shares the setup display title.
        self.body.addSubview(&page_title(
            self.mtm,
            sidebar_label(page),
            title_width(page, width),
            &palette,
        ));
        let mut cursor = f64::from(HUB_HEADER_OFFSET);
        if shows_callout(page) {
            if let Some(error) = self.services.page_error(page) {
                self.callout(&error, cursor, width, &palette);
                cursor += CALLOUT_ADVANCE;
            }
        }

        let ctx = self.page_ctx(width, palette);
        let bottom = self
            .with_page(page, |p| p.render(&ctx, cursor))
            .unwrap_or(cursor);
        self.fit_body_height(bottom);
    }

    fn page_ctx(&self, width: f64, palette: Palette) -> PageCtx {
        let navigate_weak = self.me.clone();
        let rebuild_weak = self.me.clone();
        PageCtx {
            mtm: self.mtm,
            body: self.body.clone(),
            width,
            viewport_height: self.viewport_height(),
            palette,
            services: self.services.clone(),
            target: self.page_target.clone(),
            measure: self.measure.clone(),
            // Called from inside control actions while the page is borrowed:
            // always deferred, never synchronous.
            navigate: Rc::new(move |page: &str| {
                if let Some(hub) = navigate_weak.upgrade() {
                    let page = page.to_string();
                    hub.defer(move |hub| hub.show_page(&page));
                }
            }),
            rebuild: Rc::new(move || {
                if let Some(hub) = rebuild_weak.upgrade() {
                    hub.defer(|hub| hub.rebuild_current_page());
                }
            }),
        }
    }

    /// Re-apply the palette to the shell's own views.
    fn restyle_shell(&self, palette: &Palette, page: &str) {
        set_layer_background(&self.root, "paper", palette);
        set_layer_background(&self.sidebar, "sidebar", palette);
        set_layer_background(&self.mark, "sun", palette);
        self.brand
            .setTextColor(Some(&color("sidebar_text", palette)));
        for (button, name) in self.sidebar_buttons.iter().zip(HUB_PAGES.iter()) {
            style_flat_button(button, *name == page, palette);
        }
    }

    /// The red-barred "Something went wrong" card (`_callout`).
    fn callout(&self, message: &str, y: f64, width: f64, palette: &Palette) {
        let mtm = self.mtm;
        let card = FlippedView::new(mtm, rect(0.0, y, width, CALLOUT_HEIGHT));
        style_surface(&card, palette, &SurfaceStyle::default());
        let bar = NSView::initWithFrame(mtm.alloc::<NSView>(), rect(0.0, 0.0, 4.0, CALLOUT_HEIGHT));
        set_layer_background(&bar, "red", palette);
        hide_from_accessibility(&bar);
        card.addSubview(&bar);
        card.addSubview(&label(
            mtm,
            "Something went wrong",
            16.0,
            8.0,
            width - 40.0,
            LabelStyle {
                size: 13.0,
                color: "red",
                height: 18.0,
                ..LabelStyle::default()
            },
            palette,
        ));
        card.addSubview(&label(
            mtm,
            message,
            16.0,
            30.0,
            width - 40.0,
            LabelStyle {
                size: 11.0,
                color: "muted",
                height: 18.0,
                weight: Weight::Regular,
                wrap: false,
            },
            palette,
        ));
        self.body.addSubview(&card);
    }

    /// Re-lay out the current page, keeping drafts (through the pages'
    /// `capture`/`render`) and the scroll position.
    pub fn rebuild_current_page(&self) {
        if self.busy.get() {
            self.defer(|hub| hub.rebuild_current_page());
            return;
        }
        let page = self.current_page();
        let scroll_y = self.scroll.contentView().bounds().origin.y;
        self.show_page(&page);
        self.scroll_to(scroll_y);
    }

    fn drain_deferred(&self) {
        let timers = std::mem::take(&mut *self.deferred.borrow_mut());
        for timer in timers {
            if timer.is_valid() {
                timer.fire();
                timer.invalidate();
            }
        }
    }

    /// Deliver request results and their deferred UI work on the main thread.
    /// Count pending work after rendering, including requests started by it.
    pub fn drain_requests(&self) -> usize {
        self.drain_deferred();
        let current = self.current_page();
        let mut repaint = false;
        for (name, _) in &self.pages {
            repaint |= self
                .with_page(name, |page| {
                    let before = page.pending_requests();
                    let after = page.drain_requests();
                    *name == current && before != after
                })
                .unwrap_or(false);
        }
        if repaint {
            self.rebuild_current_page();
        }
        self.drain_deferred();
        let pending: usize = self
            .pages
            .iter()
            .map(|(name, _)| {
                self.with_page(name, |page| page.pending_requests())
                    .unwrap_or(1)
            })
            .sum();
        pending
            + self
                .deferred
                .borrow()
                .iter()
                .filter(|timer| timer.is_valid())
                .count()
    }

    /// Scroll the page canvas, clamped to the rendered document's range.
    pub fn scroll_to(&self, y: f64) {
        let y = restored_scroll(y, self.body.frame().size.height, self.viewport_height());
        self.body.scrollPoint(NSPoint::new(0.0, y));
    }

    /// Resize the window's content (at least the Hub minimum) and re-lay out
    /// (used by snapshots).
    pub fn resize_content(&self, width: f64, height: f64) {
        self.window.setContentSize(NSSize::new(
            width.max(f64::from(HUB_MIN_WIDTH)),
            height.max(f64::from(HUB_MIN_HEIGHT)),
        ));
        self.rebuild_current_page();
    }

    /// Flush every page and stop timers (window close). Runs once, however
    /// often AppKit says the window is closing; failures are swallowed.
    pub fn shutdown(&self) {
        {
            let mut state = self.state.borrow_mut();
            if state.closed {
                return;
            }
            state.closed = true;
        }
        for (name, _) in &self.pages {
            let _ = catch_unwind(AssertUnwindSafe(|| self.with_page(name, |p| p.shutdown())));
        }
        // The run loop retains timers and timers retain closures: stop them so
        // a closed Hub is released instead of lingering.
        let timers = std::mem::take(&mut *self.deferred.borrow_mut());
        for timer in timers {
            timer.invalidate();
        }
        self.page_target.clear();
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // AppKit holds the delegate and targets unretained: detach them before
        // they are released.
        self.window.setDelegate(None);
        self.page_target.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_width_never_goes_below_the_minimum_window_layout() {
        let minimum = f64::from(HUB_MIN_WIDTH - HUB_CONTENT_X - 2 * HUB_CONTENT_GUTTER);
        assert_eq!(content_width_for(0.0), minimum);
        assert_eq!(
            content_width_for(1000.0),
            1000.0 - f64::from(HUB_CONTENT_GUTTER)
        );
    }

    #[test]
    fn body_grows_to_its_content_but_never_shrinks_below_the_viewport() {
        assert_eq!(fitted_height(100.0, 600.0), 600.0);
        assert_eq!(
            fitted_height(900.0, 600.0),
            900.0 + f64::from(HUB_PAGE_BOTTOM)
        );
    }

    #[test]
    fn scroll_offset_is_clamped_after_a_rebuild() {
        assert_eq!(restored_scroll(300.0, 1000.0, 600.0), 300.0);
        assert_eq!(restored_scroll(900.0, 1000.0, 600.0), 400.0);
        assert_eq!(restored_scroll(50.0, 400.0, 600.0), 0.0);
    }

    #[test]
    fn history_title_leaves_room_for_its_action() {
        assert_eq!(title_width("History", 800.0), 660.0);
        assert_eq!(title_width("Home", 800.0), 800.0);
    }

    #[test]
    fn dictionary_and_prompts_show_errors_inline() {
        assert!(!shows_callout("Dictionary"));
        assert!(!shows_callout("Prompts"));
        assert!(shows_callout("History"));
    }
}
