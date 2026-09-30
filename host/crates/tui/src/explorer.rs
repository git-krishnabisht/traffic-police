//! The body explorer: the top half of the detail pane shows the response body, and with focus
//! `j`/`k` move through it, `h` folds (or goes to the enclosing object or array), `l` unfolds
//! (or steps into it), Enter folds or opens the value menu on a single value.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use traffic_police_core::fmt;
use traffic_police_core::model::{BodyDir, TxnIdx};

use crate::actions::Action;
use crate::app::{App, Focus, Target};
use crate::bodyview::Window;
use crate::detail::draw_line;

/// Where the explorer is.
#[derive(Debug, Clone, Default)]
pub struct Explorer {
    /// The request and body the cursor belongs to (it starts over for another).
    shown: Option<(TxnIdx, BodyDir)>,
    pub cursor: usize,
    pub scroll: usize,
    /// Rows shown last time, for paging.
    pub height: usize,
}

impl App {
    /// The body the explorer shows: the response as the app received it (or the original, `o`).
    pub fn explorer_body(&self) -> Option<(TxnIdx, BodyDir)> {
        let txn = self.selected?;
        Some((txn, self.response_dir(txn)))
    }

    /// Starts over when the request or body changed.
    fn explorer_sync(&mut self) -> Option<(TxnIdx, BodyDir)> {
        let now = self.explorer_body();
        if self.explorer.shown != now {
            self.explorer = Explorer { shown: now, height: self.explorer.height, ..Explorer::default() };
        }
        now
    }

    pub fn explorer_action(&mut self, a: Action) {
        let Some((txn, dir)) = self.explorer_sync() else { return };
        if a == Action::Back {
            return self.close_detail();
        }
        let page = self.explorer.height.max(2) - 1;
        let cur = self.explorer.cursor;
        let mut value_menu = false;
        let next = {
            let Some(v) = self.body_view(txn, dir) else { return };
            let len = v.len(true);
            let cur = cur.min(len.saturating_sub(1));
            match a {
                Action::Up => cur.saturating_sub(1),
                Action::Down => (cur + 1).min(len.saturating_sub(1)),
                Action::PageUp => cur.saturating_sub(page),
                Action::PageDown => (cur + page).min(len.saturating_sub(1)),
                Action::Top => 0,
                Action::Bottom => len.saturating_sub(1),
                // fold, or go up to the enclosing object or array
                Action::Left => {
                    if v.set_fold(cur, true) {
                        cur
                    } else {
                        v.parent_line(cur).unwrap_or(cur)
                    }
                }
                // unfold, or step into it
                Action::Right => match v.container_at(cur) {
                    Some((_, true)) => {
                        v.set_fold(cur, false);
                        cur
                    }
                    Some((_, false)) if cur + 1 < len => cur + 1,
                    _ => cur,
                },
                Action::Activate => {
                    if !v.toggle_fold(cur) {
                        value_menu = true;
                    }
                    cur
                }
                Action::FoldAll => {
                    v.fold_all();
                    0
                }
                Action::UnfoldAll => {
                    v.unfold_all();
                    cur
                }
                _ => cur,
            }
        };
        self.explorer.cursor = next;
        self.clamp_explorer_scroll();
        if value_menu {
            self.open_value_menu(txn);
        }
    }

    fn clamp_explorer_scroll(&mut self) {
        let h = self.explorer.height.max(1);
        let e = &mut self.explorer;
        if e.cursor < e.scroll {
            e.scroll = e.cursor;
        } else if e.cursor >= e.scroll + h {
            e.scroll = e.cursor + 1 - h;
        }
    }

    /// The mouse wheel over the explorer.
    pub fn explorer_scroll(&mut self, down: bool) {
        let Some((txn, dir)) = self.explorer_sync() else { return };
        let len = self.body_view(txn, dir).map_or(0, |v| v.len(true));
        let e = &mut self.explorer;
        let max = len.saturating_sub(e.height.max(1));
        e.scroll = if down { (e.scroll + 3).min(max) } else { e.scroll.saturating_sub(3) };
        e.cursor = e.cursor.clamp(e.scroll, e.scroll + e.height.saturating_sub(1));
    }

    /// A click on explorer line `i`.
    pub fn explorer_click(&mut self, i: usize, double: bool) {
        self.explorer_sync();
        self.focus = Focus::Preview;
        self.explorer.cursor = i;
        if double {
            self.explorer_action(Action::Activate);
        }
    }
}

/// The explorer's box: the request's name, what it shows, and (top right) the close button on
/// its border.
pub fn draw(app: &mut App, r: Rect, buf: &mut Buffer) {
    let Some((txn, dir)) = app.explorer_sync() else { return };
    let t = app.theme.clone();
    let focused = app.focus == Focus::Preview;
    let inner = crate::ui::panel(buf, r, focused, &t);
    let close = crate::ui::border_labels(buf, r, r.y, true, vec![vec![Span::styled("✕", t.dim())]]);
    if let Some(c) = close.first() {
        app.hits.add(*c, Target::DetailClose);
    }
    let room = close.first().map_or(r, |c| Rect { width: c.x.saturating_sub(r.x), ..r });
    let (name, what) = {
        let tx = app.view_store().txn(txn);
        let what = match dir {
            BodyDir::Delivered => "response body (as delivered)",
            _ if tx.rule_modified() => "response body (original)",
            _ => "response body",
        };
        (tx.url.name(), what)
    };
    let title_style =
        if focused { t.accent().add_modifier(Modifier::BOLD) } else { t.title().add_modifier(Modifier::BOLD) };
    let mut labels = vec![
        vec![Span::styled(crate::ui::truncate(&name, 28), title_style)],
        vec![Span::styled(what, if focused { t.text() } else { t.dim() })],
    ];
    let content = Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner };
    app.explorer.height = content.height as usize;
    let message = |app: &App| -> Option<String> {
        let tx = app.view_store().txn(txn);
        let has_body = tx.resp_body.id.is_some() || tx.delivered_body.is_some();
        if has_body {
            return app.body_decoding(txn, dir).then(|| "decoding…".to_string());
        }
        Some(if let Some(f) = &tx.failure {
            format!("no response · {}: {}", f.short_class(), f.message.clone().unwrap_or_default())
        } else if tx.state.is_open() {
            "waiting for the response…".into()
        } else {
            "no response body".into()
        })
    };
    let note = message(app);
    let Some(view) = app.body_view(txn, dir) else {
        crate::ui::border_labels(buf, room, r.y, false, labels);
        let l = Line::styled(note.unwrap_or_default(), t.dim());
        draw_line(buf, content.x, content.y, content.width, &l, 0, Style::default());
        return;
    };
    let len = view.len(true);
    let kind = format!("{} · {}", view.decoded.kind.label(), fmt::bytes(view.decoded.bytes.len() as u64));
    labels.push(vec![Span::styled(kind, t.faint())]);
    if let Some(img) = view.image().and_then(|i| i.image.clone()) {
        crate::ui::border_labels(buf, room, r.y, false, labels);
        let area = Rect { x: content.x, y: content.y, width: content.width, height: content.height };
        app.images.render(&img, area, buf);
        return;
    }
    let cursor = app.explorer.cursor.min(len.saturating_sub(1));
    app.explorer.cursor = cursor;
    app.clamp_explorer_scroll();
    let scroll = app.explorer.scroll.min(len.saturating_sub(1));
    let Some(view) = app.body_view(txn, dir) else { return };
    let mut rows: Vec<Line<'static>> = Vec::new();
    for i in scroll..(scroll + content.height as usize).min(len) {
        rows.push(view.line(i, true, &t, Window { skip: 0, take: usize::from(content.width) }));
    }
    for (row, l) in rows.iter().enumerate() {
        let y = content.y + row as u16;
        let i = scroll + row;
        let selected = focused && i == cursor;
        let base = if selected { t.selected() } else { Style::default() };
        if selected {
            crate::ui::fill(buf, Rect { x: inner.x, y, width: inner.width, height: 1 }, t.selected());
        }
        draw_line(buf, content.x, y, content.width, l, 0, base);
        app.hits.add(Rect { x: inner.x, y, width: inner.width, height: 1 }, Target::ExplorerLine(i));
    }
    if len > content.height as usize {
        labels.push(vec![Span::styled(format!("{}/{len}", cursor + 1), t.faint())]);
    }
    crate::ui::border_labels(buf, room, r.y, false, labels);
    if focused {
        let hint = vec![Span::styled("h fold · l unfold · Enter value", t.faint())];
        crate::ui::border_labels(buf, r, r.y + r.height.saturating_sub(1), true, vec![hint]);
    }
}
