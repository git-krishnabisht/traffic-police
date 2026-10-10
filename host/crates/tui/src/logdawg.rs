//! Logdawg's view (`4`): the device's log, as Android Studio's Logcat shows it
//! (ARCHITECTURE.md §5.17).
//!
//! Only the lines that pass the filter are listed: their ids are kept, each new line is tested
//! once as it arrives, and every line again only when the filter, the app's uid or its
//! processes change. Rows are drawn for the screen only; a message of several lines, or one
//! that wraps (`[ui] wrap`), takes the rows it needs, under the message column. Columns: time,
//! pid-tid, tag, the process (on a wide screen), the level, the message.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use traffic_police_core::decode::Tok;
use traffic_police_core::decode::doc::StyledLine;
use traffic_police_core::fmt::{self, NS_PER_SEC, Ts};
use traffic_police_core::logdawg::filter::{Context, LogFilter};
use traffic_police_core::logdawg::{BUFFER_CRASH, Line as LogLine};
use traffic_police_core::model::{Transaction, TxnIdx};
use traffic_police_core::store::SessionStore;
use unicode_width::UnicodeWidthStr;

use crate::actions::Action;
use crate::app::{App, Focus, Target};
use crate::wrap::{self, Top};

/// The filter a session starts with (`[logdawg] filter`).
pub const DEFAULT_FILTER: &str = "package:mine";

const TIME_W: usize = 12;
const IDS_W: usize = 13;
const TAG_W: usize = 23;
const PROCESS_W: usize = 24;
/// From this width on the process column shows.
const PROCESS_FROM: u16 = 150;

/// A row of the view: a line of the log, or a request at its start (`R` shows or hides them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Line(u64),
    Request(TxnIdx),
}

/// Where the view is, and which lines pass its filter.
#[derive(Debug, Clone)]
pub struct LogdawgView {
    pub filter: Option<Arc<LogFilter>>,
    /// The lines that pass and the requests, in time order.
    rows: VecDeque<Row>,
    /// How many rows are lines.
    lines: usize,
    /// The next line id to test, and how many of the store's requests are placed.
    scanned: u64,
    placed: usize,
    /// Requests among the lines (`R`, `[logdawg] requests`).
    pub requests: bool,
    dirty: bool,
    seen_uid: Option<u32>,
    seen_pids: Vec<u32>,
    seen_second: u64,
    seen_range: Option<(Ts, Ts)>,
    /// The cursor's row, and the view's top.
    pub cursor: usize,
    pub top: Top,
    /// At the newest row: new rows move the cursor along.
    pub follow: bool,
    /// The message column's width and the box's rows last time.
    pub width: usize,
    pub height: usize,
}

impl Default for LogdawgView {
    fn default() -> Self {
        LogdawgView {
            filter: LogFilter::parse(DEFAULT_FILTER).ok().flatten().map(Arc::new),
            rows: VecDeque::new(),
            lines: 0,
            scanned: 0,
            placed: 0,
            requests: true,
            dirty: true,
            seen_uid: None,
            seen_pids: Vec::new(),
            seen_second: 0,
            seen_range: None,
            cursor: 0,
            top: Top::default(),
            follow: true,
            width: 80,
            height: 20,
        }
    }
}

/// When a row happened: a line's time, a request's start.
fn time_of(row: Row, store: &SessionStore) -> Ts {
    match row {
        Row::Line(id) => store.logs().get(id).map_or(0, |l| l.ts),
        Row::Request(i) => store.txns().get(i as usize).map_or(0, |t| t.start),
    }
}

/// Whether a request overlaps the graph's range (as the request list has it).
fn in_range(t: &Transaction, range: Option<(Ts, Ts)>, now: Ts) -> bool {
    range.is_none_or(|(a, b)| t.start <= b && t.end.unwrap_or(if t.state.is_open() { now } else { t.start }) >= a)
}

impl LogdawgView {
    /// Starts over (the log was cleared), keeping the filter and whether requests show.
    pub fn reset(&mut self) {
        *self = LogdawgView { filter: self.filter.take(), requests: self.requests, ..LogdawgView::default() };
    }

    pub fn set_filter(&mut self, filter: Option<LogFilter>) {
        self.filter = filter.map(Arc::new);
        self.dirty = true;
    }

    /// Requests among the lines, or not.
    pub fn set_requests(&mut self, on: bool) {
        if self.requests != on {
            self.requests = on;
            self.dirty = true;
        }
    }

    /// Rows: the lines that pass the filter, and the requests.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The lines that pass the filter.
    pub fn lines(&self) -> usize {
        self.lines
    }

    pub fn row(&self, i: usize) -> Option<Row> {
        self.rows.get(i).copied()
    }

    /// The id of the line at row `i` (`None` for a request's row).
    pub fn id(&self, i: usize) -> Option<u64> {
        match self.rows.get(i)? {
            Row::Line(id) => Some(*id),
            Row::Request(_) => None,
        }
    }

    /// Brings the list up to the store: new lines are tested and new requests placed, and
    /// everything is done again after a change that can change the answer (the filter, the app's
    /// uid or processes, the graph's range, lines that went to make room).
    pub fn sync(&mut self, store: &SessionStore, app_pids: &HashSet<u32>, now: Ts, range: Option<(Ts, Ts)>) {
        let logs = store.logs();
        let clock = self.filter.as_ref().is_some_and(|f| f.uses_clock());
        let mut pids: Vec<u32> = app_pids.iter().copied().collect();
        pids.sort_unstable();
        let first_line = self.rows.iter().find_map(|r| match r {
            Row::Line(id) => Some(*id),
            Row::Request(_) => None,
        });
        if self.dirty
            || logs.uid() != self.seen_uid
            || pids != self.seen_pids
            || range != self.seen_range
            || (clock && now / NS_PER_SEC != self.seen_second)
            || first_line.is_some_and(|id| id < logs.first_id())
            || self.placed > store.len()
        {
            // again from the start; the cursor stays at its row's time
            let at = self.row(self.cursor).map(|r| time_of(r, store));
            self.dirty = false;
            self.seen_uid = logs.uid();
            self.seen_pids = pids;
            self.seen_second = now / NS_PER_SEC;
            self.seen_range = range;
            self.rebuild(store, app_pids, now, range);
            if let Some(at) = at
                && !self.follow
            {
                self.cursor =
                    self.rows.partition_point(|r| time_of(*r, store) < at).min(self.rows.len().saturating_sub(1));
                self.top = Top { line: self.cursor, part: 0 };
            }
            return;
        }
        self.scan(store, app_pids, now, range);
    }

    /// Every line tested and every request placed, merged in time order.
    fn rebuild(&mut self, store: &SessionStore, app_pids: &HashSet<u32>, now: Ts, range: Option<(Ts, Ts)>) {
        let logs = store.logs();
        let cx = Context { app_pids, now };
        let lines: Vec<(Ts, u64)> = logs
            .iter_from(logs.first_id())
            .filter(|l| range.is_none_or(|(a, b)| l.ts >= a && l.ts <= b))
            .filter(|l| self.filter.as_ref().is_none_or(|f| f.matches(l, logs, &cx)))
            .map(|l| (l.ts, l.id))
            .collect();
        let mut requests: Vec<(Ts, TxnIdx)> = if self.requests {
            store
                .txns()
                .iter()
                .enumerate()
                .filter(|(_, t)| in_range(t, range, now))
                .map(|(i, t)| (t.start, i as TxnIdx))
                .collect()
        } else {
            Vec::new()
        };
        requests.sort_unstable();
        self.rows.clear();
        self.lines = lines.len();
        let mut r = requests.into_iter().peekable();
        for (ts, id) in lines {
            while let Some((_, i)) = r.next_if(|(start, _)| *start <= ts) {
                self.rows.push_back(Row::Request(i));
            }
            self.rows.push_back(Row::Line(id));
        }
        self.rows.extend(r.map(|(_, i)| Row::Request(i)));
        self.scanned = logs.end_id();
        self.placed = store.len();
        self.settle();
    }

    /// The lines and requests that came since: each goes in by its time, near the end.
    fn scan(&mut self, store: &SessionStore, app_pids: &HashSet<u32>, now: Ts, range: Option<(Ts, Ts)>) {
        let logs = store.logs();
        let from = self.scanned.max(logs.first_id());
        let cx = Context { app_pids, now };
        for line in logs.iter_from(from) {
            if range.is_none_or(|(a, b)| line.ts >= a && line.ts <= b)
                && self.filter.as_ref().is_none_or(|f| f.matches(&line, logs, &cx))
            {
                // before requests that start later (their start came in first)
                let mut at = self.rows.len();
                while at > 0
                    && matches!(self.rows[at - 1], Row::Request(i) if time_of(Row::Request(i), store) > line.ts)
                {
                    at -= 1;
                }
                self.insert(at, Row::Line(line.id));
                self.lines += 1;
            }
        }
        self.scanned = logs.end_id();
        if self.requests {
            for (i, t) in store.txns().iter().enumerate().skip(self.placed) {
                if !in_range(t, range, now) {
                    continue;
                }
                let mut at = self.rows.len();
                while at > 0 && time_of(self.rows[at - 1], store) > t.start {
                    at -= 1;
                }
                self.insert(at, Row::Request(i as TxnIdx));
            }
        }
        self.placed = store.len();
        self.settle();
    }

    /// A row in at `at`: the cursor and the view's top stay on theirs.
    fn insert(&mut self, at: usize, row: Row) {
        let before_cursor = !self.rows.is_empty() && at <= self.cursor;
        self.rows.insert(at, row);
        if !self.follow && before_cursor {
            self.cursor += 1;
        }
        if at < self.top.line {
            self.top.line += 1;
        }
    }

    fn settle(&mut self) {
        if self.follow {
            self.cursor = self.rows.len().saturating_sub(1);
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }
}

/// Text as the screen can show it: a tab is four spaces (stack traces start their lines with
/// one), a carriage return goes, and another control character (an ANSI escape an app logged)
/// is `�`, since a terminal would act on it. Copies keep the text as it was.
pub(crate) fn shown(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.chars().any(|c| c.is_control() && c != '\n') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\r' => {}
            '\n' => out.push('\n'),
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// A line's time: since the session's start (before it, negative), or the wall clock.
pub(crate) fn line_time(line: &LogLine<'_>, origin: Ts, wall: bool) -> String {
    if wall {
        fmt::wall_clock(line.wall_ms)
    } else if line.ts >= origin {
        fmt::offset(line.ts - origin)
    } else {
        format!("-{}", fmt::offset(origin - line.ts))
    }
}

/// A message's rows (of its [`shown`] text): byte ranges, one for each of its lines, or
/// (wrapping) for each row they take at `width`.
fn message_rows(message: &str, width: usize, wrapping: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut at = 0;
    for seg in message.split('\n') {
        if wrapping && seg.width() > width {
            for p in wrap::layout(seg, width, 0, false) {
                out.push((at + p.start, at + p.end));
            }
        } else {
            out.push((at, at + seg.len()));
        }
        at += seg.len() + 1;
    }
    if out.is_empty() {
        out.push((0, 0));
    }
    out
}

impl App {
    /// The app's processes from the network side: its capture runtimes' pids (the app's now, when
    /// another was picked in the session).
    fn app_pids(&self) -> HashSet<u32> {
        let store = self.view_store();
        let package = store.logs().package();
        store.sources().filter(|s| package.is_none_or(|p| s.package == p)).map(|s| s.pid).collect()
    }

    /// Keeps the Logdawg view up to the store (each frame while it shows).
    pub(crate) fn logdawg_refresh(&mut self) {
        let pids = self.app_pids();
        let now = self.now();
        // the graph's range (v) holds here too: the lines in it, the requests overlapping it
        let range = self.graph.selection;
        self.sync_logdawg(&pids, now, range);
    }

    fn with_logdawg_lines<R>(&self, f: impl FnOnce(&mut wrap::Lines) -> R) -> R {
        let logs = self.view_store().logs();
        let (width, wrapping) = (self.logdawg.width.max(1), self.prefs.wrap);
        let view = &self.logdawg;
        // a request takes one row
        let mut rows = |i: usize| {
            view.id(i).and_then(|id| logs.get(id)).map_or(1, |l| message_rows(&shown(l.message), width, wrapping).len())
        };
        let mut lines = wrap::Lines { len: view.len(), view: view.height.max(1), rows: &mut rows };
        f(&mut lines)
    }

    pub(crate) fn logdawg_action(&mut self, a: Action) {
        let len = self.logdawg.len();
        if len == 0 {
            return;
        }
        let last = len - 1;
        let cur = self.logdawg.cursor.min(last);
        let cursor = match a {
            Action::Up => cur.saturating_sub(1),
            Action::Down => (cur + 1).min(last),
            Action::Top => 0,
            Action::Bottom => last,
            Action::PageUp | Action::PageDown | Action::HalfPageUp | Action::HalfPageDown => {
                let n = if matches!(a, Action::PageUp | Action::PageDown) {
                    self.logdawg.height.max(2) - 1
                } else {
                    self.half_page(self.logdawg.height)
                };
                let down = matches!(a, Action::PageDown | Action::HalfPageDown);
                let top = self.logdawg.top;
                let (top, cursor) = self.with_logdawg_lines(|l| l.half_page(top, cur, n, down));
                self.logdawg.top = top;
                self.logdawg.follow = cursor == last;
                self.logdawg.cursor = cursor;
                return;
            }
            Action::Activate => return self.logdawg_open(),
            _ => return,
        };
        self.logdawg.cursor = cursor;
        // at the newest line the view follows new ones again, as the request list does
        self.logdawg.follow = cursor == last && self.prefs.follow;
        let top = self.logdawg.top;
        self.logdawg.top = self.with_logdawg_lines(|l| l.show(top, cursor, 0));
    }

    /// The line under the cursor.
    fn logdawg_line(&self) -> Option<LogLine<'_>> {
        let id = self.logdawg.id(self.logdawg.cursor)?;
        self.view_store().logs().get(id)
    }

    /// A line as `logcat -v threadtime` prints it (each of its lines with the header).
    fn threadtime(&self, l: &LogLine<'_>) -> String {
        let date = fmt::iso8601(l.wall_ms);
        // 2026-10-10T10:15:31.123+05:30 → 10-10 10:15:31.123
        let stamp = date.get(5..23).map_or(date.clone(), |d| d.replacen('T', " ", 1));
        l.message
            .split('\n')
            .map(|m| format!("{stamp} {:>5} {:>5} {} {}: {m}", l.pid, l.tid, l.level.letter(), l.tag))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `y`: the line under the cursor, as logcat prints it; a request, as cURL.
    pub(crate) fn logdawg_copy(&mut self) {
        if let Some(Row::Request(i)) = self.logdawg.row(self.logdawg.cursor) {
            match self.curl_text(i) {
                Ok(text) => self.copy_text(text),
                Err(e) => self.flash(e),
            }
            return;
        }
        let Some(text) = self.logdawg_line().map(|l| self.threadtime(&l)) else {
            self.flash("no line to copy");
            return;
        };
        self.copy_text(text);
    }

    /// Enter on a request's row: the request, open in the Connection View.
    fn open_logged_request(&mut self, i: TxnIdx) {
        self.set_view(crate::app::View::Connections);
        self.refresh();
        if self.view_rows().position(i).is_none() {
            self.flash("this request is not in the list: its filter or range hides it (/ and Esc change them)");
            return;
        }
        self.select_txn(i);
        self.open_detail();
    }

    /// `R`: requests among the lines, or not.
    pub(crate) fn toggle_logged_requests(&mut self) {
        let on = !self.logdawg.requests;
        self.logdawg.set_requests(on);
        self.flash(if on {
            "requests among the lines (R hides them)"
        } else {
            "only the log's lines (R shows the requests)"
        });
    }

    /// Enter: the whole line in a box, its fields above the message; a request opens in view 1.
    fn logdawg_open(&mut self) {
        if let Some(Row::Request(i)) = self.logdawg.row(self.logdawg.cursor) {
            return self.open_logged_request(i);
        }
        let Some((title, lines)) = self.logdawg_line().map(|l| {
            let logs = self.view_store().logs();
            let process = logs.process_of(l.pid, l.uid).unwrap_or("?");
            let time = fmt::iso8601(l.wall_ms);
            let buffer = match l.buffer {
                0 => "main",
                1 => "radio",
                3 => "system",
                BUFFER_CRASH => "crash",
                7 => "kernel",
                _ => "other",
            };
            let uid = l.uid.map_or("?".to_string(), |u| u.to_string());
            let mut out = Vec::new();
            for (k, v) in [
                ("time", time.as_str()),
                ("level", l.level.name()),
                ("tag", &shown(l.tag)),
                ("process", &shown(process)),
                ("pid", &l.pid.to_string()),
                ("tid", &l.tid.to_string()),
                ("uid", &uid),
                ("buffer", buffer),
            ] {
                let mut line = StyledLine::new();
                line.push(&format!("{k:<8}"), Tok::Key).push(v, Tok::Plain);
                out.push(line);
            }
            out.push(StyledLine::new());
            out.extend(shown(l.message).split('\n').map(StyledLine::plain));
            (format!("{} · {}", l.tag, l.level.name()), out)
        }) else {
            return;
        };
        self.decoded = Some(crate::values::Decoded { title, lines, scroll: Top::default(), page: 10, width: 60 });
        self.overlay = crate::app::Overlay::Decoded;
    }

    /// The wheel over the list: the view moves three rows, the cursor stays on it.
    pub(crate) fn logdawg_scroll(&mut self, down: bool) {
        let (top, cursor) = (self.logdawg.top, self.logdawg.cursor);
        let (top, cursor) = self.with_logdawg_lines(|l| {
            let (top, _) = l.scroll(top, if down { 3 } else { -3 });
            (top, l.keep_cursor(top, cursor))
        });
        self.logdawg.top = top;
        self.logdawg.cursor = cursor;
        self.logdawg.follow = false;
    }

    /// A click on the `i`th line.
    pub(crate) fn logdawg_click(&mut self, i: usize, double: bool) {
        self.focus = Focus::List;
        self.logdawg.cursor = i;
        self.logdawg.follow = false;
        if double {
            self.logdawg_open();
        }
    }
}

fn pad(s: &str, w: usize) -> String {
    let cut = crate::ui::truncate(s, w);
    let room = w.saturating_sub(cut.width());
    format!("{cut}{}", " ".repeat(room))
}

/// A process name cut from the left, so its end shows (`…ms.persistent`).
fn pad_left_cut(s: &str, w: usize) -> String {
    if s.width() <= w {
        return format!("{s:<w$}");
    }
    let mut out: Vec<char> = Vec::new();
    let mut used = 1;
    for c in s.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > w {
            break;
        }
        used += cw;
        out.push(c);
    }
    out.reverse();
    format!("…{}", out.into_iter().collect::<String>())
}

/// A request among the lines: at its start, the method, the status (live) and how long it took
/// over the tag's columns, and the URL in the message's (`left` on).
#[allow(clippy::too_many_arguments)]
fn request_row(
    app: &App,
    t: &Transaction,
    row: Rect,
    buf: &mut Buffer,
    selected: bool,
    origin: Ts,
    wall: bool,
    left: usize,
) {
    let th = &app.theme;
    let base = if selected { th.selected() } else { Style::default() };
    if selected {
        crate::ui::fill(buf, row, th.selected());
    }
    let time = if wall {
        app.view_store().wall_ms(t.start).map_or_else(String::new, fmt::wall_clock)
    } else if t.start >= origin {
        fmt::offset(t.start - origin)
    } else {
        format!("-{}", fmt::offset(origin - t.start))
    };
    let took = fmt::duration(t.duration(app.now()));
    let (method, status) = (format!("{:<7}", t.method), format!("{:<9}", t.status_text()));
    let took = format!("{took:>8}");
    let rest = left.saturating_sub(TIME_W + 1 + IDS_W + 1 + method.width() + status.width() + took.width());
    let spans = vec![
        Span::styled(format!("{time:<TIME_W$} "), th.dim().patch(base)),
        Span::styled(format!("{:<w$}", "⇄ request", w = IDS_W + 1), th.accent().patch(base)),
        Span::styled(method, th.title().patch(base)),
        Span::styled(status, th.status(t.status_class()).patch(base)),
        Span::styled(took, th.dim().patch(base)),
        Span::styled(" ".repeat(rest), base),
        Span::styled(t.url.raw.clone(), th.text().patch(base)),
    ];
    crate::detail::draw_line(buf, row.x, row.y, row.width, &Line::from(spans), 0, base);
}

/// The list: the lines that pass the filter and the requests, from the view's top.
pub fn draw(app: &mut App, r: Rect, buf: &mut Buffer) {
    let t = app.theme.clone();
    // no line shows: why, on the first row (the requests, if any, below it)
    if app.logdawg.lines() == 0 {
        let logs = app.view_store().logs();
        let note = if logs.is_empty() {
            match &logs.info().status {
                Some(s) => format!("{s}…"),
                None if app.replay.is_some() => "this session has no log".to_string(),
                None => "no log yet".to_string(),
            }
        } else {
            let filter = app.logdawg.filter.as_ref().map_or(String::new(), |f| f.source.clone());
            format!("{} lines, none passes {filter} · / changes the filter", logs.len())
        };
        crate::ui::text(buf, r.x, r.y, r.width, vec![Span::styled(note, t.dim())]);
    }
    let r = if app.logdawg.lines() == 0 { Rect { y: r.y + 1, height: r.height.saturating_sub(1), ..r } } else { r };
    let show_process = r.width >= PROCESS_FROM;
    let left = TIME_W + 1 + IDS_W + 1 + TAG_W + 1 + if show_process { PROCESS_W + 1 } else { 0 } + 3 + 1;
    let width = usize::from(r.width).saturating_sub(left).max(10);
    app.logdawg.width = width;
    app.logdawg.height = usize::from(r.height);
    let focused = app.focus == Focus::List;
    if app.logdawg.is_empty() || r.height == 0 {
        return;
    }
    // the view: following, the newest line at the bottom; else the cursor's line on it
    let (cursor, top) = (app.logdawg.cursor, app.logdawg.top);
    let top = app.with_logdawg_lines(|l| {
        if app.logdawg.follow {
            l.last_top()
        } else {
            let top = l.clamp(top);
            if cursor >= top.line && l.row_of(top, cursor) < l.view { top } else { l.show(top, cursor, 0) }
        }
    });
    app.logdawg.top = top;
    let origin = app.view_store().origin();
    let wall = app.wall_labels;
    let wrapping = app.prefs.wrap;
    let logs = app.view_store().logs().clone();
    let mut y = r.y;
    let bottom = r.y + r.height;
    let mut i = top.line;
    let mut skip = top.part;
    while y < bottom {
        let line = match app.logdawg.row(i) {
            None => break,
            Some(Row::Request(idx)) => {
                let selected = focused && i == cursor;
                let row = Rect { x: r.x, y, width: r.width, height: 1 };
                if let Some(t) = app.view_store().txns().get(idx as usize).cloned() {
                    request_row(app, &t, row, buf, selected, origin, wall, left);
                }
                app.hits.add(row, Target::LogLine(i));
                (y, skip, i) = (y + 1, 0, i + 1);
                continue;
            }
            Some(Row::Line(id)) => match logs.get(id) {
                Some(l) => l,
                None => break,
            },
        };
        let (badge, text_style) = t.log_level(line.level);
        let selected = focused && i == cursor;
        let base = if selected { t.selected() } else { Style::default() };
        let message = shown(line.message);
        let rows = message_rows(&message, width, wrapping);
        for (k, &(start, end)) in rows.iter().enumerate().skip(skip) {
            if y >= bottom {
                break;
            }
            let row = Rect { x: r.x, y, width: r.width, height: 1 };
            if selected {
                crate::ui::fill(buf, row, t.selected());
            }
            let mut spans: Vec<Span<'static>> = Vec::new();
            if k == 0 {
                let time = line_time(&line, origin, wall);
                spans.push(Span::styled(format!("{time:<TIME_W$} "), t.dim().patch(base)));
                spans.push(Span::styled(format!("{:>6}-{:<6} ", line.pid, line.tid), t.faint().patch(base)));
                spans.push(Span::styled(
                    format!("{} ", pad(&shown(line.tag), TAG_W)),
                    t.log_tag(line.tag_id).patch(base),
                ));
                if show_process {
                    let name = shown(logs.process_of(line.pid, line.uid).unwrap_or(""));
                    spans.push(Span::styled(format!("{} ", pad_left_cut(&name, PROCESS_W)), t.dim().patch(base)));
                }
                spans.push(Span::styled(format!(" {} ", line.level.letter()), badge));
                spans.push(Span::styled(" ", base));
            } else {
                spans.push(Span::styled(" ".repeat(left), base));
            }
            let piece = &message[start.min(message.len())..end.min(message.len())];
            spans.push(Span::styled(piece.to_string(), text_style.patch(base)));
            crate::detail::draw_line(buf, r.x, y, r.width, &Line::from(spans), 0, base);
            app.hits.add(row, Target::LogLine(i));
            y += 1;
        }
        skip = 0;
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_not_drawn() {
        assert_eq!(shown("plain"), "plain");
        assert_eq!(shown("FATAL\n\tat a.B(C.kt:1)"), "FATAL\n    at a.B(C.kt:1)");
        assert_eq!(shown("a\r\nb\u{1b}[32mgreen"), "a\nb\u{fffd}[32mgreen");
    }

    #[test]
    fn rows_of_a_message_of_several_lines_and_a_long_one() {
        assert_eq!(message_rows("one\ntwo", 20, true), vec![(0, 3), (4, 7)]);
        let long = "word ".repeat(10);
        let rows = message_rows(long.trim_end(), 12, true);
        assert!(rows.len() > 3, "{rows:?}");
        assert_eq!(message_rows(long.trim_end(), 12, false).len(), 1, "cut, not wrapped");
        assert_eq!(message_rows("", 12, true), vec![(0, 0)]);
    }

    #[test]
    fn a_long_process_name_shows_its_end() {
        assert_eq!(pad_left_cut("com.google.android.gms.persistent", 14), "…ms.persistent");
        assert_eq!(pad_left_cut("system_server", 14), "system_server ");
    }
}
