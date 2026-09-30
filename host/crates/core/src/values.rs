//! Decoding single values found in headers and bodies (ARCHITECTURE.md §5.10): JWTs, base64
//! (standard and URL-safe) and URL encoding, plus relative ages for JWT times.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::Value;

use crate::decode::doc::{StyledLine, Tok};
use crate::fmt;

/// A decoded JSON Web Token (JWS compact form). The signature is not verified.
#[derive(Debug, Clone, PartialEq)]
pub struct Jwt {
    pub header: Value,
    /// The payload: JSON claims, or (rarely) something else, as text.
    pub claims: Result<Value, String>,
    /// The third part as sent (base64url).
    pub signature: String,
}

fn b64url(part: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok()
}

fn is_b64url(part: &str) -> bool {
    part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// A JWT, when `s` is one: three base64url parts whose first decodes to a JSON object. A
/// `Bearer ` prefix is ignored.
pub fn jwt(s: &str) -> Option<Jwt> {
    let s = s.trim();
    let s = s.strip_prefix("Bearer ").or_else(|| s.strip_prefix("bearer ")).unwrap_or(s).trim();
    let mut parts = s.split('.');
    let (h, p, sig) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || h.len() < 4 || p.is_empty() || ![h, p, sig].iter().all(|x| is_b64url(x)) {
        return None;
    }
    let header: Value = serde_json::from_slice(&b64url(h)?).ok()?;
    if !header.is_object() {
        return None;
    }
    let payload = b64url(p)?;
    let claims = serde_json::from_slice(&payload).map_err(|_| String::from_utf8_lossy(&payload).into_owned());
    Some(Jwt { header, claims, signature: sig.to_string() })
}

/// Where a JWT appears in `text` (the first one), as a byte range.
pub fn find_jwt(text: &str) -> Option<std::ops::Range<usize>> {
    let token = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let mut start = None;
    for (i, c) in text.char_indices().chain([(text.len(), ' ')]) {
        match (token(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let cand = &text[s..i];
                if cand.matches('.').count() == 2 && jwt(cand).is_some() {
                    return Some(s..i);
                }
                start = None;
            }
            _ => {}
        }
    }
    None
}

/// Base64 in any of the usual alphabets and paddings: the bytes, and which alphabet it was.
/// Short values, numbers and plain lower-case words are not taken for base64.
pub fn base64(s: &str) -> Option<(Vec<u8>, &'static str)> {
    let s = s.trim();
    let b = s.as_bytes();
    let plausible = b.len() >= 8
        && !b.iter().all(u8::is_ascii_digit)
        && !b.iter().all(u8::is_ascii_lowercase)
        && !b.iter().all(u8::is_ascii_uppercase);
    if !plausible {
        return None;
    }
    let url = s.contains(['-', '_']);
    let engines: [(&base64::engine::GeneralPurpose, &str); 2] = if url {
        [(&URL_SAFE, "URL-safe base64"), (&URL_SAFE_NO_PAD, "URL-safe base64")]
    } else {
        [(&STANDARD, "base64"), (&STANDARD_NO_PAD, "base64")]
    };
    engines.iter().find_map(|(e, name)| e.decode(s).ok().map(|b| (b, *name)))
}

/// URL decoding (`%xx`, and `+` as a space), when it changes anything.
pub fn url_decode(s: &str) -> Option<String> {
    if !s.contains(['%', '+']) {
        return None;
    }
    let spaced = s.replace('+', " ");
    let out = percent_encoding::percent_decode_str(&spaced).decode_utf8_lossy().into_owned();
    (out != s).then_some(out)
}

/// `in 55 min`, `3 d 2 h ago`, for a difference in seconds (positive = in the future).
pub fn relative(delta_secs: i64) -> String {
    let a = delta_secs.unsigned_abs();
    let span = if a < 60 {
        format!("{a} s")
    } else if a < 3600 {
        format!("{} min", a / 60)
    } else if a < 86_400 {
        format!("{} h {} min", a / 3600, (a % 3600) / 60)
    } else {
        format!("{} d {} h", a / 86_400, (a % 86_400) / 3600)
    };
    if delta_secs >= 0 { format!("in {span}") } else { format!("{span} ago") }
}

fn json_lines(v: &Value, out: &mut Vec<StyledLine>) {
    let text = serde_json::to_vec_pretty(v).unwrap_or_default();
    match crate::decode::json::parse(&text) {
        Ok(doc) => out.extend(doc.lines.into_iter().map(|l| l.line)),
        Err(_) => out.push(StyledLine::plain(String::from_utf8_lossy(&text))),
    }
}

impl Jwt {
    pub fn alg(&self) -> &str {
        self.header["alg"].as_str().unwrap_or("?")
    }

    fn claim_ms(&self, name: &str) -> Option<i64> {
        let v = self.claims.as_ref().ok()?.get(name)?.as_f64()?;
        Some((v * 1000.0) as i64)
    }

    /// One line for the Overview: algorithm, when it expires, subject and issuer.
    pub fn summary(&self, now_ms: i64) -> String {
        let mut s = format!("JWT {}", self.alg());
        if let Some(exp) = self.claim_ms("exp") {
            let rel = relative((exp - now_ms) / 1000);
            let at = fmt::wall_clock(exp);
            if exp <= now_ms {
                s.push_str(&format!(" · expired {at} ({rel})"));
            } else {
                s.push_str(&format!(" · expires {at} ({rel})"));
            }
        }
        if let Ok(c) = &self.claims {
            for claim in ["sub", "iss"] {
                if let Some(v) = c.get(claim).and_then(Value::as_str) {
                    s.push_str(&format!(" · {claim} {v}"));
                }
            }
        }
        s
    }

    /// Header, claims, the times in local time with their ages, and the signature.
    pub fn lines(&self, now_ms: i64) -> Vec<StyledLine> {
        let mut out = vec![StyledLine::styled(format!("JWT · {} · signature not verified", self.alg()), Tok::Meta)];
        out.push(StyledLine::new());
        out.push(StyledLine::styled("Header", Tok::Field));
        json_lines(&self.header, &mut out);
        out.push(StyledLine::new());
        out.push(StyledLine::styled("Claims", Tok::Field));
        match &self.claims {
            Ok(v) => json_lines(v, &mut out),
            Err(text) => out.push(StyledLine::plain(text.clone())),
        }
        let times: Vec<(&str, &str, i64)> = [("iat", "issued"), ("nbf", "valid from"), ("exp", "expires")]
            .into_iter()
            .filter_map(|(k, what)| self.claim_ms(k).map(|ms| (k, what, ms)))
            .collect();
        if !times.is_empty() {
            out.push(StyledLine::new());
            out.push(StyledLine::styled("Times", Tok::Field));
            for (k, what, ms) in times {
                let mut l = StyledLine::new();
                l.push(&format!("{k:<4}"), Tok::Key);
                l.push(&format!("{what:<11}"), Tok::Plain);
                l.push(&fmt::iso8601(ms), Tok::Str);
                let rel = relative((ms - now_ms) / 1000);
                let note = if k == "exp" && ms <= now_ms { format!("  expired {rel}") } else { format!("  {rel}") };
                l.push(&note, if k == "exp" && ms <= now_ms { Tok::Error } else { Tok::Meta });
                out.push(l);
            }
        }
        out.push(StyledLine::new());
        let mut sig = StyledLine::new();
        sig.push("Signature  ", Tok::Field);
        sig.push(&format!("{} characters (not verified)", self.signature.len()), Tok::Meta);
        out.push(sig);
        out
    }
}

/// Lines as plain text, for copying.
pub fn plain_text(lines: &[StyledLine]) -> String {
    lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn token(header: &Value, claims: &Value) -> String {
        let enc = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        format!("{}.{}.c2lnbmF0dXJl", enc(header), enc(claims))
    }

    #[test]
    fn jwts_are_found_and_decoded() {
        let t = token(&json!({ "alg": "HS256", "typ": "JWT" }), &json!({ "sub": "user_1", "exp": 1_790_662_200 }));
        let j = jwt(&format!("Bearer {t}")).unwrap();
        assert_eq!(j.alg(), "HS256");
        assert_eq!(j.claims.as_ref().unwrap()["sub"], "user_1");
        // an hour before it expires
        let now = 1_790_658_600_000;
        let s = j.summary(now);
        assert!(s.starts_with("JWT HS256 · expires "), "{s}");
        assert!(s.ends_with("(in 1 h 0 min) · sub user_1"), "{s}");
        let text = plain_text(&j.lines(now + 7_200_000));
        assert!(text.contains("\"sub\": \"user_1\""), "{text}");
        assert!(text.contains("expired 1 h 0 min ago"), "{text}");
        assert!(text.contains("signature not verified"), "{text}");

        let body = format!("{{\"access_token\":\"{t}\",\"n\":1}}");
        let r = find_jwt(&body).unwrap();
        assert_eq!(&body[r], t);
        // look-alikes are not JWTs
        for s in ["a.b.c", "example.com.au", "1.2.3", "eyJhbGciOiJIUzI1NiJ9.e30", "v1.2.3-beta"] {
            assert!(jwt(s).is_none(), "{s}");
            assert!(find_jwt(s).is_none(), "{s}");
        }
    }

    #[test]
    fn base64_and_url_decoding() {
        assert_eq!(base64("aGVsbG8gd29ybGQ="), Some((b"hello world".to_vec(), "base64")));
        assert_eq!(base64("aGVsbG8gd29ybGQ"), Some((b"hello world".to_vec(), "base64")));
        for not in ["true", "-_8", "12345678", "username", "ABCDEFGH"] {
            assert_eq!(base64(not), None, "{not}");
        }
        assert_eq!(base64("__-_ab-8"), Some((vec![0xff, 0xff, 0xbf, 0x69, 0xbf, 0xbc], "URL-safe base64")));
        assert_eq!(base64("not base64!"), None);
        assert_eq!(url_decode("a%20b+c%2Fd"), Some("a b c/d".into()));
        assert_eq!(url_decode("plain"), None);
    }

    #[test]
    fn relative_times() {
        assert_eq!(relative(42), "in 42 s");
        assert_eq!(relative(-90), "1 min ago");
        assert_eq!(relative(3 * 3600 + 5 * 60), "in 3 h 5 min");
        assert_eq!(relative(-(2 * 86_400 + 3 * 3600)), "2 d 3 h ago");
    }
}
