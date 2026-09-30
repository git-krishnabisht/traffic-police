//! Terminal lifecycle and the event loop (ARCHITECTURE.md §5.2 and §5.8).
//!
//! One task owns the [`App`]: it applies event batches from the backend, handles input, and
//! redraws with frame pacing. Input redraws at up to 60 Hz, data at up to 30 Hz, and a quiet
//! live view about 4 times per second so the time axis keeps moving.

use std::io::{self, Stdout};
use std::path::Path;
use std::sync::Once;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream,
    MouseEventKind,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::ConnectionStatus;

use crate::app::{App, JqJob, JqResult};
use crate::bodycache::BodyJob;
use crate::bodyview::BodyView;
use crate::images::Images;
use crate::ui;

pub(crate) type Term = Terminal<CrosstermBackend<Stdout>>;

/// Redraw pacing: up to 60 frames a second for input and arriving data, 30 while only the
/// clock moves things (the live graph, growing bars), none when nothing changes.
const INPUT_FRAME: Duration = Duration::from_millis(16);
const DATA_FRAME: Duration = Duration::from_millis(16);
const CLOCK_FRAME: Duration = Duration::from_millis(33);

/// Put the terminal back: raw mode off, main screen, mouse and paste reporting off, cursor on.
/// Safe to call more than once and from the panic hook.
pub fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste, LeaveAlternateScreen, cursor::Show);
}

/// Restore the terminal before the default panic message is printed, so it stays readable.
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            traffic_police_core::store::spill::remove_all();
            previous(info);
        }));
    });
}

pub(crate) fn setup() -> io::Result<Term> {
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    if let Err(e) = execute!(out, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste, cursor::Hide) {
        restore_terminal();
        return Err(e);
    }
    Terminal::new(CrosstermBackend::new(out))
}

pub struct RunOptions {
    /// Probe the terminal for a graphics protocol (otherwise half-blocks).
    pub detect_images: bool,
    /// Connection status from the backend, shown in the header.
    pub status: Option<tokio::sync::watch::Receiver<ConnectionStatus>>,
}

/// Run the UI until the user quits. `events` carries batches from the backend; when it closes
/// the UI keeps showing what was captured.
pub async fn run(mut app: App, mut events: mpsc::Receiver<Vec<SessionEvent>>, opts: RunOptions) -> anyhow::Result<()> {
    install_panic_hook();
    // Under NO_COLOR crossterm turns color commands into a bare reset that also clears bold and
    // reverse video; the monochrome theme emits no colors, so crossterm's filter stays off.
    crossterm::style::force_color_output(true);
    let mut term = setup()?;
    // The probe reads the terminal's answer from stdin, so it runs before the input stream exists.
    app.images = if opts.detect_images { Images::detect() } else { Images::halfblocks() };
    let result = event_loop(&mut term, &mut app, &mut events, opts.status).await;
    restore_terminal();
    result
}

/// An editor started from the Call Stack tab; the loop keeps ingesting events while it runs.
struct Editor {
    child: tokio::process::Child,
    path: std::path::PathBuf,
}

/// Ends a background task with the loop.
struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = &self.0 {
            h.abort();
        }
    }
}

async fn event_loop(
    term: &mut Term,
    app: &mut App,
    events: &mut mpsc::Receiver<Vec<SessionEvent>>,
    mut status: Option<tokio::sync::watch::Receiver<ConnectionStatus>>,
) -> anyhow::Result<()> {
    if let Some(s) = &status {
        app.connection = Some(s.borrow().clone());
    }
    // `None` while an editor owns the terminal: the stream's reader thread would compete with
    // the editor for keystrokes.
    let mut input = Some(EventStream::new());
    let mut editor: Option<Editor> = None;
    let (jq_tx, mut jq_rx) = mpsc::unbounded_channel::<(JqJob, JqResult)>();
    let (body_tx, mut body_rx) = mpsc::unbounded_channel::<(BodyJob, BodyView)>();
    let (search_tx, mut search_rx) = mpsc::unbounded_channel::<(crate::app::SearchJob, bool)>();
    let (rules_tx, mut rules_rx) = mpsc::unbounded_channel::<traffic_police_core::rules::RulesFile>();
    // started once there is a rules file to watch (r can create it while the UI runs)
    let mut watcher = AbortOnDrop(None);
    let mut backend_open = true;
    let mut stop = StopSignals::new()?;
    let mut input_dirty = true;
    let mut data_dirty = false;
    let mut last_draw: Option<Instant> = None;
    loop {
        // wake exactly when the next frame is due (nothing to draw: wait for an event)
        let pace = if input_dirty {
            Some(INPUT_FRAME)
        } else if data_dirty {
            Some(DATA_FRAME)
        } else if editor.is_none() && app.is_time_dependent() {
            Some(CLOCK_FRAME)
        } else {
            None
        };
        let wake = pace.map(|p| tokio::time::Instant::from_std(last_draw.map_or_else(Instant::now, |t| t + p)));
        tokio::select! {
            ev = async { input.as_mut().expect("guarded").next().await }, if input.is_some() => match ev {
                Some(Ok(Event::Mouse(m))) if m.kind == MouseEventKind::Moved => {}
                Some(Ok(ev)) => {
                    app.handle_event(ev);
                    input_dirty = true;
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            status = async { editor.as_mut().expect("guarded").child.wait().await }, if editor.is_some() => {
                let Editor { path, .. } = editor.take().expect("guarded");
                resume_terminal(term)?;
                input = Some(EventStream::new());
                match status {
                    Ok(s) if s.success() => app.flash(format!("returned from the editor ({})", path.display())),
                    Ok(s) => app.flash(format!("the editor exited with {s}")),
                    Err(e) => app.flash(format!("could not wait for the editor: {e}")),
                }
                input_dirty = true;
                last_draw = None;
            }
            batch = events.recv(), if backend_open => match batch {
                Some(b) => {
                    app.ingest(b);
                    // take whatever else is queued so one redraw covers it
                    while let Ok(b) = events.try_recv() {
                        app.ingest(b);
                    }
                    data_dirty = true;
                }
                None => backend_open = false,
            },
            changed = async { status.as_mut().expect("guarded").changed().await }, if status.is_some() => {
                match changed {
                    Ok(()) => {
                        app.connection = Some(status.as_ref().expect("guarded").borrow().clone());
                    }
                    Err(_) => status = None, // the backend is gone; keep the last status
                }
                input_dirty = true;
            }
            Some((job, result)) = jq_rx.recv() => {
                app.finish_jq(job, result);
                input_dirty = true;
            }
            Some((job, view)) = body_rx.recv() => {
                app.finish_body(job, view);
                input_dirty = true;
            }
            Some((job, hit)) = search_rx.recv() => {
                app.finish_search(&job, hit);
                data_dirty = true;
            }
            Some(f) = rules_rx.recv() => {
                app.rules_reloaded(f);
                input_dirty = true;
            }
            _ = stop.recv() => {
                // SIGTERM or SIGHUP: leave the loop so the terminal is restored
                if let Some(Editor { mut child, .. }) = editor.take() {
                    let _ = child.kill().await;
                }
                traffic_police_core::store::spill::remove_all();
                return Ok(());
            }
            _ = async { tokio::time::sleep_until(wake.expect("guarded")).await }, if wake.is_some() => {}
        }
        if app.should_quit {
            return Ok(());
        }
        if watcher.0.is_none()
            && app.rules_file.is_some()
            && let Some(path) = app.rules_path()
        {
            watcher.0 = Some(tokio::spawn(crate::rules::watch(path, rules_tx.clone())));
        }
        dispatch_jobs(app, &body_tx, &jq_tx, &search_tx);
        if let Some((path, line)) = app.editor_request.take()
            && editor.is_none()
        {
            input = None;
            match start_editor(&path, line) {
                Ok(child) => editor = Some(Editor { child, path }),
                Err(e) => {
                    tracing::warn!("editor: {e:#}");
                    resume_terminal(term)?;
                    input = Some(EventStream::new());
                    app.flash(format!("could not open the editor: {e}"));
                    input_dirty = true;
                    last_draw = None;
                }
            }
        }
        if editor.is_some() {
            continue;
        }
        let since = last_draw.map_or(Duration::MAX, |t| t.elapsed());
        let due = (input_dirty && since >= INPUT_FRAME)
            || (data_dirty && since >= DATA_FRAME)
            || (app.is_time_dependent() && since >= CLOCK_FRAME);
        if due {
            let started = Instant::now();
            term.draw(|f| ui::draw(f, app))?;
            app.frames.record(started, started.elapsed());
            last_draw = Some(started);
            input_dirty = false;
            data_dirty = false;
            // drawing refreshes the rows, which can ask for body searches
            dispatch_jobs(app, &body_tx, &jq_tx, &search_tx);
        }
    }
}

/// Starts queued work off the UI task: body decodes, jq filters, and body searches.
fn dispatch_jobs(
    app: &mut App,
    body_tx: &mpsc::UnboundedSender<(BodyJob, BodyView)>,
    jq_tx: &mpsc::UnboundedSender<(JqJob, JqResult)>,
    search_tx: &mpsc::UnboundedSender<(crate::app::SearchJob, bool)>,
) {
    for job in app.take_body_jobs() {
        let tx = body_tx.clone();
        tokio::task::spawn_blocking(move || {
            let view = job.run();
            let _ = tx.send((job, view));
        });
    }
    for job in app.take_jq_jobs() {
        let runner = app.jq_runner;
        let tx = jq_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = runner(&job.filter, &job.bytes);
            let _ = tx.send((job, result));
        });
    }
    let searches = app.take_search_jobs();
    if !searches.is_empty() {
        // one blocking task works through a batch, so a big session does not flood the pool
        let tx = search_tx.clone();
        tokio::task::spawn_blocking(move || {
            for job in searches {
                let hit = job.search.run();
                if tx.send((job, hit)).is_err() {
                    return;
                }
            }
        });
    }
}

/// Give the terminal to `$VISUAL`/`$EDITOR`, opened at `path:line`.
fn start_editor(path: &Path, line: u32) -> anyhow::Result<tokio::process::Child> {
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi").to_string();
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(parts);
    let base = Path::new(&program).file_name().and_then(|s| s.to_str()).unwrap_or("");
    match base {
        "code" | "code-insiders" | "cursor" | "codium" => {
            cmd.arg("--wait").arg("-g").arg(format!("{}:{line}", path.display()));
        }
        "subl" | "zed" | "hx" => {
            cmd.arg(format!("{}:{line}", path.display()));
        }
        _ => {
            cmd.arg(format!("+{line}")).arg(path);
        }
    }
    restore_terminal();
    cmd.spawn().map_err(|e| anyhow::anyhow!("{program}: {e}"))
}

/// Take the terminal back after the editor: alternate screen, raw mode, and a full repaint.
fn resume_terminal(term: &mut Term) -> anyhow::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        cursor::Hide,
        terminal::Clear(terminal::ClearType::All)
    )?;
    // A fresh Terminal has empty buffers, so the next frame is drawn in full. (Terminal::clear
    // would ask the terminal for the cursor position first, and that query can fail right
    // after another program used the terminal.)
    *term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok(())
}

/// SIGTERM and SIGHUP (Unix). Raw mode delivers Ctrl+C as a key, so SIGINT is not needed.
#[cfg(unix)]
struct StopSignals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl StopSignals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(StopSignals { term: signal(SignalKind::terminate())?, hup: signal(SignalKind::hangup())? })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.hup.recv() => {}
        }
    }
}

/// Elsewhere (Windows) there is no terminal state to restore when the console goes away.
#[cfg(not(unix))]
struct StopSignals;

#[cfg(not(unix))]
impl StopSignals {
    fn new() -> io::Result<Self> {
        Ok(StopSignals)
    }

    async fn recv(&mut self) {
        std::future::pending::<()>().await
    }
}
