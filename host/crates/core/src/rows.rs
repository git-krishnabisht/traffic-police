//! The Connection View's rows: time-range filter, sort, and collapse repeats (ARCHITECTURE.md §5.7).

use std::collections::HashSet;

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
    expanded: HashSet<TxnIdx>,
    rows: Vec<Row>,
    order: Vec<TxnIdx>,
    seen_generation: Option<u64>,
    dirty: bool,
}

fn cmp_key(store: &SessionStore, t: &Transaction, col: Column, now: Ts) -> KeyVal {
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
    .with_tiebreak(store, t)
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum KeyVal {
    N(u64),
    S(String),
    T(Box<KeyVal>, u64, u64),
}

impl KeyVal {
    fn with_tiebreak(self, _store: &SessionStore, t: &Transaction) -> KeyVal {
        KeyVal::T(Box::new(self), t.start, t.key.txn)
    }
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
        if !self.dirty && self.seen_generation == Some(store.generation()) {
            return;
        }
        self.seen_generation = Some(store.generation());
        self.dirty = false;
        let range = self.range;
        self.order.clear();
        self.order.extend((0..store.len() as TxnIdx).filter(|&i| {
            let t = store.txn(i);
            match range {
                Some((a, b)) => t.start <= b && t.end.unwrap_or(if t.state.is_open() { now } else { t.start }) >= a,
                None => true,
            }
        }));
        let sort = self.sort;
        if sort != Sort::default() {
            let mut keyed: Vec<(KeyVal, TxnIdx)> =
                self.order.iter().map(|&i| (cmp_key(store, store.txn(i), sort.column, now), i)).collect();
            keyed.sort_by(|a, b| if sort.descending { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) });
            self.order = keyed.into_iter().map(|(_, i)| i).collect();
        } else {
            let mut keyed: Vec<(u64, u64, TxnIdx)> =
                self.order.iter().map(|&i| (store.txn(i).start, store.txn(i).key.txn, i)).collect();
            keyed.sort_unstable();
            self.order = keyed.into_iter().map(|(_, _, i)| i).collect();
        }
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

fn same_endpoint(a: &Transaction, b: &Transaction) -> bool {
    a.method == b.method && a.url.host == b.url.host && a.url.path == b.url.path
}

/// Whether a transaction is still in flight (for live displays).
pub fn is_pending(t: &Transaction) -> bool {
    t.state.is_open() || t.state == TxnState::Detached && t.resp.is_none()
}
