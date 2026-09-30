//! Terminal lifecycle and the event loop (ARCHITECTURE.md §5.2 and §5.8).
//!
//! One task owns the [`App`]: it applies event batches from the backend, handles input, and
//! redraws with frame pacing: at most `[ui] fps` frames a second, whether input, data or the
//! clock changed the picture, and none while it stands still.

use std::io::{self, Stdout};
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::Once;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream,
    MouseEventKind,
};
use crossterm::terminal::{self, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::Terminal;
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
        restore_terminal();
    }
}

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
    match Terminal::new(Screen::new(out)) {
        Ok(t) => Ok(Term(ManuallyDrop::new(t))),
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
    // dropping `term` puts the terminal back
    event_loop(&mut term, &mut app, &mut events, opts.status).await
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
    let mut editor_ended: Option<Instant> = None;
    let (jq_tx, mut jq_rx) = mpsc::unbounded_channel::<(JqJob, JqResult)>();
    let (body_tx, mut body_rx) = mpsc::unbounded_channel::<(BodyJob, BodyView)>();
    let (search_tx, mut search_rx) = mpsc::unbounded_channel::<(crate::app::SearchJob, bool)>();
    let (rules_tx, mut rules_rx) = mpsc::unbounded_channel::<traffic_police_core::rules::RulesFile>();
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
            Some((job, hit)) = search_rx.recv() => {
                app.finish_search(&job, hit);
                dirty = true;
            }
            Some(f) = rules_rx.recv() => {
                app.rules_reloaded(f);
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
                traffic_police_core::store::spill::remove_all();
                return Ok(());
            }
            _ = async { tokio::time::sleep_until(wake.expect("guarded")).await }, if wake.is_some() => {}
        }
        // Whatever this turn woke for, the input and data waiting now go in before the next frame.
        // When frames take longer than the frame time (a slow terminal, SSH), the frame timer is
        // ready at every turn and wins most of them, and each turn draws a frame: keys taken only
        // when their branch won would wait many frames each.
        if let Some(stream) = input.as_mut()
            && let Some(end) = take_waiting_input(stream, app, &mut dirty).await
        {
            return end;
        }
        while backend_open && let Ok(b) = events.try_recv() {
            app.ingest(b);
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
        let mut input = waiting([key('q'), key('j'), key('j')]);
        assert!(take_waiting_input(&mut input, &mut app, &mut false).await.is_none());
        assert!(app.should_quit);
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
