//! The body explorer: the top of the detail pane shows the response body or, on its second tab
//! (`b`, or a click on the tab), the request body. With focus `j`/`k` move through it, Ctrl+D and
//! Ctrl+U by half the box, `h` folds (or goes to the enclosing object or array), `l` unfolds (or
//! steps into it), Enter folds or opens the value menu on a single value. Long lines wrap
//! (`[ui] wrap`).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use traffic_police_core::fmt;
use traffic_police_core::model::{BodyDir, TxnIdx};

use crate::actions::Action;
use crate::app::{App, Focus, Target};
use crate::detail::draw_line;
use crate::wrap;
use unicode_width::UnicodeWidthStr;

/// Which body the box shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyTab {
    #[default]
    Response,
    Request,
}

impl BodyTab {
    pub const ALL: [BodyTab; 2] = [BodyTab::Response, BodyTab::Request];

    pub fn title(self) -> &'static str {
        match self {
            BodyTab::Response => "Response body",
            BodyTab::Request => "Request body",
        }
    }

    /// `[ui] body`: `response` or `request`.
    pub fn parse(s: &str) -> Option<BodyTab> {
        match s.trim().to_ascii_lowercase().as_str() {
            "response" => Some(BodyTab::Response),
            "request" => Some(BodyTab::Request),
            _ => None,
        }
    }
}

/// Where the explorer is.
#[derive(Debug, Clone, Default)]
pub struct Explorer {
    /// The request and body the cursor belongs to (it starts over for another).
    shown: Option<(TxnIdx, BodyDir)>,
    /// The body shown; it stays as chosen when another request is selected.
    pub tab: BodyTab,
    pub cursor: usize,
    /// The line at the top of the box, and (`[ui] wrap`) how many of its rows are above it.
    pub scroll: usize,
    pub scroll_part: usize,
    /// The box's size last time, for paging and wrapping.
    pub height: usize,
    pub width: usize,
}

impl App {
    /// The body the explorer shows: the request body on its second tab, else the response as
    /// the app received it (or the original, `o`).
    pub fn explorer_body(&self) -> Option<(TxnIdx, BodyDir)> {
        let txn = self.selected?;
        Some(match self.explorer.tab {
            BodyTab::Request => (txn, BodyDir::Request),
            BodyTab::Response => (txn, self.response_dir(txn)),
        })
    }

    /// Starts over when the request or body changed.
    fn explorer_sync(&mut self) -> Option<(TxnIdx, BodyDir)> {
        let now = self.explorer_body();
        if self.explorer.shown != now {
            let e = &self.explorer;
            self.explorer =
                Explorer { shown: now, tab: e.tab, height: e.height, width: e.width, ..Explorer::default() };
        }
        now
    }

    /// `b`: the other body.
    pub fn toggle_body_tab(&mut self) {
        let next = match self.explorer.tab {
            BodyTab::Response => BodyTab::Request,
            BodyTab::Request => BodyTab::Response,
        };
        self.set_body_tab(next);
    }

    pub fn set_body_tab(&mut self, tab: BodyTab) {
        self.explorer.tab = tab;
        self.explorer_sync();
    }

    pub fn explorer_action(&mut self, a: Action) {
        let Some((txn, dir)) = self.explorer_sync() else { return };
        match a {
            Action::Back => return self.close_detail(),
            Action::PageUp | Action::PageDown | Action::HalfPageUp | Action::HalfPageDown => {
                // the view moves by rows (a page, or half the box), and the cursor with it
                let n = if matches!(a, Action::PageUp | Action::PageDown) {
                    self.explorer.height.max(2) - 1
                } else {
                    self.half_page(self.explorer.height)
                };
                let down = matches!(a, Action::PageDown | Action::HalfPageDown);
                let (top, cursor) = (self.explorer_top(), self.explorer.cursor);
                let (top, cursor) = self.with_explorer_lines(txn, dir, |l| l.half_page(top, cursor, n, down));
                self.set_explorer_top(top);
                self.explorer.cursor = cursor;
                return;
            }
            _ => {}
        }
        let cur = self.explorer.cursor;
        let mut value_menu = false;
        let next = {
            let Some(v) = self.body_view(txn, dir) else { return };
            let len = v.len(true);
            let cur = cur.min(len.saturating_sub(1));
            match a {
                Action::Up => cur.saturating_sub(1),
                Action::Down => (cur + 1).min(len.saturating_sub(1)),
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
        if matches!(a, Action::Up | Action::Down | Action::Top | Action::Bottom | Action::FoldAll) {
            let top = self.explorer_top();
            let top = self.with_explorer_lines(txn, dir, |l| l.show(top, next, 0));
            self.set_explorer_top(top);
        }
        if value_menu {
            self.open_value_menu(txn);
        }
    }

    fn explorer_top(&self) -> wrap::Top {
        wrap::Top { line: self.explorer.scroll, part: self.explorer.scroll_part }
    }

    fn set_explorer_top(&mut self, top: wrap::Top) {
        self.explorer.scroll = top.line;
        self.explorer.scroll_part = top.part;
    }

    /// Runs `f` over the body's lines as the box sees them: one row each, or (`[ui] wrap`) the
    /// rows each takes at the box's width.
    fn with_explorer_lines<R>(&mut self, txn: TxnIdx, dir: BodyDir, f: impl FnOnce(&mut wrap::Lines) -> R) -> R {
        let (width, view, wrapping) = (self.explorer.width.max(1), self.explorer.height.max(1), self.prefs.wrap);
        let len = self.body_view(txn, dir).map_or(0, |v| v.len(true));
        let mut rows = |i: usize| match self.body_view(txn, dir) {
            Some(v) if wrapping => v.breaks(i, true, width).len(),
            _ => 1,
        };
        let mut lines = wrap::Lines { len, view, rows: &mut rows };
        f(&mut lines)
    }

    /// The mouse wheel over the explorer: the view moves three rows, the cursor stays on it.
    pub fn explorer_scroll(&mut self, down: bool) {
        let Some((txn, dir)) = self.explorer_sync() else { return };
        let (top, cursor) = (self.explorer_top(), self.explorer.cursor);
        let (top, cursor) = self.with_explorer_lines(txn, dir, |l| {
            let (top, _) = l.scroll(top, if down { 3 } else { -3 });
            (top, l.keep_cursor(top, cursor))
        });
        self.set_explorer_top(top);
        self.explorer.cursor = cursor;
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
    let (name, qualifier) = {
        let tx = app.view_store().txn(txn);
        let qualifier = match dir {
            BodyDir::Delivered => " (as delivered)",
            BodyDir::Response if tx.rule_modified() => " (original)",
            _ => "",
        };
        (tx.url.name(), qualifier)
    };
    let title_style =
        if focused { t.accent().add_modifier(Modifier::BOLD) } else { t.title().add_modifier(Modifier::BOLD) };
    // the request's name, then the two bodies as tabs (as the detail tabs are drawn); the tabs
    // keep their room, and the name gets what is left
    let tabs = |qualified: bool| -> Vec<Vec<Span<'static>>> {
        BodyTab::ALL
            .into_iter()
            .map(|tab| {
                let style = if app.explorer.tab == tab {
                    if focused {
                        t.accent().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    } else {
                        t.text().add_modifier(Modifier::UNDERLINED)
                    }
                } else {
                    t.dim()
                };
                let mut label = vec![Span::styled(tab.title(), style)];
                if qualified && app.explorer.tab == tab && tab == BodyTab::Response && !qualifier.is_empty() {
                    label.push(Span::styled(qualifier, t.dim()));
                }
                label
            })
            .collect()
    };
    let width = |labels: &[Vec<Span<'static>>]| -> usize {
        labels.iter().map(|l| l.iter().map(|s| s.content.width()).sum::<usize>() + 3).sum()
    };
    let room_w = usize::from(room.width).saturating_sub(4);
    let mut tab_labels = tabs(true);
    if width(&tab_labels) + 10 > room_w {
        tab_labels = tabs(false);
    }
    let name_w = room_w.saturating_sub(width(&tab_labels) + 3).min(28);
    let mut labels = Vec::new();
    if name_w >= 6 {
        labels.push(vec![Span::styled(crate::ui::truncate(&name, name_w), title_style)]);
    }
    let named = !labels.is_empty();
    labels.extend(tab_labels);
    let content = Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner };
    app.explorer.height = content.height as usize;
    app.explorer.width = content.width as usize;
    let message = |app: &App| -> Option<String> {
        let tx = app.view_store().txn(txn);
        let has_body = match dir {
            BodyDir::Request => tx.req_body.id.is_some(),
            _ => tx.resp_body.id.is_some() || tx.delivered_body.is_some(),
        };
        if has_body {
            return app.body_decoding(txn, dir).then(|| "decoding…".to_string());
        }
        Some(if dir == BodyDir::Request {
            if tx.req_body.total > 0 {
                format!("request body not captured ({} sent)", fmt::bytes(tx.req_body.total))
            } else {
                "no request body".into()
            }
        } else if let Some(f) = &tx.failure {
            format!("no response · {}: {}", f.short_class(), f.message.clone().unwrap_or_default())
        } else if tx.state.is_open() {
            "waiting for the response…".into()
        } else {
            "no response body".into()
        })
    };
    let note = message(app);
    // the tabs are mouse targets; what the body is goes to the right, when there is room
    let title = |app: &mut App, buf: &mut Buffer, labels: Vec<Vec<Span<'static>>>, info: Option<String>| {
        let at = crate::ui::border_labels(buf, room, r.y, false, labels);
        for (rect, tab) in at.iter().skip(usize::from(named)).zip(BodyTab::ALL) {
            app.hits.add(*rect, Target::ExplorerTab(tab));
        }
        if let Some(info) = info {
            let from = at.last().map_or(room.x, |l| l.x + l.width);
            let rest = Rect { x: from, width: (room.x + room.width).saturating_sub(from), ..room };
            crate::ui::border_labels(buf, rest, r.y, true, vec![vec![Span::styled(info, t.faint())]]);
        }
    };
    let Some(view) = app.body_view(txn, dir) else {
        title(app, buf, labels, None);
        let l = Line::styled(note.unwrap_or_default(), t.dim());
        draw_line(buf, content.x, content.y, content.width, &l, 0, Style::default());
        return;
    };
    let len = view.len(true);
    let mut info = format!("{} · {}", view.decoded.kind.label(), fmt::bytes(view.decoded.bytes.len() as u64));
    if let Some(img) = view.image().and_then(|i| i.image.clone()) {
        title(app, buf, labels, Some(info));
        let area = Rect { x: content.x, y: content.y, width: content.width, height: content.height };
        app.images.render(&img, area, buf);
        return;
    }
    let cursor = app.explorer.cursor.min(len.saturating_sub(1));
    app.explorer.cursor = cursor;
    // the view within the lines as they are now, with the cursor's line on it
    let top = app.explorer_top();
    let top = app.with_explorer_lines(txn, dir, |l| {
        let top = l.clamp(top);
        if cursor >= top.line && l.row_of(top, cursor) < l.view { top } else { l.show(top, cursor, 0) }
    });
    app.set_explorer_top(top);
    let (width, height, wrapping) = (usize::from(content.width), usize::from(content.height), app.prefs.wrap);
    let Some(view) = app.body_view(txn, dir) else { return };
    // the rows on screen: each line's rows from the top on
    let mut rows: Vec<(usize, Line<'static>)> = Vec::new();
    let (mut i, mut skip) = (top.line, top.part);
    while rows.len() < height && i < len {
        if wrapping {
            let left = height - rows.len();
            rows.extend(view.wrapped(i, true, &t, width, skip, left).into_iter().map(|(l, _)| (i, l)));
        } else {
            rows.push((i, view.window(i, true, &t, 0, width).0));
        }
        (i, skip) = (i + 1, 0);
    }
    for (row, (i, l)) in rows.iter().enumerate() {
        let (i, y) = (*i, content.y + row as u16);
        let selected = focused && i == cursor;
        let base = if selected { t.selected() } else { Style::default() };
        if selected {
            crate::ui::fill(buf, Rect { x: inner.x, y, width: inner.width, height: 1 }, t.selected());
        }
        draw_line(buf, content.x, y, content.width, l, 0, base);
        app.hits.add(Rect { x: inner.x, y, width: inner.width, height: 1 }, Target::ExplorerLine(i));
    }
    if len > content.height as usize {
        info.push_str(&format!(" · {}/{len}", cursor + 1));
    }
    title(app, buf, labels, Some(info));
    if focused {
        let other = app.keymap.key_label(Action::BodyTab);
        let hint = vec![Span::styled(format!("{other} other body · h fold · l unfold · Enter value"), t.faint())];
        crate::ui::border_labels(buf, r, r.y + r.height.saturating_sub(1), true, vec![hint]);
    }
}
