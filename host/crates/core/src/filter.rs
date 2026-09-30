//! The filter bar's language (ARCHITECTURE.md §5.10): whitespace-separated tokens that must all
//! match; `-` in front of a token negates it; double quotes keep spaces inside a token.
//!
//! | Token | Meaning |
//! |---|---|
//! | `text` | case-insensitive substring of the URL |
//! | `/regex/`, `/regex/i` | regular expression on the URL |
//! | `method:GET,POST` | method |
//! | `status:200`, `status:4xx`, `status:failed`, `status:pending`, `status:>=400` | status |
//! | `host:*.example.com` | host glob (`*` matches anything) |
//! | `path:/api/**/status` | path glob (`*` within a segment, `**` across segments) |
//! | `type:json` | the Type column, or a part of the response Content-Type |
//! | `thread:worker` | part of the initiating thread's name |
//! | `size>10k`, `size<=2mb` | response size (b, k/kb, m/mb, g/gb; 1024-based) |
//! | `time>500ms`, `time<2s` | duration (us, ms, s, m) |
//! | `rule:modified`, `rule:<id>` | changed by any rule, or by one rule |
//! | `is:pinned` | pinned requests |
//! | `body:"needle"` | a request or response body contains the text (searched in the background) |

use std::ops::Range;

use bytes::Bytes;
use regex::{Regex, RegexBuilder};

use crate::decode::decode_body;
use crate::fmt::{NS_PER_MS, NS_PER_SEC, Ts};
use crate::model::{Headers, Transaction, TxnState, header};
use crate::rows::is_pending;

/// Where a filter stops making sense, for highlighting in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Byte range in the input.
    pub span: Range<usize>,
    pub message: String,
}

/// A parsed filter.
#[derive(Debug, Clone)]
pub struct Filter {
    /// The text it was parsed from.
    pub source: String,
    terms: Vec<Term>,
    needles: Vec<String>,
}

#[derive(Debug, Clone)]
struct Term {
    negate: bool,
    test: Test,
}

#[derive(Debug, Clone)]
enum Test {
    /// Case-insensitive literal (compiled), or a user regex.
    Url(Regex),
    Method(Vec<String>),
    Status(StatusTest),
    Host(Regex),
    Path(Regex),
    Type(String),
    Thread(String),
    Size(Cmp, u64),
    Time(Cmp, u64),
    RuleModified,
    Rule(String),
    Pinned,
    /// Index into [`Filter::needles`].
    Body(usize),
}

#[derive(Debug, Clone, Copy)]
enum StatusTest {
    Code(u16),
    Class(u16),
    Failed,
    Pending,
    Cmp(Cmp, u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

impl Cmp {
    fn holds(self, a: u64, b: u64) -> bool {
        match self {
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
            Cmp::Eq => a == b,
        }
    }

    /// Splits a leading comparison operator off `s` (none means equality).
    fn split(s: &str) -> (Cmp, &str) {
        for (op, c) in [(">=", Cmp::Ge), ("<=", Cmp::Le), (">", Cmp::Gt), ("<", Cmp::Lt), ("=", Cmp::Eq)] {
            if let Some(rest) = s.strip_prefix(op) {
                return (c, rest);
            }
        }
        (Cmp::Eq, s)
    }
}

struct Token {
    text: String,
    span: Range<usize>,
    quoted: bool,
}

fn tokens(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut out = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut text = String::new();
        let mut in_quotes = false;
        let mut quoted = false;
        let mut end = start;
        while let Some(&(i, c)) = chars.peek() {
            if c == '"' {
                in_quotes = !in_quotes;
                quoted = true;
            } else if c.is_whitespace() && !in_quotes {
                break;
            } else {
                text.push(c);
            }
            end = i + c.len_utf8();
            chars.next();
        }
        if in_quotes {
            return Err(ParseError { span: start..end, message: "the quote is not closed".into() });
        }
        out.push(Token { text, span: start..end, quoted });
    }
    Ok(out)
}

fn literal(s: &str) -> Regex {
    RegexBuilder::new(&regex::escape(s)).case_insensitive(true).build().expect("an escaped literal compiles")
}

/// `*` matches anything; `?` one character.
fn host_glob(g: &str) -> Result<Regex, regex::Error> {
    let mut re = String::from("^");
    for c in g.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    RegexBuilder::new(&re).case_insensitive(true).build()
}

/// `**` crosses segments, `*` and `?` stay within one; a trailing slash is optional, and so is
/// the leading one.
fn path_glob(g: &str) -> Result<Regex, regex::Error> {
    let mut re = String::from(if g.starts_with('/') { "^" } else { "^/?" });
    let mut chars = g.trim_end_matches('/').chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                re.push_str(".*");
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push_str("/?$");
    Regex::new(&re)
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit {
        "us" | "µs" => 1_000.0,
        "" | "ms" => NS_PER_MS as f64,
        "s" => NS_PER_SEC as f64,
        "m" | "min" => 60.0 * NS_PER_SEC as f64,
        _ => return None,
    };
    Some((n * mult) as u64)
}

impl Filter {
    /// Parses a filter; `Ok(None)` for an empty one.
    pub fn parse(input: &str) -> Result<Option<Filter>, ParseError> {
        let mut terms = Vec::new();
        let mut needles = Vec::new();
        for tok in tokens(input)? {
            let err = |message: String| ParseError { span: tok.span.clone(), message };
            let (negate, body) = match tok.text.strip_prefix('-') {
                Some(rest) if !rest.is_empty() => (true, rest.to_string()),
                _ => (false, tok.text.clone()),
            };
            // size<10k, time>=2s (a colon is allowed too: size:>10k)
            let comparison = ["size", "time"].into_iter().find_map(|k| {
                let rest = body.strip_prefix(k)?;
                let rest = rest.strip_prefix(':').unwrap_or(rest);
                rest.starts_with(['<', '>', '=']).then(|| (k, rest.to_string()))
            });
            let test = if let Some((key, rest)) = comparison {
                let (cmp, value) = Cmp::split(&rest);
                if key == "size" {
                    Test::Size(
                        cmp,
                        parse_size(value).ok_or_else(|| err(format!("{value:?} is not a size (like 10k or 2mb)")))?,
                    )
                } else {
                    Test::Time(
                        cmp,
                        parse_duration(value)
                            .ok_or_else(|| err(format!("{value:?} is not a duration (like 500ms or 2s)")))?,
                    )
                }
            } else if !tok.quoted
                && body.len() > 2
                && body.starts_with('/')
                && (body.ends_with('/') || body.ends_with("/i"))
            {
                let (pattern, insensitive) = match body.strip_suffix("/i") {
                    Some(p) => (&p[1..], true),
                    None => (&body[1..body.len() - 1], false),
                };
                let re = RegexBuilder::new(pattern).case_insensitive(insensitive).size_limit(1 << 20).build();
                Test::Url(re.map_err(|e| err(format!("bad regular expression: {e}")))?)
            } else if let Some((key, value)) = body.split_once(':').filter(|(k, _)| {
                matches!(*k, "method" | "status" | "host" | "path" | "type" | "thread" | "rule" | "is" | "body")
            }) {
                if value.is_empty() && key != "body" {
                    return Err(err(format!("{key}: needs a value")));
                }
                match key {
                    "method" => Test::Method(
                        value.split(',').filter(|m| !m.is_empty()).map(|m| m.to_ascii_uppercase()).collect(),
                    ),
                    "status" => Test::Status(match value.to_ascii_lowercase().as_str() {
                        "failed" => StatusTest::Failed,
                        "pending" => StatusTest::Pending,
                        v if v.len() == 3 && v.ends_with("xx") && (b'1'..=b'5').contains(&v.as_bytes()[0]) => {
                            StatusTest::Class(u16::from(v.as_bytes()[0] - b'0'))
                        }
                        v => {
                            let (cmp, n) = Cmp::split(v);
                            let code: u16 = n.parse().map_err(|_| {
                                err(format!("{value:?}: use a code (404), a class (4xx), failed, pending, or >=400"))
                            })?;
                            if cmp == Cmp::Eq { StatusTest::Code(code) } else { StatusTest::Cmp(cmp, code) }
                        }
                    }),
                    "host" => Test::Host(host_glob(value).map_err(|e| err(e.to_string()))?),
                    "path" => Test::Path(path_glob(value).map_err(|e| err(e.to_string()))?),
                    "type" => Test::Type(value.to_ascii_lowercase()),
                    "thread" => Test::Thread(value.to_lowercase()),
                    "rule" if value == "modified" => Test::RuleModified,
                    "rule" => Test::Rule(value.to_string()),
                    "is" if value == "pinned" => Test::Pinned,
                    "is" => return Err(err(format!("is:{value} is not known; is:pinned is"))),
                    _ => {
                        if value.is_empty() {
                            return Err(err("body: needs the text to look for, like body:\"sessionId\"".into()));
                        }
                        needles.push(value.to_lowercase());
                        Test::Body(needles.len() - 1)
                    }
                }
            } else {
                Test::Url(literal(&body))
            };
            terms.push(Term { negate, test });
        }
        if terms.is_empty() {
            return Ok(None);
        }
        Ok(Some(Filter { source: input.trim().to_string(), terms, needles }))
    }

    /// The texts of `body:` terms, lower-cased; results are asked for by index.
    pub fn needles(&self) -> &[String] {
        &self.needles
    }

    /// Whether `t` passes. `body(i)` answers `body:` term `i`: `None` while the search has not
    /// run yet, which hides the request until it has.
    pub fn matches(&self, t: &Transaction, now: Ts, body: &mut dyn FnMut(usize) -> Option<bool>) -> bool {
        for term in &self.terms {
            let hit = match &term.test {
                Test::Url(re) => re.is_match(&t.url.raw),
                Test::Method(ms) => ms.iter().any(|m| m.eq_ignore_ascii_case(&t.method)),
                Test::Status(s) => match s {
                    StatusTest::Code(c) => t.status() == Some(*c),
                    StatusTest::Class(k) => t.status().is_some_and(|s| s / 100 == *k),
                    StatusTest::Failed => t.state == TxnState::Failed || t.failure.is_some(),
                    StatusTest::Pending => is_pending(t),
                    StatusTest::Cmp(cmp, n) => t.status().is_some_and(|s| cmp.holds(u64::from(s), u64::from(*n))),
                },
                Test::Host(re) => re.is_match(&t.url.host),
                Test::Path(re) => re.is_match(&t.url.path),
                Test::Type(v) => {
                    t.type_label().eq_ignore_ascii_case(v)
                        || t.resp
                            .as_ref()
                            .and_then(|r| header(&r.headers, "content-type"))
                            .is_some_and(|ct| ct.to_ascii_lowercase().contains(v.as_str()))
                }
                Test::Thread(v) => t.thread.as_ref().is_some_and(|th| th.name.to_lowercase().contains(v.as_str())),
                Test::Size(cmp, n) => cmp.holds(t.response_size(), *n),
                Test::Time(cmp, n) => cmp.holds(t.duration(now), *n),
                Test::RuleModified => t.rule_modified(),
                Test::Rule(id) => t.rules.iter().any(|h| h.rules.iter().any(|r| r.id == *id)),
                Test::Pinned => t.pinned,
                Test::Body(i) => match body(*i) {
                    Some(hit) => hit,
                    None => return false,
                },
            };
            if hit == term.negate {
                return false;
            }
        }
        true
    }
}

/// A `body:` search to run off the UI thread: the transaction's bodies as captured, with the
/// headers that say how they are encoded.
#[derive(Debug, Clone)]
pub struct BodySearch {
    pub bodies: Vec<(Bytes, Option<Headers>)>,
    /// Lower-cased.
    pub needle: String,
}

impl BodySearch {
    /// Whether any body contains the needle, after Content-Encoding decoding; text is compared
    /// without regard to case, other bytes exactly.
    pub fn run(&self) -> bool {
        self.bodies.iter().any(|(raw, headers)| {
            if raw.is_empty() {
                return false;
            }
            let decoded = decode_body(raw.clone(), headers.as_ref(), 64 << 20);
            match std::str::from_utf8(&decoded.bytes) {
                Ok(text) => text.to_lowercase().contains(&self.needle),
                Err(_) => {
                    let n = self.needle.as_bytes();
                    !n.is_empty() && decoded.bytes.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TxnKey, Url};

    fn txn(method: &str, url: &str, status: Option<u16>) -> Transaction {
        let mut t = Transaction::new_placeholder(TxnKey { source: 1, txn: 1 }, 0);
        t.placeholder = false;
        t.method = method.into();
        t.url = Url::parse(url);
        t.state = TxnState::Complete;
        t.end = Some(250 * NS_PER_MS);
        if let Some(s) = status {
            t.resp = Some(crate::model::ResponseInfo {
                at: 100,
                status: s,
                message: String::new(),
                protocol: None,
                headers: vec![("content-type".into(), "application/json; charset=utf-8".into())],
            });
        }
        t
    }

    fn check(filter: &str, t: &Transaction) -> bool {
        Filter::parse(filter).unwrap().expect("not empty").matches(t, 0, &mut |_| Some(false))
    }

    #[test]
    fn tokens_combine_and_negate() {
        let t = txn("POST", "https://api.example.app/api/sdk/sim-binding/status/?sessionId=s_1", Some(200));
        assert!(check("sim-binding", &t));
        assert!(check("SIM-BINDING method:post,put status:2xx", &t));
        assert!(!check("sim-binding -method:POST", &t));
        assert!(check("host:*.example.app path:/api/**/status", &t));
        assert!(check("path:/api/sdk/*/status", &t));
        assert!(!check("path:/api/*/status", &t), "* stays within a segment");
        assert!(check("/session[Ii]d=s_\\d/", &t));
        assert!(check("/SESSIONID/i", &t));
        assert!(check("status:>=200 status:<300 type:json time<1s time>=250ms", &t));
        assert!(!check("status:4xx", &t));
        assert!(check("\"example.app/api\"", &t));
    }

    #[test]
    fn states_sizes_and_unknown_body_results() {
        let mut failed = txn("GET", "https://cdn.example.app/model.bin", None);
        failed.state = TxnState::Failed;
        assert!(check("status:failed", &failed));
        assert!(!check("status:pending", &failed));
        assert!(check("size<1k", &failed));
        let f = Filter::parse("model body:\"Weights\"").unwrap().unwrap();
        assert_eq!(f.needles(), ["weights"]);
        assert!(!f.matches(&failed, 0, &mut |_| None), "unknown body results hide the row");
        assert!(f.matches(&failed, 0, &mut |_| Some(true)));
    }

    #[test]
    fn errors_point_at_the_token() {
        let e = Filter::parse("host:x status:4x").unwrap_err();
        assert_eq!(e.span, 7..16);
        let e = Filter::parse("size>lots").unwrap_err();
        assert!(e.message.contains("not a size"), "{}", e.message);
        assert_eq!(Filter::parse("body:\"open").unwrap_err().span, 0..10);
        assert!(Filter::parse("   ").unwrap().is_none());
        // a URL with a scheme is text, not an unknown key
        assert!(Filter::parse("https://api").unwrap().is_some());
    }

    #[test]
    fn body_search_decodes_first() {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(br#"{"verdict":"PASS"}"#).unwrap();
        let raw = Bytes::from(gz.finish().unwrap());
        let headers = vec![("Content-Encoding".to_string(), "gzip".to_string())];
        let s = BodySearch { bodies: vec![(raw, Some(headers))], needle: "\"pass\"".into() };
        assert!(s.run());
    }
}
