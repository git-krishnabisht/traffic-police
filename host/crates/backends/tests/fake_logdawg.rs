//! Logdawg's reader against the fake adb (ARCHITECTURE.md §5.17): the app's uid, the lines from
//! before and after it starts, process names, the clock mapping, a stream cut and resumed
//! without repeats, a pause, another app picked in the session, a device that is not there yet,
//! and the UI going.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use traffic_police_adb::logcat::Record;
use traffic_police_backends::{LogdawgControl, LogdawgTarget, run_logdawg};
use traffic_police_core::SessionEvent;
use traffic_police_core::logdawg::{Level, LogInfo, LogLine};
use traffic_police_fakeadb::FakeAdb;

const SERIAL: &str = "emulator-5554";
const PACKAGE: &str = "com.example.shop";

fn record(sec: u32, pid: u32, uid: u32, priority: u8, tag: &str, message: &str) -> Record {
    Record {
        pid,
        tid: pid + 19,
        sec,
        nsec: 250_000_000,
        buffer: 0,
        uid: Some(uid),
        priority,
        tag: tag.into(),
        message: message.into(),
    }
}

/// What the reader sent, gathered until `done` says it is enough (or 10 s pass).
struct Seen {
    lines: Vec<LogLine>,
    infos: Vec<LogInfo>,
}

async fn until(rx: &mut mpsc::Receiver<Vec<SessionEvent>>, seen: &mut Seen, done: impl Fn(&Seen) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done(seen) {
        let batch = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out: {} lines, infos {:?}", seen.lines.len(), seen.infos))
            .expect("the reader runs");
        for e in batch {
            match e {
                SessionEvent::Logs(l) => seen.lines.extend(l),
                SessionEvent::LogInfo(i) => seen.infos.push(*i),
                other => panic!("the reader sent {other:?}"),
            }
        }
    }
}

fn target(history: Option<u32>) -> LogdawgTarget {
    LogdawgTarget { serial: Some(SERIAL.into()), package: Some(PACKAGE.into()), history, ..LogdawgTarget::default() }
}

#[tokio::test]
async fn lines_before_and_after_the_start_with_the_apps_uid_and_process_names() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 35);
    fake.install(SERIAL, PACKAGE, 10_234);
    // pm lists every package whose name contains the text
    fake.install(SERIAL, "com.example.shop.overlay", 10_500);
    fake.start_process(SERIAL, 4312, PACKAGE);
    for i in 0..3 {
        fake.logcat(SERIAL, record(1_790_000_001 + i, 4312, 10_234, 3, "OkHttp", &format!("history {i}")));
    }
    let (tx, mut rx) = mpsc::channel(64);
    let task = tokio::spawn(run_logdawg(fake.client(), target(Some(2)), LogdawgControl::default(), tx));
    let mut seen = Seen { lines: Vec::new(), infos: Vec::new() };
    until(&mut rx, &mut seen, |s| s.lines.len() >= 2).await;
    // the latest two of the history, then what comes
    assert_eq!(seen.lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(), ["history 1", "history 2"]);
    fake.logcat(SERIAL, record(1_790_000_009, 4312, 10_234, 6, "Checkout", "failed:\nHTTP 500"));
    until(&mut rx, &mut seen, |s| s.lines.len() >= 3 && s.infos.iter().any(|i| !i.processes.is_empty())).await;
    let live = &seen.lines[2];
    assert_eq!((live.level, live.tag.as_str(), live.message.as_str()), (Level::Error, "Checkout", "failed:\nHTTP 500"));
    assert_eq!((live.pid, live.tid, live.uid), (4312, 4331, Some(10_234)));
    // the wall clock onto the boot clock: the fake device's date is 1790000000 s at 8283.09 s
    // of uptime (half a hundredth added to center /proc/uptime's rounding)
    assert_eq!(live.wall_ms, 1_790_000_009_250);
    assert_eq!(live.ts, (8283.09e9 as u64) + 5_000_000 + 9_250_000_000);
    let reading = seen.infos.iter().find(|i| i.uid.is_some()).expect("the app's uid");
    assert_eq!((reading.uid, reading.package.as_deref()), (Some(10_234), Some(PACKAGE)), "exactly the package");
    assert!(reading.status.as_deref().is_some_and(|s| s.starts_with("reading the log of")), "{reading:?}");
    let names: Vec<(u32, String)> = seen.infos.iter().flat_map(|i| i.processes.clone()).collect();
    assert_eq!(names, [(4312, PACKAGE.to_string())]);
    assert!(fake.commands(SERIAL).contains(&"logcat -B -b main,system,crash -T 2".to_string()));
    // the UI goes: the reader stops
    drop(rx);
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the reader stops").unwrap();
}

#[tokio::test]
async fn a_cut_stream_goes_on_from_its_last_line_without_repeats() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 35);
    fake.install(SERIAL, PACKAGE, 10_234);
    fake.logcat(SERIAL, record(1_790_000_001, 4312, 10_234, 4, "A", "one"));
    fake.logcat(SERIAL, record(1_790_000_002, 4312, 10_234, 4, "A", "two"));
    let (tx, mut rx) = mpsc::channel(64);
    let _task = tokio::spawn(run_logdawg(fake.client(), target(Some(100)), LogdawgControl::default(), tx));
    let mut seen = Seen { lines: Vec::new(), infos: Vec::new() };
    until(&mut rx, &mut seen, |s| s.lines.len() >= 2).await;
    fake.break_logcat(SERIAL);
    // a line while the reader is away, and one after it is back
    fake.logcat(SERIAL, record(1_790_000_003, 4312, 10_234, 4, "A", "three"));
    until(&mut rx, &mut seen, |s| s.lines.len() >= 3).await;
    fake.logcat(SERIAL, record(1_790_000_004, 4312, 10_234, 4, "A", "four"));
    until(&mut rx, &mut seen, |s| s.lines.len() >= 4).await;
    assert_eq!(seen.lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(), ["one", "two", "three", "four"]);
    let starts: Vec<String> = fake.commands(SERIAL).into_iter().filter(|c| c.starts_with("logcat ")).collect();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(starts[1].ends_with("-T 1790000002.250"), "from the last line's time: {starts:?}");
}

#[tokio::test]
async fn it_waits_for_the_device_and_says_so() {
    let fake = FakeAdb::start().await;
    let (tx, mut rx) = mpsc::channel(64);
    let _task = tokio::spawn(run_logdawg(fake.client(), target(Some(10)), LogdawgControl::default(), tx));
    let mut seen = Seen { lines: Vec::new(), infos: Vec::new() };
    until(&mut rx, &mut seen, |s| s.infos.iter().any(|i| i.status.as_deref().is_some_and(|t| t.contains("waiting"))))
        .await;
    fake.add_device(SERIAL, 26);
    fake.logcat(SERIAL, record(1_790_000_001, 4312, 10_234, 5, "Late", "here now"));
    until(&mut rx, &mut seen, |s| !s.lines.is_empty()).await;
    assert_eq!(seen.lines[0].message, "here now");
    // not installed: no uid, and the view falls back to the app's processes
    assert!(seen.infos.iter().all(|i| i.uid.is_none()));
}

#[tokio::test]
async fn a_pause_reads_nothing_and_going_on_brings_what_came_meanwhile() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 35);
    fake.install(SERIAL, PACKAGE, 10_234);
    fake.logcat(SERIAL, record(1_790_000_001, 4312, 10_234, 4, "A", "before"));
    let (pause, paused) = watch::channel(false);
    let (tx, mut rx) = mpsc::channel(64);
    let control = LogdawgControl { paused: Some(paused), ..LogdawgControl::default() };
    let _task = tokio::spawn(run_logdawg(fake.client(), target(Some(100)), control, tx));
    let mut seen = Seen { lines: Vec::new(), infos: Vec::new() };
    until(&mut rx, &mut seen, |s| !s.lines.is_empty()).await;
    pause.send(true).unwrap();
    until(&mut rx, &mut seen, |s| s.infos.iter().any(|i| i.status.as_deref().is_some_and(|t| t.starts_with("paused"))))
        .await;
    // logged while paused: nothing is read
    fake.logcat(SERIAL, record(1_790_000_002, 4312, 10_234, 4, "A", "while paused"));
    assert!(tokio::time::timeout(Duration::from_millis(500), rx.recv()).await.is_err(), "nothing read while paused");
    pause.send(false).unwrap();
    until(&mut rx, &mut seen, |s| s.lines.len() >= 2).await;
    fake.logcat(SERIAL, record(1_790_000_003, 4312, 10_234, 4, "A", "after"));
    until(&mut rx, &mut seen, |s| s.lines.len() >= 3).await;
    let messages: Vec<&str> = seen.lines.iter().map(|l| l.message.as_str()).collect();
    assert_eq!(messages, ["before", "while paused", "after"], "what came meanwhile, once");
    let starts: Vec<String> = fake.commands(SERIAL).into_iter().filter(|c| c.starts_with("logcat ")).collect();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(starts[1].ends_with("-T 1790000001.250"), "from the last line's time: {starts:?}");
}

#[tokio::test]
async fn everything_the_device_has_by_default_and_nothing_from_before_with_0() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 35);
    fake.install(SERIAL, PACKAGE, 10_234);
    // the fake device's clock reads 1790000000 s: two lines from before that
    fake.logcat(SERIAL, record(1_789_999_990, 4312, 10_234, 4, "A", "old"));
    fake.logcat(SERIAL, record(1_789_999_995, 4312, 10_234, 4, "A", "older still kept"));
    let (tx, mut rx) = mpsc::channel(64);
    let task = tokio::spawn(run_logdawg(fake.client(), target(None), LogdawgControl::default(), tx));
    let mut seen = Seen { lines: Vec::new(), infos: Vec::new() };
    until(&mut rx, &mut seen, |s| s.lines.len() >= 2).await;
    assert_eq!(seen.lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(), ["old", "older still kept"]);
    assert!(fake.commands(SERIAL).contains(&"logcat -B -b main,system,crash".to_string()), "no -T: all of it");
    drop(rx);
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the reader stops").unwrap();
    // history = 0: from the device's time now on
    let (tx, mut rx) = mpsc::channel(64);
    let _task = tokio::spawn(run_logdawg(fake.client(), target(Some(0)), LogdawgControl::default(), tx));
    let mut seen = Seen { lines: Vec::new(), infos: Vec::new() };
    until(&mut rx, &mut seen, |s| s.infos.iter().any(|i| i.uid.is_some())).await;
    fake.logcat(SERIAL, record(1_790_000_005, 4312, 10_234, 4, "A", "new"));
    until(&mut rx, &mut seen, |s| !s.lines.is_empty()).await;
    assert_eq!(seen.lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(), ["new"]);
    assert!(fake.commands(SERIAL).contains(&"logcat -B -b main,system,crash -T 1790000000.000".to_string()));
}

#[tokio::test]
async fn another_app_goes_on_after_the_last_line_with_its_uid() {
    const OTHER: &str = "com.example.other";
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 35);
    fake.install(SERIAL, PACKAGE, 10_234);
    fake.install(SERIAL, OTHER, 10_301);
    fake.logcat(SERIAL, record(1_790_000_001, 4312, 10_234, 4, "A", "shop"));
    let (switch, to) = watch::channel(target(Some(100)));
    let (tx, mut rx) = mpsc::channel(64);
    let control = LogdawgControl { target: Some(to), ..LogdawgControl::default() };
    let _task = tokio::spawn(run_logdawg(fake.client(), target(Some(100)), control, tx));
    let mut seen = Seen { lines: Vec::new(), infos: Vec::new() };
    until(&mut rx, &mut seen, |s| !s.lines.is_empty()).await;
    switch.send_modify(|t| t.package = Some(OTHER.into()));
    until(&mut rx, &mut seen, |s| s.infos.iter().any(|i| i.uid == Some(10_301))).await;
    let info = seen.infos.iter().find(|i| i.uid == Some(10_301)).unwrap();
    assert_eq!(info.package.as_deref(), Some(OTHER));
    fake.logcat(SERIAL, record(1_790_000_002, 5100, 10_301, 4, "B", "other"));
    until(&mut rx, &mut seen, |s| s.lines.len() >= 2).await;
    assert_eq!(seen.lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(), ["shop", "other"], "none twice");
    let starts: Vec<String> = fake.commands(SERIAL).into_iter().filter(|c| c.starts_with("logcat ")).collect();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(starts[1].ends_with("-T 1790000001.250"), "on the same device, after the last line: {starts:?}");
}
