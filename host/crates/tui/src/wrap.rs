//! Wrapping text to a box's width (`[ui] wrap`, ARCHITECTURE.md §5.8): where a line breaks, how
//! one row of it is drawn, and a view over lines that scrolls by the rows they take.
//!
//! A row is a byte range of its line's text, so a long body line costs only the rows on screen
//! (its breaks are kept per width: `BodyView::rows`).

use std::borrow::Cow;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One row of a wrapped line: bytes `start..end` of its text, which begin at display column
/// `col` of the line and are drawn after `lead` columns of indent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Part {
    pub start: usize,
    pub end: usize,
    pub col: usize,
    pub lead: usize,
}

/// A line's text and the styles over it: byte ranges in order, `base` in between.
#[derive(Debug, Clone, Default)]
pub struct Text<'a> {
    pub text: Cow<'a, str>,
    pub runs: Vec<(usize, usize, Style)>,
    pub base: Style,
}

impl Text<'_> {
    /// A drawn line as text and styles.
    pub fn from_line(line: &Line<'_>) -> Text<'static> {
        let mut text = String::new();
        let mut runs = Vec::with_capacity(line.spans.len());
        for s in &line.spans {
            let start = text.len();
            text.push_str(&s.content);
            runs.push((start, text.len(), line.style.patch(s.style)));
        }
        Text { text: Cow::Owned(text), runs, base: line.style }
    }

    /// Columns of the line's leading spaces.
    pub fn leading(&self) -> usize {
        self.text.len() - self.text.trim_start_matches(' ').len()
    }

    /// The whole line as one drawn line.
    pub fn line(&self) -> Line<'static> {
        self.part(&Part { start: 0, end: self.text.len(), col: 0, lead: 0 })
    }

    /// Row `p` as a drawn line: its indent, then its characters in their styles.
    pub fn part(&self, p: &Part) -> Line<'static> {
        let mut spans = Vec::new();
        if p.lead > 0 {
            spans.push(Span::raw(" ".repeat(p.lead)));
        }
        let mut at = p.start;
        let mut k = self.runs.partition_point(|r| r.1 <= p.start);
        while at < p.end {
            let (to, style) = match self.runs.get(k) {
                Some(&(s, e, style)) if s <= at => {
                    k += 1;
                    (e.min(p.end), style)
                }
                Some(&(s, _, _)) => (s.min(p.end), self.base),
                None => (p.end, self.base),
            };
            spans.push(Span::styled(self.text[at..to].to_string(), style));
            at = to;
        }
        Line::from(spans)
    }

    /// Without wrapping: columns `skip..skip + take` of the line (the horizontal scroll), as a
    /// row. A wide character cut by `skip` leaves its column empty.
    pub fn window(&self, skip: usize, take: usize) -> Part {
        let mut col = 0;
        let mut start = None;
        let mut end = self.text.len();
        for (b, ch) in self.text.char_indices() {
            let w = ch.width().unwrap_or(0);
            if start.is_none() && col >= skip {
                start = Some((b, col));
            }
            if start.is_some() && col + w > skip + take {
                end = b;
                break;
            }
            col += w;
        }
        let (start, at) = start.unwrap_or((self.text.len(), skip.max(col)));
        Part { start, end: end.max(start), col: at, lead: at - skip.min(at) }
    }
}

/// Breaks `text` into rows `width` columns wide: at spaces where it can, inside a word only when
/// the word is wider than a whole row (it then starts where it is, and breaks after `/ . - _ ? &
/// = : , ;` or before `(` `[` when that leaves the row at least half full, else at the edge:
/// `callStart` and `(RealCall.kt:171)`, not `RealCa` and `ll.kt`). Rows after the first start
/// `indent` columns in (at most half the width), and the spaces where a row breaks are not
/// drawn. With `items` (the UI's own rows), an item after a ` · ` that does not fit on the row
/// starts the next one when it fits there (`wait 60 ms`, not `wait` and `60 ms`), and a longer
/// one does when its first word would be left alone (`iss` without its URL).
pub fn layout(text: &str, width: usize, indent: usize, items: bool) -> Vec<Part> {
    let width = width.max(1);
    let indent = indent.min(width / 2);
    let line_room = width - indent;
    let mut parts: Vec<Part> = Vec::new();
    // the row being filled: where it starts (byte, column), where its text ends, its width,
    // and whether it has more than spaces
    let mut cur: Option<(usize, usize)> = None;
    let mut cur_end = 0;
    let mut cur_w = 0;
    let mut has_text = false;
    let mut col = 0;
    let room = |parts: &Vec<Part>| if parts.is_empty() { width } else { line_room };
    let push = |parts: &mut Vec<Part>, cur: &mut Option<(usize, usize)>, end: usize| {
        if let Some((start, c)) = cur.take() {
            let lead = if parts.is_empty() { 0 } else { indent };
            parts.push(Part { start, end: end.max(start), col: c, lead });
        }
    };
    let mut i = 0;
    let mut first_word = true;
    let mut after_dot = false;
    while i < text.len() {
        let start = i;
        let end = next_space(text, start);
        let spaces_end = past_spaces(text, end);
        let word = &text[start..end];
        let is_dot = word == "·";
        let word_w = word.width();
        if items && after_dot && !is_dot && has_text {
            let (item_w, head_w, several) = item_widths(text, start);
            let moves = item_w <= line_room || (several && cur_w + head_w > room(&parts) && head_w <= line_room);
            if cur_w + item_w > room(&parts) && moves {
                push(&mut parts, &mut cur, cur_end);
                (cur_w, has_text) = (0, false);
            }
        }
        if has_text && cur_w + word_w > room(&parts) && word_w <= line_room {
            push(&mut parts, &mut cur, cur_end);
            (cur_w, has_text) = (0, false);
        }
        // where this row could break inside the word: (byte, column, the row's width before it)
        let mut inside: Option<(usize, usize, usize)> = None;
        let mut prev = None;
        for (off, ch) in word.char_indices() {
            let b = start + off;
            let cw = ch.width().unwrap_or(0);
            if off > 0
                && cur.is_some()
                && (matches!(prev, Some('/' | '.' | '-' | '_' | '?' | '&' | '=' | ':' | ',' | ';'))
                    || matches!(ch, '(' | '['))
            {
                inside = Some((b, col, cur_w));
            }
            prev = Some(ch);
            if cur_w + cw > room(&parts) && cur_w > 0 {
                // at the last break inside the word, when the row keeps half its room
                if let Some((at, at_col, w)) = inside.take().filter(|&(_, _, w)| w * 2 >= room(&parts)) {
                    push(&mut parts, &mut cur, at);
                    cur = Some((at, at_col));
                    cur_w -= w;
                }
                if cur_w + cw > room(&parts) && cur_w > 0 {
                    push(&mut parts, &mut cur, cur_end);
                    cur_w = 0;
                }
            }
            if cur.is_none() {
                cur = Some((b, col));
            }
            cur_w += cw;
            col += cw;
            cur_end = b + ch.len_utf8();
            has_text = true;
        }
        // spaces stay while they fit, and at the start of the line; none start a later row
        for b in end..spaces_end {
            if (cur_w > 0 && cur_w < room(&parts)) || (cur_w == 0 && parts.is_empty()) {
                if cur.is_none() {
                    cur = Some((b, col));
                }
                cur_w += 1;
            }
            col += 1;
        }
        after_dot = is_dot && !first_word;
        first_word = false;
        i = spaces_end;
    }
    if cur.is_some() || parts.is_empty() {
        if cur.is_none() {
            cur = Some((0, 0));
        }
        push(&mut parts, &mut cur, cur_end);
    }
    parts
}

fn next_space(text: &str, from: usize) -> usize {
    text[from..].find(' ').map_or(text.len(), |k| from + k)
}

fn past_spaces(text: &str, from: usize) -> usize {
    from + (text.len() - from - text[from..].trim_start_matches(' ').len())
}

/// The item that starts at byte `start`: its width with the ` ·` after it, the width of its
/// first two words, and whether it has more than one.
fn item_widths(text: &str, start: usize) -> (usize, usize, bool) {
    let (mut i, mut words, mut last_end, mut head_end) = (start, 0, start, start);
    let mut to = None;
    while i < text.len() {
        let end = next_space(text, i);
        if &text[i..end] == "·" {
            to = Some(end);
            break;
        }
        words += 1;
        last_end = end;
        if words <= 2 {
            head_end = end;
        }
        i = past_spaces(text, end);
    }
    let to = to.unwrap_or(last_end);
    (text[start..to].width(), text[start..head_end].width(), words > 1)
}

/// A drawn line wrapped to `width` (see [`layout`]; items stay together).
pub fn wrap_line(line: &Line<'_>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let text = Text::from_line(line);
    layout(&text.text, width, indent, true).iter().map(|p| text.part(p)).collect()
}

/// Where a view over wrapped lines starts: a line, and how many of its rows are above the box.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Top {
    pub line: usize,
    pub part: usize,
}

/// A view's lines: how many there are, the box's height, and the rows each one takes (1 for
/// each when nothing wraps).
pub struct Lines<'a> {
    pub len: usize,
    pub view: usize,
    pub rows: &'a mut dyn FnMut(usize) -> usize,
}

impl Lines<'_> {
    fn rows_of(&mut self, i: usize) -> usize {
        (self.rows)(i).max(1)
    }

    /// The last top there is: the last line's last row on the box's last row (the start when
    /// everything fits).
    pub fn last_top(&mut self) -> Top {
        let view = self.view.max(1);
        let mut below = 0;
        for i in (0..self.len).rev() {
            let h = self.rows_of(i);
            if below + h >= view {
                return Top { line: i, part: below + h - view };
            }
            below += h;
        }
        Top::default()
    }

    /// `top` within the lines as they are now (they may have changed, or the width).
    pub fn clamp(&mut self, top: Top) -> Top {
        if self.len == 0 {
            return Top::default();
        }
        let mut t = top;
        if t.line >= self.len {
            t = Top { line: self.len - 1, part: 0 };
        }
        t.part = t.part.min(self.rows_of(t.line) - 1);
        t.min(self.last_top())
    }

    /// `top` moved `n` rows down (up when negative), and how many rows it moved.
    pub fn scroll(&mut self, top: Top, n: isize) -> (Top, usize) {
        let mut t = self.clamp(top);
        let want = n.unsigned_abs();
        if n < 0 {
            let (t, left) = self.up(t, want);
            return (t, want - left);
        }
        let max = self.last_top();
        let mut left = want;
        while left > 0 && t < max {
            let below = self.rows_of(t.line) - 1 - t.part;
            if left <= below {
                t.part += left;
                left = 0;
            } else {
                left -= below + 1;
                t = Top { line: t.line + 1, part: 0 };
            }
        }
        if t > max {
            left += self.rows_between(max, t) - 1;
            t = max;
        }
        (t, want - left)
    }

    /// `t` moved up `n` rows, and the rows it could not move.
    fn up(&mut self, mut t: Top, mut n: usize) -> (Top, usize) {
        while n > 0 {
            if t.part >= n {
                t.part -= n;
                return (t, 0);
            }
            if t.line == 0 {
                n -= t.part;
                t.part = 0;
                return (t, n);
            }
            n -= t.part + 1;
            t.line -= 1;
            t.part = self.rows_of(t.line) - 1;
        }
        (t, 0)
    }

    /// Rows from `a` to `b`, both counted (more than the box when it gets there).
    fn rows_between(&mut self, a: Top, b: Top) -> usize {
        let mut rows = 0;
        let mut t = a;
        loop {
            if t.line >= b.line {
                return rows + b.part.saturating_sub(t.part) + 1;
            }
            rows += self.rows_of(t.line) - t.part;
            if rows > self.view {
                return rows;
            }
            t = Top { line: t.line + 1, part: 0 };
        }
    }

    /// The top that shows row `part` of line `line`, and all of the line when it fits, moving
    /// as little as it can. A line taller than the box shows from its first row when that keeps
    /// the row in view.
    pub fn show(&mut self, top: Top, line: usize, part: usize) -> Top {
        if self.len == 0 {
            return Top::default();
        }
        let view = self.view.max(1);
        let line = line.min(self.len - 1);
        let h = self.rows_of(line);
        let (first, last) = if h <= view {
            (Top { line, part: 0 }, Top { line, part: h - 1 })
        } else {
            let p = part.min(h - 1);
            (Top { line, part: p }, Top { line, part: p })
        };
        let top = self.clamp(top);
        if first < top {
            return first;
        }
        if self.rows_between(top, last) <= view {
            return top;
        }
        let at_bottom = self.up(last, view - 1).0;
        if h > view { at_bottom.max(Top { line, part: 0 }) } else { at_bottom }
    }

    /// What is on row `y` of the box: the line, and which of its rows (none below the end).
    pub fn at(&mut self, top: Top, y: usize) -> Option<Top> {
        let mut t = top;
        let mut y = y;
        while t.line < self.len {
            let left = self.rows_of(t.line).saturating_sub(t.part);
            if y < left {
                return Some(Top { line: t.line, part: t.part + y });
            }
            y -= left;
            t = Top { line: t.line + 1, part: 0 };
        }
        None
    }

    /// The row of the box where line `line` starts: 0 when it starts at the top or above it.
    pub fn row_of(&mut self, top: Top, line: usize) -> usize {
        if line <= top.line {
            return 0;
        }
        let mut rows = self.rows_of(top.line) - top.part.min(self.rows_of(top.line) - 1);
        for i in top.line + 1..line {
            if rows > self.view {
                break;
            }
            rows += self.rows_of(i);
        }
        rows
    }

    /// Ctrl+D and Ctrl+U: the view moves `n` rows and the cursor with it, keeping its row of the
    /// box; where the view cannot move further, the cursor moves the rest.
    pub fn half_page(&mut self, top: Top, cursor: usize, n: usize, down: bool) -> (Top, usize) {
        if self.len == 0 {
            return (Top::default(), 0);
        }
        let last_row = self.view.max(1) - 1;
        let y = self.row_of(top, cursor).min(last_row);
        let (new, moved) = self.scroll(top, if down { n as isize } else { -(n as isize) });
        let rest = n - moved;
        let y = if down { (y + rest).min(last_row) } else { y.saturating_sub(rest) };
        let cursor = self.at(new, y).map_or(self.len - 1, |t| t.line);
        (new, cursor)
    }

    /// After the view moved (the wheel): the cursor's line, kept on the box.
    pub fn keep_cursor(&mut self, top: Top, cursor: usize) -> usize {
        if cursor < top.line {
            return top.line;
        }
        let view = self.view.max(1);
        if self.row_of(top, cursor) >= view {
            return self.at(top, view - 1).map_or(cursor, |t| t.line);
        }
        cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    const LABEL_W: usize = 18;

    fn rows(text: &str, width: usize, indent: usize, items: bool) -> Vec<String> {
        let t = Text { text: Cow::Borrowed(text), runs: Vec::new(), base: Style::default() };
        layout(text, width, indent, items)
            .iter()
            .map(|p| t.part(p).spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_label_row_continues_under_its_value() {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let line = Line::from(vec![
            Span::styled(format!("{:<LABEL_W$}", "Token"), dim),
            Span::raw("JWT HS256 · expired 11:32:20.000 (13 d 5 h ago) · iss surepass"),
        ]);
        let parts = wrap_line(&line, 50, LABEL_W);
        assert!(parts.len() > 1, "{parts:?}");
        let text = |l: &Line<'_>| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        for p in &parts {
            assert!(p.width() <= 50, "{:?} is {} wide", text(p), p.width());
        }
        assert!(text(&parts[0]).starts_with("Token             JWT HS256"));
        for p in &parts[1..] {
            let t = text(p);
            assert!(t.starts_with(&" ".repeat(LABEL_W)) && !t[LABEL_W..].starts_with(' '), "{t:?}");
        }
        // nothing lost, nothing added (but the indents and the spaces where it breaks)
        let joined: String = parts.iter().map(|p| text(p).trim().to_string()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined.split_whitespace().collect::<Vec<_>>(), text(&line).split_whitespace().collect::<Vec<_>>());
        // styles stay with their characters
        assert_eq!(parts[0].spans[0].style, dim);
    }

    #[test]
    fn a_word_wider_than_a_line_starts_where_it_is_and_breaks_after_punctuation() {
        let pad = " ".repeat(LABEL_W);
        let url = format!("{:<LABEL_W$}https://prod-esign-api-v2.surepass.app/api/v1/flow/check-session", "URL");
        // the URL starts beside its label; its rows end after a `-` or a `/`
        assert_eq!(
            rows(&url, 40, LABEL_W, true),
            vec![
                "URL               https://prod-esign-".to_string(),
                format!("{pad}api-v2.surepass.app/"),
                format!("{pad}api/v1/flow/check-"),
                format!("{pad}session")
            ]
        );
        // a frame breaks before its file
        let frame = "    at okhttp3.internal.connection.RealCall.callStart(RealCall.kt:171)";
        assert_eq!(
            rows(frame, 60, 6, false),
            vec!["    at okhttp3.internal.connection.RealCall.callStart", "      (RealCall.kt:171)"]
        );
        // no punctuation, or too early in the row: at the edge
        assert_eq!(rows("abcdefghij", 4, 0, false), vec!["abcd", "efgh", "ij"]);
        assert_eq!(rows("a/bcdefghij", 6, 0, false), vec!["a/bcde", "fghij"]);
    }

    #[test]
    fn items_between_dots_stay_together() {
        let pad = " ".repeat(LABEL_W);
        let legend = format!("{pad}queued 500 µs · dns — · connect — · send 100 µs · wait 60 ms · receive 333 µs");
        assert_eq!(
            rows(&legend, 70, LABEL_W, true),
            vec![
                format!("{pad}queued 500 µs · dns — · connect — · send 100 µs ·"),
                format!("{pad}wait 60 ms · receive 333 µs")
            ]
        );
        // an item longer than a line starts a line too when its first word would be alone
        let token = format!("{:<LABEL_W$}JWT HS256 · iss https://auth.example.app in request header", "Token");
        assert_eq!(
            rows(&token, 50, LABEL_W, true),
            vec![
                "Token             JWT HS256 ·".to_string(),
                format!("{pad}iss https://auth.example.app in"),
                format!("{pad}request header")
            ]
        );
        // … but stays where its first two words fit
        let token = format!(
            "{:<LABEL_W$}JWT HS256 · expires 06:10:00.000 (in 59 min) · sub user_4821 · \
             iss https://auth.example.app  in request header Authorization · Enter decodes",
            "Token"
        );
        assert_eq!(
            rows(&token, 73, LABEL_W, true),
            vec![
                "Token             JWT HS256 · expires 06:10:00.000 (in 59 min) ·".to_string(),
                format!("{pad}sub user_4821 · iss https://auth.example.app  in"),
                format!("{pad}request header Authorization · Enter decodes")
            ]
        );
        // a body line knows no items
        assert_eq!(rows("a b · c d", 6, 0, false), vec!["a b ·", "c d"]);
    }

    #[test]
    fn a_body_line_keeps_its_indent_and_breaks_at_spaces() {
        let line = r#"    "description": "a long text that goes on and on""#;
        let texts = rows(line, 31, 6, false);
        assert_eq!(texts, vec![r#"    "description": "a long text"#, r#"      that goes on and on""#]);
        // the parts know where they are in the line
        let parts = layout(line, 31, 6, false);
        assert_eq!((parts[1].col, parts[1].lead), (line.find("that").unwrap(), 6));
        assert_eq!(&line[parts[1].start..parts[1].end], r#"that goes on and on""#);
    }

    #[test]
    fn short_and_empty_lines_are_one_row() {
        assert_eq!(rows("Method            GET", 80, LABEL_W, true), vec!["Method            GET"]);
        assert_eq!(layout("", 10, 2, false), vec![Part::default()]);
        assert_eq!(rows("    ", 10, 2, false), vec![""]);
    }

    #[test]
    fn a_huge_line_breaks_into_rows_of_the_width() {
        let line = "x".repeat(100_000);
        let parts = layout(&line, 80, 2, false);
        assert_eq!(parts.len(), 1 + (100_000 - 80usize).div_ceil(78));
        assert!(parts.iter().skip(1).all(|p| p.lead == 2 && p.end - p.start <= 78));
    }

    #[test]
    fn styles_are_cut_with_the_rows() {
        let red = Style::default().fg(ratatui::style::Color::Red);
        let line = Line::from(vec![Span::raw("ab "), Span::styled("cdef", red), Span::raw(" gh")]);
        let text = Text::from_line(&line);
        let parts = layout(&text.text, 5, 0, false);
        let drawn: Vec<Line> = parts.iter().map(|p| text.part(p)).collect();
        // the space where the row breaks is not drawn
        assert_eq!(drawn[0].spans.iter().map(|s| s.content.as_ref()).collect::<Vec<_>>(), vec!["ab"]);
        assert_eq!(drawn[1].spans[0].style, red);
        assert_eq!(drawn[1].spans[0].content, "cdef");
    }

    #[test]
    fn a_window_is_the_horizontal_scroll() {
        let text = Text { text: Cow::Borrowed("0123456789"), runs: Vec::new(), base: Style::default() };
        let p = text.window(3, 4);
        assert_eq!((&text.text[p.start..p.end], p.col, p.lead), ("3456", 3, 0));
        let p = text.window(20, 4);
        assert_eq!(p.start, p.end);
    }

    /// Lines of these heights in a box `view` rows tall.
    fn lines<'a>(heights: &'a [usize], view: usize, f: &'a mut dyn FnMut(usize) -> usize) -> Lines<'a> {
        Lines { len: heights.len(), view, rows: f }
    }

    #[test]
    fn the_view_scrolls_by_rows_and_stops_at_the_end() {
        let heights = [1, 3, 1, 5, 1];
        let mut f = |i: usize| heights[i];
        let mut l = lines(&heights, 4, &mut f);
        // 11 rows in all: the last top leaves the last line on the last row
        assert_eq!(l.last_top(), Top { line: 3, part: 2 });
        assert_eq!(l.scroll(Top::default(), 2), (Top { line: 1, part: 1 }, 2));
        assert_eq!(l.scroll(Top { line: 1, part: 1 }, 100), (Top { line: 3, part: 2 }, 5));
        assert_eq!(l.scroll(Top { line: 3, part: 2 }, -4), (Top { line: 1, part: 2 }, 4));
        assert_eq!(l.scroll(Top { line: 1, part: 0 }, -5), (Top::default(), 1));
        assert_eq!(l.at(Top { line: 1, part: 1 }, 2), Some(Top { line: 2, part: 0 }));
        assert_eq!(l.at(Top { line: 3, part: 2 }, 3), Some(Top { line: 4, part: 0 }));
        assert_eq!(l.at(Top { line: 4, part: 0 }, 1), None);
    }

    #[test]
    fn showing_a_line_moves_the_view_as_little_as_it_can() {
        let heights = [1, 3, 1, 5, 1, 1];
        let mut f = |i: usize| heights[i];
        let mut l = lines(&heights, 4, &mut f);
        let top = Top::default();
        // already on the box
        assert_eq!(l.show(top, 1, 0), top);
        // below: the whole line comes up to the last row
        assert_eq!(l.show(top, 2, 0), Top { line: 1, part: 0 });
        // taller than the box: it starts the box, or its asked-for row comes up to the last row
        assert_eq!(l.show(top, 3, 0), Top { line: 3, part: 0 });
        assert_eq!(l.show(Top { line: 3, part: 0 }, 3, 4), Top { line: 3, part: 1 });
        // above: it starts the box
        assert_eq!(l.show(Top { line: 4, part: 0 }, 1, 0), Top { line: 1, part: 0 });
    }

    #[test]
    fn half_pages_move_the_view_and_the_cursor_together() {
        let heights = [1; 20];
        let mut f = |i: usize| heights[i];
        let mut l = lines(&heights, 6, &mut f);
        // the cursor keeps its row of the box
        assert_eq!(l.half_page(Top::default(), 2, 3, true), (Top { line: 3, part: 0 }, 5));
        // at the end the cursor moves the rest
        assert_eq!(l.half_page(Top { line: 14, part: 0 }, 16, 3, true), (Top { line: 14, part: 0 }, 19));
        assert_eq!(l.half_page(Top { line: 1, part: 0 }, 2, 3, false), (Top::default(), 0));
    }
}
