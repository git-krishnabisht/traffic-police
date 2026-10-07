//! Enter on a header or a JSON value (ARCHITECTURE.md §5.10): a menu to copy the value, decode it
//! (JWT, base64, URL encoding) or filter by it. Decoded values show in a popup.

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use traffic_police_core::decode::doc::{StyledLine, Tok};
use traffic_police_core::decode::hex;
use traffic_police_core::model::TxnIdx;
use traffic_police_core::values;

use crate::actions::Action;
use crate::app::{App, Focus, Overlay};
use crate::detail::draw_line;
use crate::share::{Menu, MenuAction, MenuItem};
use crate::wrap::{self, Top};

/// A decoded value in a popup.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub title: String,
    pub lines: Vec<StyledLine>,
    /// The line at the top, and (`[ui] wrap`) how many of its rows are above the box.
    pub scroll: Top,
    /// The box's rows and width last time, for paging and wrapping.
    pub page: usize,
    pub width: usize,
}

impl Decoded {
    /// The rows line `i` takes at `width`: later rows start two columns past its indent.
    fn rows(&self, i: usize, width: usize) -> Vec<wrap::Part> {
        let text = &self.lines[i].text;
        wrap::layout(text, width, text.len() - text.trim_start_matches(' ').len() + 2, false)
    }

    /// Runs `f` over the lines as the box sees them (rows each takes when they wrap).
    fn with_lines<R>(&self, wrapping: bool, f: impl FnOnce(&mut wrap::Lines) -> R) -> R {
        let width = self.width.max(1);
        let mut rows = |i: usize| if wrapping { self.rows(i, width).len() } else { 1 };
        let mut lines = wrap::Lines { len: self.lines.len(), view: self.page.max(1), rows: &mut rows };
        f(&mut lines)
    }
}

/// A filter token for a value: quotes cannot be escaped, so the text stops at one; long values
/// are cut (the filter looks for a part anyway).
fn filter_part(s: &str) -> String {
    s.split('"').next().unwrap_or("").chars().take(80).collect()
}

/// Bytes as lines: text (pretty JSON when it is JSON), or a hex dump.
pub(crate) fn byte_lines(bytes: &[u8]) -> Vec<StyledLine> {
    if let Ok(doc) = traffic_police_core::decode::json::parse(bytes) {
        return doc.lines.into_iter().map(|l| l.line).collect();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.lines().map(StyledLine::plain).collect(),
        Err(_) => (0..hex::line_count(bytes.len())).map(|r| StyledLine::plain(hex::line(bytes, r))).collect(),
    }
}

impl App {
    /// The value menu for what the detail cursor is on; false when it is on no value.
    pub fn open_value_menu(&mut self, txn: TxnIdx) -> bool {
        let (title, value, filter) = if let Some((name, value)) = self.header_at_cursor(txn) {
            let filter = format!("header:\"{}={}\"", name.to_ascii_lowercase(), filter_part(&value));
            (format!("header {name}"), value, filter)
        } else if let Some(path) = self.path_at_cursor(txn) {
            let Some(json) = self.json_value(txn, &path) else { return false };
            // strings are decoded as their text; other values as JSON
            let value = match serde_json::from_str::<serde_json::Value>(&json) {
                Ok(serde_json::Value::String(s)) => s,
                _ => json,
            };
            if value.starts_with(['{', '[']) {
                // a container: Enter folds it
                return false;
            }
            let filter = format!("body:\"{}\"", filter_part(&value));
            (format!("value at {path}"), value, filter)
        } else {
            return false;
        };
        let mut items = vec![MenuItem { key: 'c', label: "copy".into(), action: MenuAction::CopyText(value.clone()) }];
        let jwt = values::jwt(&value).is_some() || values::find_jwt(&value).is_some();
        if jwt {
            items.push(MenuItem { key: 'j', label: "decode JWT".into(), action: MenuAction::DecodeJwt(value.clone()) });
        } else if let Some((bytes, alphabet)) = values::base64(&value) {
            items.push(MenuItem {
                key: 'b',
                label: format!("decode {alphabet} ({} bytes)", bytes.len()),
                action: MenuAction::DecodeBase64(value.clone()),
            });
        }
        if values::url_decode(&value).is_some() {
            items.push(MenuItem { key: 'u', label: "URL-decode".into(), action: MenuAction::DecodeUrl(value.clone()) });
        }
        items.push(MenuItem { key: 'f', label: format!("filter: {filter}"), action: MenuAction::FilterBy(filter) });
        self.menu = Some(Menu { title, items, cursor: 0 });
        self.overlay = Overlay::Menu;
        true
    }

    /// Runs a value-menu choice; false when `action` is not one.
    pub fn run_value_action(&mut self, action: &MenuAction) -> bool {
        match action {
            MenuAction::CopyText(s) => self.copy_text(s.clone()),
            MenuAction::DecodeJwt(s) => {
                let token = values::jwt(s).or_else(|| values::find_jwt(s).and_then(|r| values::jwt(&s[r])));
                match token {
                    Some(j) => {
                        let now = self.wall_now_ms();
                        self.open_decoded(format!("JWT {}", j.alg()), j.lines(now));
                    }
                    None => self.flash("not a JWT"),
                }
            }
            MenuAction::DecodeBase64(s) => match values::base64(s) {
                Some((bytes, alphabet)) => {
                    let mut lines = vec![StyledLine::styled(format!("{alphabet} · {} bytes", bytes.len()), Tok::Meta)];
                    lines.push(StyledLine::new());
                    lines.extend(byte_lines(&bytes));
                    self.open_decoded("base64".into(), lines);
                }
                None => self.flash("not base64"),
            },
            MenuAction::DecodeUrl(s) => match values::url_decode(s) {
                Some(text) => self.open_decoded("URL-decoded".into(), text.lines().map(StyledLine::plain).collect()),
                None => self.flash("nothing to URL-decode"),
            },
            MenuAction::FilterBy(f) => {
                self.filter_input = tui_input::Input::new(f.clone());
                self.apply_filter(f);
                self.focus = Focus::List;
                self.flash(format!("filter: {f}"));
            }
            _ => return false,
        }
        true
    }

    /// A WebSocket message's whole payload (as captured): JSON pretty-printed, text, or hex.
    pub fn open_ws_message(&mut self, txn: TxnIdx, i: usize) {
        let Some(m) = self.view_store().txn(txn).ws.get(i).cloned() else { return };
        let dir = if m.out { "sent" } else { "received" };
        let mut lines = Vec::new();
        if m.op == "close" {
            lines.push(StyledLine::plain(format!(
                "close {} {}",
                m.code.map(|c| c.to_string()).unwrap_or_default(),
                m.reason.unwrap_or_default()
            )));
        } else {
            if m.truncated {
                lines.push(StyledLine::styled(
                    format!("the first {} of {} bytes (the capture cap)", m.data.len(), m.size),
                    Tok::Meta,
                ));
                lines.push(StyledLine::new());
            }
            lines.extend(byte_lines(&m.data));
        }
        let title = format!("message {} · {dir} {} · {}", i + 1, m.op, traffic_police_core::fmt::bytes(m.size));
        self.open_decoded(title, lines);
    }

    pub fn open_decoded(&mut self, title: String, lines: Vec<StyledLine>) {
        self.decoded = Some(Decoded { title, lines, scroll: Top::default(), page: 10, width: 60 });
        self.overlay = Overlay::Decoded;
    }

    /// The mouse wheel over a decoded value: `n` rows down (up when negative).
    pub fn decoded_wheel(&mut self, n: isize) {
        let wrapping = self.prefs.wrap;
        if let Some(d) = &mut self.decoded {
            d.scroll = d.with_lines(wrapping, |l| l.scroll(d.scroll, n).0);
        }
    }

    pub fn decoded_key(&mut self, k: KeyEvent) {
        let half = self.half_page(self.decoded.as_ref().map_or(0, |d| d.page)) as isize;
        let wrapping = self.prefs.wrap;
        let Some(d) = &mut self.decoded else {
            self.overlay = Overlay::None;
            return;
        };
        let page = d.page.max(1) as isize;
        let by = |d: &mut Decoded, n: isize| d.scroll = d.with_lines(wrapping, |l| l.scroll(d.scroll, n).0);
        match self.keymap.action(&k) {
            Some(Action::Down) => by(d, 1),
            Some(Action::Up) => by(d, -1),
            Some(Action::PageDown) => by(d, page),
            Some(Action::PageUp) => by(d, -page),
            Some(Action::HalfPageDown) => by(d, half),
            Some(Action::HalfPageUp) => by(d, -half),
            Some(Action::Top) => d.scroll = Top::default(),
            Some(Action::Bottom) => d.scroll = d.with_lines(wrapping, |l| l.last_top()),
            Some(Action::Copy) => {
                let text = values::plain_text(&d.lines);
                self.copy_text(text);
            }
            _ => self.overlay = Overlay::None,
        }
    }
}

/// The popup of a decoded value: as wide as its widest line (within the screen), long lines
/// wrapped (`[ui] wrap`).
pub fn draw(app: &mut App, area: Rect, buf: &mut Buffer) {
    let wrapping = app.prefs.wrap;
    let Some(d) = &mut app.decoded else { return };
    let t = &app.theme;
    let widest = d.lines.iter().map(|l| unicode_width::UnicodeWidthStr::width(l.text.as_str())).max().unwrap_or(0);
    let w = (widest as u16 + 6).clamp(50, area.width.saturating_sub(8));
    d.width = w.saturating_sub(4) as usize;
    // as tall as its rows, within the screen
    let most = area.height.saturating_sub(4);
    let mut total = 0usize;
    for i in 0..d.lines.len() {
        total += if wrapping { d.rows(i, d.width.max(1)).len() } else { 1 };
        if total + 4 > usize::from(most) {
            break;
        }
    }
    let h = (total as u16 + 4).min(most);
    let r = crate::ui::centered(area, w, h);
    crate::ui::draw_box(buf, r, &d.title, t);
    let inner = Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height.saturating_sub(3) };
    let rows = inner.height as usize;
    d.page = rows;
    d.scroll = d.with_lines(wrapping, |l| l.clamp(d.scroll));
    let (mut y, mut i, mut skip, mut last) = (0, d.scroll.line, d.scroll.part, d.scroll.line);
    while y < rows && i < d.lines.len() {
        let text = crate::bodyview::text_of(&d.lines[i], Some(t));
        let parts = if wrapping { d.rows(i, d.width.max(1)) } else { vec![text.window(0, inner.width as usize)] };
        for p in parts.iter().skip(skip).take(rows - y) {
            draw_line(buf, inner.x, inner.y + y as u16, inner.width, &text.part(p), 0, Style::default());
            y += 1;
        }
        (last, i, skip) = (i, i + 1, 0);
    }
    let more = if d.scroll.line > 0 || i < d.lines.len() {
        format!("  ·  lines {}–{} of {}", d.scroll.line + 1, last + 1, d.lines.len())
    } else {
        String::new()
    };
    let foot = Line::from(Span::styled(format!("y copies · any other key closes{more}"), t.faint()));
    draw_line(buf, inner.x, r.y + r.height - 2, inner.width, &foot, 0, Style::default());
}
