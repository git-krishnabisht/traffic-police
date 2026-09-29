//! Colors by meaning, with dark and light palettes, color-depth fallback and NO_COLOR
//! (ARCHITECTURE.md §5.13).

use ratatui::style::{Color, Modifier, Style};
use traffic_police_core::decode::Tok;
use traffic_police_core::model::StatusClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    TrueColor,
    Ansi256,
    Ansi16,
    /// NO_COLOR: meaning carried by text and modifiers only.
    Mono,
}

impl Depth {
    /// From the environment: `NO_COLOR`, `COLORTERM`, `TERM`.
    pub fn detect() -> Depth {
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Depth::Mono;
        }
        let colorterm = std::env::var("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return Depth::TrueColor;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.contains("256color") || std::env::var("TERM_PROGRAM").is_ok_and(|p| p == "Apple_Terminal") {
            Depth::Ansi256
        } else if term.is_empty() && cfg!(windows) {
            Depth::TrueColor
        } else {
            Depth::Ansi16
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Palette {
    #[default]
    Dark,
    Light,
}

impl Palette {
    /// From `COLORFGBG` (`"fg;bg"`, set by rxvt, Konsole and others) when present; dark
    /// otherwise.
    pub fn detect() -> Palette {
        let bg = std::env::var("COLORFGBG")
            .ok()
            .and_then(|v| v.rsplit(';').next().and_then(|b| b.trim().parse::<u8>().ok()));
        match bg {
            Some(7 | 9..=15) => Palette::Light,
            _ => Palette::Dark,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub depth: Depth,
    pub palette: Palette,
    rgb: RgbSet,
}

#[derive(Debug, Clone, Copy)]
struct RgbSet {
    fg: (u8, u8, u8),
    dim: (u8, u8, u8),
    faint: (u8, u8, u8),
    accent: (u8, u8, u8),
    sel_bg: (u8, u8, u8),
    border: (u8, u8, u8),
    recv: (u8, u8, u8),
    send: (u8, u8, u8),
    wait: (u8, u8, u8),
    ok: (u8, u8, u8),
    redirect: (u8, u8, u8),
    client_err: (u8, u8, u8),
    server_err: (u8, u8, u8),
    marker: (u8, u8, u8),
    key: (u8, u8, u8),
    string: (u8, u8, u8),
    number: (u8, u8, u8),
    keyword: (u8, u8, u8),
    tag: (u8, u8, u8),
    attr: (u8, u8, u8),
    graph_sel: (u8, u8, u8),
}

const DARK: RgbSet = RgbSet {
    fg: (220, 223, 228),
    dim: (140, 146, 156),
    faint: (92, 98, 108),
    accent: (97, 175, 239),
    sel_bg: (38, 62, 110),
    border: (70, 76, 86),
    recv: (74, 158, 255),
    send: (242, 166, 72),
    wait: (120, 126, 136),
    ok: (106, 190, 120),
    redirect: (86, 182, 194),
    client_err: (229, 192, 90),
    server_err: (232, 96, 96),
    marker: (198, 120, 221),
    key: (224, 108, 180),
    string: (152, 195, 121),
    number: (97, 175, 239),
    keyword: (229, 170, 90),
    tag: (224, 108, 117),
    attr: (209, 154, 102),
    graph_sel: (52, 58, 80),
};

const LIGHT: RgbSet = RgbSet {
    fg: (36, 41, 47),
    dim: (95, 103, 112),
    faint: (150, 156, 163),
    accent: (9, 105, 218),
    sel_bg: (206, 226, 255),
    border: (190, 196, 204),
    recv: (9, 105, 218),
    send: (207, 110, 10),
    wait: (140, 146, 156),
    ok: (26, 127, 55),
    redirect: (5, 120, 140),
    client_err: (154, 103, 0),
    server_err: (207, 34, 46),
    marker: (130, 80, 223),
    key: (163, 21, 113),
    string: (10, 120, 50),
    number: (5, 80, 174),
    keyword: (180, 80, 0),
    tag: (160, 30, 50),
    attr: (150, 90, 20),
    graph_sel: (222, 230, 246),
};

fn ansi256((r, g, b): (u8, u8, u8)) -> u8 {
    // grey ramp for near-greys, else the 6x6x6 cube
    if r.abs_diff(g) < 12 && g.abs_diff(b) < 12 {
        let v = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
        if v < 8 {
            return 16;
        }
        if v > 238 {
            return 231;
        }
        return 232 + ((v - 8) * 24 / 231) as u8;
    }
    let q = |c: u8| -> u8 {
        if c < 48 {
            0
        } else if c < 115 {
            1
        } else {
            (c - 35) / 40
        }
    };
    16 + 36 * q(r) + 6 * q(g) + q(b)
}

fn ansi16((r, g, b): (u8, u8, u8)) -> Color {
    let bright = u16::from(r) + u16::from(g) + u16::from(b) > 450;
    let (hr, hg, hb) = (r > 140, g > 140, b > 150);
    match (hr, hg, hb) {
        (true, false, false) => {
            if bright {
                Color::LightRed
            } else {
                Color::Red
            }
        }
        (false, true, false) => {
            if bright {
                Color::LightGreen
            } else {
                Color::Green
            }
        }
        (false, false, true) => {
            if bright {
                Color::LightBlue
            } else {
                Color::Blue
            }
        }
        (true, true, false) => Color::Yellow,
        (false, true, true) => Color::Cyan,
        (true, false, true) => Color::Magenta,
        (true, true, true) => Color::White,
        (false, false, false) => {
            if bright {
                Color::Gray
            } else {
                Color::DarkGray
            }
        }
    }
}

impl Theme {
    pub fn new(palette: Palette, depth: Depth) -> Self {
        Theme { depth, palette, rgb: if palette == Palette::Light { LIGHT } else { DARK } }
    }

    fn c(&self, rgb: (u8, u8, u8)) -> Color {
        match self.depth {
            Depth::TrueColor => Color::Rgb(rgb.0, rgb.1, rgb.2),
            Depth::Ansi256 => Color::Indexed(ansi256(rgb)),
            Depth::Ansi16 => ansi16(rgb),
            Depth::Mono => Color::Reset,
        }
    }

    fn fg(&self, rgb: (u8, u8, u8)) -> Style {
        if self.depth == Depth::Mono { Style::default() } else { Style::default().fg(self.c(rgb)) }
    }

    pub fn mono(&self) -> bool {
        self.depth == Depth::Mono
    }

    pub fn text(&self) -> Style {
        self.fg(self.rgb.fg)
    }
    pub fn dim(&self) -> Style {
        if self.mono() { Style::default().add_modifier(Modifier::DIM) } else { self.fg(self.rgb.dim) }
    }
    pub fn faint(&self) -> Style {
        if self.mono() { Style::default().add_modifier(Modifier::DIM) } else { self.fg(self.rgb.faint) }
    }
    pub fn accent(&self) -> Style {
        if self.mono() { Style::default().add_modifier(Modifier::BOLD) } else { self.fg(self.rgb.accent) }
    }
    pub fn title(&self) -> Style {
        self.text().add_modifier(Modifier::BOLD)
    }
    pub fn border(&self, focused: bool) -> Style {
        if focused { self.accent() } else { self.fg(self.rgb.border) }
    }
    pub fn selected(&self) -> Style {
        if self.mono() {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(self.c(self.rgb.sel_bg))
        }
    }
    pub fn selected_bg(&self) -> Option<Color> {
        (!self.mono()).then(|| self.c(self.rgb.sel_bg))
    }
    pub fn graph_selection_bg(&self) -> Option<Color> {
        (!self.mono()).then(|| self.c(self.rgb.graph_sel))
    }
    pub fn recv(&self) -> Color {
        if self.mono() { Color::Reset } else { self.c(self.rgb.recv) }
    }
    pub fn send(&self) -> Color {
        if self.mono() { Color::Reset } else { self.c(self.rgb.send) }
    }
    pub fn wait(&self) -> Color {
        if self.mono() { Color::Reset } else { self.c(self.rgb.wait) }
    }
    pub fn marker(&self) -> Style {
        if self.mono() { Style::default().add_modifier(Modifier::BOLD) } else { self.fg(self.rgb.marker) }
    }
    pub fn marker_color(&self) -> Color {
        if self.mono() { Color::Reset } else { self.c(self.rgb.marker) }
    }
    pub fn error(&self) -> Style {
        if self.mono() { Style::default().add_modifier(Modifier::BOLD) } else { self.fg(self.rgb.server_err) }
    }
    pub fn warn(&self) -> Style {
        if self.mono() { Style::default().add_modifier(Modifier::BOLD) } else { self.fg(self.rgb.client_err) }
    }
    pub fn ok(&self) -> Style {
        self.fg(self.rgb.ok)
    }

    pub fn status(&self, class: StatusClass) -> Style {
        let s = match class {
            StatusClass::Success => self.fg(self.rgb.ok),
            StatusClass::Redirect | StatusClass::Informational => self.fg(self.rgb.redirect),
            StatusClass::ClientError => self.fg(self.rgb.client_err),
            StatusClass::ServerError | StatusClass::Failed => self.fg(self.rgb.server_err),
            StatusClass::Pending => self.dim(),
        };
        if self.mono() && matches!(class, StatusClass::ServerError | StatusClass::Failed | StatusClass::ClientError) {
            s.add_modifier(Modifier::BOLD)
        } else {
            s
        }
    }

    pub fn tok(&self, tok: Tok) -> Style {
        if self.mono() {
            return match tok {
                Tok::Key | Tok::Tag | Tok::Field => Style::default().add_modifier(Modifier::BOLD),
                Tok::Comment | Tok::Meta | Tok::Punct => Style::default().add_modifier(Modifier::DIM),
                Tok::Error => Style::default().add_modifier(Modifier::REVERSED),
                _ => Style::default(),
            };
        }
        match tok {
            Tok::Plain => self.text(),
            Tok::Punct => self.dim(),
            Tok::Key => self.fg(self.rgb.key),
            Tok::Str | Tok::AttrValue => self.fg(self.rgb.string),
            Tok::Num => self.fg(self.rgb.number),
            Tok::Bool | Tok::Null => self.fg(self.rgb.keyword),
            Tok::Tag => self.fg(self.rgb.tag),
            Tok::Attr => self.fg(self.rgb.attr),
            Tok::Comment | Tok::Meta => self.faint(),
            Tok::Field => self.fg(self.rgb.key),
            Tok::Error => self.error(),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::new(Palette::Dark, Depth::TrueColor)
    }
}
