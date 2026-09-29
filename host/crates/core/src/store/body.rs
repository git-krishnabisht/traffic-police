//! Body bytes, kept as the chunks they arrived in (ARCHITECTURE.md §5.6).
//!
//! Spilling to disk above a memory budget comes with the device backends (Phase 1); the API
//! already hides where bytes live.

use bytes::{Bytes, BytesMut};

use crate::model::BodyId;

#[derive(Debug, Default, Clone)]
struct BodyData {
    chunks: Vec<Bytes>,
    /// Next expected offset (bytes held, counting any gaps as skipped).
    next_offset: u64,
    held: u64,
    gap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    /// New bytes stored (overlap with what we already had is discarded).
    pub added: u64,
    /// This chunk started after the end of what we had: bytes were lost on the device.
    pub gap: bool,
}

#[derive(Debug, Default, Clone)]
pub struct BodyStore {
    bodies: Vec<BodyData>,
    mem_bytes: u64,
}

impl BodyStore {
    pub fn alloc(&mut self) -> BodyId {
        self.bodies.push(BodyData::default());
        (self.bodies.len() - 1) as BodyId
    }

    pub fn append(&mut self, id: BodyId, offset: u64, bytes: Bytes) -> Appended {
        let body = &mut self.bodies[id as usize];
        let end = offset + bytes.len() as u64;
        if end <= body.next_offset {
            return Appended { added: 0, gap: false }; // duplicate
        }
        let gap = offset > body.next_offset;
        let skip = body.next_offset.saturating_sub(offset) as usize;
        let fresh = bytes.slice(skip..);
        let added = fresh.len() as u64;
        body.chunks.push(fresh);
        body.next_offset = end;
        body.held += added;
        body.gap |= gap;
        self.mem_bytes += added;
        Appended { added, gap }
    }

    /// All held bytes, in order (gaps are simply missing).
    pub fn bytes(&self, id: BodyId) -> Bytes {
        let body = &self.bodies[id as usize];
        match body.chunks.len() {
            0 => Bytes::new(),
            1 => body.chunks[0].clone(),
            _ => {
                let mut out = BytesMut::with_capacity(body.held as usize);
                for c in &body.chunks {
                    out.extend_from_slice(c);
                }
                out.freeze()
            }
        }
    }

    pub fn held(&self, id: BodyId) -> u64 {
        self.bodies[id as usize].held
    }

    pub fn has_gap(&self, id: BodyId) -> bool {
        self.bodies[id as usize].gap
    }

    /// Total bytes held in memory across all bodies.
    pub fn mem_bytes(&self) -> u64 {
        self.mem_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_dedupes_and_detects_gaps() {
        let mut s = BodyStore::default();
        let id = s.alloc();
        assert_eq!(s.append(id, 0, Bytes::from_static(b"hello ")), Appended { added: 6, gap: false });
        assert_eq!(s.append(id, 0, Bytes::from_static(b"hello ")), Appended { added: 0, gap: false });
        assert_eq!(s.append(id, 3, Bytes::from_static(b"lo world")), Appended { added: 5, gap: false });
        assert_eq!(&s.bytes(id)[..], b"hello world");
        assert_eq!(s.append(id, 20, Bytes::from_static(b"!")), Appended { added: 1, gap: true });
        assert!(s.has_gap(id));
        assert_eq!(&s.bytes(id)[..], b"hello world!");
        assert_eq!(s.held(id), 12);
    }
}
