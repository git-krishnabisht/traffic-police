//! What kind of body this is, from Content-Type first and magic bytes second.

use crate::model::mime_essence;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Ico,
    Avif,
    Svg,
}

impl ImageFormat {
    pub fn name(self) -> &'static str {
        match self {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPEG",
            ImageFormat::Gif => "GIF",
            ImageFormat::Webp => "WebP",
            ImageFormat::Bmp => "BMP",
            ImageFormat::Ico => "ICO",
            ImageFormat::Avif => "AVIF",
            ImageFormat::Svg => "SVG",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Gif => "gif",
            ImageFormat::Webp => "webp",
            ImageFormat::Bmp => "bmp",
            ImageFormat::Ico => "ico",
            ImageFormat::Avif => "avif",
            ImageFormat::Svg => "svg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyKind {
    Json,
    Xml,
    Html,
    Form,
    Multipart { boundary: String },
    Image(ImageFormat),
    Protobuf,
    Grpc,
    Text,
    Binary,
}

impl BodyKind {
    pub fn label(&self) -> &'static str {
        match self {
            BodyKind::Json => "JSON",
            BodyKind::Xml => "XML",
            BodyKind::Html => "HTML",
            BodyKind::Form => "form",
            BodyKind::Multipart { .. } => "multipart",
            BodyKind::Image(_) => "image",
            BodyKind::Protobuf => "protobuf",
            BodyKind::Grpc => "gRPC",
            BodyKind::Text => "text",
            BodyKind::Binary => "binary",
        }
    }
}

pub fn sniff_image(b: &[u8]) -> Option<ImageFormat> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if b.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else if b.starts_with(b"BM") && b.len() > 14 {
        Some(ImageFormat::Bmp)
    } else if b.starts_with(&[0, 0, 1, 0]) {
        Some(ImageFormat::Ico)
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" && (&b[8..12] == b"avif" || &b[8..12] == b"avis") {
        Some(ImageFormat::Avif)
    } else {
        None
    }
}

fn param<'a>(content_type: &'a str, name: &str) -> Option<&'a str> {
    content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim().trim_matches('"'))
    })
}

/// Charset parameter of a Content-Type.
pub fn charset(content_type: &str) -> Option<&str> {
    param(content_type, "charset")
}

fn looks_like_text(b: &[u8]) -> bool {
    let sample = &b[..b.len().min(4096)];
    match std::str::from_utf8(sample) {
        Ok(s) => {
            s.chars().filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')).count() * 20 <= s.chars().count()
        }
        // a multi-byte char cut at the sample edge is fine
        Err(e) => e.valid_up_to() + 4 >= sample.len() && sample.len() == 4096,
    }
}

pub fn detect(content_type: Option<&str>, body: &[u8]) -> BodyKind {
    if let Some(ct) = content_type {
        let essence = mime_essence(ct);
        let (top, sub) = essence.split_once('/').unwrap_or((essence.as_str(), ""));
        let kind = match (top, sub) {
            (_, s) if s == "json" || s.ends_with("+json") || s == "x-ndjson" => Some(BodyKind::Json),
            ("text", "html") | ("application", "xhtml+xml") => Some(BodyKind::Html),
            ("image", "svg+xml") => Some(BodyKind::Image(ImageFormat::Svg)),
            (_, s) if s == "xml" || s.ends_with("+xml") => Some(BodyKind::Xml),
            ("application", "x-www-form-urlencoded") => Some(BodyKind::Form),
            ("multipart", _) => param(ct, "boundary").map(|b| BodyKind::Multipart { boundary: b.to_string() }),
            ("application", s) if s.starts_with("grpc") => Some(BodyKind::Grpc),
            ("application", "x-protobuf" | "protobuf" | "vnd.google.protobuf" | "x-google-protobuf") => {
                Some(BodyKind::Protobuf)
            }
            ("image", _) => sniff_image(body).map(BodyKind::Image),
            ("text", _) => Some(BodyKind::Text),
            ("application", "javascript" | "ecmascript" | "x-javascript") => Some(BodyKind::Text),
            _ => None,
        };
        if let Some(k) = kind {
            return k;
        }
    }
    if let Some(img) = sniff_image(body) {
        return BodyKind::Image(img);
    }
    let trimmed = body.iter().position(|b| !b.is_ascii_whitespace()).map(|i| &body[i..]).unwrap_or(&[]);
    if (trimmed.starts_with(b"{") || trimmed.starts_with(b"[")) && crate::decode::json::parse(body).is_ok() {
        return BodyKind::Json;
    }
    if trimmed.starts_with(b"<?xml") {
        return BodyKind::Xml;
    }
    if trimmed.len() >= 5
        && (trimmed[..5].eq_ignore_ascii_case(b"<!doc") || trimmed[..5].eq_ignore_ascii_case(b"<html"))
    {
        return BodyKind::Html;
    }
    if body.is_empty() || looks_like_text(body) { BodyKind::Text } else { BodyKind::Binary }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_type_then_sniff() {
        assert_eq!(detect(Some("application/json; charset=utf-8"), b"{}"), BodyKind::Json);
        assert_eq!(detect(Some("application/problem+json"), b"{}"), BodyKind::Json);
        assert_eq!(
            detect(Some("multipart/form-data; boundary=\"abc\""), b""),
            BodyKind::Multipart { boundary: "abc".into() }
        );
        assert_eq!(detect(Some("application/grpc+proto"), b""), BodyKind::Grpc);
        assert_eq!(detect(Some("image/png"), b"\x89PNG\r\n\x1a\n...."), BodyKind::Image(ImageFormat::Png));
        assert_eq!(detect(None, br#"  {"a":1}"#), BodyKind::Json);
        assert_eq!(detect(None, b"<!DOCTYPE html><html></html>"), BodyKind::Html);
        assert_eq!(detect(Some("application/octet-stream"), &[0, 1, 2, 3, 200, 201]), BodyKind::Binary);
        assert_eq!(detect(None, b"plain words\n"), BodyKind::Text);
        assert_eq!(charset("text/plain; charset=\"ISO-8859-1\""), Some("ISO-8859-1"));
    }
}
