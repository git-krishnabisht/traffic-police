//! Session files (PROTOCOL.md §10): `TPSESS\0` and a format version byte, then a gzip stream of
//! frames: the device frames exactly as captured, and host records that say which source they
//! belong to (17), when a source ended (18), and the user's annotations (19). Opening a file
//! replays it through the same normalizer as a live connection.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::Deserialize;
use serde_json::{Value, json};
use traffic_police_proto::frame::{self, Frame, kind};
use traffic_police_proto::{Decoder, msg};

use crate::event::{MarkerKind, SessionEvent};
use crate::fmt::Ts;
use crate::model::{SourceId, SourceInfo, TxnKey};
use crate::normalize::{Control, Normalizer};
use crate::store::{SessionStore, SourceIds};

pub const MAGIC: &[u8; 7] = b"TPSESS\0";
pub const VERSION: u8 = 1;
/// A recording's gzip stream is flushed at least this often.
const FLUSH_EVERY: Duration = Duration::from_secs(5);

/// The device a source ran on, as the file remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    pub label: String,
    pub serial: Option<String>,
}

enum Sink {
    /// Plain frames in a private temporary file, for exporting later.
    Temp {
        out: BufWriter<File>,
        path: PathBuf,
    },
    /// A session file being written as the capture happens.
    File(GzEncoder<BufWriter<File>>),
    Closed,
}

struct Inner {
    sink: Sink,
    /// The source following device frames belong to.
    current: Option<SourceId>,
    last_flush: Instant,
    buf: BytesMut,
    error: Option<String>,
}

/// Keeps the captured stream: backends hand it every frame, with the source it came from.
pub struct SessionLog {
    inner: Mutex<Inner>,
}

fn session_record(created_wall_ms: i64) -> Value {
    json!({
        "t": "session",
        "format": VERSION,
        "created_wall_ms": created_wall_ms,
        "host": { "name": "traffic-police", "version": env!("CARGO_PKG_VERSION") },
        "filter": null,
    })
}

fn now_wall_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// The user's annotations: pins, and markers not tied to a source (pause, resume).
fn annotations(store: &SessionStore) -> Value {
    let pins: Vec<Value> =
        store.txns().iter().filter(|t| t.pinned).map(|t| json!({ "source": t.key.source, "txn": t.key.txn })).collect();
    let markers: Vec<Value> = store
        .markers()
        .iter()
        .filter(|m| m.source.is_none())
        .map(|m| json!({ "at": m.at, "kind": format!("{:?}", m.kind).to_lowercase(), "label": m.label }))
        .collect();
    json!({ "t": "annotations", "pins": pins, "markers": markers, "notes": {} })
}

impl SessionLog {
    /// A log in a private temporary directory (removed at exit), for exporting on request.
    pub fn temporary() -> io::Result<SessionLog> {
        let dir = crate::store::spill::private_dir()?;
        let path = dir.join("stream.bin");
        let out = BufWriter::new(File::create(&path)?);
        Ok(SessionLog::with(Sink::Temp { out, path }))
    }

    /// A session file written as the capture happens (`traffic-police record`).
    pub fn file(path: &Path) -> io::Result<SessionLog> {
        let mut out = BufWriter::new(File::create(path)?);
        out.write_all(MAGIC)?;
        out.write_all(&[VERSION])?;
        let log = SessionLog::with(Sink::File(GzEncoder::new(out, Compression::default())));
        log.record(kind::SESSION, &session_record(now_wall_ms()));
        Ok(log)
    }

    fn with(sink: Sink) -> SessionLog {
        SessionLog {
            inner: Mutex::new(Inner {
                sink,
                current: None,
                last_flush: Instant::now(),
                buf: BytesMut::new(),
                error: None,
            }),
        }
    }

    fn write(inner: &mut Inner) {
        let bytes = inner.buf.split();
        let result = match &mut inner.sink {
            Sink::Temp { out, .. } => out.write_all(&bytes),
            Sink::File(gz) => gz.write_all(&bytes).and_then(|_| {
                if inner.last_flush.elapsed() >= FLUSH_EVERY {
                    inner.last_flush = Instant::now();
                    gz.flush()
                } else {
                    Ok(())
                }
            }),
            Sink::Closed => Ok(()),
        };
        if let Err(e) = result
            && inner.error.is_none()
        {
            tracing::warn!("session log: {e}");
            inner.error = Some(e.to_string());
        }
    }

    fn record(&self, kind: u8, v: &Value) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let json = serde_json::to_vec(v).expect("serializable");
        frame::encode_raw(kind, &json, &mut inner.buf);
        Self::write(&mut inner);
    }

    /// A source starts (or, `resumed`, reconnects): its device and the `hello` as received.
    pub fn source(&self, source: SourceId, device: &DeviceRecord, hello: &[u8], resumed: bool) {
        let hello: Value = serde_json::from_slice(hello).unwrap_or(Value::Null);
        let v = json!({
            "t": "source",
            "source": source,
            "device": { "label": device.label, "serial": device.serial },
            "hello": hello,
            "resumed": resumed,
        });
        self.record(kind::SOURCE, &v);
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).current = Some(source);
    }

    /// A device frame from `source`.
    pub fn frame(&self, source: SourceId, f: &Frame) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.current != Some(source) {
            // a short source record switches without repeating the hello
            let json = serde_json::to_vec(&json!({ "t": "source", "source": source })).expect("serializable");
            frame::encode_raw(kind::SOURCE, &json, &mut inner.buf);
            inner.current = Some(source);
        }
        frame::encode(f, &mut inner.buf);
        Self::write(&mut inner);
    }

    pub fn source_end(&self, source: SourceId, at: Ts, reason: &str) {
        self.record(kind::SOURCE_END, &json!({ "t": "source_end", "source": source, "ts": at, "reason": reason }));
    }

    /// Ends a recording: the annotations, then the end of the gzip stream.
    pub fn finish(&self, store: &SessionStore) -> io::Result<()> {
        self.record(kind::ANNOTATIONS, &annotations(store));
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match std::mem::replace(&mut inner.sink, Sink::Closed) {
            Sink::File(gz) => gz.finish()?.flush(),
            Sink::Temp { mut out, .. } => out.flush(),
            Sink::Closed => Ok(()),
        }
    }

    /// Writes a session file from a temporary log: the stream so far (only the frames of `keep`
    /// when given, plus those that belong to no request), then the store's annotations. Returns
    /// how many requests it holds.
    pub fn export(&self, store: &SessionStore, out: &Path, keep: Option<&HashSet<TxnKey>>) -> io::Result<usize> {
        let path = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match &mut inner.sink {
                Sink::Temp { out, path } => {
                    out.flush()?;
                    path.clone()
                }
                _ => return Err(io::Error::other("only a temporary log can be exported")),
            }
        };
        let mut w = BufWriter::new(File::create(out)?);
        w.write_all(MAGIC)?;
        w.write_all(&[VERSION])?;
        let mut gz = GzEncoder::new(w, Compression::default());
        let mut buf = BytesMut::new();
        frame::encode_raw(kind::SESSION, &serde_json::to_vec(&session_record(now_wall_ms()))?, &mut buf);
        gz.write_all(&buf)?;
        let mut reader = BufReader::new(File::open(&path)?);
        let mut decoder = Decoder::new();
        let mut chunk = vec![0u8; 256 * 1024];
        let mut current: Option<SourceId> = None;
        loop {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            decoder.push(&chunk[..n]);
            while let Some(f) = decoder.next_frame().map_err(io::Error::other)? {
                if let Frame::Other { kind: kind::SOURCE, payload } = &f
                    && let Ok(v) = serde_json::from_slice::<SourceProbe>(payload)
                {
                    current = Some(v.source);
                }
                if let (Some(keep), Some(source)) = (keep, current)
                    && let Some(txn) = txn_of(&f)
                    && !keep.contains(&TxnKey { source, txn })
                {
                    continue;
                }
                buf.clear();
                frame::encode(&f, &mut buf);
                gz.write_all(&buf)?;
            }
        }
        buf.clear();
        frame::encode_raw(kind::ANNOTATIONS, &serde_json::to_vec(&annotations(store))?, &mut buf);
        gz.write_all(&buf)?;
        gz.finish()?.flush()?;
        Ok(keep.map_or(store.len(), HashSet::len))
    }
}

#[derive(Deserialize)]
struct SourceProbe {
    source: SourceId,
}

#[derive(Deserialize)]
struct TxnProbe {
    #[serde(default)]
    txn: Option<u64>,
}

/// The request a device frame belongs to, if any.
fn txn_of(f: &Frame) -> Option<u64> {
    match f {
        Frame::Body(c) => Some(c.txn),
        Frame::Json(j) => serde_json::from_slice::<TxnProbe>(j).ok()?.txn,
        Frame::Other { .. } => None,
    }
}

/// A session file, read back.
#[derive(Debug, Default)]
pub struct Opened {
    /// What a live connection would have produced, in order.
    pub events: Vec<SessionEvent>,
    /// Pinned requests, with source ids as assigned while reading.
    pub pins: Vec<TxnKey>,
    pub created_wall_ms: Option<i64>,
    /// The file ended mid-stream (a recording that did not finish); everything before is kept.
    pub truncated: bool,
}

#[derive(Deserialize)]
struct SourceRecord {
    source: SourceId,
    #[serde(default)]
    device: Option<DeviceJson>,
    #[serde(default)]
    hello: Option<msg::Hello>,
    #[serde(default)]
    resumed: bool,
}

#[derive(Deserialize)]
struct DeviceJson {
    label: String,
    #[serde(default)]
    serial: Option<String>,
}

#[derive(Deserialize)]
struct EndRecord {
    source: SourceId,
    ts: Ts,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct Annotations {
    #[serde(default)]
    pins: Vec<PinRecord>,
    #[serde(default)]
    markers: Vec<MarkerRecord>,
}

#[derive(Deserialize)]
struct PinRecord {
    source: SourceId,
    txn: u64,
}

#[derive(Deserialize)]
struct MarkerRecord {
    at: Ts,
    kind: String,
    #[serde(default)]
    label: String,
}

/// Whether `path` starts like a session file.
pub fn is_session_file(path: &Path) -> bool {
    let mut head = [0u8; 7];
    File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && &head == MAGIC
}

/// Reads a session file into events; sources get fresh ids from `ids`.
pub fn open(path: &Path, ids: &SourceIds) -> io::Result<Opened> {
    let mut file = BufReader::new(File::open(path)?);
    let mut head = [0u8; 8];
    file.read_exact(&mut head).map_err(|_| io::Error::other("not a traffic-police session file"))?;
    if &head[..7] != MAGIC {
        return Err(io::Error::other("not a traffic-police session file"));
    }
    if head[7] != VERSION {
        return Err(io::Error::other(format!(
            "session file format {} (this traffic-police reads format {VERSION}; update it)",
            head[7]
        )));
    }
    // a recording cut short ends without the gzip trailer: keep what decodes
    let mut gz = GzDecoder::new(file);
    let mut bytes = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    let mut truncated = false;
    loop {
        match gz.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof || e.kind() == io::ErrorKind::InvalidInput => {
                truncated = true;
                break;
            }
            Err(e) => {
                if bytes.is_empty() {
                    return Err(e);
                }
                truncated = true;
                break;
            }
        }
    }
    let mut decoder = Decoder::new();
    decoder.push(&bytes);
    let mut out = Opened { truncated, ..Default::default() };
    let mut map: HashMap<SourceId, SourceId> = HashMap::new();
    let mut normalizers: HashMap<SourceId, Normalizer> = HashMap::new();
    let mut current: Option<SourceId> = None;
    loop {
        let f = match decoder.next_frame() {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.is_fatal() => {
                out.truncated = true;
                break;
            }
            Err(_) => continue,
        };
        match f {
            Frame::Other { kind: kind::SESSION, payload } => {
                out.created_wall_ms =
                    serde_json::from_slice::<Value>(&payload).ok().and_then(|v| v["created_wall_ms"].as_i64());
            }
            Frame::Other { kind: kind::SOURCE, payload } => {
                let Ok(r) = serde_json::from_slice::<SourceRecord>(&payload) else { continue };
                current = Some(r.source);
                if let Some(hello) = r.hello {
                    let id = match (r.resumed, map.get(&r.source)) {
                        (true, Some(&id)) => id,
                        _ => {
                            let id = ids.next();
                            map.insert(r.source, id);
                            normalizers.insert(r.source, Normalizer::new(id));
                            id
                        }
                    };
                    let (label, serial) =
                        r.device.map_or((String::from("recorded device"), None), |d| (d.label, d.serial));
                    let mut info = SourceInfo::from_hello(id, &hello, label, serial);
                    if r.resumed {
                        info.started = info.clock.map_or(info.started, |(ts, _)| ts);
                    }
                    out.events.push(SessionEvent::SourceUp(Box::new(info)));
                }
            }
            Frame::Other { kind: kind::SOURCE_END, payload } => {
                if let Ok(r) = serde_json::from_slice::<EndRecord>(&payload)
                    && let Some(&id) = map.get(&r.source)
                {
                    out.events.push(SessionEvent::SourceDown { source: id, at: r.ts, reason: r.reason });
                }
            }
            Frame::Other { kind: kind::ANNOTATIONS, payload } => {
                // the last one wins
                if let Ok(a) = serde_json::from_slice::<Annotations>(&payload) {
                    out.pins = a
                        .pins
                        .iter()
                        .filter_map(|p| map.get(&p.source).map(|&source| TxnKey { source, txn: p.txn }))
                        .collect();
                    for m in a.markers {
                        let kind = match m.kind.as_str() {
                            "pause" => MarkerKind::Pause,
                            "resume" => MarkerKind::Resume,
                            _ => MarkerKind::Note,
                        };
                        out.events.push(SessionEvent::Marker { source: None, at: m.at, kind, label: m.label });
                    }
                }
            }
            Frame::Other { .. } => {}
            device => {
                let Some(n) = current.and_then(|c| normalizers.get_mut(&c)) else { continue };
                match n.frame(device, &mut out.events) {
                    Ok(Some(Control::Hello(_))) | Ok(_) => {}
                    Err(e) => tracing::debug!("session file: {e}"),
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use traffic_police_proto::{BodyChunk, BodyDir};

    fn hello(pid: u32, ts: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "t": "hello", "protocol": 1, "runtime": { "version": "0.1.0", "mode": "library" },
            "instance": format!("inst-{pid}"),
            "app": { "package": "com.example", "process": "com.example", "pid": pid },
            "device": { "api": 36 }, "clock": { "ts": ts, "wall_ms": 1_790_000_000_000i64 },
        }))
        .unwrap()
    }

    fn json_frame(v: Value) -> Frame {
        Frame::Json(Bytes::from(serde_json::to_vec(&v).unwrap()))
    }

    fn request(log: &SessionLog, source: SourceId, txn: u64, seq: u64) {
        let url = format!("https://api.example.app/{txn}");
        log.frame(
            source,
            &json_frame(json!({ "t": "req", "seq": seq, "ts": 10 + seq, "txn": txn, "method": "GET", "url": url })),
        );
        log.frame(
            source,
            &json_frame(json!({ "t": "resp", "seq": seq + 1, "ts": 20 + seq, "txn": txn, "status": 200 })),
        );
        let chunk = BodyChunk {
            seq: seq + 2,
            txn,
            dir: BodyDir::Response,
            ts: 21 + seq,
            offset: 0,
            data: Bytes::from_static(b"{\"ok\":true}"),
        };
        log.frame(source, &Frame::Body(chunk));
        log.frame(source, &json_frame(json!({ "t": "done", "seq": seq + 3, "ts": 30 + seq, "txn": txn })));
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tp-session-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store_of(opened: &Opened) -> SessionStore {
        let mut s = SessionStore::new();
        for e in &opened.events {
            s.apply(e.clone());
        }
        s
    }

    #[test]
    fn a_recording_reopens_with_sources_requests_bodies_and_markers() {
        let dir = scratch("rec");
        let path = dir.join("a.trafficpolice");
        let device = DeviceRecord { label: "Pixel 8 [emulator-5554]".into(), serial: Some("emulator-5554".into()) };
        let log = SessionLog::file(&path).unwrap();
        log.source(1, &device, &hello(4242, 5), false);
        request(&log, 1, 1, 1);
        log.source(2, &device, &hello(5151, 50), false);
        request(&log, 2, 1, 1);
        request(&log, 1, 2, 5);
        log.source_end(1, 100, "the app exited");
        // the recorder's own store, with a pin
        let mut live = SessionStore::new();
        live.apply(SessionEvent::Marker {
            source: None,
            at: 60,
            kind: MarkerKind::Pause,
            label: "recording paused".into(),
        });
        log.finish(&live).unwrap();

        let opened = open(&path, &SourceIds::default()).unwrap();
        assert!(!opened.truncated);
        let s = store_of(&opened);
        assert_eq!(s.len(), 3);
        assert_eq!(s.sources().count(), 2);
        let first = s.sources().find(|x| x.pid == 4242).unwrap();
        assert_eq!(first.ended.as_ref().map(|e| e.1.as_str()), Some("the app exited"));
        assert_eq!(first.device_label, "Pixel 8 [emulator-5554]");
        let t = s.txn(s.find(TxnKey { source: first.id, txn: 2 }).unwrap());
        assert_eq!(t.resp.as_ref().unwrap().status, 200);
        assert_eq!(&s.body_bytes(&t.resp_body)[..], b"{\"ok\":true}");
        assert!(s.markers().iter().any(|m| m.label == "recording paused"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn export_keeps_chosen_requests_and_pins_and_survives_truncation() {
        let dir = scratch("export");
        let device = DeviceRecord { label: "demo".into(), serial: None };
        let log = SessionLog::temporary().unwrap();
        // the UI's store and the log share source ids, as with a live backend
        let mut store = SessionStore::new();
        let src = store.source_ids().next();
        log.source(src, &device, &hello(7, 5), false);
        request(&log, src, 1, 1);
        request(&log, src, 2, 5);
        let everything = dir.join("everything.trafficpolice");
        log.export(&store, &everything, None).unwrap();
        for e in open(&everything, &SourceIds::default()).unwrap().events {
            store.apply(e);
        }
        let two = store.find(TxnKey { source: src, txn: 2 }).unwrap();
        store.set_pinned(two, true);
        // only request 2, with its pin
        let path = dir.join("pinned.trafficpolice");
        let keep: HashSet<TxnKey> = [TxnKey { source: src, txn: 2 }].into_iter().collect();
        assert_eq!(log.export(&store, &path, Some(&keep)).unwrap(), 1);
        let opened = open(&path, &SourceIds::default()).unwrap();
        assert_eq!(store_of(&opened).len(), 1, "only the kept request");
        assert_eq!(opened.pins, vec![TxnKey { source: 0, txn: 2 }]);
        // cut a file short: what decodes is kept
        let bytes = std::fs::read(&everything).unwrap();
        let cut = dir.join("cut.trafficpolice");
        std::fs::write(&cut, &bytes[..bytes.len() - 20]).unwrap();
        let opened = open(&cut, &SourceIds::default()).unwrap();
        assert!(opened.truncated);
        assert!(!opened.events.is_empty());
        assert!(!is_session_file(&dir.join("missing")));
        assert!(is_session_file(&cut));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
