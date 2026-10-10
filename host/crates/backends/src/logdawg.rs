//! Logdawg's reader: the device's log while the UI runs (ARCHITECTURE.md §5.17).
//!
//! One stream per device, `exec:logcat -B` over every process, read in binary and filtered on
//! the host: the app's other processes, its restarts and its crash are its uid's lines, and the
//! view can show everything without asking the device again. Lines are sent in batches (every
//! 30 ms, or 2000 lines) on the session's event channel; each line's wall clock goes onto the
//! device's boot clock, the network timeline's, through an offset read from `/proc/uptime` and
//! `date` (and again every minute). After a break (the device gone, adb restarted) the stream
//! starts again from the last line's time, without repeating it; a pause (`Space` in view 4)
//! closes the stream, and going on is such a break.

use std::collections::HashSet;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};
use traffic_police_adb::logcat::{self, Parser, Record, Start};
use traffic_police_adb::{Adb, Device};
use traffic_police_core::SessionEvent;
use traffic_police_core::logdawg::{Level, LogInfo, LogLine};

use crate::device::choose_device;

/// What to read.
#[derive(Debug, Clone)]
pub struct LogdawgTarget {
    /// The device (the only one online when not given).
    pub serial: Option<String>,
    /// The session's app, whose uid marks its lines (`package:mine`).
    pub package: Option<String>,
    /// logcat's buffers: main, system, crash, radio, kernel.
    pub buffers: Vec<String>,
    /// Lines from before the session started: everything the device still has (`None`, as
    /// Android Studio reads it), or at most this many (0: none).
    pub history: Option<u32>,
}

impl Default for LogdawgTarget {
    fn default() -> Self {
        LogdawgTarget {
            serial: None,
            package: None,
            buffers: vec!["main".into(), "system".into(), "crash".into()],
            history: None,
        }
    }
}

/// What the reader is told while it runs.
#[derive(Debug, Default)]
pub struct LogdawgControl {
    /// From the UI: while true the log is not read. When it turns false the reader goes on after
    /// the last line it read, so what the device logged meanwhile comes too, while it has it.
    pub paused: Option<watch::Receiver<bool>>,
}

impl LogdawgControl {
    fn paused(&self) -> bool {
        self.paused.as_ref().is_some_and(|p| *p.borrow())
    }

    /// A change the reader acts on; none comes once the UI is gone.
    async fn changed(&mut self) {
        changed(&mut self.paused).await
    }
}

async fn changed<T>(rx: &mut Option<watch::Receiver<T>>) {
    let Some(r) = rx.as_mut() else { return std::future::pending().await };
    if r.changed().await.is_err() {
        *rx = None;
        std::future::pending::<()>().await
    }
}

const FLUSH: Duration = Duration::from_millis(30);
const BATCH: usize = 2000;
const NAMES: Duration = Duration::from_secs(1);
const CLOCK: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(1);

/// Why a stream ended.
enum Ended {
    /// The UI went: stop.
    Closed,
    /// The UI paused the log.
    Paused,
    /// The device or the stream went: try again.
    Lost(String),
}

/// Reads the device's log until the event receiver goes.
pub async fn run_logdawg(
    adb: Adb,
    target: LogdawgTarget,
    mut control: LogdawgControl,
    events: mpsc::Sender<Vec<SessionEvent>>,
) {
    let mut devices = adb.watch_devices();
    // the last line read, to start again after it
    let mut last: Option<i128> = None;
    let mut said = String::new();
    loop {
        if events.is_closed() {
            return;
        }
        if control.paused() {
            if !status(&events, &mut said, "paused (Space goes on)".into()).await {
                return;
            }
            // the UI may go meanwhile: look now and then
            let _ = tokio::time::timeout(RETRY * 2, control.changed()).await;
            continue;
        }
        let snapshot = devices.borrow_and_update().clone();
        let device = match choose_device(&snapshot, target.serial.as_deref()) {
            Ok(d) => d,
            Err(why) => {
                if !status(&events, &mut said, format!("waiting for the device: {why}")).await {
                    return;
                }
                let _ = tokio::time::timeout(RETRY * 2, devices.changed()).await;
                continue;
            }
        };
        match read(&adb, &device, &target, &events, &mut last, &mut said, &mut control).await {
            Ended::Closed => return,
            Ended::Paused => {}
            Ended::Lost(why) => {
                tracing::info!(device = %device.serial, "the log stream ended: {why}");
                if !status(&events, &mut said, format!("the log stopped ({why}); trying again…")).await {
                    return;
                }
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Says what the reader is doing, once per change; false when the UI is gone.
async fn status(events: &mpsc::Sender<Vec<SessionEvent>>, said: &mut String, text: String) -> bool {
    if *said == text {
        return !events.is_closed();
    }
    *said = text.clone();
    let info = LogInfo { status: Some(text), ..LogInfo::default() };
    events.send(vec![SessionEvent::LogInfo(Box::new(info))]).await.is_ok()
}

/// Wall clock minus boot clock on the device, in nanoseconds. `/proc/uptime` has hundredths of a
/// second, so half of one is added to center the error. Android 8's `date` has no `%N` (it prints
/// a literal N): then the shell waits for the next second to begin and reads the uptime right
/// away, which puts the wall clock on that second exactly (Android 8 lines were up to a second
/// off on the timeline before; found on an API 26 emulator).
async fn clock_offset(adb: &Adb, device: &Device) -> Result<DeviceClock, String> {
    let shell = |cmd: &'static str| async move {
        adb.shell(device.transport_id, cmd).await.map(|o| o.stdout_text()).map_err(|e| e.to_string())
    };
    match offset_from(&shell("cat /proc/uptime; date +%s%N").await?)? {
        Some(clock) => Ok(clock),
        None => offset_from(&shell(TICK).await?)?.ok_or_else(|| "no wall clock".to_string()),
    }
}

/// The device's clocks, read at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeviceClock {
    /// Wall clock minus boot clock, in nanoseconds.
    offset: i128,
    /// The wall clock then.
    wall: i128,
}

/// Waits for the device's second to change, then the uptime and that second.
const TICK: &str = "a=$(date +%s); while [ \"$(date +%s)\" = \"$a\" ]; do :; done; cat /proc/uptime; date +%s";

/// The clocks from `/proc/uptime` and a `date` line: `None` when the date has no nanoseconds
/// (`1791639342N`). A line of whole seconds (from [`TICK`]) is taken as the second's start.
fn offset_from(text: &str) -> Result<Option<DeviceClock>, String> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let uptime: f64 =
        lines.next().and_then(|l| l.split_whitespace().next()).and_then(|s| s.parse().ok()).ok_or("no /proc/uptime")?;
    let date = lines.next().unwrap_or_default();
    if date.ends_with('N') {
        return Ok(None);
    }
    let n: i128 = date.parse().map_err(|_| format!("no date: {date:?}"))?;
    let wall_ns = if date.len() >= 18 { n } else { n * 1_000_000_000 };
    let boot_ns = (uptime * 1e9) as i128 + 5_000_000;
    Ok(Some(DeviceClock { offset: wall_ns - boot_ns, wall: wall_ns }))
}

/// logcat's start at a wall-clock time.
fn since(wall_ns: i128) -> Start {
    Start::Since { sec: (wall_ns / 1_000_000_000) as u32, millis: ((wall_ns / 1_000_000) % 1000) as u32 }
}

fn line_of(r: Record, offset: i128) -> LogLine {
    let wall = r.wall_ns();
    LogLine {
        ts: (wall - offset).clamp(0, i128::from(u64::MAX)) as u64,
        wall_ms: (wall / 1_000_000) as i64,
        pid: r.pid,
        tid: r.tid,
        uid: r.uid,
        level: Level::from_priority(r.priority),
        buffer: r.buffer,
        tag: r.tag,
        message: r.message,
    }
}

async fn read(
    adb: &Adb,
    device: &Device,
    target: &LogdawgTarget,
    events: &mpsc::Sender<Vec<SessionEvent>>,
    last: &mut Option<i128>,
    said: &mut String,
    control: &mut LogdawgControl,
) -> Ended {
    let id = device.transport_id;
    let now = match clock_offset(adb, device).await {
        Ok(c) => c,
        Err(e) => return Ended::Lost(format!("the device's clock: {e}")),
    };
    let mut offset = now.offset;
    let uid = match &target.package {
        Some(p) => adb.package_uid(id, p).await.ok().flatten(),
        None => None,
    };
    let start = match (*last, target.history) {
        // after a break or a pause: from the last line read
        (Some(ns), _) => since(ns),
        (None, None) => Start::All,
        // nothing from before: from the device's time now
        (None, Some(0)) => since(now.wall),
        (None, Some(n)) => Start::Latest(n),
    };
    let mut stream = match adb.logcat(id, &logcat::command(&target.buffers, start)).await {
        Ok(s) => s,
        Err(e) => return Ended::Lost(e.to_string()),
    };
    let reading = format!("reading the log of {}", device.label());
    *said = reading.clone();
    let info = LogInfo {
        device: Some(device.label()),
        package: target.package.clone(),
        uid,
        processes: Vec::new(),
        status: Some(reading),
    };
    if events.send(vec![SessionEvent::LogInfo(Box::new(info))]).await.is_err() {
        return Ended::Closed;
    }
    let mut parser = Parser::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut batch: Vec<LogLine> = Vec::new();
    let mut known: HashSet<u32> = HashSet::new();
    let mut unnamed: Vec<u32> = Vec::new();
    let resume_after = *last;
    let mut flush = tokio::time::interval(FLUSH);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut names = tokio::time::interval(NAMES);
    let mut clock = tokio::time::interval(CLOCK);
    clock.tick().await;
    loop {
        tokio::select! {
            n = stream.read(&mut buf) => {
                let n = match n {
                    Ok(0) => return Ended::Lost("the device closed it".into()),
                    Ok(n) => n,
                    Err(e) => return Ended::Lost(e.to_string()),
                };
                parser.push(&buf[..n]);
                loop {
                    let r = match parser.next_record() {
                        Ok(Some(r)) => r,
                        Ok(None) => break,
                        Err(e) => return Ended::Lost(e.to_string()),
                    };
                    let wall = r.wall_ns();
                    // after a break, logcat -T repeats the lines of the millisecond it starts at
                    if resume_after.is_some_and(|after| wall <= after) {
                        continue;
                    }
                    *last = Some(wall);
                    if known.insert(r.pid) {
                        unnamed.push(r.pid);
                    }
                    batch.push(line_of(r, offset));
                }
                if batch.len() >= BATCH && events.send(vec![SessionEvent::Logs(std::mem::take(&mut batch))]).await.is_err() {
                    return Ended::Closed;
                }
            }
            _ = flush.tick() => {
                if !batch.is_empty() && events.send(vec![SessionEvent::Logs(std::mem::take(&mut batch))]).await.is_err() {
                    return Ended::Closed;
                }
                if events.is_closed() {
                    return Ended::Closed;
                }
            }
            _ = names.tick() => {
                if unnamed.is_empty() {
                    continue;
                }
                let pids: Vec<u32> = unnamed.drain(..unnamed.len().min(200)).collect();
                // a process that is gone has no name to find: it is not asked again
                if let Ok(found) = adb.process_names(id, &pids).await
                    && !found.is_empty()
                {
                    let info = LogInfo { processes: found, ..LogInfo::default() };
                    if events.send(vec![SessionEvent::LogInfo(Box::new(info))]).await.is_err() {
                        return Ended::Closed;
                    }
                }
            }
            _ = clock.tick() => {
                // the wall clock can be set (NTP, the user): the next lines use the new offset
                if let Ok(c) = clock_offset(adb, device).await {
                    offset = c.offset;
                }
            }
            _ = control.changed() => {
                if control.paused() {
                    // what was read goes to the UI first; the stream closes when it is dropped
                    if !batch.is_empty() && events.send(vec![SessionEvent::Logs(std::mem::take(&mut batch))]).await.is_err() {
                        return Ended::Closed;
                    }
                    return Ended::Paused;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::offset_from;

    #[test]
    fn clock_offsets_with_nanoseconds_whole_seconds_and_none() {
        let c = offset_from("8283.09 18565.22\n1790000000123456789\n").unwrap().unwrap();
        assert_eq!(c.offset, 1_790_000_000_123_456_789 - 8_283_095_000_000);
        assert_eq!(c.wall, 1_790_000_000_123_456_789);
        // Android 8: no %N, so the tick-over command is needed
        assert_eq!(offset_from("1197.99 4771.40\n1791639342N\n").unwrap(), None);
        // the tick-over command's answer: the uptime at the start of second 1791639343
        let c = offset_from("1198.40 4771.80\n1791639343\n").unwrap().unwrap();
        assert_eq!(c.offset, 1_791_639_343_000_000_000 - 1_198_405_000_000);
        assert!(offset_from("").is_err());
    }
}
