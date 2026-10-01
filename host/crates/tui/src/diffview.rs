//! The diff view: `d` marks a request, `d` on another compares them (ARCHITECTURE.md §5.10). The
//! comparison comes from [`traffic_police_core::diff`]; this scrolls it in a box over the screen.

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use traffic_police_core::diff::{self, Diff, Kind};
use traffic_police_core::fmt;
use traffic_police_core::model::TxnIdx;

use crate::actions::Action;
use crate::app::{App, Overlay};
use crate::detail::draw_line;
use crate::theme::Theme;
use crate::wrap::{self, Text, Top};

#[derive(Debug, Clone)]
pub struct DiffView {
    pub a: TxnIdx,
    pub b: TxnIdx,
    /// Headers compared sorted, as sets.
    pub sets: bool,
    pub doc: Diff,
    /// The line at the top, and (`[ui] wrap`) how many of its rows are above the box.
    pub scroll: Top,
    /// The box's rows and width last time, for paging and wrapping.
    pub page: usize,
    pub width: usize,
}

impl DiffView {
    /// Where each run of changes starts.
    fn change_starts(&self) -> Vec<usize> {
        let changed = |k: Kind| matches!(k, Kind::Removed | Kind::Added);
        let l = &self.doc.lines;
        (0..l.len()).filter(|&i| changed(l[i].kind) && (i == 0 || !changed(l[i - 1].kind))).collect()
    }

    /// Line `i` as drawn: its mark (`-`, `+`) and text in the line's style, and where its later
    /// rows start (past the mark and the text's indent).
    fn line(&self, i: usize, t: &Theme) -> (Text<'static>, usize) {
        let l = &self.doc.lines[i];
        let (prefix, style) = match l.kind {
            Kind::Heading => ("", t.accent().add_modifier(Modifier::BOLD)),
            Kind::Same => ("  ", t.text()),
            Kind::Removed => ("- ", t.error()),
            Kind::Added => ("+ ", t.ok()),
            Kind::Note => ("  ", t.faint().add_modifier(Modifier::ITALIC)),
            Kind::Gap => ("  ", t.faint()),
        };
        let indent = prefix.len() + (l.text.len() - l.text.trim_start_matches(' ').len()) + 2;
        (Text { text: format!("{prefix}{}", l.text).into(), runs: Vec::new(), base: style }, indent)
    }

    /// Runs `f` over the lines as the box sees them (rows each takes when they wrap).
    fn with_lines<R>(&self, t: &Theme, wrapping: bool, f: impl FnOnce(&mut wrap::Lines) -> R) -> R {
        let width = self.width.max(1);
        let mut rows = |i: usize| {
            if !wrapping {
                return 1;
            }
            let (text, indent) = self.line(i, t);
            wrap::layout(&text.text, width, indent, false).len()
        };
        let mut lines = wrap::Lines { len: self.doc.lines.len(), view: self.page.max(1), rows: &mut rows };
        f(&mut lines)
    }
}

impl App {
    /// `d`: marks the selected request; `d` on another compares the two; `d` on the marked one
    /// clears the mark.
    pub fn diff_action(&mut self) {
        let Some(txn) = self.selected else {
            self.flash("select a request to compare it");
            return;
        };
        let key = self.view_store().txn(txn).key;
        match self.diff_mark {
            None => {
                self.diff_mark = Some(key);
                self.flash("marked for diff (◆) · d on another request compares them");
            }
            Some(k) if k == key => {
                self.diff_mark = None;
                self.flash("diff mark cleared");
            }
            Some(k) => match self.view_store().find(k) {
                Some(a) => {
                    self.diff_mark = None;
                    self.open_diff(a, txn);
                }
                None => {
                    self.diff_mark = Some(key);
                    self.flash("the marked request is gone; marked this one instead");
                }
            },
        }
    }

    pub fn open_diff(&mut self, a: TxnIdx, b: TxnIdx) {
        let doc = diff::diff(self.view_store(), a, b, false);
        self.diff = Some(DiffView { a, b, sets: false, doc, scroll: Top::default(), page: 10, width: 80 });
        self.overlay = Overlay::Diff;
    }

    pub fn diff_key(&mut self, k: KeyEvent) {
        let Some(mut v) = self.diff.take() else {
            self.overlay = Overlay::None;
            return;
        };
        let page = v.page.max(1) as isize;
        let half = self.half_page(v.page) as isize;
        let (theme, wrapping) = (self.theme.clone(), self.prefs.wrap);
        let by = |v: &mut DiffView, n: isize| v.scroll = v.with_lines(&theme, wrapping, |l| l.scroll(v.scroll, n).0);
        match self.keymap.action(&k) {
            Some(Action::Down) => by(&mut v, 1),
            Some(Action::Up) => by(&mut v, -1),
            Some(Action::PageDown) => by(&mut v, page),
            Some(Action::PageUp) => by(&mut v, -page),
            Some(Action::HalfPageDown) => by(&mut v, half),
            Some(Action::HalfPageUp) => by(&mut v, -half),
            Some(Action::Top) => v.scroll = Top::default(),
            Some(Action::Bottom) => v.scroll = v.with_lines(&theme, wrapping, |l| l.last_top()),
            Some(Action::FindNext) => {
                match v.change_starts().into_iter().find(|&s| s.saturating_sub(2) > v.scroll.line) {
                    Some(s) => v.scroll = Top { line: s.saturating_sub(2), part: 0 },
                    None => self.flash("no more changes below"),
                }
            }
            Some(Action::FindPrev) => {
                match v.change_starts().into_iter().rev().find(|&s| s.saturating_sub(2) < v.scroll.line) {
                    Some(s) => v.scroll = Top { line: s.saturating_sub(2), part: 0 },
                    None => self.flash("no more changes above"),
                }
            }
            Some(Action::Sort) => {
                v.sets = !v.sets;
                v.doc = diff::diff(self.view_store(), v.a, v.b, v.sets);
                self.flash(if v.sets {
                    "headers compared as sets (sorted by name)"
                } else {
                    "headers compared in order"
                });
            }
            Some(Action::Copy) => {
                let text = format!("# A: {}\n# B: {}\n\n{}", self.diff_label(v.a), self.diff_label(v.b), v.doc.text());
                self.copy_text(text);
            }
            Some(Action::Back | Action::Diff | Action::Quit) => {
                self.overlay = Overlay::None;
                return;
            }
            _ => {}
        }
        v.scroll = v.with_lines(&theme, wrapping, |l| l.clamp(v.scroll));
        self.diff = Some(v);
    }

    /// The mouse wheel over the comparison: `n` rows down (up when negative).
    pub fn diff_wheel(&mut self, n: isize) {
        let (theme, wrapping) = (self.theme.clone(), self.prefs.wrap);
        if let Some(v) = &mut self.diff {
            v.scroll = v.with_lines(&theme, wrapping, |l| l.scroll(v.scroll, n).0);
        }
    }

    fn diff_label(&self, i: TxnIdx) -> String {
        let t = self.view_store().txn(i);
        let status = t.resp.as_ref().map_or_else(|| t.status_text(), |r| r.status.to_string());
        format!("{} {}  {status}  ({}:{})", t.method, t.url.raw, t.key.source, t.key.txn)
    }
}

/// The diff box, over most of the screen.
pub fn draw(app: &mut App, area: Rect, buf: &mut Buffer) {
    let Some(v) = &app.diff else { return };
    let t = &app.theme;
    // everything between the header and the key hints
    let r = Rect { y: area.y + 1, height: area.height.saturating_sub(2), ..area };
    crate::ui::draw_box(buf, r, "diff", t);
    let inner = Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height.saturating_sub(2) };
    let side = |label: &'static str, i: TxnIdx, style: Style| {
        let tx = app.view_store().txn(i);
        let status = tx.resp.as_ref().map_or_else(|| tx.status_text(), |r| r.status.to_string());
        Line::from(vec![
            Span::styled(format!("{label} "), style.add_modifier(Modifier::BOLD)),
            Span::styled(format!("{} ", tx.method), t.title()),
            Span::styled(tx.url.raw.clone(), t.text()),
            Span::styled(format!("  {status}  {}", fmt::duration(tx.duration(app.now()))), t.dim()),
        ])
    };
    let summary = if v.doc.differing.is_empty() {
        Line::from(Span::styled("no differences", t.ok()))
    } else {
        Line::from(vec![
            Span::styled("differs in ", t.dim()),
            Span::styled(v.doc.differing.join(", "), t.warn()),
            Span::styled(if v.sets { "  · headers compared as sets" } else { "" }, t.faint()),
        ])
    };
    let head = [side("A", v.a, t.error()), side("B", v.b, t.ok()), summary];
    for (i, line) in head.iter().enumerate() {
        draw_line(buf, inner.x, inner.y + i as u16, inner.width, line, 0, Style::default());
    }
    let body = Rect { y: inner.y + 4, height: inner.height.saturating_sub(4), ..inner };
    let (rows, width, wrapping) = (body.height as usize, body.width as usize, app.prefs.wrap);
    // the rows from the top on: each line's rows, wrapped to the box (or cut at its edge)
    let mut y = 0;
    let (mut i, mut skip) = (v.scroll.line, v.scroll.part);
    let mut last = i;
    while y < rows && i < v.doc.lines.len() {
        let (text, indent) = v.line(i, t);
        let parts = if wrapping { wrap::layout(&text.text, width, indent, false) } else { vec![text.window(0, width)] };
        for p in parts.iter().skip(skip).take(rows - y) {
            draw_line(buf, body.x, body.y + y as u16, body.width, &text.part(p), 0, Style::default());
            y += 1;
        }
        (last, i, skip) = (i, i + 1, 0);
    }
    let total = v.doc.lines.len();
    let pos = if v.scroll.line == 0 && i >= total && y <= rows {
        format!(" {total} lines ")
    } else {
        format!(" lines {}–{} of {total} ", v.scroll.line + 1, last + 1)
    };
    let w = pos.chars().count() as u16;
    let line = Line::from(Span::styled(pos, t.faint()));
    draw_line(buf, (r.x + r.width).saturating_sub(w + 2), r.y + r.height - 1, w, &line, 0, Style::default());
    if let Some(v) = &mut app.diff {
        (v.page, v.width) = (rows, width);
    }
}
