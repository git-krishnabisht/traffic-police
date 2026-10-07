//! End to end on a real device: the sample app runs every scenario while the device backend
//! watches both of its processes, then the app is killed and relaunched (`--follow`). Every
//! request is checked for what the capture promises: method, URL, status, headers, bodies,
//! timings, thread, call stack, and failures.
//!
//! The attach-mode test does the same with the sample's plain build, which has no
//! traffic-police code: the agent is attached to it while it starts and again while it runs (its
//! OkHttp client was built before), follows a restart, and is loaded at start with `--launch`.
//!
//! Ignored by default. They force-stop and start `io.trafficpolice.sample` (and `.plain`) on the
//! device named by `TP_E2E_SERIAL` (install the sample's debug and plain builds first; the attach
//! test also needs the agent, built by `./gradlew :attach-agent:agentArtifacts`, or in
//! `TP_E2E_AGENT_DIR`):
//!
//! ```text
//! TP_E2E_SERIAL=emulator-5554 cargo test -p traffic-police-backends --test device_e2e -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use traffic_police_adb::{Adb, TransportId};
use traffic_police_backends::{AgentKit, DeviceTarget, Launch, run_device};
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::ConnectionStatus;
use traffic_police_core::decode::decode_body;
use traffic_police_core::model::{BodyMeta, BodyState, SourceId, Transaction, TxnState, header};
use traffic_police_core::store::SessionStore;

const PACKAGE: &str = "io.trafficpolice.sample";
const WORKER: &str = "io.trafficpolice.sample:worker";
const LAUNCH: &str = "am start -n io.trafficpolice.sample/.MainActivity --es run all";

/// Collects problems instead of stopping at the first, so one run reports everything.
#[derive(Default)]
struct Report {
    problems: Vec<String>,
}

impl Report {
    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        if !ok {
            self.problems.push(what());
        }
    }
}

struct Session {
    store: SessionStore,
    events: mpsc::Receiver<Vec<SessionEvent>>,
}

impl Session {
    /// Applies events until `done` holds, or fails after `limit`.
    async fn until(&mut self, what: &str, limit: Duration, done: impl Fn(&SessionStore) -> bool) {
        let deadline = Instant::now() + limit;
        while !done(&self.store) {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some(batch)) => {
                    for e in batch {
                        self.store.apply(e);
                    }
                }
                Ok(None) => {
                    summary(&self.store);
                    panic!("the backends stopped while waiting for {what}");
                }
                Err(_) => {
                    summary(&self.store);
                    panic!("timed out after {limit:?} waiting for {what}");
                }
            }
        }
    }
}

fn source_of(store: &SessionStore, process: &str, nth: usize) -> Option<SourceId> {
    let mut ids: Vec<(u64, SourceId)> =
        store.sources().filter(|s| s.process == process).map(|s| (s.started, s.id)).collect();
    ids.sort();
    ids.get(nth).map(|&(_, id)| id)
}

fn txns(store: &SessionStore, source: SourceId) -> Vec<&Transaction> {
    store.txns().iter().map(|t| t.as_ref()).filter(|t| t.key.source == source).collect()
}

fn has_done(store: &SessionStore, source: Option<SourceId>) -> bool {
    source.is_some_and(|s| txns(store, s).iter().any(|t| t.url.path == "/done" && t.state == TxnState::Complete))
}

fn body(store: &SessionStore, t: &Transaction, meta: &BodyMeta, request: bool) -> Vec<u8> {
    let raw = store.body_bytes(meta);
    let headers = if request { Some(&t.req_headers) } else { t.resp.as_ref().map(|r| &r.headers) };
    decode_body(raw, headers, 64 << 20).bytes.to_vec()
}

fn text(store: &SessionStore, t: &Transaction, request: bool) -> String {
    let meta = if request { &t.req_body } else { &t.resp_body };
    String::from_utf8_lossy(&body(store, t, meta, request)).into_owned()
}

fn status(t: &Transaction) -> u16 {
    t.resp.as_ref().map_or(0, |r| r.status)
}

fn resp_header<'a>(t: &'a Transaction, name: &str) -> Option<&'a str> {
    t.resp.as_ref().and_then(|r| header(&r.headers, name))
}

fn app_frame(t: &Transaction, class_prefix: &str) -> bool {
    t.stack.iter().any(|f| f.c.starts_with(class_prefix) && f.l.is_some_and(|l| l > 0) && f.f.is_some())
}

/// Checks one run of `Scenarios.runAll()` in the main process.
fn check_main_run(r: &mut Report, store: &SessionStore, source: SourceId, run: &str) {
    let all = txns(store, source);
    let by_path = |p: &str| -> Vec<&Transaction> { all.iter().copied().filter(|t| t.url.path == p).collect() };
    let one = |r: &mut Report, p: &str| -> Option<&Transaction> {
        let v = by_path(p);
        r.check(v.len() == 1, || format!("{run}: expected one {p}, found {}", v.len()));
        v.first().copied()
    };

    // everything finished, nothing lost
    for t in &all {
        r.check(!t.state.is_open(), || format!("{run}: {} {} still {:?}", t.method, t.url.raw, t.state));
        r.check(!t.placeholder && !t.lossy, || format!("{run}: {} is incomplete (placeholder or lossy)", t.url.raw));
        r.check(t.client.is_some(), || format!("{run}: {} has no client", t.url.raw));
        r.check(t.thread.is_some(), || format!("{run}: {} has no thread", t.url.raw));
        r.check(!t.stack.is_empty(), || format!("{run}: {} has no call stack", t.url.raw));
        if let Some(end) = t.end {
            r.check(t.start <= t.req_at && t.req_at <= end, || {
                format!("{run}: {} has times out of order ({} {} {})", t.url.raw, t.start, t.req_at, end)
            });
        }
    }
    r.check(all.len() == 26, || format!("{run}: expected 26 requests, found {}", all.len()));

    // Retrofit suspend functions on a coroutine worker; the stack reaches the scenario
    if let Some(t) = one(r, "/api/sdk/init") {
        r.check(t.method == "POST" && status(t) == 200, || format!("{run}: init {} {}", t.method, status(t)));
        r.check(text(store, t, true) == r#"{"sdkVersion":"2.4.1","platform":"android"}"#, || {
            format!("{run}: init request body {:?}", text(store, t, true))
        });
        r.check(header(&t.req_headers, "content-type") == Some("application/json; charset=utf-8"), || {
            format!("{run}: init request headers {:?}", t.req_headers)
        });
        r.check(text(store, t, false).contains(r#""sessionId":"session_1""#), || {
            format!("{run}: init response body {:?}", text(store, t, false))
        });
        r.check(resp_header(t, "x-request-id").is_some(), || format!("{run}: init response headers lack X-Request-Id"));
        let thread = t.thread.as_ref();
        r.check(thread.is_some_and(|th| th.name.starts_with("DefaultDispatcher-worker")), || {
            format!("{run}: init thread {thread:?}")
        });
        r.check(thread.is_some_and(|th| th.origin.as_deref() == Some("call")), || {
            format!("{run}: init thread origin {thread:?}")
        });
        r.check(app_frame(t, "io.trafficpolice.sample.Scenarios"), || {
            format!("{run}: init stack lacks the scenario frame")
        });
        let names: Vec<&str> = t.marks.iter().map(|(n, _)| n.as_str()).collect();
        for m in [
            "call_start",
            "dns_start",
            "dns_end",
            "connect_start",
            "connect_end",
            "req_headers_start",
            "resp_headers_end",
        ] {
            r.check(names.contains(&m), || format!("{run}: init lacks timing mark {m} (has {names:?})"));
        }
        let conn = t.conn.as_ref();
        r.check(
            conn.and_then(|c| c.remote.as_ref()).is_some_and(|a| a.ip == "127.0.0.1" && Some(a.port) == t.url.port),
            || format!("{run}: init remote address {conn:?}"),
        );
        r.check(conn.and_then(|c| c.protocol.as_deref()) == Some("http/1.1"), || {
            format!("{run}: init protocol {conn:?}")
        });
    }
    if let Some(t) = one(r, "/api/sdk/challenge") {
        r.check(t.method == "GET" && status(t) == 200, || format!("{run}: challenge {} {}", t.method, status(t)));
        r.check(t.url.query.as_deref() == Some("session=session_1"), || {
            format!("{run}: challenge query {:?}", t.url.query)
        });
    }
    if let Some(t) = one(r, "/api/sdk/attest") {
        r.check(text(store, t, true).contains(r#""integrity":"ok""#), || format!("{run}: attest request body"));
    }
    if let Some(t) = one(r, "/api/sdk/enroll") {
        r.check(text(store, t, false).contains(r#""enrolled":true"#), || format!("{run}: enroll response body"));
    }
    let polls = by_path("/api/sdk/status");
    r.check(polls.len() == 3, || format!("{run}: expected 3 status polls, found {}", polls.len()));
    for (i, t) in polls.iter().enumerate() {
        let want = format!(r#""poll":{}"#, i + 1);
        r.check(text(store, t, false).contains(&want), || {
            format!("{run}: status poll {} body {:?}", i + 1, text(store, t, false))
        });
    }

    // gzip on the wire: the capture holds the encoded bytes, the host decodes them
    if let Some(t) = one(r, "/api/profile") {
        r.check(resp_header(t, "content-encoding") == Some("gzip"), || {
            format!("{run}: profile lacks Content-Encoding")
        });
        let raw = store.body_bytes(&t.resp_body);
        r.check(raw.starts_with(&[0x1f, 0x8b]), || format!("{run}: profile body is not the gzip wire bytes"));
        r.check(text(store, t, false).contains("Asha Verma"), || format!("{run}: profile body does not decode"));
    }

    // enqueue from the main thread: the call site is the caller's thread
    if let Some(t) = by_path("/api/feed").first() {
        let thread = t.thread.as_ref();
        r.check(thread.is_some_and(|th| th.name == "main" && th.origin.as_deref() == Some("call")), || {
            format!("{run}: feed thread {thread:?}")
        });
        let cookies = t
            .resp
            .as_ref()
            .map_or(0, |r| r.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("set-cookie")).count());
        r.check(cookies == 2, || format!("{run}: feed has {cookies} Set-Cookie headers, want 2"));
        r.check(text(store, t, false).starts_with(r#"{"page":2"#), || {
            format!("{run}: feed body {:?}", text(store, t, false))
        });
    } else {
        r.check(false, || format!("{run}: no /api/feed"));
    }

    // HttpURLConnection
    if let Some(t) = one(r, "/huc/config") {
        r.check(t.client.as_ref().is_some_and(|c| c.kind == "huc"), || {
            format!("{run}: huc config client {:?}", t.client)
        });
        r.check(text(store, t, false).contains(r#""version":42"#), || format!("{run}: huc config body"));
        r.check(t.thread.as_ref().is_some_and(|th| th.origin.as_deref() == Some("huc")), || {
            format!("{run}: huc thread {:?}", t.thread)
        });
        r.check(app_frame(t, "io.trafficpolice.sample.Scenarios"), || {
            format!("{run}: huc config stack lacks the scenario frame")
        });
    }
    if let Some(t) = one(r, "/huc/submit") {
        r.check(t.method == "POST" && status(t) == 201, || format!("{run}: huc submit {} {}", t.method, status(t)));
        r.check(text(store, t, true) == r#"{"answer":42}"#, || {
            format!("{run}: huc submit request body {:?}", text(store, t, true))
        });
        r.check(header(&t.req_headers, "content-type") == Some("application/json"), || {
            format!("{run}: huc submit request headers {:?}", t.req_headers)
        });
        r.check(text(store, t, false) == r#"{"created":true,"bytes":13}"#, || {
            format!("{run}: huc submit response {:?}", text(store, t, false))
        });
    }

    // streaming and large bodies
    if let Some(t) = one(r, "/stream") {
        r.check(t.resp_body.total == 10_292 && t.resp_body.state == BodyState::Complete, || {
            format!("{run}: stream body {:?}", t.resp_body)
        });
    }
    if let Some(t) = one(r, "/download/model.bin") {
        r.check(
            t.resp_body.total == 5 << 20 && t.resp_body.captured == 5 << 20 && t.resp_body.state == BodyState::Complete,
            || format!("{run}: download body {:?}", t.resp_body),
        );
    }
    if let Some(t) = one(r, "/upload") {
        r.check(header(&t.req_headers, "content-type").is_some_and(|c| c.starts_with("multipart/form-data")), || {
            format!("{run}: upload content type {:?}", header(&t.req_headers, "content-type"))
        });
        r.check(t.req_body.total > 200 * 1024 && t.req_body.state == BodyState::Complete, || {
            format!("{run}: upload body {:?}", t.req_body)
        });
        let received = format!(r#"{{"received":{}}}"#, t.req_body.total);
        r.check(text(store, t, false) == received, || {
            format!("{run}: upload response {:?}, want {received}", text(store, t, false))
        });
    }

    // a redirect is two hops of one call
    let (old, new) = (by_path("/old"), by_path("/new"));
    if let (Some(o), Some(n)) = (old.first(), new.first()) {
        r.check(status(o) == 302 && resp_header(o, "location") == Some("/new"), || {
            format!("{run}: /old {} {:?}", status(o), resp_header(o, "location"))
        });
        r.check(status(n) == 200 && n.hop == 1 && n.call.is_some() && n.call == o.call, || {
            format!("{run}: /new status {} hop {} call {:?} vs {:?}", status(n), n.hop, n.call, o.call)
        });
    } else {
        r.check(false, || format!("{run}: redirect hops missing ({} /old, {} /new)", old.len(), new.len()));
    }
    if let Some(t) = one(r, "/missing") {
        r.check(status(t) == 404, || format!("{run}: /missing {}", status(t)));
    }
    if let Some(t) = one(r, "/error") {
        r.check(status(t) == 500 && resp_header(t, "content-type").is_some_and(|c| c.starts_with("text/html")), || {
            format!("{run}: /error")
        });
    }

    // failures: a read timeout, a cancel, an unknown host
    let slow = by_path("/slow");
    r.check(slow.len() == 2, || format!("{run}: expected 2 /slow, found {}", slow.len()));
    let timed_out = slow.iter().any(|t| {
        t.state == TxnState::Failed
            && t.failure.as_ref().is_some_and(|f| !f.canceled && f.class.ends_with("SocketTimeoutException"))
    });
    r.check(timed_out, || {
        format!(
            "{run}: no /slow failed with SocketTimeoutException: {:?}",
            slow.iter().map(|t| &t.failure).collect::<Vec<_>>()
        )
    });
    let canceled = slow.iter().any(|t| t.state == TxnState::Failed && t.failure.as_ref().is_some_and(|f| f.canceled));
    r.check(canceled, || {
        format!("{run}: no canceled /slow: {:?}", slow.iter().map(|t| &t.failure).collect::<Vec<_>>())
    });
    let unknown: Vec<&&Transaction> = all.iter().filter(|t| t.url.host == "api.nonexistent.invalid").collect();
    r.check(
        unknown.len() == 1 && unknown[0].failure.as_ref().is_some_and(|f| f.class.ends_with("UnknownHostException")),
        || format!("{run}: unknown host {:?}", unknown.iter().map(|t| &t.failure).collect::<Vec<_>>()),
    );

    // binary bodies
    if let Some(t) = one(r, "/avatar.png") {
        let raw = store.body_bytes(&t.resp_body);
        r.check(raw.starts_with(b"\x89PNG") && resp_header(t, "content-type") == Some("image/png"), || {
            format!("{run}: avatar is not a PNG")
        });
    }
    if let Some(t) = one(r, "/metrics") {
        let raw = store.body_bytes(&t.req_body);
        r.check(raw.starts_with(b"\x0a\x05dev-1"), || {
            format!("{run}: metrics request body {:?}", &raw[..raw.len().min(16)])
        });
        // the app closes the response without reading it
        r.check(t.resp_body.state == BodyState::ClosedEarly && t.resp_body.total == 0, || {
            format!("{run}: metrics response body {:?}", t.resp_body)
        });
    }

    // HTTPS through both clients
    let secure = by_path("/secure");
    r.check(secure.len() == 2, || format!("{run}: expected 2 /secure, found {}", secure.len()));
    for t in &secure {
        let tls = t.conn.as_ref().and_then(|c| c.tls.as_ref());
        r.check(t.url.scheme == "https" && status(t) == 200, || {
            format!("{run}: /secure {} {}", t.url.scheme, status(t))
        });
        r.check(tls.is_some_and(|x| x.cipher.is_some()), || {
            format!("{run}: /secure via {:?} lacks TLS details: {tls:?}", t.client)
        });
    }
    r.check(secure.iter().any(|t| t.client.as_ref().is_some_and(|c| c.kind == "huc")), || {
        format!("{run}: no HttpsURLConnection /secure")
    });

    if let Some(t) = one(r, "/done") {
        r.check(status(t) == 204 && t.resp_body.state == BodyState::None, || {
            format!("{run}: /done {} {:?}", status(t), t.resp_body.state)
        });
    }
}

fn check_worker(r: &mut Report, store: &SessionStore, source: SourceId, run: &str) {
    let all = txns(store, source);
    let pings: Vec<&&Transaction> = all.iter().filter(|t| t.url.path == "/worker/ping").collect();
    r.check(!pings.is_empty(), || format!("{run}: the worker's /worker/ping was not captured"));
    for t in pings {
        r.check(status(t) == 200, || format!("{run}: worker ping status {}", status(t)));
        r.check(t.thread.as_ref().is_some_and(|th| th.name == "worker-sync"), || {
            format!("{run}: worker ping thread {:?}", t.thread)
        });
    }
}

fn summary(store: &SessionStore) {
    for s in store.sources() {
        let n = txns(store, s.id).len();
        println!("source {} {} pid {} api {:?}: {n} requests, ended {:?}", s.id, s.process, s.pid, s.api, s.ended);
    }
    for t in store.txns() {
        println!(
            "  [{}] {:<6} {:<48} {:>3} {:>9} B  {:<24} {}",
            t.key.source,
            t.method,
            t.url.name(),
            status(t),
            t.resp_body.total,
            t.thread.as_ref().map_or("", |th| th.name.as_str()),
            t.failure.as_ref().map_or(String::new(), |f| f.short_class().to_string()),
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "drives the sample app on a device: set TP_E2E_SERIAL"]
async fn sample_app_end_to_end() {
    let serial = std::env::var("TP_E2E_SERIAL").expect("set TP_E2E_SERIAL to the device's serial");
    let adb = Adb::from_env();
    let device = adb
        .devices()
        .await
        .expect("adb server")
        .into_iter()
        .find(|d| d.serial == serial && d.is_online())
        .unwrap_or_else(|| panic!("{serial} is not online"));
    let id = device.transport_id;
    let installed = adb.shell(id, &format!("pm path {PACKAGE}")).await.unwrap();
    assert!(installed.stdout_text().contains("base.apk"), "install the sample's debug build on {serial} first");
    adb.shell(id, &format!("am force-stop {PACKAGE}")).await.unwrap();

    // two backends, main process and worker, both following restarts
    let store = SessionStore::new();
    let (event_tx, events) = mpsc::channel(1024);
    let mut quit = Vec::new();
    let mut tasks = Vec::new();
    for process in [PACKAGE, WORKER] {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (status_tx, _status_rx) = watch::channel(ConnectionStatus::Waiting(String::new()));
        let target = DeviceTarget {
            serial: Some(serial.clone()),
            package: PACKAGE.into(),
            process: Some(process.into()),
            pid: None,
            follow: true,
            capture: Default::default(),
            rules: Default::default(),
            attach: None,
            launch: None,
        };
        tasks.push(tokio::spawn(run_device(
            adb.clone(),
            target,
            store.source_ids(),
            event_tx.clone(),
            cmd_rx,
            status_tx,
            None,
        )));
        quit.push(cmd_tx);
    }
    drop(event_tx);
    let mut session = Session { store, events };
    let mut report = Report::default();

    // run 1
    let started = adb.shell(id, LAUNCH).await.unwrap();
    assert_eq!(started.exit, 0, "am start failed: {}", started.stdout_text());
    session.until("the first run to finish", Duration::from_secs(120), |s| has_done(s, source_of(s, PACKAGE, 0))).await;
    session
        .until("the worker's first request", Duration::from_secs(30), |s| {
            source_of(s, WORKER, 0)
                .is_some_and(|w| txns(s, w).iter().any(|t| t.url.path == "/worker/ping" && !t.state.is_open()))
        })
        .await;
    let (main1, worker1) =
        (source_of(&session.store, PACKAGE, 0).unwrap(), source_of(&session.store, WORKER, 0).unwrap());
    check_main_run(&mut report, &session.store, main1, "run 1");
    check_worker(&mut report, &session.store, worker1, "run 1 worker");

    // kill: both sources end, their data stays
    adb.shell(id, &format!("am force-stop {PACKAGE}")).await.unwrap();
    session
        .until("both processes to end", Duration::from_secs(30), |s| {
            [main1, worker1].iter().all(|&src| s.source(src).is_some_and(|x| x.ended.is_some()))
        })
        .await;
    let reason = session.store.source(main1).and_then(|s| s.ended.clone()).map(|(_, r)| r);
    report.check(reason.as_deref() == Some("the app exited"), || format!("run 1 ended with {reason:?}"));

    // relaunch: --follow attaches to the new processes as new segments
    let started = adb.shell(id, LAUNCH).await.unwrap();
    assert_eq!(started.exit, 0);
    session
        .until("the second run to finish", Duration::from_secs(120), |s| has_done(s, source_of(s, PACKAGE, 1)))
        .await;
    session
        .until("the worker's second request", Duration::from_secs(30), |s| {
            source_of(s, WORKER, 1)
                .is_some_and(|w| txns(s, w).iter().any(|t| t.url.path == "/worker/ping" && !t.state.is_open()))
        })
        .await;
    let (main2, worker2) =
        (source_of(&session.store, PACKAGE, 1).unwrap(), source_of(&session.store, WORKER, 1).unwrap());
    check_main_run(&mut report, &session.store, main2, "run 2");
    check_worker(&mut report, &session.store, worker2, "run 2 worker");
    let (p1, p2) = (session.store.source(main1).unwrap().pid, session.store.source(main2).unwrap().pid);
    report.check(p1 != p2, || format!("the relaunched app has the same pid {p1}"));
    // run 1 is still there, untouched
    check_main_run(&mut report, &session.store, main1, "run 1 after the relaunch");

    // another uid (here: the app itself, as another app would) is refused without a byte
    adb.shell(id, "logcat -c").await.unwrap();
    adb.shell(id, "am start -n io.trafficpolice.sample/.MainActivity --es run security").await.unwrap();
    let mut verdict = String::new();
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let log = adb.shell(id, "logcat -d -s TrafficPoliceSample").await.unwrap().stdout_text();
        if let Some(line) = log.lines().find(|l| l.contains("security:") || l.contains("SECURITY FAILURE")) {
            verdict = line.to_string();
            break;
        }
    }
    report.check(verdict.contains("closed without a byte"), || format!("same-uid connection: {verdict:?}"));

    // quit: each backend says goodbye and removes its forward
    drop(quit);
    for t in tasks {
        tokio::time::timeout(Duration::from_secs(5), t).await.expect("backend did not stop").unwrap();
    }
    let ours: Vec<String> = session.store.sources().map(|s| format!("_{}", s.pid)).collect();
    let left: Vec<_> = adb
        .list_forwards()
        .await
        .unwrap()
        .into_iter()
        .filter(|(s, _, remote)| {
            *s == serial && remote.contains("traffic-police_") && ours.iter().any(|p| remote.ends_with(p))
        })
        .collect();
    report.check(left.is_empty(), || format!("forwards left behind: {left:?}"));

    summary(&session.store);
    assert!(report.problems.is_empty(), "{} problems:\n  {}", report.problems.len(), report.problems.join("\n  "));
    // leave the app stopped
    let _ = adb.shell(id, &format!("am force-stop {PACKAGE}")).await;
}

const PLAIN: &str = "io.trafficpolice.sample.plain";
const PLAIN_WORKER: &str = "io.trafficpolice.sample.plain:worker";
const PLAIN_ACTIVITY: &str = "io.trafficpolice.sample.plain/io.trafficpolice.sample.MainActivity";

/// The agent: `TP_E2E_AGENT_DIR`, else the build output in this source tree.
fn agent_kit() -> Arc<AgentKit> {
    let dir = std::env::var_os("TP_E2E_AGENT_DIR").map(PathBuf::from).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../android/attach-agent/build/outputs/agent")
    });
    Arc::new(AgentKit::from_dir(&dir).unwrap_or_else(|e| {
        panic!("{e:#}: build the agent (cd android && ./gradlew :attach-agent:agentArtifacts) or set TP_E2E_AGENT_DIR")
    }))
}

/// Our file in the plain app's `code_cache/startup_agents`, if it is there.
async fn startup_agent(adb: &Adb, id: TransportId) -> bool {
    let out = adb.shell(id, &format!("run-as {PLAIN} ls code_cache/startup_agents")).await.unwrap();
    out.stdout_text().contains("libtrafficpolice_agent.so")
}

/// Forwards to our sockets that `serial` still has.
async fn our_forwards(adb: &Adb, serial: &str) -> Vec<(String, String, String)> {
    adb.list_forwards()
        .await
        .unwrap()
        .into_iter()
        .filter(|(s, _, remote)| s == serial && remote.contains("traffic-police_"))
        .collect()
}

fn attach_target(
    serial: &str,
    process: &str,
    kit: &Arc<AgentKit>,
    follow: bool,
    launch: Option<Launch>,
) -> DeviceTarget {
    DeviceTarget {
        serial: Some(serial.to_string()),
        package: PLAIN.into(),
        process: Some(process.into()),
        pid: None,
        follow,
        capture: Default::default(),
        rules: Default::default(),
        attach: Some(kit.clone()),
        launch,
    }
}

/// Starts the plain app afresh and waits for its process, and with `ready`, until it has logged
/// that line (`backend on`: its activity was created, and with it its OkHttp client).
async fn start_plain(adb: &Adb, id: TransportId, ready: Option<&str>) {
    adb.shell(id, &format!("am force-stop {PLAIN}")).await.unwrap();
    let started = adb.shell(id, &format!("am start -n {PLAIN_ACTIVITY}")).await.unwrap();
    assert_eq!(started.exit, 0, "am start failed: {}", started.stdout_text());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let pid = adb.shell(id, &format!("pidof {PLAIN}")).await.unwrap().stdout_text().trim().to_string();
        if !pid.is_empty() {
            let Some(ready) = ready else { return };
            let log = adb.shell(id, &format!("logcat -d --pid={pid} -s TrafficPoliceSample:I")).await.unwrap();
            if log.stdout_text().contains(ready) {
                return;
            }
        }
        assert!(Instant::now() < deadline, "the plain app did not start");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "attaches to the sample's plain build on a device: set TP_E2E_SERIAL"]
async fn plain_app_attach_end_to_end() {
    let serial = std::env::var("TP_E2E_SERIAL").expect("set TP_E2E_SERIAL to the device's serial");
    let kit = agent_kit();
    let adb = Adb::from_env();
    let device = adb
        .devices()
        .await
        .expect("adb server")
        .into_iter()
        .find(|d| d.serial == serial && d.is_online())
        .unwrap_or_else(|| panic!("{serial} is not online"));
    let id = device.transport_id;
    let installed = adb.shell(id, &format!("pm path {PLAIN}")).await.unwrap();
    assert!(installed.stdout_text().contains("base.apk"), "install the sample's plain build on {serial} first");
    let api: u32 = adb.shell(id, "getprop ro.build.version.sdk").await.unwrap().stdout_text().trim().parse().unwrap();
    // from Android 11 a startup agent loads the agent as a process starts; before, a runtime
    // attach comes a moment after, and a process's first requests may be missed
    let from_start = api >= 30;
    // --launch loads it at the start from Android 8.1 on: `am start --attach-agent` on 27 to 29
    // (first tried on a device, API 28, 2026-10-07), a startup agent from 30
    let launch_from_start = api >= 27;

    let store = SessionStore::new();
    let (event_tx, events) = mpsc::channel(1024);
    let mut session = Session { store, events };
    let mut report = Report::default();

    // run 0: the agent arrives as soon as the app's process exists, so the app loads its OkHttp
    // client class while the agent's runtime is still starting. Up to 0.3.1 a class defined then
    // stayed unhooked for a moment, and a slow device lost the first requests (the nightly API 36
    // failures of 2026-10-02 to 10-06: init, challenge, attest, enroll and a status poll).
    start_plain(&adb, id, None).await;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (status_tx, _status_rx) = watch::channel(ConnectionStatus::Waiting(String::new()));
    let task = tokio::spawn(run_device(
        adb.clone(),
        attach_target(&serial, PLAIN, &kit, false, None),
        session.store.source_ids(),
        event_tx.clone(),
        cmd_rx,
        status_tx,
        None,
    ));
    session.until("the agent in the starting app", Duration::from_secs(30), |s| source_of(s, PLAIN, 0).is_some()).await;
    let early = source_of(&session.store, PLAIN, 0).unwrap();
    let hooks: Vec<String> =
        session.store.source(early).unwrap().hooks.iter().map(|h| format!("{} {}", h.id, h.status)).collect();
    println!("attach run 0: hooks at hello: {}", hooks.join(", "));
    let run = adb.shell(id, &format!("am start -n {PLAIN_ACTIVITY} --es run all")).await.unwrap();
    assert_eq!(run.exit, 0);
    session.until("attach run 0 to finish", Duration::from_secs(120), |s| has_done(s, Some(early))).await;
    check_main_run(&mut report, &session.store, early, "attach run 0 (attached while the app started)");
    drop(cmd_tx);
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("backend did not stop").unwrap();

    // run 1: the app runs before anything is attached, so its OkHttp client exists before the
    // agent (the app logs once its activity is created, which builds the client)
    start_plain(&adb, id, Some("backend on")).await;
    let mut quit = Vec::new();
    let mut tasks = Vec::new();
    for process in [PLAIN, PLAIN_WORKER] {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (status_tx, _status_rx) = watch::channel(ConnectionStatus::Waiting(String::new()));
        let target = attach_target(&serial, process, &kit, true, None);
        tasks.push(tokio::spawn(run_device(
            adb.clone(),
            target,
            session.store.source_ids(),
            event_tx.clone(),
            cmd_rx,
            status_tx,
            None,
        )));
        quit.push(cmd_tx);
    }
    session.until("the agent in the running app", Duration::from_secs(30), |s| source_of(s, PLAIN, 1).is_some()).await;
    let main1 = source_of(&session.store, PLAIN, 1).unwrap();
    let source = session.store.source(main1).unwrap();
    report.check(source.mode == "attach", || format!("attach run 1: mode {:?}", source.mode));
    for h in &source.hooks {
        report.check(h.status == "installed", || format!("attach run 1: hook {} is {}", h.id, h.status));
    }
    report.check(source.hooks.len() == 4, || format!("attach run 1: {} hooks", source.hooks.len()));

    // run 1: the scenarios in the attached process (the intent reaches the running activity)
    let run = adb.shell(id, &format!("am start -n {PLAIN_ACTIVITY} --es run all")).await.unwrap();
    assert_eq!(run.exit, 0);
    session.until("attach run 1 to finish", Duration::from_secs(120), |s| has_done(s, source_of(s, PLAIN, 1))).await;
    check_main_run(&mut report, &session.store, main1, "attach run 1");
    if from_start {
        // the worker process started during the run: the startup agent was in it from its start
        session
            .until("the worker's request", Duration::from_secs(30), |s| {
                source_of(s, PLAIN_WORKER, 0)
                    .is_some_and(|w| txns(s, w).iter().any(|t| t.url.path == "/worker/ping" && !t.state.is_open()))
            })
            .await;
        check_worker(
            &mut report,
            &session.store,
            source_of(&session.store, PLAIN_WORKER, 0).unwrap(),
            "attach run 1 worker",
        );
        report.check(startup_agent(&adb, id).await, || "--follow on API 30+: no startup agent in the app".into());
    }

    // run 2: a restart is followed; with a startup agent its requests are captured from its start
    adb.shell(id, &format!("am force-stop {PLAIN}")).await.unwrap();
    session
        .until("run 1 to end", Duration::from_secs(30), |s| s.source(main1).is_some_and(|x| x.ended.is_some()))
        .await;
    let run = adb.shell(id, &format!("am start -n {PLAIN_ACTIVITY} --es run all")).await.unwrap();
    assert_eq!(run.exit, 0);
    session.until("attach run 2 to finish", Duration::from_secs(120), |s| has_done(s, source_of(s, PLAIN, 2))).await;
    let main2 = source_of(&session.store, PLAIN, 2).unwrap();
    if from_start {
        check_main_run(&mut report, &session.store, main2, "attach run 2 (from its start)");
    }
    check_main_run(&mut report, &session.store, main1, "attach run 1 after the restart");

    drop(quit);
    for t in tasks {
        tokio::time::timeout(Duration::from_secs(5), t).await.expect("backend did not stop").unwrap();
    }
    report.check(!startup_agent(&adb, id).await, || "the startup agent stayed after the session".into());
    let left = our_forwards(&adb, &serial).await;
    report.check(left.is_empty(), || format!("forwards left behind: {left:?}"));

    // run 3: --launch restarts the app with the agent loading at its start (API 27+)
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (status_tx, _status_rx) = watch::channel(ConnectionStatus::Waiting(String::new()));
    let launch = Launch { extras: vec!["--es".into(), "run".into(), "all".into()] };
    let target = attach_target(&serial, PLAIN, &kit, false, Some(launch));
    let task = tokio::spawn(run_device(
        adb.clone(),
        target,
        session.store.source_ids(),
        event_tx.clone(),
        cmd_rx,
        status_tx,
        None,
    ));
    drop(event_tx);
    session
        .until("the launched run to finish", Duration::from_secs(120), |s| has_done(s, source_of(s, PLAIN, 3)))
        .await;
    let main3 = source_of(&session.store, PLAIN, 3).unwrap();
    if launch_from_start {
        check_main_run(&mut report, &session.store, main3, "attach run 3 (--launch)");
    }
    // without --follow the startup agent went once the launched process connected
    report.check(!startup_agent(&adb, id).await, || "--launch: the startup agent stayed after the connection".into());
    drop(cmd_tx);
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("backend did not stop").unwrap();
    let left = our_forwards(&adb, &serial).await;
    report.check(left.is_empty(), || format!("forwards left behind: {left:?}"));

    summary(&session.store);
    assert!(report.problems.is_empty(), "{} problems:\n  {}", report.problems.len(), report.problems.join("\n  "));
    let _ = adb.shell(id, &format!("am force-stop {PLAIN}")).await;
}
