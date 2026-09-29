//! `application/x-www-form-urlencoded` (and URL query strings).

use percent_encoding::percent_decode_str;

fn decode_component(s: &str) -> String {
    let spaced = s.replace('+', " ");
    percent_decode_str(&spaced).decode_utf8_lossy().into_owned()
}

/// Decoded `(key, value)` pairs in order, duplicates kept. A key without `=` has an empty value.
pub fn parse_pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode_component(k), decode_component(v)),
            None => (decode_component(p), String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_pairs() {
        assert_eq!(
            parse_pairs("grant_type=password&username=a%40b.com&msg=hi+there&flag&x=1&x=2"),
            vec![
                ("grant_type".into(), "password".into()),
                ("username".into(), "a@b.com".into()),
                ("msg".into(), "hi there".into()),
                ("flag".into(), "".into()),
                ("x".into(), "1".into()),
                ("x".into(), "2".into()),
            ]
        );
    }
}
