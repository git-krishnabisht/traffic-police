//! The lines of `traffic-police tail`: JSON (PROTOCOL.md Appendix B, schema version 1) or text.

use base64::Engine;
use serde_json::{Value, json};

use crate::decode::decode_body;
use crate::fmt::{self, NS_PER_MS, Ts, iso8601};
use crate::model::{BodyMeta, Headers, Transaction, TxnIdx, TxnState, header};
use crate::store::SessionStore;

fn ms(ns: u64) -> f64 {
    (ns as f64 / NS_PER_MS as f64 * 10.0).round() / 10.0
}

fn span_ms(p: Option<(Ts, Ts)>) -> Value {
    p.map_or(json!(-1), |(a, b)| json!(ms(b.saturating_sub(a))))
}

fn pairs(h: &Headers) -> Value {
    Value::Array(h.iter().map(|(n, v)| json!([n, v])).collect())
}

fn body(store: &SessionStore, t: &Transaction, meta: &BodyMeta, headers: Option<&Headers>) -> Option<(Value, usize)> {
    meta.id?;
    let d = decode_body(store.body_bytes(meta), store.decoding_headers(t, headers).as_deref(), 256 << 20);
    let n = d.bytes.len();
    Some(match std::str::from_utf8(&d.bytes) {
        Ok(s) => (json!({ "text": s }), n),
        Err(_) => (json!({ "base64": base64::engine::general_purpose::STANDARD.encode(&d.bytes) }), n),
    })
}

/// Whether a transaction has finished (its line is due).
pub fn finished(t: &Transaction) -> bool {
    matches!(t.state, TxnState::Complete | TxnState::Failed | TxnState::Detached)
}

/// One `{"v":1,"type":"txn",…}` line for a finished transaction.
pub fn json_line(store: &SessionStore, i: TxnIdx, now: Ts, bodies: bool) -> Value {
    let t = store.txn(i);
    let src = store.source(t.key.source);
    let mut request = json!({ "headers": pairs(&t.req_headers), "body_bytes": t.req_body.total });
    if bodies && let Some((b, _)) = body(store, t, &t.req_body, Some(&t.req_headers)) {
        request["body"] = b;
    }
    let response = t.resp.as_ref().map(|r| {
        let decoded = body(store, t, &t.resp_body, Some(&r.headers));
        let mut v = json!({
            "protocol": r.protocol,
            "headers": pairs(&r.headers),
            "body_bytes": t.resp_body.total,
            "decoded_bytes": decoded.as_ref().map_or(0, |(_, n)| *n),
            "content_type": header(&r.headers, "content-type"),
        });
        if bodies && let Some((b, _)) = decoded {
            v["body"] = b;
        }
        v
    });
    let p = t.phases(now);
    let mut line = json!({
        "v": 1,
        "type": "txn",
        "id": format!("{}:{}", t.key.source, t.key.txn),
        "source": src.map(|s| json!({ "serial": s.serial, "package": s.package, "process": s.process, "pid": s.pid })),
        "start": store.wall_ms(t.start).map(iso8601),
        "start_rel_ms": ms(t.start.saturating_sub(store.origin())),
        "duration_ms": ms(t.duration(now)),
        "method": t.method,
        "url": t.url.raw,
        "status": t.status(),
        "state": format!("{:?}", t.state).to_lowercase(),
        "request": request,
        "response": response,
        "timing": {
            "queued": span_ms(p.queued), "dns": span_ms(p.dns), "connect": span_ms(p.connect),
            "ssl": span_ms(p.tls), "send": span_ms(p.send), "wait": span_ms(p.wait), "receive": span_ms(p.receive),
        },
        "thread": t.thread.as_ref().map(|th| json!({ "name": th.name, "origin": th.origin })),
        "stack": t.stack.iter().map(|f| {
            let at = match (&f.f, f.l) {
                (Some(file), Some(line)) if line >= 0 => format!("{file}:{line}"),
                (Some(file), _) => file.clone(),
                (None, _) => "Unknown Source".to_string(),
            };
            format!("{}.{}({at})", f.c, f.m)
        }).collect::<Vec<_>>(),
        "client": t.client.as_ref().map(|c| match &c.version {
            Some(v) => format!("{}/{v}", c.kind),
            None => c.kind.clone(),
        }),
        "rules": t.rules.iter().flat_map(|h| h.rules.iter().map(|r| r.id.clone())).collect::<Vec<_>>(),
    });
    if t.stack_truncated {
        line["stack_truncated"] = json!(true);
    }
    if let Some(f) = &t.failure {
        line["failure"] = json!({ "class": f.class, "message": f.message, "phase": f.phase, "canceled": f.canceled });
    }
    line
}

/// One line for people: `07:10:01.123  200  GET      305 ms    225 B  https://…`, with the
/// failure or the end of the source after the URL.
pub fn text_line(store: &SessionStore, i: TxnIdx, now: Ts) -> String {
    let t = store.txn(i);
    let at = match store.wall_ms(t.start) {
        Some(ms) => fmt::wall_clock(ms),
        None => format!("+{}", fmt::offset(t.start.saturating_sub(store.origin()))),
    };
    let status = match (t.status(), t.state) {
        (Some(s), _) => s.to_string(),
        (None, TxnState::Failed) => "ERR".to_string(),
        (None, _) => "---".to_string(),
    };
    let size = if t.resp.is_some() { fmt::bytes(t.response_size()) } else { "-".to_string() };
    let mut line =
        format!("{at}  {status:>3}  {:<7}  {:>8}  {size:>8}  {}", t.method, fmt::duration(t.duration(now)), t.url.raw);
    if let Some(f) = &t.failure {
        line.push_str(&format!("  ✕ {}", f.short_class()));
        if let Some(m) = &f.message {
            line.push_str(&format!(": {m}"));
        }
    } else if t.state == TxnState::Detached {
        line.push_str("  (unfinished: the app went away)");
    }
    if t.rule_modified() {
        let ids: Vec<&str> = t.rules.iter().flat_map(|h| h.rules.iter().map(|r| r.id.as_str())).collect();
        line.push_str(&format!("  ✎ {}", ids.join(",")));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEvent;
    use crate::model::SourceInfo;
    use crate::normalize::Normalizer;
    use bytes::Bytes;
    use traffic_police_proto::frame::Frame;
    use traffic_police_proto::msg::Hello;
    use traffic_police_proto::{BodyChunk, BodyDir};

    fn store(frames: Vec<Value>, body: &[u8]) -> SessionStore {
        let hello: Hello = serde_json::from_value(json!({
            "t": "hello", "protocol": 1, "runtime": { "version": "0.1.0", "mode": "library" },
            "instance": "i", "app": { "package": "com.example", "process": "com.example", "pid": 4312 },
            "device": { "api": 36 }, "clock": { "ts": 1_000_000_000u64, "wall_ms": 1_790_000_000_000i64 },
        }))
        .unwrap();
        let mut s = SessionStore::new();
        let info = SourceInfo::from_hello(0, &hello, "Pixel 8".into(), Some("emulator-5554".into()));
        s.apply(SessionEvent::SourceUp(Box::new(info)));
        let mut n = Normalizer::new(0);
        let mut events = Vec::new();
        for f in frames {
            if f["t"] == "done" && !body.is_empty() {
                // just before `done`, in sequence
                let seq = f["seq"].as_u64().unwrap() - 1;
                let data = Bytes::copy_from_slice(body);
                let c = BodyChunk { seq, txn: 1, dir: BodyDir::Response, ts: 1_200_000_000, offset: 0, data };
                n.frame(Frame::Body(c), &mut events).unwrap();
            }
            n.frame(Frame::Json(Bytes::from(serde_json::to_vec(&f).unwrap())), &mut events).unwrap();
        }
        s.apply_all(events);
        s
    }

    fn req() -> Value {
        json!({ "t": "req", "seq": 1, "ts": 1_000_000_000u64, "txn": 1, "method": "GET",
            "url": "https://api.example.com/status?id=7", "headers": [["Accept", "*/*"]],
            "client": { "kind": "okhttp", "version": "4.12.0" },
            "thread": { "name": "worker-3", "id": 42, "origin": "call" },
            "stack": [{ "c": "com.example.Api", "m": "status", "f": "Api.kt", "l": 12 }, { "c": "com.example.Main", "m": "run" }] })
    }

    #[test]
    fn a_finished_request_as_json() {
        let s = store(
            vec![
                req(),
                json!({ "t": "resp", "seq": 2, "ts": 1_150_000_000u64, "txn": 1, "status": 200, "protocol": "h2",
                    "headers": [["content-type", "application/json"]] }),
                json!({ "t": "done", "seq": 4, "ts": 1_305_200_000u64, "txn": 1 }),
            ],
            b"{\"ok\":true}",
        );
        let v = json_line(&s, 0, s.latest(), true);
        assert_eq!(v["v"], 1);
        assert_eq!(v["id"], "0:1");
        assert_eq!(v["source"]["package"], "com.example");
        assert_eq!(v["source"]["serial"], "emulator-5554");
        assert_eq!(v["duration_ms"], 305.2);
        assert_eq!(v["status"], 200);
        assert_eq!(v["state"], "complete");
        assert_eq!(v["request"]["headers"], json!([["Accept", "*/*"]]));
        assert_eq!(v["response"]["body"], json!({ "text": "{\"ok\":true}" }));
        assert_eq!(v["response"]["content_type"], "application/json");
        assert_eq!(v["thread"]["name"], "worker-3");
        assert_eq!(v["client"], "okhttp/4.12.0");
        assert_eq!(v["stack"], json!(["com.example.Api.status(Api.kt:12)", "com.example.Main.run(Unknown Source)"]));
        assert!(v["start"].as_str().unwrap().starts_with("2026-"), "{}", v["start"]);
        assert!(v.get("failure").is_none());
        // without --bodies there are only sizes
        let v = json_line(&s, 0, s.latest(), false);
        assert!(v["response"].get("body").is_none());
        assert_eq!(v["response"]["decoded_bytes"], 11);
    }

    #[test]
    fn a_failed_request_as_json_and_text() {
        let s = store(
            vec![
                req(),
                json!({ "t": "fail", "seq": 2, "ts": 1_012_000_000u64, "txn": 1, "phase": "connect",
                    "error": { "class": "java.net.UnknownHostException", "message": "api.example.com" } }),
            ],
            b"",
        );
        assert!(finished(s.txn(0)));
        let v = json_line(&s, 0, s.latest(), false);
        assert_eq!(v["state"], "failed");
        assert_eq!(v["response"], Value::Null);
        assert_eq!(v["failure"]["class"], "java.net.UnknownHostException");
        assert_eq!(v["failure"]["phase"], "connect");
        let text = text_line(&s, 0, s.latest());
        assert!(
            text.ends_with(
                "ERR  GET         12 ms         -  https://api.example.com/status?id=7  ✕ UnknownHostException: api.example.com"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_text_line() {
        let s = store(
            vec![
                req(),
                json!({ "t": "resp", "seq": 2, "ts": 1_150_000_000u64, "txn": 1, "status": 404 }),
                json!({ "t": "done", "seq": 4, "ts": 1_305_000_000u64, "txn": 1 }),
            ],
            b"missing",
        );
        let text = text_line(&s, 0, s.latest());
        // the time of day depends on the local offset; the rest does not
        assert_eq!(&text[12..], "  404  GET        305 ms       7 B  https://api.example.com/status?id=7", "{text}");
    }
}
