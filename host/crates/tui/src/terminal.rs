//! Terminal lifecycle and the event loop (ARCHITECTURE.md §5.2 and §5.8).
//!
//! One task owns the [`App`]: it applies event batches from the backend, handles input, and
//! redraws with frame pacing: at most `[ui] fps` frames a second, whether input, data or the
//! clock changed the picture, and none while it stands still.

use std::cell::Cell;
use std::io::{self, Stdout};
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream,
    MouseEventKind,
};
use crossterm::terminal::{self, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::Backend;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::ConnectionStatus;

use crate::app::{App, FPS_RANGE, JqJob, JqResult};
use crate::bodycache::BodyJob;
use crate::bodyview::BodyView;
use crate::images::Images;
use crate::screen::Screen;
use crate::ui;

type Tty = Terminal<Screen<Stdout>>;

/// The terminal while a UI owns it. Dropping it puts the terminal back ([`restore_terminal`],
/// errors ignored) and never runs ratatui's own drop: that one shows the cursor again and reports
/// a failure with `eprintln!`, which panics once the window is closed, and the panic would skip
/// the goodbye that removes the adb forward.
pub(crate) struct Term(ManuallyDrop<Tty>);

impl Term {
    /// A fresh ratatui terminal in place of this one (after an editor used the screen).
    fn replace(&mut self, fresh: Tty) {
        let mut old = std::mem::replace(&mut *self.0, fresh);
        // with the cursor shown, ratatui's drop has nothing to report
        if old.show_cursor().is_ok() {
            drop(old);
        } else {
            std::mem::forget(old);
        }
    }
}

impl Deref for Term {
    type Target = Tty;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Term {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // not again after a panic's hook did it: leaving the alternate screen twice puts the
        // cursor back at the top, where the shell would then write over the panic's message
        if TERMINAL_OWNED.swap(false, Ordering::Relaxed) {
            restore_terminal();
        }
        OWNS_TERMINAL.set(false);
    }
}

thread_local! {
    /// This thread owns the terminal: the UI's, while a [`Term`] lives on it.
    static OWNS_TERMINAL: Cell<bool> = const { Cell::new(false) };
}

/// Some thread owns the terminal, so a panic on another must not write over the screen.
static TERMINAL_OWNED: AtomicBool = AtomicBool::new(false);

/// What the latest panic on a worker said, for the UI to show ([`take_worker_panic`]).
static WORKER_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Redraw pacing: at most `fps` frames a second, and the first one without waiting.
///
/// Frames that follow each other keep a beat: each is due one frame time after the one before
/// was due, not after it was drawn. Tokio's timer rounds up to the millisecond and the loop
/// wakes a little after that: paced from the drawing, the demo drew 177 frames a second
/// instead of 240, and 54 instead of 60.
struct Pacer {
    frame: Duration,
    /// When the next frame may be drawn (`None`: now).
    next: Option<Instant>,
}

impl Pacer {
    fn new(fps: u16) -> Self {
        let fps = fps.clamp(*FPS_RANGE.start(), *FPS_RANGE.end());
        Pacer { frame: Duration::from_secs(1) / u32::from(fps), next: None }
    }

    /// When the next frame may be drawn.
    fn next(&self) -> Instant {
        self.next.unwrap_or_else(Instant::now)
    }

    fn due(&self, now: Instant) -> bool {
        self.next.is_none_or(|t| now >= t)
    }

    /// A frame was drawn at `now`.
    fn drawn(&mut self, now: Instant) {
        self.next = Some(match self.next {
            Some(due) if now < due + self.frame => due + self.frame,
            // the first frame, or one that came a whole frame late (after a pause): a new beat
            _ => now + self.frame,
        });
    }

    /// Draw the next frame without waiting.
    fn reset(&mut self) {
        self.next = None;
    }
}

/// The most waiting input events taken in one turn of the loop, so that a flood of them still
/// lets frames through.
const WAITING_INPUT: usize = 256;

/// The longest a turn of the loop takes in data before it reads input and draws again
/// (ARCHITECTURE.md §5.2). What is left waits in the channel for the next turn, which comes at
/// once, so the data keeps its pace; up to 0.5.1 a turn took every batch waiting, and while a
/// backend kept the channel full (a replay, a busy app after a reconnect) keys and frames waited.
pub const INGEST_PER_TURN: Duration = Duration::from_millis(8);

/// What [`ingest_waiting`] took in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ingested {
    pub batches: usize,
    pub events: usize,
    /// The backend is gone: its channel closed.
    pub closed: bool,
}

/// Applies the batches of data already waiting, without waiting for more, until `budget` is
/// spent; always at least one, so the data moves on whatever the budget.
pub fn ingest_waiting(events: &mut mpsc::Receiver<Vec<SessionEvent>>, app: &mut App, budget: Duration) -> Ingested {
    let started = Instant::now();
    let mut taken = Ingested::default();
    while taken.batches == 0 || started.elapsed() < budget {
        match events.try_recv() {
            Ok(batch) => {
                taken.batches += 1;
                taken.events += batch.len();
                app.ingest(batch);
            }
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                taken.closed = true;
                break;
            }
        }
    }
    taken
}

/// Ctrl+C typed in an editor reaches this process as well, possibly just after the editor has
/// exited because of it; an interrupt this soon after the editor is the editor's.
const EDITOR_INTERRUPT: Duration = Duration::from_secs(1);

/// Put the terminal back: raw mode off, main screen, mouse and paste reporting off, cursor on,
/// and a frame that was being written (a panic, a failed write) ended. Safe to call more than
/// once and from the panic hook.
pub fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        EndSynchronizedUpdate,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
}

/// What the panic hook does: a panic on the thread that owns the terminal ends the program (the UI
/// unwinds, the backend says goodbye, then the process ends), so the terminal is put back before
/// the message is printed, so that it can be read. A panic on another thread, while the UI runs,
/// is a worker's (a body decode, a search, jq, an export, a backend's task): tokio catches it and
/// the UI goes on, so the terminal and the spilled bodies stay, the message goes to the log
/// instead of over the screen, and the footer says a background task failed. Up to 0.5.1 every
/// panic put the terminal back and removed the spill directories, so after a worker's the UI went
/// on with raw mode off and its spilled bodies gone. Spill directories go when the process ends
/// (the binary removes them after its runtime), or here when a panic aborts the process.
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let what = panic_text(info);
            // built with panic = "abort", every panic ends the process right after this
            let owns = OWNS_TERMINAL.try_with(Cell::get).unwrap_or(false);
            let plan = PanicPlan::new(owns, TERMINAL_OWNED.load(Ordering::Relaxed), cfg!(panic = "abort"));
            if plan.restore {
                restore_terminal();
                TERMINAL_OWNED.store(false, Ordering::Relaxed);
            }
            tracing::error!("{what}");
            if plan.remove_spill {
                traffic_police_core::store::spill::remove_all();
            }
            if plan.print {
                previous(info);
            } else {
                *WORKER_PANIC.lock().unwrap_or_else(|e| e.into_inner()) = Some(what);
            }
        }));
    });
}

/// What the panic hook does with a panic ([`install_panic_hook`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PanicPlan {
    /// Put the terminal back first, so the message can be read.
    restore: bool,
    /// Remove the spill directories: the process ends now (panic = "abort").
    remove_spill: bool,
    /// Print the message (else it is logged and the footer says a background task failed).
    print: bool,
}

impl PanicPlan {
    /// `owns`: the panic is on the thread that owns the terminal; `owned`: some thread does;
    /// `aborts`: the process ends with the hook.
    fn new(owns: bool, owned: bool, aborts: bool) -> PanicPlan {
        let ends = owns || aborts;
        PanicPlan { restore: ends && owned, remove_spill: aborts, print: ends || !owned }
    }
}

/// `thread 'name' panicked at file:line: message`, for the log.
fn panic_text(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("unnamed");
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(no message)");
    match info.location() {
        Some(at) => format!("thread '{name}' panicked at {at}: {message}"),
        None => format!("thread '{name}' panicked: {message}"),
    }
}

/// The latest panic on a worker since the last call, as the log has it.
pub fn take_worker_panic() -> Option<String> {
    WORKER_PANIC.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Runs a worker's job. A panic in it (a bug) is caught here, so that the job can report that it
/// failed instead of leaving the UI waiting for it; the panic hook has logged it.
pub(crate) fn contained<T>(job: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).ok()
}

pub(crate) fn setup() -> io::Result<Term> {
    terminal::enable_raw_mode()?;
    let mut out = io::stdout();
    if let Err(e) = execute!(out, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste, cursor::Hide) {
        restore_terminal();
        return Err(e);
    }
    match Terminal::new(Screen::new(out)) {
        Ok(t) => {
            OWNS_TERMINAL.set(true);
            TERMINAL_OWNED.store(true, Ordering::Relaxed);
            Ok(Term(ManuallyDrop::new(t)))
        }
        Err(e) => {
            restore_terminal();
            Err(e)
        }
    }
}

pub struct RunOptions {
    /// Probe the terminal for a graphics protocol (otherwise half-blocks).
    pub detect_images: bool,
    /// Connection status from the backend, shown in the header.
    pub status: Option<tokio::sync::watch::Receiver<ConnectionStatus>>,
    /// Another app picked in the session (`A`); `None` where that cannot be (the demo, a file).
    pub switch: Option<Switching>,
}

/// What picking another app in a session needs: the device and app picker on the session's
/// screen, and where the choice goes.
pub struct Switching {
    pub adb: traffic_police_adb::Adb,
    /// Whether a process without the capture runtime can be picked (the agent is then attached
    /// to it), or why not.
    pub attach: Result<(), String>,
    /// Takes the choice: the session moves its capture and its log there.
    pub to: Box<dyn Fn(crate::picker::Picked) + Send>,
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
    app.images = if opts.detect_images {
        let images = Images::detect();
        // a terminal without a graphics protocol may print the query as text (Terminal.app shows
        // `Gi=31,…`), and the first frame only paints the cells it changes. Not
        // `Terminal::clear`: it also asks where the cursor is, and after the picker crossterm's
        // reader gives up on the answer at once.
        term.backend_mut().clear()?;
        images
    } else {
        Images::halfblocks()
    };
    app.switch_apps = opts.switch.is_some();
    // dropping `term` puts the terminal back
    let end = event_loop(&mut term, &mut app, &mut events, opts.status, opts.switch).await;
    // what still runs on workers for the UI is not wanted any more: searches, and files that
    // quitting did not wait for (a signal, a second quit, the terminal gone), which are removed
    app.stop_searches();
    app.stop_exports();
    end
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
    switch: Option<Switching>,
) -> anyhow::Result<()> {
    if let Some(s) = &status {
        app.connection = Some(s.borrow().clone());
    }
    // `None` while an editor owns the terminal: the stream's reader thread would compete with
    // the editor for keystrokes.
    let mut input = Some(EventStream::new());
    let mut editor: Option<Editor> = None;
    let mut editor_ended: Option<Instant> = None;
    let (jq_tx, mut jq_rx) = mpsc::unbounded_channel::<(JqJob, JqResult)>();
    let (body_tx, mut body_rx) = mpsc::unbounded_channel::<(BodyJob, BodyView)>();
    let (search_tx, mut search_rx) = mpsc::unbounded_channel::<Vec<crate::app::SearchResult>>();
    let (rules_tx, mut rules_rx) = mpsc::unbounded_channel::<traffic_police_core::rules::RulesFile>();
    let (export_tx, mut export_rx) = mpsc::unbounded_channel::<(std::path::PathBuf, Result<String, String>)>();
    // started once there is a rules file to watch (r can create it while the UI runs)
    let mut watcher = AbortOnDrop(None);
    let mut backend_open = true;
    let mut stop = StopSignals::new()?;
    let mut pacer = Pacer::new(app.fps);
    // input or data changed what the next frame shows (the clock does that without a flag)
    let mut dirty = true;
    loop {
        // Wake when the next frame is due. None is due with nothing to draw, and none while an
        // editor has the terminal: what changes meanwhile is drawn when the editor returns.
        let pending = editor.is_none() && (dirty || app.is_time_dependent());
        let wake = pending.then(|| tokio::time::Instant::from_std(pacer.next()));
        tokio::select! {
            ev = async { input.as_mut().expect("guarded").next().await }, if input.is_some() => {
                if let Some(end) = on_input(app, ev, &mut dirty) {
                    return end;
                }
            }
            status = async { editor.as_mut().expect("guarded").child.wait().await }, if editor.is_some() => {
                let Editor { path, .. } = editor.take().expect("guarded");
                editor_ended = Some(Instant::now());
                resume_terminal(term)?;
                input = Some(EventStream::new());
                match status {
                    Ok(s) if s.success() => app.flash(format!("returned from the editor ({})", path.display())),
                    Ok(s) => app.flash(format!("the editor exited with {s}")),
                    Err(e) => app.flash(format!("could not wait for the editor: {e}")),
                }
                dirty = true;
                pacer.reset();
            }
            batch = events.recv(), if backend_open => match batch {
                Some(b) => {
                    app.ingest(b);
                    dirty = true;
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
                dirty = true;
            }
            Some((job, result)) = jq_rx.recv() => {
                app.finish_jq(job, result);
                dirty = true;
            }
            Some((job, view)) = body_rx.recv() => {
                app.finish_body(job, view);
                dirty = true;
            }
            Some(results) = search_rx.recv() => {
                app.finish_searches(&results);
                dirty = true;
            }
            Some(f) = rules_rx.recv() => {
                app.rules_reloaded(f);
                dirty = true;
            }
            Some((path, result)) = export_rx.recv() => {
                app.finish_export(&path, result);
                dirty = true;
            }
            signal = stop.recv() => {
                let near_editor = editor.is_some() || editor_ended.is_some_and(|t| t.elapsed() < EDITOR_INTERRUPT);
                if signal == StopSignal::Interrupt && near_editor {
                    continue;
                }
                tracing::info!(?signal, "stopping");
                if let Some(Editor { mut child, .. }) = editor.take() {
                    let _ = child.kill().await;
                }
                return Ok(());
            }
            _ = async { tokio::time::sleep_until(wake.expect("guarded")).await }, if wake.is_some() => {}
        }
        // Whatever this turn woke for, the input and data waiting now go in before the next frame
        // (the data for at most INGEST_PER_TURN). When frames take longer than the frame time (a
        // slow terminal, SSH), the frame timer is ready at every turn and wins most of them, and
        // each turn draws a frame: keys taken only when their branch won would wait many frames
        // each.
        if let Some(stream) = input.as_mut()
            && let Some(end) = take_waiting_input(stream, app, &mut dirty).await
        {
            return end;
        }
        if backend_open {
            let taken = ingest_waiting(events, app, INGEST_PER_TURN);
            dirty |= taken.batches > 0;
            backend_open = !taken.closed;
        }
        if let Some(what) = take_worker_panic() {
            let what = what.split_once(": ").map_or(what.as_str(), |(_, m)| m).to_string();
            app.flash(format!("a background task failed (a bug; details in the log): {what}"));
            dirty = true;
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
        for job in app.take_export_jobs() {
            let tx = export_tx.clone();
            tokio::task::spawn_blocking(move || {
                let path = job.path.clone();
                let result = contained(|| job.run()).unwrap_or_else(|| {
                    Err(format!("{} could not be written: an internal error (details in the log)", path.display()))
                });
                let _ = tx.send((path, result));
            });
        }
        if std::mem::take(&mut app.switch_request)
            && let Some(sw) = &switch
        {
            // the picker reads the keys meanwhile, with its own stream, on this screen; what the
            // backends send waits in the channel and goes in after
            drop(input.take());
            let serial = app.store.current_source().and_then(|s| s.serial.clone());
            let (adb, theme, attach) = (sw.adb.clone(), app.theme.clone(), sw.attach.clone());
            let picked = crate::picker::pick_in_session(term, adb, theme, &app.keymap, attach, serial).await;
            input = Some(EventStream::new());
            match picked {
                Ok(Some(p)) if app.inspecting(&p.serial, &p.process) => {
                    app.flash(format!("{} is the app inspected now", p.process));
                }
                Ok(Some(p)) => {
                    app.switched_to(&p.package, &p.serial);
                    (sw.to)(p);
                }
                Ok(None) => {}
                Err(e) => app.flash(format!("the device picker failed: {e:#}")),
            }
            dirty = true;
            pacer.reset();
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
                    dirty = true;
                    pacer.reset();
                }
            }
        }
        if editor.is_some() {
            continue;
        }
        let started = Instant::now();
        if (dirty || app.is_time_dependent()) && pacer.due(started) {
            term.draw(|f| ui::draw(f, app))?;
            app.frames.record(started, started.elapsed());
            pacer.drawn(started);
            dirty = false;
            // drawing refreshes the rows, which can ask for body searches
            dispatch_jobs(app, &body_tx, &jq_tx, &search_tx);
        }
    }
}

/// Applies the input events already waiting, without waiting for more: at most
/// [`WAITING_INPUT`], and none after one that quits or opens the editor (the editor gets the
/// keys typed after it). `Some` ends the loop with that result, as [`on_input`].
async fn take_waiting_input<S>(stream: &mut S, app: &mut App, dirty: &mut bool) -> Option<anyhow::Result<()>>
where
    S: tokio_stream::Stream<Item = io::Result<Event>> + Unpin,
{
    for _ in 0..WAITING_INPUT {
        if app.should_quit || app.editor_request.is_some() {
            break;
        }
        let ev = tokio::select! {
            biased;
            ev = stream.next() => ev,
            () = std::future::ready(()) => break,
        };
        if let Some(end) = on_input(app, ev, dirty) {
            return Some(end);
        }
    }
    None
}

/// Applies one event from the terminal; `Some` ends the loop with that result (reading the
/// terminal failed, or its input ended).
fn on_input(app: &mut App, ev: Option<io::Result<Event>>, dirty: &mut bool) -> Option<anyhow::Result<()>> {
    match ev {
        Some(Ok(Event::Mouse(m))) if m.kind == MouseEventKind::Moved => None,
        Some(Ok(ev)) => {
            app.handle_event(ev);
            *dirty = true;
            None
        }
        Some(Err(e)) => Some(Err(e.into())),
        None => Some(Ok(())),
    }
}

/// Starts queued work off the UI task: body decodes, jq filters, and body searches.
fn dispatch_jobs(
    app: &mut App,
    body_tx: &mpsc::UnboundedSender<(BodyJob, BodyView)>,
    jq_tx: &mpsc::UnboundedSender<(JqJob, JqResult)>,
    search_tx: &mpsc::UnboundedSender<Vec<crate::app::SearchResult>>,
) {
    for job in app.take_body_jobs() {
        let tx = body_tx.clone();
        tokio::task::spawn_blocking(move || {
            let view = contained(|| job.run()).unwrap_or_else(failed_view);
            let _ = tx.send((job, view));
        });
    }
    for job in app.take_jq_jobs() {
        let runner = app.jq_runner;
        let tx = jq_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = contained(|| runner(&job.filter, &job.bytes))
                .unwrap_or_else(|| Err("the filter stopped with an internal error (details in the log)".into()));
            let _ = tx.send((job, result));
        });
    }
    for batch in app.take_search_jobs() {
        let tx = search_tx.clone();
        tokio::task::spawn_blocking(move || batch.run(|results| tx.send(results).is_ok()));
    }
}

/// What the body box shows for a body whose decoding failed with a bug.
fn failed_view() -> BodyView {
    let mut view = BodyView::build(bytes::Bytes::new(), None);
    view.note = Some("this body could not be shown: an internal error (details in the log)".into());
    view
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
    term.replace(Terminal::new(Screen::new(io::stdout()))?);
    Ok(())
}

/// What asked the UI to stop from outside. Each ends it the way `q` does: the app is told
/// goodbye, which removes the adb forward.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StopSignal {
    /// SIGTERM (Unix).
    #[cfg(unix)]
    Terminate,
    /// SIGHUP, or the console window closed (Windows): the terminal is gone.
    HangUp,
    /// SIGINT or Ctrl+Break. Raw mode delivers Ctrl+C as a key, so this comes from kill, or
    /// from Ctrl+C typed while an editor has the terminal (and is then the editor's).
    Interrupt,
}

#[cfg(unix)]
pub(crate) struct StopSignals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl StopSignals {
    pub(crate) fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(StopSignals {
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    pub(crate) async fn recv(&mut self) -> StopSignal {
        tokio::select! {
            _ = self.term.recv() => StopSignal::Terminate,
            _ = self.hup.recv() => StopSignal::HangUp,
            _ = self.int.recv() => StopSignal::Interrupt,
        }
    }
}

/// When the console window closes, Windows ends the process as soon as the handler returns, or
/// after about 5 seconds; tokio's handler waits, so the goodbye (at most 3 seconds) fits.
#[cfg(windows)]
pub(crate) struct StopSignals {
    close: tokio::signal::windows::CtrlClose,
    brk: tokio::signal::windows::CtrlBreak,
}

#[cfg(windows)]
impl StopSignals {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(StopSignals { close: tokio::signal::windows::ctrl_close()?, brk: tokio::signal::windows::ctrl_break()? })
    }

    pub(crate) async fn recv(&mut self) -> StopSignal {
        tokio::select! {
            _ = self.close.recv() => StopSignal::HangUp,
            _ = self.brk.recv() => StopSignal::Interrupt,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
    use traffic_police_core::SessionStore;

    use super::*;
    use crate::theme::Theme;

    const MS: Duration = Duration::from_millis(1);

    fn key(c: char) -> io::Result<Event> {
        Ok(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)))
    }

    /// Input with events waiting, then nothing more for now. (`tokio_stream::iter` is not
    /// ready after every 32 items, so it cannot stand for input that is all there.)
    struct Waiting(std::collections::VecDeque<io::Result<Event>>);

    impl tokio_stream::Stream for Waiting {
        type Item = io::Result<Event>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.0.pop_front().map_or(std::task::Poll::Pending, |ev| std::task::Poll::Ready(Some(ev)))
        }
    }

    fn waiting(events: impl IntoIterator<Item = io::Result<Event>>) -> Waiting {
        Waiting(events.into_iter().collect())
    }

    #[tokio::test]
    async fn the_keys_waiting_all_go_in_before_the_next_frame() {
        let mut app = App::new(SessionStore::new(), Theme::default());
        let mut input = waiting([key(':'), key('f'), key('p'), key('s')]);
        let mut dirty = false;
        assert!(take_waiting_input(&mut input, &mut app, &mut dirty).await.is_none());
        assert!(dirty);
        assert_eq!(app.palette.as_ref().map(|p| p.input.value()), Some("fps"));
    }

    #[tokio::test]
    async fn keys_after_quit_stay_waiting() {
        let mut app = App::new(SessionStore::new(), Theme::default());
        let enter = Ok(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        let mut input = waiting([key(':'), key('q'), enter, key('j'), key('j')]);
        assert!(take_waiting_input(&mut input, &mut app, &mut false).await.is_none());
        assert!(app.should_quit, ":q Enter quits");
        assert_eq!(input.0.len(), 2);
    }

    #[tokio::test]
    async fn a_flood_of_input_still_lets_a_frame_through() {
        let mut app = App::new(SessionStore::new(), Theme::default());
        let moved = || {
            let m = MouseEvent { kind: MouseEventKind::Moved, column: 1, row: 1, modifiers: KeyModifiers::NONE };
            Ok(Event::Mouse(m))
        };
        let mut input = waiting((0..WAITING_INPUT + 10).map(|_| moved()));
        let mut dirty = false;
        assert!(take_waiting_input(&mut input, &mut app, &mut dirty).await.is_none());
        assert!(!dirty, "moving the mouse changes nothing");
        assert_eq!(input.0.len(), 10);
    }

    #[tokio::test]
    async fn input_that_ends_or_fails_ends_the_loop() {
        let mut app = App::new(SessionStore::new(), Theme::default());
        let mut ended = tokio_stream::iter(vec![key('j')]);
        assert!(matches!(take_waiting_input(&mut ended, &mut app, &mut false).await, Some(Ok(()))));
        let mut failed = waiting([Err(io::Error::other("the terminal went away"))]);
        assert!(matches!(take_waiting_input(&mut failed, &mut app, &mut false).await, Some(Err(_))));
    }

    #[test]
    fn a_panic_in_the_ui_ends_it_one_in_a_worker_leaves_the_terminal_and_the_spilled_bodies() {
        // the UI's: the terminal goes back so the message can be read; spill goes at the end
        let ui = PanicPlan::new(true, true, false);
        assert_eq!(ui, PanicPlan { restore: true, remove_spill: false, print: true });
        // a worker's while the UI runs: nothing of the UI's is touched, the message is logged
        let worker = PanicPlan::new(false, true, false);
        assert_eq!(worker, PanicPlan { restore: false, remove_spill: false, print: false });
        // no UI (before it starts, after it ends): the message as usual
        assert_eq!(PanicPlan::new(false, false, false), PanicPlan { restore: false, remove_spill: false, print: true });
        // with panic = "abort" any panic ends the process: everything at once
        assert_eq!(PanicPlan::new(false, true, true), PanicPlan { restore: true, remove_spill: true, print: true });
    }

    #[tokio::test]
    async fn a_worker_job_that_panics_reports_a_failure() {
        let job = tokio::task::spawn_blocking(|| contained(|| -> bool { panic!("a bug in a decoder") }));
        assert_eq!(job.await.expect("the worker's task ends normally"), None);
        assert_eq!(contained(|| 7), Some(7));
        let view = failed_view();
        assert!(view.note.as_deref().is_some_and(|n| n.contains("internal error")), "{:?}", view.note);
    }

    #[test]
    fn a_turn_takes_in_data_for_its_budget_and_leaves_the_rest_waiting() {
        let mut app = App::new(SessionStore::new(), Theme::default());
        let (tx, mut rx) = mpsc::channel(256);
        let note = |i: u64| traffic_police_core::SessionEvent::Marker {
            source: None,
            at: i,
            kind: traffic_police_core::event::MarkerKind::Note,
            label: String::new(),
        };
        for i in 0..100 {
            tx.try_send(vec![note(i), note(i)]).unwrap();
        }
        // no time at all: one batch, so the data still moves on
        assert_eq!(
            ingest_waiting(&mut rx, &mut app, Duration::ZERO),
            Ingested { batches: 1, events: 2, closed: false }
        );
        // time enough: all that waits, and not a moment's wait for more
        let rest = ingest_waiting(&mut rx, &mut app, Duration::from_secs(60));
        assert_eq!(rest, Ingested { batches: 99, events: 198, closed: false });
        assert_eq!(app.store.markers().len(), 200);
        drop(tx);
        assert!(ingest_waiting(&mut rx, &mut app, Duration::from_secs(60)).closed);
    }

    #[test]
    fn the_first_frame_and_the_first_after_a_pause_are_drawn_at_once() {
        let t0 = Instant::now();
        let mut p = Pacer::new(100);
        assert!(p.due(t0));
        p.drawn(t0);
        assert!(!p.due(t0 + 9 * MS) && p.due(t0 + 10 * MS));
        // a second later nothing has been drawn: due at once, and the beat starts from there
        let t1 = t0 + Duration::from_secs(1);
        assert!(p.due(t1));
        p.drawn(t1);
        assert!(!p.due(t1 + 9 * MS) && p.due(t1 + 10 * MS));
        // after an editor had the terminal
        p.reset();
        assert!(p.due(t1));
    }

    #[test]
    fn frames_keep_their_rate_when_the_timer_wakes_late() {
        for fps in [10, 60, 120, 240] {
            let t0 = Instant::now();
            let mut p = Pacer::new(fps);
            let (mut now, mut frames) = (t0, 0);
            while now < t0 + Duration::from_secs(1) {
                assert!(p.due(now));
                p.drawn(now);
                frames += 1;
                // asleep until the next frame is due, and awake most of a millisecond after that
                now = p.next() + Duration::from_micros(900);
            }
            assert_eq!(frames, fps, "frames in a second at {fps} fps");
        }
    }

    #[test]
    fn a_frame_that_comes_a_whole_frame_late_starts_a_new_beat() {
        let t0 = Instant::now();
        let mut p = Pacer::new(100);
        p.drawn(t0);
        // late by less than a frame: the next one is due on the beat
        p.drawn(t0 + 14 * MS);
        assert!(!p.due(t0 + 19 * MS) && p.due(t0 + 20 * MS));
        // late by more: no burst of frames to catch up
        p.drawn(t0 + 45 * MS);
        assert!(!p.due(t0 + 54 * MS) && p.due(t0 + 55 * MS));
    }

    #[test]
    fn the_rate_stays_in_the_range_of_the_setting() {
        let (min, max) = (u32::from(*FPS_RANGE.start()), u32::from(*FPS_RANGE.end()));
        assert_eq!(Pacer::new(0).frame, Duration::from_secs(1) / min);
        assert_eq!(Pacer::new(u16::MAX).frame, Duration::from_secs(1) / max);
    }
}
