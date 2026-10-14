//! Body bytes, kept as the chunks they arrived in (ARCHITECTURE.md §5.6). Past a memory budget
//! (256 MiB by default) the oldest bodies move to the spill file; readers do not notice.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};

use super::spill::SpillFile;
use crate::model::BodyId;

/// Bytes held in memory before bodies start moving to disk.
pub const DEFAULT_MEMORY_BUDGET: u64 = 256 << 20;

/// The budget of new stores (`[storage] memory`).
static BUDGET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(DEFAULT_MEMORY_BUDGET);

/// Sets the memory budget of stores created from now on.
pub fn set_default_budget(bytes: u64) {
    BUDGET.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

/// Bodies with at least this much in memory move first.
const LARGE: u64 = 64 << 10;

#[derive(Debug, Clone)]
enum Segment {
    Mem(Bytes),
    Disk { offset: u64, len: u64 },
}

impl Segment {
    fn len(&self) -> u64 {
        match self {
            Segment::Mem(b) => b.len() as u64,
            Segment::Disk { len, .. } => *len,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct BodyData {
    segments: Vec<Segment>,
    /// Where each segment starts in what is held (to read part of a body).
    starts: Vec<u64>,
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
    /// Ids of bodies let go ([`BodyStore::release`]), handed out again.
    free: Vec<BodyId>,
    mem_bytes: u64,
    budget: u64,
    spill: Option<Arc<SpillFile>>,
    spilled_bytes: u64,
}

impl Default for BodyStore {
    fn default() -> Self {
        Self::with_budget(BUDGET.load(std::sync::atomic::Ordering::Relaxed))
    }
}

impl BodyStore {
    pub fn with_budget(budget: u64) -> Self {
        BodyStore { bodies: Vec::new(), free: Vec::new(), mem_bytes: 0, budget, spill: None, spilled_bytes: 0 }
    }

    pub fn alloc(&mut self) -> BodyId {
        if let Some(id) = self.free.pop() {
            return id;
        }
        self.bodies.push(BodyData::default());
        (self.bodies.len() - 1) as BodyId
    }

    /// Lets a body go: its bytes in memory are freed (what it had in the spill file stays there,
    /// unread) and its id is handed out again. Only for a body nothing refers to any more.
    pub fn release(&mut self, id: BodyId) {
        let body = std::mem::take(&mut self.bodies[id as usize]);
        self.mem_bytes -= body.in_mem;
        self.free.push(id);
    }

    pub fn append(&mut self, id: BodyId, offset: u64, bytes: Bytes) -> Appended {
        let body = &mut self.bodies[id as usize];
        // an offset no body reaches comes only from a broken stream: drop the chunk
        let Some(end) = offset.checked_add(bytes.len() as u64) else {
            return Appended { added: 0, gap: false };
        };
        if end <= body.next_offset {
            return Appended { added: 0, gap: false }; // duplicate
        }
        let gap = offset > body.next_offset;
        let skip = body.next_offset.saturating_sub(offset) as usize;
        // a chunk is a view into the buffer it was read into (64 KB or more, a whole inflated
        // piece of a session file): kept as it is, it keeps that buffer alive while the budget
        // counts only the chunk, and spilling it frees nothing
        let fresh = Bytes::copy_from_slice(&bytes[skip..]);
        let added = fresh.len() as u64;
        body.starts.push(body.held);
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

    /// `len` bytes of what is held of a body, from `offset` on (a WebSocket message's payload,
    /// among the socket's others): from memory or the spill file, as they are.
    pub fn slice(&self, id: BodyId, offset: u64, len: u64) -> Bytes {
        let body = &self.bodies[id as usize];
        let end = offset.saturating_add(len).min(body.held);
        if offset >= end {
            return Bytes::new();
        }
        // the segment that holds `offset`: the last one that starts at or before it
        let first = body.starts.partition_point(|&s| s <= offset).saturating_sub(1);
        let mut out = BytesMut::new();
        for (seg, &start) in body.segments[first..].iter().zip(&body.starts[first..]) {
            if start >= end {
                break;
            }
            let (from, to) = (offset.saturating_sub(start), (end - start).min(seg.len()));
            match seg {
                // within one segment in memory: a view of it
                Segment::Mem(b) if out.is_empty() && end - start <= seg.len() => {
                    return b.slice(from as usize..to as usize);
                }
                Segment::Mem(b) => out.extend_from_slice(&b[from as usize..to as usize]),
                Segment::Disk { offset: at, .. } => {
                    let read = match &self.spill {
                        Some(f) => f.read(at + from, to - from, &mut out),
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

/// A body's bytes as the store holds them at one moment, to read later on another thread: the
/// chunks in memory (shared, not copied) and where the spilled ones are in the spill file. The
/// file is append-only and the chunks are shared, so what the store does meanwhile (more bytes,
/// spilling) does not change what these read. What a `body:` search reads, so that the UI task
/// copies no bytes and reads nothing back from disk to make one.
#[derive(Debug, Clone, Default)]
pub struct BodyPieces {
    segments: Vec<Segment>,
    held: u64,
    spill: Option<Arc<SpillFile>>,
}

impl BodyStore {
    /// `id`'s bytes as held now, to read later ([`BodyPieces::bytes`]); nothing is copied here.
    pub fn pieces(&self, id: BodyId) -> BodyPieces {
        let body = &self.bodies[id as usize];
        BodyPieces { segments: body.segments.clone(), held: body.held, spill: self.spill.clone() }
    }
}

impl BodyPieces {
    /// How many bytes [`BodyPieces::bytes`] gives.
    pub fn len(&self) -> u64 {
        self.held
    }

    pub fn is_empty(&self) -> bool {
        self.held == 0
    }

    /// All the bytes, in order, as [`BodyStore::bytes`] gives them (gaps are simply missing); the
    /// spilled ones are read back from the spill file.
    pub fn bytes(&self) -> Bytes {
        match self.segments.as_slice() {
            [] => Bytes::new(),
            [Segment::Mem(b)] => b.clone(),
            segments => {
                let mut out = BytesMut::with_capacity(self.held as usize);
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
    body.starts = out.iter().scan(0, |at, s| Some(std::mem::replace(at, *at + s.len()))).collect();
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

    /// Up to 0.5.1 every read buffer stayed in memory as long as any chunk of it did, and
    /// spilling the chunk freed nothing.
    #[test]
    fn chunks_are_kept_as_copies_not_views_of_the_buffer_they_were_read_into() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct ReadBuffer(Vec<u8>, Arc<AtomicBool>);
        impl AsRef<[u8]> for ReadBuffer {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }
        impl Drop for ReadBuffer {
            fn drop(&mut self) {
                self.1.store(true, Ordering::SeqCst);
            }
        }
        let freed = Arc::new(AtomicBool::new(false));
        let read = Bytes::from_owner(ReadBuffer(vec![7u8; 64 << 10], freed.clone()));
        let mut s = BodyStore::default();
        let id = s.alloc();
        s.append(id, 0, read.slice(100..110));
        s.append(id, 5, read.slice(200..220));
        drop(read);
        assert!(freed.load(Ordering::SeqCst), "the read buffer went with its frames");
        assert_eq!(&s.bytes(id)[..], &[7u8; 25]);
        assert_eq!(s.mem_bytes(), 25);
    }

    /// Part of a body, as a WebSocket message's payload is read: from one segment, across
    /// several, and from the spill file once the body went there.
    #[test]
    fn parts_of_a_body_read_back_from_memory_and_disk() {
        let mut s = BodyStore::with_budget(1000);
        let id = s.alloc();
        let parts: Vec<Bytes> = (0..6).map(|i| pattern(100, i)).collect();
        for (i, p) in parts.iter().enumerate() {
            s.append(id, i as u64 * 100, p.clone());
        }
        let all = joined(&parts);
        let check = |s: &BodyStore| {
            for (offset, len) in [(0, 100), (100, 100), (150, 100), (0, 600), (550, 100), (599, 1), (600, 5), (10, 0)] {
                let end = (offset + len).min(600);
                assert_eq!(&s.slice(id, offset as u64, len as u64)[..], &all[offset.min(end)..end], "{offset}+{len}");
            }
        };
        check(&s);
        // another body pushes this one over the budget, onto the disk
        let other = s.alloc();
        s.append(other, 0, pattern(500, 9));
        assert!(s.spilled_bytes() >= 600, "{}", s.spilled_bytes());
        check(&s);
        // more after the spill: part on disk, part in memory
        s.append(id, 600, pattern(50, 7));
        assert_eq!(&s.slice(id, 580, 70)[..], &[&all[580..], &pattern(50, 7)[..]].concat()[..]);
    }

    #[test]
    fn released_bodies_free_their_memory_and_their_ids_come_back() {
        let mut s = BodyStore::default();
        let a = s.alloc();
        s.append(a, 0, pattern(300, 1));
        let b = s.alloc();
        s.append(b, 0, pattern(200, 2));
        s.release(a);
        assert_eq!(s.mem_bytes(), 200);
        let c = s.alloc();
        assert_eq!(c, a, "the id is used again");
        assert_eq!((s.held(c), s.bytes(c).len()), (0, 0), "empty");
        assert_eq!(&s.bytes(b)[..], &pattern(200, 2)[..]);
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

    #[test]
    fn pieces_read_what_the_store_held_when_they_were_taken() {
        let mut s = BodyStore::with_budget(1000);
        let a = s.alloc();
        s.append(a, 0, pattern(300, 1));
        s.append(a, 300, pattern(300, 2));
        let before = s.pieces(a);
        // more bytes, and the body goes to the spill file: the pieces still read the 600 bytes
        let b = s.alloc();
        s.append(b, 0, pattern(900, 3));
        s.append(a, 600, pattern(100, 4));
        assert!(s.spilled_bytes() > 0);
        assert_eq!(before.len(), 600);
        assert_eq!(&before.bytes()[..], &joined(&[pattern(300, 1), pattern(300, 2)])[..]);
        // taken now, partly from the file: what the store reads
        let after = s.pieces(a);
        assert_eq!(after.bytes(), s.bytes(a));
        assert_eq!(after.len(), 700);
        assert!(BodyPieces::default().bytes().is_empty());
    }
}
