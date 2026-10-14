//! Schemaless protobuf decoding in the style of `protoc --decode_raw`, and gRPC message framing.
//!
//! A message is checked whole first, then shown field by field as it is read again (no tree of
//! its fields is kept), within a [`Budget`].

use super::doc::{Budget, StyledLine, Tok, cut_line};

const MAX_NESTING: usize = 32;

/// Why bytes are no field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    /// The field goes on past the end of the bytes.
    Short,
    /// Not protobuf.
    Bad,
}

fn varint(b: &[u8], i: &mut usize) -> Result<u64, Fail> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i).ok_or(Fail::Short)?;
        *i += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Fail::Bad)
}

#[derive(Debug, Clone, Copy)]
enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(&'a [u8]),
    /// A group starts (wire type 3); its fields follow, up to its end.
    Group,
    /// The end of the group (wire type 4).
    End,
}

/// The field at `i`, moving past it, or why the bytes are no field there.
fn field<'a>(b: &'a [u8], i: &mut usize) -> Result<(u64, Field<'a>), Fail> {
    let key = varint(b, i)?;
    let (num, wt) = (key >> 3, key & 7);
    if num == 0 || num > (1 << 29) - 1 {
        return Err(Fail::Bad);
    }
    let f = match wt {
        0 => Field::Varint(varint(b, i)?),
        1 => {
            let v = u64::from_le_bytes(b.get(*i..*i + 8).ok_or(Fail::Short)?.try_into().expect("8 bytes"));
            *i += 8;
            Field::Fixed64(v)
        }
        2 => {
            let len = usize::try_from(varint(b, i)?).map_err(|_| Fail::Bad)?;
            let s = b.get(*i..i.checked_add(len).ok_or(Fail::Bad)?).ok_or(Fail::Short)?;
            *i += len;
            Field::Bytes(s)
        }
        3 => Field::Group,
        4 => Field::End,
        5 => {
            let v = u32::from_le_bytes(b.get(*i..*i + 4).ok_or(Fail::Short)?.try_into().expect("4 bytes"));
            *i += 4;
            Field::Fixed32(v)
        }
        _ => return Err(Fail::Bad),
    };
    Ok((num, f))
}

/// Checks the fields of `b` from `i` on (`end_group`: the body of that group, up to its end),
/// keeping nothing. A failure leaves `i` where the field that failed starts.
fn scan(b: &[u8], i: &mut usize, depth: usize, end_group: Option<u64>) -> Result<(), Fail> {
    if depth > MAX_NESTING {
        return Err(Fail::Bad);
    }
    while *i < b.len() {
        let start = *i;
        match field(b, i) {
            Err(e) => {
                *i = start;
                return Err(e);
            }
            Ok((num, Field::Group)) => scan(b, i, depth + 1, Some(num))?,
            Ok((num, Field::End)) => return if end_group == Some(num) { Ok(()) } else { Err(Fail::Bad) },
            Ok(_) => {}
        }
    }
    // a group open at the end goes on past it
    if end_group.is_none() { Ok(()) } else { Err(Fail::Short) }
}

/// Whether `b` is a message (its fields, nested `depth` deep).
fn is_message(b: &[u8], depth: usize) -> bool {
    scan(b, &mut 0, depth, None).is_ok()
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

/// The lines of a message being shown, within a budget.
struct Out<'b> {
    lines: Vec<StyledLine>,
    budget: &'b mut Budget,
}

impl Out<'_> {
    /// False once the budget is spent: the message stops there.
    fn push(&mut self, line: StyledLine) -> bool {
        let fits = self.budget.take(line.text.len());
        if fits {
            self.lines.push(line);
        }
        fits
    }

    /// Whether a line of up to `len` bytes could still fit, before it is made (a field of
    /// megabytes of bytes is four times as long escaped).
    fn room_for(&mut self, len: usize) -> bool {
        if len > self.budget.room() {
            self.budget.spend();
        }
        !self.budget.spent()
    }
}

fn closing(depth: usize) -> StyledLine {
    let mut close = StyledLine::new();
    close.indent(depth);
    close.push("}", Tok::Punct);
    close
}

/// How showing fields ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    /// The message's (or the group's) last field was shown.
    Whole,
    /// The bytes ended inside a group: the message was cut there.
    Open,
    /// The budget is spent.
    Full,
}

/// Shows the fields of a message checked by [`scan`], from `i` up to its end (`in_group`: up
/// to the end of the group they are in).
fn emit(b: &[u8], i: &mut usize, depth: usize, in_group: bool, out: &mut Out<'_>) -> Shown {
    while *i < b.len() {
        let Ok((num, f)) = field(b, i) else { return Shown::Whole };
        let mut line = StyledLine::new();
        line.indent(depth);
        line.push(&num.to_string(), Tok::Field);
        let fits = match f {
            // the group's end ([`scan`] checked that it is this group's)
            Field::End => return Shown::Whole,
            Field::Varint(v) => {
                line.push(": ", Tok::Punct);
                line.push(&v.to_string(), Tok::Num);
                out.push(line)
            }
            Field::Fixed64(v) => {
                line.push(": ", Tok::Punct);
                line.push(&format!("0x{v:016x}"), Tok::Num);
                out.push(line)
            }
            Field::Fixed32(v) => {
                line.push(": ", Tok::Punct);
                line.push(&format!("0x{v:08x}"), Tok::Num);
                out.push(line)
            }
            Field::Group => {
                line.push(" {", Tok::Punct);
                if !out.push(line) {
                    return Shown::Full;
                }
                match emit(b, i, depth + 1, true, out) {
                    Shown::Whole => out.push(closing(depth)),
                    other => return other,
                }
            }
            Field::Bytes(s) => {
                let as_str = std::str::from_utf8(s).ok();
                if let Some(text) = as_str.filter(|t| printable(t)) {
                    out.room_for(2 * text.len() + 2) && {
                        line.push(": ", Tok::Punct);
                        line.push(&escape(text), Tok::Str);
                        out.push(line)
                    }
                } else if !s.is_empty() && is_message(s, depth + 1) {
                    line.push(" {", Tok::Punct);
                    out.push(line) && emit(s, &mut 0, depth + 1, false, out) == Shown::Whole && out.push(closing(depth))
                } else {
                    out.room_for(4 * s.len() + 2) && {
                        line.push(": ", Tok::Punct);
                        match as_str {
                            Some(text) => line.push(&escape(text), Tok::Str),
                            None => line.push(&bytes_literal(s), Tok::Str),
                        };
                        out.push(line)
                    }
                }
            }
        };
        if !fits {
            return Shown::Full;
        }
    }
    if in_group { Shown::Open } else { Shown::Whole }
}

/// Decode a protobuf message without a schema, within the default [`Budget`].
pub fn decode_raw(b: &[u8]) -> Result<Vec<StyledLine>, String> {
    decode_raw_in(b, &mut Budget::default(), false)
}

/// [`decode_raw`] within `budget`: past it the last line says the rest is not shown. `cut` says
/// `b` is the start of a longer message: a field that goes on past its end is where it was cut,
/// and the fields before it are shown (when there are any: bytes that are no protobuf can look
/// like a field that goes on, too).
pub fn decode_raw_in(b: &[u8], budget: &mut Budget, cut: bool) -> Result<Vec<StyledLine>, String> {
    let mut end = 0;
    let b = match scan(b, &mut end, 0, None) {
        Ok(()) => b,
        Err(Fail::Short) if cut && end > 0 => &b[..end],
        Err(_) => return Err("not a valid protobuf message".to_string()),
    };
    let mut out = Out { lines: Vec::new(), budget };
    if emit(b, &mut 0, 0, false, &mut out) == Shown::Full {
        out.lines.push(cut_line());
    }
    Ok(out.lines)
}

/// The length-prefixed messages of a gRPC body, one at a time: `(compressed, message)`, or why
/// the framing is broken (and nothing after that).
pub fn grpc_frames(b: &[u8]) -> impl Iterator<Item = Result<(bool, &[u8]), String>> + '_ {
    let mut i = 0;
    let mut broken = false;
    std::iter::from_fn(move || {
        if broken || i >= b.len() {
            return None;
        }
        let hdr = b.get(i..i + 5);
        let len = hdr.map_or(0, |h| u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize);
        let frame = match (hdr, b.get(i + 5..i + 5 + len)) {
            (None, _) => Err("truncated gRPC frame header".to_string()),
            (Some(_), None) => Err("truncated gRPC message".to_string()),
            (Some(h), Some(msg)) => Ok((h[0] == 1, msg)),
        };
        broken = frame.is_err();
        i += 5 + len;
        Some(frame)
    })
}

/// Split a gRPC body into its length-prefixed messages: `(compressed, message)`.
pub fn grpc_messages(b: &[u8]) -> Result<Vec<(bool, &[u8])>, String> {
    grpc_frames(b).collect()
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
    fn fields_stop_at_the_budget() {
        // a thousand varints, each a line
        let msg = [0x08u8, 0x01].repeat(1000);
        let mut budget = Budget::new(10, 1 << 20);
        let lines = decode_raw_in(&msg, &mut budget, false).unwrap();
        assert_eq!(lines.len(), 11);
        assert_eq!(lines[10], cut_line());
        // nested messages and groups end where the budget does
        let nested = [0x1a, 0x04, 0x08, 0x01, 0x08, 0x02, 0x0b, 0x08, 0x03, 0x0c];
        let lines = decode_raw_in(&nested, &mut Budget::new(3, 1 << 20), false).unwrap();
        assert_eq!(text(&lines[..3]), "3 {\n  1: 1\n  1: 2");
        assert_eq!(lines[3], cut_line());
        assert_eq!(text(&decode_raw(&nested).unwrap()), "3 {\n  1: 1\n  1: 2\n}\n1 {\n  1: 3\n}");
        // a field of bytes four times as long escaped is not made when it cannot fit
        let mut blob = vec![0x12, 0xe8, 0x07];
        blob.extend(std::iter::repeat_n(0x01u8, 1000));
        let mut budget = Budget::new(10, 2000);
        assert_eq!(decode_raw_in(&blob, &mut budget, false).unwrap(), vec![cut_line()]);
        assert_eq!(decode_raw(&blob).unwrap()[0].text.len(), 3 + 4 * 1000 + 2);
    }

    /// The start of a longer message (the viewer formats the first 32 MB): the field the cut goes
    /// through is left out, wherever it falls; bytes that are no protobuf are still refused.
    #[test]
    fn a_message_cut_short_shows_the_fields_before_the_cut() {
        let msg = [
            0x08, 0x96, 0x01, 0x12, 0x07, b't', b'e', b's', b't', b'i', b'n', b'g', 0x0b, 0x08, 0x03, 0x0c, 0x21, 1, 0,
            0, 0, 0, 0, 0, 0,
        ];
        let whole = text(&decode_raw(&msg).unwrap());
        assert_eq!(whole, "1: 150\n2: \"testing\"\n1 {\n  1: 3\n}\n4: 0x0000000000000001");
        for end in 3..msg.len() {
            let lines = decode_raw_in(&msg[..end], &mut Budget::default(), true).unwrap();
            assert!(whole.starts_with(&text(&lines)), "cut at {end}: {:?}", text(&lines));
        }
        // a cut through the first field leaves nothing to show
        assert!(decode_raw_in(&msg[..2], &mut Budget::default(), true).is_err());
        // inside the group (which is not closed) and inside the string field
        assert!(decode_raw(&msg[..14]).is_err());
        assert_eq!(
            text(&decode_raw_in(&msg[..14], &mut Budget::default(), true).unwrap()),
            "1: 150\n2: \"testing\"\n1 {"
        );
        assert_eq!(text(&decode_raw_in(&msg[..8], &mut Budget::default(), true).unwrap()), "1: 150");
        assert!(decode_raw_in(&[0xff, 0xff], &mut Budget::default(), true).is_err());
        assert!(decode_raw_in(&[0x08, 0x01, 0x07], &mut Budget::default(), true).is_err(), "wire type 7");
    }

    #[test]
    fn splits_grpc_frames() {
        let body = [0, 0, 0, 0, 3, 0x08, 0x96, 0x01, 1, 0, 0, 0, 1, 0xaa];
        let msgs = grpc_messages(&body).unwrap();
        assert_eq!(msgs, vec![(false, &[0x08, 0x96, 0x01][..]), (true, &[0xaa][..])]);
        assert!(grpc_messages(&[0, 0, 0, 0, 9, 1]).is_err());
        let frames: Vec<_> = grpc_frames(&[0, 0, 0, 0, 1, 7, 0, 0]).collect();
        assert_eq!(frames, vec![Ok((false, &[7][..])), Err("truncated gRPC frame header".to_string())]);
    }
}
