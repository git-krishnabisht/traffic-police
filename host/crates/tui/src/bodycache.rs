//! Decoded body views: built on the UI task when small, on a worker when large, and kept in an
//! LRU cache with a memory budget (ARCHITECTURE.md §5.9).

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use traffic_police_core::decode::encoding::{self, DecodeError};
use traffic_police_core::model::{BodyDir, TxnIdx};
use traffic_police_core::store::BodyPieces;
use traffic_police_proto::Headers;

use crate::bodyview::BodyView;

/// Bodies up to this size, as captured and once decoded, are built on the UI task; larger ones
/// on a worker.
pub const INLINE_LIMIT: usize = 256 * 1024;

/// Whether a body is small enough to build on the UI task: at most [`INLINE_LIMIT`] as captured
/// and once its Content-Encoding is undone. A few KB of brotli or zstd, or 250 KB of gzip, can
/// hold 256 MB (up to 0.5.1 such a body was decoded and formatted on the UI task); finding out
/// decodes no more than the limit.
pub fn builds_inline(raw: &[u8], headers: Option<&Headers>) -> bool {
    if raw.len() > INLINE_LIMIT {
        return false;
    }
    let encodings = headers.map(encoding::encodings).unwrap_or_default();
    encodings.is_empty() || !matches!(encoding::decode(raw, &encodings, INLINE_LIMIT), Err(DecodeError::TooLarge(_)))
}
/// Memory budget for cached views (ARCHITECTURE.md §5.9).
pub const BUDGET: usize = 128 << 20;

/// Identifies a body's content at one moment: views are rebuilt when it changes (a streaming
/// body grows, a body completes).
pub type BodyKey = u64;

/// A view to build off the UI task.
#[derive(Debug, Clone)]
pub struct BodyJob {
    /// Session epoch (bumped by clearing the session, which reuses transaction indexes).
    pub epoch: u64,
    pub txn: TxnIdx,
    pub dir: BodyDir,
    pub key: BodyKey,
    /// Where the body's bytes are: read where the job runs, since joining a body's chunks or
    /// reading it back from the spill file takes a while for a large one (until 2026-10-14 the UI
    /// task read them to make the job).
    pub raw: BodyPieces,
    pub headers: Option<Headers>,
}

impl BodyJob {
    pub fn run(&self) -> BodyView {
        BodyView::build(self.raw.bytes(), self.headers.as_ref())
    }
}

struct Cached {
    key: BodyKey,
    view: BodyView,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
pub struct BodyCache {
    entries: HashMap<(TxnIdx, BodyDir), Cached>,
    /// Builds handed to a worker, by body, with the key being built.
    in_flight: HashMap<(TxnIdx, BodyDir), BodyKey>,
    queued: Vec<BodyJob>,
    total: usize,
    clock: u64,
    budget: usize,
    epoch: u64,
}

/// What [`BodyCache::lookup`] found.
pub enum Lookup {
    /// A view for the current content.
    Current,
    /// A view for older content (still shown while the new one is built), or none yet.
    Building { stale: bool },
    /// Nothing cached; build it now on this task.
    BuildHere,
}

impl BodyCache {
    pub fn new() -> Self {
        BodyCache { budget: BUDGET, ..Default::default() }
    }

    #[cfg(test)]
    fn with_budget(budget: usize) -> Self {
        BodyCache { budget, ..Default::default() }
    }

    /// Forget everything; results of builds already running are ignored.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.in_flight.clear();
        self.queued.clear();
        self.total = 0;
        self.epoch += 1;
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Decide how to serve a view of a body: `inline` says whether it is small enough to build
    /// here (see [`builds_inline`]; asked only when there is no view of it and no build).
    /// Queues a worker build for large bodies; `make_job` is only called then.
    pub fn lookup(
        &mut self,
        txn: TxnIdx,
        dir: BodyDir,
        key: BodyKey,
        inline: impl FnOnce() -> bool,
        make_job: impl FnOnce() -> BodyJob,
    ) -> Lookup {
        let id = (txn, dir);
        let cached = self.entries.get(&id).map(|e| e.key);
        if cached == Some(key) {
            return Lookup::Current;
        }
        // one build per body at a time; a newer key is picked up when it finishes
        if let Entry::Vacant(slot) = self.in_flight.entry(id) {
            if inline() {
                return Lookup::BuildHere;
            }
            slot.insert(key);
            self.queued.push(make_job());
        }
        Lookup::Building { stale: cached.is_some() }
    }

    pub fn has_queued(&self) -> bool {
        !self.queued.is_empty()
    }

    /// Whether any build is queued or running.
    pub fn building(&self) -> bool {
        !self.in_flight.is_empty()
    }

    pub fn is_building(&self, txn: TxnIdx, dir: BodyDir) -> bool {
        self.in_flight.contains_key(&(txn, dir))
    }

    pub fn take_jobs(&mut self) -> Vec<BodyJob> {
        std::mem::take(&mut self.queued)
    }

    /// A worker finished `job`.
    pub fn finish(&mut self, job: &BodyJob, view: BodyView) {
        if job.epoch != self.epoch {
            return;
        }
        let id = (job.txn, job.dir);
        if self.in_flight.get(&id) == Some(&job.key) {
            self.in_flight.remove(&id);
        }
        self.insert(job.txn, job.dir, job.key, view);
    }

    pub fn insert(&mut self, txn: TxnIdx, dir: BodyDir, key: BodyKey, view: BodyView) {
        self.clock += 1;
        let bytes = view.mem_bytes();
        if let Some(old) = self.entries.insert((txn, dir), Cached { key, view, bytes, used: self.clock }) {
            self.total -= old.bytes;
        }
        self.total += bytes;
        self.evict((txn, dir));
    }

    pub fn get_mut(&mut self, txn: TxnIdx, dir: BodyDir) -> Option<&mut BodyView> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(&(txn, dir)).map(|e| {
            e.used = clock;
            &mut e.view
        })
    }

    /// Drop least recently used views until the budget holds; `keep` always stays.
    fn evict(&mut self, keep: (TxnIdx, BodyDir)) {
        while self.total > self.budget && self.entries.len() > 1 {
            let victim =
                self.entries.iter().filter(|(id, _)| **id != keep).min_by_key(|(_, e)| e.used).map(|(id, _)| *id);
            match victim.and_then(|id| self.entries.remove(&id)) {
                Some(e) => self.total -= e.bytes,
                None => break,
            }
        }
    }

    pub fn mem_bytes(&self) -> usize {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn view(n: usize) -> BodyView {
        let body = Bytes::from(format!("[{}]", vec!["1"; n].join(",")));
        BodyView::build(body, Some(&vec![("content-type".into(), "application/json".into())]))
    }

    #[test]
    fn small_bodies_build_here_large_ones_on_a_worker() {
        let mut c = BodyCache::new();
        assert!(matches!(c.lookup(0, BodyDir::Response, 1, || true, || unreachable!()), Lookup::BuildHere));
        let job =
            || BodyJob { epoch: 0, txn: 1, dir: BodyDir::Response, key: 7, raw: BodyPieces::default(), headers: None };
        assert!(matches!(c.lookup(1, BodyDir::Response, 7, || false, job), Lookup::Building { stale: false }));
        // asking again does not queue a second build, nor look at the body again
        assert!(matches!(
            c.lookup(1, BodyDir::Response, 8, || unreachable!(), || unreachable!()),
            Lookup::Building { .. }
        ));
        let jobs = c.take_jobs();
        assert_eq!(jobs.len(), 1);
        c.finish(&jobs[0], view(3));
        assert!(!c.is_building(1, BodyDir::Response));
        // the body grew meanwhile: the old view is served while the new one builds
        let job =
            || BodyJob { epoch: 0, txn: 1, dir: BodyDir::Response, key: 8, raw: BodyPieces::default(), headers: None };
        assert!(matches!(c.lookup(1, BodyDir::Response, 8, || false, job), Lookup::Building { stale: true }));
        assert!(c.get_mut(1, BodyDir::Response).is_some());
    }

    /// What decides: the size as captured, and as decoded (asked of no more than the limit).
    #[test]
    fn compressed_bodies_that_decode_large_build_on_a_worker() {
        use traffic_police_backends::demo::content::gzip;
        let headers = |enc: &str| Some(vec![("Content-Encoding".to_string(), enc.to_string())]);
        assert!(builds_inline(&[b'x'; INLINE_LIMIT], None));
        assert!(!builds_inline(&[b'x'; INLINE_LIMIT + 1], None));
        assert!(builds_inline(&gzip(&[b'x'; INLINE_LIMIT]), headers("gzip").as_ref()));
        // 5 KB of gzip, 5 MB once decoded
        let bomb = gzip(&vec![0u8; 5 << 20]);
        assert!(bomb.len() < 8 << 10, "{}", bomb.len());
        assert!(!builds_inline(&bomb, headers("gzip").as_ref()));
        // what does not decode is shown as captured, which is small
        assert!(builds_inline(b"not gzip", headers("gzip").as_ref()));
        assert!(builds_inline(b"x", headers("snappy").as_ref()));
    }

    #[test]
    fn evicts_least_recently_used_over_budget() {
        let one = view(1000).mem_bytes();
        let mut c = BodyCache::with_budget(one * 2 + one / 2);
        c.insert(0, BodyDir::Response, 1, view(1000));
        c.insert(1, BodyDir::Response, 1, view(1000));
        assert!(c.get_mut(0, BodyDir::Response).is_some()); // 0 is now more recent than 1
        c.insert(2, BodyDir::Response, 1, view(1000));
        assert!(c.get_mut(1, BodyDir::Response).is_none(), "1 was least recently used");
        assert!(c.get_mut(0, BodyDir::Response).is_some());
        assert!(c.get_mut(2, BodyDir::Response).is_some());
        assert!(c.mem_bytes() <= one * 2 + one / 2);
    }
}
