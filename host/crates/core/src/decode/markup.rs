//! Tolerant XML/HTML pretty-printer and highlighter.
//!
//! Markup is shown as it was sent (entities are not unescaped), one tag per line, with an
//! element that holds only short text kept on one line. HTML void elements (`<br>`) do not
//! nest, and `<script>`/`<style>` content is kept verbatim. Tokens are read as the lines are
//! made, and the lines stop at a [`Budget`].

use std::collections::VecDeque;

use super::doc::{Budget, StyledLine, Tok, cut_line};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Xml,
    Html,
}

const VOID: [&str; 14] =
    ["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];
const RAW: [&str; 3] = ["script", "style", "textarea"];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token<'a> {
    Start {
        name: &'a str,
        raw: &'a str,
        self_closing: bool,
    },
    End {
        name: &'a str,
        raw: &'a str,
    },
    Text(&'a str),
    Comment(&'a str),
    /// `<?xml ...?>`, `<!DOCTYPE ...>`, `<![CDATA[...]]>`
    Other(&'a str),
}

fn tag_name(inner: &str) -> &str {
    let end = inner.find(|c: char| c.is_whitespace() || c == '/' || c == '>').unwrap_or(inner.len());
    &inner[..end]
}

/// Where the end tag of a raw-text element (`</script`) starts in `rest`, in any case.
fn find_end_tag(rest: &str, name: &str) -> Option<usize> {
    let b = rest.as_bytes();
    let mut from = 0;
    while let Some(p) = rest[from..].find("</") {
        let at = from + p;
        if b.get(at + 2..at + 2 + name.len()).is_some_and(|n| n.eq_ignore_ascii_case(name.as_bytes())) {
            return Some(at);
        }
        from = at + 2;
    }
    None
}

/// The tokens of a document, one at a time.
struct Tokens<'a> {
    s: &'a str,
    dialect: Dialect,
    i: usize,
    /// A raw-text element's content, after its start tag.
    pending: Option<Token<'a>>,
}

impl<'a> Tokens<'a> {
    fn new(s: &'a str, dialect: Dialect) -> Self {
        Tokens { s, dialect, i: 0, pending: None }
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        if let Some(t) = self.pending.take() {
            return Some(t);
        }
        let (s, dialect, i) = (self.s, self.dialect, self.i);
        let b = s.as_bytes();
        if i >= b.len() {
            return None;
        }
        if b[i] != b'<' {
            let end = s[i..].find('<').map_or(s.len(), |p| i + p);
            self.i = end;
            return Some(Token::Text(&s[i..end]));
        }
        let rest = &s[i..];
        let (end, tok) = if rest.starts_with("<!--") {
            let e = rest.find("-->").map_or(s.len(), |p| i + p + 3);
            (e, Token::Comment(&s[i..e]))
        } else if rest.starts_with("<![CDATA[") {
            let e = rest.find("]]>").map_or(s.len(), |p| i + p + 3);
            (e, Token::Other(&s[i..e]))
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            let e = rest.find('>').map_or(s.len(), |p| i + p + 1);
            (e, Token::Other(&s[i..e]))
        } else if rest.starts_with("</") {
            let e = rest.find('>').map_or(s.len(), |p| i + p + 1);
            let raw = &s[i..e];
            (e, Token::End { name: tag_name(raw[2..].trim_start()), raw })
        } else if rest.len() > 1 && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_' || b[i + 1] == b':') {
            // find the closing '>' outside quotes
            let mut j = i + 1;
            let mut quote = 0u8;
            while j < b.len() {
                match b[j] {
                    q @ (b'"' | b'\'') if quote == 0 => quote = q,
                    q if q == quote => quote = 0,
                    b'>' if quote == 0 => break,
                    _ => {}
                }
                j += 1;
            }
            let e = (j + 1).min(s.len());
            let raw = &s[i..e];
            let name = tag_name(&raw[1..]);
            let self_closing =
                raw.ends_with("/>") || (dialect == Dialect::Html && VOID.iter().any(|v| v.eq_ignore_ascii_case(name)));
            // raw-text elements: everything up to the matching end tag is text (found without a
            // lowercase copy of the rest, which a page of many scripts made again for each)
            self.i = e;
            if dialect == Dialect::Html && !self_closing && RAW.iter().any(|r| r.eq_ignore_ascii_case(name)) {
                let content_end = find_end_tag(&s[e..], name).map_or(s.len(), |p| e + p);
                if content_end > e {
                    self.pending = Some(Token::Other(&s[e..content_end]));
                }
                self.i = content_end;
            }
            return Some(Token::Start { name, raw, self_closing });
        } else {
            // a stray '<' in text
            let end = s[i + 1..].find('<').map_or(s.len(), |p| i + 1 + p);
            (end, Token::Text(&s[i..end]))
        };
        self.i = end;
        Some(tok)
    }
}

fn highlight_tag(line: &mut StyledLine, raw: &str) {
    // raw is "<name attr="v" ...>" or "</name>" or "<name/>"
    let b = raw.as_bytes();
    let mut i = 0;
    let open_len = if raw.starts_with("</") { 2 } else { 1 };
    line.push(&raw[..open_len], Tok::Punct);
    i += open_len;
    let name_end = raw[i..].find(|c: char| c.is_whitespace() || c == '/' || c == '>').map_or(raw.len(), |p| i + p);
    line.push(&raw[i..name_end], Tok::Tag);
    i = name_end;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            let e = raw[i..].find(|c: char| !c.is_whitespace()).map_or(raw.len(), |p| i + p);
            line.push(" ", Tok::Plain);
            i = e;
        } else if c == b'/' || c == b'>' {
            line.push(&raw[i..i + 1], Tok::Punct);
            i += 1;
        } else if c == b'=' {
            line.push("=", Tok::Punct);
            i += 1;
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i] as char;
                let e = raw[i + 1..].find(q).map_or(raw.len(), |p| i + 1 + p + 1);
                line.push(&raw[i..e], Tok::AttrValue);
                i = e;
            } else {
                let e = raw[i..].find(|c: char| c.is_whitespace() || c == '>').map_or(raw.len(), |p| i + p);
                line.push(&raw[i..e], Tok::AttrValue);
                i = e;
            }
        } else {
            let e = raw[i..]
                .find(|c: char| c.is_whitespace() || c == '=' || c == '>' || c == '/')
                .map_or(raw.len(), |p| i + p);
            // at least one whole character: a space that is not ASCII (U+00A0) stops the search
            // where it starts, and one byte on would cut it in half (up to 0.3.1 that panicked)
            let e = if e > i { e } else { i + raw[i..].chars().next().map_or(1, char::len_utf8) };
            line.push(&raw[i..e], Tok::Attr);
            i = e;
        }
    }
}

/// Lines of a comment or a block kept as sent, at `depth`; false once the budget is spent.
fn push_block(out: &mut Vec<StyledLine>, budget: &mut Budget, depth: usize, text: &str, tok: Tok) -> bool {
    for l in text.lines() {
        let t = l.trim_end();
        if t.trim().is_empty() {
            continue;
        }
        let mut line = StyledLine::new();
        line.indent(depth);
        line.push(t.trim_start(), tok);
        if !push(out, budget, line) {
            return false;
        }
    }
    true
}

fn push(out: &mut Vec<StyledLine>, budget: &mut Budget, line: StyledLine) -> bool {
    let fits = budget.take(line.text.len());
    if fits {
        out.push(line);
    }
    fits
}

/// Pretty-print markup, within the default [`Budget`]. Never fails: malformed input still
/// produces readable lines.
pub fn pretty(s: &str, dialect: Dialect) -> Vec<StyledLine> {
    pretty_in(s, dialect, &mut Budget::default())
}

/// [`pretty`] within `budget`: past it the last line says the rest is not shown.
pub fn pretty_in(s: &str, dialect: Dialect, budget: &mut Budget) -> Vec<StyledLine> {
    let mut toks = Tokens::new(s, dialect);
    // the token being shown and the two after it (`<a>`, text, `</a>` make one line)
    let mut ahead: VecDeque<Token<'_>> = VecDeque::with_capacity(3);
    let mut out = Vec::new();
    let mut depth = 0usize;
    loop {
        while ahead.len() < 3
            && let Some(t) = toks.next()
        {
            ahead.push_back(t);
        }
        let Some(tok) = ahead.pop_front() else { break };
        let fits = match tok {
            Token::Start { name, raw, self_closing } => {
                // <a>short text</a> on one line
                if !self_closing
                    && let (Some(Token::Text(t)), Some(Token::End { name: end_name, raw: end_raw })) =
                        (ahead.front(), ahead.get(1))
                    && end_name.eq_ignore_ascii_case(name)
                    && !t.contains('\n')
                    && t.trim().len() <= 120
                {
                    let mut line = StyledLine::new();
                    line.indent(depth);
                    highlight_tag(&mut line, raw);
                    line.push(t.trim(), Tok::Plain);
                    highlight_tag(&mut line, end_raw);
                    ahead.drain(..2);
                    push(&mut out, budget, line)
                } else {
                    let mut line = StyledLine::new();
                    line.indent(depth);
                    highlight_tag(&mut line, raw);
                    if !self_closing {
                        depth += 1;
                    }
                    push(&mut out, budget, line)
                }
            }
            Token::End { raw, .. } => {
                depth = depth.saturating_sub(1);
                let mut line = StyledLine::new();
                line.indent(depth);
                highlight_tag(&mut line, raw);
                push(&mut out, budget, line)
            }
            Token::Text(t) => {
                let collapsed: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
                if collapsed.is_empty() {
                    true
                } else {
                    let mut line = StyledLine::new();
                    line.indent(depth);
                    line.push(&collapsed, Tok::Plain);
                    push(&mut out, budget, line)
                }
            }
            Token::Comment(c) => push_block(&mut out, budget, depth, c, Tok::Comment),
            Token::Other(o) => {
                push_block(&mut out, budget, depth, o, if o.starts_with('<') { Tok::Meta } else { Tok::Plain })
            }
        };
        if !fits {
            out.push(cut_line());
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[StyledLine]) -> String {
        lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn xml_is_indented_and_short_text_inline() {
        let out = pretty(r#"<?xml version="1.0"?><a x="1"><b>hi &amp; bye</b><c/><!-- note --></a>"#, Dialect::Xml);
        assert_eq!(
            text(&out),
            "<?xml version=\"1.0\"?>\n<a x=\"1\">\n  <b>hi &amp; bye</b>\n  <c/>\n  <!-- note -->\n</a>"
        );
        let a = &out[1];
        assert!(a.spans.iter().any(|&(s, e, t)| t == Tok::AttrValue && &a.text[s as usize..e as usize] == "\"1\""));
    }

    /// Spaces that are not ASCII inside a tag: the tag is shown whole, character by character
    /// (up to 0.3.1 a U+00A0 there panicked; found by fuzzing).
    #[test]
    fn non_ascii_spaces_inside_a_tag() {
        for dialect in [Dialect::Html, Dialect::Xml] {
            let out = pretty("<a\u{a0}b=\"1\"\u{2003}c\u{a0}>x</a>", dialect);
            assert!(text(&out).contains("\u{a0}b"), "{:?}", text(&out));
            let _ = pretty("<a b=\u{a0}\u{a0}>", dialect);
            let _ = pretty("<\u{a0}>", dialect);
        }
    }

    /// 255 KB of `<a>`, never closed: up to 0.5.1 each line was indented two spaces a level,
    /// 7 GB in all. Now no line is indented past 64 columns, and a deeper one says its depth.
    #[test]
    fn deep_nesting_is_indented_64_columns_at_most() {
        let out = pretty(&"<a>".repeat(85_000), Dialect::Html);
        assert_eq!(out.len(), 85_000);
        assert!(out.iter().all(|l| l.text.chars().count() <= 64 + 3), "{:?}", out.last());
        assert_eq!(out[32].text, format!("{}<a>", " ".repeat(64)));
        assert_eq!(out[33].text, format!("{}33› <a>", " ".repeat(60)));
        assert_eq!(out[84_999].text, format!("{}84999› <a>", " ".repeat(57)));
        let total: usize = out.iter().map(|l| l.text.len()).sum();
        assert!(total < 6 << 20, "{total} bytes");
    }

    #[test]
    fn lines_stop_at_the_budget() {
        let doc = "<ul>".to_string() + &"<li>one</li><!-- a\nb -->".repeat(100) + "</ul>";
        let mut budget = Budget::new(10, 1 << 20);
        let out = pretty_in(&doc, Dialect::Html, &mut budget);
        assert!(budget.spent());
        assert_eq!(out.len(), 11);
        assert_eq!(out[1].text, "  <li>one</li>");
        assert_eq!(out[10], cut_line());
        // within the budget nothing is said
        let out = pretty("<a>x</a>", Dialect::Xml);
        assert_eq!(text(&out), "<a>x</a>");
    }

    /// Each `<script>` looked for its end in a lowercase copy of the rest of the page: a page of
    /// many scripts took time and memory with the square of its size.
    #[test]
    fn many_scripts_are_read_once() {
        let page = "<body>".to_string() + &"<SCRIPT>if (a < b) {}</Script>".repeat(20_000) + "</body>";
        let out = pretty(&page, Dialect::Html);
        assert_eq!(out.len(), 2 + 3 * 20_000);
        assert_eq!(text(&out[1..4]), "  <SCRIPT>\n    if (a < b) {}\n  </Script>");
    }

    #[test]
    fn html_void_and_raw_elements() {
        let out = pretty(
            "<!DOCTYPE html><html><head><meta charset=utf-8><script>if (a < b) { x() }</script></head><body><p>Hi<br>there</p></body></html>",
            Dialect::Html,
        );
        let t = text(&out);
        assert!(t.contains("\n    <meta charset=utf-8>\n"), "{t}");
        assert!(t.contains("if (a < b) { x() }"), "{t}");
        assert!(t.ends_with("</html>"), "{t}");
    }
}
