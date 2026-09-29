//! JSON pretty-printer with fold ranges and paths, built in one pass over the raw text.
//!
//! Scalars keep their exact source spelling (big integers, `1.0`, escapes), so the view shows
//! what was sent. Nesting deeper than [`MAX_DEPTH`] is rejected rather than risking the stack.

use std::collections::HashSet;

use super::doc::{StyledLine, Tok};

pub const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    pub offset: usize,
    pub message: String,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Root,
    Key(String),
    Index(u32),
}

#[derive(Debug, Clone)]
struct PathEntry {
    parent: u32,
    seg: Seg,
}

#[derive(Debug, Clone)]
pub struct JsonLine {
    pub line: StyledLine,
    pub depth: u16,
    /// The container this line opens (`{` or `[` at its end).
    pub opens: Option<u32>,
    /// Path of the value on this line (for closing lines: the container's path).
    pub path: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonNode {
    pub open_line: u32,
    pub close_line: u32,
    pub is_array: bool,
    /// Members (object keys or array items).
    pub len: u32,
    pub path: u32,
}

#[derive(Debug, Clone, Default)]
pub struct JsonDoc {
    pub lines: Vec<JsonLine>,
    pub nodes: Vec<JsonNode>,
    paths: Vec<PathEntry>,
}

impl JsonDoc {
    /// `$`, `$.data.items[3].name`, `$["odd key"]`.
    pub fn path_string(&self, path: u32) -> String {
        let mut segs = Vec::new();
        let mut p = path;
        while let Some(e) = self.paths.get(p as usize) {
            if e.seg == Seg::Root {
                break;
            }
            segs.push(&e.seg);
            p = e.parent;
        }
        let mut out = String::from("$");
        for seg in segs.into_iter().rev() {
            match seg {
                Seg::Key(k) if is_ident(k) => {
                    out.push('.');
                    out.push_str(k);
                }
                Seg::Key(k) => {
                    out.push_str("[\"");
                    out.push_str(&k.replace('\\', "\\\\").replace('"', "\\\""));
                    out.push_str("\"]");
                }
                Seg::Index(i) => out.push_str(&format!("[{i}]")),
                Seg::Root => {}
            }
        }
        out
    }

    /// Line indices shown when the nodes in `folded` are collapsed.
    pub fn visible_lines(&self, folded: &HashSet<u32>) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.lines.len());
        let mut i = 0usize;
        while i < self.lines.len() {
            out.push(i as u32);
            match self.lines[i].opens {
                Some(n) if folded.contains(&n) => i = self.nodes[n as usize].close_line as usize + 1,
                _ => i += 1,
            }
        }
        out
    }

    /// Every container node (for "fold all").
    pub fn all_nodes(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.nodes.len() as u32).filter(|&n| n != 0 || self.nodes.len() == 1)
    }

    /// Whether the closing line of a node ends with a comma (to render a folded line).
    pub fn close_has_comma(&self, node: u32) -> bool {
        let n = &self.nodes[node as usize];
        self.lines[n.close_line as usize].line.text.ends_with(',')
    }
}

fn is_ident(k: &str) -> bool {
    let mut chars = k.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Decode a JSON string literal (including its quotes) into its value.
pub fn unescape(lit: &str) -> String {
    let inner = &lit[1..lit.len().saturating_sub(1).max(1)];
    if !inner.contains('\\') {
        return inner.to_string();
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let hi = u32::from_str_radix(&hex, 16).unwrap_or(0xfffd);
                let cp = if (0xd800..0xdc00).contains(&hi) {
                    let mut look = chars.clone();
                    if look.next() == Some('\\') && look.next() == Some('u') {
                        let lo_hex: String = look.by_ref().take(4).collect();
                        match u32::from_str_radix(&lo_hex, 16) {
                            Ok(lo) if (0xdc00..0xe000).contains(&lo) => {
                                chars = look;
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            }
                            _ => 0xfffd,
                        }
                    } else {
                        0xfffd
                    }
                } else {
                    hi
                };
                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
    doc: JsonDoc,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, message: &str) -> Result<T, JsonError> {
        Err(JsonError { offset: self.i, message: message.to_string() })
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn path(&mut self, parent: u32, seg: Seg) -> u32 {
        self.doc.paths.push(PathEntry { parent, seg });
        (self.doc.paths.len() - 1) as u32
    }

    fn string(&mut self) -> Result<&'a str, JsonError> {
        let start = self.i;
        self.i += 1; // opening quote
        while let Some(c) = self.peek() {
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(&self.s[start..self.i]);
                }
                b'\\' => self.i += 2,
                c if c < 0x20 => return self.err("control character in string"),
                _ => self.i += 1,
            }
        }
        Err(JsonError { offset: start, message: "unterminated string".into() })
    }

    fn number(&mut self) -> Result<&'a str, JsonError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while matches!(p.peek(), Some(b'0'..=b'9')) {
                p.i += 1;
            }
            p.i > s
        };
        if !digits(self) {
            return self.err("expected a digit");
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if !digits(self) {
                return self.err("expected a digit after '.'");
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return self.err("expected an exponent");
            }
        }
        Ok(&self.s[start..self.i])
    }

    fn literal(&mut self, word: &'static str) -> Result<&'a str, JsonError> {
        if self.s[self.i..].starts_with(word) {
            let start = self.i;
            self.i += word.len();
            Ok(&self.s[start..self.i])
        } else {
            self.err("unexpected token")
        }
    }

    fn push_line(&mut self, line: StyledLine, depth: usize, opens: Option<u32>, path: u32) {
        self.doc.lines.push(JsonLine { line, depth: depth.min(u16::MAX as usize) as u16, opens, path });
    }

    fn comma_on_last_line(&mut self) {
        if let Some(l) = self.doc.lines.last_mut() {
            l.line.push(",", Tok::Punct);
        }
    }

    fn value(&mut self, depth: usize, key: Option<&'a str>, path: u32) -> Result<(), JsonError> {
        if depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        self.ws();
        let mut line = StyledLine::new();
        line.indent(depth);
        if let Some(k) = key {
            line.push(k, Tok::Key);
            line.push(": ", Tok::Punct);
        }
        match self.peek() {
            Some(open @ (b'{' | b'[')) => {
                let is_array = open == b'[';
                let close = if is_array { b']' } else { b'}' };
                self.i += 1;
                self.ws();
                if self.peek() == Some(close) {
                    self.i += 1;
                    line.push(if is_array { "[]" } else { "{}" }, Tok::Punct);
                    self.push_line(line, depth, None, path);
                    return Ok(());
                }
                line.push(if is_array { "[" } else { "{" }, Tok::Punct);
                let node = self.doc.nodes.len() as u32;
                self.doc.nodes.push(JsonNode {
                    open_line: self.doc.lines.len() as u32,
                    close_line: 0,
                    is_array,
                    len: 0,
                    path,
                });
                self.push_line(line, depth, Some(node), path);
                let mut count = 0u32;
                loop {
                    self.ws();
                    if is_array {
                        let child = self.path(path, Seg::Index(count));
                        self.value(depth + 1, None, child)?;
                    } else {
                        if self.peek() != Some(b'"') {
                            return self.err("expected a key");
                        }
                        let k = self.string()?;
                        self.ws();
                        if self.peek() != Some(b':') {
                            return self.err("expected ':'");
                        }
                        self.i += 1;
                        let child = self.path(path, Seg::Key(unescape(k)));
                        self.value(depth + 1, Some(k), child)?;
                    }
                    count += 1;
                    self.ws();
                    match self.peek() {
                        Some(b',') => {
                            self.i += 1;
                            self.comma_on_last_line();
                        }
                        Some(c) if c == close => {
                            self.i += 1;
                            break;
                        }
                        _ => return self.err(if is_array { "expected ',' or ']'" } else { "expected ',' or '}'" }),
                    }
                }
                let mut close_line = StyledLine::new();
                close_line.indent(depth);
                close_line.push(if is_array { "]" } else { "}" }, Tok::Punct);
                let close_idx = self.doc.lines.len() as u32;
                self.push_line(close_line, depth, None, path);
                let n = &mut self.doc.nodes[node as usize];
                n.close_line = close_idx;
                n.len = count;
                Ok(())
            }
            Some(b'"') => {
                let s = self.string()?;
                line.push(s, Tok::Str);
                self.push_line(line, depth, None, path);
                Ok(())
            }
            Some(b't') => {
                let s = self.literal("true")?;
                line.push(s, Tok::Bool);
                self.push_line(line, depth, None, path);
                Ok(())
            }
            Some(b'f') => {
                let s = self.literal("false")?;
                line.push(s, Tok::Bool);
                self.push_line(line, depth, None, path);
                Ok(())
            }
            Some(b'n') => {
                let s = self.literal("null")?;
                line.push(s, Tok::Null);
                self.push_line(line, depth, None, path);
                Ok(())
            }
            Some(b'-' | b'0'..=b'9') => {
                let s = self.number()?;
                line.push(s, Tok::Num);
                self.push_line(line, depth, None, path);
                Ok(())
            }
            Some(_) => self.err("unexpected character"),
            None => self.err("unexpected end of input"),
        }
    }
}

/// Parse and pretty-print one JSON document.
pub fn parse(input: &[u8]) -> Result<JsonDoc, JsonError> {
    let s = std::str::from_utf8(input)
        .map_err(|e| JsonError { offset: e.valid_up_to(), message: "not valid UTF-8".into() })?;
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let mut p = Parser { s, b: s.as_bytes(), i: 0, doc: JsonDoc::default() };
    let root = p.path(0, Seg::Root);
    p.value(0, None, root)?;
    p.ws();
    if p.i != p.b.len() {
        return p.err("unexpected data after the JSON value");
    }
    Ok(p.doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(doc: &JsonDoc) -> String {
        doc.lines.iter().map(|l| l.line.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn pretty_prints_with_exact_scalars() {
        let doc =
            parse(br#"{"ok":true,"n":12345678901234567890,"f":1.0,"s":"a\"b","e":{},"a":[1,{"x":null}]}"#).unwrap();
        assert_eq!(
            text(&doc),
            r#"{
  "ok": true,
  "n": 12345678901234567890,
  "f": 1.0,
  "s": "a\"b",
  "e": {},
  "a": [
    1,
    {
      "x": null
    }
  ]
}"#
        );
        assert_eq!(doc.nodes.len(), 3);
        assert_eq!(doc.nodes[1].len, 2);
    }

    #[test]
    fn paths_and_folding() {
        let doc = parse(br#"{"data":{"items":[{"name":"a"},{"odd key":1}]}}"#).unwrap();
        let find = |needle: &str| doc.lines.iter().find(|l| l.line.text.contains(needle)).unwrap().path;
        assert_eq!(doc.path_string(find("\"name\"")), "$.data.items[0].name");
        assert_eq!(doc.path_string(find("\"odd key\"")), "$.data.items[1][\"odd key\"]");
        assert_eq!(doc.path_string(doc.lines[0].path), "$");
        let items_node = doc.lines.iter().find(|l| l.line.text.contains("\"items\"")).unwrap().opens.unwrap();
        let all = doc.visible_lines(&HashSet::new());
        let folded = doc.visible_lines(&HashSet::from([items_node]));
        assert_eq!(all.len(), doc.lines.len());
        assert_eq!(folded.len(), doc.lines.len() - 7);
    }

    #[test]
    fn errors_and_unescape() {
        assert!(parse(b"{\"a\":}").is_err());
        assert!(parse(b"[1,2").is_err());
        assert!(parse(b"{} x").is_err());
        assert!(parse(&b"[".repeat(1000)).is_err());
        assert_eq!(unescape(r#""aé😀\n""#), "aé😀\n");
    }
}
