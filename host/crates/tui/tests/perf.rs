//! Frame time with 50,000 transactions (ARCHITECTURE.md §6: at most 33 ms, target under 8 ms).
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
            thread: Some(ThreadInfo { id: 50 + th as i64, name: threads[th].into(), origin: Some("call".into()) }),
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
