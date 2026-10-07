//! The command palette (`:`): every named action, and settings without a key of their own (graph
//! styles), found by typing part of their names (ARCHITECTURE.md §5.10).

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;
use unicode_width::UnicodeWidthStr;

use crate::actions::{ACTIONS, Action};
use crate::app::{App, Overlay};
use crate::detail::draw_line;
use crate::graph::GraphStyle;

/// What a palette entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Action(Action),
    GraphStyle(GraphStyle),
}

#[derive(Debug, Clone)]
pub struct Item {
    pub command: Command,
    pub title: String,
    /// The action's name, for `[keymap]`.
    pub name: String,
    pub keys: String,
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub input: Input,
    pub items: Vec<Item>,
    /// Indexes into `items` that match, best first.
    pub shown: Vec<usize>,
    pub cursor: usize,
}

/// Actions that make no sense from a list: moving around, and the palette itself.
const LEFT_OUT: [Action; 14] = [
    Action::Up,
    Action::Down,
    Action::Left,
    Action::Right,
    Action::Top,
    Action::Bottom,
    Action::PageUp,
    Action::PageDown,
    Action::HalfPageUp,
    Action::HalfPageDown,
    Action::ScrollLeft,
    Action::ScrollRight,
    Action::Back,
    Action::Palette,
];

/// Neovim's ways out, typed after `:` (`:q`, `:q!`, `:qa`, `:wq`, `:x`, …): Enter on them quits.
pub fn is_quit_command(s: &str) -> bool {
    matches!(
        s.trim(),
        "q" | "q!"
            | "qa"
            | "qa!"
            | "qall"
            | "qall!"
            | "quit"
            | "quit!"
            | "quitall"
            | "quitall!"
            | "wq"
            | "wq!"
            | "wqa"
            | "wqa!"
            | "wqall"
            | "x"
            | "x!"
            | "xa"
            | "xa!"
            | "xall"
            | "exit"
            | "exit!"
    )
}

/// The picker's `:` line (it has no palette): what a key does to it.
#[derive(Debug, PartialEq, Eq)]
pub enum CommandLine {
    /// Still typing (or it was closed with Esc).
    Editing,
    /// Enter on a quit command.
    Quit,
    /// Enter on something else.
    Unknown(String),
}

/// One key on a `:` line being typed: letters add, Backspace removes (and closes it when it is
/// empty, as in Neovim), Esc closes, Enter runs.
pub fn command_line_key(line: &mut Option<String>, k: &KeyEvent) -> CommandLine {
    let Some(text) = line.as_mut() else { return CommandLine::Editing };
    match k.code {
        KeyCode::Enter => {
            let text = line.take().unwrap_or_default();
            if is_quit_command(&text) { CommandLine::Quit } else { CommandLine::Unknown(text) }
        }
        KeyCode::Esc => {
            *line = None;
            CommandLine::Editing
        }
        KeyCode::Backspace => {
            if text.pop().is_none() {
                *line = None;
            }
            CommandLine::Editing
        }
        KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            text.push(c);
            CommandLine::Editing
        }
        _ => CommandLine::Editing,
    }
}

/// How well `query` matches `text` (lower is better): every word of the query must appear, a
/// match at the start of a word counts most; a word that only matches as a subsequence (`cpc`
/// for "copy as cURL") ranks after those.
pub fn score(query: &str, text: &str) -> Option<usize> {
    let text = text.to_lowercase();
    let query = query.to_lowercase();
    let mut total = 0usize;
    for word in query.split_whitespace() {
        let at_word_start = text
            .match_indices(word)
            .find(|(i, _)| *i == 0 || !text[..*i].chars().next_back().is_some_and(char::is_alphanumeric));
        total += match (at_word_start, text.find(word)) {
            (Some((i, _)), _) => i,
            (None, Some(i)) => 100 + i,
            (None, None) => {
                let mut chars = text.chars();
                if !word.chars().all(|w| chars.any(|c| c == w)) {
                    return None;
                }
                1000
            }
        };
    }
    Some(total)
}

impl Palette {
    fn new(app: &App) -> Palette {
        let mut items: Vec<Item> = ACTIONS
            .iter()
            .filter(|i| !LEFT_OUT.contains(&i.action))
            .map(|i| {
                let mut keys: Vec<String> = app.keymap.keys(i.action).iter().map(ToString::to_string).collect();
                if i.action == Action::Quit {
                    keys.push(":q".into());
                }
                Item {
                    command: Command::Action(i.action),
                    title: i.title.to_string(),
                    name: i.name.to_string(),
                    keys: keys.join(" "),
                }
            })
            .collect();
        for g in GraphStyle::ALL {
            let current = if g == app.graph_style { " (current)" } else { "" };
            items.push(Item {
                command: Command::GraphStyle(g),
                title: format!("Graph style: {}{current}", g.name()),
                name: "graph-style".into(),
                keys: String::new(),
            });
        }
        let mut p = Palette { input: Input::default(), shown: (0..items.len()).collect(), items, cursor: 0 };
        p.refilter();
        p
    }

    /// Types `s` into the query (a paste).
    pub fn insert(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.input.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        self.cursor = 0;
        self.refilter();
    }

    fn refilter(&mut self) {
        let q = self.input.value().trim().to_string();
        let mut scored: Vec<(usize, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| score(&q, &format!("{} {} {}", it.title, it.name, it.keys)).map(|s| (s, i)))
            .collect();
        scored.sort();
        self.shown = scored.into_iter().map(|(_, i)| i).collect();
        // `:q`, `:wq`, `:x` and the like: Quit first, so Enter quits as in Neovim
        if is_quit_command(&q)
            && let Some(quit) = self.items.iter().position(|it| it.command == Command::Action(Action::Quit))
        {
            self.shown.retain(|&i| i != quit);
            self.shown.insert(0, quit);
        }
        self.cursor = self.cursor.min(self.shown.len().saturating_sub(1));
    }
}

impl App {
    pub fn open_palette(&mut self) {
        self.palette = Some(Palette::new(self));
        self.overlay = Overlay::Palette;
    }

    pub fn palette_key(&mut self, k: KeyEvent) {
        let Some(p) = &mut self.palette else {
            self.overlay = Overlay::None;
            return;
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // letters type into the query; other keys move as `[keymap]` says (Ctrl+P and Ctrl+N
        // too, as in most pickers), and Esc and Enter always close and run
        let typed =
            matches!(k.code, KeyCode::Char(_)) && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let action = if typed { None } else { self.keymap.action(&k) };
        let last = p.shown.len().saturating_sub(1);
        match k.code {
            KeyCode::Char('p') if ctrl => p.cursor = p.cursor.saturating_sub(1),
            KeyCode::Char('n') if ctrl => p.cursor = (p.cursor + 1).min(last),
            _ if k.code == KeyCode::Esc || action == Some(Action::Back) => {
                self.overlay = Overlay::None;
                self.palette = None;
            }
            _ if k.code == KeyCode::Enter || action == Some(Action::Activate) => {
                let chosen = p.shown.get(p.cursor).map(|&i| p.items[i].command);
                self.overlay = Overlay::None;
                self.palette = None;
                match chosen {
                    Some(Command::Action(a)) => self.run(a),
                    Some(Command::GraphStyle(g)) => {
                        self.graph_style = g;
                        self.flash(format!("graph style: {}", g.name()));
                    }
                    None => self.flash("no command matches"),
                }
            }
            _ if action == Some(Action::Up) => p.cursor = p.cursor.saturating_sub(1),
            _ if action == Some(Action::Down) => p.cursor = (p.cursor + 1).min(last),
            _ if action == Some(Action::PageUp) => p.cursor = p.cursor.saturating_sub(10),
            _ if action == Some(Action::PageDown) => p.cursor = (p.cursor + 10).min(last),
            _ => {
                let before = p.input.value().to_string();
                p.input.handle_event(&Event::Key(k));
                if p.input.value() != before {
                    p.cursor = 0;
                    p.refilter();
                }
            }
        }
    }
}

/// The palette: a box near the top with the typed text and the matching commands.
pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(p) = &app.palette else { return };
    let t = &app.theme;
    let w = area.width.saturating_sub(8).min(96);
    let rows = p.shown.len().clamp(1, (area.height as usize).saturating_sub(10).max(3));
    let h = rows as u16 + 4;
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + 3, width: w, height: h.min(area.height - 4) };
    let buf = f.buffer_mut();
    crate::ui::draw_box(buf, r, "commands", t);
    let inner = Rect { x: r.x + 2, y: r.y + 1, width: r.width.saturating_sub(4), height: r.height - 2 };
    let prompt = Line::from(vec![
        Span::styled(": ", t.accent().add_modifier(Modifier::BOLD)),
        Span::styled(p.input.value().to_string(), t.text()),
    ]);
    draw_line(buf, inner.x, inner.y, inner.width, &prompt, 0, Style::default());
    let list_y = inner.y + 2;
    let visible = (inner.height as usize).saturating_sub(2);
    let first = p.cursor.saturating_sub(visible.saturating_sub(1));
    if p.shown.is_empty() {
        let none = Line::from(Span::styled("no command matches", t.faint()));
        draw_line(buf, inner.x, list_y, inner.width, &none, 0, Style::default());
    }
    for (row, &i) in p.shown.iter().enumerate().skip(first).take(visible) {
        let it = &p.items[i];
        let y = list_y + (row - first) as u16;
        let selected = row == p.cursor;
        let base = if selected { t.selected() } else { Style::default() };
        if selected {
            crate::ui::fill(buf, Rect { x: r.x + 1, y, width: r.width - 2, height: 1 }, base);
        }
        let keys_w = it.keys.width() as u16;
        let left = Line::from(vec![
            Span::styled(it.title.clone(), t.text().patch(base)),
            Span::styled(format!("  {}", it.name), t.faint().patch(base)),
        ]);
        draw_line(buf, inner.x, y, inner.width.saturating_sub(keys_w + 2), &left, 0, Style::default());
        let keys = Line::from(Span::styled(it.keys.clone(), t.accent().patch(base)));
        draw_line(buf, inner.x + inner.width - keys_w, y, keys_w, &keys, 0, Style::default());
    }
    let x = inner.x + 2 + p.input.visual_cursor() as u16;
    f.set_cursor_position(Position::new(x.min(inner.x + inner.width), inner.y));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neovims_quit_commands() {
        for c in ["q", "q!", " q ", "qa", "qall!", "quit", "wq", "wqa", "x", "xa!", "exit"] {
            assert!(is_quit_command(c), "{c}");
        }
        for c in ["", "w", "qq", "quitter", "jq", "copy"] {
            assert!(!is_quit_command(c), "{c}");
        }
    }

    #[test]
    fn a_command_line_types_runs_and_closes() {
        let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let mut line = Some(String::new());
        for c in "wq".chars() {
            assert_eq!(command_line_key(&mut line, &key(KeyCode::Char(c))), CommandLine::Editing);
        }
        assert_eq!(line.as_deref(), Some("wq"));
        assert_eq!(command_line_key(&mut line, &key(KeyCode::Enter)), CommandLine::Quit);
        assert_eq!(line, None);
        let mut line = Some("w".to_string());
        assert_eq!(command_line_key(&mut line, &key(KeyCode::Enter)), CommandLine::Unknown("w".into()));
        // Backspace on an empty line closes it, as in Neovim; Esc does too
        let mut line = Some("q".to_string());
        command_line_key(&mut line, &key(KeyCode::Backspace));
        assert_eq!(line.as_deref(), Some(""));
        command_line_key(&mut line, &key(KeyCode::Backspace));
        assert_eq!(line, None);
        let mut line = Some("q".to_string());
        command_line_key(&mut line, &key(KeyCode::Esc));
        assert_eq!(line, None);
    }

    #[test]
    fn words_match_anywhere_and_word_starts_rank_first() {
        assert!(score("", "anything").is_some());
        assert!(score("har", "Export HAR").is_some());
        assert!(score("style heavy", "Graph style: heavy").is_some());
        assert!(score("heavy style", "Graph style: heavy").is_some(), "any order");
        assert!(score("zzz", "Copy").is_none());
        assert!(score("cpc", "Copy as cURL").is_some(), "a subsequence still matches");
        assert!(score("cop", "Copy") < score("cop", "Scope"), "a word start beats the middle");
        assert!(score("cop", "Scope") < score("cpy", "Copy"), "a substring beats a subsequence");
    }
}
