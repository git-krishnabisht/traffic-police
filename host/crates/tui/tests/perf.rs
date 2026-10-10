//! Frame time with 50,000 transactions (ARCHITECTURE.md §6: at most 33 ms, target under 8 ms),
//! and with half a million lines of the device's log in Logdawg.
//!
//! Ignored by default because it is only meaningful in release builds:
//! `cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture`

mod common;

use std::time::{Duration, Instant};

use bytes::Bytes;
use traffic_police_backends::demo::DemoSession;
use traffic_police_core::SessionEvent;
use traffic_police_core::event::{RequestStarted, ResponseStarted};
use traffic_police_core::fmt::{NS_PER_MS, NS_PER_SEC};
use traffic_police_core::model::TxnKey;
use traffic_police_proto::BodyDir;
use traffic_police_proto::msg::{ClientInfo, StackFrame, ThreadInfo};
use traffic_police_tui::{App, parse_keys, render_keys, render_text};

const N: u64 = 50_000;

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

fn time_frames(label: &str, app: &mut App, frames: u32, mut between: impl FnMut(&mut App, u32)) -> Duration {
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
    let avg = start.elapsed() / frames;
    println!("{label:<44} avg {:>8.2?}  worst {:>8.2?}", avg, worst);
    avg
}

#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn frame_time_with_50k_transactions() {
    let (mut app, _session, end) = big_app();
    assert!(app.store.len() as u64 >= N);
    let budget = Duration::from_millis(33);
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
    for avg in results {
        assert!(avg < budget, "a frame took {avg:?} on average, over the {budget:?} budget");
    }
}

/// Logdawg (ARCHITECTURE.md §5.17): half a million lines, what a busy device writes in about
/// forty minutes, one in ten of them the app's; then a new batch every frame while following.
#[test]
#[ignore = "run in release: cargo test --release -p traffic-police-tui --test perf -- --ignored --nocapture"]
fn logdawg_with_500k_lines() {
    use traffic_police_core::logdawg::{Level, LogLine};
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
    let budget = Duration::from_millis(33);
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
    for avg in results {
        assert!(avg < budget, "a frame took {avg:?} on average, over the {budget:?} budget");
    }
}
