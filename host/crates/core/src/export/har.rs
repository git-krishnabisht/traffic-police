//! HAR 1.2 (ARCHITECTURE.md §5.10): the standard fields, plus `_trafficPolice` on each entry for
//! what HAR has no place for (thread, call stack, capture state, failure, rules, source).

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

fn timing(p: Option<(Ts, Ts)>, required: bool) -> Value {
    match p {
        Some((a, b)) => json!(ms(b.saturating_sub(a))),
        None if required => json!(0),
        None => json!(-1),
    }
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

    let p = t.phases(now);
    let timings = json!({
        "blocked": timing(p.queued, false),
        "dns": timing(p.dns, false),
        "connect": timing(p.connect, false),
        "ssl": timing(p.tls, false),
        "send": timing(p.send, true),
        "wait": timing(p.wait, true),
        "receive": timing(p.receive, true),
    });

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
        extra.insert("thread".into(), json!({ "name": th.name, "id": th.id, "origin": th.origin }));
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
        "time": ms(t.duration(now)),
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
    e
}

/// A HAR document for these transactions (in the order given).
pub fn har(store: &SessionStore, txns: &[TxnIdx], now: Ts) -> Value {
    let entries: Vec<Value> = txns.iter().map(|&i| entry(store, store.txn(i), now)).collect();
    json!({
        "log": {
            "version": "1.2",
            "creator": { "name": "traffic-police", "version": env!("CARGO_PKG_VERSION") },
            "pages": [],
            "entries": entries,
        }
    })
}
