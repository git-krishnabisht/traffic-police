//! The Flutter backend (ARCHITECTURE.md §5.16) against a fake adb server and a fake Dart VM
//! service: the address found in logcat, the forward, HTTP logging turned on in the app's
//! isolate, requests read from the profile with their bodies, a failure, pause, DDS in front of
//! the VM service, an app too old, and the app's exit. No device or Flutter app is needed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::BytesMut;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use traffic_police_backends::flutter::ws;
use traffic_police_backends::{DeviceTarget, run_device};
use traffic_police_core::backend::{BackendCommand, ConnectionStatus};
use traffic_police_core::decode::decode_body;
use traffic_police_core::model::TxnState;
use traffic_police_core::store::SessionStore;
use traffic_police_fakeadb::FakeAdb;

const SERIAL: &str = "fake-1";
const PACKAGE: &str = "com.example.flutter_shop";
const PID: u32 = 4321;
const DEVICE_PORT: u16 = 41_000;
const TOKEN: &str = "Wq9tyH3o9fo=";

/// What the fake VM service answers, and what it was asked.
struct VmState {
    token: String,
    /// Answers the upgrade with a redirect there (DDS owns this VM service).
    redirect: Option<String>,
    dart_io_major: u64,
    /// The profile's requests; every poll returns them all (`updatedSince` is inclusive, so a
    /// client sees entries again and must not repeat itself).
    requests: Vec<Value>,
    /// `getHttpProfileRequest` answers, by id.
    full: HashMap<String, Value>,
    calls: Vec<(String, Value)>,
    /// HTTP logging by isolate (off until a client turns it on, as in dart:io).
    logging: HashMap<String, bool>,
}

struct FakeVm {
    addr: SocketAddr,
    state: Arc<Mutex<VmState>>,
    /// Drops every connection (the app exited).
    stop: watch::Sender<bool>,
}

impl FakeVm {
    async fn start(token: &str) -> FakeVm {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(VmState {
            token: token.into(),
            redirect: None,
            dart_io_major: 4,
            requests: Vec::new(),
            full: HashMap::new(),
            calls: Vec::new(),
            logging: HashMap::new(),
        }));
        let (stop, stop_rx) = watch::channel(false);
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else { return };
                let (st, mut stop) = (st.clone(), stop_rx.clone());
                tokio::spawn(async move {
                    let _ = serve(s, st, &mut stop).await;
                });
            }
        });
        FakeVm { addr, state, stop }
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        self.state.lock().unwrap().calls.iter().filter(|(m, _)| m == method).map(|(_, p)| p.clone()).collect()
    }

    async fn until_called(&self, what: &str, done: impl Fn(&FakeVm) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "the VM service was not asked {what}: {:?}",
                self.state.lock().unwrap().calls
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn serve(mut s: TcpStream, state: Arc<Mutex<VmState>>, stop: &mut watch::Receiver<bool>) -> std::io::Result<()> {
    let mut buf = BytesMut::new();
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if s.read_buf(&mut buf).await? == 0 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let _ = buf.split_to(end);
    let (token, redirect) = {
        let st = state.lock().unwrap();
        (st.token.clone(), st.redirect.clone())
    };
    if let Some(to) = redirect {
        let r = format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n");
        return s.write_all(r.as_bytes()).await;
    }
    // the VM service checks the auth code and the Host
    let host_ok = head.lines().any(|l| l.to_ascii_lowercase().starts_with("host: 127.0.0.1:"));
    if !head.starts_with(&format!("GET /{token}/ws HTTP/1.1")) || !host_ok {
        return s.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await;
    }
    s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: x\r\n\r\n")
        .await?;
    loop {
        while let Some(f) = ws::decode(&mut buf)? {
            if f.opcode == ws::OP_CLOSE {
                return Ok(());
            }
            if f.opcode != ws::OP_TEXT {
                continue;
            }
            let req: Value = serde_json::from_slice(&f.payload).expect("JSON-RPC");
            let reply = answer(&state, &req);
            // a long answer goes in two fragments, as servers may send it
            let text = reply.to_string();
            let (a, b) = text.split_at(text.len() / 2);
            let mut first = ws::encode(ws::OP_TEXT, a.as_bytes(), None);
            first[0] &= 0x7f; // not the last fragment
            s.write_all(&first).await?;
            s.write_all(&ws::encode(ws::OP_CONTINUATION, b.as_bytes(), None)).await?;
        }
        tokio::select! {
            n = s.read_buf(&mut buf) => if n? == 0 { return Ok(()) },
            _ = stop.changed() => return Ok(()),
        }
    }
}

fn answer(state: &Mutex<VmState>, req: &Value) -> Value {
    let mut st = state.lock().unwrap();
    let method = req["method"].as_str().unwrap_or_default().to_string();
    st.calls.push((method.clone(), req["params"].clone()));
    let result = match method.as_str() {
        "getVM" => {
            json!({ "type": "VM", "version": "3.13.5 (stable) (Tue Sep 30 12:00:00 2026 +0000) on \"android_arm64\"",
            "pid": PID, "isolates": [{ "type": "@Isolate", "id": "isolates/77", "number": "77", "name": "main", "isSystemIsolate": false }] })
        }
        "streamListen" => json!({ "type": "Success" }),
        "ext.dart.io.getVersion" => json!({ "type": "Version", "major": st.dart_io_major, "minor": 0 }),
        "ext.dart.io.httpEnableTimelineLogging" => {
            let iso = req["params"]["isolateId"].as_str().unwrap_or_default().to_string();
            if let Some(on) = req["params"]["enabled"].as_bool() {
                st.logging.insert(iso.clone(), on);
            }
            json!({ "type": "HttpTimelineLoggingState", "enabled": st.logging.get(&iso).copied().unwrap_or(false) })
        }
        "ext.dart.io.getHttpProfile" => {
            json!({ "type": "HttpProfile", "timestamp": 1_790_000_100_000_000i64, "requests": st.requests })
        }
        "ext.dart.io.getHttpProfileRequest" => match st.full.get(req["params"]["id"].as_str().unwrap_or_default()) {
            Some(f) => f.clone(),
            None => {
                return json!({ "jsonrpc": "2.0", "id": req["id"], "error": { "code": -32602, "message": "Invalid params",
                    "data": { "details": "Unable to find request" } } });
            }
        },
        _ => {
            return json!({ "jsonrpc": "2.0", "id": req["id"], "error": { "code": -32601, "message": "Method not found" } });
        }
    };
    json!({ "jsonrpc": "2.0", "id": req["id"], "result": result })
}

/// A GET as dart:io profiles it once it is done: gzip on the wire, decompressed for the app.
fn get_entry() -> Value {
    json!({ "type": "@HttpProfileRequest", "id": "-8392571130443811", "isolateId": "isolates/77", "method": "GET",
        "uri": "https://api.example.com/v1/items?page=2", "startTime": 1_790_000_050_000_000i64,
        "endTime": 1_790_000_050_020_000i64,
        "events": [{ "timestamp": 1_790_000_050_010_000i64, "event": "Connection established" },
                   { "timestamp": 1_790_000_050_010_050i64, "event": "Request sent" },
                   { "timestamp": 1_790_000_050_200_000i64, "event": "Waiting (TTFB)" },
                   { "timestamp": 1_790_000_050_230_000i64, "event": "Content Download" }],
        "request": { "headers": { "user-agent": ["Dart/3.13 (dart:io)"], "accept-encoding": ["gzip"], "host": ["api.example.com"] },
                     "connectionInfo": { "localPort": 40512, "remoteAddress": "203.0.113.10", "remotePort": 443 },
                     "contentLength": 0, "method": "GET", "uri": "https://api.example.com/v1/items?page=2" },
        "response": { "startTime": 1_790_000_050_200_100i64, "statusCode": 200, "reasonPhrase": "OK",
                      "headers": { "content-type": ["application/json; charset=utf-8"], "content-encoding": ["gzip"] },
                      "compressionState": "HttpClientResponseCompressionState.decompressed",
                      "connectionInfo": { "localPort": 40512, "remoteAddress": "203.0.113.10", "remotePort": 443 },
                      "contentLength": -1, "redirects": [], "endTime": 1_790_000_050_230_000i64 } })
}

/// A POST still being sent: dart:io has no `request` for it yet.
fn post_entry() -> Value {
    json!({ "type": "@HttpProfileRequest", "id": "17", "isolateId": "isolates/77", "method": "POST",
        "uri": "http://10.0.2.2:9/v1/events", "startTime": 1_790_000_060_000_000i64, "events": [] })
}

fn serve_get(vm: &FakeVm) {
    let mut st = vm.state.lock().unwrap();
    let mut full = get_entry();
    full["type"] = json!("HttpProfileRequest");
    full["requestBody"] = json!([]);
    full["responseBody"] = json!(br#"{"items":[1,2]}"#.to_vec());
    st.full.insert("-8392571130443811".into(), full);
    st.requests = vec![get_entry(), post_entry()];
}

struct Harness {
    store: SessionStore,
    events: mpsc::Receiver<Vec<traffic_police_core::SessionEvent>>,
    status: watch::Receiver<ConnectionStatus>,
    commands: Option<mpsc::UnboundedSender<BackendCommand>>,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    fn start(fake: &FakeAdb) -> Harness {
        let target = DeviceTarget {
            serial: Some(SERIAL.into()),
            package: PACKAGE.into(),
            flutter: true,
            ..DeviceTarget::default()
        };
        let store = SessionStore::new();
        let (event_tx, events) = mpsc::channel(256);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(ConnectionStatus::Waiting(String::new()));
        let task =
            tokio::spawn(run_device(fake.client(), target, store.source_ids(), event_tx, cmd_rx, status_tx, None));
        Harness { store, events, status, commands: Some(cmd_tx), task }
    }

    async fn until(&mut self, what: &str, done: impl Fn(&SessionStore, &ConnectionStatus) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if done(&self.store, &self.status.borrow()) {
                return;
            }
            tokio::select! {
                batch = self.events.recv() => match batch {
                    Some(batch) => batch.into_iter().for_each(|e| self.store.apply(e)),
                    None => {
                        assert!(done(&self.store, &self.status.borrow()), "the backend ended before {what}: {:?}", *self.status.borrow());
                        return;
                    }
                },
                _ = self.status.changed() => {}
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timed out waiting for {what}; status {:?}; {} requests", *self.status.borrow(), self.store.len());
                }
            }
        }
    }

    fn send(&self, cmd: BackendCommand) {
        self.commands.as_ref().expect("running").send(cmd).unwrap();
    }

    async fn quit(mut self) -> SessionStore {
        if let Some(c) = self.commands.take() {
            let _ = c.send(BackendCommand::Shutdown);
        }
        tokio::time::timeout(Duration::from_secs(5), &mut self.task).await.expect("the backend did not stop").unwrap();
        while let Ok(batch) = self.events.try_recv() {
            batch.into_iter().for_each(|e| self.store.apply(e));
        }
        self.store
    }
}

fn the_app(fake: &FakeAdb, vm: &FakeVm) {
    fake.add_device(SERIAL, 36);
    fake.start_process(SERIAL, PID, PACKAGE);
    fake.listen_tcp(SERIAL, DEVICE_PORT, vm.addr);
    fake.log(
        SERIAL,
        PID,
        "I/flutter ( 4321): The Dart VM service is listening on http://127.0.0.1:41000/Wq9tyH3o9fo=/",
    );
}

fn live(s: &ConnectionStatus) -> bool {
    matches!(s, ConnectionStatus::Live(_))
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_requests_with_their_bodies_and_follows_pause_and_exit() {
    let fake = FakeAdb::start().await;
    let vm = FakeVm::start(TOKEN).await;
    the_app(&fake, &vm);
    serve_get(&vm);
    let mut h = Harness::start(&fake);
    h.until("the GET", |s, st| live(st) && s.len() == 1 && !s.txn(0).state.is_open()).await;

    // HTTP logging is on in the app's isolate (asked first, then turned on)
    assert_eq!(vm.state.lock().unwrap().logging.get("isolates/77"), Some(&true));

    let src = h.store.sources().next().expect("a source");
    assert_eq!((src.mode.as_str(), src.package.as_str(), src.pid), ("flutter", PACKAGE, PID));
    let t = h.store.txn(0);
    assert_eq!((t.method.as_str(), t.url.raw.as_str()), ("GET", "https://api.example.com/v1/items?page=2"));
    assert_eq!(t.status(), Some(200));
    assert_eq!(t.client.as_ref().map(|c| c.label()).as_deref(), Some("dart:io 3.13.5"));
    assert_eq!(t.thread.as_ref().unwrap().name, "isolate main");
    assert_eq!(t.conn.as_ref().unwrap().remote.as_ref().unwrap().ip, "203.0.113.10");
    // the body as the app read it, decompressed, while the header still says gzip
    assert!(t.resp_body.decoded);
    let headers = t.resp.as_ref().map(|r| &r.headers);
    assert!(headers.unwrap().iter().any(|(n, v)| n == "content-encoding" && v == "gzip"));
    let d = decode_body(h.store.body_bytes(&t.resp_body), h.store.decoding_headers(t, headers).as_deref(), 1 << 20);
    assert_eq!((d.error, &d.bytes[..]), (None, &br#"{"items":[1,2]}"#[..]));
    // its timing, from dart:io's events (device time: the clocks the fake device reported):
    // connected 10 ms after the start, sent at 20 ms, headers at 200 ms, the body at 230 ms
    let p = t.phases(h.store.latest());
    assert_eq!(p.connect.map(|(a, b)| b - a), Some(10_000_000), "{p:?}");
    assert_eq!(p.wait.map(|(a, b)| b - a), Some(180_000_000), "{p:?}");
    assert_eq!(p.receive.map(|(a, b)| b - a), Some(30_000_000), "{p:?}");
    assert_eq!(t.start, 8_283_090_000_000 + 50_000_000_000, "wall-clock µs to device ns");
    // the POST is not shown before dart:io has its request
    assert_eq!(h.store.len(), 1);

    // the POST fails
    {
        let mut st = vm.state.lock().unwrap();
        let mut failed = post_entry();
        failed["endTime"] = json!(1_790_000_060_005_000i64);
        failed["request"] = json!({ "error": "SocketException: Connection refused (OS Error: Connection refused, errno = 111), address = 10.0.2.2, port = 9" });
        st.full.insert("17".into(), failed.clone());
        st.requests = vec![get_entry(), failed];
    }
    h.until("the failure", |s, _| s.len() == 2 && s.txn(1).state == TxnState::Failed).await;
    let f = h.store.txn(1).failure.clone().unwrap();
    assert_eq!((f.class.as_str(), f.phase.as_deref()), ("SocketException", Some("connect")));
    assert_eq!(h.store.len(), 2, "each request once, however often it is polled");
    assert_eq!(vm.calls("ext.dart.io.getHttpProfileRequest").len(), 2, "each body fetched once");

    // pause: dart:io stops recording new requests
    h.send(BackendCommand::SetRecording(false));
    vm.until_called("to stop logging", |vm| {
        vm.calls("ext.dart.io.httpEnableTimelineLogging").last().is_some_and(|p| p["enabled"] == false)
    })
    .await;

    // the app exits
    fake.kill_process(SERIAL, PID);
    let _ = vm.stop.send(true);
    h.until("the end", |_, st| matches!(st, ConnectionStatus::Detached(_))).await;
    let store = h.quit().await;
    assert!(store.sources().next().unwrap().ended.is_some(), "the source ended");
    assert!(fake.forwards().is_empty(), "the forward is gone: {:?}", fake.forwards());
}

#[tokio::test(flavor = "multi_thread")]
async fn waits_for_the_address_and_goes_through_dds() {
    let fake = FakeAdb::start().await;
    // a Flutter tool's DDS on this computer owns the VM service: the upgrade is redirected there
    let dds = FakeVm::start("dds=").await;
    serve_get(&dds);
    let vm = FakeVm::start(TOKEN).await;
    vm.state.lock().unwrap().redirect = Some(format!("http://127.0.0.1:{}/dds=/", dds.addr.port()));
    fake.add_device(SERIAL, 36);
    fake.start_process(SERIAL, PID, PACKAGE);
    fake.listen_tcp(SERIAL, DEVICE_PORT, vm.addr);
    let mut h = Harness::start(&fake);
    // no address logged yet
    h.until(
        "waiting for the address",
        |_, st| matches!(st, ConnectionStatus::Waiting(m) if m.contains("Dart VM service")),
    )
    .await;
    fake.log(
        SERIAL,
        PID,
        "I/flutter ( 4321): The Dart VM service is listening on http://127.0.0.1:41000/Wq9tyH3o9fo=/",
    );
    h.until("the GET through DDS", |s, st| live(st) && s.len() == 1 && !s.txn(0).state.is_open()).await;
    assert!(vm.calls("getVM").is_empty(), "the VM service itself was not used");
    assert!(!dds.calls("getVM").is_empty());
    assert_eq!(dds.state.lock().unwrap().logging.get("isolates/77"), Some(&true));
    h.quit().await;
    // the app is left as it was found: logging off
    assert_eq!(dds.state.lock().unwrap().logging.get("isolates/77"), Some(&false));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dart_too_old_is_refused_with_its_version() {
    let fake = FakeAdb::start().await;
    let vm = FakeVm::start(TOKEN).await;
    vm.state.lock().unwrap().dart_io_major = 3;
    the_app(&fake, &vm);
    let mut h = Harness::start(&fake);
    h.until("the refusal", |_, st| matches!(st, ConnectionStatus::Failed(_))).await;
    let ConnectionStatus::Failed(m) = h.status.borrow().clone() else { unreachable!() };
    assert!(m.contains("version 3") && m.contains("Flutter 3.22"), "{m}");
    h.quit().await;
    assert!(fake.forwards().is_empty());
}
