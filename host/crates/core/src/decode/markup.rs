//! Tolerant XML/HTML pretty-printer and highlighter.
//!
//! Markup is shown as it was sent (entities are not unescaped), one tag per line, with an
//! element that holds only short text kept on one line. HTML void elements (`<br>`) do not
//! nest, and `<script>`/`<style>` content is kept verbatim.

use super::doc::{StyledLine, Tok};

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

fn tokenize(s: &str, dialect: Dialect) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            let end = s[i..].find('<').map_or(s.len(), |p| i + p);
            out.push(Token::Text(&s[i..end]));
            i = end;
            continue;
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
            out.push(Token::Start { name, raw, self_closing });
            // raw-text elements: everything up to the matching end tag is text
            if dialect == Dialect::Html && !self_closing && RAW.iter().any(|r| r.eq_ignore_ascii_case(name)) {
                let close = format!("</{}", name.to_ascii_lowercase());
                let lower = s[e..].to_ascii_lowercase();
                let content_end = lower.find(&close).map_or(s.len(), |p| e + p);
                if content_end > e {
                    out.push(Token::Other(&s[e..content_end]));
                }
                i = content_end;
                continue;
            }
            i = e;
            continue;
        } else {
            // a stray '<' in text
            let end = s[i + 1..].find('<').map_or(s.len(), |p| i + 1 + p);
            (end, Token::Text(&s[i..end]))
        };
        out.push(tok);
        i = end;
    }
    out
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
            let e = e.max(i + 1);
            line.push(&raw[i..e], Tok::Attr);
            i = e;
        }
    }
}

fn push_block(out: &mut Vec<StyledLine>, depth: usize, text: &str, tok: Tok) {
    for l in text.lines() {
        let t = l.trim_end();
        if t.trim().is_empty() {
            continue;
        }
        let mut line = StyledLine::new();
        line.indent(depth);
        line.push(t.trim_start(), tok);
        out.push(line);
    }
}

/// Pretty-print markup. Never fails: malformed input still produces readable lines.
pub fn pretty(s: &str, dialect: Dialect) -> Vec<StyledLine> {
    let toks = tokenize(s, dialect);
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Token::Start { name, raw, self_closing } => {
                // <a>short text</a> on one line
                if !self_closing
                    && let (Some(Token::Text(t)), Some(Token::End { name: end_name, raw: end_raw })) =
                        (toks.get(i + 1), toks.get(i + 2))
                    && end_name.eq_ignore_ascii_case(name)
                    && !t.contains('\n')
                    && t.trim().len() <= 120
                {
                    let mut line = StyledLine::new();
                    line.indent(depth);
                    highlight_tag(&mut line, raw);
                    line.push(t.trim(), Tok::Plain);
                    highlight_tag(&mut line, end_raw);
                    out.push(line);
                    i += 3;
                    continue;
                }
                let mut line = StyledLine::new();
                line.indent(depth);
                highlight_tag(&mut line, raw);
                out.push(line);
                if !self_closing {
                    depth += 1;
                }
            }
            Token::End { raw, .. } => {
                depth = depth.saturating_sub(1);
                let mut line = StyledLine::new();
                line.indent(depth);
                highlight_tag(&mut line, raw);
                out.push(line);
            }
            Token::Text(t) => {
                let collapsed: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
                if !collapsed.is_empty() {
                    let mut line = StyledLine::new();
                    line.indent(depth);
                    line.push(&collapsed, Tok::Plain);
                    out.push(line);
                }
            }
            Token::Comment(c) => push_block(&mut out, depth, c, Tok::Comment),
            Token::Other(o) => push_block(&mut out, depth, o, if o.starts_with('<') { Tok::Meta } else { Tok::Plain }),
        }
        i += 1;
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
