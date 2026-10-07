//! JSON messages (PROTOCOL.md §4, §7, §8).
//!
//! Receivers ignore unknown fields (serde's default) and unknown message types
//! ([`DeviceMsg::Unknown`], [`HostMsg::Unknown`]). Enum-like fields that a newer runtime might
//! extend (body states, failure phases, reasons) are kept as strings and interpreted by the core.

use std::collections::BTreeMap;

use bytes::BytesMut;
use serde::{Deserialize, Serialize};

use crate::frame::{self, BodyDir};

/// Ordered header list; duplicates kept, name case as sent.
pub type Headers = Vec<(String, String)>;

// ---------------------------------------------------------------------------------------------
// Device -> host
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DeviceMsg {
    Hello(Hello),
    Replay(Replay),
    Req(Req),
    Resp(Resp),
    BodyEnd(BodyEnd),
    Prog(Prog),
    Mark(Mark),
    Done(Done),
    Fail(Fail),
    Rule(RuleApplied),
    /// A WebSocket message on a transaction's socket.
    Ws(Ws),
    Dropped(Dropped),
    Traffic(Traffic),
    Diag(Diag),
    RulesAck(RulesAck),
    ConfigAck(ConfigAck),
    Pong(Pong),
    Bye(Bye),
    /// A message type from a newer protocol revision.
    #[serde(other)]
    Unknown,
}

impl DeviceMsg {
    /// The event's device time, for events (control messages have none).
    pub fn ts(&self) -> Option<u64> {
        Some(match self {
            DeviceMsg::Req(m) => m.ts,
            DeviceMsg::Resp(m) => m.ts,
            DeviceMsg::BodyEnd(m) => m.ts,
            DeviceMsg::Prog(m) => m.ts,
            DeviceMsg::Mark(m) => m.ts,
            DeviceMsg::Done(m) => m.ts,
            DeviceMsg::Fail(m) => m.ts,
            DeviceMsg::Rule(m) => m.ts,
            DeviceMsg::Ws(m) => m.ts,
            DeviceMsg::Dropped(m) => m.ts,
            DeviceMsg::Traffic(m) => m.ts,
            DeviceMsg::Diag(m) => m.ts,
            _ => return None,
        })
    }

    /// The event sequence number, for events (control messages have none).
    pub fn seq(&self) -> Option<u64> {
        Some(match self {
            DeviceMsg::Req(m) => m.seq,
            DeviceMsg::Resp(m) => m.seq,
            DeviceMsg::BodyEnd(m) => m.seq,
            DeviceMsg::Prog(m) => m.seq,
            DeviceMsg::Mark(m) => m.seq,
            DeviceMsg::Done(m) => m.seq,
            DeviceMsg::Fail(m) => m.seq,
            DeviceMsg::Rule(m) => m.seq,
            DeviceMsg::Ws(m) => m.seq,
            DeviceMsg::Dropped(m) => m.seq,
            DeviceMsg::Traffic(m) => m.seq,
            DeviceMsg::Diag(m) => m.seq,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub runtime: RuntimeInfo,
    pub instance: String,
    pub app: AppInfo,
    pub device: DeviceInfo,
    pub clock: Clock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub clients: BTreeMap<String, Option<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer: Option<BufferInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<CaptureConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// `"library"` or `"attach"`.
    pub mode: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppInfo {
    pub package: String,
    pub process: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debuggable: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub api: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub abis: Vec<String>,
}

/// A device monotonic time and the wall clock, sampled back to back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    pub ts: u64,
    pub wall_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookStatus {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// `installed`, `pending`, `class_not_found`, `method_not_found`, `failed`, `other_loader`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hits: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BufferInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_txns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub txns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
}

/// Effective capture configuration (in `hello`, `hello_ack`, `config_ack`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub recording: bool,
    pub body_cap: u64,
    pub capture_request_bodies: bool,
    pub capture_response_bodies: bool,
    pub stack_depth: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            recording: true,
            body_cap: 10 * 1024 * 1024,
            capture_request_bodies: true,
            capture_response_bodies: true,
            stack_depth: 64,
        }
    }
}

/// Partial update for `set_config`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_cap: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_request_bodies: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_response_bodies: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_depth: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Replay {
    /// `"begin"` or `"end"`.
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    /// `"okhttp"` or `"huc"`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadInfo {
    pub name: String,
    pub id: i64,
    /// `"call"`, `"interceptor"` or `"huc"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// One stack frame, innermost first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackFrame {
    /// Class.
    pub c: String,
    /// Method.
    pub m: String,
    /// Source file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f: Option<String>,
    /// Line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub l: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReqBodyInfo {
    #[serde(default = "minus_one")]
    pub length: i64,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub one_shot: bool,
    #[serde(default)]
    pub duplex: bool,
}

fn minus_one() -> i64 {
    -1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Req {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<u64>,
    #[serde(default)]
    pub hop: u32,
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Headers,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ClientInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<StackFrame>,
    #[serde(default)]
    pub stack_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<ReqBodyInfo>,
    /// Timing marks recorded before the transaction existed: `[name, ts]` pairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<(String, u64)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conn: Option<Conn>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Conn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Addr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<Tls>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addr {
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer: Vec<Cert>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cert {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub san: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resp {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
    pub status: u16,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default)]
    pub headers: Headers,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conn: Option<Conn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BodyEnd {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
    pub dir: BodyDir,
    pub bytes: u64,
    pub captured: u64,
    /// `complete`, `truncated`, `closed_early`, `none`, `not_captured`, `error`.
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prog {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
    pub dir: BodyDir,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
    pub m: String,
}

/// The `ws` event: one WebSocket message, its payload up to the capture cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ws {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
    /// `out` (the app sent it) or `in`.
    pub dir: String,
    /// `text`, `binary` or `close` (a newer runtime may send others).
    pub op: String,
    /// The payload's whole size in bytes.
    #[serde(default)]
    pub size: u64,
    /// The payload of a text message (UTF-8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The payload of a binary message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
    /// Only the first part of the payload is here (the capture cap).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// A close: its status code and reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<ErrorCause>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorCause {
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fail {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
    /// `connect`, `request`, `response_headers`, `response_body`, `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default)]
    pub canceled: bool,
    #[serde(default)]
    pub simulated: bool,
    pub error: ErrorInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conn: Option<Conn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One change a rule made. `op` is `status`, `header_set`, `header_add`, `header_remove`,
/// `body_replace`, `body_edit`, `delay` or `fail`; the other fields depend on it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub old: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matches: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeliveredResponse {
    pub status: u16,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub headers: Headers,
}

/// The `rule` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleApplied {
    pub seq: u64,
    pub ts: u64,
    pub txn: u64,
    #[serde(default)]
    pub rules: Vec<RuleRef>,
    #[serde(default)]
    pub changes: Vec<Change>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered: Option<DeliveredResponse>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dropped {
    pub seq: u64,
    pub ts: u64,
    pub events: u64,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub txns: Vec<u64>,
    #[serde(default)]
    pub txns_truncated: bool,
}

/// Whole-app byte counters (the default graph source).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Traffic {
    pub seq: u64,
    pub ts: u64,
    /// Cumulative bytes received by the app's uid.
    pub rx: u64,
    /// Cumulative bytes sent by the app's uid.
    pub tx: u64,
    /// Time of the previous sampling tick: the change since the previous `traffic` event
    /// happened within `(since, ts]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diag {
    pub seq: u64,
    pub ts: u64,
    /// `info`, `warn` or `error`.
    pub level: String,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleError {
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RulesAck {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<RuleError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigAck {
    pub id: u64,
    pub config: CaptureConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pong {
    pub id: u64,
    pub clock: Clock,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bye {
    /// `shutdown`, `replaced`, `protocol_mismatch`, `bad_frame`, `timeout`, `internal_error`.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported: Vec<u32>,
}

// ---------------------------------------------------------------------------------------------
// Host -> device
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum HostMsg {
    HelloAck(HelloAck),
    SetRules(SetRules),
    SetConfig(SetConfig),
    Ping(Ping),
    Bye(Bye),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloAck {
    pub id: u64,
    pub protocol: u32,
    pub host: HostInfo,
    #[serde(default)]
    pub resume_after_seq: u64,
    pub config: CaptureConfig,
    pub rules: RuleSet,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetRules {
    pub id: u64,
    pub rules: RuleSet,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetConfig {
    pub id: u64,
    pub config: CaptureConfigPatch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ping {
    pub id: u64,
}

// ---------------------------------------------------------------------------------------------
// Rules, wire form (PROTOCOL.md §8.2)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuleSet {
    pub version: String,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(rename = "match", default)]
    pub matcher: RuleMatch,
    #[serde(default)]
    pub actions: Vec<RuleAction>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_rewrites: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuleMatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<Pattern>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<Pattern>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<QueryMatch>,
}

/// `{ "exact": s }`, `{ "glob": s }` or `{ "regex": s }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pattern {
    Exact(String),
    Glob(String),
    Regex(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryMatch {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Pattern>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuleAction {
    Delay {
        ms: u64,
    },
    Fail {
        exception: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Status {
        code: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Header {
        /// `add`, `set` or `remove`.
        op: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    Body {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base64: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
    Replace {
        find: String,
        with: String,
        #[serde(default)]
        regex: bool,
    },
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The latest device time a message may carry: 2^62 ns, about 146 years after the device booted
/// (`ts` is `SystemClock.elapsedRealtimeNanos()`). Anything later comes from a broken or forged
/// stream, and the host's arithmetic on device times stays clear of overflow below it.
pub const MAX_DEVICE_TS: u64 = 1 << 62;

/// Parse the payload of a JSON frame from a device. A message with a device time past
/// [`MAX_DEVICE_TS`] is an error.
pub fn parse_device(json: &[u8]) -> serde_json::Result<DeviceMsg> {
    let msg: DeviceMsg = serde_json::from_slice(json)?;
    let ts = match &msg {
        DeviceMsg::Hello(h) => h.clock.ts.max(h.started_ts.unwrap_or(0)),
        DeviceMsg::Pong(p) => p.clock.ts,
        DeviceMsg::Req(r) => r.marks.iter().map(|(_, t)| *t).fold(r.ts, u64::max),
        DeviceMsg::Traffic(t) => t.ts.max(t.since.unwrap_or(0)),
        other => other.ts().unwrap_or(0),
    };
    if ts > MAX_DEVICE_TS {
        return Err(serde::de::Error::custom(format!("device time {ts} ns is out of range")));
    }
    Ok(msg)
}

/// Parse the payload of a JSON frame from a host.
pub fn parse_host(json: &[u8]) -> serde_json::Result<HostMsg> {
    serde_json::from_slice(json)
}

/// Serialize a message and append it as a JSON frame.
pub fn encode_msg<T: Serialize>(msg: &T, out: &mut BytesMut) -> serde_json::Result<()> {
    let json = serde_json::to_vec(msg)?;
    frame::encode_json(&json, out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The examples in PROTOCOL.md §7 must parse (comments removed).
    #[test]
    fn protocol_doc_examples_parse() {
        let req = r#"{
          "t": "req", "seq": 41, "ts": 5821334411223, "txn": 7, "call": 5, "hop": 0,
          "method": "GET",
          "url": "https://api.example.com/api/v1/orders/status?orderId=ord_4b67",
          "headers": [["Host", "api.example.com"], ["Accept-Encoding", "gzip"], ["User-Agent", "okhttp/4.12.0"]],
          "client": { "kind": "okhttp", "version": "4.12.0" },
          "thread": { "name": "DefaultDispatcher-worker-11", "id": 88, "origin": "call" },
          "stack": [ { "c": "com.example.shop.orders.OrderStatusPoller", "m": "poll", "f": "OrderStatusPoller.kt", "l": 41 } ],
          "stack_truncated": false,
          "body": { "length": -1, "type": null, "one_shot": false, "duplex": false },
          "marks": [["call_start", 5821290000000], ["dns_start", 5821291000000], ["dns_end", 5821300000000]],
          "conn": { "id": "c-17", "reused": true, "protocol": "h2",
                    "remote": { "ip": "142.250.183.14", "port": 443 }, "proxy": "DIRECT",
                    "tls": { "version": "TLSv1.3", "cipher": "TLS_AES_128_GCM_SHA256",
                             "peer": [ { "subject": "CN=*.example.com", "issuer": "CN=Example Issuing CA 1, O=Example Trust, C=US",
                                         "not_before_ms": 1780000000000, "not_after_ms": 1787776000000,
                                         "sha256": "3f", "san": ["*.example.com", "example.com"] } ] } }
        }"#;
        let DeviceMsg::Req(req) = parse_device(req.as_bytes()).unwrap() else { panic!("not req") };
        assert_eq!(req.headers[1], ("Accept-Encoding".into(), "gzip".into()));
        assert_eq!(req.marks.len(), 3);
        assert_eq!(req.conn.unwrap().tls.unwrap().peer[0].san.len(), 2);

        let rule = r#"{ "t": "rule", "seq": 47, "ts": 5822012000000, "txn": 7,
          "rules": [{ "id": "force-paid", "name": "Force payment captured" }],
          "changes": [
            { "op": "status", "from": 200, "to": 500, "reason": "Internal Server Error" },
            { "op": "header_set", "name": "Cache-Control", "value": "no-store", "old": ["max-age=60"] },
            { "op": "body_edit", "matches": 3 },
            { "op": "fail", "exception": "java.net.SocketTimeoutException" }
          ],
          "delivered": { "status": 500, "message": "Internal Server Error",
                         "headers": [["content-type", "application/json"], ["Cache-Control", "no-store"]] } }"#;
        let DeviceMsg::Rule(rule) = parse_device(rule.as_bytes()).unwrap() else { panic!("not rule") };
        assert_eq!(rule.changes[1].old, vec!["max-age=60".to_string()]);
        assert_eq!(rule.delivered.unwrap().status, 500);

        let hello = r#"{
          "t": "hello", "protocol": 1,
          "runtime": { "version": "0.1.0", "build": "3f2c1ab", "mode": "library" },
          "instance": "9d1c7e0f5b2a4c83a1e6f0d2b7c94e51",
          "app": { "package": "com.example.shop", "process": "com.example.shop",
                   "pid": 4312, "uid": 10234, "debuggable": true },
          "device": { "api": 35, "release": "15", "manufacturer": "Google", "model": "Pixel 8",
                      "abi": "arm64-v8a", "abis": ["arm64-v8a", "armeabi-v7a", "armeabi"] },
          "clock": { "ts": 5800000000000, "wall_ms": 1790658651000 },
          "started_ts": 5790000000000,
          "capabilities": ["okhttp", "okhttp_events", "huc", "rules", "pause", "resume", "prog", "traffic"],
          "clients": { "okhttp": "4.12.0", "huc": null },
          "hooks": [ { "id": "okhttp.networkInterceptors", "target": "okhttp3.OkHttpClient#networkInterceptors()Ljava/util/List;", "status": "installed", "hits": 12 } ],
          "buffer": { "max_txns": 1000, "max_body_bytes": 33554432, "txns": 37, "body_bytes": 812345, "first_seq": 1, "last_seq": 412 },
          "config": { "recording": true, "body_cap": 10485760, "capture_request_bodies": true, "capture_response_bodies": true, "stack_depth": 64 }
        }"#;
        let DeviceMsg::Hello(hello) = parse_device(hello.as_bytes()).unwrap() else { panic!("not hello") };
        assert_eq!(hello.app.pid, 4312);
        assert_eq!(hello.clients.get("huc"), Some(&None));

        for small in [
            r#"{ "t": "body_end", "seq": 51, "ts": 5822130000000, "txn": 7, "dir": "response", "bytes": 225, "captured": 225, "state": "complete" }"#,
            r#"{ "t": "prog", "seq": 60, "ts": 5823000000000, "txn": 9, "dir": "response", "bytes": 15728640 }"#,
            r#"{ "t": "mark", "seq": 45, "ts": 5822011500000, "txn": 7, "m": "resp_body_start" }"#,
            r#"{ "t": "done", "seq": 52, "ts": 5822130100000, "txn": 7 }"#,
            r#"{ "t": "fail", "seq": 70, "ts": 5830000000000, "txn": 11, "phase": "response_headers", "canceled": false, "simulated": false, "error": { "class": "java.net.SocketTimeoutException", "message": "timeout" } }"#,
            r#"{ "t": "dropped", "seq": 900, "ts": 5900000000000, "events": 412, "bytes": 3355443, "txns": [301, 302, 305], "txns_truncated": false }"#,
            r#"{ "t": "traffic", "seq": 88, "ts": 5824000000000, "rx": 18234112, "tx": 1203340, "since": 5823500000000 }"#,
            r#"{ "t": "diag", "seq": 3, "ts": 5800000000000, "level": "warn", "code": "hook_failed", "message": "not found", "data": { "hook": "okhttp.networkInterceptors" } }"#,
            r#"{ "t": "replay", "phase": "begin", "from_seq": 1, "to_seq": 412, "events": 412 }"#,
            r#"{ "t": "rules_ack", "id": 6, "version": "b7e1", "active": 2, "errors": [{ "rule": "bad-regex", "field": "match.path.regex", "message": "Unclosed group near index 7" }] }"#,
            r#"{ "t": "config_ack", "id": 5, "config": { "recording": false, "body_cap": 10485760, "capture_request_bodies": true, "capture_response_bodies": true, "stack_depth": 64 } }"#,
            r#"{ "t": "pong", "id": 7, "clock": { "ts": 5900000000000, "wall_ms": 1790658751000 } }"#,
            r#"{ "t": "bye", "reason": "protocol_mismatch", "message": "device speaks protocol 2", "supported": [1] }"#,
            r#"{ "t": "resp", "seq": 44, "ts": 5822011000000, "txn": 7, "status": 200, "message": "OK", "protocol": "h2", "headers": [["set-cookie", "a=1"], ["set-cookie", "b=2"]] }"#,
            r#"{ "t": "ws", "seq": 93, "ts": 5824100000000, "txn": 12, "dir": "out", "op": "text", "size": 39, "text": "{\"type\":\"subscribe\",\"channel\":\"orders\"}" }"#,
            r#"{ "t": "ws", "seq": 94, "ts": 5824160000000, "txn": 12, "dir": "in", "op": "binary", "size": 3, "base64": "AQID" }"#,
            r#"{ "t": "ws", "seq": 99, "ts": 5839000000000, "txn": 12, "dir": "out", "op": "close", "size": 0, "code": 1000, "reason": "done" }"#,
        ] {
            let msg = parse_device(small.as_bytes()).unwrap_or_else(|e| panic!("{small}: {e}"));
            assert_ne!(msg, DeviceMsg::Unknown, "{small}");
        }
    }

    #[test]
    fn host_examples_parse_and_round_trip() {
        let ack = r#"{ "t": "hello_ack", "id": 1, "protocol": 1,
          "host": { "name": "traffic-police", "version": "0.1.0" }, "resume_after_seq": 0,
          "config": { "recording": true, "body_cap": 10485760, "capture_request_bodies": true, "capture_response_bodies": true, "stack_depth": 64 },
          "rules": { "version": "b7e1", "rules": [
            { "id": "force-paid", "name": "Force payment captured", "enabled": true,
              "match": { "methods": ["GET"], "scheme": "https", "host": { "glob": "*.example.com" }, "port": 443,
                         "path": { "exact": "/api/v1/orders/status" },
                         "query": [{ "name": "orderId", "value": { "glob": "*" } }] },
              "actions": [
                { "type": "replace", "find": "\"payment\":\"pending\"", "with": "\"payment\":\"captured\"", "regex": false },
                { "type": "status", "code": 200, "reason": "OK" },
                { "type": "header", "op": "set", "name": "Cache-Control", "value": "no-store" },
                { "type": "body", "text": "{\"ok\":false}", "content_type": "application/json" },
                { "type": "delay", "ms": 3000 },
                { "type": "fail", "exception": "timeout", "message": "simulated by traffic-police" } ] } ] } }"#;
        let msg = parse_host(ack.as_bytes()).unwrap();
        let HostMsg::HelloAck(a) = &msg else { panic!("not hello_ack") };
        let rule = &a.rules.rules[0];
        assert_eq!(rule.matcher.host, Some(Pattern::Glob("*.example.com".into())));
        assert_eq!(rule.actions.len(), 6);
        let back: HostMsg = serde_json::from_slice(&serde_json::to_vec(&msg).unwrap()).unwrap();
        assert_eq!(back, msg);

        for m in [
            HostMsg::Ping(Ping { id: 7 }),
            HostMsg::SetConfig(SetConfig {
                id: 5,
                config: CaptureConfigPatch { recording: Some(false), ..Default::default() },
            }),
        ] {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(parse_host(json.as_bytes()).unwrap(), m, "{json}");
        }
        assert_eq!(serde_json::to_string(&HostMsg::Ping(Ping { id: 7 })).unwrap(), r#"{"t":"ping","id":7}"#);
    }

    #[test]
    fn unknown_types_and_fields_are_ignored() {
        assert_eq!(parse_device(br#"{"t":"hologram","x":1}"#).unwrap(), DeviceMsg::Unknown);
        let msg = parse_device(br#"{"t":"done","seq":1,"ts":2,"txn":3,"new_field":{"a":[1]}}"#).unwrap();
        assert_eq!(msg, DeviceMsg::Done(Done { seq: 1, ts: 2, txn: 3 }));
        assert_eq!(msg.seq(), Some(1));
        let action: RuleAction = serde_json::from_str(r#"{"type":"teleport"}"#).unwrap();
        assert_eq!(action, RuleAction::Unknown);
    }

    #[test]
    fn u64_values_survive_exactly() {
        let big = u64::MAX - 1;
        // the latest device time there may be, which a float could not hold exactly either
        let ts = MAX_DEVICE_TS - 1;
        let json = format!(r#"{{"t":"done","seq":{big},"ts":{ts},"txn":{big}}}"#);
        let DeviceMsg::Done(d) = parse_device(json.as_bytes()).unwrap() else { panic!() };
        assert_eq!((d.seq, d.ts, d.txn), (big, ts, big));
    }
}
