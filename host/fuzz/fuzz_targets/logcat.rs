//! The device's log as `logcat -B` writes it, in pieces of any size (Logdawg, ARCHITECTURE.md
//! §5.17): the parser never panics, never holds more than one record waiting for its rest, and a
//! record it reads comes back the same through `encode`, which the stand-in devices write with.

#![no_main]

use libfuzzer_sys::fuzz_target;
use traffic_police_adb::logcat::{Parser, Record, encode};

/// The most a record can be: the largest header the parser takes and the largest payload.
const MOST: usize = 128 + u16::MAX as usize;

fuzz_target!(|data: &[u8]| {
    // the first bytes say how the rest is cut up
    let (sizes, rest) = data.split_at(data.len().min(8));
    let mut parser = Parser::new();
    let (mut at, mut i) = (0, 0);
    while at < rest.len() {
        let n = usize::from(sizes.get(i % sizes.len().max(1)).copied().unwrap_or(255)).max(1);
        let end = (at + n).min(rest.len());
        parser.push(&rest[at..end]);
        at = end;
        i += 1;
        loop {
            match parser.next_record() {
                Ok(Some(r)) => again(r),
                Ok(None) => break,
                // Logdawg's reader stops there: the stream is not logcat's binary output
                Err(_) => return,
            }
        }
        assert!(parser.pending() < MOST, "{} bytes held", parser.pending());
    }
});

/// `encode` writes header version 4, so a record from an older header comes back with uid 0.
/// (Text made valid UTF-8 can grow past the 64 KB a record holds: then there is none to read.)
fn again(r: Record) {
    if r.tag.len() + r.message.len() + 3 > usize::from(u16::MAX) {
        return;
    }
    let mut parser = Parser::new();
    parser.push(&encode(&r));
    let back = parser.next_record().expect("an encoded record reads").expect("all of it");
    assert_eq!(back, Record { uid: Some(r.uid.unwrap_or(0)), ..r });
    assert_eq!(parser.pending(), 0);
}
