//! Redaction of secrets and personal data (ARCHITECTURE.md §5.10). On by default in the UI and
//! in every export. This module has the built-in header list; configurable headers, query
//! parameters and JSON paths come with the config file.

/// Headers whose values are masked unless the user reveals them.
pub const BUILTIN_HEADERS: [&str; 7] =
    ["authorization", "proxy-authorization", "cookie", "set-cookie", "x-api-key", "api-key", "x-auth-token"];

pub fn is_sensitive_header(name: &str) -> bool {
    BUILTIN_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// A masked value: `‹redacted 32 chars›`.
pub fn mask(value: &str) -> String {
    format!("‹redacted {} chars›", value.chars().count())
}

/// The value to show for a header, or `None` if it is not sensitive. The auth scheme
/// (`Bearer`, `Basic`), cookie names and `Set-Cookie` attributes stay visible; only the
/// secret parts are masked.
pub fn header_value(name: &str, value: &str) -> Option<String> {
    if !is_sensitive_header(name) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    Some(match lower.as_str() {
        "authorization" | "proxy-authorization" => match value.split_once(' ') {
            Some((scheme, rest))
                if !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') =>
            {
                format!("{scheme} {}", mask(rest.trim_start()))
            }
            _ => mask(value),
        },
        "cookie" => value
            .split(';')
            .map(|pair| match pair.trim().split_once('=') {
                Some((n, v)) => format!("{n}={}", mask(v)),
                None => mask(pair.trim()),
            })
            .collect::<Vec<_>>()
            .join("; "),
        "set-cookie" => {
            let mut parts = value.split(';');
            let first = parts.next().unwrap_or("");
            let mut out = match first.trim().split_once('=') {
                Some((n, v)) => format!("{n}={}", mask(v)),
                None => mask(first.trim()),
            };
            for attr in parts {
                out.push(';');
                out.push_str(attr);
            }
            out
        }
        _ => mask(value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secret_parts_only() {
        assert_eq!(header_value("Content-Type", "application/json"), None);
        assert_eq!(header_value("Authorization", "Bearer abc.def").as_deref(), Some("Bearer ‹redacted 7 chars›"));
        assert_eq!(header_value("authorization", "c2VjcmV0").as_deref(), Some("‹redacted 8 chars›"));
        assert_eq!(
            header_value("Cookie", "dsid=abcd; region=in").as_deref(),
            Some("dsid=‹redacted 4 chars›; region=‹redacted 2 chars›")
        );
        assert_eq!(
            header_value("Set-Cookie", "dsid=abcd; Path=/; Secure").as_deref(),
            Some("dsid=‹redacted 4 chars›; Path=/; Secure")
        );
        assert_eq!(header_value("X-API-Key", "k-123").as_deref(), Some("‹redacted 5 chars›"));
    }
}
