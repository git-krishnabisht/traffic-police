//! What a capture runtime could send after its `hello`, all the way in: decoded, normalized,
//! applied to a session store, and read back as the UI and the exports read it. Noise never
//! panics anywhere on the way.

#![no_main]

use libfuzzer_sys::fuzz_target;
use traffic_police_core::SessionEvent;
use traffic_police_core::model::SourceInfo;
use traffic_police_core::normalize::Normalizer;
use traffic_police_core::store::SessionStore;
use traffic_police_proto::{Decoder, DeviceMsg, msg};

const HELLO: &[u8] = br#"{"t":"hello","protocol":1,"runtime":{"version":"0.1.0","mode":"library"},"instance":"fuzz","app":{"package":"com.example","process":"com.example","pid":1},"device":{"api":36},"clock":{"ts":1000000000,"wall_ms":1790000000000}}"#;

fuzz_target!(|data: &[u8]| {
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
        let _ = t.segments(now);
        let _ = traffic_police_core::decode::decode_body(store.body_bytes(&t.resp_body), t.resp.as_ref().map(|r| &r.headers), 1 << 20);
    }
    let all: Vec<u32> = (0..store.len() as u32).collect();
    let _ = traffic_police_core::export::har::har(&store, &all, now);
});
