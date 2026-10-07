//! Any body under any Content-Type and Content-Encoding: decoding and every viewer's parser never
//! panic (ARCHITECTURE.md §5.9).

#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use traffic_police_core::decode::{self, decode_body, json, kind, markup, multipart, protobuf};

const TYPES: &[&str] = &[
    "application/json",
    "text/html; charset=utf-8",
    "application/xml",
    "application/x-www-form-urlencoded",
    "multipart/form-data; boundary=b",
    "application/x-protobuf",
    "application/grpc",
    "image/png",
    "text/plain; charset=iso-8859-1",
    "application/octet-stream",
];
const ENCODINGS: &[&str] = &["", "gzip", "deflate", "br", "zstd", "gzip, br", "identity"];

fuzz_target!(|data: &[u8]| {
    let Some((&pick, body)) = data.split_first() else { return };
    let content_type = TYPES[usize::from(pick) % TYPES.len()];
    let encoding = ENCODINGS[usize::from(pick / 16) % ENCODINGS.len()];
    let mut headers = vec![("Content-Type".to_string(), content_type.to_string())];
    if !encoding.is_empty() {
        headers.push(("Content-Encoding".to_string(), encoding.to_string()));
    }
    let decoded = decode_body(Bytes::copy_from_slice(body), Some(&headers), 1 << 20);
    let bytes = decoded.bytes.clone();
    let _ = kind::detect(Some(content_type), &bytes);
    let _ = json::parse(&bytes);
    let text = String::from_utf8_lossy(&bytes);
    let _ = markup::pretty(&text, markup::Dialect::Html);
    let _ = markup::pretty(&text, markup::Dialect::Xml);
    let _ = decode::form::parse_pairs(&text);
    let _ = multipart::parse(&bytes, "b");
    let _ = protobuf::decode_raw(&bytes);
    let _ = protobuf::grpc_messages(&bytes);
    let _ = decode::doc::text_lines(&text);
    for row in 0..decode::hex::line_count(bytes.len()).min(64) {
        let _ = decode::hex::line(&bytes, row);
    }
});
