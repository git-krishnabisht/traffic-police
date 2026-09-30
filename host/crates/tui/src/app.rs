//! Application state and input handling. Rendering reads this; nothing here draws.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::{BackendCommand, Capabilities};
use traffic_police_core::event::MarkerKind;
use traffic_police_core::fmt::{NS_PER_MS, NS_PER_SEC, Ts};
use traffic_police_core::model::{BodyDir, TxnIdx};
use traffic_police_core::rows::{Column, Row, RowModel, Sort};
use traffic_police_core::store::{GraphSource, SessionStore};
use traffic_police_proto::msg::RuleSet;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::bodycache::{BodyCache, BodyJob, Lookup};
use crate::bodyview::BodyView;
use crate::detail::{self, DocRow};
use crate::images::Images;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Connections,
    Threads,
    Rules,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Graph,
    List,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Response,
    Request,
    CallStack,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Overview, Tab::Response, Tab::Request, Tab::CallStack];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Response => "Response",
            Tab::Request => "Request",
            Tab::CallStack => "Call Stack",
        }
    }
    fn index(self) -> usize {
        Tab::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Help,
    Columns { cursor: usize },
    ConfirmClear,
    Jq,
}

/// Graph zoom levels (visible window width).
pub const SPANS: [u64; 13] = [
    NS_PER_SEC,
    2 * NS_PER_SEC,
    5 * NS_PER_SEC,
    10 * NS_PER_SEC,
    15 * NS_PER_SEC,
    30 * NS_PER_SEC,
    60 * NS_PER_SEC,
    120 * NS_PER_SEC,
    300 * NS_PER_SEC,
    600 * NS_PER_SEC,
    1800 * NS_PER_SEC,
    3600 * NS_PER_SEC,
    4 * 3600 * NS_PER_SEC,
];
pub const DEFAULT_SPAN: u64 = 30 * NS_PER_SEC;

#[derive(Debug, Clone, Default)]
pub struct GraphState {
    pub span: u64,
    /// `None`: follow the live edge. `Some(t)`: the window ends at `t`.
    pub pinned_right: Option<Ts>,
    /// Keyboard cursor on the graph (graph focus).
    pub cursor: Option<Ts>,
    /// Start of a range selection in progress.
    pub anchor: Option<Ts>,
    pub selection: Option<(Ts, Ts)>,
}

#[derive(Debug, Clone)]
pub struct DetailState {
    pub tab: Tab,
    pub cursor: usize,
    pub scroll: usize,
    pub hscroll: u16,
    pub parsed: bool,
    pub original: bool,
    pub expanded_runs: HashSet<usize>,
}

impl Default for DetailState {
    fn default() -> Self {
        DetailState {
            tab: Tab::Overview,
            cursor: 0,
            scroll: 0,
            hscroll: 0,
            parsed: true,
            original: false,
            expanded_runs: HashSet::new(),
        }
    }
}

/// Screen regions recorded while drawing, for mouse hit-testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    ListRow(usize),
    ListHeader(Column),
    ViewTab(View),
    DetailTab(Tab),
    DetailLine(usize),
    DetailClose,
    Divider,
    Graph,
    ThreadBar(usize),
    RuleRow(usize),
    List,
}

#[derive(Debug, Clone, Default)]
pub struct HitMap(Vec<(Rect, Target)>);

impl HitMap {
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn add(&mut self, r: Rect, t: Target) {
        self.0.push((r, t));
    }
    pub fn at(&self, x: u16, y: u16) -> Option<Target> {
        self.0
            .iter()
            .rev()
            .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|&(_, t)| t)
    }
    pub fn rect_of(&self, t: Target) -> Option<Rect> {
        self.0.iter().rev().find(|(_, x)| *x == t).map(|&(r, _)| r)
    }
}

#[derive(Debug, Clone, Copy)]
enum Drag {
    Divider,
    Graph { anchor: Ts },
}

/// Outcome of a jq filter: `Ok((outputs, truncated))` or an error message.
pub type JqResult = Result<(Vec<String>, bool), String>;

/// Runs a jq filter over a body.
pub type JqRunner = fn(&str, &[u8]) -> JqResult;

/// In-process jq (tests); the binary installs a child-process runner with a time limit.
pub fn inproc_jq(filter: &str, input: &[u8]) -> JqResult {
    traffic_police_core::jq::run(filter, input, 200).map(|o| (o.values, o.truncated)).map_err(|e| e.to_string())
}

/// A filter waiting to run. The event loop runs it off the UI task and hands the result to
/// [`App::finish_jq`].
#[derive(Debug, Clone)]
pub struct JqJob {
    pub txn: TxnIdx,
    pub dir: BodyDir,
    pub filter: String,
    pub bytes: bytes::Bytes,
}

struct Frozen {
    store: SessionStore,
    rows: RowModel,
    now: Ts,
    events_since: u64,
}

/// Where the Thread View's bars are, flattened in visual order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    pub lane: usize,
    pub sub: usize,
    pub txn: TxnIdx,
}

pub struct App {
    pub store: SessionStore,
    pub rows: RowModel,
    pub theme: Theme,
    pub view: View,
    pub focus: Focus,
    pub selected: Option<TxnIdx>,
    pub list_cursor: usize,
    pub list_offset: usize,
    pub list_height: usize,
    pub detail_open: bool,
    pub detail: DetailState,
    pub detail_height: usize,
    pub graph: GraphState,
    pub graph_source: GraphSource,
    pub wall_labels: bool,
    pub columns: Vec<Column>,
    pub split_pct: u16,
    pub overlay: Overlay,
    pub jq_input: Input,
    pub message: Option<(String, Instant)>,
    pub rules: Option<RuleSet>,
    pub rules_cursor: usize,
    pub bars: Vec<Bar>,
    pub bar_cursor: usize,
    pub recording: bool,
    pub caps: Capabilities,
    pub commands: Option<mpsc::UnboundedSender<BackendCommand>>,
    pub hits: HitMap,
    pub should_quit: bool,
    /// Fixed "now" for tests and single-frame renders.
    pub now_override: Option<Ts>,
    pub images: Images,
    pub jq_runner: JqRunner,
    jq_jobs: Vec<JqJob>,
    jq_running: usize,
    pub source_roots: Vec<PathBuf>,
    /// Set by input; the terminal loop suspends the UI and opens `$EDITOR`.
    pub editor_request: Option<(PathBuf, u32)>,
    pub area: Rect,
    frozen: Option<Box<Frozen>>,
    live_anchor: Option<(Ts, Instant)>,
    bodies: BodyCache,
    last_click: Option<(Instant, u16, u16)>,
    drag: Option<Drag>,
    follow_list: bool,
}

impl App {
    pub fn new(store: SessionStore, theme: Theme) -> Self {
        App {
            store,
            rows: RowModel::new(),
            theme,
            view: View::Connections,
            focus: Focus::List,
            selected: None,
            list_cursor: 0,
            list_offset: 0,
            list_height: 10,
            detail_open: false,
            detail: DetailState::default(),
            detail_height: 10,
            graph: GraphState { span: DEFAULT_SPAN, ..Default::default() },
            graph_source: GraphSource::AppTotal,
            wall_labels: false,
            columns: Column::DEFAULT.to_vec(),
            split_pct: 55,
            overlay: Overlay::None,
            jq_input: Input::default(),
            message: None,
            rules: None,
            rules_cursor: 0,
            bars: Vec::new(),
            bar_cursor: 0,
            recording: true,
            caps: Capabilities::default(),
            commands: None,
            hits: HitMap::default(),
            should_quit: false,
            now_override: None,
            images: Images::halfblocks(),
            jq_runner: inproc_jq,
            jq_jobs: Vec::new(),
            jq_running: 0,
            source_roots: Vec::new(),
            editor_request: None,
            area: Rect::default(),
            frozen: None,
            live_anchor: None,
            bodies: BodyCache::new(),
            last_click: None,
            drag: None,
            follow_list: true,
        }
    }

    // --- data --------------------------------------------------------------------------------

    /// Apply a batch from a backend. Always goes to the live store, also while frozen.
    pub fn ingest(&mut self, batch: Vec<SessionEvent>) {
        let before = self.store.latest();
        let n = batch.len() as u64;
        for e in batch {
            self.store.apply(e);
        }
        if let Some(f) = &mut self.frozen {
            f.events_since += n;
        }
        if self.store.latest() > before {
            self.live_anchor = Some((self.store.latest(), Instant::now()));
        }
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }

    pub fn frozen_events(&self) -> Option<u64> {
        self.frozen.as_ref().map(|f| f.events_since)
    }

    /// The store the UI shows (a snapshot while frozen).
    pub fn view_store(&self) -> &SessionStore {
        self.frozen.as_ref().map_or(&self.store, |f| &f.store)
    }

    pub fn view_rows(&self) -> &RowModel {
        self.frozen.as_ref().map_or(&self.rows, |f| &f.rows)
    }

    /// Current device time for live displays.
    pub fn now(&self) -> Ts {
        if let Some(f) = &self.frozen {
            return f.now;
        }
        if let Some(t) = self.now_override {
            return t;
        }
        // Between batches, extrapolate from the last event so live bars grow smoothly; stop after
        // 1.5 s without events so a quiet or detached app does not drift ahead of its data.
        match self.live_anchor {
            Some((ts, at)) => ts + at.elapsed().min(Duration::from_millis(1500)).as_nanos() as u64,
            None => self.store.latest(),
        }
        .max(self.store.latest())
    }

    /// Visible graph window `[left, right)`.
    pub fn window(&self) -> (Ts, Ts) {
        let right = self.graph.pinned_right.unwrap_or_else(|| self.now());
        (right.saturating_sub(self.graph.span), right)
    }

    /// Time span of the Timeline column and the Thread View: the selected range if there is
    /// one, otherwise the graph window.
    pub fn list_window(&self) -> (Ts, Ts) {
        self.graph.selection.unwrap_or_else(|| self.window())
    }

    pub fn is_live(&self) -> bool {
        self.graph.pinned_right.is_none() && self.frozen.is_none()
    }

    /// Refresh derived rows and keep the cursor and selection consistent.
    pub fn refresh(&mut self) {
        let now = self.now();
        let range = self.graph.selection;
        let frozen = self.frozen.is_some();
        let (store, rows) = match &mut self.frozen {
            Some(f) => (&f.store, &mut f.rows),
            None => (&self.store, &mut self.rows),
        };
        rows.set_range(range);
        rows.refresh(store, now);
        let len = rows.len();
        if len == 0 {
            self.list_cursor = 0;
            self.list_offset = 0;
            return;
        }
        if self.follow_list && !self.detail_open && !frozen {
            self.list_cursor = len - 1;
        } else if let Some(sel) = self.selected
            && let Some(pos) = rows.position(sel)
        {
            self.list_cursor = pos;
        }
        self.list_cursor = self.list_cursor.min(len - 1);
        if self.selected.is_none() || (self.follow_list && !self.detail_open) {
            self.selected = Some(rows.rows()[self.list_cursor].txn());
        }
        self.clamp_list_offset();
    }

    pub fn clamp_list_offset(&mut self) {
        let h = self.list_height.max(1);
        if self.list_cursor < self.list_offset {
            self.list_offset = self.list_cursor;
        } else if self.list_cursor >= self.list_offset + h {
            self.list_offset = self.list_cursor + 1 - h;
        }
        let len = self.view_rows().len();
        if len <= h {
            self.list_offset = 0;
        } else if self.list_offset > len - h {
            self.list_offset = len - h;
        }
    }

    /// The body view for a transaction's body, cached until the body changes. Large bodies are
    /// decoded on a worker: until that finishes this returns the previous view, or `None`
    /// (see [`App::body_decoding`]).
    pub fn body_view(&mut self, txn: TxnIdx, dir: BodyDir) -> Option<&mut BodyView> {
        let store = self.frozen.as_ref().map_or(&self.store, |f| &f.store);
        let t = store.txn(txn);
        let (meta, headers) = match dir {
            BodyDir::Request => (&t.req_body, Some(&t.req_headers)),
            BodyDir::Response => (&t.resp_body, t.resp.as_ref().map(|r| &r.headers)),
            BodyDir::Delivered => (t.delivered_body.as_ref()?, t.delivered.as_ref().map(|d| &d.headers)),
        };
        meta.id?;
        let key = meta.captured * 16 + meta.state as u64;
        let epoch = self.bodies.epoch();
        let lookup = self.bodies.lookup(txn, dir, key, meta.captured as usize, || BodyJob {
            epoch,
            txn,
            dir,
            key,
            raw: store.body_bytes(meta),
            headers: headers.cloned(),
        });
        if let Lookup::BuildHere = lookup {
            let view = BodyView::build(store.body_bytes(meta), headers);
            self.bodies.insert(txn, dir, key, view);
        }
        self.bodies.get_mut(txn, dir)
    }

    /// Whether a worker is decoding this body.
    pub fn body_decoding(&self, txn: TxnIdx, dir: BodyDir) -> bool {
        self.bodies.is_building(txn, dir)
    }

    /// Bodies queued for decoding, for the event loop to run.
    pub fn take_body_jobs(&mut self) -> Vec<BodyJob> {
        self.bodies.take_jobs()
    }

    pub fn finish_body(&mut self, job: BodyJob, view: BodyView) {
        self.bodies.finish(&job, view);
    }

    /// Which response body the detail pane shows.
    pub fn response_dir(&self, txn: TxnIdx) -> BodyDir {
        let t = self.view_store().txn(txn);
        if t.delivered_body.is_some() && !self.detail.original { BodyDir::Delivered } else { BodyDir::Response }
    }

    // --- messages and commands ---------------------------------------------------------------

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    pub fn current_message(&self) -> Option<&str> {
        self.message.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(4)).map(|(m, _)| m.as_str())
    }

    fn send(&mut self, cmd: BackendCommand) -> bool {
        match &self.commands {
            Some(tx) => tx.send(cmd).is_ok(),
            None => false,
        }
    }

    // --- selection ---------------------------------------------------------------------------

    fn select_row(&mut self, row: usize) {
        let len = self.view_rows().len();
        if len == 0 {
            return;
        }
        let row = row.min(len - 1);
        self.list_cursor = row;
        self.follow_list = row == len - 1 && self.is_live();
        let txn = self.view_rows().rows()[row].txn();
        if self.selected != Some(txn) {
            self.selected = Some(txn);
            self.detail.cursor = 0;
            self.detail.scroll = 0;
            self.detail.hscroll = 0;
            self.detail.expanded_runs.clear();
            self.detail.original = false;
        }
        self.clamp_list_offset();
    }

    pub fn select_txn(&mut self, txn: TxnIdx) {
        if let Some(pos) = self.view_rows().position(txn) {
            self.select_row(pos);
        } else {
            self.selected = Some(txn);
            self.detail.cursor = 0;
            self.detail.scroll = 0;
        }
    }

    fn open_detail(&mut self) {
        if self.selected.is_some() {
            self.detail_open = true;
            self.focus = Focus::Detail;
        }
    }

    fn close_detail(&mut self) {
        self.detail_open = false;
        if self.focus == Focus::Detail {
            self.focus = Focus::List;
        }
    }

    // --- graph -------------------------------------------------------------------------------

    fn zoom(&mut self, dir: i32) {
        let i = SPANS.iter().position(|&s| s >= self.graph.span).unwrap_or(SPANS.len() - 1) as i32;
        let j = (i - dir).clamp(0, SPANS.len() as i32 - 1) as usize;
        let (_, right) = self.window();
        let old = self.graph.span;
        self.graph.span = SPANS[j];
        if let Some(c) = self.graph.cursor {
            // keep the cursor at the same relative position
            let frac = (right.saturating_sub(c)) as f64 / old as f64;
            let new_right = c + (frac * self.graph.span as f64) as u64;
            if self.graph.pinned_right.is_some() {
                self.graph.pinned_right = Some(new_right);
            }
        }
        self.flash(format!("window {}", traffic_police_core::fmt::duration(self.graph.span)));
    }

    fn pan(&mut self, frac: f64) {
        let (left, right) = self.window();
        let step = (self.graph.span as f64 * frac.abs()) as u64;
        let cursor = self.graph.cursor.unwrap_or(right.saturating_sub(self.graph.span / 2));
        let c = if frac < 0.0 { cursor.saturating_sub(step) } else { cursor + step };
        let origin = self.view_store().origin();
        let live_right = self.now();
        let c = c.clamp(origin, live_right);
        self.graph.cursor = Some(c);
        if c < left {
            self.graph.pinned_right = Some(c + self.graph.span);
        } else if c > right {
            let r = c.min(live_right);
            self.graph.pinned_right = if r >= live_right { None } else { Some(r) };
        }
    }

    /// Move the graph window by `frac` of its width (negative: back in time). Reaching the live
    /// edge resumes following it.
    fn shift_window(&mut self, frac: f64) {
        let (_, right) = self.window();
        let live = self.now();
        let step = (self.graph.span as f64 * frac.abs()) as u64;
        let r = if frac < 0.0 { right.saturating_sub(step) } else { right + step };
        let earliest = self.view_store().origin() + self.graph.span / 10;
        let r = r.max(earliest.min(live));
        self.graph.pinned_right = if r >= live { None } else { Some(r) };
        if self.graph.pinned_right.is_none() {
            self.follow_list = true;
        }
    }

    fn apply_graph_selection(&mut self, a: Ts, b: Ts) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        if hi - lo < NS_PER_MS {
            self.graph.selection = None;
            return;
        }
        self.graph.selection = Some((lo, hi));
        self.rows.invalidate();
        self.follow_list = false;
        self.flash("showing requests in the selected range · Esc clears");
    }

    fn clear_graph_selection(&mut self) -> bool {
        let had = self.graph.selection.is_some() || self.graph.anchor.is_some();
        self.graph.selection = None;
        self.graph.anchor = None;
        self.rows.invalidate();
        had
    }

    /// Time at a column of the graph plot, if the point is on the plot.
    pub fn graph_time_at(&self, x: u16) -> Option<Ts> {
        let r = self.hits.rect_of(Target::Graph)?;
        if x < r.x || x >= r.x + r.width {
            return None;
        }
        let (left, right) = self.window();
        let frac = f64::from(x - r.x) / f64::from(r.width.max(1));
        Some(left + ((right - left) as f64 * frac) as u64)
    }

    // --- freeze, pause, clear ----------------------------------------------------------------

    fn toggle_freeze(&mut self) {
        if self.frozen.take().is_some() {
            self.flash("unfrozen");
            self.rows.invalidate();
        } else {
            let now = self.now();
            let mut rows = self.rows.clone();
            rows.invalidate();
            self.frozen = Some(Box::new(Frozen { store: self.store.clone(), rows, now, events_since: 0 }));
            self.flash("frozen: capture continues in the background · F to unfreeze");
        }
    }

    fn toggle_recording(&mut self) {
        if !self.caps.pause {
            self.flash("this source cannot pause");
            return;
        }
        self.recording = !self.recording;
        let on = self.recording;
        self.send(BackendCommand::SetRecording(on));
        let now = self.now();
        let (kind, label) =
            if on { (MarkerKind::Resume, "recording resumed") } else { (MarkerKind::Pause, "recording paused") };
        self.store.apply(SessionEvent::Marker { source: None, at: now, kind, label: label.into() });
        self.flash(label);
    }

    fn clear_session(&mut self) {
        let ids = self.store.source_ids();
        let sources: Vec<_> = self.store.sources().cloned().collect();
        let mut store = SessionStore::with_source_ids(ids);
        for s in sources.into_iter().filter(|s| s.ended.is_none()) {
            store.apply(SessionEvent::SourceUp(Box::new(s)));
        }
        self.store = store;
        self.rows = RowModel::new();
        self.frozen = None;
        self.selected = None;
        self.detail_open = false;
        self.focus = Focus::List;
        self.bodies.clear();
        self.list_cursor = 0;
        self.list_offset = 0;
        self.follow_list = true;
        self.graph.selection = None;
        self.flash("session cleared");
    }

    // --- input -------------------------------------------------------------------------------

    pub fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.handle_key(k),
            Event::Mouse(m) => self.handle_mouse(m),
            Event::Paste(s) if self.overlay == Overlay::Jq => {
                for c in s.chars().filter(|c| !c.is_control()) {
                    self.jq_input.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
                }
            }
            _ => {}
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) {
        // keys can arrive faster than frames; act on the rows as they are now
        self.refresh();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.overlay {
            Overlay::Help => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::ConfirmClear => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.clear_session();
                }
                self.overlay = Overlay::None;
                return;
            }
            Overlay::Columns { cursor } => {
                let n = Column::OPTIONAL.len();
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.overlay = Overlay::Columns { cursor: (cursor + n - 1) % n }
                    }
                    KeyCode::Down | KeyCode::Char('j') => self.overlay = Overlay::Columns { cursor: (cursor + 1) % n },
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        let col = Column::OPTIONAL[cursor];
                        if let Some(i) = self.columns.iter().position(|&c| c == col) {
                            self.columns.remove(i);
                        } else {
                            let at =
                                self.columns.iter().position(|&c| c == Column::Timeline).unwrap_or(self.columns.len());
                            self.columns.insert(at, col);
                        }
                    }
                    _ => self.overlay = Overlay::None,
                }
                return;
            }
            Overlay::Jq => {
                match k.code {
                    KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        self.run_jq();
                    }
                    _ => {
                        self.jq_input.handle_event(&Event::Key(k));
                    }
                }
                return;
            }
            Overlay::None => {}
        }

        // global keys
        match k.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                return;
            }
            KeyCode::Char('1') => return self.set_view(View::Connections),
            KeyCode::Char('2') => return self.set_view(View::Threads),
            KeyCode::Char('3') => return self.set_view(View::Rules),
            KeyCode::Tab => return self.cycle_focus(1),
            KeyCode::BackTab => return self.cycle_focus(-1),
            KeyCode::Char(' ') => return self.toggle_recording(),
            KeyCode::Char('F') => return self.toggle_freeze(),
            KeyCode::Char('L') => {
                self.graph.pinned_right = None;
                self.graph.cursor = None;
                self.follow_list = true;
                if !self.detail_open {
                    self.list_cursor = self.view_rows().len().saturating_sub(1);
                }
                self.flash("following live");
                return;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => return self.zoom(1),
            KeyCode::Char('-') => return self.zoom(-1),
            KeyCode::Char('0') => {
                self.graph.span = DEFAULT_SPAN;
                self.flash("window reset");
                return;
            }
            KeyCode::Char('T') => {
                self.graph_source = self.graph_source.toggled();
                self.flash(format!("graph: {}", self.graph_source.label()));
                return;
            }
            KeyCode::Char('t') => {
                self.wall_labels = !self.wall_labels;
                self.flash(if self.wall_labels { "time axis: wall clock" } else { "time axis: since session start" });
                return;
            }
            KeyCode::Char('c') if self.focus != Focus::Detail => {
                let on = !self.rows.collapse;
                self.rows.set_collapse(on);
                if let Some(f) = &mut self.frozen {
                    f.rows.set_collapse(on);
                }
                self.flash(if on { "collapsing repeated calls" } else { "showing every call" });
                return;
            }
            KeyCode::Char('C') => {
                self.overlay = Overlay::Columns { cursor: 0 };
                return;
            }
            KeyCode::Char('s') if self.focus != Focus::Detail => return self.cycle_sort(),
            KeyCode::Char('S') if self.focus != Focus::Detail => {
                let mut s = self.rows.sort;
                s.descending = !s.descending;
                self.set_sort(s);
                return;
            }
            KeyCode::Char('x') => {
                self.overlay = Overlay::ConfirmClear;
                return;
            }
            KeyCode::Char('v') if self.focus != Focus::Detail => {
                self.focus = Focus::Graph;
                let (_, right) = self.window();
                let c = self.graph.cursor.unwrap_or(right.saturating_sub(self.graph.span / 4));
                self.graph.cursor = Some(c);
                match self.graph.anchor.take() {
                    None => {
                        self.graph.anchor = Some(c);
                        self.flash("range: move with ←/→, v or Enter to apply, Esc to cancel");
                    }
                    Some(a) => self.apply_graph_selection(a, c),
                }
                return;
            }
            KeyCode::Esc => {
                if self.focus == Focus::Graph && self.clear_graph_selection() {
                    return;
                }
                if self.detail_open {
                    self.close_detail();
                    return;
                }
                if self.clear_graph_selection() {
                    return;
                }
                if self.focus == Focus::Graph {
                    self.focus = Focus::List;
                }
                return;
            }
            _ => {}
        }

        match self.focus {
            Focus::Graph => self.graph_key(k),
            Focus::List => match self.view {
                View::Connections => self.list_key(k),
                View::Threads => self.threads_key(k),
                View::Rules => self.rules_key(k),
            },
            Focus::Detail => self.detail_key(k),
        }
    }

    fn set_view(&mut self, v: View) {
        self.view = v;
        if self.focus == Focus::Graph {
            self.focus = Focus::List;
        }
    }

    fn cycle_focus(&mut self, dir: i32) {
        let order: Vec<Focus> = if self.detail_open {
            vec![Focus::Graph, Focus::List, Focus::Detail]
        } else {
            vec![Focus::Graph, Focus::List]
        };
        let i = order.iter().position(|&f| f == self.focus).unwrap_or(1) as i32;
        let n = order.len() as i32;
        self.focus = order[((i + dir).rem_euclid(n)) as usize];
    }

    fn cycle_sort(&mut self) {
        let cols: Vec<Column> = self.columns.clone();
        let cur = self.rows.sort.column;
        let i = cols.iter().position(|&c| c == cur).map_or(0, |i| (i + 1) % cols.len());
        self.set_sort(Sort { column: cols[i], descending: false });
    }

    fn set_sort(&mut self, s: Sort) {
        self.rows.set_sort(s);
        if let Some(f) = &mut self.frozen {
            f.rows.set_sort(s);
        }
        self.follow_list = false;
        let what = if s.column == Column::Timeline { "time".to_string() } else { s.column.title().to_lowercase() };
        self.flash(format!("sorted by {what}{}", if s.descending { ", descending" } else { "" }));
    }

    fn page(&self) -> usize {
        self.list_height.max(2) - 1
    }

    fn list_key(&mut self, k: KeyEvent) {
        let len = self.view_rows().len();
        if len == 0 {
            return;
        }
        let cur = self.list_cursor;
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_row(cur.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select_row(cur + 1),
            KeyCode::PageUp => self.select_row(cur.saturating_sub(self.page())),
            KeyCode::PageDown => self.select_row(cur + self.page()),
            KeyCode::Home | KeyCode::Char('g') => self.select_row(0),
            KeyCode::End | KeyCode::Char('G') => self.select_row(len - 1),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                let row = self.view_rows().rows()[cur].clone();
                match row {
                    Row::Group { members, expanded } if k.code != KeyCode::Enter || !expanded => {
                        self.rows.toggle_group(members[0]);
                        if let Some(f) = &mut self.frozen {
                            f.rows.toggle_group(members[0]);
                        }
                    }
                    _ if k.code == KeyCode::Enter => self.open_detail(),
                    _ => {}
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Row::Group { members, expanded: true } = self.view_rows().rows()[cur].clone() {
                    self.rows.toggle_group(members[0]);
                }
            }
            _ => {}
        }
    }

    fn threads_key(&mut self, k: KeyEvent) {
        if self.bars.is_empty() {
            return;
        }
        let n = self.bars.len();
        let cur = self.bar_cursor.min(n - 1);
        let next = match k.code {
            KeyCode::Down | KeyCode::Char('j') => {
                let lane = self.bars[cur].lane;
                self.bars.iter().position(|b| b.lane > lane).unwrap_or(cur)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let lane = self.bars[cur].lane;
                self.bars
                    .iter()
                    .rposition(|b| b.lane < lane)
                    .map(|i| {
                        let l = self.bars[i].lane;
                        self.bars.iter().position(|b| b.lane == l).unwrap_or(i)
                    })
                    .unwrap_or(cur)
            }
            KeyCode::Right | KeyCode::Char('l') => (cur + 1).min(n - 1),
            KeyCode::Left | KeyCode::Char('h') => cur.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => n - 1,
            KeyCode::Enter => {
                let txn = self.bars[cur].txn;
                self.select_txn(txn);
                self.open_detail();
                return;
            }
            _ => cur,
        };
        self.bar_cursor = next;
        let txn = self.bars[next].txn;
        self.select_txn(txn);
    }

    fn rules_key(&mut self, k: KeyEvent) {
        let n = self.rules.as_ref().map_or(0, |r| r.rules.len());
        if n == 0 {
            return;
        }
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.rules_cursor = self.rules_cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.rules_cursor = (self.rules_cursor + 1).min(n - 1),
            KeyCode::Home | KeyCode::Char('g') => self.rules_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.rules_cursor = n - 1,
            _ => {}
        }
    }

    fn graph_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Left | KeyCode::Char('h') => self.pan(-0.05),
            KeyCode::Right | KeyCode::Char('l') => self.pan(0.05),
            KeyCode::PageUp => self.pan(-0.5),
            KeyCode::PageDown => self.pan(0.5),
            KeyCode::Enter => {
                if let (Some(a), Some(c)) = (self.graph.anchor.take(), self.graph.cursor) {
                    self.apply_graph_selection(a, c);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.focus = Focus::List,
            _ => {}
        }
    }

    fn detail_key(&mut self, k: KeyEvent) {
        let Some(txn) = self.selected else { return };
        match k.code {
            KeyCode::Left | KeyCode::Char('h') => return self.set_tab(Tab::ALL[(self.detail.tab.index() + 3) % 4]),
            KeyCode::Right | KeyCode::Char('l') => return self.set_tab(Tab::ALL[(self.detail.tab.index() + 1) % 4]),
            KeyCode::Char('p') => {
                self.detail.parsed = !self.detail.parsed;
                self.flash(if self.detail.parsed { "parsed view" } else { "source view" });
                return;
            }
            KeyCode::Char('o') => {
                let t = self.view_store().txn(txn);
                if t.rule_modified() {
                    self.detail.original = !self.detail.original;
                    self.flash(if self.detail.original {
                        "showing the original response"
                    } else {
                        "showing what the app received"
                    });
                } else {
                    self.flash("no rule changed this response");
                }
                return;
            }
            KeyCode::Char('|') => {
                if matches!(self.detail.tab, Tab::Response | Tab::Request) {
                    self.overlay = Overlay::Jq;
                } else {
                    self.flash("jq filters work on the Response and Request tabs");
                }
                return;
            }
            KeyCode::Char('<') => {
                self.detail.hscroll = self.detail.hscroll.saturating_sub(20);
                return;
            }
            KeyCode::Char('>') => {
                self.detail.hscroll = self.detail.hscroll.saturating_add(20);
                return;
            }
            _ => {}
        }
        let doc = detail::build_doc(self);
        let len = doc.len();
        let cur = self.detail.cursor.min(len.saturating_sub(1));
        let page = self.detail_height.max(2) - 1;
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.detail.cursor = cur.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.detail.cursor = (cur + 1).min(len.saturating_sub(1)),
            KeyCode::PageUp => self.detail.cursor = cur.saturating_sub(page),
            KeyCode::PageDown => self.detail.cursor = (cur + page).min(len.saturating_sub(1)),
            KeyCode::Home | KeyCode::Char('g') => self.detail.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.detail.cursor = len.saturating_sub(1),
            KeyCode::Char('[') | KeyCode::Char(']') => {
                if let Some(dir) = doc.body_dir {
                    let fold = k.code == KeyCode::Char('[');
                    if let Some(v) = self.body_view(txn, dir) {
                        if fold { v.fold_all() } else { v.unfold_all() }
                    }
                }
            }
            KeyCode::Enter => match doc.row(cur) {
                Some(DocRow::Body(i)) => {
                    if let Some(dir) = doc.body_dir
                        && self.detail.parsed
                        && let Some(v) = self.body_view(txn, dir)
                        && v.jq.is_none()
                    {
                        v.toggle_fold(i);
                    }
                }
                Some(DocRow::FrameRun { run }) => {
                    if !self.detail.expanded_runs.remove(&run) {
                        self.detail.expanded_runs.insert(run);
                    }
                }
                Some(DocRow::Frame { index }) => self.open_frame(txn, index),
                _ => {}
            },
            _ => {}
        }
        self.clamp_detail_scroll();
    }

    pub fn clamp_detail_scroll(&mut self) {
        let h = self.detail_height.max(1);
        if self.detail.cursor < self.detail.scroll {
            self.detail.scroll = self.detail.cursor;
        } else if self.detail.cursor >= self.detail.scroll + h {
            self.detail.scroll = self.detail.cursor + 1 - h;
        }
    }

    fn set_tab(&mut self, t: Tab) {
        if self.detail.tab != t {
            self.detail.tab = t;
            self.detail.cursor = 0;
            self.detail.scroll = 0;
            self.detail.hscroll = 0;
        }
    }

    fn run_jq(&mut self) {
        let Some(txn) = self.selected else { return };
        let dir = if self.detail.tab == Tab::Request { BodyDir::Request } else { self.response_dir(txn) };
        let filter = self.jq_input.value().trim().to_string();
        let open = self.view_store().txn(txn).state.is_open();
        let Some(view) = self.body_view(txn, dir) else {
            let why = if self.body_decoding(txn, dir) {
                "the body is still being decoded; try again in a moment"
            } else if open {
                "the body has not arrived yet"
            } else {
                "this request has no body to filter"
            };
            self.flash(why);
            return;
        };
        if filter.is_empty() || filter == "." {
            view.clear_jq();
            self.flash("jq filter cleared");
            return;
        }
        let bytes = view.decoded.bytes.clone();
        self.jq_jobs.push(JqJob { txn, dir, filter, bytes });
        self.jq_running += 1;
        self.flash("running jq…");
    }

    /// Filters queued by input, for the event loop to run.
    pub fn take_jq_jobs(&mut self) -> Vec<JqJob> {
        std::mem::take(&mut self.jq_jobs)
    }

    /// Show a filter's result (ignored if the body changed or the user moved on).
    pub fn finish_jq(&mut self, job: JqJob, result: JqResult) {
        self.jq_running = self.jq_running.saturating_sub(1);
        let current = self.jq_input.value().trim() == job.filter;
        let Some(view) = self.body_view(job.txn, job.dir) else { return };
        if !current || view.decoded.bytes != job.bytes {
            return;
        }
        let ok = result.is_ok();
        view.set_jq(job.filter.clone(), result);
        if self.selected == Some(job.txn) {
            self.detail.parsed = true;
            self.detail.cursor = 0;
            self.detail.scroll = 0;
        }
        if ok {
            self.flash(format!("jq: {}  (| to edit, empty filter clears)", job.filter));
        } else {
            self.flash("jq failed; the error is shown in the body");
        }
    }

    /// Whether filters or body decodes are waiting to be run.
    pub fn has_queued_jobs(&self) -> bool {
        !self.jq_jobs.is_empty() || self.bodies.has_queued()
    }

    /// Run queued filters and body decodes on this thread (tests and single-frame renders).
    pub fn run_jobs_inline(&mut self) {
        for job in self.take_body_jobs() {
            let view = job.run();
            self.finish_body(job, view);
        }
        for job in self.take_jq_jobs() {
            let result = (self.jq_runner)(&job.filter, &job.bytes);
            self.finish_jq(job, result);
        }
    }

    /// Whether something on screen changes with time alone (live axis, open requests, a
    /// message that will expire, a running filter).
    pub fn is_time_dependent(&self) -> bool {
        if self.message.as_ref().is_some_and(|(_, at)| at.elapsed() < Duration::from_secs(5))
            || self.jq_running > 0
            || self.bodies.building()
        {
            return true;
        }
        if self.frozen.is_some() {
            return false;
        }
        self.graph.pinned_right.is_none() && self.store.sources().any(|s| s.ended.is_none())
            || self.store.txns().iter().rev().take(256).any(|t| t.state.is_open())
    }

    fn open_frame(&mut self, txn: TxnIdx, index: usize) {
        let t = self.view_store().txn(txn);
        let Some(frame) = t.stack.get(index) else { return };
        let Some(file) = frame.f.clone() else {
            self.flash("this frame has no source file");
            return;
        };
        let line = frame.l.unwrap_or(1).max(1) as u32;
        if self.source_roots.is_empty() {
            self.flash("set source_roots in .traffic-police/project.toml to open frames in $EDITOR");
            return;
        }
        // the package directory first, then any file of that name under the roots
        let pkg: PathBuf = frame.c.rsplit_once('.').map(|(p, _)| p.replace('.', "/")).unwrap_or_default().into();
        let direct = self.source_roots.iter().map(|r| r.join(&pkg).join(&file)).find(|p| p.is_file());
        let found = direct.or_else(|| {
            let mut hits = Vec::new();
            for root in &self.source_roots {
                find_files(root, &file, &mut hits, 0, &mut 0);
            }
            // prefer a path that ends with the package directories
            hits.sort_by_key(|p: &PathBuf| !p.parent().is_some_and(|d| d.ends_with(&pkg)));
            hits.into_iter().next()
        });
        match found {
            Some(path) => self.editor_request = Some((path, line)),
            None => self.flash(format!("{file} not found under the source roots")),
        }
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) {
        self.refresh();
        let (x, y) = (m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let double = self
                    .last_click
                    .is_some_and(|(at, lx, ly)| at.elapsed() < Duration::from_millis(400) && lx == x && ly == y);
                self.last_click = Some((Instant::now(), x, y));
                match self.hits.at(x, y) {
                    Some(Target::ListRow(r)) => {
                        self.focus = Focus::List;
                        self.select_row(r);
                        if double {
                            if let Some(Row::Group { members, .. }) = self.view_rows().rows().get(r).cloned() {
                                self.rows.toggle_group(members[0]);
                            } else {
                                self.open_detail();
                            }
                        }
                    }
                    Some(Target::ListHeader(c)) => {
                        let s = self.rows.sort;
                        self.set_sort(Sort { column: c, descending: s.column == c && !s.descending });
                    }
                    Some(Target::ViewTab(v)) => self.set_view(v),
                    Some(Target::DetailTab(t)) => {
                        self.focus = Focus::Detail;
                        self.set_tab(t);
                    }
                    Some(Target::DetailClose) => self.close_detail(),
                    Some(Target::DetailLine(i)) => {
                        self.focus = Focus::Detail;
                        self.detail.cursor = i;
                        if double {
                            self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                        }
                    }
                    Some(Target::Divider) => self.drag = Some(Drag::Divider),
                    Some(Target::Graph) => {
                        self.focus = Focus::Graph;
                        if let Some(t) = self.graph_time_at(x) {
                            self.graph.cursor = Some(t);
                            self.drag = Some(Drag::Graph { anchor: t });
                        }
                    }
                    Some(Target::ThreadBar(i)) => {
                        self.focus = Focus::List;
                        self.bar_cursor = i;
                        if let Some(b) = self.bars.get(i).copied() {
                            self.select_txn(b.txn);
                            if double {
                                self.open_detail();
                            }
                        }
                    }
                    Some(Target::RuleRow(i)) => {
                        self.focus = Focus::List;
                        self.rules_cursor = i;
                    }
                    Some(Target::List) => self.focus = Focus::List,
                    None => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.drag {
                Some(Drag::Divider) => {
                    if self.area.width > 0 {
                        let pct = (u32::from(x.saturating_sub(self.area.x)) * 100 / u32::from(self.area.width)) as u16;
                        self.split_pct = pct.clamp(25, 80);
                    }
                }
                Some(Drag::Graph { anchor }) => {
                    if let Some(t) = self.graph_time_at(x) {
                        self.graph.cursor = Some(t);
                        self.graph.anchor = Some(anchor);
                    }
                }
                None => {}
            },
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(Drag::Graph { anchor }) = self.drag.take() {
                    self.graph.anchor = None;
                    if let Some(t) = self.graph_time_at(x) {
                        self.apply_graph_selection(anchor, t);
                    }
                }
                self.drag = None;
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                if let Some(Target::Graph) = self.hits.at(x, y) {
                    self.shift_window(if m.kind == MouseEventKind::ScrollRight { 0.1 } else { -0.1 });
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                match self.hits.at(x, y) {
                    Some(Target::DetailLine(_) | Target::DetailTab(_)) => {
                        let doc_len = detail::build_doc(self).len();
                        let max = doc_len.saturating_sub(self.detail_height.max(1));
                        self.detail.scroll =
                            if down { (self.detail.scroll + 3).min(max) } else { self.detail.scroll.saturating_sub(3) };
                        self.detail.cursor = self
                            .detail
                            .cursor
                            .clamp(self.detail.scroll, self.detail.scroll + self.detail_height.saturating_sub(1));
                    }
                    // Shift+wheel moves along the time axis; the wheel alone zooms
                    Some(Target::Graph) if m.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.shift_window(if down { 0.1 } else { -0.1 })
                    }
                    Some(Target::Graph) => self.zoom(if down { -1 } else { 1 }),
                    _ => {
                        let len = self.view_rows().len();
                        let max = len.saturating_sub(self.list_height.max(1));
                        self.list_offset =
                            if down { (self.list_offset + 3).min(max) } else { self.list_offset.saturating_sub(3) };
                        self.follow_list = false;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Files named `name` under `dir` (depth- and size-limited, skipping build and VCS directories).
fn find_files(dir: &std::path::Path, name: &str, out: &mut Vec<PathBuf>, depth: usize, seen: &mut usize) {
    const MAX_DEPTH: usize = 24;
    const MAX_ENTRIES: usize = 200_000;
    if depth > MAX_DEPTH || *seen > MAX_ENTRIES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        *seen += 1;
        let path = e.path();
        let Ok(kind) = e.file_type() else { continue };
        if kind.is_dir() {
            let skip = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with('.') || matches!(n, "build" | "target" | "node_modules" | "intermediates")
            });
            if !skip {
                find_files(&path, name, out, depth + 1, seen);
            }
        } else if path.file_name().is_some_and(|n| n == name) {
            out.push(path);
        }
    }
}
