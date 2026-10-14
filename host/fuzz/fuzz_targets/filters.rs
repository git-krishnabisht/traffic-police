//! Whatever is typed into a filter bar: view 1's (the requests, ARCHITECTURE.md §5.10) and
//! Logdawg's (the device's log, §5.17). Parsing never panics, an error points at a place in the
//! text (the commands without the UI underline it), and a filter that parses matches requests
//! and log lines without panicking.

#![no_main]

use std::collections::HashSet;

use libfuzzer_sys::fuzz_target;
use traffic_police_core::filter::{Filter, ParseError};
use traffic_police_core::fmt::NS_PER_SEC;
use traffic_police_core::logdawg::filter::{Context, LogFilter};
use traffic_police_core::logdawg::{BUFFER_CRASH, Level, LogInfo, LogLine, LogStore};
use traffic_police_core::store::SessionStore;

/// Three requests: JSON that came, a POST that failed on the server, one with no answer.
const HAR: &[u8] = br#"{"log":{"version":"1.2","creator":{"name":"fuzz","version":"1"},"entries":[
{"startedDateTime":"2026-10-14T10:00:00.000Z","time":120,"request":{"method":"GET","url":"https://api.example.app/api/v1/products?page=2","httpVersion":"HTTP/1.1","headers":[{"name":"Accept","value":"application/json"}],"queryString":[{"name":"page","value":"2"}],"cookies":[],"headersSize":-1,"bodySize":0},"response":{"status":200,"statusText":"OK","httpVersion":"HTTP/1.1","headers":[{"name":"Content-Type","value":"application/json; charset=utf-8"}],"cookies":[],"content":{"size":27,"mimeType":"application/json","text":"{\"products\":[{\"id\":\"p_1\"}]}"},"redirectURL":"","headersSize":-1,"bodySize":27},"cache":{},"timings":{"send":1,"wait":100,"receive":19}},
{"startedDateTime":"2026-10-14T10:00:01.000Z","time":800,"request":{"method":"POST","url":"https://api.example.app/api/v1/checkout","httpVersion":"HTTP/2","headers":[{"name":"X-Request-Id","value":"abc-123"},{"name":"Content-Type","value":"application/json"}],"queryString":[],"cookies":[],"headersSize":-1,"bodySize":15,"postData":{"mimeType":"application/json","text":"{\"cart\":\"c_9\"}"}},"response":{"status":500,"statusText":"Internal Server Error","httpVersion":"HTTP/2","headers":[{"name":"Content-Type","value":"text/plain"}],"cookies":[],"content":{"size":5,"mimeType":"text/plain","text":"oops!"},"redirectURL":"","headersSize":-1,"bodySize":5},"cache":{},"timings":{"send":2,"wait":790,"receive":8}},
{"startedDateTime":"2026-10-14T10:00:02.000Z","time":10000,"request":{"method":"GET","url":"http://cdn.example.app/model.bin","httpVersion":"HTTP/1.1","headers":[],"queryString":[],"cookies":[],"headersSize":-1,"bodySize":0},"response":{"status":0,"statusText":"","httpVersion":"","headers":[],"cookies":[],"content":{"size":0,"mimeType":""},"redirectURL":"","headersSize":-1,"bodySize":-1,"_error":"timeout"},"cache":{},"timings":{"send":0,"wait":10000,"receive":0}}
]}}"#;

thread_local! {
    static REQUESTS: SessionStore = requests();
    static LINES: LogStore = lines();
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    match Filter::parse(&text) {
        Ok(Some(f)) => REQUESTS.with(|store| {
            let now = store.latest();
            for t in store.txns() {
                let _ = f.matches(t, now, &mut |i| Some(i % 2 == 0));
                let _ = f.matches(t, now, &mut |_| None);
            }
            let _ = (f.uses_clock(), f.needles());
        }),
        Ok(None) => {}
        Err(e) => at_a_place(&text, &e),
    }
    match LogFilter::parse(&text) {
        Ok(Some(f)) => LINES.with(|store| {
            let pids = HashSet::from([4312]);
            for now in [0, 6 * NS_PER_SEC, u64::MAX] {
                let cx = Context { app_pids: &pids, now };
                for line in store.iter_from(0) {
                    let _ = f.matches(&line, store, &cx);
                }
            }
            let _ = f.uses_clock();
        }),
        Ok(None) => {}
        Err(e) => at_a_place(&text, &e),
    }
});

fn at_a_place(text: &str, e: &ParseError) {
    assert!(text.get(e.span.clone()).is_some(), "{text:?}: the error is at {:?}", e.span);
}

fn requests() -> SessionStore {
    let mut store = SessionStore::new();
    let opened = traffic_police_core::import::har(HAR, &store.source_ids()).expect("the requests import");
    for e in opened.events {
        store.apply(e);
    }
    assert_eq!(store.len(), 3, "the requests import");
    store
}

/// The app's lines and the system's, a crash among them, with and without a known uid.
fn lines() -> LogStore {
    let mut s = LogStore::new();
    s.apply_info(LogInfo {
        package: Some("com.example.shop".into()),
        uid: Some(10_234),
        processes: vec![(4312, "com.example.shop".into()), (612, "system_server".into())],
        ..LogInfo::default()
    });
    let line = |ts: u64, pid: u32, uid: Option<u32>, level: Level, buffer: u8, tag: &str, message: &str| LogLine {
        ts,
        wall_ms: 1_790_000_000_000 + (ts / 1_000_000) as i64,
        pid,
        tid: pid + 1,
        uid,
        level,
        buffer,
        tag: tag.into(),
        message: message.into(),
    };
    s.push(line(
        NS_PER_SEC,
        4312,
        Some(10_234),
        Level::Debug,
        0,
        "OkHttp",
        "--> GET https://api.example.app/api/v1/products",
    ));
    s.push(line(
        2 * NS_PER_SEC,
        612,
        Some(1000),
        Level::Info,
        3,
        "ActivityManager",
        "Start proc 4498:com.example.shop/u0a234",
    ));
    s.push(line(
        3 * NS_PER_SEC,
        4498,
        Some(10_234),
        Level::Error,
        BUFFER_CRASH,
        "AndroidRuntime",
        "FATAL EXCEPTION: main\njava.lang.IllegalStateException: no cart\n\tat com.example.shop.Cart.total(Cart.kt:42)",
    ));
    s.push(line(4 * NS_PER_SEC, 77, None, Level::Warn, 0, "", "ünïcödé\u{0}and a zero"));
    s
}
