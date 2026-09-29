//! Schemaless protobuf decoding in the style of `protoc --decode_raw`, and gRPC message framing.

use super::doc::{StyledLine, Tok};

const MAX_NESTING: usize = 32;

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i)?;
        *i += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

#[derive(Debug, Clone)]
enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(&'a [u8]),
    Group(Vec<(u64, Field<'a>)>),
}

fn fields(b: &[u8], depth: usize) -> Option<Vec<(u64, Field<'_>)>> {
    fields_until(b, &mut 0, depth, None)
}

fn fields_until<'a>(b: &'a [u8], i: &mut usize, depth: usize, end_group: Option<u64>) -> Option<Vec<(u64, Field<'a>)>> {
    if depth > MAX_NESTING {
        return None;
    }
    let mut out = Vec::new();
    while *i < b.len() {
        let key = varint(b, i)?;
        let (num, wt) = (key >> 3, key & 7);
        if num == 0 || num > (1 << 29) - 1 {
            return None;
        }
        let f = match wt {
            0 => Field::Varint(varint(b, i)?),
            1 => {
                let v = u64::from_le_bytes(b.get(*i..*i + 8)?.try_into().ok()?);
                *i += 8;
                Field::Fixed64(v)
            }
            2 => {
                let len = usize::try_from(varint(b, i)?).ok()?;
                let s = b.get(*i..i.checked_add(len)?)?;
                *i += len;
                Field::Bytes(s)
            }
            3 => Field::Group(fields_until(b, i, depth + 1, Some(num))?),
            4 => return (end_group == Some(num)).then_some(out),
            5 => {
                let v = u32::from_le_bytes(b.get(*i..*i + 4)?.try_into().ok()?);
                *i += 4;
                Field::Fixed32(v)
            }
            _ => return None,
        };
        out.push((num, f));
    }
    end_group.is_none().then_some(out)
}

fn printable(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
}

fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if c.is_control() => o.push_str(&format!("\\{:03o}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn bytes_literal(b: &[u8]) -> String {
    let mut o = String::from("\"");
    for &x in b {
        match x {
            b'"' => o.push_str("\\\""),
            b'\\' => o.push_str("\\\\"),
            0x20..=0x7e => o.push(x as char),
            _ => o.push_str(&format!("\\{x:03o}")),
        }
    }
    o.push('"');
    o
}

fn emit(out: &mut Vec<StyledLine>, fs: &[(u64, Field<'_>)], depth: usize) {
    for (num, f) in fs {
        let mut line = StyledLine::new();
        line.indent(depth);
        line.push(&num.to_string(), Tok::Field);
        match f {
            Field::Varint(v) => {
                line.push(": ", Tok::Punct);
                line.push(&v.to_string(), Tok::Num);
                out.push(line);
            }
            Field::Fixed64(v) => {
                line.push(": ", Tok::Punct);
                line.push(&format!("0x{v:016x}"), Tok::Num);
                out.push(line);
            }
            Field::Fixed32(v) => {
                line.push(": ", Tok::Punct);
                line.push(&format!("0x{v:08x}"), Tok::Num);
                out.push(line);
            }
            Field::Group(g) => {
                line.push(" {", Tok::Punct);
                out.push(line);
                emit(out, g, depth + 1);
                let mut close = StyledLine::new();
                close.indent(depth);
                close.push("}", Tok::Punct);
                out.push(close);
            }
            Field::Bytes(b) => {
                let as_str = std::str::from_utf8(b).ok();
                if let Some(s) = as_str.filter(|s| printable(s)) {
                    line.push(": ", Tok::Punct);
                    line.push(&escape(s), Tok::Str);
                    out.push(line);
                } else if let Some(nested) =
                    (!b.is_empty()).then(|| fields(b, depth + 1)).flatten().filter(|n| !n.is_empty())
                {
                    line.push(" {", Tok::Punct);
                    out.push(line);
                    emit(out, &nested, depth + 1);
                    let mut close = StyledLine::new();
                    close.indent(depth);
                    close.push("}", Tok::Punct);
                    out.push(close);
                } else {
                    line.push(": ", Tok::Punct);
                    match as_str {
                        Some(s) => line.push(&escape(s), Tok::Str),
                        None => line.push(&bytes_literal(b), Tok::Str),
                    };
                    out.push(line);
                }
            }
        }
    }
}

/// Decode a protobuf message without a schema.
pub fn decode_raw(b: &[u8]) -> Result<Vec<StyledLine>, String> {
    let fs = fields(b, 0).ok_or_else(|| "not a valid protobuf message".to_string())?;
    let mut out = Vec::new();
    emit(&mut out, &fs, 0);
    Ok(out)
}

/// Split a gRPC body into its length-prefixed messages: `(compressed, message)`.
pub fn grpc_messages(b: &[u8]) -> Result<Vec<(bool, &[u8])>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let hdr = b.get(i..i + 5).ok_or("truncated gRPC frame header")?;
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        let msg = b.get(i + 5..i + 5 + len).ok_or("truncated gRPC message")?;
        out.push((hdr[0] == 1, msg));
        i += 5 + len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[StyledLine]) -> String {
        lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn decodes_like_protoc_raw() {
        // 1: 150, 2: "testing", 3 { 1: 150 }, 4: fixed64 1, 5: fixed32 2
        let msg = [
            0x08, 0x96, 0x01, 0x12, 0x07, b't', b'e', b's', b't', b'i', b'n', b'g', 0x1a, 0x03, 0x08, 0x96, 0x01, 0x21,
            1, 0, 0, 0, 0, 0, 0, 0, 0x2d, 2, 0, 0, 0,
        ];
        assert_eq!(
            text(&decode_raw(&msg).unwrap()),
            "1: 150\n2: \"testing\"\n3 {\n  1: 150\n}\n4: 0x0000000000000001\n5: 0x00000002"
        );
        assert!(decode_raw(&[0xff, 0xff]).is_err());
    }

    #[test]
    fn splits_grpc_frames() {
        let body = [0, 0, 0, 0, 3, 0x08, 0x96, 0x01, 1, 0, 0, 0, 1, 0xaa];
        let msgs = grpc_messages(&body).unwrap();
        assert_eq!(msgs, vec![(false, &[0x08, 0x96, 0x01][..]), (true, &[0xaa][..])]);
        assert!(grpc_messages(&[0, 0, 0, 0, 9, 1]).is_err());
    }
}
