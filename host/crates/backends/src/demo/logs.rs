//! The demo's device log, for Logdawg (ARCHITECTURE.md §5.17): what the pretend shop app and
//! its system would log, made from the demo's own events (so lines and requests line up on the
//! timeline) and a seeded chatter of the system's, the same on every run.

use std::collections::HashMap;

use traffic_police_core::SessionEvent;
use traffic_police_core::fmt::Ts;
use traffic_police_core::logdawg::{BUFFER_CRASH, Level, LogInfo, LogLine};
use traffic_police_core::model::{SourceId, TxnKey};

use super::content::Rng;
use super::{MS, PACKAGE};

/// The app's uid: every process of it writes with this one.
pub const APP_UID: u32 = 10_234;
/// A process of the system's: pid, uid, name.
type Process = (u32, u32, &'static str);

const SYSTEM_SERVER: Process = (612, 1000, "system_server");
const GMS: Process = (1023, 10_101, "com.google.android.gms.persistent");
const SURFACEFLINGER: Process = (388, 1000, "/system/bin/surfaceflinger");
const WIFI: Process = (731, 1010, "/system/bin/wificond");
const BUFFER_MAIN: u8 = 0;
const BUFFER_SYSTEM: u8 = 3;

/// The system's chatter: who writes it, the level, the tag, the message.
const CHATTER: [(Process, Level, &str, &str); 9] = [
    (WIFI, Level::Debug, "wificond", "Scan result ready event"),
    (SYSTEM_SERVER, Level::Info, "chatty", "uid=1000(system) android.bg identical 3 lines"),
    (GMS, Level::Info, "NetworkScheduler.Stats", "Task com.google.android.gms/.gcm finished executing. result: 1"),
    (SURFACEFLINGER, Level::Debug, "SurfaceFlinger", "Finished setting power mode 2 on display 0"),
    (SYSTEM_SERVER, Level::Warn, "ActivityManager", "Slow operation: 112ms so far, now at startProcess"),
    (GMS, Level::Debug, "BoundBrokerSvc", "onUnbind: Intent { act=com.google.android.gms.scheduler.ACTION_PROXY }"),
    (SYSTEM_SERVER, Level::Debug, "ConnectivityService", "NetworkAgentInfo [WIFI () - 100] validation passed"),
    (WIFI, Level::Info, "wpa_supplicant", "wlan0: CTRL-EVENT-SIGNAL-CHANGE above=1 signal=-52 noise=9999"),
    (SYSTEM_SERVER, Level::Verbose, "WindowManager", "Relayout Window{4f2a com.example.shop/.MainActivity}"),
];

pub(crate) struct DemoLog {
    rng: Rng,
    started: bool,
    /// The app's pid for each source, and each open request's URL and start.
    pids: HashMap<SourceId, u32>,
    requests: HashMap<TxnKey, (String, Ts)>,
    next_chatter: Ts,
    next_frames: Ts,
}

impl DemoLog {
    pub fn new() -> DemoLog {
        DemoLog {
            rng: Rng::new(0x0010_9da3),
            started: false,
            pids: HashMap::new(),
            requests: HashMap::new(),
            next_chatter: 0,
            next_frames: 0,
        }
    }

    /// The lines for this step's events and the chatter up to `now`, in time order.
    pub fn step(&mut self, events: &[SessionEvent], now: Ts, wall: impl Fn(Ts) -> i64) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        let mut lines: Vec<LogLine> = Vec::new();
        let line = |ts: Ts, (pid, tid, uid): (u32, u32, u32), level: Level, buffer: u8, tag: &str, msg: String| {
            LogLine { ts, wall_ms: wall(ts), pid, tid, uid: Some(uid), level, buffer, tag: tag.into(), message: msg }
        };
        if !self.started {
            self.started = true;
            self.next_chatter = now;
            self.next_frames = now + 3_000 * MS;
            let processes = [SYSTEM_SERVER, GMS, SURFACEFLINGER, WIFI].map(|(pid, _, name)| (pid, name.to_string()));
            out.push(SessionEvent::LogInfo(Box::new(LogInfo {
                device: Some("Pixel 8 [demo]".into()),
                package: Some(PACKAGE.into()),
                uid: Some(APP_UID),
                processes: processes.to_vec(),
                status: Some("reading the log of Pixel 8 [demo]".into()),
            })));
        }
        let sys = |(pid, uid, _): Process| (pid, pid + 31, uid);
        for e in events {
            match e {
                SessionEvent::SourceUp(info) => {
                    self.pids.insert(info.id, info.pid);
                    out.push(SessionEvent::LogInfo(Box::new(LogInfo {
                        processes: vec![(info.pid, info.process.clone())],
                        ..LogInfo::default()
                    })));
                    let at = info.started;
                    lines.push(line(
                        at,
                        sys(SYSTEM_SERVER),
                        Level::Info,
                        BUFFER_SYSTEM,
                        "ActivityManager",
                        format!(
                            "Start proc {}:{}/u0a234 for top-activity {{{PACKAGE}/.MainActivity}}",
                            info.pid, info.process
                        ),
                    ));
                    lines.push(line(
                        at,
                        (info.pid, info.pid, APP_UID),
                        Level::Info,
                        BUFFER_MAIN,
                        "TrafficPolice",
                        format!("capturing in {}; socket @traffic-police_{PACKAGE}_{}", info.process, info.pid),
                    ));
                }
                SessionEvent::SourceDown { source, at, reason } => {
                    let pid = self.pids.get(source).copied().unwrap_or(4312);
                    if reason.contains("crash") {
                        lines.push(line(
                            *at,
                            (pid, pid, APP_UID),
                            Level::Error,
                            BUFFER_CRASH,
                            "AndroidRuntime",
                            format!(
                                "FATAL EXCEPTION: main\nProcess: {PACKAGE}, PID: {pid}\njava.lang.IllegalStateException: the cart \
                                 has no prices\n\tat com.example.shop.cart.CartRepository.total(CartRepository.kt:58)\n\tat \
                                 com.example.shop.cart.CartViewModel.refresh(CartViewModel.kt:31)\n\tat \
                                 com.example.shop.MainActivity.onResume(MainActivity.kt:88)"
                            ),
                        ));
                    }
                    lines.push(line(
                        *at,
                        sys(SYSTEM_SERVER),
                        Level::Info,
                        BUFFER_SYSTEM,
                        "ActivityManager",
                        format!("Process {PACKAGE} (pid {pid}) has died: fg  TOP"),
                    ));
                }
                SessionEvent::Request(r) => {
                    let pid = self.pids.get(&r.key.source).copied().unwrap_or(4312);
                    let tid = r.thread.as_ref().and_then(|t| t.tid).unwrap_or(pid);
                    self.requests.insert(r.key, (r.url.clone(), r.at));
                    lines.push(line(
                        r.at,
                        (pid, tid, APP_UID),
                        Level::Debug,
                        BUFFER_MAIN,
                        "OkHttp",
                        format!("--> {} {}", r.method, r.url),
                    ));
                }
                SessionEvent::Response(r) => {
                    let pid = self.pids.get(&r.key.source).copied().unwrap_or(4312);
                    let (url, start) = self.requests.get(&r.key).cloned().unwrap_or_default();
                    let ms = r.at.saturating_sub(start) / MS;
                    lines.push(line(
                        r.at,
                        (pid, pid + 57, APP_UID),
                        Level::Debug,
                        BUFFER_MAIN,
                        "OkHttp",
                        format!("<-- {} {} {url} ({ms}ms)", r.status, r.message),
                    ));
                    if r.status >= 500 {
                        lines.push(line(
                            r.at,
                            (pid, pid, APP_UID),
                            Level::Warn,
                            BUFFER_MAIN,
                            "CheckoutViewModel",
                            format!("request failed: HTTP {} {} (will retry)", r.status, r.message),
                        ));
                    }
                }
                SessionEvent::Failed(f) => {
                    let pid = self.pids.get(&f.key.source).copied().unwrap_or(4312);
                    let what = f.error.message.clone().unwrap_or_default();
                    lines.push(line(
                        f.at,
                        (pid, pid + 57, APP_UID),
                        Level::Error,
                        BUFFER_MAIN,
                        "OkHttp",
                        format!("<-- HTTP FAILED: {}: {what}", f.error.class),
                    ));
                }
                _ => {}
            }
        }
        // the system's chatter, and the app's main thread falling behind now and then
        while self.next_chatter <= now {
            let at = self.next_chatter;
            let n = self.rng.range(0, CHATTER.len() as u64 - 1) as usize;
            let (who, level, tag, msg) = CHATTER[n];
            let buffer = if who.0 == SYSTEM_SERVER.0 { BUFFER_SYSTEM } else { BUFFER_MAIN };
            lines.push(line(at, sys(who), level, buffer, tag, msg.to_string()));
            self.next_chatter = at + self.rng.range(300, 1_600) * MS;
        }
        while self.next_frames <= now {
            let at = self.next_frames;
            if let Some(&pid) = self.pids.values().max() {
                let frames = self.rng.range(31, 120);
                lines.push(line(
                    at,
                    (pid, pid, APP_UID),
                    Level::Info,
                    BUFFER_MAIN,
                    "Choreographer",
                    format!("Skipped {frames} frames!  The application may be doing too much work on its main thread."),
                ));
            }
            self.next_frames = at + self.rng.range(4_000, 9_000) * MS;
        }
        lines.sort_by_key(|l| l.ts);
        if !lines.is_empty() {
            out.push(SessionEvent::Logs(lines));
        }
        out
    }
}
