//! Getting data out: the clipboard, the copy menu (`y`), saving a body (`w`), and the export
//! menu (`e`) (ARCHITECTURE.md §5.10).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine;
use traffic_police_core::decode::decode_body;
use traffic_police_core::export::curl::{CurlBody, curl};
use traffic_police_core::export::har::har;
use traffic_police_core::model::{BodyDir, TxnIdx, header};
use tui_input::Input;

use crate::app::{App, Overlay, Tab};

/// Where copied text goes (`clipboard` in the user config).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClipboardMode {
    /// OSC 52 over SSH or when no clipboard tool is found, the system clipboard otherwise.
    #[default]
    Auto,
    Osc52,
    Native,
    /// Keep it in traffic-police only (tests; `clipboard = "off"`).
    Off,
}

impl ClipboardMode {
    pub fn parse(s: &str) -> Option<ClipboardMode> {
        match s {
            "auto" => Some(ClipboardMode::Auto),
            "osc52" => Some(ClipboardMode::Osc52),
            "native" => Some(ClipboardMode::Native),
            "off" => Some(ClipboardMode::Off),
            _ => None,
        }
    }
}

fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"].iter().any(|v| std::env::var_os(v).is_some())
}

/// The system clipboard's command, if one is installed.
fn native_tool() -> Option<(&'static str, &'static [&'static str])> {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("clip", &[])]
    } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    } else {
        &[("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    candidates.iter().copied().find(|(cmd, _)| which(cmd))
}

fn which(cmd: &str) -> bool {
    let exe = if cfg!(windows) { format!("{cmd}.exe") } else { cmd.to_string() };
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&exe).is_file()))
}

fn copy_native(tool: (&str, &[&str]), text: &str) -> Result<(), String> {
    let mut child = Command::new(tool.0)
        .args(tool.1)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", tool.0))?;
    child.stdin.take().expect("piped").write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("{} exited with {status}", tool.0)) }
}

/// OSC 52: the terminal (or the one at the other end of SSH) sets its clipboard.
fn copy_osc52(text: &str) -> Result<(), String> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{b64}\x07").and_then(|_| out.flush()).map_err(|e| e.to_string())
}

/// Copies text; returns how, for the message.
pub fn copy(mode: ClipboardMode, text: &str) -> Result<&'static str, String> {
    match mode {
        ClipboardMode::Off => Ok("kept in traffic-police (clipboard is off)"),
        ClipboardMode::Osc52 => copy_osc52(text).map(|_| "copied (OSC 52)"),
        ClipboardMode::Native => {
            let tool = native_tool().ok_or("no clipboard tool found (pbcopy, wl-copy, xclip, xsel or clip)")?;
            copy_native(tool, text).map(|_| "copied")
        }
        ClipboardMode::Auto => {
            if !over_ssh()
                && let Some(tool) = native_tool()
                && copy_native(tool, text).is_ok()
            {
                return Ok("copied");
            }
            copy_osc52(text).map(|_| {
                if std::env::var_os("TMUX").is_some() {
                    "copied with OSC 52 (inside tmux this needs `set -g set-clipboard on`)"
                } else {
                    "copied (OSC 52)"
                }
            })
        }
    }
}

/// What a menu entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    CopyCurl,
    CopyUrl,
    CopyRequestHeaders,
    CopyResponseHeaders,
    CopyBody(BodyDir),
    CopyHeader(String, String),
    CopyValue(String),
    ExportHar(Scope),
    ExportSession(Scope),
}

/// Which requests an export covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    All,
    Listed,
    Selected,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub key: char,
    pub label: String,
    pub action: MenuAction,
}

#[derive(Debug, Clone)]
pub struct Menu {
    pub title: &'static str,
    pub items: Vec<MenuItem>,
    pub cursor: usize,
}

/// What a path typed in the bottom line is for.
#[derive(Debug, Clone)]
pub enum PromptAction {
    SaveBody(TxnIdx, BodyDir),
    Har(Vec<TxnIdx>),
    /// `None`: every request.
    Session(Option<Vec<TxnIdx>>),
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub label: &'static str,
    pub input: Input,
    pub action: PromptAction,
}

/// A file name from the last URL segment, safe on every platform.
fn safe_name(s: &str) -> String {
    let s: String =
        s.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "body".into() } else { s.chars().take(60).collect() }
}

/// An extension for a body: from Content-Type, else from the first bytes.
fn extension(content_type: Option<&str>, bytes: &[u8]) -> &'static str {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let by_type = [
        ("json", "json"),
        ("html", "html"),
        ("xml", "xml"),
        ("png", "png"),
        ("jpeg", "jpg"),
        ("gif", "gif"),
        ("webp", "webp"),
        ("protobuf", "pb"),
        ("grpc", "grpc"),
        ("javascript", "js"),
        ("css", "css"),
        ("pdf", "pdf"),
        ("x-www-form-urlencoded", "txt"),
        ("octet-stream", "bin"),
        ("text/", "txt"),
    ];
    if let Some((_, ext)) = by_type.iter().find(|(k, _)| ct.contains(k)) {
        return ext;
    }
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "png",
        [0xff, 0xd8, 0xff, ..] => "jpg",
        [b'G', b'I', b'F', ..] => "gif",
        [b'%', b'P', b'D', b'F', ..] => "pdf",
        [b'{', ..] | [b'[', ..] => "json",
        _ if std::str::from_utf8(bytes)
            .is_ok_and(|s| !s.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))) =>
        {
            "txt"
        }
        _ => "bin",
    }
}

/// `name`, or `name-2`, `name-3`… so nothing is overwritten.
fn unused_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..).map(|n| dir.join(format!("{stem}-{n}.{ext}"))).find(|p| !p.exists()).expect("some name is free")
}

fn timestamp_name() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let iso = traffic_police_core::fmt::iso8601(ms);
    // 2026-09-30T14:32:05.123+05:30 → 2026-09-30-1432
    format!("traffic-police-{}-{}{}", &iso[..10], &iso[11..13], &iso[14..16])
}

impl App {
    /// The body direction the detail pane is on (the Request tab means the request body).
    fn body_dir_here(&self, txn: TxnIdx) -> BodyDir {
        if self.detail_open && self.detail.tab == Tab::Request { BodyDir::Request } else { self.response_dir(txn) }
    }

    /// The header on the detail cursor's row, when the cursor is on one.
    fn header_at_cursor(&mut self, txn: TxnIdx) -> Option<(String, String)> {
        if !self.detail_open || !matches!(self.detail.tab, Tab::Request | Tab::Response) {
            return None;
        }
        let doc = crate::detail::build_doc(self);
        let text = crate::detail::row_text(self, &doc, self.detail.cursor, txn)?;
        let t = self.view_store().txn(txn);
        let headers = if self.detail.tab == Tab::Request { &t.req_headers } else { &t.resp.as_ref()?.headers };
        headers.iter().find(|(n, v)| text == format!("{n}: {v}")).cloned()
    }

    /// The JSON path of the detail cursor's body line, when it has one.
    fn path_at_cursor(&mut self, txn: TxnIdx) -> Option<String> {
        if !self.detail_open || !matches!(self.detail.tab, Tab::Request | Tab::Response) {
            return None;
        }
        let doc = crate::detail::build_doc(self);
        let crate::detail::DocRow::Body(i) = doc.row(self.detail.cursor)? else { return None };
        let dir = doc.body_dir?;
        let parsed = self.detail.parsed;
        self.body_view(txn, dir)?.path_at(i, parsed)
    }

    pub fn open_copy_menu(&mut self) {
        let Some(txn) = self.selected else {
            self.flash("select a request to copy from it");
            return;
        };
        let mut items = vec![
            MenuItem { key: 'c', label: "as cURL".into(), action: MenuAction::CopyCurl },
            MenuItem { key: 'u', label: "URL".into(), action: MenuAction::CopyUrl },
            MenuItem { key: 'h', label: "request headers".into(), action: MenuAction::CopyRequestHeaders },
        ];
        let (has_resp, req_body, resp_body) = {
            let t = self.view_store().txn(txn);
            (t.resp.is_some(), t.req_body.id.is_some(), t.resp_body.id.is_some() || t.delivered_body.is_some())
        };
        if has_resp {
            items.push(MenuItem {
                key: 'r',
                label: "response headers".into(),
                action: MenuAction::CopyResponseHeaders,
            });
        }
        if resp_body {
            let dir = self.response_dir(txn);
            items.push(MenuItem {
                key: 'b',
                label: "response body (decoded)".into(),
                action: MenuAction::CopyBody(dir),
            });
        }
        if req_body {
            items.push(MenuItem {
                key: 'q',
                label: "request body".into(),
                action: MenuAction::CopyBody(BodyDir::Request),
            });
        }
        if let Some((name, value)) = self.header_at_cursor(txn) {
            items.push(MenuItem {
                key: 'k',
                label: format!("header {name}"),
                action: MenuAction::CopyHeader(name, value),
            });
        }
        if let Some(path) = self.path_at_cursor(txn) {
            items.push(MenuItem { key: 'v', label: format!("value at {path}"), action: MenuAction::CopyValue(path) });
        }
        self.menu = Some(Menu { title: "copy", items, cursor: 0 });
        self.overlay = Overlay::Menu;
    }

    pub fn open_export_menu(&mut self) {
        let total = self.view_store().len();
        let listed = self.view_rows().matched();
        let mut items = vec![MenuItem {
            key: 'a',
            label: format!("all {total} requests as HAR"),
            action: MenuAction::ExportHar(Scope::All),
        }];
        if listed != total {
            items.push(MenuItem {
                key: 'l',
                label: format!("the {listed} requests in the list as HAR"),
                action: MenuAction::ExportHar(Scope::Listed),
            });
        }
        if self.selected.is_some() {
            items.push(MenuItem {
                key: 'r',
                label: "this request as HAR".into(),
                action: MenuAction::ExportHar(Scope::Selected),
            });
        }
        if self.session_log.is_some() {
            items.push(MenuItem {
                key: 's',
                label: "the session (.trafficpolice: reopens with timings, threads and stacks)".into(),
                action: MenuAction::ExportSession(Scope::All),
            });
            if listed != total {
                items.push(MenuItem {
                    key: 'S',
                    label: format!("the {listed} requests in the list as a session"),
                    action: MenuAction::ExportSession(Scope::Listed),
                });
            }
        }
        self.menu = Some(Menu { title: "export", items, cursor: 0 });
        self.overlay = Overlay::Menu;
    }

    pub fn open_save_prompt(&mut self) {
        let Some(txn) = self.selected else {
            self.flash("select a request to save a body");
            return;
        };
        let dir = self.body_dir_here(txn);
        let t = self.view_store().txn(txn);
        let (meta, headers) = match dir {
            BodyDir::Request => (Some(&t.req_body), Some(&t.req_headers)),
            BodyDir::Response => (Some(&t.resp_body), t.resp.as_ref().map(|r| &r.headers)),
            BodyDir::Delivered => (t.delivered_body.as_ref(), t.delivered.as_ref().map(|d| &d.headers)),
        };
        let Some(meta) = meta.filter(|m| m.id.is_some()) else {
            self.flash("this request has no body captured to save");
            return;
        };
        let raw = self.view_store().body_bytes(meta);
        let d = decode_body(raw, headers, 256 << 20);
        let ext = extension(headers.and_then(|h| header(h, "content-type")), &d.bytes);
        let stem = safe_name(t.url.path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("body"));
        let stem = stem.rsplit_once('.').filter(|(_, e)| *e == ext).map_or(stem.clone(), |(s, _)| s.to_string());
        let path = unused_path(Path::new("."), &stem, ext);
        let what = if dir == BodyDir::Request { "save the request body to" } else { "save the response body to" };
        self.prompt = Some(Prompt {
            label: what,
            input: Input::new(path.display().to_string()),
            action: PromptAction::SaveBody(txn, dir),
        });
        self.overlay = Overlay::Prompt;
    }

    /// Runs the chosen menu entry.
    pub fn run_menu(&mut self, action: MenuAction) {
        self.overlay = Overlay::None;
        self.menu = None;
        match action {
            MenuAction::ExportHar(scope) => {
                let txns = self.scope_txns(scope);
                let path = format!("./{}.har", timestamp_name());
                self.prompt =
                    Some(Prompt { label: "export HAR to", input: Input::new(path), action: PromptAction::Har(txns) });
                self.overlay = Overlay::Prompt;
                return;
            }
            MenuAction::ExportSession(scope) => {
                let keep = (scope != Scope::All).then(|| self.scope_txns(scope));
                let path = format!("./{}.trafficpolice", timestamp_name());
                self.prompt = Some(Prompt {
                    label: "save the session to",
                    input: Input::new(path),
                    action: PromptAction::Session(keep),
                });
                self.overlay = Overlay::Prompt;
                return;
            }
            _ => {}
        }
        let Some(txn) = self.selected else { return };
        let text = match action {
            MenuAction::ExportHar(_) | MenuAction::ExportSession(_) => return,
            MenuAction::CopyUrl => self.view_store().txn(txn).url.raw.clone(),
            MenuAction::CopyRequestHeaders => {
                self.view_store().txn(txn).req_headers.iter().map(|(n, v)| format!("{n}: {v}\n")).collect()
            }
            MenuAction::CopyResponseHeaders => {
                let t = self.view_store().txn(txn);
                t.resp
                    .as_ref()
                    .map(|r| r.headers.iter().map(|(n, v)| format!("{n}: {v}\n")).collect())
                    .unwrap_or_default()
            }
            MenuAction::CopyHeader(_, value) => value,
            MenuAction::CopyBody(dir) => match self.body_text(txn, dir) {
                Ok(s) => s,
                Err(e) => return self.flash(e),
            },
            MenuAction::CopyValue(path) => match self.json_value(txn, &path) {
                Some(v) => v,
                None => return self.flash(format!("no value at {path}")),
            },
            MenuAction::CopyCurl => match self.curl_text(txn) {
                Ok(s) => s,
                Err(e) => return self.flash(e),
            },
        };
        self.copy_text(text);
    }

    /// The transactions an export covers.
    fn scope_txns(&self, scope: Scope) -> Vec<TxnIdx> {
        match scope {
            Scope::All => (0..self.view_store().len() as TxnIdx).collect(),
            Scope::Listed => {
                let mut v: Vec<TxnIdx> = Vec::new();
                for r in self.view_rows().rows() {
                    match r {
                        traffic_police_core::rows::Row::Group { members, expanded: false } => v.extend(members),
                        other => v.push(other.txn()),
                    }
                }
                v.sort_unstable();
                v.dedup();
                v
            }
            Scope::Selected => self.selected.into_iter().collect(),
        }
    }

    pub fn copy_text(&mut self, text: String) {
        let n = text.chars().count();
        match copy(self.clipboard, &text) {
            Ok(how) => self.flash(format!("{how}: {n} characters")),
            Err(e) => self.flash(format!("copy failed: {e}")),
        }
        self.copied = Some(text);
    }

    /// A body as text (decoded); binary bodies are refused.
    fn body_text(&self, txn: TxnIdx, dir: BodyDir) -> Result<String, String> {
        let t = self.view_store().txn(txn);
        let (meta, headers) = match dir {
            BodyDir::Request => (Some(&t.req_body), Some(&t.req_headers)),
            BodyDir::Response => (Some(&t.resp_body), t.resp.as_ref().map(|r| &r.headers)),
            BodyDir::Delivered => (t.delivered_body.as_ref(), t.delivered.as_ref().map(|d| &d.headers)),
        };
        let meta = meta.filter(|m| m.id.is_some()).ok_or("no body captured")?;
        let d = decode_body(self.view_store().body_bytes(meta), headers, 256 << 20);
        String::from_utf8(d.bytes.to_vec()).map_err(|_| "the body is binary; w saves it to a file".to_string())
    }

    /// The JSON value at a path like `$.config.items[2].id`, as JSON text.
    fn json_value(&self, txn: TxnIdx, path: &str) -> Option<String> {
        let dir = if self.detail.tab == Tab::Request { BodyDir::Request } else { self.response_dir(txn) };
        let text = self.body_text(txn, dir).ok()?;
        let mut v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let rest = path.strip_prefix('$')?;
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '.' => {
                    let mut key = String::new();
                    while let Some(&n) = chars.peek() {
                        if n == '.' || n == '[' {
                            break;
                        }
                        key.push(n);
                        chars.next();
                    }
                    v = v.get(&key)?.clone();
                }
                '[' => {
                    let mut inner = String::new();
                    for n in chars.by_ref() {
                        if n == ']' {
                            break;
                        }
                        inner.push(n);
                    }
                    v = match inner.parse::<usize>() {
                        Ok(i) => v.get(i)?.clone(),
                        Err(_) => v.get(serde_json::from_str::<String>(&inner).ok()?.as_str())?.clone(),
                    };
                }
                _ => return None,
            }
        }
        Some(match &v {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).ok()?,
        })
    }

    /// The cURL command; a binary request body is saved next to it and referenced with `@`.
    fn curl_text(&mut self, txn: TxnIdx) -> Result<String, String> {
        let t = self.view_store().txn(txn).clone();
        if t.req_body.id.is_none() {
            return Ok(curl(&t, CurlBody::None));
        }
        let raw = self.view_store().body_bytes(&t.req_body);
        if let Ok(s) = std::str::from_utf8(&raw) {
            return Ok(curl(&t, CurlBody::Text(s)));
        }
        let path = unused_path(Path::new("."), &format!("request-body-{}", t.key.txn), "bin");
        std::fs::write(&path, &raw).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("request-body.bin").to_string();
        Ok(curl(&t, CurlBody::File(&name)))
    }

    /// Runs the prompt's action with the path typed.
    pub fn finish_prompt(&mut self) {
        let Some(p) = self.prompt.take() else { return };
        self.overlay = Overlay::None;
        let path = PathBuf::from(p.input.value().trim());
        if path.as_os_str().is_empty() {
            return self.flash("no file name given");
        }
        if path.exists() {
            self.prompt = Some(p);
            self.overlay = Overlay::Prompt;
            return self.flash(format!("{} exists; choose another name", path.display()));
        }
        let result = match p.action {
            PromptAction::SaveBody(txn, dir) => {
                let t = self.view_store().txn(txn);
                let (meta, headers) = match dir {
                    BodyDir::Request => (Some(&t.req_body), Some(&t.req_headers)),
                    BodyDir::Response => (Some(&t.resp_body), t.resp.as_ref().map(|r| &r.headers)),
                    BodyDir::Delivered => (t.delivered_body.as_ref(), t.delivered.as_ref().map(|d| &d.headers)),
                };
                match meta {
                    Some(m) => {
                        let d = decode_body(self.view_store().body_bytes(m), headers, 256 << 20);
                        std::fs::write(&path, &d.bytes).map(|_| format!("saved {} bytes", d.bytes.len()))
                    }
                    None => Ok("nothing to save".into()),
                }
            }
            PromptAction::Har(txns) => {
                let doc = har(self.view_store(), &txns, self.now());
                serde_json::to_vec_pretty(&doc)
                    .map_err(std::io::Error::other)
                    .and_then(|bytes| std::fs::write(&path, bytes))
                    .map(|_| format!("wrote {} requests", txns.len()))
            }
            PromptAction::Session(keep) => match self.session_log.clone() {
                Some(log) => {
                    let store = self.view_store();
                    let keep: Option<std::collections::HashSet<_>> =
                        keep.map(|v| v.iter().map(|&i| store.txn(i).key).collect());
                    log.export(store, &path, keep.as_ref()).map(|n| format!("saved a session of {n} requests"))
                }
                None => Ok("this session has no recording to save".into()),
            },
        };
        match result {
            Ok(what) => self.flash(format!("{what} to {}", path.display())),
            Err(e) => self.flash(format!("cannot write {}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_extensions() {
        assert_eq!(safe_name("model v3?.bin"), "model_v3_.bin");
        assert_eq!(safe_name("///"), "body");
        assert_eq!(extension(Some("application/json; charset=utf-8"), b"{}"), "json");
        assert_eq!(extension(None, b"\x89PNG\r\n"), "png");
        assert_eq!(extension(Some("application/octet-stream"), b"\x00\x01"), "bin");
        assert_eq!(extension(None, b"plain words\n"), "txt");
        assert_eq!(extension(None, b"\x00\x01"), "bin", "control bytes are not text");
    }
}
