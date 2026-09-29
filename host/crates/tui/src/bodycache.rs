//! Decoded body views: built on the UI task when small, on a worker when large, and kept in an
//! LRU cache with a memory budget (ARCHITECTURE.md §5.9).

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use bytes::Bytes;
use traffic_police_core::model::{BodyDir, TxnIdx};
use traffic_police_proto::Headers;

use crate::bodyview::BodyView;

/// Bodies up to this size (as captured) are decoded on the UI task; larger ones on a worker.
pub const INLINE_LIMIT: usize = 256 * 1024;
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
    pub raw: Bytes,
    pub headers: Option<Headers>,
}

impl BodyJob {
    pub fn run(&self) -> BodyView {
        BodyView::build(self.raw.clone(), self.headers.as_ref())
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

    /// Decide how to serve a view of `raw` (captured size `raw_len`). Queues a worker build for
    /// large bodies; `make_job` is only called then.
    pub fn lookup(
        &mut self,
        txn: TxnIdx,
        dir: BodyDir,
        key: BodyKey,
        raw_len: usize,
        make_job: impl FnOnce() -> BodyJob,
    ) -> Lookup {
        let id = (txn, dir);
        let cached = self.entries.get(&id).map(|e| e.key);
        if cached == Some(key) {
            return Lookup::Current;
        }
        if raw_len <= INLINE_LIMIT {
            return Lookup::BuildHere;
        }
        // one build per body at a time; a newer key is picked up when it finishes
        if let Entry::Vacant(slot) = self.in_flight.entry(id) {
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
    use super::*;

    fn view(n: usize) -> BodyView {
        let body = Bytes::from(format!("[{}]", vec!["1"; n].join(",")));
        BodyView::build(body, Some(&vec![("content-type".into(), "application/json".into())]))
    }

    #[test]
    fn small_bodies_build_here_large_ones_on_a_worker() {
        let mut c = BodyCache::new();
        assert!(matches!(c.lookup(0, BodyDir::Response, 1, 10, || unreachable!()), Lookup::BuildHere));
        let job = || BodyJob { epoch: 0, txn: 1, dir: BodyDir::Response, key: 7, raw: Bytes::new(), headers: None };
        assert!(matches!(c.lookup(1, BodyDir::Response, 7, INLINE_LIMIT + 1, job), Lookup::Building { stale: false }));
        // asking again does not queue a second build
        assert!(matches!(
            c.lookup(1, BodyDir::Response, 8, INLINE_LIMIT + 2, || unreachable!()),
            Lookup::Building { .. }
        ));
        let jobs = c.take_jobs();
        assert_eq!(jobs.len(), 1);
        c.finish(&jobs[0], view(3));
        assert!(!c.is_building(1, BodyDir::Response));
        // the body grew meanwhile: the old view is served while the new one builds
        let job = || BodyJob { epoch: 0, txn: 1, dir: BodyDir::Response, key: 8, raw: Bytes::new(), headers: None };
        assert!(matches!(c.lookup(1, BodyDir::Response, 8, INLINE_LIMIT + 2, job), Lookup::Building { stale: true }));
        assert!(c.get_mut(1, BodyDir::Response).is_some());
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
