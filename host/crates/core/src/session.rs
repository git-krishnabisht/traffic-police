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
/// The most of a session file that is read, inflated. The frames are gzip, and gzip inflates a
/// thousand times: a file of a few MB could hold gigabytes. What comes after this is not read,
/// and [`Opened::too_large`] says so.
pub const MAX_INFLATED: u64 = 4 << 30;
/// A recording's gzip stream is flushed at least this often.
const FLUSH_EVERY: Duration = Duration::from_secs(5);
/// Lines of the device's log in one frame of a session file (type 20)...
const LOG_BATCH: usize = 2000;
/// ...and at most this many bytes of them, well under the 16 MiB frame limit: a message can be
/// long, and quotes and control characters take up to six times the room once escaped (up to
/// 0.5.1 only the count was kept, and a batch of such lines stopped the program while it saved).
const LOG_BATCH_BYTES: usize = 4 << 20;

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
        if let Err(e) = result {
            Self::failed(inner, e.to_string());
        }
    }

    /// Something could not be recorded: logged, the first such problem kept.
    fn failed(inner: &mut Inner, e: String) {
        if inner.error.is_none() {
            tracing::warn!("session log: {e}");
            inner.error = Some(e);
        }
    }

    fn record(&self, kind: u8, v: &Value) {
        self.record_raw(kind, &serde_json::to_vec(v).expect("serializable"));
    }

    /// A host record whose JSON is `payload`; one over the frame limit is left out.
    fn record_raw(&self, kind: u8, payload: &[u8]) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match frame::encode_raw(kind, payload, &mut inner.buf) {
            Ok(()) => Self::write(&mut inner),
            Err(e) => Self::failed(&mut inner, format!("a record left out: {e}")),
        }
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
            if frame::encode_raw(kind::SOURCE, &json, &mut inner.buf).is_ok() {
                inner.current = Some(source);
            }
        }
        // a device frame was decoded within the limit; a backend's own (Flutter) might not be
        if let Err(e) = frame::encode(f, &mut inner.buf) {
            Self::failed(&mut inner, format!("a frame left out: {e}"));
        }
        Self::write(&mut inner);
    }

    pub fn record_source_end(&self, source: SourceId, at: Ts, reason: &str) {
        self.record(kind::SOURCE_END, &json!({ "t": "source_end", "source": source, "ts": at, "reason": reason }));
    }

    /// Ends a recording: the device's log (when it was read), the annotations, then the end of
    /// the gzip stream.
    pub fn finish(&self, store: &SessionStore) -> io::Result<()> {
        log_records(store, None, |record| {
            self.record_raw(kind::LOG, record);
            Ok(())
        })?;
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
        self.export_until(store, out, keep, &|| false)
    }

    /// [`SessionLog::export`], asking `stop` as it goes (between two pieces of the stream, and
    /// two records of the log); when it says so the export ends with an error of kind
    /// `Interrupted`, and the file is left unfinished.
    pub fn export_until(
        &self,
        store: &SessionStore,
        out: &Path,
        keep: Option<&HashSet<TxnKey>>,
        stop: &dyn Fn() -> bool,
    ) -> io::Result<usize> {
        // the device's log: all of it, or with some requests the lines from the first's start to
        // the last's end
        let span = keep.map(|keep| {
            let kept = store.txns().iter().filter(|t| keep.contains(&t.key));
            kept.fold((Ts::MAX, 0), |(a, b), t| (a.min(t.start), b.max(t.end.unwrap_or(t.start))))
        });
        self.export_spanning(store, out, keep, span, stop)
    }

    /// [`SessionLog::export_until`], given the time the requests of `keep` span (`record
    /// --filter` lets requests go as it writes them, so the store no longer has them all).
    pub fn export_spanning(
        &self,
        store: &SessionStore,
        out: &Path,
        keep: Option<&HashSet<TxnKey>>,
        span: Option<(Ts, Ts)>,
        stop: &dyn Fn() -> bool,
    ) -> io::Result<usize> {
        let stopped = || io::Error::new(io::ErrorKind::Interrupted, "stopped");
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
        frame::encode_raw(kind::SESSION, &serde_json::to_vec(&session_record(now_wall_ms()))?, &mut buf)
            .map_err(io::Error::other)?;
        gz.write_all(&buf)?;
        let mut reader = BufReader::new(File::open(&path)?);
        let mut decoder = Decoder::new();
        let mut chunk = vec![0u8; 256 * 1024];
        let mut current: Option<SourceId> = None;
        loop {
            if stop() {
                return Err(stopped());
            }
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
                frame::encode(&f, &mut buf).map_err(io::Error::other)?;
                gz.write_all(&buf)?;
            }
        }
        log_records(store, span, |record| {
            if stop() {
                return Err(stopped());
            }
            buf.clear();
            frame::encode_raw(kind::LOG, record, &mut buf).map_err(io::Error::other)?;
            gz.write_all(&buf)
        })?;
        buf.clear();
        frame::encode_raw(kind::ANNOTATIONS, &serde_json::to_vec(&annotations(store))?, &mut buf)
            .map_err(io::Error::other)?;
        gz.write_all(&buf)?;
        gz.finish()?.flush()?;
        Ok(keep.map_or(store.len(), HashSet::len))
    }
}

/// The device's log as session file records (type 20), each handed to `emit` as its JSON: what
/// the reader knew in the first, then the lines (in `span` when given) in batches of at most
/// [`LOG_BATCH`] lines and [`LOG_BATCH_BYTES`]. Nothing when the log was not read. A line that
/// alone is over the byte limit (only a file made elsewhere could have one) is left out.
fn log_records(
    store: &SessionStore,
    span: Option<(Ts, Ts)>,
    mut emit: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    const HEAD: &[u8] = br#"{"t":"log","lines":["#;
    const TAIL: &[u8] = b"]}";
    let logs = store.logs();
    let info = logs.info();
    if logs.is_empty() && info.package.is_none() {
        return Ok(());
    }
    let mut processes: Vec<(u32, &str)> = logs.processes().collect();
    processes.sort_unstable();
    let first = json!({
        "t": "log",
        "info": { "device": info.device, "package": info.package, "uid": info.uid, "processes": processes },
        "lines": [],
    });
    emit(&serde_json::to_vec(&first)?)?;
    let mut record = HEAD.to_vec();
    let mut lines = 0;
    for l in logs.iter_from(logs.first_id()) {
        if span.is_some_and(|(a, b)| l.ts < a || l.ts > b) {
            continue;
        }
        let line = json!([l.ts, l.wall_ms, l.pid, l.tid, l.uid, l.level.letter(), l.buffer, l.tag, l.message]);
        let line = serde_json::to_vec(&line)?;
        if HEAD.len() + line.len() + TAIL.len() > LOG_BATCH_BYTES {
            tracing::warn!("a log line of {} bytes left out of the session file", line.len());
            continue;
        }
        if lines == LOG_BATCH || record.len() + 1 + line.len() + TAIL.len() > LOG_BATCH_BYTES {
            record.extend_from_slice(TAIL);
            emit(&record)?;
            record.truncate(HEAD.len());
            lines = 0;
        }
        if lines > 0 {
            record.push(b',');
        }
        record.extend_from_slice(&line);
        lines += 1;
    }
    if lines > 0 {
        record.extend_from_slice(TAIL);
        emit(&record)?;
    }
    Ok(())
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
    /// The file holds more than [`MAX_INFLATED`] once inflated: what comes before is kept, the
    /// rest was not read.
    pub too_large: bool,
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

/// Reads a session file, handing its events to `sink` a batch at a time as they are read (see
/// [`read_into`]).
pub fn open_into(path: &Path, ids: &SourceIds, sink: &mut dyn FnMut(Vec<SessionEvent>)) -> io::Result<Opened> {
    read_into(BufReader::new(File::open(path)?), ids, sink)
}

/// Reads a session file's bytes into events (see [`open`]). Nothing in them is trusted: a file
/// from someone else may be damaged or made up (the fuzz target `session_file` feeds it noise).
pub fn read(file: impl Read, ids: &SourceIds) -> io::Result<Opened> {
    let mut events = Vec::new();
    let mut opened = read_into(file, ids, &mut |batch| events.extend(batch))?;
    opened.events = events;
    Ok(opened)
}

/// Reads a session file's bytes as [`read`] does, handing the events to `sink` a batch at a time
/// as the file is inflated, so that a store can take them (and spill their bodies) as they come:
/// the file is never in memory whole, inflated or as events (up to 0.5.1 it was inflated into
/// one buffer, with no limit). The returned [`Opened`] has no events.
pub fn read_into(file: impl Read, ids: &SourceIds, sink: &mut dyn FnMut(Vec<SessionEvent>)) -> io::Result<Opened> {
    read_within(file, ids, sink, MAX_INFLATED)
}

/// [`read_into`], reading at most `max` bytes of the inflated stream.
fn read_within(
    mut file: impl Read,
    ids: &SourceIds,
    sink: &mut dyn FnMut(Vec<SessionEvent>),
    max: u64,
) -> io::Result<Opened> {
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
    let mut gz = GzDecoder::new(file);
    let mut decoder = Decoder::new();
    let mut piece = vec![0u8; 256 * 1024];
    let mut reader = Reader {
        ids,
        out: Opened::default(),
        events: Vec::new(),
        markers: Vec::new(),
        map: HashMap::new(),
        normalizers: HashMap::new(),
        current: None,
    };
    let mut inflated = 0u64;
    loop {
        let n = match gz.read(&mut piece) {
            Ok(0) => break,
            Ok(n) => n,
            // a recording cut short ends without the gzip trailer: keep what decodes
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof || e.kind() == io::ErrorKind::InvalidInput => {
                reader.out.truncated = true;
                break;
            }
            Err(e) => {
                if inflated == 0 {
                    return Err(e);
                }
                reader.out.truncated = true;
                break;
            }
        };
        let room = usize::try_from(max - inflated).unwrap_or(usize::MAX);
        decoder.push(&piece[..n.min(room)]);
        inflated += n.min(room) as u64;
        if !reader.frames(&mut decoder) {
            break;
        }
        if !reader.events.is_empty() {
            sink(std::mem::take(&mut reader.events));
        }
        if n > room {
            reader.out.too_large = true;
            break;
        }
    }
    // the annotations come last in a file (the last ones win): their markers, then
    reader.events.append(&mut reader.markers);
    if !reader.events.is_empty() {
        sink(std::mem::take(&mut reader.events));
    }
    Ok(reader.out)
}

/// What reading a session file has found so far.
struct Reader<'a> {
    ids: &'a SourceIds,
    out: Opened,
    /// Events not handed on yet.
    events: Vec<SessionEvent>,
    /// The markers of the last annotations record.
    markers: Vec<SessionEvent>,
    map: HashMap<SourceId, SourceId>,
    normalizers: HashMap<SourceId, Normalizer>,
    current: Option<SourceId>,
}

impl Reader<'_> {
    /// The frames `decoder` holds; false when the stream can no longer be read.
    fn frames(&mut self, decoder: &mut Decoder) -> bool {
        loop {
            match decoder.next_frame() {
                Ok(Some(f)) => self.frame(f),
                Ok(None) => return true,
                Err(e) if e.is_fatal() => {
                    self.out.truncated = true;
                    return false;
                }
                Err(_) => continue,
            }
        }
    }

    fn frame(&mut self, f: Frame) {
        match f {
            Frame::Other { kind: kind::SESSION, payload } => {
                self.out.created_wall_ms =
                    serde_json::from_slice::<Value>(&payload).ok().and_then(|v| v["created_wall_ms"].as_i64());
            }
            Frame::Other { kind: kind::SOURCE, payload } => {
                let Ok(r) = serde_json::from_slice::<SourceRecord>(&payload) else { return };
                self.current = Some(r.source);
                if let Some(hello) = r.hello {
                    let id = match (r.resumed, self.map.get(&r.source)) {
                        (true, Some(&id)) => id,
                        _ => {
                            let id = self.ids.next();
                            self.map.insert(r.source, id);
                            self.normalizers.insert(r.source, Normalizer::new(id));
                            id
                        }
                    };
                    let (label, serial) =
                        r.device.map_or((String::from("recorded device"), None), |d| (d.label, d.serial));
                    let mut info = SourceInfo::from_hello(id, &hello, label, serial);
                    if r.resumed {
                        info.started = info.clock.map_or(info.started, |(ts, _)| ts);
                    }
                    self.events.push(SessionEvent::SourceUp(Box::new(info)));
                }
            }
            Frame::Other { kind: kind::SOURCE_END, payload } => {
                if let Ok(r) = serde_json::from_slice::<EndRecord>(&payload)
                    && let Some(&id) = self.map.get(&r.source)
                {
                    self.events.push(SessionEvent::SourceDown { source: id, at: r.ts, reason: r.reason });
                }
            }
            Frame::Other { kind: kind::ANNOTATIONS, payload } => {
                // the last one wins: its pins, and its markers (a file made up of the same
                // record many times would otherwise add them each time)
                if let Ok(a) = serde_json::from_slice::<Annotations>(&payload) {
                    self.out.pins = a
                        .pins
                        .iter()
                        .filter_map(|p| self.map.get(&p.source).map(|&source| TxnKey { source, txn: p.txn }))
                        .collect();
                    self.markers = a
                        .markers
                        .into_iter()
                        .map(|m| {
                            let kind = match m.kind.as_str() {
                                "pause" => MarkerKind::Pause,
                                "resume" => MarkerKind::Resume,
                                _ => MarkerKind::Note,
                            };
                            SessionEvent::Marker { source: None, at: m.at, kind, label: m.label }
                        })
                        .collect();
                }
            }
            Frame::Other { kind: kind::LOG, payload } => {
                let Ok(v) = serde_json::from_slice::<Value>(&payload) else { return };
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
                    self.events.push(SessionEvent::LogInfo(Box::new(LogInfo {
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
                    self.events.push(SessionEvent::Logs(lines));
                }
            }
            Frame::Other { .. } => {}
            device => {
                let Some(n) = self.current.and_then(|c| self.normalizers.get_mut(&c)) else { return };
                match n.frame(device, &mut self.events) {
                    Ok(Some(Control::Hello(_))) | Ok(_) => {}
                    Err(e) => tracing::debug!("session file: {e}"),
                }
            }
        }
    }
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

    /// A long recording is read a piece at a time: its events come in batches as the file is
    /// inflated, so the store takes them as they come; cut short or past the limit, what came
    /// before is kept.
    #[test]
    fn a_file_is_read_in_pieces_up_to_the_limit() {
        let dir = scratch("pieces");
        let path = dir.join("long.trafficpolice");
        let device = DeviceRecord { label: "demo".into(), serial: None };
        let log = SessionLog::file(&path).unwrap();
        log.source(1, &device, &hello(7, 5), false);
        for txn in 1..=2000 {
            request(&log, 1, txn, txn * 5);
            let chunk = BodyChunk {
                seq: txn * 5 + 4,
                txn,
                dir: BodyDir::Request,
                ts: 10 + txn * 5,
                offset: 0,
                data: Bytes::from(format!("{txn:01000}")),
            };
            log.frame(1, &Frame::Body(chunk));
        }
        log.finish(&SessionStore::new()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let (mut batches, mut events) = (0, 0);
        let opened = read_into(&bytes[..], &SourceIds::default(), &mut |b| {
            batches += 1;
            events += b.len();
        })
        .unwrap();
        assert!(batches > 5 && events > 10_000, "{batches} batches, {events} events");
        assert!(!opened.truncated && !opened.too_large && opened.events.is_empty());
        // read up to 1 MB inflated: the requests in it, and the file is said to hold more
        let mut store = SessionStore::new();
        let ids = store.source_ids();
        let part = read_within(&bytes[..], &ids, &mut |b| store.apply_all(b), 1 << 20).unwrap();
        assert!(part.too_large && !part.truncated);
        assert!((100..2000).contains(&store.len()), "{}", store.len());
        let t = store.txn(5);
        assert_eq!(store.body_bytes(&t.req_body).len(), 1000);
        // cut in the middle: what decodes before the cut is kept
        let mut store = SessionStore::new();
        let ids = store.source_ids();
        let cut = read_into(&bytes[..bytes.len() / 2], &ids, &mut |b| store.apply_all(b)).unwrap();
        assert!(cut.truncated && !cut.too_large);
        assert!((100..2000).contains(&store.len()), "{}", store.len());
        assert_eq!(MAX_INFLATED, 4 << 30, "as the UI and the headless commands say");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file of a few KB that inflates to megabytes (the same record over and over): reading
    /// stops at the limit, and the annotations' markers are the last record's only.
    #[test]
    fn a_gzip_bomb_stops_at_the_limit() {
        let record = json!({ "t": "annotations", "pins": [], "markers": [{ "at": 5, "kind": "note", "label": "x" }] });
        let record = serde_json::to_vec(&record).unwrap();
        let mut frames = BytesMut::new();
        for _ in 0..200_000 {
            frame::encode_raw(kind::ANNOTATIONS, &record, &mut frames).unwrap();
        }
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        gz.write_all(&frames).unwrap();
        let file = [MAGIC.as_slice(), &[VERSION], &gz.finish().unwrap()].concat();
        assert!(file.len() * 100 < frames.len(), "{} of {}", file.len(), frames.len());
        let mut events = Vec::new();
        let opened = read_within(&file[..], &SourceIds::default(), &mut |b| events.extend(b), 1 << 20).unwrap();
        assert!(opened.too_large);
        assert_eq!(events.len(), 1, "one marker, the last record's: {events:?}");
        // read whole, the same one marker
        let opened = read(&file[..], &SourceIds::default()).unwrap();
        assert!(!opened.too_large);
        assert_eq!(opened.events.len(), 1);
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

    /// 2,000 lines whose escaped JSON is over the 16 MiB frame limit (up to 0.5.1 saving them
    /// stopped the program): the records stay under the byte limit, and every line comes back.
    #[test]
    fn long_escaped_log_lines_are_batched_by_bytes_too() {
        let dir = scratch("escaped");
        let device = DeviceRecord { label: "demo".into(), serial: None };
        let log = SessionLog::temporary().unwrap();
        let mut store = SessionStore::new();
        let src = store.source_ids().next();
        log.source(src, &device, &hello(7, 5), false);
        // a control character is escaped as \u0001: 9 KB a line, 18 MB for the 2,000
        let message = "\u{1}".repeat(1500);
        let lines: Vec<LogLine> = (0..2000)
            .map(|i| LogLine {
                ts: 10 + i,
                wall_ms: 1_790_000_000_000,
                pid: 7,
                tid: 7,
                uid: None,
                level: Level::Info,
                buffer: 0,
                tag: "Escaped".into(),
                message: message.clone(),
            })
            .collect();
        store.apply(SessionEvent::Logs(lines));
        let mut sizes = Vec::new();
        log_records(&store, None, |r| {
            sizes.push(r.len());
            Ok(())
        })
        .unwrap();
        assert!(sizes.len() >= 6 && sizes.iter().all(|&n| n <= LOG_BATCH_BYTES), "{sizes:?}");
        let path = dir.join("escaped.trafficpolice");
        log.export(&store, &path, None).unwrap();
        let back = store_of(&open(&path, &SourceIds::default()).unwrap());
        assert_eq!(back.logs().len(), 2000);
        assert!(back.logs().iter_from(0).all(|l| l.message == message));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
