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
use crate::bodyview::{BodyView, Window};
use crate::theme::Theme;

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

fn label_row(theme: &Theme, label: &str, value: Vec<Span<'static>>) -> DocRow {
    let mut spans = vec![Span::styled(format!("{label:<18}"), theme.dim())];
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
    rows.push(title(theme, format!("Headers ({})", headers.len())));
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
        Tab::Overview => overview_rows(app, txn, &mut doc.head),
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
            let dir = app.response_dir(txn);
            doc.body_len = body_rows(app, txn, dir, &mut doc.head);
            doc.body_dir = Some(dir);
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

fn overview_rows(app: &mut App, txn: TxnIdx, rows: &mut Vec<DocRow>) {
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
    rows.push(label_row(&theme, "Response size", plain(&theme, size)));
    if t.req_body.total > 0 {
        rows.push(label_row(&theme, "Request size", plain(&theme, fmt::bytes(t.req_body.total))));
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
            let mut v = vec![Span::styled(format!("{}:{}", r.ip, r.port), theme.text())];
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
    rows.push(DocRow::Line(Line::from(vec![Span::raw(" ".repeat(18)), Span::styled(parts.join(" · "), theme.dim())])));
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

/// Render the pane's content rows for the current tab into `area`, with the Overview preview.
/// A row's text as drawn (before horizontal scrolling), for search; `None` for rows that are
/// not text (the timing bar, images).
pub fn row_text(app: &mut App, doc: &Doc, i: usize, txn: TxnIdx) -> Option<String> {
    match doc.row(i)? {
        DocRow::Line(l) => Some(l.spans.iter().map(|s| s.content.as_ref()).collect()),
        DocRow::Body(bi) => {
            let dir = doc.body_dir?;
            let parsed = app.detail.parsed;
            app.body_view(txn, dir).map(|v| v.plain(bi, parsed))
        }
        DocRow::Frame { index } => {
            let t = app.view_store().txn(txn);
            let f = t.stack.get(index)?;
            let lead = if is_framework(&f.c) { "    at " } else { "  at " };
            let loc = match (&f.f, f.l) {
                (Some(file), Some(l)) => format!("({file}:{l})"),
                (Some(file), None) => format!("({file})"),
                _ => "(Unknown Source)".into(),
            };
            Some(format!("{lead}{}.{}{loc}", f.c, f.m))
        }
        DocRow::FrameRun { .. } | DocRow::TimingBar | DocRow::Image { .. } | DocRow::ImageCont => None,
    }
}

pub fn draw(app: &mut App, area: Rect, buf: &mut Buffer) {
    let Some(txn) = app.selected else { return };
    let theme = app.theme.clone();
    let focused = app.focus == Focus::Detail;
    let mut area = area;
    if app.detail.tab == Tab::Overview {
        let preview_h = (area.height * 2 / 5).clamp(4, 14);
        let preview = Rect { height: preview_h.min(area.height.saturating_sub(4)), ..area };
        draw_preview(app, txn, preview, buf);
        area = Rect { y: area.y + preview.height + 1, height: area.height.saturating_sub(preview.height + 1), ..area };
        // separator
        for x in area.x..area.x + area.width {
            if let Some(c) = buf.cell_mut((x, area.y.saturating_sub(1))) {
                c.set_symbol("─").set_style(theme.faint());
            }
        }
    }
    app.detail_height = area.height as usize;
    let doc = build_doc(app);
    let len = doc.len();
    if len == 0 {
        return;
    }
    app.refresh_search(&doc);
    let hits = app.search.matches.clone();
    let current_hit = app.search.current;
    app.detail.cursor = app.detail.cursor.min(len - 1);
    app.clamp_detail_scroll();
    let scroll = app.detail.scroll.min(len.saturating_sub(1));
    let parsed = app.detail.parsed;
    let hs = app.detail.hscroll;
    let dir = doc.body_dir;
    let (stack, now) = {
        let t = app.view_store().txn(txn);
        (t.stack.clone(), app.now())
    };
    let txn_clone = app.view_store().txn(txn).clone();
    for row in 0..area.height as usize {
        let i = scroll + row;
        if i >= len {
            break;
        }
        let y = area.y + row as u16;
        let selected = focused && i == app.detail.cursor;
        let base = if selected { theme.selected() } else { Style::default() };
        if selected {
            for x in area.x..area.x + area.width {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_style(theme.selected());
                }
            }
        }
        app.hits.add(Rect { x: area.x, y, width: area.width, height: 1 }, Target::DetailLine(i));
        match doc.row(i) {
            Some(DocRow::Line(l)) => draw_line(buf, area.x, y, area.width, &l, hs, base),
            Some(DocRow::Body(bi)) => {
                if let Some(dir) = dir
                    && let Some(v) = app.body_view(txn, dir)
                {
                    let win = Window { skip: usize::from(hs), take: usize::from(area.width) };
                    let l = v.line(bi, parsed, &theme, win);
                    draw_line(buf, area.x, y, area.width, &l, 0, base);
                }
            }
            Some(DocRow::Frame { index }) => {
                if let Some(f) = stack.get(index) {
                    let app_frame = !is_framework(&f.c);
                    let loc = match (&f.f, f.l) {
                        (Some(file), Some(l)) => format!("({file}:{l})"),
                        (Some(file), None) => format!("({file})"),
                        _ => "(Unknown Source)".into(),
                    };
                    let style = if app_frame { theme.accent().add_modifier(Modifier::BOLD) } else { theme.dim() };
                    let l = Line::from(vec![
                        Span::styled(if app_frame { "  at " } else { "    at " }, theme.faint()),
                        Span::styled(format!("{}.{}", f.c, f.m), style),
                        Span::styled(loc, if app_frame { theme.text() } else { theme.faint() }),
                    ]);
                    draw_line(buf, area.x, y, area.width, &l, hs, base);
                }
            }
            Some(DocRow::FrameRun { run }) => {
                let mut j = run;
                while j < stack.len() && is_framework(&stack[j].c) {
                    j += 1;
                }
                let mut pkgs: Vec<String> = Vec::new();
                for f in &stack[run..j] {
                    let p = FRAMEWORK_PREFIXES
                        .iter()
                        .find(|p| f.c.starts_with(**p))
                        .map(|p| p.trim_end_matches('.').to_string())
                        .unwrap_or_default();
                    if !pkgs.contains(&p) {
                        pkgs.push(p);
                    }
                }
                let expanded = app.detail.expanded_runs.contains(&run);
                let n = j - run;
                let l = Line::from(vec![
                    Span::styled(if expanded { "  ▾ " } else { "  ▸ " }, theme.faint()),
                    Span::styled(
                        format!("{n} framework frame{} ({})", if n == 1 { "" } else { "s" }, pkgs.join(", ")),
                        theme.faint(),
                    ),
                    Span::styled(if expanded { "" } else { "  Enter expands" }, theme.faint()),
                ]);
                draw_line(buf, area.x, y, area.width, &l, 0, base);
            }
            Some(DocRow::TimingBar) => {
                draw_timing_bar(&theme, &txn_clone, now, buf, Rect { x: area.x, y, width: area.width, height: 1 })
            }
            Some(DocRow::Image { rows }) => {
                if let Some(dir) = dir {
                    let img = app.body_view(txn, dir).and_then(|v| v.image().and_then(|i| i.image.clone()));
                    if let Some(img) = img {
                        let avail = (area.y + area.height).saturating_sub(y).min(rows);
                        let r =
                            Rect { x: area.x + 1, y, width: area.width.saturating_sub(2).min(rows * 4), height: avail };
                        app.images.render(&img, r, buf);
                    }
                }
            }
            Some(DocRow::ImageCont) | None => {}
        }
        // search matches on this row, over what was drawn
        let first = hits.partition_point(|m| m.row < i);
        for (k, m) in hits.iter().enumerate().skip(first).take_while(|(_, m)| m.row == i) {
            let style = theme.search_hit(current_hit == Some(k));
            let from = m.start.saturating_sub(usize::from(hs));
            let to = m.end.saturating_sub(usize::from(hs)).min(usize::from(area.width));
            for x in from..to {
                if let Some(c) = buf.cell_mut((area.x + x as u16, y)) {
                    c.set_style(style);
                }
            }
        }
    }
    // JSON path of the cursor line
    if focused
        && let Some(DocRow::Body(bi)) = doc.row(app.detail.cursor)
        && let Some(dir) = dir
        && let Some(path) = app.body_view(txn, dir).and_then(|v| v.path_at(bi, parsed))
    {
        let y = area.y + area.height.saturating_sub(1);
        let text = format!(" {path} ");
        let w = (text.chars().count() as u16).min(area.width);
        let x = area.x + area.width - w;
        draw_line(
            buf,
            x,
            y,
            w,
            &Line::styled(text, theme.accent().add_modifier(Modifier::REVERSED)),
            0,
            Style::default(),
        );
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

fn draw_preview(app: &mut App, txn: TxnIdx, area: Rect, buf: &mut Buffer) {
    let theme = app.theme.clone();
    let dir = app.response_dir(txn);
    let has_body =
        app.view_store().txn(txn).resp_body.id.is_some() || app.view_store().txn(txn).delivered_body.is_some();
    if !has_body {
        let t = app.view_store().txn(txn);
        let msg = if let Some(f) = &t.failure {
            format!("{}: {}", f.class, f.message.clone().unwrap_or_default())
        } else if t.state.is_open() {
            "waiting for the response…".into()
        } else {
            "no response body".into()
        };
        draw_line(
            buf,
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            &Line::styled(msg, theme.dim()),
            0,
            Style::default(),
        );
        return;
    }
    let Some(view) = app.body_view(txn, dir) else {
        let msg = if app.body_decoding(txn, dir) { "decoding…" } else { "" };
        draw_line(
            buf,
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            &Line::styled(msg, theme.dim()),
            0,
            Style::default(),
        );
        return;
    };
    if let Some(img) = view.image().and_then(|i| i.image.clone()) {
        let r = Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: area.height };
        app.images.render(&img, r, buf);
        return;
    }
    let view: &BodyView = view;
    let n = view.len(true);
    for row in 0..area.height as usize {
        if row >= n {
            break;
        }
        let width = area.width.saturating_sub(2);
        let l = view.line(row, true, &theme, Window { skip: 0, take: usize::from(width) });
        draw_line(buf, area.x + 1, area.y + row as u16, width, &l, 0, Style::default());
    }
    if n > area.height as usize {
        let more = format!(" +{} lines · Response tab ", n - area.height as usize);
        let w = more.chars().count() as u16;
        draw_line(
            buf,
            area.x + area.width.saturating_sub(w + 1),
            area.y + area.height - 1,
            w,
            &Line::styled(more, theme.faint()),
            0,
            Style::default(),
        );
    }
}

/// Header value lookup for other modules.
pub fn content_type(t: &Transaction) -> Option<&str> {
    header(t.response_headers()?, "content-type")
}
