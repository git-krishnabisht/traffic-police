//! Application state and input handling. Rendering reads this; nothing here draws.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::{BackendCommand, Capabilities, ConnectionStatus};
use traffic_police_core::event::MarkerKind;
use traffic_police_core::filter::{BodySearch, Filter, ParseError};
use traffic_police_core::fmt::{NS_PER_MS, NS_PER_SEC, Ts};
use traffic_police_core::model::{BodyDir, TxnIdx};
use traffic_police_core::rows::{Column, Row, RowModel, Sort};
use traffic_police_core::store::{GraphSource, SessionStore};
use traffic_police_proto::msg::RuleSet;
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, Keymap};
use crate::bodycache::{BodyCache, BodyJob, Lookup};
use crate::bodyview::BodyView;
use crate::detail::{self, DocRow};
use crate::images::Images;
use crate::theme::Theme;
use crate::wrap;

/// What Ctrl+C and a lone `q` say: only `:q` (and `:wq`, `:x`, …) quits, as in Neovim.
pub const QUIT_HINT: &str = "type :q and press Enter to quit";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Connections,
    Threads,
    Rules,
}

impl View {
    /// `[ui] view`: connections, threads or rules.
    pub fn parse(s: &str) -> Option<View> {
        match s.trim().to_ascii_lowercase().as_str() {
            "connections" | "connection" => Some(View::Connections),
            "threads" | "thread" => Some(View::Threads),
            "rules" => Some(View::Rules),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Graph,
    List,
    /// The body explorer above the detail tabs.
    Preview,
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

    /// `[ui] tab`: overview, response, request or call-stack.
    pub fn parse(s: &str) -> Option<Tab> {
        match s.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
            "overview" => Some(Tab::Overview),
            "response" => Some(Tab::Response),
            "request" => Some(Tab::Request),
            "call-stack" | "stack" => Some(Tab::CallStack),
            _ => None,
        }
    }

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
    Columns {
        cursor: usize,
    },
    ConfirmClear,
    /// Leaving the rule form with changes: discard them? (`App::form`)
    ConfirmDiscard,
    Jq,
    /// Typing a filter in the bottom line.
    Filter,
    /// Typing a search for the detail pane in the bottom line.
    Search,
    /// A menu of choices (copy, export): `App::menu`.
    Menu,
    /// A path typed in the bottom line (save a body, export): `App::prompt`.
    Prompt,
    /// Two requests compared: `App::diff`.
    Diff,
    /// A decoded value (JWT, base64, URL encoding): `App::decoded`.
    Decoded,
    /// The command palette (`:`): `App::palette`.
    Palette,
}

/// A search match in the detail pane: a row, and display columns in it (however it wraps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub row: usize,
    pub start: usize,
    pub end: usize,
}

/// Search in the detail pane (`/` there): matches of the current tab, found again when the
/// request, the tab, the view or the body changes.
#[derive(Debug, Clone, Default)]
pub struct DetailSearch {
    pub query: String,
    pub matches: Vec<SearchMatch>,
    pub current: Option<usize>,
    /// What the matches were found in: (request, tab, parsed, rows).
    found_in: Option<(TxnIdx, Tab, bool, usize)>,
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
    /// The window the graph was last drawn with: ends on a column, and while following live
    /// a little before "now" ([`App::graph_window`]). The mouse picks times from it.
    pub drawn: Option<(Ts, Ts)>,
    /// The top of the graph's y axis: the one scale of the overlay layout, or the receiving
    /// half's in the mirror layout.
    pub scale: Option<crate::graph::Scale>,
    /// The sending half's scale in the mirror layout (its own peak, read downward).
    pub send_scale: Option<crate::graph::Scale>,
}

#[derive(Debug, Clone)]
pub struct DetailState {
    pub tab: Tab,
    pub cursor: usize,
    /// The row at the top of the pane, and (`[ui] wrap`) how many of its rows are above it.
    pub scroll: usize,
    pub scroll_part: usize,
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
            scroll_part: 0,
            hscroll: 0,
            parsed: true,
            original: false,
            expanded_runs: HashSet::new(),
        }
    }
}

/// `[ui]` settings for layout and movement (ARCHITECTURE.md §5.13); colors, borders and keys
/// live in the theme and the keymap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefs {
    /// Wrap lines wider than their box instead of cutting them off (every box of text).
    pub wrap: bool,
    /// Lines a half-page jump (Ctrl+D, Ctrl+U) moves; 0 is half the box, like Neovim's 'scroll'.
    pub scroll: u16,
    /// The list follows new requests while its cursor is on the newest.
    pub follow: bool,
    /// Rows of the traffic graph: `None` sizes it to the screen (8 to 14); 0 hides it.
    pub graph_height: Option<u16>,
    /// The body box's share of the detail pane, in percent; 0 hides the box.
    pub body_height: u16,
    /// From this width on, the detail pane sits beside the list instead of covering it.
    pub side_by_side: u16,
    /// Key hints in the footer.
    pub hints: bool,
    /// Blank rows between boxes one above the other; boxes side by side get `2 × gap + 1`
    /// columns, since a terminal cell is about twice as tall as it is wide.
    pub gap: u16,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            wrap: true,
            scroll: 0,
            follow: true,
            graph_height: None,
            body_height: 40,
            side_by_side: crate::ui::SIDE_BY_SIDE_WIDTH,
            hints: true,
            gap: 0,
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
    ExplorerTab(crate::explorer::BodyTab),
    ExplorerLine(usize),
    Divider,
    Graph,
    ThreadBar(usize),
    RuleRow(usize),
    /// A line of the rule form.
    FormRow(usize),
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

/// A `body:` search for the filter, run off the UI task.
#[derive(Debug, Clone)]
pub struct SearchJob {
    pub txn: TxnIdx,
    pub needle: usize,
    pub rev: u64,
    pub search: BodySearch,
}

/// The most frames drawn a second, unless `[ui] fps` says otherwise.
pub const DEFAULT_FPS: u16 = 60;
/// What `[ui] fps` accepts.
pub const FPS_RANGE: std::ops::RangeInclusive<u16> = 10..=240;

/// Recent frames, for the optional frame-rate readout.
#[derive(Debug, Clone, Default)]
pub struct FrameStats {
    /// Show the readout in the footer.
    pub visible: bool,
    /// Start of each frame in the last second, and how long it took to draw.
    recent: std::collections::VecDeque<(Instant, Duration)>,
}

impl FrameStats {
    pub fn record(&mut self, at: Instant, took: Duration) {
        self.recent.push_back((at, took));
        while self.recent.front().is_some_and(|(t, _)| at.duration_since(*t) > Duration::from_secs(1)) {
            self.recent.pop_front();
        }
    }

    /// Frames drawn in the last second, and the mean time to draw one.
    pub fn summary(&self) -> (usize, Duration) {
        let n = self.recent.len();
        let total: Duration = self.recent.iter().map(|(_, d)| *d).sum();
        (n, if n == 0 { Duration::ZERO } else { total / n as u32 })
    }
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
    /// Whether the last frame had room for the body box above the tabs (`[ui] body_height`).
    pub body_box_shown: bool,
    pub detail: DetailState,
    pub detail_height: usize,
    /// The detail tabs' width when last drawn, for wrapping.
    pub detail_width: usize,
    pub explorer: crate::explorer::Explorer,
    pub graph: GraphState,
    pub graph_source: GraphSource,
    pub graph_style: crate::graph::GraphStyle,
    pub graph_layout: crate::graph::GraphLayout,
    /// Seconds the graph's curves are averaged over (`[ui] graph_smoothing`).
    pub graph_smoothing: f64,
    pub frames: FrameStats,
    /// The most frames drawn a second (`[ui] fps`).
    pub fps: u16,
    pub keymap: Keymap,
    pub wall_labels: bool,
    pub columns: Vec<Column>,
    pub split_pct: u16,
    /// Layout and movement settings from `[ui]`.
    pub prefs: Prefs,
    pub overlay: Overlay,
    pub jq_input: Input,
    pub filter_input: Input,
    pub search: DetailSearch,
    pub menu: Option<crate::share::Menu>,
    pub prompt: Option<crate::share::Prompt>,
    /// Where copied text goes.
    pub clipboard: crate::share::ClipboardMode,
    /// The last text copied (also kept when the clipboard is off).
    pub copied: Option<String>,
    /// The file being replayed (`open FILE`), for the header.
    pub replay: Option<String>,
    /// The request marked for comparison (`d`).
    pub diff_mark: Option<traffic_police_core::model::TxnKey>,
    pub diff: Option<crate::diffview::DiffView>,
    pub decoded: Option<crate::values::Decoded>,
    pub palette: Option<crate::palette::Palette>,
    /// The captured stream, for saving the session (`e`).
    pub session_log: Option<Arc<traffic_police_core::session::SessionLog>>,
    pub search_input: Input,
    search_before: Option<String>,
    /// Where the text being typed stops parsing (the previous filter stays active meanwhile).
    pub filter_error: Option<ParseError>,
    /// The filter before editing started, for Esc.
    filter_before: Option<String>,
    pub message: Option<(String, Instant)>,
    /// The rules the app runs (the demo's built-in ones, or the last valid rules.toml).
    pub rules: Option<RuleSet>,
    pub rules_cursor: usize,
    /// rules.toml as last read (with its problems), and the `.traffic-police` directory.
    pub rules_file: Option<traffic_police_core::rules::RulesFile>,
    /// The rule form, while it is open in the Rules view.
    pub form: Option<crate::ruleform::RuleForm>,
    /// Where the terminal cursor goes after this frame (a field being typed into).
    pub cursor_position: Option<(u16, u16)>,
    pub rules_dir: Option<PathBuf>,
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
    /// What a live backend says about its connection, for the header.
    pub connection: Option<ConnectionStatus>,
    /// Set by input; the terminal loop suspends the UI and opens `$EDITOR`.
    pub editor_request: Option<(PathBuf, u32)>,
    pub area: Rect,
    frozen: Option<Box<Frozen>>,
    live_anchor: Option<(Ts, Instant)>,
    bodies: BodyCache,
    last_click: Option<(Instant, u16, u16)>,
    drag: Option<Drag>,
    follow_list: bool,
    /// The user went to the bottom (G) with a request open: the list, and so the open request,
    /// follows new ones until the cursor moves away.
    follow_open: bool,
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
            body_box_shown: true,
            detail: DetailState::default(),
            detail_height: 10,
            detail_width: 0,
            explorer: crate::explorer::Explorer::default(),
            graph: GraphState { span: DEFAULT_SPAN, ..Default::default() },
            graph_source: GraphSource::AppTotal,
            graph_style: crate::graph::GraphStyle::default(),
            graph_layout: crate::graph::GraphLayout::default(),
            graph_smoothing: crate::graph::SMOOTHING_SECS,
            keymap: Keymap::default(),
            frames: FrameStats { visible: std::env::var_os("TRAFFIC_POLICE_FPS").is_some(), ..Default::default() },
            fps: DEFAULT_FPS,
            wall_labels: false,
            columns: Column::DEFAULT.to_vec(),
            split_pct: 55,
            prefs: Prefs::default(),
            overlay: Overlay::None,
            jq_input: Input::default(),
            filter_input: Input::default(),
            search: DetailSearch::default(),
            menu: None,
            prompt: None,
            clipboard: crate::share::ClipboardMode::default(),
            copied: None,
            session_log: None,
            replay: None,
            diff_mark: None,
            diff: None,
            decoded: None,
            palette: None,
            search_input: Input::default(),
            search_before: None,
            filter_error: None,
            filter_before: None,
            message: None,
            rules: None,
            rules_cursor: 0,
            form: None,
            cursor_position: None,
            rules_file: None,
            rules_dir: None,
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
            connection: None,
            editor_request: None,
            area: Rect::default(),
            frozen: None,
            live_anchor: None,
            bodies: BodyCache::new(),
            last_click: None,
            drag: None,
            follow_list: true,
            follow_open: false,
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

    /// The wall clock at [`App::now`]: the device's time while live, the end of a recording in a
    /// replay (the system clock when the session has no clock).
    pub fn wall_now_ms(&self) -> i64 {
        self.view_store().wall_ms(self.now()).unwrap_or_else(|| {
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
        })
    }

    /// Current device time for live displays.
    pub fn now(&self) -> Ts {
        if let Some(f) = &self.frozen {
            return f.now;
        }
        if let Some(t) = self.now_override {
            return t;
        }
        // Between batches, extrapolate from the last event or clock reading so live bars grow
        // smoothly. A connected app reports its clock at least every 5 s (pongs), so allow a
        // little more than that; once no source is connected, time stops with the data.
        let connected = self.store.sources().any(|s| s.ended.is_none());
        let limit = if connected { Duration::from_secs(7) } else { Duration::ZERO };
        match self.live_anchor {
            Some((ts, at)) => ts + at.elapsed().min(limit).as_nanos() as u64,
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

    /// The graph's window: [`window`](Self::window), but while it follows "now" with an app
    /// connected, [`lag_ns`](crate::graph::lag_ns) before it, where the app's counters have
    /// arrived.
    pub fn graph_window(&self) -> (Ts, Ts) {
        let (left, right) = self.window();
        if self.graph.pinned_right.is_some() || !self.store.sources().any(|s| s.ended.is_none()) {
            return (left, right);
        }
        let lag = crate::graph::lag_ns(self.graph_smoothing);
        (left.saturating_sub(lag), right.saturating_sub(lag))
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
        let following = self.prefs.follow && self.follow_list && (!self.detail_open || self.follow_open);
        if following && !frozen {
            self.list_cursor = len - 1;
        } else if let Some(sel) = self.selected
            && let Some(pos) = rows.position(sel)
        {
            self.list_cursor = pos;
        }
        self.list_cursor = self.list_cursor.min(len - 1);
        if self.selected.is_none() || following {
            let txn = rows.rows()[self.list_cursor].txn();
            if self.selected != Some(txn) && self.detail_open {
                // the open request follows the newest: start at its top
                self.reset_detail_position();
            }
            self.selected = Some(txn);
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
        let headers = store.decoding_headers(t, headers);
        let key = meta.captured * 16 + meta.state as u64;
        let epoch = self.bodies.epoch();
        let lookup = self.bodies.lookup(txn, dir, key, meta.captured as usize, || BodyJob {
            epoch,
            txn,
            dir,
            key,
            raw: store.body_bytes(meta),
            headers: headers.as_deref().cloned(),
        });
        if let Lookup::BuildHere = lookup {
            let view = BodyView::build(store.body_bytes(meta), headers.as_deref());
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
        if !self.follow_list {
            self.follow_open = false;
        }
        let txn = self.view_rows().rows()[row].txn();
        if self.selected != Some(txn) {
            self.selected = Some(txn);
            self.reset_detail_position();
        }
        self.clamp_list_offset();
    }

    /// Another request in the detail pane starts at its top.
    fn reset_detail_position(&mut self) {
        self.detail.cursor = 0;
        self.set_detail_top(wrap::Top::default());
        self.detail.hscroll = 0;
        self.detail.expanded_runs.clear();
        self.detail.original = false;
    }

    /// Lines a half-page jump moves in a box `visible` rows tall (`[ui] scroll`, else half).
    pub(crate) fn half_page(&self, visible: usize) -> usize {
        if self.prefs.scroll > 0 { usize::from(self.prefs.scroll) } else { (visible / 2).max(1) }
    }

    pub fn select_txn(&mut self, txn: TxnIdx) {
        if let Some(pos) = self.view_rows().position(txn) {
            self.select_row(pos);
        } else {
            self.selected = Some(txn);
            self.detail.cursor = 0;
            self.set_detail_top(wrap::Top::default());
        }
    }

    fn open_detail(&mut self) {
        if self.selected.is_some() {
            self.detail_open = true;
            self.follow_open = false;
            self.focus = Focus::Detail;
        }
    }

    pub(crate) fn close_detail(&mut self) {
        self.detail_open = false;
        if matches!(self.focus, Focus::Detail | Focus::Preview) {
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
        // not `clamp`, which panics if the clock ever reads earlier than the session's start
        let c = c.max(origin).min(live_right);
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
        let (left, right) = self.graph.drawn.unwrap_or_else(|| self.graph_window());
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
        self.diff_mark = None;
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
            Event::Paste(s) if self.overlay == Overlay::Search => {
                for c in s.chars().filter(|c| !c.is_control()) {
                    self.search_input.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
                }
                let q = self.search_input.value().to_string();
                self.set_search(&q, true);
            }
            Event::Paste(s) if self.overlay == Overlay::Prompt => {
                if let Some(p) = &mut self.prompt {
                    for c in s.chars().filter(|c| !c.is_control()) {
                        p.input.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
                    }
                }
            }
            Event::Paste(s) if self.overlay == Overlay::Palette => {
                if let Some(p) = &mut self.palette {
                    p.insert(&s);
                }
            }
            Event::Paste(s) if self.overlay == Overlay::Filter => {
                for c in s.chars().filter(|c| !c.is_control()) {
                    self.filter_input.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
                }
                let text = self.filter_input.value().to_string();
                self.apply_filter(&text);
            }
            _ => {}
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) {
        // keys can arrive faster than frames; act on the rows as they are now
        self.refresh();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // as in Neovim, Ctrl+C does not quit: it says what does
        if ctrl && k.code == KeyCode::Char('c') {
            self.flash(QUIT_HINT);
            return;
        }
        match self.overlay {
            Overlay::Help => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::Diff => {
                self.diff_key(k);
                return;
            }
            Overlay::Decoded => {
                self.decoded_key(k);
                return;
            }
            Overlay::Palette => {
                self.palette_key(k);
                return;
            }
            Overlay::ConfirmClear => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.clear_session();
                }
                self.overlay = Overlay::None;
                return;
            }
            Overlay::ConfirmDiscard => {
                self.overlay = Overlay::None;
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.form_discard();
                }
                return;
            }
            Overlay::Columns { cursor } => {
                let n = Column::OPTIONAL.len();
                let action = self.keymap.action(&k);
                match action {
                    Some(Action::Up) => self.overlay = Overlay::Columns { cursor: (cursor + n - 1) % n },
                    Some(Action::Down) => self.overlay = Overlay::Columns { cursor: (cursor + 1) % n },
                    Some(Action::Top) => self.overlay = Overlay::Columns { cursor: 0 },
                    Some(Action::Bottom) => self.overlay = Overlay::Columns { cursor: n - 1 },
                    _ if action == Some(Action::Activate) || k.code == KeyCode::Char(' ') => {
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
            Overlay::Filter => {
                match k.code {
                    KeyCode::Esc => {
                        let before = self.filter_before.take().unwrap_or_default();
                        self.filter_input = Input::new(before.clone());
                        self.apply_filter(&before);
                        self.filter_error = None;
                        self.overlay = Overlay::None;
                    }
                    KeyCode::Enter => {
                        self.filter_before = None;
                        self.filter_error = None;
                        self.overlay = Overlay::None;
                        let text = self.filter_input.value().trim().to_string();
                        if text.is_empty() {
                            self.flash("filter cleared");
                        }
                    }
                    _ => {
                        self.filter_input.handle_event(&Event::Key(k));
                        let text = self.filter_input.value().to_string();
                        self.apply_filter(&text);
                    }
                }
                return;
            }
            Overlay::Menu => {
                let Some(menu) = &mut self.menu else {
                    self.overlay = Overlay::None;
                    return;
                };
                let n = menu.items.len();
                // an entry's own letter first: the menu shows it, so it wins over the keys that
                // move and close (up to 0.3.1 `q`, `j` and `k` did those instead of running the
                // copy menu's "request body" and "one header" and the value menu's "decode JWT")
                let plain = !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                if let KeyCode::Char(c) = k.code
                    && plain
                    && let Some(item) = menu.items.iter().find(|i| i.key == c)
                {
                    let action = item.action.clone();
                    self.run_menu(action);
                    return;
                }
                match self.keymap.action(&k) {
                    Some(Action::Back | Action::Quit) => {
                        self.overlay = Overlay::None;
                        self.menu = None;
                    }
                    Some(Action::Up) if n > 0 => menu.cursor = (menu.cursor + n - 1) % n,
                    Some(Action::Down) if n > 0 => menu.cursor = (menu.cursor + 1) % n,
                    Some(Action::Top | Action::PageUp | Action::HalfPageUp) => menu.cursor = 0,
                    Some(Action::Bottom | Action::PageDown | Action::HalfPageDown) => menu.cursor = n.saturating_sub(1),
                    Some(Action::Activate) => {
                        if let Some(item) = menu.items.get(menu.cursor) {
                            let action = item.action.clone();
                            self.run_menu(action);
                        }
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Prompt => {
                match k.code {
                    KeyCode::Esc => {
                        self.overlay = Overlay::None;
                        self.prompt = None;
                    }
                    KeyCode::Enter => self.finish_prompt(),
                    _ => {
                        if let Some(p) = &mut self.prompt {
                            p.input.handle_event(&Event::Key(k));
                        }
                    }
                }
                return;
            }
            Overlay::Search => {
                match k.code {
                    KeyCode::Esc => {
                        let before = self.search_before.take().unwrap_or_default();
                        self.search_input = Input::new(before.clone());
                        self.set_search(&before, false);
                        self.overlay = Overlay::None;
                    }
                    KeyCode::Enter => {
                        self.search_before = None;
                        self.overlay = Overlay::None;
                        self.report_search();
                    }
                    _ => {
                        self.search_input.handle_event(&Event::Key(k));
                        let q = self.search_input.value().to_string();
                        self.set_search(&q, true);
                    }
                }
                return;
            }
            Overlay::None => {}
        }

        // the rule form takes the keys of the Rules view while it is open
        if self.form.is_some() && self.view == View::Rules && self.focus == Focus::List && self.form_key(k) {
            return;
        }
        if let Some(action) = self.keymap.action(&k) {
            self.run(action);
        } else if k.code == KeyCode::Char('q') && k.modifiers.is_empty() {
            // `q` quit until 2026-10-08: say what does now
            self.flash(QUIT_HINT);
        }
    }

    /// Lists what a filter object keeps (the rule form's match), showing its label in the bar.
    pub fn set_filter_object(&mut self, filter: Filter) {
        self.filter_input = Input::new(filter.source.clone());
        self.filter_error = None;
        let f = Some(Arc::new(filter));
        self.rows.set_filter(f.clone());
        if let Some(fr) = &mut self.frozen {
            fr.rows.set_filter(f);
        }
        self.follow_list = self.is_live() && !self.detail_open;
    }

    /// Runs an action, from a key or the command palette.
    pub fn run(&mut self, a: Action) {
        let not_detail = !matches!(self.focus, Focus::Detail | Focus::Preview);
        match a {
            Action::Quit => self.should_quit = true,
            Action::Help => self.overlay = Overlay::Help,
            Action::ViewConnections => self.set_view(View::Connections),
            Action::ViewThreads => self.set_view(View::Threads),
            Action::ViewRules => self.set_view(View::Rules),
            Action::FocusNext => self.cycle_focus(1),
            Action::FocusPrev => self.cycle_focus(-1),
            Action::Pause if self.view == View::Rules && self.focus == Focus::List => self.toggle_rule(),
            Action::Pause => self.toggle_recording(),
            Action::Freeze => self.toggle_freeze(),
            Action::Live => {
                self.graph.pinned_right = None;
                self.graph.cursor = None;
                self.follow_list = true;
                if !self.detail_open {
                    self.list_cursor = self.view_rows().len().saturating_sub(1);
                }
                self.flash("following live");
            }
            Action::ZoomIn => self.zoom(1),
            Action::ZoomOut => self.zoom(-1),
            Action::ZoomReset => {
                self.graph.span = DEFAULT_SPAN;
                self.flash("window reset");
            }
            Action::GraphSource => {
                self.graph_source = self.graph_source.toggled();
                self.flash(format!("graph: {}", self.graph_source.label()));
            }
            Action::TimeLabels => {
                self.wall_labels = !self.wall_labels;
                self.flash(if self.wall_labels { "time axis: wall clock" } else { "time axis: since session start" });
            }
            Action::GraphStyle => {
                self.graph_style = self.graph_style.next();
                self.flash(format!("graph style: {}", self.graph_style.name()));
            }
            Action::GraphLayout => {
                self.graph_layout = self.graph_layout.next();
                self.flash(format!("graph layout: {}", self.graph_layout.name()));
            }
            Action::FrameRate => {
                self.frames.visible = !self.frames.visible;
                self.flash(if self.frames.visible { "showing the frame rate" } else { "frame rate hidden" });
            }
            Action::Collapse if not_detail => {
                let on = !self.rows.collapse;
                self.rows.set_collapse(on);
                if let Some(f) = &mut self.frozen {
                    f.rows.set_collapse(on);
                }
                self.flash(if on { "collapsing repeated calls" } else { "showing every call" });
            }
            Action::Columns => self.overlay = Overlay::Columns { cursor: 0 },
            Action::Sort if not_detail => self.cycle_sort(),
            Action::SortReverse if not_detail => {
                let mut s = self.rows.sort;
                s.descending = !s.descending;
                self.set_sort(s);
            }
            Action::Clear => self.overlay = Overlay::ConfirmClear,
            Action::SelectRange if not_detail => {
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
            }
            Action::Back => self.back(),
            Action::Find if self.focus != Focus::Detail => {
                self.filter_before = Some(self.filter_input.value().to_string());
                self.filter_error = None;
                self.overlay = Overlay::Filter;
                if self.focus == Focus::Graph {
                    self.focus = Focus::List;
                }
            }
            Action::Pin => self.toggle_pin(),
            Action::NewRule => self.new_rule_from_selected(),
            Action::Find if self.detail_open => {
                self.search_before = Some(self.search.query.clone());
                self.search_input = Input::new(self.search.query.clone());
                self.overlay = Overlay::Search;
            }
            Action::FindNext | Action::FindPrev if self.focus == Focus::Detail => {
                self.step_search(if a == Action::FindNext { 1 } else { -1 });
            }
            Action::Find | Action::FindNext | Action::FindPrev => {
                self.flash("open a request (Enter) to search it; / in the list filters");
            }
            Action::Copy => self.open_copy_menu(),
            Action::Save => self.open_save_prompt(),
            Action::Export => self.open_export_menu(),
            Action::Diff => self.diff_action(),
            Action::Palette => self.open_palette(),
            Action::BodyTab if self.detail_open => self.toggle_body_tab(),
            Action::BodyTab => self.flash("open a request (Enter) to see its bodies"),
            _ => match self.focus {
                Focus::Graph => self.graph_action(a),
                Focus::List => match self.view {
                    View::Connections => self.list_action(a),
                    View::Threads => self.threads_action(a),
                    View::Rules => self.rules_action(a),
                },
                Focus::Preview => self.explorer_action(a),
                Focus::Detail => self.detail_action(a),
            },
        }
    }

    /// Applies filter text as it is typed: a filter that parses replaces the active one; one
    /// that does not is marked where it breaks and the active filter stays.
    pub fn apply_filter(&mut self, text: &str) {
        match Filter::parse(text) {
            Ok(f) => {
                let f = f.map(Arc::new);
                self.rows.set_filter(f.clone());
                if let Some(fr) = &mut self.frozen {
                    fr.rows.set_filter(f);
                }
                self.filter_error = None;
                self.follow_list = self.is_live() && !self.detail_open;
            }
            Err(e) => self.filter_error = Some(e),
        }
    }

    /// Sets the detail search; `jump` moves to the first match from the cursor on.
    fn set_search(&mut self, q: &str, jump: bool) {
        self.search.query = q.to_string();
        self.search.found_in = None;
        let doc = detail::build_doc(self);
        self.refresh_search(&doc);
        if jump && !self.search.matches.is_empty() {
            let cur = self.detail.cursor;
            let k = self.search.matches.iter().position(|m| m.row >= cur).unwrap_or(0);
            self.go_to_match(k);
        }
    }

    /// Finds the matches again if what the pane shows changed since they were found.
    pub fn refresh_search(&mut self, doc: &detail::Doc) {
        let Some(txn) = self.selected else { return };
        if self.search.query.is_empty() {
            self.search.matches.clear();
            self.search.current = None;
            return;
        }
        let key = (txn, self.detail.tab, self.detail.parsed, doc.len());
        if self.search.found_in == Some(key) {
            return;
        }
        self.search.found_in = Some(key);
        self.search.matches.clear();
        let Ok(re) = regex::RegexBuilder::new(&regex::escape(&self.search.query)).case_insensitive(true).build() else {
            return;
        };
        // each row whole, so a match where a row wraps is found too
        for i in 0..doc.len() {
            let Some(text) = detail::row_text(self, doc, i, txn) else { continue };
            for m in re.find_iter(&text) {
                let start = text[..m.start()].width();
                let end = start + m.as_str().width();
                self.search.matches.push(SearchMatch { row: i, start, end });
            }
        }
        if self.search.current.is_some_and(|c| c >= self.search.matches.len()) {
            self.search.current = None;
        }
    }

    fn go_to_match(&mut self, k: usize) {
        let Some(m) = self.search.matches.get(k).copied() else { return };
        self.search.current = Some(k);
        self.focus = Focus::Detail;
        self.detail.cursor = m.row;
        let Some(txn) = self.selected else { return };
        let doc = detail::build_doc(self);
        if self.prefs.wrap {
            // the row of a wrapped line the match is on
            let part = detail::row_part_at(self, &doc, m.row, txn, self.detail_width.max(1), m.start);
            self.show_detail_row(&doc, txn, part);
            return;
        }
        self.show_detail_row(&doc, txn, 0);
        // keep a match on a long line in view
        let hs = usize::from(self.detail.hscroll);
        let width = self.area.width.saturating_sub(4) as usize / 2;
        if m.start < hs || m.end > hs + width.max(20) {
            self.detail.hscroll = m.start.saturating_sub(10).min(u16::MAX as usize) as u16;
        }
    }

    fn step_search(&mut self, dir: i32) {
        if self.search.query.is_empty() {
            self.flash("/ searches the detail pane");
            return;
        }
        let n = self.search.matches.len();
        if n == 0 {
            self.flash(format!("no matches for {:?}", self.search.query));
            return;
        }
        let cur = self.detail.cursor;
        let k = match (self.search.current, dir) {
            (Some(c), 1) if self.search.matches[c].row == cur => (c + 1) % n,
            (Some(c), _) if self.search.matches[c].row == cur => (c + n - 1) % n,
            (_, 1) => self.search.matches.iter().position(|m| m.row > cur).unwrap_or(0),
            _ => self.search.matches.iter().rposition(|m| m.row < cur).unwrap_or(n - 1),
        };
        self.go_to_match(k);
        self.report_search();
    }

    fn report_search(&mut self) {
        let n = self.search.matches.len();
        if self.search.query.is_empty() {
            return;
        }
        match self.search.current {
            _ if n == 0 => self.flash(format!("no matches for {:?}", self.search.query)),
            Some(c) => self.flash(format!("match {} of {n} · n next · N previous", c + 1)),
            None => self.flash(format!("{n} matches · n next · N previous")),
        }
    }

    fn toggle_pin(&mut self) {
        let Some(txn) = self.selected else {
            self.flash("select a request to pin it");
            return;
        };
        let on = !self.view_store().txn(txn).pinned;
        self.store.set_pinned(txn, on);
        if let Some(f) = &mut self.frozen {
            f.store.set_pinned(txn, on);
        }
        self.flash(if on { "pinned · is:pinned filters pinned requests" } else { "unpinned" });
    }

    /// `body:` searches the filter is waiting for, ready to run off the UI task.
    pub fn take_search_jobs(&mut self) -> Vec<SearchJob> {
        let mut jobs = Vec::new();
        let mut collect = |rows: &mut RowModel, store: &SessionStore| {
            let Some(filter) = rows.filter.clone() else {
                rows.take_body_wanted();
                return;
            };
            for (txn, needle) in rows.take_body_wanted() {
                let t = store.txn(txn);
                let needle_text = filter.needles().get(needle).cloned().unwrap_or_default();
                jobs.push(SearchJob { txn, needle, rev: t.rev, search: BodySearch::new(store, t, needle_text) });
            }
        };
        collect(&mut self.rows, &self.store);
        if let Some(f) = &mut self.frozen {
            collect(&mut f.rows, &f.store);
        }
        jobs
    }

    pub fn finish_search(&mut self, job: &SearchJob, hit: bool) {
        self.rows.set_body_hit(job.txn, job.needle, job.rev, hit);
        if let Some(f) = &mut self.frozen {
            f.rows.set_body_hit(job.txn, job.needle, job.rev, hit);
        }
    }

    /// Esc: one step back (a range being selected, the detail pane, a range, graph focus).
    fn back(&mut self) {
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
    }

    pub(crate) fn set_view(&mut self, v: View) {
        self.view = v;
        if self.focus == Focus::Graph {
            self.focus = Focus::List;
        }
    }

    fn cycle_focus(&mut self, dir: i32) {
        let mut order: Vec<Focus> = if self.detail_open {
            vec![Focus::Graph, Focus::List, Focus::Preview, Focus::Detail]
        } else {
            vec![Focus::Graph, Focus::List]
        };
        // hidden boxes take no focus
        order.retain(|f| match f {
            Focus::Graph => self.prefs.graph_height != Some(0),
            Focus::Preview => self.prefs.body_height > 0 && self.body_box_shown,
            _ => true,
        });
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

    fn list_action(&mut self, a: Action) {
        let len = self.view_rows().len();
        if len == 0 {
            return;
        }
        let cur = self.list_cursor;
        match a {
            Action::Up => self.select_row(cur.saturating_sub(1)),
            Action::Down => self.select_row(cur + 1),
            Action::PageUp => self.select_row(cur.saturating_sub(self.page())),
            Action::PageDown => self.select_row(cur + self.page()),
            Action::HalfPageUp | Action::HalfPageDown => {
                // like Neovim: the view and the cursor move together
                let n = self.half_page(self.list_height);
                let down = a == Action::HalfPageDown;
                let max_offset = len.saturating_sub(self.list_height.max(1));
                self.list_offset =
                    if down { (self.list_offset + n).min(max_offset) } else { self.list_offset.saturating_sub(n) };
                self.select_row(if down { cur + n } else { cur.saturating_sub(n) });
            }
            Action::Top => self.select_row(0),
            Action::Bottom => {
                self.select_row(len - 1);
                // at the bottom, the list follows new requests again, also with a request open
                self.follow_open = self.follow_list && self.detail_open;
            }
            Action::Activate | Action::Right => {
                let row = self.view_rows().rows()[cur].clone();
                match row {
                    Row::Group { members, expanded } if a != Action::Activate || !expanded => {
                        self.rows.toggle_group(members[0]);
                        if let Some(f) = &mut self.frozen {
                            f.rows.toggle_group(members[0]);
                        }
                    }
                    _ if a == Action::Activate => self.open_detail(),
                    _ => {}
                }
            }
            Action::Left => {
                if let Row::Group { members, expanded: true } = self.view_rows().rows()[cur].clone() {
                    self.rows.toggle_group(members[0]);
                }
            }
            _ => {}
        }
    }

    fn threads_action(&mut self, a: Action) {
        if self.bars.is_empty() {
            return;
        }
        let n = self.bars.len();
        let cur = self.bar_cursor.min(n - 1);
        let next = match a {
            Action::Down => {
                let lane = self.bars[cur].lane;
                self.bars.iter().position(|b| b.lane > lane).unwrap_or(cur)
            }
            Action::Up => {
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
            Action::Right => (cur + 1).min(n - 1),
            Action::Left => cur.saturating_sub(1),
            Action::Top => 0,
            Action::Bottom => n - 1,
            Action::Activate => {
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

    fn rules_action(&mut self, a: Action) {
        match a {
            Action::Activate => return self.edit_rule_in_form(),
            Action::EditFile => return self.edit_rules(),
            Action::NewRule | Action::AddItem => return self.new_rule_in_form(),
            Action::MoveUp => return self.move_rule(true),
            Action::MoveDown => return self.move_rule(false),
            _ => {}
        }
        let n = match &self.rules_file {
            Some(f) => f.entries.len(),
            None => self.rules.as_ref().map_or(0, |r| r.rules.len()),
        };
        if n == 0 {
            return;
        }
        match a {
            Action::Up => self.rules_cursor = self.rules_cursor.saturating_sub(1),
            Action::Down => self.rules_cursor = (self.rules_cursor + 1).min(n - 1),
            Action::HalfPageUp => {
                self.rules_cursor = self.rules_cursor.saturating_sub(self.half_page(self.list_height))
            }
            Action::HalfPageDown => {
                self.rules_cursor = (self.rules_cursor + self.half_page(self.list_height)).min(n - 1)
            }
            Action::Top => self.rules_cursor = 0,
            Action::Bottom => self.rules_cursor = n - 1,
            _ => {}
        }
    }

    fn graph_action(&mut self, a: Action) {
        match a {
            Action::Left => self.pan(-0.05),
            Action::Right => self.pan(0.05),
            Action::PageUp => self.pan(-0.5),
            Action::PageDown => self.pan(0.5),
            Action::HalfPageUp => self.pan(-0.25),
            Action::HalfPageDown => self.pan(0.25),
            Action::Activate => {
                if let (Some(a), Some(c)) = (self.graph.anchor.take(), self.graph.cursor) {
                    self.apply_graph_selection(a, c);
                }
            }
            Action::Down => self.focus = Focus::List,
            _ => {}
        }
    }

    fn detail_action(&mut self, a: Action) {
        let Some(txn) = self.selected else { return };
        match a {
            Action::Left => return self.set_tab(Tab::ALL[(self.detail.tab.index() + 3) % 4]),
            Action::Right => return self.set_tab(Tab::ALL[(self.detail.tab.index() + 1) % 4]),
            Action::Parsed => {
                self.detail.parsed = !self.detail.parsed;
                self.flash(if self.detail.parsed { "parsed view" } else { "source view" });
                return;
            }
            Action::Original => {
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
            Action::Jq => {
                if matches!(self.detail.tab, Tab::Response | Tab::Request) {
                    self.overlay = Overlay::Jq;
                } else {
                    self.flash("jq filters work on the Response and Request tabs");
                }
                return;
            }
            Action::ScrollLeft | Action::ScrollRight if self.prefs.wrap => {
                self.flash("long lines wrap here; with [ui] wrap = false they scroll sideways instead");
                return;
            }
            Action::ScrollLeft => {
                self.detail.hscroll = self.detail.hscroll.saturating_sub(20);
                return;
            }
            Action::ScrollRight => {
                self.detail.hscroll = self.detail.hscroll.saturating_add(20);
                return;
            }
            _ => {}
        }
        let doc = detail::build_doc(self);
        let len = doc.len();
        let cur = self.detail.cursor.min(len.saturating_sub(1));
        let page = self.detail_height.max(2) - 1;
        match a {
            Action::Up => self.detail.cursor = cur.saturating_sub(1),
            Action::Down => self.detail.cursor = (cur + 1).min(len.saturating_sub(1)),
            // the view moves by rows (a page, or half the box), and the cursor with it
            Action::PageUp | Action::PageDown | Action::HalfPageUp | Action::HalfPageDown => {
                let n = if matches!(a, Action::PageUp | Action::PageDown) {
                    page
                } else {
                    self.half_page(self.detail_height)
                };
                let down = matches!(a, Action::PageDown | Action::HalfPageDown);
                let top = self.detail_top();
                let (top, cursor) = self.with_detail_lines(&doc, txn, |l| l.half_page(top, cur, n, down));
                self.set_detail_top(top);
                self.detail.cursor = cursor;
                return;
            }
            Action::Top => self.detail.cursor = 0,
            Action::Bottom => self.detail.cursor = len.saturating_sub(1),
            Action::FoldAll | Action::UnfoldAll => {
                if let Some(dir) = doc.body_dir {
                    let fold = a == Action::FoldAll;
                    if let Some(v) = self.body_view(txn, dir) {
                        if fold { v.fold_all() } else { v.unfold_all() }
                    }
                }
            }
            Action::Activate => match doc.row(cur) {
                Some(DocRow::Body(i)) => {
                    let folded = doc.body_dir.is_some_and(|dir| {
                        self.detail.parsed
                            && self.body_view(txn, dir).is_some_and(|v| v.jq.is_none() && v.toggle_fold(i))
                    });
                    // a value (not a container): the value menu
                    if !folded {
                        self.open_value_menu(txn);
                    }
                }
                Some(DocRow::Line(_)) => {
                    if let Some(&(_, i)) = doc.messages.iter().find(|(r, _)| *r == cur) {
                        self.open_ws_message(txn, i);
                    } else if let Some((_, token)) = doc.tokens.iter().find(|(r, _)| *r == cur) {
                        let token = token.clone();
                        self.run_value_action(&crate::share::MenuAction::DecodeJwt(token));
                    } else {
                        self.open_value_menu(txn);
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
        // what changed the rows (a fold) is shown as drawn; a move brings the cursor into view
        if matches!(a, Action::Up | Action::Down | Action::Top | Action::Bottom) {
            self.show_detail_row(&doc, txn, 0);
        }
    }

    /// The detail pane's top: a row, and how many of its rows are above the pane.
    pub(crate) fn detail_top(&self) -> wrap::Top {
        wrap::Top { line: self.detail.scroll, part: self.detail.scroll_part }
    }

    pub(crate) fn set_detail_top(&mut self, top: wrap::Top) {
        self.detail.scroll = top.line;
        self.detail.scroll_part = top.part;
    }

    /// Runs `f` over the detail pane's rows as the view sees them: one screen row each, or
    /// (`[ui] wrap`) the rows each takes at the pane's width.
    pub(crate) fn with_detail_lines<R>(
        &mut self,
        doc: &detail::Doc,
        txn: TxnIdx,
        f: impl FnOnce(&mut wrap::Lines) -> R,
    ) -> R {
        let (width, view) = (self.detail_width.max(1), self.detail_height.max(1));
        let mut rows = |i: usize| detail::row_height(self, doc, i, txn, width);
        let mut lines = wrap::Lines { len: doc.len(), view, rows: &mut rows };
        f(&mut lines)
    }

    /// Brings the cursor's row into view (all of it when it fits; else its row `part`).
    fn show_detail_row(&mut self, doc: &detail::Doc, txn: TxnIdx, part: usize) {
        let (top, cursor) = (self.detail_top(), self.detail.cursor);
        let top = self.with_detail_lines(doc, txn, |l| l.show(top, cursor, part));
        self.set_detail_top(top);
    }

    fn set_tab(&mut self, t: Tab) {
        if self.detail.tab != t {
            self.detail.tab = t;
            self.detail.cursor = 0;
            self.set_detail_top(wrap::Top::default());
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
            self.set_detail_top(wrap::Top::default());
        }
        if ok {
            self.flash(format!("jq: {}  (| to edit, empty filter clears)", job.filter));
        } else {
            self.flash("jq failed; the error is shown in the body");
        }
    }

    /// Whether filters or body decodes are waiting to be run.
    pub fn has_queued_jobs(&self) -> bool {
        !self.jq_jobs.is_empty()
            || self.bodies.has_queued()
            || self.rows.has_body_wanted()
            || self.frozen.as_ref().is_some_and(|f| f.rows.has_body_wanted())
    }

    /// Run queued filters and body decodes on this thread (tests and single-frame renders).
    pub fn run_jobs_inline(&mut self) {
        for job in self.take_search_jobs() {
            let hit = job.search.run();
            self.finish_search(&job, hit);
        }
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
            || self.graph.scale.is_some_and(|s| s.easing())
            || self.graph.send_scale.is_some_and(|s| s.easing())
        {
            return true;
        }
        if self.frozen.is_some() || self.now_override.is_some() {
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
        // boxes over the screen take the mouse
        match self.overlay {
            Overlay::Diff => {
                match m.kind {
                    MouseEventKind::ScrollDown => self.diff_wheel(3),
                    MouseEventKind::ScrollUp => self.diff_wheel(-3),
                    _ => {}
                }
                return;
            }
            Overlay::Decoded => {
                match m.kind {
                    MouseEventKind::ScrollDown => self.decoded_wheel(3),
                    MouseEventKind::ScrollUp => self.decoded_wheel(-3),
                    MouseEventKind::Down(_) => self.overlay = Overlay::None,
                    _ => {}
                }
                return;
            }
            Overlay::Help if matches!(m.kind, MouseEventKind::Down(_)) => {
                self.overlay = Overlay::None;
                return;
            }
            Overlay::Help | Overlay::Menu | Overlay::Columns { .. } | Overlay::ConfirmClear | Overlay::Palette => {
                return;
            }
            _ => {}
        }
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
                    Some(Target::ExplorerTab(tab)) => {
                        self.focus = Focus::Preview;
                        self.set_body_tab(tab);
                    }
                    Some(Target::ExplorerLine(i)) => self.explorer_click(i, double),
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
                        if double {
                            self.edit_rule_in_form();
                        }
                    }
                    Some(Target::FormRow(i)) => {
                        self.focus = Focus::List;
                        if let Some(form) = &mut self.form {
                            // a click on the line the cursor is on changes it, as Enter does
                            let again = form.cursor == i && form.editing.is_none();
                            form.cursor = i;
                            if again || double {
                                self.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                            }
                        }
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
                    Some(Target::ExplorerLine(_)) => self.explorer_scroll(down),
                    Some(Target::DetailLine(_) | Target::DetailTab(_)) => {
                        // the view moves three rows; the cursor stays on it
                        if let Some(txn) = self.selected {
                            let doc = detail::build_doc(self);
                            let (top, cursor) = (self.detail_top(), self.detail.cursor);
                            let (top, cursor) = self.with_detail_lines(&doc, txn, |l| {
                                let (top, _) = l.scroll(top, if down { 3 } else { -3 });
                                (top, l.keep_cursor(top, cursor))
                            });
                            self.set_detail_top(top);
                            self.detail.cursor = cursor;
                        }
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
