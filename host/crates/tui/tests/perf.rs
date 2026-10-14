//! Frame time with 50,000 transactions (ARCHITECTURE.md §6: at most 33 ms on average, target
//! under 8 ms, and no frame over [`WORST_FRAME`]): the views, groups of up to 50,000 repeats, a
//! `body:` filter typed while its searches run, a HAR written in the background, data coming in;
//! with half a million lines of the device's log in Logdawg; and with bodies that are large or
//! decode large.
//!
//! Ignored by default because it is only meaningful in release builds:
//! `cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture`

mod common;

use std::time::{Duration, Instant};

use bytes::Bytes;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use traffic_police_backends::demo::DemoSession;
use traffic_police_core::SessionEvent;
use traffic_police_core::event::{RequestStarted, ResponseStarted};
use traffic_police_core::fmt::{NS_PER_MS, NS_PER_SEC};
use traffic_police_core::model::{TxnIdx, TxnKey};
use traffic_police_core::rows::Row;
use traffic_police_proto::BodyDir;
use traffic_police_proto::msg::{ClientInfo, StackFrame, ThreadInfo};
use traffic_police_tui::app::{Focus, SearchResult, Tab};
use traffic_police_tui::terminal::{INGEST_PER_TURN, ingest_waiting};
use traffic_police_tui::{App, parse_keys, render_keys, render_text, ui};

const N: u64 = 50_000;

/// A frame's budget on average.
const BUDGET: Duration = Duration::from_millis(33);

/// The worst frame allowed in any case: three times the average budget, room for the scheduling
/// of a busy CI machine, and well under the stalls these cases are here to catch (hundreds of
/// milliseconds and more: a HAR of 50,000 requests made on the UI task took 0.6 to 3.4 s).
const WORST_FRAME: Duration = Duration::from_millis(100);

/// The cases take turns, so that each is measured alone: the test harness runs them at once,
/// and CI's machines have four cores.
fn alone() -> std::sync::MutexGuard<'static, ()> {
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}

/// A case's frames: on average and at worst.
struct Frames {
    label: String,
    avg: Duration,
    worst: Duration,
}

impl Frames {
    fn new(label: &str, total: Duration, frames: u32, worst: Duration) -> Frames {
        let avg = total / frames.max(1);
        println!("{label:<44} avg {avg:>8.2?}  worst {worst:>8.2?}");
        Frames { label: label.into(), avg, worst }
    }

    /// Within [`BUDGET`] on average and [`WORST_FRAME`] at worst.
    fn check(&self) {
        let Frames { label, avg, worst } = self;
        assert!(*avg < BUDGET, "{label}: a frame took {avg:?} on average, over the {BUDGET:?} budget");
        assert!(*worst < WORST_FRAME, "{label}: a frame took {worst:?}, over {WORST_FRAME:?}");
    }
}

/// `count` completed requests for the demo's source, `gap` apart, ending at `end`.
fn synth(source: u32, first_txn: u64, count: u64, end: u64, gap: u64) -> Vec<SessionEvent> {
    let mut out = Vec::with_capacity(count as usize * 6);
    let threads = [
        "main",
        "OkHttp Dispatcher",
        "DefaultDispatcher-worker-1",
        "DefaultDispatcher-worker-2",
        "Telemetry-Uploader",
        "glide-source-thread-0",
    ];
    let paths = [
        "/api/v1/notifications",
        "/api/v1/orders/status",
        "/v1/events",
        "/avatars/u_1.png",
        "/api/v1/sessions",
        "/v1/metrics",
    ];
    for i in 0..count {
        let key = TxnKey { source, txn: first_txn + i };
        let at = end - (count - i) * gap;
        let th = (i % threads.len() as u64) as usize;
        out.push(SessionEvent::Request(Box::new(RequestStarted {
            key,
            at,
            call: Some(first_txn + i),
            hop: 0,
            method: if i % 3 == 0 { "POST".into() } else { "GET".into() },
            url: format!("https://api.example.app{}?page={}", paths[(i % 6) as usize], i % 17),
            headers: vec![("Accept".into(), "application/json".into()), ("Authorization".into(), "Bearer abc".into())],
            client: Some(ClientInfo { kind: "okhttp".into(), version: Some("4.12.0".into()) }),
            thread: Some(ThreadInfo {
                id: 50 + th as i64,
                name: threads[th].into(),
                tid: None,
                origin: Some("call".into()),
            }),
            stack: vec![StackFrame {
                c: "com.example.App".into(),
                m: "load".into(),
                f: Some("App.kt".into()),
                l: Some(12),
            }],
            stack_truncated: false,
            body: None,
            marks: Vec::new(),
            conn: None,
        })));
        let rt = at + 40 * NS_PER_MS + (i % 7) * 30 * NS_PER_MS;
        out.push(SessionEvent::Response(Box::new(ResponseStarted {
            key,
            at: rt,
            status: if i % 50 == 0 { 500 } else { 200 },
            message: "OK".into(),
            protocol: Some("h2".into()),
            headers: vec![("content-type".into(), "application/json".into())],
            conn: None,
        })));
        let body = Bytes::from(format!("{{\"i\":{i},\"ok\":true,\"items\":[1,2,3]}}"));
        let len = body.len() as u64;
        out.push(SessionEvent::Body { key, dir: BodyDir::Response, at: rt + NS_PER_MS, offset: 0, bytes: body });
        out.push(SessionEvent::BodyEnd {
            key,
            dir: BodyDir::Response,
            at: rt + 2 * NS_PER_MS,
            total: len,
            captured: len,
            state: "complete".into(),
            decoded: false,
        });
        out.push(SessionEvent::Completed { key, at: rt + 2 * NS_PER_MS });
    }
    out
}

fn big_app() -> (App, DemoSession, u64) {
    let mut app = common::app_at(2.0);
    let session = DemoSession::new(Default::default(), app.store.source_ids());
    let source = app.store.current_source().expect("demo source").id;
    let end = DemoSession::clock_at(2 * NS_PER_SEC);
    app.ingest(synth(source, 10_000_000, N, end, 40 * NS_PER_MS));
    app.now_override = Some(end);
    (app, session, end)
}

fn time_frames(label: &str, app: &mut App, frames: u32, mut between: impl FnMut(&mut App, u32)) -> Frames {
    let (w, h) = common::LARGE;
    render_text(app, w, h);
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    for i in 0..frames {
        between(app, i);
        let t = Instant::now();
        render_text(app, w, h);
        worst = worst.max(t.elapsed());
    }
    Frames::new(label, start.elapsed(), frames, worst)
}

/// Requests to one endpoint in runs of `run`, so that collapsing repeats makes groups of `run`.
fn synth_runs(source: u32, first_txn: u64, count: u64, end: u64, gap: u64, run: u64) -> Vec<SessionEvent> {
    let mut out = synth(source, first_txn, count, end, gap);
    for e in &mut out {
        if let SessionEvent::Request(r) = e {
            r.method = "GET".into();
            r.url = format!("https://api.example.app/api/v1/poll/{}", (r.key.txn - first_txn) / run);
        }
    }
    out
}

#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn frame_time_with_50k_transactions() {
    let _alone = alone();
    let (mut app, _session, end) = big_app();
    assert!(app.store.len() as u64 >= N);
    let mut results = Vec::new();
    results.push(time_frames("Connection View, nothing new", &mut app, 200, |_, _| {}));
    // a new request every frame forces the row model to rebuild
    let source = app.store.current_source().unwrap().id;
    let mut next = 20_000_000u64;
    let mut now = end;
    results.push(time_frames("Connection View, new request every frame", &mut app, 200, |app, _| {
        now += 33 * NS_PER_MS;
        app.ingest(synth(source, next, 1, now, 10 * NS_PER_MS));
        app.now_override = Some(now);
        next += 1;
    }));
    render_keys(&mut app, 200, 50, &parse_keys("c").unwrap());
    results.push(time_frames("collapsed repeats, new request every frame", &mut app, 100, |app, _| {
        now += 33 * NS_PER_MS;
        app.ingest(synth(source, next, 1, now, 10 * NS_PER_MS));
        app.now_override = Some(now);
        next += 1;
    }));
    render_keys(&mut app, 200, 50, &parse_keys("cs").unwrap());
    results.push(time_frames("sorted by Name, new request every frame", &mut app, 100, |app, _| {
        now += 33 * NS_PER_MS;
        app.ingest(synth(source, next, 1, now, 10 * NS_PER_MS));
        app.now_override = Some(now);
        next += 1;
    }));
    app.apply_filter("method:GET path:/api/** -status:5xx header:authorization");
    results.push(time_frames("filtered, new request every frame", &mut app, 100, |app, _| {
        now += 33 * NS_PER_MS;
        app.ingest(synth(source, next, 1, now, 10 * NS_PER_MS));
        app.now_override = Some(now);
        next += 1;
    }));
    app.apply_filter("");
    render_keys(&mut app, 200, 50, &parse_keys("<Esc>2").unwrap());
    results.push(time_frames("Thread View", &mut app, 200, |_, _| {}));
    render_keys(&mut app, 200, 50, &parse_keys("1G<Enter>l").unwrap());
    results.push(time_frames("detail open on Response", &mut app, 200, |_, _| {}));
    render_keys(&mut app, 200, 50, &parse_keys("<Esc>----").unwrap());
    results.push(time_frames("graph zoomed out to 10 minutes", &mut app, 200, |_, _| {}));
    results.iter().for_each(Frames::check);
}

/// Repeats of one endpoint collapsed into groups of 50 to 50,000 (ARCHITECTURE.md §5.7), with the
/// graph's window over all of them, so that each group's row draws its members' bars; then a
/// member more every frame. Up to 0.5.1 every member's bar was drawn: 22 to 24 ms a frame for
/// groups of 5,000 and 50,000, and 92 to 131 ms at worst.
#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn frame_time_with_large_groups() {
    let _alone = alone();
    let mut results = Vec::new();
    for run in [50, 5_000, N] {
        let mut app = common::app_at(2.0);
        let source = app.store.current_source().expect("demo source").id;
        let end = DemoSession::clock_at(2 * NS_PER_SEC);
        app.ingest(synth_runs(source, 10_000_000, N, end, 40 * NS_PER_MS, run));
        app.now_override = Some(end);
        // collapsed, and a window of an hour: the 33 minutes of requests in it
        render_keys(&mut app, 200, 50, &parse_keys("c------").unwrap());
        let groups = app.view_rows().rows().iter().filter(|r| matches!(r, Row::Group { .. })).count();
        assert!(groups as u64 >= N / run, "{groups} groups");
        results.push(time_frames(&format!("groups of {run}, nothing new"), &mut app, 100, |_, _| {}));
        let (mut now, mut next) = (end, 20_000_000u64);
        let last = format!("https://api.example.app/api/v1/poll/{}", (N - 1) / run);
        results.push(time_frames(&format!("groups of {run}, a member more every frame"), &mut app, 100, |app, _| {
            now += 33 * NS_PER_MS;
            let mut one = synth(source, next, 1, now, 10 * NS_PER_MS);
            if let SessionEvent::Request(r) = &mut one[0] {
                r.method = "GET".into();
                r.url = last.clone();
            }
            app.ingest(one);
            app.now_override = Some(now);
            next += 1;
        }));
    }
    results.iter().for_each(Frames::check);
}

/// A `body:` filter typed into a session of 50,000 requests, a key a frame, with the searches on
/// worker threads as the live loop runs them, until all 50,000 have been searched. Up to 0.5.1
/// each keystroke made a search of every body on the UI task.
#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn frame_time_while_a_body_filter_is_typed() {
    let _alone = alone();
    let (mut app, _session, _end) = big_app();
    let mut term = Terminal::new(TestBackend::new(common::LARGE.0, common::LARGE.1)).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<SearchResult>>();
    let mut keys = parse_keys("/body:\"items\"<Enter>").unwrap().into_iter();
    let (start, mut total, mut worst, mut frames) = (Instant::now(), Duration::ZERO, Duration::ZERO, 0);
    loop {
        // a turn of the loop: the results that came, a key, a frame, the searches to start
        let t = Instant::now();
        while let Ok(r) = rx.try_recv() {
            app.finish_searches(&r);
        }
        let key = keys.next();
        if let Some(k) = key {
            app.handle_key(k);
        }
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        for batch in app.take_search_jobs() {
            let tx = tx.clone();
            std::thread::spawn(move || batch.run(|r| tx.send(r).is_ok()));
        }
        let took = t.elapsed();
        (total, worst, frames) = (total + took, worst.max(took), frames + 1);
        // every synthetic body has "items"
        if key.is_none() && app.view_rows().len() as u64 >= N && !app.has_queued_jobs() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "the searches did not end");
        std::thread::sleep(Duration::from_millis(16));
    }
    println!("{:<44} {:>8.2?}", "  every body searched after", start.elapsed());
    Frames::new("typing body:\"items\", 50,000 searched", total, frames, worst).check();
}

/// A HAR of 50,000 requests written while the UI goes on, a new request every frame: the UI task
/// only takes the snapshot. Up to 0.5.1 the UI task made the file: 0.6 to 3.4 s.
#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn frame_time_while_a_har_is_written() {
    let _alone = alone();
    let (mut app, _session, end) = big_app();
    let source = app.store.current_source().expect("demo source").id;
    let path = std::env::temp_dir().join(format!("tp-perf-{}.har", std::process::id()));
    let _ = std::fs::remove_file(&path);
    render_keys(&mut app, 200, 50, &parse_keys("ea").unwrap());
    app.prompt.as_mut().expect("the path's prompt").input = tui_input::Input::new(path.display().to_string());
    let requests = app.store.len();
    let t = Instant::now();
    app.finish_prompt();
    let snapshot = t.elapsed();
    let (tx, rx) = std::sync::mpsc::channel();
    for job in app.take_export_jobs() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let path = job.path.clone();
            let _ = tx.send((path, job.run()));
        });
    }
    let mut term = Terminal::new(TestBackend::new(common::LARGE.0, common::LARGE.1)).unwrap();
    let (start, mut total, mut worst, mut frames) = (Instant::now(), Duration::ZERO, Duration::ZERO, 0);
    let (mut now, mut next) = (end, 20_000_000u64);
    loop {
        if let Ok((p, result)) = rx.try_recv() {
            app.finish_export(&p, result);
            break;
        }
        let t = Instant::now();
        now += 33 * NS_PER_MS;
        app.ingest(synth(source, next, 1, now, 10 * NS_PER_MS));
        app.now_override = Some(now);
        next += 1;
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        let took = t.elapsed();
        (total, worst, frames) = (total + took, worst.max(took), frames + 1);
        assert!(start.elapsed() < Duration::from_secs(60), "the export did not end");
        std::thread::sleep(Duration::from_millis(16).saturating_sub(took));
    }
    println!("{:<44} {snapshot:>8.2?}, written in {:.2?}", "HAR of 50,000: the snapshot", start.elapsed());
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(doc["log"]["entries"].as_array().map(Vec::len), Some(requests));
    assert!(app.current_message().is_some_and(|m| m.starts_with("wrote ")), "{:?}", app.current_message());
    assert!(snapshot < WORST_FRAME, "the snapshot took {snapshot:?}");
    Frames::new("frames while it is written, a request each", total, frames, worst).check();
}

/// Ingest through the UI's channel (ARCHITECTURE.md §6: 20,000 events a second sustained, a
/// 50,000-event replay in under a second), into a store of 50,000 requests, while the UI takes
/// them in for at most [`INGEST_PER_TURN`] a turn and draws at 60 frames a second: first a burst
/// of 50,000 events in batches of 200 as fast as the channel takes them, then 40,000 events a
/// second for a second, 400 every 10 ms, as a busy app's socket brings them, which the UI takes
/// as they come.
#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn ingest_through_the_ui_while_it_draws() {
    let _alone = alone();
    let (mut app, _session, end) = big_app();
    let source = app.store.current_source().expect("demo source").id;
    let mut term = Terminal::new(TestBackend::new(common::LARGE.0, common::LARGE.1)).unwrap();
    let mut next_txn = 40_000_000;
    let mut now = end;
    let mut run = |label: &str, requests: u64, pace: Option<Duration>| {
        now += requests * 2 * NS_PER_MS;
        let events = synth(source, next_txn, requests, now, 2 * NS_PER_MS);
        next_txn += requests;
        let total = events.len();
        let size = if pace.is_some() { 400 } else { 200 };
        let batches: Vec<Vec<SessionEvent>> = events.chunks(size).map(<[SessionEvent]>::to_vec).collect();
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        let backend = std::thread::spawn(move || {
            let mut last = Instant::now();
            for b in batches {
                if tx.blocking_send(b).is_err() {
                    break;
                }
                last = Instant::now();
                if let Some(p) = pace {
                    std::thread::sleep(p);
                }
            }
            last
        });
        let frame = Duration::from_secs(1) / 60;
        let start = Instant::now();
        let (mut taken, mut frames, mut next_frame) = (0, 0, start);
        let (mut worst_turn, mut worst_frame) = (Duration::ZERO, Duration::ZERO);
        while taken < total {
            let t = Instant::now();
            let turn = ingest_waiting(&mut rx, &mut app, INGEST_PER_TURN);
            worst_turn = worst_turn.max(t.elapsed());
            taken += turn.events;
            assert!(!turn.closed || taken == total, "the backend ended early");
            if Instant::now() >= next_frame {
                app.now_override = Some(now);
                let t = Instant::now();
                term.draw(|f| ui::draw(f, &mut app)).unwrap();
                worst_frame = worst_frame.max(t.elapsed());
                frames += 1;
                next_frame = Instant::now() + frame;
            }
            if turn.batches == 0 {
                std::thread::yield_now();
            }
        }
        let done = Instant::now();
        let took = done - start;
        // how long after the backend sent its last batch the UI had it all
        let behind = done.saturating_duration_since(backend.join().unwrap());
        let rate = total as f64 / took.as_secs_f64();
        println!(
            "{label:<44} {took:>8.2?}  ({rate:.0} events/s, {frames} frames, worst turn {worst_turn:.2?}, worst frame {worst_frame:.2?}, {behind:.2?} behind)"
        );
        assert!(worst_frame < WORST_FRAME, "a frame took {worst_frame:?}");
        (took, rate, behind)
    };
    let (took, _, _) = run("ingest: a burst of 50,000 events", 10_000, None);
    assert!(took < Duration::from_secs(1), "50,000 events took {took:?}");
    let (_, rate, behind) = run("ingest: 40,000 events a second for a second", 8_000, Some(Duration::from_millis(10)));
    assert!(rate > 20_000.0, "{rate:.0} events a second");
    assert!(behind < WORST_FRAME, "the UI was {behind:?} behind the backend");
}

/// Logdawg (ARCHITECTURE.md §5.17): half a million lines, what a busy device writes in about
/// forty minutes, one in ten of them the app's; then a new batch every frame while following.
#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn logdawg_with_500k_lines() {
    let _alone = alone();
    use traffic_police_core::logdawg::{Level, LogLine};
    let _alone = alone();
    const LINES: u64 = 500_000;
    const APP_UID: u32 = 10_234;
    let tags = [
        "OkHttp",
        "ActivityManager",
        "chatty",
        "SurfaceFlinger",
        "wificond",
        "NetworkScheduler.Stats",
        "CheckoutViewModel",
        "Choreographer",
        "ConnectivityService",
        "BoundBrokerSvc",
        "WindowManager",
        "AndroidRuntime",
    ];
    let messages = [
        "--> GET https://api.example.app/api/v1/notifications?page=3",
        "<-- 200 OK https://api.example.app/api/v1/orders/status (412ms)",
        "Slow operation: 112ms so far, now at startProcess",
        "uid=1000(system) android.bg identical 3 lines",
        "Finished setting power mode 2 on display 0",
        "NetworkAgentInfo [WIFI () - 100] validation passed",
        "<-- HTTP FAILED: java.net.SocketTimeoutException: timeout",
        "Skipped 85 frames!  The application may be doing too much work on its main thread.",
    ];
    let mut app = common::app_at(2.0);
    app.store.logs_mut().set_keep(1 << 30);
    let start = DemoSession::clock_at(2 * NS_PER_SEC);
    let line = |i: u64| {
        let mine = i.is_multiple_of(10);
        let pid = if mine { 4312 } else { 600 + (i % 37) as u32 };
        LogLine {
            ts: start + i * 4 * NS_PER_MS,
            wall_ms: 1_791_000_000_000 + (i * 4) as i64,
            pid,
            tid: pid + (i % 5) as u32,
            uid: Some(if mine { APP_UID } else { 1000 + (i % 3) as u32 }),
            level: Level::ALL[(i % 23 % 6) as usize],
            buffer: if i.is_multiple_of(4) { 3 } else { 0 },
            tag: tags[(i % tags.len() as u64) as usize].into(),
            message: messages[(i % messages.len() as u64) as usize].into(),
        }
    };
    let batches: Vec<Vec<LogLine>> =
        (0..LINES / 2000).map(|b| (b * 2000..(b + 1) * 2000).map(line).collect()).collect();
    let t = Instant::now();
    for b in batches {
        app.ingest(vec![SessionEvent::Logs(b)]);
    }
    let took = t.elapsed();
    let logs = app.store.logs();
    println!(
        "{:<44} {:>8.2?}  ({:.1} M lines/s, {} bytes a line)",
        "Logdawg: 500,000 lines into the store",
        took,
        LINES as f64 / took.as_secs_f64() / 1e6,
        logs.bytes() / logs.len()
    );
    assert_eq!(logs.dropped(), 0, "all of them kept");
    let end = start + LINES * 4 * NS_PER_MS;
    app.now_override = Some(end);
    // the first frame scans every line against package:mine
    let t = Instant::now();
    render_keys(&mut app, 200, 50, &parse_keys("4").unwrap());
    println!("{:<44} {:>8.2?}", "first frame, every line tested (package:mine)", t.elapsed());
    assert!(app.logdawg.len() as u64 >= LINES / 10);
    let mut results = Vec::new();
    let mut i = LINES;
    let mut now = end;
    // 20 new lines a frame: 600 a second at 30 frames
    let mut feed = |app: &mut App, _| {
        let batch: Vec<LogLine> = (i..i + 20).map(line).collect();
        i += 20;
        now += 33 * NS_PER_MS;
        app.ingest(vec![SessionEvent::Logs(batch)]);
        app.now_override = Some(now);
    };
    results.push(time_frames("Logdawg following, 20 new lines every frame", &mut app, 200, &mut feed));
    for (filter, label) in [
        ("", "every line"),
        ("level:w tag:OkHttp", "level:w tag:OkHttp"),
        ("/timeout|refused/i -tag:chatty", "a regex"),
        ("age:5m", "age:5m (tested again each second)"),
    ] {
        app.apply_log_filter(filter);
        let t = Instant::now();
        render_text(&mut app, 200, 50);
        println!(
            "{:<44} {:>8.2?}  ({} lines pass)",
            format!("filter {label}: every line tested"),
            t.elapsed(),
            app.logdawg.len()
        );
        results.push(time_frames("  then 20 new lines every frame", &mut app, 100, &mut feed));
    }
    // the cursor up in the list, a message of several lines on screen
    app.apply_log_filter("package:mine");
    render_keys(&mut app, 200, 50, &parse_keys("<C-u><C-u>kkk").unwrap());
    results.push(time_frames("Logdawg not following, 20 new lines a frame", &mut app, 200, &mut feed));
    results.iter().for_each(Frames::check);
}

/// A zstd frame of `blocks` RLE blocks, each 128 KB of `byte` from 4 bytes.
fn zstd_rle(byte: u8, blocks: usize) -> Vec<u8> {
    // the magic number, a header without a content size, a 128 KB window
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 7 << 3];
    for i in 0..blocks {
        let header = (128u32 << 10) << 3 | 1 << 1 | u32::from(i + 1 == blocks);
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.push(byte);
    }
    frame
}

/// A finished GET whose response is `body`, sent with `headers`; returns its index.
fn add_response(app: &mut App, txn: u64, headers: Vec<(String, String)>, body: Bytes) -> TxnIdx {
    add_exchange(app, txn, 200, headers, body)
}

/// A finished GET answered with `status`, `headers` and `body`; returns its index.
fn add_exchange(app: &mut App, txn: u64, status: u16, headers: Vec<(String, String)>, body: Bytes) -> TxnIdx {
    let source = app.store.current_source().expect("demo source").id;
    let key = TxnKey { source, txn };
    let at = app.store.latest() + NS_PER_MS;
    let len = body.len() as u64;
    app.ingest(vec![
        SessionEvent::Request(Box::new(RequestStarted {
            key,
            at,
            call: None,
            hop: 0,
            method: "GET".into(),
            url: format!("https://api.example.app/big/{txn}"),
            headers: Vec::new(),
            client: None,
            thread: None,
            stack: Vec::new(),
            stack_truncated: false,
            body: None,
            marks: Vec::new(),
            conn: None,
        })),
        SessionEvent::Response(Box::new(ResponseStarted {
            key,
            at: at + NS_PER_MS,
            status,
            message: "OK".into(),
            protocol: None,
            headers,
            conn: None,
        })),
        SessionEvent::Body { key, dir: BodyDir::Response, at: at + 2 * NS_PER_MS, offset: 0, bytes: body },
        SessionEvent::BodyEnd {
            key,
            dir: BodyDir::Response,
            at: at + 3 * NS_PER_MS,
            total: len,
            captured: len,
            state: "complete".into(),
            decoded: false,
        },
        SessionEvent::Completed { key, at: at + 3 * NS_PER_MS },
    ]);
    app.now_override = Some(app.store.latest());
    app.store.find(key).expect("added")
}

/// One frame as the UI task draws it, without running what it queues for a worker.
fn draw_once(app: &mut App) -> Duration {
    let (w, h) = common::LARGE;
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    let t = Instant::now();
    term.draw(|f| traffic_police_tui::ui::draw(f, app)).expect("draw");
    t.elapsed()
}

/// Bodies that are large, or small and decode large (ARCHITECTURE.md §5.9): the frame that
/// opens one (the UI task's part), the build (a worker's), what the view holds, and frames with
/// it open. Up to 0.5.1 a body of up to 256 KB as captured was built on the UI task however
/// large it decoded, and formatted whole: 200 KB of gzip took seconds and gigabytes there.
#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn large_and_compressed_bodies() {
    use traffic_police_backends::demo::content::gzip;
    let _alone = alone();
    let typed = |ct: &str, enc: Option<&str>| {
        let mut h = vec![("Content-Type".to_string(), ct.to_string())];
        if let Some(e) = enc {
            h.push(("Content-Encoding".to_string(), e.to_string()));
        }
        h
    };
    let json = |items: usize| {
        let item = r#"{"id":12345,"name":"Ada Lovelace","tags":["a","b"],"ok":true},"#;
        Bytes::from(format!("[{}{{}}]", item.repeat(items)))
    };
    // what it is, its headers, the body, and whether it is built on a worker
    type Case = (&'static str, Vec<(String, String)>, Bytes, bool);
    let cases: Vec<Case> = vec![
        ("JSON, 10 MB", typed("application/json", None), json(10 << 20 >> 6), true),
        ("gzip text, 200 KB to 200 MB", typed("text/plain", Some("gzip")), gzip(&vec![b'a'; 200 << 20]), true),
        ("zstd text, 8 KB to 250 MB", typed("text/plain", Some("zstd")), Bytes::from(zstd_rle(b'a', 2000)), true),
        ("HTML, 255 KB of <a> never closed", typed("text/html", None), Bytes::from("<a>".repeat(85_000)), false),
        (
            "HTML, 30 MB of <li> never closed",
            typed("text/html", None),
            Bytes::from("<li>item ".repeat(30 << 20 >> 3)),
            true,
        ),
    ];
    let mut app = common::app_at(2.0);
    let mut frames = Vec::new();
    for (n, (label, headers, body, worker)) in cases.into_iter().enumerate() {
        let sent = body.len();
        let txn = add_response(&mut app, 90_000 + n as u64, headers, body);
        render_text(&mut app, 200, 50);
        app.select_txn(txn);
        app.detail_open = true;
        app.focus = Focus::Detail;
        app.detail.tab = Tab::Response;
        let first = draw_once(&mut app);
        assert_eq!(app.body_decoding(txn, BodyDir::Response), worker, "{label}: built on a worker");
        let t = Instant::now();
        app.run_jobs_inline();
        let build = t.elapsed();
        let view = app.body_view(txn, BodyDir::Response).expect("built");
        let (decoded, held, lines, partial) =
            (view.decoded.bytes.len(), view.mem_bytes(), view.len(true), view.partial);
        println!(
            "{label:<44} sent {:>7}, decoded {:>7}: first frame {:>8.2?}, build {:>8.2?}, view {:>7} in {lines} lines{}",
            traffic_police_core::fmt::bytes(sent as u64),
            traffic_police_core::fmt::bytes(decoded as u64),
            first,
            build,
            traffic_police_core::fmt::bytes(held as u64),
            if partial { " (the start)" } else { "" },
        );
        assert!(first < BUDGET, "{label}: the frame that opened it took {first:?}");
        assert!(build < Duration::from_secs(5), "{label}: the build took {build:?}");
        // the decoded bytes (256 MB at most) and what is formatted of them (bounded)
        assert!(held < decoded + (200 << 20), "{label}: the view holds {held} bytes");
        frames.push(time_frames("  then frames with it open", &mut app, 50, |_, _| {}));
    }
    for f in frames {
        f.check();
    }
}

/// A busy WebSocket: 200,000 messages, then a new one every frame, with the Response tab of its
/// handshake open, where the messages are listed (and the list's Size column adds up what it
/// received). Up to 0.5.1 every message's row was made for every frame, and the Size cell added
/// up every message.
#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn a_busy_websocket() {
    use traffic_police_core::normalize::Normalizer;
    use traffic_police_proto::msg::{self, DeviceMsg};
    let _alone = alone();
    const MESSAGES: u64 = 200_000;
    let mut app = common::app_at(2.0);
    let upgrade = vec![("Upgrade".to_string(), "websocket".to_string())];
    let txn = add_exchange(&mut app, 96_000, 101, upgrade, Bytes::new());
    let source = app.store.current_source().expect("demo source").id;
    let mut normalizer = Normalizer::new(source);
    let start = app.store.latest();
    // the messages as the runtime sends them (PROTOCOL.md §7.1 `ws`)
    let mut message = |i: u64| {
        let text = format!(r#"{{"type":"order","id":"o_{i}","state":"delivered","eta":{}}}"#, i % 600);
        let ws = msg::Ws {
            seq: 1 + i,
            ts: start + i * NS_PER_MS,
            txn: 96_000,
            dir: (if i.is_multiple_of(8) { "out" } else { "in" }).into(),
            op: "text".into(),
            size: text.len() as u64,
            text: Some(text),
            base64: None,
            truncated: false,
            code: None,
            reason: None,
        };
        let mut events = Vec::new();
        normalizer.message(DeviceMsg::Ws(ws), &mut events);
        events
    };
    let t = Instant::now();
    for batch in 0..MESSAGES / 5000 {
        app.ingest((batch * 5000..(batch + 1) * 5000).flat_map(&mut message).collect());
    }
    let took = t.elapsed();
    println!("{:<44} {:>8.2?}", "WebSocket: 200,000 messages into the store", took);
    app.now_override = Some(app.store.latest());
    render_text(&mut app, 200, 50);
    app.select_txn(txn);
    app.detail_open = true;
    app.focus = Focus::Detail;
    app.detail.tab = Tab::Response;
    let mut i = MESSAGES;
    let frames = time_frames("  its messages open, a new one every frame", &mut app, 200, |app, _| {
        app.ingest(message(i));
        app.now_override = Some(app.store.latest());
        i += 1;
    });
    assert_eq!(app.view_store().txn(txn).ws.len() as u64, i);
    frames.check();
}
