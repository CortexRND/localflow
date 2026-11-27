//! Settings → Agents → Voice triggers: the switch, one row per trigger,
//! the inline editor (new or existing; one at a time), and Try it.
//!
//! Part of [`AgentsSettings`]: it shares the tab's loaded state, its one
//! write at a time and its generation-checked load. Writes go through
//! `agents-api v1` (`/hot-commands`) and, for the switch, `PUT /api/config`.
//! Client checks run on Save only; the service stays the authority, and a
//! refused write keeps the draft as typed. Try it is explicit (Test or
//! Enter), never per keystroke. Decisions and copy live in
//! [`crate::controllers::agents_triggers`]; the Agent choice is data from
//! `/providers`, never a provider id in code.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    canvas, div, point, prelude::*, px, Bounds, Context, Entity, FocusHandle, Pixels, ScrollHandle,
    SharedString, Size, Window,
};

use super::{AgentsSettings, PendingFocus, Row};
use crate::app::Confirm;
use crate::controllers::agents_triggers::{
    self as ctl, DraftErrors, Rule, RuleDraft, RuleFailure, TestShown,
};
use crate::theme::{space, Theme, Tone};
use crate::ui::input::{InputEvent, TextInput};
use crate::ui::select::{Select, SelectEvent};
use crate::ui::{self, Button, Switch};

/// The open trigger editor: which rule (`None` for a new one), the draft's
/// select-backed fields, what it started from, and its errors. Say and
/// Task live in their fields.
pub(crate) struct RuleEditor {
    pub id: Option<String>,
    /// Bumped on every open, so a late reply only lands on its own editor.
    instance: u64,
    /// Provider, profile, repo and shortest task as chosen.
    draft: RuleDraft,
    /// The draft as opened (unsaved changes are differences from this).
    baseline: RuleDraft,
    errors: DraftErrors,
    /// Under the button row.
    form_error: Option<String>,
}

pub(crate) struct Triggers {
    pub(super) editor: Option<RuleEditor>,
    instance: u64,
    pub(super) say: Entity<TextInput>,
    task: Entity<TextInput>,
    agent: Entity<Select>,
    agent_values: Vec<(String, String)>,
    workspace: Entity<Select>,
    workspace_values: Vec<String>,
    min: Entity<Select>,
    min_values: Vec<u32>,
    try_input: Entity<TextInput>,
    result: Option<TestShown>,
    /// Bumped when the sentence changes: an older reply is dropped.
    test_generation: u64,
    /// The Voice triggers switch's value while its write is in flight.
    pub(super) pending_hot: Option<bool>,
    /// A trigger switch's value while its write is in flight.
    pending_rule: Option<(String, bool)>,
    /// Where the editor was opened from (`Some(None)`: Add trigger).
    opener: Option<Option<String>>,
    pub(super) opener_focus: FocusHandle,
    /// Fixture scene `agents-settings-trigger`: once read, test the example
    /// sentence and open the editor on the first enabled trigger.
    fixture: bool,
    /// The Settings column (set by the page) and where the Voice triggers
    /// section was last laid out in it (recorded at prepaint).
    pub(super) page_scroll: Option<ScrollHandle>,
    anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Bring the section into view: requested by the trigger scene, then
    /// shown once a drawn frame has it in place.
    reveal: Reveal,
}

/// Bringing the Voice triggers section to the top of the Settings column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reveal {
    Idle,
    /// Re-measured each frame (the content above may still be growing as
    /// replies land) until the section sits where it should; the count
    /// bounds the attempts.
    Requested(u8),
    /// In place for a column of this size; a resize (the scene is captured
    /// at two sizes in one window) brings it back into view again.
    Shown(Size<Pixels>),
}

/// Frames a reveal may take before it settles for where it is.
const REVEAL_FRAMES: u8 = 12;

/// Space kept above the section when it is brought into view.
const REVEAL_MARGIN: f32 = 16.;

/// The column offset (`y`, ≤ 0) that puts content at `section_top` (in
/// window coordinates, laid out at `offset_y`) `margin` below the top of
/// `viewport_top`, clamped to what the column can scroll.
pub(crate) fn reveal_offset(
    section_top: Pixels,
    viewport_top: Pixels,
    offset_y: Pixels,
    max_scroll: Pixels,
    margin: Pixels,
) -> Pixels {
    let wanted = offset_y - (section_top - viewport_top - margin);
    wanted.min(px(0.)).max(-max_scroll)
}

impl Triggers {
    pub(super) fn new(cx: &mut Context<AgentsSettings>) -> Self {
        let say = cx.new(|cx| {
            TextInput::new(cx, ctl::SAY_PLACEHOLDER)
                .max_chars(200)
                .label(ctl::SAY_LABEL)
        });
        let task = cx.new(|cx| {
            TextInput::new(cx, ctl::PROMPT_PLACEHOLDER)
                .multiline(2, 4)
                .submit_on_enter()
                .max_chars(ctl::MAX_TEMPLATE_CHARS)
                .label(ctl::TASK_LABEL)
        });
        for (input, task_field) in [(&say, false), (&task, true)] {
            cx.subscribe(input, move |this, _, event, cx| match event {
                InputEvent::Submit => this.save_rule(cx),
                InputEvent::Changed => {
                    if task_field {
                        this.sync_min(cx);
                    }
                    cx.notify();
                }
            })
            .detach();
        }
        let select = |cx: &mut Context<AgentsSettings>, w: f32| {
            cx.new(|cx| Select::new(cx, Vec::new(), None).width(w))
        };
        let agent = select(cx, 240.);
        let workspace = select(cx, 240.);
        let min = select(cx, 180.);
        cx.subscribe(&agent, |this, _, event, cx| {
            let SelectEvent::Changed(i) = event;
            if let (Some(v), Some(e)) = (
                this.triggers.agent_values.get(*i).cloned(),
                this.triggers.editor.as_mut(),
            ) {
                (e.draft.provider, e.draft.profile) = v;
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&workspace, |this, _, event, cx| {
            let SelectEvent::Changed(i) = event;
            if let (Some(v), Some(e)) = (
                this.triggers.workspace_values.get(*i).cloned(),
                this.triggers.editor.as_mut(),
            ) {
                e.draft.repo = v;
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&min, |this, _, event, cx| {
            let SelectEvent::Changed(i) = event;
            if let (Some(v), Some(e)) = (
                this.triggers.min_values.get(*i).copied(),
                this.triggers.editor.as_mut(),
            ) {
                e.draft.min_prompt_words = v;
                cx.notify();
            }
        })
        .detach();
        let try_input = cx.new(|cx| {
            TextInput::new(cx, ctl::TRY_PLACEHOLDER)
                .max_chars(ctl::MAX_TEMPLATE_CHARS)
                .label(ctl::TRY_LABEL)
        });
        cx.subscribe(&try_input, |this, _, event, cx| match event {
            InputEvent::Submit => this.run_test(cx),
            InputEvent::Changed => this.test_text_changed(cx),
        })
        .detach();
        Triggers {
            editor: None,
            instance: 0,
            say,
            task,
            agent,
            agent_values: Vec::new(),
            workspace,
            workspace_values: Vec::new(),
            min,
            min_values: Vec::new(),
            try_input,
            result: None,
            test_generation: 0,
            pending_hot: None,
            pending_rule: None,
            opener: None,
            opener_focus: cx.focus_handle().tab_stop(true),
            fixture: false,
            page_scroll: None,
            anchor: Rc::new(Cell::new(None)),
            reveal: Reveal::Idle,
        }
    }
}

impl AgentsSettings {
    fn rules(&self) -> &[Rule] {
        self.loaded
            .as_ref()
            .map(|l| l.rules.as_slice())
            .unwrap_or_default()
    }

    fn rule(&self, id: &str) -> Option<&Rule> {
        self.rules().iter().find(|r| r.id == id)
    }

    /// After every load: drop an editor whose rule is gone, and run the
    /// trigger fixture once there is something to show.
    pub(super) fn triggers_loaded(&mut self, cx: &mut Context<Self>) {
        if let Some(Some(id)) = self.triggers.editor.as_ref().map(|e| e.id.clone()) {
            if self.loaded.is_some() && self.rule(&id).is_none() {
                self.triggers.editor = None;
            }
        }
        if self.triggers.fixture && self.loaded.is_some() {
            self.open_trigger_fixture(cx);
        }
    }

    // --------------------------------------------------------- the switch ---

    pub(crate) fn set_hot_commands(&mut self, on: bool, cx: &mut Context<Self>) {
        let agents_on = self.loaded.as_ref().is_some_and(|l| l.settings.enabled);
        if !self.writable() || !agents_on {
            return;
        }
        self.triggers.pending_hot = Some(on);
        self.write_config(
            Row::HotCommands,
            ctl::hot_commands_patch(on),
            |this, _| this.triggers.result = None,
            cx,
        );
    }

    // ------------------------------------------------------------- editor ---

    /// The draft as it stands: Say and Task from their fields, the rest
    /// from the selects.
    fn current_draft(&self, cx: &gpui::App) -> Option<RuleDraft> {
        let editor = self.triggers.editor.as_ref()?;
        Some(RuleDraft {
            trigger: self.triggers.say.read(cx).text().to_string(),
            prompt_template: self.triggers.task.read(cx).text().to_string(),
            ..editor.draft.clone()
        })
    }

    /// The open editor has changes that aren't saved.
    fn rule_dirty(&self, cx: &gpui::App) -> bool {
        match (&self.triggers.editor, self.current_draft(cx)) {
            (Some(e), Some(draft)) => draft != e.baseline,
            _ => false,
        }
    }

    /// Add trigger (`None`) or Edit: the editor opens (one at a time) with
    /// Say focused.
    pub(crate) fn open_rule(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_rule_editor(id, cx);
        self.triggers.say.read(cx).focus(window);
    }

    fn open_rule_editor(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        let Some(loaded) = &self.loaded else {
            return;
        };
        let draft = match &id {
            Some(id) => match loaded.rules.iter().find(|r| r.id == *id) {
                Some(rule) => RuleDraft::from_rule(rule),
                None => return,
            },
            None => RuleDraft::new(&loaded.providers, &loaded.settings),
        };
        // One editor at a time: a key editor (and its text) closes.
        self.key_input.update(cx, |i, cx| i.clear(cx));
        self.editor = None;
        self.key_error = None;
        let (labels, values, selected) =
            ctl::agent_choices(&loaded.providers, (&draft.provider, &draft.profile));
        self.triggers.agent_values = values;
        self.triggers.agent.update(cx, |s, cx| {
            s.set_options(
                labels.into_iter().map(SharedString::from).collect(),
                selected,
                cx,
            );
            s.set_disabled(false, cx);
        });
        let w = ctl::workspace_choices(&loaded.settings, &draft.repo);
        self.triggers.workspace_values = w.values;
        self.triggers.workspace.update(cx, |s, cx| {
            s.set_options(
                w.labels.into_iter().map(SharedString::from).collect(),
                w.selected,
                cx,
            );
            s.set_disabled(false, cx);
        });
        let (labels, values, _) = ctl::min_word_choices(draft.min_prompt_words);
        self.triggers.min_values = values;
        self.triggers.min.update(cx, |s, cx| {
            s.set_options(
                labels.into_iter().map(SharedString::from).collect(),
                None,
                cx,
            )
        });
        let (trigger, task) = (draft.trigger.clone(), draft.prompt_template.clone());
        self.triggers
            .say
            .update(cx, |i, cx| i.set_text(trigger, cx));
        self.triggers.task.update(cx, |i, cx| i.set_text(task, cx));
        self.triggers.instance = self.triggers.instance.wrapping_add(1);
        self.triggers.editor = Some(RuleEditor {
            id: id.clone(),
            instance: self.triggers.instance,
            baseline: draft.clone(),
            draft,
            errors: DraftErrors::default(),
            form_error: None,
        });
        self.triggers.opener = Some(id);
        self.sync_min(cx);
        cx.notify();
    }

    /// Shortest task is disabled, showing No minimum, while the task has no
    /// `{prompt}`; otherwise it shows the chosen count.
    fn sync_min(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.current_draft(cx) else {
            return;
        };
        let fixed = !draft.uses_prompt();
        let selected = if fixed {
            self.triggers.min_values.iter().position(|v| *v == 0)
        } else {
            self.triggers
                .min_values
                .iter()
                .position(|v| *v == draft.min_prompt_words)
        };
        self.triggers.min.update(cx, |s, cx| {
            s.set_selected(selected, cx);
            s.set_disabled(fixed, cx);
        });
    }

    /// Cancel (or Esc): the editor closes; focus returns to its opener.
    pub(crate) fn cancel_rule(&mut self, cx: &mut Context<Self>) {
        if self.triggers.editor.take().is_some() {
            self.pending_focus = Some(PendingFocus::RuleOpener);
        }
        cx.notify();
    }

    /// Save: client checks first (nothing is sent on a failure), then
    /// `POST` for a new trigger or a `PUT` with only what changed.
    pub(crate) fn save_rule(&mut self, cx: &mut Context<Self>) {
        let Some(draft) = self.current_draft(cx) else {
            return;
        };
        if self.load_error.is_some() || self.op.is_some() {
            return;
        }
        let errors = ctl::validate_draft(&draft);
        let Some(editor) = self.triggers.editor.as_mut() else {
            return;
        };
        editor.form_error = None;
        editor.errors = errors.clone();
        if !errors.is_empty() {
            cx.notify();
            return;
        }
        let (id, instance) = (editor.id.clone(), editor.instance);
        let baseline = editor.baseline.clone();
        let body = match &id {
            None => ctl::create_body(&draft),
            Some(id) => {
                if self.rule(id).is_none() {
                    self.rule_gone(cx);
                    return;
                }
                // What the user changed since opening, not the difference
                // from a reload: an edit made elsewhere to a field left
                // alone here survives.
                let patch = ctl::edit_patch(&baseline, &draft);
                if patch.as_object().is_some_and(|m| m.is_empty()) {
                    // Nothing changed: nothing to send.
                    self.cancel_rule(cx);
                    return;
                }
                patch
            }
        };
        let Some(generation) = self.begin(Row::RuleEditor, cx) else {
            return;
        };
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match &id {
                        None => backend.create_rule(&body),
                        Some(id) => backend.update_rule(id, &body),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish(generation, cx) {
                    return;
                }
                match result {
                    Ok(_) => {
                        if this.editor_instance() == Some(instance) {
                            this.triggers.editor = None;
                            this.pending_focus = Some(PendingFocus::RuleOpener);
                        }
                        this.triggers.result = None;
                        this.pcx.toast(ctl::SAVED, cx);
                        this.load(cx);
                    }
                    Err(error) => this.rule_refused(instance, &error, cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn editor_instance(&self) -> Option<u64> {
        self.triggers.editor.as_ref().map(|e| e.instance)
    }

    /// A refused save or delete: beside the right field, the draft kept;
    /// a removed trigger closes the editor and reloads.
    fn rule_refused(
        &mut self,
        instance: u64,
        error: &crate::controllers::agents::AgentsError,
        cx: &mut Context<Self>,
    ) {
        let failure = ctl::rule_failure(error);
        if failure == RuleFailure::Gone {
            self.rule_gone(cx);
            return;
        }
        let Some(editor) = self
            .triggers
            .editor
            .as_mut()
            .filter(|e| e.instance == instance)
        else {
            // Its editor closed meanwhile: say so on the section.
            if let RuleFailure::Trigger(text) | RuleFailure::Form(text) = failure {
                self.errors.insert(Row::RuleEditor, text);
            }
            return;
        };
        match failure {
            RuleFailure::Trigger(text) => editor.errors.trigger = Some(text),
            RuleFailure::Form(text) => editor.form_error = Some(text),
            RuleFailure::Gone => {}
        }
    }

    /// The trigger no longer exists at the service.
    fn rule_gone(&mut self, cx: &mut Context<Self>) {
        self.triggers.editor = None;
        self.pcx.toast(ctl::GONE, cx);
        self.load(cx);
        cx.notify();
    }

    /// Delete trigger… asks first (destructive; focus starts on Cancel).
    pub(crate) fn confirm_delete_rule(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.triggers.editor.as_ref().and_then(|e| e.id.clone()) else {
            return;
        };
        let Some(rule) = self.rule(&id) else {
            self.rule_gone(cx);
            return;
        };
        let (title, body, verb) = ctl::delete_confirm(&rule.trigger);
        let this = cx.entity().downgrade();
        self.pcx.confirm(
            Confirm {
                title: title.into(),
                body: body.into(),
                verb: verb.into(),
                destructive: true,
                on_confirm: Rc::new(move |_, cx| {
                    let id = id.clone();
                    let _ = this.update(cx, |p, cx| p.delete_rule(id, cx));
                }),
            },
            cx,
        );
    }

    fn delete_rule(&mut self, id: String, cx: &mut Context<Self>) {
        if self.load_error.is_some() {
            return;
        }
        let instance = self.editor_instance().unwrap_or(0);
        let Some(generation) = self.begin(Row::RuleEditor, cx) else {
            return;
        };
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.delete_rule(&id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish(generation, cx) {
                    return;
                }
                match result {
                    Ok(()) => {
                        this.triggers.editor = None;
                        this.triggers.opener = None;
                        this.triggers.result = None;
                        this.pcx.toast(ctl::DELETED, cx);
                        this.load(cx);
                    }
                    Err(error) => this.rule_refused(instance, &error, cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A trigger's switch: `PUT {enabled}`, no confirmation.
    pub(crate) fn toggle_rule(&mut self, id: String, on: bool, cx: &mut Context<Self>) {
        if self.load_error.is_some() || self.rule(&id).is_none() {
            return;
        }
        let row = Row::Rule(id.clone());
        let Some(generation) = self.begin(row.clone(), cx) else {
            return;
        };
        self.triggers.pending_rule = Some((id.clone(), on));
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.update_rule(&id, &ctl::enabled_patch(on)) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish(generation, cx) {
                    return;
                }
                this.triggers.pending_rule = None;
                match result {
                    Ok(rule) => {
                        if let Some(loaded) = &mut this.loaded {
                            if let Some(slot) = loaded.rules.iter_mut().find(|r| r.id == rule.id) {
                                *slot = rule;
                            }
                        }
                        this.triggers.result = None;
                    }
                    Err(error) => match ctl::rule_failure(&error) {
                        RuleFailure::Gone => this.rule_gone(cx),
                        RuleFailure::Trigger(text) | RuleFailure::Form(text) => {
                            this.errors.insert(row, text);
                        }
                    },
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ------------------------------------------------------------- try it ---

    fn test_text_changed(&mut self, cx: &mut Context<Self>) {
        self.triggers.test_generation = self.triggers.test_generation.wrapping_add(1);
        self.triggers.result = None;
        self.errors.remove(&Row::Test);
        cx.notify();
    }

    /// Test (or Enter): `POST /hot-commands/test` with `{"text": …}`.
    /// Nothing is started.
    pub(crate) fn run_test(&mut self, cx: &mut Context<Self>) {
        let text = self.triggers.try_input.read(cx).text().to_string();
        if text.trim().is_empty() || self.load_error.is_some() {
            return;
        }
        let Some(generation) = self.begin(Row::Test, cx) else {
            return;
        };
        let sentence = self.triggers.test_generation;
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { backend.test_rule(&text) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.finish(generation, cx) || this.triggers.test_generation != sentence {
                    return;
                }
                let providers = this
                    .loaded
                    .as_ref()
                    .map(|l| l.providers.clone())
                    .unwrap_or_default();
                this.triggers.result = Some(match result {
                    Ok(reply) => ctl::test_copy(&reply, &providers),
                    Err(error) => ctl::test_error(&error),
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ------------------------------------------------------------ fixture ---

    /// The `agents-settings-trigger` scene: once read, test the example
    /// sentence and open the editor on the first enabled trigger.
    pub fn run_trigger_fixture(&mut self, cx: &mut Context<Self>) {
        self.triggers.fixture = true;
        self.triggers.reveal = Reveal::Requested(0);
        if self.loaded.is_some() {
            self.open_trigger_fixture(cx);
        }
    }

    fn open_trigger_fixture(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self
            .rules()
            .iter()
            .find(|r| r.enabled)
            .map(|r| r.id.clone())
        else {
            return;
        };
        self.triggers.fixture = false;
        self.triggers
            .try_input
            .update(cx, |i, cx| i.set_text(ctl::TRY_PLACEHOLDER, cx));
        self.run_test(cx);
        self.open_rule_editor(Some(id), cx);
        self.pending_focus = Some(PendingFocus::SayField);
    }

    /// A requested reveal: scroll the Settings column so the Voice triggers
    /// section starts at its top (clamped to the column's range), from the
    /// position recorded at the last prepaint. Measured again every frame,
    /// since the content above may still grow as replies land; `true` once
    /// the last drawn frame already had it in place (or after
    /// [`REVEAL_FRAMES`] attempts). Without a column (the tab built alone)
    /// there is nothing to scroll.
    pub(super) fn reveal_triggers(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(scroll) = self.triggers.page_scroll.clone() else {
            if self.triggers.reveal != Reveal::Idle {
                self.triggers.reveal = Reveal::Shown(Size::default());
            }
            return true;
        };
        let viewport = scroll.bounds();
        let tries = match self.triggers.reveal {
            Reveal::Idle => return true,
            Reveal::Shown(at) if at == viewport.size => return true,
            Reveal::Shown(_) => 0,
            Reveal::Requested(tries) => tries,
        };
        if let Some(section) = self
            .triggers
            .anchor
            .get()
            .filter(|_| viewport.size.height > px(0.))
        {
            let offset = scroll.offset();
            let y = reveal_offset(
                section.top(),
                viewport.top(),
                offset.y,
                scroll.max_offset().height,
                px(REVEAL_MARGIN),
            );
            if (y - offset.y).abs() < px(0.5) || tries >= REVEAL_FRAMES {
                self.triggers.reveal = Reveal::Shown(viewport.size);
                return true;
            }
            scroll.set_offset(point(offset.x, y));
        }
        // Not laid out yet, or just moved: look again on the next frame.
        self.triggers.reveal = Reveal::Requested(tries + 1);
        cx.on_next_frame(window, |_, _, cx| cx.notify());
        false
    }

    #[cfg(test)]
    pub(crate) fn reveal_done(&self) -> bool {
        matches!(self.triggers.reveal, Reveal::Shown(_))
    }

    /// The trigger scene's end state is on screen.
    pub(super) fn trigger_scene_ready(&self) -> bool {
        self.triggers
            .editor
            .as_ref()
            .is_some_and(|e| e.id.is_some())
            && matches!(self.triggers.result, Some(TestShown::Match { .. }))
    }

    // -------------------------------------------------------- test access ---

    #[cfg(test)]
    pub(crate) fn rules_for_test(&self) -> &[Rule] {
        self.rules()
    }

    #[cfg(test)]
    pub(crate) fn rule_editor(&self) -> Option<&RuleEditor> {
        self.triggers.editor.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn rule_errors(&self) -> Option<(DraftErrors, Option<String>)> {
        self.triggers
            .editor
            .as_ref()
            .map(|e| (e.errors.clone(), e.form_error.clone()))
    }

    #[cfg(test)]
    pub(crate) fn test_result(&self) -> Option<&TestShown> {
        self.triggers.result.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn type_rule(&mut self, trigger: &str, task: &str, cx: &mut Context<Self>) {
        self.triggers
            .say
            .update(cx, |i, cx| i.set_text(trigger, cx));
        self.triggers.task.update(cx, |i, cx| i.set_text(task, cx));
        self.sync_min(cx);
    }

    #[cfg(test)]
    pub(crate) fn choose_rule_agent(&mut self, id: &str, profile: &str, cx: &mut Context<Self>) {
        if let Some(e) = self.triggers.editor.as_mut() {
            e.draft.provider = id.into();
            e.draft.profile = profile.into();
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn choose_rule_repo(&mut self, repo: &str, cx: &mut Context<Self>) {
        if let Some(i) = self
            .triggers
            .workspace_values
            .iter()
            .position(|v| v == repo)
        {
            if let Some(e) = self.triggers.editor.as_mut() {
                e.draft.repo = self.triggers.workspace_values[i].clone();
            }
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn min_select_disabled(&self, cx: &gpui::App) -> (bool, Option<usize>) {
        let s = self.triggers.min.read(cx);
        (s.is_disabled(), s.selected())
    }

    /// Type into Try it as the user would (the text changes).
    #[cfg(test)]
    pub(crate) fn type_test(&mut self, text: &str, cx: &mut Context<Self>) {
        self.triggers
            .try_input
            .update(cx, |i, cx| i.set_text(text, cx));
        self.test_text_changed(cx);
    }

    #[cfg(test)]
    pub(crate) fn unsaved_rule(&self, cx: &gpui::App) -> bool {
        self.rule_dirty(cx)
    }

    // ------------------------------------------------------------- render ---

    pub(super) fn render_triggers(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(loaded) = &self.loaded else {
            return div().into_any_element();
        };
        let agents_on = loaded.settings.enabled;
        let stored = loaded.settings.hot_commands;
        let on = self.triggers.pending_hot.unwrap_or(stored);
        let this = cx.entity().downgrade();
        let switch = Switch::new("agents-hot-commands", on, ctl::SWITCH_LABEL)
            .disabled(!self.writable() || !agents_on)
            .on_toggle(move |on, _, cx| {
                let _ = this.update(cx, |p, cx| p.set_hot_commands(on, cx));
            });
        let note = self
            .note(&Row::HotCommands)
            .or_else(|| (!agents_on).then(|| (Tone::Neutral, SharedString::from(ctl::AGENTS_OFF))));
        let mut rows = vec![ui::setting_row(
            t,
            ctl::SWITCH_LABEL,
            ctl::SWITCH_DESCRIPTION,
            switch.into_any_element(),
            note,
        )
        .into_any_element()];
        let rules = loaded.rules.clone();
        for (i, rule) in rules.iter().enumerate() {
            rows.push(self.rule_row(t, i, rule, cx));
        }
        let new_open = self
            .triggers
            .editor
            .as_ref()
            .is_some_and(|e| e.id.is_none());
        if new_open {
            rows.push(
                div()
                    .px(ui::row_inset())
                    .py(space::s5())
                    .child(self.rule_editor_view(t, cx))
                    .into_any_element(),
            );
        } else if rules.len() < ctl::MAX_RULES {
            let mut add = Button::new("agents-rule-add", ctl::ADD_TRIGGER)
                .ghost()
                .small()
                .icon("plus")
                .disabled(!self.writable())
                .on_click(cx.listener(|this, _, window, cx| this.open_rule(None, window, cx)));
            if self.triggers.opener == Some(None) {
                add = add.track_focus(&self.triggers.opener_focus);
            }
            rows.push(
                div()
                    .debug_selector(|| "agents-rule-add-row".into())
                    .flex()
                    .px(ui::row_inset())
                    .py(space::s4())
                    .child(add)
                    .into_any_element(),
            );
        } else {
            rows.push(
                div()
                    .px(ui::row_inset())
                    .py(space::s5())
                    .child(ui::supporting(t, ctl::TOO_MANY))
                    .into_any_element(),
            );
        }
        if let Some(e) = self.errors.get(&Row::RuleEditor).cloned() {
            rows.push(
                div()
                    .px(ui::row_inset())
                    .py(space::s4())
                    .child(ui::error_line(t, e))
                    .into_any_element(),
            );
        }
        rows.push(self.try_row(t, cx));
        let anchor = self.triggers.anchor.clone();
        ui::section(t, ctl::HEADING, Some(ctl::SUPPORT.into()), None)
            .debug_selector(|| "agents-triggers".into())
            .relative()
            // Where the section is, for bringing it into view (no paint).
            .child(
                canvas(
                    move |bounds, _, _| anchor.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size(px(1.)),
            )
            .child(ui::group_rows(t, rows, ui::row_inset()))
            .into_any_element()
    }

    fn rule_row(
        &self,
        t: &Theme,
        index: usize,
        rule: &Rule,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(loaded) = &self.loaded else {
            return div().into_any_element();
        };
        let row = Row::Rule(rule.id.clone());
        let busy = self.busy_row(&row);
        let on = match &self.triggers.pending_rule {
            Some((id, on)) if *id == rule.id => *on,
            _ => rule.enabled,
        };
        let summary = ctl::rule_summary(rule, &loaded.providers, &loaded.settings);
        let note = if let Some(e) = self.errors.get(&row) {
            Some(ui::error_line(t, e.clone()))
        } else {
            ctl::rule_warning(rule, &loaded.providers, &loaded.settings)
                .map(|w| ui::supporting(t, w).text_color(t.tone(Tone::Warning).0))
        };
        let editing = self
            .triggers
            .editor
            .as_ref()
            .is_some_and(|e| e.id.as_deref() == Some(rule.id.as_str()));
        let this = cx.entity().downgrade();
        let id = rule.id.clone();
        let switch = Switch::new(
            ("agents-rule-switch", index as u64),
            on,
            rule.trigger.clone(),
        )
        .disabled(!self.writable())
        .on_toggle(move |on, _, cx| {
            let id = id.clone();
            let _ = this.update(cx, |p, cx| p.toggle_rule(id, on, cx));
        });
        let id = rule.id.clone();
        let mut edit =
            Button::new(("agents-rule-edit", index as u64), ctl::EDIT)
                .small()
                .busy(busy)
                .disabled(!self.writable() && !busy)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_rule(Some(id.clone()), window, cx)
                }));
        if self.triggers.opener.as_ref() == Some(&Some(rule.id.clone())) {
            edit = edit.track_focus(&self.triggers.opener_focus);
        }
        let mut item = div()
            .debug_selector({
                let key = format!("agents-rule:{}", rule.id);
                move || key.clone()
            })
            .flex()
            .flex_col()
            .gap(space::s4())
            .w_full()
            .px(ui::row_inset())
            .py(space::s5())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(space::s8())
                    .w_full()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(space::s1())
                            .flex_1()
                            .min_w_0()
                            .child(ui::label(t, ctl::title(&rule.trigger)))
                            .child(
                                ui::supporting(t, summary)
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis(),
                            )
                            .children(note),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap(space::s4())
                            .child(switch)
                            .child(edit),
                    ),
            );
        if editing {
            item = item.child(self.rule_editor_view(t, cx));
        }
        item.into_any_element()
    }

    fn field(
        t: &Theme,
        label: &'static str,
        control: gpui::AnyElement,
        help: Option<String>,
        error: Option<String>,
    ) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(space::s2())
            .min_w_0()
            .child(ui::label(t, label))
            .child(control)
            .when_some(error, |d, e| d.child(ui::error_line(t, e)))
            .when_some(help, |d, h| d.child(ui::supporting(t, h)))
    }

    /// A text field across the editor's width. A percentage width (`w_full`)
    /// around the input resolves against a width-auto column and collapses;
    /// the input instead fills a flex row (`flex_auto` + `min_w_0`), the
    /// same pattern as the key field.
    fn wide(selector: &'static str, input: Entity<TextInput>) -> gpui::AnyElement {
        div()
            .debug_selector(move || selector.into())
            .flex()
            .w_full()
            .min_w_0()
            .child(div().flex_auto().min_w_0().child(input))
            .into_any_element()
    }

    fn rule_editor_view(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (Some(editor), Some(loaded)) = (&self.triggers.editor, &self.loaded) else {
            return div().into_any_element();
        };
        let busy = self.busy_row(&Row::RuleEditor);
        let blocked = self.op.is_some() || self.load_error.is_some();
        let say = Self::field(
            t,
            ctl::SAY_LABEL,
            Self::wide("agents-rule-say", self.triggers.say.clone()),
            Some(ctl::SAY_HELP.into()),
            editor.errors.trigger.clone(),
        )
        .w_full();
        let agent = Self::field(
            t,
            ctl::AGENT_LABEL,
            self.triggers.agent.clone().into_any_element(),
            None,
            None,
        );
        let workspace = Self::field(
            t,
            ctl::WORKSPACE_LABEL,
            self.triggers.workspace.clone().into_any_element(),
            ctl::workspace_help(&loaded.settings),
            None,
        );
        let task = Self::field(
            t,
            ctl::TASK_LABEL,
            Self::wide("agents-rule-task", self.triggers.task.clone()),
            Some(ctl::TASK_HELP.into()),
            editor.errors.task.clone(),
        )
        .w_full();
        let min = Self::field(
            t,
            ctl::MIN_LABEL,
            self.triggers.min.clone().into_any_element(),
            Some(ctl::MIN_HELP.into()),
            None,
        );
        let delete = editor.id.is_some().then(|| {
            Button::new("agents-rule-delete", ctl::DELETE)
                .destructive()
                .small()
                .disabled(blocked)
                .on_click(cx.listener(|this, _, _, cx| this.confirm_delete_rule(cx)))
        });
        let buttons = div()
            .flex()
            .items_center()
            .gap(space::s4())
            .w_full()
            .children(delete)
            .child(div().flex_auto())
            .child(
                Button::new("agents-rule-cancel", ctl::CANCEL)
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_rule(cx))),
            )
            .child(
                Button::new("agents-rule-save", ctl::SAVE)
                    .primary()
                    .small()
                    .busy(busy)
                    .disabled(blocked && !busy)
                    .on_click(cx.listener(|this, _, _, cx| this.save_rule(cx))),
            );
        div()
            .debug_selector(|| "agents-rule-editor".into())
            .flex()
            .flex_col()
            .gap(space::s5())
            .w_full()
            .p(space::s6())
            .rounded(crate::theme::size::radius_inner())
            .bg(t.bg_muted)
            .child(say)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(space::s6())
                    .child(agent)
                    .child(workspace),
            )
            .child(task)
            .child(min)
            .child(buttons)
            .when_some(editor.form_error.clone(), |d, e| {
                d.child(ui::error_line(t, e))
            })
            .into_any_element()
    }

    fn try_row(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let busy = self.busy_row(&Row::Test);
        let empty = self.triggers.try_input.read(cx).text().trim().is_empty();
        let mut row = div()
            .debug_selector(|| "agents-rule-try".into())
            .flex()
            .flex_col()
            .gap(space::s3())
            .w_full()
            .px(ui::row_inset())
            .py(space::s5())
            .child(ui::label(t, ctl::TRY_LABEL))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::s4())
                    .w_full()
                    .child(
                        div()
                            .flex_auto()
                            .min_w_0()
                            .child(self.triggers.try_input.clone()),
                    )
                    .child(
                        Button::new("agents-rule-test", ctl::TEST)
                            .small()
                            .busy(busy)
                            .disabled(
                                empty
                                    || (!busy && (self.op.is_some() || self.load_error.is_some())),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.run_test(cx))),
                    ),
            );
        if self.rule_dirty(cx) {
            row = row.child(ui::supporting(t, ctl::UNSAVED_NOTE));
        }
        match &self.triggers.result {
            Some(TestShown::Match { summary, prompt }) => {
                row = row
                    .child(
                        div()
                            .debug_selector(|| "agents-rule-result".into())
                            .flex()
                            .items_center()
                            .gap(space::s4())
                            .child(ui::token(t, Tone::Success, ctl::MATCHES))
                            .child(ui::secondary(t, summary.clone())),
                    )
                    .child(
                        ui::group(t).child(
                            div()
                                .px(space::s5())
                                .py(space::s4())
                                .child(ui::body(t, prompt.clone())),
                        ),
                    )
                    .child(ui::supporting(t, ctl::NOTHING_STARTED));
            }
            Some(TestShown::Note(text)) => {
                row = row.child(
                    ui::secondary(t, text.clone()).debug_selector(|| "agents-rule-result".into()),
                );
            }
            Some(TestShown::Error(text)) => {
                row = row.child(
                    ui::error_line(t, text.clone()).debug_selector(|| "agents-rule-result".into()),
                );
            }
            None => {}
        }
        row.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reveal_offset_puts_the_section_at_the_top_within_range() {
        // Section 900 px into a column whose viewport starts at 60: scroll
        // up by 900 − 60 − 16.
        assert_eq!(
            reveal_offset(px(900.), px(60.), px(0.), px(2000.), px(16.)),
            px(-824.)
        );
        // Already scrolled by 100: the same place in content.
        assert_eq!(
            reveal_offset(px(800.), px(60.), px(-100.), px(2000.), px(16.)),
            px(-824.)
        );
        // Clamped to what the column can scroll, and never below 0.
        assert_eq!(
            reveal_offset(px(900.), px(60.), px(0.), px(300.), px(16.)),
            px(-300.)
        );
        assert_eq!(
            reveal_offset(px(50.), px(60.), px(0.), px(300.), px(16.)),
            px(0.)
        );
    }
}
