//! A session file someone sent you (`traffic-police open`), damaged or made up: reading it never
//! panics, and neither does what reads the events it gave.

#![no_main]

use libfuzzer_sys::fuzz_target;
use traffic_police_core::store::SessionStore;

fuzz_target!(|data: &[u8]| {
    let mut store = SessionStore::new();
    // a valid header half the time, so the fuzzer gets past it to the gzip stream and the frames
    let mut bytes = Vec::with_capacity(data.len() + 8);
    if data.first().is_some_and(|b| b & 1 == 1) {
        bytes.extend_from_slice(b"TPSESS\x00\x01");
        bytes.extend_from_slice(&data[1..]);
    } else {
        bytes.extend_from_slice(data);
    }
    let Ok(opened) = traffic_police_core::session::read(&bytes[..], &store.source_ids()) else { return };
    for e in opened.events {
        store.apply(e);
    }
    let all: Vec<u32> = (0..store.len() as u32).collect();
    let _ = traffic_police_core::export::har::har(&store, &all, store.latest());
});
