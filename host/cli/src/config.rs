//! The user's settings (ARCHITECTURE.md §5.13): `config.toml` in `$XDG_CONFIG_HOME/traffic-police`
//! (default `~/.config/traffic-police`) on Linux and macOS, `%APPDATA%\traffic-police` on
//! Windows, or the file `TRAFFIC_POLICE_CONFIG` names. Problems are reported with their line, and
//! the defaults apply in their place.
//!
//! ```toml
//! [ui]
//! theme = "dark"              # auto, dark, light (--theme wins)
//! graph_style = "heavy"       # heavy, lines, area, braille
//! time = "wall"               # relative (since the session started) or wall (clock time)
//! columns = ["method", "host"] # optional columns shown beside the default ones
//! divider = 55                # the list's share of the width, in percent (25-80)
//! clipboard = "auto"          # auto, osc52, native, off
//! images = true               # false draws images with half-blocks (as --no-images)
//! fps = 60                    # frames drawn a second at most (10-240); a still screen draws none
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
//! path = "/opt/android/platform-tools/adb"  # the adb doctor compares with the server
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
use traffic_police_core::rows::Column;
use traffic_police_proto::msg::CaptureConfig;
use traffic_police_tui::App;
use traffic_police_tui::app::FPS_RANGE;
use traffic_police_tui::graph::GraphStyle;
use traffic_police_tui::share::ClipboardMode;

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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    pub theme: Option<Spanned<String>>,
    pub graph_style: Option<Spanned<String>>,
    pub time: Option<Spanned<String>>,
    pub columns: Option<Spanned<Vec<String>>>,
    pub divider: Option<Spanned<u16>>,
    pub clipboard: Option<Spanned<String>>,
    pub images: Option<bool>,
    pub fps: Option<Spanned<u16>>,
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
                format!("[ui] graph_style {:?}: use heavy, lines, area or braille", g.get_ref()),
            ));
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

    /// The theme, when the file sets a valid one.
    pub fn theme(&self) -> Option<&str> {
        self.config.ui.theme.as_ref().map(|t| t.get_ref().as_str()).filter(|t| matches!(*t, "auto" | "dark" | "light"))
    }

    /// Images through the terminal's graphics protocol (`[ui] images`).
    pub fn images(&self) -> bool {
        self.config.ui.images.unwrap_or(true)
    }

    /// Applies the `[ui]` and `[keymap]` settings to the app (invalid ones were reported).
    pub fn apply_ui(&self, app: &mut App) {
        let ui = &self.config.ui;
        if let Some(g) = ui.graph_style.as_ref().and_then(|g| GraphStyle::parse(g.get_ref())) {
            app.graph_style = g;
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
        for (name, keys) in &self.config.keymap {
            let _ = app.keymap.apply(&[(name.clone(), key_list(keys.get_ref()))]);
        }
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

    /// The adb server: `[adb] server`, else the environment (as adb itself reads it).
    pub fn adb(&self) -> Adb {
        match self.config.adb.server.as_ref().and_then(|s| server(s.get_ref())) {
            Some((host, port)) => Adb::at(host, port),
            None => Adb::from_env(),
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

    #[test]
    fn a_full_file_applies() {
        let text = r#"
[ui]
theme = "light"
graph_style = "braille"
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
            "[ui]\ntheme = \"blue\"\ngraph_style = \"heavy\"\nfps = {}\n[keymap]\npuase = \"p\"\ncopy = \"ctrl+\"\n\n[capture]\nbody_cap = \"lots\"\n",
            max + 1
        );
        let l = parse(&text);
        assert_eq!(l.problems.len(), 5, "{:?}", l.problems);
        assert!(l.problems[0].starts_with("line 2: [ui] theme \"blue\""), "{:?}", l.problems);
        assert_eq!(l.problems[1], format!("line 4: [ui] fps {}: use {min} to {max} (frames a second)", max + 1));
        assert!(
            l.problems
                .iter()
                .any(|p| p.starts_with("line 6: [keymap] unknown action \"puase\"; valid names: up, down"))
        );
        assert!(l.problems.iter().any(|p| p.starts_with("line 7: [keymap] copy:")), "{:?}", l.problems);
        assert!(l.problems.iter().any(|p| p.starts_with("line 10: [capture] body_cap")), "{:?}", l.problems);
        assert_eq!(l.theme(), None);
        let mut app = App::new(traffic_police_core::SessionStore::new(), traffic_police_tui::Theme::default());
        l.apply_ui(&mut app);
        assert_eq!((app.graph_style, app.fps), (GraphStyle::Heavy, traffic_police_tui::app::DEFAULT_FPS));
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
