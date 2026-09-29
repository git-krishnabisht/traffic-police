//! A body prepared for display: decoded, parsed per kind, with fold state and jq output
//! (ARCHITECTURE.md §5.9). Lines are produced on demand for the visible window.

use std::collections::HashSet;

use bytes::Bytes;
use ratatui::text::{Line, Span};
use traffic_police_core::decode::json::JsonDoc;
use traffic_police_core::decode::{self, BodyKind, Decoded, StyledLine, Tok, doc, hex, markup, multipart, protobuf};
use traffic_police_core::fmt;
use traffic_police_core::model::Headers;

use crate::theme::Theme;

/// Decoded images are capped to keep a hostile body from exhausting memory.
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;

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
    pub folded: HashSet<u32>,
    visible: Vec<u32>,
    pub jq: Option<JqView>,
}

fn text_or_hex(bytes: &Bytes) -> Content {
    match std::str::from_utf8(bytes) {
        Ok(s) => Content::Lines(doc::text_lines(s)),
        Err(e) if e.valid_up_to() > 0 && bytes.len() - e.valid_up_to() < 4 => {
            Content::Lines(doc::text_lines(&String::from_utf8_lossy(bytes)))
        }
        Err(_) => Content::Hex,
    }
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

fn pretty_json_lines(json: &[u8]) -> Option<Vec<StyledLine>> {
    decode::json::parse(json).ok().map(|d| d.lines.into_iter().map(|l| l.line).collect())
}

fn section(title: &str) -> StyledLine {
    StyledLine::styled(format!("── {title} ──"), Tok::Field)
}

/// A nested preview (multipart parts): JSON/text/markup inline, other kinds summarized.
fn preview_lines(body: &Bytes, headers: &Headers, out: &mut Vec<StyledLine>) {
    let d = decode::decode_body(body.clone(), Some(headers), decode::encoding::DEFAULT_DECODE_LIMIT);
    match &d.kind {
        BodyKind::Json => match pretty_json_lines(&d.bytes) {
            Some(lines) => out.extend(lines),
            None => out.extend(doc::text_lines(&String::from_utf8_lossy(&d.bytes))),
        },
        BodyKind::Xml => out.extend(markup::pretty(&String::from_utf8_lossy(&d.bytes), markup::Dialect::Xml)),
        BodyKind::Html => out.extend(markup::pretty(&String::from_utf8_lossy(&d.bytes), markup::Dialect::Html)),
        BodyKind::Text | BodyKind::Form => out.extend(doc::text_lines(&String::from_utf8_lossy(&d.bytes))),
        BodyKind::Image(f) => {
            let info = image_info(&d.bytes, *f);
            out.push(StyledLine::styled(
                format!(
                    "[{} image, {}×{}, {}]",
                    info.format,
                    info.width,
                    info.height,
                    fmt::bytes(d.bytes.len() as u64)
                ),
                Tok::Meta,
            ));
        }
        _ => {
            for row in 0..hex::line_count(d.bytes.len()).min(8) {
                out.push(StyledLine::styled(hex::line(&d.bytes, row), Tok::Plain));
            }
            if d.bytes.len() > 8 * hex::BYTES_PER_LINE {
                out.push(StyledLine::styled(format!("… {} in total", fmt::bytes(d.bytes.len() as u64)), Tok::Meta));
            }
        }
    }
}

impl BodyView {
    pub fn build(raw: Bytes, headers: Option<&Headers>) -> BodyView {
        let decoded = decode::decode_body(raw, headers, decode::encoding::DEFAULT_DECODE_LIMIT);
        let bytes = decoded.bytes.clone();
        let source = if bytes.is_empty() { Content::Empty } else { text_or_hex(&bytes) };
        let mut note = decoded.error.clone();
        let parsed = if bytes.is_empty() {
            Content::Empty
        } else {
            match &decoded.kind {
                BodyKind::Json => match decode::json::parse(&bytes) {
                    Ok(d) => Content::Json(Box::new(d)),
                    Err(e) => {
                        note = Some(format!("not valid JSON ({e}); showing the source"));
                        text_or_hex(&bytes)
                    }
                },
                BodyKind::Xml => Content::Lines(markup::pretty(&String::from_utf8_lossy(&bytes), markup::Dialect::Xml)),
                BodyKind::Html => {
                    Content::Lines(markup::pretty(&String::from_utf8_lossy(&bytes), markup::Dialect::Html))
                }
                BodyKind::Form => {
                    let pairs = decode::form::parse_pairs(&String::from_utf8_lossy(&bytes));
                    let w = pairs.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0).min(32);
                    Content::Lines(
                        pairs
                            .into_iter()
                            .map(|(k, v)| {
                                let mut l = StyledLine::new();
                                l.push(&format!("{k:<w$}"), Tok::Field);
                                l.push(" = ", Tok::Punct);
                                l.push(&v, Tok::Str);
                                l
                            })
                            .collect(),
                    )
                }
                BodyKind::Multipart { boundary } => match multipart::parse(&bytes, boundary) {
                    Some(parts) => {
                        let mut out = Vec::new();
                        for (i, p) in parts.iter().enumerate() {
                            let (name, file) = p.disposition();
                            let ct =
                                traffic_police_core::model::header(&p.headers, "content-type").unwrap_or("text/plain");
                            let label = match (name, file) {
                                (Some(n), Some(f)) => {
                                    format!("part {}: {n} ({f}, {ct}, {})", i + 1, fmt::bytes(p.body.len() as u64))
                                }
                                (Some(n), None) => {
                                    format!("part {}: {n} ({ct}, {})", i + 1, fmt::bytes(p.body.len() as u64))
                                }
                                _ => format!("part {} ({ct}, {})", i + 1, fmt::bytes(p.body.len() as u64)),
                            };
                            out.push(section(&label));
                            for (k, v) in &p.headers {
                                let mut l = StyledLine::new();
                                l.push(k, Tok::Meta);
                                l.push(": ", Tok::Meta);
                                l.push(v, Tok::Meta);
                                out.push(l);
                            }
                            preview_lines(&p.body, &p.headers, &mut out);
                        }
                        Content::Lines(out)
                    }
                    None => {
                        note = Some("multipart boundary not found; showing the source".into());
                        text_or_hex(&bytes)
                    }
                },
                BodyKind::Image(f) => Content::Image(image_info(&bytes, *f)),
                BodyKind::Protobuf => match protobuf::decode_raw(&bytes) {
                    Ok(lines) => Content::Lines(lines),
                    Err(e) => {
                        note = Some(format!("{e}; showing hex"));
                        Content::Hex
                    }
                },
                BodyKind::Grpc => match protobuf::grpc_messages(&bytes) {
                    Ok(msgs) => {
                        let mut out = Vec::new();
                        for (i, (compressed, m)) in msgs.iter().enumerate() {
                            out.push(section(&format!("message {} ({})", i + 1, fmt::bytes(m.len() as u64))));
                            if *compressed {
                                out.push(StyledLine::styled("compressed with grpc-encoding; not decoded", Tok::Meta));
                            } else {
                                match protobuf::decode_raw(m) {
                                    Ok(lines) => out.extend(lines),
                                    Err(e) => out.push(StyledLine::styled(e, Tok::Error)),
                                }
                            }
                        }
                        Content::Lines(out)
                    }
                    Err(e) => {
                        note = Some(format!("{e}; showing hex"));
                        Content::Hex
                    }
                },
                BodyKind::Text => text_or_hex(&bytes),
                BodyKind::Binary => Content::Hex,
            }
        };
        let mut v = BodyView { decoded, parsed, source, note, folded: HashSet::new(), visible: Vec::new(), jq: None };
        v.refold();
        v
    }

    fn refold(&mut self) {
        if let Content::Json(doc) = &self.parsed {
            self.visible = doc.visible_lines(&self.folded);
        }
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
            Content::Json(doc) => self.visible.get(i).map(|&l| doc.path_string(doc.lines[l as usize].path)),
            _ => None,
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

    /// Render visible line `i` of the chosen view.
    pub fn line(&self, i: usize, parsed: bool, theme: &Theme) -> Line<'static> {
        if parsed && let Some(jq) = &self.jq {
            return jq.lines.get(i).map(|l| styled(l, theme)).unwrap_or_default();
        }
        match self.content(parsed) {
            Content::Json(doc) => {
                let Some(&li) = self.visible.get(i) else { return Line::default() };
                let jl = &doc.lines[li as usize];
                let mut line = styled(&jl.line, theme);
                if let Some(n) = jl.opens
                    && self.folded.contains(&n)
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
            Content::Lines(lines) => lines.get(i).map(|l| styled(l, theme)).unwrap_or_default(),
            Content::Hex => Line::styled(hex::line(&self.decoded.bytes, i), theme.text()),
            Content::Image(info) => match &info.error {
                Some(e) => Line::styled(format!("{} image: {e}", info.format), theme.warn()),
                None => Line::styled(format!("{} image, {}×{}", info.format, info.width, info.height), theme.dim()),
            },
            Content::Empty => Line::default(),
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
                for (i, v) in values.iter().enumerate() {
                    if values.len() > 1 {
                        out.push(section(&format!("output {}", i + 1)));
                    }
                    match pretty_json_lines(v.as_bytes()) {
                        Some(lines) => out.extend(lines),
                        None => out.push(StyledLine::plain(v.clone())),
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
    }

    pub fn clear_jq(&mut self) {
        self.jq = None;
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
