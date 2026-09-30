//! The Connection View's rows: time-range filter, sort, and collapse repeats (ARCHITECTURE.md §5.7).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::filter::Filter;
use crate::fmt::Ts;
use crate::model::{Transaction, TxnIdx, TxnState};
use crate::store::SessionStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    Name,
    Size,
    Type,
    Status,
    Time,
    Timeline,
    Method,
    Host,
    Path,
    Thread,
    Start,
    ReqSize,
    Protocol,
    Client,
}

impl Column {
    pub const DEFAULT: [Column; 6] =
        [Column::Name, Column::Size, Column::Type, Column::Status, Column::Time, Column::Timeline];
    pub const OPTIONAL: [Column; 8] = [
        Column::Method,
        Column::Host,
        Column::Path,
        Column::Thread,
        Column::Start,
        Column::ReqSize,
        Column::Protocol,
        Column::Client,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Column::Name => "Name",
            Column::Size => "Size",
            Column::Type => "Type",
            Column::Status => "Status",
            Column::Time => "Time",
            Column::Timeline => "Timeline",
            Column::Method => "Method",
            Column::Host => "Host",
            Column::Path => "Path",
            Column::Thread => "Thread",
            Column::Start => "Start",
            Column::ReqSize => "Req size",
            Column::Protocol => "Protocol",
            Column::Client => "Client",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub column: Column,
    pub descending: bool,
}

impl Default for Sort {
    /// Chronological (request start), which the Timeline column stands for.
    fn default() -> Self {
        Sort { column: Column::Timeline, descending: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Txn(TxnIdx),
    /// Consecutive calls to the same method and path. `members` is in display order; the row
    /// shows the last one's status and size.
    Group {
        members: Vec<TxnIdx>,
        expanded: bool,
    },
    /// A member shown under an expanded group.
    Member(TxnIdx),
}

impl Row {
    /// The transaction a selection on this row refers to.
    pub fn txn(&self) -> TxnIdx {
        match self {
            Row::Txn(i) | Row::Member(i) => *i,
            Row::Group { members, .. } => *members.last().expect("groups are never empty"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RowModel {
    pub sort: Sort,
    pub collapse: bool,
    /// Show only transactions overlapping this time range (graph selection).
    pub range: Option<(Ts, Ts)>,
    /// The filter bar's filter.
    pub filter: Option<Arc<Filter>>,
    /// `body:` results per (transaction, needle index), with the revision they were found for.
    body_hits: HashMap<(TxnIdx, usize), (u64, bool)>,
    /// `body:` searches the filter needs and nobody has run yet.
    body_wanted: HashSet<(TxnIdx, usize)>,
    /// The filter's result per transaction, with the revision it was computed for (a new
    /// request or a change is filtered again; the others are not).
    filtered: Vec<Option<(u64, bool)>>,
    expanded: HashSet<TxnIdx>,
    rows: Vec<Row>,
    order: Vec<TxnIdx>,
    /// Every transaction in the current sort order, kept between refreshes so re-sorting after
    /// a few changes is close to linear (the sort is adaptive).
    sorted: Vec<TxnIdx>,
    /// Sort key per transaction, with the transaction revision it was computed from.
    keys: Vec<Option<(u64, KeyVal)>>,
    keyed_for: Option<Sort>,
    seen_generation: Option<u64>,
    seen_now: Ts,
    dirty: bool,
}

fn sort_key(t: &Transaction, col: Column, now: Ts) -> KeyVal {
    match col {
        Column::Name => KeyVal::S(t.url.name().to_lowercase()),
        Column::Size => KeyVal::N(t.response_size()),
        Column::Type => KeyVal::S(t.type_label()),
        Column::Status => KeyVal::N(t.status().map(u64::from).unwrap_or(if t.failure.is_some() { 1000 } else { 999 })),
        Column::Time => KeyVal::N(t.duration(now)),
        Column::Timeline | Column::Start => KeyVal::N(t.start),
        Column::Method => KeyVal::S(t.method.clone()),
        Column::Host => KeyVal::S(t.url.host.clone()),
        Column::Path => KeyVal::S(t.url.path.clone()),
        Column::Thread => KeyVal::S(t.thread.as_ref().map(|th| th.name.clone()).unwrap_or_default()),
        Column::ReqSize => KeyVal::N(t.req_body.total),
        Column::Protocol => KeyVal::S(t.resp.as_ref().and_then(|r| r.protocol.clone()).unwrap_or_default()),
        Column::Client => KeyVal::S(t.client.as_ref().map(|c| c.label()).unwrap_or_default()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum KeyVal {
    N(u64),
    S(String),
}

impl RowModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Transactions that pass the range and the filter.
    pub fn matched(&self) -> usize {
        self.order.len()
    }

    /// Index of the row that shows `txn` (a group counts if it contains it).
    pub fn position(&self, txn: TxnIdx) -> Option<usize> {
        self.rows.iter().position(|r| match r {
            Row::Txn(i) | Row::Member(i) => *i == txn,
            Row::Group { members, expanded } => !expanded && members.contains(&txn),
        })
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub fn set_sort(&mut self, sort: Sort) {
        if self.sort != sort {
            self.sort = sort;
            self.dirty = true;
        }
    }

    pub fn set_collapse(&mut self, on: bool) {
        if self.collapse != on {
            self.collapse = on;
            self.dirty = true;
        }
    }

    pub fn set_filter(&mut self, filter: Option<Arc<Filter>>) {
        let same = match (&self.filter, &filter) {
            (Some(a), Some(b)) => a.source == b.source,
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.filter = filter;
            self.body_hits.clear();
            self.body_wanted.clear();
            self.filtered.clear();
            self.dirty = true;
        }
    }

    /// The `body:` searches the filter is waiting for: `(transaction, needle index)`. Each is
    /// handed out once; report the result with [`RowModel::set_body_hit`].
    pub fn take_body_wanted(&mut self) -> Vec<(TxnIdx, usize)> {
        let mut v: Vec<_> = self.body_wanted.drain().collect();
        v.sort_unstable();
        v
    }

    /// Whether `body:` searches are waiting to be handed out.
    pub fn has_body_wanted(&self) -> bool {
        !self.body_wanted.is_empty()
    }

    /// A finished `body:` search, for the transaction revision it looked at.
    pub fn set_body_hit(&mut self, txn: TxnIdx, needle: usize, rev: u64, hit: bool) {
        self.body_hits.insert((txn, needle), (rev, hit));
        if let Some(slot) = self.filtered.get_mut(txn as usize) {
            *slot = None;
        }
        self.dirty = true;
    }

    pub fn set_range(&mut self, range: Option<(Ts, Ts)>) {
        if self.range != range {
            self.range = range;
            self.dirty = true;
        }
    }

    /// Expand or collapse the group whose first member is `first`.
    pub fn toggle_group(&mut self, first: TxnIdx) {
        if !self.expanded.remove(&first) {
            self.expanded.insert(first);
        }
        self.dirty = true;
    }

    /// Rebuild if the store changed or settings changed. Cheap when nothing changed.
    pub fn refresh(&mut self, store: &SessionStore, now: Ts) {
        // Durations of open requests grow with the clock, so a Time sort, a range, and a
        // filter with `time` terms can change even without new events.
        let clock_matters = self.sort.column == Column::Time
            || self.range.is_some()
            || self.filter.as_ref().is_some_and(|f| f.uses_clock());
        if !self.dirty && self.seen_generation == Some(store.generation()) && !(clock_matters && now != self.seen_now) {
            return;
        }
        self.seen_generation = Some(store.generation());
        self.seen_now = now;
        self.dirty = false;
        let range = self.range;
        let filter = self.filter.clone();
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        self.filtered.resize(store.len(), None);
        let sorted = self.sort != Sort::default();
        if sorted {
            self.sort_by_key(store, now);
        }
        let mut check = Check {
            range,
            filter: filter.as_deref(),
            clock: filter.as_ref().is_some_and(|f| f.uses_clock()),
            now,
            hits: &self.body_hits,
            wanted: &mut self.body_wanted,
            cache: &mut self.filtered,
        };
        if sorted {
            for &i in &self.sorted {
                if check.passes(i, store.txn(i)) {
                    order.push(i);
                }
            }
        } else {
            let mut keyed: Vec<(u64, u64, TxnIdx)> = Vec::new();
            for i in 0..store.len() as TxnIdx {
                let t = store.txn(i);
                if check.passes(i, t) {
                    keyed.push((t.start, t.key.txn, i));
                }
            }
            keyed.sort_unstable();
            order.extend(keyed.into_iter().map(|(_, _, i)| i));
        }
        self.order = order;
        self.rows.clear();
        if !self.collapse {
            self.rows.extend(self.order.iter().map(|&i| Row::Txn(i)));
            return;
        }
        let mut i = 0;
        while i < self.order.len() {
            let a = store.txn(self.order[i]);
            let mut j = i + 1;
            while j < self.order.len() && same_endpoint(a, store.txn(self.order[j])) {
                j += 1;
            }
            if j - i == 1 {
                self.rows.push(Row::Txn(self.order[i]));
            } else {
                let members: Vec<TxnIdx> = self.order[i..j].to_vec();
                let expanded = self.expanded.contains(&members[0]);
                self.rows.push(Row::Group { members: members.clone(), expanded });
                if expanded {
                    self.rows.extend(members.into_iter().map(Row::Member));
                }
            }
            i = j;
        }
    }
}

impl RowModel {
    /// Bring `sorted` up to date for a non-default sort: new transactions are appended, keys
    /// are recomputed only for transactions that changed (and for open ones when sorting by
    /// duration), and the nearly sorted order is sorted again.
    fn sort_by_key(&mut self, store: &SessionStore, now: Ts) {
        let sort = self.sort;
        if self.keyed_for != Some(sort) {
            self.keyed_for = Some(sort);
            self.keys.clear();
            self.sorted.clear();
        }
        let known = self.sorted.len();
        self.sorted.extend(known as TxnIdx..store.len() as TxnIdx);
        self.keys.resize(store.len(), None);
        let mut changed = known < store.len();
        for i in 0..store.len() {
            let t = store.txn(i as TxnIdx);
            let fresh = matches!(&self.keys[i], Some((rev, _)) if *rev == t.rev)
                && !(sort.column == Column::Time && t.end.is_none());
            if !fresh {
                self.keys[i] = Some((t.rev, sort_key(t, sort.column, now)));
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let keys = &self.keys;
        let key = |i: TxnIdx| {
            let t = store.txn(i);
            (&keys[i as usize].as_ref().expect("keyed").1, t.start, t.key.txn)
        };
        self.sorted.sort_by(|&a, &b| {
            let o = key(a).cmp(&key(b));
            if sort.descending { o.reverse() } else { o }
        });
    }
}

/// The time range and the filter for one refresh.
struct Check<'a> {
    range: Option<(Ts, Ts)>,
    filter: Option<&'a Filter>,
    /// The filter has terms that change with the clock.
    clock: bool,
    now: Ts,
    hits: &'a HashMap<(TxnIdx, usize), (u64, bool)>,
    wanted: &'a mut HashSet<(TxnIdx, usize)>,
    cache: &'a mut Vec<Option<(u64, bool)>>,
}

impl Check<'_> {
    /// Whether `t` passes the range and the filter. `body:` terms without a result yet are
    /// queued in `wanted` and hide the row until the result arrives. Filter results are kept
    /// per revision, except for open requests under a filter that uses the clock.
    fn passes(&mut self, i: TxnIdx, t: &Transaction) -> bool {
        let now = self.now;
        if let Some((a, b)) = self.range
            && !(t.start <= b && t.end.unwrap_or(if t.state.is_open() { now } else { t.start }) >= a)
        {
            return false;
        }
        let Some(f) = self.filter else { return true };
        let volatile = self.clock && t.state.is_open();
        if !volatile
            && let Some(Some((rev, ok))) = self.cache.get(i as usize)
            && *rev == t.rev
        {
            return *ok;
        }
        let mut waiting = false;
        let (hits, wanted) = (self.hits, &mut *self.wanted);
        let ok = f.matches(t, now, &mut |needle| match hits.get(&(i, needle)) {
            Some(&(rev, hit)) if rev == t.rev => Some(hit),
            _ => {
                wanted.insert((i, needle));
                waiting = true;
                None
            }
        });
        if !waiting && let Some(slot) = self.cache.get_mut(i as usize) {
            *slot = Some((t.rev, ok));
        }
        ok
    }
}

fn same_endpoint(a: &Transaction, b: &Transaction) -> bool {
    a.method == b.method && a.url.host == b.url.host && a.url.path == b.url.path
}

/// Whether a transaction is still in flight (for live displays).
pub fn is_pending(t: &Transaction) -> bool {
    t.state.is_open() || t.state == TxnState::Detached && t.resp.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEvent;
    use crate::event::{RequestStarted, ResponseStarted};
    use crate::fmt::NS_PER_SEC;
    use crate::model::TxnKey;

    fn request(s: &mut SessionStore, txn: u64, path: &str, at: Ts) -> TxnKey {
        let key = TxnKey { source: 0, txn };
        s.apply(SessionEvent::Request(Box::new(RequestStarted {
            key,
            at,
            call: None,
            hop: 0,
            method: "GET".into(),
            url: format!("https://api.example.app{path}"),
            headers: Vec::new(),
            client: None,
            thread: None,
            stack: Vec::new(),
            stack_truncated: false,
            body: None,
            marks: Vec::new(),
            conn: None,
        })));
        key
    }

    fn respond(s: &mut SessionStore, key: TxnKey, status: u16, at: Ts) {
        let r = ResponseStarted {
            key,
            at,
            status,
            message: String::new(),
            protocol: None,
            headers: Vec::new(),
            conn: None,
        };
        s.apply(SessionEvent::Response(Box::new(r)));
        s.apply(SessionEvent::Completed { key, at });
    }

    fn filtered(text: &str) -> RowModel {
        let mut m = RowModel::new();
        m.set_filter(Filter::parse(text).unwrap().map(Arc::new));
        m
    }

    fn shown(m: &RowModel, s: &SessionStore) -> Vec<String> {
        m.rows().iter().map(|r| s.txn(r.txn()).url.path.clone()).collect()
    }

    #[test]
    fn a_request_is_filtered_again_when_it_changes() {
        let mut s = SessionStore::new();
        let a = request(&mut s, 1, "/a", NS_PER_SEC);
        let b = request(&mut s, 2, "/b", NS_PER_SEC);
        let mut m = filtered("status:4xx");
        m.refresh(&s, 2 * NS_PER_SEC);
        assert!(shown(&m, &s).is_empty());
        respond(&mut s, a, 200, 2 * NS_PER_SEC);
        respond(&mut s, b, 404, 2 * NS_PER_SEC);
        m.refresh(&s, 3 * NS_PER_SEC);
        assert_eq!(shown(&m, &s), ["/b"]);
    }

    #[test]
    fn a_time_filter_follows_the_clock_for_open_requests() {
        let mut s = SessionStore::new();
        request(&mut s, 1, "/slow", NS_PER_SEC);
        let mut m = filtered("time>2s");
        m.refresh(&s, 2 * NS_PER_SEC);
        assert!(shown(&m, &s).is_empty());
        // no new events: only the clock moved
        m.refresh(&s, 4 * NS_PER_SEC);
        assert_eq!(shown(&m, &s), ["/slow"]);
    }

    #[test]
    fn a_body_result_shows_the_row_when_it_arrives() {
        let mut s = SessionStore::new();
        let a = request(&mut s, 1, "/a", NS_PER_SEC);
        respond(&mut s, a, 200, 2 * NS_PER_SEC);
        let mut m = filtered("body:token");
        m.refresh(&s, 3 * NS_PER_SEC);
        assert!(shown(&m, &s).is_empty(), "hidden until the search has run");
        let wanted = m.take_body_wanted();
        assert_eq!(wanted, [(0, 0)]);
        m.set_body_hit(0, 0, s.txn(0).rev, true);
        m.refresh(&s, 3 * NS_PER_SEC);
        assert_eq!(shown(&m, &s), ["/a"]);
    }
}
