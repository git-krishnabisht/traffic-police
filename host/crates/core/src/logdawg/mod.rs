//! Logdawg: the device's log (logcat) beside the network (ARCHITECTURE.md §5.17).
//!
//! [`LogStore`] keeps the lines in chunks of [`CHUNK`] behind `Arc`s, so the copy a frozen view
//! takes shares them, and the oldest go a whole chunk at a time once the lines pass the memory
//! they may use (`[logdawg] keep`). Each line is a fixed record (about 50 bytes) with its message
//! in its chunk's text; tags are kept once each. A line's id counts every line the session read,
//! so ids stay valid while older lines go.

pub mod filter;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::fmt::Ts;

/// Lines per chunk.
pub const CHUNK: usize = 4096;
/// Memory the lines may use unless `[logdawg] keep` says otherwise.
pub const DEFAULT_KEEP: usize = 64 * 1024 * 1024;

/// A line's priority, as Android names them (`V` `D` `I` `W` `E` `A`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Level {
    Verbose,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
    Assert,
}

impl Level {
    pub const ALL: [Level; 6] = [Level::Verbose, Level::Debug, Level::Info, Level::Warn, Level::Error, Level::Assert];

    /// Android's priority number (2 verbose to 7 assert).
    pub fn from_priority(p: u8) -> Level {
        match p {
            0..=2 => Level::Verbose,
            3 => Level::Debug,
            4 => Level::Info,
            5 => Level::Warn,
            6 => Level::Error,
            _ => Level::Assert,
        }
    }

    pub fn letter(self) -> char {
        match self {
            Level::Verbose => 'V',
            Level::Debug => 'D',
            Level::Info => 'I',
            Level::Warn => 'W',
            Level::Error => 'E',
            Level::Assert => 'A',
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Verbose => "verbose",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
            Level::Assert => "assert",
        }
    }

    /// `v`, `verbose`, `d`, `debug`, `i`, `info`, `w`, `warn`, `warning`, `e`, `error`, `a`,
    /// `assert`, `f`, `fatal` (any case).
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "v" | "verbose" => Level::Verbose,
            "d" | "debug" => Level::Debug,
            "i" | "info" => Level::Info,
            "w" | "warn" | "warning" => Level::Warn,
            "e" | "error" => Level::Error,
            "a" | "assert" | "f" | "fatal" => Level::Assert,
            _ => return None,
        })
    }
}

/// The crash buffer's id (logcat's `-b crash`).
pub const BUFFER_CRASH: u8 = 4;

/// One line of the device's log, as a reader sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// The device's boot clock, the timeline's (mapped from the wall clock it was written at).
    pub ts: Ts,
    /// The wall clock it was written at, Unix milliseconds.
    pub wall_ms: i64,
    pub pid: u32,
    pub tid: u32,
    /// The writer's uid, where the device says (Android 8 and newer).
    pub uid: Option<u32>,
    pub level: Level,
    /// logcat's buffer id: 0 main, 1 radio, 3 system, 4 crash, 7 kernel.
    pub buffer: u8,
    pub tag: String,
    /// All of it: a message of several lines (a stack trace) is one line of the log.
    pub message: String,
}

/// What a reader learned about the log besides its lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogInfo {
    /// The device whose log it is.
    pub device: Option<String>,
    /// The session's app, and the uid its processes run as (`package:mine`).
    pub package: Option<String>,
    pub uid: Option<u32>,
    /// Process names by pid, as they become known.
    pub processes: Vec<(u32, String)>,
    /// What the reader is doing, for an empty view: reading, waiting for the device, an error.
    pub status: Option<String>,
}

/// A line as the store holds it.
#[derive(Debug, Clone, Copy)]
struct Entry {
    ts: Ts,
    wall_ms: i64,
    pid: u32,
    tid: u32,
    uid: u32,
    tag: u32,
    start: u32,
    len: u32,
    level: Level,
    buffer: u8,
}

const NO_UID: u32 = u32::MAX;

#[derive(Debug, Clone, Default)]
struct Chunk {
    entries: Vec<Entry>,
    text: String,
    bytes: usize,
}

#[derive(Debug, Clone, Default)]
struct Tags {
    names: Vec<Arc<str>>,
    index: HashMap<Arc<str>, u32>,
}

/// A line of the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line<'a> {
    pub id: u64,
    pub ts: Ts,
    pub wall_ms: i64,
    pub pid: u32,
    pub tid: u32,
    pub uid: Option<u32>,
    pub level: Level,
    pub buffer: u8,
    /// The tag's number in the store (the same tag, the same number).
    pub tag_id: u32,
    pub tag: &'a str,
    pub message: &'a str,
}

/// The session's log lines (see the module's documentation).
#[derive(Debug, Clone)]
pub struct LogStore {
    chunks: VecDeque<Arc<Chunk>>,
    /// The id of the first line kept (the first chunk's first line).
    first: u64,
    /// The id the next line gets.
    end: u64,
    tags: Arc<Tags>,
    processes: Arc<HashMap<u32, Arc<str>>>,
    info: LogInfo,
    bytes: usize,
    keep: usize,
    dropped: u64,
    latest: Ts,
    generation: u64,
}

impl Default for LogStore {
    fn default() -> Self {
        LogStore {
            chunks: VecDeque::new(),
            first: 0,
            end: 0,
            tags: Arc::default(),
            processes: Arc::default(),
            info: LogInfo::default(),
            bytes: 0,
            keep: DEFAULT_KEEP,
            dropped: 0,
            latest: 0,
            generation: 0,
        }
    }
}

impl LogStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The memory the lines may use (`[logdawg] keep`); older lines go when they pass it.
    pub fn set_keep(&mut self, bytes: usize) {
        self.keep = bytes.max(CHUNK * 256);
        self.evict();
    }

    pub fn keep(&self) -> usize {
        self.keep
    }

    /// Ids `first..end` are kept.
    pub fn first_id(&self) -> u64 {
        self.first
    }

    pub fn end_id(&self) -> u64 {
        self.end
    }

    pub fn len(&self) -> usize {
        (self.end - self.first) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.first
    }

    /// Lines that went to make room (`keep`).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Bumped on every change, for views that follow the store.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The newest line's time.
    pub fn latest(&self) -> Ts {
        self.latest
    }

    /// The memory the lines use now (their records and text).
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn info(&self) -> &LogInfo {
        &self.info
    }

    pub fn uid(&self) -> Option<u32> {
        self.info.uid
    }

    pub fn package(&self) -> Option<&str> {
        self.info.package.as_deref()
    }

    /// A process's name, once the reader learned it.
    pub fn process(&self, pid: u32) -> Option<&str> {
        self.processes.get(&pid).map(|n| &**n)
    }

    /// A line's process: its name once the reader learned it, else the app's package for a line
    /// with the app's uid (a process of the app's that ended before its name was read).
    pub fn process_of(&self, pid: u32, uid: Option<u32>) -> Option<&str> {
        self.process(pid).or_else(|| uid.filter(|u| Some(*u) == self.uid()).and(self.package()))
    }

    /// Tag number `id`'s text.
    pub fn tag(&self, id: u32) -> &str {
        self.tags.names.get(id as usize).map_or("", |t| t)
    }

    /// How many different tags there are.
    pub fn tag_count(&self) -> usize {
        self.tags.names.len()
    }

    pub fn apply_info(&mut self, info: LogInfo) {
        self.generation += 1;
        if !info.processes.is_empty() {
            let processes = Arc::make_mut(&mut self.processes);
            for (pid, name) in &info.processes {
                processes.insert(*pid, Arc::from(name.as_str()));
            }
        }
        let LogInfo { device, package, uid, processes: _, status } = info;
        if device.is_some() {
            self.info.device = device;
        }
        // another app (picked in the session): the uid that was the app's is not its
        if let Some(p) = package {
            if self.info.package.as_deref() != Some(p.as_str()) {
                self.info.uid = None;
            }
            self.info.package = Some(p);
        }
        if uid.is_some() {
            self.info.uid = uid;
        }
        if status.is_some() {
            self.info.status = status;
        }
    }

    pub fn push(&mut self, line: LogLine) {
        self.generation += 1;
        self.latest = self.latest.max(line.ts);
        let tag = self.intern(&line.tag);
        if self.chunks.back().is_none_or(|c| c.entries.len() >= CHUNK) {
            self.chunks.push_back(Arc::new(Chunk { entries: Vec::with_capacity(CHUNK), ..Chunk::default() }));
        }
        let chunk = Arc::make_mut(self.chunks.back_mut().expect("a chunk was just made"));
        let start = chunk.text.len() as u32;
        chunk.text.push_str(&line.message);
        chunk.entries.push(Entry {
            ts: line.ts,
            wall_ms: line.wall_ms,
            pid: line.pid,
            tid: line.tid,
            uid: line.uid.unwrap_or(NO_UID),
            tag,
            start,
            len: line.message.len() as u32,
            level: line.level,
            buffer: line.buffer,
        });
        let size = std::mem::size_of::<Entry>() + line.message.len();
        chunk.bytes += size;
        self.bytes += size;
        self.end += 1;
        self.evict();
    }

    pub fn extend(&mut self, lines: impl IntoIterator<Item = LogLine>) {
        for l in lines {
            self.push(l);
        }
    }

    fn intern(&mut self, tag: &str) -> u32 {
        if let Some(&id) = self.tags.index.get(tag) {
            return id;
        }
        let tags = Arc::make_mut(&mut self.tags);
        let id = tags.names.len() as u32;
        let name: Arc<str> = Arc::from(tag);
        tags.names.push(name.clone());
        tags.index.insert(name, id);
        self.bytes += tag.len() + 32;
        id
    }

    /// The oldest whole chunks go while the lines use more than `keep` (the newest chunk stays).
    fn evict(&mut self) {
        while self.bytes > self.keep && self.chunks.len() > 1 {
            let c = self.chunks.pop_front().expect("more than one chunk");
            self.bytes -= c.bytes;
            self.first += c.entries.len() as u64;
            self.dropped += c.entries.len() as u64;
        }
    }

    /// Every line goes; ids go on from where they were, and what the reader said stays.
    pub fn clear(&mut self) {
        self.generation += 1;
        self.chunks.clear();
        self.first = self.end;
        self.bytes = self.tags.names.iter().map(|t| t.len() + 32).sum();
    }

    /// An empty store that knows what this one knows: the reader's facts and process names, the
    /// memory it may use, and where ids go on (for a session cleared while the reader runs).
    pub fn emptied(&self) -> LogStore {
        let mut out = self.clone();
        out.clear();
        out.dropped = 0;
        out
    }

    pub fn get(&self, id: u64) -> Option<Line<'_>> {
        if id < self.first || id >= self.end {
            return None;
        }
        // every chunk but the last is full
        let at = (id - self.first) as usize;
        let chunk = self.chunks.get(at / CHUNK)?;
        let e = chunk.entries.get(at % CHUNK)?;
        let message = chunk.text.get(e.start as usize..(e.start + e.len) as usize).unwrap_or_default();
        Some(Line {
            id,
            ts: e.ts,
            wall_ms: e.wall_ms,
            pid: e.pid,
            tid: e.tid,
            uid: (e.uid != NO_UID).then_some(e.uid),
            level: e.level,
            buffer: e.buffer,
            tag_id: e.tag,
            tag: self.tag(e.tag),
            message,
        })
    }

    /// Lines `from..` in order (from the first kept one when `from` is older).
    pub fn iter_from(&self, from: u64) -> impl Iterator<Item = Line<'_>> + '_ {
        (from.max(self.first)..self.end).filter_map(move |id| self.get(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn line(ts: Ts, pid: u32, level: Level, tag: &str, message: &str) -> LogLine {
        LogLine {
            ts,
            wall_ms: 1_790_000_000_000 + (ts / 1_000_000) as i64,
            pid,
            tid: pid,
            uid: Some(10_000 + pid),
            level,
            buffer: 0,
            tag: tag.into(),
            message: message.into(),
        }
    }

    #[test]
    fn lines_keep_their_ids_and_text() {
        let mut s = LogStore::new();
        for i in 0..(CHUNK as u64 * 2 + 10) {
            s.push(line(i, 4312, Level::Debug, if i % 2 == 0 { "OkHttp" } else { "Main" }, &format!("line {i}")));
        }
        assert_eq!((s.first_id(), s.end_id(), s.len()), (0, CHUNK as u64 * 2 + 10, CHUNK * 2 + 10));
        let l = s.get(CHUNK as u64 + 1).unwrap();
        assert_eq!((l.message, l.tag, l.pid, l.uid), (&*format!("line {}", CHUNK + 1), "Main", 4312, Some(14_312)));
        assert_eq!(s.tag_count(), 2, "each tag kept once");
        assert!(s.get(s.end_id()).is_none());
        assert_eq!(s.iter_from(s.end_id() - 3).map(|l| l.id).collect::<Vec<_>>().len(), 3);
    }

    #[test]
    fn the_oldest_chunks_go_past_keep_and_ids_stay() {
        let mut s = LogStore::new();
        s.set_keep(0); // the floor: 256 chunks' worth of records
        let big = "x".repeat(1000);
        for i in 0..(CHUNK as u64 * 4) {
            s.push(line(i, 1, Level::Info, "t", &big));
        }
        assert!(s.bytes() <= s.keep() + CHUNK * (std::mem::size_of::<Entry>() + 1000));
        assert!(s.first_id() > 0 && s.first_id().is_multiple_of(CHUNK as u64), "whole chunks went: {}", s.first_id());
        assert_eq!(s.dropped(), s.first_id());
        assert!(s.get(s.first_id() - 1).is_none());
        assert_eq!(s.get(s.end_id() - 1).unwrap().id, s.end_id() - 1);
    }

    #[test]
    fn a_frozen_copy_shares_lines_and_sees_none_of_the_new_ones() {
        let mut s = LogStore::new();
        s.push(line(1, 1, Level::Info, "a", "first"));
        let frozen = s.clone();
        s.push(line(2, 1, Level::Warn, "b", "second"));
        assert_eq!((frozen.len(), s.len()), (1, 2));
        assert_eq!(frozen.get(0).unwrap().message, "first");
        assert_eq!(s.get(1).unwrap().tag, "b");
        assert_eq!(frozen.tag_count(), 1, "the copy keeps its own tags");
    }

    #[test]
    fn clear_keeps_the_ids_going_and_what_the_reader_said() {
        let mut s = LogStore::new();
        s.apply_info(LogInfo {
            package: Some("com.example.shop".into()),
            uid: Some(10_234),
            processes: vec![(4312, "com.example.shop".into())],
            ..LogInfo::default()
        });
        s.push(line(1, 4312, Level::Info, "a", "one"));
        s.clear();
        assert!(s.is_empty());
        s.push(line(2, 4312, Level::Info, "a", "two"));
        assert_eq!((s.first_id(), s.get(1).unwrap().message), (1, "two"));
        assert_eq!((s.uid(), s.process(4312)), (Some(10_234), Some("com.example.shop")));
    }

    #[test]
    fn another_app_drops_the_uid_of_the_one_before() {
        let mut s = LogStore::new();
        let info =
            |package: &str, uid: Option<u32>| LogInfo { package: Some(package.into()), uid, ..LogInfo::default() };
        s.apply_info(info("com.example.shop", Some(10_234)));
        // the reader says the same app again, without the uid (status lines): it stays
        s.apply_info(LogInfo { status: Some("reading".into()), ..LogInfo::default() });
        s.apply_info(info("com.example.shop", None));
        assert_eq!(s.uid(), Some(10_234));
        // another app whose uid was not found: no uid, not the old one
        s.apply_info(info("com.example.other", None));
        assert_eq!((s.package(), s.uid()), (Some("com.example.other"), None));
        s.apply_info(info("com.example.other", Some(10_301)));
        assert_eq!(s.uid(), Some(10_301));
    }

    #[test]
    fn levels_from_priorities_and_names() {
        assert_eq!(Level::from_priority(2), Level::Verbose);
        assert_eq!(Level::from_priority(7), Level::Assert);
        assert_eq!(Level::parse("W"), Some(Level::Warn));
        assert_eq!(Level::parse("fatal"), Some(Level::Assert));
        assert!(Level::Error > Level::Warn);
    }
}
