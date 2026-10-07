//! Comparing two transactions (ARCHITECTURE.md §5.10): request lines, status lines, headers (in
//! order, or as sets) and bodies. JSON bodies are canonicalized (keys sorted recursively,
//! pretty-printed) before a line diff, so key order does not matter; other text is diffed as is;
//! binary bodies are compared by size and SHA-256.

use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use similar::{ChangeTag, DiffOp, TextDiff};

use crate::decode::decode_body;
use crate::fmt;
use crate::model::{BodyMeta, Headers, Transaction, TxnIdx};
use crate::store::SessionStore;

/// Bodies larger than this are compared by size and hash only.
const LINE_DIFF_LIMIT: usize = 4 << 20;
/// Unchanged lines kept around each change in a body.
const CONTEXT: usize = 3;
/// Header lists longer than this are shortened like bodies.
const FULL_HEADERS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Heading,
    Same,
    /// Only in A.
    Removed,
    /// Only in B.
    Added,
    Note,
    /// Unchanged lines left out.
    Gap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub text: String,
}

/// The comparison, as lines to show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub lines: Vec<Line>,
    /// Sections that differ.
    pub differing: Vec<&'static str>,
}

impl Diff {
    fn line(&mut self, kind: Kind, text: impl Into<String>) {
        self.lines.push(Line { kind, text: text.into() });
    }

    /// As unified-diff-like text (`-`, `+`, and two spaces), for copying.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            let prefix = match l.kind {
                Kind::Heading => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    "## "
                }
                Kind::Same => "  ",
                Kind::Removed => "- ",
                Kind::Added => "+ ",
                Kind::Note | Kind::Gap => "  ",
            };
            out.push_str(prefix);
            out.push_str(&l.text);
            out.push('\n');
        }
        out
    }
}

/// Compare `a` with `b`; `headers_as_sets` compares headers sorted by name instead of in order.
pub fn diff(store: &SessionStore, a: TxnIdx, b: TxnIdx, headers_as_sets: bool) -> Diff {
    let (ta, tb) = (store.txn(a), store.txn(b));
    let mut d = Diff::default();

    d.line(Kind::Heading, "Request");
    let line = |t: &Transaction| format!("{} {}", t.method, t.url.raw);
    single(&mut d, "request", &line(ta), &line(tb));

    d.line(Kind::Heading, "Response");
    single(&mut d, "response", &status_line(ta), &status_line(tb));

    d.line(Kind::Heading, "Request headers");
    headers(&mut d, "request headers", Some(&ta.req_headers), Some(&tb.req_headers), headers_as_sets);
    d.line(Kind::Heading, "Request body");
    body(
        &mut d,
        "request body",
        store,
        (ta, &ta.req_body, Some(&ta.req_headers)),
        (tb, &tb.req_body, Some(&tb.req_headers)),
    );

    d.line(Kind::Heading, "Response headers");
    let (ha, hb) = (ta.resp.as_ref().map(|r| &r.headers), tb.resp.as_ref().map(|r| &r.headers));
    headers(&mut d, "response headers", ha, hb, headers_as_sets);
    d.line(Kind::Heading, "Response body");
    body(&mut d, "response body", store, (ta, &ta.resp_body, ha), (tb, &tb.resp_body, hb));
    d
}

fn status_line(t: &Transaction) -> String {
    match (&t.resp, &t.failure) {
        (Some(r), _) => {
            let proto = r.protocol.as_deref().map_or(String::new(), |p| format!("{p} "));
            format!("{proto}{} {}", r.status, r.message).trim_end().to_string()
        }
        (None, Some(f)) => match &f.message {
            Some(m) => format!("failed: {}: {m}", f.short_class()),
            None => format!("failed: {}", f.short_class()),
        },
        (None, None) => format!("no response ({})", format!("{:?}", t.state).to_lowercase()),
    }
}

fn single(d: &mut Diff, section: &'static str, a: &str, b: &str) {
    if a == b {
        d.line(Kind::Same, a);
    } else {
        d.line(Kind::Removed, a);
        d.line(Kind::Added, b);
        d.differing.push(section);
    }
}

/// `name: value` lines: as sent, or (as a set) with names lower-cased and sorted.
fn header_lines(h: &Headers, as_set: bool) -> Vec<String> {
    if !as_set {
        return h.iter().map(|(n, v)| format!("{n}: {v}")).collect();
    }
    let mut v: Vec<String> = h.iter().map(|(n, v)| format!("{}: {v}", n.to_ascii_lowercase())).collect();
    v.sort();
    v
}

fn headers(d: &mut Diff, section: &'static str, a: Option<&Headers>, b: Option<&Headers>, as_sets: bool) {
    let (a, b) = match (a, b) {
        (None, None) => {
            d.line(Kind::Note, "no response in either");
            return;
        }
        (a, b) => (a.cloned().unwrap_or_default(), b.cloned().unwrap_or_default()),
    };
    let (la, lb) = (header_lines(&a, as_sets), header_lines(&b, as_sets));
    let context = if la.len().max(lb.len()) > FULL_HEADERS { CONTEXT } else { usize::MAX };
    let changed = lines(d, &la, &lb, context);
    if changed {
        d.differing.push(section);
        if !as_sets && header_lines(&a, true) == header_lines(&b, true) {
            d.line(Kind::Note, "the same headers in a different order");
        }
    } else if la.is_empty() {
        d.line(Kind::Note, "none in either");
    }
}

/// A line diff; unchanged runs longer than `context` on either side of a change are shortened.
/// Returns whether anything differs.
fn lines(d: &mut Diff, a: &[String], b: &[String], context: usize) -> bool {
    let ta: Vec<&str> = a.iter().map(String::as_str).collect();
    let tb: Vec<&str> = b.iter().map(String::as_str).collect();
    let diff = similar::TextDiffConfig::default().timeout(Duration::from_millis(500)).diff_slices(&ta, &tb);
    let changed = diff.ops().iter().any(|op| !matches!(op, DiffOp::Equal { .. }));
    if !changed && context != usize::MAX && !a.is_empty() {
        d.line(Kind::Note, format!("the same ({} line{})", a.len(), if a.len() == 1 { "" } else { "s" }));
        return false;
    }
    if !changed || context == usize::MAX {
        push_changes(d, &diff, diff.ops());
        return changed;
    }
    let groups = diff.grouped_ops(context);
    let mut shown_to = 0usize;
    for group in &groups {
        let first = group.first().map_or(0, |op| op.old_range().start);
        if first > shown_to {
            d.line(
                Kind::Gap,
                format!("⋯ {} same line{}", first - shown_to, if first - shown_to == 1 { "" } else { "s" }),
            );
        }
        push_changes(d, &diff, group);
        shown_to = group.last().map_or(first, |op| op.old_range().end);
    }
    if a.len() > shown_to {
        let n = a.len() - shown_to;
        d.line(Kind::Gap, format!("⋯ {n} same line{}", if n == 1 { "" } else { "s" }));
    }
    true
}

fn push_changes(d: &mut Diff, diff: &TextDiff<'_, '_, str>, ops: &[DiffOp]) {
    for op in ops {
        for c in diff.iter_changes(op) {
            let kind = match c.tag() {
                ChangeTag::Equal => Kind::Same,
                ChangeTag::Delete => Kind::Removed,
                ChangeTag::Insert => Kind::Added,
            };
            d.line(kind, c.value().to_string());
        }
    }
}

/// JSON with keys sorted at every level.
fn canonical(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = m.into_iter().map(|(k, v)| (k, canonical(v))).collect();
            entries.sort_by(|x, y| x.0.cmp(&y.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(canonical).collect()),
        other => other,
    }
}

enum Content {
    Empty,
    Lines(Vec<String>, &'static str),
    Bytes(bytes::Bytes),
}

fn content(store: &SessionStore, t: &Transaction, meta: &BodyMeta, headers: Option<&Headers>) -> Content {
    if meta.id.is_none() {
        return Content::Empty;
    }
    let d = decode_body(store.body_bytes(meta), store.decoding_headers(t, headers).as_deref(), 256 << 20);
    if d.bytes.is_empty() {
        return Content::Empty;
    }
    if d.bytes.len() > LINE_DIFF_LIMIT {
        return Content::Bytes(d.bytes);
    }
    if let Ok(v) = serde_json::from_slice::<Value>(&d.bytes) {
        let text = serde_json::to_string_pretty(&canonical(v)).unwrap_or_default();
        return Content::Lines(text.lines().map(str::to_string).collect(), "JSON, keys sorted");
    }
    match std::str::from_utf8(&d.bytes) {
        Ok(s) => Content::Lines(s.lines().map(str::to_string).collect(), "text"),
        Err(_) => Content::Bytes(d.bytes),
    }
}

fn digest(b: &[u8]) -> String {
    let h = Sha256::digest(b);
    h.iter().map(|x| format!("{x:02x}")).collect()
}

type Side<'a> = (&'a Transaction, &'a BodyMeta, Option<&'a Headers>);

fn body(d: &mut Diff, section: &'static str, store: &SessionStore, a: Side<'_>, b: Side<'_>) {
    let ca = content(store, a.0, a.1, a.2);
    let cb = content(store, b.0, b.1, b.2);
    match (ca, cb) {
        (Content::Empty, Content::Empty) => d.line(Kind::Note, "no body in either"),
        (Content::Lines(la, what_a), Content::Lines(lb, what_b)) => {
            if what_a != what_b {
                d.line(Kind::Note, format!("A is {what_a}, B is {what_b}"));
            } else if what_a != "text" {
                d.line(Kind::Note, what_a);
            }
            if lines(d, &la, &lb, CONTEXT) {
                d.differing.push(section);
            }
        }
        (ca, cb) => {
            let bytes = |c: Content| match c {
                Content::Empty => Vec::new(),
                Content::Lines(l, _) => l.join("\n").into_bytes(),
                Content::Bytes(b) => b.to_vec(),
            };
            let (ba, bb) = (bytes(ca), bytes(cb));
            let describe = |b: &[u8]| {
                if b.is_empty() {
                    "no body".to_string()
                } else {
                    format!("{} · sha256 {}", fmt::bytes(b.len() as u64), &digest(b)[..16])
                }
            };
            if ba == bb {
                d.line(Kind::Same, describe(&ba));
                d.line(Kind::Note, "the same bytes");
            } else {
                d.line(Kind::Removed, describe(&ba));
                d.line(Kind::Added, describe(&bb));
                d.differing.push(section);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEvent;
    use crate::event::{RequestStarted, ResponseStarted};
    use crate::model::TxnKey;
    use bytes::Bytes;
    use traffic_police_proto::BodyDir;

    fn add(
        s: &mut SessionStore,
        txn: u64,
        url: &str,
        req_headers: Headers,
        status: u16,
        headers: Headers,
        body: &[u8],
    ) {
        let key = TxnKey { source: 0, txn };
        let at = txn * 1_000_000_000;
        s.apply(SessionEvent::Request(Box::new(RequestStarted {
            key,
            at,
            call: None,
            hop: 0,
            method: "GET".into(),
            url: url.into(),
            headers: req_headers,
            client: None,
            thread: None,
            stack: Vec::new(),
            stack_truncated: false,
            body: None,
            marks: Vec::new(),
            conn: None,
        })));
        s.apply(SessionEvent::Response(Box::new(ResponseStarted {
            key,
            at: at + 10,
            status,
            message: "OK".into(),
            protocol: Some("h2".into()),
            headers,
            conn: None,
        })));
        let bytes = Bytes::copy_from_slice(body);
        s.apply(SessionEvent::Body { key, dir: BodyDir::Response, at: at + 20, offset: 0, bytes });
        let n = body.len() as u64;
        s.apply(SessionEvent::BodyEnd {
            key,
            dir: BodyDir::Response,
            at: at + 30,
            total: n,
            captured: n,
            state: "complete".into(),
            decoded: false,
        });
        s.apply(SessionEvent::Completed { key, at: at + 40 });
    }

    fn h(pairs: &[(&str, &str)]) -> Headers {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    fn kinds(d: &Diff, kind: Kind) -> Vec<&str> {
        d.lines.iter().filter(|l| l.kind == kind).map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn json_key_order_does_not_matter_but_values_do() {
        let mut s = SessionStore::new();
        let ct = h(&[("content-type", "application/json")]);
        add(
            &mut s,
            1,
            "https://a.example/x?id=1",
            h(&[("accept", "*/*")]),
            200,
            ct.clone(),
            br#"{"b":1,"a":{"y":2,"x":1}}"#,
        );
        add(
            &mut s,
            2,
            "https://a.example/x?id=1",
            h(&[("accept", "*/*")]),
            200,
            ct.clone(),
            br#"{"a":{"x":1,"y":2},"b":1}"#,
        );
        add(&mut s, 3, "https://a.example/x?id=2", h(&[("accept", "*/*")]), 404, ct, br#"{"a":{"x":1,"y":3},"b":1}"#);
        let same = diff(&s, 0, 1, false);
        assert!(same.differing.is_empty(), "{}", same.text());
        let d = diff(&s, 0, 2, false);
        assert_eq!(d.differing, vec!["request", "response", "response body"], "{}", d.text());
        assert_eq!(kinds(&d, Kind::Removed), vec!["GET https://a.example/x?id=1", "h2 200 OK", r#"    "y": 2"#]);
        assert_eq!(kinds(&d, Kind::Added), vec!["GET https://a.example/x?id=2", "h2 404 OK", r#"    "y": 3"#]);
    }

    #[test]
    fn headers_in_order_or_as_sets() {
        let mut s = SessionStore::new();
        add(&mut s, 1, "https://a.example/", h(&[("Accept", "*/*"), ("X-Id", "1")]), 200, h(&[]), b"");
        add(&mut s, 2, "https://a.example/", h(&[("X-Id", "1"), ("Accept", "*/*")]), 200, h(&[]), b"");
        let ordered = diff(&s, 0, 1, false);
        assert_eq!(ordered.differing, vec!["request headers"]);
        assert!(ordered.lines.iter().any(|l| l.text == "the same headers in a different order"));
        let sets = diff(&s, 0, 1, true);
        assert!(sets.differing.is_empty(), "{}", sets.text());
        assert!(sets.text().contains("## Response body\n  no body in either"), "{}", sets.text());
    }

    #[test]
    fn long_bodies_keep_context_and_binary_bodies_compare_by_hash() {
        let mut s = SessionStore::new();
        let long_a: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let long_b = long_a.replace("line 20\n", "line twenty\n");
        add(&mut s, 1, "https://a.example/", h(&[]), 200, h(&[]), long_a.as_bytes());
        add(&mut s, 2, "https://a.example/", h(&[]), 200, h(&[]), long_b.as_bytes());
        let d = diff(&s, 0, 1, false);
        assert_eq!(kinds(&d, Kind::Gap), vec!["⋯ 17 same lines", "⋯ 16 same lines"], "{}", d.text());
        assert_eq!(kinds(&d, Kind::Removed), vec!["line 20"]);

        add(&mut s, 3, "https://a.example/", h(&[]), 200, h(&[]), b"\x89PNG\x00\x01");
        add(&mut s, 4, "https://a.example/", h(&[]), 200, h(&[]), b"\x89PNG\x00\x02");
        let d = diff(&s, 2, 3, false);
        let removed = kinds(&d, Kind::Removed);
        assert!(removed[0].starts_with("6 B · sha256 "), "{removed:?}");
        assert_eq!(d.differing, vec!["response body"]);
        let same = diff(&s, 2, 2, false);
        assert!(same.differing.is_empty());
    }
}
