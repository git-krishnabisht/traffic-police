//! The detail pane: Overview, Response, Request and Call Stack tabs (ARCHITECTURE.md §5.8).
//!
//! Each tab is a [`Doc`]: a few static rows around a lazily rendered body, so a 200,000-line
//! JSON body costs only the visible rows per frame.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use traffic_police_core::fmt;
use traffic_police_core::model::{BodyDir, BodyMeta, BodyState, Transaction, TxnIdx, TxnState, header};

use crate::app::{App, Focus, Tab, Target};
use crate::theme::Theme;
use crate::wrap::{self, Part, Text};
use traffic_police_proto::msg::StackFrame;

/// A WebSocket's messages after its handshake's headers: one line each, with the direction,
/// the time since the socket opened, the type, the size and the start of the payload.
fn ws_rows(theme: &Theme, t: &Transaction, doc: &mut Doc) {
    let sent = t.ws.iter().filter(|m| m.out).count();
    let received = t.ws.len() - sent;
    doc.head
        .push(title(theme, format!("Messages ({} · {sent} sent, {received} received · Enter opens one)", t.ws.len())));
    if t.ws.is_empty() {
        let what = if t.state.is_open() { "none yet" } else { "none" };
        doc.head.push(DocRow::Line(Line::styled(what.to_string(), theme.dim())));
    }
    for (i, m) in t.ws.iter().enumerate() {
        let row = doc.head.len();
        doc.head.push(DocRow::Line(ws_line(theme, t.start, m)));
        doc.messages.push((row, i));
    }
}

/// `↑ +1.204 s  text    27 B  {"type":"subscribe"}`
pub fn ws_line(theme: &Theme, start: fmt::Ts, m: &traffic_police_core::model::WsMessage) -> Line<'static> {
    let arrow = if m.out {
        Span::styled("↑ ", Style::default().fg(theme.send()))
    } else {
        Span::styled("↓ ", Style::default().fg(theme.recv()))
    };
    let since = m.at.saturating_sub(start);
    let preview = match m.op.as_str() {
        "close" => {
            format!("{} {}", m.code.map(|c| c.to_string()).unwrap_or_default(), m.reason.clone().unwrap_or_default())
        }
        "text" => {
            let s = String::from_utf8_lossy(&m.data);
            let one: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).take(240).collect();
            one
        }
        _ => m.data.iter().take(24).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
    };
    let size = if m.op == "close" { String::new() } else { fmt::bytes(m.size) };
    let mut spans = vec![
        arrow,
        Span::styled(format!("+{:<9}", fmt::duration(since)), theme.dim()),
        Span::styled(format!("{:<7}", m.op), theme.title()),
        Span::styled(format!("{size:>8}  "), theme.dim()),
        Span::styled(preview, theme.text()),
    ];
    if m.truncated {
        spans.push(Span::styled(" (truncated)".to_string(), theme.warn()));
    }
    Line::from(spans)
}

#[derive(Debug, Clone)]
pub enum DocRow {
    Line(Line<'static>),
    /// Line `i` of the body view.
    Body(usize),
    /// An app frame at stack index `index`.
    Frame {
        index: usize,
    },
    /// A collapsed run of framework frames starting at stack index `run`.
    FrameRun {
        run: usize,
    },
    /// The Overview timing bar (drawn to the available width).
    TimingBar,
    /// Space reserved for an inline image (the first of `rows` rows).
    Image {
        rows: u16,
    },
    ImageCont,
}

#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub head: Vec<DocRow>,
    pub body_len: usize,
    pub body_dir: Option<BodyDir>,
    pub tail: Vec<DocRow>,
    /// Rows of `head` that show a JWT (Enter decodes it), with the token.
    pub tokens: Vec<(usize, String)>,
    /// Rows of `head` that show a WebSocket message (Enter opens it), with its index.
    pub messages: Vec<(usize, usize)>,
}

impl Doc {
    pub fn len(&self) -> usize {
        self.head.len() + self.body_len + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn row(&self, i: usize) -> Option<DocRow> {
        if i < self.head.len() {
            return Some(self.head[i].clone());
        }
        let j = i - self.head.len();
        if j < self.body_len {
            return Some(DocRow::Body(j));
        }
        self.tail.get(j - self.body_len).cloned()
    }
}

/// Frames of these packages are collapsed by default (the brief's list plus common libraries).
pub const FRAMEWORK_PREFIXES: [&str; 20] = [
    "java.",
    "javax.",
    "jdk.",
    "sun.",
    "libcore.",
    "dalvik.",
    "android.",
    "androidx.",
    "com.android.",
    "kotlin.",
    "kotlinx.coroutines.",
    "okhttp3.",
    "okio.",
    "retrofit2.",
    "$Proxy",
    "com.bumptech.glide.",
    "coil.",
    "coil3.",
    "io.ktor.",
    "com.squareup.",
];

pub fn is_framework(class: &str) -> bool {
    FRAMEWORK_PREFIXES.iter().any(|p| class.starts_with(p))
}

/// The width of the Overview's label column.
pub const LABEL_W: usize = 18;

fn label_row(theme: &Theme, label: &str, value: Vec<Span<'static>>) -> DocRow {
    let mut spans = vec![Span::styled(format!("{label:<LABEL_W$}"), theme.dim())];
    spans.extend(value);
    DocRow::Line(Line::from(spans))
}

fn plain(theme: &Theme, s: impl Into<String>) -> Vec<Span<'static>> {
    vec![Span::styled(s.into(), theme.text())]
}

fn title(theme: &Theme, s: impl Into<String>) -> DocRow {
    DocRow::Line(Line::styled(s.into(), theme.title()))
}

fn blank() -> DocRow {
    DocRow::Line(Line::default())
}

fn http_version(t: &Transaction) -> String {
    let p =
        t.resp.as_ref().and_then(|r| r.protocol.clone()).or_else(|| t.conn.as_ref().and_then(|c| c.protocol.clone()));
    match p.as_deref() {
        Some("h2") | Some("h2_prior_knowledge") => "HTTP/2".into(),
        Some("h3") | Some("quic") => "HTTP/3".into(),
        Some("http/1.0") => "HTTP/1.0".into(),
        _ => "HTTP/1.1".into(),
    }
}

fn body_state_note(meta: &BodyMeta, dir: BodyDir, t: &Transaction) -> Option<String> {
    let what = if dir == BodyDir::Request { "request body" } else { "response body" };
    let s = match meta.state {
        BodyState::Truncated => {
            format!("truncated at {} of {}", fmt::bytes(meta.captured), fmt::bytes(meta.total))
        }
        BodyState::ClosedEarly if meta.total == 0 => "not consumed by the app (closed without reading)".into(),
        BodyState::ClosedEarly => format!("closed early by the app after {}", fmt::bytes(meta.total)),
        BodyState::NotCaptured => {
            format!("{what} not captured (capture disabled), {} passed through", fmt::bytes(meta.total))
        }
        BodyState::Streaming => format!("streaming… {} so far", fmt::bytes(meta.total)),
        BodyState::Error => format!("{what} ended with an error after {}", fmt::bytes(meta.total)),
        BodyState::Pending if t.state.is_open() => "waiting for the body…".into(),
        BodyState::Pending if t.state == TxnState::Detached => "the app went away before the body arrived".into(),
        _ => return meta.gap.then(|| "part of this body was lost on the device (buffer overflow)".into()),
    };
    Some(if meta.gap { format!("{s}; part of it was lost on the device") } else { s })
}

fn headers_rows(theme: &Theme, rows: &mut Vec<DocRow>, headers: &[(String, String)]) {
    pair_rows(theme, rows, "Headers", headers);
}

/// `Headers (3)` or `Trailers (2)`, then `name: value` rows.
fn pair_rows(theme: &Theme, rows: &mut Vec<DocRow>, what: &str, headers: &[(String, String)]) {
    rows.push(title(theme, format!("{what} ({})", headers.len())));
    for (n, v) in headers {
        rows.push(DocRow::Line(Line::from(vec![
            Span::styled(n.clone(), theme.tok(traffic_police_core::decode::Tok::Key)),
            Span::styled(": ", theme.dim()),
            Span::styled(v.clone(), theme.text()),
        ])));
    }
}

/// Body section rows; returns the body view length for the doc.
fn body_rows(app: &mut App, txn: TxnIdx, dir: BodyDir, rows: &mut Vec<DocRow>) -> usize {
    let theme = app.theme.clone();
    let parsed = app.detail.parsed;
    let (meta, state_note) = {
        let t = app.view_store().txn(txn);
        let meta = match dir {
            BodyDir::Request => t.req_body.clone(),
            BodyDir::Response => t.resp_body.clone(),
            BodyDir::Delivered => t.delivered_body.clone().unwrap_or_default(),
        };
        let note = body_state_note(&meta, dir, t);
        (meta, note)
    };
    if meta.id.is_none() {
        let msg = match meta.state {
            BodyState::None => "No body".to_string(),
            BodyState::NotCaptured => format!("Body not captured (capture disabled), {}", fmt::bytes(meta.total)),
            _ => state_note.clone().unwrap_or_else(|| "No body captured".into()),
        };
        rows.push(DocRow::Line(Line::styled(msg, theme.dim())));
        return 0;
    }
    let decoding = app.body_decoding(txn, dir);
    let Some(view) = app.body_view(txn, dir) else {
        if decoding {
            rows.push(DocRow::Line(Line::styled(
                format!("Body  decoding {}…", fmt::bytes(meta.captured)),
                theme.dim(),
            )));
        }
        return 0;
    };
    let mode = if parsed { "parsed · p: source" } else { "source · p: parsed" };
    rows.push(DocRow::Line(Line::from(vec![
        Span::styled("Body", theme.title()),
        Span::styled(format!("  {}  ", view.summary()), theme.dim()),
        Span::styled(format!("[{mode}]"), theme.faint()),
    ])));
    if let Some(n) = &state_note {
        rows.push(DocRow::Line(Line::styled(format!("⚠ {n}"), theme.warn())));
    }
    if let Some(n) = &view.note {
        rows.push(DocRow::Line(Line::styled(format!("⚠ {n}"), theme.warn())));
    }
    if let Some(jq) = &view.jq
        && parsed
    {
        rows.push(DocRow::Line(Line::styled(
            format!("jq filter: {}   (| edits · empty filter clears)", jq.filter),
            theme.accent(),
        )));
    }
    if parsed && view.jq.is_none() && view.image().is_some_and(|i| i.image.is_some()) {
        rows.push(DocRow::Image { rows: 12 });
        for _ in 1..12 {
            rows.push(DocRow::ImageCont);
        }
        return 0;
    }
    view.len(parsed)
}

pub fn build_doc(app: &mut App) -> Doc {
    let Some(txn) = app.selected else { return Doc::default() };
    let theme = app.theme.clone();
    let tab = app.detail.tab;
    let mut doc = Doc::default();
    match tab {
        Tab::Overview => overview_rows(app, txn, &mut doc.head, &mut doc.tokens),
        Tab::Response => {
            let (status_line, rule_note, headers, has_resp) = {
                let t = app.view_store().txn(txn);
                let original = app.detail.original || t.delivered.is_none();
                let (status, message, headers) = match (&t.resp, &t.delivered, original) {
                    (_, Some(d), false) => (Some(d.status), d.message.clone(), d.headers.clone()),
                    (Some(r), _, _) => (Some(r.status), r.message.clone(), r.headers.clone()),
                    _ => (None, String::new(), Vec::new()),
                };
                let status_line = status.map(|s| {
                    Line::from(vec![
                        Span::styled(format!("{} ", http_version(t)), theme.dim()),
                        Span::styled(s.to_string(), theme.status(t.status_class()).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" {message}"), theme.text()),
                    ])
                });
                let rule_note = t.rule_modified().then(|| {
                    let names: Vec<String> = t
                        .rules
                        .iter()
                        .flat_map(|h| h.rules.iter().map(|r| r.name.clone().unwrap_or_else(|| r.id.clone())))
                        .collect();
                    if original {
                        format!(
                            "✎ showing the ORIGINAL response; rule \"{}\" changed what the app received · o: delivered",
                            names.join(", ")
                        )
                    } else {
                        format!("✎ changed by rule \"{}\" · o: original", names.join(", "))
                    }
                });
                (status_line, rule_note, headers, t.resp.is_some())
            };
            if !has_resp {
                let failure = app.view_store().txn(txn).failure.clone();
                match failure {
                    Some(f) => doc.head.push(DocRow::Line(Line::styled(
                        format!("No response: {}: {}", f.class, f.message.unwrap_or_default()),
                        theme.error(),
                    ))),
                    None => doc.head.push(DocRow::Line(Line::styled("Waiting for the response…", theme.dim()))),
                }
                return doc;
            }
            if let Some(l) = status_line {
                doc.head.push(DocRow::Line(l));
            }
            if let Some(n) = rule_note {
                doc.head.push(DocRow::Line(Line::styled(n, theme.marker())));
            }
            doc.head.push(blank());
            headers_rows(&theme, &mut doc.head, &headers);
            doc.head.push(blank());
            let t = app.view_store().txn(txn);
            if t.is_websocket() {
                ws_rows(&theme, t, &mut doc);
                return doc;
            }
            let dir = app.response_dir(txn);
            doc.body_len = body_rows(app, txn, dir, &mut doc.head);
            doc.body_dir = Some(dir);
            let trailers = app.view_store().txn(txn).trailers.clone();
            if !trailers.is_empty() {
                doc.tail.push(blank());
                pair_rows(&theme, &mut doc.tail, "Trailers", &trailers);
            }
        }
        Tab::Request => {
            let (req_line, query, headers) = {
                let t = app.view_store().txn(txn);
                let target = match &t.url.query {
                    Some(q) => format!("{}?{q}", t.url.path),
                    None => t.url.path.clone(),
                };
                let req_line = Line::from(vec![
                    Span::styled(format!("{} ", t.method), theme.title()),
                    Span::styled(target, theme.text()),
                    Span::styled(format!(" {}", http_version(t)), theme.dim()),
                ]);
                (req_line, t.url.query_pairs(), t.req_headers.clone())
            };
            doc.head.push(DocRow::Line(req_line));
            doc.head.push(blank());
            if !query.is_empty() {
                doc.head.push(title(&theme, format!("Query parameters ({})", query.len())));
                let w = query.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0).min(28);
                for (k, v) in query {
                    doc.head.push(DocRow::Line(Line::from(vec![
                        Span::styled(format!("{k:<w$}"), theme.tok(traffic_police_core::decode::Tok::Field)),
                        Span::styled(" = ", theme.dim()),
                        Span::styled(v, theme.tok(traffic_police_core::decode::Tok::Str)),
                    ])));
                }
                doc.head.push(blank());
            }
            headers_rows(&theme, &mut doc.head, &headers);
            doc.head.push(blank());
            doc.body_len = body_rows(app, txn, BodyDir::Request, &mut doc.head);
            doc.body_dir = Some(BodyDir::Request);
        }
        Tab::CallStack => {
            let t = app.view_store().txn(txn);
            match &t.thread {
                Some(th) => {
                    doc.head.push(DocRow::Line(Line::from(vec![
                        Span::styled("Thread  ", theme.dim()),
                        Span::styled(th.name.clone(), theme.title()),
                        Span::styled(format!("  (id {})", th.id), theme.dim()),
                    ])));
                    let origin = match th.origin.as_deref() {
                        Some("call") => "captured when the app called execute() or enqueue(): the real call site",
                        Some("interceptor") => {
                            "captured in the network interceptor (no event listener installed; for enqueue() this is an OkHttp thread)"
                        }
                        Some("huc") => "captured on the thread that opened the HttpURLConnection",
                        _ => "origin unknown",
                    };
                    doc.head.push(DocRow::Line(Line::styled(origin.to_string(), theme.faint())));
                }
                None => doc.head.push(DocRow::Line(Line::styled("No thread information", theme.dim()))),
            }
            doc.head.push(blank());
            if t.stack.is_empty() {
                doc.head.push(DocRow::Line(Line::styled("No call stack captured", theme.dim())));
            }
            let mut i = 0;
            while i < t.stack.len() {
                if is_framework(&t.stack[i].c) {
                    let run = i;
                    let mut j = i;
                    while j < t.stack.len() && is_framework(&t.stack[j].c) {
                        j += 1;
                    }
                    if app.detail.expanded_runs.contains(&run) {
                        doc.head.push(DocRow::FrameRun { run });
                        for k in run..j {
                            doc.head.push(DocRow::Frame { index: k });
                        }
                    } else {
                        doc.head.push(DocRow::FrameRun { run });
                    }
                    i = j;
                } else {
                    doc.head.push(DocRow::Frame { index: i });
                    i += 1;
                }
            }
            if t.stack_truncated {
                doc.head.push(DocRow::Line(Line::styled(
                    "… more frames were not captured (stack depth limit)",
                    theme.faint(),
                )));
            }
        }
    }
    doc
}

/// What a doc row is drawn as: text that wraps (with its indent, and whether its ` · ` items
/// stay together), a body line, or something one row high.
enum RowKind {
    Text(Text<'static>, usize, bool),
    Body(usize),
    Fixed,
}

fn row_kind(app: &mut App, doc: &Doc, i: usize, txn: TxnIdx) -> RowKind {
    match doc.row(i) {
        Some(DocRow::Line(l)) => {
            // a label row continues under its value; the pane's other rows a little indented
            let label =
                l.spans.first().is_some_and(|s| s.content.chars().count() == LABEL_W && s.content.ends_with(' '));
            RowKind::Text(Text::from_line(&l), if label { LABEL_W } else { 2 }, true)
        }
        Some(DocRow::Body(bi)) => RowKind::Body(bi),
        Some(DocRow::Frame { index }) => {
            let theme = app.theme.clone();
            let t = app.view_store().txn(txn);
            match t.stack.get(index) {
                Some(f) => {
                    let text = Text::from_line(&frame_line(&theme, f));
                    let indent = text.leading() + 2;
                    RowKind::Text(text, indent, false)
                }
                None => RowKind::Fixed,
            }
        }
        Some(DocRow::FrameRun { run }) => {
            let theme = app.theme.clone();
            let expanded = app.detail.expanded_runs.contains(&run);
            let stack = app.view_store().txn(txn).stack.clone();
            let text = Text::from_line(&frame_run_line(&theme, &stack, run, expanded));
            let indent = text.leading() + 2;
            RowKind::Text(text, indent, false)
        }
        Some(DocRow::TimingBar | DocRow::Image { .. } | DocRow::ImageCont) | None => RowKind::Fixed,
    }
}

/// The rows doc row `i` takes in a pane `width` wide: one when nothing wraps (`[ui] wrap`).
pub fn row_height(app: &mut App, doc: &Doc, i: usize, txn: TxnIdx, width: usize) -> usize {
    if !app.prefs.wrap {
        return 1;
    }
    let parsed = app.detail.parsed;
    match row_kind(app, doc, i, txn) {
        RowKind::Text(text, indent, items) => wrap::layout(&text.text, width, indent, items).len(),
        RowKind::Body(bi) => match doc.body_dir.and_then(|dir| app.body_view(txn, dir)) {
            Some(v) => v.breaks(bi, parsed, width).len(),
            None => 1,
        },
        RowKind::Fixed => 1,
    }
}

/// The row of doc row `i` (wrapped at `width`) that column `col` of it is on.
pub fn row_part_at(app: &mut App, doc: &Doc, i: usize, txn: TxnIdx, width: usize, col: usize) -> usize {
    if !app.prefs.wrap {
        return 0;
    }
    let parsed = app.detail.parsed;
    let parts = match row_kind(app, doc, i, txn) {
        RowKind::Text(text, indent, items) => wrap::layout(&text.text, width, indent, items),
        RowKind::Body(bi) => match doc.body_dir.and_then(|dir| app.body_view(txn, dir)) {
            Some(v) => v.breaks(bi, parsed, width).to_vec(),
            None => return 0,
        },
        RowKind::Fixed => return 0,
    };
    parts.iter().rposition(|p| p.col <= col).unwrap_or(0)
}

/// Rows `from..from + n` of doc row `i`, drawn, with where each one is in the row: wrapped at
/// `width`, or (not wrapping) the columns from the horizontal scroll `hs` on.
fn row_parts(
    app: &mut App,
    doc: &Doc,
    i: usize,
    txn: TxnIdx,
    width: usize,
    from: usize,
    n: usize,
) -> Vec<(Line<'static>, Part)> {
    let (wrapping, parsed, hs) = (app.prefs.wrap, app.detail.parsed, usize::from(app.detail.hscroll));
    let theme = app.theme.clone();
    match row_kind(app, doc, i, txn) {
        RowKind::Text(text, indent, items) => {
            if wrapping {
                let parts = wrap::layout(&text.text, width, indent, items);
                parts.iter().skip(from).take(n).map(|p| (text.part(p), *p)).collect()
            } else {
                let p = text.window(hs, width);
                vec![(text.part(&p), p)]
            }
        }
        RowKind::Body(bi) => match doc.body_dir.and_then(|dir| app.body_view(txn, dir)) {
            Some(v) if wrapping => v.wrapped(bi, parsed, &theme, width, from, n),
            Some(v) => vec![v.window(bi, parsed, &theme, hs, width)],
            None => Vec::new(),
        },
        RowKind::Fixed => Vec::new(),
    }
}

/// A stack frame: `at class.method(file:line)`, app frames in the accent.
fn frame_line(theme: &Theme, f: &StackFrame) -> Line<'static> {
    let app_frame = !is_framework(&f.c);
    let loc = match (&f.f, f.l) {
        (Some(file), Some(l)) => format!("({file}:{l})"),
        (Some(file), None) => format!("({file})"),
        _ => "(Unknown Source)".into(),
    };
    let style = if app_frame { theme.accent().add_modifier(Modifier::BOLD) } else { theme.dim() };
    Line::from(vec![
        Span::styled(if app_frame { "  at " } else { "    at " }, theme.faint()),
        Span::styled(format!("{}.{}", f.c, f.m), style),
        Span::styled(loc, if app_frame { theme.text() } else { theme.faint() }),
    ])
}

/// A run of framework frames: how many, from which packages, and whether Enter expands it.
fn frame_run_line(theme: &Theme, stack: &[StackFrame], run: usize, expanded: bool) -> Line<'static> {
    let mut j = run;
    while j < stack.len() && is_framework(&stack[j].c) {
        j += 1;
    }
    let mut pkgs: Vec<String> = Vec::new();
    for f in &stack[run.min(j)..j] {
        let p = FRAMEWORK_PREFIXES
            .iter()
            .find(|p| f.c.starts_with(**p))
            .map(|p| p.trim_end_matches('.').to_string())
            .unwrap_or_default();
        if !pkgs.contains(&p) {
            pkgs.push(p);
        }
    }
    let n = j.saturating_sub(run);
    Line::from(vec![
        Span::styled(if expanded { "  ▾ " } else { "  ▸ " }, theme.faint()),
        Span::styled(
            format!("{n} framework frame{} ({})", if n == 1 { "" } else { "s" }, pkgs.join(", ")),
            theme.faint(),
        ),
        Span::styled(if expanded { "" } else { "  Enter expands" }, theme.faint()),
    ])
}

/// How this request relates to the other hops of the same call (redirects, auth retries).
/// Hops are recorded close together, so only nearby transactions are searched.
fn redirect_note(store: &traffic_police_core::SessionStore, txn: TxnIdx) -> Option<String> {
    const NEAR: usize = 512;
    let t = store.txn(txn);
    t.call?;
    let same_call = |i: usize, hop: u32| {
        let o = store.txn(i as TxnIdx);
        o.key.source == t.key.source && o.call == t.call && o.hop == hop && !o.placeholder
    };
    let i = txn as usize;
    let prev = if t.hop > 0 { (i.saturating_sub(NEAR)..i).rev().find(|&j| same_call(j, t.hop - 1)) } else { None };
    let next = (i + 1..(i + NEAR).min(store.len())).find(|&j| same_call(j, t.hop + 1));
    let describe = |j: usize| {
        let o = store.txn(j as TxnIdx);
        let status = o.status().map(|s| format!(" ({s})")).unwrap_or_default();
        format!("{}{status}", o.url.raw)
    };
    match (prev, next) {
        (Some(p), Some(n)) => {
            Some(format!("hop {} of this call · after {} · then {}", t.hop + 1, describe(p), describe(n)))
        }
        (Some(p), None) => Some(format!("hop {} of this call · after {}", t.hop + 1, describe(p))),
        (None, Some(n)) => Some(format!("followed by {}", describe(n))),
        (None, None) => None,
    }
}

/// JWTs of a request, and where each was: headers (either direction), then small bodies.
fn jwts(app: &mut App, txn: TxnIdx, t: &Transaction) -> Vec<(String, String)> {
    use traffic_police_core::values::find_jwt;
    let mut out: Vec<(String, String)> = Vec::new();
    let mut add = |place: String, text: &str| {
        if let Some(r) = find_jwt(text) {
            let token = text[r].to_string();
            if !out.iter().any(|(_, t)| *t == token) {
                out.push((place, token));
            }
        }
    };
    for (n, v) in &t.req_headers {
        add(format!("request header {n}"), v);
    }
    for (n, v) in t.resp.iter().flat_map(|r| &r.headers) {
        add(format!("response header {n}"), v);
    }
    for (dir, what) in [(BodyDir::Request, "the request body"), (app.response_dir(txn), "the response body")] {
        let small = |m: &BodyMeta| m.captured > 0 && m.captured <= 256 << 10;
        let meta = match dir {
            BodyDir::Request => Some(&t.req_body),
            BodyDir::Response => Some(&t.resp_body),
            BodyDir::Delivered => t.delivered_body.as_ref(),
        };
        if meta.is_some_and(small)
            && let Some(v) = app.body_view(txn, dir)
            && let Ok(text) = std::str::from_utf8(&v.decoded.bytes)
        {
            let text = text.to_string();
            add(what.to_string(), &text);
        }
    }
    out
}

fn overview_rows(app: &mut App, txn: TxnIdx, rows: &mut Vec<DocRow>, tokens: &mut Vec<(usize, String)>) {
    let theme = app.theme.clone();
    let now = app.now();
    let origin = app.view_store().origin();
    let (wall, t) = {
        let s = app.view_store();
        let t = s.txn(txn).clone();
        (s.wall_ms(t.start), t)
    };
    rows.push(label_row(&theme, "Request", plain(&theme, t.url.name())));
    rows.push(label_row(&theme, "Method", plain(&theme, t.method.clone())));
    let status = match (t.status(), &t.failure) {
        (_, Some(f)) => vec![Span::styled(
            format!("{}: {}", f.short_class(), f.message.clone().unwrap_or_default()),
            theme.status(t.status_class()),
        )],
        (Some(s), None) => {
            let msg = t
                .delivered
                .as_ref()
                .map(|d| d.message.clone())
                .or(t.resp.as_ref().map(|r| r.message.clone()))
                .unwrap_or_default();
            vec![Span::styled(format!("{s} {msg}"), theme.status(t.status_class()))]
        }
        (None, None) => vec![Span::styled(t.status_text(), theme.dim())],
    };
    rows.push(label_row(&theme, "Status", status));
    if let Some(g) = &t.grpc {
        let mut text = format!("{} ({})", g.name, g.code);
        if let Some(m) = g.message.as_deref().filter(|m| !m.is_empty()) {
            text.push_str(&format!(": {m}"));
        }
        rows.push(label_row(&theme, "gRPC status", vec![Span::styled(text, theme.status(t.status_class()))]));
    }
    rows.push(label_row(&theme, "URL", vec![Span::styled(t.url.raw.clone(), theme.accent())]));
    if let Some(r) = redirect_note(app.view_store(), txn) {
        rows.push(label_row(&theme, "Redirect", plain(&theme, r)));
    }
    rows.push(label_row(&theme, "Request type", plain(&theme, t.request_content_type().unwrap_or("—").to_string())));
    rows.push(label_row(&theme, "Response type", plain(&theme, t.response_content_type().unwrap_or("—").to_string())));
    let dir = app.response_dir(txn);
    let size = if t.resp_body.total > 0 {
        let decoded = app.body_view(txn, dir).map(|v| (v.decoded.bytes.len(), !v.decoded.encodings.is_empty()));
        match decoded {
            Some((d, true)) => {
                format!("{} transferred, {} decoded", fmt::bytes(t.resp_body.total), fmt::bytes(d as u64))
            }
            _ => fmt::bytes(t.resp_body.total),
        }
    } else if t.resp.is_some() {
        "0 B".into()
    } else {
        "—".into()
    };
    // a socket's size is what went each way, on the next row
    if !t.is_websocket() {
        rows.push(label_row(&theme, "Response size", plain(&theme, size)));
    }
    if t.is_websocket() {
        let (sent, received): (Vec<_>, Vec<_>) = t.ws.iter().partition(|m| m.out);
        let bytes = |v: &[&traffic_police_core::model::WsMessage]| fmt::bytes(v.iter().map(|m| m.size).sum());
        let state = if t.state.is_open() { "open" } else { "closed" };
        rows.push(label_row(
            &theme,
            "WebSocket",
            plain(
                &theme,
                format!(
                    "{state} · {} sent ({}) · {} received ({}) · the messages are on the Response tab",
                    sent.len(),
                    bytes(&sent),
                    received.len(),
                    bytes(&received)
                ),
            ),
        ));
    }
    if t.req_body.total > 0 {
        rows.push(label_row(&theme, "Request size", plain(&theme, fmt::bytes(t.req_body.total))));
    }
    for (place, token) in jwts(app, txn, &t) {
        let Some(jwt) = traffic_police_core::values::jwt(&token) else { continue };
        let now_ms = app.wall_now_ms();
        tokens.push((rows.len(), token));
        rows.push(label_row(
            &theme,
            "Token",
            vec![
                Span::styled(jwt.summary(now_ms), theme.text()),
                Span::styled(format!("  in {place} · Enter decodes"), theme.dim()),
            ],
        ));
    }
    if let Some(th) = &t.thread {
        let origin = match th.origin.as_deref() {
            Some("call") => "call site",
            Some("interceptor") => "interceptor thread",
            Some("huc") => "HttpURLConnection",
            _ => "",
        };
        rows.push(label_row(
            &theme,
            "Initiating thread",
            vec![
                Span::styled(th.name.clone(), theme.text()),
                Span::styled(format!("  id {} · {origin}", th.id), theme.dim()),
            ],
        ));
    }
    if let Some(c) = &t.client {
        rows.push(label_row(&theme, "Client", plain(&theme, c.label())));
    }
    let protocol =
        t.resp.as_ref().and_then(|r| r.protocol.clone()).or_else(|| t.conn.as_ref().and_then(|c| c.protocol.clone()));
    if let Some(p) = protocol {
        rows.push(label_row(&theme, "Protocol", plain(&theme, p)));
    }
    if let Some(c) = &t.conn {
        if let Some(r) = &c.remote {
            let addr =
                if r.ip.contains(':') { format!("[{}]:{}", r.ip, r.port) } else { format!("{}:{}", r.ip, r.port) };
            let mut v = vec![Span::styled(addr, theme.text())];
            if let Some(id) = &c.id {
                let reused = if c.reused == Some(true) { "reused" } else { "new" };
                v.push(Span::styled(format!("  connection {id} ({reused})"), theme.dim()));
            }
            if let Some(p) = c.proxy.as_deref().filter(|p| *p != "DIRECT") {
                v.push(Span::styled(format!("  via {p}"), theme.dim()));
            }
            rows.push(label_row(&theme, "Remote address", v));
        }
        if let Some(tls) = &c.tls {
            rows.push(label_row(
                &theme,
                "TLS",
                plain(
                    &theme,
                    format!("{} · {}", tls.version.clone().unwrap_or_default(), tls.cipher.clone().unwrap_or_default()),
                ),
            ));
            if let Some(leaf) = tls.peer.first() {
                let issuer = leaf
                    .issuer
                    .as_deref()
                    .and_then(|i| i.split(',').next())
                    .unwrap_or("")
                    .trim_start_matches("CN=")
                    .to_string();
                let subject = leaf.subject.clone().unwrap_or_default();
                rows.push(label_row(&theme, "Certificate", plain(&theme, format!("{subject} · issued by {issuer}"))));
            }
        }
    }
    let started = fmt::offset(t.start.saturating_sub(origin));
    let started = match wall {
        Some(w) => format!("{started}  ({})", fmt::wall_clock(w)),
        None => started,
    };
    rows.push(label_row(&theme, "Started", plain(&theme, started)));
    rows.push(label_row(
        &theme,
        "Duration",
        plain(&theme, fmt::duration(t.duration(now)) + if t.state.is_open() { " so far" } else { "" }),
    ));
    rows.push(blank());
    rows.push(DocRow::TimingBar);
    let p = t.phases(now);
    let d = |s: Option<(u64, u64)>| s.map_or("—".to_string(), |(a, b)| fmt::duration(b - a));
    let mut parts = vec![];
    if p.queued.is_some() {
        parts.push(format!("queued {}", d(p.queued)));
    }
    parts.push(format!("dns {}", d(p.dns)));
    let tls = p.tls.map(|(a, b)| format!(" (tls {})", fmt::duration(b - a))).unwrap_or_default();
    parts.push(format!("connect {}{tls}", d(p.connect)));
    parts.push(format!("send {}", d(p.send)));
    parts.push(format!("wait {}", d(p.wait)));
    parts.push(format!("receive {}", d(p.receive)));
    rows.push(DocRow::Line(Line::from(vec![
        Span::raw(" ".repeat(LABEL_W)),
        Span::styled(parts.join(" · "), theme.dim()),
    ])));
    if t.rule_modified() {
        rows.push(blank());
        for hit in &t.rules {
            let names: Vec<String> = hit.rules.iter().map(|r| r.name.clone().unwrap_or_else(|| r.id.clone())).collect();
            let changes: Vec<String> = hit
                .changes
                .iter()
                .map(|c| match c.op.as_str() {
                    "status" => format!("status {}→{}", c.from.unwrap_or(0), c.to.unwrap_or(0)),
                    "header_set" => format!("set {}", c.name.clone().unwrap_or_default()),
                    "header_add" => format!("add {}", c.name.clone().unwrap_or_default()),
                    "header_remove" => format!("remove {}", c.name.clone().unwrap_or_default()),
                    "body_replace" => "body replaced".into(),
                    "body_edit" => format!(
                        "body edited ({} match{})",
                        c.matches.unwrap_or(0),
                        if c.matches == Some(1) { "" } else { "es" }
                    ),
                    "delay" => format!("delayed {} ms", c.ms.unwrap_or(0)),
                    "fail" => format!("failed with {}", c.exception.clone().unwrap_or_default()),
                    other => other.to_string(),
                })
                .collect();
            rows.push(label_row(
                &theme,
                "Rule",
                vec![
                    Span::styled(format!("✎ \"{}\" changed this response: ", names.join(", ")), theme.marker()),
                    Span::styled(format!("{} · o in Response shows the original", changes.join(", ")), theme.text()),
                ],
            ));
        }
    }
    if let Some(f) = &t.failure {
        rows.push(blank());
        let phase = f.phase.as_deref().map(|p| format!(" (during {})", p.replace('_', " "))).unwrap_or_default();
        let kind = if f.canceled {
            "Canceled"
        } else if f.simulated {
            "Simulated failure"
        } else {
            "Failed"
        };
        rows.push(label_row(
            &theme,
            kind,
            vec![Span::styled(format!("{}: {}{phase}", f.class, f.message.clone().unwrap_or_default()), theme.error())],
        ));
    }
    if t.lossy {
        rows.push(label_row(
            &theme,
            "Note",
            vec![Span::styled("some events of this request were dropped on the device", theme.warn())],
        ));
    }
    if t.placeholder {
        rows.push(label_row(
            &theme,
            "Note",
            vec![Span::styled("the start of this request was not captured", theme.warn())],
        ));
    }
}

/// Draw `line` starting at `x`, skipping the first `skip` columns (horizontal scroll).
pub fn draw_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: &Line<'_>, skip: u16, base: Style) {
    let mut col: u16 = 0;
    let mut out_x = x;
    let end = x + width;
    for span in &line.spans {
        let style = base.patch(span.style);
        for g in unicode_width_graphemes(&span.content) {
            let w = unicode_width::UnicodeWidthStr::width(g) as u16;
            if col + w <= skip {
                col += w;
                continue;
            }
            if out_x + w > end {
                return;
            }
            if let Some(cell) = buf.cell_mut((out_x, y)) {
                cell.set_symbol(g).set_style(style);
            }
            if w == 2
                && let Some(cell) = buf.cell_mut((out_x + 1, y))
            {
                cell.set_symbol("").set_style(style);
            }
            out_x += w.max(1);
            col += w;
        }
    }
}

fn unicode_width_graphemes(s: &str) -> impl Iterator<Item = &str> {
    // good enough for headers and bodies: split by char (combining marks are rare here)
    s.char_indices().map(move |(i, c)| &s[i..i + c.len_utf8()])
}

/// A row's text (all of it, however it wraps), for search, Enter and copy; `None` for rows that
/// are not text (the timing bar, images).
pub fn row_text(app: &mut App, doc: &Doc, i: usize, txn: TxnIdx) -> Option<String> {
    match doc.row(i)? {
        DocRow::Line(l) => Some(l.spans.iter().map(|s| s.content.as_ref()).collect()),
        DocRow::Body(bi) => {
            let dir = doc.body_dir?;
            let parsed = app.detail.parsed;
            app.body_view(txn, dir).map(|v| v.plain(bi, parsed))
        }
        DocRow::Frame { .. } | DocRow::FrameRun { .. } => match row_kind(app, doc, i, txn) {
            RowKind::Text(text, ..) => Some(text.text.into_owned()),
            _ => None,
        },
        DocRow::TimingBar | DocRow::Image { .. } | DocRow::ImageCont => None,
    }
}

/// Draws the current tab into `area`; returns the cursor line's JSON path, for the box's border
/// (drawn over the last row, it would hide what is there).
pub fn draw(app: &mut App, area: Rect, buf: &mut Buffer) -> Option<String> {
    let txn = app.selected?;
    let theme = app.theme.clone();
    let focused = app.focus == Focus::Detail;
    app.detail_height = area.height as usize;
    app.detail_width = area.width as usize;
    let doc = build_doc(app);
    let len = doc.len();
    if len == 0 {
        return None;
    }
    app.refresh_search(&doc);
    let hits = app.search.matches.clone();
    let current_hit = app.search.current;
    app.detail.cursor = app.detail.cursor.min(len - 1);
    // the view within the rows as they are now, with the cursor's row on it
    let cursor = app.detail.cursor;
    let top = app.detail_top();
    let top = app.with_detail_lines(&doc, txn, |l| {
        let top = l.clamp(top);
        if cursor >= top.line && l.row_of(top, cursor) < l.view { top } else { l.show(top, cursor, 0) }
    });
    app.set_detail_top(top);
    let parsed = app.detail.parsed;
    let dir = doc.body_dir;
    let now = app.now();
    let txn_clone = app.view_store().txn(txn).clone();
    let (width, view) = (area.width as usize, area.height as usize);
    let mut y = 0;
    let mut i = top.line;
    let mut skip = top.part;
    while y < view && i < len {
        let selected = focused && i == cursor;
        let base = if selected { theme.selected() } else { Style::default() };
        let row = doc.row(i);
        let parts = match row {
            Some(DocRow::TimingBar | DocRow::Image { .. } | DocRow::ImageCont) | None => Vec::new(),
            Some(_) => row_parts(app, &doc, i, txn, width, skip, view - y),
        };
        let rows = parts.len().max(1);
        for k in 0..rows.min(view - y) {
            let ry = area.y + (y + k) as u16;
            let r = Rect { x: area.x, y: ry, width: area.width, height: 1 };
            if selected {
                crate::ui::fill(buf, r, theme.selected());
            }
            app.hits.add(r, Target::DetailLine(i));
        }
        match row {
            Some(DocRow::TimingBar) => draw_timing_bar(
                &theme,
                &txn_clone,
                now,
                buf,
                Rect { x: area.x, y: area.y + y as u16, width: area.width, height: 1 },
            ),
            Some(DocRow::Image { rows }) => {
                if let Some(dir) = dir {
                    let img = app.body_view(txn, dir).and_then(|v| v.image().and_then(|i| i.image.clone()));
                    if let Some(img) = img {
                        let ry = area.y + y as u16;
                        let avail = (area.y + area.height).saturating_sub(ry).min(rows);
                        let r = Rect {
                            x: area.x + 1,
                            y: ry,
                            width: area.width.saturating_sub(2).min(rows * 4),
                            height: avail,
                        };
                        app.images.render(&img, r, buf);
                    }
                }
            }
            _ => {}
        }
        // each row of it, and the search matches over what it shows
        let first = hits.partition_point(|m| m.row < i);
        for (k, (line, part)) in parts.iter().enumerate() {
            let ry = area.y + (y + k) as u16;
            draw_line(buf, area.x, ry, area.width, line, 0, base);
            let shown = line.width().saturating_sub(part.lead);
            for (h, m) in hits.iter().enumerate().skip(first).take_while(|(_, m)| m.row == i) {
                let (from, to) = (m.start.max(part.col), m.end.min(part.col + shown));
                if from >= to {
                    continue;
                }
                let style = theme.search_hit(current_hit == Some(h));
                for x in part.lead + from - part.col..(part.lead + to - part.col).min(width) {
                    if let Some(c) = buf.cell_mut((area.x + x as u16, ry)) {
                        c.set_style(style);
                    }
                }
            }
        }
        y += rows;
        skip = 0;
        i += 1;
    }
    // the JSON path of the cursor line
    match (focused, doc.row(cursor), dir) {
        (true, Some(DocRow::Body(bi)), Some(dir)) => app.body_view(txn, dir).and_then(|v| v.path_at(bi, parsed)),
        _ => None,
    }
}

fn draw_timing_bar(theme: &Theme, t: &Transaction, now: u64, buf: &mut Buffer, area: Rect) {
    let label_w = 18u16;
    draw_line(buf, area.x, area.y, label_w, &Line::styled("Timing", theme.dim()), 0, Style::default());
    let x0 = area.x + label_w;
    let w = area.width.saturating_sub(label_w + 1);
    if w < 4 {
        return;
    }
    let seg = t.segments(now);
    let total = seg.end.saturating_sub(seg.start).max(1) as f64;
    let pos = |ts: u64| ((ts.saturating_sub(seg.start)) as f64 / total * f64::from(w)).round() as u16;
    let sent = pos(seg.sent);
    let fb = seg.first_byte.map(pos).unwrap_or(w);
    for i in 0..w {
        let color = if i < sent {
            theme.send()
        } else if i < fb {
            theme.wait()
        } else {
            theme.recv()
        };
        if let Some(c) = buf.cell_mut((x0 + i, area.y)) {
            c.set_symbol(if theme.mono() {
                if i < sent {
                    "▒"
                } else if i < fb {
                    "░"
                } else {
                    "█"
                }
            } else {
                "█"
            })
            .set_style(Style::default().fg(color));
        }
    }
}

/// Header value lookup for other modules.
pub fn content_type(t: &Transaction) -> Option<&str> {
    header(t.response_headers()?, "content-type")
}
