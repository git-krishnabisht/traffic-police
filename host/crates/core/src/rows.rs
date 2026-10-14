//! The Connection View's rows: time-range filter, sort, and collapse repeats (ARCHITECTURE.md §5.7).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

impl Sort {
    /// `[ui] sort`: a column's name (`status`, `req-size`; `timeline` is the start order), and
    /// `desc` after it for the reverse.
    pub fn parse(s: &str) -> Option<Sort> {
        let mut words = s.split_whitespace();
        let name = words.next()?.to_ascii_lowercase().replace('_', "-");
        let descending = match words.next().map(str::to_ascii_lowercase).as_deref() {
            None | Some("asc" | "ascending") => false,
            Some("desc" | "descending") => true,
            _ => return None,
        };
        if words.next().is_some() {
            return None;
        }
        let all = Column::DEFAULT.into_iter().chain(Column::OPTIONAL);
        let column = all.into_iter().find(|c| c.title().to_ascii_lowercase().replace(' ', "-") == name)?;
        Some(Sort { column, descending })
    }
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

/// Which `body:` searches a row model asked for, as long as its filter's needles stay: a number
/// unique in the process, which the results carry, so that results for other needles (an earlier
/// keystroke's, a cleared session's) are told apart and dropped, and a flag that stops the
/// searches still running once the needles change.
#[derive(Debug, Clone)]
pub struct SearchTicket {
    pub generation: u64,
    cancel: Arc<AtomicBool>,
}

impl Default for SearchTicket {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        SearchTicket { generation: NEXT.fetch_add(1, Ordering::Relaxed), cancel: Arc::default() }
    }
}

impl SearchTicket {
    /// The searches are no longer wanted: the needles changed, or the rows went.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// One needle's `body:` results: by transaction, whether its bodies have the needle, with the
/// transaction revision that was found for.
type NeedleHits = (String, HashMap<TxnIdx, (u64, bool)>);

#[derive(Debug, Clone, Default)]
pub struct RowModel {
    pub sort: Sort,
    pub collapse: bool,
    /// Show only transactions overlapping this time range (graph selection).
    pub range: Option<(Ts, Ts)>,
    /// The filter bar's filter.
    pub filter: Option<Arc<Filter>>,
    /// `body:` results for each needle of the filter (in its order), by transaction, with the
    /// transaction revision they were found for. A needle that stays when the filter changes
    /// keeps its results.
    body_hits: Vec<NeedleHits>,
    /// `body:` searches the filter needs and nobody has asked for yet: (transaction, needle).
    body_wanted: BTreeSet<(TxnIdx, usize)>,
    /// Searches handed out and not answered yet, which are not asked for again meanwhile: up to
    /// 0.5.1 every refresh asked again for every search still running, so each result that came
    /// in (it refreshes the rows) queued all the others once more.
    body_pending: HashSet<(TxnIdx, usize)>,
    search: SearchTicket,
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
        if same {
            return;
        }
        let needles = filter.as_deref().map_or(&[][..], Filter::needles);
        if !self.body_hits.iter().map(|(n, _)| n).eq(needles) {
            // other needles: the searches for the old ones stop, and what they still send is
            // dropped; the results for needles that stay are kept
            self.stop_searches();
            let mut old = std::mem::take(&mut self.body_hits);
            self.body_hits = needles
                .iter()
                .map(|n| {
                    let hits = old.iter_mut().find(|(o, _)| o == n).map(|(_, h)| std::mem::take(h));
                    (n.clone(), hits.unwrap_or_default())
                })
                .collect();
        }
        self.filter = filter;
        self.body_wanted.clear();
        self.filtered.clear();
        self.dirty = true;
    }

    /// At most `max` of the `body:` searches the filter is waiting for, the newest requests first
    /// (the list's live end): `(transaction, needle index)`, with the ticket their results are to
    /// carry ([`RowModel::set_body_hit`]). Each is handed out once, until its result comes in or
    /// the needles change.
    pub fn take_body_wanted(&mut self, max: usize) -> (SearchTicket, Vec<(TxnIdx, usize)>) {
        let v: Vec<_> = std::iter::from_fn(|| self.body_wanted.pop_last()).take(max).collect();
        self.body_pending.extend(v.iter().copied());
        (self.search.clone(), v)
    }

    /// Whether `body:` searches are waiting to be handed out.
    pub fn has_body_wanted(&self) -> bool {
        !self.body_wanted.is_empty()
    }

    /// A finished `body:` search of `generation`'s ticket, for the transaction revision it
    /// looked at. One of another ticket (other needles) is dropped: `false`.
    pub fn set_body_hit(&mut self, generation: u64, txn: TxnIdx, needle: usize, rev: u64, hit: bool) -> bool {
        if generation != self.search.generation {
            return false;
        }
        self.body_pending.remove(&(txn, needle));
        let Some((_, hits)) = self.body_hits.get_mut(needle) else { return false };
        hits.insert(txn, (rev, hit));
        if let Some(slot) = self.filtered.get_mut(txn as usize) {
            *slot = None;
        }
        self.dirty = true;
        true
    }

    /// The generation of the `body:` searches asked for now.
    pub fn search_generation(&self) -> u64 {
        self.search.generation
    }

    /// Stops the `body:` searches still running and forgets what was asked for (the results
    /// found stay): the needles changed, or these rows are going.
    pub fn stop_searches(&mut self) {
        self.search.cancel();
        self.search = SearchTicket::default();
        self.body_wanted.clear();
        self.body_pending.clear();
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
            pending: &self.body_pending,
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
    hits: &'a [NeedleHits],
    wanted: &'a mut BTreeSet<(TxnIdx, usize)>,
    pending: &'a HashSet<(TxnIdx, usize)>,
    cache: &'a mut Vec<Option<(u64, bool)>>,
}

impl Check<'_> {
    /// Whether `t` passes the range and the filter. `body:` terms without a result yet are
    /// queued in `wanted` (unless their search is running) and hide the row until the result
    /// arrives. Filter results are kept per revision, except for open requests under a filter
    /// that uses the clock.
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
        let (hits, wanted, pending) = (self.hits, &mut *self.wanted, self.pending);
        let ok = f.matches(t, now, &mut |needle| match hits.get(needle).and_then(|(_, h)| h.get(&i)) {
            Some(&(rev, hit)) if rev == t.rev => Some(hit),
            _ => {
                if !pending.contains(&(i, needle)) {
                    wanted.insert((i, needle));
                }
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
        let (ticket, wanted) = m.take_body_wanted(usize::MAX);
        assert_eq!(wanted, [(0, 0)]);
        assert!(m.set_body_hit(ticket.generation, 0, 0, s.txn(0).rev, true));
        m.refresh(&s, 3 * NS_PER_SEC);
        assert_eq!(shown(&m, &s), ["/a"]);
    }

    fn set_filter(m: &mut RowModel, text: &str) {
        m.set_filter(Filter::parse(text).unwrap().map(Arc::new));
    }

    /// Typing `body:ab` and then `c`: what the search for "ab" sends after that is not taken for
    /// "abc" (up to 0.5.1 results were kept by the needle's index, so it was, and rows that have
    /// "ab" but not "abc" showed), and that search is told to stop.
    #[test]
    fn a_result_for_an_earlier_keystroke_is_dropped() {
        let mut s = SessionStore::new();
        let a = request(&mut s, 1, "/a", NS_PER_SEC);
        respond(&mut s, a, 200, 2 * NS_PER_SEC);
        let mut m = filtered("body:ab");
        m.refresh(&s, 3 * NS_PER_SEC);
        let (ab, wanted) = m.take_body_wanted(usize::MAX);
        assert_eq!(wanted, [(0, 0)]);
        set_filter(&mut m, "body:abc");
        assert!(ab.cancelled(), "the search for \"ab\" stops");
        m.refresh(&s, 3 * NS_PER_SEC);
        // "ab" was found; it must not count for "abc"
        assert!(!m.set_body_hit(ab.generation, 0, 0, s.txn(0).rev, true));
        m.refresh(&s, 3 * NS_PER_SEC);
        assert!(shown(&m, &s).is_empty());
        let (abc, wanted) = m.take_body_wanted(usize::MAX);
        assert_eq!(wanted, [(0, 0)], "asked for again, for \"abc\"");
        assert!(!abc.cancelled());
        assert!(m.set_body_hit(abc.generation, 0, 0, s.txn(0).rev, false));
        m.refresh(&s, 3 * NS_PER_SEC);
        assert!(shown(&m, &s).is_empty(), "no \"abc\" in it");
        // a cleared session's rows: their results are not taken by the new rows either
        let mut fresh = filtered("body:abc");
        fresh.refresh(&s, 3 * NS_PER_SEC);
        assert!(!fresh.set_body_hit(abc.generation, 0, 0, s.txn(0).rev, true));
    }

    /// While a search runs, new traffic (or another result) refreshes the rows: the search is not
    /// asked for a second time; a request that changed meanwhile is searched again once the
    /// result for its old revision is in.
    #[test]
    fn a_search_that_runs_is_not_asked_for_again() {
        let mut s = SessionStore::new();
        let a = request(&mut s, 1, "/a", NS_PER_SEC);
        let mut m = filtered("body:token");
        m.refresh(&s, 2 * NS_PER_SEC);
        let (ticket, wanted) = m.take_body_wanted(usize::MAX);
        assert_eq!(wanted, [(0, 0)]);
        let old_rev = s.txn(0).rev;
        // the request changes and another comes while the search runs
        respond(&mut s, a, 200, 2 * NS_PER_SEC);
        request(&mut s, 2, "/b", 2 * NS_PER_SEC);
        m.refresh(&s, 3 * NS_PER_SEC);
        assert_eq!(m.take_body_wanted(usize::MAX).1, [(1, 0)], "only the new one");
        // the result for the old revision: the request is searched again
        assert!(m.set_body_hit(ticket.generation, 0, 0, old_rev, true));
        m.refresh(&s, 3 * NS_PER_SEC);
        assert!(shown(&m, &s).is_empty(), "not shown on an old revision's result");
        assert_eq!(m.take_body_wanted(usize::MAX).1, [(0, 0)]);
    }

    /// Searches are handed out a turn's worth at a time, the newest requests first.
    #[test]
    fn searches_are_handed_out_newest_first_and_a_few_at_a_time() {
        let mut s = SessionStore::new();
        for txn in 1..=3 {
            request(&mut s, txn, "/a", NS_PER_SEC);
        }
        let mut m = filtered("body:token");
        m.refresh(&s, 2 * NS_PER_SEC);
        assert_eq!(m.take_body_wanted(2).1, [(2, 0), (1, 0)]);
        assert!(m.has_body_wanted());
        assert_eq!(m.take_body_wanted(2).1, [(0, 0)]);
        assert!(!m.has_body_wanted());
    }

    /// A needle that stays when the filter changes keeps its results and its running searches;
    /// one that goes takes its results with it.
    #[test]
    fn results_stay_for_a_needle_that_stays() {
        let mut s = SessionStore::new();
        let a = request(&mut s, 1, "/a", NS_PER_SEC);
        respond(&mut s, a, 200, 2 * NS_PER_SEC);
        let mut m = filtered("body:token");
        m.refresh(&s, 3 * NS_PER_SEC);
        let (ticket, _) = m.take_body_wanted(usize::MAX);
        assert!(m.set_body_hit(ticket.generation, 0, 0, s.txn(0).rev, true));
        set_filter(&mut m, "body:token status:200");
        assert!(!ticket.cancelled());
        assert_eq!(m.search_generation(), ticket.generation);
        m.refresh(&s, 3 * NS_PER_SEC);
        assert_eq!(shown(&m, &s), ["/a"]);
        assert!(!m.has_body_wanted(), "nothing to search again");
        set_filter(&mut m, "status:200");
        assert!(ticket.cancelled());
        set_filter(&mut m, "body:token");
        m.refresh(&s, 3 * NS_PER_SEC);
        assert!(m.has_body_wanted(), "searched again after the needle went");
    }
}
