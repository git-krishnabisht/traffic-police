//! The device's log as Android's `logcat` writes it in binary (`logcat -B`), which Logdawg reads
//! (ARCHITECTURE.md §5.17): one record per entry, so nothing is parsed out of text, and a
//! message of several lines is one entry.
//!
//! A record is liblog's `logger_entry`: the payload's length and the header's size (both u16),
//! then pid, tid, seconds and nanoseconds of the wall clock, and from version 3 the log buffer's
//! id and from version 4 the writer's uid (all 32 bits, little-endian). Android 8 to 17 write
//! version 4 (28 bytes; checked on API 26, 31 and 37 emulators). For the text buffers (main,
//! system, crash, radio) the payload is the priority (one byte), the tag and the message, each
//! ending in a zero byte.

use bytes::{Buf, BytesMut};

/// Log buffer ids (`log_id_t`).
pub const BUFFER_MAIN: u8 = 0;
pub const BUFFER_RADIO: u8 = 1;
pub const BUFFER_EVENTS: u8 = 2;
pub const BUFFER_SYSTEM: u8 = 3;
pub const BUFFER_CRASH: u8 = 4;
pub const BUFFER_STATS: u8 = 5;
pub const BUFFER_SECURITY: u8 = 6;
pub const BUFFER_KERNEL: u8 = 7;

/// The smallest header (version 1: no buffer id, no uid) and a bound past any version so far.
const MIN_HEADER: usize = 20;
const MAX_HEADER: usize = 128;

/// One entry as logcat wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub pid: u32,
    pub tid: u32,
    /// Wall clock (CLOCK_REALTIME) when it was written.
    pub sec: u32,
    pub nsec: u32,
    /// The buffer (`BUFFER_*`); main when the header is too old to say.
    pub buffer: u8,
    /// The writer's uid (from header version 4).
    pub uid: Option<u32>,
    /// Android's priority: 2 verbose, 3 debug, 4 info, 5 warning, 6 error, 7 assert.
    pub priority: u8,
    pub tag: String,
    pub message: String,
}

impl Record {
    /// Wall-clock nanoseconds since the Unix epoch.
    pub fn wall_ns(&self) -> i128 {
        i128::from(self.sec) * 1_000_000_000 + i128::from(self.nsec)
    }
}

/// What does not read as a stream of records: it is not logcat's binary output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a logcat record: header of {header} bytes for a payload of {len}")]
pub struct BadRecord {
    pub header: usize,
    pub len: usize,
}

/// Records out of bytes in pieces of any size.
#[derive(Debug, Default)]
pub struct Parser {
    buf: BytesMut,
}

impl Parser {
    pub fn new() -> Parser {
        Parser::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes waiting for the rest of their record.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// The next whole record, if one is there. Records of the binary buffers (events, stats,
    /// security) are skipped: they are not text.
    pub fn next_record(&mut self) -> Result<Option<Record>, BadRecord> {
        loop {
            if self.buf.len() < 4 {
                return Ok(None);
            }
            let len = usize::from(u16::from_le_bytes([self.buf[0], self.buf[1]]));
            let header = usize::from(u16::from_le_bytes([self.buf[2], self.buf[3]]));
            if !(MIN_HEADER..=MAX_HEADER).contains(&header) {
                return Err(BadRecord { header, len });
            }
            if self.buf.len() < header + len {
                return Ok(None);
            }
            let word =
                |at: usize| u32::from_le_bytes([self.buf[at], self.buf[at + 1], self.buf[at + 2], self.buf[at + 3]]);
            let (pid, tid, sec, nsec) = (word(4), word(8), word(12), word(16));
            let buffer = if header >= 24 { word(20).min(255) as u8 } else { BUFFER_MAIN };
            let uid = (header >= 28).then(|| word(24));
            let payload = &self.buf[header..header + len];
            let record = if matches!(buffer, BUFFER_EVENTS | BUFFER_STATS | BUFFER_SECURITY) || payload.is_empty() {
                None
            } else {
                let priority = payload[0];
                let rest = &payload[1..];
                let tag_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
                let tag = String::from_utf8_lossy(&rest[..tag_end]).into_owned();
                let msg = rest.get(tag_end + 1..).unwrap_or_default();
                let msg_end = msg.iter().position(|&b| b == 0).unwrap_or(msg.len());
                // logcat's text output drops the newline a message ends with, and so does this
                let message = String::from_utf8_lossy(&msg[..msg_end]).trim_end_matches(['\n', '\r']).to_string();
                Some(Record { pid, tid, sec, nsec, buffer, uid, priority, tag, message })
            };
            self.buf.advance(header + len);
            if let Some(r) = record {
                return Ok(Some(r));
            }
        }
    }
}

/// A record as logcat writes it (header version 4), for stand-in devices and tests.
pub fn encode(r: &Record) -> Vec<u8> {
    let mut payload = vec![r.priority];
    payload.extend_from_slice(r.tag.as_bytes());
    payload.push(0);
    payload.extend_from_slice(r.message.as_bytes());
    payload.push(0);
    let mut out = Vec::with_capacity(28 + payload.len());
    out.extend_from_slice(&(payload.len().min(usize::from(u16::MAX)) as u16).to_le_bytes());
    out.extend_from_slice(&28u16.to_le_bytes());
    for w in [r.pid, r.tid, r.sec, r.nsec, u32::from(r.buffer), r.uid.unwrap_or(0)] {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.extend_from_slice(&payload);
    out
}

/// The `logcat` arguments for buffers by name (`main`, `system`, `crash`, `radio`), reading from
/// `start` on: a count of the latest entries, or a wall-clock time (`seconds.millis`) to resume
/// after a gap without repeating what was read.
pub fn command(buffers: &[String], start: Start) -> String {
    let buffers: Vec<&str> = buffers
        .iter()
        .map(String::as_str)
        .filter(|b| matches!(*b, "main" | "system" | "crash" | "radio" | "kernel"))
        .collect();
    let buffers = if buffers.is_empty() { "main,system,crash".to_string() } else { buffers.join(",") };
    let start = match start {
        Start::Latest(n) => n.max(1).to_string(),
        Start::Since { sec, millis } => format!("{sec}.{millis:03}"),
    };
    format!("logcat -B -b {buffers} -T {start}")
}

/// Where a stream of the log starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// The latest `n` entries, then what comes.
    Latest(u32),
    /// From this wall-clock time on (logcat's `-T sssss.mmm`).
    Since { sec: u32, millis: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn record(
        pid: u32,
        tid: u32,
        sec: u32,
        nsec: u32,
        buffer: u8,
        uid: u32,
        priority: u8,
        tag: &str,
        msg: &str,
    ) -> Vec<u8> {
        encode(&Record { pid, tid, sec, nsec, buffer, uid: Some(uid), priority, tag: tag.into(), message: msg.into() })
    }

    #[test]
    fn records_in_pieces_of_any_size() {
        let mut bytes = record(4312, 4331, 1_790_000_000, 123_456_789, 0, 10_234, 3, "OkHttp", "--> GET /api\n");
        bytes.extend(record(
            612,
            640,
            1_790_000_001,
            5,
            4,
            1000,
            7,
            "AndroidRuntime",
            "FATAL EXCEPTION: main\n\tat a.b(C.java:1)",
        ));
        for step in [1, 2, 7, 64, bytes.len()] {
            let mut p = Parser::new();
            let mut got = Vec::new();
            for piece in bytes.chunks(step) {
                p.push(piece);
                while let Some(r) = p.next_record().unwrap() {
                    got.push(r);
                }
            }
            assert_eq!(got.len(), 2, "pieces of {step}");
            assert_eq!(p.pending(), 0);
            let r = &got[0];
            assert_eq!((r.pid, r.tid, r.uid, r.priority, r.buffer), (4312, 4331, Some(10_234), 3, BUFFER_MAIN));
            assert_eq!((r.tag.as_str(), r.message.as_str()), ("OkHttp", "--> GET /api"));
            assert_eq!(r.wall_ns(), 1_790_000_000_123_456_789);
            assert_eq!(got[1].buffer, BUFFER_CRASH);
            assert_eq!(got[1].message, "FATAL EXCEPTION: main\n\tat a.b(C.java:1)", "one entry, all its lines");
        }
    }

    #[test]
    fn older_headers_binary_buffers_and_garbage() {
        // version 1: 20 bytes, no buffer id and no uid
        let mut v1 = record(1, 2, 3, 4, 0, 0, 4, "tag", "msg");
        v1[2..4].copy_from_slice(&20u16.to_le_bytes());
        v1.drain(20..28);
        let mut p = Parser::new();
        p.push(&v1);
        let r = p.next_record().unwrap().unwrap();
        assert_eq!((r.uid, r.buffer, r.tag.as_str(), r.message.as_str()), (None, BUFFER_MAIN, "tag", "msg"));
        // an events-buffer record is skipped, the next one read
        let mut both = record(1, 2, 3, 4, BUFFER_EVENTS, 0, 4, "x", "y");
        both.extend(record(5, 6, 7, 8, 0, 0, 5, "next", "one"));
        let mut p = Parser::new();
        p.push(&both);
        assert_eq!(p.next_record().unwrap().unwrap().tag, "next");
        // text that is not logcat's binary output
        let mut p = Parser::new();
        p.push(b"--------- beginning of main\n");
        assert!(p.next_record().is_err());
    }

    #[test]
    fn real_records_from_android_8_12_and_17() {
        // `adb exec-out logcat -B -d -t 40 -b main,system,crash` on API 26, 31 and 37 emulators
        for (api, file) in [(26, "api26.bin"), (31, "api31.bin"), (37, "api37.bin")] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/logcat").join(file);
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let mut p = Parser::new();
            p.push(&bytes);
            let mut n = 0;
            while let Some(r) = p.next_record().unwrap() {
                assert!((2..=7).contains(&r.priority), "API {api}: priority {}", r.priority);
                assert!(r.uid.is_some(), "API {api}: version 4 headers carry the uid");
                assert!(matches!(r.buffer, BUFFER_MAIN | BUFFER_SYSTEM | BUFFER_CRASH), "API {api}");
                assert!(r.sec > 1_700_000_000, "API {api}: a wall-clock time");
                n += 1;
            }
            assert_eq!((n, p.pending()), (40, 0), "API {api}");
        }
    }

    #[test]
    fn the_command_names_buffers_and_where_to_start() {
        let b = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(command(&b(&["main", "crash"]), Start::Latest(5000)), "logcat -B -b main,crash -T 5000");
        assert_eq!(
            command(&b(&["events", "nonsense"]), Start::Since { sec: 1_790_000_000, millis: 7 }),
            "logcat -B -b main,system,crash -T 1790000000.007"
        );
    }
}
