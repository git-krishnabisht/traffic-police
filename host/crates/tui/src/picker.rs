//! The device and process pickers (ARCHITECTURE.md §5.12): `traffic-police` without a target
//! lists devices, then the chosen device's debuggable processes, marking the ones that already
//! run the capture runtime. The others can be picked too when attach mode is possible: the
//! agent is then attached to them.

use std::collections::HashMap;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use traffic_police_adb::{Adb, Device, TransportId, quote};
use unicode_width::UnicodeWidthStr;

use crate::actions::{Action, Keymap};
use crate::terminal;
use crate::theme::Theme;

/// The process the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub serial: String,
    pub package: String,
    pub process: String,
    pub pid: u32,
    /// It runs the capture runtime already (the library, or an agent attached before); if not,
    /// the agent is to be attached.
    pub capturing: bool,
}

#[derive(Debug, Clone)]
struct ProcessRow {
    pid: u32,
    name: String,
    package: String,
    capturing: bool,
    /// Held by Android's cached-apps freezer.
    frozen: bool,
    arch: Option<String>,
}

enum Snapshot {
    /// "Android 16 · API 36", read once per device.
    About(TransportId, String),
    Processes(TransportId, Result<Vec<ProcessRow>, String>),
}

async fn about(adb: &Adb, id: TransportId) -> Option<String> {
    let out = adb.shell(id, "getprop ro.build.version.release; getprop ro.build.version.sdk").await.ok()?;
    let text = out.stdout_text();
    let mut lines = text.lines().map(str::trim);
    let (release, sdk) = (lines.next()?, lines.next()?);
    Some(format!("Android {release} · API {sdk}"))
}

/// Runs the pickers; `None` when the user quits. `attach`: whether a process without the
/// capture runtime can be picked (the agent is then attached to it), or why not. The keys are
/// the user's (`[keymap]`): up, down, top, bottom, the page keys, open, back and quit.
pub async fn pick(
    adb: Adb,
    theme: Theme,
    keymap: &Keymap,
    attach: Result<(), String>,
) -> anyhow::Result<Option<Picked>> {
    terminal::install_panic_hook();
    let mut term = terminal::setup()?;
    // dropping `term` puts the terminal back
    run(&mut term, adb, theme, keymap, attach).await
}

/// Which packages are debuggable, asked of `run-as` once each. On userdebug builds (many
/// emulator images) Android runs every app with JDWP, so the process list calls them all
/// debuggable; neither mode can capture the ones `run-as` refuses. With `run-as` disabled on the
/// device, every package counts.
async fn learn_debuggable(adb: &Adb, id: TransportId, packages: &[&str], known: &mut HashMap<String, bool>) {
    let unknown: Vec<&str> = packages.iter().copied().filter(|p| !known.contains_key(*p)).collect();
    if unknown.is_empty() {
        return;
    }
    let list: Vec<String> = unknown.iter().map(|p| quote(p)).collect();
    let cmd = format!(
        "[ \"$(getprop ro.boot.disable_runas)\" = 1 ] && echo '*' && exit 0; for p in {}; do run-as \"$p\" true >/dev/null 2>&1 && echo \"$p\"; done; true",
        list.join(" ")
    );
    let Ok(out) = adb.shell(id, &cmd).await else { return };
    let text = out.stdout_text();
    let yes: Vec<&str> = text.lines().map(str::trim).collect();
    for p in unknown {
        known.insert(p.to_string(), yes.contains(&"*") || yes.contains(&p));
    }
}

async fn fetch_processes(
    adb: &Adb,
    id: TransportId,
    debuggable: &mut HashMap<String, bool>,
) -> Result<Vec<ProcessRow>, String> {
    let features = adb.device_features(id).await.map_err(|e| e.to_string())?;
    let procs = adb.app_processes(id, &features).await.map_err(|e| e.to_string())?;
    let sockets = adb.runtime_sockets(id).await.map_err(|e| e.to_string())?;
    let mut rows: Vec<ProcessRow> = procs
        .into_iter()
        .filter(|p| p.debuggable)
        .map(|p| {
            let name = p.process_name.clone().unwrap_or_else(|| format!("pid {}", p.pid));
            let package =
                p.package_names.first().cloned().unwrap_or_else(|| name.split(':').next().unwrap_or("").to_string());
            let capturing = sockets.iter().any(|s| s.pid == p.pid);
            ProcessRow { pid: p.pid, name, package, capturing, frozen: false, arch: p.architecture }
        })
        .collect();
    let packages: Vec<&str> = rows.iter().filter(|r| !r.capturing).map(|r| r.package.as_str()).collect();
    learn_debuggable(adb, id, &packages, debuggable).await;
    rows.retain(|r| r.capturing || debuggable.get(&r.package) != Some(&false));
    // a runtime socket whose process the tracker did not list
    for s in sockets {
        if !rows.iter().any(|r| r.pid == s.pid) {
            rows.push(ProcessRow {
                pid: s.pid,
                name: s.package_part.clone(),
                package: s.package_part.clone(),
                capturing: true,
                frozen: false,
                arch: None,
            });
        }
    }
    let pids: Vec<u32> = rows.iter().map(|r| r.pid).collect();
    if let Ok(frozen) = adb.frozen_pids(id, &pids).await {
        for r in &mut rows {
            r.frozen = frozen.contains(&r.pid);
        }
    }
    rows.sort_by(|a, b| b.capturing.cmp(&a.capturing).then(a.name.cmp(&b.name)));
    Ok(rows)
}

async fn run(
    term: &mut terminal::Term,
    adb: Adb,
    theme: Theme,
    keymap: &Keymap,
    attach: Result<(), String>,
) -> anyhow::Result<Option<Picked>> {
    let mut stop = terminal::StopSignals::new()?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Snapshot>();
    let (want_tx, mut want_rx) = mpsc::unbounded_channel::<Option<TransportId>>();
    // devices as adb reports them (pushed on every change)
    let mut device_watch = adb.watch_devices();
    // background refresh: each device's Android version once, and the chosen device's processes
    // every second
    let fetch_adb = adb.clone();
    let mut fetch_watch = device_watch.clone();
    let fetcher = tokio::spawn(async move {
        let mut device: Option<TransportId> = None;
        let mut asked = std::collections::HashSet::new();
        // per device: package → debuggable (run-as works)
        let mut debuggable: HashMap<TransportId, HashMap<String, bool>> = HashMap::new();
        loop {
            let online: Vec<TransportId> = match &*fetch_watch.borrow_and_update() {
                Some(Ok(list)) => list.iter().filter(|d| d.is_online()).map(|d| d.transport_id).collect(),
                _ => Vec::new(),
            };
            for id in online {
                if asked.insert(id)
                    && let Some(text) = about(&fetch_adb, id).await
                    && tx.send(Snapshot::About(id, text)).is_err()
                {
                    return;
                }
            }
            if let Some(id) = device {
                let procs = fetch_processes(&fetch_adb, id, debuggable.entry(id).or_default()).await;
                if tx.send(Snapshot::Processes(id, procs)).is_err() {
                    return;
                }
            }
            tokio::select! {
                w = want_rx.recv() => match w {
                    Some(d) => device = d,
                    None => return,
                },
                _ = fetch_watch.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    });

    let mut input = EventStream::new();
    let mut devices: Result<Vec<Device>, String> = Ok(Vec::new());
    let mut abouts: HashMap<TransportId, String> = HashMap::new();
    let mut loaded = false;
    let mut chosen: Option<Device> = None;
    let mut auto_chosen = false;
    let mut processes: Option<Result<Vec<ProcessRow>, String>> = None;
    let mut cursor = 0usize;
    let mut message: Option<String> = None;
    loop {
        term.draw(|f| {
            let area = f.area();
            let buf = f.buffer_mut();
            let view = View {
                loaded,
                devices: &devices,
                abouts: &abouts,
                chosen: chosen.as_ref(),
                processes: processes.as_ref(),
                cursor,
                message: message.as_deref(),
                attach: attach.is_ok(),
                keymap,
            };
            draw(buf, area, &theme, &view);
        })?;
        tokio::select! {
            changed = device_watch.changed() => {
                if changed.is_err() {
                    break;
                }
                let latest = device_watch.borrow_and_update().clone();
                if let Some(d) = latest {
                    loaded = true;
                    devices = d;
                    // with one online device, go straight to its processes (once)
                    if chosen.is_none() && !auto_chosen && let Ok(list) = &devices {
                        let online: Vec<&Device> = list.iter().filter(|d| d.is_online()).collect();
                        if online.len() == 1 {
                            auto_chosen = true;
                            chosen = Some(online[0].clone());
                            cursor = 0;
                            let _ = want_tx.send(Some(online[0].transport_id));
                        }
                    }
                }
            }
            snap = rx.recv() => match snap {
                Some(Snapshot::About(id, text)) => {
                    abouts.insert(id, text);
                }
                Some(Snapshot::Processes(id, p)) => {
                    if chosen.as_ref().is_some_and(|d| d.transport_id == id) {
                        processes = Some(p);
                    }
                }
                None => break,
            },
            signal = stop.recv() => {
                tracing::info!(?signal, "stopping in the picker");
                break;
            }
            ev = input.next() => {
                let Some(Ok(Event::Key(k))) = ev else { continue };
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                let action = keymap.action(&k);
                if action == Some(Action::Quit) || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL)) {
                    fetcher.abort();
                    return Ok(None);
                }
                let len = match (&chosen, &processes, &devices) {
                    (Some(_), Some(Ok(p)), _) => p.len(),
                    (None, _, Ok(d)) => d.len(),
                    _ => 0,
                };
                let last = len.saturating_sub(1);
                let page = usize::from(term.size().map_or(20, |s| s.height).saturating_sub(10).max(1));
                let back = matches!(action, Some(Action::Back | Action::Left)) || k.code == KeyCode::Backspace;
                match action {
                    Some(Action::Up) => cursor = cursor.saturating_sub(1),
                    Some(Action::Down) => cursor = (cursor + 1).min(last),
                    Some(Action::Top) => cursor = 0,
                    Some(Action::Bottom) => cursor = last,
                    Some(Action::PageUp | Action::HalfPageUp) => cursor = cursor.saturating_sub(page),
                    Some(Action::PageDown | Action::HalfPageDown) => cursor = (cursor + page).min(last),
                    _ if back && chosen.is_some() => {
                        chosen = None;
                        processes = None;
                        cursor = 0;
                        message = None;
                        let _ = want_tx.send(None);
                    }
                    Some(Action::Activate | Action::Right) => {
                        message = None;
                        match (&chosen, &processes, &devices) {
                            (None, _, Ok(d)) => {
                                if let Some(dev) = d.get(cursor) {
                                    if dev.is_online() {
                                        chosen = Some(dev.clone());
                                        processes = None;
                                        cursor = 0;
                                        let _ = want_tx.send(Some(dev.transport_id));
                                    } else {
                                        message = Some(format!("{} is {}", dev.label(), dev.state));
                                    }
                                }
                            }
                            (Some(dev), Some(Ok(p)), _) => {
                                if let Some(row) = p.get(cursor) {
                                    match (&attach, row.capturing) {
                                        (_, true) | (Ok(()), false) => {
                                            fetcher.abort();
                                            return Ok(Some(Picked {
                                                serial: dev.serial.clone(),
                                                package: row.package.clone(),
                                                process: row.name.clone(),
                                                pid: row.pid,
                                                capturing: row.capturing,
                                            }));
                                        }
                                        (Err(why), false) => {
                                            message = Some(format!(
                                                "{} does not run the traffic-police library. {why}",
                                                row.name
                                            ));
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    fetcher.abort();
    Ok(None)
}

/// What the pickers show.
struct View<'a> {
    loaded: bool,
    devices: &'a Result<Vec<Device>, String>,
    abouts: &'a HashMap<TransportId, String>,
    chosen: Option<&'a Device>,
    processes: Option<&'a Result<Vec<ProcessRow>, String>>,
    cursor: usize,
    message: Option<&'a str>,
    /// Processes without the capture runtime can be picked (attach mode).
    attach: bool,
    /// The user's keys, for the hints at the bottom.
    keymap: &'a Keymap,
}

/// The picker's box: its title on the border, the list inside with a margin, the columns spread
/// over the whole width, and the keys at the bottom.
fn draw(buf: &mut ratatui::buffer::Buffer, area: Rect, t: &Theme, v: &View) {
    let View { loaded, devices, abouts, chosen, processes, cursor, message, attach, keymap } = *v;
    Block::bordered().border_type(t.borders).border_style(t.accent()).render(area, buf);
    // the title after the corner, as on the main screen's boxes (`╭─ Choose … ─`)
    let bold = t.accent().add_modifier(Modifier::BOLD);
    let title = match chosen {
        None => vec![Span::styled("Choose a device", bold)],
        Some(d) => vec![Span::styled("Choose an app process on ", bold), Span::styled(d.label(), t.text())],
    };
    if crate::ui::border_labels(buf, area, area.y, false, vec![title]).is_empty() {
        crate::ui::border_labels(buf, area, area.y, false, vec![vec![Span::styled("Choose", bold)]]);
    }
    // a margin of one row and two columns inside the border
    let inner = Rect {
        x: area.x + 3,
        y: area.y + 2,
        width: area.width.saturating_sub(6),
        height: area.height.saturating_sub(4),
    };
    if inner.width < 10 || inner.height < 3 {
        return;
    }
    let w = usize::from(inner.width);
    let selected = t.selected();
    // rows from the top; the message and the keys from the bottom
    let mut lines: Vec<Line> = Vec::new();
    match (chosen, processes) {
        (None, _) => match devices {
            _ if !loaded => lines.push(Line::styled("Asking adb for devices…", t.dim())),
            Err(e) => lines.push(Line::styled(format!("adb: {e}"), t.error())),
            Ok(list) if list.is_empty() => lines.push(Line::styled(
                "No devices. Connect a phone with USB debugging on, or start an emulator.",
                t.dim(),
            )),
            Ok(list) => {
                // device · state · Android version, spread over the width
                let state_w = 16;
                let device_w = (w.saturating_sub(state_w + 6) * 2 / 5).max(20);
                let about_w = w.saturating_sub(device_w + state_w + 6);
                let header = format!("{:<device_w$}   {:<state_w$}   {}", "device", "state", "Android");
                lines.push(Line::styled(header, t.dim().add_modifier(Modifier::BOLD)));
                for (i, d) in list.iter().enumerate() {
                    let style = if i == cursor { selected } else { Style::default() };
                    let state = if d.is_online() { t.ok() } else { t.warn() };
                    let about = abouts.get(&d.transport_id).cloned().unwrap_or_default();
                    lines.push(
                        Line::from(vec![
                            Span::styled(format!("{:<device_w$}   ", truncate(&d.label(), device_w)), t.text()),
                            Span::styled(format!("{:<state_w$}   ", truncate(&d.state, state_w)), state),
                            Span::styled(format!("{:<about_w$}", truncate(&about, about_w)), t.dim()),
                        ])
                        .style(style),
                    );
                }
            }
        },
        (Some(_), None) => lines.push(Line::styled("Reading the device's processes…", t.dim())),
        (Some(_), Some(Err(e))) => lines.push(Line::styled(format!("adb: {e}"), t.error())),
        (Some(_), Some(Ok(rows))) if rows.is_empty() => lines.push(Line::styled(
            if attach {
                "No debuggable app is running. Start a debug build of your app."
            } else {
                "No debuggable app is running. Start a debug build of your app (with the traffic-police library)."
            },
            t.dim(),
        )),
        (Some(_), Some(Ok(rows))) => {
            // mark · process · pid · arch · capture: the process and capture columns share the
            // width left over, so the table spans the box
            let (pid_w, arch_w, gaps) = (8, 8, 9);
            let flex = w.saturating_sub(2 + pid_w + arch_w + gaps);
            let process_w = (flex * 2 / 5).max(16);
            let note_w = flex.saturating_sub(process_w);
            let header =
                format!("  {:<process_w$}   {:>pid_w$}   {:<arch_w$}   {}", "process", "pid", "arch", "capture");
            lines.push(Line::styled(header, t.dim().add_modifier(Modifier::BOLD)));
            for (i, r) in rows.iter().enumerate() {
                let style = if i == cursor { selected } else { Style::default() };
                let (mark, note, st) = match (r.capturing, r.frozen, attach) {
                    (true, true, _) => {
                        ("◐", "capture running · frozen by Android (cached): connects when it runs", t.warn())
                    }
                    (true, false, _) => ("●", "capture running: ready", t.ok()),
                    (false, _, true) => ("○", "no library: Enter attaches the agent", t.dim()),
                    (false, _, false) => ("○", "debuggable, no library", t.dim()),
                };
                lines.push(
                    Line::from(vec![
                        Span::styled(format!("{mark} "), st),
                        Span::styled(format!("{:<process_w$}   ", truncate(&r.name, process_w)), t.text()),
                        Span::styled(format!("{:>pid_w$}   ", r.pid), t.dim()),
                        Span::styled(format!("{:<arch_w$}   ", r.arch.as_deref().unwrap_or("")), t.dim()),
                        Span::styled(format!("{:<note_w$}", truncate(note, note_w)), st),
                    ])
                    .style(style),
                );
            }
        }
    }
    let key = |a: Action| keymap.key_label(a);
    let mut help = format!("{}{} choose · {} open", key(Action::Up), key(Action::Down), key(Action::Activate));
    if chosen.is_some() {
        help.push_str(&format!(" · {} back to devices", key(Action::Back)));
    }
    help.push_str(&format!(" · {} quit", key(Action::Quit)));
    let help = help.as_str();
    let mut bottom: Vec<Line> = Vec::new();
    if let Some(m) = message {
        bottom.extend(crate::wrap::wrap_line(&Line::styled(m.to_string(), t.warn()), w, 0));
        bottom.push(Line::default());
    }
    bottom.push(Line::styled(help, t.faint()));
    let bottom_h = (bottom.len() as u16).min(inner.height.saturating_sub(1));
    let list_h = inner.height.saturating_sub(bottom_h + 1);
    // keep the cursor's row in view
    let skip = (cursor + 2).saturating_sub(usize::from(list_h));
    let header = lines.first().cloned();
    let body: Vec<Line> = lines.into_iter().skip(1).skip(skip).collect();
    let mut list = Vec::with_capacity(body.len() + 1);
    list.extend(header);
    list.extend(body);
    Paragraph::new(list).render(Rect { height: list_h, ..inner }, buf);
    Paragraph::new(bottom).render(Rect { y: inner.y + inner.height - bottom_h, height: bottom_h, ..inner }, buf);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Depth, Palette};

    fn render(v: &View, area: Rect) -> ratatui::buffer::Buffer {
        let mut buf = ratatui::buffer::Buffer::empty(area);
        draw(&mut buf, area, &Theme::new(Palette::Dark, Depth::TrueColor), v);
        buf
    }

    fn text(v: &View) -> String {
        let area = Rect::new(0, 0, 110, 12);
        let buf = render(v, area);
        (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn phone() -> Device {
        Device {
            serial: "0123456789ABCDEF".into(),
            state: "device".into(),
            product: None,
            model: Some("Pixel_8".into()),
            device: None,
            transport_id: 1,
        }
    }

    #[test]
    fn the_process_list_spans_the_width_inside_a_margin() {
        let device = phone();
        let row = |pid, name: &str| ProcessRow {
            pid,
            name: name.into(),
            package: name.into(),
            capturing: false,
            frozen: false,
            arch: Some("arm64".into()),
        };
        let rows = Ok(vec![row(10771, "com.example.shop"), row(20, "com.example.other")]);
        let devices = Ok(vec![device.clone()]);
        let abouts = HashMap::from([(1, "Android 14 (API 34)".to_string())]);
        let keymap = Keymap::default();
        let mut view = View {
            loaded: true,
            devices: &devices,
            abouts: &abouts,
            chosen: Some(&device),
            processes: Some(&rows),
            cursor: 0,
            message: None,
            attach: true,
            keymap: &keymap,
        };
        let area = Rect::new(0, 0, 160, 14);
        let selected = Theme::new(Palette::Dark, Depth::TrueColor).selected().bg.unwrap();
        for (title, last_column) in
            [("╭─ Choose an app process on Pixel 8", "capture"), ("╭─ Choose a device ─", "Android")]
        {
            let buf = render(&view, area);
            let row_text = |y: u16| (0..area.width).map(|x| buf[(x, y)].symbol()).collect::<String>();
            assert!(row_text(0).starts_with(title), "{}", row_text(0));
            // the header sits one row and two columns inside the border (the process list's after
            // the column of marks)
            let first = row_text(2).chars().skip(1).position(|c| c != ' ').unwrap() + 1;
            assert!((3..=5).contains(&first), "{:?}", row_text(2));
            // the chosen row is highlighted across the width, up to the margin on the right
            assert_eq!(buf[(3, 3)].bg, selected);
            assert_eq!(buf[(area.width - 4, 3)].bg, selected);
            assert_ne!(buf[(area.width - 3, 3)].bg, selected, "the margin stays clear");
            // the columns are spread over the width, not packed on the left
            let at = row_text(2).find(last_column).unwrap();
            assert!(at > 70, "{last_column} at {at}: {:?}", row_text(2));
            view.chosen = None;
        }
    }

    #[test]
    fn processes_without_the_library_can_be_attached_to_when_the_agent_is_there() {
        let device = Device {
            serial: "emulator-5554".into(),
            state: "device".into(),
            product: None,
            model: Some("Pixel_7".into()),
            device: None,
            transport_id: 1,
        };
        let row = |pid, name: &str, capturing| ProcessRow {
            pid,
            name: name.into(),
            package: name.into(),
            capturing,
            frozen: false,
            arch: Some("arm64".into()),
        };
        let rows = Ok(vec![row(10, "com.library.app", true), row(20, "com.plain.app", false)]);
        let devices = Ok(vec![device.clone()]);
        let abouts = HashMap::new();
        let keymap = Keymap::default();
        let mut view = View {
            loaded: true,
            devices: &devices,
            abouts: &abouts,
            chosen: Some(&device),
            processes: Some(&rows),
            cursor: 0,
            message: None,
            attach: true,
            keymap: &keymap,
        };
        let shown = text(&view);
        assert!(shown.contains("● com.library.app") && shown.contains("capture running: ready"), "{shown}");
        assert!(shown.contains("○ com.plain.app") && shown.contains("no library: Enter attaches the agent"), "{shown}");
        view.attach = false;
        let shown = text(&view);
        assert!(shown.contains("debuggable, no library") && !shown.contains("Enter attaches"), "{shown}");
    }
}
