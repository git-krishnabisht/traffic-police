//! Styled text lines shared by all body viewers, and the limits on how many a body makes.

/// Token kinds a viewer can color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tok {
    Plain,
    Punct,
    Key,
    Str,
    Num,
    Bool,
    Null,
    Tag,
    Attr,
    AttrValue,
    Comment,
    /// Protobuf field numbers, form keys, multipart part titles.
    Field,
    /// Secondary information (sizes, counts, notes).
    Meta,
    Error,
}

/// One line of text with styled byte ranges. Unstyled ranges render as [`Tok::Plain`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyledLine {
    pub text: String,
    /// `(start, end, tok)` byte ranges into `text`, in order, non-overlapping.
    pub spans: Vec<(u32, u32, Tok)>,
}

impl StyledLine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plain(text: impl Into<String>) -> Self {
        StyledLine { text: text.into(), spans: Vec::new() }
    }

    pub fn styled(text: impl Into<String>, tok: Tok) -> Self {
        let mut l = StyledLine::new();
        l.push(&text.into(), tok);
        l
    }

    pub fn push(&mut self, s: &str, tok: Tok) -> &mut Self {
        let start = self.text.len() as u32;
        self.text.push_str(s);
        if tok != Tok::Plain && !s.is_empty() {
            self.spans.push((start, self.text.len() as u32, tok));
        }
        self
    }

    /// Two spaces a level of nesting, up to [`MAX_INDENT`] levels: a deeper line goes no further
    /// and says its depth there (`40› `).
    pub fn indent(&mut self, depth: usize) -> &mut Self {
        if depth <= MAX_INDENT {
            self.text.extend(std::iter::repeat_n(' ', 2 * depth));
            return self;
        }
        let mark = format!("{depth}› ");
        self.text.extend(std::iter::repeat_n(' ', (2 * MAX_INDENT).saturating_sub(mark.chars().count())));
        self.push(&mark, Tok::Meta)
    }

    /// The pieces of this line with their token kinds, gaps filled with [`Tok::Plain`].
    pub fn pieces(&self) -> Vec<(&str, Tok)> {
        let mut out = Vec::with_capacity(self.spans.len() * 2 + 1);
        let mut pos = 0usize;
        for &(s, e, tok) in &self.spans {
            let (s, e) = (s as usize, e as usize);
            if s > pos {
                out.push((&self.text[pos..s], Tok::Plain));
            }
            out.push((&self.text[s..e], tok));
            pos = e;
        }
        if pos < self.text.len() {
            out.push((&self.text[pos..], Tok::Plain));
        }
        out
    }
}

/// Split plain text into lines (handles `\r\n`), dropping a final empty line; within the
/// default [`Budget`].
pub fn text_lines(s: &str) -> Vec<StyledLine> {
    text_lines_in(s, &mut Budget::default())
}

/// [`text_lines`] within `budget`.
pub fn text_lines_in(s: &str, budget: &mut Budget) -> Vec<StyledLine> {
    let mut v = Vec::new();
    for l in s.split('\n') {
        let l = l.strip_suffix('\r').unwrap_or(l);
        if !budget.take(l.len()) {
            v.push(cut_line());
            return v;
        }
        v.push(StyledLine::plain(l));
    }
    if v.last().is_some_and(|l| l.text.is_empty()) {
        v.pop();
    }
    v
}

/// Nesting shown as indentation, at most: two columns a level, so 64 columns. Up to 0.5.1 every
/// level indented further, and 255 KB of `<a>` (85,000 levels) made 7 GB of spaces.
pub const MAX_INDENT: usize = 32;
/// The most lines a formatter makes of one body.
pub const MAX_LINES: usize = 1_000_000;
/// The most text a formatter makes of one body, its lines added up (in bytes).
pub const MAX_TEXT: usize = 64 << 20;

/// What a formatter may still make of one body: lines, and bytes of text. A body of short lines
/// or of deep nesting makes many times its size in lines (each line costs about 50 bytes besides
/// its text); past the budget the rest is left out, and the last line, [`cut_line`], says so.
/// Bodies in parts (multipart) share one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    lines: usize,
    text: usize,
    spent: bool,
}

impl Default for Budget {
    fn default() -> Self {
        Budget::new(MAX_LINES, MAX_TEXT)
    }
}

impl Budget {
    pub fn new(lines: usize, text: usize) -> Budget {
        Budget { lines, text, spent: false }
    }

    /// Room for one more line of `len` bytes. Once a line does not fit the budget is spent, and
    /// no later line fits either (even a shorter one), so a body is cut in one place.
    pub fn take(&mut self, len: usize) -> bool {
        if self.spent || self.lines == 0 || len > self.text {
            self.spent = true;
            return false;
        }
        self.lines -= 1;
        self.text -= len;
        true
    }

    /// Bytes of text still allowed (a formatter can check a long line before it makes it).
    pub fn room(&self) -> usize {
        if self.spent { 0 } else { self.text }
    }

    /// Spends what is left: the formatter stops here.
    pub fn spend(&mut self) {
        self.spent = true;
    }

    /// Whether a line did not fit: what was made ends where it stopped.
    pub fn spent(&self) -> bool {
        self.spent
    }
}

/// The last line of a body whose [`Budget`] ran out.
pub fn cut_line() -> StyledLine {
    StyledLine::styled("… the rest is not shown: a view holds 1,000,000 lines and 64 MB of text at most", Tok::Meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_lines_stop_at_the_indent_limit_and_say_how_deep() {
        let mut l = StyledLine::new();
        l.indent(3).push("x", Tok::Plain);
        assert_eq!(l.text, "      x");
        let mut l = StyledLine::new();
        l.indent(MAX_INDENT).push("x", Tok::Plain);
        assert_eq!(l.text.len(), 2 * MAX_INDENT + 1);
        let mut l = StyledLine::new();
        l.indent(40).push("x", Tok::Plain);
        assert_eq!(l.text, format!("{}40› x", " ".repeat(60)));
        assert_eq!(l.pieces()[1], ("40› ", Tok::Meta));
        let mut l = StyledLine::new();
        l.indent(85_000);
        assert_eq!(l.text.chars().count(), 2 * MAX_INDENT, "{:?}", l.text);
    }

    #[test]
    fn text_lines_stop_at_the_budget_and_say_so() {
        assert_eq!(text_lines("a\r\nb\n").len(), 2);
        let mut budget = Budget::new(3, 100);
        let lines = text_lines_in(&"x\n".repeat(10), &mut budget);
        assert!(budget.spent());
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[3], cut_line());
        // by text too, and nothing after the first line that does not fit
        let mut budget = Budget::new(100, 10);
        let lines = text_lines_in("12345\n123456789\nab", &mut budget);
        assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["12345", &cut_line().text]);
        assert!(!budget.take(1));
        // a million empty lines (about 50 MB of lines from 1 MB) stop at the default budget
        let lines = text_lines(&"\n".repeat(1 << 20));
        assert_eq!(lines.len(), MAX_LINES + 1);
        assert_eq!((MAX_LINES, MAX_TEXT), (1_000_000, 64 << 20), "as cut_line says");
    }
}
