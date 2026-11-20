//! GPUI-free text editing state behind `ui::input::TextInput`: the text, the
//! selection, grapheme/word/line movement, edits with newline normalisation
//! and length limits, undo, UTF-16 mapping for IME, and the vertical
//! scroll-follow arithmetic. Kept separate so it is unit-tested without a
//! window or text system.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// Undo depth (snapshots).
const UNDO_LIMIT: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditBuffer {
    pub text: String,
    /// Byte range, always on char (and in practice grapheme) boundaries.
    pub selected: Range<usize>,
    /// The caret is at `selected.start` when true.
    pub reversed: bool,
    pub multiline: bool,
    pub max_chars: Option<usize>,
    /// When false no snapshots are kept (secret fields keep no extra copies).
    pub keep_undo: bool,
    undo: Vec<(String, Range<usize>)>,
}

impl EditBuffer {
    pub fn new(multiline: bool) -> Self {
        EditBuffer {
            multiline,
            keep_undo: true,
            ..Default::default()
        }
    }

    pub fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selected.clone()]
    }

    /// Replace everything (programmatic load); clears undo.
    pub fn set_text(&mut self, text: String) -> bool {
        if self.text == text {
            return false;
        }
        self.text = text;
        let end = self.text.len();
        self.selected = end..end;
        self.reversed = false;
        self.clear_undo();
        true
    }

    pub fn clear_undo(&mut self) {
        self.undo.clear();
        self.undo.shrink_to_fit();
    }

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    // -------------------------------------------------------------- moves ---

    pub fn move_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        self.selected = offset..offset;
        self.reversed = false;
    }

    pub fn select_to(&mut self, offset: usize) {
        let offset = offset.min(self.text.len());
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
    }

    pub fn select_all(&mut self) {
        self.selected = 0..self.text.len();
        self.reversed = false;
    }

    pub fn prev_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }

    pub fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(self.text.len())
    }

    pub fn prev_word(&self, offset: usize) -> usize {
        let mut last = 0;
        for (i, w) in self.text.split_word_bound_indices() {
            if i >= offset {
                break;
            }
            if !w.trim().is_empty() {
                last = i;
            }
        }
        last
    }

    pub fn next_word(&self, offset: usize) -> usize {
        for (i, w) in self.text.split_word_bound_indices() {
            let end = i + w.len();
            if end > offset && !w.trim().is_empty() {
                return end;
            }
        }
        self.text.len()
    }

    /// The word around `offset` (double click).
    pub fn word_at(&self, offset: usize) -> Range<usize> {
        for (i, w) in self.text.split_word_bound_indices() {
            if offset >= i && offset < i + w.len() {
                return i..i + w.len();
            }
        }
        let end = self.text.len();
        end..end
    }

    pub fn line_start(&self, offset: usize) -> usize {
        if !self.multiline {
            return 0;
        }
        let offset = offset.min(self.text.len());
        self.text[..offset].rfind('\n').map_or(0, |i| i + 1)
    }

    pub fn line_end(&self, offset: usize) -> usize {
        if !self.multiline {
            return self.text.len();
        }
        let offset = offset.min(self.text.len());
        self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |i| offset + i)
    }

    // -------------------------------------------------------------- edits ---

    /// Text as it may be inserted: CRLF folded (multi-line) or line breaks
    /// turned into spaces (single line), then cut to the length limit.
    pub fn prepare_insert(&self, range: &Range<usize>, new_text: &str) -> String {
        let mut text = if self.multiline {
            new_text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            new_text.replace("\r\n", " ").replace(['\r', '\n'], " ")
        };
        if let Some(max) = self.max_chars {
            let kept = self.text.chars().count() - self.text[range.clone()].chars().count();
            let remaining = max.saturating_sub(kept);
            if text.chars().count() > remaining {
                text = text.chars().take(remaining).collect();
            }
        }
        text
    }

    /// Replace `range` with `new_text`; the caret ends after the insertion.
    /// Returns whether the text changed.
    pub fn edit(&mut self, range: Range<usize>, new_text: &str) -> bool {
        let range = range.start.min(self.text.len())..range.end.min(self.text.len());
        let text = self.prepare_insert(&range, new_text);
        if text.is_empty() && range.is_empty() {
            return false;
        }
        if self.keep_undo {
            if self.undo.len() >= UNDO_LIMIT {
                self.undo.remove(0);
            }
            self.undo.push((self.text.clone(), self.selected.clone()));
        }
        self.text.replace_range(range.clone(), &text);
        let end = range.start + text.len();
        self.selected = end..end;
        self.reversed = false;
        true
    }

    pub fn insert(&mut self, new_text: &str) -> bool {
        self.edit(self.selected.clone(), new_text)
    }

    pub fn backspace(&mut self) -> bool {
        if self.selected.is_empty() {
            let start = self.prev_boundary(self.cursor());
            self.selected = start..self.selected.end;
        }
        self.edit(self.selected.clone(), "")
    }

    pub fn delete_forward(&mut self) -> bool {
        if self.selected.is_empty() {
            let end = self.next_boundary(self.cursor());
            self.selected = self.selected.start..end;
        }
        self.edit(self.selected.clone(), "")
    }

    /// Delete to the previous word start (whole field when `whole`).
    pub fn delete_word_left(&mut self, whole: bool) -> bool {
        if self.selected.is_empty() {
            let start = if whole {
                0
            } else {
                self.prev_word(self.cursor())
            };
            self.selected = start..self.selected.end;
        }
        self.edit(self.selected.clone(), "")
    }

    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some((text, sel)) => {
                self.text = text;
                let len = self.text.len();
                self.selected = sel.start.min(len)..sel.end.min(len);
                self.reversed = false;
                true
            }
            None => false,
        }
    }

    // -------------------------------------------------------------- utf-16 ---

    pub fn offset_from_utf16(&self, offset: usize) -> usize {
        utf16_to_utf8(&self.text, offset)
    }

    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())]
            .chars()
            .map(char::len_utf16)
            .sum()
    }
}

/// UTF-16 offset to a byte offset in `text` (clamped to the end).
pub fn utf16_to_utf8(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for ch in text.chars() {
        if utf16 >= offset {
            break;
        }
        utf16 += ch.len_utf16();
        utf8 += ch.len_utf8();
    }
    utf8
}

/// Largest valid vertical scroll for content `content_h` in a view `view_h`.
pub fn max_scroll(view_h: f32, content_h: f32) -> f32 {
    (content_h - view_h).max(0.0)
}

pub fn clamp_scroll(scroll: f32, view_h: f32, content_h: f32) -> f32 {
    scroll.clamp(0.0, max_scroll(view_h, content_h))
}

/// Scroll so the caret line `[caret_top, caret_top + line_h)` is visible,
/// moving as little as possible, then clamp.
pub fn follow_caret(scroll: f32, caret_top: f32, line_h: f32, view_h: f32, content_h: f32) -> f32 {
    let mut s = scroll;
    if caret_top + line_h > s + view_h {
        s = caret_top + line_h - view_h;
    }
    if caret_top < s {
        s = caret_top;
    }
    clamp_scroll(s, view_h, content_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_unicode() -> String {
        let mut text = String::new();
        for i in 0..60 {
            text.push_str(&format!("Line {i}: naïve café 会議 مرحبا 👩‍💻 e\u{301}nd\n"));
        }
        text
    }

    #[test]
    fn grapheme_moves_never_split_clusters() {
        let mut b = EditBuffer::new(true);
        b.set_text("a👩‍💻e\u{301}z".into());
        b.move_to(b.text.len());
        let mut stops = vec![b.cursor()];
        while b.cursor() > 0 {
            let to = b.prev_boundary(b.cursor());
            b.move_to(to);
            stops.push(to);
            assert!(b.text.is_char_boundary(to));
        }
        // z, é (e + combining accent), the ZWJ emoji, a, start.
        assert_eq!(stops.len(), 5, "{stops:?}");
        b.move_to(1);
        assert!(b.backspace());
        assert_eq!(b.text, "👩‍💻e\u{301}z");
        b.move_to(0);
        assert!(b.delete_forward());
        assert_eq!(b.text, "e\u{301}z", "the whole emoji sequence goes at once");
    }

    #[test]
    fn long_multiline_selection_and_edit() {
        let mut b = EditBuffer::new(true);
        b.set_text(long_unicode());
        let total_lines = b.text.matches('\n').count();
        assert_eq!(total_lines, 60);
        // Caret into line 40, select to the end of line 45 with the keyboard model.
        let line40 = b.text.match_indices('\n').nth(39).unwrap().0 + 1;
        b.move_to(line40);
        assert_eq!(b.line_start(line40 + 5), line40);
        let mut end = line40;
        for _ in 0..6 {
            end = b.line_end(end) + 1;
        }
        b.select_to(end);
        assert!(b.selected_text().starts_with("Line 40:"));
        assert!(b.selected_text().ends_with("e\u{301}nd\n"));
        assert_eq!(b.selected_text().matches('\n').count(), 6);
        // Reverse the selection past its anchor.
        b.select_to(line40 - 1);
        assert!(b.reversed && b.selected.end == line40);
        // Replace the selection; undo restores it exactly.
        b.select_to(end);
        let before = b.text.clone();
        assert!(b.insert("REPLACED\r\nTWO"));
        assert_eq!(b.text.matches('\n').count(), 60 - 6 + 1);
        assert!(b.text.contains("REPLACED\nTWO"));
        assert!(b.undo());
        assert_eq!(b.text, before);
    }

    #[test]
    fn words_lines_and_single_line_rules() {
        let mut b = EditBuffer::new(false);
        b.set_text("send my address".into());
        b.move_to(b.text.len());
        assert_eq!(b.prev_word(b.cursor()), 8);
        assert_eq!(b.next_word(0), 4);
        assert_eq!(b.word_at(9), 8..15);
        assert_eq!(b.line_start(10), 0);
        assert_eq!(b.line_end(0), b.text.len());
        b.move_to(4);
        assert!(b.insert("\nall\r\n"));
        assert_eq!(b.text, "send all  my address", "single line folds breaks");
        assert!(b.delete_word_left(false));
        assert_eq!(b.text, "send  my address", "deletes back to the word start");
    }

    #[test]
    fn length_limit_counts_chars_not_bytes() {
        let mut b = EditBuffer::new(true);
        b.max_chars = Some(5);
        assert!(b.insert("会議会議会議"));
        assert_eq!(b.text.chars().count(), 5);
        assert!(!b.insert("x"), "full: nothing inserted");
        b.select_all();
        assert!(b.insert("ab"));
        assert_eq!(b.text, "ab");
    }

    #[test]
    fn secret_buffers_keep_no_snapshots() {
        let mut b = EditBuffer::new(false);
        b.keep_undo = false;
        b.insert("sk-secret");
        b.backspace();
        assert_eq!(b.undo_depth(), 0);
        assert!(!b.undo());
        b.set_text(String::new());
        assert!(b.text.is_empty());
    }

    #[test]
    fn utf16_mapping_round_trips_through_astral_chars() {
        let mut b = EditBuffer::new(true);
        b.set_text("a😀b\n会".into());
        for (u8off, _) in b.text.char_indices().chain([(b.text.len(), ' ')]) {
            let u16off = b.offset_to_utf16(u8off);
            assert_eq!(b.offset_from_utf16(u16off), u8off);
        }
        assert_eq!(b.offset_to_utf16(b.text.len()), 6);
    }

    #[test]
    fn caret_follow_keeps_long_text_reachable() {
        let (line, view) = (24.0, 240.0); // 10 visible rows
        let content = 60.0 * line;
        // Typing at the bottom scrolls down just enough.
        let s = follow_caret(0.0, 30.0 * line, line, view, content);
        assert_eq!(s, 31.0 * line - view);
        // Moving back to the top scrolls up.
        assert_eq!(follow_caret(s, 2.0 * line, line, view, content), 2.0 * line);
        // A visible caret does not move the view.
        assert_eq!(follow_caret(100.0, 120.0, line, view, content), 100.0);
        // Never past either end.
        assert_eq!(
            follow_caret(0.0, 10_000.0, line, view, content),
            content - view
        );
        assert_eq!(clamp_scroll(-5.0, view, content), 0.0);
        assert_eq!(
            clamp_scroll(50.0, view, 100.0),
            0.0,
            "short text never scrolls"
        );
    }
}
