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

#[derive(Debug, Clone)]
pub struct DiffView {
    pub a: TxnIdx,
    pub b: TxnIdx,
    /// Headers compared sorted, as sets.
    pub sets: bool,
    pub doc: Diff,
    pub scroll: usize,
    /// Rows shown last time, for paging.
    pub page: usize,
}

impl DiffView {
    /// Where each run of changes starts.
    fn change_starts(&self) -> Vec<usize> {
        let changed = |k: Kind| matches!(k, Kind::Removed | Kind::Added);
        let l = &self.doc.lines;
        (0..l.len()).filter(|&i| changed(l[i].kind) && (i == 0 || !changed(l[i - 1].kind))).collect()
    }

    pub(crate) fn max_scroll(&self) -> usize {
        self.doc.lines.len().saturating_sub(self.page.max(1))
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
        self.diff = Some(DiffView { a, b, sets: false, doc, scroll: 0, page: 10 });
        self.overlay = Overlay::Diff;
    }

    pub fn diff_key(&mut self, k: KeyEvent) {
        let Some(mut v) = self.diff.take() else {
            self.overlay = Overlay::None;
            return;
        };
        let page = v.page.max(1);
        match self.keymap.action(&k) {
            Some(Action::Down) => v.scroll += 1,
            Some(Action::Up) => v.scroll = v.scroll.saturating_sub(1),
            Some(Action::PageDown) => v.scroll += page,
            Some(Action::PageUp) => v.scroll = v.scroll.saturating_sub(page),
            Some(Action::Top) => v.scroll = 0,
            Some(Action::Bottom) => v.scroll = v.max_scroll(),
            Some(Action::FindNext) => match v.change_starts().into_iter().find(|&s| s.saturating_sub(2) > v.scroll) {
                Some(s) => v.scroll = s.saturating_sub(2),
                None => self.flash("no more changes below"),
            },
            Some(Action::FindPrev) => {
                match v.change_starts().into_iter().rev().find(|&s| s.saturating_sub(2) < v.scroll) {
                    Some(s) => v.scroll = s.saturating_sub(2),
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
        v.scroll = v.scroll.min(v.max_scroll());
        self.diff = Some(v);
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
    let rows = body.height as usize;
    for (i, l) in v.doc.lines.iter().skip(v.scroll).take(rows).enumerate() {
        let (prefix, style) = match l.kind {
            Kind::Heading => ("", t.accent().add_modifier(Modifier::BOLD)),
            Kind::Same => ("  ", t.text()),
            Kind::Removed => ("- ", t.error()),
            Kind::Added => ("+ ", t.ok()),
            Kind::Note => ("  ", t.faint().add_modifier(Modifier::ITALIC)),
            Kind::Gap => ("  ", t.faint()),
        };
        let line = Line::from(Span::styled(format!("{prefix}{}", l.text), style));
        draw_line(buf, body.x, body.y + i as u16, body.width, &line, 0, Style::default());
    }
    let total = v.doc.lines.len();
    let pos = if total <= rows {
        format!(" {total} lines ")
    } else {
        format!(" lines {}–{} of {total} ", v.scroll + 1, (v.scroll + rows).min(total))
    };
    let w = pos.chars().count() as u16;
    let line = Line::from(Span::styled(pos, t.faint()));
    draw_line(buf, (r.x + r.width).saturating_sub(w + 2), r.y + r.height - 1, w, &line, 0, Style::default());
    if let Some(v) = &mut app.diff {
        v.page = rows;
        v.scroll = v.scroll.min(v.max_scroll());
    }
}
