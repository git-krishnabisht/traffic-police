//! Inputs the fuzz targets (`host/fuzz`) once crashed on, kept in `testdata/fuzz/<target>/` and
//! run here on every build (stable, no fuzzer needed).

use std::path::PathBuf;

use traffic_police_core::SessionEvent;
use traffic_police_core::model::SourceInfo;
use traffic_police_core::normalize::Normalizer;
use traffic_police_core::store::SessionStore;
use traffic_police_proto::{Decoder, DeviceMsg, msg};

const HELLO: &[u8] = br#"{"t":"hello","protocol":1,"runtime":{"version":"0.1.0","mode":"library"},"instance":"fuzz","app":{"package":"com.example","process":"com.example","pid":1},"device":{"api":36},"clock":{"ts":1000000000,"wall_ms":1790000000000}}"#;

fn inputs(target: &str) -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/fuzz").join(target);
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read(&p).unwrap()))
        .collect();
    out.sort();
    out
}

/// The `device_stream` target: bytes after a `hello`, decoded, normalized, applied, read back.
fn device_stream(data: &[u8]) {
    let mut store = SessionStore::new();
    let id = store.source_ids().next();
    let Ok(DeviceMsg::Hello(hello)) = msg::parse_device(HELLO) else { panic!("the hello parses") };
    store.apply(SessionEvent::SourceUp(Box::new(SourceInfo::from_hello(id, &hello, "fuzz".into(), None))));
    let mut normalizer = Normalizer::new(id);
    let mut decoder = Decoder::new();
    decoder.push(data);
    let mut events = Vec::new();
    loop {
        match decoder.next_frame() {
            Ok(Some(frame)) => {
                let _ = normalizer.frame(frame, &mut events);
            }
            Ok(None) => break,
            Err(e) if e.is_fatal() => break,
            Err(_) => {}
        }
    }
    for e in events {
        store.apply(e);
    }
    let now = store.latest();
    for t in store.txns() {
        let _ = t.phases(now);
        let _ = traffic_police_core::decode::decode_body(
            store.body_bytes(&t.resp_body),
            t.resp.as_ref().map(|r| &r.headers),
            1 << 20,
        );
    }
    let all: Vec<u32> = (0..store.len() as u32).collect();
    let _ = traffic_police_core::export::har::har(&store, &all, now);
}

/// Body chunks and progress at offsets near the top of `u64`, and byte counts that add up past
/// it: up to 0.3.1 the store's arithmetic overflowed (a panic in debug builds, a wrong total in
/// release ones).
#[test]
fn device_streams_that_once_crashed() {
    let all = inputs("device_stream");
    assert!(all.len() >= 3);
    for (name, data) in all {
        let r = std::panic::catch_unwind(|| device_stream(&data));
        assert!(r.is_ok(), "{name} panics again");
    }
}

#[test]
fn device_times_past_the_limit_are_refused() {
    let at = |ts: u64| format!(r#"{{"t":"done","seq":1,"ts":{ts},"txn":1}}"#);
    assert!(msg::parse_device(at(msg::MAX_DEVICE_TS).as_bytes()).is_ok());
    let e = msg::parse_device(at(msg::MAX_DEVICE_TS + 1).as_bytes()).unwrap_err();
    assert!(e.to_string().contains("out of range"), "{e}");
}
