//! The Flutter backend (ARCHITECTURE.md §5.16): the dart:io HTTP traffic of a Flutter app's
//! debug or profile build, read from its Dart VM service, the way DevTools' Network page reads
//! it. No library and no agent: the app logs its VM service address at start; traffic-police
//! forwards that port, turns HTTP logging on in every isolate, polls the profile and fetches the
//! bodies of the requests that ended. What it reads becomes the device protocol's messages
//! ([`profile`]), so the store, the session log and `tail` take them as they take the capture
//! runtime's.

mod profile;
mod vm;
pub mod ws;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use traffic_police_adb::{Adb, Device};
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::{BackendCommand, ConnectionStatus};
use traffic_police_core::model::{SourceId, SourceInfo};
use traffic_police_core::normalize::Normalizer;
use traffic_police_core::session::{DeviceRecord, StreamSink};
use traffic_police_core::store::SourceIds;
use traffic_police_proto::frame::Frame;
use traffic_police_proto::msg::{self, DeviceMsg};

use crate::device::{DeviceTarget, choose_device, is_frozen, set_status};
use profile::Translator;
use vm::{Open, RpcError, Vm};

pub use vm::ws_target;

/// How often the profile is read (DevTools reads it every 2 s).
const POLL: Duration = Duration::from_secs(1);
const CALL: Duration = Duration::from_secs(5);
/// Fetching a body: large ones arrive as long JSON arrays.
const FETCH: Duration = Duration::from_secs(30);
/// The dart:io profiling protocol whose times are wall-clock µs (Dart 3.4, Flutter 3.22).
const MIN_DART_IO: u64 = 4;

/// The last VM service address a process logged, from its logcat lines (`None` once it said it
/// stopped). Matches Dart ≥ 2.17's text and the older `Observatory listening on`.
pub fn vm_service_uri(log: &str) -> Option<String> {
    let mut found = None;
    for line in log.lines() {
        if line.contains("no longer listening") {
            found = None;
            continue;
        }
        let Some(i) = line.find(" listening on ") else { continue };
        if !(line[..i].ends_with("The Dart VM service is") || line[..i].ends_with("Observatory")) {
            continue;
        }
        let rest = &line[i + " listening on ".len()..];
        let uri: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || ":/=_-.[]".contains(*c)).collect();
        if uri.starts_with("http") || uri.starts_with("//") {
            found = Some(uri);
        }
    }
    found
}

/// The device's facts for `hello`, and its clocks sampled together.
struct DeviceFacts {
    api: u32,
    release: Option<String>,
    manufacturer: Option<String>,
    model: Option<String>,
    abi: Option<String>,
    /// CLOCK_BOOTTIME ns (the protocol's `ts`) and wall-clock ns, read back to back.
    boot_ns: u64,
    wall_ns: i128,
}

async fn device_facts(adb: &Adb, device: &Device) -> Result<DeviceFacts, String> {
    let cmd = "getprop ro.build.version.sdk; getprop ro.build.version.release; getprop ro.product.manufacturer; \
               getprop ro.product.model; getprop ro.product.cpu.abi; cat /proc/uptime; date +%s%N";
    let out = adb.shell(device.transport_id, cmd).await.map_err(|e| e.to_string())?;
    let text = out.stdout_text();
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let get = |i: usize| lines.get(i).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let uptime: f64 =
        lines.get(5).and_then(|l| l.split_whitespace().next()).and_then(|s| s.parse().ok()).ok_or("no /proc/uptime")?;
    let date = lines.get(6).copied().unwrap_or_default();
    // toybox's date has %N; without it the line ends in a literal N: seconds then
    let wall_ns: i128 = match date.parse::<i128>() {
        Ok(n) if date.len() >= 18 => n,
        _ => date.trim_end_matches('N').parse::<i128>().map_err(|_| format!("no date: {date:?}"))? * 1_000_000_000,
    };
    Ok(DeviceFacts {
        api: get(0).and_then(|s| s.parse().ok()).unwrap_or(0),
        release: get(1),
        manufacturer: get(2),
        model: get(3),
        abi: get(4),
        boot_ns: (uptime * 1e9) as u64,
        wall_ns,
    })
}

/// The pid of the target's process, if it runs.
async fn find_pid(adb: &Adb, device: &Device, target: &DeviceTarget, not: Option<u32>) -> Option<u32> {
    if let Some(pid) = target.pid {
        let out = adb.shell(device.transport_id, &format!("test -d /proc/{pid} && echo up")).await.ok()?;
        return out.stdout_text().contains("up").then_some(pid);
    }
    let name = target.process.as_deref().unwrap_or(&target.package);
    let out = adb.shell(device.transport_id, &format!("pidof {}", traffic_police_adb::quote(name))).await.ok()?;
    out.stdout_text().split_whitespace().filter_map(|p| p.parse::<u32>().ok()).filter(|p| Some(*p) != not).max()
}

async fn find_uri(adb: &Adb, device: &Device, pid: u32) -> Option<String> {
    let out = adb.shell(device.transport_id, &format!("logcat -d -v brief --pid={pid}")).await.ok()?;
    vm_service_uri(&out.stdout_text())
}

/// Why a connection to the VM service ended.
enum End {
    Shutdown,
    /// The connection closed or failed; the process may still run.
    Lost(String),
    /// DDS took the VM service over: continue there.
    Redirect(String),
    /// The app cannot be captured this way.
    Fatal(String),
}

/// What carries over a reconnection to the same process: its source and what was sent.
struct Session {
    source: SourceId,
    pid: u32,
    translator: Translator,
    normalizer: Normalizer,
    /// Per isolate, the profile's `timestamp` of the last read (the next `updatedSince`).
    cursors: HashMap<String, i64>,
    /// Device boottime minus wall clock, ns.
    offset_ns: i128,
    closed: bool,
    /// Isolates whose HTTP logging was off when traffic-police came: off again when it leaves
    /// (dart:io keeps every request and body it records in the app's memory).
    found_off: Vec<String>,
}

struct Out {
    events: mpsc::Sender<Vec<SessionEvent>>,
    log: Option<Arc<dyn StreamSink>>,
}

impl Out {
    async fn frames(&self, session: &mut Session, frames: Vec<Frame>) -> bool {
        if frames.is_empty() {
            return true;
        }
        let mut batch = Vec::new();
        for f in frames {
            if let Some(log) = &self.log {
                log.frame(session.source, &f);
            }
            if let Err(e) = session.normalizer.frame(f, &mut batch) {
                tracing::warn!("a translated message did not decode: {e}");
            }
        }
        self.events.send(batch).await.is_ok()
    }
}

fn wall_now_ns() -> i128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i128)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_flutter(
    adb: Adb,
    target: DeviceTarget,
    ids: SourceIds,
    events: mpsc::Sender<Vec<SessionEvent>>,
    mut commands: mpsc::UnboundedReceiver<BackendCommand>,
    status: watch::Sender<ConnectionStatus>,
    log: Option<Arc<dyn StreamSink>>,
) {
    let out = Out { events, log };
    let mut devices = adb.watch_devices();
    let mut recording = target.capture.recording;
    let mut launch = target.launch.clone();
    let mut session: Option<Session> = None;
    let mut left: Option<u32> = None;
    let name = target.process.clone().unwrap_or_else(|| target.package.clone());
    loop {
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                BackendCommand::Shutdown => return,
                BackendCommand::SetRecording(on) => recording = on,
                _ => {}
            }
        }
        if out.events.is_closed() {
            return;
        }
        let wait = |status: &watch::Sender<ConnectionStatus>, what: String| {
            set_status(status, ConnectionStatus::Waiting(what))
        };
        let snapshot = devices.borrow_and_update().clone();
        let device = match choose_device(&snapshot, target.serial.as_deref()) {
            Ok(d) => d,
            Err(m) => {
                wait(&status, m);
                if pause(&mut commands, &mut recording, POLL).await {
                    return;
                }
                continue;
            }
        };
        if let Some(l) = launch.take() {
            wait(&status, format!("starting {}…", target.package));
            match crate::attach::start_app(&adb, &device, &target.package, false, None, &l.extras).await {
                Ok(()) => {}
                Err(crate::attach::Problem::Fatal(m)) => {
                    set_status(&status, ConnectionStatus::Failed(m));
                    return;
                }
                Err(crate::attach::Problem::Passing(m)) => {
                    launch = Some(l);
                    wait(&status, m);
                    if pause(&mut commands, &mut recording, POLL).await {
                        return;
                    }
                    continue;
                }
            }
        }
        let pid = match find_pid(&adb, &device, &target, left).await {
            Some(p) => p,
            None => {
                if let Some(mut s) = session.take() {
                    close(&out, &mut s, "the app exited").await;
                    if !target.follow {
                        set_status(&status, ConnectionStatus::Detached("the app exited · data kept".into()));
                        return;
                    }
                    left = Some(s.pid);
                }
                let what = if left.is_some() {
                    format!("{name} exited; waiting for it to start again (--follow)")
                } else {
                    format!(
                        "waiting for {name} on {} (start the app: a debug or profile build of a Flutter app)",
                        device.label()
                    )
                };
                wait(&status, what);
                if pause(&mut commands, &mut recording, POLL).await {
                    return;
                }
                continue;
            }
        };
        if session.as_ref().is_some_and(|s| s.pid != pid) {
            let mut s = session.take().expect("some");
            close(&out, &mut s, "the app exited").await;
        }
        let Some(uri) = find_uri(&adb, &device, pid).await else {
            wait(
                &status,
                format!(
                    "{name} (pid {pid}) has logged no Dart VM service address: it needs a debug or profile build of a Flutter app; if it started long ago, its log line is gone (restart it, or use --launch)"
                ),
            );
            if pause(&mut commands, &mut recording, POLL).await {
                return;
            }
            continue;
        };
        if is_frozen(&adb, &device, pid).await {
            wait(&status, crate::device::frozen_text(&name, pid));
            if pause(&mut commands, &mut recording, POLL).await {
                return;
            }
            continue;
        }
        let Some((_, device_port, path)) = ws_target(&uri) else {
            set_status(&status, ConnectionStatus::Failed(format!("cannot read the VM service address {uri}")));
            return;
        };
        let local = match adb.forward(device.transport_id, &format!("tcp:{device_port}")).await {
            Ok(p) => p,
            Err(e) => {
                wait(&status, format!("adb forward failed: {e}"));
                if pause(&mut commands, &mut recording, POLL).await {
                    return;
                }
                continue;
            }
        };
        let end = connect(
            &adb,
            &device,
            &target,
            &ids,
            &out,
            &mut commands,
            &status,
            &mut session,
            &mut recording,
            pid,
            local,
            &path,
        )
        .await;
        if let Err(e) = adb.kill_forward(device.transport_id, local).await {
            tracing::debug!("removing forward tcp:{local}: {e}");
        }
        match end {
            End::Shutdown => {
                if let Some(s) = session.as_mut() {
                    close(&out, s, "traffic-police quit").await;
                }
                return;
            }
            End::Fatal(m) => {
                if let Some(s) = session.as_mut() {
                    close(&out, s, "traffic-police stopped").await;
                }
                set_status(&status, ConnectionStatus::Failed(m));
                return;
            }
            End::Redirect(why) | End::Lost(why) => {
                // the same process again (a hiccup, or DDS took over), or its end: the next round
                // finds out
                tracing::info!("the VM service connection ended: {why}");
                wait(&status, format!("{why}; reconnecting"));
                if pause(&mut commands, &mut recording, Duration::from_millis(300)).await {
                    if let Some(s) = session.as_mut() {
                        close(&out, s, "traffic-police quit").await;
                    }
                    return;
                }
            }
        }
    }
}

/// Waits `d` for a command; true when it was to shut down.
async fn pause(commands: &mut mpsc::UnboundedReceiver<BackendCommand>, recording: &mut bool, d: Duration) -> bool {
    let until = Instant::now() + d;
    loop {
        match tokio::time::timeout_at(until, commands.recv()).await {
            Err(_) => return false,
            Ok(Some(BackendCommand::Shutdown)) | Ok(None) => return true,
            Ok(Some(BackendCommand::SetRecording(on))) => *recording = on,
            Ok(Some(_)) => {}
        }
    }
}

async fn close(out: &Out, s: &mut Session, reason: &str) {
    if s.closed {
        return;
    }
    s.closed = true;
    let at = (wall_now_ns() + s.offset_ns).max(0) as u64;
    if let Some(log) = &out.log {
        log.source_end(s.source, at, reason);
    }
    let _ = out.events.send(vec![SessionEvent::SourceDown { source: s.source, at, reason: reason.to_string() }]).await;
}

/// Whether an isolate's HTTP logging is on (asked without changing it).
async fn logging_on(vm: &mut Vm, isolate: &str) -> Option<bool> {
    let state =
        vm.call("ext.dart.io.httpEnableTimelineLogging", json!({ "isolateId": isolate }), Duration::from_secs(2));
    state.await.ok().and_then(|v| v["enabled"].as_bool())
}

/// Leaves the app as it was found: HTTP logging off again where it was off.
async fn restore(vm: &mut Vm, s: &Session) {
    for iso in &s.found_off {
        let off = json!({ "isolateId": iso, "enabled": false });
        let _ = vm.call("ext.dart.io.httpEnableTimelineLogging", off, Duration::from_secs(1)).await;
    }
}

/// One isolate's profiling: on, with dart:io's profiling protocol new enough. `Ok(None)` for an
/// isolate without it, `Ok(Some(version))` otherwise; `Err` when the VM service went away.
async fn enable(vm: &mut Vm, isolate: &str, on: bool) -> Result<Option<u64>, RpcError> {
    let version = match vm.call("ext.dart.io.getVersion", json!({ "isolateId": isolate }), Duration::from_secs(2)).await
    {
        Ok(v) => v["major"].as_u64().unwrap_or(0),
        // no dart:io in it (or it is paused and the call waits): try again later
        Err(RpcError::Rpc(..)) | Err(RpcError::Timeout) => return Ok(None),
        Err(e) => return Err(e),
    };
    if version < MIN_DART_IO {
        return Ok(Some(version));
    }
    match vm
        .call(
            "ext.dart.io.httpEnableTimelineLogging",
            json!({ "isolateId": isolate, "enabled": on }),
            Duration::from_secs(2),
        )
        .await
    {
        Ok(_) | Err(RpcError::Rpc(..)) | Err(RpcError::Timeout) => Ok(Some(version)),
        Err(e) => Err(e),
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect(
    adb: &Adb,
    device: &Device,
    target: &DeviceTarget,
    ids: &SourceIds,
    out: &Out,
    commands: &mut mpsc::UnboundedReceiver<BackendCommand>,
    status: &watch::Sender<ConnectionStatus>,
    session: &mut Option<Session>,
    recording: &mut bool,
    pid: u32,
    local: u16,
    path: &str,
) -> End {
    // the VM service, or DDS on this computer when a Flutter tool runs the app
    let mut vm = match Vm::open("127.0.0.1", local, path).await {
        Ok(Open::Vm(vm)) => vm,
        Ok(Open::Redirect(to)) => match ws_target(&to) {
            Some((host, port, path)) => match Vm::open(&host, port, &path).await {
                Ok(Open::Vm(vm)) => vm,
                Ok(Open::Redirect(again)) => return End::Lost(format!("redirected twice ({again})")),
                Err(e) => return End::Lost(format!("cannot reach DDS at {to}: {e}")),
            },
            None => return End::Lost(format!("cannot read the DDS address {to}")),
        },
        Err(e) => return End::Lost(format!("cannot reach the Dart VM service: {e}")),
    };
    let end = stream(adb, device, target, ids, out, commands, status, session, recording, pid, &mut vm).await;
    vm.close().await;
    end
}

#[allow(clippy::too_many_arguments)]
async fn stream(
    adb: &Adb,
    device: &Device,
    target: &DeviceTarget,
    ids: &SourceIds,
    out: &Out,
    commands: &mut mpsc::UnboundedReceiver<BackendCommand>,
    status: &watch::Sender<ConnectionStatus>,
    session: &mut Option<Session>,
    recording: &mut bool,
    pid: u32,
    vm: &mut Vm,
) -> End {
    let lost = |e: RpcError| End::Lost(e.to_string());
    let info = match vm.call("getVM", json!({}), CALL).await {
        Ok(v) => v,
        Err(e) => return lost(e),
    };
    let dart = info["version"].as_str().and_then(|v| v.split_whitespace().next()).map(str::to_string);
    for stream in ["Isolate", "Extension"] {
        let _ = vm.call("streamListen", json!({ "streamId": stream }), CALL).await;
    }
    // every isolate with dart:io: profiling on
    let mut isolates: Vec<(String, String, i64)> = Vec::new();
    let mut too_old = None;
    let mut found_off = Vec::new();
    for iso in info["isolates"].as_array().cloned().unwrap_or_default() {
        let (Some(id), name) = (iso["id"].as_str(), iso["name"].as_str().unwrap_or("isolate")) else { continue };
        let number = iso["number"].as_str().and_then(|n| n.parse().ok()).unwrap_or(0);
        if logging_on(vm, id).await == Some(false) {
            found_off.push(id.to_string());
        }
        match enable(vm, id, *recording).await {
            Ok(Some(v)) if v >= MIN_DART_IO => isolates.push((id.to_string(), name.to_string(), number)),
            Ok(Some(v)) => too_old = Some(v),
            Ok(None) => {}
            Err(e) => return lost(e),
        }
    }
    if isolates.is_empty()
        && let Some(v) = too_old
    {
        return End::Fatal(format!(
            "this app's dart:io profiling is version {v}; traffic-police reads version {MIN_DART_IO} and newer (Flutter 3.22 / Dart 3.4)"
        ));
    }

    // the source: once per process, kept across reconnections
    if session.as_ref().is_none_or(|s| s.pid != pid || s.closed) {
        let facts = match device_facts(adb, device).await {
            Ok(f) => f,
            Err(e) => return End::Lost(format!("cannot read the device's clock: {e}")),
        };
        let offset_ns = i128::from(facts.boot_ns) - facts.wall_ns;
        let source = ids.next();
        let process = target.process.clone().unwrap_or_else(|| target.package.clone());
        let hello = msg::Hello {
            protocol: traffic_police_proto::PROTOCOL_VERSION,
            runtime: msg::RuntimeInfo {
                version: dart.clone().unwrap_or_else(|| "?".into()),
                build: None,
                mode: "flutter".into(),
            },
            instance: format!("flutter-{pid}-{}", facts.boot_ns),
            app: msg::AppInfo { package: target.package.clone(), process, pid, uid: None, debuggable: Some(true) },
            device: msg::DeviceInfo {
                api: facts.api,
                release: facts.release,
                manufacturer: facts.manufacturer,
                model: facts.model,
                abi: facts.abi,
                abis: Vec::new(),
            },
            clock: msg::Clock { ts: facts.boot_ns, wall_ms: (facts.wall_ns / 1_000_000) as i64 },
            started_ts: None,
            capabilities: Vec::new(),
            clients: [("dart:io".to_string(), dart.clone())].into_iter().collect(),
            hooks: Vec::new(),
            buffer: None,
            config: Some(target.capture.clone()),
        };
        let raw = serde_json::to_vec(&DeviceMsg::Hello(hello.clone())).expect("hello serializes");
        let label = device.label();
        if let Some(log) = &out.log {
            log.source(
                source,
                &DeviceRecord { label: label.clone(), serial: Some(device.serial.clone()) },
                &raw,
                false,
            );
        }
        let info = SourceInfo::from_hello(source, &hello, label, Some(device.serial.clone()));
        if out.events.send(vec![SessionEvent::SourceUp(Box::new(info))]).await.is_err() {
            return End::Shutdown;
        }
        *session = Some(Session {
            source,
            pid,
            translator: Translator::new(offset_ns, target.capture.clone(), dart.clone()),
            normalizer: Normalizer::new(source),
            cursors: HashMap::new(),
            offset_ns,
            closed: false,
            found_off: Vec::new(),
        });
    }
    let s = session.as_mut().expect("set");
    // a reconnection finds logging on (it was turned on): what was off stays known
    for iso in found_off {
        if !s.found_off.contains(&iso) {
            s.found_off.push(iso);
        }
    }
    for (id, name, number) in &isolates {
        s.translator.isolate(id, name, *number);
    }
    let mut polled: Vec<String> = isolates.into_iter().map(|(id, ..)| id).collect();
    set_status(status, ConnectionStatus::Live(format!("{} · {} · Dart VM service", device.label(), target.package)));

    let mut next_poll = Instant::now();
    loop {
        // a poll of every isolate's profile, and the bodies of what ended
        if Instant::now() >= next_poll {
            next_poll = Instant::now() + POLL;
            let mut frames = Vec::new();
            for iso in polled.clone() {
                let since = s.cursors.get(&iso).copied();
                let mut params = json!({ "isolateId": iso });
                if let Some(t) = since {
                    params["updatedSince"] = json!(t);
                }
                let profile = match vm.call("ext.dart.io.getHttpProfile", params, CALL).await {
                    Ok(p) => p,
                    // the isolate went away, or is paused: its turn comes again
                    Err(RpcError::Rpc(..)) | Err(RpcError::Timeout) => continue,
                    Err(e) => return lost(e),
                };
                if profile["type"] == "Sentinel" {
                    polled.retain(|i| i != &iso);
                    continue;
                }
                if let Some(t) = profile["timestamp"].as_i64() {
                    s.cursors.insert(iso.clone(), t);
                }
                for entry in profile["requests"].as_array().map(Vec::as_slice).unwrap_or_default() {
                    if !s.translator.update(entry, &mut frames) {
                        continue;
                    }
                    let id = entry["id"].as_str().unwrap_or_default();
                    match vm
                        .call("ext.dart.io.getHttpProfileRequest", json!({ "isolateId": iso, "id": id }), FETCH)
                        .await
                    {
                        Ok(full) => s.translator.finish(&full, &mut frames),
                        // ended without its bodies (the profile was cleared, or the fetch failed)
                        Err(RpcError::Rpc(..)) | Err(RpcError::Timeout) => s.translator.finish(entry, &mut frames),
                        Err(e) => return lost(e),
                    }
                }
            }
            if !out.frames(s, frames).await {
                restore(vm, s).await;
                return End::Shutdown;
            }
        }
        // events and commands until the next poll
        let wait = next_poll.saturating_duration_since(Instant::now());
        let mut frames = Vec::new();
        tokio::select! {
            ev = vm.next_event(wait) => match ev {
                Ok(Some(ev)) => {
                    match handle_event(vm, &ev, &mut polled, s, *recording, &mut frames).await {
                        Ok(Some(end)) => return end,
                        Ok(None) => {}
                        Err(e) => return lost(e),
                    }
                }
                Ok(None) => {}
                Err(e) => return lost(e),
            },
            cmd = commands.recv() => match cmd {
                Some(BackendCommand::Shutdown) | None => {
                    restore(vm, s).await;
                    return End::Shutdown;
                }
                Some(BackendCommand::SetRecording(on)) => {
                    *recording = on;
                    for iso in polled.clone() {
                        if let Err(e) = enable(vm, &iso, on).await {
                            return lost(e);
                        }
                    }
                }
                Some(_) => {}
            },
        }
        for ev in vm.take_events() {
            match handle_event(vm, &ev, &mut polled, s, *recording, &mut frames).await {
                Ok(Some(end)) => return end,
                Ok(None) => {}
                Err(e) => return lost(e),
            }
        }
        if !out.frames(s, frames).await {
            restore(vm, s).await;
            return End::Shutdown;
        }
    }
}

/// One `streamNotify`: a new isolate gets profiling, one that exits is dropped, DDS taking over
/// ends this connection (to continue through DDS), and logging turned off elsewhere is said.
async fn handle_event(
    vm: &mut Vm,
    ev: &Value,
    polled: &mut Vec<String>,
    s: &mut Session,
    recording: bool,
    frames: &mut Vec<Frame>,
) -> Result<Option<End>, RpcError> {
    let e = &ev["params"]["event"];
    let iso = e["isolate"]["id"].as_str().unwrap_or_default().to_string();
    match e["kind"].as_str().unwrap_or_default() {
        "IsolateRunnable" | "ServiceExtensionAdded" if !iso.is_empty() && !polled.contains(&iso) => {
            if e["kind"] == "ServiceExtensionAdded" && e["extensionRPC"] != "ext.dart.io.httpEnableTimelineLogging" {
                return Ok(None);
            }
            if logging_on(vm, &iso).await == Some(false) && !s.found_off.contains(&iso) {
                s.found_off.push(iso.clone());
            }
            if let Some(v) = enable(vm, &iso, recording).await?
                && v >= MIN_DART_IO
            {
                let name = e["isolate"]["name"].as_str().unwrap_or("isolate");
                let number = e["isolate"]["number"].as_str().and_then(|n| n.parse().ok()).unwrap_or(0);
                s.translator.isolate(&iso, name, number);
                polled.push(iso);
            }
        }
        "IsolateExit" => polled.retain(|i| i != &iso),
        "DartDevelopmentServiceConnected" => {
            let uri = e["uri"].as_str().unwrap_or_default().to_string();
            return Ok(Some(End::Redirect(format!("a Flutter tool's DDS took over the VM service ({uri})"))));
        }
        "Extension"
            if e["extensionKind"] == "HttpTimelineLoggingStateChange"
                && recording
                && e["extensionData"]["enabled"] == Value::Bool(false) =>
        {
            let at = (wall_now_ns() + s.offset_ns).max(0) as u64;
            frames.push(s.translator.diag(
                at,
                "warn",
                "flutter_logging_off",
                "another tool (or the app) turned dart:io's HTTP logging off: new requests are not recorded until it is on again",
            ));
        }
        _ => {}
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_address_a_process_logged() {
        let log = "\
10-07 16:40:01.000 I/flutter ( 4321): Observatory listening on http://127.0.0.1:40001/AAAAAAAAAAA=/
10-07 16:45:12.345 I/flutter ( 4321): The Dart VM service is listening on http://127.0.0.1:43217/Wq9tyH3o9fo=/
10-07 16:45:13.000 I/flutter ( 4321): some app output mentioning listening on http://example.com/
";
        assert_eq!(vm_service_uri(log).as_deref(), Some("http://127.0.0.1:43217/Wq9tyH3o9fo=/"));
        let stopped = format!(
            "{log}10-07 16:50:00.000 I/flutter ( 4321): Dart VM service no longer listening on http://127.0.0.1:43217/Wq9tyH3o9fo=/\n"
        );
        assert_eq!(vm_service_uri(&stopped), None);
        assert_eq!(
            vm_service_uri("I/flutter ( 1): The Dart VM service is listening on http://[::1]:8181/abc=/").as_deref(),
            Some("http://[::1]:8181/abc=/")
        );
    }
}
