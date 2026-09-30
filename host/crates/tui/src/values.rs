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

/// A decoded value in a popup.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub title: String,
    pub lines: Vec<StyledLine>,
    pub scroll: usize,
    pub page: usize,
}

/// A filter token for a value: quotes cannot be escaped, so the text stops at one; long values
/// are cut (the filter looks for a part anyway).
fn filter_part(s: &str) -> String {
    s.split('"').next().unwrap_or("").chars().take(80).collect()
}

/// Bytes as lines: text (pretty JSON when it is JSON), or a hex dump.
fn byte_lines(bytes: &[u8]) -> Vec<StyledLine> {
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

    pub fn open_decoded(&mut self, title: String, lines: Vec<StyledLine>) {
        self.decoded = Some(Decoded { title, lines, scroll: 0, page: 10 });
        self.overlay = Overlay::Decoded;
    }

    pub fn decoded_key(&mut self, k: KeyEvent) {
        let Some(d) = &mut self.decoded else {
            self.overlay = Overlay::None;
            return;
        };
        let max = d.lines.len().saturating_sub(d.page.max(1));
        match self.keymap.action(&k) {
            Some(Action::Down) => d.scroll = (d.scroll + 1).min(max),
            Some(Action::Up) => d.scroll = d.scroll.saturating_sub(1),
            Some(Action::PageDown) => d.scroll = (d.scroll + d.page.max(1)).min(max),
            Some(Action::PageUp) => d.scroll = d.scroll.saturating_sub(d.page.max(1)),
            Some(Action::Top) => d.scroll = 0,
            Some(Action::Bottom) => d.scroll = max,
            Some(Action::Copy) => {
                let text = values::plain_text(&d.lines);
                self.copy_text(text);
            }
            _ => self.overlay = Overlay::None,
        }
    }
}

/// The popup of a decoded value.
pub fn draw(app: &mut App, area: Rect, buf: &mut Buffer) {
    let Some(d) = &app.decoded else { return };
    let t = &app.theme;
    let widest = d.lines.iter().map(|l| unicode_width::UnicodeWidthStr::width(l.text.as_str())).max().unwrap_or(0);
    let w = (widest as u16 + 6).clamp(50, area.width.saturating_sub(8));
    let h = (d.lines.len() as u16 + 4).min(area.height.saturating_sub(4));
    let r = crate::ui::centered(area, w, h);
    crate::ui::draw_box(buf, r, &d.title, t);
    let inner = Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height.saturating_sub(3) };
    let rows = inner.height as usize;
    for (i, l) in d.lines.iter().skip(d.scroll).take(rows).enumerate() {
        let line = crate::bodyview::styled(l, t);
        draw_line(buf, inner.x, inner.y + i as u16, inner.width, &line, 0, Style::default());
    }
    let more = if d.lines.len() > rows {
        format!("  ·  lines {}–{} of {}", d.scroll + 1, (d.scroll + rows).min(d.lines.len()), d.lines.len())
    } else {
        String::new()
    };
    let foot = Line::from(Span::styled(format!("y copies · any other key closes{more}"), t.faint()));
    draw_line(buf, inner.x, r.y + r.height - 2, inner.width, &foot, 0, Style::default());
    if let Some(d) = &mut app.decoded {
        d.page = rows;
    }
}
