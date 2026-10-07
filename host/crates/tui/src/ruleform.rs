//! The rule form (ARCHITECTURE.md §5.11.1): one rule of `rules.toml`, made or changed inside the
//! Rules view. Every change is checked at once with the file's own checks, each problem shown at
//! its field, and a rule with problems cannot be saved. The form says how many captured requests
//! the match selects and can show them in the list. Saving writes the rule into the file and
//! leaves the rest of it as it was (comments, blank lines, the other rules); `$EDITOR` stays one
//! key away.

use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Span;
use traffic_police_core::project::DIR;
use traffic_police_core::ruleform::{
    self, ACTION_TYPES, ActionDraft, ActionSlot, BodySource, EXCEPTIONS, Field, HEADER_OPS, METHODS, PatternDraft,
    PatternKind, RuleDraft,
};
use traffic_police_core::rules::{self, RulesFile};
use tui_input::Input;
use tui_input::backend::crossterm::EventHandler;

use crate::actions::Action;
use crate::app::{App, Overlay, Target, View};
use crate::share::{Menu, MenuAction, MenuItem};

/// A field of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Ms,
    Exception,
    Message,
    Code,
    Reason,
    Op,
    Name,
    Value,
    Source,
    Body,
    ContentType,
    Find,
    With,
    Regex,
}

/// One line of the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Section(&'static str),
    Blank,
    Id,
    Name,
    Enabled,
    CacheRewrites,
    Methods,
    Scheme,
    Host,
    Path,
    Port,
    QueryName(usize),
    QueryValue(usize),
    AddQuery,
    /// An action's type (its first line).
    Action(usize),
    ActionField(usize, Key),
    AddAction,
    Matches,
}

impl Row {
    fn selectable(self) -> bool {
        !matches!(self, Row::Section(_) | Row::Blank)
    }

    /// The field whose problems show on this line.
    fn field(self) -> Option<Field> {
        Some(match self {
            Row::Id => Field::Id,
            Row::Name => Field::Name,
            Row::Enabled => Field::Enabled,
            Row::CacheRewrites => Field::CacheRewrites,
            Row::Methods => Field::Methods,
            Row::Scheme => Field::Scheme,
            Row::Host => Field::Host,
            Row::Path => Field::Path,
            Row::Port => Field::Port,
            Row::QueryName(i) => Field::Query(i),
            Row::Action(i) => Field::Action(i),
            _ => return None,
        })
    }
}

/// The form's state, kept in [`App::form`] while it is open.
#[derive(Debug, Clone)]
pub struct RuleForm {
    pub draft: RuleDraft,
    /// The rule's place among the file's rules; `None` for a new rule (appended on save).
    pub index: Option<usize>,
    /// The rule as it was when the form opened: leaving asks first when they differ.
    opened: RuleDraft,
    pub cursor: usize,
    /// The methods line: which method the cursor is on.
    pub chip: usize,
    /// A field being typed into, and what it held before (Esc puts it back).
    pub editing: Option<(Input, String)>,
    pub problems: Vec<(Field, String)>,
    /// How many captured requests the match selects, or why it cannot be tried here.
    pub matches: Result<(usize, usize), String>,
    /// The first line shown.
    pub top: usize,
    /// Where `.traffic-police/` is (body files are read from there).
    base: PathBuf,
    /// The ids of the file's other rules.
    others: Vec<String>,
}

impl RuleForm {
    fn new(draft: RuleDraft, index: Option<usize>, file: Option<&RulesFile>, base: PathBuf) -> RuleForm {
        let others = file
            .map(|f| {
                f.entries.iter().enumerate().filter(|(i, _)| Some(*i) != index).map(|(_, e)| e.id.clone()).collect()
            })
            .unwrap_or_default();
        let mut form = RuleForm {
            opened: draft.clone(),
            draft,
            index,
            cursor: 0,
            chip: 0,
            editing: None,
            problems: Vec::new(),
            matches: Ok((0, 0)),
            top: 0,
            base,
            others,
        };
        form.cursor = form.rows().iter().position(|r| r.selectable()).unwrap_or(0);
        form
    }

    pub fn is_new(&self) -> bool {
        self.index.is_none()
    }

    pub fn changed(&self) -> bool {
        self.draft != self.opened
    }

    /// The lines of the form, from the rule as it is now.
    pub fn rows(&self) -> Vec<Row> {
        let d = &self.draft;
        let mut rows = vec![Row::Section("Rule"), Row::Id, Row::Name, Row::Enabled, Row::CacheRewrites];
        rows.extend([Row::Blank, Row::Section("Match"), Row::Methods, Row::Scheme, Row::Host, Row::Path, Row::Port]);
        for i in 0..d.query.len() {
            rows.extend([Row::QueryName(i), Row::QueryValue(i)]);
        }
        rows.extend([Row::AddQuery, Row::Blank, Row::Section("Actions, in order")]);
        for (i, slot) in d.actions.iter().enumerate() {
            rows.push(Row::Action(i));
            let keys: &[Key] = match &slot.action {
                ActionDraft::Delay { .. } => &[Key::Ms],
                ActionDraft::Fail { .. } => &[Key::Exception, Key::Message],
                ActionDraft::Status { .. } => &[Key::Code, Key::Reason],
                ActionDraft::Header { op, .. } if op == "remove" => &[Key::Op, Key::Name],
                ActionDraft::Header { .. } => &[Key::Op, Key::Name, Key::Value],
                ActionDraft::Body { .. } => &[Key::Source, Key::Body, Key::ContentType],
                ActionDraft::Replace { .. } => &[Key::Find, Key::With, Key::Regex],
            };
            rows.extend(keys.iter().map(|k| Row::ActionField(i, *k)));
        }
        rows.extend([Row::AddAction, Row::Blank, Row::Matches]);
        rows
    }

    fn row(&self) -> Row {
        let rows = self.rows();
        rows.get(self.cursor.min(rows.len().saturating_sub(1))).copied().unwrap_or(Row::Id)
    }

    /// The text of a line that is typed into, `None` for the others.
    fn text_of(&self, row: Row) -> Option<String> {
        let d = &self.draft;
        Some(match row {
            Row::Id => d.id.clone(),
            Row::Name => d.name.clone(),
            Row::Host => d.host.text.clone(),
            Row::Path => d.path.text.clone(),
            Row::Port => d.port.clone(),
            Row::QueryName(i) => d.query.get(i)?.0.clone(),
            Row::QueryValue(i) => d.query.get(i)?.1.text.clone(),
            Row::ActionField(i, k) => {
                let a = &d.actions.get(i)?.action;
                match (a, k) {
                    (ActionDraft::Delay { ms }, Key::Ms) => ms.clone(),
                    (ActionDraft::Fail { message, .. }, Key::Message) => message.clone(),
                    (ActionDraft::Status { code, .. }, Key::Code) => code.clone(),
                    (ActionDraft::Status { reason, .. }, Key::Reason) => reason.clone(),
                    (ActionDraft::Header { name, .. }, Key::Name) => name.clone(),
                    (ActionDraft::Header { value, .. }, Key::Value) => value.clone(),
                    (ActionDraft::Body { value, .. }, Key::Body) => value.clone(),
                    (ActionDraft::Body { content_type, .. }, Key::ContentType) => content_type.clone(),
                    (ActionDraft::Replace { find, .. }, Key::Find) => find.clone(),
                    (ActionDraft::Replace { with, .. }, Key::With) => with.clone(),
                    _ => return None,
                }
            }
            _ => return None,
        })
    }

    fn set_text(&mut self, row: Row, v: String) {
        let d = &mut self.draft;
        match row {
            Row::Id => d.id = v,
            Row::Name => d.name = v,
            Row::Host => d.host.text = v,
            Row::Path => d.path.text = v,
            Row::Port => d.port = v,
            Row::QueryName(i) => {
                if let Some(q) = d.query.get_mut(i) {
                    q.0 = v;
                }
            }
            Row::QueryValue(i) => {
                if let Some(q) = d.query.get_mut(i) {
                    q.1.text = v;
                }
            }
            Row::ActionField(i, k) => {
                let Some(slot) = d.actions.get_mut(i) else { return };
                match (&mut slot.action, k) {
                    (ActionDraft::Delay { ms }, Key::Ms) => *ms = v,
                    (ActionDraft::Fail { message, .. }, Key::Message) => *message = v,
                    (ActionDraft::Status { code, .. }, Key::Code) => *code = v,
                    (ActionDraft::Status { reason, .. }, Key::Reason) => *reason = v,
                    (ActionDraft::Header { name, .. }, Key::Name) => *name = v,
                    (ActionDraft::Header { value, .. }, Key::Value) => *value = v,
                    (ActionDraft::Body { value, .. }, Key::Body) => *value = v,
                    (ActionDraft::Body { content_type, .. }, Key::ContentType) => *content_type = v,
                    (ActionDraft::Replace { find, .. }, Key::Find) => *find = v,
                    (ActionDraft::Replace { with, .. }, Key::With) => *with = v,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// A text that cannot be typed into on one line (a body with line breaks): `$EDITOR` it is.
    fn multi_line(&self, row: Row) -> bool {
        self.text_of(row).is_some_and(|t| t.contains('\n'))
    }

    /// Left and right on a line with choices: the previous or next one.
    fn cycle(&mut self, row: Row, dir: isize) -> bool {
        fn step<T: Copy + PartialEq>(all: &[T], cur: T, dir: isize) -> T {
            let i = all.iter().position(|x| *x == cur).unwrap_or(0) as isize;
            all[(i + dir).rem_euclid(all.len() as isize) as usize]
        }
        let d = &mut self.draft;
        match row {
            Row::Scheme => d.scheme = step(&["", "http", "https"], d.scheme.as_str(), dir).to_string(),
            Row::Host => d.host.kind = step(&PatternKind::ALL, d.host.kind, dir),
            Row::Path => d.path.kind = step(&PatternKind::ALL, d.path.kind, dir),
            Row::QueryValue(i) => {
                if let Some(q) = d.query.get_mut(i) {
                    q.1.kind = step(&PatternKind::ALL, q.1.kind, dir);
                }
            }
            Row::Action(i) => {
                let Some(slot) = d.actions.get_mut(i) else { return false };
                let kind = step(&ACTION_TYPES, slot.action.kind(), dir);
                slot.action = ActionDraft::new(kind);
            }
            Row::ActionField(i, k) => {
                let Some(slot) = d.actions.get_mut(i) else { return false };
                match (&mut slot.action, k) {
                    (ActionDraft::Fail { exception, .. }, Key::Exception) => {
                        *exception = step(&EXCEPTIONS, exception.as_str(), dir).to_string()
                    }
                    (ActionDraft::Header { op, .. }, Key::Op) => *op = step(&HEADER_OPS, op.as_str(), dir).to_string(),
                    (ActionDraft::Body { source, value, .. }, Key::Source) => {
                        *source = step(&BodySource::ALL, *source, dir);
                        value.clear();
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
        true
    }

    /// Space or Enter on an on/off line.
    fn toggle(&mut self, row: Row) -> bool {
        let d = &mut self.draft;
        match row {
            Row::Enabled => d.enabled = !d.enabled,
            Row::CacheRewrites => d.cache_rewrites = !d.cache_rewrites,
            Row::Methods => {
                let all = method_chips(d);
                let Some(m) = all.get(self.chip.min(all.len().saturating_sub(1))).cloned() else { return false };
                match d.methods.iter().position(|x| *x == m) {
                    Some(i) => {
                        d.methods.remove(i);
                    }
                    None => {
                        d.methods.push(m);
                        // in the checklist's order
                        d.methods.sort_by_key(|x| all.iter().position(|a| a == x).unwrap_or(usize::MAX));
                    }
                }
            }
            Row::ActionField(i, Key::Regex) => {
                if let Some(ActionSlot { action: ActionDraft::Replace { regex, .. }, .. }) = d.actions.get_mut(i) {
                    *regex = !*regex;
                }
            }
            _ => return false,
        }
        true
    }

    /// The checks again, and the match tried on the captured requests.
    fn recheck(&mut self, store: &traffic_police_core::store::SessionStore) {
        let others: Vec<&str> = self.others.iter().map(String::as_str).collect();
        self.problems = self.draft.problems(&others, &self.base);
        self.matches = match self.draft.wire_match() {
            None => Err("the match has a problem".into()),
            Some(m) => {
                let mut n = 0;
                let mut err = None;
                for t in store.txns() {
                    match ruleform::matches_request(&m, t) {
                        Ok(true) => n += 1,
                        Ok(false) => {}
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                }
                match err {
                    Some(e) => Err(e),
                    None => Ok((n, store.len())),
                }
            }
        };
    }

    fn move_cursor(&mut self, dir: isize, steps: usize) {
        let rows = self.rows();
        let mut c = self.cursor as isize;
        for _ in 0..steps.max(1) {
            let mut next = c + dir;
            while next >= 0 && (next as usize) < rows.len() && !rows[next as usize].selectable() {
                next += dir;
            }
            if next < 0 || next as usize >= rows.len() {
                break;
            }
            c = next;
        }
        self.cursor = c as usize;
    }

    /// The cursor on the first line of what `pred` picks.
    fn go_to(&mut self, pred: impl Fn(Row) -> bool) {
        if let Some(i) = self.rows().into_iter().position(pred) {
            self.cursor = i;
        }
    }
}

/// The methods the checklist shows: the usual ones, then any other the rule names.
fn method_chips(d: &RuleDraft) -> Vec<String> {
    let mut all: Vec<String> = METHODS.iter().map(|m| m.to_string()).collect();
    for m in &d.methods {
        if !all.contains(m) {
            all.push(m.clone());
        }
    }
    all
}

impl App {
    /// The `.traffic-police` directory, made in the working directory when there is none.
    fn rules_dir_or_create(&mut self) -> Option<PathBuf> {
        if let Some(d) = &self.rules_dir {
            return Some(d.clone());
        }
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(DIR);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.flash(format!("cannot create {}: {e}", dir.display()));
            return None;
        }
        self.rules_dir = Some(dir.clone());
        Some(dir)
    }

    fn open_form(&mut self, draft: RuleDraft, index: Option<usize>) {
        let base = self.rules_dir.clone().unwrap_or_else(|| PathBuf::from(DIR));
        let mut form = RuleForm::new(draft, index, self.rules_file.as_ref(), base);
        form.recheck(self.view_store());
        self.form = Some(form);
        self.set_view(View::Rules);
        self.focus = crate::app::Focus::List;
    }

    /// Enter in the Rules view: the selected rule in the form.
    pub fn edit_rule_in_form(&mut self) {
        let Some(f) = &self.rules_file else {
            self.flash("these rules are built into the demo; a project's come from .traffic-police/rules.toml");
            return;
        };
        if f.entries.is_empty() {
            return self.new_rule_in_form();
        }
        let index = self.rules_cursor.min(f.entries.len() - 1);
        match RuleDraft::read(&f.text, index) {
            Ok(d) => self.open_form(d, Some(index)),
            Err(e) => {
                self.flash(format!("rules.toml does not read ({e}); opening it in $EDITOR"));
                self.edit_rules();
            }
        }
    }

    /// `r` in the Rules view: a new, empty rule in the form.
    pub fn new_rule_in_form(&mut self) {
        if self.rules_dir_or_create().is_none() {
            return;
        }
        let file = self.rules_path().map(|p| rules::load(&p)).unwrap_or_default();
        self.open_form(RuleDraft::blank(&file), None);
    }

    /// `r` on a request: a new rule that matches it, in the form (nothing is written until it is
    /// saved).
    pub fn new_rule_for_request(&mut self, txn: traffic_police_core::model::TxnIdx) {
        if self.rules_dir_or_create().is_none() {
            return;
        }
        let file = self.rules_path().map(|p| rules::load(&p)).unwrap_or_default();
        let draft = RuleDraft::for_request(&file, self.view_store().txn(txn));
        self.open_form(draft, None);
        self.flash("a rule that matches the request: add what it does (a), then save (Ctrl+S)");
    }

    /// Keys while the form is open in the Rules view (the caller checked); true when used.
    pub fn form_key(&mut self, k: KeyEvent) -> bool {
        let Some(form) = &mut self.form else { return false };
        let row = form.row();
        if let Some((input, before)) = &mut form.editing {
            match k.code {
                KeyCode::Esc => {
                    let before = before.clone();
                    form.set_text(row, before);
                    form.editing = None;
                }
                KeyCode::Enter | KeyCode::Tab => {
                    form.editing = None;
                    if k.code == KeyCode::Tab {
                        form.move_cursor(1, 1);
                    }
                }
                _ => {
                    input.handle_event(&Event::Key(k));
                    let v = input.value().to_string();
                    form.set_text(row, v);
                }
            }
            let store = self.view_store().clone();
            if let Some(form) = &mut self.form {
                form.recheck(&store);
            }
            return true;
        }
        // Space toggles, as in any checklist
        if k.code == KeyCode::Char(' ') && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            let row = form.row();
            if form.toggle(row) {
                let store = self.view_store().clone();
                if let Some(form) = &mut self.form {
                    form.recheck(&store);
                }
            }
            return true;
        }
        let Some(a) = self.keymap.action(&k) else { return true };
        self.form_action(a);
        true
    }

    fn form_action(&mut self, a: Action) {
        let page = usize::from(self.area.height / 2).max(1);
        let Some(form) = &mut self.form else { return };
        let row = form.row();
        let mut changed = false;
        match a {
            Action::Up | Action::FocusPrev => form.move_cursor(-1, 1),
            Action::Down | Action::FocusNext => form.move_cursor(1, 1),
            Action::PageUp | Action::HalfPageUp => form.move_cursor(-1, page),
            Action::PageDown | Action::HalfPageDown => form.move_cursor(1, page),
            Action::Top => form.move_cursor(-1, usize::MAX >> 1),
            Action::Bottom => form.move_cursor(1, usize::MAX >> 1),
            Action::Left | Action::Right => {
                let dir = if a == Action::Left { -1 } else { 1 };
                if row == Row::Methods {
                    let n = method_chips(&form.draft).len();
                    form.chip = (form.chip as isize + dir).clamp(0, n as isize - 1) as usize;
                } else {
                    changed = form.cycle(row, dir);
                }
            }
            Action::Activate => match row {
                Row::AddQuery => {
                    form.draft.query.push((String::new(), PatternDraft { kind: PatternKind::Glob, text: "*".into() }));
                    let i = form.draft.query.len() - 1;
                    form.go_to(|r| r == Row::QueryName(i));
                    form.editing = Some((Input::new(String::new()), String::new()));
                    changed = true;
                }
                Row::AddAction => return self.form_choose_action(),
                Row::Matches => return self.form_show_matches(),
                r if form.multi_line(r) => {
                    self.flash("this text has line breaks: E edits it in $EDITOR");
                    return;
                }
                r => match form.text_of(r) {
                    Some(t) => form.editing = Some((Input::new(t.clone()), t)),
                    None => changed = form.toggle(r) || form.cycle(r, 1),
                },
            },
            Action::Pause => changed = form.toggle(row),
            Action::AddItem => return self.form_choose_action(),
            Action::RemoveItem => match row {
                Row::Action(i) | Row::ActionField(i, _) => {
                    form.draft.actions.remove(i);
                    form.cursor = form.cursor.min(form.rows().len().saturating_sub(1));
                    changed = true;
                }
                Row::QueryName(i) | Row::QueryValue(i) => {
                    form.draft.query.remove(i);
                    changed = true;
                }
                _ => {}
            },
            Action::MoveUp | Action::MoveDown => {
                if let Row::Action(i) | Row::ActionField(i, _) = row {
                    let j = if a == Action::MoveUp { i.checked_sub(1) } else { Some(i + 1) };
                    if let Some(j) = j.filter(|j| *j < form.draft.actions.len()) {
                        form.draft.actions.swap(i, j);
                        form.go_to(|r| r == Row::Action(j));
                        changed = true;
                    }
                }
            }
            Action::SaveRule => return self.form_save(),
            Action::EditFile => {
                if form.changed() {
                    self.flash("save the form first (Ctrl+S), or leave it (Esc), then E edits the file");
                    return;
                }
                let line =
                    form.index.and_then(|i| self.rules_file.as_ref().and_then(|f| f.entries.get(i))).map(|e| e.line);
                self.form = None;
                if let Some(line) = line
                    && let Some(path) = self.rules_path()
                {
                    self.editor_request = Some((path, line as u32));
                } else {
                    self.edit_rules();
                }
                return;
            }
            Action::Back | Action::Quit => return self.form_leave(),
            Action::Help | Action::Palette => return self.run(a),
            Action::ViewConnections | Action::ViewThreads => return self.run(a),
            _ => {}
        }
        if changed {
            let store = self.view_store().clone();
            if let Some(form) = &mut self.form {
                form.recheck(&store);
            }
        }
    }

    /// The action type to add, as a menu (`a`, or Enter on "add an action").
    fn form_choose_action(&mut self) {
        let items = ACTION_TYPES
            .iter()
            .map(|t| {
                let (key, what) = match *t {
                    "delay" => ('d', "delay: wait before the request goes out"),
                    "fail" => ('f', "fail: an IOException instead of a response"),
                    "status" => ('s', "status: another status code and reason"),
                    "header" => ('h', "header: add, set or remove a response header"),
                    "body" => ('b', "body: another response body"),
                    _ => ('r', "replace: find and replace in the response body"),
                };
                MenuItem { key, label: what.to_string(), action: MenuAction::AddRuleAction(t) }
            })
            .collect();
        self.menu = Some(Menu { title: "add an action".into(), items, cursor: 0 });
        self.overlay = Overlay::Menu;
    }

    /// The menu's choice: a new action of that type, after the one the cursor is on.
    pub(crate) fn form_add_action(&mut self, kind: &str) {
        let store = self.view_store().clone();
        let Some(form) = &mut self.form else { return };
        let at = match form.row() {
            Row::Action(i) | Row::ActionField(i, _) => i + 1,
            _ => form.draft.actions.len(),
        };
        form.draft.actions.insert(at, ActionSlot { action: ActionDraft::new(kind), origin: None });
        form.go_to(|r| r == Row::Action(at));
        // straight to its first value
        form.move_cursor(1, 1);
        form.recheck(&store);
    }

    /// Enter on the match line: the Connection View lists just what the match selects.
    fn form_show_matches(&mut self) {
        let Some(form) = &self.form else { return };
        let Some(m) = form.draft.wire_match() else {
            self.flash("the match has a problem; fix it to see what it selects");
            return;
        };
        let label = format!("(the rule form's match: {})", form.draft.id);
        let filter = traffic_police_core::filter::Filter::rule_match(&label, m);
        self.set_filter_object(filter);
        self.set_view(View::Connections);
        self.flash("the requests the rule's match selects (/ to change the filter; 3 back to the form)");
    }

    fn form_leave(&mut self) {
        let Some(form) = &self.form else { return };
        if form.changed() {
            self.overlay = Overlay::ConfirmDiscard;
        } else {
            self.form = None;
        }
    }

    /// `y` in the "discard your changes?" question.
    pub(crate) fn form_discard(&mut self) {
        self.form = None;
        self.flash("the changes were left unsaved");
    }

    /// Ctrl+S: the rule into `rules.toml`, if it has no problems.
    fn form_save(&mut self) {
        let Some(form) = &mut self.form else { return };
        form.editing = None;
        if !form.problems.is_empty() {
            let n = form.problems.len();
            let first = form.problems[0].1.clone();
            self.flash(format!("{n} problem{} to fix first: {first}", if n == 1 { "" } else { "s" }));
            return;
        }
        let Some(dir) = self.rules_dir_or_create() else { return };
        let path = dir.join(rules::FILE);
        let Some(form) = &self.form else { return };
        let current = rules::load(&path);
        // the file may have changed since the form opened (an edit in $EDITOR): find the rule by
        // the id it had then
        let index = form.index.and_then(|i| {
            let id = &form.opened.id;
            current.entries.iter().position(|e| e.id == *id).or((i < current.entries.len()).then_some(i))
        });
        if form.index.is_some() && index.is_none() {
            self.flash("the rule is no longer in rules.toml; Esc leaves the form");
            return;
        }
        let text = match form.draft.save_into(&current.text, index) {
            Ok(t) => t,
            Err(e) => return self.flash(format!("rules.toml does not read ({e}); E opens it in $EDITOR")),
        };
        let check = rules::parse(&text, &dir);
        if let Some(p) = check.problems.first() {
            return self.flash(format!("not saved: {p}"));
        }
        let id = form.draft.id.clone();
        if let Err(e) = std::fs::write(&path, text) {
            return self.flash(format!("cannot write {}: {e}", path.display()));
        }
        self.rules_reloaded(rules::load(&path));
        if let Some(i) = self.rules_file.as_ref().and_then(|f| f.entries.iter().position(|e| e.id == id)) {
            self.rules_cursor = i;
        }
        self.form = None;
        self.flash(format!("rule {id} saved to rules.toml"));
    }

    /// `K` / `J` in the Rules view: the selected rule up or down in the file (rules apply in
    /// file order).
    pub fn move_rule(&mut self, up: bool) {
        let Some(f) = &self.rules_file else {
            self.flash("these rules are built into the demo; a project's come from .traffic-police/rules.toml");
            return;
        };
        let n = f.entries.len();
        let from = self.rules_cursor.min(n.saturating_sub(1));
        let Some(to) = (if up { from.checked_sub(1) } else { Some(from + 1).filter(|t| *t < n) }) else { return };
        let path = f.path.clone();
        match ruleform::move_rule(&f.text, from, to) {
            Ok(text) => match std::fs::write(&path, text) {
                Ok(()) => {
                    let id = f.entries[from].id.clone();
                    self.rules_reloaded(rules::load(&path));
                    self.rules_cursor = to;
                    self.flash(format!(
                        "rule {id} moved {} (rules apply in this order)",
                        if up { "up" } else { "down" }
                    ));
                }
                Err(e) => self.flash(format!("cannot write {}: {e}", path.display())),
            },
            Err(e) => self.flash(format!("rules.toml does not read ({e})")),
        }
    }
}

// --- drawing ---------------------------------------------------------------------------------

/// The form in the Rules view's box.
pub fn draw(app: &mut App, r: Rect, buf: &mut Buffer) {
    let t = app.theme.clone();
    let Some(form) = &mut app.form else { return };
    let rows = form.rows();
    form.cursor = form.cursor.min(rows.len().saturating_sub(1));
    let label_w: u16 = 18;
    let foot_h: u16 = 2;
    let body_h = usize::from(r.height.saturating_sub(foot_h)).max(1);
    // problems show on the line after their field's, so a field and its problem stay together
    let mut lines: Vec<(Option<usize>, Vec<Span<'static>>)> = Vec::new();
    let mut cursor_line = 0;
    for (i, row) in rows.iter().enumerate() {
        if i == form.cursor {
            cursor_line = lines.len();
        }
        let selected = i == form.cursor;
        lines.push((Some(i), row_spans(form, *row, selected, &t, usize::from(r.width.saturating_sub(label_w + 2)))));
        if let Some(field) = row.field() {
            for (_, p) in form.problems.iter().filter(|(f, _)| *f == field) {
                lines.push((
                    None,
                    vec![Span::raw(" ".repeat(usize::from(label_w))), Span::styled(format!("✗ {p}"), t.error())],
                ));
            }
        }
        if *row == Row::Id {
            for (_, p) in form.problems.iter().filter(|(f, _)| *f == Field::Rule) {
                lines.push((None, vec![Span::raw("  "), Span::styled(format!("✗ {p}"), t.error())]));
            }
        }
    }
    // keep the cursor in view
    if cursor_line < form.top {
        form.top = cursor_line;
    } else if cursor_line >= form.top + body_h {
        form.top = cursor_line + 1 - body_h;
    }
    form.top = form.top.min(lines.len().saturating_sub(1));
    let mut hits = Vec::new();
    for (k, (row, spans)) in lines.iter().skip(form.top).take(body_h).enumerate() {
        let y = r.y + k as u16;
        if row.is_some_and(|i| i == form.cursor)
            && let Some(bg) = t.selected_bg()
        {
            crate::ui::fill(buf, Rect { x: r.x, y, width: r.width, height: 1 }, t.selected().bg(bg));
        }
        crate::ui::text(buf, r.x, y, r.width, spans.clone());
        if let Some(i) = row {
            hits.push((Rect { x: r.x, y, width: r.width, height: 1 }, Target::FormRow(*i)));
        }
    }
    // the cursor of the field being typed
    let editing_cursor = form.editing.as_ref().map(|(input, _)| {
        let shown_from = input.visual_scroll(usize::from(r.width.saturating_sub(label_w + 4)));
        (input.visual_cursor().saturating_sub(shown_from) as u16, cursor_line)
    });
    // the bottom: what the match selects, and the keys
    let status = match &form.matches {
        Ok((0, 0)) => Span::styled("no requests captured yet to try the match on".to_string(), t.dim()),
        Ok((n, of)) => Span::styled(
            format!("the match selects {n} of the {of} captured requests (Enter on the last line lists them)"),
            if *n > 0 { t.accent() } else { t.dim() },
        ),
        Err(e) => Span::styled(e.clone(), t.warn()),
    };
    let foot_y = r.y + r.height.saturating_sub(foot_h);
    crate::ui::text(buf, r.x, foot_y, r.width, vec![status]);
    let problems = form.problems.len();
    let state = match (form.is_new(), form.changed(), problems) {
        (_, _, n) if n > 0 => Span::styled(format!("{n} problem{} · ", if n == 1 { "" } else { "s" }), t.error()),
        (true, ..) => Span::styled("new rule · ".to_string(), t.accent()),
        (false, true, _) => Span::styled("changed · ".to_string(), t.accent()),
        _ => Span::styled(String::new(), t.dim()),
    };
    let key = |a: Action| app.keymap.key_label(a);
    let keys = format!(
        "{} save · {} leave · {} add action · {} remove · {}/{} move · {} $EDITOR",
        key(Action::SaveRule),
        key(Action::Back),
        key(Action::AddItem),
        key(Action::RemoveItem),
        key(Action::MoveUp),
        key(Action::MoveDown),
        key(Action::EditFile)
    );
    crate::ui::text(buf, r.x, foot_y + 1, r.width, vec![state, Span::styled(keys, t.faint())]);
    for (rect, target) in hits {
        app.hits.add(rect, target);
    }
    if let Some((x, line)) = editing_cursor
        && line >= app.form.as_ref().map_or(0, |f| f.top)
    {
        let top = app.form.as_ref().map_or(0, |f| f.top);
        let y = r.y + (line - top) as u16;
        if y < foot_y {
            app.cursor_position = Some((r.x + label_w + x, y));
        }
    }
}

fn row_spans(form: &RuleForm, row: Row, selected: bool, t: &crate::theme::Theme, width: usize) -> Vec<Span<'static>> {
    let d = &form.draft;
    let label = |s: &str| {
        Span::styled(format!("  {:<16}", crate::ui::truncate(s, 15)), if selected { t.accent() } else { t.dim() })
    };
    let value = |s: String| Span::styled(crate::ui::truncate(&s, width), t.text());
    let faint = |s: &str| Span::styled(s.to_string(), t.faint());
    let pattern = |p: &PatternDraft| -> Vec<Span<'static>> {
        if p.text.is_empty() && !(selected && form.editing.is_some()) {
            return vec![faint("any"), faint(&format!("  ({} · ←→ exact, glob, regex)", p.kind.name()))];
        }
        vec![Span::styled(format!("{:<6}", p.kind.name()), t.accent()), value(p.text.clone())]
    };
    let editing = || -> Option<Vec<Span<'static>>> {
        if !selected {
            return None;
        }
        let (input, _) = form.editing.as_ref()?;
        let shown_from = input.visual_scroll(width.saturating_sub(2));
        let v: String = input.value().chars().skip(shown_from).collect();
        Some(vec![Span::styled(crate::ui::truncate(&v, width), t.text().add_modifier(Modifier::UNDERLINED))])
    };
    let mut spans = match row {
        Row::Section(s) => return vec![Span::styled(s.to_string(), t.title())],
        Row::Blank => return vec![],
        Row::Id => vec![label("id"), value(d.id.clone())],
        Row::Name => vec![label("name"), if d.name.is_empty() { faint("(none)") } else { value(d.name.clone()) }],
        Row::Enabled => vec![
            label("on"),
            Span::styled(
                if d.enabled { "● yes" } else { "○ no" }.to_string(),
                if d.enabled { t.accent() } else { t.dim() },
            ),
        ],
        Row::CacheRewrites => vec![
            label("cache rewrites"),
            value(if d.cache_rewrites { "yes" } else { "no" }.into()),
            faint(if d.cache_rewrites {
                "  (the app's HTTP cache may keep what a rule changed)"
            } else {
                "  (a changed response gets Cache-Control: no-store)"
            }),
        ],
        Row::Methods => {
            let mut s = vec![label("methods")];
            for (i, m) in method_chips(d).iter().enumerate() {
                let on = d.methods.contains(m);
                let style = if selected && i == form.chip {
                    t.accent().add_modifier(Modifier::REVERSED)
                } else if on {
                    t.text()
                } else {
                    t.faint()
                };
                s.push(Span::styled(format!("{} {m}", if on { "[x]" } else { "[ ]" }), style));
                s.push(Span::raw("  "));
            }
            if d.methods.is_empty() {
                s.push(faint("(none ticked: any method)"));
            }
            s
        }
        Row::Scheme => vec![label("scheme"), if d.scheme.is_empty() { faint("any") } else { value(d.scheme.clone()) }],
        Row::Host => [vec![label("host")], pattern(&d.host)].concat(),
        Row::Path => [vec![label("path")], pattern(&d.path)].concat(),
        Row::Port => vec![label("port"), if d.port.is_empty() { faint("any") } else { value(d.port.clone()) }],
        Row::QueryName(i) => {
            vec![label("query parameter"), value(d.query.get(i).map(|q| q.0.clone()).unwrap_or_default())]
        }
        Row::QueryValue(i) => {
            let p = d.query.get(i).map(|q| q.1.clone()).unwrap_or_default();
            [vec![label("  its value")], pattern(&p)].concat()
        }
        Row::AddQuery => vec![faint("  + a query parameter (Enter)")],
        Row::Action(i) => {
            let kind = d.actions.get(i).map(|a| a.action.kind()).unwrap_or("");
            vec![
                Span::styled(format!("  {:<16}", format!("{}.", i + 1)), if selected { t.accent() } else { t.dim() }),
                Span::styled(kind.to_string(), t.title()),
                faint("  (←→ another type)"),
            ]
        }
        Row::ActionField(i, k) => {
            let a = d.actions.get(i).map(|s| s.action.clone());
            let (name, shown): (&str, Vec<Span<'static>>) = match (a, k) {
                (Some(ActionDraft::Delay { ms }), Key::Ms) => ("ms", vec![value(ms)]),
                (Some(ActionDraft::Fail { exception, .. }), Key::Exception) => {
                    ("exception", vec![value(exception), faint("  (←→)")])
                }
                (Some(ActionDraft::Fail { message, .. }), Key::Message) => {
                    ("message", vec![if message.is_empty() { faint("(the exception's own)") } else { value(message) }])
                }
                (Some(ActionDraft::Status { code, .. }), Key::Code) => ("code", vec![value(code)]),
                (Some(ActionDraft::Status { reason, .. }), Key::Reason) => {
                    ("reason", vec![if reason.is_empty() { faint("(the code's usual)") } else { value(reason) }])
                }
                (Some(ActionDraft::Header { op, .. }), Key::Op) => {
                    ("op", vec![value(op), faint("  (←→ set, add, remove)")])
                }
                (Some(ActionDraft::Header { name, .. }), Key::Name) => ("header", vec![value(name)]),
                (Some(ActionDraft::Header { value: v, .. }), Key::Value) => ("value", vec![value(v)]),
                (Some(ActionDraft::Body { source, .. }), Key::Source) => {
                    ("from", vec![value(source.key().into()), faint("  (←→ text, a file in .traffic-police/, base64)")])
                }
                (Some(ActionDraft::Body { value: v, source, .. }), Key::Body) => {
                    let shown = if v.contains('\n') {
                        format!("{} … ({} lines: E edits it)", v.lines().next().unwrap_or(""), v.lines().count())
                    } else {
                        v
                    };
                    (source.key(), vec![value(shown)])
                }
                (Some(ActionDraft::Body { content_type, .. }), Key::ContentType) => (
                    "content type",
                    vec![if content_type.is_empty() { faint("(the response's)") } else { value(content_type) }],
                ),
                (Some(ActionDraft::Replace { find, .. }), Key::Find) => ("find", vec![value(find)]),
                (Some(ActionDraft::Replace { with, .. }), Key::With) => ("with", vec![value(with)]),
                (Some(ActionDraft::Replace { regex, .. }), Key::Regex) => {
                    ("as a regex", vec![value(if regex { "yes" } else { "no (literal text)" }.into())])
                }
                _ => ("", vec![]),
            };
            [vec![Span::styled(format!("    {:<14}", name), if selected { t.accent() } else { t.dim() })], shown]
                .concat()
        }
        Row::AddAction => vec![faint("  + an action (Enter, or a)")],
        Row::Matches => vec![Span::styled(
            "  try the match: list the captured requests it selects (Enter)".to_string(),
            if selected { t.accent() } else { t.dim() },
        )],
    };
    if let Some(e) = editing()
        && form.text_of(row).is_some()
    {
        spans.truncate(1);
        if matches!(row, Row::Host | Row::Path | Row::QueryValue(_)) {
            let kind = match row {
                Row::Host => d.host.kind,
                Row::Path => d.path.kind,
                Row::QueryValue(i) => d.query.get(i).map(|q| q.1.kind).unwrap_or_default(),
                _ => PatternKind::Glob,
            };
            spans.push(Span::styled(format!("{:<6}", kind.name()), t.accent()));
        }
        spans.extend(e);
    }
    spans
}
