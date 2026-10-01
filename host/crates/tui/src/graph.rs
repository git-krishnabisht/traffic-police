//! Graph renderers: smooth curves in braille dots (the default), step lines drawn with box
//! characters (one value per column, one level per row), and a filled area with eighth-block tops.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use traffic_police_core::fmt::{self, NS_PER_MS};

/// How the traffic graph draws its two series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphStyle {
    /// Smooth curves in braille dots (2 × 4 per cell): the traffic smoothed over the half-second
    /// ticks of the app's counters, so steps become slopes.
    #[default]
    Smooth,
    /// Step lines in heavy box characters (`┏━┓ ┃ ┗━┛`), one value per column.
    Heavy,
    /// Step lines with rounded corners (`╭─╮ │ ╰─╯`).
    Lines,
    /// Receiving as a filled area (eighth blocks), sending as a line on top.
    Area,
    /// Thin braille dots: the finest resolution (2 × 4 dots per cell).
    Braille,
}

impl GraphStyle {
    pub const ALL: [GraphStyle; 5] =
        [GraphStyle::Smooth, GraphStyle::Heavy, GraphStyle::Lines, GraphStyle::Area, GraphStyle::Braille];

    pub fn name(self) -> &'static str {
        match self {
            GraphStyle::Smooth => "smooth",
            GraphStyle::Heavy => "heavy",
            GraphStyle::Lines => "lines",
            GraphStyle::Area => "area",
            GraphStyle::Braille => "braille",
        }
    }

    pub fn parse(s: &str) -> Option<GraphStyle> {
        GraphStyle::ALL.into_iter().find(|g| g.name() == s)
    }

    pub fn next(self) -> GraphStyle {
        let i = GraphStyle::ALL.iter().position(|g| *g == self).unwrap_or(0);
        GraphStyle::ALL[(i + 1) % GraphStyle::ALL.len()]
    }
}

/// The app's counters arrive a tick at a time: every 500 ms, the runtime's default
/// (PROTOCOL.md §7.1). The smooth style spreads each value over a tick.
pub const TICK_NS: u64 = 500 * NS_PER_MS;

/// While following live, the graph ends this far before "now": the tick in progress has not
/// arrived yet, and smoothing looks ahead by most of a tick. Drawn sooner, the newest stretch
/// would show a drop to zero and then fill in.
pub const LAG_NS: u64 = 2 * TICK_NS;

/// How long the y scale takes to come most of the way (63 %) to a new top.
const SCALE_EASE: Duration = Duration::from_millis(150);

/// The top of the y axis: a round rate above the highest value shown. It moves to a new top
/// gently, never below the highest value, so nothing is cut off; it comes down only once the
/// values fit with room to spare under a lower top, so values near a step don't flap it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scale {
    /// The round top it is going to.
    pub target: f64,
    /// Where it is now (labels and drawing).
    pub shown: f64,
    at: Instant,
}

impl Scale {
    /// At the top for `max` at once.
    pub fn new(max: f64, at: Instant) -> Scale {
        let top = fmt::nice_rate_ceil(max.max(1024.0));
        Scale { target: top, shown: top, at }
    }

    /// The scale for a frame drawn at `at` whose highest value is `max`.
    pub fn next(self, max: f64, at: Instant) -> Scale {
        let max = max.max(1024.0);
        let target = if max > self.target {
            fmt::nice_rate_ceil(max)
        } else {
            // down only with room to spare under the lower top
            self.target.min(fmt::nice_rate_ceil(max * 1.15))
        };
        let k = (-at.saturating_duration_since(self.at).as_secs_f64() / SCALE_EASE.as_secs_f64()).exp();
        let mut shown = (target + (self.shown - target) * k).max(max);
        if (shown - target).abs() < target * 0.02 {
            shown = target;
        }
        Scale { target, shown, at }
    }

    /// Still on its way: frames keep coming until it arrives (or stops being drawn).
    pub fn easing(&self) -> bool {
        self.shown != self.target && self.at.elapsed() < Duration::from_secs(1)
    }
}

/// Averages over `w` columns (a fraction is a part of a column) centered on each column, the
/// values being steady within a column. At the ends the window shrinks to what there is.
fn box_filter(v: &[f64], w: f64) -> Vec<f64> {
    let n = v.len();
    if n == 0 || w <= 1.0 {
        return v.to_vec();
    }
    let mut sum = vec![0.0; n + 1];
    for (i, x) in v.iter().enumerate() {
        sum[i + 1] = sum[i] + x;
    }
    // the running total up to `x` columns
    let total = |x: f64| {
        let i = (x as usize).min(n - 1);
        sum[i] + (x - i as f64) * v[i]
    };
    (0..n)
        .map(|i| {
            let c = i as f64 + 0.5;
            let (lo, hi) = ((c - w / 2.0).max(0.0), (c + w / 2.0).min(n as f64));
            (total(hi) - total(lo)) / (hi - lo)
        })
        .collect()
}

/// Rates as smooth curves, `tick` being the length of a tick in columns. The app's counters are
/// steps a tick wide: averaged over a tick, they become slopes between the ticks' middles, and a
/// pass half as wide rounds the corners. Captured bytes are `spiky`, each at the moment it moved:
/// a first pass spreads them over a tick.
pub fn smooth(v: &[f64], tick: f64, spiky: bool) -> Vec<f64> {
    let v = if spiky { box_filter(v, tick) } else { v.to_vec() };
    box_filter(&box_filter(&v, tick), tick / 2.0)
}

/// How many columns [`smooth`] looks to each side.
pub fn smooth_reach(tick: f64, spiky: bool) -> usize {
    let half = if spiky { 1.25 } else { 0.75 } * tick;
    half.ceil() as usize + 1
}

/// Draws series as lines in braille dots (2 × 4 per cell): one value per dot column, `None`
/// where there is no line. Values beyond the `2 × area.width` columns are for columns just left
/// of the area: the line comes in from them as it scrolls. A steep stretch climbs half in the
/// column before and half in its own, so the line stays joined. Where series meet, the cell
/// takes the color of the later one.
pub fn draw_curves(buf: &mut Buffer, area: Rect, series: &[(&[Option<f64>], Style)], ymax: f64) {
    const BIT: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    let (cols, rows) = (usize::from(area.width), usize::from(area.height));
    let (w, h) = (cols * 2, rows * 4);
    if w == 0 || h == 0 {
        return;
    }
    let mut bits = vec![0u8; cols * rows];
    let mut owner = vec![0usize; cols * rows];
    let level = |v: f64| -> usize {
        let top = (h - 1) as f64;
        if ymax > 0.0 && v > 0.0 { (v / ymax * top).round().clamp(0.0, top) as usize } else { 0 }
    };
    for (s, (values, _)) in series.iter().enumerate() {
        let lead = values.len().saturating_sub(w);
        let mut dot = |x: usize, y: usize| {
            // x counts from the first value
            if let Some(x) = x.checked_sub(lead).filter(|&x| x < w) {
                let from_top = h - 1 - y;
                let i = (from_top / 4) * cols + x / 2;
                bits[i] |= BIT[x % 2][from_top % 4];
                owner[i] = s;
            }
        };
        let mut prev: Option<usize> = None;
        for (x, v) in values.iter().enumerate() {
            let Some(v) = v else {
                prev = None;
                continue;
            };
            let y = level(*v);
            match prev {
                Some(p) if p.abs_diff(y) > 1 => {
                    let mid = (p + y) / 2;
                    for yy in p.min(mid)..=p.max(mid) {
                        dot(x - 1, yy);
                    }
                    for yy in mid.min(y)..=mid.max(y) {
                        dot(x, yy);
                    }
                }
                _ => dot(x, y),
            }
            prev = Some(y);
        }
    }
    for (i, &b) in bits.iter().enumerate() {
        if b == 0 {
            continue;
        }
        let (x, y) = (area.x + (i % cols) as u16, area.y + (i / cols) as u16);
        if let (Some(c), Some(ch)) = (buf.cell_mut((x, y)), char::from_u32(0x2800 + u32::from(b))) {
            c.set_symbol(ch.encode_utf8(&mut [0; 4])).set_style(series[owner[i]].1);
        }
    }
}

/// The glyphs of a step line: flat, upright, and the four turns.
struct Glyphs {
    flat: &'static str,
    upright: &'static str,
    /// Arrives from the left, turns up (bottom of a rise).
    left_up: &'static str,
    /// Arrives from below, turns right (top of a rise).
    down_right: &'static str,
    /// Arrives from the left, turns down (top of a fall).
    left_down: &'static str,
    /// Arrives from above, turns right (bottom of a fall).
    up_right: &'static str,
}

const ROUNDED: Glyphs =
    Glyphs { flat: "─", upright: "│", left_up: "╯", down_right: "╭", left_down: "╮", up_right: "╰" };
const HEAVY: Glyphs =
    Glyphs { flat: "━", upright: "┃", left_up: "┛", down_right: "┏", left_down: "┓", up_right: "┗" };

/// The row level (0 = bottom row) of a value, rounded to the nearest row.
fn level(v: f64, ymax: f64, rows: u16) -> u16 {
    if rows <= 1 || ymax <= 0.0 || v <= 0.0 {
        return 0;
    }
    let top = f64::from(rows - 1);
    (v / ymax * top).round().clamp(0.0, top) as u16
}

/// Draws one series as a step line, one value per column of `area`; `None` values (before the
/// session started, after now) leave their column empty.
pub fn draw_steps(buf: &mut Buffer, area: Rect, values: &[Option<f64>], ymax: f64, style: Style, heavy: bool) {
    let g = if heavy { &HEAVY } else { &ROUNDED };
    let row = |level: u16| area.y + area.height - 1 - level;
    let mut put = |x: u16, y: u16, s: &str| {
        if let Some(c) = buf.cell_mut((x, y)) {
            // where the lines cross, a flat stretch gives way to the other line's turns
            if s == g.flat && c.symbol() != " " && c.symbol() != g.flat {
                return;
            }
            c.set_symbol(s).set_style(style);
        }
    };
    let mut prev: Option<u16> = None;
    for (i, v) in values.iter().enumerate().take(area.width as usize) {
        let x = area.x + i as u16;
        let Some(v) = v else {
            prev = None;
            continue;
        };
        let y = level(*v, ymax, area.height);
        match prev {
            Some(p) if y > p => {
                put(x, row(p), g.left_up);
                for l in p + 1..y {
                    put(x, row(l), g.upright);
                }
                put(x, row(y), g.down_right);
            }
            Some(p) if y < p => {
                put(x, row(p), g.left_down);
                for l in y + 1..p {
                    put(x, row(l), g.upright);
                }
                put(x, row(y), g.up_right);
            }
            _ => put(x, row(y), g.flat),
        }
        prev = Some(y);
    }
}

/// Draws one series as a filled area with eighth-block tops (8 steps per row).
pub fn draw_area(buf: &mut Buffer, area: Rect, values: &[Option<f64>], ymax: f64, style: Style) {
    const EIGHTHS: [&str; 8] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇"];
    for (i, v) in values.iter().enumerate().take(area.width as usize) {
        let Some(v) = v else { continue };
        if *v <= 0.0 || ymax <= 0.0 {
            continue;
        }
        let eighths = ((v / ymax) * f64::from(area.height) * 8.0).round().max(1.0) as u32;
        let x = area.x + i as u16;
        for r in 0..area.height {
            let filled = eighths.saturating_sub(u32::from(r) * 8).min(8);
            if filled == 0 {
                break;
            }
            let s = if filled == 8 { "█" } else { EIGHTHS[filled as usize] };
            if let Some(c) = buf.cell_mut((x, area.y + area.height - 1 - r)) {
                c.set_symbol(s).set_style(style);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    fn render(values: &[f64], height: u16, heavy: bool) -> String {
        let area = Rect::new(0, 0, values.len() as u16, height);
        let mut buf = Buffer::empty(area);
        let v: Vec<Option<f64>> = values.iter().map(|&x| Some(x)).collect();
        draw_steps(&mut buf, area, &v, 4.0, Style::default(), heavy);
        (0..height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn steps_rise_and_fall_with_rounded_corners() {
        let out = render(&[0.0, 0.0, 4.0, 4.0, 2.0, 0.0, 0.0], 5, false);
        assert_eq!(out, "  ╭─╮  \n  │ │  \n  │ ╰╮ \n  │  │ \n──╯  ╰─");
    }

    #[test]
    fn smoothing_turns_steps_into_slopes_and_keeps_levels() {
        // ticks of 5 columns: 0, then 10 for three ticks, then 0
        let v: Vec<f64> = [0.0; 10].into_iter().chain([10.0; 15]).chain([0.0; 10]).collect();
        let s = smooth(&v, 5.0, false);
        // a rise and a fall instead of two cliffs
        assert!(s[8] > 0.0 && s[8] < s[10] && s[10] < s[12], "{s:?}");
        assert!(s[27] < s[25] && s[27] > 0.0, "{s:?}");
        // a level held for a while keeps its height
        assert!((s[17] - 10.0).abs() < 1e-9, "{s:?}");
        // the ends are averaged over what there is, not pulled toward nothing
        assert_eq!(smooth(&[4.0; 8], 5.0, false), vec![4.0; 8]);
        // the same bytes in the end, wherever they are spread
        let total = |v: &[f64]| v.iter().sum::<f64>();
        assert!((total(&s) - total(&v)).abs() < 1e-6);
    }

    #[test]
    fn spiky_bytes_spread_over_a_tick_each_way() {
        let mut v = vec![0.0; 21];
        v[10] = 50.0;
        let s = smooth(&v, 5.0, true);
        assert!(s[10] > s[8] && s[8] > s[6] && s[6] > 0.0, "{s:?}");
        assert!((s[8] - s[12]).abs() < 1e-9, "symmetric: {s:?}");
        assert!(s.iter().take(10 - smooth_reach(5.0, true)).all(|&x| x == 0.0), "{s:?}");
    }

    fn curves(series: &[(&[Option<f64>], Style)], width: u16) -> (String, Buffer) {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        draw_curves(&mut buf, area, series, 3.0);
        ((0..width).map(|x| buf[(x, 0)].symbol().to_string()).collect(), buf)
    }

    #[test]
    fn curves_are_braille_lines_that_stay_joined() {
        let some = |v: &[f64]| v.iter().copied().map(Some).collect::<Vec<_>>();
        // 6 dot columns, 4 dot rows: a climb to the top and back, half in the column before
        let (text, _) = curves(&[(&some(&[0.0, 0.0, 3.0, 3.0, 0.0, 0.0]), Style::default())], 3);
        assert_eq!(text, "⣠⠿⣄");
        // a value for the column left of the area: the line comes in from it
        let (text, _) = curves(&[(&some(&[3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]), Style::default())], 3);
        assert_eq!(text, "⣄⣀⣀");
        // no line where there is no value
        let (text, _) = curves(&[(&[None; 6], Style::default())], 3);
        assert_eq!(text, "   ");
    }

    #[test]
    fn where_curves_meet_the_later_one_colors_the_cell() {
        let (red, blue) = (Style::default().fg(Color::Red), Style::default().fg(Color::Blue));
        let first = [Some(0.0); 6];
        let second = [None, None, None, None, Some(0.0), Some(0.0)];
        let (text, buf) = curves(&[(&first, red), (&second, blue)], 3);
        assert_eq!(text, "⣀⣀⣀");
        assert_eq!([buf[(0, 0)].fg, buf[(1, 0)].fg, buf[(2, 0)].fg], [Color::Red, Color::Red, Color::Blue]);
    }

    #[test]
    fn the_scale_never_cuts_a_peak_off_and_comes_down_gently() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let s = Scale::new(3000.0, t0);
        assert_eq!((s.target, s.shown), (4096.0, 4096.0));
        // a higher peak: the scale is at least the peak at once, and goes on to a round top
        let s = s.next(10_000.0, at(10));
        assert_eq!(s.target, 16384.0);
        assert_eq!(s.shown, 10_000.0);
        let s = s.next(10_000.0, at(1000));
        assert_eq!(s.shown, 16384.0);
        // the peak gone: down gently
        let s = s.next(1500.0, at(1010));
        assert_eq!(s.target, 2048.0);
        assert!(s.shown > 15_000.0, "{s:?}");
        let s = s.next(1500.0, at(2000));
        assert_eq!(s.shown, 2048.0);
        // down a step only with room to spare under it
        assert_eq!(Scale::new(4000.0, t0).next(1900.0, at(500)).target, 4096.0);
        assert_eq!(Scale::new(4000.0, t0).next(1700.0, at(500)).target, 2048.0);
    }

    #[test]
    fn heavy_steps_round_to_the_nearest_row() {
        // on 3 rows with a maximum of 4: 1.2 is nearest row 1 (value 2), 0.9 nearest row 0
        let out = render(&[0.0, 1.2, 0.9], 3, true);
        assert_eq!(out, "   \n ┏┓\n━┛┗");
    }
}
