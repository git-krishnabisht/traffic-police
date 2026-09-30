//! Graph renderers besides braille: step lines drawn with box characters (one value per column,
//! one level per row), and a filled area with eighth-block tops.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

/// How the traffic graph draws its two series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphStyle {
    /// Step lines in heavy box characters (`┏━┓ ┃ ┗━┛`), one value per column.
    #[default]
    Heavy,
    /// Step lines with rounded corners (`╭─╮ │ ╰─╯`).
    Lines,
    /// Receiving as a filled area (eighth blocks), sending as a line on top.
    Area,
    /// Thin braille dots: the finest resolution (2 × 4 dots per cell).
    Braille,
}

impl GraphStyle {
    pub const ALL: [GraphStyle; 4] = [GraphStyle::Heavy, GraphStyle::Lines, GraphStyle::Area, GraphStyle::Braille];

    pub fn name(self) -> &'static str {
        match self {
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
    fn heavy_steps_round_to_the_nearest_row() {
        // on 3 rows with a maximum of 4: 1.2 is nearest row 1 (value 2), 0.9 nearest row 0
        let out = render(&[0.0, 1.2, 0.9], 3, true);
        assert_eq!(out, "   \n ┏┓\n━┛┗");
    }
}
