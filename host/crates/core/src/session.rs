//! Session files (PROTOCOL.md §10): `TPSESS\0` and a format version byte, then a gzip stream of
//! frames: the device frames exactly as captured, and host records that say which source they
//! belong to (17), when a source ended (18), the user's annotations (19), and the device's log
//! (20). Opening a file replays it through the same normalizer as a live connection.

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
use crate::logdawg::{Level, LogInfo, LogLine};
use crate::model::{SourceId, SourceInfo, TxnKey};
use crate::normalize::{Control, Normalizer};
use crate::store::{SessionStore, SourceIds};

pub const MAGIC: &[u8; 7] = b"TPSESS\0";
pub const VERSION: u8 = 1;
/// A recording's gzip stream is flushed at least this often.
const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// Lines of the device's log in one frame of a session file (type 20).
const LOG_BATCH: usize = 2000;

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

/// Something that wants the captured stream as it arrives: backends hand it each source's hello,
/// every device frame with the source it came from, and when sources end.
pub trait StreamSink: Send + Sync {
    /// A source starts (or, `resumed`, reconnects): its device and the `hello` as received.
    fn source(&self, source: SourceId, device: &DeviceRecord, hello: &[u8], resumed: bool);
    /// A device frame from `source`.
    fn frame(&self, source: SourceId, f: &Frame);
    fn source_end(&self, source: SourceId, at: Ts, reason: &str);
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
    pub fn record_source(&self, source: SourceId, device: &DeviceRecord, hello: &[u8], resumed: bool) {
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
    pub fn record_frame(&self, source: SourceId, f: &Frame) {
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

    pub fn record_source_end(&self, source: SourceId, at: Ts, reason: &str) {
        self.record(kind::SOURCE_END, &json!({ "t": "source_end", "source": source, "ts": at, "reason": reason }));
    }

    /// Ends a recording: the device's log (when it was read), the annotations, then the end of
    /// the gzip stream.
    pub fn finish(&self, store: &SessionStore) -> io::Result<()> {
        for frame in log_records(store, None) {
            self.record(kind::LOG, &frame);
        }
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
        // the device's log: all of it, or with some requests the lines from the first's start to
        // the last's end
        let span = keep.map(|keep| {
            let kept = store.txns().iter().filter(|t| keep.contains(&t.key));
            kept.fold((Ts::MAX, 0), |(a, b), t| (a.min(t.start), b.max(t.end.unwrap_or(t.start))))
        });
        for record in log_records(store, span) {
            buf.clear();
            frame::encode_raw(kind::LOG, &serde_json::to_vec(&record)?, &mut buf);
            gz.write_all(&buf)?;
        }
        buf.clear();
        frame::encode_raw(kind::ANNOTATIONS, &serde_json::to_vec(&annotations(store))?, &mut buf);
        gz.write_all(&buf)?;
        gz.finish()?.flush()?;
        Ok(keep.map_or(store.len(), HashSet::len))
    }
}

/// The device's log as session file records (type 20): what the reader knew in the first, then
/// the lines (in `span` when given) in batches. None when the log was not read.
fn log_records(store: &SessionStore, span: Option<(Ts, Ts)>) -> Vec<Value> {
    let logs = store.logs();
    let info = logs.info();
    if logs.is_empty() && info.package.is_none() {
        return Vec::new();
    }
    let mut processes: Vec<(u32, &str)> = logs.processes().collect();
    processes.sort_unstable();
    let mut out = vec![json!({
        "t": "log",
        "info": { "device": info.device, "package": info.package, "uid": info.uid, "processes": processes },
        "lines": [],
    })];
    let mut lines = Vec::new();
    for l in logs.iter_from(logs.first_id()) {
        if span.is_some_and(|(a, b)| l.ts < a || l.ts > b) {
            continue;
        }
        lines.push(json!([l.ts, l.wall_ms, l.pid, l.tid, l.uid, l.level.letter(), l.buffer, l.tag, l.message]));
        if lines.len() == LOG_BATCH {
            out.push(json!({ "t": "log", "lines": std::mem::take(&mut lines) }));
        }
    }
    if !lines.is_empty() {
        out.push(json!({ "t": "log", "lines": lines }));
    }
    out
}

/// A line of a log record, `[ts, wall_ms, pid, tid, uid, level, buffer, tag, message]`; `None`
/// for one that is not (a file from someone else may hold anything).
fn log_line(v: &Value) -> Option<LogLine> {
    let a = v.as_array().filter(|a| a.len() == 9)?;
    let num = |i: usize| a[i].as_u64();
    let small = |i: usize| num(i).and_then(|n| u32::try_from(n).ok());
    Some(LogLine {
        ts: num(0)?,
        wall_ms: a[1].as_i64()?,
        pid: small(2)?,
        tid: small(3)?,
        uid: if a[4].is_null() { None } else { Some(small(4)?) },
        level: Level::parse(a[5].as_str()?)?,
        buffer: u8::try_from(num(6)?).ok()?,
        tag: a[7].as_str()?.to_string(),
        message: a[8].as_str()?.to_string(),
    })
}

impl StreamSink for SessionLog {
    fn source(&self, source: SourceId, device: &DeviceRecord, hello: &[u8], resumed: bool) {
        self.record_source(source, device, hello, resumed);
    }
    fn frame(&self, source: SourceId, f: &Frame) {
        self.record_frame(source, f);
    }
    fn source_end(&self, source: SourceId, at: Ts, reason: &str) {
        self.record_source_end(source, at, reason);
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
    /// What could not be read (HAR entries without a date or URL), for a note.
    pub skipped: Vec<String>,
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
    read(BufReader::new(File::open(path)?), ids)
}

/// Reads a session file's bytes into events (see [`open`]). Nothing in them is trusted: a file
/// from someone else may be damaged or made up (the fuzz target `session_file` feeds it noise).
pub fn read(mut file: impl Read, ids: &SourceIds) -> io::Result<Opened> {
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
            Frame::Other { kind: kind::LOG, payload } => {
                let Ok(v) = serde_json::from_slice::<Value>(&payload) else { continue };
                if let Some(info) = v.get("info").filter(|i| i.is_object()) {
                    let text = |k: &str| info[k].as_str().map(str::to_string);
                    let processes = info["processes"]
                        .as_array()
                        .map(|ps| {
                            ps.iter()
                                .filter_map(|p| Some((u32::try_from(p[0].as_u64()?).ok()?, p[1].as_str()?.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    out.events.push(SessionEvent::LogInfo(Box::new(LogInfo {
                        device: text("device"),
                        package: text("package"),
                        uid: info["uid"].as_u64().and_then(|u| u32::try_from(u).ok()),
                        processes,
                        status: None,
                    })));
                }
                let lines: Vec<LogLine> =
                    v["lines"].as_array().map(|ls| ls.iter().filter_map(log_line).collect()).unwrap_or_default();
                if !lines.is_empty() {
                    out.events.push(SessionEvent::Logs(lines));
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

    #[test]
    fn the_devices_log_goes_into_the_file_and_comes_back() {
        let dir = scratch("log");
        let device = DeviceRecord { label: "demo".into(), serial: None };
        let log = SessionLog::temporary().unwrap();
        let mut store = SessionStore::new();
        let src = store.source_ids().next();
        log.source(src, &device, &hello(7, 5), false);
        request(&log, src, 1, 1);
        let line = |ts: Ts, uid: Option<u32>, level: Level, tag: &str, message: &str| LogLine {
            ts,
            wall_ms: 1_790_000_000_000 + ts as i64,
            pid: 7,
            tid: 9,
            uid,
            level,
            buffer: 0,
            tag: tag.into(),
            message: message.into(),
        };
        let lines = vec![
            line(8, Some(10_234), Level::Debug, "OkHttp", "--> GET https://api.example.app/1"),
            line(15, None, Level::Error, "AndroidRuntime", "FATAL EXCEPTION: main\n\tat a.B(B.kt:1) \"quoted\" ✓"),
            line(90, Some(1000), Level::Warn, "ActivityManager", "after the request"),
        ];
        store.apply(SessionEvent::LogInfo(Box::new(LogInfo {
            device: Some("demo".into()),
            package: Some("com.example".into()),
            uid: Some(10_234),
            processes: vec![(7, "com.example".into())],
            status: Some("reading the log of demo".into()),
        })));
        store.apply(SessionEvent::Logs(lines.clone()));
        let path = dir.join("with-log.trafficpolice");
        log.export(&store, &path, None).unwrap();
        let back = store_of(&open(&path, &SourceIds::default()).unwrap());
        let logs = back.logs();
        let read: Vec<LogLine> = logs
            .iter_from(logs.first_id())
            .map(|l| LogLine {
                ts: l.ts,
                wall_ms: l.wall_ms,
                pid: l.pid,
                tid: l.tid,
                uid: l.uid,
                level: l.level,
                buffer: l.buffer,
                tag: l.tag.to_string(),
                message: l.message.to_string(),
            })
            .collect();
        assert_eq!(read, lines, "every field, the text as it was");
        assert_eq!(
            (logs.package(), logs.uid(), logs.process(7)),
            (Some("com.example"), Some(10_234), Some("com.example"))
        );
        assert_eq!(logs.info().status, None, "a file's log is not being read");
        // only some requests (from a store that has them): the lines from the first one's start
        // to the last one's end
        let keep: HashSet<TxnKey> = [TxnKey { source: src, txn: 1 }].into_iter().collect();
        let path = dir.join("some.trafficpolice");
        log.export(&back, &path, Some(&keep)).unwrap();
        let some = store_of(&open(&path, &SourceIds::default()).unwrap());
        let tags: Vec<String> = some.logs().iter_from(0).map(|l| l.tag.to_string()).collect();
        assert_eq!(tags, ["AndroidRuntime"], "the line at 15, within the request's 11..31");
        // a log line that is not one is left out; the rest of its batch stays
        assert!(log_line(&json!([1, 2, 3])).is_none());
        assert!(log_line(&json!([1, 2, 3, 4, null, "loud", 0, "t", "m"])).is_none());
        assert!(log_line(&json!([1, 2, 3, 4, null, "W", 0, "t", "m"])).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
