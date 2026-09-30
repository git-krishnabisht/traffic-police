//! Body bytes, kept as the chunks they arrived in (ARCHITECTURE.md §5.6). Past a memory budget
//! (256 MiB by default) the oldest bodies move to the spill file; readers do not notice.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};

use super::spill::SpillFile;
use crate::model::BodyId;

/// Bytes held in memory before bodies start moving to disk.
pub const DEFAULT_MEMORY_BUDGET: u64 = 256 << 20;

/// Bodies with at least this much in memory move first.
const LARGE: u64 = 64 << 10;

#[derive(Debug, Clone)]
enum Segment {
    Mem(Bytes),
    Disk { offset: u64, len: u64 },
}

#[derive(Debug, Default, Clone)]
struct BodyData {
    segments: Vec<Segment>,
    /// Next expected offset (bytes held, counting any gaps as skipped).
    next_offset: u64,
    held: u64,
    /// The part of `held` that is in memory.
    in_mem: u64,
    gap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    /// New bytes stored (overlap with what we already had is discarded).
    pub added: u64,
    /// This chunk started after the end of what we had: bytes were lost on the device.
    pub gap: bool,
}

/// Clones (the UI's frozen snapshot) share the spill file, which is append-only.
#[derive(Debug, Clone)]
pub struct BodyStore {
    bodies: Vec<BodyData>,
    mem_bytes: u64,
    budget: u64,
    spill: Option<Arc<SpillFile>>,
    spilled_bytes: u64,
}

impl Default for BodyStore {
    fn default() -> Self {
        Self::with_budget(DEFAULT_MEMORY_BUDGET)
    }
}

impl BodyStore {
    pub fn with_budget(budget: u64) -> Self {
        BodyStore { bodies: Vec::new(), mem_bytes: 0, budget, spill: None, spilled_bytes: 0 }
    }

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
        body.segments.push(Segment::Mem(fresh));
        body.next_offset = end;
        body.held += added;
        body.in_mem += added;
        body.gap |= gap;
        self.mem_bytes += added;
        if self.mem_bytes > self.budget {
            self.spill_oldest();
        }
        Appended { added, gap }
    }

    /// Moves the oldest bodies (large ones first) to the spill file until memory is back to
    /// three quarters of the budget.
    fn spill_oldest(&mut self) {
        let file = match &self.spill {
            Some(f) => f.clone(),
            None => match SpillFile::create() {
                Ok(f) => self.spill.insert(Arc::new(f)).clone(),
                Err(e) => {
                    tracing::warn!("cannot create a spill file, keeping all bodies in memory: {e}");
                    self.budget = u64::MAX;
                    return;
                }
            },
        };
        let target = self.budget / 4 * 3;
        for min in [LARGE, 1] {
            for body in &mut self.bodies {
                if self.mem_bytes <= target {
                    return;
                }
                if body.in_mem < min {
                    continue;
                }
                let before = body.in_mem;
                if let Err(e) = spill_body(&file, body) {
                    tracing::warn!("spilling a body failed, keeping bodies in memory: {e}");
                    self.budget = u64::MAX;
                    return;
                }
                self.mem_bytes -= before;
                self.spilled_bytes += before;
            }
        }
    }

    /// All held bytes, in order (gaps are simply missing).
    pub fn bytes(&self, id: BodyId) -> Bytes {
        let body = &self.bodies[id as usize];
        match body.segments.as_slice() {
            [] => Bytes::new(),
            [Segment::Mem(b)] => b.clone(),
            segments => {
                let mut out = BytesMut::with_capacity(body.held as usize);
                for s in segments {
                    match s {
                        Segment::Mem(b) => out.extend_from_slice(b),
                        Segment::Disk { offset, len } => {
                            let read = match &self.spill {
                                Some(f) => f.read(*offset, *len, &mut out),
                                None => Err(std::io::Error::other("no spill file")),
                            };
                            if let Err(e) = read {
                                tracing::warn!("reading spilled body bytes failed: {e}");
                                break;
                            }
                        }
                    }
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

    /// Total bytes moved to the spill file.
    pub fn spilled_bytes(&self) -> u64 {
        self.spilled_bytes
    }
}

/// Writes each run of in-memory segments to the file and replaces it with a disk segment.
fn spill_body(file: &SpillFile, body: &mut BodyData) -> std::io::Result<()> {
    let mut out = Vec::with_capacity(body.segments.len());
    let mut run: Vec<&Bytes> = Vec::new();
    for s in &body.segments {
        match s {
            Segment::Mem(b) => run.push(b),
            disk @ Segment::Disk { .. } => {
                if !run.is_empty() {
                    let (offset, len) = file.append(&run)?;
                    out.push(Segment::Disk { offset, len });
                    run.clear();
                }
                out.push(disk.clone());
            }
        }
    }
    if !run.is_empty() {
        let (offset, len) = file.append(&run)?;
        out.push(Segment::Disk { offset, len });
    }
    body.segments = out;
    body.in_mem = 0;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(n: usize, seed: u8) -> Bytes {
        Bytes::from((0..n).map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed)).collect::<Vec<u8>>())
    }

    fn joined(parts: &[Bytes]) -> Vec<u8> {
        parts.iter().flat_map(|b| b.iter().copied()).collect()
    }

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

    #[test]
    fn spills_the_oldest_bodies_past_the_budget_and_reads_them_back() {
        let mut s = BodyStore::with_budget(1000);
        let a = s.alloc();
        let b = s.alloc();
        s.append(a, 0, pattern(300, 1));
        s.append(b, 0, pattern(400, 2));
        assert_eq!(s.spilled_bytes(), 0);
        // a streaming body pushes memory over the budget: the oldest bodies go to disk
        let c = s.alloc();
        s.append(c, 0, pattern(200, 3));
        s.append(c, 200, pattern(200, 4));
        assert!(s.mem_bytes() <= 750, "{}", s.mem_bytes());
        assert_eq!(s.mem_bytes() + s.spilled_bytes(), 1100);
        // more bytes for a spilled body: disk and memory parts read back in order
        s.append(a, 300, pattern(50, 5));
        assert_eq!(&s.bytes(a)[..], &joined(&[pattern(300, 1), pattern(50, 5)])[..]);
        assert_eq!(&s.bytes(b)[..], &pattern(400, 2)[..]);
        assert_eq!(&s.bytes(c)[..], &joined(&[pattern(200, 3), pattern(200, 4)])[..]);
        // a clone (the UI's frozen snapshot) reads the same bytes from the shared file
        let snapshot = s.clone();
        assert_eq!(&snapshot.bytes(a)[..], &joined(&[pattern(300, 1), pattern(50, 5)])[..]);
    }
}
