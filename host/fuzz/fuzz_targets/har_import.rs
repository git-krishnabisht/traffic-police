//! A HAR file from anywhere (`traffic-police open file.har`): importing it never panics, and
//! neither does exporting what it gave.

#![no_main]

use libfuzzer_sys::fuzz_target;
use traffic_police_core::store::SessionStore;

fuzz_target!(|data: &[u8]| {
    let mut store = SessionStore::new();
    let Ok(opened) = traffic_police_core::import::har(data, &store.source_ids()) else { return };
    for e in opened.events {
        store.apply(e);
    }
    let all: Vec<u32> = (0..store.len() as u32).collect();
    let _ = traffic_police_core::export::har::har(&store, &all, store.latest());
});
