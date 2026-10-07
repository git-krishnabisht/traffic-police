//! Bytes from a capture runtime, in pieces of any size: the frame decoder never panics, and it
//! never holds much more than one frame (PROTOCOL.md §3: 16 MiB at most).

#![no_main]

use libfuzzer_sys::fuzz_target;
use traffic_police_proto::Decoder;
use traffic_police_proto::frame::MAX_FRAME_LEN;

fuzz_target!(|data: &[u8]| {
    // the first bytes say how the rest is cut up
    let (sizes, rest) = data.split_at(data.len().min(8));
    let mut decoder = Decoder::new();
    let (mut at, mut i) = (0, 0);
    while at < rest.len() {
        let n = usize::from(sizes.get(i % sizes.len().max(1)).copied().unwrap_or(255)).max(1);
        let end = (at + n).min(rest.len());
        decoder.push(&rest[at..end]);
        at = end;
        i += 1;
        loop {
            match decoder.next_frame() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) if e.is_fatal() => return,
                Err(_) => {}
            }
        }
        assert!(decoder.buffered() <= 4 + MAX_FRAME_LEN as usize + 256, "{} bytes held", decoder.buffered());
    }
});
