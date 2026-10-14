//! HAR 1.2 (ARCHITECTURE.md §5.10): the standard fields, plus `_trafficPolice` on each entry for
//! what HAR has no place for (thread, call stack, capture state, failure, rules, source).

use std::io::{self, Write};

use base64::Engine;
use serde_json::{Map, Value, json};

use crate::decode::decode_body;
use crate::fmt::{NS_PER_MS, Ts, iso8601};
use crate::model::{BodyMeta, Headers, Transaction, TxnIdx, header};
use crate::store::SessionStore;

fn ms(ns: u64) -> f64 {
    (ns as f64 / NS_PER_MS as f64 * 1000.0).round() / 1000.0
}

fn http_version(t: &Transaction) -> String {
    match t.resp.as_ref().and_then(|r| r.protocol.as_deref()).unwrap_or("") {
        "h2" | "h2_prior_knowledge" => "HTTP/2".into(),
        "h3" | "quic" => "HTTP/3".into(),
        "http/1.0" => "HTTP/1.0".into(),
        _ => "HTTP/1.1".into(),
    }
}

fn name_values(pairs: impl IntoIterator<Item = (String, String)>) -> Value {
    Value::Array(pairs.into_iter().map(|(n, v)| json!({ "name": n, "value": v })).collect())
}

fn request_cookies(headers: &Headers) -> Value {
    name_values(
        headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("cookie"))
            .flat_map(|(_, v)| v.split(';'))
            .filter_map(|c| c.trim().split_once('=').map(|(n, v)| (n.to_string(), v.to_string()))),
    )
}

fn response_cookies(headers: &Headers) -> Value {
    name_values(
        headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("set-cookie")).filter_map(|(_, v)| {
            v.split(';').next()?.trim().split_once('=').map(|(n, v)| (n.to_string(), v.to_string()))
        }),
    )
}

/// A body for HAR: decoded; UTF-8 as text, anything else in base64.
fn body_text(
    store: &SessionStore,
    t: &Transaction,
    meta: &BodyMeta,
    headers: Option<&Headers>,
) -> Option<(String, bool, usize)> {
    meta.id?;
    let d = decode_body(store.body_bytes(meta), store.decoding_headers(t, headers).as_deref(), 256 << 20);
    let size = d.bytes.len();
    Some(match std::str::from_utf8(&d.bytes) {
        Ok(s) => (s.to_string(), false, size),
        Err(_) => (base64::engine::general_purpose::STANDARD.encode(&d.bytes), true, size),
    })
}

/// HAR's `time` and `timings`, which HAR 1.2 requires to add up: `time` is the sum of the
/// timings that apply (`ssl` is counted inside `connect`). The phases come from the marks, each
/// cut to start where the one before ended (OkHttp 5 may connect while it still resolves); a
/// request that failed or was cancelled before an answer spent its last part waiting; and
/// `blocked` is what no phase covers (queueing, picking a connection, the end of the call).
/// Up to 0.3.1 the timings were the bare phases, so the time between them was in no timing, and a
/// timeout's ten seconds were in none at all.
fn timings(t: &Transaction, now: Ts) -> (f64, Value) {
    let p = t.phases(now);
    let seg = t.segments(now);
    let (begin, end) = (seg.start, seg.end);
    let mut cursor = begin;
    let mut take = |span: Option<(Ts, Ts)>| -> Option<f64> {
        let (a, b) = span?;
        let a = a.max(cursor).min(end);
        let b = b.max(a).min(end);
        cursor = b;
        Some(ms(b - a))
    };
    let dns = take(p.dns);
    let connect = take(p.connect);
    let send = take(p.send.or(Some((seg.sent, seg.sent)))).unwrap_or(0.0);
    let wait = take(Some((seg.sent, seg.first_byte.unwrap_or(end)))).unwrap_or(0.0);
    let receive = seg.first_byte.and_then(|f| take(Some((f, end)))).unwrap_or(0.0);
    // TLS is part of connect
    let ssl = match (p.tls, p.connect) {
        (Some((a, b)), Some((ca, cb))) => {
            let (a, b) = (a.max(ca), b.min(cb));
            (b > a).then(|| ms(b - a)).map(|x| x.min(connect.unwrap_or(0.0)))
        }
        _ => None,
    };
    let time = ms(end - begin);
    let phases = dns.unwrap_or(0.0) + connect.unwrap_or(0.0) + send + wait + receive;
    let blocked = ((time - phases) * 1000.0).round().max(0.0) / 1000.0;
    let or_none = |x: Option<f64>| x.map_or(json!(-1), |v| json!(v));
    let timings = json!({
        "blocked": blocked,
        "dns": or_none(dns),
        "connect": or_none(connect),
        "ssl": or_none(ssl),
        "send": send,
        "wait": wait,
        "receive": receive,
    });
    (time, timings)
}

fn entry(store: &SessionStore, t: &Transaction, now: Ts) -> Value {
    let started = store.wall_ms(t.start).map_or_else(|| iso8601(0), iso8601);
    let mime = |h: Option<&Headers>| h.and_then(|h| header(h, "content-type")).unwrap_or("x-unknown").to_string();

    let mut request = json!({
        "method": t.method,
        "url": t.url.raw,
        "httpVersion": http_version(t),
        "cookies": request_cookies(&t.req_headers),
        "headers": name_values(t.req_headers.clone()),
        "queryString": name_values(t.url.query_pairs()),
        "headersSize": -1,
        "bodySize": t.req_body.total,
    });
    if let Some((text, b64, _)) = body_text(store, t, &t.req_body, Some(&t.req_headers)) {
        let mut post = json!({ "mimeType": mime(Some(&t.req_headers)), "text": text });
        if b64 {
            post["encoding"] = json!("base64");
        }
        request["postData"] = post;
    }

    // the response as the network gave it
    let resp_headers = t.resp.as_ref().map(|r| &r.headers);
    let content_of = |meta: &BodyMeta, headers: Option<&Headers>| {
        let mut content = json!({ "size": meta.total, "mimeType": mime(headers) });
        if let Some((text, b64, size)) = body_text(store, t, meta, headers) {
            content["size"] = json!(size);
            content["compression"] = json!(size as i64 - meta.captured as i64);
            content["text"] = json!(text);
            if b64 {
                content["encoding"] = json!("base64");
            }
        }
        content
    };
    let content = content_of(&t.resp_body, resp_headers);
    let original = json!({
        "status": t.resp.as_ref().map_or(0, |r| r.status),
        "statusText": t.resp.as_ref().map_or(String::new(), |r| r.message.clone()),
        "httpVersion": http_version(t),
        "cookies": resp_headers.map_or(json!([]), response_cookies),
        "headers": name_values(resp_headers.cloned().unwrap_or_default()),
        "content": content,
        "redirectURL": resp_headers.and_then(|h| header(h, "location")).unwrap_or(""),
        "headersSize": -1,
        "bodySize": if t.resp.is_some() { json!(t.resp_body.total) } else { json!(-1) },
    });
    // what the app received: the original, unless a rule changed it (then the original is kept
    // in _trafficPolice.original)
    let (response, replaced) = match &t.delivered {
        Some(d) => {
            let mut r = original.clone();
            r["status"] = json!(d.status);
            r["statusText"] = json!(d.message);
            r["headers"] = name_values(d.headers.clone());
            r["cookies"] = response_cookies(&d.headers);
            r["redirectURL"] = json!(header(&d.headers, "location").unwrap_or(""));
            if let Some(meta) = &t.delivered_body {
                r["content"] = content_of(meta, Some(&d.headers));
                r["bodySize"] = json!(meta.total);
            }
            (r, Some(original))
        }
        None => (original, None),
    };

    let (time, timings) = timings(t, now);

    let mut extra = Map::new();
    if let Some(s) = store.source(t.key.source) {
        extra.insert(
            "source".into(),
            json!({ "device": s.device_label, "serial": s.serial, "package": s.package, "process": s.process, "pid": s.pid }),
        );
    }
    extra.insert("state".into(), json!(format!("{:?}", t.state).to_lowercase()));
    if let Some(c) = &t.client {
        extra.insert("client".into(), json!(c.label()));
    }
    if let Some(th) = &t.thread {
        let mut thread = json!({ "name": th.name, "id": th.id, "origin": th.origin });
        if let Some(tid) = th.tid {
            thread["tid"] = json!(tid);
        }
        extra.insert("thread".into(), thread);
    }
    if !t.stack.is_empty() {
        let frames: Vec<Value> =
            t.stack.iter().map(|f| json!({ "class": f.c, "method": f.m, "file": f.f, "line": f.l })).collect();
        extra.insert("stack".into(), Value::Array(frames));
    }
    if let Some(call) = t.call {
        extra.insert("call".into(), json!(call));
        extra.insert("hop".into(), json!(t.hop));
    }
    if let Some(f) = &t.failure {
        extra.insert(
            "failure".into(),
            json!({ "class": f.class, "message": f.message, "phase": f.phase, "canceled": f.canceled, "simulated": f.simulated }),
        );
    }
    // HAR has no trailers: a gRPC call's are here, with its status
    if !t.trailers.is_empty() {
        let pairs: Vec<Value> = t.trailers.iter().map(|(n, v)| json!({ "name": n, "value": v })).collect();
        extra.insert("trailers".into(), Value::Array(pairs));
    }
    if let Some(g) = &t.grpc {
        extra.insert("grpc".into(), json!({ "code": g.code, "status": g.name, "message": g.message }));
    }
    if !t.rules.is_empty() {
        let hits: Vec<Value> = t
            .rules
            .iter()
            .map(|h| json!({ "rules": h.rules.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), "changes": h.changes.len() }))
            .collect();
        extra.insert("rules".into(), Value::Array(hits));
    }
    if t.pinned {
        extra.insert("pinned".into(), json!(true));
    }
    if let Some(o) = replaced {
        extra.insert("original".into(), o);
    }
    let marks: Map<String, Value> =
        t.marks.iter().map(|(n, ts)| (n.clone(), json!(ms(ts.saturating_sub(t.start))))).collect();
    if !marks.is_empty() {
        extra.insert("marks_ms".into(), Value::Object(marks));
    }

    let mut e = json!({
        "startedDateTime": started,
        "time": time,
        "request": request,
        "response": response,
        "cache": {},
        "timings": timings,
        "_trafficPolice": Value::Object(extra),
    });
    if let Some(conn) = &t.conn {
        if let Some(r) = &conn.remote {
            e["serverIPAddress"] = json!(r.ip);
        }
        if let Some(id) = &conn.id {
            e["connection"] = json!(id);
        }
    }
    if !t.ws.is_empty() {
        e["_webSocketMessages"] = ws_messages(store, t);
    }
    e
}

/// A WebSocket's messages as Chrome's DevTools write them in a HAR (`_webSocketMessages`):
/// `type` send or receive, `time` in seconds since the epoch, `opcode` 1 (text), 2 (binary, the
/// data in base64) or 8 (close, the data its code and reason), and the data as captured.
fn ws_messages(store: &SessionStore, t: &Transaction) -> Value {
    let messages: Vec<Value> = t
        .ws
        .iter()
        .map(|m| {
            let (opcode, data) = match m.op.as_str() {
                "text" => (1, String::from_utf8_lossy(&m.data).into_owned()),
                "binary" => (2, base64::engine::general_purpose::STANDARD.encode(&m.data)),
                "close" => (
                    8,
                    format!(
                        "{} {}",
                        m.code.map(|c| c.to_string()).unwrap_or_default(),
                        m.reason.as_deref().unwrap_or("")
                    )
                    .trim()
                    .to_string(),
                ),
                _ => (0, String::new()),
            };
            let time = store.wall_ms(m.at).map_or(0.0, |ms| ms as f64 / 1000.0);
            let mut v =
                json!({ "type": if m.out { "send" } else { "receive" }, "time": time, "opcode": opcode, "data": data });
            if m.truncated {
                v["_size"] = json!(m.size);
            }
            v
        })
        .collect();
    Value::Array(messages)
}

fn creator() -> Value {
    json!({ "name": "traffic-police", "version": env!("CARGO_PKG_VERSION") })
}

/// A HAR document for these transactions (in the order given).
pub fn har(store: &SessionStore, txns: &[TxnIdx], now: Ts) -> Value {
    let entries: Vec<Value> = txns.iter().map(|&i| entry(store, store.txn(i), now)).collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": creator(),
            "pages": [],
            "entries": entries,
        }
    })
}

/// Writes the document [`har`] makes, as `serde_json::to_writer_pretty` writes it, one entry at
/// a time, so a big session's document is never all in memory (50,000 requests made 110 MB of
/// text, and a tree of values several times that). `stop` is asked before each entry; when it
/// says so the writing ends with an error of kind `Interrupted`.
pub fn write_har(
    store: &SessionStore,
    txns: &[TxnIdx],
    now: Ts,
    out: &mut impl Write,
    stop: &dyn Fn() -> bool,
) -> io::Result<()> {
    out.write_all(b"{\n  \"log\": {\n    \"version\": \"1.2\",\n    \"creator\": ")?;
    write_nested(out, &creator(), 4)?;
    out.write_all(b",\n    \"pages\": [],\n    \"entries\": [")?;
    for (n, &i) in txns.iter().enumerate() {
        if stop() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
        }
        out.write_all(if n == 0 { b"\n      " } else { b",\n      " })?;
        write_nested(out, &entry(store, store.txn(i), now), 6)?;
    }
    out.write_all(if txns.is_empty() { b"]\n  }\n}" } else { b"\n    ]\n  }\n}" })
}

/// `v` pretty-printed as a value `indent` columns in: every line after its first is moved over.
/// (A newline in JSON text is only ever between two lines: inside strings it is escaped.)
fn write_nested(out: &mut impl Write, v: &Value, indent: usize) -> io::Result<()> {
    let text = serde_json::to_vec_pretty(v)?;
    let pad = vec![b' '; indent];
    for (n, line) in text.split(|&b| b == b'\n').enumerate() {
        if n > 0 {
            out.write_all(b"\n")?;
            out.write_all(&pad)?;
        }
        out.write_all(line)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SourceIds;

    /// The document written entry by entry is the one `har` makes, byte for byte, printed.
    #[test]
    fn written_entry_by_entry_it_is_the_same_document() {
        let entries = json!({ "log": { "version": "1.2", "creator": { "name": "x", "version": "1" }, "entries": [
            { "startedDateTime": "2026-09-29T10:40:01.000+05:30", "time": 120.5,
              "request": { "method": "POST", "url": "https://api.example.com/login?a=1", "headers": [
                { "name": "content-type", "value": "application/json" }],
                "postData": { "mimeType": "application/json", "text": "{\"user\":\"a\\nb\"}" } },
              "response": { "status": 200, "headers": [{ "name": "content-type", "value": "application/json" }],
                "content": { "size": 11, "mimeType": "application/json", "text": "{\"ok\":true}" } },
              "timings": { "send": 0.5, "wait": 70, "receive": 9.5 } },
            { "startedDateTime": "2026-09-29T10:40:02.000+05:30", "time": 3,
              "request": { "method": "GET", "url": "https://gone.example.com/", "headers": [] },
              "response": { "status": 0, "headers": [], "content": { "size": 0 }, "_error": "refused" },
              "timings": { "send": 0, "wait": 3, "receive": 0 } }
        ] } });
        let opened = crate::import::har(&serde_json::to_vec(&entries).unwrap(), &SourceIds::default()).unwrap();
        let mut store = SessionStore::new();
        store.apply_all(opened.events);
        let now = store.latest();
        for txns in [vec![], vec![0], vec![1, 0]] {
            let mut written = Vec::new();
            write_har(&store, &txns, now, &mut written, &|| false).unwrap();
            let made = serde_json::to_vec_pretty(&har(&store, &txns, now)).unwrap();
            assert_eq!(String::from_utf8(written).unwrap(), String::from_utf8(made).unwrap(), "{txns:?}");
        }
        let err = write_har(&store, &[0, 1], now, &mut Vec::new(), &|| true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }
}
