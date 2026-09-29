//! Content-Encoding decoding (gzip, deflate, br, zstd), all pure Rust, with an output cap.

use std::io::Read;

use crate::model::{Headers, header_all};

/// Default cap on decoded output (decompression-bomb guard).
pub const DEFAULT_DECODE_LIMIT: usize = 256 * 1024 * 1024;

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
            let dec = ruzstd::decoding::StreamingDecoder::new(data)
                .map_err(|e| DecodeError::Corrupt { encoding: encoding.into(), message: e.to_string() })?;
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
