//! The rule form's model (ARCHITECTURE.md §5.11.1): one rule of `rules.toml` as editable fields,
//! read from the file, checked with the file's own checks (each problem tied to its field),
//! written back without disturbing the rest of the file (comments and layout stay), and tried
//! against captured requests the way the app matches them (PROTOCOL.md §8.2).

use std::path::Path;

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, TableLike, Value};
use traffic_police_proto::msg::{Pattern, RuleMatch};

use crate::model::Transaction;
use crate::rules::{self, RulesFile};

/// The methods the form offers as a checklist (others in the file are kept and shown too).
pub const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
/// `fail` exceptions (PROTOCOL.md §8.1).
pub const EXCEPTIONS: [&str; 5] = ["timeout", "io", "protocol", "unknown_host", "connect"];
pub const HEADER_OPS: [&str; 3] = ["set", "add", "remove"];
pub const ACTION_TYPES: [&str; 6] = ["delay", "fail", "status", "header", "body", "replace"];

/// How a host, path or query value is matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PatternKind {
    Exact,
    /// `*` within a segment, `**` across segments, `?` one character (the file's plain string).
    #[default]
    Glob,
    Regex,
}

impl PatternKind {
    pub const ALL: [PatternKind; 3] = [PatternKind::Exact, PatternKind::Glob, PatternKind::Regex];

    pub fn name(self) -> &'static str {
        match self {
            PatternKind::Exact => "exact",
            PatternKind::Glob => "glob",
            PatternKind::Regex => "regex",
        }
    }
}

/// A pattern being edited; empty text means "any".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PatternDraft {
    pub kind: PatternKind,
    pub text: String,
}

impl PatternDraft {
    fn wire(&self) -> Option<Pattern> {
        if self.text.is_empty() {
            return None;
        }
        Some(match self.kind {
            PatternKind::Exact => Pattern::Exact(self.text.clone()),
            PatternKind::Glob => Pattern::Glob(self.text.clone()),
            PatternKind::Regex => Pattern::Regex(self.text.clone()),
        })
    }

    /// The file's form: a glob as a plain string, the others as `{ exact = … }` / `{ regex = … }`.
    fn toml(&self) -> Value {
        match self.kind {
            PatternKind::Glob => Value::from(self.text.as_str()),
            k => {
                let mut t = InlineTable::new();
                t.insert(k.name(), Value::from(self.text.as_str()));
                Value::InlineTable(t)
            }
        }
    }

    fn read(v: &Item) -> Option<PatternDraft> {
        if let Some(s) = v.as_str() {
            return Some(PatternDraft { kind: PatternKind::Glob, text: s.to_string() });
        }
        let t = v.as_table_like()?;
        PatternKind::ALL
            .into_iter()
            .find_map(|k| t.get(k.name()).and_then(Item::as_str).map(|s| PatternDraft { kind: k, text: s.to_string() }))
    }
}

/// Where a body action's bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodySource {
    #[default]
    Text,
    /// A file in `.traffic-police/`, read on the host.
    File,
    Base64,
}

impl BodySource {
    pub const ALL: [BodySource; 3] = [BodySource::Text, BodySource::File, BodySource::Base64];

    pub fn key(self) -> &'static str {
        match self {
            BodySource::Text => "text",
            BodySource::File => "file",
            BodySource::Base64 => "base64",
        }
    }
}

/// One action being edited (PROTOCOL.md §8.1); numbers stay text until they are checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionDraft {
    Delay { ms: String },
    Fail { exception: String, message: String },
    Status { code: String, reason: String },
    Header { op: String, name: String, value: String },
    Body { source: BodySource, value: String, content_type: String },
    Replace { find: String, with: String, regex: bool },
}

impl ActionDraft {
    /// A new action of a type, with values that pass the checks where there is an obvious one.
    pub fn new(kind: &str) -> ActionDraft {
        match kind {
            "delay" => ActionDraft::Delay { ms: "2000".into() },
            "fail" => ActionDraft::Fail { exception: "timeout".into(), message: String::new() },
            "status" => ActionDraft::Status { code: "500".into(), reason: String::new() },
            "header" => ActionDraft::Header { op: "set".into(), name: String::new(), value: String::new() },
            "body" => ActionDraft::Body {
                source: BodySource::Text,
                value: String::new(),
                content_type: "application/json".into(),
            },
            _ => ActionDraft::Replace { find: String::new(), with: String::new(), regex: false },
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            ActionDraft::Delay { .. } => "delay",
            ActionDraft::Fail { .. } => "fail",
            ActionDraft::Status { .. } => "status",
            ActionDraft::Header { .. } => "header",
            ActionDraft::Body { .. } => "body",
            ActionDraft::Replace { .. } => "replace",
        }
    }

    /// The action's keys as the file has them (empty optional values left out).
    fn entries(&self) -> Vec<(&'static str, Value)> {
        let s = |v: &str| Value::from(v);
        let num = |v: &str| v.trim().parse::<i64>().map_or_else(|_| s(v), Value::from);
        let mut out = vec![("type", s(self.kind()))];
        match self {
            ActionDraft::Delay { ms } => out.push(("ms", num(ms))),
            ActionDraft::Fail { exception, message } => {
                out.push(("exception", s(exception)));
                if !message.is_empty() {
                    out.push(("message", s(message)));
                }
            }
            ActionDraft::Status { code, reason } => {
                out.push(("code", num(code)));
                if !reason.is_empty() {
                    out.push(("reason", s(reason)));
                }
            }
            ActionDraft::Header { op, name, value } => {
                out.push(("op", s(op)));
                out.push(("name", s(name)));
                if op != "remove" {
                    out.push(("value", s(value)));
                }
            }
            ActionDraft::Body { source, value, content_type } => {
                out.push((source.key(), s(value)));
                if !content_type.is_empty() {
                    out.push(("content_type", s(content_type)));
                }
            }
            ActionDraft::Replace { find, with, regex } => {
                out.push(("find", s(find)));
                out.push(("with", s(with)));
                if *regex {
                    out.push(("regex", Value::from(true)));
                }
            }
        }
        out
    }

    /// Keys that say the default out loud (`regex = false`): kept where the file has them.
    fn stated_defaults(&self) -> Vec<(&'static str, Value)> {
        match self {
            ActionDraft::Replace { regex: false, .. } => vec![("regex", Value::from(false))],
            _ => Vec::new(),
        }
    }

    fn read(t: &dyn TableLike) -> Option<ActionDraft> {
        let s = |k: &str| t.get(k).and_then(Item::as_str).map(str::to_string).unwrap_or_default();
        let n = |k: &str| match t.get(k) {
            Some(i) => i.as_integer().map_or_else(|| i.as_str().unwrap_or_default().to_string(), |n| n.to_string()),
            None => String::new(),
        };
        Some(match t.get("type")?.as_str()? {
            "delay" => ActionDraft::Delay { ms: n("ms") },
            "fail" => ActionDraft::Fail { exception: s("exception"), message: s("message") },
            "status" => ActionDraft::Status { code: n("code"), reason: s("reason") },
            "header" => ActionDraft::Header { op: s("op"), name: s("name"), value: s("value") },
            "body" => {
                let source = BodySource::ALL.into_iter().find(|b| t.contains_key(b.key())).unwrap_or(BodySource::Text);
                ActionDraft::Body { source, value: s(source.key()), content_type: s("content_type") }
            }
            "replace" => ActionDraft::Replace {
                find: s("find"),
                with: s("with"),
                regex: t.get("regex").and_then(Item::as_bool).unwrap_or(false),
            },
            _ => return None,
        })
    }
}

/// An action and the position it had in the file (`None`: new), so a saved rule keeps each
/// action's comments when actions are moved, added or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSlot {
    pub action: ActionDraft,
    pub origin: Option<usize>,
}

/// One rule, as the form edits it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuleDraft {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub cache_rewrites: bool,
    /// Upper case; empty means any method.
    pub methods: Vec<String>,
    /// `http`, `https`, or empty for either.
    pub scheme: String,
    pub host: PatternDraft,
    /// Empty for any port.
    pub port: String,
    pub path: PatternDraft,
    pub query: Vec<(String, PatternDraft)>,
    pub actions: Vec<ActionSlot>,
}

/// A part of the form a problem belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Id,
    Name,
    Enabled,
    CacheRewrites,
    Methods,
    Scheme,
    Host,
    Port,
    Path,
    Query(usize),
    /// The action as a whole (the file reports an action's problems at its table).
    Action(usize),
    /// The rule as a whole (something no single field explains).
    Rule,
}

impl RuleDraft {
    /// A new rule that matches a request exactly: method, scheme, host, port, path, and its query
    /// parameters with any value. On, and with no actions yet.
    pub fn for_request(file: &RulesFile, t: &Transaction) -> RuleDraft {
        let mut query: Vec<(String, PatternDraft)> = Vec::new();
        for (name, _) in t.url.query_pairs() {
            if !query.iter().any(|(n, _)| *n == name) {
                query.push((name, PatternDraft { kind: PatternKind::Glob, text: "*".into() }));
            }
        }
        RuleDraft {
            id: rules::fresh_id(file, &t.url.path),
            name: format!("{} {}", t.method, t.url.path),
            enabled: true,
            methods: vec![t.method.to_ascii_uppercase()],
            scheme: t.url.scheme.clone(),
            host: PatternDraft { kind: PatternKind::Exact, text: t.url.host.clone() },
            port: t.url.effective_port().map(|p| p.to_string()).unwrap_or_default(),
            path: PatternDraft { kind: PatternKind::Exact, text: t.url.path.clone() },
            query,
            ..RuleDraft::default()
        }
    }

    /// An empty new rule with an id not taken yet.
    pub fn blank(file: &RulesFile) -> RuleDraft {
        RuleDraft { id: rules::fresh_id(file, "/rule"), enabled: true, ..RuleDraft::default() }
    }

    /// The rule at `index` of the file (its `[[rule]]` tables in order), as the file has it.
    pub fn read(text: &str, index: usize) -> Result<RuleDraft, String> {
        let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.message().to_string())?;
        let t = rule_tables(&doc).get(index).copied().ok_or("no such rule in the file")?;
        let s = |k: &str| t.get(k).and_then(Item::as_str).map(str::to_string).unwrap_or_default();
        let mut d = RuleDraft {
            id: s("id"),
            name: s("name"),
            enabled: t.get("enabled").and_then(Item::as_bool).unwrap_or(true),
            cache_rewrites: t.get("cache_rewrites").and_then(Item::as_bool).unwrap_or(false),
            ..RuleDraft::default()
        };
        if d.id.is_empty() {
            d.id = format!("rule-{}", index + 1);
        }
        if let Some(m) = t.get("match").and_then(Item::as_table_like) {
            if let Some(a) = m.get("methods").and_then(Item::as_array) {
                d.methods = a.iter().filter_map(Value::as_str).map(str::to_ascii_uppercase).collect();
            }
            d.scheme = m.get("scheme").and_then(Item::as_str).unwrap_or_default().to_string();
            d.host = m.get("host").and_then(PatternDraft::read).unwrap_or_default();
            d.path = m.get("path").and_then(PatternDraft::read).unwrap_or_default();
            d.port = m.get("port").and_then(Item::as_integer).map(|p| p.to_string()).unwrap_or_default();
            if let Some(q) = m.get("query").and_then(Item::as_table_like) {
                for (name, v) in q.iter() {
                    d.query.push((name.to_string(), PatternDraft::read(v).unwrap_or_default()));
                }
            }
        }
        for (i, a) in action_tables(t).into_iter().enumerate() {
            if let Some(action) = ActionDraft::read(a) {
                d.actions.push(ActionSlot { action, origin: Some(i) });
            }
        }
        Ok(d)
    }

    /// The rule's match in wire form, for trying it on captured requests (`None` when part of
    /// it does not parse: the problems say what).
    pub fn wire_match(&self) -> Option<RuleMatch> {
        let port = if self.port.trim().is_empty() { None } else { Some(self.port.trim().parse::<u16>().ok()?) };
        Some(RuleMatch {
            methods: self.methods.clone(),
            scheme: (!self.scheme.is_empty()).then(|| self.scheme.clone()),
            host: self.host.wire(),
            port,
            path: self.path.wire(),
            query: self
                .query
                .iter()
                .map(|(n, p)| traffic_police_proto::msg::QueryMatch { name: n.clone(), value: p.wire() })
                .collect(),
        })
    }

    /// The rule as its own TOML (the layout `rules.toml` uses), and which field each line is.
    fn text(&self, expand_query: bool) -> (String, Vec<Field>) {
        let mut text = String::new();
        let mut fields = Vec::new();
        let mut line = |t: &mut String, f: Field, s: String| {
            t.push_str(&s);
            t.push('\n');
            fields.push(f);
        };
        let q = |s: &str| Value::from(s).to_string().trim().to_string();
        line(&mut text, Field::Rule, "[[rule]]".into());
        line(&mut text, Field::Id, format!("id = {}", q(&self.id)));
        if !self.name.is_empty() {
            line(&mut text, Field::Name, format!("name = {}", q(&self.name)));
        }
        line(&mut text, Field::Enabled, format!("enabled = {}", self.enabled));
        if self.cache_rewrites {
            line(&mut text, Field::CacheRewrites, "cache_rewrites = true".into());
        }
        let any_match = !self.methods.is_empty()
            || !self.scheme.is_empty()
            || !self.host.text.is_empty()
            || !self.port.trim().is_empty()
            || !self.path.text.is_empty()
            || !self.query.is_empty();
        if any_match {
            line(&mut text, Field::Rule, String::new());
            // the file reports method problems at the match table
            line(&mut text, Field::Methods, "  [rule.match]".into());
            if !self.methods.is_empty() {
                let list: Vec<String> = self.methods.iter().map(|m| q(m)).collect();
                line(&mut text, Field::Methods, format!("  methods = [{}]", list.join(", ")));
            }
            if !self.scheme.is_empty() {
                line(&mut text, Field::Scheme, format!("  scheme = {}", q(&self.scheme)));
            }
            if !self.host.text.is_empty() {
                line(&mut text, Field::Host, format!("  host = {}", self.host.toml().to_string().trim()));
            }
            if !self.port.trim().is_empty() {
                // a port that is not a number is a string, so the check says what is wrong
                let v = self.port.trim().parse::<i64>().map_or_else(|_| q(self.port.trim()), |n| n.to_string());
                line(&mut text, Field::Port, format!("  port = {v}"));
            }
            if !self.path.text.is_empty() {
                line(&mut text, Field::Path, format!("  path = {}", self.path.toml().to_string().trim()));
            }
            if !self.query.is_empty() {
                if expand_query {
                    line(&mut text, Field::Query(0), "  [rule.match.query]".into());
                    for (i, (name, p)) in self.query.iter().enumerate() {
                        line(&mut text, Field::Query(i), format!("  {} = {}", key(name), p.toml().to_string().trim()));
                    }
                } else {
                    let parts: Vec<String> = self
                        .query
                        .iter()
                        .map(|(name, p)| format!("{} = {}", key(name), p.toml().to_string().trim()))
                        .collect();
                    line(&mut text, Field::Query(0), format!("  query = {{ {} }}", parts.join(", ")));
                }
            }
        }
        for (i, slot) in self.actions.iter().enumerate() {
            line(&mut text, Field::Rule, String::new());
            line(&mut text, Field::Action(i), "  [[rule.action]]".into());
            for (k, v) in slot.action.entries() {
                line(&mut text, Field::Action(i), format!("  {k} = {}", v.to_string().trim()));
            }
        }
        (text, fields)
    }

    /// The file's checks (PROTOCOL.md §8.1) on this rule, each problem with its field; `others`
    /// are the ids of the file's other rules; `base` is where body files are looked for.
    pub fn problems(&self, others: &[&str], base: &Path) -> Vec<(Field, String)> {
        let mut out = Vec::new();
        if self.id.trim().is_empty() {
            out.push((Field::Id, "a rule needs an id".to_string()));
        } else if others.contains(&self.id.as_str()) {
            out.push((Field::Id, "another rule has this id".to_string()));
        }
        if self.query.iter().any(|(n, _)| n.is_empty()) {
            let i = self.query.iter().position(|(n, _)| n.is_empty()).unwrap_or(0);
            out.push((Field::Query(i), "a query parameter needs a name".to_string()));
        }
        let (body, fields) = self.text(true);
        let text = format!("version = 1\n{body}");
        let parsed = rules::parse(&text, base);
        for p in parsed.problems {
            let field =
                p.line.and_then(|l| l.checked_sub(2)).and_then(|i| fields.get(i).copied()).unwrap_or(Field::Rule);
            if field == Field::Id && p.message.contains("another rule") {
                continue;
            }
            out.push((field, p.message));
        }
        out
    }

    /// `text` (the whole file) with this rule written as rule number `index`, or appended as a
    /// new rule (`None`). Only the rule's own values change: comments, blank lines and the order
    /// of keys stay, here and in the rest of the file.
    pub fn save_into(&self, text: &str, index: Option<usize>) -> Result<String, String> {
        let Some(index) = index else {
            let mut out = text.to_string();
            if out.trim().is_empty() {
                out = "# Rules for this app (docs/PROTOCOL.md §8); saved changes apply at once.\nversion = 1\n".into();
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
            out.push_str(&self.text(false).0);
            return Ok(out);
        };
        let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.message().to_string())?;
        let t = rule_tables_mut(&mut doc).into_iter().nth(index).ok_or("the rule is no longer in the file")?;
        set(t, "id", Some(Value::from(self.id.as_str())));
        set(t, "name", (!self.name.is_empty()).then(|| Value::from(self.name.as_str())));
        // enabled: written when it is there or when it is off; cache_rewrites likewise
        let has = t.contains_key("enabled");
        set(t, "enabled", (has || !self.enabled).then(|| Value::from(self.enabled)));
        let has = t.contains_key("cache_rewrites");
        set(t, "cache_rewrites", (has || self.cache_rewrites).then(|| Value::from(self.cache_rewrites)));
        self.save_match(t);
        self.save_actions(t);
        renumber(&mut doc);
        Ok(doc.to_string())
    }

    fn save_match(&self, t: &mut Table) {
        let methods = (!self.methods.is_empty()).then(|| {
            let mut a = Array::new();
            for m in &self.methods {
                a.push(m.as_str());
            }
            Value::Array(a)
        });
        let port = self.port.trim();
        let port = (!port.is_empty()).then(|| port.parse::<i64>().map_or_else(|_| Value::from(port), Value::from));
        let query = (!self.query.is_empty()).then(|| {
            let mut q = InlineTable::new();
            for (name, p) in &self.query {
                q.insert(name, p.toml());
            }
            Value::InlineTable(q)
        });
        let values: [(&str, Option<Value>); 6] = [
            ("methods", methods),
            ("scheme", (!self.scheme.is_empty()).then(|| Value::from(self.scheme.as_str()))),
            ("host", (!self.host.text.is_empty()).then(|| self.host.toml())),
            ("port", port),
            ("path", (!self.path.text.is_empty()).then(|| self.path.toml())),
            ("query", query),
        ];
        if values.iter().all(|(_, v)| v.is_none()) {
            t.remove("match");
            return;
        }
        if !t.get("match").is_some_and(|m| m.as_table_like().is_some()) {
            let mut m = Table::new();
            m.set_implicit(false);
            t.insert("match", Item::Table(m));
        }
        let m = t.get_mut("match").and_then(Item::as_table_like_mut).expect("inserted above");
        for (k, v) in values {
            set_like(m, k, v);
        }
    }

    fn save_actions(&self, t: &mut Table) {
        let old: Vec<Table> = match t.remove("action") {
            Some(Item::ArrayOfTables(a)) => a.into_iter().collect(),
            Some(Item::Value(Value::Array(a))) => {
                a.iter().map(|v| v.as_inline_table().map(|i| i.clone().into_table()).unwrap_or_default()).collect()
            }
            _ => Vec::new(),
        };
        if self.actions.is_empty() {
            return;
        }
        let mut out = ArrayOfTables::new();
        for slot in &self.actions {
            let mut table = slot.origin.and_then(|i| old.get(i).cloned()).unwrap_or_default();
            let mut entries = slot.action.entries();
            for (k, v) in slot.action.stated_defaults() {
                if table.contains_key(k) {
                    entries.push((k, v));
                }
            }
            // keys of another type go (a changed type), keys of this one are set in place
            let keep: Vec<&str> = entries.iter().map(|(k, _)| *k).collect();
            let gone: Vec<String> =
                table.iter().map(|(k, _)| k.to_string()).filter(|k| !keep.contains(&k.as_str())).collect();
            for k in gone {
                table.remove(&k);
            }
            for (k, v) in entries {
                set(&mut table, k, Some(v));
            }
            out.push(table);
        }
        t.insert("action", Item::ArrayOfTables(out));
    }
}

/// Moves rule number `from` to position `to` (both in file order): the rule's whole text goes
/// with it, comments included.
pub fn move_rule(text: &str, from: usize, to: usize) -> Result<String, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.message().to_string())?;
    let Some(Item::ArrayOfTables(rules)) = doc.get_mut("rule") else { return Err("no rules in the file".into()) };
    if from >= rules.len() || to >= rules.len() {
        return Err("no such rule".into());
    }
    let mut all: Vec<Table> = std::mem::take(rules).into_iter().collect();
    let moved = all.remove(from);
    all.insert(to, moved);
    let mut out = ArrayOfTables::new();
    for t in all {
        out.push(t);
    }
    *rules = out;
    renumber(&mut doc);
    Ok(doc.to_string())
}

/// Numbers every table in the order the document holds them (rules in list order, each rule's
/// tables after it): `toml_edit` prints tables by the positions they were read at, so moved or
/// new ones would otherwise print where the old ones were.
fn renumber(doc: &mut DocumentMut) {
    fn walk(t: &mut Table, next: &mut isize) {
        let keys: Vec<String> = t.iter().map(|(k, _)| k.to_string()).collect();
        for k in keys {
            match t.get_mut(&k) {
                Some(Item::Table(s)) => {
                    *next += 1;
                    s.set_position(Some(*next));
                    walk(s, next);
                }
                Some(Item::ArrayOfTables(a)) => {
                    for s in a.iter_mut() {
                        *next += 1;
                        s.set_position(Some(*next));
                        walk(s, next);
                    }
                }
                _ => {}
            }
        }
    }
    let mut next = 0;
    walk(doc.as_table_mut(), &mut next);
}

fn rule_tables(doc: &DocumentMut) -> Vec<&Table> {
    match doc.get("rule") {
        Some(Item::ArrayOfTables(a)) => a.iter().collect(),
        _ => Vec::new(),
    }
}

fn rule_tables_mut(doc: &mut DocumentMut) -> Vec<&mut Table> {
    match doc.get_mut("rule") {
        Some(Item::ArrayOfTables(a)) => a.iter_mut().collect(),
        _ => Vec::new(),
    }
}

fn action_tables(t: &Table) -> Vec<&dyn TableLike> {
    match t.get("action") {
        Some(Item::ArrayOfTables(a)) => a.iter().map(|t| t as &dyn TableLike).collect(),
        Some(Item::Value(Value::Array(a))) => {
            a.iter().filter_map(Value::as_inline_table).map(|t| t as &dyn TableLike).collect()
        }
        _ => Vec::new(),
    }
}

/// Sets `key` to `value` (or removes it): a value that is there keeps its place and the comment
/// after it; a new key is indented like its neighbours.
fn set(t: &mut Table, key: &str, value: Option<Value>) {
    set_like(t, key, value);
}

fn set_like(t: &mut dyn TableLike, key: &str, value: Option<Value>) {
    let Some(mut value) = value else {
        t.remove(key);
        return;
    };
    if let Some(old) = t.get_mut(key).and_then(Item::as_value_mut) {
        let same = old.to_string().trim() == value.to_string().trim();
        if !same {
            let decor = old.decor().clone();
            *value.decor_mut() = decor;
            *old = value;
        }
        return;
    }
    let indent = t
        .iter()
        .next()
        .and_then(|(k, _)| t.key(k))
        .and_then(|k| k.leaf_decor().prefix())
        .and_then(|p| p.as_str())
        .map(str::to_string);
    t.insert(key, Item::Value(value));
    if let Some(indent) = indent
        && let Some(mut k) = t.key_mut(key)
    {
        k.leaf_decor_mut().set_prefix(indent);
    }
}

fn key(name: &str) -> String {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        name.to_string()
    } else {
        Value::from(name).to_string().trim().to_string()
    }
}

// --- trying a match on captured requests ---------------------------------------------------

/// Whether a captured request is one the app would apply the match to (PROTOCOL.md §8.2): the
/// device's comparisons (hosts without case, `*` in a host glob within a label and in a path glob
/// within a segment, regexes found anywhere in the value). `Err` when a regex uses what only the
/// device's regex engine knows (look-around, back-references).
pub fn matches_request(m: &RuleMatch, t: &Transaction) -> Result<bool, String> {
    if !m.methods.is_empty() && !m.methods.iter().any(|x| x.eq_ignore_ascii_case(&t.method)) {
        return Ok(false);
    }
    if let Some(s) = &m.scheme
        && !s.eq_ignore_ascii_case(&t.url.scheme)
    {
        return Ok(false);
    }
    if let Some(h) = &m.host
        && !pattern_matches(h, &t.url.host, Some('.'), true)?
    {
        return Ok(false);
    }
    if let Some(p) = m.port
        && t.url.effective_port() != Some(p)
    {
        return Ok(false);
    }
    if let Some(p) = &m.path
        && !pattern_matches(p, &t.url.path, Some('/'), false)?
    {
        return Ok(false);
    }
    if !m.query.is_empty() {
        let pairs = t.url.query_pairs();
        for q in &m.query {
            let values: Vec<&str> = pairs.iter().filter(|(n, _)| *n == q.name).map(|(_, v)| v.as_str()).collect();
            if values.is_empty() {
                return Ok(false);
            }
            if let Some(p) = &q.value {
                let mut any = false;
                for v in values {
                    any |= pattern_matches(p, v, None, false)?;
                }
                if !any {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

fn pattern_matches(p: &Pattern, value: &str, separator: Option<char>, ignore_case: bool) -> Result<bool, String> {
    match p {
        Pattern::Exact(e) => Ok(if ignore_case { e.eq_ignore_ascii_case(value) } else { e == value }),
        Pattern::Glob(g) => Ok(glob_regex(g, separator, ignore_case).is_match(value)),
        Pattern::Regex(r) => {
            let re = regex::RegexBuilder::new(r)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|_| format!("the regex {r:?} is tried on the device only"))?;
            Ok(re.is_match(value))
        }
    }
}

/// The device's glob (RuleSet.globPattern): anchored; `*` any run without the separator, `**`
/// any run, `?` one character other than the separator.
fn glob_regex(glob: &str, separator: Option<char>, ignore_case: bool) -> regex::Regex {
    let not_sep = separator.map_or_else(|| ".".to_string(), |s| format!("[^{}]", regex::escape(&s.to_string())));
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                re.push_str(".*");
            }
            '*' => {
                re.push_str(&not_sep);
                re.push('*');
            }
            '?' => re.push_str(&not_sep),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    regex::RegexBuilder::new(&re)
        .case_insensitive(ignore_case)
        .dot_matches_new_line(true)
        .build()
        .unwrap_or_else(|_| regex::Regex::new("$^").expect("a regex that matches nothing"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Url;

    const FILE: &str = r#"version = 1

# slow down the status poll
[[rule]]
id = "slow-status"
name = "Slow status poll"   # what the team calls it
enabled = true

  [rule.match]
  methods = ["GET"]
  host = "*.example.com"
  path = "/api/v1/*/status/**"
  query = { orderId = "*" }

  # three seconds, like a bad network
  [[rule.action]]
  type = "delay"
  ms = 3000

[[rule]]
id = "force-paid"

  [rule.match]
  path = { exact = "/api/v1/orders/status" }

  [[rule.action]]
  type = "replace"
  find = '"payment":"pending"'
  with = '"payment":"captured"'

  [[rule.action]]
  type = "header"
  op = "set"
  name = "Cache-Control"
  value = "no-store"
"#;

    fn txn(method: &str, url: &str) -> Transaction {
        let mut t = Transaction::new_placeholder(crate::model::TxnKey { source: 0, txn: 1 }, 0);
        t.method = method.into();
        t.url = Url::parse(url);
        t
    }

    #[test]
    fn a_rule_reads_into_the_form_and_back_unchanged() {
        let d = RuleDraft::read(FILE, 0).unwrap();
        assert_eq!(d.id, "slow-status");
        assert_eq!(d.name, "Slow status poll");
        assert_eq!(d.methods, ["GET"]);
        assert_eq!(d.host, PatternDraft { kind: PatternKind::Glob, text: "*.example.com".into() });
        assert_eq!(d.query, [("orderId".to_string(), PatternDraft { kind: PatternKind::Glob, text: "*".into() })]);
        assert_eq!(d.actions[0].action, ActionDraft::Delay { ms: "3000".into() });
        // saving what was read changes nothing at all
        assert_eq!(d.save_into(FILE, Some(0)).unwrap(), FILE);
        let d1 = RuleDraft::read(FILE, 1).unwrap();
        assert_eq!(d1.save_into(FILE, Some(1)).unwrap(), FILE);
    }

    /// Every rule in the documented examples comes back byte for byte when saved unchanged.
    #[test]
    fn the_documented_rules_save_back_unchanged() {
        let example = |doc: &str, heading: &str| -> String {
            let doc = doc.replace("\r\n", "\n");
            let section = &doc[doc.find(heading).expect(heading)..];
            let start = section.find("```toml\n").expect("an example") + "```toml\n".len();
            let end = start + section[start..].find("```").expect("its end");
            section[start..end].to_string()
        };
        for text in [
            FILE.to_string(),
            example(include_str!("../../../../README.md"), "\n## Rules\n"),
            example(include_str!("../../../../docs/PROTOCOL.md"), "### 8.1 File format"),
        ] {
            let n = rules::parse(&text, Path::new(".")).entries.len();
            assert!(n >= 2);
            for i in 0..n {
                let d = RuleDraft::read(&text, i).unwrap();
                assert_eq!(d.save_into(&text, Some(i)).unwrap(), text, "rule {i} ({})", d.id);
            }
        }
    }

    #[test]
    fn an_edit_keeps_the_files_comments_and_layout() {
        let mut d = RuleDraft::read(FILE, 0).unwrap();
        d.name = "Slower status poll".into();
        d.port = "8443".into();
        d.actions[0].action = ActionDraft::Delay { ms: "5000".into() };
        d.actions.push(ActionSlot { action: ActionDraft::new("status"), origin: None });
        let saved = d.save_into(FILE, Some(0)).unwrap();
        assert!(saved.contains("# slow down the status poll\n[[rule]]"), "{saved}");
        assert!(saved.contains("name = \"Slower status poll\"   # what the team calls it"), "{saved}");
        assert!(saved.contains("  # three seconds, like a bad network\n  [[rule.action]]"), "{saved}");
        assert!(saved.contains("ms = 5000"), "{saved}");
        assert!(saved.contains("  port = 8443"), "a new key is indented like its neighbours: {saved}");
        // the other rule is untouched, and the saved rule reads back as the form had it
        let second = &FILE[FILE.find("[[rule]]\nid = \"force-paid\"").unwrap()..];
        assert!(saved.ends_with(second), "{saved}");
        let back = RuleDraft::read(&saved, 0).unwrap();
        assert_eq!(back.name, d.name);
        assert_eq!(back.port, "8443");
        assert_eq!(
            back.actions.iter().map(|a| &a.action).collect::<Vec<_>>(),
            d.actions.iter().map(|a| &a.action).collect::<Vec<_>>()
        );
        assert!(rules::parse(&saved, Path::new(".")).is_valid());
    }

    #[test]
    fn actions_move_with_their_comments() {
        let text = "version = 1\n[[rule]]\nid = \"r\"\n\n  # first\n  [[rule.action]]\n  type = \"delay\"\n  ms = 1\n\n  # second\n  [[rule.action]]\n  type = \"status\"\n  code = 500\n";
        let mut d = RuleDraft::read(text, 0).unwrap();
        d.actions.swap(0, 1);
        let saved = d.save_into(text, Some(0)).unwrap();
        let second = saved.find("# second").unwrap();
        let first = saved.find("# first").unwrap();
        assert!(second < first, "{saved}");
        assert!(saved[second..first].contains("code = 500"), "{saved}");
        assert_eq!(
            RuleDraft::read(&saved, 0).unwrap().actions[0].action,
            ActionDraft::Status { code: "500".into(), reason: String::new() }
        );
    }

    #[test]
    fn a_new_rule_is_appended_in_the_files_layout() {
        let mut d = RuleDraft::read(FILE, 1).unwrap();
        d.id = "copy".into();
        d.query = vec![("id".into(), PatternDraft { kind: PatternKind::Regex, text: "^[0-9]+$".into() })];
        let saved = d.save_into(FILE, None).unwrap();
        assert!(saved.starts_with(FILE));
        let f = rules::parse(&saved, Path::new("."));
        assert!(f.is_valid(), "{:?}\n{saved}", f.problems);
        let back = RuleDraft::read(&saved, 2).unwrap();
        assert_eq!(back.query, d.query);
        assert_eq!(
            back.actions.iter().map(|a| &a.action).collect::<Vec<_>>(),
            d.actions.iter().map(|a| &a.action).collect::<Vec<_>>()
        );
    }

    #[test]
    fn problems_belong_to_their_fields() {
        let mut d = RuleDraft::read(FILE, 0).unwrap();
        d.port = "70000".into();
        d.scheme = "ftp".into();
        d.path = PatternDraft { kind: PatternKind::Regex, text: "(".into() };
        d.actions.push(ActionSlot {
            action: ActionDraft::Status { code: "999".into(), reason: String::new() },
            origin: None,
        });
        let p = d.problems(&["force-paid"], Path::new("."));
        let fields: Vec<Field> = p.iter().map(|(f, _)| *f).collect();
        assert!(fields.contains(&Field::Port), "{p:?}");
        assert!(fields.contains(&Field::Scheme), "{p:?}");
        assert!(fields.contains(&Field::Path), "{p:?}");
        assert!(fields.contains(&Field::Action(1)), "{p:?}");
        d.id = "force-paid".into();
        assert!(
            d.problems(&["force-paid"], Path::new("."))
                .iter()
                .any(|(f, m)| *f == Field::Id && m.contains("another rule"))
        );
        assert!(RuleDraft::read(FILE, 0).unwrap().problems(&["force-paid"], Path::new(".")).is_empty());
    }

    #[test]
    fn rules_move_whole() {
        let moved = move_rule(FILE, 1, 0).unwrap();
        let f = rules::parse(&moved, Path::new("."));
        assert!(f.is_valid(), "{:?}\n{moved}", f.problems);
        assert_eq!(f.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["force-paid", "slow-status"]);
        // each rule keeps its comments and actions
        assert_eq!(RuleDraft::read(&moved, 1).unwrap(), RuleDraft::read(FILE, 0).unwrap());
        assert!(moved.contains("# three seconds, like a bad network"));
        assert_eq!(move_rule(&moved, 0, 1).unwrap(), FILE);
    }

    #[test]
    fn matching_is_the_devices() {
        let m = |d: &RuleDraft, method: &str, url: &str| {
            matches_request(&d.wire_match().unwrap(), &txn(method, url)).unwrap()
        };
        let slow = RuleDraft::read(FILE, 0).unwrap();
        assert!(m(&slow, "GET", "https://api.example.com/api/v1/orders/status/7?orderId=1"));
        assert!(
            m(&slow, "get", "https://API.Example.com/api/v1/orders/status/7/x?orderId=1"),
            "method and host without case"
        );
        assert!(!m(&slow, "GET", "https://a.b.example.com/api/v1/orders/status/7?orderId=1"), "* stays in one label");
        assert!(!m(&slow, "GET", "https://api.example.com/api/v1/a/b/status/7?orderId=1"), "* stays in one segment");
        assert!(
            !m(&slow, "GET", "https://api.example.com/api/v1/orders/status/7"),
            "the query parameter must be there"
        );
        assert!(!m(&slow, "POST", "https://api.example.com/api/v1/orders/status/7?orderId=1"));
        let mut d = RuleDraft { port: "443".into(), ..RuleDraft::default() };
        assert!(m(&d, "GET", "https://x.example/"), "the default port counts");
        d.port = "8080".into();
        assert!(!m(&d, "GET", "https://x.example/"));
        d.port.clear();
        d.path = PatternDraft { kind: PatternKind::Regex, text: "status".into() };
        assert!(m(&d, "GET", "http://x/a/status/b"), "a regex is found anywhere");
        d.path = PatternDraft { kind: PatternKind::Regex, text: "(?<=a)b".into() };
        assert!(matches_request(&d.wire_match().unwrap(), &txn("GET", "http://x/ab")).is_err());
    }
}
