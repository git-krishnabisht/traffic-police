//! The terminal UI: application state, input handling and drawing (ARCHITECTURE.md §5.8).

pub mod app;
pub mod bodycache;
pub mod bodyview;
pub mod detail;
pub mod images;
pub mod terminal;
pub mod theme;
pub mod ui;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

pub use app::App;
pub use theme::Theme;

/// Render one frame into memory and return it as plain text, one line per row with trailing
/// spaces removed. Used by snapshot tests and `--dump-frame`.
pub fn render_text(app: &mut App, width: u16, height: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(width, height)).expect("test backend");
    term.draw(|f| ui::draw(f, app)).expect("test backend draw");
    // finish work the frame queued (large bodies), as the live loop would a moment later
    for _ in 0..4 {
        if !app.has_queued_jobs() {
            break;
        }
        app.run_jobs_inline();
        term.draw(|f| ui::draw(f, app)).expect("test backend draw");
    }
    buffer_text(term.backend().buffer())
}

/// A buffer as plain text. Cells hidden behind a wide character are skipped.
pub fn buffer_text(buf: &Buffer) -> String {
    let width = buf.area.width as usize;
    let mut out = String::with_capacity(buf.content.len() + buf.area.height as usize);
    for row in buf.content.chunks(width.max(1)) {
        let mut line = String::with_capacity(width);
        let mut skip = 0usize;
        for cell in row {
            if skip == 0 {
                line.push_str(cell.symbol());
            }
            skip = skip.max(unicode_width::UnicodeWidthStr::width(cell.symbol())).saturating_sub(1);
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The local UTC offset now, for wall-clock labels.
pub fn local_utc_offset_secs() -> i64 {
    i64::from(jiff::tz::TimeZone::system().to_offset(jiff::Timestamp::now()).seconds())
}

/// Parse a key script: plain characters are keys; `<Enter>`, `<Esc>`, `<Tab>`, `<S-Tab>`,
/// `<Space>`, `<Up>`, `<Down>`, `<Left>`, `<Right>`, `<PgUp>`, `<PgDn>`, `<Home>`, `<End>`,
/// `<BS>`, `<C-x>` (Ctrl+x) and `<lt>` (a literal `<`) name the others.
pub fn parse_keys(script: &str) -> Result<Vec<crossterm::event::KeyEvent>, String> {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut out = Vec::new();
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            continue;
        }
        let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
        let key = match name.as_str() {
            "Enter" | "CR" => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            "Esc" => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            "Tab" => KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            "S-Tab" | "BackTab" => KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            "Space" => KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            "Up" => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            "Down" => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            "Left" => KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            "Right" => KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
            "PgUp" => KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            "PgDn" => KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            "Home" => KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
            "End" => KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
            "BS" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            "lt" => KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE),
            other => match other.strip_prefix("C-").and_then(|k| {
                let mut it = k.chars();
                it.next().filter(|_| it.next().is_none())
            }) {
                Some(k) => KeyEvent::new(KeyCode::Char(k), KeyModifiers::CONTROL),
                None => return Err(format!("unknown key <{other}>")),
            },
        };
        out.push(key);
    }
    Ok(out)
}

/// Draw, then for each key: handle it, run queued jq filters inline, and draw again (so hit
/// areas and view sizes are current, as in the live loop). Returns the last frame as text.
pub fn render_keys(app: &mut App, width: u16, height: u16, keys: &[crossterm::event::KeyEvent]) -> String {
    let mut text = render_text(app, width, height);
    for &k in keys {
        app.handle_key(k);
        app.run_jobs_inline();
        text = render_text(app, width, height);
    }
    text
}
