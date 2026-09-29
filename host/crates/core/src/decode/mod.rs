//! Body decoding: Content-Encoding, kind detection, and the per-kind viewers' models
//! (ARCHITECTURE.md §5.9).

pub mod doc;
pub mod encoding;
pub mod form;
pub mod hex;
pub mod json;
pub mod kind;
pub mod markup;
pub mod multipart;
pub mod protobuf;

use bytes::Bytes;

pub use doc::{StyledLine, Tok};
pub use kind::{BodyKind, ImageFormat};

use crate::model::{Headers, header};

/// A body after Content-Encoding decoding, with its detected kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// Bytes as captured (possibly encoded).
    pub raw_len: usize,
    /// Bytes after undoing Content-Encoding (the raw bytes if decoding failed).
    pub bytes: Bytes,
    pub encodings: Vec<String>,
    /// Why Content-Encoding decoding failed.
    pub error: Option<String>,
    pub kind: BodyKind,
    pub content_type: Option<String>,
}

/// Decode a captured body using the headers that describe it.
pub fn decode_body(raw: Bytes, headers: Option<&Headers>, limit: usize) -> Decoded {
    let encodings = headers.map(encoding::encodings).unwrap_or_default();
    let content_type = headers.and_then(|h| header(h, "content-type")).map(str::to_string);
    let (bytes, error) = if encodings.is_empty() || raw.is_empty() {
        (raw.clone(), None)
    } else {
        match encoding::decode(&raw, &encodings, limit) {
            Ok(v) => (Bytes::from(v), None),
            Err(e) => (raw.clone(), Some(e.to_string())),
        }
    };
    let kind = kind::detect(content_type.as_deref(), &bytes);
    Decoded { raw_len: raw.len(), bytes, encodings, error, kind, content_type }
}
