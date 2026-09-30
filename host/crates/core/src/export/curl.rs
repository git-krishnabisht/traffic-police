//! A request as a cURL command (ARCHITECTURE.md §5.10), for POSIX shells.

use crate::model::Transaction;

/// Single-quotes `s` for a POSIX shell (`'` becomes `'\''`).
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// How the request body goes into the command.
pub enum CurlBody<'a> {
    None,
    /// UTF-8 text, inlined with `--data-binary`.
    Text(&'a str),
    /// Bytes saved to a file, referenced as `--data-binary @file`.
    File(&'a str),
}

/// The command that repeats `t`: method, URL, the headers as the app sent them (in order), and
/// the body. Headers curl sets itself (`Host`, `Content-Length`, `Connection`) are left out;
/// when the app accepted gzip, `--compressed` replaces its `Accept-Encoding`.
pub fn curl(t: &Transaction, body: CurlBody<'_>) -> String {
    let mut parts: Vec<String> = vec!["curl".into()];
    let has_body = !matches!(body, CurlBody::None);
    let implied = if has_body { "POST" } else { "GET" };
    if !t.method.eq_ignore_ascii_case(implied) {
        parts.push(format!("-X {}", quote(&t.method)));
    }
    parts.push(quote(&t.url.raw));
    let mut compressed = false;
    for (name, value) in &t.req_headers {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "host" | "content-length" | "connection" | "transfer-encoding" => continue,
            "accept-encoding" if value.to_ascii_lowercase().contains("gzip") => {
                compressed = true;
                continue;
            }
            _ => {}
        }
        parts.push(format!("-H {}", quote(&format!("{name}: {value}"))));
    }
    if compressed {
        parts.push("--compressed".into());
    }
    match body {
        CurlBody::None => {}
        CurlBody::Text(s) => parts.push(format!("--data-binary {}", quote(s))),
        CurlBody::File(path) => parts.push(format!("--data-binary {}", quote(&format!("@{path}")))),
    }
    parts.join(" \\\n  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TxnKey, Url};

    fn txn(method: &str, url: &str, headers: &[(&str, &str)]) -> Transaction {
        let mut t = Transaction::new_placeholder(TxnKey { source: 1, txn: 1 }, 0);
        t.method = method.into();
        t.url = Url::parse(url);
        t.req_headers = headers.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect();
        t
    }

    #[test]
    fn get_with_headers_in_order_and_compressed() {
        let t = txn(
            "GET",
            "https://api.example.app/v1/status?id=7&q=a b",
            &[
                ("Host", "api.example.app"),
                ("Authorization", "Bearer x.y.z"),
                ("Accept-Encoding", "gzip"),
                ("X-Note", "it's"),
            ],
        );
        assert_eq!(
            curl(&t, CurlBody::None),
            "curl \\\n  'https://api.example.app/v1/status?id=7&q=a b' \\\n  -H 'Authorization: Bearer x.y.z' \\\n  -H 'X-Note: it'\\''s' \\\n  --compressed"
        );
    }

    #[test]
    fn bodies_inline_or_from_a_file() {
        let t = txn(
            "POST",
            "https://api.example.app/enroll",
            &[("Content-Type", "application/json"), ("Content-Length", "13")],
        );
        let c = curl(&t, CurlBody::Text(r#"{"answer":42}"#));
        assert!(c.ends_with(r#"--data-binary '{"answer":42}'"#), "{c}");
        assert!(!c.contains("-X"), "POST is implied by a body");
        assert!(!c.contains("Content-Length"));
        let t = txn("PUT", "https://cdn.example.app/upload", &[]);
        let c = curl(&t, CurlBody::File("body-7.bin"));
        assert!(c.contains("-X 'PUT'") && c.ends_with("--data-binary '@body-7.bin'"), "{c}");
    }
}
