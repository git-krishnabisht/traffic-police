//! What the host makes of each device golden (PROTOCOL.md §11): frames written by the Java
//! encoder go through the host's decoder, normalizer and store, and the resulting session is
//! summarized and pinned as a snapshot. The byte-level checks live in the proto crate.
//!
//! After an intended change: `INSTA_UPDATE=always cargo test -p traffic-police-core --test goldens`.

use std::fmt::Write;
use std::path::PathBuf;

use traffic_police_core::SessionEvent;
use traffic_police_core::decode::decode_body;
use traffic_police_core::fmt::Ts;
use traffic_police_core::model::{BodyMeta, SourceInfo, Transaction};
use traffic_police_core::normalize::{Control, Normalizer};
use traffic_police_core::store::SessionStore;
use traffic_police_proto::Decoder;

fn ms(from: Ts, to: Ts) -> String {
    let d = to as i128 - from as i128;
    format!("{}{:.3} ms", if d < 0 { "-" } else { "+" }, (d.abs() as f64) / 1e6)
}

fn body(store: &SessionStore, t: &Transaction, meta: &BodyMeta, request: bool) -> String {
    let mut s = format!("{:?} {}/{} B", meta.state, meta.captured, meta.total);
    if meta.gap {
        s.push_str(" (gap)");
    }
    if meta.captured > 0 {
        let headers = if request { Some(&t.req_headers) } else { t.resp.as_ref().map(|r| &r.headers) };
        let d = decode_body(store.body_bytes(meta), headers, 1 << 20);
        let _ = write!(s, ", decoded {} B {:?}", d.bytes.len(), d.kind);
        if d.bytes.len() <= 80 && d.bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ' || *b == b'\n') {
            let _ = write!(s, " {:?}", String::from_utf8_lossy(&d.bytes));
        }
    }
    s
}

fn headers(h: &[(String, String)]) -> String {
    h.iter().map(|(n, v)| format!("{n}: {v}")).collect::<Vec<_>>().join(" | ")
}

fn summarize(name: &str) -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/protocol/v1/device");
    let bytes = std::fs::read(dir.join(format!("{name}.frames"))).unwrap();
    let mut decoder = Decoder::new();
    decoder.push(&bytes);
    let mut store = SessionStore::new();
    let source = store.source_ids().next();
    let mut normalizer = Normalizer::new(source);
    let mut out = String::new();
    let mut events = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        match normalizer.frame(frame, &mut events) {
            Ok(Some(Control::Hello(h))) => {
                let _ = writeln!(
                    out,
                    "hello: {} pid {} on API {}, runtime {} ({}), protocol {}, capabilities {:?}",
                    h.app.process, h.app.pid, h.device.api, h.runtime.version, h.runtime.mode, h.protocol, h.capabilities
                );
                let info = SourceInfo::from_hello(source, &h, "Pixel 8 [emulator-5554]".into(), Some("emulator-5554".into()));
                events.push(SessionEvent::SourceUp(Box::new(info)));
            }
            Ok(Some(Control::Replay(r))) => {
                let _ = writeln!(out, "replay {}: seq {:?}..{:?}, {:?} events", r.phase, r.from_seq, r.to_seq, r.events);
            }
            Ok(Some(Control::RulesAck(a))) => {
                let _ = writeln!(out, "rules_ack {}: version {:?}, active {:?}, {} errors", a.id, a.version, a.active, a.errors.len());
            }
            Ok(Some(Control::ConfigAck(a))) => {
                let _ = writeln!(out, "config_ack {}: {:?}", a.id, a.config);
            }
            Ok(Some(Control::Pong(p))) => {
                let _ = writeln!(out, "pong {}: clock {} / {}", p.id, p.clock.ts, p.clock.wall_ms);
            }
            Ok(Some(Control::Bye(b))) => {
                let _ = writeln!(out, "bye: {} {:?}, supported {:?}", b.reason, b.message, b.supported);
            }
            Ok(None) => {}
            Err(e) => {
                let _ = writeln!(out, "error: {e}");
            }
        }
        for e in events.drain(..) {
            store.apply(e);
        }
    }

    let origin = store.origin();
    for t in store.txns() {
        let _ = writeln!(out, "\ntxn {} (call {:?}, hop {}): {} {}", t.key.txn, t.call, t.hop, t.method, t.url.raw);
        let client = t.client.as_ref().map_or("?".into(), |c| c.label());
        let thread = t.thread.as_ref().map_or("?".into(), |th| format!("{} #{} ({:?})", th.name, th.id, th.origin));
        let top = t.stack.first().map_or(String::new(), |f| format!(", top {}.{}:{:?}", f.c, f.m, f.l));
        let _ = writeln!(out, "  {client} · {thread} · {} frames{}{top}", t.stack.len(), if t.stack_truncated { " (truncated)" } else { "" });
        let _ = writeln!(out, "  request: {}", headers(&t.req_headers));
        if let Some(r) = &t.resp {
            let _ = writeln!(out, "  response: {} {} {:?} · {}", r.status, r.message, r.protocol, headers(&r.headers));
        }
        let _ = writeln!(
            out,
            "  state {:?} · starts {} · req {} · ends {}{}{}",
            t.state,
            ms(origin, t.start),
            ms(origin, t.req_at),
            t.end.map_or("-".into(), |e| ms(origin, e)),
            if t.lossy { " · lossy" } else { "" },
            if t.placeholder { " · placeholder" } else { "" },
        );
        let _ = writeln!(out, "  request body: {}", body(&store, t, &t.req_body, true));
        let _ = writeln!(out, "  response body: {}", body(&store, t, &t.resp_body, false));
        if !t.marks.is_empty() {
            let marks: Vec<String> = t.marks.iter().map(|(n, at)| format!("{n} {}", ms(t.start, *at))).collect();
            let _ = writeln!(out, "  marks: {}", marks.join(", "));
        }
        if let Some(c) = &t.conn {
            let tls = c.tls.as_ref().map_or(String::new(), |x| {
                format!(" · {:?} {:?} · {} certs", x.version, x.cipher, x.peer.len())
            });
            let remote = c.remote.as_ref().map_or("?".into(), |a| format!("{}:{}", a.ip, a.port));
            let _ = writeln!(out, "  conn {:?} reused {:?} {:?} {remote}{tls}", c.id, c.reused, c.protocol);
        }
        if let Some(f) = &t.failure {
            let _ = writeln!(
                out,
                "  failed: {}: {:?} (phase {:?}, canceled {}), causes {:?}",
                f.class, f.message, f.phase, f.canceled, f.causes
            );
        }
    }
    let s = store.stats();
    let _ = writeln!(
        out,
        "\nstats: {} requests, {} failed, {} B in, {} B out, {} dropped events",
        s.requests, s.failed, s.bytes_in, s.bytes_out, s.dropped_events
    );
    for d in store.diagnostics() {
        let _ = writeln!(out, "diag {} {} {:?} at {}", d.level, d.code, d.message, ms(origin, d.at));
    }
    for m in store.markers() {
        let _ = writeln!(out, "marker {:?} {:?} at {}", m.kind, m.label, ms(origin, m.at));
    }
    out
}

macro_rules! golden {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                insta::assert_snapshot!(stringify!($name), summarize(stringify!($name)));
            }
        )*
    };
}

golden!(
    handshake_replay_and_control,
    get_gzip_json,
    post_request_body,
    chunked_streaming,
    body_over_cap,
    redirect,
    failures,
    dropped_and_diag,
    largest_body_chunk,
    unknown_fields_and_types,
    protocol_mismatch,
    takeover,
);
