//! The demo device: a simulated app process that speaks the real protocol (ARCHITECTURE.md §5.3).
//!
//! [`DemoDevice`] runs a discrete-event simulation and appends protocol bytes; [`DemoSession`]
//! decodes those bytes with the same code a live connection uses and yields [`SessionEvent`]s.
//! With a fixed seed the timeline is identical on every run, which the snapshot tests rely on.

pub mod content;

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use bytes::{Bytes, BytesMut};
use serde_json::json;
use traffic_police_core::event::SessionEvent;
use traffic_police_core::model::SourceInfo;
use traffic_police_core::normalize::{Control, Normalizer};
use traffic_police_core::store::SourceIds;
use traffic_police_proto::msg::{
    self, Addr, AppInfo, Cert, Change, ClientInfo, Clock, Conn, DeliveredResponse, DeviceInfo, DeviceMsg, ErrorInfo,
    Hello, Pattern, ReqBodyInfo, Rule, RuleAction, RuleMatch, RuleRef, RuleSet, RuntimeInfo, StackFrame, ThreadInfo,
    Tls,
};
use traffic_police_proto::{BodyChunk, BodyDir, Decoder, Frame, Headers, PROTOCOL_VERSION, frame};

use content::{Rng, gzip, h, http_date};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
/// Device boot-time clock at the start of the demo (about 96 minutes after boot).
const BASE_TS: u64 = 5_800_000_000_000;
/// Wall clock used for virtual (test) runs: 2026-09-29 05:10:00 UTC.
pub const VIRTUAL_WALL_MS: i64 = 1_790_658_600_000;

/// The pretend app: a shop's debug build. Its API is the developer's local server, reached over
/// `adb reverse tcp:8080 tcp:8080`; sign-in, images and telemetry are on example.com hosts
/// (reserved for examples, RFC 2606).
const PACKAGE: &str = "com.example.shop";
const APP_VERSION: &str = "3.8.0";
const API: &str = "http://localhost:8080";
const API_HOST: &str = "localhost:8080";
const CDN: &str = "https://cdn.example.com";
const TELEMETRY: &str = "https://telemetry.example.com";
const AUTH: &str = "https://auth.example.com";

#[derive(Debug, Clone)]
pub struct DemoConfig {
    pub seed: u64,
    /// Simulate the app process dying after this long, then relaunching 2 s later.
    pub restart_after_ns: Option<u64>,
    /// Wall clock at demo time zero.
    pub wall_start_ms: i64,
}

impl Default for DemoConfig {
    fn default() -> Self {
        DemoConfig { seed: 7, restart_after_ns: None, wall_start_ms: VIRTUAL_WALL_MS }
    }
}

/// The rules the demo pretends the host pushed (shown in the Rules view).
pub fn demo_rules() -> RuleSet {
    RuleSet {
        version: "demo-1".into(),
        rules: vec![
            Rule {
                id: "force-paid".into(),
                name: Some("Force payment captured".into()),
                enabled: true,
                matcher: RuleMatch {
                    methods: vec!["GET".into()],
                    host: Some(Pattern::Exact("localhost".into())),
                    port: Some(8080),
                    path: Some(Pattern::Exact("/api/v1/orders/status".into())),
                    ..Default::default()
                },
                actions: vec![RuleAction::Replace {
                    find: "\"payment\":\"pending\"".into(),
                    with: "\"payment\":\"captured\"".into(),
                    regex: false,
                }],
                cache_rewrites: false,
            },
            Rule {
                id: "checkout-down".into(),
                name: Some("Checkout endpoint down".into()),
                enabled: false,
                matcher: RuleMatch {
                    methods: vec!["POST".into()],
                    path: Some(Pattern::Exact("/api/v1/checkout".into())),
                    ..Default::default()
                },
                actions: vec![RuleAction::Fail {
                    exception: "timeout".into(),
                    message: Some("simulated by traffic-police".into()),
                }],
                cache_rewrites: false,
            },
        ],
    }
}

// ---------------------------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
enum Emit {
    Hello,
    Msg(Box<DeviceMsg>),
    Chunk { txn: u64, dir: BodyDir, offset: u64, data: Bytes },
    Bye(String),
}

#[derive(Debug, Clone)]
enum Flow {
    Boot,
    Login,
    Order { step: u8, polls: u32, session: String },
    Telemetry,
    Notifications,
    OneOff(OneOff),
    TrafficTick,
    Die,
    Launch,
}

#[derive(Debug, Clone, Copy)]
enum OneOff {
    Avatar,
    Redirect,
    Protobuf,
    NotFound,
    Upload,
    ServerError,
    Volley,
    Timeout,
    Canceled,
    Download,
    Dropped,
    Burst,
    /// A WebSocket: live order updates for a while, then closed.
    Socket,
    /// gRPC calls: a lookup, a NOT_FOUND, a server stream.
    Grpc,
}

#[derive(Debug)]
enum Item {
    Emit { generation: u32, emit: Emit },
    Flow { generation: u32, flow: Flow },
}

struct Scheduled {
    at: u64,
    order: u64,
    item: Item,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.order) == (other.at, other.order)
    }
}
impl Eq for Scheduled {}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap: reverse for earliest-first
        (other.at, other.order).cmp(&(self.at, self.order))
    }
}

// ---------------------------------------------------------------------------------------------
// Exchanges
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Fail {
    phase: &'static str,
    class: &'static str,
    message: &'static str,
    /// Time after the request was sent (or after start, when `canceled`).
    after_ms: u64,
    canceled: bool,
}

#[derive(Debug, Clone)]
struct RuleFx {
    changes: Vec<Change>,
    delivered_headers: Headers,
    delivered_body: Bytes,
}

#[derive(Debug, Clone)]
struct Ex {
    method: &'static str,
    url: String,
    req_headers: Headers,
    req_body: Option<(String, Bytes)>,
    status: u16,
    reason: &'static str,
    resp_headers: Headers,
    resp_body: Bytes,
    protocol: &'static str,
    client: (&'static str, Option<&'static str>),
    thread: ThreadInfo,
    stack: Vec<StackFrame>,
    /// (dns, connect, tls) for a new connection; send, wait, receive always.
    setup_ms: (u64, u64, u64),
    send_ms: u64,
    wait_ms: u64,
    receive_ms: u64,
    fail: Option<Fail>,
    rule: Option<RuleFx>,
    call: Option<u64>,
    hop: u32,
    last_hop: bool,
    chunk: usize,
    /// A gRPC call's trailers and status.
    trailers: Headers,
    grpc: Option<msg::GrpcStatus>,
}

impl Ex {
    fn host(&self) -> &str {
        let rest = self.url.split_once("://").map_or(self.url.as_str(), |(_, r)| r);
        rest.split(['/', '?']).next().unwrap_or(rest)
    }
}

#[derive(Debug, Clone)]
struct ProcessState {
    generation: u32,
    pid: u32,
    instance: String,
    alive: bool,
    connections: HashMap<String, (String, bool)>,
    next_conn: u32,
    token: String,
    telemetry_txns: Vec<u64>,
}

/// One step of the order flow: request line, bodies, and the app call site.
struct OrderStep {
    method: &'static str,
    path: &'static str,
    req: Option<String>,
    resp: String,
    api: &'static str,
    caller: (&'static str, &'static str, &'static str),
    line: i32,
}

impl OrderStep {
    fn new(
        method: &'static str,
        path: &'static str,
        req: Option<String>,
        resp: String,
        api: &'static str,
        caller: (&'static str, &'static str, &'static str),
        line: i32,
    ) -> Self {
        OrderStep { method, path, req, resp, api, caller, line }
    }
}

/// A simulated app process producing protocol bytes.
pub struct DemoDevice {
    cfg: DemoConfig,
    rng: Rng,
    queue: BinaryHeap<Scheduled>,
    order: u64,
    now: u64,
    proc: ProcessState,
    next_txn: u64,
    next_call: u64,
    seq: u64,
    rx: u64,
    tx: u64,
    last_tick: u64,
    last_sent: (u64, u64),
    burst_until: u64,
    recording: bool,
    sessions: u32,
    worker: u32,
}

impl DemoDevice {
    pub fn new(cfg: DemoConfig) -> Self {
        let mut rng = Rng::new(cfg.seed);
        let instance = rng.hex(32);
        let mut d = DemoDevice {
            rng,
            queue: BinaryHeap::new(),
            order: 0,
            now: BASE_TS,
            proc: ProcessState {
                generation: 0,
                pid: 4312,
                instance,
                alive: true,
                connections: HashMap::new(),
                next_conn: 1,
                token: String::new(),
                telemetry_txns: Vec::new(),
            },
            next_txn: 1,
            next_call: 1,
            seq: 0,
            rx: 18_234_112,
            tx: 1_203_340,
            last_tick: BASE_TS,
            last_sent: (0, 0),
            burst_until: 0,
            recording: true,
            sessions: 0,
            worker: 0,
            cfg,
        };
        d.push_emit(BASE_TS, Emit::Hello);
        d.push_flow(BASE_TS, Flow::Boot);
        d.push_flow(BASE_TS, Flow::TrafficTick);
        if let Some(after) = d.cfg.restart_after_ns {
            d.push_flow(BASE_TS + after, Flow::Die);
        }
        d
    }

    /// Simulated time since the demo started.
    pub fn elapsed(&self) -> u64 {
        self.now - BASE_TS
    }

    /// Stop or resume starting new requests (in-flight ones finish), like `set_config`.
    pub fn set_recording(&mut self, on: bool) {
        self.recording = on;
    }

    fn push(&mut self, at: u64, item: Item) {
        self.order += 1;
        self.queue.push(Scheduled { at, order: self.order, item });
    }

    fn push_emit(&mut self, at: u64, emit: Emit) {
        let generation = self.proc.generation;
        self.push(at, Item::Emit { generation, emit });
    }

    fn push_flow(&mut self, at: u64, flow: Flow) {
        let generation = self.proc.generation;
        self.push(at, Item::Flow { generation, flow });
    }

    fn msg(&mut self, at: u64, m: DeviceMsg) {
        self.push_emit(at, Emit::Msg(Box::new(m)));
    }

    fn mark(&mut self, at: u64, txn: u64, name: &str) {
        self.msg(at, DeviceMsg::Mark(msg::Mark { seq: 0, ts: at, txn, m: name.into() }));
    }

    fn wall(&self, ts: u64) -> i64 {
        self.cfg.wall_start_ms + ((ts - BASE_TS) / MS) as i64
    }

    fn next_worker(&mut self) -> ThreadInfo {
        self.worker = self.worker % 4 + 1;
        content::thread(&format!("DefaultDispatcher-worker-{}", self.worker), 57 + i64::from(self.worker), "call")
    }

    /// Advance simulated time to `elapsed_ns` after the start, appending protocol bytes to `out`.
    pub fn run_until(&mut self, elapsed_ns: u64, out: &mut BytesMut) {
        let target = BASE_TS + elapsed_ns;
        while self.queue.peek().is_some_and(|s| s.at <= target) {
            let s = self.queue.pop().expect("peeked");
            self.now = self.now.max(s.at);
            match s.item {
                Item::Emit { generation, emit } => {
                    if generation == self.proc.generation {
                        self.emit(s.at, emit, out);
                    }
                }
                Item::Flow { generation, flow } => {
                    if generation == self.proc.generation || matches!(flow, Flow::TrafficTick | Flow::Launch) {
                        self.step(s.at, flow);
                    }
                }
            }
        }
        self.now = self.now.max(target);
    }

    fn set_seq(&mut self, m: &mut DeviceMsg) {
        self.seq += 1;
        let s = self.seq;
        match m {
            DeviceMsg::Req(x) => x.seq = s,
            DeviceMsg::Resp(x) => x.seq = s,
            DeviceMsg::BodyEnd(x) => x.seq = s,
            DeviceMsg::Prog(x) => x.seq = s,
            DeviceMsg::Mark(x) => x.seq = s,
            DeviceMsg::Done(x) => x.seq = s,
            DeviceMsg::Fail(x) => x.seq = s,
            DeviceMsg::Rule(x) => x.seq = s,
            DeviceMsg::Dropped(x) => x.seq = s,
            DeviceMsg::Traffic(x) => x.seq = s,
            DeviceMsg::Diag(x) => x.seq = s,
            DeviceMsg::Ws(x) => x.seq = s,
            _ => self.seq -= 1,
        }
    }

    fn emit(&mut self, at: u64, emit: Emit, out: &mut BytesMut) {
        match emit {
            Emit::Hello => {
                let hello = DeviceMsg::Hello(Hello {
                    protocol: PROTOCOL_VERSION,
                    runtime: RuntimeInfo {
                        version: env!("CARGO_PKG_VERSION").into(),
                        build: Some("demo".into()),
                        mode: "library".into(),
                    },
                    instance: self.proc.instance.clone(),
                    app: AppInfo {
                        package: PACKAGE.into(),
                        process: PACKAGE.into(),
                        pid: self.proc.pid,
                        uid: Some(10_234),
                        debuggable: Some(true),
                    },
                    device: DeviceInfo {
                        api: 36,
                        release: Some("16".into()),
                        manufacturer: Some("Google".into()),
                        model: Some("Pixel 8".into()),
                        abi: Some("arm64-v8a".into()),
                        abis: vec!["arm64-v8a".into()],
                    },
                    clock: Clock { ts: at, wall_ms: self.wall(at) },
                    started_ts: Some(at),
                    capabilities: ["okhttp", "okhttp_events", "huc", "rules", "pause", "resume", "prog", "traffic"]
                        .map(String::from)
                        .to_vec(),
                    clients: [("okhttp".to_string(), Some("4.12.0".to_string()))].into_iter().collect(),
                    hooks: Vec::new(),
                    buffer: None,
                    config: Some(msg::CaptureConfig::default()),
                });
                msg::encode_msg(&hello, out).expect("serializable");
            }
            Emit::Bye(reason) => {
                let bye = DeviceMsg::Bye(msg::Bye {
                    reason: "shutdown".into(),
                    message: Some(reason),
                    supported: Vec::new(),
                });
                msg::encode_msg(&bye, out).expect("serializable");
            }
            Emit::Msg(m) => {
                let mut m = *m;
                match &m {
                    DeviceMsg::Req(r) => {
                        self.tx += r.headers.iter().map(|(n, v)| (n.len() + v.len() + 4) as u64).sum::<u64>() + 40;
                    }
                    DeviceMsg::Resp(r) => {
                        self.rx += r.headers.iter().map(|(n, v)| (n.len() + v.len() + 4) as u64).sum::<u64>() + 20;
                    }
                    _ => {}
                }
                self.set_seq(&mut m);
                msg::encode_msg(&m, out).expect("serializable");
            }
            Emit::Chunk { txn, dir, offset, data } => {
                match dir {
                    BodyDir::Request => self.tx += data.len() as u64 * 103 / 100,
                    BodyDir::Response => self.rx += data.len() as u64 * 103 / 100,
                    BodyDir::Delivered => {}
                }
                self.seq += 1;
                frame::encode_body(&BodyChunk { seq: self.seq, txn, dir, ts: at, offset, data }, out);
            }
        }
    }

    // --- flows -------------------------------------------------------------------------------

    fn step(&mut self, at: u64, flow: Flow) {
        match flow {
            Flow::Boot => {
                self.diag(
                    at + 20 * MS,
                    "info",
                    "started",
                    "capture runtime started (library mode, OkHttp 4.12.0, HttpURLConnection)",
                );
                self.diag(
                    at + 25 * MS,
                    "warn",
                    "studio_inspector_present",
                    "Android Studio's network interceptor is also installed; it may rewrite response headers",
                );
                self.push_flow(at + 600 * MS, Flow::Login);
                self.push_flow(at + 1_500 * MS, Flow::Notifications);
                self.push_flow(at + 2_200 * MS, Flow::Telemetry);
                if self.proc.generation == 0 {
                    for (secs_x10, o) in [
                        (30, OneOff::Avatar),
                        (41, OneOff::Redirect),
                        (52, OneOff::Protobuf),
                        (63, OneOff::NotFound),
                        (74, OneOff::Upload),
                        (85, OneOff::ServerError),
                        (93, OneOff::Volley),
                        (101, OneOff::Timeout),
                        (112, OneOff::Canceled),
                        (123, OneOff::Download),
                        (165, OneOff::Dropped),
                        (190, OneOff::Burst),
                        // after the moments the snapshot tests show (12 s, 40 s)
                        (420, OneOff::Socket),
                        (445, OneOff::Grpc),
                    ] {
                        self.push_flow(at + secs_x10 * 100 * MS, Flow::OneOff(o));
                    }
                }
            }
            Flow::TrafficTick => {
                // background non-HTTP traffic (sockets, WebView, other SDKs) only the uid counters see
                if self.proc.alive {
                    self.rx += self.rng.range(200, 2_600);
                    self.tx += self.rng.range(80, 700);
                    if at < self.burst_until {
                        self.rx += self.rng.range(70_000, 110_000);
                        self.tx += self.rng.range(1_000, 3_000);
                    }
                }
                if (self.rx, self.tx) != self.last_sent {
                    let since = self.last_tick;
                    self.last_sent = (self.rx, self.tx);
                    let t = DeviceMsg::Traffic(msg::Traffic {
                        seq: 0,
                        ts: at,
                        rx: self.rx,
                        tx: self.tx,
                        since: Some(since),
                    });
                    if self.proc.alive {
                        self.msg(at, t);
                    }
                }
                self.last_tick = at;
                self.push_flow(at + 500 * MS, Flow::TrafficTick);
            }
            Flow::Login => self.login(at),
            Flow::Order { step, polls, session } => self.order(at, step, polls, session),
            Flow::Telemetry => {
                if self.recording {
                    self.telemetry(at);
                }
                let next = at + self.rng.range(3_000, 4_500) * MS;
                self.push_flow(next, Flow::Telemetry);
            }
            Flow::Notifications => {
                if self.recording {
                    self.notifications(at);
                }
                self.push_flow(at + 2_000 * MS, Flow::Notifications);
            }
            Flow::OneOff(o) => {
                if self.recording {
                    self.one_off(at, o);
                }
            }
            Flow::Die => {
                self.push_emit(at, Emit::Bye("process died (simulated crash)".into()));
                // emit the bye in this generation, then move on
                self.proc.alive = false;
                let next_gen = self.proc.generation + 1;
                self.push(at + 1, Item::Flow { generation: next_gen, flow: Flow::Launch });
            }
            Flow::Launch => {
                if self.proc.alive {
                    return;
                }
                self.proc.generation += 1;
                self.proc.alive = true;
                self.proc.pid += 186;
                self.proc.instance = self.rng.hex(32);
                self.proc.connections.clear();
                self.proc.token.clear();
                let start = at + 2 * SEC;
                self.push_emit(start, Emit::Hello);
                self.push_flow(start, Flow::Boot);
            }
        }
    }

    fn diag(&mut self, at: u64, level: &str, code: &str, message: &str) {
        let m = DeviceMsg::Diag(msg::Diag {
            seq: 0,
            ts: at,
            level: level.into(),
            code: code.into(),
            message: message.into(),
            data: None,
        });
        self.msg(at, m);
    }

    fn api_headers(&mut self, host: &str, auth: bool, content_type: Option<&str>, len: Option<usize>) -> Headers {
        let mut hs = h(&[("Host", host)]);
        if auth && !self.proc.token.is_empty() {
            hs.push(("Authorization".into(), format!("Bearer {}", self.proc.token)));
        }
        if let Some(ct) = content_type {
            hs.push(("Content-Type".into(), ct.into()));
        }
        if let Some(l) = len {
            hs.push(("Content-Length".into(), l.to_string()));
        }
        hs.push(("X-App-Version".into(), APP_VERSION.into()));
        hs.push(("X-Request-Id".into(), self.rng.uuid()));
        hs.push(("Accept-Encoding".into(), "gzip".into()));
        hs.push(("User-Agent".into(), "okhttp/4.12.0".into()));
        hs
    }

    fn json_resp_headers(&mut self, at: u64, len: usize, gzip: bool) -> Headers {
        let mut hs = h(&[("date", &http_date(self.wall(at))), ("content-type", "application/json; charset=utf-8")]);
        if gzip {
            hs.push(("content-encoding".into(), "gzip".into()));
            hs.push(("vary".into(), "Accept-Encoding".into()));
        } else {
            hs.push(("content-length".into(), len.to_string()));
        }
        hs.push(("cache-control".into(), "no-cache, no-store".into()));
        hs.push(("x-request-id".into(), self.rng.uuid()));
        hs.push(("server".into(), "envoy".into()));
        hs
    }

    fn base_ex(&mut self, method: &'static str, url: String, thread: ThreadInfo, stack: Vec<StackFrame>) -> Ex {
        // the dev server speaks cleartext HTTP/1.1; the example.com hosts h2 over TLS
        let protocol = if url.starts_with("https") { "h2" } else { "http/1.1" };
        Ex {
            method,
            url,
            req_headers: Vec::new(),
            req_body: None,
            status: 200,
            reason: "OK",
            resp_headers: Vec::new(),
            resp_body: Bytes::new(),
            protocol,
            client: ("okhttp", Some("4.12.0")),
            thread,
            stack,
            setup_ms: (self.rng.range(8, 40), self.rng.range(25, 70), self.rng.range(40, 110)),
            send_ms: 2,
            wait_ms: self.rng.range(120, 420),
            receive_ms: self.rng.range(3, 25),
            fail: None,
            rule: None,
            call: None,
            hop: 0,
            last_hop: true,
            chunk: 16 * 1024,
            trailers: Vec::new(),
            grpc: None,
        }
    }

    /// gRPC (Phase 5): a stock lookup, one for a SKU the server does not know (a trailers-only
    /// NOT_FOUND), and a server stream of an order's progress.
    fn grpc(&mut self, at: u64) {
        let token = self.proc.token.clone();
        let call = |this: &mut Self, rpc: &str, req: Vec<u8>, stack: Vec<StackFrame>| {
            let thread = this.next_worker();
            let mut ex = this.base_ex("POST", format!("https://grpc.example.com/{rpc}"), thread, stack);
            ex.protocol = "h2";
            ex.reason = "";
            ex.client = ("grpc", Some("1.84.0"));
            ex.req_headers = h(&[
                ("authorization", &format!("Bearer {token}")),
                ("x-app-version", APP_VERSION),
                ("grpc-accept-encoding", "gzip"),
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("grpc-timeout", "4999872u"),
            ]);
            ex.req_body = Some(("application/grpc".into(), Bytes::from(content::grpc_frame(&req))));
            ex.setup_ms = (0, 0, 0);
            ex
        };
        let ok = |code: u32, name: &str, message: Option<&str>| {
            let mut trailers = vec![("grpc-status".to_string(), code.to_string())];
            if let Some(m) = message {
                trailers.push(("grpc-message".into(), m.into()));
            }
            (trailers, Some(msg::GrpcStatus { code, status: name.into(), message: message.map(Into::into) }))
        };
        let stub = "com.example.shop.inventory.v1.InventoryGrpc$InventoryBlockingStub";
        let repo = "com.example.shop.inventory.StockRepository";

        let wall = self.wall(at) as u64;
        let mut ex = call(
            self,
            "shop.inventory.v1.Inventory/GetStock",
            content::stock_request("sku_0042"),
            content::grpc_stack(stub, "getStock", repo, "stock", "StockRepository.kt", 57),
        );
        ex.resp_headers =
            h(&[("content-type", "application/grpc"), ("grpc-encoding", "identity"), ("grpc-accept-encoding", "gzip")]);
        ex.resp_body = Bytes::from(content::grpc_frame(&content::stock("sku_0042", 17, 2, wall)));
        (ex.trailers, ex.grpc) = ok(0, "OK", None);
        let end = self.start(at, ex);

        let mut ex = call(
            self,
            "shop.inventory.v1.Inventory/GetStock",
            content::stock_request("sku_9999"),
            content::grpc_stack(stub, "getStock", repo, "stock", "StockRepository.kt", 57),
        );
        // trailers only: the server answers with its status and no message
        ex.resp_headers = h(&[("content-type", "application/grpc")]);
        ex.wait_ms = self.rng.range(30, 60);
        (ex.trailers, ex.grpc) = ok(5, "NOT_FOUND", Some("sku sku_9999 is not in warehouse blr-1"));
        let end = self.start(end + 300 * MS, ex);

        let order = "ord_4214";
        let mut ex = call(
            self,
            "shop.orders.v1.Orders/Track",
            content::track_request(order),
            content::grpc_stack(
                "com.example.shop.orders.v1.OrdersGrpc$OrdersBlockingStub",
                "track",
                "com.example.shop.orders.OrderTracker",
                "follow",
                "OrderTracker.kt",
                33,
            ),
        );
        ex.resp_headers = h(&[("content-type", "application/grpc"), ("grpc-encoding", "identity")]);
        let mut body = Vec::new();
        let mut frame = 0;
        for (i, state) in ["picked_up", "nearby", "delivered"].iter().enumerate() {
            let f = content::grpc_frame(&content::track_update(
                order,
                state,
                wall + 2_000 * i as u64,
                12.9716 + 0.002 * i as f64,
                77.5946 - 0.003 * i as f64,
            ));
            frame = f.len();
            body.extend(f);
        }
        ex.resp_body = Bytes::from(body);
        // one update per chunk, two seconds apart
        ex.chunk = frame;
        ex.receive_ms = 4_000;
        (ex.trailers, ex.grpc) = ok(0, "OK", None);
        self.start(end + 400 * MS, ex);
    }

    fn login(&mut self, at: u64) {
        let form = "grant_type=password&client_id=shop-android&username=demo%40example.com&password=%E2%80%A2%E2%80%A2%E2%80%A2%E2%80%A2&scope=catalog+orders";
        let iat = self.wall(at) / 1000;
        let token = content::jwt(&mut self.rng, "user_1024", iat);
        let body =
            json!({"access_token": token, "token_type": "Bearer", "expires_in": 3600, "scope": "catalog orders"})
                .to_string();
        let thread = self.next_worker();
        let mut ex = self.base_ex("POST", format!("{AUTH}/oauth/token"), thread, content::login_stack());
        ex.req_headers =
            self.api_headers("auth.example.com", false, Some("application/x-www-form-urlencoded"), Some(form.len()));
        ex.req_body = Some(("application/x-www-form-urlencoded".into(), Bytes::from(form)));
        ex.resp_headers = self.json_resp_headers(at, body.len(), false);
        ex.resp_body = Bytes::from(body);
        let end = self.start(at, ex);
        self.proc.token = token;
        let session = format!("session_{}", self.rng.hex(32));
        self.push_flow(end + 300 * MS, Flow::Order { step: 0, polls: 0, session });
    }

    /// A shopping session: open it, browse, add to the cart, check out, then poll the order
    /// until it is confirmed (the demo's rule marks the payment captured on the last poll).
    fn order(&mut self, at: u64, step: u8, polls: u32, session: String) {
        if !self.recording {
            self.push_flow(at + 500 * MS, Flow::Order { step, polls, session });
            return;
        }
        let cart = format!("cart_{}", &session[8..20]);
        let order = format!("ord_{}", &session[8..24]);
        let thread = self.next_worker();
        let OrderStep { method, path, req, resp, api, caller, line } = match step {
            0 => OrderStep::new(
                "POST",
                "/api/v1/sessions",
                Some(json!({"appVersion": APP_VERSION, "platform": "android", "apiLevel": 36, "device": {"model": "Pixel 8", "manufacturer": "Google"}, "locale": "en-US"}).to_string()),
                json!({"ok": true, "sessionId": session, "userId": "user_1024", "config": {"pollIntervalMs": 1500, "maxPolls": 20, "features": {"newCheckout": true, "wishlist": true, "darkMode": false}, "endpoints": {"orderStatus": "/api/v1/orders/status"}}, "expiresAt": "2026-09-29T06:10:00.000Z"}).to_string(),
                "createSession",
                ("com.example.shop.session.SessionRepository", "start", "SessionRepository.kt"),
                62,
            ),
            1 => {
                let items: Vec<_> = [
                    ("p_1042", "Studio Headphones", 12900, 4.6),
                    ("p_1043", "Wireless Earbuds", 7900, 4.3),
                    ("p_1044", "Noise-Cancelling Headphones", 24900, 4.8),
                    ("p_1045", "Sport Earbuds", 5900, 4.1),
                    ("p_1046", "Kids Headphones", 2900, 4.4),
                    ("p_1047", "Travel Case", 1900, 4.7),
                ]
                .iter()
                .enumerate()
                .map(|(i, (id, name, cents, rating))| json!({"id": id, "name": name, "priceCents": cents, "currency": "USD", "rating": rating, "inStock": i != 3, "image": format!("{CDN}/img/products/{id}.png")}))
                .collect();
                OrderStep::new(
                    "GET",
                    "/api/v1/products?category=headphones&page=1",
                    None,
                    json!({"page": 1, "pageSize": 6, "total": 48, "items": items}).to_string(),
                    "products",
                    ("com.example.shop.catalog.CatalogRepository", "products", "CatalogRepository.kt"),
                    88,
                )
            }
            2 => OrderStep::new(
                "POST",
                "/api/v1/cart/items",
                Some(json!({"sessionId": session, "productId": "p_1042", "quantity": 1, "options": {"color": "black"}}).to_string()),
                json!({"ok": true, "cart": {"id": cart, "items": [{"productId": "p_1042", "quantity": 1, "priceCents": 12900}], "subtotalCents": 12900, "currency": "USD"}}).to_string(),
                "addToCart",
                ("com.example.shop.cart.CartRepository", "add", "CartRepository.kt"),
                141,
            ),
            3 => OrderStep::new(
                "POST",
                "/api/v1/checkout",
                Some(json!({"sessionId": session, "cartId": cart, "shipping": {"method": "standard", "address": {"line1": "1 Example Way", "city": "Springfield", "postalCode": "00000", "country": "US"}}, "payment": {"method": "card", "token": format!("tok_{}", self.rng.hex(16)), "last4": "4242"}}).to_string()),
                json!({"ok": true, "orderId": order, "status": "pending", "totalCents": 13900, "currency": "USD"}).to_string(),
                "checkout",
                ("com.example.shop.checkout.CheckoutRepository", "placeOrder", "CheckoutRepository.kt"),
                77,
            ),
            _ => {
                let confirmed = polls + 1 >= 9;
                let body = json!({"ok": true, "orderId": order, "status": if confirmed { "confirmed" } else { "processing" }, "payment": "pending", "items": 1, "eta": null, "updatedAt": "2026-09-29T05:04:51.000Z"}).to_string();
                OrderStep::new(
                    "GET",
                    "/api/v1/orders/status",
                    None,
                    body,
                    "orderStatus",
                    ("com.example.shop.orders.OrderStatusPoller", "poll", "OrderStatusPoller.kt"),
                    41,
                )
            }
        };
        let url = if step >= 4 { format!("{API}{path}?orderId={order}") } else { format!("{API}{path}") };
        let stack = content::app_stack(api, caller.0, caller.1, caller.2, line);
        let mut ex = self.base_ex(method, url, thread, stack);
        let req_len = req.as_ref().map(|r| r.len());
        ex.req_headers =
            self.api_headers(API_HOST, true, req.as_ref().map(|_| "application/json; charset=utf-8"), req_len);
        ex.req_body = req.map(|r| ("application/json; charset=utf-8".into(), Bytes::from(r)));
        ex.resp_headers = self.json_resp_headers(at, resp.len(), false);
        if step == 0 {
            ex.resp_headers.push(("set-cookie".into(), format!("sid={}; Path=/; HttpOnly", self.rng.hex(24))));
            ex.resp_headers.push(("set-cookie".into(), "region=us-east-1; Path=/".into()));
            ex.wait_ms = self.rng.range(1_600, 2_100);
        }
        if step == 2 {
            ex.wait_ms = self.rng.range(2_400, 3_600);
        }
        if step == 3 {
            ex.wait_ms = self.rng.range(1_900, 2_700);
        }
        // the demo's "force-paid" rule rewrites the 9th poll
        let mut done = false;
        if step >= 4 && polls + 1 >= 9 {
            let delivered = resp.replace("\"payment\":\"pending\"", "\"payment\":\"captured\"");
            let mut dh = ex.resp_headers.clone();
            dh.retain(|(n, _)| !n.eq_ignore_ascii_case("content-length") && !n.eq_ignore_ascii_case("cache-control"));
            dh.push(("Content-Length".into(), delivered.len().to_string()));
            dh.push(("Cache-Control".into(), "no-store".into()));
            ex.rule = Some(RuleFx {
                changes: vec![
                    Change { op: "body_edit".into(), matches: Some(1), ..Default::default() },
                    Change {
                        op: "header_set".into(),
                        name: Some("Cache-Control".into()),
                        value: Some("no-store".into()),
                        old: vec!["no-cache, no-store".into()],
                        reason: Some("cache_guard".into()),
                        ..Default::default()
                    },
                ],
                delivered_headers: dh,
                delivered_body: Bytes::from(delivered),
            });
            done = true;
        }
        ex.resp_body = Bytes::from(resp);
        let end = self.start(at, ex);
        let next = match step {
            0..=3 => Some((end + 250 * MS, Flow::Order { step: step + 1, polls, session })),
            _ if done => {
                self.sessions += 1;
                // start another session a little later so the live demo keeps flowing
                let session = format!("session_{}", self.rng.hex(32));
                Some((end + 12 * SEC, Flow::Order { step: 0, polls: 0, session }))
            }
            _ => Some((at + 1_500 * MS, Flow::Order { step: 4, polls: polls + 1, session })),
        };
        if let Some((t, f)) = next {
            self.push_flow(t, f);
        }
    }

    fn telemetry(&mut self, at: u64) {
        let events: Vec<_> = (0..self.rng.range(3, 9))
            .map(|i| json!({"name": (["screen_view", "product_view", "add_to_cart", "begin_checkout", "purchase"][(i % 5) as usize]), "ts": self.wall(at) - (i as i64) * 700, "props": {"session": self.proc.pid, "n": i}}))
            .collect();
        let req = json!({"batch": self.rng.hex(8), "events": events}).to_string();
        let resp = json!({"accepted": true}).to_string();
        let mut ex = self.base_ex(
            "POST",
            format!("{TELEMETRY}/v1/events"),
            content::thread("Telemetry-Uploader", 71, "call"),
            content::telemetry_stack(),
        );
        ex.req_headers =
            self.api_headers("telemetry.example.com", true, Some("application/json; charset=utf-8"), Some(req.len()));
        ex.req_body = Some(("application/json; charset=utf-8".into(), Bytes::from(req)));
        ex.resp_headers = self.json_resp_headers(at, resp.len(), false);
        ex.resp_body = Bytes::from(resp);
        ex.wait_ms = self.rng.range(900, 1_500);
        let txn = self.next_txn;
        self.proc.telemetry_txns.push(txn);
        self.start(at, ex);
    }

    /// The notifications poll every 2 s, gzip-encoded.
    fn notifications(&mut self, at: u64) {
        let items: Vec<_> = (0..24)
            .map(|i| json!({"id": format!("ntf_{i:02}"), "kind": (["order_update", "price_drop", "back_in_stock"][i % 3]), "read": i >= 3, "ageMin": 5 + (i * 7) % 90}))
            .collect();
        let body = json!({"ok": true, "unread": 3, "window": "24h", "items": items, "nextPollMs": 2000}).to_string();
        let wire = gzip(body.as_bytes());
        let worker = self.next_worker();
        let mut ex = self.base_ex(
            "GET",
            format!("{API}/api/v1/notifications"),
            worker,
            content::app_stack(
                "notifications",
                "com.example.shop.notifications.NotificationPoller",
                "tick",
                "NotificationPoller.kt",
                29,
            ),
        );
        ex.req_headers = self.api_headers(API_HOST, true, None, None);
        ex.resp_headers = self.json_resp_headers(at, wire.len(), true);
        ex.resp_body = wire;
        ex.wait_ms = self.rng.range(1_300, 1_750);
        self.start(at, ex);
    }

    fn one_off(&mut self, at: u64, o: OneOff) {
        match o {
            OneOff::Avatar => {
                let png = content::avatar_png(self.rng.next_u64());
                let mut ex = self.base_ex(
                    "GET",
                    format!("{CDN}/avatars/user_1024.png"),
                    content::thread("glide-source-thread-0", 64, "call"),
                    content::glide_stack(),
                );
                ex.req_headers = self.api_headers("cdn.example.com", false, None, None);
                ex.resp_headers = h(&[
                    ("date", &http_date(self.wall(at))),
                    ("content-type", "image/png"),
                    ("content-length", &png.len().to_string()),
                    ("cache-control", "public, max-age=86400"),
                    ("etag", "\"a1f9c3\""),
                    ("age", "312"),
                ]);
                ex.resp_body = png;
                ex.receive_ms = 40;
                ex.chunk = 4096;
                self.start(at, ex);
            }
            OneOff::Redirect => {
                let call = self.next_call;
                self.next_call += 1;
                let thread = self.next_worker();
                let stack = content::app_stack(
                    "remoteConfig",
                    "com.example.shop.config.RemoteConfig",
                    "refresh",
                    "RemoteConfig.kt",
                    48,
                );
                let mut first =
                    self.base_ex("GET", "http://cdn.example.com/app/config".into(), thread.clone(), stack.clone());
                first.req_headers = self.api_headers("cdn.example.com", false, None, None);
                first.req_headers.insert(1, ("Connection".into(), "Keep-Alive".into()));
                first.status = 302;
                first.reason = "Found";
                first.resp_headers = h(&[
                    ("Date", &http_date(self.wall(at))),
                    ("Location", "https://cdn.example.com/app/v2/config.json"),
                    ("Content-Length", "0"),
                    ("Server", "nginx"),
                ]);
                first.call = Some(call);
                first.last_hop = false;
                first.wait_ms = 60;
                let end = self.start(at, first);
                let cfg = json!({"version": 42, "minAppVersion": "3.6.0", "flags": {"newCheckout": true, "wishlistSync": false}, "rollout": 0.25}).to_string();
                let mut second = self.base_ex("GET", format!("{CDN}/app/v2/config.json"), thread, stack);
                second.req_headers = self.api_headers("cdn.example.com", false, None, None);
                second.resp_headers = self.json_resp_headers(end, cfg.len(), false);
                second.resp_body = Bytes::from(cfg);
                second.call = Some(call);
                second.hop = 1;
                self.start(end + MS, second);
            }
            OneOff::Protobuf => {
                let wall = self.wall(at) as u64;
                let body = content::metrics_batch(&mut self.rng, wall);
                let mut ex = self.base_ex(
                    "POST",
                    format!("{TELEMETRY}/v1/metrics"),
                    content::thread("Telemetry-Uploader", 71, "call"),
                    content::telemetry_stack(),
                );
                ex.req_headers =
                    self.api_headers("telemetry.example.com", true, Some("application/x-protobuf"), Some(body.len()));
                ex.req_body = Some(("application/x-protobuf".into(), body));
                let ack = content::metrics_ack();
                ex.resp_headers = h(&[
                    ("date", &http_date(self.wall(at))),
                    ("content-type", "application/x-protobuf"),
                    ("content-length", &ack.len().to_string()),
                ]);
                ex.resp_body = ack;
                self.start(at, ex);
            }
            OneOff::Socket => self.socket(at),
            OneOff::Grpc => self.grpc(at),
            OneOff::NotFound => {
                let body = json!({"ok": false, "error": {"code": "NOT_FOUND", "message": "asset 'promo-banner-v3.json' does not exist", "requestId": self.rng.uuid()}}).to_string();
                let thread = self.next_worker();
                let mut ex = self.base_ex(
                    "GET",
                    format!("{API}/api/v1/assets/promo-banner-v3.json"),
                    thread,
                    content::app_stack("asset", "com.example.shop.promo.PromoBanner", "load", "PromoBanner.kt", 73),
                );
                ex.req_headers = self.api_headers(API_HOST, true, None, None);
                ex.status = 404;
                ex.reason = "Not Found";
                ex.resp_headers = self.json_resp_headers(at, body.len(), false);
                ex.resp_body = Bytes::from(body);
                self.start(at, ex);
            }
            OneOff::Upload => {
                let boundary = "----traffic-police-7d9f2c";
                let png = content::avatar_png(9);
                let mut body = Vec::new();
                body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"review\"\r\nContent-Type: application/json; charset=utf-8\r\n\r\n").as_bytes());
                body.extend_from_slice(json!({"productId": "p_1042", "rating": 5, "title": "Great sound", "text": "Comfortable for long calls, and the battery lasts all day.", "writtenAt": self.wall(at)}).to_string().as_bytes());
                body.extend_from_slice(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"photo\"; filename=\"photo.png\"\r\nContent-Type: image/png\r\n\r\n").as_bytes());
                body.extend_from_slice(&png);
                body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
                let ct = format!("multipart/form-data; boundary={boundary}");
                let resp = json!({"ok": true, "reviewId": format!("rev_{}", self.rng.hex(16)), "status": "pending_moderation", "photos": 1}).to_string();
                let thread = self.next_worker();
                let mut ex = self.base_ex(
                    "POST",
                    format!("{API}/api/v1/reviews"),
                    thread,
                    content::app_stack(
                        "postReview",
                        "com.example.shop.reviews.ReviewUploader",
                        "upload",
                        "ReviewUploader.kt",
                        55,
                    ),
                );
                ex.req_headers = self.api_headers(API_HOST, true, Some(&ct), Some(body.len()));
                ex.req_body = Some((ct, Bytes::from(body)));
                ex.send_ms = 180;
                ex.resp_headers = self.json_resp_headers(at, resp.len(), false);
                ex.resp_body = Bytes::from(resp);
                ex.chunk = 8192;
                self.start(at, ex);
            }
            OneOff::ServerError => {
                let req = json!({"reason": "changed_mind", "refund": "original_payment"}).to_string();
                let thread = self.next_worker();
                let mut ex = self.base_ex(
                    "POST",
                    format!("{API}/api/v1/orders/ord_4b6707958a104c9a/cancel"),
                    thread,
                    content::app_stack(
                        "cancelOrder",
                        "com.example.shop.orders.OrderRepository",
                        "cancel",
                        "OrderRepository.kt",
                        203,
                    ),
                );
                ex.req_headers =
                    self.api_headers(API_HOST, true, Some("application/json; charset=utf-8"), Some(req.len()));
                ex.req_body = Some(("application/json; charset=utf-8".into(), Bytes::from(req)));
                ex.status = 500;
                ex.reason = "Internal Server Error";
                ex.resp_headers = h(&[
                    ("date", &http_date(self.wall(at))),
                    ("content-type", "text/html; charset=utf-8"),
                    ("content-length", &content::ERROR_PAGE.len().to_string()),
                    ("server", "envoy"),
                ]);
                ex.resp_body = Bytes::from_static(content::ERROR_PAGE.as_bytes());
                self.start(at, ex);
            }
            OneOff::Volley => {
                let body = json!({"ip": "203.0.113.7", "country": "US", "region": "CA", "city": "Springfield", "currency": "USD", "timezone": "America/Los_Angeles"}).to_string();
                let mut ex = self.base_ex(
                    "GET",
                    "https://geo.example.com/v1/lookup?fields=country,region,currency".into(),
                    content::thread("Thread-7", 44, "huc"),
                    content::volley_stack(),
                );
                ex.client = ("huc", None);
                ex.protocol = "http/1.1";
                ex.req_headers = h(&[
                    ("User-Agent", "Dalvik/2.1.0 (Linux; U; Android 16; Pixel 8 Build/BP41.250725.006)"),
                    ("Host", "geo.example.com"),
                    ("Connection", "Keep-Alive"),
                    ("Accept-Encoding", "gzip"),
                ]);
                ex.resp_headers = h(&[
                    ("Date", &http_date(self.wall(at))),
                    ("Content-Type", "application/json"),
                    ("Content-Length", &body.len().to_string()),
                    ("Connection", "keep-alive"),
                ]);
                ex.resp_body = Bytes::from(body);
                self.start(at, ex);
            }
            OneOff::Timeout => {
                let thread = self.next_worker();
                let mut ex = self.base_ex(
                    "GET",
                    format!("{API}/api/v1/recommendations?productId=p_1042"),
                    thread,
                    content::app_stack(
                        "recommendations",
                        "com.example.shop.catalog.Recommendations",
                        "fetch",
                        "Recommendations.kt",
                        36,
                    ),
                );
                ex.req_headers = self.api_headers(API_HOST, true, None, None);
                ex.fail = Some(Fail {
                    phase: "response_headers",
                    class: "java.net.SocketTimeoutException",
                    message: "timeout",
                    after_ms: 10_000,
                    canceled: false,
                });
                self.start(at, ex);
            }
            OneOff::Canceled => {
                let thread = self.next_worker();
                let mut ex = self.base_ex(
                    "GET",
                    format!("{CDN}/fonts/fonts.json"),
                    thread,
                    content::app_stack(
                        "fonts",
                        "com.example.shop.ui.FontPrefetcher",
                        "prefetch",
                        "FontPrefetcher.kt",
                        22,
                    ),
                );
                ex.req_headers = self.api_headers("cdn.example.com", false, None, None);
                ex.fail = Some(Fail {
                    phase: "response_headers",
                    class: "java.io.IOException",
                    message: "Canceled",
                    after_ms: 380,
                    canceled: true,
                });
                self.start(at, ex);
            }
            OneOff::Download => {
                let data = Bytes::from(self.rng.bytes(5 * 1024 * 1024));
                let mut ex = self.base_ex(
                    "GET",
                    format!("{CDN}/packs/catalog-offline-v12.bin"),
                    content::thread("DownloadWorker-1", 81, "call"),
                    content::download_stack(),
                );
                ex.req_headers = self.api_headers("cdn.example.com", false, None, None);
                ex.resp_headers = h(&[
                    ("date", &http_date(self.wall(at))),
                    ("content-type", "application/octet-stream"),
                    ("content-length", &data.len().to_string()),
                    ("accept-ranges", "bytes"),
                    ("etag", "\"c12-5242880\""),
                    ("cache-control", "public, max-age=604800"),
                ]);
                ex.resp_body = data;
                ex.wait_ms = 180;
                ex.receive_ms = 6_200;
                ex.chunk = 64 * 1024;
                self.start(at, ex);
            }
            OneOff::Dropped => {
                let txns: Vec<u64> = self.proc.telemetry_txns.iter().rev().take(1).copied().collect();
                let m = DeviceMsg::Dropped(msg::Dropped {
                    seq: 0,
                    ts: at,
                    events: 12,
                    bytes: 48_112,
                    txns,
                    txns_truncated: false,
                });
                self.msg(at, m);
            }
            OneOff::Burst => {
                self.burst_until = at + 2_500 * MS;
            }
        }
    }

    // --- one exchange --------------------------------------------------------------------------

    fn conn_for(&mut self, ex: &Ex) -> (Conn, bool) {
        let host = ex.host().to_string();
        let https = ex.url.starts_with("https");
        let (id, new) = match self.proc.connections.get(&host) {
            Some((id, _)) => (id.clone(), false),
            None => {
                let id = format!("c-{}", self.proc.next_conn);
                self.proc.next_conn += 1;
                self.proc.connections.insert(host.clone(), (id.clone(), https));
                (id, true)
            }
        };
        // localhost:8080 is the dev server; the example.com hosts get documentation addresses (RFC 5737)
        let (name, port) = match host.rsplit_once(':').and_then(|(n, p)| Some((n, p.parse::<u16>().ok()?))) {
            Some((n, p)) => (n.to_string(), p),
            None => (host.clone(), if https { 443 } else { 80 }),
        };
        let ip = match name.as_str() {
            "localhost" => "127.0.0.1",
            "auth.example.com" => "203.0.113.10",
            "cdn.example.com" => "198.51.100.24",
            "telemetry.example.com" => "203.0.113.42",
            "geo.example.com" => "192.0.2.77",
            _ => "192.0.2.1",
        };
        let domain = name.split_once('.').map_or(name.as_str(), |(_, d)| d).to_string();
        let tls = https.then(|| Tls {
            version: Some("TLSv1.3".into()),
            cipher: Some("TLS_AES_128_GCM_SHA256".into()),
            peer: vec![
                Cert {
                    subject: Some(format!("CN=*.{domain}")),
                    issuer: Some("CN=Example Issuing CA 1, O=Example Trust, C=US".into()),
                    not_before_ms: Some(1_785_000_000_000),
                    not_after_ms: Some(1_792_776_000_000),
                    sha256: Some(format!("{:064x}", u128::from(ip.len() as u8) * 0x9e37_79b9_7f4a_7c15_u128)),
                    san: vec![format!("*.{domain}"), name.clone()],
                },
                Cert {
                    subject: Some("CN=Example Issuing CA 1, O=Example Trust, C=US".into()),
                    issuer: Some("CN=Example Root CA, O=Example Trust, C=US".into()),
                    ..Default::default()
                },
            ],
        });
        let conn = Conn {
            id: Some(id),
            reused: Some(!new),
            protocol: Some(ex.protocol.into()),
            remote: Some(Addr { ip: ip.into(), port }),
            proxy: Some("DIRECT".into()),
            tls,
        };
        (conn, new)
    }

    fn chunks(&mut self, txn: u64, dir: BodyDir, body: &Bytes, from: u64, dur: u64, chunk: usize) -> u64 {
        if body.is_empty() {
            return from;
        }
        let n = body.len().div_ceil(chunk.max(1));
        for i in 0..n {
            let s = i * chunk;
            let e = (s + chunk).min(body.len());
            let at = from + dur * (i as u64 + 1) / n as u64;
            self.push_emit(at, Emit::Chunk { txn, dir, offset: s as u64, data: body.slice(s..e) });
        }
        from + dur
    }

    /// Schedule one exchange starting at `at`; returns when it ends.
    /// A WebSocket (PROTOCOL.md §7.1 `ws`): the handshake, a subscription, order updates every
    /// few seconds, and the app closing it when the user leaves the orders screen.
    fn socket(&mut self, at: u64) {
        let txn = self.next_txn;
        self.next_txn += 1;
        self.next_call += 1;
        let call = self.next_call - 1;
        let url = format!("{API}/ws/orders");
        let thread = content::thread("main", 2, "call");
        let stack = content::socket_stack("com.example.shop.orders.LiveOrders", "connect", "LiveOrders.kt", 41);
        let headers = self.api_headers(API_HOST, true, None, None);
        self.msg(
            at,
            DeviceMsg::Req(msg::Req {
                seq: 0,
                ts: at,
                txn,
                call: Some(call),
                hop: 0,
                method: "GET".into(),
                url,
                headers,
                client: Some(ClientInfo { kind: "okhttp".into(), version: Some("4.12.0".into()) }),
                thread: Some(thread),
                stack,
                stack_truncated: false,
                body: None,
                marks: vec![("call_start".into(), at)],
                conn: None,
            }),
        );
        let open = at + 42 * MS;
        self.msg(
            open,
            DeviceMsg::Resp(msg::Resp {
                seq: 0,
                ts: open,
                txn,
                status: 101,
                message: "Switching Protocols".into(),
                protocol: Some("http/1.1".into()),
                headers: h(&[
                    ("Upgrade", "websocket"),
                    ("Connection", "Upgrade"),
                    ("Sec-WebSocket-Accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
                ]),
                conn: None,
            }),
        );
        let ws = |ts: u64, out: bool, op: &str, text: Option<String>, code: Option<u16>, reason: Option<&str>| {
            DeviceMsg::Ws(msg::Ws {
                seq: 0,
                ts,
                txn,
                dir: if out { "out" } else { "in" }.into(),
                op: op.into(),
                size: text.as_ref().map_or(0, |t| t.len() as u64),
                text,
                base64: None,
                truncated: false,
                code,
                reason: reason.map(Into::into),
            })
        };
        let sub = json!({"type": "subscribe", "channel": "orders", "userId": "user_1024"}).to_string();
        self.msg(open + 5 * MS, ws(open + 5 * MS, true, "text", Some(sub), None, None));
        let ack = json!({"type": "subscribed", "channel": "orders"}).to_string();
        self.msg(open + 61 * MS, ws(open + 61 * MS, false, "text", Some(ack), None, None));
        let mut t = open;
        for (i, state) in ["packed", "shipped", "out_for_delivery", "delivered"].iter().enumerate() {
            t += 3_000 * MS + self.rng.range(0, 400) * MS;
            let update = json!({"type": "order_update", "orderId": format!("ord_{:04}", 4210 + i), "state": state, "at": self.wall(t)}).to_string();
            self.msg(t, ws(t, false, "text", Some(update), None, None));
        }
        t += 1_200 * MS;
        self.msg(t, ws(t, true, "close", None, Some(1000), Some("left the orders screen")));
        t += 35 * MS;
        self.msg(t, ws(t, false, "close", None, Some(1000), Some("bye")));
        self.msg(t + MS, DeviceMsg::Done(msg::Done { seq: 0, ts: t + MS, txn, trailers: Vec::new(), grpc: None }));
    }

    fn start(&mut self, at: u64, ex: Ex) -> u64 {
        let txn = self.next_txn;
        self.next_txn += 1;
        let call = ex.call.unwrap_or_else(|| {
            self.next_call += 1;
            self.next_call - 1
        });
        let (conn, new_conn) = self.conn_for(&ex);
        let mut t = at;
        let mut marks: Vec<(String, u64)> = Vec::new();
        if ex.hop == 0 {
            marks.push(("call_start".into(), t));
            t += MS / 2;
        }
        if new_conn {
            let (dns, connect, tls) = ex.setup_ms;
            marks.push(("dns_start".into(), t));
            t += dns * MS;
            marks.push(("dns_end".into(), t));
            marks.push(("connect_start".into(), t));
            t += connect * MS;
            if ex.url.starts_with("https") {
                marks.push(("tls_start".into(), t));
                t += tls * MS;
                marks.push(("tls_end".into(), t));
            }
            marks.push(("connect_end".into(), t));
        }
        marks.push(("conn_acquired".into(), t));
        t += MS / 5;
        let body_info = ex.req_body.as_ref().map(|(ct, b)| ReqBodyInfo {
            length: b.len() as i64,
            content_type: Some(ct.clone()),
            one_shot: false,
            duplex: false,
        });
        self.msg(
            t,
            DeviceMsg::Req(msg::Req {
                seq: 0,
                ts: t,
                txn,
                call: Some(call),
                hop: ex.hop,
                method: ex.method.into(),
                url: ex.url.clone(),
                headers: ex.req_headers.clone(),
                client: Some(ClientInfo { kind: ex.client.0.into(), version: ex.client.1.map(Into::into) }),
                thread: Some(ex.thread.clone()),
                stack: ex.stack.clone(),
                stack_truncated: false,
                body: body_info,
                marks,
                conn: Some(conn.clone()),
            }),
        );
        self.mark(t, txn, "req_headers_start");
        t += MS / 10;
        self.mark(t, txn, "req_headers_end");
        if let Some((_, body)) = &ex.req_body {
            self.mark(t, txn, "req_body_start");
            let body = body.clone();
            t = self.chunks(txn, BodyDir::Request, &body, t, ex.send_ms * MS, ex.chunk);
            let len = body.len() as u64;
            self.msg(
                t,
                DeviceMsg::BodyEnd(msg::BodyEnd {
                    seq: 0,
                    ts: t,
                    txn,
                    dir: BodyDir::Request,
                    bytes: len,
                    captured: len,
                    state: "complete".into(),
                    decoded: false,
                }),
            );
            self.mark(t, txn, "req_body_end");
        }
        if let Some(fail) = &ex.fail {
            let at_fail = if fail.canceled { at + fail.after_ms * MS } else { t + fail.after_ms * MS };
            self.msg(
                at_fail,
                DeviceMsg::Fail(msg::Fail {
                    seq: 0,
                    ts: at_fail,
                    txn,
                    phase: Some(fail.phase.into()),
                    canceled: fail.canceled,
                    simulated: false,
                    error: ErrorInfo {
                        class: fail.class.into(),
                        message: Some(fail.message.into()),
                        causes: Vec::new(),
                    },
                    conn: None,
                    trailers: Vec::new(),
                    grpc: ex.grpc.clone(),
                }),
            );
            if ex.last_hop {
                self.mark(at_fail, txn, "call_end");
            }
            return at_fail;
        }
        t += ex.wait_ms * MS;
        self.mark(t, txn, "resp_headers_start");
        let t_resp = t + MS / 3;
        self.msg(
            t_resp,
            DeviceMsg::Resp(msg::Resp {
                seq: 0,
                ts: t_resp,
                txn,
                status: ex.status,
                message: ex.reason.into(),
                protocol: Some(ex.protocol.into()),
                headers: ex.resp_headers.clone(),
                conn: None,
            }),
        );
        self.mark(t_resp, txn, "resp_headers_end");
        t = t_resp;
        if let Some(rule) = &ex.rule {
            let rule = rule.clone();
            self.msg(
                t,
                DeviceMsg::Rule(msg::RuleApplied {
                    seq: 0,
                    ts: t,
                    txn,
                    rules: vec![RuleRef { id: "force-paid".into(), name: Some("Force payment captured".into()) }],
                    changes: rule.changes.clone(),
                    delivered: Some(DeliveredResponse {
                        status: ex.status,
                        message: ex.reason.into(),
                        headers: rule.delivered_headers.clone(),
                    }),
                }),
            );
        }
        if ex.resp_body.is_empty() {
            self.msg(
                t,
                DeviceMsg::BodyEnd(msg::BodyEnd {
                    seq: 0,
                    ts: t,
                    txn,
                    dir: BodyDir::Response,
                    bytes: 0,
                    captured: 0,
                    state: "none".into(),
                    decoded: false,
                }),
            );
        } else {
            self.mark(t, txn, "resp_body_start");
            let body = ex.resp_body.clone();
            t = self.chunks(txn, BodyDir::Response, &body, t, ex.receive_ms * MS, ex.chunk);
            let len = body.len() as u64;
            self.msg(
                t,
                DeviceMsg::BodyEnd(msg::BodyEnd {
                    seq: 0,
                    ts: t,
                    txn,
                    dir: BodyDir::Response,
                    bytes: len,
                    captured: len,
                    state: "complete".into(),
                    decoded: false,
                }),
            );
            if let Some(rule) = &ex.rule {
                let d = rule.delivered_body.clone();
                self.chunks(txn, BodyDir::Delivered, &d, t, 0, ex.chunk);
                let dl = d.len() as u64;
                self.msg(
                    t,
                    DeviceMsg::BodyEnd(msg::BodyEnd {
                        seq: 0,
                        ts: t,
                        txn,
                        dir: BodyDir::Delivered,
                        bytes: dl,
                        captured: dl,
                        state: "complete".into(),
                        decoded: false,
                    }),
                );
            }
            self.mark(t, txn, "resp_body_end");
        }
        t += MS / 5;
        self.msg(
            t,
            DeviceMsg::Done(msg::Done { seq: 0, ts: t, txn, trailers: ex.trailers.clone(), grpc: ex.grpc.clone() }),
        );
        if ex.last_hop {
            self.mark(t, txn, "call_end");
        }
        t
    }
}

// ---------------------------------------------------------------------------------------------
// Session: device bytes -> SessionEvents through the real decoder
// ---------------------------------------------------------------------------------------------

/// A demo "connection": runs the device and decodes its bytes like a live socket.
pub struct DemoSession {
    device: DemoDevice,
    decoder: Decoder,
    normalizer: Normalizer,
    ids: SourceIds,
    current: Option<u32>,
    buf: BytesMut,
    /// Keeps the generated stream, so a demo session can be saved like a real one.
    pub log: Option<std::sync::Arc<dyn traffic_police_core::session::StreamSink>>,
}

impl DemoSession {
    pub fn new(cfg: DemoConfig, ids: SourceIds) -> Self {
        DemoSession {
            device: DemoDevice::new(cfg),
            decoder: Decoder::new(),
            normalizer: Normalizer::new(0),
            ids,
            current: None,
            log: None,
            buf: BytesMut::new(),
        }
    }

    pub fn set_recording(&mut self, on: bool) {
        self.device.set_recording(on);
    }

    pub fn elapsed(&self) -> u64 {
        self.device.elapsed()
    }

    /// The device clock (`ts`) at `elapsed_ns` after the start.
    pub fn clock_at(elapsed_ns: u64) -> u64 {
        BASE_TS + elapsed_ns
    }

    /// Advance to `elapsed_ns` after the start and return the new events.
    pub fn advance(&mut self, elapsed_ns: u64) -> Vec<SessionEvent> {
        self.buf.clear();
        self.device.run_until(elapsed_ns, &mut self.buf);
        self.decoder.push(&self.buf);
        let mut out = Vec::new();
        loop {
            let frame = match self.decoder.next_frame() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("demo produced a bad frame: {e}");
                    if e.is_fatal() {
                        break;
                    }
                    continue;
                }
            };
            // the hello goes into the log's source record; everything else as a device frame
            let hello_json = match &frame {
                Frame::Json(j) if j.starts_with(br#"{"t":"hello""#) => Some(j.clone()),
                _ => None,
            };
            if hello_json.is_none()
                && let (Some(log), Some(id)) = (&self.log, self.current)
            {
                log.frame(id, &frame);
            }
            match self.normalizer.frame(frame, &mut out) {
                Ok(Some(Control::Hello(hello))) => {
                    let id = self.ids.next();
                    self.current = Some(id);
                    self.normalizer = Normalizer::new(id);
                    let info = SourceInfo::from_hello(id, &hello, "Pixel 8 [demo]".into(), Some("demo".into()));
                    if let (Some(log), Some(raw)) = (&self.log, &hello_json) {
                        let device = traffic_police_core::session::DeviceRecord {
                            label: "Pixel 8 [demo]".into(),
                            serial: Some("demo".into()),
                        };
                        log.source(id, &device, raw, false);
                    }
                    out.push(SessionEvent::SourceUp(Box::new(info)));
                }
                Ok(Some(Control::Bye(bye))) => {
                    if let Some(id) = self.current.take() {
                        let reason = bye.message.unwrap_or(bye.reason);
                        if let Some(log) = &self.log {
                            log.source_end(id, self.device.now, &reason);
                        }
                        out.push(SessionEvent::SourceDown { source: id, at: self.device.now, reason });
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("demo produced an undecodable message: {e}"),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use traffic_police_core::model::TxnState;
    use traffic_police_core::store::SessionStore;

    fn run(secs: u64, cfg: DemoConfig) -> SessionStore {
        let mut store = SessionStore::new();
        let mut s = DemoSession::new(cfg, store.source_ids());
        // advance in steps, as the real-time runner does
        for step in 1..=(secs * 10) {
            store.apply_all(s.advance(step * 100 * MS));
        }
        store
    }

    #[test]
    fn produces_the_documented_scenarios() {
        let store = run(40, DemoConfig::default());
        let txns: Vec<_> = store.txns().iter().map(|t| t.as_ref()).collect();
        let has = |pred: &dyn Fn(&traffic_police_core::model::Transaction) -> bool| txns.iter().any(|t| pred(t));
        assert!(txns.len() > 40, "only {} transactions", txns.len());
        assert!(has(&|t| t.url.path == "/api/v1/sessions" && t.status() == Some(200)));
        assert!(has(&|t| t.url.path == "/api/v1/orders/status" && t.rule_modified()));
        assert!(has(&|t| t.url.host == "localhost" && t.url.port == Some(8080)));
        assert!(has(&|t| t.status() == Some(302)) && has(&|t| t.hop == 1));
        assert!(has(&|t| t.status() == Some(404)) && has(&|t| t.status() == Some(500)));
        assert!(has(&|t| t.failure.as_ref().is_some_and(|f| f.class.ends_with("SocketTimeoutException"))));
        assert!(has(&|t| t.failure.as_ref().is_some_and(|f| f.canceled)));
        assert!(has(&|t| t.type_label() == "png" && t.resp_body.captured > 1000));
        assert!(has(&|t| t.type_label() == "protobuf"));
        assert!(has(&|t| t.type_label() == "binary"
            && t.resp_body.total == 5 * 1024 * 1024
            && t.state == TxnState::Complete));
        assert!(has(&|t| t
            .response_headers()
            .is_some_and(|h| h.iter().any(|(n, v)| n == "content-encoding" && v == "gzip"))));
        assert!(has(&|t| t.client.as_ref().is_some_and(|c| c.kind == "huc")));
        assert!(has(&|t| t.request_content_type().is_some_and(|c| c.starts_with("multipart/"))));
        assert!(store.lanes().len() >= 5);
        assert_eq!(store.stats().dropped_events, 12);
        assert!(store.diagnostics().len() >= 2);
        assert!(store.traffic().has_app_data());
        assert_eq!(txns.iter().filter(|t| t.placeholder).count(), 0);
    }

    #[test]
    fn restart_creates_a_second_source() {
        let store = run(30, DemoConfig { restart_after_ns: Some(20 * SEC), ..DemoConfig::default() });
        let sources: Vec<_> = store.sources().collect();
        assert_eq!(sources.len(), 2);
        assert!(sources[0].ended.is_some());
        assert_ne!(sources[0].pid, sources[1].pid);
    }

    #[test]
    fn same_seed_same_session() {
        let a = run(15, DemoConfig::default());
        let b = run(15, DemoConfig::default());
        assert_eq!(a.len(), b.len());
        assert_eq!(a.txn(3).url, b.txn(3).url);
    }
}
