//! The device and process pickers (ARCHITECTURE.md §5.12): `traffic-police` without a target
//! lists devices, then the chosen device's debuggable processes, marking the ones that already
//! run the capture runtime.

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use traffic_police_adb::{Adb, Device, TransportId};

use crate::terminal;
use crate::theme::Theme;

/// The process the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub serial: String,
    pub package: String,
    pub process: String,
    pub pid: u32,
}

#[derive(Debug, Clone)]
struct ProcessRow {
    pid: u32,
    name: String,
    package: String,
    capturing: bool,
    arch: Option<String>,
}

enum Snapshot {
    Devices(Result<Vec<Device>, String>),
    Processes(TransportId, Result<Vec<ProcessRow>, String>),
}

/// Runs the pickers; `None` when the user quits.
pub async fn pick(adb: Adb, theme: Theme) -> anyhow::Result<Option<Picked>> {
    terminal::install_panic_hook();
    let mut term = terminal::setup()?;
    let result = run(&mut term, adb, theme).await;
    terminal::restore_terminal();
    result
}

async fn fetch_processes(adb: &Adb, id: TransportId) -> Result<Vec<ProcessRow>, String> {
    let features = adb.device_features(id).await.map_err(|e| e.to_string())?;
    let procs = adb.app_processes(id, &features).await.map_err(|e| e.to_string())?;
    let sockets = adb.runtime_sockets(id).await.map_err(|e| e.to_string())?;
    let mut rows: Vec<ProcessRow> = procs
        .into_iter()
        .filter(|p| p.debuggable)
        .map(|p| {
            let name = p.process_name.clone().unwrap_or_else(|| format!("pid {}", p.pid));
            let package = p.package_names.first().cloned().unwrap_or_else(|| name.split(':').next().unwrap_or("").to_string());
            let capturing = sockets.iter().any(|s| s.pid == p.pid);
            ProcessRow { pid: p.pid, name, package, capturing, arch: p.architecture }
        })
        .collect();
    // a runtime socket whose process the tracker did not list
    for s in sockets {
        if !rows.iter().any(|r| r.pid == s.pid) {
            rows.push(ProcessRow {
                pid: s.pid,
                name: s.package_part.clone(),
                package: s.package_part.clone(),
                capturing: true,
                arch: None,
            });
        }
    }
    rows.sort_by(|a, b| b.capturing.cmp(&a.capturing).then(a.name.cmp(&b.name)));
    Ok(rows)
}

async fn run(term: &mut terminal::Term, adb: Adb, theme: Theme) -> anyhow::Result<Option<Picked>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Snapshot>();
    let (want_tx, mut want_rx) = mpsc::unbounded_channel::<Option<TransportId>>();
    // background refresh: the device list, and the processes of the chosen device, every second
    let fetch_adb = adb.clone();
    let fetcher = tokio::spawn(async move {
        let mut device: Option<TransportId> = None;
        loop {
            let devices = fetch_adb.devices().await.map_err(|e| e.to_string());
            if tx.send(Snapshot::Devices(devices)).is_err() {
                return;
            }
            if let Some(id) = device {
                let procs = fetch_processes(&fetch_adb, id).await;
                if tx.send(Snapshot::Processes(id, procs)).is_err() {
                    return;
                }
            }
            tokio::select! {
                w = want_rx.recv() => match w {
                    Some(d) => device = d,
                    None => return,
                },
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    });

    let mut input = EventStream::new();
    let mut devices: Result<Vec<Device>, String> = Ok(Vec::new());
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
            draw(buf, area, &theme, loaded, &devices, chosen.as_ref(), processes.as_ref(), cursor, message.as_deref());
        })?;
        tokio::select! {
            snap = rx.recv() => match snap {
                Some(Snapshot::Devices(d)) => {
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
                Some(Snapshot::Processes(id, p)) => {
                    if chosen.as_ref().is_some_and(|d| d.transport_id == id) {
                        processes = Some(p);
                    }
                }
                None => break,
            },
            ev = input.next() => {
                let Some(Ok(Event::Key(k))) = ev else { continue };
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                if k.code == KeyCode::Char('q') || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL)) {
                    fetcher.abort();
                    return Ok(None);
                }
                let len = match (&chosen, &processes, &devices) {
                    (Some(_), Some(Ok(p)), _) => p.len(),
                    (None, _, Ok(d)) => d.len(),
                    _ => 0,
                };
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => cursor = cursor.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => cursor = (cursor + 1).min(len.saturating_sub(1)),
                    KeyCode::Esc | KeyCode::Backspace | KeyCode::Left if chosen.is_some() => {
                        chosen = None;
                        processes = None;
                        cursor = 0;
                        message = None;
                        let _ = want_tx.send(None);
                    }
                    KeyCode::Enter | KeyCode::Right => {
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
                                    if row.capturing {
                                        fetcher.abort();
                                        return Ok(Some(Picked {
                                            serial: dev.serial.clone(),
                                            package: row.package.clone(),
                                            process: row.name.clone(),
                                            pid: row.pid,
                                        }));
                                    }
                                    message = Some(format!(
                                        "{} does not run the traffic-police library. Add it to the app's debug build (see the README), then restart the app. Attach mode, which needs no library, arrives in Phase 4.",
                                        row.name
                                    ));
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

#[allow(clippy::too_many_arguments)]
fn draw(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    t: &Theme,
    loaded: bool,
    devices: &Result<Vec<Device>, String>,
    chosen: Option<&Device>,
    processes: Option<&Result<Vec<ProcessRow>, String>>,
    cursor: usize,
    message: Option<&str>,
) {
    let mut lines: Vec<Line> = Vec::new();
    let title = match chosen {
        None => " Choose a device ".to_string(),
        Some(d) => format!(" Choose an app process on {} ", d.label()),
    };
    let selected = t.selected();
    match (chosen, processes) {
        (None, _) => match devices {
            _ if !loaded => lines.push(Line::styled("Asking adb for devices…", t.dim())),
            Err(e) => lines.push(Line::styled(format!("adb: {e}"), t.error())),
            Ok(list) if list.is_empty() => {
                lines.push(Line::styled("No devices. Connect a phone with USB debugging on, or start an emulator.", t.dim()))
            }
            Ok(list) => {
                for (i, d) in list.iter().enumerate() {
                    let style = if i == cursor { selected } else { Style::default() };
                    let state = if d.is_online() { t.ok() } else { t.warn() };
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {:<34}", d.label()), t.text().patch(style)),
                        Span::styled(format!("{:<16}", d.state), state.patch(style)),
                    ]));
                }
            }
        },
        (Some(_), None) => lines.push(Line::styled("Reading the device's processes…", t.dim())),
        (Some(_), Some(Err(e))) => lines.push(Line::styled(format!("adb: {e}"), t.error())),
        (Some(_), Some(Ok(rows))) if rows.is_empty() => lines.push(Line::styled(
            "No debuggable app is running. Start a debug build of your app (with the traffic-police library).",
            t.dim(),
        )),
        (Some(_), Some(Ok(rows))) => {
            lines.push(Line::styled(
                format!("   {:<46}{:>8}  {:<8}{}", "process", "pid", "arch", "capture"),
                t.dim().add_modifier(Modifier::BOLD),
            ));
            for (i, r) in rows.iter().enumerate() {
                let style = if i == cursor { selected } else { Style::default() };
                let (mark, note, st) = if r.capturing {
                    ("●", "library running: ready", t.ok())
                } else {
                    ("○", "debuggable, no library (attach: Phase 4)", t.dim())
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {mark} "), st.patch(style)),
                    Span::styled(format!("{:<46}", r.name), t.text().patch(style)),
                    Span::styled(format!("{:>8}  ", r.pid), t.dim().patch(style)),
                    Span::styled(format!("{:<8}", r.arch.as_deref().unwrap_or("")), t.dim().patch(style)),
                    Span::styled(note, st.patch(style)),
                ]));
            }
        }
    }
    lines.push(Line::default());
    if let Some(m) = message {
        lines.push(Line::styled(m.to_string(), t.warn()));
        lines.push(Line::default());
    }
    let help = if chosen.is_some() { "↑↓ choose · Enter open · Esc back to devices · q quit" } else { "↑↓ choose · Enter open · q quit" };
    lines.push(Line::styled(help, t.faint()));
    let block = Block::bordered().title(title).border_style(t.accent());
    Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }).block(block).render(area, buf);
}
