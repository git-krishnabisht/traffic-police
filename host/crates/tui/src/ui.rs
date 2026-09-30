//! Drawing: layout and every widget except the detail tabs (ARCHITECTURE.md §5.8).

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph, Widget, Wrap};
use traffic_police_core::backend::ConnectionStatus;
use traffic_police_core::event::MarkerKind;
use traffic_police_core::fmt::{self, NS_PER_MS, NS_PER_SEC, Ts};
use traffic_police_core::model::{Transaction, TxnIdx};
use traffic_police_core::phases::Segments;
use traffic_police_core::rows::{Column, Row};
use traffic_police_core::store::SessionStore;
use unicode_width::UnicodeWidthStr;

use crate::actions::Action;
use crate::app::{App, Bar, Focus, Overlay, Tab, Target, View};
use crate::detail::{self, draw_line};
use crate::graph::{self, GraphStyle};
use crate::theme::Theme;

pub const MIN_WIDTH: u16 = 100;
pub const MIN_HEIGHT: u16 = 30;
/// Below this width the detail pane covers the list instead of sitting beside it.
pub const SIDE_BY_SIDE_WIDTH: u16 = 140;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    app.area = area;
    app.hits.clear();
    app.refresh();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        too_small(app, area, f.buffer_mut());
        return;
    }
    // header · Network box · Requests (and Detail) boxes · key hints
    let graph_h = (area.height / 4).clamp(8, 14);
    let header = Rect { height: 1, ..area };
    let network = Rect { y: area.y + 1, height: graph_h + 2, ..area };
    let footer = Rect { y: area.y + area.height - 1, height: 1, ..area };
    let main = Rect { y: network.y + network.height, height: area.height.saturating_sub(network.height + 2), ..area };
    let buf = f.buffer_mut();
    draw_header(app, header, buf);
    draw_graph(app, network, buf);
    draw_main(app, main, buf);
    draw_footer(app, footer, buf);
    match app.overlay {
        Overlay::Help => draw_help(app, area, buf),
        Overlay::Columns { cursor } => draw_columns_menu(app, area, buf, cursor),
        Overlay::ConfirmClear => draw_confirm(app, area, buf),
        Overlay::Jq => draw_jq(f, app, footer),
        Overlay::None => {}
    }
}

/// A rounded box, its border highlighted when the panel has focus. Returns the inside.
fn panel(buf: &mut Buffer, r: Rect, focused: bool, t: &Theme) -> Rect {
    let style = if focused { t.accent() } else { t.faint() };
    ratatui::widgets::Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(style)
        .render(r, buf);
    Rect { x: r.x + 1, y: r.y + 1, width: r.width.saturating_sub(2), height: r.height.saturating_sub(2) }
}

/// Labels on a panel's border: left-aligned after the corner (`╭─ label ─`), or right-aligned
/// before the other corner. Each label gets a space on both sides; labels that do not fit are
/// dropped. Returns where each label went, for mouse targets.
fn border_labels(buf: &mut Buffer, r: Rect, y: u16, right: bool, labels: Vec<Vec<Span<'_>>>) -> Vec<Rect> {
    let widths: Vec<u16> = labels.iter().map(|l| l.iter().map(|s| s.content.width() as u16).sum::<u16>() + 2).collect();
    let room = r.width.saturating_sub(4);
    let mut used = 0u16;
    let mut n = 0;
    for w in &widths {
        if used + w + u16::from(n > 0) > room {
            break;
        }
        used += w + u16::from(n > 0);
        n += 1;
    }
    let mut x = if right { r.x + r.width - 2 - used } else { r.x + 2 };
    let mut out = Vec::with_capacity(n);
    for (i, spans) in labels.into_iter().take(n).enumerate() {
        let w = widths[i];
        let mut padded = vec![Span::raw(" ")];
        padded.extend(spans);
        padded.push(Span::raw(" "));
        text(buf, x, y, w, padded);
        out.push(Rect { x, y, width: w, height: 1 });
        x += w + 1;
    }
    out
}

fn fill(buf: &mut Buffer, r: Rect, style: Style) {
    for y in r.y..r.y + r.height {
        for x in r.x..r.x + r.width {
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_symbol(" ").set_style(style);
            }
        }
    }
}

fn text(buf: &mut Buffer, x: u16, y: u16, width: u16, spans: Vec<Span<'_>>) {
    draw_line(buf, x, y, width, &Line::from(spans), 0, Style::default());
}

fn right_text(buf: &mut Buffer, r: Rect, y: u16, spans: Vec<Span<'_>>) {
    let w: u16 = spans.iter().map(|s| s.content.width() as u16).sum();
    let x = (r.x + r.width).saturating_sub(w + 1).max(r.x);
    text(buf, x, y, w, spans);
}

fn too_small(app: &App, area: Rect, buf: &mut Buffer) {
    let msg = format!(
        "traffic-police needs a terminal of at least {MIN_WIDTH}×{MIN_HEIGHT} (this one is {}×{})",
        area.width, area.height
    );
    let w = (msg.width() as u16).min(area.width);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + area.height / 2;
    text(buf, x, y, w, vec![Span::styled(msg, app.theme.warn())]);
}

// --- header ------------------------------------------------------------------------------------

fn draw_header(app: &App, r: Rect, buf: &mut Buffer) {
    let t = &app.theme;
    let store = app.view_store();
    let mut spans = vec![Span::styled(" traffic-police ", t.title().add_modifier(Modifier::REVERSED))];
    if store.current_source().is_some() || app.connection.is_none() {
        spans.push(Span::raw(" "));
    }
    match store.current_source() {
        Some(s) => {
            spans.push(Span::styled(s.device_label.clone(), t.text()));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(s.process.clone(), t.title()));
            spans.push(Span::styled(format!("  pid {}", s.pid), t.dim()));
            if s.mode == "attach" {
                spans.push(Span::styled("  attach", t.dim()));
            }
        }
        None if app.connection.is_none() => spans.push(Span::styled("waiting for the app…", t.dim())),
        None => {}
    }
    spans.push(Span::raw("  "));
    let waiting = store.current_source().is_none();
    let detached = store.current_source().is_some_and(|s| s.ended.is_some());
    // a live backend's own account of the connection comes first
    let (label, style, note) = match &app.connection {
        _ if app.is_frozen() => ("FROZEN", t.accent(), None),
        Some(ConnectionStatus::Failed(m)) => ("FAILED", t.error(), Some(m)),
        Some(ConnectionStatus::Waiting(m)) => ("WAITING", t.warn(), Some(m)),
        Some(ConnectionStatus::Detached(m)) => ("DETACHED", t.error(), Some(m)),
        _ if waiting => ("WAITING", t.dim(), None),
        _ if detached => ("DETACHED", t.error(), None),
        _ if !app.recording => ("PAUSED", t.warn(), None),
        _ => ("LIVE", t.ok(), None),
    };
    spans.push(Span::styled(format!("● {label}"), style.add_modifier(Modifier::BOLD)));
    if let Some(note) = note {
        spans.push(Span::styled(format!("  {note}"), t.dim()));
    }
    if let Some(rules) = &app.rules {
        let n = rules.rules.iter().filter(|r| r.enabled).count();
        if n > 0 {
            spans.push(Span::styled(format!("  ✎ {n} rule{} active", if n == 1 { "" } else { "s" }), t.marker()));
        }
    }
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    text(buf, r.x, r.y, r.width, spans);
    // totals, as far as they fit
    let st = store.stats();
    let mut totals = vec![
        Span::styled(format!("{} requests", store.len()), t.text()),
        Span::styled(format!(" · {} in", fmt::bytes(st.bytes_in)), t.dim()),
        Span::styled(format!(" · {} out", fmt::bytes(st.bytes_out)), t.dim()),
    ];
    if st.failed > 0 {
        totals.push(Span::styled(format!(" · {} failed", st.failed), t.error()));
    }
    if st.dropped_events > 0 {
        totals.push(Span::styled(format!(" · {} dropped", st.dropped_events), t.warn()));
    }
    let width_of = |v: &[Span]| v.iter().map(|s| s.content.width()).sum::<usize>();
    while !totals.is_empty() && used + width_of(&totals) + 3 > r.width as usize {
        totals.pop();
    }
    right_text(buf, r, r.y, totals);
}

// --- graph -------------------------------------------------------------------------------------

const GUTTER: u16 = 10;

fn tick_step(span: u64, width: u16) -> u64 {
    const STEPS: [u64; 17] = [
        100 * NS_PER_MS,
        200 * NS_PER_MS,
        500 * NS_PER_MS,
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
        900 * NS_PER_SEC,
        1800 * NS_PER_SEC,
        3600 * NS_PER_SEC,
        4 * 3600 * NS_PER_SEC,
    ];
    let min_gap = 16.0;
    STEPS
        .iter()
        .copied()
        .find(|&s| f64::from(width) * s as f64 / span.max(1) as f64 >= min_gap)
        .unwrap_or(*STEPS.last().expect("non-empty"))
}

/// `mm:ss.mmm` since the session started, or the wall-clock time of day (`t` toggles).
fn time_label(app: &App, ts: Ts) -> String {
    let store = app.view_store();
    if app.wall_labels
        && let Some(w) = store.wall_ms(ts)
    {
        return fmt::wall_clock(w);
    }
    fmt::offset(ts.saturating_sub(store.origin()))
}

/// Draw time-axis labels for `[left, right)` across `r` (one row).
fn draw_axis(app: &App, r: Rect, left: Ts, right: Ts, buf: &mut Buffer) {
    let span = right.saturating_sub(left).max(1);
    let tick = tick_step(span, r.width);
    let origin = app.view_store().origin();
    let first = left.saturating_sub(origin).div_ceil(tick) * tick + origin;
    let mut t = first;
    let mut last_end = r.x;
    while t < right {
        let x = r.x + ((t - left) as f64 / span as f64 * f64::from(r.width)) as u16;
        let label = time_label(app, t);
        let w = label.width() as u16;
        if x >= last_end && x + w <= r.x + r.width {
            text(buf, x, r.y, w, vec![Span::styled(label, app.theme.dim())]);
            last_end = x + w + 2;
        }
        t += tick;
    }
}

fn draw_graph(app: &mut App, panel_r: Rect, buf: &mut Buffer) {
    let t = app.theme.clone();
    let focused = app.focus == Focus::Graph;
    let r = panel(buf, panel_r, focused, &t);
    let (left, right) = app.window();
    let plot =
        Rect { x: r.x + GUTTER, y: r.y, width: r.width.saturating_sub(GUTTER + 1), height: r.height.saturating_sub(1) };
    // braille resolves two samples per cell; the other styles one per column
    let per_cell = if app.graph_style == GraphStyle::Braille { 2 } else { 1 };
    let n = (plot.width as usize * per_cell).max(2);
    // Columns are fixed slices of time and the window ends at the last complete one: a moving
    // window would re-slice every frame (values jitter) and the newest slice would dip while it
    // fills. The graph lags "now" by at most one column.
    let bucket_ns = (right.saturating_sub(left) / n as u64).max(1);
    let right = right / bucket_ns * bucket_ns;
    let left = right.saturating_sub(bucket_ns * n as u64);
    let b = app.view_store().traffic().buckets(app.graph_source, left, right, n);
    let max = b.rx.iter().chain(b.tx.iter()).copied().fold(0.0f64, f64::max);
    let ymax = fmt::nice_rate_ceil(max.max(1024.0));
    // border: title and source on the left, the legend on the right, notes at the bottom
    let title_style =
        if focused { t.accent().add_modifier(Modifier::BOLD) } else { t.title().add_modifier(Modifier::BOLD) };
    let src = if b.source == app.graph_source {
        app.graph_source.label().to_string()
    } else {
        format!("{} (whole-app data not available)", b.source.label())
    };
    let mut notes: Vec<Vec<Span>> = Vec::new();
    if !app.is_live() {
        notes.push(vec![Span::styled("⏸ not following live · L", t.warn())]);
    }
    if let Some(c) = app.graph.cursor.filter(|_| focused) {
        notes.push(vec![Span::styled(format!("cursor {}", time_label(app, c)), t.accent())]);
    }
    if let Some((a, z)) = app.graph.selection {
        notes.push(vec![Span::styled(
            format!("range {}–{} · Esc clears", time_label(app, a), time_label(app, z)),
            t.accent(),
        )]);
    }
    let last = |v: &[f64]| {
        let k = (v.len() / 20).max(1);
        v[v.len().saturating_sub(k)..].iter().sum::<f64>() / k as f64
    };
    let (rx_now, tx_now) = if app.is_live() { (last(&b.rx), last(&b.tx)) } else { (0.0, 0.0) };
    let legend = vec![
        Span::styled("━ ", Style::default().fg(t.recv()).add_modifier(Modifier::BOLD)),
        Span::styled(format!("Receiving {}", if app.is_live() { fmt::rate(rx_now) } else { "—".into() }), t.text()),
        Span::raw("   "),
        Span::styled("━ ", Style::default().fg(t.send()).add_modifier(Modifier::BOLD)),
        Span::styled(format!("Sending {}", if app.is_live() { fmt::rate(tx_now) } else { "—".into() }), t.text()),
    ];
    // the legend first; the title keeps the rest of the top border
    let legend_at = border_labels(buf, panel_r, panel_r.y, true, vec![legend]);
    let title_room = legend_at.first().map_or(panel_r, |l| Rect { width: l.x.saturating_sub(panel_r.x), ..panel_r });
    let title = vec![Span::styled("Network", title_style), Span::styled(format!(" · {src}"), t.dim())];
    if border_labels(buf, title_room, panel_r.y, false, vec![title]).is_empty() {
        border_labels(buf, title_room, panel_r.y, false, vec![vec![Span::styled("Network", title_style)]]);
    }
    border_labels(buf, panel_r, panel_r.y + panel_r.height - 1, false, notes);
    // selection / in-progress range shading
    let x_of = |ts: Ts| -> u16 {
        let span = right.saturating_sub(left).max(1);
        plot.x + ((ts.clamp(left, right) - left) as f64 / span as f64 * f64::from(plot.width)) as u16
    };
    let shade = app.graph.selection.or(match (app.graph.anchor, app.graph.cursor) {
        (Some(a), Some(c)) => Some((a.min(c), a.max(c))),
        _ => None,
    });
    if let (Some((a, z)), Some(bg)) = (shade, t.graph_selection_bg()) {
        let (x1, x2) = (x_of(a), x_of(z).max(x_of(a) + 1));
        for y in plot.y..plot.y + plot.height {
            for x in x1..x2.min(plot.x + plot.width) {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_bg(bg);
                }
            }
        }
    }
    // chart
    // no line before the session started or after "now"
    let origin = app.view_store().origin();
    let now = app.now();
    let bucket_ns = right.saturating_sub(left) as f64 / n as f64;
    let shown = |i: usize| {
        let t = left as f64 + (i as f64 + 0.5) * bucket_ns;
        t >= origin as f64 && t <= now as f64
    };
    let points = |v: &[f64]| -> Vec<(f64, f64)> {
        v.iter().enumerate().filter(|&(i, _)| shown(i)).map(|(i, v)| (i as f64 + 0.5, *v)).collect()
    };
    let columns =
        |v: &[f64]| -> Vec<Option<f64>> { v.iter().enumerate().map(|(i, v)| shown(i).then_some(*v)).collect() };
    let send_style = Style::default().fg(t.send()).add_modifier(Modifier::BOLD);
    let recv_style = Style::default().fg(t.recv()).add_modifier(Modifier::BOLD);
    match app.graph_style {
        GraphStyle::Braille => {
            let (rx, tx) = (points(&b.rx), points(&b.tx));
            let chart = Chart::new(vec![
                Dataset::default()
                    .marker(Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(t.send()))
                    .data(&tx),
                Dataset::default()
                    .marker(Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(t.recv()))
                    .data(&rx),
            ])
            .x_axis(Axis::default().bounds([0.0, n as f64]))
            .y_axis(Axis::default().bounds([0.0, ymax]));
            chart.render(plot, buf);
        }
        GraphStyle::Heavy | GraphStyle::Lines => {
            let heavy = app.graph_style == GraphStyle::Heavy;
            graph::draw_steps(buf, plot, &columns(&b.tx), ymax, send_style, heavy);
            graph::draw_steps(buf, plot, &columns(&b.rx), ymax, recv_style, heavy);
        }
        GraphStyle::Area => {
            graph::draw_area(buf, plot, &columns(&b.rx), ymax, Style::default().fg(t.recv()));
            graph::draw_steps(buf, plot, &columns(&b.tx), ymax, send_style, false);
        }
    }
    // markers (attach, detach, pause, ...)
    for m in app.view_store().markers().iter().filter(|m| m.at >= left && m.at < right) {
        let x = x_of(m.at);
        let color = match m.kind {
            MarkerKind::Detach => Some(t.error()),
            MarkerKind::Attach | MarkerKind::Reattach => Some(t.ok()),
            MarkerKind::Pause | MarkerKind::Resume => Some(t.warn()),
            MarkerKind::Note => Some(t.faint()),
        };
        for y in plot.y..plot.y + plot.height {
            if let Some(c) = buf.cell_mut((x, y))
                && c.symbol() == " "
            {
                c.set_symbol("┊").set_style(color.unwrap_or_default());
            }
        }
    }
    if let (Some(c), true) = (app.graph.cursor, focused) {
        let x = x_of(c);
        for y in plot.y..plot.y + plot.height {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_symbol("│").set_style(t.accent());
            }
        }
    }
    // y labels
    let gutter = |y: u16, s: String| {
        let w = s.width() as u16;
        let x = r.x + GUTTER.saturating_sub(w + 1);
        (x, y, w, s)
    };
    for (x, y, w, s) in [
        gutter(plot.y, fmt::rate(ymax)),
        gutter(plot.y + plot.height / 2, fmt::rate(ymax / 2.0)),
        gutter(plot.y + plot.height.saturating_sub(1), "0".into()),
    ] {
        text(buf, x, y, w, vec![Span::styled(s, t.faint())]);
    }
    draw_axis(
        app,
        Rect { x: plot.x, y: r.y + r.height.saturating_sub(1), width: plot.width, height: 1 },
        left,
        right,
        buf,
    );
    app.hits.add(plot, Target::Graph);
}

// --- timeline bars (Connection View and Thread View) ---------------------------------------------

const LEFT_BLOCKS: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

/// Draw one request's bar in the one-row `area`, which shows the time window `(left, right)`.
pub fn draw_bar(buf: &mut Buffer, area: Rect, (left, right): (Ts, Ts), seg: Segments, theme: &Theme, highlight: bool) {
    let (x0, y, width) = (area.x, area.y, area.width);
    if width == 0 || seg.end < left || seg.start >= right {
        return;
    }
    let total = u64::from(width) * 8;
    let span = right.saturating_sub(left).max(1) as f64;
    let pos = |ts: Ts| -> u64 { ((ts.clamp(left, right) - left) as f64 / span * total as f64).round() as u64 };
    let s = pos(seg.start);
    let mut e = pos(seg.end).max(s + 1).min(total);
    if e <= s {
        e = (s + 1).min(total);
    }
    let a = pos(seg.sent).clamp(s, e);
    let b = seg.first_byte.map_or(e, pos).clamp(a, e);
    let colors = [(s, a, theme.send()), (a, b, theme.wait()), (b, e, theme.recv())];
    let first_cell = (s / 8) as u16;
    let last_cell = (e.saturating_sub(1) / 8) as u16;
    for c in first_cell..=last_cell.min(width - 1) {
        let (lo, hi) = (u64::from(c) * 8, u64::from(c) * 8 + 8);
        let parts: Vec<(u64, u64, Color)> = colors
            .iter()
            .filter(|&&(f, t, _)| t > lo && f < hi && t > f)
            .map(|&(f, t, col)| (f.max(lo) - lo, t.min(hi) - lo, col))
            .collect();
        let Some(cell) = buf.cell_mut((x0 + c, y)) else { continue };
        if theme.mono() {
            cell.set_symbol(if highlight { "█" } else { "▆" });
            continue;
        }
        let (sym, fg, bg) = match parts.as_slice() {
            [] => continue,
            [(f, t, col)] => {
                let covered = t - f;
                if *f == 0 && *t == 8 {
                    ("█", *col, None)
                } else if *f == 0 {
                    (LEFT_BLOCKS[(*t as usize).clamp(1, 8) - 1], *col, None)
                } else if covered >= 6 {
                    ("█", *col, None)
                } else if *t == 8 && covered >= 3 {
                    ("▐", *col, None)
                } else if *t == 8 {
                    ("▕", *col, None)
                } else if covered >= 4 {
                    ("▌", *col, None)
                } else {
                    ("▏", *col, None)
                }
            }
            [(0, t1, c1), (_, 8, c2)] => (LEFT_BLOCKS[(*t1 as usize).clamp(1, 8) - 1], *c1, Some(*c2)),
            many => {
                let dominant = many.iter().max_by_key(|(f, t, _)| t - f).map(|&(_, _, c)| c).unwrap_or(theme.recv());
                ("█", dominant, None)
            }
        };
        cell.set_symbol(sym).set_fg(fg);
        if let Some(bg) = bg {
            cell.set_bg(bg);
        }
        if highlight {
            cell.modifier.insert(Modifier::BOLD);
        }
    }
}

// --- main area ---------------------------------------------------------------------------------

fn draw_main(app: &mut App, r: Rect, buf: &mut Buffer) {
    let side_by_side = r.width >= SIDE_BY_SIDE_WIDTH;
    if app.detail_open && app.selected.is_some() {
        if side_by_side {
            let lw = (u32::from(r.width) * u32::from(app.split_pct) / 100) as u16;
            let left = Rect { width: lw, ..r };
            let right = Rect { x: r.x + lw, width: r.width.saturating_sub(lw), ..r };
            draw_views(app, left, buf);
            draw_detail_pane(app, right, buf);
            // the two borders where the boxes meet: drag them to resize
            app.hits.add(
                Rect { x: r.x + lw.saturating_sub(1), width: 2, y: r.y + 1, height: r.height.saturating_sub(2) },
                Target::Divider,
            );
        } else {
            draw_detail_pane(app, r, buf);
        }
    } else {
        draw_views(app, r, buf);
    }
}

/// The views' box: the view tabs and counts on its border, the view inside.
fn draw_views(app: &mut App, r: Rect, buf: &mut Buffer) {
    let t = app.theme.clone();
    let focused = app.focus == Focus::List;
    let inner = panel(buf, r, focused, &t);
    let (count, sort, collapse) = {
        let rows = app.view_rows();
        (rows.len(), rows.sort, rows.collapse)
    };
    let title_style =
        if focused { t.accent().add_modifier(Modifier::BOLD) } else { t.title().add_modifier(Modifier::BOLD) };
    let mut labels = vec![vec![Span::styled("Requests", title_style), Span::styled(format!(" {count}"), t.dim())]];
    let views = [(View::Connections, "Connection"), (View::Threads, "Thread"), (View::Rules, "Rules")];
    for (v, name) in views {
        let style = if app.view == v {
            if focused {
                t.accent().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                t.text().add_modifier(Modifier::UNDERLINED)
            }
        } else {
            t.dim()
        };
        labels.push(vec![Span::styled(name, style)]);
    }
    let at = border_labels(buf, r, r.y, false, labels);
    for (rect, (v, _)) in at.iter().skip(1).zip(views) {
        app.hits.add(*rect, Target::ViewTab(v));
    }
    if app.view == View::Connections {
        let mut info = Vec::new();
        if sort != Default::default() {
            let what = sort.column.title().to_lowercase();
            info.push(Span::styled(format!("by {what}{}", if sort.descending { " ↓" } else { " ↑" }), t.dim()));
        }
        if collapse {
            info.push(Span::styled(
                if info.is_empty() { "repeats collapsed" } else { " · repeats collapsed" },
                t.dim(),
            ));
        }
        let used = at.last().map_or(r.x, |l| l.x + l.width);
        if !info.is_empty() {
            border_labels(
                buf,
                Rect { x: used, width: (r.x + r.width).saturating_sub(used), ..r },
                r.y,
                true,
                vec![info],
            );
        }
    }
    // a column of air between the border and the text
    let inner = Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner };
    match app.view {
        View::Connections => draw_connections(app, inner, buf),
        View::Threads => draw_threads(app, inner, buf),
        View::Rules => draw_rules(app, inner, buf),
    }
}

fn col_width(c: Column) -> u16 {
    match c {
        Column::Size | Column::ReqSize => 9,
        Column::Type => 9,
        Column::Status => 9,
        Column::Time => 9,
        Column::Method => 7,
        Column::Host => 22,
        Column::Path => 26,
        Column::Thread => 26,
        Column::Start => 10,
        Column::Protocol => 9,
        Column::Client => 16,
        Column::Name | Column::Timeline => 0,
    }
}

fn truncate(s: &str, w: usize) -> String {
    if s.width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

fn cell_text(app: &App, t: &Transaction, c: Column, now: Ts) -> (String, Style) {
    let th = &app.theme;
    let origin = app.view_store().origin();
    match c {
        Column::Name => (t.url.name(), th.text()),
        Column::Size => {
            if t.resp.is_some() || t.resp_body.total > 0 {
                (fmt::bytes(t.response_size()), th.text())
            } else {
                (String::new(), th.dim())
            }
        }
        Column::Type => (t.type_label(), th.text()),
        Column::Status => (t.status_text(), th.status(t.status_class())),
        Column::Time => (fmt::duration(t.duration(now)), if t.state.is_open() { th.dim() } else { th.text() }),
        Column::Method => (t.method.clone(), th.text()),
        Column::Host => (t.url.host.clone(), th.text()),
        Column::Path => (t.url.path.clone(), th.text()),
        Column::Thread => (t.thread.as_ref().map(|x| x.name.clone()).unwrap_or_default(), th.text()),
        Column::Start => (fmt::offset(t.start.saturating_sub(origin)), th.dim()),
        Column::ReqSize => (if t.req_body.total > 0 { fmt::bytes(t.req_body.total) } else { String::new() }, th.text()),
        Column::Protocol => (t.resp.as_ref().and_then(|r| r.protocol.clone()).unwrap_or_default(), th.dim()),
        Column::Client => (t.client.as_ref().map(|c| c.label()).unwrap_or_default(), th.dim()),
        Column::Timeline => (String::new(), th.text()),
    }
}

fn right_aligned(c: Column) -> bool {
    matches!(c, Column::Size | Column::ReqSize | Column::Time)
}

/// Where each list column goes: `(column, x, width)`, clipped to the pane. The last cell is
/// kept for the scroll indicator. The Timeline column is dropped when the pane is too narrow for
/// it; Name keeps at least 12 cells and the rightmost columns are cut first.
fn list_layout(columns: &[Column], r: Rect) -> Vec<(Column, u16, u16)> {
    const MIN_NAME: u16 = 12;
    const MIN_TIMELINE: u16 = 10;
    let avail = r.width.saturating_sub(1);
    let fixed = |cs: &[Column]| cs.iter().map(|&c| col_width(c)).sum::<u16>() + cs.len().saturating_sub(1) as u16;
    let mut cols = columns.to_vec();
    let mut flexible = avail.saturating_sub(fixed(&cols));
    let mut timeline_w = if cols.contains(&Column::Timeline) { flexible * 45 / 100 } else { 0 };
    if cols.contains(&Column::Timeline) && (timeline_w < MIN_TIMELINE || flexible - timeline_w < MIN_NAME) {
        cols.retain(|&c| c != Column::Timeline);
        flexible = avail.saturating_sub(fixed(&cols));
        timeline_w = 0;
    }
    let name_w = (flexible - timeline_w).max(MIN_NAME);
    let end = r.x + avail;
    let mut out = Vec::with_capacity(cols.len());
    let mut x = r.x;
    for c in cols {
        if x >= end {
            break;
        }
        let w = match c {
            Column::Name => name_w,
            Column::Timeline => timeline_w,
            other => col_width(other),
        }
        .min(end - x);
        out.push((c, x, w));
        x += w + 1;
    }
    out
}

fn draw_connections(app: &mut App, r: Rect, buf: &mut Buffer) {
    let t = app.theme.clone();
    let now = app.now();
    let (left, right) = app.list_window();
    let rows_len = app.view_rows().len();

    let layout = list_layout(&app.columns, r);
    // header
    for &(c, x, w) in &layout {
        let sort = app.view_rows().sort;
        let arrow = if sort.column == c && (sort != Default::default() || c == Column::Timeline) {
            if sort.descending { " ↓" } else { " ↑" }
        } else {
            ""
        };
        let title = if c == Column::Timeline && sort == Default::default() {
            format!("{}{}", c.title(), "")
        } else {
            format!("{}{arrow}", c.title())
        };
        let s = if right_aligned(c) { format!("{title:>w$}", w = w as usize) } else { truncate(&title, w as usize) };
        text(buf, x, r.y, w, vec![Span::styled(s, t.dim().add_modifier(Modifier::BOLD))]);
        app.hits.add(Rect { x, y: r.y, width: w, height: 1 }, Target::ListHeader(c));
    }
    let body = Rect { y: r.y + 1, height: r.height.saturating_sub(1), ..r };
    if app.list_height != body.height as usize {
        app.list_height = body.height as usize;
        app.clamp_list_offset();
    }
    app.hits.add(body, Target::List);
    if rows_len == 0 {
        // the connection's full story (the header may cut it short), then the usual hint
        let mut lines = Vec::new();
        match &app.connection {
            Some(ConnectionStatus::Failed(m)) => lines.push(Line::styled(m.clone(), t.error())),
            Some(ConnectionStatus::Waiting(m) | ConnectionStatus::Detached(m)) => {
                lines.push(Line::styled(m.clone(), t.warn()))
            }
            _ => {}
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled("No requests yet. Traffic appears here as the app makes calls.", t.dim()));
        let area = Rect {
            x: body.x + 1,
            y: body.y + 1,
            width: body.width.saturating_sub(2),
            height: body.height.saturating_sub(1),
        };
        Paragraph::new(lines).wrap(Wrap { trim: false }).render(area, buf);
        return;
    }
    let focused = app.focus == Focus::List;
    let offset = app.list_offset.min(rows_len.saturating_sub(1));
    let visible: Vec<(usize, Row)> = app
        .view_rows()
        .rows()
        .iter()
        .enumerate()
        .skip(offset)
        .take(body.height as usize)
        .map(|(i, r)| (i, r.clone()))
        .collect();
    for (row_i, (i, row)) in visible.into_iter().enumerate() {
        let y = body.y + row_i as u16;
        let selected = i == app.list_cursor;
        let row_style = if selected {
            if focused { t.selected() } else { t.selected().add_modifier(Modifier::DIM) }
        } else {
            Style::default()
        };
        if selected {
            fill(buf, Rect { x: r.x, y, width: r.width, height: 1 }, row_style);
        }
        app.hits.add(Rect { x: r.x, y, width: r.width, height: 1 }, Target::ListRow(i));
        let store = app.view_store();
        let (txn, members, prefix): (TxnIdx, Vec<TxnIdx>, String) = match &row {
            Row::Txn(ix) => (*ix, vec![*ix], String::new()),
            Row::Member(ix) => (*ix, vec![*ix], "  └ ".into()),
            Row::Group { members, expanded } => (
                *members.last().expect("non-empty"),
                members.clone(),
                if *expanded { "▾ ".into() } else { "▸ ".into() },
            ),
        };
        let tx = store.txn(txn);
        for &(c, x, w) in &layout {
            if c == Column::Timeline {
                for &m in &members {
                    let mt = store.txn(m);
                    draw_bar(buf, Rect::new(x, y, w, 1), (left, right), mt.segments(now), &t, selected);
                }
                continue;
            }
            let (mut s, style) = cell_text(app, tx, c, now);
            let mut spans = Vec::new();
            if c == Column::Name {
                let mut marks = prefix.clone();
                if tx.rule_modified() {
                    marks.push_str("✎ ");
                }
                if tx.hop > 0 {
                    marks.push_str("↪ ");
                }
                if tx.lossy || tx.resp_body.gap {
                    marks.push_str("! ");
                }
                let count = match &row {
                    Row::Group { members, .. } => format!(" ×{}", members.len()),
                    _ => String::new(),
                };
                let mw = marks.width();
                spans.push(Span::styled(marks, t.marker().patch(row_style)));
                s = truncate(&s, (w as usize).saturating_sub(mw + count.width()));
                spans.push(Span::styled(s, style.patch(row_style)));
                spans.push(Span::styled(count, t.accent().patch(row_style)));
            } else {
                let s = if right_aligned(c) {
                    format!("{:>w$}", truncate(&s, w as usize), w = w as usize)
                } else {
                    truncate(&s, w as usize)
                };
                spans.push(Span::styled(s, style.patch(row_style)));
            }
            text(buf, x, y, w, spans);
        }
    }
    // scroll indicator
    if rows_len > body.height as usize {
        let h = body.height as usize;
        let pos = offset * h / rows_len;
        let len = (h * h / rows_len).max(1);
        for k in 0..len.min(h) {
            if let Some(c) = buf.cell_mut((r.x + r.width - 1, body.y + (pos + k).min(h - 1) as u16)) {
                c.set_symbol("▐").set_style(t.faint());
            }
        }
    }
}

/// Lanes with at least one bar in the window: `(lane index, sub-rows)`, and the bars in visual
/// order (lane by lane, then by start time).
fn layout_lanes(store: &SessionStore, left: Ts, right: Ts, now: Ts) -> (Vec<(usize, usize)>, Vec<Bar>) {
    let mut bars: Vec<Bar> = Vec::new();
    let mut lane_rows: Vec<(usize, usize)> = Vec::new();
    for (li, lane) in store.lanes().iter().enumerate() {
        // greedy packing: a bar goes on the first sub-row that is free at its start
        let mut ends: Vec<Ts> = Vec::new();
        let first = bars.len();
        for &ix in &lane.txns {
            let tx = store.txn(ix);
            let end = tx.end.unwrap_or(now);
            if end < left || tx.start >= right {
                continue;
            }
            let sub = match ends.iter().position(|&e| e <= tx.start) {
                Some(i) => i,
                None => {
                    ends.push(0);
                    ends.len() - 1
                }
            };
            ends[sub] = end;
            bars.push(Bar { lane: li, sub, txn: ix });
        }
        if bars.len() > first {
            bars[first..].sort_by_key(|b| (store.txn(b.txn).start, b.sub));
            lane_rows.push((li, ends.len()));
        }
    }
    (lane_rows, bars)
}

fn draw_threads(app: &mut App, r: Rect, buf: &mut Buffer) {
    let t = app.theme.clone();
    let now = app.now();
    let (left, right) = app.list_window();
    let label_w = (r.width * 28 / 100).clamp(18, 34);
    let lane_area = Rect { x: r.x + label_w + 1, width: r.width.saturating_sub(label_w + 2), ..r };
    draw_axis(app, Rect { y: r.y, height: 1, ..lane_area }, left, right, buf);
    let (lane_rows, bars) = layout_lanes(app.view_store(), left, right, now);
    if let Some(sel) = app.selected
        && let Some(i) = bars.iter().position(|b| b.txn == sel)
    {
        app.bar_cursor = i;
    }
    app.bar_cursor = app.bar_cursor.min(bars.len().saturating_sub(1));
    if bars.is_empty() {
        text(
            buf,
            r.x + 1,
            r.y + 2,
            r.width.saturating_sub(2),
            vec![Span::styled("No requests in the visible time window.", t.dim())],
        );
        app.bars = bars;
        return;
    }
    let focused = app.focus == Focus::List;
    let cursor = app.bar_cursor;
    let selected_lane = bars.get(cursor).map(|b| b.lane);
    // first visual row of each lane
    let mut lane_y: Vec<(usize, usize)> = Vec::with_capacity(lane_rows.len());
    let mut acc = 0usize;
    for &(l, subs) in &lane_rows {
        lane_y.push((l, acc));
        acc += subs;
    }
    let y_of = |lane: usize| lane_y.iter().find(|(l, _)| *l == lane).map_or(0, |&(_, y)| y);
    // scroll so the selected bar's row is visible
    let body_y = r.y + 1;
    let body_h = r.height.saturating_sub(1) as usize;
    let sel_row = bars.get(cursor).map_or(0, |b| y_of(b.lane) + b.sub);
    let scroll = sel_row.saturating_sub(body_h.saturating_sub(2));
    let store = app.view_store();
    let mut hits: Vec<(Rect, Target)> = Vec::new();
    for &(l, subs) in &lane_rows {
        let ly = y_of(l);
        if ly + subs <= scroll || ly >= scroll + body_h {
            continue;
        }
        let lane = &store.lanes()[l];
        let first_row = ly.max(scroll);
        let y = body_y + (first_row - scroll) as u16;
        if ly >= scroll {
            let name = truncate(&lane.thread.name, label_w.saturating_sub(6) as usize);
            let st =
                if selected_lane == Some(l) && focused { t.accent().add_modifier(Modifier::BOLD) } else { t.text() };
            text(
                buf,
                r.x,
                y,
                label_w,
                vec![Span::styled(name, st), Span::styled(format!(" {}", lane.txns.len()), t.faint())],
            );
        }
        for row in first_row..(ly + subs).min(scroll + body_h) {
            let yy = body_y + (row - scroll) as u16;
            for x in lane_area.x..lane_area.x + lane_area.width {
                if let Some(c) = buf.cell_mut((x, yy)) {
                    c.set_symbol("·").set_style(t.faint());
                }
            }
        }
    }
    let span = right.saturating_sub(left).max(1) as f64;
    let col = |ts: Ts| f64::from(lane_area.width) * (ts.clamp(left, right) - left) as f64 / span;
    for (bi, b) in bars.iter().enumerate() {
        let row = y_of(b.lane) + b.sub;
        if row < scroll || row >= scroll + body_h {
            continue;
        }
        let y = body_y + (row - scroll) as u16;
        let seg = store.txn(b.txn).segments(now);
        let selected = bi == cursor;
        draw_bar(buf, Rect::new(lane_area.x, y, lane_area.width, 1), (left, right), seg, &t, selected);
        let x1 = lane_area.x + col(seg.start) as u16;
        let x2 = (lane_area.x + col(seg.end).ceil() as u16).clamp(x1 + 1, lane_area.x + lane_area.width);
        let hit = Rect { x: x1, y, width: x2.saturating_sub(x1).max(1), height: 1 };
        hits.push((hit, Target::ThreadBar(bi)));
        if selected && let Some(bg) = t.selected_bg() {
            for x in hit.x..hit.x + hit.width {
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.set_bg(bg);
                }
            }
        }
    }
    for (rect, target) in hits {
        app.hits.add(rect, target);
    }
    app.bars = bars;
}

fn draw_rules(app: &mut App, r: Rect, buf: &mut Buffer) {
    let t = app.theme.clone();
    let Some(rules) = app.rules.clone() else {
        text(
            buf,
            r.x + 1,
            r.y + 1,
            r.width.saturating_sub(2),
            vec![Span::styled(
                "No rules. Rules are read from .traffic-police/rules.toml (docs/PROTOCOL.md §8).",
                t.dim(),
            )],
        );
        return;
    };
    let store = app.view_store();
    let hits: Vec<usize> = rules
        .rules
        .iter()
        .map(|rule| {
            store.txns().iter().filter(|x| x.rules.iter().any(|h| h.rules.iter().any(|rr| rr.id == rule.id))).count()
        })
        .collect();
    let list_w = r.width * 55 / 100;
    text(
        buf,
        r.x,
        r.y,
        list_w,
        vec![Span::styled(
            format!("{:<5}{:<16}{:<26}{:>6}", "", "id", "name", "hits"),
            t.dim().add_modifier(Modifier::BOLD),
        )],
    );
    for (i, rule) in rules.rules.iter().enumerate() {
        let y = r.y + 1 + i as u16;
        if y >= r.y + r.height {
            break;
        }
        let selected = i == app.rules_cursor;
        if selected {
            fill(buf, Rect { x: r.x, y, width: list_w, height: 1 }, t.selected());
        }
        app.hits.add(Rect { x: r.x, y, width: list_w, height: 1 }, Target::RuleRow(i));
        let base = if rule.enabled { t.text() } else { t.dim() };
        let row_style = if selected { t.selected() } else { Style::default() };
        text(
            buf,
            r.x,
            y,
            list_w,
            vec![
                Span::styled(if rule.enabled { " [x] " } else { " [ ] " }, base.patch(row_style)),
                Span::styled(
                    format!("{:<16}", truncate(&rule.id, 15)),
                    base.add_modifier(Modifier::BOLD).patch(row_style),
                ),
                Span::styled(
                    format!("{:<26}", truncate(rule.name.as_deref().unwrap_or(""), 25)),
                    base.patch(row_style),
                ),
                Span::styled(format!("{:>6}", hits[i]), t.marker().patch(row_style)),
            ],
        );
    }
    // details of the selected rule
    let dx = r.x + list_w + 2;
    let dw = r.width.saturating_sub(list_w + 3);
    if let Some(rule) = rules.rules.get(app.rules_cursor) {
        let mut y = r.y;
        let mut line = |spans: Vec<Span<'static>>| {
            if y < r.y + r.height {
                text(buf, dx, y, dw, spans);
            }
            y += 1;
        };
        line(vec![Span::styled(rule.name.clone().unwrap_or_else(|| rule.id.clone()), t.title())]);
        line(vec![Span::styled(
            if rule.enabled { "enabled" } else { "disabled" },
            if rule.enabled { t.ok() } else { t.dim() },
        )]);
        line(vec![]);
        line(vec![Span::styled("Match", t.title())]);
        let m = &rule.matcher;
        let pat = |p: &Option<traffic_police_proto::msg::Pattern>| match p {
            Some(traffic_police_proto::msg::Pattern::Exact(s)) => s.clone(),
            Some(traffic_police_proto::msg::Pattern::Glob(s)) => format!("{s} (glob)"),
            Some(traffic_police_proto::msg::Pattern::Regex(s)) => format!("/{s}/"),
            None => "any".into(),
        };
        line(vec![Span::styled(
            format!("  methods  {}", if m.methods.is_empty() { "any".into() } else { m.methods.join(", ") }),
            t.text(),
        )]);
        if let Some(s) = &m.scheme {
            line(vec![Span::styled(format!("  scheme   {s}"), t.text())]);
        }
        line(vec![Span::styled(format!("  host     {}", pat(&m.host)), t.text())]);
        if let Some(p) = m.port {
            line(vec![Span::styled(format!("  port     {p}"), t.text())]);
        }
        line(vec![Span::styled(format!("  path     {}", pat(&m.path)), t.text())]);
        for q in &m.query {
            line(vec![Span::styled(format!("  query    {} = {}", q.name, pat(&q.value)), t.text())]);
        }
        line(vec![]);
        line(vec![Span::styled("Actions", t.title())]);
        for a in &rule.actions {
            use traffic_police_proto::msg::RuleAction as A;
            let s = match a {
                A::Delay { ms } => format!("  delay {ms} ms"),
                A::Fail { exception, .. } => format!("  fail with {exception}"),
                A::Status { code, reason } => format!("  status {code} {}", reason.clone().unwrap_or_default()),
                A::Header { op, name, value } => {
                    format!("  header {op} {name}{}", value.as_ref().map(|v| format!(": {v}")).unwrap_or_default())
                }
                A::Body { content_type, .. } => {
                    format!("  replace body{}", content_type.as_ref().map(|c| format!(" ({c})")).unwrap_or_default())
                }
                A::Replace { find, with, regex } => {
                    let what = if *regex { "regex" } else { "text" };
                    format!("  replace {what}  {find}\n  with          {with}")
                }
                A::Unknown => "  (unknown action)".into(),
            };
            for part in s.split('\n') {
                line(vec![Span::styled(part.to_string(), t.text())]);
            }
        }
    }
    text(
        buf,
        r.x,
        r.y + r.height - 1,
        r.width,
        vec![Span::styled(
            "Rules come from .traffic-police/rules.toml; editing and pushing to the device arrive in Phase 3.",
            t.faint(),
        )],
    );
}

fn draw_detail_pane(app: &mut App, r: Rect, buf: &mut Buffer) {
    let t = app.theme.clone();
    let focused = app.focus == Focus::Detail;
    let inner = panel(buf, r, focused, &t);
    let close = border_labels(buf, r, r.y, true, vec![vec![Span::styled("✕", t.dim())]]);
    if let Some(c) = close.first() {
        app.hits.add(*c, Target::DetailClose);
    }
    let room = close.first().map_or(r, |c| Rect { width: c.x.saturating_sub(r.x), ..r });
    let name = app.selected.map(|i| app.view_store().txn(i).url.name()).unwrap_or_default();
    let title_style =
        if focused { t.accent().add_modifier(Modifier::BOLD) } else { t.title().add_modifier(Modifier::BOLD) };
    let mut labels = vec![vec![Span::styled(truncate(&name, 28), title_style)]];
    for tab in Tab::ALL {
        let style = if app.detail.tab == tab {
            if focused {
                t.accent().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                t.text().add_modifier(Modifier::UNDERLINED)
            }
        } else {
            t.dim()
        };
        labels.push(vec![Span::styled(tab.title(), style)]);
    }
    let at = border_labels(buf, room, r.y, false, labels);
    for (rect, tab) in at.iter().skip(1).zip(Tab::ALL) {
        app.hits.add(*rect, Target::DetailTab(tab));
    }
    let content = Rect { x: inner.x + 1, width: inner.width.saturating_sub(1), ..inner };
    detail::draw(app, content, buf);
}

// --- status bar and overlays ---------------------------------------------------------------------

/// What the footer suggests for the focused panel: `(actions, label)`; the keys come from the
/// keymap, so remapped keys show as they are.
fn hints(app: &App) -> Vec<(&'static [Action], &'static str)> {
    use Action as A;
    let pause: &'static str = if app.recording { "pause" } else { "resume" };
    match (app.overlay, app.focus, app.view) {
        (Overlay::Columns { .. }, _, _) => {
            vec![(&[A::Up, A::Down], "move"), (&[A::Activate], "toggle"), (&[A::Back], "close")]
        }
        (Overlay::ConfirmClear, ..) => vec![],
        (_, Focus::Graph, _) => vec![
            (&[A::Left, A::Right], "move"),
            (&[A::ZoomIn, A::ZoomOut], "zoom"),
            (&[A::SelectRange], "range"),
            (&[A::GraphSource], "source"),
            (&[A::Live], "live"),
            (&[A::Back], "back"),
            (&[A::Help], "help"),
        ],
        (_, Focus::Detail, _) => vec![
            (&[A::Left, A::Right], "tabs"),
            (&[A::Up, A::Down], "move"),
            (&[A::Activate], "fold"),
            (&[A::Parsed], "parsed/source"),
            (&[A::Jq], "jq"),
            (&[A::Back], "close"),
            (&[A::Help], "help"),
        ],
        (_, Focus::List, View::Threads) => vec![
            (&[A::Up, A::Down], "threads"),
            (&[A::Left, A::Right], "requests"),
            (&[A::Activate], "open"),
            (&[A::Pause], pause),
            (&[A::FocusNext], "next panel"),
            (&[A::Help], "help"),
        ],
        (_, Focus::List, View::Rules) => {
            vec![(&[A::Up, A::Down], "select"), (&[A::FocusNext], "next panel"), (&[A::Help], "help")]
        }
        (_, Focus::List, View::Connections) => vec![
            (&[A::Activate], "open"),
            (&[A::Pause], pause),
            (&[A::Collapse], "collapse"),
            (&[A::Sort], "sort"),
            (&[A::FocusNext], "next panel"),
            (&[A::Quit], "quit"),
            (&[A::Help], "help"),
        ],
    }
}

fn draw_footer(app: &App, r: Rect, buf: &mut Buffer) {
    let t = &app.theme;
    // right: a message, else notes, and the frame rate when asked for
    let mut right: Vec<Span> = Vec::new();
    if let Some(m) = app.current_message() {
        right.push(Span::styled(truncate(m, (r.width / 2) as usize), t.accent()));
    } else {
        if let Some(n) = app.frozen_events() {
            right.push(Span::styled(format!("❄ frozen · {n} new events waiting · F"), t.accent()));
        }
        if let Some(d) = app.view_store().diagnostics().iter().rev().find(|d| d.level == "warn" || d.level == "error") {
            if !right.is_empty() {
                right.push(Span::raw("  "));
            }
            right.push(Span::styled(format!("⚠ {}", d.code.replace('_', " ")), t.warn()));
        }
    }
    if app.frames.visible {
        let (fps, took) = app.frames.summary();
        if !right.is_empty() {
            right.push(Span::raw("  "));
        }
        right.push(Span::styled(format!("{fps} fps · {:.1} ms", took.as_secs_f64() * 1e3), t.faint()));
    }
    let right_w: usize = right.iter().map(|s| s.content.width()).sum();
    // left: the hints that fit; the last one (help) stays longest
    let mut items: Vec<(String, &str)> = hints(app)
        .into_iter()
        .map(|(actions, label)| (actions.iter().map(|&a| app.keymap.key_label(a)).collect::<String>(), label))
        .filter(|(keys, _)| !keys.is_empty())
        .collect();
    let width_of = |items: &[(String, &str)]| items.iter().map(|(k, l)| k.width() + l.width() + 4).sum::<usize>();
    while items.len() > 1 && 1 + width_of(&items) + right_w + 2 > r.width as usize {
        items.remove(items.len() - 2);
    }
    let mut left: Vec<Span> = vec![Span::raw(" ")];
    for (keys, label) in items {
        left.push(Span::styled(keys, t.accent().add_modifier(Modifier::BOLD)));
        left.push(Span::styled(format!(" {label}   "), t.dim()));
    }
    text(buf, r.x, r.y, r.width, left);
    right_text(buf, r, r.y, right);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(4));
    let h = h.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_box(buf: &mut Buffer, r: Rect, title: &str, theme: &Theme) {
    fill(buf, r, Style::default().bg(theme.selected_bg().map(|_| Color::Reset).unwrap_or(Color::Reset)));
    ratatui::widgets::Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(format!(" {title} "))
        .border_style(theme.accent())
        .render(r, buf);
}

fn draw_help(app: &App, area: Rect, buf: &mut Buffer) {
    use crate::actions::{ACTIONS, Group};
    let t = &app.theme;
    let w = area.width.saturating_sub(8).min(118);
    let label_w = 9usize;
    let text_w = (w as usize).saturating_sub(4 + label_w);
    // one paragraph per group: "keys what · keys what", wrapped to the box
    let mut lines: Vec<Line> = Vec::new();
    let mut groups: Vec<(&str, Vec<(String, &str)>)> = Group::ALL
        .iter()
        .map(|g| {
            let items = ACTIONS
                .iter()
                .filter(|i| i.group == *g)
                .filter_map(|i| {
                    let keys: Vec<String> = app.keymap.keys(i.action).iter().map(ToString::to_string).collect();
                    (!keys.is_empty()).then(|| (keys.join("/"), i.hint))
                })
                .collect();
            (g.title(), items)
        })
        .collect();
    groups.push((
        "Mouse",
        vec![
            ("click".into(), "rows and tabs"),
            ("double-click".into(), "opens"),
            ("wheel".into(), "scrolls (zooms on the graph; Shift+wheel moves in time)"),
            ("drag".into(), "the graph to select a range, the border between boxes to resize"),
        ],
    ));
    for (title, items) in groups {
        let mut row: Vec<Span> = vec![Span::styled(format!("{title:<label_w$}"), t.accent())];
        let mut used = 0usize;
        for (i, (keys, what)) in items.into_iter().enumerate() {
            let sep = if i == 0 { "" } else { " · " };
            let item_w = sep.width() + keys.width() + 1 + what.width();
            if used + item_w > text_w && used > 0 {
                lines.push(Line::from(std::mem::take(&mut row)));
                row.push(Span::raw(" ".repeat(label_w)));
                used = 0;
                row.push(Span::styled(keys, t.text().add_modifier(Modifier::BOLD)));
            } else {
                row.push(Span::styled(sep, t.faint()));
                row.push(Span::styled(keys, t.text().add_modifier(Modifier::BOLD)));
            }
            row.push(Span::styled(format!(" {what}"), t.dim()));
            used += item_w;
        }
        lines.push(Line::from(row));
    }
    let r = centered(area, w, lines.len() as u16 + 4);
    draw_box(buf, r, "keys", t);
    for (i, line) in lines.iter().enumerate().take(r.height.saturating_sub(3) as usize) {
        draw_line(buf, r.x + 2, r.y + 1 + i as u16, r.width - 4, line, 0, Style::default());
    }
    text(buf, r.x + 2, r.y + r.height - 2, r.width - 4, vec![Span::styled("any key closes", t.faint())]);
}

fn draw_columns_menu(app: &App, area: Rect, buf: &mut Buffer, cursor: usize) {
    let t = &app.theme;
    let r = centered(area, 40, Column::OPTIONAL.len() as u16 + 4);
    draw_box(buf, r, "columns", t);
    for (i, c) in Column::OPTIONAL.iter().enumerate() {
        let on = app.columns.contains(c);
        let style = if i == cursor { t.selected() } else { Style::default() };
        text(
            buf,
            r.x + 2,
            r.y + 1 + i as u16,
            r.width - 4,
            vec![Span::styled(format!("{} {}", if on { "[x]" } else { "[ ]" }, c.title()), t.text().patch(style))],
        );
    }
    text(buf, r.x + 2, r.y + r.height - 2, r.width - 4, vec![Span::styled("Enter toggles · Esc closes", t.faint())]);
}

fn draw_confirm(app: &App, area: Rect, buf: &mut Buffer) {
    let t = &app.theme;
    let r = centered(area, 56, 5);
    draw_box(buf, r, "clear session", t);
    text(
        buf,
        r.x + 2,
        r.y + 2,
        r.width - 4,
        vec![
            Span::styled("Discard every captured request? ", t.text()),
            Span::styled("y", t.accent()),
            Span::styled(" / n", t.dim()),
        ],
    );
}

fn draw_jq(f: &mut Frame, app: &mut App, main: Rect) {
    let t = app.theme.clone();
    let y = main.y + main.height.saturating_sub(1);
    let r = Rect { x: main.x, y, width: main.width, height: 1 };
    let buf = f.buffer_mut();
    fill(buf, r, t.selected());
    let prompt = " jq ▸ ";
    let pw = prompt.width() as u16;
    let width = r.width.saturating_sub(pw + 1) as usize;
    let scroll = app.jq_input.visual_scroll(width);
    let value: String = app.jq_input.value().chars().skip(scroll).collect();
    text(
        buf,
        r.x,
        y,
        r.width,
        vec![Span::styled(prompt, t.accent().patch(t.selected())), Span::styled(value, t.text().patch(t.selected()))],
    );
    let cx = (app.jq_input.visual_cursor().max(scroll) - scroll) as u16;
    f.set_cursor_position((r.x + pw + cx, y));
}
