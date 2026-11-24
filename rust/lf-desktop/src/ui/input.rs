//! Text entry built on GPUI's input-handler interface (GPUI core ships no
//! text field). Single-line, multi-line (soft-wrapped) and masked secret
//! modes; Unicode grapheme movement, IME composition, mouse selection,
//! clipboard, word movement and undo.

use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, relative, size, App, Bounds, ClipboardItem,
    ContentMask, Context, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, Hsla, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ScrollWheelEvent,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine,
};

use super::text_edit::{self, EditBuffer};
use crate::theme::{size as tsize, space, text, theme};

actions!(
    text_input,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        Left,
        Right,
        Up,
        Down,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Paste,
        Cut,
        Copy,
        Undo,
        Enter,
        NewLine,
    ]
);

const CONTEXT: &str = "TextInput";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("shift-backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("ctrl-backspace", DeleteWordLeft, c),
        KeyBinding::new("alt-backspace", DeleteWordLeft, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("ctrl-left", WordLeft, c),
        KeyBinding::new("ctrl-right", WordRight, c),
        KeyBinding::new("alt-left", WordLeft, c),
        KeyBinding::new("alt-right", WordRight, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("ctrl-shift-left", SelectWordLeft, c),
        KeyBinding::new("ctrl-shift-right", SelectWordRight, c),
        KeyBinding::new("alt-shift-left", SelectWordLeft, c),
        KeyBinding::new("alt-shift-right", SelectWordRight, c),
        KeyBinding::new("secondary-a", SelectAll, c),
        KeyBinding::new("home", Home, c),
        KeyBinding::new("end", End, c),
        KeyBinding::new("cmd-left", Home, c),
        KeyBinding::new("cmd-right", End, c),
        KeyBinding::new("shift-home", SelectHome, c),
        KeyBinding::new("shift-end", SelectEnd, c),
        KeyBinding::new("secondary-v", Paste, c),
        KeyBinding::new("shift-insert", Paste, c),
        KeyBinding::new("secondary-c", Copy, c),
        KeyBinding::new("ctrl-insert", Copy, c),
        KeyBinding::new("secondary-x", Cut, c),
        KeyBinding::new("secondary-z", Undo, c),
        KeyBinding::new("enter", Enter, c),
        KeyBinding::new("shift-enter", NewLine, c),
    ]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Changed,
    /// Enter in a single-line field (or in a multi-line field with
    /// `submit_on_enter`).
    Submit,
}

struct Layout {
    /// Shaped paragraphs (display text split on '\n').
    lines: Vec<WrappedLine>,
    /// Display byte offset where each paragraph starts.
    starts: Vec<usize>,
    /// y offset of each paragraph from the text origin.
    tops: Vec<Pixels>,
    line_height: Pixels,
    /// Origin of the text, already offset by the scroll position, so window
    /// to text mapping (clicks, IME bounds) is correct when scrolled.
    origin: Point<Pixels>,
    bounds: Bounds<Pixels>,
    content_height: Pixels,
}

pub struct TextInput {
    focus: FocusHandle,
    buf: EditBuffer,
    placeholder: SharedString,
    marked: Option<Range<usize>>,
    multiline: bool,
    submit_on_enter: bool,
    masked: bool,
    disabled: bool,
    invalid: bool,
    max_chars: Option<usize>,
    min_rows: usize,
    max_rows: usize,
    scroll_x: Pixels,
    /// Vertical scroll of a multi-line field (bounded to the content).
    scroll_y: Pixels,
    /// Bring the caret into view on the next paint (after edits and moves,
    /// not after the user scrolled with the wheel).
    autoscroll: bool,
    selecting: bool,
    layout: Option<Layout>,
    accessible_label: SharedString,
}

impl EventEmitter<InputEvent> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        TextInput {
            focus: cx.focus_handle().tab_stop(true),
            buf: EditBuffer::new(false),
            placeholder: placeholder.into(),
            marked: None,
            multiline: false,
            submit_on_enter: false,
            masked: false,
            disabled: false,
            invalid: false,
            max_chars: None,
            min_rows: 3,
            max_rows: 10,
            scroll_x: px(0.),
            scroll_y: px(0.),
            autoscroll: true,
            selecting: false,
            layout: None,
            accessible_label: SharedString::default(),
        }
    }

    pub fn multiline(mut self, min_rows: usize, max_rows: usize) -> Self {
        self.multiline = true;
        self.buf.multiline = true;
        self.min_rows = min_rows.max(1);
        self.max_rows = max_rows.max(self.min_rows);
        self
    }

    pub fn submit_on_enter(mut self) -> Self {
        self.submit_on_enter = true;
        self
    }

    pub fn masked(mut self) -> Self {
        self.masked = true;
        // No undo snapshots: a secret is never copied around in memory.
        self.buf.keep_undo = false;
        self
    }

    pub fn max_chars(mut self, max: usize) -> Self {
        self.max_chars = Some(max);
        self.buf.max_chars = Some(max);
        self
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.accessible_label = label.into();
        self
    }

    pub fn text(&self) -> &str {
        &self.buf.text
    }

    pub fn is_empty(&self) -> bool {
        self.buf.text.is_empty()
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Replace the text without emitting `Changed` (programmatic loads).
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        if self.buf.set_text(text.into()) {
            self.marked = None;
            self.autoscroll = true;
            cx.notify();
        }
    }

    /// Clear the text and its undo history (used after a secret is saved).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text(String::new(), cx);
        self.buf.clear_undo();
    }

    pub fn set_placeholder(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = text.into();
        cx.notify();
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled != disabled {
            self.disabled = disabled;
            cx.notify();
        }
    }

    pub fn set_invalid(&mut self, invalid: bool, cx: &mut Context<Self>) {
        if self.invalid != invalid {
            self.invalid = invalid;
            cx.notify();
        }
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    // ------------------------------------------------------------ editing ---

    fn cursor(&self) -> usize {
        self.buf.cursor()
    }

    fn moved(&mut self, cx: &mut Context<Self>) {
        self.autoscroll = true;
        cx.notify();
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.buf.move_to(offset);
        self.moved(cx);
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.buf.select_to(offset);
        self.moved(cx);
    }

    fn changed(&mut self, changed: bool, cx: &mut Context<Self>) {
        if changed {
            self.marked = None;
            self.autoscroll = true;
            cx.emit(InputEvent::Changed);
            cx.notify();
        }
    }

    fn edit(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        let changed = self.buf.edit(range, new_text);
        self.changed(changed, cx);
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled {
            let changed = self.buf.backspace();
            self.changed(changed, cx);
        }
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled {
            let changed = self.buf.delete_forward();
            self.changed(changed, cx);
        }
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled {
            let changed = self.buf.delete_word_left(self.masked);
            self.changed(changed, cx);
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.buf.selected.is_empty() {
            self.buf.prev_boundary(self.cursor())
        } else {
            self.buf.selected.start
        };
        self.move_to(to, cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.buf.selected.is_empty() {
            self.buf.next_boundary(self.cursor())
        } else {
            self.buf.selected.end
        };
        self.move_to(to, cx);
    }

    fn vertical(&self, offset: usize, delta: i32) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let pos = self.position_for(offset)?;
        let target = point(
            pos.x,
            pos.y + layout.line_height * (delta as f32) + layout.line_height / 2.,
        );
        if target.y < px(0.) {
            return Some(0);
        }
        Some(self.index_for_local(target))
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            cx.propagate();
            return;
        }
        let to = self.vertical(self.cursor(), -1).unwrap_or(0);
        self.move_to(to, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            cx.propagate();
            return;
        }
        let to = self
            .vertical(self.cursor(), 1)
            .unwrap_or(self.buf.text.len());
        self.move_to(to, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.multiline {
            self.vertical(self.cursor(), -1).unwrap_or(0)
        } else {
            0
        };
        self.select_to(to, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.buf.text.len();
        let to = if self.multiline {
            self.vertical(self.cursor(), 1).unwrap_or(len)
        } else {
            len
        };
        self.select_to(to, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buf.prev_word(self.cursor()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buf.next_word(self.cursor()), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.prev_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.next_boundary(self.cursor()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.prev_word(self.cursor()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.next_word(self.cursor()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.buf.select_all();
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buf.line_start(self.cursor()), cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buf.line_end(self.cursor()), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.line_start(self.cursor()), cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.line_end(self.cursor()), cx);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let range = self.marked.clone().unwrap_or(self.buf.selected.clone());
            self.edit(range, &text, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.buf.selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.buf.selected_text().to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.buf.selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.buf.selected_text().to_string(),
            ));
            self.edit(self.buf.selected.clone(), "", cx);
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        let changed = self.buf.undo();
        self.changed(changed, cx);
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline && !self.submit_on_enter {
            self.edit(self.buf.selected.clone(), "\n", cx);
        } else {
            cx.emit(InputEvent::Submit);
        }
    }

    fn new_line(&mut self, _: &NewLine, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.edit(self.buf.selected.clone(), "\n", cx);
        } else {
            cx.emit(InputEvent::Submit);
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            return;
        }
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let delta = event.delta.pixel_delta(layout.line_height).y;
        let content: f32 = layout.content_height.into();
        let view: f32 = layout.bounds.size.height.into();
        let before: f32 = self.scroll_y.into();
        let after = text_edit::clamp_scroll(before - f32::from(delta), view, content);
        if after != before {
            self.scroll_y = px(after);
            self.autoscroll = false;
            cx.stop_propagation();
            cx.notify();
        }
    }

    // --------------------------------------------------------------- mouse ---

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        self.selecting = true;
        let index = self.index_for_window(event.position);
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else if event.click_count == 2 {
            let word = self.buf.word_at(index);
            self.buf.selected = word;
            self.buf.reversed = false;
            cx.notify();
        } else if event.click_count >= 3 {
            self.buf.selected = self.buf.line_start(index)..self.buf.line_end(index);
            self.buf.reversed = false;
            cx.notify();
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            let index = self.index_for_window(event.position);
            self.select_to(index, cx);
        }
    }

    // ------------------------------------------------------ display mapping ---

    /// Display text and a mapping flag: masked text shows one bullet per char.
    fn display_text(&self) -> String {
        if self.masked {
            "•".repeat(self.buf.text.chars().count())
        } else {
            self.buf.text.clone()
        }
    }

    fn to_display(&self, offset: usize) -> usize {
        if self.masked {
            self.buf.text[..offset.min(self.buf.text.len())]
                .chars()
                .count()
                * '•'.len_utf8()
        } else {
            offset
        }
    }

    fn content_offset(&self, offset: usize) -> usize {
        if self.masked {
            let chars = offset / '•'.len_utf8();
            self.buf
                .text
                .char_indices()
                .nth(chars)
                .map_or(self.buf.text.len(), |(i, _)| i)
        } else {
            offset.min(self.buf.text.len())
        }
    }

    /// Position (relative to the text origin) of a content offset.
    fn position_for(&self, offset: usize) -> Option<Point<Pixels>> {
        let layout = self.layout.as_ref()?;
        let d = self.to_display(offset);
        let mut para = 0;
        for (i, start) in layout.starts.iter().enumerate() {
            if *start <= d {
                para = i;
            }
        }
        let line = layout.lines.get(para)?;
        let local = (d - layout.starts[para]).min(line.len());
        let p = line.position_for_index(local, layout.line_height)?;
        Some(point(p.x, p.y + layout.tops[para]))
    }

    fn index_for_local(&self, local: Point<Pixels>) -> usize {
        let Some(layout) = self.layout.as_ref() else {
            return self.buf.text.len();
        };
        if layout.lines.is_empty() {
            return 0;
        }
        let mut para = 0;
        for (i, top) in layout.tops.iter().enumerate() {
            if local.y >= *top {
                para = i;
            }
        }
        let line = &layout.lines[para];
        let inner = point(
            local.x.max(px(0.)),
            (local.y - layout.tops[para]).max(px(0.)),
        );
        let height = line.size(layout.line_height).height;
        let inner = point(inner.x, inner.y.min(height - px(1.)));
        let idx = match line.closest_index_for_position(inner, layout.line_height) {
            Ok(i) | Err(i) => i,
        };
        self.content_offset(layout.starts[para] + idx)
    }

    fn index_for_window(&self, position: Point<Pixels>) -> usize {
        let Some(layout) = self.layout.as_ref() else {
            return self.buf.text.len();
        };
        if position.y < layout.bounds.top() {
            return 0;
        }
        if position.y > layout.bounds.bottom() && !self.multiline {
            return self.buf.text.len();
        }
        self.index_for_local(point(
            position.x - layout.origin.x,
            position.y - layout.origin.y,
        ))
    }

    // ------------------------------------------------------------- utf-16 ---

    fn offset_from_utf16(&self, offset: usize) -> usize {
        self.buf.offset_from_utf16(offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        self.buf.offset_to_utf16(offset)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        if self.masked {
            return Some("•".repeat(self.buf.text[range].chars().count()));
        }
        Some(self.buf.text[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.buf.selected),
            reversed: self.buf.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked.clone())
            .unwrap_or(self.buf.selected.clone());
        self.edit(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked.clone())
            .unwrap_or(self.buf.selected.clone());
        self.buf.text.replace_range(range.clone(), new_text);
        self.marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.autoscroll = true;
        self.buf.selected = new_selected_range_utf16
            .as_ref()
            .map(|r| {
                // The selection is relative to the inserted (marked) text.
                let to8 = |u16off: usize| {
                    let mut utf8 = 0;
                    let mut utf16 = 0;
                    for ch in new_text.chars() {
                        if utf16 >= u16off {
                            break;
                        }
                        utf16 += ch.len_utf16();
                        utf8 += ch.len_utf8();
                    }
                    utf8
                };
                range.start + to8(r.start)..range.start + to8(r.end)
            })
            .unwrap_or_else(|| {
                let end = range.start + new_text.len();
                end..end
            });
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let layout = self.layout.as_ref()?;
        let start = self.position_for(range.start)?;
        let end = self.position_for(range.end)?;
        Some(Bounds::from_corners(
            point(layout.origin.x + start.x, layout.origin.y + start.y),
            point(
                layout.origin.x + end.x.max(start.x + px(1.)),
                layout.origin.y + end.y + layout.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.index_for_window(point);
        Some(self.offset_to_utf16(index))
    }
}

/// The element that shapes and paints the text, caret and selection.
struct TextElement {
    input: Entity<TextInput>,
    color: Hsla,
    placeholder_color: Hsla,
    selection: Hsla,
    caret: Hsla,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

struct Prepaint {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    tops: Vec<Pixels>,
    origin: Point<Pixels>,
    quads: Vec<PaintQuad>,
    caret: Option<PaintQuad>,
    line_height: Pixels,
    scroll_x: Pixels,
    scroll_y: Pixels,
    content_height: Pixels,
}

fn shape(
    window: &mut Window,
    display: &str,
    placeholder: bool,
    color: Hsla,
    marked: Option<Range<usize>>,
    wrap: Option<Pixels>,
) -> (Vec<WrappedLine>, Vec<usize>) {
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());
    let base = TextRun {
        len: display.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = match marked.filter(|_| !placeholder) {
        Some(m) if m.end <= display.len() => vec![
            TextRun {
                len: m.start,
                ..base.clone()
            },
            TextRun {
                len: m.end - m.start,
                underline: Some(UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.),
                    wavy: false,
                }),
                ..base.clone()
            },
            TextRun {
                len: display.len() - m.end,
                ..base
            },
        ]
        .into_iter()
        .filter(|r| r.len > 0)
        .collect::<Vec<_>>(),
        _ => vec![base],
    };
    let shared: SharedString = display.to_string().into();
    let lines: Vec<WrappedLine> = window
        .text_system()
        .shape_text(shared, font_size, &runs, wrap, None)
        .map(|l| l.into_iter().collect())
        .unwrap_or_default();
    let mut starts = Vec::with_capacity(lines.len());
    let mut at = 0;
    for line in &lines {
        starts.push(at);
        at += line.len() + 1;
    }
    (lines, starts)
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let input = self.input.read(cx);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let line_height = window.line_height();
        if !input.multiline {
            style.size.height = line_height.into();
            return (window.request_layout(style, [], cx), ());
        }
        let display = if input.buf.text.is_empty() {
            input.placeholder.to_string()
        } else {
            input.display_text()
        };
        let (min_rows, max_rows) = (input.min_rows, input.max_rows);
        let text_style = window.text_style();
        let layout =
            window.request_measured_layout(style, move |_known, available, window, _cx| {
                let width = match available.width {
                    gpui::AvailableSpace::Definite(w) => Some(w),
                    _ => None,
                };
                let font_size = text_style.font_size.to_pixels(window.rem_size());
                let run = TextRun {
                    len: display.len(),
                    font: text_style.font(),
                    color: text_style.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let rows: usize = window
                    .text_system()
                    .shape_text(display.clone().into(), font_size, &[run], width, None)
                    .map(|lines| lines.iter().map(|l| l.wrap_boundaries().len() + 1).sum())
                    .unwrap_or(1);
                let rows = rows.clamp(min_rows, max_rows);
                size(width.unwrap_or(px(200.)), line_height * rows as f32)
            });
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let input = self.input.read(cx);
        let placeholder = input.buf.text.is_empty();
        let display = if placeholder {
            input.placeholder.to_string()
        } else {
            input.display_text()
        };
        let color = if placeholder {
            self.placeholder_color
        } else {
            self.color
        };
        let marked = input
            .marked
            .clone()
            .map(|m| input.to_display(m.start)..input.to_display(m.end));
        let wrap = input.multiline.then_some(bounds.size.width);
        let (lines, starts) = shape(window, &display, placeholder, color, marked, wrap);
        let line_height = window.line_height();
        let mut tops = Vec::with_capacity(lines.len());
        let mut y = px(0.);
        for line in &lines {
            tops.push(y);
            y += line.size(line_height).height;
        }
        let sel = if placeholder {
            0..0
        } else {
            input.to_display(input.buf.selected.start)..input.to_display(input.buf.selected.end)
        };
        let cursor = if placeholder {
            0
        } else {
            input.to_display(input.cursor())
        };
        let locate = |offset: usize| -> Point<Pixels> {
            let mut para = 0;
            for (i, s) in starts.iter().enumerate() {
                if *s <= offset {
                    para = i;
                }
            }
            match lines.get(para) {
                Some(line) => {
                    let local = (offset - starts[para]).min(line.len());
                    let p = line
                        .position_for_index(local, line_height)
                        .unwrap_or(point(px(0.), px(0.)));
                    point(p.x, p.y + tops[para])
                }
                None => point(px(0.), px(0.)),
            }
        };
        // Horizontal scroll for single-line fields keeps the caret visible.
        let mut scroll_x = input.scroll_x;
        if !input.multiline {
            let cx_pos = locate(cursor).x;
            let width = bounds.size.width - px(2.);
            if cx_pos - scroll_x > width {
                scroll_x = cx_pos - width;
            }
            if cx_pos < scroll_x {
                scroll_x = cx_pos;
            }
            let full = lines.first().map_or(px(0.), |l| l.width());
            if full <= width {
                scroll_x = px(0.);
            }
        }
        // Vertical scroll for multi-line fields: bounded, follows the caret
        // after edits and moves, otherwise stays where the wheel left it.
        let content_height = y;
        let mut scroll_y = input.scroll_y;
        if input.multiline {
            let view: f32 = bounds.size.height.into();
            let content: f32 = content_height.into();
            let lh: f32 = line_height.into();
            let current: f32 = scroll_y.into();
            let next = if input.autoscroll {
                let caret_top: f32 = locate(cursor).y.into();
                text_edit::follow_caret(current, caret_top, lh, view, content)
            } else {
                text_edit::clamp_scroll(current, view, content)
            };
            scroll_y = px(next);
        } else {
            scroll_y = px(0.);
        }
        let origin = point(bounds.left() - scroll_x, bounds.top() - scroll_y);
        let mut quads = Vec::new();
        if !sel.is_empty() {
            let a = locate(sel.start);
            let b = locate(sel.end);
            if a.y == b.y {
                quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + a.x, origin.y + a.y),
                        point(origin.x + b.x, origin.y + a.y + line_height),
                    ),
                    self.selection,
                ));
            } else {
                quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + a.x, origin.y + a.y),
                        point(bounds.right(), origin.y + a.y + line_height),
                    ),
                    self.selection,
                ));
                if b.y > a.y + line_height {
                    quads.push(fill(
                        Bounds::from_corners(
                            point(bounds.left(), origin.y + a.y + line_height),
                            point(bounds.right(), origin.y + b.y),
                        ),
                        self.selection,
                    ));
                }
                quads.push(fill(
                    Bounds::from_corners(
                        point(bounds.left(), origin.y + b.y),
                        point(origin.x + b.x, origin.y + b.y + line_height),
                    ),
                    self.selection,
                ));
            }
        }
        let caret = sel.is_empty().then(|| {
            let p = locate(cursor);
            fill(
                Bounds::new(
                    point(origin.x + p.x, origin.y + p.y),
                    size(px(1.5), line_height),
                ),
                self.caret,
            )
        });
        Prepaint {
            lines,
            starts,
            tops,
            origin,
            quads,
            caret,
            line_height,
            scroll_x,
            scroll_y,
            content_height,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        let focused = focus.is_focused(window);
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let origin = prepaint.origin;
        let line_height = prepaint.line_height;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            if focused {
                for quad in prepaint.quads.drain(..) {
                    window.paint_quad(quad);
                }
            }
            for (line, top) in prepaint.lines.iter().zip(prepaint.tops.iter()) {
                let _ = line.paint(
                    point(origin.x, origin.y + *top),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            if focused {
                if let Some(caret) = prepaint.caret.take() {
                    window.paint_quad(caret);
                }
            }
        });
        let lines = std::mem::take(&mut prepaint.lines);
        let starts = std::mem::take(&mut prepaint.starts);
        let tops = std::mem::take(&mut prepaint.tops);
        let scroll_x = prepaint.scroll_x;
        let scroll_y = prepaint.scroll_y;
        let content_height = prepaint.content_height;
        self.input.update(cx, |input, _| {
            input.scroll_x = scroll_x;
            input.scroll_y = scroll_y;
            input.autoscroll = false;
            input.layout = Some(Layout {
                lines,
                starts,
                tops,
                line_height,
                origin,
                bounds,
                content_height,
            });
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let focused = self.focus.is_focused(window);
        let (font, line) = if self.multiline {
            text::reading()
        } else {
            text::body()
        };
        let border = if self.invalid {
            t.tone(crate::theme::Tone::Error).0
        } else if focused {
            t.focus
        } else {
            t.border_control
        };
        let element = TextElement {
            input: cx.entity(),
            color: if self.disabled {
                t.text_disabled
            } else {
                t.text
            },
            placeholder_color: t.text_secondary,
            selection: t.accent.opacity(0.28),
            caret: t.accent,
        };
        let mut root = div()
            .id(ElementId::Name(
                format!("text-input-{}", cx.entity_id()).into(),
            ))
            .debug_selector(|| format!("text-input-{}", self.placeholder))
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .w_full()
            .flex()
            .items_center()
            .px(space::s5())
            .rounded(tsize::radius_element())
            .border_1()
            .border_color(border)
            .bg(if self.disabled {
                t.bg_muted
            } else {
                t.bg_control
            })
            .text_size(font)
            .line_height(line)
            .text_color(t.text);
        if self.multiline {
            root = root.py(space::s4()).items_start();
        } else {
            root = root.h(tsize::control_md());
        }
        if focused {
            // Neutral edge plus the shared focus ring (no layout shift).
            root = root.shadow(super::ring(&t));
        }
        if !self.disabled {
            root = root
                .on_action(cx.listener(Self::backspace))
                .on_action(cx.listener(Self::delete))
                .on_action(cx.listener(Self::delete_word_left))
                .on_action(cx.listener(Self::paste))
                .on_action(cx.listener(Self::cut))
                .on_action(cx.listener(Self::undo))
                .on_action(cx.listener(Self::enter))
                .on_action(cx.listener(Self::new_line));
        }
        root.on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(div().flex_1().min_w_0().overflow_hidden().child(element))
    }
}
