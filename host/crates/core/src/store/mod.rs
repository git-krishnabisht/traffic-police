//! The session store: the single owner of captured state (ARCHITECTURE.md §5.6).
//!
//! Backends never touch it; the UI task (or a headless command) applies [`SessionEvent`]s in
//! order. Transactions are `Arc`s updated copy-on-write, so a frozen view can keep an old
//! snapshot cheaply.

pub mod body;
pub mod traffic;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use traffic_police_proto::msg::DeliveredResponse;

use crate::event::{MarkerKind, SessionEvent};
use crate::fmt::Ts;
use crate::model::{
    BodyDir, BodyMeta, BodyState, ClientInfo, Failure, ResponseInfo, RuleHit, SourceId, SourceInfo, ThreadInfo,
    Transaction, TxnIdx, TxnKey, TxnState, Url,
};

pub use body::BodyStore;
pub use traffic::{Buckets, GraphSource, TrafficSeries};

/// Hands out source ids; clone it into every backend.
#[derive(Debug, Clone, Default)]
pub struct SourceIds(Arc<AtomicU32>);

impl SourceIds {
    pub fn next(&self) -> SourceId {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub at: Ts,
    pub source: Option<SourceId>,
    pub kind: MarkerKind,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub at: Ts,
    pub source: SourceId,
    pub level: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub requests: u64,
    pub failed: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub dropped_events: u64,
    pub rule_modified: u64,
}

/// One lane of the Thread View.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub thread: ThreadInfo,
    pub txns: Vec<TxnIdx>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionStore {
    ids: SourceIds,
    origin: Option<Ts>,
    latest: Ts,
    sources: BTreeMap<SourceId, SourceInfo>,
    txns: Vec<Arc<Transaction>>,
    index: HashMap<TxnKey, TxnIdx>,
    bodies: BodyStore,
    traffic: TrafficSeries,
    lanes: Vec<Lane>,
    lane_index: HashMap<(SourceId, i64, String), u32>,
    markers: Vec<Marker>,
    diagnostics: Vec<Diagnostic>,
    stats: Stats,
    generation: u64,
    changed: Vec<TxnIdx>,
}

/// Approximate size of a header block on the wire (for the captured-traffic series).
fn header_bytes(headers: &[(String, String)]) -> u64 {
    headers.iter().map(|(n, v)| (n.len() + v.len() + 4) as u64).sum::<u64>() + 2
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The allocator backends use for source ids.
    pub fn source_ids(&self) -> SourceIds {
        self.ids.clone()
    }

    /// Session time zero (earliest event seen).
    pub fn origin(&self) -> Ts {
        self.origin.unwrap_or(0)
    }

    /// Latest event time seen.
    pub fn latest(&self) -> Ts {
        self.latest
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.txns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txns.is_empty()
    }

    pub fn txn(&self, idx: TxnIdx) -> &Transaction {
        &self.txns[idx as usize]
    }

    pub fn txns(&self) -> &[Arc<Transaction>] {
        &self.txns
    }

    pub fn find(&self, key: TxnKey) -> Option<TxnIdx> {
        self.index.get(&key).copied()
    }

    pub fn sources(&self) -> impl Iterator<Item = &SourceInfo> {
        self.sources.values()
    }

    pub fn source(&self, id: SourceId) -> Option<&SourceInfo> {
        self.sources.get(&id)
    }

    /// The most recently started source.
    pub fn current_source(&self) -> Option<&SourceInfo> {
        self.sources.values().max_by_key(|s| s.started)
    }

    pub fn lanes(&self) -> &[Lane] {
        &self.lanes
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn traffic(&self) -> &TrafficSeries {
        &self.traffic
    }

    pub fn bodies(&self) -> &BodyStore {
        &self.bodies
    }

    /// Captured bytes of a body.
    pub fn body_bytes(&self, meta: &BodyMeta) -> Bytes {
        meta.id.map(|id| self.bodies.bytes(id)).unwrap_or_default()
    }

    /// Transactions changed since the last call.
    pub fn drain_changed(&mut self) -> Vec<TxnIdx> {
        std::mem::take(&mut self.changed)
    }

    /// Wall-clock milliseconds for a timestamp, using the closest source's clock pair.
    pub fn wall_ms(&self, ts: Ts) -> Option<i64> {
        self.sources.values().filter(|s| s.clock.is_some()).min_by_key(|s| s.started.abs_diff(ts))?.wall_ms(ts)
    }

    pub fn apply_all(&mut self, events: impl IntoIterator<Item = SessionEvent>) {
        for e in events {
            self.apply(e);
        }
    }

    fn see(&mut self, at: Ts) {
        if at > self.latest {
            self.latest = at;
        }
        if self.origin.is_none_or(|o| at < o) {
            self.origin = Some(at);
        }
    }

    fn idx_or_placeholder(&mut self, key: TxnKey, at: Ts) -> TxnIdx {
        if let Some(&i) = self.index.get(&key) {
            return i;
        }
        let idx = self.txns.len() as TxnIdx;
        self.txns.push(Arc::new(Transaction::new_placeholder(key, at)));
        self.index.insert(key, idx);
        idx
    }

    fn touch(&mut self, idx: TxnIdx) -> &mut Transaction {
        if self.changed.last() != Some(&idx) {
            self.changed.push(idx);
        }
        Arc::make_mut(&mut self.txns[idx as usize])
    }

    fn lane_for(&mut self, thread: &ThreadInfo) -> u32 {
        let k = (thread.source, thread.id, thread.name.clone());
        if let Some(&l) = self.lane_index.get(&k) {
            return l;
        }
        let l = self.lanes.len() as u32;
        self.lanes.push(Lane { thread: thread.clone(), txns: Vec::new() });
        self.lane_index.insert(k, l);
        l
    }

    fn body_meta(t: &mut Transaction, dir: BodyDir) -> &mut BodyMeta {
        match dir {
            BodyDir::Request => &mut t.req_body,
            BodyDir::Response => &mut t.resp_body,
            BodyDir::Delivered => t.delivered_body.get_or_insert_with(BodyMeta::default),
        }
    }

    /// Account bytes the body passed through, for the captured-traffic series and totals.
    fn account(&mut self, dir: BodyDir, at: Ts, delta: u64) {
        match dir {
            BodyDir::Request => {
                self.traffic.add_captured(at, 0, delta);
                self.stats.bytes_out += delta;
            }
            BodyDir::Response => {
                self.traffic.add_captured(at, delta, 0);
                self.stats.bytes_in += delta;
            }
            BodyDir::Delivered => {}
        }
    }

    pub fn apply(&mut self, event: SessionEvent) {
        self.generation += 1;
        match event {
            SessionEvent::SourceUp(info) => {
                self.see(info.started);
                self.markers.push(Marker {
                    at: info.started,
                    source: Some(info.id),
                    kind: if self.sources.is_empty() { MarkerKind::Attach } else { MarkerKind::Reattach },
                    label: format!("{} pid {}", info.process, info.pid),
                });
                self.sources.insert(info.id, *info);
            }
            SessionEvent::SourceDown { source, at, reason } => {
                self.see(at);
                if let Some(s) = self.sources.get_mut(&source) {
                    s.ended = Some((at, reason.clone()));
                }
                let open: Vec<TxnIdx> = self
                    .txns
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.key.source == source && t.state.is_open())
                    .map(|(i, _)| i as TxnIdx)
                    .collect();
                for i in open {
                    let t = self.touch(i);
                    t.state = TxnState::Detached;
                    t.end = Some(at);
                }
                self.markers.push(Marker { at, source: Some(source), kind: MarkerKind::Detach, label: reason });
            }
            SessionEvent::Clock { source, ts, wall_ms } => {
                if let Some(s) = self.sources.get_mut(&source) {
                    s.clock = Some((ts, wall_ms));
                }
            }
            SessionEvent::Request(r) => {
                self.see(r.at);
                let r = *r;
                let idx = self.idx_or_placeholder(r.key, r.at);
                let lane = r.thread.as_ref().map(|t| {
                    let info =
                        ThreadInfo { source: r.key.source, id: t.id, name: t.name.clone(), origin: t.origin.clone() };
                    (self.lane_for(&info), info)
                });
                let req_hdr = header_bytes(&r.headers) + (r.method.len() + r.url.len() + 12) as u64;
                self.traffic.add_captured(r.at, 0, req_hdr);
                self.stats.bytes_out += req_hdr;
                self.stats.requests += 1;
                let start = r.marks.iter().map(|&(_, t)| t).chain([r.at]).min().unwrap_or(r.at);
                let t = self.touch(idx);
                t.placeholder = false;
                t.call = r.call;
                t.hop = r.hop;
                t.client = r.client.map(|c| ClientInfo { kind: c.kind, version: c.version });
                t.method = r.method;
                t.url = Url::parse(&r.url);
                t.req_headers = r.headers;
                t.req_body.state = if r.body.is_some() { BodyState::Pending } else { BodyState::None };
                t.stack = Arc::from(r.stack);
                t.stack_truncated = r.stack_truncated;
                let mut marks = r.marks;
                marks.append(&mut t.marks);
                marks.sort_by_key(|&(_, ts)| ts);
                t.marks = marks;
                t.req_at = r.at;
                t.start = start.min(t.start);
                if t.state == TxnState::Waiting && t.resp.is_none() {
                    t.state = TxnState::Sending;
                }
                if r.conn.is_some() {
                    t.conn = r.conn;
                }
                if let Some((l, info)) = lane {
                    t.lane = Some(l);
                    t.thread = Some(info);
                    self.lanes[l as usize].txns.push(idx);
                }
            }
            SessionEvent::Response(r) => {
                self.see(r.at);
                let r = *r;
                let idx = self.idx_or_placeholder(r.key, r.at);
                let resp_hdr = header_bytes(&r.headers) + 16;
                self.traffic.add_captured(r.at, resp_hdr, 0);
                self.stats.bytes_in += resp_hdr;
                let t = self.touch(idx);
                if t.state.is_open() {
                    t.state = TxnState::Receiving;
                }
                if r.conn.is_some() {
                    t.conn = r.conn;
                }
                t.resp = Some(ResponseInfo {
                    at: r.at,
                    status: r.status,
                    message: r.message,
                    protocol: r.protocol,
                    headers: r.headers,
                });
            }
            SessionEvent::Body { key, dir, at, offset, bytes } => {
                self.see(at);
                let idx = self.idx_or_placeholder(key, at);
                let len = bytes.len() as u64;
                let id = {
                    let t = self.touch(idx);
                    let meta = Self::body_meta(t, dir);
                    meta.id
                };
                let id = id.unwrap_or_else(|| self.bodies.alloc());
                let appended = self.bodies.append(id, offset, bytes);
                let prev_total;
                {
                    let t = self.touch(idx);
                    let meta = Self::body_meta(t, dir);
                    meta.id = Some(id);
                    meta.captured += appended.added;
                    meta.gap |= appended.gap;
                    prev_total = meta.total;
                    meta.total = meta.total.max(offset + len);
                    if !meta.state.is_final() {
                        meta.state = BodyState::Streaming;
                    }
                }
                let delta = (offset + len).saturating_sub(prev_total);
                self.account(dir, at, delta);
            }
            SessionEvent::BodyProgress { key, dir, at, total } => {
                self.see(at);
                let idx = self.idx_or_placeholder(key, at);
                let t = self.touch(idx);
                let meta = Self::body_meta(t, dir);
                let delta = total.saturating_sub(meta.total);
                meta.total = meta.total.max(total);
                if !meta.state.is_final() {
                    meta.state = BodyState::Streaming;
                }
                self.account(dir, at, delta);
            }
            SessionEvent::BodyEnd { key, dir, at, total, captured, state } => {
                self.see(at);
                let idx = self.idx_or_placeholder(key, at);
                let t = self.touch(idx);
                let meta = Self::body_meta(t, dir);
                let delta = total.saturating_sub(meta.total);
                meta.total = meta.total.max(total);
                meta.state = BodyState::from_wire(&state);
                // `captured` counts what we hold; the device sending more means chunks were lost
                if captured > meta.captured && meta.state != BodyState::NotCaptured {
                    meta.gap = true;
                }
                meta.ended = Some(at);
                if dir == BodyDir::Request && t.state == TxnState::Sending {
                    t.state = TxnState::Waiting;
                }
                self.account(dir, at, delta);
            }
            SessionEvent::Mark { key, at, name } => {
                self.see(at);
                let idx = self.idx_or_placeholder(key, at);
                let t = self.touch(idx);
                if matches!(name.as_str(), "req_body_end" | "req_headers_end") && t.state == TxnState::Sending {
                    t.state = TxnState::Waiting;
                }
                let pos = t.marks.partition_point(|&(_, ts)| ts <= at);
                t.marks.insert(pos, (name, at));
            }
            SessionEvent::Completed { key, at } => {
                self.see(at);
                let idx = self.idx_or_placeholder(key, at);
                let t = self.touch(idx);
                if t.state != TxnState::Failed {
                    t.state = TxnState::Complete;
                }
                t.end = Some(t.end.map_or(at, |e| e.max(at)));
            }
            SessionEvent::Failed(f) => {
                self.see(f.at);
                let f = *f;
                let idx = self.idx_or_placeholder(f.key, f.at);
                self.stats.failed += 1;
                let t = self.touch(idx);
                t.state = TxnState::Failed;
                t.end = Some(f.at);
                if f.conn.is_some() {
                    t.conn = f.conn;
                }
                for meta in [&mut t.req_body, &mut t.resp_body] {
                    if !meta.state.is_final() && meta.id.is_some() {
                        meta.state = BodyState::Error;
                    }
                }
                t.failure = Some(Failure {
                    at: f.at,
                    class: f.error.class,
                    message: f.error.message,
                    causes: f.error.causes.into_iter().map(|c| (c.class, c.message)).collect(),
                    phase: f.phase,
                    canceled: f.canceled,
                    simulated: f.simulated,
                });
            }
            SessionEvent::RuleApplied(r) => {
                self.see(r.at);
                let r = *r;
                let idx = self.idx_or_placeholder(r.key, r.at);
                let first = self.txns[idx as usize].rules.is_empty();
                if first {
                    self.stats.rule_modified += 1;
                }
                let t = self.touch(idx);
                t.rules.push(RuleHit { at: r.at, rules: r.rules, changes: r.changes });
                if let Some(d) = r.delivered {
                    t.delivered = Some(DeliveredResponse { status: d.status, message: d.message, headers: d.headers });
                }
            }
            SessionEvent::Traffic { source, at, since, rx, tx } => {
                self.see(at);
                self.traffic.add_app_sample(source, at, since, rx, tx);
            }
            SessionEvent::Dropped { source, at, events, bytes: _, txns } => {
                self.see(at);
                self.stats.dropped_events += events;
                for txn in txns {
                    if let Some(&i) = self.index.get(&TxnKey { source, txn }) {
                        self.touch(i).lossy = true;
                    }
                }
                self.markers.push(Marker {
                    at,
                    source: Some(source),
                    kind: MarkerKind::Note,
                    label: format!("{events} events dropped"),
                });
            }
            SessionEvent::Diagnostic { source, at, level, code, message } => {
                self.see(at);
                self.diagnostics.push(Diagnostic { at, source, level, code, message });
            }
            SessionEvent::Marker { source, at, kind, label } => {
                self.see(at);
                self.markers.push(Marker { at, source, kind, label });
            }
        }
    }
}
