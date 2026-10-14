//! dart:io's HTTP profile (`ext.dart.io.getHttpProfile`, protocol extension 4.x) as device
//! protocol frames, so a Flutter app's requests reach the store, the session log and `tail` the
//! way the capture runtime's do (PROTOCOL.md §7.1; ARCHITECTURE.md §5.16).
//!
//! One profile entry is one transaction. Its `req` waits for the entry's `request` (dart:io adds
//! it once the request is sent: then its headers are final); marks follow the entry's events;
//! `resp` comes with the status; when the entry ends, its bodies are fetched
//! (`getHttpProfileRequest`) and sent, then `done` or `fail`.

use std::collections::{HashMap, HashSet};
use std::fmt;

use bytes::Bytes;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use traffic_police_proto::BodyDir;
use traffic_police_proto::frame::{BodyChunk, Frame, MAX_CHUNK_BYTES};
use traffic_police_proto::msg::{
    self, Addr, CaptureConfig, ClientInfo, Conn, DeviceMsg, ErrorInfo, Headers, ReqBodyInfo, ThreadInfo,
};

/// What one entry has sent so far.
#[derive(Debug, Default)]
struct Entry {
    txn: u64,
    req: bool,
    resp: bool,
    /// Events turned into marks (the list only grows).
    events: usize,
    req_ended: bool,
    /// Its bodies were asked for; it is finished once they are in.
    ending: bool,
    finished: bool,
}

/// A body as `getHttpProfileRequest` sends it, a JSON array of byte values, read straight into
/// bytes: the first `cap` kept, all of them counted. Up to 0.5.1 the array was read into JSON
/// values first (about 80 bytes a byte with `preserve_order`) and copied once more before the
/// cap applied, so a 30 MB download took gigabytes of the host's memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Body {
    /// The bytes kept (at most the cap).
    pub bytes: Vec<u8>,
    /// How many the body has.
    pub len: u64,
}

/// An entry as `getHttpProfileRequest` returns it: the entry (as JSON) without its bodies, and
/// the bodies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Full {
    pub entry: Value,
    pub request_body: Body,
    pub response_body: Body,
}

impl Full {
    /// An entry whose bodies could not be fetched.
    pub fn without_bodies(entry: &Value) -> Full {
        Full { entry: entry.clone(), ..Full::default() }
    }
}

/// Reads `getHttpProfileRequest`'s reply (`{"jsonrpc", "id", "result": {…}}`), keeping at most
/// `caps` bytes of the request and of the response body; `None` without a result.
pub fn read_full(reply: &str, caps: (usize, usize)) -> serde_json::Result<Option<Full>> {
    let mut de = serde_json::Deserializer::from_str(reply);
    let full = ReplySeed(caps).deserialize(&mut de)?;
    de.end()?;
    Ok(full)
}

/// The reply: its `result`, the rest skipped.
struct ReplySeed((usize, usize));

impl<'de> DeserializeSeed<'de> for ReplySeed {
    type Value = Option<Full>;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for ReplySeed {
    type Value = Option<Full>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON-RPC reply")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut full = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == "result" {
                full = Some(map.next_value_seed(EntrySeed(self.0))?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(full)
    }
}

/// The entry: `requestBody` and `responseBody` as [`Body`]s, the rest as JSON.
struct EntrySeed((usize, usize));

impl<'de> DeserializeSeed<'de> for EntrySeed {
    type Value = Full;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for EntrySeed {
    type Value = Full;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an HTTP profile entry")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let (request_cap, response_cap) = self.0;
        let mut entry = serde_json::Map::new();
        let mut full = Full::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "requestBody" => full.request_body = map.next_value_seed(BodySeed(request_cap))?,
                "responseBody" => full.response_body = map.next_value_seed(BodySeed(response_cap))?,
                _ => {
                    let v = map.next_value::<Value>()?;
                    entry.insert(key, v);
                }
            }
        }
        full.entry = Value::Object(entry);
        Ok(full)
    }
}

/// A body's array of byte values (or null), the first `.0` kept.
struct BodySeed(usize);

impl<'de> DeserializeSeed<'de> for BodySeed {
    type Value = Body;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for BodySeed {
    type Value = Body;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of byte values")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Body, E> {
        Ok(Body::default())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Body, A::Error> {
        let mut body = Body { bytes: Vec::with_capacity(seq.size_hint().unwrap_or(0).min(self.0)), len: 0 };
        while let Some(b) = seq.next_element::<u64>()? {
            if body.bytes.len() < self.0 {
                body.bytes.push(b as u8);
            }
            body.len += 1;
        }
        Ok(body)
    }
}

/// Turns profile entries into frames.
pub struct Translator {
    /// Device CLOCK_BOOTTIME ns minus wall-clock ns: the profile's times are wall-clock µs.
    offset_ns: i128,
    capture: CaptureConfig,
    /// The Dart version, for each transaction's `client`.
    dart: Option<String>,
    seq: u64,
    next_txn: u64,
    entries: HashMap<(String, String), Entry>,
    /// Isolate id → its name and number (the `thread` of its requests).
    isolates: HashMap<String, (String, i64)>,
    /// Local ports seen: a connection seen again was reused.
    connections: HashSet<String>,
}

/// The phases dart:io's events mark the end of (research notes: each fires as its phase ends).
fn mark_of(event: &str) -> Option<&'static str> {
    match event {
        "Connection established" => Some("connect_end"),
        "Request sent" => Some("req_headers_end"),
        "Waiting (TTFB)" => Some("resp_headers_start"),
        "Content Download" => Some("resp_body_end"),
        _ => None,
    }
}

/// `{name: [values…]}` as pairs, in the profile's order (dart:io keeps no order across names;
/// the values of one name stay in theirs).
fn pairs(headers: &Value) -> Headers {
    let mut out = Vec::new();
    if let Some(map) = headers.as_object() {
        for (name, values) in map {
            match values {
                Value::Array(vs) => {
                    for v in vs {
                        out.push((name.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string)));
                    }
                }
                Value::String(v) => out.push((name.clone(), v.clone())),
                other => out.push((name.clone(), other.to_string())),
            }
        }
    }
    out
}

/// `SocketException: Failed host lookup: 'x' (OS Error: …)` → its class and the rest.
fn error_info(text: &str) -> ErrorInfo {
    let (class, message) = match text.split_once(": ") {
        Some((c, m)) if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.') => {
            (c.to_string(), Some(m.to_string()))
        }
        _ => ("dart:io error".to_string(), Some(text.to_string())),
    };
    ErrorInfo { class, message, causes: Vec::new() }
}

impl Translator {
    pub fn new(offset_ns: i128, capture: CaptureConfig, dart: Option<String>) -> Translator {
        Translator {
            offset_ns,
            capture,
            dart,
            seq: 0,
            next_txn: 1,
            entries: HashMap::new(),
            isolates: HashMap::new(),
            connections: HashSet::new(),
        }
    }

    /// The device time of a profile time (µs since the epoch).
    pub fn ts(&self, us: i64) -> u64 {
        (i128::from(us) * 1000 + self.offset_ns).clamp(0, i128::from(msg::MAX_DEVICE_TS)) as u64
    }

    pub fn isolate(&mut self, id: &str, name: &str, number: i64) {
        self.isolates.insert(id.to_string(), (name.to_string(), number));
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn json(&mut self, m: DeviceMsg) -> Frame {
        Frame::Json(Bytes::from(serde_json::to_vec(&m).expect("a device message serializes")))
    }

    fn mark(&mut self, txn: u64, ts: u64, name: &str, out: &mut Vec<Frame>) {
        let seq = self.next_seq();
        out.push(self.json(DeviceMsg::Mark(msg::Mark { seq, ts, txn, m: name.into() })));
    }

    /// A diagnostic, as the runtime would send it.
    pub fn diag(&mut self, ts: u64, level: &str, code: &str, message: &str) -> Frame {
        let seq = self.next_seq();
        self.json(DeviceMsg::Diag(msg::Diag {
            seq,
            ts,
            level: level.into(),
            code: code.into(),
            message: message.into(),
            data: None,
        }))
    }

    fn conn(&mut self, info: &Value) -> Option<Conn> {
        let ip = info["remoteAddress"].as_str()?;
        let port = info["remotePort"].as_u64().and_then(|p| u16::try_from(p).ok())?;
        let id = info["localPort"].as_u64().map(|p| format!("dart:{p}"));
        let reused = id.as_ref().map(|i| !self.connections.insert(i.clone()));
        Some(Conn {
            id,
            reused,
            protocol: Some("http/1.1".into()),
            remote: Some(Addr { ip: ip.into(), port }),
            ..Conn::default()
        })
    }

    /// One entry of `getHttpProfile`: its new messages into `out`. True when it has ended and
    /// its bodies should be fetched (once), to end it with [`Translator::finish`].
    pub fn update(&mut self, e: &Value, out: &mut Vec<Frame>) -> bool {
        let (Some(isolate), Some(id)) = (e["isolateId"].as_str(), e["id"].as_str()) else { return false };
        let key = (isolate.to_string(), id.to_string());
        if !self.entries.contains_key(&key) {
            let txn = self.next_txn;
            self.next_txn += 1;
            self.entries.insert(key.clone(), Entry { txn, ..Entry::default() });
        }
        let (txn, req_sent, finished) = {
            let en = &self.entries[&key];
            (en.txn, en.req, en.finished || en.ending)
        };
        if finished {
            return false;
        }
        let request = &e["request"];
        let response = &e["response"];
        let request_error = request["error"].as_str();
        let response_error = response["error"].as_str();
        let has_status = response["statusCode"].as_u64().is_some();
        let ended = response["endTime"].is_i64() || request_error.is_some() || response_error.is_some();
        // a package:http_profile entry has `request` from the start; dart:io's, once it is sent
        let ready = request.as_object().is_some_and(|o| !o.is_empty()) || e["endTime"].is_i64() || has_status || ended;
        let events: Vec<(u64, &'static str)> = e["events"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|ev| Some((self.ts(ev["timestamp"].as_i64()?), mark_of(ev["event"].as_str()?)?)))
                    .collect()
            })
            .unwrap_or_default();
        let event_count = e["events"].as_array().map_or(0, Vec::len);

        if !req_sent {
            if !ready {
                return false;
            }
            let start = self.ts(e["startTime"].as_i64().unwrap_or(0));
            let headers = pairs(&request["headers"]);
            let content_type =
                headers.iter().find(|(n, _)| n.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.clone());
            let package = request["connectionInfo"]["package"].as_str().map(str::to_string);
            let (thread_name, thread_id) =
                self.isolates.get(isolate).map_or_else(|| (isolate.to_string(), 0), |(n, num)| (n.clone(), *num));
            let conn = self.conn(&request["connectionInfo"]);
            let mut marks = vec![("call_start".to_string(), start)];
            // dart:io's start is before DNS and connecting; it says only when the connection was
            // there, so setting up (or taking a pooled one) runs from the start
            if events.iter().any(|(_, m)| *m == "connect_end") {
                marks.push(("connect_start".to_string(), start));
            }
            marks.extend(events.iter().map(|(t, m)| (m.to_string(), *t)));
            let seq = self.next_seq();
            let req = DeviceMsg::Req(msg::Req {
                seq,
                ts: start,
                txn,
                call: Some(txn),
                hop: 0,
                method: e["method"].as_str().unwrap_or("GET").to_string(),
                url: e["uri"].as_str().unwrap_or_default().to_string(),
                headers,
                client: Some(match package {
                    Some(p) => ClientInfo { kind: p, version: None },
                    None => ClientInfo { kind: "dart:io".into(), version: self.dart.clone() },
                }),
                thread: Some(ThreadInfo {
                    name: format!("isolate {thread_name}"),
                    id: thread_id,
                    tid: None,
                    origin: Some("call".into()),
                }),
                stack: Vec::new(),
                stack_truncated: false,
                body: Some(ReqBodyInfo {
                    length: request["contentLength"].as_i64().unwrap_or(-1),
                    content_type,
                    one_shot: false,
                    duplex: false,
                }),
                marks,
                conn,
            });
            out.push(self.json(req));
            let en = self.entries.get_mut(&key).expect("present");
            en.req = true;
            en.events = event_count;
        } else {
            let done = self.entries[&key].events;
            for (t, m) in events.iter().skip(done) {
                self.mark(txn, *t, m, out);
            }
            self.entries.get_mut(&key).expect("present").events = event_count.max(done);
        }

        if let Some(end) = e["endTime"].as_i64()
            && !self.entries[&key].req_ended
        {
            let t = self.ts(end);
            self.mark(txn, t, "req_body_end", out);
            self.entries.get_mut(&key).expect("present").req_ended = true;
        }

        if has_status && !self.entries[&key].resp {
            let at = self.ts(response["startTime"].as_i64().or(e["startTime"].as_i64()).unwrap_or(0));
            let conn = self.conn(&response["connectionInfo"]);
            let seq = self.next_seq();
            let resp = DeviceMsg::Resp(msg::Resp {
                seq,
                ts: at,
                txn,
                status: response["statusCode"].as_u64().and_then(|s| u16::try_from(s).ok()).unwrap_or(0),
                message: response["reasonPhrase"].as_str().unwrap_or_default().to_string(),
                protocol: (!request["connectionInfo"]["package"].is_string()).then(|| "http/1.1".to_string()),
                headers: pairs(&response["headers"]),
                conn,
            });
            out.push(self.json(resp));
            self.mark(txn, at, "resp_headers_end", out);
            self.entries.get_mut(&key).expect("present").resp = true;
        }

        if ended {
            self.entries.get_mut(&key).expect("present").ending = true;
        }
        ended
    }

    /// What of each body (request, response) a fetch keeps: the capture cap, or nothing for a
    /// direction not captured (its size is still counted).
    pub fn body_caps(&self) -> (usize, usize) {
        let cap = usize::try_from(self.capture.body_cap).unwrap_or(usize::MAX);
        let of = |on: bool| if on { cap } else { 0 };
        (of(self.capture.capture_request_bodies), of(self.capture.capture_response_bodies))
    }

    /// The bytes of one body as frames: in the capture cap, the rest counted. Returns what was
    /// captured.
    fn body(&mut self, txn: u64, dir: BodyDir, ts: u64, bytes: &[u8], capture: bool, out: &mut Vec<Frame>) -> u64 {
        if !capture {
            return 0;
        }
        let keep = bytes.len().min(usize::try_from(self.capture.body_cap).unwrap_or(usize::MAX));
        for (i, piece) in bytes[..keep].chunks(MAX_CHUNK_BYTES).enumerate() {
            let seq = self.next_seq();
            out.push(Frame::Body(BodyChunk {
                seq,
                txn,
                dir,
                ts,
                offset: (i * MAX_CHUNK_BYTES) as u64,
                data: Bytes::copy_from_slice(piece),
            }));
        }
        keep as u64
    }

    #[allow(clippy::too_many_arguments)]
    fn body_end(
        &mut self,
        txn: u64,
        dir: BodyDir,
        ts: u64,
        total: u64,
        captured: u64,
        capture: bool,
        decoded: bool,
    ) -> Frame {
        let state = if total == 0 {
            "none"
        } else if !capture {
            "not_captured"
        } else if captured < total {
            "truncated"
        } else {
            "complete"
        };
        let seq = self.next_seq();
        self.json(DeviceMsg::BodyEnd(msg::BodyEnd {
            seq,
            ts,
            txn,
            dir,
            bytes: total,
            captured,
            state: state.into(),
            decoded: decoded && total > 0,
        }))
    }

    /// The entry as `getHttpProfileRequest` returns it, with its bodies (see [`read_full`]):
    /// its bodies, then `done` or `fail`. The bodies may be missing (they could not be fetched).
    pub fn finish(&mut self, f: &Full, out: &mut Vec<Frame>) {
        let full = &f.entry;
        let (Some(isolate), Some(id)) = (full["isolateId"].as_str(), full["id"].as_str()) else { return };
        let key = (isolate.to_string(), id.to_string());
        let Some(en) = self.entries.get(&key) else { return };
        if en.finished {
            return;
        }
        let txn = en.txn;
        if !en.req {
            // ended before it was ever sent (an error at once): its request first
            self.entries.get_mut(&key).expect("present").ending = false;
            self.update(full, out);
        }
        let request = &full["request"];
        let response = &full["response"];
        let start = full["startTime"].as_i64().unwrap_or(0);
        let req_at = self.ts(full["endTime"].as_i64().unwrap_or(start));
        let capture_req = self.capture.capture_request_bodies;
        let got = self.body(txn, BodyDir::Request, req_at, &f.request_body.bytes, capture_req, out);
        let end = self.body_end(txn, BodyDir::Request, req_at, f.request_body.len, got, capture_req, false);
        out.push(end);

        let resp_at = self.ts(response["endTime"].as_i64().or(response["startTime"].as_i64()).unwrap_or(start));
        let request_error = request["error"].as_str().map(str::to_string);
        let response_error = response["error"].as_str().map(str::to_string);
        // a WebSocket's handshake ends when dart:io hands its socket to the WebSocket
        let upgraded = response["statusCode"].as_u64() == Some(101)
            && response_error.as_deref().is_some_and(|e| e.contains("detached") || e.contains("upgraded"));
        if response["statusCode"].as_u64().is_some() {
            let capture_resp = self.capture.capture_response_bodies;
            let got = self.body(txn, BodyDir::Response, resp_at, &f.response_body.bytes, capture_resp, out);
            let state = response["compressionState"].as_str().unwrap_or_default();
            let decoded = state.ends_with("decompressed");
            let end = self.body_end(txn, BodyDir::Response, resp_at, f.response_body.len, got, capture_resp, decoded);
            out.push(end);
        }
        let seq = self.next_seq();
        let last = match (&request_error, &response_error) {
            (_, Some(_)) if upgraded => None,
            (Some(e), _) => {
                Some((if request["connectionInfo"].is_object() { "request" } else { "connect" }, e.clone()))
            }
            (None, Some(e)) => Some(("response_body", e.clone())),
            (None, None) => None,
        };
        let m = match last {
            None => DeviceMsg::Done(msg::Done { seq, ts: resp_at, txn, trailers: Vec::new(), grpc: None }),
            Some((phase, error)) => DeviceMsg::Fail(msg::Fail {
                seq,
                ts: resp_at.max(req_at),
                txn,
                phase: Some(phase.into()),
                canceled: false,
                simulated: false,
                error: error_info(&error),
                conn: None,
                trailers: Vec::new(),
                grpc: None,
            }),
        };
        out.push(self.json(m));
        let en = self.entries.get_mut(&key).expect("present");
        en.finished = true;
        en.ending = false;
    }

    /// Entries sent and not finished.
    #[cfg(test)]
    pub fn open(&self) -> usize {
        self.entries.values().filter(|e| e.req && !e.finished).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use traffic_police_proto::msg::parse_device;

    /// `entry` as `getHttpProfileRequest` would return it, read as the backend reads it.
    fn fetched(t: &Translator, entry: &Value) -> Full {
        let reply = json!({ "jsonrpc": "2.0", "id": "tp7", "result": entry }).to_string();
        read_full(&reply, t.body_caps()).unwrap().expect("a result")
    }

    fn msgs(frames: &[Frame]) -> Vec<String> {
        frames
            .iter()
            .map(|f| match f {
                Frame::Json(j) => {
                    let v: Value = serde_json::from_slice(j).unwrap();
                    parse_device(j).unwrap();
                    match v["t"].as_str().unwrap() {
                        "mark" => format!("mark {}", v["m"].as_str().unwrap()),
                        "body_end" => format!("body_end {} {} {}", v["dir"], v["state"].as_str().unwrap(), v["bytes"]),
                        t => t.to_string(),
                    }
                }
                Frame::Body(c) => format!("chunk {:?} {}", c.dir, c.data.len()),
                Frame::Other { .. } => "other".into(),
            })
            .collect()
    }

    /// A GET as dart:io reports it over three polls: sent, answered, done.
    #[test]
    fn a_dart_io_get_becomes_one_transaction() {
        let mut t = Translator::new(-1_790_000_000_000_000_000, CaptureConfig::default(), Some("3.13.5".into()));
        t.isolate("isolates/1", "main", 1);
        let base = json!({ "type": "@HttpProfileRequest", "id": "-42", "isolateId": "isolates/1", "method": "GET",
            "uri": "https://api.example.com/v1/orders?id=4b67", "startTime": 1_790_000_000_950_000i64,
            "events": [{ "timestamp": 1_790_000_001_010_000i64, "event": "Connection established" }] });
        // nothing until the request is sent
        let mut out = Vec::new();
        assert!(!t.update(&base, &mut out));
        assert!(out.is_empty());

        let mut sent = base.clone();
        sent["endTime"] = json!(1_790_000_001_010_100i64);
        sent["request"] = json!({ "headers": { "user-agent": ["Dart/3.13 (dart:io)"], "accept-encoding": ["gzip"] },
            "connectionInfo": { "localPort": 40512, "remoteAddress": "203.0.113.10", "remotePort": 443 }, "contentLength": 0 });
        sent["events"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "timestamp": 1_790_000_001_010_050i64, "event": "Request sent" }));
        assert!(!t.update(&sent, &mut out));
        assert_eq!(msgs(&out), ["req", "mark req_body_end"]);
        let Frame::Json(j) = &out[0] else { panic!() };
        let DeviceMsg::Req(r) = parse_device(j).unwrap() else { panic!() };
        assert_eq!(r.ts, 950_000_000);
        assert_eq!(
            r.marks,
            vec![
                ("call_start".into(), 950_000_000),
                ("connect_start".into(), 950_000_000),
                ("connect_end".into(), 1_010_000_000),
                ("req_headers_end".into(), 1_010_050_000)
            ]
        );
        assert_eq!(r.headers[1], ("accept-encoding".into(), "gzip".into()));
        assert_eq!(r.client.unwrap().kind, "dart:io");
        assert_eq!(r.conn.unwrap().remote.unwrap().port, 443);

        let mut answered = sent.clone();
        answered["response"] = json!({ "startTime": 1_790_000_001_200_100i64, "statusCode": 200, "reasonPhrase": "OK",
            "headers": { "content-type": ["application/json"], "content-encoding": ["gzip"], "set-cookie": ["a=1", "b=2"] },
            "compressionState": "HttpClientResponseCompressionState.decompressed", "endTime": 1_790_000_001_230_000i64 });
        answered["events"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "timestamp": 1_790_000_001_200_000i64, "event": "Waiting (TTFB)" }));
        out.clear();
        assert!(t.update(&answered, &mut out), "it ended: fetch its bodies");
        assert_eq!(msgs(&out), ["mark resp_headers_start", "resp", "mark resp_headers_end"]);
        // polled again before the bodies came: nothing more
        out.clear();
        assert!(!t.update(&answered, &mut out));
        assert!(out.is_empty());

        let mut full = answered.clone();
        full["requestBody"] = json!([]);
        full["responseBody"] = json!([123, 34, 105, 100, 34, 58, 49, 125]);
        t.finish(&fetched(&t, &full), &mut out);
        assert_eq!(
            msgs(&out),
            ["body_end \"request\" none 0", "chunk Response 8", "body_end \"response\" complete 8", "done"]
        );
        let Frame::Json(j) = &out[2] else { panic!() };
        let DeviceMsg::BodyEnd(b) = parse_device(j).unwrap() else { panic!() };
        assert!(b.decoded, "dart:io decompressed it");
        assert_eq!(t.open(), 0);
    }

    #[test]
    fn errors_fail_and_an_upgrade_is_done() {
        let mut t = Translator::new(0, CaptureConfig::default(), None);
        let failed = json!({ "id": "7", "isolateId": "isolates/1", "method": "GET", "uri": "https://nope.invalid/",
            "startTime": 1000, "endTime": 2000,
            "request": { "error": "SocketException: Failed host lookup: 'nope.invalid'" } });
        let mut out = Vec::new();
        assert!(t.update(&failed, &mut out));
        t.finish(&Full::without_bodies(&failed), &mut out);
        assert_eq!(msgs(&out), ["req", "mark req_body_end", "body_end \"request\" none 0", "fail"]);
        let Frame::Json(j) = out.last().unwrap() else { panic!() };
        let DeviceMsg::Fail(f) = parse_device(j).unwrap() else { panic!() };
        assert_eq!((f.error.class.as_str(), f.phase.as_deref()), ("SocketException", Some("connect")));

        let upgrade = json!({ "id": "8", "isolateId": "isolates/1", "method": "GET", "uri": "https://live.example.com/ws",
            "startTime": 1000, "endTime": 1500, "request": { "headers": { "upgrade": ["websocket"] } },
            "response": { "statusCode": 101, "startTime": 1800, "endTime": 1900, "error": "Socket has been detached", "headers": {} } });
        out.clear();
        assert!(t.update(&upgrade, &mut out));
        t.finish(&fetched(&t, &upgrade), &mut out);
        assert_eq!(msgs(&out).last().unwrap(), "done");
    }

    #[test]
    fn bodies_over_the_cap_are_cut_and_counted() {
        let cfg = CaptureConfig { body_cap: 100_000, ..CaptureConfig::default() };
        let mut t = Translator::new(0, cfg, None);
        let body: Vec<u8> = (0..150_000u32).map(|i| (i % 7) as u8).collect();
        let e = json!({ "id": "9", "isolateId": "isolates/1", "method": "POST", "uri": "http://10.0.2.2:8080/up",
            "startTime": 1, "endTime": 2, "request": { "headers": {}, "contentLength": 150000 },
            "response": { "statusCode": 204, "startTime": 3, "endTime": 4, "headers": {} },
            "requestBody": body, "responseBody": [] });
        let mut out = Vec::new();
        assert!(t.update(&e, &mut out));
        out.clear();
        t.finish(&fetched(&t, &e), &mut out);
        assert_eq!(
            msgs(&out),
            [
                "chunk Request 65536",
                "chunk Request 34464",
                "body_end \"request\" truncated 150000",
                "body_end \"response\" none 0",
                "done"
            ]
        );
    }

    /// A body arrives as an array of numbers, about four bytes of JSON a byte: it is read straight
    /// into bytes up to the cap and counted past it, and nothing of it stays in the entry.
    #[test]
    fn bodies_are_read_as_bytes_up_to_the_cap() {
        let cfg = CaptureConfig { body_cap: 1 << 20, capture_request_bodies: false, ..CaptureConfig::default() };
        let t = Translator::new(0, cfg, None);
        let body: Vec<u8> = (0..3 << 20).map(|i: u32| (i % 251) as u8).collect();
        let entry = json!({ "id": "9", "isolateId": "isolates/1", "method": "POST", "uri": "http://10.0.2.2:8080/up",
            "requestBody": [1, 2, 3], "responseBody": body, "response": { "statusCode": 200 } });
        let full = fetched(&t, &entry);
        assert_eq!(full.response_body.len, 3 << 20);
        assert_eq!(full.response_body.bytes.len(), 1 << 20);
        assert_eq!(full.response_body.bytes[..], body[..1 << 20]);
        assert_eq!(full.request_body, Body { bytes: Vec::new(), len: 3 }, "not captured, still counted");
        assert!(full.entry.get("responseBody").is_none() && full.entry["response"]["statusCode"] == 200);
        // null bodies, no result, and a body that is no array of numbers
        let reply = r#"{"jsonrpc":"2.0","id":"tp1","result":{"id":"1","requestBody":null}}"#;
        assert_eq!(read_full(reply, (10, 10)).unwrap().unwrap().request_body, Body::default());
        assert_eq!(read_full(r#"{"jsonrpc":"2.0","id":"tp1"}"#, (10, 10)).unwrap(), None);
        assert!(read_full(r#"{"result":{"responseBody":["x"]}}"#, (10, 10)).is_err());
        assert!(read_full(r#"{"result":{"responseBody":[1]}} x"#, (10, 10)).is_err());
    }
}
