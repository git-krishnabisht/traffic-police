//! Styled text lines shared by all body viewers.

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

    pub fn indent(&mut self, depth: usize) -> &mut Self {
        for _ in 0..depth {
            self.text.push_str("  ");
        }
        self
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

/// Split plain text into lines (handles `\r\n`), dropping a final empty line.
pub fn text_lines(s: &str) -> Vec<StyledLine> {
    let mut v: Vec<StyledLine> = s.split('\n').map(|l| StyledLine::plain(l.strip_suffix('\r').unwrap_or(l))).collect();
    if v.last().is_some_and(|l| l.text.is_empty()) {
        v.pop();
    }
    v
}
