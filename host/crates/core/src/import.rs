//! Opening saved traffic for replay: session files (see [`crate::session`]) and HAR files. Each
//! HAR entry becomes a transaction of a `har` source, with its timings, headers and bodies, and,
//! for files traffic-police wrote, the `_trafficPolice` fields (source, thread, call stack,
//! failure, pin).
//!
//! HAR keeps bodies decoded, so imported bodies are stored decoded; their Content-Encoding
//! headers stay as recorded (see [`SessionStore::decoding_headers`]).

use std::collections::HashMap;
use std::path::Path;

use base64::Engine;
use bytes::Bytes;
use serde_json::Value;
use traffic_police_proto::msg::{self, Addr, Conn, ErrorInfo, ReqBodyInfo, RuleRef, StackFrame};
use traffic_police_proto::{BodyDir, Headers};

use crate::event::{Failed, RequestStarted, ResponseStarted, RuleApplied, SessionEvent};
use crate::fmt::{NS_PER_MS, Ts, parse_iso8601};
use crate::model::{SourceId, SourceInfo, TxnKey, Url};
use crate::session::{self, Opened};
use crate::store::{SessionStore, SourceIds};

/// Imported time starts at 1 s, so no timestamp is zero.
const T0: Ts = 1_000_000_000;

/// A saved session or a HAR file, read for replay. Entries a HAR file could not supply are
/// listed in `Opened::skipped`.
pub fn open(path: &Path, ids: &SourceIds) -> Result<Opened, String> {
    if session::is_session_file(path) {
        return session::open(path, ids).map_err(|e| e.to_string());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if !looks_like_json(&bytes) {
        return Err("not a traffic-police session file (.trafficpolice) or a HAR file".into());
    }
    har(&bytes, ids)
}

fn looks_like_json(bytes: &[u8]) -> bool {
    let b = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    b.iter().find(|c| !c.is_ascii_whitespace()) == Some(&b'{')
}

/// Reads a HAR document; sources get fresh ids from `ids`.
pub fn har(bytes: &[u8], ids: &SourceIds) -> Result<Opened, String> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| format!("not a HAR file: {e}"))?;
    let log = doc.get("log").ok_or("not a HAR file (no \"log\")")?;
    let entries = log.get("entries").and_then(Value::as_array).ok_or("not a HAR file (no \"log.entries\")")?;
    let creator = [&log["creator"]["name"], &log["creator"]["version"]]
        .iter()
        .filter_map(|v| v.as_str().filter(|s| !s.is_empty()))
        .collect::<Vec<_>>()
        .join(" ");

    let mut out = Opened::default();
    let mut dated: Vec<(f64, usize, &Value)> = Vec::new();
    for (n, e) in entries.iter().enumerate() {
        match e["startedDateTime"].as_str().and_then(parse_iso8601) {
            Some(ms) if e["request"]["url"].is_string() => dated.push((ms, n, e)),
            Some(_) => out.skipped.push(format!("entry {}: no request URL", n + 1)),
            None => out.skipped.push(format!("entry {}: no startedDateTime", n + 1)),
        }
    }
    dated.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let Some(&(base_ms, _, _)) = dated.first() else {
        return Ok(out);
    };
    out.created_wall_ms = Some(base_ms as i64);

    let mut sources: HashMap<String, Source> = HashMap::new();
    for (ms, _, e) in dated {
        let tp = &e["_trafficPolice"];
        let source_key = tp.get("source").map_or(String::new(), Value::to_string);
        let start = T0 + ((ms - base_ms) * NS_PER_MS as f64).round() as Ts;
        let src = sources.entry(source_key).or_insert_with(|| {
            let id = ids.next();
            let info = source_info(id, &tp["source"], &creator, start, base_ms);
            out.events.push(SessionEvent::SourceUp(Box::new(info)));
            Source { id, next_txn: 1, last: start, unfinished: false }
        });
        let key = TxnKey { source: src.id, txn: src.next_txn };
        src.next_txn += 1;
        let entry = entry_events(e, key, start, &mut out.events);
        src.last = src.last.max(entry.end);
        src.unfinished |= entry.unfinished;
        if tp["pinned"] == Value::Bool(true) {
            out.pins.push(key);
        }
    }
    // requests that were still running when the recording ended
    let mut ended: Vec<&Source> = sources.values().filter(|s| s.unfinished).collect();
    ended.sort_by_key(|s| s.id);
    for s in ended {
        out.events.push(SessionEvent::SourceDown { source: s.id, at: s.last, reason: "end of the HAR file".into() });
    }
    Ok(out)
}

struct Source {
    id: SourceId,
    next_txn: u64,
    last: Ts,
    unfinished: bool,
}

fn source_info(id: SourceId, s: &Value, creator: &str, started: Ts, base_ms: f64) -> SourceInfo {
    let text = |v: &Value| v.as_str().map(str::to_string);
    let package = text(&s["package"]).unwrap_or_default();
    SourceInfo {
        id,
        device_label: text(&s["device"]).unwrap_or_else(|| "HAR file".into()),
        serial: text(&s["serial"]),
        process: text(&s["process"]).unwrap_or_else(|| {
            if !package.is_empty() {
                package.clone()
            } else if creator.is_empty() {
                "unknown creator".into()
            } else {
                format!("from {creator}")
            }
        }),
        package,
        pid: s["pid"].as_u64().map_or(0, |p| p as u32),
        instance: format!("har-{id}"),
        mode: "har".into(),
        api: None,
        runtime_version: None,
        capabilities: Vec::new(),
        hooks: Vec::new(),
        okhttp_version: None,
        clock: Some((T0, base_ms.round() as i64)),
        started,
        ended: None,
    }
}

fn name_values(v: &Value) -> Headers {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|h| Some((h["name"].as_str()?.to_string(), h["value"].as_str().unwrap_or("").to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// A HAR body: `text`, in base64 when `encoding` says so.
fn body_bytes(v: &Value) -> Option<Bytes> {
    let text = v["text"].as_str()?;
    if v["encoding"].as_str() == Some("base64") {
        base64::engine::general_purpose::STANDARD.decode(text.trim()).ok().map(Bytes::from)
    } else {
        Some(Bytes::copy_from_slice(text.as_bytes()))
    }
}

fn protocol(http_version: &str) -> Option<String> {
    let v = http_version.to_ascii_lowercase();
    Some(
        match v.as_str() {
            "" | "unknown" => return None,
            "h2" | "http/2" | "http/2.0" => "h2",
            "h3" | "http/3" | "http/3.0" => "h3",
            "http/1.0" => "http/1.0",
            "http/1.1" => "http/1.1",
            other if other.starts_with("h3-") => "h3",
            other => return Some(other.to_string()),
        }
        .to_string(),
    )
}

/// `OkHttp 4.12.0` (as the HAR export writes it) back to kind and version.
fn client(label: Option<&str>) -> msg::ClientInfo {
    let Some(label) = label else { return msg::ClientInfo { kind: "har".into(), version: None } };
    let (name, version) = match label.split_once(' ') {
        Some((n, v)) => (n, Some(v.to_string())),
        None => (label, None),
    };
    let kind = match name {
        "OkHttp" => "okhttp",
        "HttpURLConnection" => "huc",
        "HAR" => "har",
        other => other,
    };
    msg::ClientInfo { kind: kind.to_string(), version }
}

struct EntryEnd {
    end: Ts,
    unfinished: bool,
}

/// The events of one entry, starting at `start`.
fn entry_events(e: &Value, key: TxnKey, start: Ts, out: &mut Vec<SessionEvent>) -> EntryEnd {
    let tp = &e["_trafficPolice"];
    let req = &e["request"];
    // a response a rule changed: the network's is in _trafficPolice.original, the app's in response
    let (resp, delivered) =
        if tp["original"].is_object() { (&tp["original"], Some(&e["response"])) } else { (&e["response"], None) };
    let timings = &e["timings"];
    let at = |ms: f64| start + (ms.max(0.0) * NS_PER_MS as f64).round() as Ts;
    let phase = |name: &str| timings[name].as_f64().filter(|x| *x >= 0.0);

    // timing marks: as recorded by traffic-police, else from the HAR timings
    let mut marks: Vec<(String, Ts)> = Vec::new();
    let (req_at, first_byte, end);
    if let Some(recorded) = tp["marks_ms"].as_object() {
        marks = recorded.iter().filter_map(|(n, v)| Some((n.clone(), at(v.as_f64()?)))).collect();
        marks.sort_by_key(|m| m.1);
        let mark = |name: &str| marks.iter().find(|m| m.0 == name).map(|m| m.1);
        req_at = mark("req_headers_start").or_else(|| mark("call_start")).unwrap_or(start);
        first_byte = mark("resp_headers_start");
        end = at(e["time"].as_f64().unwrap_or(0.0)).max(marks.last().map_or(start, |m| m.1));
    } else {
        let mut c = 0.0;
        marks.push(("call_start".into(), start));
        c += phase("blocked").unwrap_or(0.0);
        if let Some(d) = phase("dns") {
            marks.push(("dns_start".into(), at(c)));
            c += d;
            marks.push(("dns_end".into(), at(c)));
        }
        if let Some(k) = phase("connect") {
            marks.push(("connect_start".into(), at(c)));
            if let Some(s) = phase("ssl").filter(|s| *s <= k) {
                marks.push(("tls_start".into(), at(c + k - s)));
                marks.push(("tls_end".into(), at(c + k)));
            }
            c += k;
            marks.push(("connect_end".into(), at(c)));
        }
        req_at = at(c);
        marks.push(("req_headers_start".into(), req_at));
        c += phase("send").unwrap_or(0.0);
        marks.push(("req_headers_end".into(), at(c)));
        c += phase("wait").unwrap_or(0.0);
        let status = resp["status"].as_u64().unwrap_or(0);
        first_byte = (status > 0).then(|| at(c));
        if let Some(f) = first_byte {
            marks.push(("resp_headers_start".into(), f));
        }
        c += phase("receive").unwrap_or(0.0);
        end = at(c.max(e["time"].as_f64().unwrap_or(0.0)));
        if first_byte.is_some() {
            marks.push(("resp_body_end".into(), end));
        }
    }
    let (early, later): (Vec<_>, Vec<_>) = marks.into_iter().partition(|m| m.1 <= req_at);

    let url = req["url"].as_str().unwrap_or_default().to_string();
    let conn = e["serverIPAddress"].as_str().filter(|ip| !ip.is_empty()).map(|ip| Conn {
        id: e["connection"].as_str().map(str::to_string),
        remote: Some(Addr {
            ip: ip.trim_matches(['[', ']']).to_string(),
            port: Url::parse(&url).effective_port().unwrap_or(0),
        }),
        ..Conn::default()
    });
    let req_headers = name_values(&req["headers"]);
    let req_body = body_bytes(&req["postData"]);
    let thread = tp["thread"]["name"].as_str().map(|name| msg::ThreadInfo {
        name: name.to_string(),
        id: tp["thread"]["id"].as_i64().unwrap_or(0),
        origin: tp["thread"]["origin"].as_str().map(str::to_string),
    });
    let stack: Vec<StackFrame> = tp["stack"]
        .as_array()
        .map(|frames| {
            frames
                .iter()
                .filter_map(|f| {
                    Some(StackFrame {
                        c: f["class"].as_str()?.to_string(),
                        m: f["method"].as_str().unwrap_or("").to_string(),
                        f: f["file"].as_str().map(str::to_string),
                        l: f["line"].as_i64().map(|l| l as i32),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    out.push(SessionEvent::Request(Box::new(RequestStarted {
        key,
        at: req_at,
        call: tp["call"].as_u64(),
        hop: tp["hop"].as_u64().map_or(0, |h| h as u32),
        method: req["method"].as_str().unwrap_or("GET").to_string(),
        url,
        headers: req_headers,
        client: Some(client(tp["client"].as_str())),
        thread,
        stack,
        stack_truncated: false,
        body: req_body.as_ref().map(|b| ReqBodyInfo {
            length: b.len() as i64,
            content_type: req["postData"]["mimeType"].as_str().map(str::to_string),
            one_shot: false,
            duplex: false,
        }),
        marks: early,
        conn: conn.clone(),
    })));
    let sent = later.iter().find(|m| m.0 == "req_headers_end" || m.0 == "req_body_end").map_or(req_at, |m| m.1);
    if let Some(b) = req_body {
        let n = b.len() as u64;
        out.push(SessionEvent::Body { key, dir: BodyDir::Request, at: req_at, offset: 0, bytes: b });
        out.push(body_end(key, BodyDir::Request, sent, n, n, "complete"));
    }

    let status = resp["status"].as_u64().unwrap_or(0);
    if status > 0 {
        let at_first = first_byte.unwrap_or(end);
        out.push(SessionEvent::Response(Box::new(ResponseStarted {
            key,
            at: at_first,
            status: status.min(u64::from(u16::MAX)) as u16,
            message: resp["statusText"].as_str().unwrap_or("").to_string(),
            protocol: protocol(resp["httpVersion"].as_str().unwrap_or("")),
            headers: name_values(&resp["headers"]),
            conn,
        })));
        // the body ends after `receive`; the exchange may end a little later
        let body_done = phase("receive").map_or(end, |r| (at_first + (r * NS_PER_MS as f64).round() as Ts).min(end));
        match body_bytes(&resp["content"]) {
            Some(b) if !b.is_empty() => {
                let n = b.len() as u64;
                out.push(SessionEvent::Body { key, dir: BodyDir::Response, at: at_first, offset: 0, bytes: b });
                out.push(body_end(key, BodyDir::Response, body_done, n, n, "complete"));
            }
            _ => {
                let size = resp["content"]["size"].as_i64().filter(|s| *s > 0);
                let (total, state) = match size {
                    Some(s) => (s as u64, "not_captured"),
                    None => (0, "none"),
                };
                out.push(body_end(key, BodyDir::Response, body_done, total, 0, state));
            }
        }
    }
    if let Some(d) = delivered.filter(|_| status > 0) {
        let at = first_byte.unwrap_or(end);
        let rules = tp["rules"]
            .as_array()
            .map(|hits| {
                let mut ids: Vec<RuleRef> = Vec::new();
                for id in hits.iter().flat_map(|h| h["rules"].as_array().cloned().unwrap_or_default()) {
                    if let Some(id) = id.as_str()
                        && !ids.iter().any(|r| r.id == id)
                    {
                        ids.push(RuleRef { id: id.to_string(), name: None });
                    }
                }
                ids
            })
            .unwrap_or_default();
        let headers = name_values(&d["headers"]);
        out.push(SessionEvent::RuleApplied(Box::new(RuleApplied {
            key,
            at,
            rules,
            changes: Vec::new(),
            delivered: Some(msg::DeliveredResponse {
                status: d["status"].as_u64().unwrap_or(0).min(u64::from(u16::MAX)) as u16,
                message: d["statusText"].as_str().unwrap_or("").to_string(),
                headers,
            }),
        })));
        if let Some(b) = body_bytes(&d["content"])
            && d["content"] != resp["content"]
        {
            let n = b.len() as u64;
            out.push(SessionEvent::Body { key, dir: BodyDir::Delivered, at, offset: 0, bytes: b });
            out.push(body_end(key, BodyDir::Delivered, at, n, n, "complete"));
        }
    }
    for (name, at) in later {
        out.push(SessionEvent::Mark { key, at, name });
    }

    // how it ended
    let failure = &tp["failure"];
    let error = [&resp["_error"], &e["_error"]].into_iter().find_map(|v| v.as_str()).filter(|s| !s.is_empty());
    // still running when the file was written: ended by the end of the file, as a detach
    let unfinished = matches!(tp["state"].as_str(), Some("detached" | "sending" | "waiting" | "receiving"));
    if unfinished {
        // the source's end marks it
    } else if let Some(class) = failure["class"].as_str() {
        out.push(SessionEvent::Failed(Box::new(Failed {
            key,
            at: end,
            phase: failure["phase"].as_str().map(str::to_string),
            canceled: failure["canceled"] == Value::Bool(true),
            simulated: failure["simulated"] == Value::Bool(true),
            error: ErrorInfo {
                class: class.to_string(),
                message: failure["message"].as_str().map(str::to_string),
                causes: Vec::new(),
            },
            conn: None,
        })));
    } else if status > 0 && error.is_none() {
        out.push(SessionEvent::Completed { key, at: end });
    } else {
        out.push(SessionEvent::Failed(Box::new(Failed {
            key,
            at: end,
            phase: None,
            canceled: false,
            simulated: false,
            error: ErrorInfo {
                class: error.unwrap_or("no response").to_string(),
                message: Some("as recorded in the HAR file".into()),
                causes: Vec::new(),
            },
            conn: None,
        })));
    }
    EntryEnd { end, unfinished }
}

fn body_end(key: TxnKey, dir: BodyDir, at: Ts, total: u64, captured: u64, state: &str) -> SessionEvent {
    SessionEvent::BodyEnd { key, dir, at, total, captured, state: state.into() }
}

impl SessionStore {
    /// The headers to decode one of `t`'s bodies with: as captured, except for HAR imports,
    /// whose bodies are stored decoded, so their Content-Encoding is left out. (The headers
    /// shown are unchanged.)
    pub fn decoding_headers<'a>(
        &self,
        t: &crate::model::Transaction,
        headers: Option<&'a Headers>,
    ) -> Option<std::borrow::Cow<'a, Headers>> {
        let h = headers?;
        let imported = self.source(t.key.source).is_some_and(|s| s.mode == "har");
        if imported && h.iter().any(|(n, _)| n.eq_ignore_ascii_case("content-encoding")) {
            Some(std::borrow::Cow::Owned(
                h.iter().filter(|(n, _)| !n.eq_ignore_ascii_case("content-encoding")).cloned().collect(),
            ))
        } else {
            Some(std::borrow::Cow::Borrowed(h))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::har::har as export_har;
    use crate::model::TxnState;
    use serde_json::json;

    fn store_of(opened: &Opened) -> SessionStore {
        let mut s = SessionStore::new();
        s.apply_all(opened.events.iter().cloned());
        for &k in &opened.pins {
            let i = s.find(k).unwrap();
            s.set_pinned(i, true);
        }
        s
    }

    /// A HAR as a browser writes it: no `_trafficPolice`, decoded bodies with their
    /// Content-Encoding header, a failed entry, and an entry without a date.
    fn browser_har() -> Value {
        json!({ "log": { "version": "1.2", "creator": { "name": "WebInspector", "version": "537.36" }, "entries": [
            { "startedDateTime": "2026-09-29T10:40:01.000+05:30", "time": 120.5,
              "request": { "method": "POST", "url": "https://api.example.com/login", "httpVersion": "http/2.0",
                "headers": [{ "name": "content-type", "value": "application/json" }],
                "postData": { "mimeType": "application/json", "text": "{\"user\":\"a\"}" } },
              "response": { "status": 200, "statusText": "", "httpVersion": "http/2.0",
                "headers": [{ "name": "content-encoding", "value": "gzip" }, { "name": "content-type", "value": "application/json" }],
                "content": { "size": 11, "mimeType": "application/json", "text": "{\"ok\":true}" } },
              "timings": { "blocked": 0.5, "dns": 10, "connect": 30, "ssl": 20, "send": 0.5, "wait": 70, "receive": 9.5 },
              "serverIPAddress": "[2001:db8::1]", "connection": "443" },
            { "startedDateTime": "2026-09-29T10:40:00.500+05:30", "time": 5,
              "request": { "method": "GET", "url": "https://cdn.example.com/logo.png", "headers": [] },
              "response": { "status": 200, "headers": [{ "name": "content-type", "value": "image/png" }],
                "content": { "size": 4, "mimeType": "image/png", "text": "iVBORw==", "encoding": "base64" } },
              "timings": { "send": 1, "wait": 3, "receive": 1 } },
            { "startedDateTime": "2026-09-29T10:40:02.000+05:30", "time": 3,
              "request": { "method": "GET", "url": "https://gone.example.com/", "headers": [] },
              "response": { "status": 0, "headers": [], "content": { "size": 0 }, "_error": "net::ERR_NAME_NOT_RESOLVED" },
              "timings": { "send": 0, "wait": 3, "receive": 0 } },
            { "request": { "method": "GET", "url": "https://x.example.com/" } }
        ] } })
    }

    #[test]
    fn a_browser_har_opens_with_timings_bodies_and_failures() {
        let bytes = serde_json::to_vec(&browser_har()).unwrap();
        let opened = har(&bytes, &SourceIds::default()).unwrap();
        assert_eq!(opened.skipped, vec!["entry 4: no startedDateTime".to_string()]);
        let s = store_of(&opened);
        assert_eq!(s.len(), 3);
        let src = s.sources().next().unwrap();
        assert_eq!((src.mode.as_str(), src.process.as_str()), ("har", "from WebInspector 537.36"));
        // in time order: the image started first
        let img = s.txn(0);
        assert_eq!(img.url.host, "cdn.example.com");
        assert_eq!(&s.body_bytes(&img.resp_body)[..], b"\x89PNG");
        assert_eq!(s.wall_ms(img.start), Some(1_790_658_600_500));

        let login = s.txn(1);
        assert_eq!(login.state, TxnState::Complete);
        assert_eq!(login.resp.as_ref().unwrap().protocol.as_deref(), Some("h2"));
        assert_eq!(&s.body_bytes(&login.req_body)[..], b"{\"user\":\"a\"}");
        assert_eq!(login.duration(s.latest()), 120_500_000);
        let p = login.phases(s.latest());
        assert_eq!(p.dns.map(|(a, b)| b - a), Some(10_000_000));
        assert_eq!(p.connect.map(|(a, b)| b - a), Some(30_000_000));
        assert_eq!(p.tls.map(|(a, b)| b - a), Some(20_000_000));
        assert_eq!(p.wait.map(|(a, b)| b - a), Some(70_000_000));
        assert_eq!(p.receive.map(|(a, b)| b - a), Some(9_500_000));
        assert_eq!(login.conn.as_ref().unwrap().remote.as_ref().unwrap().ip, "2001:db8::1");
        // stored decoded: decoding leaves out the recorded gzip
        let headers = login.resp.as_ref().map(|r| &r.headers);
        assert_eq!(headers.unwrap().len(), 2, "shown as recorded");
        let d = crate::decode::decode_body(
            s.body_bytes(&login.resp_body),
            s.decoding_headers(login, headers).as_deref(),
            1 << 20,
        );
        assert_eq!((d.error, &d.bytes[..]), (None, &b"{\"ok\":true}"[..]));

        let gone = s.txn(2);
        assert_eq!(gone.state, TxnState::Failed);
        assert_eq!(gone.failure.as_ref().unwrap().class, "net::ERR_NAME_NOT_RESOLVED");
    }

    #[test]
    fn our_own_har_comes_back_with_thread_stack_pins_and_marks() {
        // a transaction as captured, exported, then imported
        let bytes = serde_json::to_vec(&browser_har()).unwrap();
        let first = store_of(&har(&bytes, &SourceIds::default()).unwrap());
        let mut exported = export_har(&first, &[1, 2], first.latest());
        let tp = &mut exported["log"]["entries"][0]["_trafficPolice"];
        tp["thread"] = json!({ "name": "worker-3", "id": 42, "origin": "call" });
        tp["stack"] = json!([{ "class": "com.example.Api", "method": "login", "file": "Api.kt", "line": 12 }]);
        tp["client"] = json!("OkHttp 4.12.0");
        tp["pinned"] = json!(true);
        let again = har(&serde_json::to_vec(&exported).unwrap(), &SourceIds::default()).unwrap();
        let s = store_of(&again);
        assert_eq!(s.len(), 2);
        let login = s.txn(0);
        assert!(login.pinned);
        assert_eq!(login.thread.as_ref().unwrap().name, "worker-3");
        assert_eq!(login.stack[0].c, "com.example.Api");
        assert_eq!(login.client.as_ref().unwrap().label(), "OkHttp 4.12.0");
        let (a, b) = (first.txn(1), login);
        assert_eq!(a.phases(first.latest()), {
            let p = b.phases(s.latest());
            // same spans, shifted to the new file's start
            let shift = |x: Option<(Ts, Ts)>| x.map(|(u, v)| (u - b.start + a.start, v - b.start + a.start));
            crate::phases::Phases {
                queued: shift(p.queued),
                dns: shift(p.dns),
                connect: shift(p.connect),
                tls: shift(p.tls),
                send: shift(p.send),
                wait: shift(p.wait),
                receive: shift(p.receive),
            }
        });
        assert_eq!(s.txn(1).failure.as_ref().unwrap().class, "net::ERR_NAME_NOT_RESOLVED");
    }

    #[test]
    fn not_a_har() {
        for bad in [&b"[]"[..], b"{\"log\":{}}", b"{", b"\xef\xbb\xbf{\"x\":1}"] {
            assert!(har(bad, &SourceIds::default()).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let empty = har(b"{\"log\":{\"entries\":[]}}", &SourceIds::default()).unwrap();
        assert!(empty.events.is_empty());
    }
}
