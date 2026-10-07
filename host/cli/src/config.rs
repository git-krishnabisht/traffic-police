//! The user's settings (ARCHITECTURE.md §5.13): `config.toml` in `$XDG_CONFIG_HOME/traffic-police`
//! (default `~/.config/traffic-police`) on Linux and macOS, `%APPDATA%\traffic-police` on
//! Windows, or the file `TRAFFIC_POLICE_CONFIG` names. Problems are reported with their line, and
//! the defaults apply in their place.
//!
//! ```toml
//! [ui]
//! theme = "dark"              # auto, dark, light (--theme wins)
//! borders = "rounded"         # rounded, plain, double, thick
//! graph_style = "smooth"      # smooth, curves, heavy, lines, braille
//! graph_layout = "mirror"     # mirror (receiving above the zero line, sending below, each at its own scale) or overlay (both above, one scale)
//! graph_smoothing = 1.0       # seconds the curves are averaged over (0.5 to 5); longer is calmer, and the live edge trails a little more
//! graph = "app"               # what the graph starts with: app (all app traffic) or requests (T switches)
//! graph_height = 12           # rows of the graph (0 hides it); by default a quarter of the screen
//! time = "wall"               # relative (since the session started) or wall (clock time)
//! columns = ["method", "host"] # optional columns shown beside the default ones
//! sort = "status desc"        # the list's starting order: a column, `desc` for the reverse
//! collapse = false            # start with repeated calls collapsed
//! view = "connections"        # the view at start: connections, threads, rules
//! divider = 55                # the list's share of the width, in percent (25-80)
//! side_by_side = 140          # from this width on the detail pane sits beside the list
//! tab = "overview"            # the detail tab a request opens on: overview, response, request, call-stack
//! body = "response"           # the body box's tab: response or request (b switches)
//! body_height = 40            # the body box's share of the detail pane, percent (15-85; 0 hides it)
//! wrap = true                 # wrap long lines in every box instead of cutting them off
//! scroll = 0                  # lines Ctrl+D and Ctrl+U move (0: half the box, like Neovim)
//! follow = true               # follow new requests while the cursor is on the newest
//! hints = true                # key hints in the footer
//! gap = 0                     # blank rows between boxes; side by side 2 × gap + 1 columns
//! clipboard = "auto"          # auto, osc52, native, off
//! images = true               # false draws images with half-blocks (as --no-images)
//! fps = 60                    # frames drawn a second at most (10-240); a still screen draws none
//!
//! [colors]                    # any color, as #rrggbb, for both palettes (the names: README, Config)
//! accent = "#61afef"
//! [colors.dark]               # only with the dark palette (and [colors.light] with the light one)
//! selection = "#2a4a7f"
//!
//! [capture]
//! body_cap = "10mb"           # bytes of each body kept (also 1048576)
//! stack_depth = 64
//! request_bodies = true
//! response_bodies = true
//!
//! [keymap]
//! pause = "p"                 # an action's name, then one key or a list of keys
//! copy = ["y", "ctrl+c"]
//!
//! [adb]
//! server = "127.0.0.1:5037"   # the adb server (ADB_SERVER_SOCKET and friends otherwise)
//! path = "/opt/android/platform-tools/adb"  # starts the server when none runs; doctor compares with it
//!
//! [storage]
//! memory = "256mb"            # body bytes kept in memory before spilling to disk
//! spill_dir = "/var/tmp"      # where spilled bodies and temporary recordings go
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml::Spanned;
use traffic_police_adb::Adb;
use traffic_police_core::rows::{Column, Sort};
use traffic_police_core::store::GraphSource;
use traffic_police_proto::msg::CaptureConfig;
use traffic_police_tui::App;
use traffic_police_tui::app::{FPS_RANGE, Tab, View};
use traffic_police_tui::explorer::BodyTab;
use traffic_police_tui::graph::{GraphLayout, GraphStyle, SMOOTHING_RANGE};
use traffic_police_tui::share::ClipboardMode;
use traffic_police_tui::theme::{COLOR_SLOTS, Palette, Theme, parse_borders, parse_hex};

/// `[ui] scroll`: lines a half-page jump moves (0: half the box).
const SCROLL_RANGE: std::ops::RangeInclusive<u16> = 0..=500;
/// `[ui] graph_height`: rows of the graph; 0 hides it.
const GRAPH_HEIGHT_RANGE: std::ops::RangeInclusive<u16> = 0..=40;
/// `[ui] body_height`: the body box's share of the detail pane (0 hides it).
const BODY_HEIGHT_RANGE: std::ops::RangeInclusive<u16> = 15..=85;
/// `[ui] side_by_side`: the width from which the detail pane sits beside the list.
const SIDE_BY_SIDE_RANGE: std::ops::RangeInclusive<u16> = 100..=500;
/// `[ui] gap`: blank rows between boxes one above the other.
const GAP_RANGE: std::ops::RangeInclusive<u16> = 0..=4;

/// A `[ui]` setting checked against its valid values: the name, the value, the check, and what
/// to use instead.
type WordCheck<'a> = (&'a str, &'a Option<Spanned<String>>, fn(&str) -> bool, &'a str);
type NumberCheck<'a> = (&'a str, &'a Option<Spanned<u16>>, fn(u16) -> bool, String);

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub ui: Ui,
    #[serde(default)]
    pub capture: Capture,
    #[serde(default)]
    pub keymap: BTreeMap<String, Spanned<Keys>>,
    #[serde(default)]
    pub adb: AdbSection,
    #[serde(default)]
    pub storage: Storage,
    /// `name = "#rrggbb"` for both palettes, and `dark` / `light` tables for one.
    #[serde(default)]
    pub colors: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    pub theme: Option<Spanned<String>>,
    pub graph_style: Option<Spanned<String>>,
    pub graph_layout: Option<Spanned<String>>,
    pub graph_smoothing: Option<Spanned<f64>>,
    pub time: Option<Spanned<String>>,
    pub columns: Option<Spanned<Vec<String>>>,
    pub divider: Option<Spanned<u16>>,
    pub clipboard: Option<Spanned<String>>,
    pub images: Option<bool>,
    pub fps: Option<Spanned<u16>>,
    pub borders: Option<Spanned<String>>,
    pub graph: Option<Spanned<String>>,
    pub graph_height: Option<Spanned<u16>>,
    pub sort: Option<Spanned<String>>,
    pub collapse: Option<bool>,
    pub view: Option<Spanned<String>>,
    pub side_by_side: Option<Spanned<u16>>,
    pub tab: Option<Spanned<String>>,
    pub body: Option<Spanned<String>>,
    pub body_height: Option<Spanned<u16>>,
    pub wrap: Option<bool>,
    pub scroll: Option<Spanned<u16>>,
    pub follow: Option<bool>,
    pub hints: Option<bool>,
    pub gap: Option<Spanned<u16>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub body_cap: Option<Spanned<Size>>,
    pub stack_depth: Option<Spanned<u32>>,
    pub request_bodies: Option<bool>,
    pub response_bodies: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdbSection {
    pub server: Option<Spanned<String>>,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub memory: Option<Spanned<Size>>,
    pub spill_dir: Option<PathBuf>,
}

/// A byte count: a number, or text like `10mb`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Size {
    Bytes(u64),
    Text(String),
}

/// One key or several.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Keys {
    One(String),
    Many(Vec<String>),
}

/// The settings as read, and what was wrong with them.
#[derive(Debug, Default)]
pub struct Loaded {
    pub config: Config,
    /// The file read (none when there is none).
    pub path: Option<PathBuf>,
    pub text: String,
    pub problems: Vec<String>,
}

/// Where the config file is looked for.
pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TRAFFIC_POLICE_CONFIG").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let dir = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        PathBuf::from(d)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".config")
    };
    Some(dir.join("traffic-police").join("config.toml"))
}

/// Reads the config file; a missing file is no problem, an unreadable or invalid one is.
pub fn load() -> Loaded {
    let Some(path) = path() else { return Loaded::default() };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let mut loaded = parse(&text);
            loaded.path = Some(path);
            loaded
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && std::env::var_os("TRAFFIC_POLICE_CONFIG").is_none() => {
            Loaded::default()
        }
        Err(e) => Loaded { problems: vec![format!("cannot read {}: {e}", path.display())], ..Loaded::default() },
    }
}

pub fn parse(text: &str) -> Loaded {
    match toml::from_str::<Config>(text) {
        Ok(config) => {
            let mut loaded = Loaded { config, text: text.to_string(), ..Loaded::default() };
            loaded.check();
            loaded
        }
        Err(e) => Loaded {
            text: text.to_string(),
            problems: vec![format!("the file was not applied: {}", e.to_string().trim_end())],
            ..Loaded::default()
        },
    }
}

/// `10mb`, `512k`, `2 GB`, `1048576` (1024-based).
fn size(s: &Size) -> Result<u64, String> {
    let text = match s {
        Size::Bytes(n) => return Ok(*n),
        Size::Text(t) => t.trim().to_ascii_lowercase(),
    };
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let (n, unit) = text.split_at(split);
    let n: f64 = n.parse().map_err(|_| format!("{text:?} is not a size (like 10mb)"))?;
    let mult: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return Err(format!("{text:?} is not a size (like 10mb)")),
    };
    Ok((n * mult as f64) as u64)
}

/// `method`, `req-size` (or the column's title).
fn column(name: &str) -> Option<Column> {
    let want = name.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    Column::OPTIONAL.into_iter().find(|c| c.title().to_ascii_lowercase().replace(' ', "-") == want)
}

impl Loaded {
    /// The line of a byte offset in the file.
    fn line(&self, offset: usize) -> usize {
        self.text[..offset.min(self.text.len())].matches('\n').count() + 1
    }

    /// Finds the problems that only show when the values are read.
    fn check(&mut self) {
        let mut found: Vec<(usize, String)> = Vec::new();
        let ui = &self.config.ui;
        if let Some(t) = &ui.theme
            && !matches!(t.get_ref().as_str(), "auto" | "dark" | "light")
        {
            found.push((t.span().start, format!("[ui] theme {:?}: use auto, dark or light", t.get_ref())));
        }
        if let Some(g) = &ui.graph_style
            && GraphStyle::parse(g.get_ref()).is_none()
        {
            found.push((
                g.span().start,
                format!("[ui] graph_style {:?}: use smooth, curves, heavy, lines or braille", g.get_ref()),
            ));
        }
        if let Some(l) = &ui.graph_layout
            && GraphLayout::parse(l.get_ref()).is_none()
        {
            found.push((l.span().start, format!("[ui] graph_layout {:?}: use mirror or overlay", l.get_ref())));
        }
        if let Some(g) = &ui.graph_smoothing
            && !SMOOTHING_RANGE.contains(g.get_ref())
        {
            let (min, max) = (SMOOTHING_RANGE.start(), SMOOTHING_RANGE.end());
            found.push((g.span().start, format!("[ui] graph_smoothing {}: use {min} to {max} (seconds)", g.get_ref())));
        }
        if let Some(t) = &ui.time
            && !matches!(t.get_ref().as_str(), "relative" | "wall")
        {
            found.push((t.span().start, format!("[ui] time {:?}: use relative or wall", t.get_ref())));
        }
        if let Some(cols) = &ui.columns {
            for c in cols.get_ref() {
                if column(c).is_none() {
                    let names: Vec<String> =
                        Column::OPTIONAL.iter().map(|c| c.title().to_ascii_lowercase().replace(' ', "-")).collect();
                    found.push((cols.span().start, format!("[ui] columns: {c:?} is not one of {}", names.join(", "))));
                }
            }
        }
        if let Some(d) = &ui.divider
            && !(25..=80).contains(d.get_ref())
        {
            found.push((d.span().start, format!("[ui] divider {}: use 25 to 80 (percent)", d.get_ref())));
        }
        if let Some(c) = &ui.clipboard
            && ClipboardMode::parse(c.get_ref()).is_none()
        {
            found.push((c.span().start, format!("[ui] clipboard {:?}: use auto, osc52, native or off", c.get_ref())));
        }
        if let Some(f) = &ui.fps
            && !FPS_RANGE.contains(f.get_ref())
        {
            let (min, max) = (FPS_RANGE.start(), FPS_RANGE.end());
            found.push((f.span().start, format!("[ui] fps {}: use {min} to {max} (frames a second)", f.get_ref())));
        }
        let words: [WordCheck; 6] = [
            ("borders", &ui.borders, |s| parse_borders(s).is_some(), "rounded, plain, double or thick"),
            ("graph", &ui.graph, |s| GraphSource::parse(s).is_some(), "app or requests"),
            ("sort", &ui.sort, |s| Sort::parse(s).is_some(), "a column's name, like \"status\" or \"time desc\""),
            ("view", &ui.view, |s| View::parse(s).is_some(), "connections, threads or rules"),
            ("tab", &ui.tab, |s| Tab::parse(s).is_some(), "overview, response, request or call-stack"),
            ("body", &ui.body, |s| BodyTab::parse(s).is_some(), "response or request"),
        ];
        for (name, value, valid, choices) in words {
            if let Some(v) = value
                && !valid(v.get_ref())
            {
                found.push((v.span().start, format!("[ui] {name} {:?}: use {choices}", v.get_ref())));
            }
        }
        let numbers: [NumberCheck; 5] = [
            (
                "scroll",
                &ui.scroll,
                |n| SCROLL_RANGE.contains(&n),
                format!("0 (half the box) to {}", SCROLL_RANGE.end()),
            ),
            (
                "graph_height",
                &ui.graph_height,
                |n| GRAPH_HEIGHT_RANGE.contains(&n),
                format!("0 (no graph) to {} rows", GRAPH_HEIGHT_RANGE.end()),
            ),
            (
                "body_height",
                &ui.body_height,
                |n| n == 0 || BODY_HEIGHT_RANGE.contains(&n),
                format!("{} to {} (percent), or 0 for no body box", BODY_HEIGHT_RANGE.start(), BODY_HEIGHT_RANGE.end()),
            ),
            (
                "side_by_side",
                &ui.side_by_side,
                |n| SIDE_BY_SIDE_RANGE.contains(&n),
                format!("{} to {} (columns)", SIDE_BY_SIDE_RANGE.start(), SIDE_BY_SIDE_RANGE.end()),
            ),
            ("gap", &ui.gap, |n| GAP_RANGE.contains(&n), format!("0 to {} (rows)", GAP_RANGE.end())),
        ];
        for (name, value, valid, choices) in numbers {
            if let Some(v) = value
                && !valid(*v.get_ref())
            {
                found.push((v.span().start, format!("[ui] {name} {}: use {choices}", v.get_ref())));
            }
        }
        found.extend(self.color_problems());
        for (what, v) in
            [("[capture] body_cap", &self.config.capture.body_cap), ("[storage] memory", &self.config.storage.memory)]
        {
            if let Some(v) = v
                && let Err(e) = size(v.get_ref())
            {
                found.push((v.span().start, format!("{what}: {e}")));
            }
        }
        if let Some(s) = &self.config.adb.server
            && server(s.get_ref()).is_none()
        {
            found.push((s.span().start, format!("[adb] server {:?}: use host:port, like 127.0.0.1:5037", s.get_ref())));
        }
        // keys: checked against a scratch keymap
        let mut keymap = traffic_police_tui::actions::Keymap::default();
        for (name, keys) in &self.config.keymap {
            for e in keymap.apply(&[(name.clone(), key_list(keys.get_ref()))]) {
                found.push((keys.span().start, format!("[keymap] {e}")));
            }
        }
        found.sort_by_key(|(at, _)| *at);
        for (at, what) in found {
            let line = self.line(at);
            self.problems.push(format!("line {line}: {what}"));
        }
    }

    /// `[colors]`: unknown names and values that are not `#rrggbb`, each at its line.
    fn color_problems(&self) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        let names: Vec<&str> = COLOR_SLOTS.iter().map(|(n, _)| *n).collect();
        let mut check = |section: &str, key: &str, value: &toml::Value| {
            let at = self.key_offset(section, key);
            if !names.contains(&key) {
                out.push((at, format!("[{section}] {key:?} is not a color's name; the names: {}", names.join(", "))));
            } else if value.as_str().and_then(parse_hex).is_none() {
                out.push((at, format!("[{section}] {key}: use a color like \"#61afef\"")));
            }
        };
        for (key, value) in &self.config.colors {
            match (key.as_str(), value) {
                ("dark" | "light", toml::Value::Table(t)) => {
                    for (k, v) in t {
                        check(&format!("colors.{key}"), k, v);
                    }
                }
                _ => check("colors", key, value),
            }
        }
        out
    }

    /// Where `key = …` is in the file, for a problem's line: in `[section]` when it is there,
    /// else anywhere (an inline table), else the start.
    fn key_offset(&self, section: &str, key: &str) -> usize {
        let mut current = String::new();
        let mut anywhere = None;
        let mut at = 0;
        for line in self.text.split_inclusive('\n') {
            let t = line.trim_start();
            if let Some(name) = t.strip_prefix('[').and_then(|r| r.split_once(']')).map(|(n, _)| n) {
                current = name.trim().to_string();
            } else if let Some(rest) = t.strip_prefix(key)
                && rest.trim_start().starts_with('=')
            {
                let here = at + (line.len() - t.len());
                if current == section {
                    return here;
                }
                anywhere.get_or_insert(here);
            }
            at += line.len();
        }
        anywhere.unwrap_or(0)
    }

    /// `[ui] borders` and `[colors]`, applied to the theme (invalid ones were reported).
    pub fn apply_theme(&self, theme: &mut Theme) {
        if let Some(b) = self.config.ui.borders.as_ref().and_then(|b| parse_borders(b.get_ref())) {
            theme.borders = b;
        }
        let palette = match theme.palette {
            Palette::Dark => "dark",
            Palette::Light => "light",
        };
        let mut set = |key: &str, value: &toml::Value| {
            if let Some(rgb) = value.as_str().and_then(parse_hex) {
                theme.set_color(key, rgb);
            }
        };
        for (key, value) in &self.config.colors {
            if !matches!(key.as_str(), "dark" | "light") {
                set(key, value);
            }
        }
        if let Some(toml::Value::Table(t)) = self.config.colors.get(palette) {
            for (key, value) in t {
                set(key, value);
            }
        }
    }

    /// The theme, when the file sets a valid one.
    pub fn theme(&self) -> Option<&str> {
        self.config.ui.theme.as_ref().map(|t| t.get_ref().as_str()).filter(|t| matches!(*t, "auto" | "dark" | "light"))
    }

    /// Images through the terminal's graphics protocol (`[ui] images`).
    pub fn images(&self) -> bool {
        self.config.ui.images.unwrap_or(true)
    }

    /// The default keys with the `[keymap]` entries applied (invalid ones were reported).
    pub fn keymap(&self) -> traffic_police_tui::actions::Keymap {
        let mut keymap = traffic_police_tui::actions::Keymap::default();
        for (name, keys) in &self.config.keymap {
            let _ = keymap.apply(&[(name.clone(), key_list(keys.get_ref()))]);
        }
        keymap
    }

    /// Applies the `[ui]` and `[keymap]` settings to the app (invalid ones were reported).
    pub fn apply_ui(&self, app: &mut App) {
        let ui = &self.config.ui;
        if let Some(g) = ui.graph_style.as_ref().and_then(|g| GraphStyle::parse(g.get_ref())) {
            app.graph_style = g;
        }
        if let Some(l) = ui.graph_layout.as_ref().and_then(|l| GraphLayout::parse(l.get_ref())) {
            app.graph_layout = l;
        }
        if let Some(g) = ui.graph_smoothing.as_ref().map(|g| *g.get_ref()).filter(|g| SMOOTHING_RANGE.contains(g)) {
            app.graph_smoothing = g;
        }
        if let Some(t) = &ui.time {
            match t.get_ref().as_str() {
                "wall" => app.wall_labels = true,
                "relative" => app.wall_labels = false,
                _ => {}
            }
        }
        if let Some(cols) = &ui.columns {
            for c in cols.get_ref().iter().filter_map(|c| column(c)) {
                if !app.columns.contains(&c) {
                    let at = app.columns.iter().position(|&x| x == Column::Timeline).unwrap_or(app.columns.len());
                    app.columns.insert(at, c);
                }
            }
        }
        if let Some(d) = ui.divider.as_ref().map(|d| *d.get_ref()).filter(|d| (25..=80).contains(d)) {
            app.split_pct = d;
        }
        if let Some(c) = ui.clipboard.as_ref().and_then(|c| ClipboardMode::parse(c.get_ref())) {
            app.clipboard = c;
        }
        if let Some(f) = ui.fps.as_ref().map(|f| *f.get_ref()).filter(|f| FPS_RANGE.contains(f)) {
            app.fps = f;
        }
        let number = |v: &Option<Spanned<u16>>| v.as_ref().map(|v| *v.get_ref());
        let word = |v: &Option<Spanned<String>>| v.as_ref().map(|v| v.get_ref().clone());
        let p = &mut app.prefs;
        if let Some(w) = ui.wrap {
            p.wrap = w;
        }
        if let Some(n) = number(&ui.scroll).filter(|n| SCROLL_RANGE.contains(n)) {
            p.scroll = n;
        }
        if let Some(f) = ui.follow {
            p.follow = f;
        }
        if let Some(n) = number(&ui.graph_height).filter(|n| GRAPH_HEIGHT_RANGE.contains(n)) {
            p.graph_height = Some(n);
        }
        if let Some(n) = number(&ui.body_height).filter(|n| *n == 0 || BODY_HEIGHT_RANGE.contains(n)) {
            p.body_height = n;
        }
        if let Some(n) = number(&ui.side_by_side).filter(|n| SIDE_BY_SIDE_RANGE.contains(n)) {
            p.side_by_side = n;
        }
        if let Some(h) = ui.hints {
            p.hints = h;
        }
        if let Some(n) = number(&ui.gap).filter(|n| GAP_RANGE.contains(n)) {
            p.gap = n;
        }
        if let Some(v) = word(&ui.view).and_then(|v| View::parse(&v)) {
            app.view = v;
        }
        if let Some(t) = word(&ui.tab).and_then(|t| Tab::parse(&t)) {
            app.detail.tab = t;
        }
        if let Some(b) = word(&ui.body).and_then(|b| BodyTab::parse(&b)) {
            app.explorer.tab = b;
        }
        if let Some(g) = word(&ui.graph).and_then(|g| GraphSource::parse(&g)) {
            app.graph_source = g;
        }
        if let Some(s) = word(&ui.sort).and_then(|s| Sort::parse(&s)) {
            app.rows.set_sort(s);
        }
        if let Some(c) = ui.collapse {
            app.rows.set_collapse(c);
        }
        app.keymap = self.keymap();
        if !self.problems.is_empty() {
            for p in &self.problems {
                tracing::warn!("config: {p}");
            }
            let n = self.problems.len();
            app.flash(format!(
                "config.toml: {n} problem{} (defaults used); traffic-police doctor lists them",
                if n == 1 { "" } else { "s" }
            ));
        }
    }

    /// What to capture, from `[capture]`.
    pub fn capture(&self) -> CaptureConfig {
        let c = &self.config.capture;
        let mut out = CaptureConfig::default();
        if let Some(n) = c.body_cap.as_ref().and_then(|s| size(s.get_ref()).ok()) {
            out.body_cap = n;
        }
        if let Some(d) = &c.stack_depth {
            out.stack_depth = *d.get_ref();
        }
        if let Some(b) = c.request_bodies {
            out.capture_request_bodies = b;
        }
        if let Some(b) = c.response_bodies {
            out.capture_response_bodies = b;
        }
        out
    }

    /// The adb server (`[adb] server`, else the environment, else 127.0.0.1:5037), started with
    /// `[adb] path` when it is not running.
    pub fn adb(&self) -> Adb {
        let adb = match self.config.adb.server.as_ref().and_then(|s| server(s.get_ref())) {
            Some((host, port)) => Adb::at(host, port),
            None => Adb::from_env(),
        };
        match self.adb_path() {
            Some(path) => adb.with_binary(path),
            None => adb,
        }
    }

    /// `[storage]`: the memory budget and where spilled bodies go.
    pub fn apply_storage(&self) {
        if let Some(n) = self.config.storage.memory.as_ref().and_then(|s| size(s.get_ref()).ok()) {
            traffic_police_core::store::set_default_budget(n);
        }
        if let Some(dir) = &self.config.storage.spill_dir {
            traffic_police_core::store::spill::set_parent(dir.clone());
        }
    }

    /// `[ui] clipboard`, when valid.
    pub fn clipboard(&self) -> Option<ClipboardMode> {
        self.config.ui.clipboard.as_ref().and_then(|c| ClipboardMode::parse(c.get_ref()))
    }

    pub fn adb_path(&self) -> Option<&Path> {
        self.config.adb.path.as_deref()
    }
}

fn key_list(k: &Keys) -> Vec<String> {
    match k {
        Keys::One(s) => vec![s.clone()],
        Keys::Many(v) => v.clone(),
    }
}

/// `host:port`, `tcp:host:port`, `[::1]:5037`.
fn server(s: &str) -> Option<(String, u16)> {
    let s = s.strip_prefix("tcp:").unwrap_or(s);
    let (host, port) = s.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (!host.is_empty()).then(|| Some((host.to_string(), port.parse().ok()?)))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use traffic_police_tui::theme::Depth;

    #[test]
    fn a_full_file_applies() {
        let text = r#"
[ui]
theme = "light"
graph_style = "braille"
graph_layout = "overlay"
graph_smoothing = 2
time = "wall"
columns = ["method", "req-size"]
divider = 60
clipboard = "off"
images = false
fps = 120

[capture]
body_cap = "1mb"
stack_depth = 16
response_bodies = false

[keymap]
pause = "p"
copy = ["y", "ctrl+y"]

[adb]
server = "tcp:127.0.0.1:5038"

[storage]
memory = 1048576
"#;
        let l = parse(text);
        assert!(l.problems.is_empty(), "{:?}", l.problems);
        assert_eq!(l.theme(), Some("light"));
        assert!(!l.images());
        let c = l.capture();
        assert_eq!((c.body_cap, c.stack_depth, c.capture_response_bodies), (1 << 20, 16, false));
        assert_eq!(l.adb().address(), "127.0.0.1:5038");
        let mut app = App::new(traffic_police_core::SessionStore::new(), traffic_police_tui::Theme::default());
        l.apply_ui(&mut app);
        assert_eq!(app.graph_style, GraphStyle::Braille);
        assert_eq!(app.graph_layout, GraphLayout::Overlay);
        assert_eq!(app.graph_smoothing, 2.0, "a whole number of seconds reads as a float");
        assert!(app.wall_labels);
        assert!(app.columns.contains(&Column::Method) && app.columns.contains(&Column::ReqSize));
        assert_eq!(app.columns.last(), Some(&Column::Timeline), "extra columns go before the timeline");
        assert_eq!((app.split_pct, app.clipboard, app.fps), (60, ClipboardMode::Off, 120));
        let p = traffic_police_tui::actions::Action::Pause;
        assert_eq!(app.keymap.key_label(p), "p");
    }

    #[test]
    fn problems_name_their_line_and_the_rest_still_applies() {
        let (min, max) = (FPS_RANGE.start(), FPS_RANGE.end());
        let text = format!(
            "[ui]\ntheme = \"blue\"\ngraph_style = \"heavy\"\nfps = {}\ngraph_layout = \"stacked\"\ngraph_smoothing = 0.1\n[keymap]\npuase = \"p\"\ncopy = \"ctrl+\"\n\n[capture]\nbody_cap = \"lots\"\n",
            max + 1
        );
        let l = parse(&text);
        assert_eq!(l.problems.len(), 7, "{:?}", l.problems);
        assert!(
            l.problems.contains(&"line 6: [ui] graph_smoothing 0.1: use 0.5 to 5 (seconds)".to_string()),
            "{:?}",
            l.problems
        );
        assert!(l.problems[0].starts_with("line 2: [ui] theme \"blue\""), "{:?}", l.problems);
        assert!(
            l.problems.contains(&"line 5: [ui] graph_layout \"stacked\": use mirror or overlay".to_string()),
            "{:?}",
            l.problems
        );
        assert!(l.problems.contains(&format!("line 4: [ui] fps {}: use {min} to {max} (frames a second)", max + 1)));
        assert!(
            l.problems
                .iter()
                .any(|p| p.starts_with("line 8: [keymap] unknown action \"puase\"; valid names: up, down"))
        );
        assert!(l.problems.iter().any(|p| p.starts_with("line 9: [keymap] copy:")), "{:?}", l.problems);
        assert!(l.problems.iter().any(|p| p.starts_with("line 12: [capture] body_cap")), "{:?}", l.problems);
        assert_eq!(l.theme(), None);
        let mut app = App::new(traffic_police_core::SessionStore::new(), traffic_police_tui::Theme::default());
        l.apply_ui(&mut app);
        assert_eq!((app.graph_style, app.fps), (GraphStyle::Heavy, traffic_police_tui::app::DEFAULT_FPS));
        assert_eq!(app.graph_layout, GraphLayout::Mirror, "the default stays when the value is bad");
        assert_eq!(app.graph_smoothing, traffic_police_tui::graph::SMOOTHING_SECS);
    }

    #[test]
    fn layout_behavior_and_start_settings_apply() {
        let text = r#"
[ui]
borders = "double"
graph = "requests"
graph_height = 0
sort = "status desc"
collapse = true
view = "threads"
side_by_side = 200
tab = "call-stack"
body = "request"
body_height = 0
wrap = false
scroll = 10
follow = false
hints = false
gap = 2
"#;
        let l = parse(text);
        assert!(l.problems.is_empty(), "{:?}", l.problems);
        let mut app = App::new(traffic_police_core::SessionStore::new(), Theme::default());
        l.apply_ui(&mut app);
        let p = &app.prefs;
        assert_eq!((p.wrap, p.scroll, p.follow, p.hints, p.gap), (false, 10, false, false, 2));
        assert_eq!((p.graph_height, p.body_height, p.side_by_side), (Some(0), 0, 200));
        assert_eq!((app.view, app.detail.tab, app.explorer.tab), (View::Threads, Tab::CallStack, BodyTab::Request));
        assert_eq!(app.graph_source, GraphSource::Captured);
        assert_eq!(app.rows.sort, Sort { column: Column::Status, descending: true });
        assert!(app.rows.collapse);
        let mut theme = Theme::default();
        l.apply_theme(&mut theme);
        assert_eq!(Some(theme.borders), parse_borders("double"));
    }

    #[test]
    fn colors_apply_to_both_palettes_or_to_one() {
        let text = "[colors]\naccent = \"#112233\"\n[colors.dark]\nselection = \"#445566\"\n[colors.light]\nselection = \"#778899\"\n";
        let l = parse(text);
        assert!(l.problems.is_empty(), "{:?}", l.problems);
        for (palette, selection) in [(Palette::Dark, (0x44, 0x55, 0x66)), (Palette::Light, (0x77, 0x88, 0x99))] {
            let mut theme = Theme::new(palette, Depth::TrueColor);
            l.apply_theme(&mut theme);
            let mut want = Theme::new(palette, Depth::TrueColor);
            want.set_color("accent", (0x11, 0x22, 0x33));
            want.set_color("selection", selection);
            assert_eq!((theme.accent(), theme.selected()), (want.accent(), want.selected()), "{palette:?}");
            assert_eq!(theme.text(), Theme::new(palette, Depth::TrueColor).text(), "the rest is unchanged");
        }
    }

    #[test]
    fn bad_settings_and_colors_name_their_line() {
        let text = "[ui]\nborders = \"dotted\"\nscroll = 900\nbody_height = 5\ntab = \"headers\"\ngap = 9\n\
                    [colors]\naccent = \"blue\"\ncolour = \"#000000\"\nselection = \"#000000\"\n\
                    [colors.dark]\nselection = \"#12345\"\n";
        let l = parse(text);
        let want = [
            "line 2: [ui] borders \"dotted\": use rounded, plain, double or thick",
            "line 3: [ui] scroll 900: use 0 (half the box) to 500",
            "line 4: [ui] body_height 5: use 15 to 85 (percent), or 0 for no body box",
            "line 5: [ui] tab \"headers\": use overview, response, request or call-stack",
            "line 6: [ui] gap 9: use 0 to 4 (rows)",
            "line 8: [colors] accent: use a color like \"#61afef\"",
            "line 9: [colors] \"colour\" is not a color's name; the names: text, dim,",
            "line 12: [colors.dark] selection: use a color like \"#61afef\"",
        ];
        assert_eq!(l.problems.len(), want.len(), "{:?}", l.problems);
        for (got, want) in l.problems.iter().zip(want) {
            assert!(got.starts_with(want), "{got:?} should start with {want:?}");
        }
        // the valid settings beside them still apply
        let mut theme = Theme::default();
        l.apply_theme(&mut theme);
        let mut black = Theme::default();
        black.set_color("selection", (0, 0, 0));
        assert_eq!(theme.selected(), black.selected());
        assert_eq!(theme.accent(), Theme::default().accent());
    }

    #[test]
    fn a_broken_file_is_not_applied() {
        let l = parse("[ui]\ncolour = \"dark\"\n");
        assert_eq!(l.problems.len(), 1);
        assert!(l.problems[0].contains("unknown field `colour`"), "{}", l.problems[0]);
        assert!(l.problems[0].contains("line 2"), "{}", l.problems[0]);
        assert!(parse("[ui\n").problems[0].starts_with("the file was not applied"));
    }
}
