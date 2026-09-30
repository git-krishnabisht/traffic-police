//! Protocol conformance goldens (PROTOCOL.md §11), shared with the Java runtime's tests.
//!
//! - `testdata/protocol/v1/device/*.frames` are written by the Java encoder; every frame must
//!   decode here, and every field Java sends must survive a round trip through the typed model
//!   (fields named `future_*` and message types named `future_*` stand for a newer runtime and
//!   must be ignored).
//! - `testdata/protocol/v1/host/*.frames` are written here, by
//!   `cargo test -p traffic-police-proto -- --ignored update_goldens`, and decoded by the Java tests.

use std::path::PathBuf;

use bytes::BytesMut;
use serde_json::Value;
use traffic_police_proto::frame::Frame;
use traffic_police_proto::msg::{
    self, Bye, CaptureConfig, CaptureConfigPatch, DeviceMsg, HelloAck, HostInfo, HostMsg, Pattern, Ping, QueryMatch,
    Rule, RuleAction, RuleMatch, RuleSet, SetConfig, SetRules,
};
use traffic_police_proto::{Decoder, PROTOCOL_VERSION};

fn dir(side: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/protocol/v1").join(side)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn decode_all(name: &str, bytes: &[u8]) -> Vec<Frame> {
    let mut d = Decoder::new();
    d.push(bytes);
    let mut out = Vec::new();
    while let Some(f) = d.next_frame().unwrap_or_else(|e| panic!("{name}: {e}")) {
        out.push(f);
    }
    assert!(d.buffered() == 0, "{name}: {} trailing bytes", d.buffered());
    out
}

/// A value the typed model may drop or fill in: it means the same as an absent field.
fn is_default(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Every field the device sent must come back from the typed model with the same value.
fn covered(sent: &Value, back: &Value, path: &str, problems: &mut Vec<String>) {
    match (sent, back) {
        (Value::Object(s), Value::Object(b)) => {
            for (k, v) in s {
                let p = format!("{path}.{k}");
                if k.starts_with("future_") {
                    if b.contains_key(k) {
                        problems.push(format!("{p}: an unknown field was kept"));
                    }
                    continue;
                }
                match b.get(k) {
                    Some(bv) => covered(v, bv, &p, problems),
                    None if is_default(v) => {}
                    None => problems.push(format!("{p}: sent as {v}, not understood by the host")),
                }
            }
            for (k, v) in b {
                if !s.contains_key(k) && !is_default(v) {
                    problems.push(format!("{path}.{k}: the host reads {v} although nothing was sent"));
                }
            }
        }
        (Value::Array(s), Value::Array(b)) => {
            if s.len() != b.len() {
                problems.push(format!("{path}: {} items sent, {} read", s.len(), b.len()));
            }
            for (i, (sv, bv)) in s.iter().zip(b).enumerate() {
                covered(sv, bv, &format!("{path}[{i}]"), problems);
            }
        }
        (Value::Number(s), Value::Number(b)) => {
            if s.as_f64() != b.as_f64() {
                problems.push(format!("{path}: sent {s}, read {b}"));
            }
        }
        _ => {
            if sent != back {
                problems.push(format!("{path}: sent {sent}, read {back}"));
            }
        }
    }
}

#[test]
fn device_goldens_decode_as_the_java_encoder_meant() {
    let dir = dir("device");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e} (run ./gradlew :capture-core:updateProtocolGoldens)", dir.display()))
        .filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".frames").map(str::to_string))
        .collect();
    names.sort();
    assert!(names.len() >= 10, "expected the device goldens in {}", dir.display());
    let mut problems = Vec::new();
    for name in &names {
        let bytes = std::fs::read(dir.join(format!("{name}.frames"))).unwrap();
        let expected: Vec<Value> =
            serde_json::from_slice(&std::fs::read(dir.join(format!("{name}.expected.json"))).unwrap()).unwrap();
        let frames = decode_all(name, &bytes);
        assert_eq!(frames.len(), expected.len(), "{name}: frame count");
        let mut offset = 0usize;
        for (i, (frame, want)) in frames.iter().zip(&expected).enumerate() {
            let at = format!("{name} #{i}");
            match frame {
                Frame::Json(json) => {
                    offset += 5 + json.len();
                    assert_eq!(want["frame"], "json", "{at}");
                    let sent: Value = serde_json::from_slice(json).unwrap();
                    assert_eq!(sent, want["msg"], "{at}: the frame holds other JSON than the encoder meant");
                    let t = sent["t"].as_str().unwrap_or_default().to_string();
                    let typed = msg::parse_device(json).unwrap_or_else(|e| panic!("{at} ({t}): {e}"));
                    if t.starts_with("future_") {
                        assert_eq!(typed, DeviceMsg::Unknown, "{at}: an unknown message type must be ignored");
                        continue;
                    }
                    let back = serde_json::to_value(&typed).unwrap();
                    covered(&sent, &back, &format!("{at} {t}"), &mut problems);
                }
                Frame::Body(c) => {
                    assert_eq!(want["frame"], "body", "{at}");
                    let got = serde_json::json!({
                        "frame": "body", "seq": c.seq, "txn": c.txn, "dir": c.dir.as_str(),
                        "flags": bytes[offset + 5 + 17], "ts": c.ts, "offset": c.offset, "len": c.data.len(),
                        "crc32": format!("{:08x}", crc32(&c.data)),
                    });
                    assert_eq!(&got, want, "{at}: body chunk");
                    offset += 5 + 34 + c.data.len();
                }
                Frame::Other { kind, .. } => panic!("{at}: unexpected frame type {kind}"),
            }
        }
    }
    assert!(problems.is_empty(), "{} problems:\n  {}", problems.len(), problems.join("\n  "));
}

/// The host-to-device goldens: one message per file, every matcher and action in `set_rules`.
fn host_goldens() -> Vec<(&'static str, HostMsg)> {
    let every_matcher = RuleMatch {
        methods: vec!["GET".into(), "POST".into()],
        scheme: Some("https".into()),
        host: Some(Pattern::Exact("api.example.app".into())),
        port: Some(443),
        path: Some(Pattern::Glob("/v1/sdk/*/status".into())),
        query: vec![
            QueryMatch { name: "sessionId".into(), value: Some(Pattern::Regex("^session_[0-9]+$".into())) },
            QueryMatch { name: "debug".into(), value: None },
        ],
    };
    let other_patterns = RuleMatch {
        host: Some(Pattern::Regex(r"^(cdn|img)\.example\.app$".into())),
        path: Some(Pattern::Exact("/v1/profile".into())),
        ..RuleMatch::default()
    };
    let rules = RuleSet {
        version: "r-2026-09-30".into(),
        rules: vec![
            Rule {
                id: "verdict-fail".into(),
                name: Some("Force a failed verdict".into()),
                enabled: true,
                matcher: every_matcher,
                actions: vec![
                    RuleAction::Delay { ms: 1500 },
                    RuleAction::Status { code: 503, reason: Some("Service Unavailable".into()) },
                    RuleAction::Header { op: "set".into(), name: "Retry-After".into(), value: Some("5".into()) },
                    RuleAction::Header { op: "add".into(), name: "Set-Cookie".into(), value: Some("a=b".into()) },
                    RuleAction::Header { op: "remove".into(), name: "ETag".into(), value: None },
                    RuleAction::Body {
                        text: Some(r#"{"verdict":"fail"}"#.into()),
                        base64: None,
                        content_type: Some("application/json".into()),
                    },
                    RuleAction::Replace { find: "\"pass\"".into(), with: "\"fail\"".into(), regex: false },
                    RuleAction::Replace { find: r#""score":\d+"#.into(), with: r#""score":0"#.into(), regex: true },
                ],
                cache_rewrites: false,
            },
            Rule {
                id: "offline-images".into(),
                name: None,
                enabled: false,
                matcher: other_patterns,
                actions: vec![
                    RuleAction::Fail {
                        exception: "java.net.UnknownHostException".into(),
                        message: Some("offline (rule)".into()),
                    },
                    RuleAction::Body {
                        text: None,
                        base64: Some("iVBORw0KGgo=".into()),
                        content_type: Some("image/png".into()),
                    },
                ],
                cache_rewrites: true,
            },
        ],
    };
    vec![
        (
            "hello_ack",
            HostMsg::HelloAck(HelloAck {
                id: 1,
                protocol: PROTOCOL_VERSION,
                host: HostInfo { name: "traffic-police".into(), version: "0.1.0".into() },
                resume_after_seq: 0,
                config: CaptureConfig {
                    recording: true,
                    body_cap: 4 << 20,
                    capture_request_bodies: true,
                    capture_response_bodies: false,
                    stack_depth: 32,
                },
                rules: RuleSet { version: "none".into(), rules: Vec::new() },
            }),
        ),
        (
            "set_config",
            HostMsg::SetConfig(SetConfig {
                id: 2,
                config: CaptureConfigPatch {
                    recording: Some(false),
                    body_cap: Some(1 << 20),
                    stack_depth: Some(16),
                    ..CaptureConfigPatch::default()
                },
            }),
        ),
        ("set_rules", HostMsg::SetRules(SetRules { id: 3, rules })),
        ("ping", HostMsg::Ping(Ping { id: 7 })),
        (
            "bye",
            HostMsg::Bye(Bye {
                reason: "shutdown".into(),
                message: Some("traffic-police quit".into()),
                supported: Vec::new(),
            }),
        ),
    ]
}

fn encode(m: &HostMsg) -> Vec<u8> {
    let mut out = BytesMut::new();
    msg::encode_msg(m, &mut out).unwrap();
    out.to_vec()
}

#[test]
fn host_goldens_are_current() {
    let dir = dir("host");
    for (name, m) in host_goldens() {
        let path = dir.join(format!("{name}.frames"));
        let on_disk = std::fs::read(&path).unwrap_or_else(|e| {
            panic!("{}: {e} (run cargo test -p traffic-police-proto -- --ignored update_goldens)", path.display())
        });
        assert_eq!(
            on_disk,
            encode(&m),
            "{name}: the encoding changed; regenerate the goldens and rerun the Java tests"
        );
        // and it reads back as the same message
        let frames = decode_all(name, &on_disk);
        let [Frame::Json(json)] = frames.as_slice() else { panic!("{name}: expected one JSON frame") };
        assert_eq!(msg::parse_host(json).unwrap(), m);
    }
}

#[test]
#[ignore = "rewrites testdata/protocol/v1/host; run on purpose"]
fn update_goldens() {
    let dir = dir("host");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, m) in host_goldens() {
        let bytes = encode(&m);
        std::fs::write(dir.join(format!("{name}.frames")), &bytes).unwrap();
        let pretty = serde_json::to_string_pretty(&m).unwrap();
        std::fs::write(dir.join(format!("{name}.expected.json")), pretty + "\n").unwrap();
    }
}
