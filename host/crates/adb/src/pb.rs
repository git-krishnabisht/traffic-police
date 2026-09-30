//! Just enough protobuf decoding for adb's `Devices` and `AppProcesses` messages.

/// One field of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(u64),
    Fixed32(u32),
}

impl<'a> Value<'a> {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Varint(v) | Value::Fixed64(v) => Some(*v),
            Value::Fixed32(v) => Some(u64::from(*v)),
            Value::Bytes(_) => None,
        }
    }

    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            Value::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

fn varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The fields of a message in order, or `None` if it is malformed.
pub fn fields(buf: &[u8]) -> Option<Vec<(u32, Value<'_>)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let key = varint(buf, &mut pos)?;
        let field = u32::try_from(key >> 3).ok()?;
        let value = match key & 7 {
            0 => Value::Varint(varint(buf, &mut pos)?),
            1 => {
                let b: [u8; 8] = buf.get(pos..pos + 8)?.try_into().ok()?;
                pos += 8;
                Value::Fixed64(u64::from_le_bytes(b))
            }
            2 => {
                let len = usize::try_from(varint(buf, &mut pos)?).ok()?;
                let b = buf.get(pos..pos.checked_add(len)?)?;
                pos += len;
                Value::Bytes(b)
            }
            5 => {
                let b: [u8; 4] = buf.get(pos..pos + 4)?.try_into().ok()?;
                pos += 4;
                Value::Fixed32(u32::from_le_bytes(b))
            }
            _ => return None,
        };
        out.push((field, value));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_varints_strings_and_rejects_garbage() {
        // field 1 = "ab", field 2 = 300
        let f = fields(&[0x0a, 0x02, b'a', b'b', 0x10, 0xac, 0x02]).unwrap();
        assert_eq!(f[0], (1, Value::Bytes(b"ab")));
        assert_eq!(f[1], (2, Value::Varint(300)));
        assert!(fields(&[0x0a, 0x05, b'a']).is_none());
        assert!(fields(&[0x0f]).is_none());
    }
}
