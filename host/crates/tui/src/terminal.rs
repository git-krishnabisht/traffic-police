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
use tokio::time::MissedTickBehavior;
use tokio_stream::StreamExt;
use traffic_police_core::SessionEvent;

use crate::app::{App, JqJob, JqResult};
use crate::bodycache::BodyJob;
use crate::bodyview::BodyView;
use crate::images::Images;
use crate::ui;

type Term = Terminal<CrosstermBackend<Stdout>>;

const INPUT_FRAME: Duration = Duration::from_millis(16);
const DATA_FRAME: Duration = Duration::from_millis(33);
const IDLE_FRAME: Duration = Duration::from_millis(250);

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
            previous(info);
        }));
    });
}

fn setup() -> io::Result<Term> {
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
    let result = event_loop(&mut term, &mut app, &mut events).await;
    restore_terminal();
    result
}

/// An editor started from the Call Stack tab; the loop keeps ingesting events while it runs.
struct Editor {
    child: tokio::process::Child,
    path: std::path::PathBuf,
}

async fn event_loop(
    term: &mut Term,
    app: &mut App,
    events: &mut mpsc::Receiver<Vec<SessionEvent>>,
) -> anyhow::Result<()> {
    // `None` while an editor owns the terminal: the stream's reader thread would compete with
    // the editor for keystrokes.
    let mut input = Some(EventStream::new());
    let mut editor: Option<Editor> = None;
    let (jq_tx, mut jq_rx) = mpsc::unbounded_channel::<(JqJob, JqResult)>();
    let (body_tx, mut body_rx) = mpsc::unbounded_channel::<(BodyJob, BodyView)>();
    let mut ticker = tokio::time::interval(INPUT_FRAME);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut backend_open = true;
    let mut input_dirty = true;
    let mut data_dirty = false;
    let mut last_draw: Option<Instant> = None;
    loop {
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
            Some((job, result)) = jq_rx.recv() => {
                app.finish_jq(job, result);
                input_dirty = true;
            }
            Some((job, view)) = body_rx.recv() => {
                app.finish_body(job, view);
                input_dirty = true;
            }
            _ = ticker.tick() => {}
        }
        if app.should_quit {
            return Ok(());
        }
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
            || (app.is_time_dependent() && since >= IDLE_FRAME);
        if due {
            term.draw(|f| ui::draw(f, app))?;
            last_draw = Some(Instant::now());
            input_dirty = false;
            data_dirty = false;
        }
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
