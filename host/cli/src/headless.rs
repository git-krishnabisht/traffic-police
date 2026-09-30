//! Commands without the terminal UI (ARCHITECTURE.md §5.12): `tail`, `record` and `export`.
//! Status goes to stderr; stdout carries only data.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use serde_json::json;
use tokio::sync::{mpsc, watch};
use traffic_police_adb::Adb;
use traffic_police_backends::{DemoConfig, DeviceTarget, run_device};
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::{BackendCommand, ConnectionStatus};
use traffic_police_core::export::har::har;
use traffic_police_core::export::tail::{finished, json_line, text_line};
use traffic_police_core::filter::{BodySearch, Filter};
use traffic_police_core::fmt::Ts;
use traffic_police_core::model::{SourceId, TxnIdx, TxnKey};
use traffic_police_core::session::{DeviceRecord, SessionLog, StreamSink};
use traffic_police_core::store::SessionStore;
use traffic_police_proto::frame::Frame;

/// Ctrl+C and the like, once [`stop_on_signals`] listens for them (a stored permit, so none is
/// missed).
static INTERRUPT: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Makes Ctrl+C, SIGTERM and a closed terminal (SIGHUP; on Windows the console window) end the
/// command properly (the app is told goodbye, which removes the adb forward, and files are
/// finished) instead of killing it.
pub fn stop_on_signals() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            INTERRUPT.notify_one();
        }
    });
    #[cfg(unix)]
    for kind in [tokio::signal::unix::SignalKind::terminate(), tokio::signal::unix::SignalKind::hangup()] {
        if let Ok(mut signal) = tokio::signal::unix::signal(kind) {
            tokio::spawn(async move {
                if signal.recv().await.is_some() {
                    INTERRUPT.notify_one();
                }
            });
        }
    }
    #[cfg(windows)]
    if let Ok(mut close) = tokio::signal::windows::ctrl_close() {
        tokio::spawn(async move {
            if close.recv().await.is_some() {
                INTERRUPT.notify_one();
            }
        });
    }
}

/// A status line on stderr. Unlike `eprintln!` it never panics: once the terminal is closed
/// writing fails, and the command still has to say goodbye and finish its files.
pub fn note(text: impl std::fmt::Display) {
    let _ = writeln!(std::io::stderr(), "traffic-police: {text}");
}

/// `500ms`, `60s` (or `60`), `5m`, `1h`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("{s:?} is not a duration (like 60s, 5m or 1h)");
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (n, unit) = t.split_at(split);
    let n: f64 = n.parse().map_err(|_| bad())?;
    let secs = match unit {
        "ms" => n / 1000.0,
        "" | "s" => n,
        "m" | "min" => n * 60.0,
        "h" => n * 3600.0,
        _ => return Err(bad()),
    };
    Duration::try_from_secs_f64(secs).ok().filter(|d| !d.is_zero()).ok_or_else(bad)
}

/// A filter from the command line, in the filter bar's language; errors point at the problem.
pub fn parse_filter(text: &str) -> anyhow::Result<Option<Filter>> {
    Filter::parse(text).map_err(|e| {
        let start = text[..e.span.start].chars().count();
        let width = text[e.span.clone()].chars().count().max(1);
        anyhow::anyhow!("filter: {}\n  {text}\n  {}{}", e.message, " ".repeat(start), "^".repeat(width))
    })
}

/// Whether request `i` passes the filter (body searches run here, in line).
fn passes(store: &SessionStore, i: TxnIdx, filter: Option<&Filter>, now: Ts) -> bool {
    let Some(f) = filter else { return true };
    let t = store.txn(i);
    f.matches(t, now, &mut |needle| {
        let text = f.needles().get(needle).cloned().unwrap_or_default();
        Some(BodySearch::new(store, t, text).run())
    })
}

/// Where a capture comes from.
pub enum Source {
    Device(DeviceTarget, Adb),
    /// The pretend app of `traffic-police demo` (a hidden `--demo` flag, for trying the commands
    /// and for tests), at `speed`.
    Demo(DemoConfig, f64),
}

/// A capture feeding a store, without the UI.
struct Capture {
    store: SessionStore,
    events: mpsc::Receiver<Vec<SessionEvent>>,
    commands: Option<mpsc::UnboundedSender<BackendCommand>>,
    backend: tokio::task::JoinHandle<()>,
    status: watch::Receiver<ConnectionStatus>,
}

/// What ended a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Interrupted,
    Elapsed,
    /// The app exited (without --follow), or the connection failed.
    Ended,
    /// `each` asked to stop.
    Done,
}

impl Stop {
    fn text(self) -> &'static str {
        match self {
            Stop::Interrupted => "stopped",
            Stop::Elapsed => "time is up",
            Stop::Ended => "the capture ended",
            Stop::Done => "done",
        }
    }
}

impl Capture {
    async fn start(source: Source, sink: Option<Arc<dyn StreamSink>>) -> anyhow::Result<Capture> {
        let store = SessionStore::new();
        let ids = store.source_ids();
        let (event_tx, events) = mpsc::channel(256);
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(ConnectionStatus::Waiting("looking for the device…".into()));
        let backend = match source {
            Source::Device(target, adb) => {
                adb.ensure_server().await.context("traffic-police talks to devices through the adb server")?;
                tokio::spawn(run_device(adb, target, ids, event_tx, command_rx, status_tx, sink))
            }
            Source::Demo(cfg, speed) => {
                let _ = status_tx.send(ConnectionStatus::Live("the demo app".into()));
                tokio::spawn(async move {
                    traffic_police_backends::run_demo(cfg, speed, ids, event_tx, command_rx, sink).await;
                    drop(status_tx);
                })
            }
        };
        Ok(Capture { store, events, commands: Some(command_tx), backend, status })
    }

    /// Runs until Ctrl+C, the duration, the backend's end, or `each` returning false; `each`
    /// sees the store after every batch. Connection changes are reported on stderr.
    async fn run(&mut self, duration: Option<Duration>, mut each: impl FnMut(&SessionStore) -> bool) -> Stop {
        let deadline = duration.map(|d| tokio::time::Instant::now() + d);
        let mut status_open = true;
        let mut last = String::new();
        let status_text = |st: &ConnectionStatus| match st {
            ConnectionStatus::Live(s) => format!("capturing {s} · Ctrl+C stops"),
            ConnectionStatus::Waiting(s) => s.clone(),
            ConnectionStatus::Detached(s) => format!("detached: {s}"),
            ConnectionStatus::Failed(s) => format!("failed: {s}"),
        };
        loop {
            tokio::select! {
                batch = self.events.recv() => match batch {
                    Some(b) => {
                        self.store.apply_all(b);
                        if !each(&self.store) {
                            return Stop::Done;
                        }
                    }
                    None => {
                        // the backend's last word (the app exited, or the connection failed)
                        let text = status_text(&self.status.borrow());
                        if text != last {
                            note(&text);
                        }
                        return Stop::Ended;
                    }
                },
                changed = self.status.changed(), if status_open => {
                    if changed.is_err() {
                        // the backend ended; its last events are still in the channel
                        status_open = false;
                        continue;
                    }
                    let text = status_text(&self.status.borrow());
                    if text != last {
                        note(&text);
                        last = text;
                    }
                }
                () = INTERRUPT.notified() => return Stop::Interrupted,
                () = sleep_until(deadline) => return Stop::Elapsed,
            }
        }
    }

    /// Ends the capture: the backend says goodbye to the app (which removes our forward, and
    /// records the end in a session), and the last events are applied. `each` sees them too.
    async fn stop(mut self, mut each: impl FnMut(&SessionStore) -> bool) -> SessionStore {
        self.commands = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while let Ok(Some(batch)) = tokio::time::timeout_at(deadline, self.events.recv()).await {
            self.store.apply_all(batch);
            each(&self.store);
        }
        let _ = tokio::time::timeout_at(deadline, &mut self.backend).await;
        self.store
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Writes lines (to stdout); notices when the reader went away (`| head`).
pub struct Lines {
    out: Mutex<Box<dyn Write + Send>>,
    closed: AtomicBool,
}

impl Lines {
    pub fn new(out: Box<dyn Write + Send>) -> Lines {
        Lines { out: Mutex::new(out), closed: AtomicBool::new(false) }
    }

    fn write(&self, line: &dyn std::fmt::Display) {
        if self.is_closed() {
            return;
        }
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            self.closed.store(true, Ordering::Relaxed);
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// `tail --events`: each captured message as a line, as the app sent it.
struct EventLines {
    lines: Arc<Lines>,
    bodies: bool,
}

impl StreamSink for EventLines {
    fn source(&self, source: SourceId, device: &DeviceRecord, hello: &[u8], resumed: bool) {
        let hello: serde_json::Value = serde_json::from_slice(hello).unwrap_or_default();
        self.lines.write(&json!({
            "v": 1, "type": "source", "source": source,
            "device": { "label": device.label, "serial": device.serial },
            "resumed": resumed, "hello": hello,
        }));
    }

    fn frame(&self, source: SourceId, f: &Frame) {
        match f {
            Frame::Json(j) => {
                let msg: serde_json::Value = serde_json::from_slice(j).unwrap_or_default();
                self.lines.write(&json!({ "v": 1, "type": "event", "source": source, "msg": msg }));
            }
            Frame::Body(c) => {
                let mut v = json!({
                    "v": 1, "type": "body", "source": source, "txn": c.txn, "dir": c.dir.as_str(),
                    "seq": c.seq, "ts": c.ts, "offset": c.offset, "len": c.data.len(),
                });
                if self.bodies {
                    v["base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&c.data));
                }
                self.lines.write(&v);
            }
            Frame::Other { .. } => {}
        }
    }

    fn source_end(&self, source: SourceId, at: Ts, reason: &str) {
        self.lines.write(&json!({ "v": 1, "type": "source_end", "source": source, "ts": at, "reason": reason }));
    }
}

/// What `tail` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailFormat {
    Text,
    /// PROTOCOL.md Appendix B; `bodies` adds them.
    Json {
        bodies: bool,
    },
    /// Every captured message; `bodies` adds body chunks' bytes.
    Events {
        bodies: bool,
    },
}

/// `tail`: a line per finished request that passes the filter (or per captured message), until
/// Ctrl+C, the duration, or the app's exit.
pub async fn tail(
    source: Source,
    filter: Option<Filter>,
    format: TailFormat,
    duration: Option<Duration>,
    lines: Arc<Lines>,
) -> anyhow::Result<()> {
    if let TailFormat::Events { bodies } = format {
        let sink = Arc::new(EventLines { lines: lines.clone(), bodies });
        let mut cap = Capture::start(source, Some(sink)).await?;
        cap.run(duration, |_| !lines.is_closed()).await;
        cap.stop(|_| true).await;
        return Ok(());
    }
    let mut cap = Capture::start(source, None).await?;
    // requests not printed yet (unfinished)
    let mut open: Vec<TxnIdx> = Vec::new();
    let mut seen = 0usize;
    let mut print = |store: &SessionStore| {
        open.extend((seen..store.len()).map(|i| i as TxnIdx));
        seen = store.len();
        let now = store.latest();
        open.retain(|&i| {
            if !finished(store.txn(i)) {
                return true;
            }
            if passes(store, i, filter.as_ref(), now) {
                match format {
                    TailFormat::Json { bodies } => lines.write(&json_line(store, i, now, bodies)),
                    _ => lines.write(&text_line(store, i, now)),
                }
            }
            false
        });
        !lines.is_closed()
    };
    cap.run(duration, &mut print).await;
    cap.stop(&mut print).await;
    Ok(())
}

/// `record`: a session file, written as the capture happens (with a filter: at the end, with
/// the matching requests only).
pub async fn record(
    source: Source,
    out: &Path,
    duration: Option<Duration>,
    filter: Option<Filter>,
) -> anyhow::Result<()> {
    if out.exists() {
        bail!("{} exists; choose another name", out.display());
    }
    let log = Arc::new(match &filter {
        None => SessionLog::file(out).with_context(|| format!("cannot write {}", out.display()))?,
        Some(_) => SessionLog::temporary().context("cannot create a temporary recording")?,
    });
    let mut cap = Capture::start(source, Some(log.clone() as Arc<dyn StreamSink>)).await?;
    let mut reported = 0usize;
    let stop = cap
        .run(duration, |store| {
            if store.len() >= reported + 50 {
                reported = store.len() / 50 * 50;
                note(format_args!("{} requests so far", store.len()));
            }
            true
        })
        .await;
    let store = cap.stop(|_| true).await;
    match &filter {
        None => {
            log.finish(&store).with_context(|| format!("cannot finish {}", out.display()))?;
            note(format_args!("{}; recorded {} requests to {}", stop.text(), store.len(), out.display()));
        }
        Some(f) => {
            let now = store.latest();
            let keep: HashSet<TxnKey> = (0..store.len() as TxnIdx)
                .filter(|&i| passes(&store, i, Some(f), now))
                .map(|i| store.txn(i).key)
                .collect();
            let n = log.export(&store, out, Some(&keep)).with_context(|| format!("cannot write {}", out.display()))?;
            note(format_args!(
                "{}; recorded {n} of {} requests (those matching {:?}) to {}",
                stop.text(),
                store.len(),
                f.source,
                out.display()
            ));
        }
    }
    Ok(())
}

/// Where `export` takes requests from.
pub enum ExportInput<'a> {
    File(&'a Path),
    Live(Source, Duration),
}

/// `export --har`: the requests of a saved session, or of a live capture, as a HAR file.
pub async fn export_har(out: &Path, input: ExportInput<'_>, filter: Option<Filter>) -> anyhow::Result<()> {
    if out.exists() {
        bail!("{} exists; choose another name", out.display());
    }
    let store = match input {
        ExportInput::File(path) => open_file(path)?,
        ExportInput::Live(source, duration) => {
            let mut cap = Capture::start(source, None).await?;
            let stop = cap.run(Some(duration), |_| true).await;
            note(stop.text());
            cap.stop(|_| true).await
        }
    };
    let now = store.latest();
    let mut txns: Vec<TxnIdx> =
        (0..store.len() as TxnIdx).filter(|&i| passes(&store, i, filter.as_ref(), now)).collect();
    txns.sort_by_key(|&i| store.txn(i).start);
    let doc = har(&store, &txns, now);
    let bytes = serde_json::to_vec_pretty(&doc)?;
    std::fs::write(out, bytes).with_context(|| format!("cannot write {}", out.display()))?;
    note(format_args!("wrote {} of {} requests to {}", txns.len(), store.len(), out.display()));
    Ok(())
}

/// A saved session or HAR file, read into a store.
fn open_file(path: &Path) -> anyhow::Result<SessionStore> {
    let mut store = SessionStore::new();
    let opened = traffic_police_core::import::open(path, &store.source_ids())
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", path.display()))?;
    for s in &opened.skipped {
        note(format_args!("{}: left out {s}", path.display()));
    }
    store.apply_all(opened.events);
    for key in opened.pins {
        if let Some(i) = store.find(key) {
            store.set_pinned(i, true);
        }
    }
    if opened.truncated {
        note(format_args!("{} was cut short; exporting what it holds", path.display()));
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Lines written to memory.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// The demo app, 20 times faster than real time.
    fn demo() -> Source {
        Source::Demo(DemoConfig::default(), 20.0)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tp-headless-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn tail_prints_a_json_line_per_finished_request_that_matches() {
        let buf = Shared::default();
        let lines = Arc::new(Lines::new(Box::new(buf.clone())));
        let filter = parse_filter("method:GET").unwrap();
        let format = TailFormat::Json { bodies: true };
        tail(demo(), filter, format, Some(Duration::from_millis(800)), lines).await.unwrap();
        let text = buf.text();
        let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert!(rows.len() >= 3, "{text}");
        for r in &rows {
            assert_eq!((&r["v"], &r["type"], &r["method"]), (&json!(1), &json!("txn"), &json!("GET")), "{r}");
            assert_ne!(r["state"], "waiting");
        }
        assert!(rows.iter().any(|r| r["response"]["body"].is_object()), "bodies are included");
    }

    #[tokio::test]
    async fn tail_events_prints_the_source_then_its_messages() {
        let buf = Shared::default();
        let lines = Arc::new(Lines::new(Box::new(buf.clone())));
        let format = TailFormat::Events { bodies: false };
        tail(demo(), None, format, Some(Duration::from_millis(400)), lines).await.unwrap();
        let text = buf.text();
        let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows[0]["type"], "source", "{text}");
        assert_eq!(rows[0]["hello"]["t"], "hello");
        assert!(rows.iter().any(|r| r["type"] == "event" && r["msg"]["t"] == "req"), "{text}");
        assert!(rows.iter().any(|r| r["type"] == "body" && r.get("base64").is_none()), "{text}");
    }

    #[tokio::test]
    async fn record_writes_sessions_that_reopen_and_export() {
        let dir = scratch("record");
        let all = dir.join("all.trafficpolice");
        record(demo(), &all, Some(Duration::from_millis(600)), None).await.unwrap();
        let store = open_file(&all).unwrap();
        assert!(store.len() >= 3, "{} requests", store.len());
        assert!(record(demo(), &all, Some(Duration::from_millis(100)), None).await.is_err(), "never overwrites");

        let posts = dir.join("posts.trafficpolice");
        record(demo(), &posts, Some(Duration::from_millis(600)), parse_filter("method:POST").unwrap()).await.unwrap();
        let store = open_file(&posts).unwrap();
        assert!(!store.is_empty());
        assert!(store.txns().iter().all(|t| t.method == "POST"));

        let har_path = dir.join("ok.har");
        export_har(&har_path, ExportInput::File(&all), parse_filter("status:2xx").unwrap()).await.unwrap();
        let doc: Value = serde_json::from_slice(&std::fs::read(&har_path).unwrap()).unwrap();
        let entries = doc["log"]["entries"].as_array().unwrap();
        assert!(!entries.is_empty());
        assert!(entries.iter().all(|e| e["response"]["status"].as_u64().is_some_and(|s| (200..300).contains(&s))));
        // a HAR reads back too
        let again = dir.join("again.har");
        export_har(&again, ExportInput::File(&har_path), None).await.unwrap();
        let doc2: Value = serde_json::from_slice(&std::fs::read(&again).unwrap()).unwrap();
        let urls = |d: &Value| {
            d["log"]["entries"].as_array().unwrap().iter().map(|e| e["request"]["url"].clone()).collect::<Vec<_>>()
        };
        assert_eq!(urls(&doc2), urls(&doc));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("60"), Ok(Duration::from_secs(60)));
        assert_eq!(parse_duration("1.5s"), Ok(Duration::from_millis(1500)));
        assert_eq!(parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        for bad in ["", "s", "5x", "0s", "-1s", "1e3s"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn filter_errors_point_at_the_problem() {
        let f = parse_filter("method:GET status:4xx").unwrap().unwrap();
        assert_eq!(f.source, "method:GET status:4xx");
        assert!(parse_filter("").unwrap().is_none());
        let e = parse_filter("method:GET status:abc").unwrap_err().to_string();
        assert!(e.ends_with("  method:GET status:abc\n             ^^^^^^^^^^"), "{e}");
    }
}
