//! Hex dump lines, produced on demand for the visible window.

pub const BYTES_PER_LINE: usize = 16;

pub fn line_count(len: usize) -> usize {
    len.div_ceil(BYTES_PER_LINE).max(1)
}

/// `00000010  7b 22 6f 6b 22 3a 74 72  75 65 7d              |{"ok":true}|`
pub fn line(bytes: &[u8], row: usize) -> String {
    let start = row * BYTES_PER_LINE;
    let chunk = bytes.get(start..(start + BYTES_PER_LINE).min(bytes.len())).unwrap_or(&[]);
    let mut s = format!("{start:08x}  ");
    for i in 0..BYTES_PER_LINE {
        match chunk.get(i) {
            Some(b) => s.push_str(&format!("{b:02x} ")),
            None => s.push_str("   "),
        }
        if i == 7 {
            s.push(' ');
        }
    }
    s.push(' ');
    s.push('|');
    s.extend(chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }));
    s.push('|');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rows() {
        let data = b"{\"ok\":true}";
        assert_eq!(line(data, 0), "00000000  7b 22 6f 6b 22 3a 74 72  75 65 7d                 |{\"ok\":true}|");
        assert_eq!(line_count(0), 1);
        assert_eq!(line_count(33), 3);
    }
}
