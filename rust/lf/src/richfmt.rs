//! Plain-text emulation of what the Python CLI's Rich `Console` prints when
//! stdout is not a terminal (no ANSI codes): markup tags are dropped, text is
//! soft-wrapped at the console width and `Table(box=None)` is laid out with
//! Rich's column-width algorithm. Each function mirrors a Rich routine
//! (`rich/_wrap.py`, `rich/text.py`, `rich/cells.py`, `rich/table.py`,
//! `rich/markup.py`); positions are code points, as in Python.
//!
//! Known gaps, all for input no LocalFlow command produces on its own:
//! `:emoji:` codes are not replaced, emoji ZWJ / variation-selector
//! sequences are measured per code point, and markup Rich would reject
//! (an unmatched closing tag such as `[/x]`, which makes Python raise
//! `MarkupError`) is dropped instead of failing.

use std::env;

use unicode_width::UnicodeWidthChar;

use crate::pyfmt::py_space;

/// Rich's console width: `COLUMNS` when it is all digits, else 80, minus
/// one on legacy Windows consoles (Rich reserves the last column there).
pub fn console_width(legacy_windows: bool) -> usize {
    effective_width(
        env::var("COLUMNS")
            .ok()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse().ok()),
        legacy_windows,
    )
}

/// Rich's `Console` reserves the last column on legacy Windows consoles:
/// it subtracts `legacy_windows` from `COLUMNS` or the detected width.
pub fn effective_width(columns: Option<usize>, legacy_windows: bool) -> usize {
    columns
        .filter(|&width| width > 0)
        .unwrap_or(80)
        .saturating_sub(usize::from(legacy_windows))
}

/// Rich's `legacy_windows`: on Windows, stdout is not a console with VT
/// processing enabled (`rich._windows.get_windows_console_features`), which
/// includes redirected or captured stdout.
#[cfg(windows)]
pub fn legacy_windows_console() -> bool {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    let mut mode = 0;
    // SAFETY: GetStdHandle takes no pointers; GetConsoleMode only writes the
    // u32 out-parameter and fails cleanly for invalid or non-console handles.
    let ok = unsafe { GetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), &mut mode) } != 0;
    !ok || mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0
}

#[cfg(not(windows))]
pub fn legacy_windows_console() -> bool {
    false
}

/// A non-terminal Rich `Console` of a fixed width.
#[derive(Clone, Copy, Debug)]
pub struct Console {
    pub width: usize,
}

impl Console {
    /// The width the Python CLI's module-level `Console()` would use.
    pub fn detect() -> Self {
        Console {
            width: console_width(legacy_windows_console()),
        }
    }

    /// `console.print(markup)`: tags dropped, wrapped, newline-terminated.
    pub fn markup(&self, markup: &str) -> String {
        self.text(&markup_plain(markup))
    }

    /// `console.print(Text(text))`: no markup, wrapped, newline-terminated.
    pub fn text(&self, text: &str) -> String {
        let plain = strip_control_codes(text);
        let mut out = String::new();
        for line in plain.split('\n') {
            let chars: Vec<char> = line.chars().collect();
            for piece in wrap_line(&chars, self.width, Overflow::Fold, false) {
                out.extend(piece);
                out.push('\n');
            }
        }
        out
    }

    /// `console.print(table)` for `Table(box=None)` with default padding
    /// `(0, 1)`, a header row and cells given as markup strings.
    pub fn table(&self, headers: &[&str], rows: &[Vec<String>]) -> String {
        let columns: Vec<Vec<Vec<char>>> = (0..headers.len())
            .map(|index| {
                std::iter::once(headers[index].to_string())
                    .chain(
                        rows.iter()
                            .map(|row| row.get(index).cloned().unwrap_or_default()),
                    )
                    .map(|cell| strip_control_codes(&markup_plain(&cell)).chars().collect())
                    .collect()
            })
            .collect();
        let widths = column_widths(&columns, self.width);
        let mut out = String::new();
        for row in 0..=rows.len() {
            let cells: Vec<Vec<Vec<char>>> = columns
                .iter()
                .zip(&widths)
                .map(|(column, &width)| render_cell(&column[row], width))
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(0).max(1);
            for line in 0..height {
                for (cell, &width) in cells.iter().zip(&widths) {
                    // The header row is aligned to the bottom, body rows to the top.
                    let offset = if row == 0 { height - cell.len() } else { 0 };
                    match line.checked_sub(offset).and_then(|i| cell.get(i)) {
                        Some(text) => out.extend(text),
                        None => out.extend(std::iter::repeat_n(' ', width)),
                    }
                }
                out.push('\n');
            }
        }
        out
    }
}

/// `rich.control.strip_control_codes`: drops BEL, BS, VT, FF and CR.
fn strip_control_codes(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(*c as u32, 7 | 8 | 11 | 12 | 13))
        .collect()
}

/// `rich.markup.render(...).plain`: console markup with every tag removed
/// and `\[` escapes resolved. See the module docs for the deliberate gaps.
pub fn markup_plain(markup: &str) -> String {
    let chars: Vec<char> = markup.chars().collect();
    let mut out = String::new();
    let plain = |segment: &[char], out: &mut String| {
        // `plain_text.replace("\\[", "[")` on every non-tag segment.
        let text: String = segment.iter().collect();
        out.push_str(&text.replace("\\[", "["));
    };
    let mut position = 0;
    let mut start = 0;
    while start < chars.len() {
        let Some((escapes, end)) = match_tag(&chars, start) else {
            start += 1;
            continue;
        };
        if start > position {
            plain(&chars[position..start], &mut out);
        }
        let (backslashes, escaped) = (escapes / 2, escapes % 2 == 1);
        if backslashes > 0 {
            out.extend(std::iter::repeat_n('\\', backslashes));
        }
        if escaped {
            plain(&chars[start + escapes..end], &mut out);
        }
        position = end;
        start = end;
    }
    if position < chars.len() {
        plain(&chars[position..], &mut out);
    }
    out
}

/// `RE_TAGS = r"((\\*)\[([a-z#/@][^[]*?)])"` anchored at `start`: the
/// number of leading backslashes and the end of the match.
fn match_tag(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let escapes = chars[start..].iter().take_while(|&&c| c == '\\').count();
    let open = start + escapes;
    if chars.get(open) != Some(&'[') {
        return None;
    }
    let first = *chars.get(open + 1)?;
    if !(first.is_ascii_lowercase() || matches!(first, '#' | '/' | '@')) {
        return None;
    }
    for (index, &c) in chars.iter().enumerate().skip(open + 2) {
        match c {
            ']' => return Some((escapes, index + 1)),
            '[' => return None,
            _ => {}
        }
    }
    None
}

/// `rich.cells.get_character_cell_size`.
fn char_cells(c: char) -> usize {
    let code = c as u32;
    if (code != 0 && code < 32) || (0x7f..0xa0).contains(&code) {
        return 0;
    }
    c.width().unwrap_or(0)
}

fn cell_len(chars: &[char]) -> usize {
    chars.iter().map(|&c| char_cells(c)).sum()
}

/// `rich.cells.split_graphemes`, per code point: zero-width characters
/// belong to the preceding span. Spans are `(start, end, cells)`.
fn spans(chars: &[char]) -> Vec<(usize, usize, usize)> {
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (index, &c) in chars.iter().enumerate() {
        let cells = char_cells(c);
        match spans.last_mut() {
            Some(last) if cells == 0 => last.1 = index + 1,
            _ => spans.push((index, index + 1, cells)),
        }
    }
    spans
}

/// `rich.cells.chop_cells`.
fn chop_cells(chars: &[char], width: usize) -> Vec<&[char]> {
    let mut lines = Vec::new();
    let mut size = 0;
    let mut offset = 0;
    for (start, _, cells) in spans(chars) {
        if size + cells > width {
            lines.push(&chars[offset..start]);
            offset = start;
            size = 0;
        }
        size += cells;
    }
    if size > 0 {
        lines.push(&chars[offset..]);
    }
    lines
}

/// `rich.cells.set_cell_size`: crop or pad to exactly `total` cells; a
/// double-width character cut in half becomes a space.
fn set_cell_size(chars: &[char], total: usize) -> Vec<char> {
    let size = cell_len(chars);
    if size <= total {
        let mut out = chars.to_vec();
        out.extend(std::iter::repeat_n(' ', total - size));
        return out;
    }
    let mut left = 0;
    for (start, _, cells) in spans(chars) {
        if left == total {
            return chars[..start].to_vec();
        }
        if left + cells > total {
            let mut out = chars[..start].to_vec();
            out.push(' ');
            return out;
        }
        left += cells;
    }
    chars.to_vec()
}

/// `rich._wrap.words`: `\s*\S+\s*` matched back to back from the start.
fn words(chars: &[char]) -> Vec<(usize, usize)> {
    let mut words = Vec::new();
    let mut position = 0;
    loop {
        let mut index = position;
        while index < chars.len() && py_space(chars[index]) {
            index += 1;
        }
        if index == chars.len() {
            return words;
        }
        while index < chars.len() && !py_space(chars[index]) {
            index += 1;
        }
        while index < chars.len() && py_space(chars[index]) {
            index += 1;
        }
        words.push((position, index));
        position = index;
    }
}

fn rstripped(chars: &[char]) -> &[char] {
    let end = chars.len() - chars.iter().rev().take_while(|&&c| py_space(c)).count();
    &chars[..end]
}

/// `rich._wrap.divide_line`: offsets to break the line at.
fn divide_line(chars: &[char], width: usize, fold: bool) -> Vec<usize> {
    let mut breaks = Vec::new();
    let mut offset = 0;
    for (start, end) in words(chars) {
        let word = &chars[start..end];
        let length = cell_len(rstripped(word));
        if width as isize - offset as isize >= length as isize {
            offset += cell_len(word);
        } else if length > width {
            if fold {
                let mut start = start;
                let folded = chop_cells(word, width);
                for (index, line) in folded.iter().enumerate() {
                    if start > 0 {
                        breaks.push(start);
                    }
                    if index + 1 == folded.len() {
                        offset = cell_len(line);
                    } else {
                        start += line.len();
                    }
                }
            } else {
                if start > 0 {
                    breaks.push(start);
                }
                offset = cell_len(word);
            }
        } else if offset > 0 && start > 0 {
            breaks.push(start);
            offset = cell_len(word);
        }
    }
    breaks
}

/// `Text.expand_tabs` for one line (tab size 8).
fn expand_tabs(chars: &[char]) -> Vec<char> {
    let mut out = Vec::new();
    let mut position = 0;
    for part in chars.split_inclusive(|&c| c == '\t') {
        if part.last() == Some(&'\t') {
            out.extend(&part[..part.len() - 1]);
            out.push(' ');
            position += cell_len(part) + 1;
            let remainder = position % 8;
            if remainder > 0 {
                out.extend(std::iter::repeat_n(' ', 8 - remainder));
                position += 8 - remainder;
            }
        } else {
            out.extend(part);
            position += cell_len(part);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Overflow {
    Fold,
    Ellipsis,
}

/// `Text.truncate(width, overflow=..., pad=...)`.
fn truncate(line: Vec<char>, width: usize, overflow: Overflow, pad: bool) -> Vec<char> {
    let length = cell_len(&line);
    let mut line = if length > width {
        if overflow == Overflow::Ellipsis {
            let mut cut = set_cell_size(&line, width.saturating_sub(1));
            cut.push('…');
            cut
        } else {
            set_cell_size(&line, width)
        }
    } else {
        line
    };
    if pad && length < width {
        line.extend(std::iter::repeat_n(' ', width - length));
    }
    line
}

/// `Text.wrap` for one line without newlines. `left` is `justify="left"`
/// (table cells), which pads every line to the width.
fn wrap_line(chars: &[char], width: usize, overflow: Overflow, left: bool) -> Vec<Vec<char>> {
    let expanded;
    let chars = if chars.contains(&'\t') {
        expanded = expand_tabs(chars);
        &expanded[..]
    } else {
        chars
    };
    let breaks = divide_line(chars, width, overflow == Overflow::Fold);
    let bounds: Vec<usize> = std::iter::once(0)
        .chain(breaks)
        .chain(std::iter::once(chars.len()))
        .collect();
    bounds
        .windows(2)
        .map(|pair| {
            let mut line = chars[pair[0]..pair[1]].to_vec();
            // `rstrip_end(width)`: drop trailing whitespace past the width
            // (measured in code points, as Rich does).
            if line.len() > width {
                let excess = line.len() - width;
                let spaces = line.len() - rstripped(&line).len();
                line.truncate(line.len() - spaces.min(excess));
            }
            if left {
                line = truncate(line, width, overflow, true);
            }
            truncate(line, width, overflow, false)
        })
        .collect()
}

/// Python `str.splitlines()` boundaries.
fn is_line_boundary(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// `Measurement.get` of `Padding(Text(cell), (0, 1))` at `max_width`.
fn measure_cell(cell: &[char], max_width: usize) -> (usize, usize) {
    if max_width < 3 {
        return (max_width, max_width);
    }
    let longest = |parts: &mut dyn Iterator<Item = &[char]>| parts.map(cell_len).max();
    let maximum = longest(&mut cell.split(|&c| is_line_boundary(c))).unwrap_or(0);
    let minimum =
        longest(&mut cell.split(|&c| py_space(c)).filter(|w| !w.is_empty())).unwrap_or(maximum);
    // Measurement(...).normalize().with_maximum(max_width) of the text.
    let maximum = maximum.min(max_width);
    let minimum = minimum.min(maximum);
    let (minimum, maximum) = if maximum < 1 {
        (0, 0)
    } else {
        (minimum, maximum)
    };
    ((minimum + 2).min(max_width), (maximum + 2).min(max_width))
}

/// `Table._measure_column`: the widest cell, capped at `max_width`.
fn measure_column(cells: &[Vec<char>], max_width: usize) -> usize {
    if max_width < 1 {
        return 0;
    }
    cells
        .iter()
        .map(|cell| measure_cell(cell, max_width).1)
        .max()
        .unwrap_or(max_width)
        .min(max_width)
}

/// `rich._ratio.ratio_reduce`.
fn ratio_reduce(
    total: usize,
    ratios: &[usize],
    maximums: &[usize],
    values: &[usize],
) -> Vec<usize> {
    let ratios: Vec<usize> = ratios
        .iter()
        .zip(maximums)
        .map(|(&ratio, &maximum)| if maximum > 0 { ratio } else { 0 })
        .collect();
    let mut total_ratio: usize = ratios.iter().sum();
    if total_ratio == 0 {
        return values.to_vec();
    }
    let mut remaining = total as f64;
    let mut result = Vec::with_capacity(values.len());
    for ((&ratio, &maximum), &value) in ratios.iter().zip(maximums).zip(values) {
        if ratio > 0 && total_ratio > 0 {
            let share = (ratio as f64 * remaining / total_ratio as f64).round_ties_even();
            let distributed = (maximum as f64).min(share) as usize;
            result.push(value.saturating_sub(distributed));
            remaining -= distributed as f64;
            total_ratio -= ratio;
        } else {
            result.push(value);
        }
    }
    result
}

/// `Table._collapse_widths` with every column wrappable.
fn collapse_widths(mut widths: Vec<usize>, max_width: usize) -> Vec<usize> {
    let mut total: usize = widths.iter().sum();
    while total > 0 && total > max_width {
        let excess = total - max_width;
        let widest = widths.iter().copied().max().unwrap_or(0);
        let second = widths
            .iter()
            .copied()
            .filter(|&w| w != widest)
            .max()
            .unwrap_or(0);
        let difference = widest - second;
        if difference == 0 {
            break;
        }
        let ratios: Vec<usize> = widths.iter().map(|&w| usize::from(w == widest)).collect();
        let maximums = vec![excess.min(difference); widths.len()];
        widths = ratio_reduce(excess, &ratios, &maximums, &widths);
        total = widths.iter().sum();
    }
    widths
}

/// `Table._calculate_column_widths` (no box, no expand, padding included).
fn column_widths(columns: &[Vec<Vec<char>>], max_width: usize) -> Vec<usize> {
    let mut widths: Vec<usize> = columns
        .iter()
        .map(|cells| measure_column(cells, max_width).max(1))
        .collect();
    if widths.iter().sum::<usize>() > max_width {
        widths = collapse_widths(widths, max_width);
        let total: usize = widths.iter().sum();
        if total > max_width {
            let ones = vec![1; widths.len()];
            widths = ratio_reduce(total - max_width, &ones, &widths, &widths);
        }
        widths = columns
            .iter()
            .zip(&widths)
            .map(|(cells, &width)| measure_column(cells, width))
            .collect();
    }
    widths
}

/// One table cell: `Padding(text, (0, 1))` rendered at `width`, every line
/// exactly `width` cells (`justify="left"`, `overflow="ellipsis"`).
fn render_cell(cell: &[char], width: usize) -> Vec<Vec<char>> {
    let mut lines = Vec::new();
    // Rich renders no lines at all when the padding leaves no room.
    let Some(inner) = width.checked_sub(2).filter(|&inner| inner > 0) else {
        return lines;
    };
    for line in cell.split(|&c| c == '\n') {
        for piece in wrap_line(line, inner, Overflow::Ellipsis, true) {
            let mut padded = vec![' '];
            padded.extend(set_cell_size(&piece, inner));
            padded.push(' ');
            lines.push(padded);
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(width: usize) -> Console {
        Console { width }
    }

    #[test]
    fn markup_tags_are_dropped_and_escapes_resolved() {
        assert_eq!(markup_plain("[red]error:[/red] x"), "error: x");
        assert_eq!(markup_plain("[WIP] [1] a[b"), "[WIP] [1] a[b");
        assert_eq!(markup_plain("see [link](url)"), "see (url)");
        assert_eq!(markup_plain(r"\[bold] \\[bold]x"), r"[bold] \x");
        assert_eq!(markup_plain("[a[b]c]"), "[ac]");
        assert_eq!(markup_plain("no tags"), "no tags");
    }

    #[test]
    fn text_wraps_like_rich() {
        // Expected values are Rich 15's output for the same calls.
        assert_eq!(console(10).text("aaaa bbbb cccc"), "aaaa bbbb \ncccc\n");
        assert_eq!(console(5).text("abcdefghijkl x"), "abcde\nfghij\nkl x\n");
        assert_eq!(console(10).text(""), "\n");
        assert_eq!(console(10).text("a\n\nb\n"), "a\n\nb\n\n");
        assert_eq!(console(10).text("a\tb"), "a       b\n");
        assert_eq!(console(4).text("ab      cd"), "ab  \ncd\n");
    }

    #[test]
    fn table_pads_and_collapses_like_rich() {
        let rows = vec![vec!["1".to_string(), "two".to_string()]];
        assert_eq!(
            console(80).table(&["id", "name"], &rows),
            " id  name \n 1   two  \n"
        );
        let rows = vec![vec!["x".into(), "aaaa bbbb cccc".into()]];
        assert_eq!(
            console(12).table(&["a", "b"], &rows),
            " a  b       \n x  aaaa    \n    bbbb    \n    cccc    \n"
        );
    }

    #[test]
    fn widths_follow_columns_and_legacy_windows() {
        assert_eq!(effective_width(Some(80), false), 80);
        assert_eq!(effective_width(None, true), 79);
        assert_eq!(effective_width(Some(0), false), 80);
    }
}
