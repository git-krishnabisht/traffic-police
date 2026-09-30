//! Named actions and the keymap (ARCHITECTURE.md §5.13). Every command has a name (used by the
//! user config's `[keymap]` and the command palette), a description, and default keys; the help
//! screen, the key hints in the footer and the palette are all built from this table.

use std::collections::HashMap;
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Everything the user can do from the keyboard. Navigation actions (`Up`, `Left`, `Activate`,
/// ...) mean what the focused panel makes of them; the others do the same thing everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Up,
    Down,
    Left,
    Right,
    Top,
    Bottom,
    PageUp,
    PageDown,
    Activate,
    Back,
    FocusNext,
    FocusPrev,
    ViewConnections,
    ViewThreads,
    ViewRules,
    Pause,
    Freeze,
    Live,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    SelectRange,
    GraphSource,
    TimeLabels,
    GraphStyle,
    Collapse,
    Sort,
    SortReverse,
    Columns,
    Find,
    FindNext,
    FindPrev,
    Pin,
    NewRule,
    Diff,
    Copy,
    Save,
    Export,
    Parsed,
    Original,
    Jq,
    ScrollLeft,
    ScrollRight,
    FoldAll,
    UnfoldAll,
    Palette,
    Help,
    Clear,
    FrameRate,
    Quit,
}

/// Where an action is listed in the help screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Move,
    Views,
    Live,
    Graph,
    List,
    Detail,
    Session,
}

impl Group {
    pub const ALL: [Group; 7] =
        [Group::Move, Group::Views, Group::List, Group::Detail, Group::Live, Group::Graph, Group::Session];

    pub fn title(self) -> &'static str {
        match self {
            Group::Move => "Move",
            Group::Views => "Views",
            Group::Live => "Live",
            Group::Graph => "Graph",
            Group::List => "List",
            Group::Detail => "Detail",
            Group::Session => "Session",
        }
    }
}

pub struct Info {
    pub action: Action,
    /// For `[keymap]` and the palette: `pause`, `copy`, `graph-style`.
    pub name: &'static str,
    /// What it does, in the help screen and the palette.
    pub title: &'static str,
    /// Short label in the footer's key hints.
    pub hint: &'static str,
    pub group: Group,
    pub keys: &'static [&'static str],
}

macro_rules! info {
    ($a:ident, $name:literal, $group:ident, $hint:literal, $title:literal, [$($k:literal),*]) => {
        Info { action: Action::$a, name: $name, title: $title, hint: $hint, group: Group::$group, keys: &[$($k),*] }
    };
}

/// The table: default keys follow the brief, plus `T` (graph source).
pub const ACTIONS: &[Info] = &[
    info!(Up, "up", Move, "up", "Move up", ["up", "k"]),
    info!(Down, "down", Move, "down", "Move down", ["down", "j"]),
    info!(Left, "left", Move, "left", "Left: previous tab, collapse a group, earlier on the graph", ["left", "h"]),
    info!(Right, "right", Move, "right", "Right: next tab, expand a group, later on the graph", ["right", "l"]),
    info!(Top, "top", Move, "top", "Go to the top", ["g", "home"]),
    info!(Bottom, "bottom", Move, "bottom", "Go to the bottom", ["G", "end"]),
    info!(PageUp, "page-up", Move, "page up", "Page up", ["pgup"]),
    info!(PageDown, "page-down", Move, "page down", "Page down", ["pgdn"]),
    info!(
        Activate,
        "open",
        Move,
        "open/decode",
        "Open the request; fold or unfold; on a header or value, copy, decode or filter by it",
        ["enter"]
    ),
    info!(Back, "back", Move, "back", "Close, cancel or clear, one step back", ["esc"]),
    info!(FocusNext, "focus-next", Move, "next panel", "Focus the next panel", ["tab"]),
    info!(FocusPrev, "focus-previous", Move, "previous panel", "Focus the previous panel", ["backtab"]),
    info!(ViewConnections, "view-connections", Views, "connections", "Connection View", ["1"]),
    info!(ViewThreads, "view-threads", Views, "threads", "Thread View", ["2"]),
    info!(ViewRules, "view-rules", Views, "rules", "Rules", ["3"]),
    info!(Pause, "pause", Live, "pause", "Pause or resume recording on the device", ["space"]),
    info!(Freeze, "freeze", Live, "freeze", "Freeze the view (capture continues)", ["F"]),
    info!(Live, "live", Live, "live", "Back to live", ["L"]),
    info!(ZoomIn, "zoom-in", Live, "zoom in", "Zoom in (shorter time window)", ["+", "="]),
    info!(ZoomOut, "zoom-out", Live, "zoom out", "Zoom out (longer time window)", ["-"]),
    info!(ZoomReset, "zoom-reset", Live, "reset zoom", "Reset the zoom", ["0"]),
    info!(SelectRange, "select-range", Live, "range", "Select a time range (again to apply)", ["v"]),
    info!(GraphSource, "graph-source", Graph, "source", "Graph: whole-app traffic or captured requests", ["T"]),
    info!(TimeLabels, "time-labels", Graph, "clock", "Time axis: since start or wall clock", ["t"]),
    info!(GraphStyle, "graph-style", Graph, "style", "Graph style: heavy, lines, area, braille", []),
    info!(Collapse, "collapse", List, "collapse", "Collapse repeated calls", ["c"]),
    info!(Sort, "sort", List, "sort", "Sort by the next column", ["s"]),
    info!(SortReverse, "sort-reverse", List, "reverse", "Reverse the sort", ["S"]),
    info!(Columns, "columns", List, "columns", "Choose columns", ["C"]),
    info!(Find, "find", List, "filter", "Filter the list, or search the body in the detail pane", ["/"]),
    info!(FindNext, "find-next", Detail, "next match", "Next search match", ["n"]),
    info!(FindPrev, "find-previous", Detail, "previous match", "Previous search match", ["N"]),
    info!(Pin, "pin", List, "pin", "Pin or unpin the request", ["m"]),
    info!(
        NewRule,
        "new-rule",
        List,
        "new rule",
        "New rule matching the request (in .traffic-police/rules.toml)",
        ["r"]
    ),
    info!(Diff, "diff", List, "diff", "Mark for diff (the second mark opens the diff)", ["d"]),
    info!(Copy, "copy", Session, "copy", "Copy: cURL, URL, headers, body, value", ["y"]),
    info!(Save, "save", Session, "save body", "Save a body to a file", ["w"]),
    info!(Export, "export", Session, "export", "Export: HAR or session file", ["e"]),
    info!(Parsed, "parsed", Detail, "parsed/source", "Parsed or source view of the body", ["p"]),
    info!(Original, "original", Detail, "original", "Original or rule-modified response", ["o"]),
    info!(Jq, "jq", Detail, "jq", "jq filter on the body (empty clears)", ["|"]),
    info!(ScrollLeft, "scroll-left", Detail, "scroll left", "Scroll the body left", ["<"]),
    info!(ScrollRight, "scroll-right", Detail, "scroll right", "Scroll the body right", [">"]),
    info!(FoldAll, "fold-all", Detail, "fold all", "Fold every JSON node", ["["]),
    info!(UnfoldAll, "unfold-all", Detail, "unfold all", "Unfold every JSON node", ["]"]),
    info!(Palette, "palette", Session, "commands", "Command palette", [":"]),
    info!(Help, "help", Session, "help", "Help: every key", ["?"]),
    info!(Clear, "clear", Session, "clear", "Clear the session (asks first)", ["x"]),
    info!(FrameRate, "frame-rate", Session, "fps", "Show or hide the frame rate (fps)", []),
    info!(Quit, "quit", Session, "quit", "Quit", ["q"]),
];

impl Action {
    pub fn info(self) -> &'static Info {
        ACTIONS.iter().find(|i| i.action == self).expect("every action is in the table")
    }

    pub fn by_name(name: &str) -> Option<Action> {
        ACTIONS.iter().find(|i| i.name == name).map(|i| i.action)
    }
}

/// One key with its modifiers, as the user writes it (`ctrl+r`, `shift+tab`, `F`, `?`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Key {
    /// The key of a terminal event. Shift is folded into the character for printable keys, so
    /// `G` and `shift+g` are the same key.
    pub fn of(k: &KeyEvent) -> Key {
        let mut mods = k.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let code = match k.code {
            KeyCode::Char(c) => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::Char(c)
            }
            KeyCode::BackTab => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            other => other,
        };
        Key { code, mods }
    }

    pub fn parse(s: &str) -> Result<Key, String> {
        let mut mods = KeyModifiers::NONE;
        let mut rest = s.trim();
        loop {
            let lower = rest.to_ascii_lowercase();
            if let Some(r) = lower.strip_prefix("ctrl+") {
                mods |= KeyModifiers::CONTROL;
                rest = &rest[rest.len() - r.len()..];
            } else if let Some(r) = lower.strip_prefix("alt+") {
                mods |= KeyModifiers::ALT;
                rest = &rest[rest.len() - r.len()..];
            } else if let Some(r) = lower.strip_prefix("shift+") {
                mods |= KeyModifiers::SHIFT;
                rest = &rest[rest.len() - r.len()..];
            } else {
                break;
            }
        }
        let code = match rest.to_ascii_lowercase().as_str() {
            "space" => KeyCode::Char(' '),
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" if mods.contains(KeyModifiers::SHIFT) => {
                mods.remove(KeyModifiers::SHIFT);
                KeyCode::BackTab
            }
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pgup" | "pageup" => KeyCode::PageUp,
            "pgdn" | "pagedown" => KeyCode::PageDown,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => {
                KeyCode::F(f[1..].parse().expect("checked"))
            }
            _ => {
                let mut chars = rest.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => {
                        let c = if mods.contains(KeyModifiers::SHIFT) { c.to_ascii_uppercase() } else { c };
                        mods.remove(KeyModifiers::SHIFT);
                        KeyCode::Char(c)
                    }
                    _ => return Err(format!("unknown key {s:?}")),
                }
            }
        };
        Ok(Key { code, mods })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods.contains(KeyModifiers::CONTROL) {
            f.write_str("Ctrl+")?;
        }
        if self.mods.contains(KeyModifiers::ALT) {
            f.write_str("Alt+")?;
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            f.write_str("Shift+")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("Space"),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => f.write_str("Enter"),
            KeyCode::Esc => f.write_str("Esc"),
            KeyCode::Tab => f.write_str("Tab"),
            KeyCode::BackTab => f.write_str("Shift+Tab"),
            KeyCode::Up => f.write_str("↑"),
            KeyCode::Down => f.write_str("↓"),
            KeyCode::Left => f.write_str("←"),
            KeyCode::Right => f.write_str("→"),
            KeyCode::Home => f.write_str("Home"),
            KeyCode::End => f.write_str("End"),
            KeyCode::PageUp => f.write_str("PgUp"),
            KeyCode::PageDown => f.write_str("PgDn"),
            KeyCode::Backspace => f.write_str("Backspace"),
            KeyCode::Delete => f.write_str("Del"),
            KeyCode::F(n) => write!(f, "F{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Keys to actions. Built from the defaults, then the user's `[keymap]`: naming an action
/// there replaces its default keys.
#[derive(Debug, Clone)]
pub struct Keymap {
    by_key: HashMap<Key, Action>,
    by_action: HashMap<Action, Vec<Key>>,
}

impl Default for Keymap {
    fn default() -> Self {
        let mut m = Keymap { by_key: HashMap::new(), by_action: HashMap::new() };
        for i in ACTIONS {
            let keys: Vec<Key> = i.keys.iter().map(|k| Key::parse(k).expect("default keys parse")).collect();
            m.set(i.action, keys);
        }
        m
    }
}

impl Keymap {
    fn set(&mut self, action: Action, keys: Vec<Key>) {
        if let Some(old) = self.by_action.remove(&action) {
            for k in old {
                if self.by_key.get(&k) == Some(&action) {
                    self.by_key.remove(&k);
                }
            }
        }
        for k in &keys {
            // a key moved to this action leaves the action that had it
            if let Some(prev) = self.by_key.insert(*k, action)
                && prev != action
                && let Some(v) = self.by_action.get_mut(&prev)
            {
                v.retain(|x| x != k);
            }
        }
        self.by_action.insert(action, keys);
    }

    /// Apply `[keymap]` entries: `name = "key"` or `name = ["key", "key"]` (an empty list
    /// unbinds). Errors name the line's problem and the valid action names.
    pub fn apply(&mut self, entries: &[(String, Vec<String>)]) -> Vec<String> {
        let mut errors = Vec::new();
        for (name, keys) in entries {
            let Some(action) = Action::by_name(name) else {
                let names: Vec<&str> = ACTIONS.iter().map(|i| i.name).collect();
                errors.push(format!("unknown action {name:?}; valid names: {}", names.join(", ")));
                continue;
            };
            match keys.iter().map(|k| Key::parse(k)).collect::<Result<Vec<_>, _>>() {
                Ok(keys) => self.set(action, keys),
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        errors
    }

    pub fn action(&self, k: &KeyEvent) -> Option<Action> {
        self.by_key.get(&Key::of(k)).copied()
    }

    pub fn keys(&self, action: Action) -> &[Key] {
        self.by_action.get(&action).map_or(&[], |v| v.as_slice())
    }

    /// The first key of an action, for hints ("" when it has none).
    pub fn key_label(&self, action: Action) -> String {
        self.keys(action).first().map(ToString::to_string).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn default_keys_resolve() {
        let m = Keymap::default();
        assert_eq!(m.action(&ev(KeyCode::Char('j'), KeyModifiers::NONE)), Some(Action::Down));
        assert_eq!(m.action(&ev(KeyCode::Char(' '), KeyModifiers::NONE)), Some(Action::Pause));
        // terminals report capital letters with or without SHIFT
        assert_eq!(m.action(&ev(KeyCode::Char('G'), KeyModifiers::SHIFT)), Some(Action::Bottom));
        assert_eq!(m.action(&ev(KeyCode::Char('G'), KeyModifiers::NONE)), Some(Action::Bottom));
        assert_eq!(m.action(&ev(KeyCode::BackTab, KeyModifiers::SHIFT)), Some(Action::FocusPrev));
        assert_eq!(m.key_label(Action::Pause), "Space");
    }

    #[test]
    fn every_action_has_a_unique_name() {
        let mut names: Vec<&str> = ACTIONS.iter().map(|i| i.name).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n);
    }

    #[test]
    fn keymap_overrides_replace_and_steal_keys() {
        let mut m = Keymap::default();
        let errors = m.apply(&[
            ("pause".into(), vec!["ctrl+p".into()]),
            ("freeze".into(), vec!["space".into()]),
            ("quit".into(), vec![]),
            ("fly".into(), vec!["f".into()]),
            ("copy".into(), vec!["ctrl+shift+notakey".into()]),
        ]);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("unknown action \"fly\""));
        assert_eq!(m.action(&ev(KeyCode::Char('p'), KeyModifiers::CONTROL)), Some(Action::Pause));
        assert_eq!(m.action(&ev(KeyCode::Char(' '), KeyModifiers::NONE)), Some(Action::Freeze));
        assert_eq!(m.action(&ev(KeyCode::Char('q'), KeyModifiers::NONE)), None);
        assert_eq!(m.action(&ev(KeyCode::Char('F'), KeyModifiers::NONE)), None, "freeze's old key is gone");
        assert!(m.keys(Action::Pause).iter().all(|k| k.code != KeyCode::Char(' ')));
        assert_eq!(Key::parse("shift+tab").unwrap().code, KeyCode::BackTab);
        assert_eq!(Key::parse("shift+g").unwrap(), Key::parse("G").unwrap());
    }
}
