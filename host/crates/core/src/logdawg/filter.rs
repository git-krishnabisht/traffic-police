//! Logdawg's filter bar: the query language of Android Studio's Logcat (ARCHITECTURE.md §5.17),
//! as Studio's lexer, parser and filters define it (JetBrains/android, `logcat/…/filters/`).
//!
//! | Term | Meaning |
//! |---|---|
//! | `package:mine` | the session's app: its uid's lines (all its processes, across restarts) |
//! | `package:shop`, `process:shop` | a process whose name contains the text |
//! | `tag:OkHttp` | a tag that contains the text; `tag=:OkHttp` exactly; `tag~:Ok.*p` a regex |
//! | `message:timeout`, `msg:` | a message that contains the text (also `=:` and `~:`) |
//! | `timeout`, `"two words"`, `line:timeout` | the tag, the message or the process's name |
//! | `/regex/`, `/regex/i` | the same, by a regular expression |
//! | `level:warn` | warnings and above (`verbose` … `assert`, or `v` … `a`) |
//! | `is:crash`, `is:stacktrace`, `is:error` | a crash, a Java stack trace, exactly that level |
//! | `age:30s`, `age:5m`, `age:2h`, `age:1d` | newer than that |
//! | `pid:4312`, `tid:4331`, `uid:10234` | a process, thread or user id |
//! | `name:…` | the filter's name (Studio's saved filters); every line passes it |
//!
//! `-` in front of a term (or of parentheses) rules it out; `a | b` is either, `a & b` both, `&`
//! before `|`, and parentheses group. Without operators, as in Studio, terms with the same key
//! are alternatives (`tag:OkHttp tag:Retrofit`: either one) and everything else must hold, each
//! word without a key too. Text is matched without regard to case; quotes (`"…"` or `'…'`) or
//! `\ ` keep spaces in a value, and a space may follow the colon (`tag: OkHttp`).

use std::collections::HashSet;
use std::ops::Range;
use std::sync::LazyLock;

use regex::{Regex, RegexBuilder};

use super::{BUFFER_CRASH, Level, Line, LogStore};
use crate::filter::ParseError;
use crate::fmt::{NS_PER_MS, NS_PER_SEC, Ts};

/// A parsed filter.
#[derive(Debug, Clone)]
pub struct LogFilter {
    /// The text it was parsed from.
    pub source: String,
    root: Node,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Package,
    Tag,
    Message,
    Line,
    Level,
    Pid,
    Tid,
    Uid,
    Is,
    Age,
    Name,
}

fn key_named(name: &str) -> Option<Key> {
    Some(match name.to_ascii_lowercase().as_str() {
        "package" | "process" | "app" => Key::Package,
        "tag" => Key::Tag,
        "message" | "msg" => Key::Message,
        "line" => Key::Line,
        "level" => Key::Level,
        "pid" => Key::Pid,
        "tid" => Key::Tid,
        "uid" => Key::Uid,
        "is" => Key::Is,
        "age" => Key::Age,
        "name" => Key::Name,
        _ => return None,
    })
}

#[derive(Debug, Clone)]
enum Node {
    /// A term; `group` is its key when terms of that key beside it are alternatives.
    Test {
        group: Option<Key>,
        test: Test,
    },
    Not(Box<Node>),
    All(Vec<Node>),
    Any(Vec<Node>),
}

#[derive(Debug, Clone)]
enum Text {
    /// Case-insensitive literal (compiled), or a user regex.
    Find(Regex),
    Exact(String),
}

impl Text {
    fn test(&self, s: &str) -> bool {
        match self {
            Text::Find(r) => r.is_match(s),
            Text::Exact(e) => s.eq_ignore_ascii_case(e),
        }
    }
}

#[derive(Debug, Clone)]
enum Test {
    Mine,
    Process(Text),
    Tag(Text),
    Message(Text),
    /// The tag, the message or the process's name (Studio's whole line).
    Line(Text),
    AtLeast(Level),
    Exactly(Level),
    Pid(u32),
    Tid(u32),
    Uid(u32),
    Crash,
    StackTrace,
    Newer(u64),
    Always,
}

/// What a line is matched against besides itself.
pub struct Context<'a> {
    /// The app's pids from the network side (its capture runtimes), for when the uid is not
    /// known.
    pub app_pids: &'a HashSet<u32>,
    /// The device's time now (`age:`).
    pub now: Ts,
}

fn literal(s: &str) -> Regex {
    RegexBuilder::new(&regex::escape(s)).case_insensitive(true).build().expect("an escaped literal compiles")
}

/// `30s`, `5m`, `2h`, `1d`, `500ms`.
fn parse_age(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok().filter(|n: &f64| *n >= 0.0)?;
    let mult = match unit {
        "ms" => NS_PER_MS as f64,
        "s" | "" => NS_PER_SEC as f64,
        "m" | "min" => 60.0 * NS_PER_SEC as f64,
        "h" => 3600.0 * NS_PER_SEC as f64,
        "d" => 86_400.0 * NS_PER_SEC as f64,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// A frame of a Java stack trace (Studio's pattern, which also takes one on the last line).
static FRAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n\s*at .+\(.+\)(\n|$)").expect("the frame pattern compiles"));

// --- reading the text ------------------------------------------------------------------------

#[derive(Debug)]
enum Lex {
    Open,
    /// `-(`: the group is ruled out.
    NotOpen,
    Close,
    Or,
    And,
    Term(Term),
}

#[derive(Debug)]
struct Term {
    negate: bool,
    /// The key and how its value is matched: `:` contains, `=` exactly, `~` a regex.
    key: Option<(Key, char)>,
    value: String,
    quoted: bool,
}

fn error(span: Range<usize>, message: impl Into<String>) -> ParseError {
    ParseError { span, message: message.into() }
}

/// A value from `at`: quoted (`"…"`, `'…'`, a backslash keeps the quote inside), or up to a space
/// or a `)` that closes no `(` of its own, `\ ` keeping a space. Returns the value, where it ends
/// and whether it was quoted.
fn read_value(input: &str, at: usize) -> Result<(String, usize, bool), ParseError> {
    let mut chars = input[at..].char_indices().map(|(k, c)| (at + k, c)).peekable();
    let mut out = String::new();
    match chars.peek() {
        Some(&(_, q)) if q == '"' || q == '\'' => {
            chars.next();
            while let Some((k, c)) = chars.next() {
                if c == '\\' && chars.peek().is_some_and(|&(_, d)| d == q) {
                    out.push(q);
                    chars.next();
                } else if c == q {
                    return Ok((out, k + 1, true));
                } else {
                    out.push(c);
                }
            }
            Err(error(at..input.len(), format!("the quote {q} is not closed")))
        }
        _ => {
            let mut depth = 0u32;
            while let Some(&(k, c)) = chars.peek() {
                if c.is_whitespace() || (c == ')' && depth == 0) {
                    return Ok((out, k, false));
                }
                chars.next();
                match c {
                    '\\' => match chars.next() {
                        Some((_, d)) if d.is_whitespace() => out.push(d),
                        Some((_, d)) => {
                            out.push('\\');
                            out.push(d);
                        }
                        None => out.push('\\'),
                    },
                    '(' => {
                        depth += 1;
                        out.push(c);
                    }
                    ')' => {
                        depth -= 1;
                        out.push(c);
                    }
                    _ => out.push(c),
                }
            }
            Ok((out, input.len(), false))
        }
    }
}

fn lex(input: &str) -> Result<Vec<(Lex, Range<usize>)>, ParseError> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(c) = input[i..].chars().next() {
        let start = i;
        let one = |lex| (lex, start..start + 1);
        match c {
            c if c.is_whitespace() => {
                i += c.len_utf8();
                continue;
            }
            '(' => out.push(one(Lex::Open)),
            ')' => out.push(one(Lex::Close)),
            '|' => out.push(one(Lex::Or)),
            '&' => out.push(one(Lex::And)),
            _ => {}
        }
        if matches!(c, '(' | ')' | '|' | '&') {
            i += 1;
            continue;
        }
        let next = input[i + c.len_utf8()..].chars().next();
        let negate = c == '-' && next.is_some_and(|d| !d.is_whitespace() && d != ')');
        let j = if negate { i + 1 } else { i };
        if negate && next == Some('(') {
            out.push((Lex::NotOpen, start..j + 1));
            i = j + 1;
            continue;
        }
        // a key: its letters, then `:`, `=:` or `~:`
        let letters = input[j..].find(|ch: char| !ch.is_ascii_alphabetic()).map_or(input.len(), |n| j + n);
        let how = [(":", ':'), ("=:", '='), ("~:", '~')].into_iter().find(|(s, _)| input[letters..].starts_with(s));
        if let Some((op, how)) = how
            && let Some(key) = key_named(&input[j..letters])
        {
            let key_end = letters + op.len();
            let at = key_end + input[key_end..].find(|ch: char| !ch.is_whitespace()).unwrap_or(input.len() - key_end);
            let needs = || error(start..key_end, format!("{} needs a value", &input[j..key_end]));
            if input[at..].chars().next().is_none_or(|ch| matches!(ch, ')' | '|' | '&')) {
                return Err(needs());
            }
            let (value, end, quoted) = read_value(input, at)?;
            if value.is_empty() && !quoted {
                return Err(needs());
            }
            out.push((Lex::Term(Term { negate, key: Some((key, how)), value, quoted }), start..end));
            i = end;
            continue;
        }
        let (value, end, quoted) = read_value(input, j)?;
        out.push((Lex::Term(Term { negate, key: None, value, quoted }), start..end));
        i = end.max(j + 1);
    }
    Ok(out)
}

/// A term's test, and the key it groups by.
fn term(t: Term, span: Range<usize>) -> Result<Node, ParseError> {
    let err = |message: String| error(span.clone(), message);
    let regex = |pattern: &str, insensitive: bool| {
        RegexBuilder::new(pattern).case_insensitive(insensitive).build().map_err(|e| err(format!("not a regex: {e}")))
    };
    let (group, test) = match t.key {
        // a word, quoted text, or a /regex/: the whole line
        None => {
            let v = &t.value;
            let slashed = |suffix: &str| {
                (!t.quoted && v.len() > suffix.len() + 1 && v.starts_with('/') && v.ends_with(suffix))
                    .then(|| &v[1..v.len() - suffix.len()])
            };
            let text = match (slashed("/i"), slashed("/")) {
                (Some(p), _) => Text::Find(regex(p, true)?),
                (None, Some(p)) => Text::Find(regex(p, false)?),
                _ => Text::Find(literal(v)),
            };
            (None, Test::Line(text))
        }
        Some((key, how)) => {
            let value = t.value.as_str();
            let text = || -> Result<Text, ParseError> {
                Ok(match how {
                    '=' => Text::Exact(value.to_string()),
                    '~' => Text::Find(regex(value, true)?),
                    _ => Text::Find(literal(value)),
                })
            };
            let number = || value.trim().parse::<u32>().map_err(|_| err(format!("{value:?} is not a number")));
            let level = || {
                Level::parse(value).ok_or_else(|| {
                    err(format!("{value:?} is not a level: verbose, debug, info, warn, error or assert"))
                })
            };
            let test = match key {
                Key::Package if value.eq_ignore_ascii_case("mine") && how == ':' => Test::Mine,
                Key::Package => Test::Process(text()?),
                Key::Tag => Test::Tag(text()?),
                Key::Message => Test::Message(text()?),
                Key::Line => Test::Line(text()?),
                Key::Level => Test::AtLeast(level()?),
                Key::Pid => Test::Pid(number()?),
                Key::Tid => Test::Tid(number()?),
                Key::Uid => Test::Uid(number()?),
                Key::Is => match value.to_ascii_lowercase().as_str() {
                    "crash" => Test::Crash,
                    "stacktrace" => Test::StackTrace,
                    _ => Test::Exactly(Level::parse(value).ok_or_else(|| {
                        err(format!("is:{value} means nothing here: is:crash, is:stacktrace or a level (is:error)"))
                    })?),
                },
                Key::Age => Test::Newer(
                    parse_age(value).ok_or_else(|| err(format!("{value:?} is not an age (like 30s, 5m, 2h, 1d)")))?,
                ),
                Key::Name => Test::Always,
            };
            (Some(key).filter(|k| *k != Key::Name), test)
        }
    };
    let node = Node::Test { group: if t.negate { None } else { group }, test };
    Ok(if t.negate { Node::Not(Box::new(node)) } else { node })
}

/// Terms side by side: those of one key are alternatives, the rest must all hold (Studio's
/// implicit grouping).
fn side_by_side(items: Vec<Node>) -> Node {
    let mut groups: Vec<(Key, Vec<Node>)> = Vec::new();
    let mut all = Vec::new();
    for n in items {
        match n {
            Node::Test { group: Some(key), .. } => match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, g)) => g.push(n),
                None => groups.push((key, vec![n])),
            },
            n => all.push(n),
        }
    }
    let mut out: Vec<Node> =
        groups.into_iter().map(|(_, mut g)| if g.len() == 1 { g.remove(0) } else { Node::Any(g) }).collect();
    out.extend(all);
    if out.len() == 1 { out.remove(0) } else { Node::All(out) }
}

struct Parser {
    lexed: std::iter::Peekable<std::vec::IntoIter<(Lex, Range<usize>)>>,
    end: usize,
}

impl Parser {
    /// Terms side by side, up to the end or (`open`: inside parentheses) the `)`.
    fn sequence(&mut self, open: Option<Range<usize>>) -> Result<Vec<Node>, ParseError> {
        let mut items = Vec::new();
        loop {
            match self.lexed.peek() {
                None => match open {
                    Some(span) => return Err(error(span, "this ( is not closed")),
                    None => return Ok(items),
                },
                Some((Lex::Close, span)) => {
                    let span = span.clone();
                    self.lexed.next();
                    return match open {
                        Some(_) => Ok(items),
                        None => Err(error(span, "this ) closes no (")),
                    };
                }
                _ => items.push(self.either()?),
            }
        }
    }

    /// `a | b | …`
    fn either(&mut self) -> Result<Node, ParseError> {
        let mut parts = vec![self.both()?];
        while let Some((Lex::Or, span)) = self.lexed.peek() {
            let span = span.clone();
            self.lexed.next();
            if !self.term_next() {
                return Err(error(span, "| needs a term after it"));
            }
            parts.push(self.both()?);
        }
        Ok(if parts.len() == 1 { parts.remove(0) } else { Node::Any(parts) })
    }

    /// `a & b & …`
    fn both(&mut self) -> Result<Node, ParseError> {
        let mut parts = vec![self.single()?];
        while let Some((Lex::And, span)) = self.lexed.peek() {
            let span = span.clone();
            self.lexed.next();
            if !self.term_next() {
                return Err(error(span, "& needs a term after it"));
            }
            parts.push(self.single()?);
        }
        Ok(if parts.len() == 1 { parts.remove(0) } else { Node::All(parts) })
    }

    fn term_next(&mut self) -> bool {
        matches!(self.lexed.peek(), Some((Lex::Term(_) | Lex::Open | Lex::NotOpen, _)))
    }

    /// A term, or a group in parentheses.
    fn single(&mut self) -> Result<Node, ParseError> {
        let Some((lex, span)) = self.lexed.next() else {
            return Err(error(self.end..self.end, "a term is missing"));
        };
        match lex {
            Lex::Term(t) => term(t, span),
            Lex::Open | Lex::NotOpen => {
                let items = self.sequence(Some(span.clone()))?;
                if items.is_empty() {
                    return Err(error(span, "nothing between ( and )"));
                }
                let group = side_by_side(items);
                Ok(if matches!(lex, Lex::NotOpen) { Node::Not(Box::new(group)) } else { group })
            }
            Lex::Close => Err(error(span, "this ) closes no (")),
            Lex::Or => Err(error(span, "| needs a term before it")),
            Lex::And => Err(error(span, "& needs a term before it")),
        }
    }
}

impl LogFilter {
    /// `None` for an empty filter (every line).
    pub fn parse(input: &str) -> Result<Option<LogFilter>, ParseError> {
        let mut p = Parser { lexed: lex(input)?.into_iter().peekable(), end: input.len() };
        let items = p.sequence(None)?;
        if items.is_empty() {
            return Ok(None);
        }
        Ok(Some(LogFilter { source: input.trim().to_string(), root: side_by_side(items) }))
    }

    /// Whether `age:` makes the result change with time.
    pub fn uses_clock(&self) -> bool {
        fn clock(n: &Node) -> bool {
            match n {
                Node::Test { test, .. } => matches!(test, Test::Newer(_)),
                Node::Not(n) => clock(n),
                Node::All(v) | Node::Any(v) => v.iter().any(clock),
            }
        }
        clock(&self.root)
    }

    pub fn matches(&self, line: &Line<'_>, store: &LogStore, cx: &Context<'_>) -> bool {
        fn eval(n: &Node, line: &Line<'_>, store: &LogStore, cx: &Context<'_>) -> bool {
            match n {
                Node::Test { test: t, .. } => test(t, line, store, cx),
                Node::Not(n) => !eval(n, line, store, cx),
                Node::All(v) => v.iter().all(|n| eval(n, line, store, cx)),
                Node::Any(v) => v.iter().any(|n| eval(n, line, store, cx)),
            }
        }
        eval(&self.root, line, store, cx)
    }
}

/// The session's app: its uid's lines, else the pids of its capture runtimes, else processes
/// named after its package; everything when nothing says which app it is.
pub fn is_mine(line: &Line<'_>, store: &LogStore, app_pids: &HashSet<u32>) -> bool {
    if let Some(uid) = store.uid() {
        return line.uid == Some(uid) || app_pids.contains(&line.pid);
    }
    if !app_pids.is_empty() {
        return app_pids.contains(&line.pid);
    }
    match store.package() {
        Some(p) => {
            store.process(line.pid).is_some_and(|n| n == p || n.strip_prefix(p).is_some_and(|r| r.starts_with(':')))
        }
        None => true,
    }
}

fn test(t: &Test, line: &Line<'_>, store: &LogStore, cx: &Context<'_>) -> bool {
    match t {
        Test::Mine => is_mine(line, store, cx.app_pids),
        Test::Process(x) => store.process_of(line.pid, line.uid).is_some_and(|n| x.test(n)),
        Test::Tag(x) => x.test(line.tag),
        Test::Message(x) => x.test(line.message),
        Test::Line(x) => {
            x.test(line.tag) || x.test(line.message) || store.process_of(line.pid, line.uid).is_some_and(|n| x.test(n))
        }
        Test::AtLeast(l) => line.level >= *l,
        Test::Exactly(l) => line.level == *l,
        Test::Pid(p) => line.pid == *p,
        Test::Tid(p) => line.tid == *p,
        Test::Uid(u) => line.uid == Some(*u),
        // the crash buffer, and what Studio takes for a crash: a Java one, a native one
        Test::Crash => {
            line.buffer == BUFFER_CRASH
                || (line.level == Level::Error
                    && line.tag == "AndroidRuntime"
                    && line.message.starts_with("FATAL EXCEPTION"))
                || (line.level == Level::Assert && (line.tag == "DEBUG" || line.tag == "libc"))
        }
        Test::StackTrace => FRAME.is_match(line.message),
        Test::Newer(age) => line.ts.saturating_add(*age) >= cx.now,
        Test::Always => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logdawg::{LogInfo, LogLine};

    fn store() -> LogStore {
        let mut s = LogStore::new();
        s.apply_info(LogInfo {
            package: Some("com.example.shop".into()),
            uid: Some(10_234),
            processes: vec![(4312, "com.example.shop".into()), (612, "system_server".into())],
            ..LogInfo::default()
        });
        let l = |ts: Ts, pid: u32, uid: u32, level: Level, buffer: u8, tag: &str, msg: &str| LogLine {
            ts,
            wall_ms: 0,
            pid,
            tid: pid + 1,
            uid: Some(uid),
            level,
            buffer,
            tag: tag.into(),
            message: msg.into(),
        };
        s.push(l(NS_PER_SEC, 4312, 10_234, Level::Debug, 0, "OkHttp", "--> GET http://localhost:8080/api/v1/products"));
        s.push(l(2 * NS_PER_SEC, 4312, 10_234, Level::Warn, 0, "CheckoutViewModel", "checkout failed: HTTP 500"));
        s.push(l(
            3 * NS_PER_SEC,
            612,
            1000,
            Level::Info,
            3,
            "ActivityManager",
            "Start proc 4498:com.example.shop/u0a234",
        ));
        s.push(l(
            4 * NS_PER_SEC,
            4498,
            10_234,
            Level::Error,
            BUFFER_CRASH,
            "AndroidRuntime",
            "FATAL EXCEPTION: main\njava.lang.IllegalStateException: no cart\n\tat com.example.shop.Cart.total(Cart.kt:42)",
        ));
        s.push(l(5 * NS_PER_SEC, 4312, 10_234, Level::Info, 0, "Retrofit", "retrying the catalog"));
        s
    }

    fn ids(filter: &str) -> Vec<u64> {
        let s = store();
        let pids = HashSet::new();
        let cx = Context { app_pids: &pids, now: 6 * NS_PER_SEC };
        let f = LogFilter::parse(filter).unwrap();
        s.iter_from(0).filter(|l| f.as_ref().is_none_or(|f| f.matches(l, &s, &cx))).map(|l| l.id).collect()
    }

    #[test]
    fn package_mine_is_the_apps_uid_in_every_process() {
        // the crash came from a new process of the app (pid 4498): the same uid
        assert_eq!(ids("package:mine"), vec![0, 1, 3, 4]);
        assert_eq!(ids("-package:mine"), vec![2]);
        assert_eq!(ids("package:system"), vec![2], "a process name");
        assert_eq!(ids("package:shop"), vec![0, 1, 3, 4], "pid 4498 has no name yet: the app's uid gives its package");
    }

    #[test]
    fn the_same_key_is_either_different_keys_both() {
        assert_eq!(ids("tag:OkHttp tag:Retrofit"), vec![0, 4]);
        assert_eq!(ids("tag:OkHttp tag:Retrofit level:info"), vec![4]);
        assert_eq!(ids("level:w"), vec![1, 3]);
        assert_eq!(ids("tag=:okhttp"), vec![0], "exactly, without regard to case");
        assert_eq!(ids("tag~:^(Ok|Re)"), vec![0, 4]);
        assert_eq!(ids("-tag:OkHttp level:debug package:mine"), vec![1, 3, 4]);
    }

    #[test]
    fn words_regexes_crashes_and_age() {
        assert_eq!(ids("checkout"), vec![1], "the tag or the message");
        assert_eq!(ids("cart total"), vec![3], "every word");
        assert_eq!(ids("/HTTP.\\d+/"), vec![1]);
        assert_eq!(ids("is:crash"), vec![3]);
        assert_eq!(ids("is:stacktrace"), vec![3]);
        assert_eq!(ids("age:2s"), vec![3, 4]);
        assert_eq!(ids("pid:612"), vec![2]);
        assert_eq!(ids("\"checkout failed\""), vec![1], "quotes keep the space");
        assert_eq!(ids(""), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn mistakes_name_their_place() {
        for (input, want) in [
            ("level:loud", "not a level"),
            ("is:everything", "is:crash, is:stacktrace or a level"),
            ("age:soon", "not an age"),
            ("pid:abc", "not a number"),
            ("tag~:(", "not a regex"),
            ("tag:", "needs a value"),
        ] {
            let e = LogFilter::parse(input).unwrap_err();
            assert!(e.message.contains(want), "{input}: {}", e.message);
            assert_eq!(&input[e.span.clone()], input, "{input}");
        }
        let e = LogFilter::parse("tag:ok level:x").unwrap_err();
        assert_eq!(e.span, 7..14);
    }

    #[test]
    fn operators_and_parentheses_as_in_studio() {
        assert_eq!(ids("tag:OkHttp | level:w"), vec![0, 1, 3]);
        // & before |, and parentheses first
        assert_eq!(ids("tag:ActivityManager | level:w & package:mine"), vec![1, 2, 3]);
        assert_eq!(ids("(tag:ActivityManager | level:w) & package:mine"), vec![1, 3]);
        assert_eq!(ids("-(tag:OkHttp | tag:Retrofit)"), vec![1, 2, 3]);
        // Studio's own example: words, then an either
        assert_eq!(ids("retrying catalog tag:Retrofit | tag:OkHttp"), vec![4]);
        assert_eq!(ids("(tag:OkHttp tag:Retrofit) & level:i"), vec![4], "inside parentheses, the same grouping");
        assert_eq!(ids("tag~:(Ok|Re)"), vec![0, 4], "a regex's own parentheses");
        assert_eq!(ids("(tag~:(Ok|Re))"), vec![0, 4]);
    }

    #[test]
    fn values_as_studio_writes_them() {
        assert_eq!(ids("tag: OkHttp"), vec![0], "a space after the colon");
        assert_eq!(ids("age: 2s"), vec![3, 4]);
        assert_eq!(ids("message:'HTTP 500'"), vec![1]);
        assert_eq!(ids("message:checkout\\ failed"), vec![1], "a backslash keeps the space");
        assert_eq!(ids("'checkout failed'"), vec![1]);
        let quoted = r#"'it\'s' "say \"hi\"" tag"#;
        assert_eq!(read_value(quoted, 0).unwrap(), ("it's".to_string(), 7, true), "a quote kept inside quotes");
        assert_eq!(read_value(quoted, 8).unwrap(), ("say \"hi\"".to_string(), 20, true));
        // a word is looked for in the tag, the message and the process's name
        assert_eq!(ids("system_server"), vec![2]);
        assert_eq!(ids("line:system_server line:Retrofit"), vec![2, 4], "line: terms are alternatives");
        assert_eq!(ids("Retrofit OkHttp"), Vec::<u64>::new(), "words must all be found");
        assert_eq!(ids("/api/v1/products"), vec![0], "a path, not a regex");
        assert_eq!(ids("is:error"), vec![3], "exactly that level");
        assert_eq!(ids("is:warn is:crash"), vec![1, 3]);
        assert_eq!(ids("name:checkout level:w"), vec![1, 3], "a name matches every line");
    }

    #[test]
    fn operators_in_the_wrong_place_name_it() {
        for (input, want, span) in [
            ("tag:a |", "| needs a term after it", 6..7),
            ("| tag:a", "| needs a term before it", 0..1),
            ("tag:a & & level:e", "& needs a term after it", 6..7),
            ("(tag:a", "this ( is not closed", 0..1),
            ("tag:a)", "this ) closes no (", 5..6),
            ("()", "nothing between ( and )", 0..1),
            ("\"open", "the quote \" is not closed", 0..5),
            ("tag: | level:e", "tag: needs a value", 0..4),
        ] {
            let e = LogFilter::parse(input).unwrap_err();
            assert_eq!((e.message.as_str(), e.span), (want, span), "{input}");
        }
    }

    #[test]
    fn mine_without_a_uid_falls_back_to_the_runtimes_pids_then_the_process_name() {
        let mut s = LogStore::new();
        s.apply_info(LogInfo {
            package: Some("com.example.shop".into()),
            processes: vec![(10, "com.example.shop:sync".into()), (11, "com.example.shopping".into())],
            ..LogInfo::default()
        });
        for pid in [10, 11, 12] {
            s.push(LogLine {
                ts: 0,
                wall_ms: 0,
                pid,
                tid: pid,
                uid: None,
                level: Level::Info,
                buffer: 0,
                tag: "t".into(),
                message: "m".into(),
            });
        }
        let none = HashSet::new();
        let mine: Vec<u32> = s.iter_from(0).filter(|l| is_mine(l, &s, &none)).map(|l| l.pid).collect();
        assert_eq!(mine, vec![10], "its own processes, not another app whose name starts the same");
        let runtimes = HashSet::from([12]);
        let mine: Vec<u32> = s.iter_from(0).filter(|l| is_mine(l, &s, &runtimes)).map(|l| l.pid).collect();
        assert_eq!(mine, vec![12]);
    }
}
