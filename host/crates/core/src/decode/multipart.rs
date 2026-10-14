//! `multipart/*` bodies split into parts.

use bytes::Bytes;

use crate::model::{Headers, header};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub headers: Headers,
    pub body: Bytes,
}

impl Part {
    /// `name` and `filename` from Content-Disposition.
    pub fn disposition(&self) -> (Option<String>, Option<String>) {
        let Some(cd) = header(&self.headers, "content-disposition") else { return (None, None) };
        let get = |key: &str| {
            cd.split(';').skip(1).find_map(|p| {
                let (k, v) = p.split_once('=')?;
                k.trim().eq_ignore_ascii_case(key).then(|| v.trim().trim_matches('"').to_string())
            })
        };
        (get("name"), get("filename"))
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

/// The parts of a body split on `--boundary` delimiters, one at a time (a body of tiny parts
/// would make millions of them).
pub struct Parts {
    body: Bytes,
    delim: Vec<u8>,
    /// Where the next part's delimiter starts; `None` once the parts are over.
    pos: Option<usize>,
}

impl Iterator for Parts {
    type Item = Part;

    fn next(&mut self) -> Option<Part> {
        let (body, d) = (&self.body, self.delim.as_slice());
        let after = self.pos? + d.len();
        if body[after..].starts_with(b"--") {
            self.pos = None;
            return None; // closing delimiter
        }
        // skip the line break after the delimiter
        let content_start = if body[after..].starts_with(b"\r\n") {
            after + 2
        } else if body[after..].starts_with(b"\n") {
            after + 1
        } else {
            after
        };
        let next = find(body, d, content_start).unwrap_or(body.len());
        // part content ends before the line break preceding the next delimiter
        let mut content_end = next;
        if content_end >= 2 && &body[content_end - 2..content_end] == b"\r\n" {
            content_end -= 2;
        } else if content_end >= 1 && body[content_end - 1] == b'\n' {
            content_end -= 1;
        }
        let content = body.slice(content_start..content_end.max(content_start));
        let (hdr_end, body_start) = match find(&content, b"\r\n\r\n", 0) {
            Some(p) => (p, p + 4),
            None => match find(&content, b"\n\n", 0) {
                Some(p) => (p, p + 2),
                None => (0, 0),
            },
        };
        let headers = String::from_utf8_lossy(&content[..hdr_end])
            .lines()
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
            .collect();
        self.pos = (next < body.len()).then_some(next);
        Some(Part { headers, body: content.slice(body_start..) })
    }
}

/// The parts of a body split on `--boundary` delimiters; `None` if no delimiter is found.
pub fn parts(body: &Bytes, boundary: &str) -> Option<Parts> {
    let delim = format!("--{boundary}").into_bytes();
    let pos = find(body, &delim, 0)?;
    Some(Parts { body: body.clone(), delim, pos: Some(pos) })
}

/// Split a body on `--boundary` delimiters. Returns `None` if no delimiter is found.
pub fn parse(body: &Bytes, boundary: &str) -> Option<Vec<Part>> {
    parts(body, boundary).map(Iterator::collect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_parts() {
        let body = Bytes::from_static(
            b"--XyZ\r\nContent-Disposition: form-data; name=\"meta\"\r\nContent-Type: application/json\r\n\r\n{\"a\":1}\r\n--XyZ\r\nContent-Disposition: form-data; name=\"photo\"; filename=\"face.png\"\r\nContent-Type: image/png\r\n\r\n\x89PNG\r\n--XyZ--\r\n",
        );
        let parts = parse(&body, "XyZ").unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(&parts[0].body[..], b"{\"a\":1}");
        assert_eq!(parts[1].disposition(), (Some("photo".into()), Some("face.png".into())));
        assert_eq!(&parts[1].body[..], b"\x89PNG");
        assert!(parse(&body, "nope").is_none());
    }
}
