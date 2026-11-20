//! Window, shell (sidebar + page host), toasts, confirm dialogs and the
//! global keyboard map.

use std::rc::Rc;
use std::time::{Duration, SystemTime};

use gpui::{
    actions, div, prelude::*, px, size, AnyElement, App, Application, Bounds, Context, Entity,
    FocusHandle, Focusable, FontWeight, KeyBinding, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};

use crate::data::Services;
use crate::page::{Page, NAV_FOOTER, NAV_GROUPS};
use crate::pages;
use crate::remote::Health;
use crate::theme::{size as tsize, space, text, theme, Theme, Tone};
use crate::ui::{self, assets::Assets, Button};

actions!(
    localflow,
    [
        GoHome,
        Go1,
        Go2,
        Go3,
        Go4,
        Go5,
        Go6,
        Go7,
        Go8,
        Go9,
        Go10,
        FocusSearch,
        NewItem,
        OpenSettings,
        Quit,
        CloseWindow,
        FocusNext,
        FocusPrev,
        Escape,
        NavUp,
        NavDown,
    ]
);

/// What `main` hands to the app.
pub struct Launch {
    pub services: Services,
    pub page: Page,
    pub setup: bool,
    pub notice: Option<String>,
}

pub struct Toast {
    pub message: SharedString,
    pub action: Option<(SharedString, ui::Action)>,
    generation: u64,
}

pub struct Confirm {
    pub title: SharedString,
    pub body: SharedString,
    pub verb: SharedString,
    pub destructive: bool,
    pub on_confirm: ui::Action,
}

/// Shared by every page: services plus a handle to the shell.
///
/// Every call that changes the shell is deferred to the end of the current
/// effect cycle, so pages may call these from any callback (including ones the
/// shell itself runs, such as confirm dialogs and toast actions) without
/// re-entering an entity that is already being updated.
#[derive(Clone)]
pub struct PageCx {
    pub services: Rc<Services>,
    pub shell: gpui::WeakEntity<Shell>,
    /// Last known local-service health (written by the shell).
    pub status: Rc<std::cell::RefCell<Health>>,
    /// The page on screen (written by the shell); lets pages check this
    /// without reading the shell entity, which may be mid-update.
    pub current_page: Rc<std::cell::RefCell<Page>>,
}

impl PageCx {
    fn with_shell(&self, cx: &mut App, f: impl FnOnce(&mut Shell, &mut Context<Shell>) + 'static) {
        let shell = self.shell.clone();
        cx.defer(move |cx| {
            let _ = shell.update(cx, f);
        });
    }
    pub fn navigate(&self, page: Page, target: Option<&'static str>, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| shell.navigate(page, target, cx));
    }
    pub fn open_history(&self, id: i64, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| shell.open_history_entry(id, cx));
    }
    pub fn toast(&self, message: impl Into<SharedString>, cx: &mut App) {
        let message = message.into();
        self.with_shell(cx, move |shell, cx| shell.toast(message, None, cx));
    }
    pub fn toast_with(
        &self,
        message: impl Into<SharedString>,
        action: impl Into<SharedString>,
        f: impl Fn(&mut Window, &mut App) + 'static,
        cx: &mut App,
    ) {
        let message = message.into();
        let action = Some((action.into(), Rc::new(f) as ui::Action));
        self.with_shell(cx, move |shell, cx| shell.toast(message, action, cx));
    }
    pub fn confirm(&self, confirm: Confirm, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| {
            shell.confirm = Some(confirm);
            shell.dialog_armed = false;
            cx.notify();
        });
    }
    /// Config changed: repaint the shell (status row, badges).
    pub fn config_changed(&self, cx: &mut App) {
        self.with_shell(cx, |_, cx| cx.notify());
    }
    pub fn health(&self, _: &App) -> Health {
        self.status.borrow().clone()
    }
    pub fn recheck_health(&self, cx: &mut App) {
        self.with_shell(cx, |shell, cx| shell.check_health(cx));
    }
    pub fn set_pending_prompts(&self, n: usize, cx: &mut App) {
        self.with_shell(cx, move |shell, cx| {
            if shell.pending_prompts != n {
                shell.pending_prompts = n;
                cx.notify();
            }
        });
    }
}

pub enum View {
    Home(Entity<pages::home::HomePage>),
    History(Entity<pages::history::HistoryPage>),
    Dictionary(Entity<pages::dictionary::DictionaryPage>),
    Snippets(Entity<pages::snippets::SnippetsPage>),
    Style(Entity<pages::style::StylePage>),
    Ask(Entity<pages::ask::AskPage>),
    Meetings(Entity<pages::meetings::MeetingsPage>),
    Prompts(Entity<pages::prompts::PromptsPage>),
    Models(Entity<pages::models::ModelsPage>),
    Settings(Entity<pages::settings::SettingsPage>),
}

pub struct Shell {
    pub cx: PageCx,
    pub page: Page,
    views: Vec<(Page, View)>,
    pub health: Health,
    pub pending_prompts: usize,
    toast: Option<Toast>,
    toast_gen: u64,
    pub confirm: Option<Confirm>,
    focus: FocusHandle,
    nav_focus: FocusHandle,
    /// Dialog buttons (focus is moved and trapped between them).
    cancel_focus: FocusHandle,
    ok_focus: FocusHandle,
    /// Where focus was before the dialog opened; restored on close.
    restore_focus: Option<FocusHandle>,
    /// The dialog's initial focus has been scheduled.
    dialog_armed: bool,
    /// The user chose to close or quit despite unsaved changes.
    force_close: bool,
    config_mtime: Option<SystemTime>,
    setup: bool,
}

impl Focusable for Shell {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Shell {
    fn new(launch: Launch, cx: &mut Context<Self>) -> Self {
        let services = Rc::new(launch.services);
        let initial = if services.is_sandbox() {
            Health::Simulated
        } else {
            Health::Unknown
        };
        let page_cx = PageCx {
            services: services.clone(),
            shell: cx.weak_entity(),
            status: Rc::new(std::cell::RefCell::new(initial.clone())),
            current_page: Rc::new(std::cell::RefCell::new(launch.page)),
        };
        let config_mtime = mtime(&services.root.config_path);
        let mut shell = Shell {
            cx: page_cx,
            page: launch.page,
            views: Vec::new(),
            health: if services.is_sandbox() {
                Health::Simulated
            } else {
                Health::Unknown
            },
            pending_prompts: 0,
            toast: None,
            toast_gen: 0,
            confirm: None,
            focus: cx.focus_handle(),
            nav_focus: cx.focus_handle(),
            cancel_focus: cx.focus_handle().tab_stop(true),
            ok_focus: cx.focus_handle().tab_stop(true),
            restore_focus: None,
            dialog_armed: false,
            force_close: false,
            config_mtime,
            setup: launch.setup,
        };
        if let Some(notice) = launch.notice {
            shell.toast(notice.into(), None, cx);
        }
        shell.check_health(cx);
        shell.start_timers(cx);
        shell
    }

    fn start_timers(&mut self, cx: &mut Context<Self>) {
        // Service health every 5 s; config file changes every 1.5 s.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(1500))
                .await;
            let Ok(()) = this.update(cx, |shell, cx| shell.watch_config(cx)) else {
                break;
            };
        })
        .detach();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let Ok(()) = this.update(cx, |shell, cx| shell.check_health(cx)) else {
                break;
            };
        })
        .detach();
    }

    fn watch_config(&mut self, cx: &mut Context<Self>) {
        let now = mtime(&self.cx.services.root.config_path);
        if now != self.config_mtime {
            self.config_mtime = now;
            self.cx.services.reload_config();
            self.refresh_views(cx);
            cx.notify();
        }
    }

    /// Re-read config-derived state on every created page.
    pub fn refresh_views(&mut self, cx: &mut Context<Self>) {
        for (_, view) in &self.views {
            match view {
                View::Home(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::History(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Dictionary(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Snippets(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Style(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Ask(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Meetings(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Prompts(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Models(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Settings(v) => v.update(cx, |p, cx| p.refresh(cx)),
            }
        }
    }

    pub fn check_health(&mut self, cx: &mut Context<Self>) {
        let remote = self.cx.services.remote.clone();
        let config = self.cx.services.config();
        cx.spawn(async move |this, cx| {
            let health = cx
                .background_executor()
                .spawn(async move { remote.health(&config) })
                .await;
            let _ = this.update(cx, |shell, cx| {
                if shell.health != health {
                    *shell.cx.status.borrow_mut() = health.clone();
                    shell.health = health;
                    shell.refresh_views(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn navigate(&mut self, page: Page, target: Option<&'static str>, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            return; // a dialog is open: no navigation behind it
        }
        let leaving = self.page;
        if leaving == Page::Snippets && page != Page::Snippets {
            if let Some(View::Snippets(v)) = self.find(Page::Snippets) {
                let ok = v.update(cx, |p, cx| p.leave(cx));
                if !ok {
                    self.toast(
                        "Fix the snippet before leaving; it couldn't be saved.".into(),
                        None,
                        cx,
                    );
                    return;
                }
            }
        }
        if leaving == Page::Models && page != Page::Models {
            if let Some(View::Models(v)) = self.find(Page::Models) {
                v.update(cx, |p, cx| p.clear_secret(cx));
            }
        }
        self.page = page;
        *self.cx.current_page.borrow_mut() = page;
        self.ensure_view(page, cx);
        if let Some(target) = target {
            self.deep_link(page, target, cx);
        }
        if let Some(view) = self.find(page) {
            match view {
                View::Home(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::History(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Dictionary(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Snippets(v) => v.update(cx, |p, cx| p.show(cx)),
                View::Style(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Ask(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Meetings(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Prompts(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Models(v) => v.update(cx, |p, cx| p.refresh(cx)),
                View::Settings(v) => v.update(cx, |p, cx| p.refresh(cx)),
            }
        }
        cx.notify();
    }

    fn deep_link(&mut self, page: Page, target: &str, cx: &mut Context<Self>) {
        match self.find(page) {
            Some(View::Settings(v)) => v.update(cx, |p, cx| p.open_tab(target, cx)),
            Some(View::Models(v)) => v.update(cx, |p, cx| p.open_tab(target, cx)),
            Some(View::History(v)) => {
                if let Ok(id) = target.parse::<i64>() {
                    v.update(cx, |p, cx| p.select_id(id, cx));
                }
            }
            _ => {}
        }
    }

    /// Open History on a specific entry.
    pub fn open_history_entry(&mut self, id: i64, cx: &mut Context<Self>) {
        self.navigate(Page::History, None, cx);
        if let Some(View::History(v)) = self.find(Page::History) {
            v.update(cx, |p, cx| p.select_id(id, cx));
        }
    }

    fn find(&self, page: Page) -> Option<&View> {
        self.views.iter().find(|(p, _)| *p == page).map(|(_, v)| v)
    }

    fn ensure_view(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.find(page).is_some() {
            return;
        }
        let pcx = self.cx.clone();
        let setup = self.setup;
        let view = match page {
            Page::Home => View::Home(cx.new(|cx| pages::home::HomePage::new(pcx, setup, cx))),
            Page::History => View::History(cx.new(|cx| pages::history::HistoryPage::new(pcx, cx))),
            Page::Dictionary => {
                View::Dictionary(cx.new(|cx| pages::dictionary::DictionaryPage::new(pcx, cx)))
            }
            Page::Snippets => {
                View::Snippets(cx.new(|cx| pages::snippets::SnippetsPage::new(pcx, cx)))
            }
            Page::Style => View::Style(cx.new(|cx| pages::style::StylePage::new(pcx, cx))),
            Page::Ask => View::Ask(cx.new(|cx| pages::ask::AskPage::new(pcx, cx))),
            Page::Meetings => {
                View::Meetings(cx.new(|cx| pages::meetings::MeetingsPage::new(pcx, cx)))
            }
            Page::Prompts => View::Prompts(cx.new(|cx| pages::prompts::PromptsPage::new(pcx, cx))),
            Page::Models => View::Models(cx.new(|cx| pages::models::ModelsPage::new(pcx, cx))),
            Page::Settings => {
                View::Settings(cx.new(|cx| pages::settings::SettingsPage::new(pcx, cx)))
            }
        };
        self.views.push((page, view));
    }

    pub fn toast(
        &mut self,
        message: SharedString,
        action: Option<(SharedString, ui::Action)>,
        cx: &mut Context<Self>,
    ) {
        self.toast_gen += 1;
        let generation = self.toast_gen;
        self.toast = Some(Toast {
            message,
            action,
            generation,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(6)).await;
            let _ = this.update(cx, |shell, cx| {
                if shell
                    .toast
                    .as_ref()
                    .is_some_and(|t| t.generation == generation)
                {
                    shell.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn go_index(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            return;
        }
        if let Some(page) = Page::sidebar_order().get(index).copied() {
            self.navigate(page, None, cx);
        }
    }

    fn page_element(&self) -> AnyElement {
        match self.find(self.page) {
            Some(View::Home(v)) => v.clone().into_any_element(),
            Some(View::History(v)) => v.clone().into_any_element(),
            Some(View::Dictionary(v)) => v.clone().into_any_element(),
            Some(View::Snippets(v)) => v.clone().into_any_element(),
            Some(View::Style(v)) => v.clone().into_any_element(),
            Some(View::Ask(v)) => v.clone().into_any_element(),
            Some(View::Meetings(v)) => v.clone().into_any_element(),
            Some(View::Prompts(v)) => v.clone().into_any_element(),
            Some(View::Models(v)) => v.clone().into_any_element(),
            Some(View::Settings(v)) => v.clone().into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            return;
        }
        match self.find(self.page) {
            Some(View::History(v)) => v.update(cx, |p, cx| p.focus_search(window, cx)),
            Some(View::Snippets(v)) => v.update(cx, |p, cx| p.focus_search(window, cx)),
            Some(View::Dictionary(v)) => v.update(cx, |p, cx| p.focus_search(window, cx)),
            _ => {}
        }
    }

    fn new_item(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            return;
        }
        match self.find(self.page) {
            Some(View::Snippets(v)) => v.update(cx, |p, cx| p.new_snippet(window, cx)),
            Some(View::Dictionary(v)) => v.update(cx, |p, cx| p.focus_new_word(window, cx)),
            Some(View::Ask(v)) => v.update(cx, |p, cx| p.new_question(window, cx)),
            _ => {}
        }
    }

    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            // Cancel: the action is never run.
            self.close_dialog(false, window, cx);
            return;
        }
        if self.toast.take().is_some() {
            cx.notify();
            return;
        }
        match self.find(self.page) {
            Some(View::History(v)) => v.update(cx, |p, cx| p.clear_search(window, cx)),
            _ => cx.propagate(),
        }
    }

    fn nav_step(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_some() {
            return;
        }
        if !self.nav_focus.contains_focused(window, cx) {
            cx.propagate();
            return;
        }
        let order = Page::sidebar_order();
        let i = order.iter().position(|p| *p == self.page).unwrap_or(0) as i32;
        let n = order.len() as i32;
        let next = order[((i + delta).rem_euclid(n)) as usize];
        self.navigate(next, None, cx);
    }

    // ---------------------------------------------------------------- views ---

    fn render_sidebar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let services = &self.cx.services;
        let config = services.config();
        let nav_item = |page: Page, cx: &mut Context<Self>| {
            let selected = self.page == page;
            let (sz, lh) = text::body();
            let badge = (page == Page::Prompts && self.pending_prompts > 0)
                .then(|| ui::badge(t, self.pending_prompts));
            ui::row_button(t, ("nav", page as u64), selected)
                .h(tsize::control_md())
                .items_center()
                .gap(space::s5())
                .px(space::s5())
                .rounded(tsize::radius_element())
                .text_size(sz)
                .line_height(lh)
                .text_color(if selected { t.text } else { t.text_secondary })
                .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                .on_click(cx.listener(move |this, _, _, cx| this.navigate(page, None, cx)))
                .when(selected, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(8.))
                            .w(px(2.))
                            .h(px(16.))
                            .rounded_full()
                            .bg(t.accent),
                    )
                })
                .child(ui::icon(
                    page.icon(),
                    if selected {
                        t.icon_accent
                    } else {
                        t.icon_secondary
                    },
                ))
                .child(div().flex_1().child(page.label()))
                .children(badge)
        };
        let mut groups = div().flex().flex_col().gap(space::s1()).px(space::s4());
        for (label, pages) in NAV_GROUPS {
            if let Some(label) = label {
                groups = groups.child(
                    ui::supporting(t, label)
                        .font_weight(FontWeight::MEDIUM)
                        .px(space::s5())
                        .pt(space::s6())
                        .pb(space::s2()),
                );
            }
            for page in pages {
                groups = groups.child(nav_item(*page, cx));
            }
        }
        let mut footer = div()
            .flex()
            .flex_col()
            .gap(space::s1())
            .px(space::s4())
            .pb(space::s4());
        for page in NAV_FOOTER {
            footer = footer.child(nav_item(page, cx));
        }
        // Status row: dictation config and service reachability, shown on
        // every page. It never claims a runtime is listening.
        let status_view = crate::controllers::status::describe(
            services.is_sandbox(),
            config.field_bool("dictation_enabled").unwrap_or(true),
            &lf_ui::hub_model::format_hotkey(config.field_str("hotkey")),
            &self.health,
            &services.remote.base_url(&config),
            cfg!(target_os = "macos"),
        );
        let (level, line1, line2) = status_view.sidebar;
        let tone = match level {
            crate::controllers::status::Level::Ok => Tone::Success,
            crate::controllers::status::Level::Warn => Tone::Warning,
            crate::controllers::status::Level::Neutral => Tone::Neutral,
        };
        let status = ui::row_button(t, "status-row", false)
            .flex_col()
            .gap(space::s1())
            .px(space::s6())
            .py(space::s5())
            .on_click(cx.listener(|this, _, _, cx| this.navigate(Page::Home, None, cx)))
            .child(ui::status_dot(t, tone, line1).font_weight(FontWeight::MEDIUM))
            .when(!line2.is_empty(), |d| {
                d.child(ui::supporting(t, line2).pl(px(20.)))
            });

        div()
            .id("sidebar")
            .track_focus(&self.nav_focus)
            .flex()
            .flex_col()
            .flex_none()
            .w(px(tsize::SIDEBAR))
            .h_full()
            .bg(t.bg_body)
            .border_r_1()
            .border_color(t.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s5())
                    .h(px(56.))
                    .px(space::s6())
                    .child(ui::brand(t))
                    .child(ui::label(t, "LocalFlow").font_weight(FontWeight::SEMIBOLD)),
            )
            .child(
                div()
                    .id("nav-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(groups),
            )
            .child(footer)
            .child(ui::divider(t))
            .child(status)
    }

    fn render_toast(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let toast = self.toast.as_ref()?;
        let (sz, lh) = text::body();
        let action = toast.action.clone();
        Some(
            div()
                .absolute()
                .bottom(space::s8())
                .left(px(0.))
                .right(px(0.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("toast")
                        .occlude()
                        .flex()
                        .items_center()
                        .gap(space::s6())
                        .px(space::s6())
                        .py(space::s4())
                        .rounded(tsize::radius_element())
                        .bg(t.bg_inverted)
                        .text_color(t.text_inverted)
                        .text_size(sz)
                        .line_height(lh)
                        .shadow_lg()
                        .max_w(px(560.))
                        .child(toast.message.clone())
                        .when_some(action, |d, (label, f)| {
                            let ring = ui::ring(t);
                            d.child(
                                div()
                                    .id("toast-action")
                                    .tab_index(0)
                                    .cursor_pointer()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(t.accent_muted)
                                    .focus(move |s| s.shadow(ring.clone()))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.toast = None;
                                        f(window, cx);
                                        cx.notify();
                                    }))
                                    .child(label),
                            )
                        })
                        .child(ui::IconButton::new("toast-close", "x", "Dismiss").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.toast = None;
                                cx.notify();
                            }),
                        )),
                ),
        )
    }

    /// Flush every page's dirty state and wait (bounded) for the writes.
    pub fn prepare_close(
        &mut self,
        cx: &mut Context<Self>,
    ) -> crate::controllers::shutdown::CloseCheck {
        use crate::controllers::shutdown;
        let mut failures = Vec::new();
        let threads = shutdown::failures();
        for (_, view) in &self.views {
            match view {
                View::Snippets(v) => {
                    if let Err(e) = v.update(cx, |p, cx| p.flush_for_close(cx)) {
                        failures.push(e);
                    }
                }
                View::Meetings(v) => {
                    let threads = threads.clone();
                    v.update(cx, |p, cx| p.flush_for_close(threads, cx));
                }
                _ => {}
            }
        }
        shutdown::finish(failures, &threads, shutdown::CLOSE_WAIT)
    }

    /// `true` when the window may close now. Otherwise a dialog explains what
    /// wasn't saved and offers to close anyway.
    pub fn should_close(
        &mut self,
        quit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.force_close {
            return true;
        }
        let check = self.prepare_close(cx);
        if check.ok() {
            self.force_close = true;
            return true;
        }
        let shell = cx.weak_entity();
        self.confirm = Some(Confirm {
            title: "Some changes weren't saved".into(),
            body: format!(
                "{} Keep the window open to fix this, or close anyway and lose them.",
                check.message()
            )
            .into(),
            verb: if quit { "Quit anyway" } else { "Close anyway" }.into(),
            destructive: true,
            on_confirm: Rc::new(move |window, cx| {
                // Runs inside the shell's own listener: defer the shell update.
                let handle = window.window_handle();
                let shell = shell.clone();
                cx.defer(move |cx| {
                    let _ = shell.update(cx, |s, _| s.force_close = true);
                    if quit {
                        cx.quit();
                    } else {
                        let _ = handle.update(cx, |_, window, _| window.remove_window());
                    }
                });
            }),
        });
        self.dialog_armed = false;
        window.refresh();
        cx.notify();
        false
    }

    /// Ctrl+Q / Ctrl+W.
    fn request_close(&mut self, quit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.should_close(quit, window, cx) {
            if quit {
                cx.quit();
            } else {
                window.remove_window();
            }
        }
    }

    /// Close the dialog, restore focus, then run the action when confirmed.
    fn close_dialog(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        self.dialog_armed = false;
        match self.restore_focus.take() {
            Some(handle) => window.focus(&handle),
            None => window.focus(&self.focus),
        }
        if run {
            (confirm.on_confirm)(window, cx);
        }
        cx.notify();
    }

    /// Tab / Shift-Tab: cycle inside an open dialog, else move normally.
    fn tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirm.is_none() {
            if forward {
                window.focus_next();
            } else {
                window.focus_prev();
            }
            return;
        }
        let order = [self.cancel_focus.clone(), self.ok_focus.clone()];
        let current = order.iter().position(|h| h.is_focused(window));
        let next = trap_step(current, order.len(), forward);
        window.focus(&order[next]);
        cx.notify();
    }

    fn render_confirm(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let confirm = self.confirm.as_ref()?;
        let verb = Button::new("confirm-ok", confirm.verb.clone())
            .track_focus(&self.ok_focus)
            .variant(if confirm.destructive {
                ui::Variant::Destructive
            } else {
                ui::Variant::Primary
            })
            .on_click(cx.listener(|this, _, window, cx| this.close_dialog(true, window, cx)));
        Some(
            div()
                .id("scrim")
                .occlude()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(t.scrim)
                .child(
                    div()
                        .id("dialog")
                        .occlude()
                        .flex()
                        .flex_col()
                        .gap(space::s5())
                        .w(px(440.))
                        .p(space::s8())
                        .rounded(tsize::radius_container())
                        .bg(t.bg_popover)
                        .shadow_lg()
                        .child(ui::heading(t, confirm.title.clone()))
                        .child(ui::secondary(t, confirm.body.clone()))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(space::s4())
                                .pt(space::s4())
                                .child(
                                    Button::new("confirm-cancel", "Cancel")
                                        .track_focus(&self.cancel_focus)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_dialog(false, window, cx)
                                        })),
                                )
                                .child(verb),
                        ),
                ),
        )
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        if self.confirm.is_some() && !self.dialog_armed {
            // Move focus into the dialog (on Cancel) once it is on screen.
            self.dialog_armed = true;
            cx.on_next_frame(window, |this, window, cx| {
                if this.confirm.is_some() {
                    let current = window.focused(cx);
                    let inside = current
                        .as_ref()
                        .is_some_and(|h| *h == this.cancel_focus || *h == this.ok_focus);
                    if !inside {
                        this.restore_focus = current;
                    }
                    window.focus(&this.cancel_focus);
                    cx.notify();
                }
            });
        }
        self.ensure_view(self.page, cx);
        let sandbox = self.cx.services.is_sandbox();
        let sidebar = self.render_sidebar(&t, cx);
        let page = self.page_element();
        let toast = self.render_toast(&t, cx);
        let confirm = self.render_confirm(&t, cx);
        let (sz, lh) = text::body();
        div()
            .id("shell")
            .key_context("Shell")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Go1, _, cx| this.go_index(0, cx)))
            .on_action(cx.listener(|this, _: &Go2, _, cx| this.go_index(1, cx)))
            .on_action(cx.listener(|this, _: &Go3, _, cx| this.go_index(2, cx)))
            .on_action(cx.listener(|this, _: &Go4, _, cx| this.go_index(3, cx)))
            .on_action(cx.listener(|this, _: &Go5, _, cx| this.go_index(4, cx)))
            .on_action(cx.listener(|this, _: &Go6, _, cx| this.go_index(5, cx)))
            .on_action(cx.listener(|this, _: &Go7, _, cx| this.go_index(6, cx)))
            .on_action(cx.listener(|this, _: &Go8, _, cx| this.go_index(7, cx)))
            .on_action(cx.listener(|this, _: &Go9, _, cx| this.go_index(8, cx)))
            .on_action(cx.listener(|this, _: &Go10, _, cx| this.go_index(9, cx)))
            .on_action(cx.listener(|this, _: &GoHome, _, cx| this.go_index(0, cx)))
            .on_action(
                cx.listener(|this, _: &OpenSettings, _, cx| {
                    this.navigate(Page::Settings, None, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &FocusSearch, w, cx| this.focus_search(w, cx)))
            .on_action(cx.listener(|this, _: &NewItem, w, cx| this.new_item(w, cx)))
            .on_action(cx.listener(|this, _: &Escape, w, cx| this.escape(w, cx)))
            .on_action(cx.listener(|this, _: &NavUp, w, cx| this.nav_step(-1, w, cx)))
            .on_action(cx.listener(|this, _: &NavDown, w, cx| this.nav_step(1, w, cx)))
            .on_action(cx.listener(|this, _: &FocusNext, window, cx| this.tab(true, window, cx)))
            .on_action(cx.listener(|this, _: &FocusPrev, window, cx| this.tab(false, window, cx)))
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                this.request_close(false, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &Quit, window, cx| this.request_close(true, window, cx)),
            )
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .font_family(t.font_family.clone())
            .text_size(sz)
            .line_height(lh)
            .text_color(t.text)
            .bg(t.bg_surface)
            .when(sandbox, |d| {
                let dir = self
                    .cx
                    .services
                    .root
                    .sandbox_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                d.child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(space::s4())
                        .h(px(28.))
                        .px(space::s6())
                        .bg(t.sandbox_bg)
                        .text_color(t.sandbox_text)
                        .text_size(text::supporting().0)
                        .child(ui::icon("triangle-alert", t.sandbox_text).size(px(14.)))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Sandbox — example data."),
                        )
                        .child(format!(
                            "Nothing here touches your real LocalFlow data. Stored in {dir}"
                        )),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(div().flex_1().min_w_0().h_full().child(page)),
            )
            .children(toast)
            .children(confirm)
    }
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-1", Go1, None),
        KeyBinding::new("secondary-2", Go2, None),
        KeyBinding::new("secondary-3", Go3, None),
        KeyBinding::new("secondary-4", Go4, None),
        KeyBinding::new("secondary-5", Go5, None),
        KeyBinding::new("secondary-6", Go6, None),
        KeyBinding::new("secondary-7", Go7, None),
        KeyBinding::new("secondary-8", Go8, None),
        KeyBinding::new("secondary-9", Go9, None),
        KeyBinding::new("secondary-0", Go10, None),
        KeyBinding::new("secondary-k", FocusSearch, None),
        KeyBinding::new("secondary-f", FocusSearch, None),
        KeyBinding::new("secondary-n", NewItem, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrev, None),
        KeyBinding::new("escape", Escape, None),
        KeyBinding::new("up", NavUp, Some("Shell")),
        KeyBinding::new("down", NavDown, Some("Shell")),
    ]);
    ui::input::bind_keys(cx);
    ui::select::bind_keys(cx);
}

/// Prefer Figtree (if installed), then common Linux UI fonts.
fn pick_font(cx: &App) -> SharedString {
    if cfg!(target_os = "macos") || cfg!(windows) {
        return ".SystemUIFont".into();
    }
    let names = cx.text_system().all_font_names();
    for want in [
        "Figtree",
        "Inter",
        "Cantarell",
        "Ubuntu",
        "Noto Sans",
        "Liberation Sans",
        "DejaVu Sans",
    ] {
        if names.iter().any(|n| n == want) {
            return want.into();
        }
    }
    ".SystemUIFont".into()
}

pub fn run(launch: Launch) {
    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            let mut base = Theme::for_appearance(cx.window_appearance());
            base.font_family = pick_font(cx);
            cx.set_global(base);
            bind_keys(cx);
            // Quit with nothing focused in the shell: ask the shell window.
            cx.on_action(|_: &Quit, cx| {
                let shells: Vec<_> = cx
                    .windows()
                    .into_iter()
                    .filter_map(|w| w.downcast::<Shell>())
                    .collect();
                if shells.is_empty() {
                    lf_ui::shutdown::wait_for_pending_writes(Duration::from_secs(5));
                    cx.quit();
                }
                for shell in shells {
                    let _ = shell.update(cx, |s, window, cx| s.request_close(true, window, cx));
                }
            });
            let sandbox = launch.services.is_sandbox();
            let bounds = Bounds::centered(None, size(px(1280.), px(850.)), cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(960.), px(640.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some(
                            if sandbox {
                                "LocalFlow (Sandbox)"
                            } else {
                                "LocalFlow"
                            }
                            .into(),
                        ),
                        ..Default::default()
                    }),
                    app_id: Some("localflow-desktop".into()),
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| Shell::new(launch, cx)),
            );
            match window {
                Ok(window) => {
                    let _ = window.update(cx, |shell, window, cx| {
                        window.focus(&shell.focus_handle(cx));
                        cx.activate(true);
                        // Titlebar close: flush first; keep the window open
                        // (with an explanation) when something wasn't saved.
                        let weak = cx.weak_entity();
                        window.on_window_should_close(cx, move |window, cx| {
                            weak.update(cx, |s, cx| s.should_close(false, window, cx))
                                .unwrap_or(true)
                        });
                        // Follow the system light/dark setting live.
                        cx.observe_window_appearance(window, |_, window, cx| {
                            let want = Theme::for_appearance(window.appearance());
                            if want.dark != theme(cx).dark {
                                let font_family = theme(cx).font_family.clone();
                                cx.set_global(Theme {
                                    font_family,
                                    ..want
                                });
                                cx.notify();
                            }
                        })
                        .detach();
                    });
                }
                Err(err) => {
                    eprintln!("lf-desktop: couldn't open a window: {err:#}");
                    cx.quit();
                }
            }
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    lf_ui::shutdown::wait_for_pending_writes(Duration::from_secs(5));
                    cx.quit();
                }
            })
            .detach();
        });
}

/// Next index in a focus trap of `len` stops (wraps; starts at the first
/// stop going forward and the last going backward).
pub fn trap_step(current: Option<usize>, len: usize, forward: bool) -> usize {
    match (current, forward) {
        (None, true) => 0,
        (None, false) => len.saturating_sub(1),
        (Some(i), true) => (i + 1) % len.max(1),
        (Some(i), false) => (i + len.max(1) - 1) % len.max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::trap_step;

    #[test]
    fn dialog_focus_trap_cycles_within_its_buttons() {
        assert_eq!(
            trap_step(None, 2, true),
            0,
            "Tab from outside lands on Cancel"
        );
        assert_eq!(trap_step(None, 2, false), 1);
        assert_eq!(trap_step(Some(0), 2, true), 1);
        assert_eq!(trap_step(Some(1), 2, true), 0, "wraps instead of leaving");
        assert_eq!(trap_step(Some(0), 2, false), 1);
        assert_eq!(trap_step(Some(1), 2, false), 0);
    }
}
