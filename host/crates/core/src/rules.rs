//! The project's rules: `.traffic-police/rules.toml` (PROTOCOL.md §8.1), turned into the wire
//! form the app applies (§8.2). Every problem is reported with its line; a file with problems is
//! not used, so the rules that were active stay active (ARCHITECTURE.md §5.11).

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde::Deserialize;
use toml::Spanned;
use traffic_police_proto::msg::{Pattern, QueryMatch, Rule, RuleAction, RuleMatch, RuleSet};

use crate::project::DIR;

pub const FILE: &str = "rules.toml";

/// The `.traffic-police` directory for `start`: the nearest one at or above it.
pub fn find_dir(start: &Path) -> Option<PathBuf> {
    start.ancestors().map(|d| d.join(DIR)).find(|d| d.is_dir())
}

/// One rule as the file has it, for the Rules view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub name: Option<String>,
    pub enabled: bool,
    /// The line of its `[[rule]]` header.
    pub line: usize,
    /// Where its `enabled` value is, when it has one (for toggling in place).
    pub enabled_span: Option<Range<usize>>,
    /// The byte offset of the line after its header (to insert `enabled` when absent).
    pub body_start: usize,
}

/// Something wrong in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub line: Option<usize>,
    /// The rule it is about.
    pub rule: Option<String>,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(l) = self.line {
            write!(f, "line {l}: ")?;
        }
        if let Some(r) = &self.rule {
            write!(f, "rule {r}: ")?;
        }
        f.write_str(&self.message)
    }
}

/// The file, read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RulesFile {
    pub path: PathBuf,
    pub text: String,
    /// The rules in wire form (only meaningful when there are no problems).
    pub set: RuleSet,
    pub entries: Vec<Entry>,
    pub problems: Vec<Problem>,
    /// The files its body actions read (`file = "…"`), so they can be watched with it.
    pub files: Vec<PathBuf>,
}

impl RulesFile {
    pub fn is_valid(&self) -> bool {
        self.problems.is_empty()
    }
}

// --- the file's shape ------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileToml {
    version: Option<Spanned<i64>>,
    #[serde(default, rename = "rule")]
    rules: Vec<Spanned<RuleToml>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleToml {
    id: Option<Spanned<String>>,
    name: Option<String>,
    enabled: Option<Spanned<bool>>,
    cache_rewrites: Option<bool>,
    #[serde(rename = "match")]
    matcher: Option<Spanned<MatchToml>>,
    #[serde(default, rename = "action")]
    actions: Vec<Spanned<ActionToml>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchToml {
    methods: Option<Vec<String>>,
    scheme: Option<Spanned<String>>,
    host: Option<Spanned<PatternToml>>,
    port: Option<Spanned<i64>>,
    path: Option<Spanned<PatternToml>>,
    query: Option<BTreeMap<String, Spanned<PatternToml>>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PatternToml {
    Glob(String),
    Table(PatternTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatternTable {
    exact: Option<String>,
    glob: Option<String>,
    regex: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionToml {
    #[serde(rename = "type")]
    kind: String,
    ms: Option<i64>,
    exception: Option<String>,
    message: Option<String>,
    code: Option<i64>,
    reason: Option<String>,
    op: Option<String>,
    name: Option<String>,
    value: Option<String>,
    text: Option<String>,
    base64: Option<String>,
    file: Option<String>,
    content_type: Option<String>,
    find: Option<String>,
    with: Option<String>,
    regex: Option<bool>,
}

const EXCEPTIONS: [&str; 5] = ["timeout", "io", "protocol", "unknown_host", "connect"];
const MAX_DELAY_MS: i64 = 10 * 60 * 1000;

/// Reads the file; a missing file is an empty set.
pub fn load(path: &Path) -> RulesFile {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let base = path.parent().unwrap_or(Path::new("."));
            let mut f = parse(&text, base);
            f.path = path.to_path_buf();
            f
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            RulesFile { path: path.to_path_buf(), set: empty(), ..RulesFile::default() }
        }
        Err(e) => RulesFile {
            path: path.to_path_buf(),
            set: empty(),
            problems: vec![Problem { line: None, rule: None, message: format!("cannot read it: {e}") }],
            ..RulesFile::default()
        },
    }
}

fn empty() -> RuleSet {
    RuleSet { version: "none".into(), rules: Vec::new() }
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// Parses rules; `base` is where `file = "…"` paths start (the `.traffic-police` directory).
pub fn parse(text: &str, base: &Path) -> RulesFile {
    let mut out = RulesFile { text: text.to_string(), set: empty(), ..RulesFile::default() };
    let file: FileToml = match toml::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            let line = e.span().map(|s| line_of(text, s.start));
            out.problems.push(Problem { line, rule: None, message: first_line(e.message()) });
            return out;
        }
    };
    if let Some(v) = &file.version
        && *v.get_ref() != 1
    {
        out.problems.push(Problem {
            line: Some(line_of(text, v.span().start)),
            rule: None,
            message: format!("version {} is not known; this traffic-police reads version 1", v.get_ref()),
        });
    }
    let mut ids = HashSet::new();
    for (i, spanned) in file.rules.iter().enumerate() {
        let span = spanned.span();
        let r = spanned.get_ref();
        for a in &r.actions {
            if let Some(f) = &a.get_ref().file {
                out.files.push(base.join(f));
            }
        }
        // the table's span starts at its `[[rule]]` header
        let lead = text[span.start..].len() - text[span.start..].trim_start().len();
        let header = if text[span.start + lead..].starts_with("[[") {
            span.start + lead
        } else {
            text[..span.start].rfind("[[rule]]").unwrap_or(span.start)
        };
        let body_start = text[header..].find('\n').map_or(text.len(), |n| header + n + 1);
        let id = r.id.as_ref().map_or_else(|| format!("rule-{}", i + 1), |s| s.get_ref().clone());
        let line = line_of(text, header);
        out.entries.push(Entry {
            id: id.clone(),
            name: r.name.clone(),
            enabled: r.enabled.as_ref().is_none_or(|e| *e.get_ref()),
            line,
            enabled_span: r.enabled.as_ref().map(Spanned::span),
            body_start,
        });
        if !ids.insert(id.clone()) {
            let at = r.id.as_ref().map_or(line, |s| line_of(text, s.span().start));
            out.problems.push(Problem {
                line: Some(at),
                rule: Some(id.clone()),
                message: "another rule has this id".into(),
            });
        }
        let mut problems = Vec::new();
        let rule = to_wire(text, base, &id, r, &mut problems);
        out.problems.extend(problems.into_iter().map(|(line, message)| Problem {
            line: Some(line),
            rule: Some(id.clone()),
            message,
        }));
        if let Some(rule) = rule {
            out.set.rules.push(rule);
        }
    }
    if out.problems.is_empty() {
        out.set.version = version_of(&out.set);
    } else {
        out.set = empty();
    }
    out
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_string()
}

/// A short hash of the rules, so an app's `rules_ack` can be matched to what was sent.
fn version_of(set: &RuleSet) -> String {
    let json = serde_json::to_vec(&set.rules).unwrap_or_default();
    // FNV-1a: stable across runs and platforms
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in json {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", h as u32)
}

fn pattern(text: &str, p: &Spanned<PatternToml>, what: &str, problems: &mut Vec<(usize, String)>) -> Option<Pattern> {
    let line = line_of(text, p.span().start);
    match p.get_ref() {
        PatternToml::Glob(g) => Some(Pattern::Glob(g.clone())),
        PatternToml::Table(t) => match (&t.exact, &t.glob, &t.regex) {
            (Some(e), None, None) => Some(Pattern::Exact(e.clone())),
            (None, Some(g), None) => Some(Pattern::Glob(g.clone())),
            (None, None, Some(r)) => match check_regex(r) {
                Ok(()) => Some(Pattern::Regex(r.clone())),
                Err(e) => {
                    problems.push((line, format!("{what}: {e}")));
                    None
                }
            },
            _ => {
                problems.push((line, format!("{what}: give exactly one of exact, glob or regex")));
                None
            }
        },
    }
}

/// Regexes run on the device (java.util.regex); Rust's regex parses a subset of that syntax, so
/// only errors that are errors there too are reported here (the app reports the rest).
pub fn check_regex(r: &str) -> Result<(), String> {
    match regex::Regex::new(r) {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("look-around") || msg.contains("backreferences") || msg.contains("size limit") {
                Ok(())
            } else {
                Err(msg
                    .lines()
                    .filter(|l| l.starts_with("error:"))
                    .map(|l| l.trim_start_matches("error: ").to_string())
                    .next()
                    .unwrap_or(msg))
            }
        }
    }
}

fn to_wire(text: &str, base: &Path, id: &str, r: &RuleToml, problems: &mut Vec<(usize, String)>) -> Option<Rule> {
    let before = problems.len();
    let mut m = RuleMatch::default();
    if let Some(sm) = &r.matcher {
        let t = sm.get_ref();
        let at = line_of(text, sm.span().start);
        if let Some(methods) = &t.methods {
            for method in methods {
                if method.is_empty() || !method.chars().all(|c| c.is_ascii_alphabetic()) {
                    problems.push((at, format!("methods: {method:?} is not a method name")));
                }
            }
            m.methods = methods.iter().map(|s| s.to_ascii_uppercase()).collect();
        }
        if let Some(s) = &t.scheme {
            match s.get_ref().as_str() {
                "http" | "https" => m.scheme = Some(s.get_ref().clone()),
                other => problems.push((line_of(text, s.span().start), format!("scheme {other:?}: use http or https"))),
            }
        }
        if let Some(h) = &t.host {
            m.host = pattern(text, h, "host", problems);
        }
        if let Some(p) = &t.port {
            match u16::try_from(*p.get_ref()) {
                Ok(port) if port > 0 => m.port = Some(port),
                _ => problems.push((line_of(text, p.span().start), format!("port {}: use 1 to 65535", p.get_ref()))),
            }
        }
        if let Some(p) = &t.path {
            m.path = pattern(text, p, "path", problems);
        }
        if let Some(q) = &t.query {
            for (name, value) in q {
                let value = pattern(text, value, &format!("query {name}"), problems);
                m.query.push(QueryMatch { name: name.clone(), value });
            }
        }
    }
    let mut actions = Vec::new();
    for a in &r.actions {
        let at = line_of(text, a.span().start);
        match action(a.get_ref(), base) {
            Ok(x) => actions.push(x),
            Err(e) => problems.push((at, e)),
        }
    }
    if problems.len() > before {
        return None;
    }
    Some(Rule {
        id: id.to_string(),
        name: r.name.clone(),
        enabled: r.enabled.as_ref().is_none_or(|e| *e.get_ref()),
        matcher: m,
        actions,
        cache_rewrites: r.cache_rewrites.unwrap_or(false),
    })
}

/// OkHttp accepts printable ASCII and tabs in header values.
fn header_value_ok(v: &str) -> bool {
    v.chars().all(|c| c == '\t' || (' '..='~').contains(&c))
}

fn action(a: &ActionToml, base: &Path) -> Result<RuleAction, String> {
    // fields that belong to other action types are mistakes worth pointing at
    let given: Vec<(&str, bool)> = vec![
        ("ms", a.ms.is_some()),
        ("exception", a.exception.is_some()),
        ("message", a.message.is_some()),
        ("code", a.code.is_some()),
        ("reason", a.reason.is_some()),
        ("op", a.op.is_some()),
        ("name", a.name.is_some()),
        ("value", a.value.is_some()),
        ("text", a.text.is_some()),
        ("base64", a.base64.is_some()),
        ("file", a.file.is_some()),
        ("content_type", a.content_type.is_some()),
        ("find", a.find.is_some()),
        ("with", a.with.is_some()),
        ("regex", a.regex.is_some()),
    ];
    let allowed: &[&str] = match a.kind.as_str() {
        "delay" => &["ms"],
        "fail" => &["exception", "message"],
        "status" => &["code", "reason"],
        "header" => &["op", "name", "value"],
        "body" => &["text", "base64", "file", "content_type"],
        "replace" => &["find", "with", "regex"],
        other => {
            return Err(format!(
                "action type {other:?} is not known: use delay, fail, status, header, body or replace"
            ));
        }
    };
    if let Some((field, _)) = given.iter().find(|(f, set)| *set && !allowed.contains(f)) {
        return Err(format!("{field} does not belong to a {} action", a.kind));
    }
    Ok(match a.kind.as_str() {
        "delay" => {
            let ms = a.ms.ok_or("a delay needs ms")?;
            if !(0..=MAX_DELAY_MS).contains(&ms) {
                return Err(format!("delay ms {ms}: use 0 to {MAX_DELAY_MS}"));
            }
            RuleAction::Delay { ms: ms as u64 }
        }
        "fail" => {
            let e =
                a.exception.clone().ok_or("a fail needs exception (timeout, io, protocol, unknown_host or connect)")?;
            if !EXCEPTIONS.contains(&e.as_str()) {
                return Err(format!("exception {e:?}: use timeout, io, protocol, unknown_host or connect"));
            }
            RuleAction::Fail { exception: e, message: a.message.clone() }
        }
        "status" => {
            let code = a.code.ok_or("a status needs code")?;
            if !(100..=599).contains(&code) {
                return Err(format!("status code {code}: use 100 to 599"));
            }
            RuleAction::Status { code: code as u16, reason: a.reason.clone() }
        }
        "header" => {
            let op = a.op.clone().ok_or("a header action needs op (add, set or remove)")?;
            if !matches!(op.as_str(), "add" | "set" | "remove") {
                return Err(format!("op {op:?}: use add, set or remove"));
            }
            let name = a.name.clone().ok_or("a header action needs name")?;
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)) {
                return Err(format!("header name {name:?} is not valid"));
            }
            if op != "remove" {
                let v = a.value.as_deref().ok_or("add and set need value")?;
                if !header_value_ok(v) {
                    return Err("header values must be printable ASCII (OkHttp refuses others)".into());
                }
            }
            RuleAction::Header { op, name, value: a.value.clone() }
        }
        "body" => {
            let (text, base64) = match (&a.text, &a.base64, &a.file) {
                (Some(t), None, None) => (Some(t.clone()), None),
                (None, Some(b), None) => {
                    base64::engine::general_purpose::STANDARD
                        .decode(b.trim())
                        .map_err(|e| format!("base64 does not decode: {e}"))?;
                    (None, Some(b.trim().to_string()))
                }
                (None, None, Some(f)) => {
                    let path = base.join(f);
                    let bytes = std::fs::read(&path).map_err(|e| format!("file {}: {e}", path.display()))?;
                    match String::from_utf8(bytes) {
                        Ok(s) => (Some(s), None),
                        Err(e) => (None, Some(base64::engine::general_purpose::STANDARD.encode(e.as_bytes()))),
                    }
                }
                _ => return Err("a body action has exactly one of text, base64 or file".into()),
            };
            if let Some(ct) = &a.content_type
                && !header_value_ok(ct)
            {
                return Err("content_type must be printable ASCII".into());
            }
            RuleAction::Body { text, base64, content_type: a.content_type.clone() }
        }
        _ => {
            let find = a.find.clone().filter(|f| !f.is_empty()).ok_or("replace needs find (not empty)")?;
            let with = a.with.clone().ok_or("replace needs with")?;
            let regex = a.regex.unwrap_or(false);
            if regex {
                check_regex(&find).map_err(|e| format!("find: {e}"))?;
            }
            RuleAction::Replace { find, with, regex }
        }
    })
}

// --- editing in place (comments and layout stay as they are) --------------------------------

/// The file with the rule `id` enabled or disabled.
pub fn with_enabled(file: &RulesFile, id: &str, on: bool) -> Option<String> {
    let e = file.entries.iter().find(|e| e.id == id)?;
    let mut text = file.text.clone();
    match &e.enabled_span {
        Some(span) => text.replace_range(span.clone(), if on { "true" } else { "false" }),
        None => text.insert_str(e.body_start, &format!("enabled = {on}\n")),
    }
    Some(text)
}

/// A new rule for a request, appended to `text`; returns the text and the new rule's line.
pub fn with_new_rule(text: &str, rule: &NewRule) -> (String, usize) {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if out.trim().is_empty() {
        out = "version = 1\n".into();
    }
    out.push('\n');
    let line = out.matches('\n').count() + 1;
    let q = |s: &str| toml::Value::String(s.to_string()).to_string();
    out.push_str("[[rule]]\n");
    out.push_str(&format!("id = {}\n", q(&rule.id)));
    out.push_str(&format!("name = {}\n", q(&rule.name)));
    out.push_str("# off until you say what it does: Space in the Rules view, or enabled = true\n");
    out.push_str("enabled = false\n");
    out.push_str("\n  [rule.match]\n");
    out.push_str(&format!("  methods = [{}]\n", q(&rule.method)));
    out.push_str(&format!("  scheme = {}\n", q(&rule.scheme)));
    out.push_str(&format!("  host = {{ exact = {} }}\n", q(&rule.host)));
    out.push_str(&format!("  port = {}\n", rule.port));
    out.push_str(&format!("  path = {{ exact = {} }}\n", q(&rule.path)));
    if !rule.query.is_empty() {
        let parts: Vec<String> = rule.query.iter().map(|n| format!("{} = \"*\"", toml_key(n))).collect();
        out.push_str(&format!("  query = {{ {} }}\n", parts.join(", ")));
    }
    out.push_str("\n  # what it does: delay, fail, status, header, body or replace (docs/PROTOCOL.md §8)\n");
    out.push_str("  [[rule.action]]\n  type = \"status\"\n  code = 500\n");
    (out, line)
}

fn toml_key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        k.to_string()
    } else {
        toml::Value::String(k.to_string()).to_string()
    }
}

/// What `r` knows about the selected request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRule {
    pub id: String,
    pub name: String,
    pub method: String,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub query: Vec<String>,
}

/// An id not taken yet, from the request's last path segment.
pub fn fresh_id(file: &RulesFile, path: &str) -> String {
    let stem: String = path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("rule")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let stem = stem.trim_matches('-').to_string();
    let stem = if stem.is_empty() { "rule".to_string() } else { stem };
    let taken: HashSet<&str> = file.entries.iter().map(|e| e.id.as_str()).collect();
    if !taken.contains(stem.as_str()) {
        return stem;
    }
    (2..).map(|n| format!("{stem}-{n}")).find(|c| !taken.contains(c.as_str())).expect("some number is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"version = 1

# slow down the status poll
[[rule]]
id = "slow-status"
name = "Slow status poll"
enabled = true

  [rule.match]
  methods = ["GET"]
  scheme = "https"
  host = "*.example.app"
  port = 443
  path = "/api/sdk/*/status/**"
  query = { sessionId = "*" }

  [[rule.action]]
  type = "delay"
  ms = 3000

[[rule]]
id = "force-pass"

  [rule.match]
  path = { exact = "/api/sdk/sim-binding/status/" }

  [[rule.action]]
  type = "replace"
  find = '"verdict":"pending"'
  with = '"verdict":"pass"'

  [[rule.action]]
  type = "header"
  op = "set"
  name = "Cache-Control"
  value = "no-store"

[[rule]]
name = "stub"

  [rule.match]
  path = { regex = '^/config(\?|$)' }

  [[rule.action]]
  type = "body"
  file = "fixtures/config.json"
  content_type = "application/json"
"#;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tp-rules-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("fixtures")).unwrap();
        d
    }

    #[test]
    fn the_readme_example_is_valid() {
        let readme = include_str!("../../../../README.md");
        let section = &readme[readme.find("\n## Rules\n").expect("the README's Rules section")..];
        let start = section.find("```toml\n").expect("its example") + "```toml\n".len();
        let end = start + section[start..].find("```").expect("the example's end");
        let f = parse(&section[start..end], Path::new("."));
        assert!(f.is_valid(), "{:?}", f.problems);
        assert_eq!(
            f.entries.iter().map(|e| (e.id.as_str(), e.enabled)).collect::<Vec<_>>(),
            [("force-pass", true), ("enroll-down", false)]
        );
    }

    #[test]
    fn the_protocol_example_is_valid() {
        let doc = include_str!("../../../../docs/PROTOCOL.md");
        let section = &doc[doc.find("### 8.1 File format").expect("PROTOCOL.md §8.1")..];
        let start = section.find("```toml\n").expect("its example") + "```toml\n".len();
        let end = start + section[start..].find("```").expect("the example's end");
        let base = dir("protocol");
        std::fs::write(base.join("fixtures/config-error.json"), "{\"error\":true}").unwrap();
        let f = parse(&section[start..end], &base);
        assert!(f.is_valid(), "{:?}", f.problems);
        assert_eq!(f.entries.len(), 4);
    }

    #[test]
    fn the_files_bodies_come_from_are_listed_to_be_watched() {
        let base = dir("files");
        // listed even when missing: the rule works once the file appears
        let f = parse(EXAMPLE, &base);
        assert_eq!(f.files, vec![base.join("fixtures/config.json")]);
        assert!(!f.is_valid());
    }

    #[test]
    fn the_documented_example_becomes_the_wire_form() {
        let base = dir("example");
        std::fs::write(base.join("fixtures/config.json"), "{\"error\":true}").unwrap();
        let f = parse(EXAMPLE, &base);
        assert!(f.is_valid(), "{:?}", f.problems);
        assert_eq!(f.set.rules.len(), 3);
        assert_eq!(f.set.version.len(), 8);
        let slow = &f.set.rules[0];
        assert_eq!(slow.matcher.host, Some(Pattern::Glob("*.example.app".into())));
        assert_eq!(slow.matcher.port, Some(443));
        assert_eq!(
            slow.matcher.query,
            vec![QueryMatch { name: "sessionId".into(), value: Some(Pattern::Glob("*".into())) }]
        );
        assert_eq!(slow.actions, vec![RuleAction::Delay { ms: 3000 }]);
        let stub = &f.set.rules[2];
        assert_eq!(stub.id, "rule-3", "a missing id is made up from the position");
        assert_eq!(
            stub.actions,
            vec![RuleAction::Body {
                text: Some("{\"error\":true}".into()),
                base64: None,
                content_type: Some("application/json".into())
            }]
        );
        // the wire JSON is what PROTOCOL.md §8.2 shows
        let json = serde_json::to_value(&f.set.rules[1]).unwrap();
        assert_eq!(json["match"]["path"], serde_json::json!({ "exact": "/api/sdk/sim-binding/status/" }));
        assert_eq!(json["actions"][0]["type"], "replace");
        assert_eq!(f.entries.iter().map(|e| e.line).collect::<Vec<_>>(), vec![4, 21, 38]);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn problems_have_lines_and_the_set_is_not_used() {
        let text = r#"[[rule]]
id = "a"
  [rule.match]
  scheme = "ftp"
  path = { exact = "/x", glob = "/y" }
  [[rule.action]]
  type = "delay"
  ms = -1
  [[rule.action]]
  type = "status"
  code = 200
  ms = 5
[[rule]]
id = "a"
  [[rule.action]]
  type = "replace"
  find = "(open"
  with = "x"
  regex = true
[[rule]]
id = "fine"
  [rule.match]
  path = { regex = '(?<=/)x' }
"#;
        let f = parse(text, Path::new("."));
        let got: Vec<String> = f.problems.iter().map(ToString::to_string).collect();
        assert_eq!(
            got,
            vec![
                "line 4: rule a: scheme \"ftp\": use http or https",
                "line 5: rule a: path: give exactly one of exact, glob or regex",
                "line 6: rule a: delay ms -1: use 0 to 600000",
                "line 9: rule a: ms does not belong to a status action",
                "line 14: rule a: another rule has this id",
                "line 15: rule a: find: unclosed group",
            ],
            "{got:#?}"
        );
        assert!(f.set.rules.is_empty(), "a file with problems is not used");
        assert!(f.entries.iter().any(|e| e.id == "fine"), "look-behind is left to the app");
        // broken TOML: one problem, with its line
        let f = parse("[[rule]]\nid = \"x\"\nbogus = 1\n", Path::new("."));
        assert_eq!(f.problems.len(), 1);
        assert_eq!(f.problems[0].line, Some(3));
        assert!(f.problems[0].message.contains("bogus"), "{}", f.problems[0].message);
    }

    #[test]
    fn toggling_and_adding_keep_the_rest_of_the_file() {
        let f =
            parse(EXAMPLE.replace("  file = \"fixtures/config.json\"\n", "  text = \"{}\"\n").as_str(), Path::new("."));
        assert!(f.is_valid(), "{:?}", f.problems);
        // a rule with enabled: its value changes in place
        let off = with_enabled(&f, "slow-status", false).unwrap();
        assert!(off.contains("# slow down the status poll\n[[rule]]\nid = \"slow-status\"\nname = \"Slow status poll\"\nenabled = false\n"));
        let again = parse(&off, Path::new("."));
        assert!(!again.entries[0].enabled);
        assert!(!again.set.rules[0].enabled);
        // one without: the key is added under its header
        let off = with_enabled(&f, "force-pass", false).unwrap();
        assert!(off.contains("[[rule]]\nenabled = false\nid = \"force-pass\""), "{off}");
        assert!(!parse(&off, Path::new(".")).entries[1].enabled);
        // a new rule for a request, appended; it parses and matches the request exactly
        let new = NewRule {
            id: fresh_id(&f, "/api/sdk/init"),
            name: "POST /api/sdk/init".into(),
            method: "POST".into(),
            scheme: "https".into(),
            host: "deepid.example.app".into(),
            port: 443,
            path: "/api/sdk/init".into(),
            query: vec!["session id".into(), "v".into()],
        };
        assert_eq!(new.id, "init");
        let (text, line) = with_new_rule(&f.text, &new);
        let g = parse(&text, Path::new("."));
        assert!(g.is_valid(), "{:?}\n{text}", g.problems);
        let r = g.set.rules.last().unwrap();
        assert!(!r.enabled, "a new rule starts off");
        assert_eq!(r.matcher.path, Some(Pattern::Exact("/api/sdk/init".into())));
        assert_eq!(r.matcher.query.len(), 2);
        assert_eq!(g.entries.last().unwrap().line, line);
        // into an empty file
        let (text, _) = with_new_rule("", &new);
        assert!(text.starts_with("version = 1\n"));
        assert!(parse(&text, Path::new(".")).is_valid());
    }
}
