//! Rules in the UI (ARCHITECTURE.md §5.11): `.traffic-police/rules.toml` is watched, with the
//! files its bodies come from, and a valid change goes to the app at once; an invalid one is
//! shown while the active rules stay. In the Rules view Space turns a rule on or off and Enter
//! opens the file at it in `$EDITOR`; `r` on a request writes a new rule that matches it.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use traffic_police_core::backend::BackendCommand;
use traffic_police_core::project::DIR;
use traffic_police_core::rules::{self, FILE, NewRule, RulesFile};

use crate::app::App;

/// How often the file is looked at, and how long it must stay unchanged before it is read.
const POLL: Duration = Duration::from_millis(250);
const SETTLE: Duration = Duration::from_millis(200);

/// Watches the rules file and the files its body actions read (polling: it works the same on
/// every platform and needs no dependency), and sends the rules, read again, after each change
/// has settled.
pub async fn watch(path: PathBuf, tx: mpsc::UnboundedSender<RulesFile>) {
    let stamp = |paths: &[PathBuf]| -> Vec<Option<(SystemTime, u64)>> {
        paths
            .iter()
            .map(|p| std::fs::metadata(p).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len())))
            .collect()
    };
    let watched = |f: &RulesFile| -> Vec<PathBuf> { std::iter::once(path.clone()).chain(f.files.clone()).collect() };
    let mut paths = watched(&rules::load(&path));
    let mut last = stamp(&paths);
    loop {
        tokio::time::sleep(POLL).await;
        let now = stamp(&paths);
        if now == last {
            continue;
        }
        // wait until the editor has finished writing
        let mut settled = now;
        loop {
            tokio::time::sleep(SETTLE).await;
            let again = stamp(&paths);
            if again == settled {
                break;
            }
            settled = again;
        }
        let f = rules::load(&path);
        let now_watched = watched(&f);
        // a file named for the first time is stamped as it is now
        last = if now_watched == paths { settled } else { stamp(&now_watched) };
        paths = now_watched;
        if tx.send(f).is_err() {
            return;
        }
    }
}

impl App {
    /// The rules file's path, when the project has a `.traffic-police` directory.
    pub fn rules_path(&self) -> Option<PathBuf> {
        self.rules_dir.as_ref().map(|d| d.join(FILE))
    }

    /// The file was read again: a valid one becomes the active rules (and goes to the app when
    /// it changed); an invalid one is reported and the active rules stay.
    pub fn rules_reloaded(&mut self, f: RulesFile) {
        if f.is_valid() {
            if self.rules.as_ref() != Some(&f.set) {
                self.rules = Some(f.set.clone());
                if let Some(tx) = &self.commands {
                    let _ = tx.send(BackendCommand::SetRules(f.set.clone()));
                }
                let n = f.set.rules.iter().filter(|r| r.enabled).count();
                self.flash(format!("rules.toml read: {n} rule{} on", if n == 1 { "" } else { "s" }));
            }
        } else {
            let first = f.problems[0].to_string();
            self.flash(format!("rules.toml: {first} (the rules that were active stay active)"));
        }
        let n = f.entries.len();
        self.rules_file = Some(f);
        self.rules_cursor = self.rules_cursor.min(n.saturating_sub(1));
    }

    /// Space in the Rules view: the selected rule on or off, written into the file.
    pub fn toggle_rule(&mut self) {
        let Some(f) = &self.rules_file else {
            self.flash("these rules are built into the demo; a project's come from .traffic-police/rules.toml");
            return;
        };
        let Some(e) = f.entries.get(self.rules_cursor).cloned() else { return };
        let Some(text) = rules::with_enabled(f, &e.id, !e.enabled) else { return };
        let path = f.path.clone();
        match std::fs::write(&path, text) {
            Ok(()) => {
                self.rules_reloaded(rules::load(&path));
                self.flash(format!("rule {} {}", e.id, if e.enabled { "off" } else { "on" }));
            }
            Err(err) => self.flash(format!("cannot write {}: {err}", path.display())),
        }
    }

    /// Enter in the Rules view: the file in `$EDITOR`, at the selected rule.
    pub fn edit_rules(&mut self) {
        let Some(path) = self.rules_path() else {
            self.flash("no .traffic-police directory here; r on a request creates one with a rule");
            return;
        };
        let line = self.rules_file.as_ref().and_then(|f| f.entries.get(self.rules_cursor)).map_or(1, |e| e.line);
        if !path.exists() {
            let header = "# Rules for this app (docs/PROTOCOL.md §8); saved changes apply at once.\nversion = 1\n";
            if let Err(e) = std::fs::write(&path, header) {
                self.flash(format!("cannot create {}: {e}", path.display()));
                return;
            }
        }
        self.editor_request = Some((path, line as u32));
    }

    /// `r`: a rule that matches the selected request exactly, appended (off) to rules.toml and
    /// opened in `$EDITOR`.
    pub fn new_rule_from_selected(&mut self) {
        let Some(txn) = self.selected else {
            self.flash("select a request to make a rule for it");
            return;
        };
        if self.rules_dir.is_none() {
            let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(DIR);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                self.flash(format!("cannot create {}: {e}", dir.display()));
                return;
            }
            self.rules_dir = Some(dir);
        }
        let path = self.rules_path().expect("set above");
        let current = rules::load(&path);
        let t = self.view_store().txn(txn);
        let mut query: Vec<String> = Vec::new();
        for (name, _) in t.url.query_pairs() {
            if !query.contains(&name) {
                query.push(name);
            }
        }
        let new = NewRule {
            id: rules::fresh_id(&current, &t.url.path),
            name: format!("{} {}", t.method, t.url.path),
            method: t.method.clone(),
            scheme: t.url.scheme.clone(),
            host: t.url.host.clone(),
            port: t.url.effective_port().unwrap_or(443),
            path: t.url.path.clone(),
            query,
        };
        let (text, line) = rules::with_new_rule(&current.text, &new);
        match std::fs::write(&path, text) {
            Ok(()) => {
                self.rules_reloaded(rules::load(&path));
                self.editor_request = Some((path, line as u32));
                self.flash(format!("rule {} added (off) to rules.toml; say what it does, then turn it on", new.id));
            }
            Err(e) => self.flash(format!("cannot write {}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use traffic_police_proto::msg::RuleAction;

    use super::*;

    #[tokio::test]
    async fn a_changed_body_file_is_read_again() {
        let dir = std::env::temp_dir().join(format!("tp-rules-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("fixtures")).unwrap();
        let path = dir.join(FILE);
        let text = "version = 1\n[[rule]]\nid = \"stub\"\n  [[rule.action]]\n  type = \"body\"\n  file = \"fixtures/b.json\"\n";
        std::fs::write(&path, text).unwrap();
        std::fs::write(dir.join("fixtures/b.json"), "{\"v\":1}").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let watcher = tokio::spawn(watch(path, tx));
        // its first look, then only the body file changes (in length too, for coarse clocks)
        tokio::time::sleep(POLL * 2).await;
        std::fs::write(dir.join("fixtures/b.json"), "{\"v\":22}").unwrap();
        let f = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("read again").expect("sent");
        watcher.abort();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            f.set.rules[0].actions,
            vec![RuleAction::Body { text: Some("{\"v\":22}".into()), base64: None, content_type: None }]
        );
    }
}
