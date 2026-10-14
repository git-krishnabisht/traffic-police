//! A body prepared for display: decoded, parsed per kind, with fold state and jq output
//! (ARCHITECTURE.md §5.9). Lines are produced on demand for the visible window.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use ratatui::text::{Line, Span};
use traffic_police_core::decode::doc::Budget;
use traffic_police_core::decode::encoding::{self, DecodeError};
use traffic_police_core::decode::json::JsonDoc;
use traffic_police_core::decode::{self, BodyKind, Decoded, StyledLine, Tok, doc, hex, markup, multipart, protobuf};
use traffic_police_core::fmt;
use traffic_police_core::model::{Headers, header};

use crate::theme::Theme;
use crate::wrap::{self, Part, Text};

/// Decoded images are capped to keep a hostile body from exhausting memory.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Lines longer than this keep where they break (a minified body is one line of megabytes);
/// shorter ones are laid out as they are drawn.
const LONG_LINE: usize = 4096;

/// The most of a body (decoded) that is formatted: its first 32 MB. Formatting makes several
/// times its input in lines (and the formatters stop at their own [`Budget`] too), so up to
/// 0.5.1 a body that decoded to 256 MB could take gigabytes; the rest is left out and the view
/// says so (`w` saves the whole body). The parts of a multipart body decode to this much
/// together.
pub const VIEW_LIMIT: usize = 32 << 20;

#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub format: &'static str,
    pub width: u32,
    pub height: u32,
    pub image: Option<std::sync::Arc<image::DynamicImage>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Content {
    Json(Box<JsonDoc>),
    Lines(Vec<StyledLine>),
    /// Hex dump of the decoded bytes, rendered lazily.
    Hex,
    Image(ImageInfo),
    Empty,
}

#[derive(Debug, Clone)]
pub struct JqView {
    pub filter: String,
    pub lines: Vec<StyledLine>,
}

#[derive(Debug, Clone)]
pub struct BodyView {
    pub decoded: Decoded,
    pub parsed: Content,
    pub source: Content,
    /// Why the parsed view fell back (shown above the body).
    pub note: Option<String>,
    /// A view holds the start of the body only (longer than [`VIEW_LIMIT`], or more lines than
    /// a [`Budget`]): its last line says so.
    pub partial: bool,
    pub folded: HashSet<u32>,
    visible: Vec<u32>,
    pub jq: Option<JqView>,
    /// Where long lines break, by line: the width, and the rows.
    breaks: HashMap<LineKey, (usize, Arc<Vec<Part>>)>,
}

/// A line whose breaks are kept: jq output or not, the parsed view or the source, the line, and
/// whether it is folded.
type LineKey = (bool, bool, u32, bool);

/// A styled line as text to wrap (plain without a theme).
pub(crate) fn text_of<'a>(l: &'a StyledLine, theme: Option<&Theme>) -> Text<'a> {
    let (runs, base) = match theme {
        Some(t) => {
            (l.spans.iter().map(|&(s, e, tok)| (s as usize, e as usize, t.tok(tok))).collect(), t.tok(Tok::Plain))
        }
        None => (Vec::new(), Default::default()),
    };
    Text { text: Cow::Borrowed(&l.text), runs, base }
}

/// Where the first `limit` bytes of `b` end without cutting a character in two.
fn char_end(b: &[u8], limit: usize) -> usize {
    if b.len() <= limit {
        return b.len();
    }
    let mut end = limit;
    // back over continuation bytes (three at most) to where a character starts
    while end + 3 > limit && end > 0 && b[end] & 0xc0 == 0x80 {
        end -= 1;
    }
    end
}

/// The last line of a body longer than [`VIEW_LIMIT`].
fn rest_not_shown() -> StyledLine {
    StyledLine::styled("… the rest is not shown: the viewer formats the first 32 MB of a body", Tok::Meta)
}

/// What is formatted of one body: its start (cut at [`VIEW_LIMIT`]) within a [`Budget`].
struct Fmt {
    shown: Bytes,
    /// `shown` is the start of a longer body.
    cut: bool,
    budget: Budget,
    /// What was made leaves out some of the body.
    partial: bool,
}

impl Fmt {
    fn new(bytes: &Bytes, limit: usize) -> Fmt {
        let shown = bytes.slice(..char_end(bytes, limit));
        Fmt { cut: shown.len() < bytes.len(), shown, budget: Budget::default(), partial: false }
    }

    /// Formatted lines, ended with a note when the body went on past them.
    fn lines(&mut self, mut lines: Vec<StyledLine>) -> Content {
        if self.budget.spent() {
            // the formatter's last line says so
            self.partial = true;
        } else if self.cut {
            lines.push(rest_not_shown());
            self.partial = true;
        }
        Content::Lines(lines)
    }

    fn json(&mut self, mut d: JsonDoc) -> Content {
        if self.budget.spent() {
            self.partial = true;
        } else if self.cut {
            d.push_note(rest_not_shown());
            self.partial = true;
        }
        Content::Json(Box::new(d))
    }

    fn text_or_hex(&mut self) -> Content {
        let bytes = self.shown.clone();
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => Cow::Borrowed(s),
            Err(e) if e.valid_up_to() > 0 && bytes.len() - e.valid_up_to() < 4 => String::from_utf8_lossy(&bytes),
            Err(_) => return Content::Hex,
        };
        let lines = doc::text_lines_in(&text, &mut self.budget);
        self.lines(lines)
    }
}

/// Pushes `line` while the budget holds; the first that does not fit ends `out` with a note.
fn put(out: &mut Vec<StyledLine>, budget: &mut Budget, line: StyledLine) -> bool {
    let spent = budget.spent();
    if budget.take(line.text.len()) {
        out.push(line);
        return true;
    }
    if !spent {
        out.push(doc::cut_line());
    }
    false
}

fn image_info(bytes: &Bytes, format: decode::ImageFormat) -> ImageInfo {
    let name = format.name();
    if format == decode::ImageFormat::Svg || format == decode::ImageFormat::Avif {
        return ImageInfo {
            format: name,
            width: 0,
            height: 0,
            image: None,
            error: Some(format!("{name} preview is not supported")),
        };
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes.as_ref()));
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_BYTES);
    reader.limits(limits);
    let decoded =
        reader.with_guessed_format().map_err(|e| e.to_string()).and_then(|r| r.decode().map_err(|e| e.to_string()));
    match decoded {
        Ok(img) => ImageInfo {
            format: name,
            width: img.width(),
            height: img.height(),
            image: Some(std::sync::Arc::new(img)),
            error: None,
        },
        Err(e) => ImageInfo { format: name, width: 0, height: 0, image: None, error: Some(e) },
    }
}

fn pretty_json_lines(json: &[u8], budget: &mut Budget) -> Option<Vec<StyledLine>> {
    let before = *budget;
    match decode::json::parse_in(json, budget, false) {
        Ok(d) => Some(d.lines.into_iter().map(|l| l.line).collect()),
        Err(_) => {
            // what was made is dropped: the budget is as it was
            *budget = before;
            None
        }
    }
}

fn section(title: &str) -> StyledLine {
    StyledLine::styled(format!("── {title} ──"), Tok::Field)
}

/// A part of a multipart body: its title, headers and a nested preview (JSON, text and markup
/// inline, other kinds summarized). The parts share the budget, and decode to at most `left`
/// bytes together; false once either is spent.
fn part_lines(i: usize, p: &multipart::Part, out: &mut Vec<StyledLine>, budget: &mut Budget, left: &mut usize) -> bool {
    let (name, file) = p.disposition();
    let ct = header(&p.headers, "content-type").unwrap_or("text/plain");
    let size = fmt::bytes(p.body.len() as u64);
    let label = match (name, file) {
        (Some(n), Some(f)) => format!("part {}: {n} ({f}, {ct}, {size})", i + 1),
        (Some(n), None) => format!("part {}: {n} ({ct}, {size})", i + 1),
        _ => format!("part {} ({ct}, {size})", i + 1),
    };
    if !put(out, budget, section(&label)) {
        return false;
    }
    for (k, v) in &p.headers {
        let mut l = StyledLine::new();
        l.push(k, Tok::Meta);
        l.push(": ", Tok::Meta);
        l.push(v, Tok::Meta);
        if !put(out, budget, l) {
            return false;
        }
    }
    let encodings = encoding::encodings(&p.headers);
    let bytes = if encodings.is_empty() || p.body.is_empty() {
        p.body.clone()
    } else {
        match encoding::decode(&p.body, &encodings, *left) {
            Ok(v) => {
                *left -= v.len();
                Bytes::from(v)
            }
            Err(DecodeError::TooLarge(_)) => {
                let what = "… this part decodes to more than is left of the 32 MB the viewer formats of a body";
                put(out, budget, StyledLine::styled(what, Tok::Meta));
                return false;
            }
            // shown as sent
            Err(_) => p.body.clone(),
        }
    };
    let text = || String::from_utf8_lossy(&bytes).into_owned();
    match decode::kind::detect(header(&p.headers, "content-type"), &bytes) {
        BodyKind::Json => match pretty_json_lines(&bytes, budget) {
            Some(lines) => out.extend(lines),
            None => out.extend(doc::text_lines_in(&text(), budget)),
        },
        BodyKind::Xml => out.extend(markup::pretty_in(&text(), markup::Dialect::Xml, budget)),
        BodyKind::Html => out.extend(markup::pretty_in(&text(), markup::Dialect::Html, budget)),
        BodyKind::Text | BodyKind::Form => out.extend(doc::text_lines_in(&text(), budget)),
        BodyKind::Image(f) => {
            // the size from the image's header, without decoding its pixels
            let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes.as_ref()))
                .with_guessed_format()
                .ok()
                .and_then(|r| r.into_dimensions().ok())
                .unwrap_or_default();
            let line = format!("[{} image, {w}×{h}, {}]", f.name(), fmt::bytes(bytes.len() as u64));
            return put(out, budget, StyledLine::styled(line, Tok::Meta));
        }
        _ => {
            for row in 0..hex::line_count(bytes.len()).min(8) {
                if !put(out, budget, StyledLine::styled(hex::line(&bytes, row), Tok::Plain)) {
                    return false;
                }
            }
            if bytes.len() > 8 * hex::BYTES_PER_LINE {
                let line = StyledLine::styled(format!("… {} in total", fmt::bytes(bytes.len() as u64)), Tok::Meta);
                return put(out, budget, line);
            }
        }
    }
    !budget.spent()
}

/// The messages of a gRPC body (checked by [`protobuf::grpc_frames`]), each decoded.
fn grpc_lines(body: &[u8], budget: &mut Budget) -> Vec<StyledLine> {
    let mut out = Vec::new();
    for (i, frame) in protobuf::grpc_frames(body).enumerate() {
        let Ok((compressed, m)) = frame else { break };
        if !put(&mut out, budget, section(&format!("message {} ({})", i + 1, fmt::bytes(m.len() as u64)))) {
            break;
        }
        let shown = if compressed {
            put(&mut out, budget, StyledLine::styled("compressed with grpc-encoding; not decoded", Tok::Meta))
        } else {
            match protobuf::decode_raw_in(m, budget, false) {
                Ok(lines) => {
                    out.extend(lines);
                    !budget.spent()
                }
                Err(e) => put(&mut out, budget, StyledLine::styled(e, Tok::Error)),
            }
        };
        if !shown {
            break;
        }
    }
    out
}

impl BodyView {
    pub fn build(raw: Bytes, headers: Option<&Headers>) -> BodyView {
        Self::build_within(raw, headers, VIEW_LIMIT)
    }

    /// [`BodyView::build`], formatting the first `limit` bytes of the body.
    fn build_within(raw: Bytes, headers: Option<&Headers>, limit: usize) -> BodyView {
        let decoded = decode::decode_body(raw, headers, decode::encoding::DEFAULT_DECODE_LIMIT);
        let bytes = decoded.bytes.clone();
        let mut src = Fmt::new(&bytes, limit);
        let source = if bytes.is_empty() { Content::Empty } else { src.text_or_hex() };
        let mut note = decoded.error.clone();
        let mut f = Fmt::new(&bytes, limit);
        let parsed = if bytes.is_empty() {
            Content::Empty
        } else {
            match &decoded.kind {
                BodyKind::Json => {
                    let before = f.budget;
                    match decode::json::parse_in(&f.shown, &mut f.budget, f.cut) {
                        Ok(d) => f.json(d),
                        Err(e) => {
                            note = Some(format!("not valid JSON ({e}); showing the source"));
                            f.budget = before;
                            f.text_or_hex()
                        }
                    }
                }
                BodyKind::Xml => {
                    let lines =
                        markup::pretty_in(&String::from_utf8_lossy(&f.shown), markup::Dialect::Xml, &mut f.budget);
                    f.lines(lines)
                }
                BodyKind::Html => {
                    let lines =
                        markup::pretty_in(&String::from_utf8_lossy(&f.shown), markup::Dialect::Html, &mut f.budget);
                    f.lines(lines)
                }
                BodyKind::Form => {
                    let text = String::from_utf8_lossy(&f.shown).into_owned();
                    let mut pairs = Vec::new();
                    for (k, v) in decode::form::pairs(&text) {
                        if !f.budget.take(k.len() + v.len() + 3) {
                            break;
                        }
                        pairs.push((k, v));
                    }
                    let w = pairs.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0).min(32);
                    let mut lines: Vec<StyledLine> = pairs
                        .into_iter()
                        .map(|(k, v)| {
                            let mut l = StyledLine::new();
                            l.push(&format!("{k:<w$}"), Tok::Field);
                            l.push(" = ", Tok::Punct);
                            l.push(&v, Tok::Str);
                            l
                        })
                        .collect();
                    if f.budget.spent() {
                        lines.push(doc::cut_line());
                    }
                    f.lines(lines)
                }
                BodyKind::Multipart { boundary } => match multipart::parts(&f.shown, boundary) {
                    Some(parts) => {
                        let mut out = Vec::new();
                        let mut left = limit;
                        for (i, p) in parts.enumerate() {
                            if !part_lines(i, &p, &mut out, &mut f.budget, &mut left) {
                                // what is not shown is said: the budget's note, or the decoding's
                                f.partial = true;
                                break;
                            }
                        }
                        f.lines(out)
                    }
                    None => {
                        note = Some("multipart boundary not found; showing the source".into());
                        f.text_or_hex()
                    }
                },
                BodyKind::Image(img) => Content::Image(image_info(&bytes, *img)),
                BodyKind::Protobuf => match protobuf::decode_raw_in(&f.shown, &mut f.budget, f.cut) {
                    Ok(lines) => f.lines(lines),
                    Err(e) => {
                        note = Some(format!("{e}; showing hex"));
                        Content::Hex
                    }
                },
                BodyKind::Grpc => {
                    // the framing is checked whole first (in a body cut short, the message the cut
                    // goes through is where it ends)
                    let broken = protobuf::grpc_frames(&f.shown).find_map(Result::err);
                    match broken {
                        Some(e) if !f.cut => {
                            note = Some(format!("{e}; showing hex"));
                            Content::Hex
                        }
                        _ => {
                            let out = grpc_lines(&f.shown, &mut f.budget);
                            f.lines(out)
                        }
                    }
                }
                BodyKind::Text => f.text_or_hex(),
                BodyKind::Binary => Content::Hex,
            }
        };
        let mut v = BodyView {
            decoded,
            parsed,
            source,
            note,
            partial: f.partial || src.partial,
            folded: HashSet::new(),
            visible: Vec::new(),
            jq: None,
            breaks: HashMap::new(),
        };
        v.refold();
        v
    }

    fn refold(&mut self) {
        if let Content::Json(doc) = &self.parsed {
            self.visible = doc.visible_lines(&self.folded);
        }
    }

    /// Approximate heap memory this view holds (for the cache budget).
    pub fn mem_bytes(&self) -> usize {
        fn lines(ls: &[StyledLine]) -> usize {
            ls.iter().map(|l| l.text.len() + l.spans.len() * 12 + 48).sum()
        }
        fn content(c: &Content) -> usize {
            match c {
                Content::Json(d) => {
                    d.lines.iter().map(|l| l.line.text.len() + l.line.spans.len() * 12 + 64).sum::<usize>()
                        + std::mem::size_of_val(&d.nodes[..])
                }
                Content::Lines(ls) => lines(ls),
                Content::Image(i) => i.image.as_ref().map_or(0, |img| img.as_bytes().len()),
                Content::Hex | Content::Empty => 0,
            }
        }
        // decoded bytes share the captured buffer unless Content-Encoding was undone
        let decoded = if self.decoded.encodings.is_empty() { 0 } else { self.decoded.bytes.len() };
        decoded
            + content(&self.parsed)
            + content(&self.source)
            + self.visible.len() * 4
            + self.jq.as_ref().map_or(0, |j| lines(&j.lines))
            + 256
    }

    pub fn is_image(&self) -> bool {
        matches!(self.parsed, Content::Image(_))
    }

    pub fn image(&self) -> Option<&ImageInfo> {
        match &self.parsed {
            Content::Image(i) => Some(i),
            _ => None,
        }
    }

    fn content(&self, parsed: bool) -> &Content {
        if parsed { &self.parsed } else { &self.source }
    }

    /// Number of lines in the chosen view.
    pub fn len(&self, parsed: bool) -> usize {
        if parsed && let Some(jq) = &self.jq {
            return jq.lines.len();
        }
        match self.content(parsed) {
            Content::Json(_) => self.visible.len(),
            Content::Lines(l) => l.len(),
            Content::Hex => hex::line_count(self.decoded.bytes.len()),
            Content::Image(_) => 1,
            Content::Empty => 0,
        }
    }

    /// JSON path of the value on a visible line.
    pub fn path_at(&self, i: usize, parsed: bool) -> Option<String> {
        if !parsed || self.jq.is_some() {
            return None;
        }
        match &self.parsed {
            Content::Json(doc) => {
                let path = doc.lines[*self.visible.get(i)? as usize].path;
                // the line after a document cut short is no value
                (path != decode::json::NO_PATH).then(|| doc.path_string(path))
            }
            _ => None,
        }
    }

    /// For the body explorer: the container visible line `i` opens (parsed JSON, no jq result),
    /// and whether it is folded.
    pub fn container_at(&self, i: usize) -> Option<(u32, bool)> {
        if self.jq.is_some() {
            return None;
        }
        let Content::Json(doc) = &self.parsed else { return None };
        let node = doc.lines[*self.visible.get(i)? as usize].opens?;
        Some((node, self.folded.contains(&node)))
    }

    /// For the body explorer: the visible line that opens the container holding visible line
    /// `i` (for a closing line, the line that opened it).
    pub fn parent_line(&self, i: usize) -> Option<usize> {
        if self.jq.is_some() {
            return None;
        }
        let Content::Json(doc) = &self.parsed else { return None };
        let line = *self.visible.get(i)?;
        if let Some(n) = doc.nodes.iter().find(|n| n.close_line == line && n.open_line != line) {
            return self.visible.iter().position(|&l| l == n.open_line);
        }
        let depth = doc.lines[line as usize].depth;
        (0..i).rev().find(|&j| {
            let l = &doc.lines[self.visible[j] as usize];
            l.depth < depth && l.opens.is_some()
        })
    }

    /// Folds (`fold`) or unfolds the container opening on visible line `i`; returns whether
    /// anything changed.
    pub fn set_fold(&mut self, i: usize, fold: bool) -> bool {
        match self.container_at(i) {
            Some((_, folded)) if folded != fold => self.toggle_fold(i),
            _ => false,
        }
    }

    /// Toggle the fold of the container opening on visible line `i`; returns whether it was one.
    pub fn toggle_fold(&mut self, i: usize) -> bool {
        let node = match &self.parsed {
            Content::Json(doc) => self.visible.get(i).and_then(|&l| doc.lines[l as usize].opens),
            _ => None,
        };
        match node {
            Some(n) => {
                if !self.folded.remove(&n) {
                    self.folded.insert(n);
                }
                self.refold();
                true
            }
            None => false,
        }
    }

    pub fn fold_all(&mut self) {
        if let Content::Json(doc) = &self.parsed {
            self.folded = doc.all_nodes().collect();
            self.refold();
        }
    }

    pub fn unfold_all(&mut self) {
        self.folded.clear();
        self.refold();
    }

    /// Render visible line `i` of the chosen view, only the columns in `win`.
    pub fn line(&self, i: usize, parsed: bool, theme: &Theme, win: Window) -> Line<'static> {
        if parsed && let Some(jq) = &self.jq {
            return jq.lines.get(i).map(|l| styled_window(l, theme, win).0).unwrap_or_default();
        }
        match self.content(parsed) {
            Content::Json(doc) => {
                let Some(&li) = self.visible.get(i) else { return Line::default() };
                let jl = &doc.lines[li as usize];
                let (mut line, whole) = styled_window(&jl.line, theme, win);
                if let Some(n) = jl.opens
                    && self.folded.contains(&n)
                    && whole
                {
                    let node = doc.nodes[n as usize];
                    line.spans.push(Span::styled("…", theme.dim()));
                    line.spans.push(Span::styled(if node.is_array { "]" } else { "}" }, theme.tok(Tok::Punct)));
                    if doc.close_has_comma(n) {
                        line.spans.push(Span::styled(",", theme.tok(Tok::Punct)));
                    }
                    let what = if node.is_array { "item" } else { "key" };
                    let s = if node.len == 1 { "" } else { "s" };
                    line.spans.push(Span::styled(format!("  {} {what}{s}", node.len), theme.faint()));
                }
                line
            }
            Content::Lines(lines) => lines.get(i).map(|l| styled_window(l, theme, win).0).unwrap_or_default(),
            Content::Hex => {
                let text = hex::line(&self.decoded.bytes, i);
                styled_window(&StyledLine::plain(text), theme, win).0
            }
            Content::Image(info) => match &info.error {
                Some(e) => Line::styled(format!("{} image: {e}", info.format), theme.warn()),
                None => Line::styled(format!("{} image, {}×{}", info.format, info.width, info.height), theme.dim()),
            },
            Content::Empty => Line::default(),
        }
    }

    /// Visible line `i` as text to wrap (styled with `theme`; plain without): a folded object
    /// or array says after it what it holds, as [`BodyView::line`] draws it.
    pub fn text(&self, i: usize, parsed: bool, theme: Option<&Theme>) -> Text<'_> {
        let plain = theme.map(|t| t.tok(Tok::Plain)).unwrap_or_default();
        if parsed && let Some(jq) = &self.jq {
            return jq.lines.get(i).map(|l| text_of(l, theme)).unwrap_or_default();
        }
        match self.content(parsed) {
            Content::Json(doc) => {
                let Some(&li) = self.visible.get(i) else { return Text::default() };
                let jl = &doc.lines[li as usize];
                let mut text = text_of(&jl.line, theme);
                if let Some(n) = jl.opens
                    && self.folded.contains(&n)
                {
                    let node = doc.nodes[n as usize];
                    let what = if node.is_array { "item" } else { "key" };
                    let s = if node.len == 1 { "" } else { "s" };
                    let style = |f: fn(&Theme) -> ratatui::style::Style| theme.map(f).unwrap_or_default();
                    let mut more = vec![
                        ("…".to_string(), style(Theme::dim)),
                        (
                            (if node.is_array { "]" } else { "}" }).to_string(),
                            theme.map(|t| t.tok(Tok::Punct)).unwrap_or_default(),
                        ),
                    ];
                    if doc.close_has_comma(n) {
                        more.push((",".to_string(), theme.map(|t| t.tok(Tok::Punct)).unwrap_or_default()));
                    }
                    more.push((format!("  {} {what}{s}", node.len), style(Theme::faint)));
                    let mut owned = text.text.into_owned();
                    for (piece, st) in more {
                        let start = owned.len();
                        owned.push_str(&piece);
                        text.runs.push((start, owned.len(), st));
                    }
                    text.text = Cow::Owned(owned);
                }
                text
            }
            Content::Lines(lines) => lines.get(i).map(|l| text_of(l, theme)).unwrap_or_default(),
            Content::Hex => Text { text: Cow::Owned(hex::line(&self.decoded.bytes, i)), runs: Vec::new(), base: plain },
            Content::Image(_) => match theme {
                Some(t) => Text::from_line(&self.line(i, parsed, t, Window::ALL)),
                None => Text::default(),
            },
            Content::Empty => Text::default(),
        }
    }

    /// Where visible line `i` breaks at `width` (`[ui] wrap`): its later rows start two columns
    /// past its indent. Long lines keep their breaks.
    pub fn breaks(&mut self, i: usize, parsed: bool, width: usize) -> Arc<Vec<Part>> {
        let key = self.line_key(i, parsed);
        if let Some((w, b)) = self.breaks.get(&key)
            && *w == width
        {
            return b.clone();
        }
        let text = self.text(i, parsed, None);
        let b = Arc::new(wrap::layout(&text.text, width, text.leading() + 2, false));
        if text.text.len() > LONG_LINE {
            if self.breaks.len() >= 64 {
                self.breaks.clear();
            }
            self.breaks.insert(key, (width, b.clone()));
        }
        b
    }

    /// Rows `from..from + n` of visible line `i` wrapped at `width`, drawn, with where each one
    /// is in the line.
    pub fn wrapped(
        &mut self,
        i: usize,
        parsed: bool,
        theme: &Theme,
        width: usize,
        from: usize,
        n: usize,
    ) -> Vec<(Line<'static>, Part)> {
        let breaks = self.breaks(i, parsed, width);
        let text = self.text(i, parsed, Some(theme));
        breaks.iter().skip(from).take(n).map(|p| (text.part(p), *p)).collect()
    }

    /// Without wrapping: columns `skip..skip + take` of visible line `i`, with where they are.
    pub fn window(&self, i: usize, parsed: bool, theme: &Theme, skip: usize, take: usize) -> (Line<'static>, Part) {
        let text = self.text(i, parsed, Some(theme));
        let p = text.window(skip, take);
        (text.part(&p), p)
    }

    /// What a line's breaks are kept under: the jq output, the view and the line, folded or not.
    fn line_key(&self, i: usize, parsed: bool) -> LineKey {
        if parsed && self.jq.is_some() {
            return (true, parsed, i as u32, false);
        }
        match self.content(parsed) {
            Content::Json(doc) => {
                let li = self.visible.get(i).copied().unwrap_or(u32::MAX);
                let folded = doc.lines.get(li as usize).and_then(|l| l.opens).is_some_and(|n| self.folded.contains(&n));
                (false, parsed, li, folded)
            }
            _ => (false, parsed, i as u32, false),
        }
    }

    /// Visible line `i` as plain text, for search (columns match [`BodyView::line`]).
    pub fn plain(&self, i: usize, parsed: bool) -> String {
        if parsed && let Some(jq) = &self.jq {
            return jq.lines.get(i).map(|l| l.text.clone()).unwrap_or_default();
        }
        match self.content(parsed) {
            Content::Json(doc) => {
                self.visible.get(i).map(|&l| doc.lines[l as usize].line.text.clone()).unwrap_or_default()
            }
            Content::Lines(lines) => lines.get(i).map(|l| l.text.clone()).unwrap_or_default(),
            Content::Hex => hex::line(&self.decoded.bytes, i),
            Content::Image(_) | Content::Empty => String::new(),
        }
    }

    /// Replace the parsed view with jq outputs (`Ok`) or an error message (`Err`).
    pub fn set_jq(&mut self, filter: String, result: Result<(Vec<String>, bool), String>) {
        let lines = match result {
            Ok((values, truncated)) => {
                let mut out = Vec::new();
                if values.is_empty() {
                    out.push(StyledLine::styled("(no output)", Tok::Meta));
                }
                // the outputs (up to 200 of them) share one budget
                let mut budget = Budget::default();
                for (i, v) in values.iter().enumerate() {
                    if values.len() > 1 && !put(&mut out, &mut budget, section(&format!("output {}", i + 1))) {
                        break;
                    }
                    match pretty_json_lines(v.as_bytes(), &mut budget) {
                        Some(lines) => out.extend(lines),
                        None => {
                            if !put(&mut out, &mut budget, StyledLine::plain(v.clone())) {
                                break;
                            }
                        }
                    }
                    if budget.spent() {
                        break;
                    }
                }
                if truncated {
                    out.push(StyledLine::styled("… more outputs not shown", Tok::Meta));
                }
                out
            }
            Err(e) => vec![StyledLine::styled(e, Tok::Error)],
        };
        self.jq = Some(JqView { filter, lines });
        self.breaks.clear();
    }

    pub fn clear_jq(&mut self) {
        self.jq = None;
        self.breaks.clear();
    }

    /// One-line description: kind, sizes, encoding.
    pub fn summary(&self) -> String {
        let d = &self.decoded;
        let mut s = d.kind.label().to_string();
        if let Some(img) = self.image()
            && img.error.is_none()
        {
            s = format!("{} image {}×{}", img.format, img.width, img.height);
        }
        if d.encodings.is_empty() || d.error.is_some() {
            s.push_str(&format!(", {}", fmt::bytes(d.raw_len as u64)));
        } else {
            s.push_str(&format!(
                ", {} transferred ({}), {} decoded",
                fmt::bytes(d.raw_len as u64),
                d.encodings.join(", "),
                fmt::bytes(d.bytes.len() as u64)
            ));
        }
        s
    }
}

pub fn styled(l: &StyledLine, theme: &Theme) -> Line<'static> {
    Line::from(l.pieces().into_iter().map(|(t, tok)| Span::styled(t.to_string(), theme.tok(tok))).collect::<Vec<_>>())
}

/// The columns of a line to materialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Columns scrolled off to the left.
    pub skip: usize,
    /// Columns that fit.
    pub take: usize,
}

impl Window {
    pub const ALL: Window = Window { skip: 0, take: usize::MAX };
}

/// Like [`styled`], but copies only the columns in `win`, so a multi-megabyte line (minified
/// JSON, a long text line) costs what fits on screen. Also returns whether the whole rest of the
/// line fit.
pub fn styled_window(l: &StyledLine, theme: &Theme, win: Window) -> (Line<'static>, bool) {
    let end = win.skip.saturating_add(win.take);
    let mut spans = Vec::new();
    let mut col = 0usize;
    for (text, tok) in l.pieces() {
        if col >= end {
            return (Line::from(spans), false);
        }
        let mut s = String::new();
        for ch in text.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + w > end {
                col = end;
                break;
            }
            if col >= win.skip {
                s.push(ch);
            }
            col += w;
        }
        if !s.is_empty() {
            spans.push(Span::styled(s, theme.tok(tok)));
        }
    }
    let whole = col < end;
    (Line::from(spans), whole)
}

#[cfg(test)]
mod tests {
    use super::*;
    use traffic_police_backends::demo::content::gzip;

    fn hs(pairs: &[(&str, &str)]) -> Headers {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn texts(v: &BodyView, parsed: bool) -> Vec<String> {
        (0..v.len(parsed)).map(|i| v.plain(i, parsed)).collect()
    }

    /// A body longer than the viewer formats: its start, cut where a character ends, then a line
    /// that says the rest is not shown. The decoded bytes stay whole (for hex, jq and saving).
    #[test]
    fn a_long_body_shows_its_start_and_says_so() {
        let text = "é".repeat(600);
        let plain = hs(&[("content-type", "text/plain")]);
        let v = BodyView::build_within(Bytes::from(text.clone()), Some(&plain), 1001);
        assert!(v.partial);
        for parsed in [true, false] {
            assert_eq!(texts(&v, parsed), ["é".repeat(500), rest_not_shown().text]);
        }
        assert_eq!(v.decoded.bytes.len(), 1200);
        let v = BodyView::build_within(Bytes::from(text), Some(&plain), 1200);
        assert!(!v.partial);
        assert_eq!(texts(&v, true).len(), 1);
        // bytes that are no text are a hex dump of all of them, which is never cut
        let v = BodyView::build_within(Bytes::from(vec![0xffu8; 5000]), None, 1000);
        assert!(!v.partial && matches!(v.parsed, Content::Hex));
        assert_eq!(v.len(true), 5000_usize.div_ceil(16));
    }

    #[test]
    fn a_long_json_body_is_shown_up_to_the_cut() {
        let body = format!("{{\"items\":[{}]}}", vec![r#"{"id":1,"name":"x"}"#; 100].join(","));
        let json = hs(&[("content-type", "application/json")]);
        let mut v = BodyView::build_within(Bytes::from(body.clone()), Some(&json), 500);
        assert!(v.partial && v.note.is_none(), "{:?}", v.note);
        let lines = texts(&v, true);
        assert_eq!(lines.last(), Some(&rest_not_shown().text));
        assert_eq!(v.path_at(lines.len() - 1, true), None, "the note is no value");
        assert_eq!(v.path_at(3, true).as_deref(), Some("$.items[0].id"));
        // what was shown still folds
        assert!(v.toggle_fold(1));
        assert_eq!(texts(&v, true)[1], "  \"items\": [");
        assert_eq!(v.len(true), 3);
        let v = BodyView::build_within(Bytes::from(body), Some(&json), 1 << 20);
        assert!(!v.partial);
    }

    /// Each part used to decode up to 256 MB on its own; the parts now share the viewer's limit.
    #[test]
    fn multipart_parts_decode_to_the_limit_together() {
        let part = |name: &str| {
            let mut p = format!(
                "--b\r\nContent-Disposition: form-data; name=\"{name}\"\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\n\r\n"
            )
            .into_bytes();
            p.extend_from_slice(&gzip(&[b'x'; 600]));
            p.extend_from_slice(b"\r\n");
            p
        };
        let body = [part("a"), part("b"), b"--b--\r\n".to_vec()].concat();
        let multipart = hs(&[("content-type", "multipart/form-data; boundary=b")]);
        let v = BodyView::build_within(Bytes::from(body.clone()), Some(&multipart), 1000);
        assert!(v.partial);
        let lines = texts(&v, true);
        assert!(lines[0].starts_with("── part 1: a"), "{lines:#?}");
        assert_eq!(lines[4], "x".repeat(600));
        assert!(lines[5].starts_with("── part 2: b"), "{lines:#?}");
        assert!(lines.last().unwrap().contains("decodes to more than is left"), "{lines:#?}");
        let v = BodyView::build_within(Bytes::from(body), Some(&multipart), 2000);
        assert!(!v.partial);
        assert_eq!(texts(&v, true).len(), 10);
    }

    #[test]
    fn long_protobuf_and_grpc_bodies_show_what_comes_before_the_cut() {
        let grpc = hs(&[("content-type", "application/grpc")]);
        let body = [0u8, 0, 0, 0, 3, 0x08, 0x96, 0x01].repeat(3);
        let v = BodyView::build_within(Bytes::from(body.clone()), Some(&grpc), 20);
        assert!(v.partial && v.note.is_none(), "{:?}", v.note);
        assert_eq!(
            texts(&v, true),
            ["── message 1 (3 B) ──", "1: 150", "── message 2 (3 B) ──", "1: 150", &rest_not_shown().text]
        );
        let v = BodyView::build_within(Bytes::from(body.clone()), Some(&grpc), 1000);
        assert!(!v.partial);
        assert_eq!(texts(&v, true).len(), 6);
        // framing that is broken, in a body that is not cut, is shown as hex
        let v = BodyView::build_within(Bytes::from(body[..20].to_vec()), Some(&grpc), 1000);
        assert!(matches!(v.parsed, Content::Hex) && v.note.is_some());

        let protobuf = hs(&[("content-type", "application/x-protobuf")]);
        let body = [0x08u8, 0x96, 0x01].repeat(10);
        let v = BodyView::build_within(Bytes::from(body), Some(&protobuf), 10);
        assert!(v.partial);
        assert_eq!(texts(&v, true), ["1: 150", "1: 150", "1: 150", &rest_not_shown().text]);
    }
}
