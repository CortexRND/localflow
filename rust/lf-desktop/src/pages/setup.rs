//! The setup wizard overlay (full window over the shell).
//!
//! State is `controllers::setup_flow::SetupFlow`; this view only renders it
//! and runs its blocking work (checks, device listing, inspection, the mic
//! meter open, downloads, the trial, Apply) on the background executor with
//! generation checks. Closing keeps the draft for the session.

use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, ClipboardItem, Context, Entity, FontWeight, SharedString, Window};

use crate::app::PageCx;
use crate::controllers::jobs::{self, JobState};
use crate::controllers::setup_flow::{
    self as ctl, Applying, CleanupMode, Loadable, MicTest, SetupFlow, Step, Trial,
};
use crate::theme::{size, space, theme, Theme, Tone};
use crate::ui::select::{Select, SelectEvent};
use crate::ui::{self, Button, IconButton};

pub struct SetupView {
    pcx: PageCx,
    flow: Option<SetupFlow>,
    init_error: Option<String>,
    devices: Entity<Select>,
    device_values: Vec<String>,
    ticking: bool,
    polling_jobs: bool,
    /// Which job kinds ran at the last poll (refresh each as it finishes).
    job_watch: jobs::KindWatch,
}

impl SetupView {
    pub fn new(pcx: PageCx, cx: &mut Context<Self>) -> Self {
        let devices = cx.new(|cx| Select::new(cx, Vec::new(), None).width(320.));
        cx.subscribe(&devices, |this, _, event, cx| {
            let SelectEvent::Changed(i) = event;
            if let Some(device) = this.device_values.get(*i).cloned() {
                if let Some(flow) = this.flow.as_mut() {
                    flow.set_device(&device);
                }
                cx.notify();
            }
        })
        .detach();
        let job_watch = jobs::KindWatch::new(&pcx.services.downloads);
        let mut view = SetupView {
            pcx,
            flow: None,
            init_error: None,
            devices,
            device_values: Vec::new(),
            ticking: false,
            polling_jobs: false,
            job_watch,
        };
        view.build(cx);
        view
    }

    /// Choices differ from the saved config (a draft would be lost).
    pub fn dirty(&self) -> bool {
        self.flow.as_ref().is_some_and(SetupFlow::dirty)
    }

    /// Release audio (window deactivated, overlay hidden, quitting).
    pub fn release_audio(&mut self, cx: &mut Context<Self>) {
        if let Some(flow) = self.flow.as_mut() {
            if flow.release_audio() {
                cx.notify();
            }
        }
    }

    fn build(&mut self, cx: &mut Context<Self>) {
        let config = self.pcx.services.config();
        let deps = self.pcx.services.setup.clone();
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    let hw = deps.platform.detect_hardware()?;
                    SetupFlow::new(
                        &config,
                        &hw,
                        deps.platform.as_ref(),
                        deps.launch_agent.as_ref(),
                        deps.simulated,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match built {
                    Ok(flow) => {
                        this.flow = Some(flow);
                        this.enter_step(cx);
                    }
                    Err(error) => this.init_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Load what the current step shows (each load is local; Ollama's list
    /// is a loopback request only on the Cleanup step).
    fn enter_step(&mut self, cx: &mut Context<Self>) {
        let Some(step) = self.flow.as_ref().map(|f| f.step) else {
            return;
        };
        match step {
            Step::Checkup => {
                if self
                    .flow
                    .as_ref()
                    .is_some_and(|f| f.checks == Loadable::Idle)
                {
                    self.run_checks(cx);
                }
            }
            Step::Microphone => {
                if self
                    .flow
                    .as_ref()
                    .is_some_and(|f| f.devices == Loadable::Idle)
                {
                    self.load_devices(cx);
                }
            }
            Step::Speech | Step::Trial | Step::Review => {
                if self
                    .flow
                    .as_ref()
                    .is_some_and(|f| f.installed == Loadable::Idle)
                {
                    self.inspect(cx);
                }
            }
            Step::Cleanup => {
                let wants = self
                    .flow
                    .as_ref()
                    .is_some_and(|f| f.local_allowed() && f.ollama == Loadable::Idle);
                if wants {
                    self.load_ollama(cx);
                }
            }
            Step::Shortcut => {}
        }
    }

    fn run_checks(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.flow.as_mut() else {
            return;
        };
        if !flow.begin_checks() {
            return;
        }
        let config = self.pcx.services.config();
        let deps = self.pcx.services.setup.clone();
        cx.spawn(async move |this, cx| {
            let checks = cx
                .background_executor()
                .spawn(async move { ctl::checks(&config, deps.platform.as_ref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(flow) = this.flow.as_mut() {
                    flow.checks_done(checks);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_devices(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.flow.as_mut() else {
            return;
        };
        let generation = flow.begin_fact(ctl::Fact::Devices);
        let deps = self.pcx.services.setup.clone();
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move { deps.platform.input_devices() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(flow) = this.flow.as_mut() {
                    flow.devices_done(generation, listed);
                }
                this.sync_devices(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_devices(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.flow.as_ref() else {
            return;
        };
        let mut values = vec![String::new()];
        let mut labels = vec![SharedString::from("System default")];
        for device in flow.devices.ready().cloned().unwrap_or_default() {
            labels.push(device.clone().into());
            values.push(device);
        }
        let current = flow.choices.input_device.clone();
        if !values.contains(&current) {
            labels.push(format!("{current} (not connected)").into());
            values.push(current.clone());
        }
        let selected = values.iter().position(|v| *v == current);
        self.device_values = values;
        self.devices
            .update(cx, |s, cx| s.set_options(labels, selected, cx));
    }

    fn inspect(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.flow.as_mut() else {
            return;
        };
        let deps = self.pcx.services.setup.clone();
        if !deps.native_speech {
            flow.installed = Loadable::Failed(deps.speech_reason.clone().unwrap_or_default());
            return;
        }
        let generation = flow.begin_fact(ctl::Fact::Installed);
        let models = flow.models.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { deps.models.speech_downloads(&models) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(flow) = this.flow.as_mut() {
                    flow.installed_done(generation, found);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_ollama(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.flow.as_mut() else {
            return;
        };
        let generation = flow.begin_fact(ctl::Fact::Ollama);
        let base = self
            .pcx
            .services
            .config()
            .field_str("llm_base_url")
            .unwrap_or("http://localhost:11434")
            .to_string();
        let deps = self.pcx.services.setup.clone();
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move { deps.models.ollama(&base) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(flow) = this.flow.as_mut() {
                    flow.ollama_done(generation, listed);
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ------------------------------------------------------- navigation ---

    fn go(&mut self, step: Step, cx: &mut Context<Self>) {
        if let Some(flow) = self.flow.as_mut() {
            if flow.go_to(step) {
                self.enter_step(cx);
            }
        }
        cx.notify();
    }

    /// A checkup row's fix button. The sandbox opens no external URL and
    /// says what it would have opened; step navigation and copy still work.
    fn apply_fix(&mut self, action: ctl::CheckAction, cx: &mut Context<Self>) {
        match ctl::fix_effect(action, self.pcx.services.setup.simulated) {
            ctl::FixEffect::Open(url) => cx.open_url(&url),
            ctl::FixEffect::GoTo(step) => self.go(step, cx),
            ctl::FixEffect::Copy(cmd) => {
                cx.write_to_clipboard(ClipboardItem::new_string(cmd.clone()));
                // Name the command: the toast is the only confirmation of
                // what's on the clipboard.
                self.pcx.toast(
                    format!("Copied “{cmd}” — run it in a terminal, then Check again."),
                    cx,
                );
            }
            ctl::FixEffect::Notice(message) => self.pcx.toast(message, cx),
        }
    }

    fn next(&mut self, cx: &mut Context<Self>) {
        if let Some(flow) = self.flow.as_mut() {
            if flow.forward() {
                self.enter_step(cx);
            }
        }
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(flow) = self.flow.as_mut() {
            if flow.back() {
                self.enter_step(cx);
            }
        }
        cx.notify();
    }

    /// Setup is shown again: models, microphones and Ollama may have
    /// changed meanwhile (e.g. in Models & keys). Reload what the current
    /// step shows; the draft's choices are kept.
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if let Some(flow) = self.flow.as_mut() {
            flow.invalidate_facts();
        }
        self.enter_step(cx);
        cx.notify();
    }

    /// Close the overlay (choices kept for the session).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        let dirty = self.dirty();
        if let Some(flow) = self.flow.as_mut() {
            flow.close();
        }
        self.pcx.close_setup(cx);
        if dirty {
            self.pcx.toast(
                "Setup paused. Your choices are kept until you quit — Resume setup on Home.",
                cx,
            );
        }
    }

    // ------------------------------------------------------- microphone ---

    fn test_mic(&mut self, cx: &mut Context<Self>) {
        let Some((generation, device)) = self.flow.as_mut().and_then(SetupFlow::begin_mic_test)
        else {
            return;
        };
        let meter = self.pcx.services.setup.meter.clone();
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move { meter.open(device.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(flow) = this.flow.as_mut() {
                    flow.mic_opened(generation, opened, Instant::now());
                }
                this.start_ticking(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn start_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ticking || !self.flow.as_ref().is_some_and(SetupFlow::mic_open) {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            let go_on = this
                .update(cx, |this, cx| {
                    let running = this
                        .flow
                        .as_mut()
                        .is_some_and(|f| f.tick_mic(Instant::now()));
                    if !running {
                        this.ticking = false;
                    }
                    cx.notify();
                    running
                })
                .unwrap_or(false);
            if !go_on {
                break;
            }
        })
        .detach();
    }

    // ----------------------------------------------------------- models ---

    fn download_selected(&mut self, then_next: bool, cx: &mut Context<Self>) {
        let Some(flow) = self.flow.as_ref() else {
            return;
        };
        let Some(model) = flow.model(&flow.choices.stt_id).cloned() else {
            return;
        };
        if let Some(reason) = self.pcx.runtime_busy() {
            self.pcx.toast(reason, cx);
            return;
        }
        let backend = self.pcx.services.setup.models.clone();
        self.pcx.services.downloads.start(
            &jobs::speech_key(&model.id),
            Box::new(move |progress, cancel| backend.download_speech(&model, progress, cancel)),
        );
        self.poll_jobs(cx);
        if then_next {
            self.next(cx);
        }
    }

    fn pull_cleanup(&mut self, then_next: bool, cx: &mut Context<Self>) {
        let Some(id) = self.flow.as_ref().and_then(|f| f.choices.llm_id.clone()) else {
            return;
        };
        let base = self
            .pcx
            .services
            .config()
            .field_str("llm_base_url")
            .unwrap_or("http://localhost:11434")
            .to_string();
        let backend = self.pcx.services.setup.models.clone();
        let model = id.clone();
        self.pcx.services.downloads.start(
            &jobs::cleanup_key(&id),
            Box::new(move |progress, cancel| backend.pull_cleanup(&base, &model, progress, cancel)),
        );
        self.poll_jobs(cx);
        if then_next {
            self.next(cx);
        }
    }

    /// Repaint while any job runs. Each kind is refreshed as soon as its last
    /// job ends (speech inventory, Ollama list), not only when every job has
    /// ended: the trial needs only speech, so a speech model that finished
    /// is usable while a cleanup pull still runs.
    fn poll_jobs(&mut self, cx: &mut Context<Self>) {
        // Hand out any completion that ended before this poll began (the
        // watch counts finishes, so none is missed between polls).
        let finished = self.job_watch.observe(&self.pcx.services.downloads);
        self.refresh_finished(finished, cx);
        if self.polling_jobs {
            return;
        }
        self.polling_jobs = true;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let go_on = this
                .update(cx, |this, cx| {
                    let finished = this.job_watch.observe(&this.pcx.services.downloads);
                    let running = this.pcx.services.downloads.any_running();
                    if running {
                        this.refresh_finished(finished, cx);
                    } else {
                        this.polling_jobs = false;
                        this.inspect(cx);
                        this.load_ollama_if_local(cx);
                    }
                    cx.notify();
                    running
                })
                .unwrap_or(false);
            if !go_on {
                break;
            }
        })
        .detach();
    }

    /// Refresh only what a finished kind changed (at most once per kind per
    /// finish; generations drop results that a newer refresh replaced).
    fn refresh_finished(&mut self, finished: jobs::Finished, cx: &mut Context<Self>) {
        if finished.speech {
            self.inspect(cx);
        }
        if finished.cleanup {
            self.load_ollama_if_local(cx);
        }
    }

    fn load_ollama_if_local(&mut self, cx: &mut Context<Self>) {
        if self
            .flow
            .as_ref()
            .is_some_and(|f| f.local_allowed() && f.ollama != Loadable::Idle)
        {
            self.load_ollama(cx);
        }
    }

    // ------------------------------------------------------------ trial ---

    fn start_trial(&mut self, cx: &mut Context<Self>) {
        let busy = self.pcx.runtime_busy();
        let native = self.pcx.services.setup.native_speech;
        let Some(ctl::TrialJob {
            generation,
            cancel,
            model,
            device,
            language,
        }) = self.flow.as_mut().and_then(|f| f.begin_trial(native, busy))
        else {
            cx.notify();
            return;
        };
        let backend = self.pcx.services.setup.models.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    backend.try_dictation_cancellable(
                        &model,
                        device.as_deref(),
                        language.as_deref(),
                        cancel,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(flow) = this.flow.as_mut() {
                    flow.trial_done(generation, result);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ------------------------------------------------------------ apply ---

    fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.flow.as_mut().and_then(SetupFlow::begin_apply) else {
            return;
        };
        let services = &self.pcx.services;
        let path = services.root.config_path.clone();
        // No preparatory write: a missing file starts from this root's own
        // defaults inside the locked transaction on the worker.
        let root = services.root.clone();
        let write = lf_ui::shutdown::register_pending_write();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let result = ctl::apply(&job, &path, || crate::data::load_config_at(&root));
                    drop(write);
                    result
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let ok = result.is_ok();
                if let Some(flow) = this.flow.as_mut() {
                    flow.apply_done(result);
                }
                if ok {
                    this.pcx.services.reload_config();
                    this.pcx.config_changed(cx);
                    this.pcx.setup_applied(cx);
                    this.pcx.toast("Setup saved.", cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ----------------------------------------------------------- render ---

    fn simulated(&self, t: &Theme) -> Option<gpui::Div> {
        self.pcx
            .services
            .setup
            .simulated
            .then(|| ui::token(t, Tone::Warning, "Simulated"))
    }

    fn render_track(&self, t: &Theme, flow: &SetupFlow, cx: &mut Context<Self>) -> gpui::Div {
        let mut track = div().flex().flex_1().gap(space::s2());
        for step in Step::ALL {
            let done_or_current = step.index() <= flow.step.index() || flow.visited(step);
            let problem = flow.visited(step)
                && match step {
                    Step::Checkup => flow.failed_checks() > 0,
                    Step::Shortcut => flow.hotkey_error.is_some(),
                    _ => false,
                };
            let color = if problem {
                t.dot(Tone::Warning)
            } else if done_or_current {
                t.accent
            } else {
                t.bg_muted
            };
            let segment = div()
                .id(("setup-track", step.index() as u64))
                .flex_1()
                .h(px(4.))
                .rounded(size::radius_inner())
                .bg(color)
                .tooltip(move |_, cx| ui::Tooltip::view(step.label().into(), cx));
            let segment = if flow.visited(step) {
                segment
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.go(step, cx)))
            } else {
                segment
            };
            track = track.child(segment);
        }
        div()
            .flex()
            .items_center()
            .gap(space::s6())
            .child(track)
            .child(ui::supporting(
                t,
                format!(
                    "Step {} of {} · {}",
                    flow.step.index() + 1,
                    Step::ALL.len(),
                    flow.step.label()
                ),
            ))
    }

    fn check_rows(&self, t: &Theme, flow: &SetupFlow, cx: &mut Context<Self>) -> gpui::Div {
        let mut list = ui::list(t);
        match &flow.checks {
            Loadable::Ready(checks) => {
                for (i, check) in checks.iter().enumerate() {
                    let (icon, color) = match check.ok {
                        Some(true) => ("circle-check", t.icon_accent),
                        Some(false) => ("circle-alert", t.dot(Tone::Warning)),
                        None => ("info", t.icon_secondary),
                    };
                    if i > 0 {
                        list = list.child(ui::separator(
                            t,
                            ui::row_inset() + size::icon() + space::s5(),
                        ));
                    }
                    list = list.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s5())
                            .pl(ui::row_inset())
                            .pr(space::s4())
                            .py(space::s4())
                            .child(ui::icon(icon, color))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(ui::label(t, check.label.clone()))
                                    .child(ui::supporting(t, check.detail.clone())),
                            )
                            .when_some(ctl::check_fix(check), |row, fix| {
                                let tooltip = ctl::fix_tooltip(&fix.action);
                                let action = fix.action;
                                row.child(
                                    Button::new(
                                        SharedString::from(format!("setup-fix-{}", check.key)),
                                        fix.label,
                                    )
                                    .small()
                                    .tooltip(tooltip)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| this.apply_fix(action.clone(), cx),
                                    )),
                                )
                            }),
                    );
                }
            }
            _ => {
                list = list.child(
                    div()
                        .px(space::s4())
                        .py(space::s4())
                        .child(ui::secondary(t, "Checking…")),
                );
            }
        }
        list
    }

    fn body(&self, t: &Theme, flow: &SetupFlow, cx: &mut Context<Self>) -> gpui::Div {
        let services = &self.pcx.services;
        let deps = &services.setup;
        let mut body = div().flex().flex_col().gap(space::s6());
        match flow.step {
            Step::Checkup => {
                body = body.child(self.check_rows(t, flow, cx));
                if flow.failed_checks() > 0 {
                    body = body.child(ui::banner(
                        t,
                        Tone::Warning,
                        "You can continue and fix these later.",
                        None,
                    ));
                }
                if let Some(sim) = self.simulated(t) {
                    body = body.child(div().flex().child(sim));
                }
            }
            Step::Microphone => {
                body = body.child(
                    ui::group(t).child(ui::setting_row(
                        t,
                        "Microphone",
                        "LocalFlow records from this input when you dictate.",
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s3())
                            .child(self.devices.clone())
                            .child(
                                Button::new("mic-list-refresh", "Refresh")
                                    .small()
                                    .icon("refresh-cw")
                                    .busy(flow.devices == Loadable::Loading)
                                    .tooltip("Look for newly connected microphones")
                                    .on_click(cx.listener(|this, _, _, cx| this.load_devices(cx))),
                            )
                            .into_any_element(),
                        None,
                    )),
                );
                if let Loadable::Failed(error) = &flow.devices {
                    body = body.child(ui::banner(
                        t,
                        Tone::Warning,
                        format!("Couldn't list microphones: {error}"),
                        Some(
                            Button::new("mic-refresh", "Refresh")
                                .small()
                                .icon("refresh-cw")
                                .on_click(cx.listener(|this, _, _, cx| this.load_devices(cx)))
                                .into_any_element(),
                        ),
                    ));
                } else if flow.devices.ready().is_some_and(Vec::is_empty) {
                    body = body.child(ui::banner(
                        t,
                        Tone::Warning,
                        "No microphones found. Connect one, then refresh.",
                        Some(
                            Button::new("mic-refresh", "Refresh")
                                .small()
                                .icon("refresh-cw")
                                .on_click(cx.listener(|this, _, _, cx| this.load_devices(cx)))
                                .into_any_element(),
                        ),
                    ));
                }
                let testing = matches!(flow.mic, MicTest::Opening | MicTest::Testing { .. });
                let mut card = ui::card(t).flex_col().gap(space::s4()).child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::s4())
                        .child(
                            Button::new(
                                "mic-test",
                                if testing {
                                    "Testing…"
                                } else {
                                    "Test microphone"
                                },
                            )
                            .icon("mic")
                            .busy(testing)
                            .on_click(cx.listener(|this, _, _, cx| this.test_mic(cx))),
                        )
                        .children(self.simulated(t)),
                );
                let line = match &flow.mic {
                    MicTest::Off => (Tone::Neutral, "The microphone stays off until you press Test microphone.".to_string()),
                    MicTest::Opening => (Tone::Info, "Opening the microphone…".into()),
                    MicTest::Testing { heard: true, .. } => (Tone::Success, "We can hear you.".into()),
                    MicTest::Testing { .. } => (Tone::Info, "Listening for 10 seconds… say something.".into()),
                    MicTest::Heard => (Tone::Success, "We can hear you.".into()),
                    MicTest::Silent => (Tone::Warning, "We can't hear anything from this microphone. Check it's not muted, or pick another.".into()),
                    MicTest::Failed(error) => (Tone::Error, error.clone()),
                };
                if let MicTest::Testing { level, heard } = &flow.mic {
                    card = card.child(ui::level_meter(
                        t,
                        f32::from(*level) / 100.0,
                        if *heard { Tone::Success } else { Tone::Info },
                    ));
                }
                card = card.child(ui::status_dot(t, line.0, line.1));
                if deps.simulated {
                    card = card.child(ui::supporting(t, "Sandbox: your microphone is not opened."));
                }
                body = body.child(card);
            }
            Step::Speech => {
                let rows = crate::controllers::model_storage::model_rows(
                    &flow.models,
                    &flow.installed,
                    &services.downloads,
                    &flow.choices.stt_id,
                    &flow.recommended,
                    None,
                );
                let mut list = ui::list(t);
                for (i, row) in rows.into_iter().enumerate() {
                    let id = row.id.clone();
                    let (tone, word) = row_token(&row.state);
                    let mut item = ui::row_button(t, ("setup-model", i as u64), row.selected)
                        .flex_col()
                        .gap(space::s2())
                        .pl(ui::row_inset())
                        .pr(space::s4())
                        .py(space::s4())
                        .rounded(size::radius_inner())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(flow) = this.flow.as_mut() {
                                flow.select_model(&id);
                            }
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(space::s4())
                                .child(ui::icon(
                                    if row.selected {
                                        "circle-check"
                                    } else {
                                        "circle"
                                    },
                                    if row.selected {
                                        t.icon_accent
                                    } else {
                                        t.icon_secondary
                                    },
                                ))
                                .child(ui::label(t, row.label.clone()).flex_1())
                                .when(row.recommended, |d| {
                                    d.child(ui::token(t, Tone::Accent, "Recommended"))
                                })
                                .child(ui::token(t, tone, word))
                                .child(ui::supporting(
                                    t,
                                    format!(
                                        "about {}",
                                        crate::controllers::model_storage::format_bytes(
                                            u64::from(row.size_mb) * 1_000_000
                                        )
                                    ),
                                )),
                        )
                        .child(ui::supporting(t, row.blurb).pl(px(28.)));
                    if let crate::controllers::model_storage::RowState::Downloading(job) =
                        &row.state
                    {
                        let key = jobs::speech_key(&row.id);
                        let downloads = services.downloads.clone();
                        item = item.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(space::s4())
                                .pl(px(28.))
                                .child(div().flex_1().child(ui::progress(t, job.fraction())))
                                .child(ui::supporting(t, job.progress_text()))
                                .child(
                                    Button::new(("setup-cancel-dl", i as u64), "Cancel")
                                        .small()
                                        .ghost()
                                        .on_click(move |_, _, _| {
                                            downloads.cancel(&key);
                                        }),
                                ),
                        );
                        if job.stalled(Instant::now()) {
                            item = item.child(
                                ui::supporting(t, "Still working… check your connection.")
                                    .pl(px(28.)),
                            );
                        }
                    }
                    if let crate::controllers::model_storage::RowState::Failed(error) = &row.state {
                        item = item.child(ui::error_line(t, error.clone()).pl(px(28.)));
                    }
                    if i > 0 {
                        list = list.child(ui::separator(
                            t,
                            ui::row_inset() + size::icon() + space::s4(),
                        ));
                    }
                    list = list.child(item);
                }
                body = body.child(list);
                if let Some(reason) = &deps.speech_reason {
                    body = body.child(ui::banner(t, Tone::Info, format!("Your choice is saved, but this window can't check or download models. {reason}"), None));
                }
                if let Some(sim) = self.simulated(t) {
                    body = body.child(div().flex().child(sim));
                }
            }
            Step::Cleanup => {
                let label = ctl::provider_label(&flow.provider);
                let options: Vec<(CleanupMode, String)> = if flow.local_allowed() {
                    vec![
                        (CleanupMode::Local, "With Ollama".into()),
                        (CleanupMode::Off, "No cleanup".into()),
                    ]
                } else {
                    vec![
                        (CleanupMode::KeepProvider, format!("Keep {label}")),
                        (CleanupMode::Off, "No cleanup".into()),
                    ]
                };
                let selected = options.iter().position(|(m, _)| *m == flow.cleanup);
                let modes: Vec<CleanupMode> = options.iter().map(|(m, _)| *m).collect();
                let this = cx.entity().downgrade();
                body = body.child(
                    ui::Segmented::new(
                        "cleanup-mode",
                        options.into_iter().map(|(_, l)| l),
                        selected,
                    )
                    .on_select(move |i, _, cx| {
                        let _ = this.update(cx, |this, cx| {
                            if let (Some(flow), Some(mode)) = (this.flow.as_mut(), modes.get(i)) {
                                flow.set_cleanup(*mode);
                            }
                            this.enter_step(cx);
                            cx.notify();
                        });
                    }),
                );
                match flow.cleanup {
                    CleanupMode::Local => {
                        let mut list = ui::list(t);
                        for (i, model) in lf_ui::setup_flow::LLM_MODELS.iter().enumerate() {
                            let chosen = flow.choices.llm_id.as_deref() == Some(model.id);
                            let id = model.id;
                            if i > 0 {
                                list = list.child(ui::separator(
                                    t,
                                    ui::row_inset() + size::icon() + space::s4(),
                                ));
                            }
                            list = list.child(
                                ui::row_button(t, ("setup-llm", i as u64), chosen)
                                    .items_center()
                                    .gap(space::s4())
                                    .pl(ui::row_inset())
                                    .pr(space::s4())
                                    .py(space::s4())
                                    .rounded(size::radius_inner())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(flow) = this.flow.as_mut() {
                                            flow.choose_llm(id);
                                        }
                                        cx.notify();
                                    }))
                                    .child(ui::icon(
                                        if chosen { "circle-check" } else { "circle" },
                                        if chosen {
                                            t.icon_accent
                                        } else {
                                            t.icon_secondary
                                        },
                                    ))
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .flex_1()
                                            .child(ui::label(t, model.label))
                                            .child(ui::supporting(t, model.blurb)),
                                    )
                                    .when(flow.recommended_llm.as_deref() == Some(model.id), |d| {
                                        d.child(ui::token(t, Tone::Accent, "Recommended"))
                                    })
                                    .child(ui::supporting(
                                        t,
                                        crate::controllers::model_storage::format_bytes(
                                            u64::from(model.size_mb) * 1_000_000,
                                        ),
                                    )),
                            );
                        }
                        body = body.child(list);
                        let state = match &flow.ollama {
                            Loadable::Ready(_) => match flow.llm_installed() {
                                Some(true) => (Tone::Success, "Ollama has this model.".to_string()),
                                _ => (Tone::Info, "Ollama doesn't have this model yet. Download & continue pulls it.".into()),
                            },
                            Loadable::Failed(error) => (Tone::Warning, format!("{error}. Install or start Ollama (https://ollama.com/download), then come back.")),
                            _ => (Tone::Neutral, "Asking Ollama…".into()),
                        };
                        body = body.child(ui::status_dot(t, state.0, state.1));
                        if let Some(id) = flow.choices.llm_id.as_deref() {
                            if let Some(job) = services.downloads.state(&jobs::cleanup_key(id)) {
                                body = body.child(job_line(
                                    t,
                                    &job,
                                    &services.downloads,
                                    &jobs::cleanup_key(id),
                                ));
                            }
                        }
                    }
                    CleanupMode::KeepProvider => {
                        body = body.child(ui::banner(t, Tone::Info, if flow.provider_cleanup_on {
                                format!("Cleanup keeps using {label}, so dictated text is sent to {label}. Change provider or keys in Models & keys after setup.")
                            } else {
                                format!("Your current settings are kept: {label} is the provider and cleanup stays off, so nothing is sent. Turn cleanup on in Style, or change provider in Models & keys.")
                            }, None));
                    }
                    CleanupMode::Off => {
                        body = body.child(ui::secondary(t, "Transcripts are pasted as spoken. You can turn cleanup on later in Style."));
                    }
                }
            }
            Step::Shortcut => {
                let mut list = ui::list(t);
                for (i, (key, label)) in flow.hotkey_options().into_iter().enumerate() {
                    let chosen = flow.choices.hotkey == key;
                    if i > 0 {
                        list = list.child(ui::separator(
                            t,
                            ui::row_inset() + size::icon() + space::s4(),
                        ));
                    }
                    list = list.child(
                        ui::row_button(t, ("setup-hotkey", i as u64), chosen)
                            .items_center()
                            .gap(space::s4())
                            .pl(ui::row_inset())
                            .pr(space::s4())
                            .py(space::s4())
                            .rounded(size::radius_inner())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(flow) = this.flow.as_mut() {
                                    flow.set_hotkey(key);
                                }
                                cx.notify();
                            }))
                            .child(ui::icon(
                                if chosen { "circle-check" } else { "circle" },
                                if chosen {
                                    t.icon_accent
                                } else {
                                    t.icon_secondary
                                },
                            ))
                            .child(ui::label(t, label).flex_1())
                            .child(ui::keys(t, &lf_ui::hub_model::format_hotkey(Some(key)))),
                    );
                }
                body = body.child(list);
                if let Some(error) = &flow.hotkey_error {
                    body = body.child(ui::error_line(t, error.clone()));
                }
                match crate::controllers::runtime_state::Session::detect() {
                    crate::controllers::runtime_state::Session::Wayland => {
                        body = body.child(ui::banner(
                            t,
                            Tone::Warning,
                            format!(
                                "{} Your choice is saved but can't work in this session.",
                                lf_input::linux::WAYLAND_UNSUPPORTED
                            ),
                            None,
                        ));
                    }
                    crate::controllers::runtime_state::Session::X11 => {
                        body = body.child(ui::banner(
                            t,
                            Tone::Info,
                            "Global shortcuts work in X11 sessions once LocalFlow is running.",
                            None,
                        ));
                    }
                    _ => {}
                }
            }
            Step::Trial => {
                let busy = self.pcx.runtime_busy();
                let blocker = flow.trial_blocker(deps.native_speech, busy);
                let mut card = ui::card(t).flex_col().gap(space::s4());
                let (tone, text) = match &flow.trial {
                    Trial::Ready => (Tone::Neutral, "Press Start trial, then say a sentence. LocalFlow records for 5 seconds after the model loads.".to_string()),
                    Trial::Running => (Tone::Info, "Loading the model, then recording for 5 seconds…".into()),
                    Trial::Success(_) => (Tone::Success, "It works. This is what LocalFlow heard:".into()),
                    Trial::Failed(error) => (Tone::Error, error.clone()),
                };
                card = card
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(
                                Button::new(
                                    "trial-start",
                                    if matches!(flow.trial, Trial::Success(_) | Trial::Failed(_)) {
                                        "Try again"
                                    } else {
                                        "Start trial"
                                    },
                                )
                                .icon("mic")
                                .busy(flow.trial == Trial::Running)
                                .disabled(blocker.is_some())
                                .tooltip(
                                    blocker
                                        .clone()
                                        .unwrap_or_else(|| "Record about 5 seconds".into()),
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.start_trial(cx))),
                            )
                            .when(flow.trial == Trial::Running, |d| {
                                d.child(Button::new("trial-cancel", "Cancel").ghost().on_click(
                                    cx.listener(|this, _, _, cx| {
                                        if let Some(flow) = this.flow.as_mut() {
                                            flow.cancel_trial();
                                        }
                                        cx.notify();
                                    }),
                                ))
                            })
                            .children(self.simulated(t)),
                    )
                    .child(ui::status_dot(t, tone, text));
                if let Trial::Success(transcript) = &flow.trial {
                    card = card.child(ui::reading(t, transcript.clone()));
                }
                if flow.trial == Trial::Running {
                    card = card.child(ui::progress(t, None));
                }
                if let Some(reason) = blocker {
                    card = card.child(ui::supporting(t, reason));
                    if !deps.native_speech {
                        if let Some(why) = &deps.speech_reason {
                            card = card.child(ui::supporting(t, why.clone()));
                        }
                    } else if flow.installed(&flow.choices.stt_id) == Some(false) {
                        card = card.child(
                            Button::new("trial-to-speech", "Back to Speech model")
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| this.go(Step::Speech, cx))),
                        );
                    }
                }
                card = card.child(ui::supporting(
                    t,
                    "Nothing is saved to History and nothing is pasted.",
                ));
                body = body.child(card);
            }
            Step::Review => {
                for warning in flow.warnings() {
                    body = body.child(ui::banner(t, Tone::Warning, warning, None));
                }
                let mut rows = Vec::new();
                for (i, row) in flow.review_rows().into_iter().enumerate() {
                    let step = row.step;
                    let change = (step != Step::Review).then(|| {
                        Button::new(("review-change", i as u64), "Change")
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| this.go(step, cx)))
                            .into_any_element()
                    });
                    rows.push(
                        ui::setting_row(
                            t,
                            row.label,
                            row.value,
                            change.unwrap_or_else(|| div().into_any_element()),
                            None,
                        )
                        .into_any_element(),
                    );
                }
                body = body
                    .child(ui::group_rows(t, rows, ui::row_inset()))
                    .child(ui::supporting(
                        t,
                        format!("Saves to {}.", services.root.config_path.display()),
                    ));
                if let Applying::Failed(error) = &flow.apply {
                    body = body.child(ui::banner(
                        t,
                        Tone::Error,
                        format!("Couldn't save: {error}"),
                        None,
                    ));
                }
            }
        }
        body
    }

    fn footer(&self, t: &Theme, flow: &SetupFlow, cx: &mut Context<Self>) -> gpui::Div {
        let native = self.pcx.services.setup.native_speech;
        let applying = flow.apply == Applying::Busy;
        let blocked = flow.blocker();
        let mut right = div().flex().items_center().gap(space::s4());
        if let Some(reason) = &blocked {
            right = right.child(
                ui::supporting(t, reason.clone())
                    .text_color(t.tone(Tone::Error).0)
                    .max_w(px(320.)),
            );
        }
        let missing = flow.installed(&flow.choices.stt_id) == Some(false);
        let downloading = self
            .pcx
            .services
            .downloads
            .state(&jobs::speech_key(&flow.choices.stt_id))
            .is_some_and(|s| s.running());
        let primary = match flow.step {
            Step::Speech if missing && native && !downloading => {
                right = right.child(
                    Button::new("setup-skip-download", "Continue without downloading")
                        .on_click(cx.listener(|this, _, _, cx| this.next(cx))),
                );
                Button::new("setup-primary", "Download & continue")
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| this.download_selected(true, cx)))
            }
            Step::Cleanup
                if flow.cleanup == CleanupMode::Local && flow.llm_installed() == Some(false) =>
            {
                right = right.child(
                    Button::new("setup-skip-pull", "Continue without downloading")
                        .on_click(cx.listener(|this, _, _, cx| this.next(cx))),
                );
                Button::new("setup-primary", "Download & continue")
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| this.pull_cleanup(true, cx)))
            }
            Step::Checkup => {
                right = right.child(
                    Button::new("setup-recheck", "Check again")
                        .icon("refresh-cw")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(flow) = this.flow.as_mut() {
                                flow.checks = Loadable::Idle;
                            }
                            this.run_checks(cx);
                        })),
                );
                Button::new("setup-primary", "Continue")
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| this.next(cx)))
            }
            Step::Review => Button::new("setup-primary", "Apply and finish")
                .primary()
                .busy(applying)
                .disabled(blocked.is_some())
                .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
            _ => Button::new("setup-primary", "Continue")
                .primary()
                .disabled(blocked.is_some())
                .on_click(cx.listener(|this, _, _, cx| this.next(cx))),
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(space::s6())
            .pt(space::s6())
            .border_t_1()
            .border_color(t.border)
            .child(
                Button::new("setup-back", "Back")
                    .disabled(flow.step == Step::Checkup || applying)
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
            )
            .child(right.child(primary))
    }
}

fn row_token(state: &crate::controllers::model_storage::RowState) -> (Tone, String) {
    use crate::controllers::model_storage::RowState;
    match state {
        RowState::NotInstalled => (Tone::Neutral, "Not downloaded".into()),
        RowState::Downloading(_) => (Tone::Info, "Downloading".into()),
        RowState::Failed(_) => (Tone::Error, "Failed".into()),
        RowState::Installed => (Tone::Neutral, "Downloaded".into()),
        RowState::Active => (Tone::Success, "In use".into()),
        RowState::Unavailable(_) => (Tone::Neutral, "Not available".into()),
        RowState::Unknown(_) => (Tone::Neutral, "Unknown".into()),
    }
}

fn job_line(
    t: &Theme,
    job: &JobState,
    downloads: &crate::controllers::jobs::Downloads,
    key: &str,
) -> gpui::Div {
    let downloads = downloads.clone();
    let key = key.to_string();
    div()
        .flex()
        .items_center()
        .gap(space::s4())
        .when(job.running(), |d| {
            d.child(div().flex_1().child(ui::progress(t, job.fraction())))
        })
        .child(ui::supporting(t, job.progress_text()))
        .when(job.running(), |d| {
            d.child(
                Button::new("setup-cancel-pull", "Cancel")
                    .small()
                    .ghost()
                    .on_click(move |_, _, _| {
                        downloads.cancel(&key);
                    }),
            )
        })
}

impl Render for SetupView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let header = div()
            .flex()
            .items_center()
            .gap(space::s5())
            .child(ui::brand(&t))
            .child(ui::heading(&t, "Set up LocalFlow").flex_1())
            .child(
                IconButton::new("setup-close", "x", "Close setup")
                    .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            );
        let content = match (&self.flow, &self.init_error) {
            (_, Some(error)) => div().flex().flex_col().gap(space::s6()).child(ui::banner(
                &t,
                Tone::Error,
                format!("Setup can't start: {error}"),
                None,
            )),
            (None, None) => div().child(ui::secondary(&t, "Getting setup ready…")),
            (Some(flow), None) => {
                let step = flow.step;
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(space::s8())
                    .child(self.render_track(&t, flow, cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s1())
                            .child(ui::title(&t, step.title()))
                            .child(ui::secondary(&t, step.intro())),
                    )
                    .child(
                        div()
                            .id("setup-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(self.body(&t, flow, cx)),
                    )
                    .child(self.footer(&t, flow, cx))
            }
        };
        div()
            .id("setup-overlay")
            .occlude()
            .size_full()
            .flex()
            .justify_center()
            .bg(t.bg_surface)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::s8())
                    .w_full()
                    .max_w(px(size::COLUMN))
                    .h_full()
                    .px(space::s10())
                    .py(space::s8())
                    .font_weight(FontWeight::NORMAL)
                    .child(header)
                    .child(content),
            )
    }
}
