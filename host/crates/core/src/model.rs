//! The normalized domain model every event source feeds into (ARCHITECTURE.md §5.3, §5.6).

use std::sync::Arc;

pub use traffic_police_proto::msg::{Change, Conn, DeliveredResponse, HookStatus, RuleRef, StackFrame};
pub use traffic_police_proto::{BodyDir, Headers};

use crate::fmt::Ts;

/// A process segment: one capture-runtime instance in one app process.
pub type SourceId = u32;
/// Dense index of a transaction in the store, in arrival order.
pub type TxnIdx = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TxnKey {
    pub source: SourceId,
    pub txn: u64,
}

/// What we know about a source (from `hello`, plus the device label from adb).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    pub id: SourceId,
    /// e.g. `Pixel 8 [emulator-5554]`.
    pub device_label: String,
    pub serial: Option<String>,
    pub package: String,
    pub process: String,
    pub pid: u32,
    pub instance: String,
    /// `library`, `attach`, `demo`, `file`, `har`.
    pub mode: String,
    pub api: Option<u32>,
    pub runtime_version: Option<String>,
    pub capabilities: Vec<String>,
    pub hooks: Vec<HookStatus>,
    pub okhttp_version: Option<String>,
    /// Latest (device ts, wall ms) pair, for wall-clock labels.
    pub clock: Option<(Ts, i64)>,
    pub started: Ts,
    pub ended: Option<(Ts, String)>,
}

impl SourceInfo {
    /// Wall-clock milliseconds for a device timestamp, when a clock pair is known.
    pub fn wall_ms(&self, ts: Ts) -> Option<i64> {
        let (mono, wall) = self.clock?;
        Some(wall + ((ts as i128 - mono as i128) / 1_000_000) as i64)
    }
}

/// A parsed URL. OkHttp and HttpURLConnection hand us canonical absolute URLs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Url {
    pub raw: String,
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    /// Encoded path, `/` when empty.
    pub path: String,
    /// Encoded query without `?`.
    pub query: Option<String>,
}

impl Url {
    pub fn parse(raw: &str) -> Url {
        let mut url = Url { raw: raw.to_string(), path: "/".into(), ..Default::default() };
        let rest = match raw.split_once("://") {
            Some((scheme, rest)) => {
                url.scheme = scheme.to_ascii_lowercase();
                rest
            }
            None => raw,
        };
        let rest = rest.split('#').next().unwrap_or("");
        let (authority, path_query) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let hostport = authority.rsplit('@').next().unwrap_or(authority);
        if let Some(stripped) = hostport.strip_prefix('[') {
            // IPv6 literal
            if let Some((h, after)) = stripped.split_once(']') {
                url.host = format!("[{h}]");
                url.port = after.strip_prefix(':').and_then(|p| p.parse().ok());
            }
        } else if let Some((h, p)) = hostport.rsplit_once(':') {
            url.host = h.to_ascii_lowercase();
            url.port = p.parse().ok();
        } else {
            url.host = hostport.to_ascii_lowercase();
        }
        let (path, query) = match path_query.split_once('?') {
            Some((p, q)) => (p, Some(q.to_string())),
            None => (path_query, None),
        };
        if !path.is_empty() {
            url.path = path.to_string();
        }
        url.query = query;
        url
    }

    /// The port, with the scheme default filled in.
    pub fn effective_port(&self) -> Option<u16> {
        self.port.or(match self.scheme.as_str() {
            "https" | "wss" => Some(443),
            "http" | "ws" => Some(80),
            _ => None,
        })
    }

    /// The Name column: last path segment plus the query (Studio's convention); the host for `/`.
    pub fn name(&self) -> String {
        let seg = self.path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let base = if seg.is_empty() {
            // `.../status/` keeps the previous segment; `/` shows the host
            let trimmed = self.path.trim_end_matches('/');
            if trimmed.is_empty() { self.host.clone() } else { seg.to_string() }
        } else {
            seg.to_string()
        };
        match &self.query {
            Some(q) if !q.is_empty() => format!("{base}?{q}"),
            _ => base,
        }
    }

    /// Decoded query parameters in order (duplicates kept).
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        self.query.as_deref().map(crate::decode::form::parse_pairs).unwrap_or_default()
    }
}

/// Which HTTP client produced a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    /// `okhttp`, `huc`, `har`, ...
    pub kind: String,
    pub version: Option<String>,
}

impl ClientInfo {
    pub fn label(&self) -> String {
        let name = match self.kind.as_str() {
            "okhttp" => "OkHttp",
            "huc" => "HttpURLConnection",
            "har" => "HAR",
            other => other,
        };
        match &self.version {
            Some(v) => format!("{name} {v}"),
            None => name.to_string(),
        }
    }
}

/// State of one captured body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyState {
    /// Nothing seen yet.
    #[default]
    Pending,
    /// Bytes are flowing.
    Streaming,
    Complete,
    /// Reached its end; only part was captured because of the cap.
    Truncated,
    /// The app closed the stream before its end (0 bytes read = not consumed by the app).
    ClosedEarly,
    /// There is no body (HEAD, 204, 304, ...).
    None,
    /// Capture was disabled for this direction.
    NotCaptured,
    /// Reading or writing failed.
    Error,
}

impl BodyState {
    pub fn from_wire(s: &str) -> BodyState {
        match s {
            "complete" => BodyState::Complete,
            "truncated" => BodyState::Truncated,
            "closed_early" => BodyState::ClosedEarly,
            "none" => BodyState::None,
            "not_captured" => BodyState::NotCaptured,
            "error" => BodyState::Error,
            _ => BodyState::Complete,
        }
    }

    pub fn is_final(self) -> bool {
        !matches!(self, BodyState::Pending | BodyState::Streaming)
    }
}

/// Handle to bytes in the body store.
pub type BodyId = u32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyMeta {
    pub id: Option<BodyId>,
    /// Bytes that passed through, including past the cap.
    pub total: u64,
    /// Bytes we hold.
    pub captured: u64,
    pub state: BodyState,
    /// Some chunks were lost to device-side overflow.
    pub gap: bool,
    pub ended: Option<Ts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseInfo {
    pub at: Ts,
    pub status: u16,
    pub message: String,
    pub protocol: Option<String>,
    pub headers: Headers,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuleHit {
    pub at: Ts,
    pub rules: Vec<RuleRef>,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub at: Ts,
    pub class: String,
    pub message: Option<String>,
    pub causes: Vec<(String, Option<String>)>,
    pub phase: Option<String>,
    pub canceled: bool,
    pub simulated: bool,
}

impl Failure {
    pub fn short_class(&self) -> &str {
        self.class.rsplit('.').next().unwrap_or(&self.class)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TxnState {
    /// Request going out.
    Sending,
    /// Request sent, waiting for the first response byte.
    Waiting,
    /// Response headers received, body streaming.
    Receiving,
    Complete,
    Failed,
    /// The source went away before the transaction finished.
    Detached,
}

impl TxnState {
    pub fn is_open(self) -> bool {
        matches!(self, TxnState::Sending | TxnState::Waiting | TxnState::Receiving)
    }
}

/// Initiating thread as captured.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadInfo {
    pub source: SourceId,
    pub id: i64,
    pub name: String,
    /// `call`, `interceptor`, `huc`.
    pub origin: Option<String>,
}

/// One HTTP exchange (one network attempt; a redirect makes two, sharing `call`).
#[derive(Debug, Clone, PartialEq)]
pub struct Transaction {
    pub key: TxnKey,
    pub call: Option<u64>,
    pub hop: u32,
    /// True when events arrived for a transaction whose `req` we never saw.
    pub placeholder: bool,
    pub client: Option<ClientInfo>,
    pub method: String,
    pub url: Url,
    pub req_headers: Headers,
    pub req_body: BodyMeta,
    pub resp: Option<ResponseInfo>,
    pub resp_body: BodyMeta,
    /// What the app received when a rule changed the response (status line and headers).
    pub delivered: Option<DeliveredResponse>,
    pub delivered_body: Option<BodyMeta>,
    pub rules: Vec<RuleHit>,
    pub conn: Option<Conn>,
    pub thread: Option<ThreadInfo>,
    /// Index into the store's thread registry (lanes).
    pub lane: Option<u32>,
    pub stack: Arc<[StackFrame]>,
    pub stack_truncated: bool,
    pub marks: Vec<(String, Ts)>,
    /// The `req` event time (network-interceptor entry).
    pub req_at: Ts,
    /// Earliest known moment of this exchange (call start or connection setup, else `req_at`).
    pub start: Ts,
    pub end: Option<Ts>,
    pub state: TxnState,
    pub failure: Option<Failure>,
    /// Some events of this transaction were dropped on the device.
    pub lossy: bool,
    /// Bookmarked by the user (`m`); saved in session files.
    pub pinned: bool,
    /// The messages of a WebSocket (this transaction is its handshake), in order.
    pub ws: Arc<Vec<WsMessage>>,
    /// Store generation of the last change (lets views cache derived values).
    pub rev: u64,
}

/// One WebSocket message (PROTOCOL.md §7.1 `ws`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsMessage {
    pub at: Ts,
    /// Sent by the app (else received).
    pub out: bool,
    /// `text`, `binary` or `close`.
    pub op: String,
    /// The payload's whole size.
    pub size: u64,
    /// The payload as captured (at most the capture cap).
    pub data: bytes::Bytes,
    pub truncated: bool,
    /// A close's status code and reason.
    pub code: Option<u16>,
    pub reason: Option<String>,
}

impl Transaction {
    pub fn new_placeholder(key: TxnKey, at: Ts) -> Transaction {
        Transaction {
            key,
            call: None,
            hop: 0,
            placeholder: true,
            client: None,
            method: "?".into(),
            url: Url { raw: "(start not captured)".into(), path: "/".into(), ..Default::default() },
            req_headers: Vec::new(),
            req_body: BodyMeta::default(),
            resp: None,
            resp_body: BodyMeta::default(),
            delivered: None,
            delivered_body: None,
            rules: Vec::new(),
            conn: None,
            thread: None,
            lane: None,
            stack: Arc::from(Vec::new()),
            stack_truncated: false,
            marks: Vec::new(),
            req_at: at,
            start: at,
            end: None,
            state: TxnState::Waiting,
            failure: None,
            lossy: false,
            pinned: false,
            ws: Arc::new(Vec::new()),
            rev: 0,
        }
    }

    /// A WebSocket's handshake: a 101 that upgraded to `websocket`, or messages seen.
    pub fn is_websocket(&self) -> bool {
        !self.ws.is_empty()
            || self.resp.as_ref().is_some_and(|r| {
                r.status == 101 && header(&r.headers, "upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"))
            })
            || header(&self.req_headers, "upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"))
    }

    /// Status as the app saw it (after rules).
    pub fn status(&self) -> Option<u16> {
        self.delivered.as_ref().map(|d| d.status).or(self.resp.as_ref().map(|r| r.status))
    }

    /// Response headers as the app saw them.
    pub fn response_headers(&self) -> Option<&Headers> {
        self.delivered.as_ref().map(|d| &d.headers).or(self.resp.as_ref().map(|r| &r.headers))
    }

    pub fn rule_modified(&self) -> bool {
        self.delivered.is_some() || !self.rules.is_empty()
    }

    /// Duration so far (`now` for open transactions).
    pub fn duration(&self, now: Ts) -> u64 {
        self.end.unwrap_or(now).saturating_sub(self.start)
    }

    pub fn mark(&self, name: &str) -> Option<Ts> {
        self.marks.iter().find(|(n, _)| n == name).map(|&(_, t)| t)
    }

    /// Response `Content-Type` as received.
    pub fn response_content_type(&self) -> Option<&str> {
        header(self.response_headers()?, "content-type")
    }

    pub fn request_content_type(&self) -> Option<&str> {
        header(&self.req_headers, "content-type")
    }

    /// Short label for the Type column (`json`, `png`, `html`, ...).
    pub fn type_label(&self) -> String {
        if self.is_websocket() {
            return "ws".into();
        }
        match self.response_content_type() {
            Some(ct) => content_type_label(ct),
            None => match &self.resp {
                Some(_) => "".into(),
                None => "".into(),
            },
        }
    }

    /// Transferred response size (bytes on the wire for the body; for a WebSocket, what it
    /// received).
    pub fn response_size(&self) -> u64 {
        if !self.ws.is_empty() {
            return self.ws.iter().filter(|m| !m.out).map(|m| m.size).sum();
        }
        self.resp_body.total
    }
}

/// First value of a header, case-insensitively.
pub fn header<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// All values of a header, case-insensitively, in order.
pub fn header_all<'a>(headers: &'a Headers, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    headers.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// The media type without parameters, lowercased: `application/json`.
pub fn mime_essence(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

/// Studio-style short type label from a Content-Type.
pub fn content_type_label(content_type: &str) -> String {
    let essence = mime_essence(content_type);
    let (top, sub) = essence.split_once('/').unwrap_or((essence.as_str(), ""));
    let sub_base = sub.rsplit('+').next().unwrap_or(sub);
    match (top, sub) {
        (_, s) if s == "json" || s.ends_with("+json") || s == "problem+json" => "json".into(),
        (_, s) if s == "xml" || s.ends_with("+xml") => {
            if s.starts_with("svg") {
                "svg".into()
            } else {
                "xml".into()
            }
        }
        ("text", "html") => "html".into(),
        ("text", "plain") => "text".into(),
        ("text", "css") => "css".into(),
        ("text", "javascript") | ("application", "javascript") => "js".into(),
        ("application", "x-www-form-urlencoded") => "form".into(),
        ("multipart", _) => "multipart".into(),
        ("application", s) if s.starts_with("grpc") => "grpc".into(),
        ("application", "x-protobuf") | ("application", "protobuf") | ("application", "vnd.google.protobuf") => {
            "protobuf".into()
        }
        ("application", "octet-stream") => "binary".into(),
        ("image", s) => match s {
            "jpeg" | "jpg" => "jpeg".into(),
            "svg+xml" => "svg".into(),
            "x-icon" | "vnd.microsoft.icon" => "ico".into(),
            other => other.into(),
        },
        ("font", s) => s.into(),
        ("video", _) | ("audio", _) => top.into(),
        ("text", s) => s.into(),
        _ => sub_base.to_string(),
    }
}

/// Status class for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Informational,
    Success,
    Redirect,
    ClientError,
    ServerError,
    Failed,
    Pending,
}

impl Transaction {
    pub fn status_class(&self) -> StatusClass {
        if self.failure.is_some() || self.state == TxnState::Failed {
            return StatusClass::Failed;
        }
        match self.status() {
            None => StatusClass::Pending,
            Some(s) if s < 200 => StatusClass::Informational,
            Some(s) if s < 300 => StatusClass::Success,
            Some(s) if s < 400 => StatusClass::Redirect,
            Some(s) if s < 500 => StatusClass::ClientError,
            Some(_) => StatusClass::ServerError,
        }
    }

    /// Text for the Status column.
    pub fn status_text(&self) -> String {
        if self.failure.is_some() || self.state == TxnState::Failed {
            return if self.failure.as_ref().is_some_and(|f| f.canceled) { "canceled".into() } else { "failed".into() };
        }
        match self.status() {
            Some(s) => s.to_string(),
            None if self.state == TxnState::Detached => "detached".into(),
            None => "···".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parts_and_name() {
        let u = Url::parse("https://api.example.com/api/v1/orders/status/?orderId=abc&x=1");
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "api.example.com");
        assert_eq!(u.port, None);
        assert_eq!(u.effective_port(), Some(443));
        assert_eq!(u.path, "/api/v1/orders/status/");
        assert_eq!(u.query.as_deref(), Some("orderId=abc&x=1"));
        assert_eq!(u.name(), "status?orderId=abc&x=1");
        assert_eq!(u.query_pairs(), vec![("orderId".into(), "abc".into()), ("x".into(), "1".into())]);

        let u = Url::parse("http://10.0.2.2:8080");
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("10.0.2.2", Some(8080), "/"));
        assert_eq!(u.name(), "10.0.2.2");

        let u = Url::parse("https://[::1]:8443/a/b.png#frag");
        assert_eq!((u.host.as_str(), u.port, u.name().as_str()), ("[::1]", Some(8443), "b.png"));
    }

    #[test]
    fn type_labels() {
        for (ct, label) in [
            ("application/json; charset=utf-8", "json"),
            ("application/vnd.api+json", "json"),
            ("text/html", "html"),
            ("image/png", "png"),
            ("image/jpeg", "jpeg"),
            ("application/x-protobuf", "protobuf"),
            ("application/grpc+proto", "grpc"),
            ("multipart/form-data; boundary=x", "multipart"),
            ("application/x-www-form-urlencoded", "form"),
            ("application/octet-stream", "binary"),
            ("image/svg+xml", "svg"),
        ] {
            assert_eq!(content_type_label(ct), label, "{ct}");
        }
    }
}
