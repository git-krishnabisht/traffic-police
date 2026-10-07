//! Ingest speed (ARCHITECTURE.md §6): a 50,000-event replay applied in under a second, and at
//! least 20,000 events a second sustained, measured from protocol bytes to the session store
//! (decode, normalize, apply), with a store that already holds a long session.
//!
//! Ignored by default because it is only meaningful in release builds:
//! `cargo test --release -p traffic-police-core --test ingest -- --ignored --nocapture`

use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use serde_json::json;
use traffic_police_core::SessionEvent;
use traffic_police_core::model::SourceInfo;
use traffic_police_core::normalize::Normalizer;
use traffic_police_core::store::SessionStore;
use traffic_police_proto::frame::{encode_body, encode_json};
use traffic_police_proto::{BodyChunk, BodyDir, Decoder, DeviceMsg, msg};

const HELLO: &[u8] = br#"{"t":"hello","protocol":1,"runtime":{"version":"0.1.0","mode":"library"},"instance":"ingest","app":{"package":"com.example","process":"com.example","pid":1},"device":{"api":36},"clock":{"ts":1000000000,"wall_ms":1790000000000}}"#;

/// Protocol bytes for `count` requests from `first` on: request, response, a 600-byte JSON body
/// chunk, its end, and done (five events each), 2 ms apart.
fn wire(first: u64, count: u64) -> BytesMut {
    let mut out = BytesMut::new();
    let body = Bytes::from(format!("{{\"items\":[{}]}}", "{\"id\":1,\"name\":\"item\"},".repeat(24) + "{}"));
    let paths = ["/api/v1/notifications", "/api/v1/orders/status", "/v1/events", "/api/v1/sessions"];
    for i in first..first + count {
        let ts = 2_000_000_000 + i * 2_000_000;
        let seq = i * 5;
        let url = format!("https://api.example.app{}?page={}", paths[(i % 4) as usize], i % 17);
        let msgs = [
            json!({"t": "req", "seq": seq + 1, "ts": ts, "txn": i + 1, "call": i + 1, "method": "GET", "url": url,
                "headers": [["Accept", "application/json"], ["User-Agent", "okhttp/4.12.0"]],
                "client": {"kind": "okhttp", "version": "4.12.0"},
                "thread": {"name": format!("DefaultDispatcher-worker-{}", i % 4), "id": 50 + i % 4, "origin": "call"},
                "stack": [{"c": "com.example.App", "m": "load", "f": "App.kt", "l": 12}],
                "marks": [["call_start", ts], ["conn_acquired", ts]]}),
            json!({"t": "resp", "seq": seq + 2, "ts": ts + 900_000, "txn": i + 1, "status": 200, "message": "OK",
                "protocol": "h2", "headers": [["Content-Type", "application/json"], ["Content-Length", body.len().to_string()]]}),
        ];
        for m in &msgs {
            encode_json(m.to_string().as_bytes(), &mut out);
        }
        encode_body(
            &BodyChunk {
                seq: seq + 3,
                txn: i + 1,
                dir: BodyDir::Response,
                ts: ts + 1_000_000,
                offset: 0,
                data: body.clone(),
            },
            &mut out,
        );
        for m in [
            json!({"t": "body_end", "seq": seq + 4, "ts": ts + 1_100_000, "txn": i + 1, "dir": "response",
                "bytes": body.len(), "captured": body.len(), "state": "complete"}),
            json!({"t": "done", "seq": seq + 5, "ts": ts + 1_100_000, "txn": i + 1}),
        ] {
            encode_json(m.to_string().as_bytes(), &mut out);
        }
    }
    out
}

/// Decodes, normalizes and applies `bytes`; returns the events applied.
fn ingest(store: &mut SessionStore, normalizer: &mut Normalizer, bytes: &[u8]) -> usize {
    let mut decoder = Decoder::new();
    decoder.push(bytes);
    let mut events = Vec::new();
    while let Some(frame) = decoder.next_frame().expect("valid frames") {
        normalizer.frame(frame, &mut events).expect("valid messages");
    }
    let n = events.len();
    store.apply_all(events);
    n
}

#[test]
#[ignore = "release builds only: cargo test --release -p traffic-police-core --test ingest -- --ignored"]
fn replay_and_sustained_ingest() {
    let mut store = SessionStore::new();
    let id = store.source_ids().next();
    let Ok(DeviceMsg::Hello(hello)) = msg::parse_device(HELLO) else { panic!("the hello parses") };
    store.apply(SessionEvent::SourceUp(Box::new(SourceInfo::from_hello(id, &hello, "test".into(), None))));
    let mut normalizer = Normalizer::new(id);

    // a replay burst: 10,000 requests, 50,000 events, at once
    let burst = wire(0, 10_000);
    let started = Instant::now();
    let n = ingest(&mut store, &mut normalizer, &burst);
    let replay = started.elapsed();
    assert_eq!(n, 50_000);
    println!("replay: {n} events in {replay:?}");
    assert!(replay < Duration::from_secs(1), "a 50,000-event replay took {replay:?}");

    // sustained: 40,000 more requests in batches of 200 events, as socket reads deliver them,
    // into a store that keeps growing
    let rest = wire(10_000, 40_000);
    let started = Instant::now();
    let mut applied = 0;
    let mut decoder = Decoder::new();
    let mut batch = Vec::with_capacity(256);
    for piece in rest.chunks(64 * 1024) {
        decoder.push(piece);
        while let Some(frame) = decoder.next_frame().expect("valid frames") {
            normalizer.frame(frame, &mut batch).expect("valid messages");
            if batch.len() >= 200 {
                applied += batch.len();
                store.apply_all(batch.drain(..));
            }
        }
    }
    applied += batch.len();
    store.apply_all(batch.drain(..));
    let took = started.elapsed();
    let rate = applied as f64 / took.as_secs_f64();
    println!("sustained: {applied} events in {took:?}, {rate:.0} events a second; {} requests stored", store.len());
    assert_eq!(store.len(), 50_000);
    assert!(rate > 20_000.0, "{rate:.0} events a second");
}
