//! Content-Encoding decoding (gzip, deflate, br, zstd), all pure Rust, with an output cap.

use std::io::Read;

use ruzstd::decoding::errors::FrameDecoderError;

use crate::model::{Headers, header_all};

/// Default cap on decoded output (decompression-bomb guard).
pub const DEFAULT_DECODE_LIMIT: usize = 256 * 1024 * 1024;

/// The largest window a zstd body may need. HTTP's `zstd` content coding is limited to 8 MB
/// (RFC 9659), and a frame that asks for more is refused before anything is allocated (ruzstd's
/// own limit is 128 MB, allocated as the frame asks).
pub const ZSTD_MAX_WINDOW: u64 = 8 << 20;

/// Encodings from all `Content-Encoding` headers, in the order they were applied.
pub fn encodings(headers: &Headers) -> Vec<String> {
    header_all(headers, "content-encoding")
        .flat_map(|v| v.split(','))
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e != "identity")
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("unsupported Content-Encoding \"{0}\"")]
    Unsupported(String),
    #[error("{encoding} decoding failed: {message}")]
    Corrupt { encoding: String, message: String },
    #[error("decoded body exceeds the {0}-byte limit")]
    TooLarge(usize),
}

fn read_capped(mut r: impl Read, limit: usize, encoding: &str) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::new();
    let n = (&mut r)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| DecodeError::Corrupt { encoding: encoding.into(), message: e.to_string() })?;
    if n > limit {
        return Err(DecodeError::TooLarge(limit));
    }
    Ok(out)
}

fn decode_one(data: &[u8], encoding: &str, limit: usize) -> Result<Vec<u8>, DecodeError> {
    match encoding {
        "gzip" | "x-gzip" => read_capped(flate2::read::MultiGzDecoder::new(data), limit, encoding),
        // HTTP "deflate" is supposed to be zlib-wrapped; some servers send raw deflate.
        "deflate" => read_capped(flate2::read::ZlibDecoder::new(data), limit, encoding)
            .or_else(|_| read_capped(flate2::read::DeflateDecoder::new(data), limit, encoding)),
        "br" => read_capped(brotli_decompressor::Decompressor::new(data, 4096), limit, encoding),
        "zstd" => {
            let dec =
                ruzstd::decoding::StreamingDecoder::new_with_max_window_size(data, ZSTD_MAX_WINDOW).map_err(|e| {
                    let message = match e {
                        FrameDecoderError::WindowSizeTooBig { requested, .. } => format!(
                            "the frame needs a {} window, more than the {} HTTP allows (RFC 9659)",
                            crate::fmt::bytes(requested),
                            crate::fmt::bytes(ZSTD_MAX_WINDOW)
                        ),
                        e => e.to_string(),
                    };
                    DecodeError::Corrupt { encoding: encoding.into(), message }
                })?;
            read_capped(dec, limit, encoding)
        }
        other => Err(DecodeError::Unsupported(other.into())),
    }
}

/// Undo `encodings` (applied first-to-last, so decoded last-to-first).
pub fn decode(data: &[u8], encodings: &[String], limit: usize) -> Result<Vec<u8>, DecodeError> {
    let mut cur: Option<Vec<u8>> = None;
    for enc in encodings.iter().rev() {
        let input = cur.as_deref().unwrap_or(data);
        cur = Some(decode_one(input, enc, limit)?);
    }
    Ok(cur.unwrap_or_else(|| data.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn decodes_chains_and_variants() {
        let body = br#"{"ok":true,"status":"pending"}"#.repeat(20);
        assert_eq!(decode(&gzip(&body), &["gzip".into()], 1 << 20).unwrap(), body);

        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&body).unwrap();
        assert_eq!(decode(&z.finish().unwrap(), &["deflate".into()], 1 << 20).unwrap(), body);

        let mut raw = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        raw.write_all(&body).unwrap();
        assert_eq!(decode(&raw.finish().unwrap(), &["deflate".into()], 1 << 20).unwrap(), body);

        let twice = gzip(&gzip(&body));
        assert_eq!(decode(&twice, &["gzip".into(), "gzip".into()], 1 << 20).unwrap(), body);

        let zst = ruzstd::encoding::compress_to_vec(&body[..], ruzstd::encoding::CompressionLevel::Fastest);
        assert_eq!(decode(&zst, &["zstd".into()], 1 << 20).unwrap(), body);
    }

    #[test]
    fn brotli_round_trip() {
        let body = b"hello hello hello, brotli".repeat(50);
        let mut enc = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
        enc.write_all(&body).unwrap();
        let compressed = enc.into_inner();
        assert!(compressed.len() < body.len());
        assert_eq!(decode(&compressed, &["br".into()], 1 << 20).unwrap(), body);
    }

    /// A zstd frame of `hello` in one raw block that declares a window of 2^`log` bytes.
    fn zstd_frame(log: u8) -> Vec<u8> {
        // the magic number, a header without a content size, the window, the last (raw) block
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, (log - 10) << 3, (5 << 3) | 1, 0, 0];
        frame.extend_from_slice(b"hello");
        frame
    }

    /// RFC 9659 holds HTTP's zstd to an 8 MB window; a frame that asks for more is refused
    /// before its window is allocated (ruzstd would take up to 128 MB for it).
    #[test]
    fn zstd_windows_over_8_mb_are_refused() {
        let zstd = ["zstd".to_string()];
        assert_eq!(decode(&zstd_frame(10), &zstd, 100).unwrap(), b"hello");
        assert_eq!(decode(&zstd_frame(23), &zstd, 100).unwrap(), b"hello", "8 MB is allowed");
        let e = decode(&zstd_frame(24), &zstd, 100).unwrap_err();
        assert_eq!(
            e.to_string(),
            "zstd decoding failed: the frame needs a 16 MB window, more than the 8.0 MB HTTP allows (RFC 9659)"
        );
        // as a body: undecodable, shown as captured with the reason
        let hs: Headers = vec![("Content-Encoding".into(), "zstd".into())];
        let d = super::super::decode_body(bytes::Bytes::from(zstd_frame(30)), Some(&hs), 1 << 20);
        assert_eq!(d.bytes.len(), zstd_frame(30).len());
        assert!(d.error.is_some_and(|e| e.contains("1.0 GB window")));
    }

    #[test]
    fn caps_output_and_reports_errors() {
        let bomb = gzip(&vec![0u8; 1 << 20]);
        assert_eq!(decode(&bomb, &["gzip".into()], 1000).unwrap_err(), DecodeError::TooLarge(1000));
        assert!(matches!(decode(b"not gzip", &["gzip".into()], 100), Err(DecodeError::Corrupt { .. })));
        assert_eq!(decode(b"x", &["snappy".into()], 100).unwrap_err(), DecodeError::Unsupported("snappy".into()));
        let hs: Headers =
            vec![("Content-Encoding".into(), "gzip, br".into()), ("content-encoding".into(), "identity".into())];
        assert_eq!(encodings(&hs), vec!["gzip".to_string(), "br".to_string()]);
    }
}
