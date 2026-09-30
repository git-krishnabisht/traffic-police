//! The device backend (ARCHITECTURE.md §5.5): finds the app's capture runtime over adb, connects
//! through a forward, does the handshake, and streams its events into the session. It keeps
//! going across hiccups: the same process is resumed where it left off (by `seq`), a restarted
//! app is followed with `--follow`, and anything else ends in DETACHED with all data kept.

use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use traffic_police_adb::{Adb, AdbError, Device, DeviceList, RuntimeSocket};
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::{BackendCommand, ConnectionStatus};
use traffic_police_core::model::{SourceId, SourceInfo};
use traffic_police_core::normalize::{Control, Normalizer};
use traffic_police_core::session::{DeviceRecord, SessionLog};
use traffic_police_core::store::SourceIds;
use traffic_police_proto::msg::{self, CaptureConfig, CaptureConfigPatch, HostMsg, RuleSet};
use traffic_police_proto::{Decoder, PROTOCOL_VERSION};

/// Which app to watch.
#[derive(Debug, Clone, Default)]
pub struct DeviceTarget {
    /// Device serial; with one online device it may be left out.
    pub serial: Option<String>,
    pub package: String,
    /// Process name (e.g. `com.app:sync`); the package's main process when absent.
    pub process: Option<String>,
    pub pid: Option<u32>,
    /// Reattach when the app restarts, as a new segment on the timeline.
    pub follow: bool,
}

/// Removes forwards to capture sockets that no longer exist on the device: a traffic-police that
/// was killed (or crashed) could not remove its own. Forwards to live sockets, and anything that
/// is not a traffic-police socket, are left alone.
async fn sweep_stale_forwards(adb: &Adb, device: &Device) {
    let (Ok(forwards), Ok(sockets)) = (adb.list_forwards().await, adb.runtime_sockets(device.transport_id).await)
    else {
        return;
    };
    for (serial, local, remote) in forwards {
        let Some(name) = remote.strip_prefix("localabstract:") else { continue };
        if serial != device.serial
            || !name.starts_with(traffic_police_adb::PREFIX)
            || sockets.iter().any(|s| s.name == name)
        {
            continue;
        }
        if let Some(port) = local.strip_prefix("tcp:").and_then(|p| p.parse().ok()) {
            tracing::info!("removing a stale forward tcp:{port} to @{name} (its process is gone)");
            let _ = adb.kill_forward(device.transport_id, port).await;
        }
    }
}

/// Whether Android's cached-apps freezer holds the process (it then runs no code at all).
async fn is_frozen(adb: &Adb, device: &Device, pid: u32) -> bool {
    adb.frozen_pids(device.transport_id, &[pid]).await.is_ok_and(|f| f.contains(&pid))
}

fn frozen_text(process: &str, pid: u32) -> String {
    format!(
        "{process} (pid {pid}) is frozen by Android: it is cached in the background and runs no code. Capture continues when the app runs again."
    )
}

/// Publishes a status change (and logs it, for troubleshooting).
fn set_status(status: &watch::Sender<ConnectionStatus>, s: ConnectionStatus) {
    if *status.borrow() != s {
        tracing::info!("connection: {s:?}");
        status.send_replace(s);
    }
}

const HELLO_TIMEOUT: Duration = Duration::from_secs(3);
const PING_EVERY: Duration = Duration::from_secs(5);
const STALL_AFTER: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_secs(1);

/// A process we were attached to, so a reconnect can resume it.
struct Resume {
    instance: String,
    source: SourceId,
    pid: u32,
    last_seq: u64,
    /// The device clock at a known host instant (from `hello`, refreshed by `pong`).
    clock: (u64, Instant),
    /// Its source has been ended on the timeline (it gets a new segment if it comes back).
    closed: bool,
}

impl Resume {
    /// The device's clock now, estimated from the last clock pair.
    fn device_now(&self) -> u64 {
        self.clock.0 + self.clock.1.elapsed().as_nanos() as u64
    }
}

/// Why a connection ended.
enum End {
    /// The user quit or the UI went away.
    Shutdown,
    /// The socket closed or stalled; the process may still be alive.
    Lost(String),
    /// The runtime said goodbye.
    Bye(String, String),
    /// Nothing answered on the socket (not a capture runtime, or it just exited).
    NoRuntime,
    /// The runtime speaks another protocol version.
    Mismatch(String),
}

/// Where a connection's results go: events for the store, and the captured stream for the
/// session log (when one is kept).
struct Out {
    events: mpsc::Sender<Vec<SessionEvent>>,
    log: Option<Arc<SessionLog>>,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_device(
    adb: Adb,
    target: DeviceTarget,
    ids: SourceIds,
    events: mpsc::Sender<Vec<SessionEvent>>,
    mut commands: mpsc::UnboundedReceiver<BackendCommand>,
    status: watch::Sender<ConnectionStatus>,
    log: Option<Arc<SessionLog>>,
) {
    let out = Out { events, log };
    // the process we attach to (kept across hiccups and device disconnects, to resume it)
    let mut resume: Option<Resume> = None;
    let mut devices = adb.watch_devices();
    let mut config = CaptureConfig::default();
    // with --follow, the process that exited (its socket may linger for a moment)
    let mut followed_away_from: Option<u32> = None;
    let mut swept = std::collections::HashSet::new();
    loop {
        // a quit while waiting ends the backend
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                BackendCommand::Shutdown => return,
                BackendCommand::SetRecording(on) => config.recording = on,
                _ => {}
            }
        }
        if out.events.is_closed() {
            return;
        }
        let snapshot = devices.borrow_and_update().clone();
        let device = match choose_device(&snapshot, target.serial.as_deref()) {
            Ok(d) => d,
            Err(msg) => {
                let msg = if resume.is_some() {
                    format!("the device was disconnected; reconnect it to continue ({msg})")
                } else {
                    msg
                };
                set_status(&status, ConnectionStatus::Waiting(msg));
                if wait_or_quit(&mut commands, &mut config, &mut devices, POLL).await {
                    return;
                }
                continue;
            }
        };
        if swept.insert(device.transport_id) {
            sweep_stale_forwards(&adb, &device).await;
        }
        let socket = match find_socket(&adb, &device, &target, resume.as_ref(), followed_away_from).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                if let Some(mut r) = resume.take() {
                    // the process we were attached to is gone
                    close_source(&out, &mut r, "the app exited").await;
                    if !target.follow {
                        set_status(&status, ConnectionStatus::Detached("the app exited · data kept".into()));
                        return;
                    }
                    followed_away_from = Some(r.pid);
                }
                let what = if followed_away_from.is_some() {
                    format!("{} exited; waiting for it to start again (--follow)", target.package)
                } else {
                    let name = target.process.as_deref().unwrap_or(&target.package);
                    let name = match target.pid {
                        Some(pid) => format!("{name} pid {pid}"),
                        None => name.to_string(),
                    };
                    format!(
                        "waiting for {name} on {} (start the app; it needs a debug build with the traffic-police library)",
                        device.label()
                    )
                };
                set_status(&status, ConnectionStatus::Waiting(what));
                if wait_or_quit(&mut commands, &mut config, &mut devices, POLL).await {
                    return;
                }
                continue;
            }
            Err(e) => {
                set_status(&status, ConnectionStatus::Waiting(format!("{}: {e}", device.label())));
                if wait_or_quit(&mut commands, &mut config, &mut devices, POLL).await {
                    return;
                }
                continue;
            }
        };

        // a frozen process would leave our connection unanswered in its backlog: wait for it to thaw
        if is_frozen(&adb, &device, socket.pid).await {
            let name = target.process.as_deref().unwrap_or(&target.package);
            set_status(&status, ConnectionStatus::Waiting(frozen_text(name, socket.pid)));
            if wait_or_quit(&mut commands, &mut config, &mut devices, POLL).await {
                return;
            }
            continue;
        }

        let end = connect(&adb, &device, &socket, &ids, &out, &mut commands, &status, &mut resume, &mut config).await;
        let reason = match end {
            End::Shutdown => {
                if let Some(r) = resume.as_mut() {
                    close_source(&out, r, "traffic-police quit").await;
                }
                return;
            }
            End::Mismatch(msg) => {
                set_status(&status, ConnectionStatus::Failed(msg));
                return;
            }
            End::Bye(reason, message) if reason == "replaced" => {
                if let Some(r) = resume.as_mut() {
                    close_source(&out, r, "another traffic-police connected to the app").await;
                }
                set_status(
                    &status,
                    ConnectionStatus::Detached(format!("another traffic-police took over ({message})")),
                );
                return;
            }
            End::Bye(reason, message) => {
                let detail = if message.is_empty() { String::new() } else { format!(": {message}") };
                format!("the app closed the connection ({reason}{detail})")
            }
            End::NoRuntime if resume.is_none() => {
                // a socket without a runtime behind it (it just exited): look again
                if wait_or_quit(&mut commands, &mut config, &mut devices, POLL).await {
                    return;
                }
                continue;
            }
            End::NoRuntime => "the app exited".to_string(),
            End::Lost(reason) => reason,
        };
        // Is the same process still there? Then this was a connection hiccup: resume it.
        let alive = match &resume {
            Some(r) => adb
                .runtime_sockets(device.transport_id)
                .await
                .map(|s| s.iter().any(|x| x.pid == r.pid && x.is_for(&target.package)))
                .unwrap_or(false),
            None => false,
        };
        if alive {
            set_status(&status, ConnectionStatus::Waiting(format!("{reason}; reconnecting")));
            if wait_or_quit(&mut commands, &mut config, &mut devices, Duration::from_millis(300)).await {
                return;
            }
            continue;
        }
        if find_device(&adb, target.serial.as_deref()).await.is_err() {
            // keep `resume`: when the device returns with the process alive, it continues
            if let Some(r) = resume.as_mut() {
                close_source(&out, r, "the device was disconnected").await;
            }
            continue;
        }
        // the process is gone
        if let Some(mut r) = resume.take() {
            close_source(&out, &mut r, "the app exited").await;
            followed_away_from = Some(r.pid);
        }
        if !target.follow {
            set_status(&status, ConnectionStatus::Detached("the app exited · data kept".into()));
            return;
        }
        set_status(
            &status,
            ConnectionStatus::Waiting(format!("{} exited; waiting for it to start again (--follow)", target.package)),
        );
    }
}

/// Waits up to `d`, or until the device list changes; true if a shutdown arrived meanwhile.
/// Pause and resume that arrive while waiting apply to the next connection's configuration.
async fn wait_or_quit(
    commands: &mut mpsc::UnboundedReceiver<BackendCommand>,
    config: &mut CaptureConfig,
    devices: &mut watch::Receiver<DeviceList>,
    d: Duration,
) -> bool {
    let deadline = Instant::now() + d;
    loop {
        tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(BackendCommand::Shutdown) | None => return true,
                Some(BackendCommand::SetRecording(on)) => config.recording = on,
                Some(_) => {}
            },
            changed = devices.changed() => {
                if changed.is_err() {
                    tokio::time::sleep_until(deadline).await;
                }
                return false;
            }
            _ = tokio::time::sleep_until(deadline) => return false,
        }
    }
}

/// Ends the process's source on the timeline (once).
async fn close_source(out: &Out, r: &mut Resume, reason: &str) {
    if !r.closed {
        r.closed = true;
        let at = r.device_now();
        if let Some(log) = &out.log {
            log.source_end(r.source, at, reason);
        }
        let _ =
            out.events.send(vec![SessionEvent::SourceDown { source: r.source, at, reason: reason.to_string() }]).await;
    }
}

/// The online device to use, asking adb now (the tracker can lag a disconnect by a moment).
async fn find_device(adb: &Adb, serial: Option<&str>) -> Result<Device, String> {
    choose_device(&Some(adb.devices().await.map_err(|e| e.to_string())), serial)
}

/// The online device to use from a device list: the given serial, or the only online one.
fn choose_device(list: &DeviceList, serial: Option<&str>) -> Result<Device, String> {
    let devices = match list {
        None => return Err("asking adb for devices…".into()),
        Some(Err(e)) => return Err(format!("cannot reach the adb server: {e}")),
        Some(Ok(d)) => d.clone(),
    };
    match serial {
        Some(s) => match devices.into_iter().find(|d| d.serial == s) {
            Some(d) if d.is_online() => Ok(d),
            Some(d) => Err(format!("{} is {}", d.label(), d.state)),
            None => Err(format!("device {s} is not connected")),
        },
        None => {
            let online: Vec<Device> = devices.into_iter().filter(Device::is_online).collect();
            match online.len() {
                0 => Err("no device connected (plug one in, or start an emulator)".into()),
                1 => Ok(online.into_iter().next().expect("one")),
                n => Err(format!("{n} devices connected; choose one with --serial")),
            }
        }
    }
}

/// The runtime socket for the target, if its process runs with capture.
async fn find_socket(
    adb: &Adb,
    device: &Device,
    target: &DeviceTarget,
    resume: Option<&Resume>,
    not_pid: Option<u32>,
) -> Result<Option<RuntimeSocket>, AdbError> {
    let sockets: Vec<RuntimeSocket> =
        adb.runtime_sockets(device.transport_id).await?.into_iter().filter(|s| s.is_for(&target.package)).collect();
    if let Some(r) = resume {
        return Ok(sockets.into_iter().find(|s| s.pid == r.pid));
    }
    if let Some(pid) = target.pid {
        return Ok(sockets.into_iter().find(|s| s.pid == pid));
    }
    let candidates: Vec<RuntimeSocket> = sockets.into_iter().filter(|s| Some(s.pid) != not_pid).collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    // choose by process name: the requested one, else the package's main process
    let wanted = target.process.clone().unwrap_or_else(|| target.package.clone());
    let pids: Vec<u32> = candidates.iter().map(|s| s.pid).collect();
    let names = adb.process_names(device.transport_id, &pids).await.unwrap_or_default();
    let named = |pid: u32| names.iter().find(|(p, _)| *p == pid).map(|(_, n)| n.as_str());
    if let Some(s) = candidates.iter().find(|s| named(s.pid) == Some(wanted.as_str())) {
        return Ok(Some(s.clone()));
    }
    if target.process.is_some() {
        return Ok(None);
    }
    Ok(candidates.into_iter().max_by_key(|s| s.pid))
}

/// One connection: forward, handshake, stream until it ends. Always removes its forward.
#[allow(clippy::too_many_arguments)]
async fn connect(
    adb: &Adb,
    device: &Device,
    socket: &RuntimeSocket,
    ids: &SourceIds,
    out: &Out,
    commands: &mut mpsc::UnboundedReceiver<BackendCommand>,
    status: &watch::Sender<ConnectionStatus>,
    resume: &mut Option<Resume>,
    config: &mut CaptureConfig,
) -> End {
    let port = match adb.forward(device.transport_id, &format!("localabstract:{}", socket.name)).await {
        Ok(p) => p,
        Err(e) => return End::Lost(format!("adb forward failed: {e}")),
    };
    let end = stream(adb, port, device, socket, ids, out, commands, status, resume, config).await;
    if let Err(e) = adb.kill_forward(device.transport_id, port).await {
        tracing::debug!("removing forward tcp:{port}: {e}");
    }
    end
}

#[allow(clippy::too_many_arguments)]
async fn stream(
    adb: &Adb,
    port: u16,
    device: &Device,
    socket: &RuntimeSocket,
    ids: &SourceIds,
    out: &Out,
    commands: &mut mpsc::UnboundedReceiver<BackendCommand>,
    status: &watch::Sender<ConnectionStatus>,
    resume: &mut Option<Resume>,
    config: &mut CaptureConfig,
) -> End {
    let tcp = match TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(e) => return End::Lost(format!("cannot reach the forwarded port: {e}")),
    };
    let _ = tcp.set_nodelay(true);
    let (mut rd, mut wr) = tcp.into_split();
    let mut decoder = Decoder::new();
    let mut buf = vec![0u8; 64 * 1024];

    // hello, within a few seconds (a forward to a vanished socket connects and then reads EOF);
    // a process that froze meanwhile answers once Android thaws it, so keep this connection
    let hello = loop {
        tokio::select! {
            r = read_hello(&mut rd, &mut decoder, &mut buf) => match r {
                Ok(Some((h, raw))) => break (h, raw),
                Ok(None) => return End::NoRuntime,
                Err(e) => return End::Lost(e),
            },
            cmd = commands.recv() => match cmd {
                Some(BackendCommand::Shutdown) | None => return End::Shutdown,
                Some(BackendCommand::SetRecording(on)) => config.recording = on,
                Some(_) => {}
            },
            _ = tokio::time::sleep(HELLO_TIMEOUT) => {
                if !is_frozen(adb, device, socket.pid).await {
                    return End::NoRuntime;
                }
                set_status(status, ConnectionStatus::Waiting(frozen_text(&socket.package_part, socket.pid)));
            }
        }
    };
    let (hello, raw_hello) = hello;
    if hello.protocol != PROTOCOL_VERSION {
        let bye = HostMsg::Bye(msg::Bye {
            reason: "protocol_mismatch".into(),
            message: Some(format!("this traffic-police supports protocol {PROTOCOL_VERSION}")),
            supported: vec![PROTOCOL_VERSION],
        });
        let _ = send(&mut wr, &bye).await;
        let (older, update) = if hello.protocol > PROTOCOL_VERSION {
            ("host", "traffic-police")
        } else {
            ("library", "the capture library in the app")
        };
        return End::Mismatch(format!(
            "the capture runtime in {} speaks protocol {}; this traffic-police supports {PROTOCOL_VERSION}. The {older} is older: update {update}.",
            hello.app.process, hello.protocol
        ));
    }

    let resumed = resume.as_ref().filter(|r| r.instance == hello.instance).map(|r| (r.source, r.last_seq));
    if resumed.is_none()
        && let Some(old) = resume.as_mut()
    {
        // same pid, different runtime instance: the old process is gone
        close_source(out, old, "the app restarted").await;
    }
    let (source, resume_after) = resumed.unwrap_or_else(|| (ids.next(), 0));
    let ack = HostMsg::HelloAck(msg::HelloAck {
        id: 1,
        protocol: PROTOCOL_VERSION,
        host: msg::HostInfo { name: "traffic-police".into(), version: env!("CARGO_PKG_VERSION").into() },
        resume_after_seq: resume_after,
        config: config.clone(),
        rules: RuleSet { version: "none".into(), rules: Vec::new() },
    });
    if let Err(e) = send(&mut wr, &ack).await {
        return End::Lost(e);
    }
    let mut info = SourceInfo::from_hello(source, &hello, device.label(), Some(device.serial.clone()));
    if resumed.is_some() {
        info.started = info.clock.map_or(info.started, |(ts, _)| ts);
    }
    if let Some(log) = &out.log {
        let device = DeviceRecord { label: device.label(), serial: Some(device.serial.clone()) };
        log.source(source, &device, &raw_hello, resumed.is_some());
    }
    // a resumed process gets a reattach marker from the store (same source, new segment)
    if out.events.send(vec![SessionEvent::SourceUp(Box::new(info))]).await.is_err() {
        return End::Shutdown;
    }
    *resume = Some(Resume {
        instance: hello.instance.clone(),
        source,
        pid: socket.pid,
        last_seq: resume_after,
        clock: (hello.clock.ts, Instant::now()),
        closed: false,
    });
    let live = format!("{} · {} (pid {})", device.label(), hello.app.process, hello.app.pid);
    set_status(status, ConnectionStatus::Live(live.clone()));
    let mut frozen = false;

    let mut normalizer =
        if resume_after > 0 { Normalizer::resume_after(source, resume_after) } else { Normalizer::new(source) };
    let mut next_id = 2u64;
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_frame = Instant::now();

    // frames already buffered behind the hello
    let mut batch = Vec::new();
    if let Some(end) = drain(&mut decoder, &mut normalizer, &mut batch, out.log.as_deref()) {
        return end;
    }
    loop {
        if !batch.is_empty() {
            if let Some(r) = resume.as_mut() {
                r.last_seq = normalizer.last_seq();
                for e in &batch {
                    if let SessionEvent::Clock { ts, .. } = e {
                        r.clock = (*ts, Instant::now());
                    }
                }
            }
            if out.events.send(std::mem::take(&mut batch)).await.is_err() {
                return End::Shutdown;
            }
        }
        tokio::select! {
            n = rd.read(&mut buf) => match n {
                Ok(0) => return End::Lost("the app closed the connection".into()),
                Ok(n) => {
                    last_frame = Instant::now();
                    if frozen {
                        frozen = false;
                        set_status(status, ConnectionStatus::Live(live.clone()));
                    }
                    decoder.push(&buf[..n]);
                    if let Some(end) = drain(&mut decoder, &mut normalizer, &mut batch, out.log.as_deref()) {
                        if let Some(r) = resume.as_mut() {
                            r.last_seq = normalizer.last_seq();
                        }
                        let _ = out.events.send(std::mem::take(&mut batch)).await;
                        return end;
                    }
                }
                Err(e) => return End::Lost(format!("connection error: {e}")),
            },
            cmd = commands.recv() => match cmd {
                Some(BackendCommand::SetRecording(on)) => {
                    config.recording = on;
                    let m = HostMsg::SetConfig(msg::SetConfig {
                        id: next_id,
                        config: CaptureConfigPatch { recording: Some(on), ..Default::default() },
                    });
                    next_id += 1;
                    if let Err(e) = send(&mut wr, &m).await {
                        return End::Lost(e);
                    }
                }
                Some(BackendCommand::Ping) => {
                    let _ = send(&mut wr, &HostMsg::Ping(msg::Ping { id: next_id })).await;
                    next_id += 1;
                }
                Some(BackendCommand::SetRules(_)) => {
                    // rules arrive with rule support; this runtime does not apply them
                }
                Some(BackendCommand::Shutdown) | None => {
                    let bye = HostMsg::Bye(msg::Bye { reason: "shutdown".into(), message: None, supported: Vec::new() });
                    let _ = send(&mut wr, &bye).await;
                    return End::Shutdown;
                }
            },
            _ = ping.tick() => {
                // pongs come back within milliseconds; silence past the next ping means trouble
                let quiet = last_frame.elapsed();
                if quiet > PING_EVERY + Duration::from_secs(2) {
                    if is_frozen(adb, device, socket.pid).await {
                        // the connection survives the freeze; the app answers once thawed
                        if !frozen {
                            frozen = true;
                            set_status(status, ConnectionStatus::Waiting(frozen_text(&hello.app.process, hello.app.pid)));
                        }
                        continue;
                    }
                    if frozen {
                        // thawed: it answers this ping, or stalls from here on
                        frozen = false;
                        last_frame = Instant::now();
                        set_status(status, ConnectionStatus::Live(live.clone()));
                    } else if quiet > STALL_AFTER {
                        return End::Lost("no response from the app for 15 s".into());
                    }
                }
                if let Err(e) = send(&mut wr, &HostMsg::Ping(msg::Ping { id: next_id })).await {
                    return End::Lost(e);
                }
                next_id += 1;
            }
        }
    }
}

/// Decodes buffered frames into `batch`; returns how the connection ends, if it does.
fn drain(
    decoder: &mut Decoder,
    normalizer: &mut Normalizer,
    batch: &mut Vec<SessionEvent>,
    log: Option<&SessionLog>,
) -> Option<End> {
    loop {
        match decoder.next_frame() {
            // pongs become Clock events in the batch (the normalizer adds them)
            Ok(Some(frame)) => {
                if let Some(log) = log {
                    log.frame(normalizer.source(), &frame);
                }
                let control = normalizer.frame(frame, batch);
                match control {
                    Ok(Some(Control::Bye(b))) => return Some(End::Bye(b.reason, b.message.unwrap_or_default())),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("undecodable message from the device: {e}"),
                }
            }
            Ok(None) => return None,
            Err(e) if e.is_fatal() => return Some(End::Lost(format!("corrupt stream from the device: {e}"))),
            Err(e) => tracing::warn!("bad frame from the device: {e}"),
        }
    }
}

async fn read_hello(
    rd: &mut tokio::net::tcp::OwnedReadHalf,
    decoder: &mut Decoder,
    buf: &mut [u8],
) -> Result<Option<(Box<msg::Hello>, bytes::Bytes)>, String> {
    loop {
        match decoder.next_frame() {
            Ok(Some(traffic_police_proto::Frame::Json(json))) => {
                return match msg::parse_device(&json) {
                    Ok(msg::DeviceMsg::Hello(h)) => Ok(Some((Box::new(h), json))),
                    Ok(other) => Err(format!("expected hello, got {other:?}")),
                    Err(e) => Err(format!("undecodable hello: {e}")),
                };
            }
            Ok(Some(_)) => return Err("expected hello, got a body chunk".into()),
            Ok(None) => {}
            Err(e) => return Err(format!("bad frame: {e}")),
        }
        let n = rd.read(buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        decoder.push(&buf[..n]);
    }
}

async fn send(wr: &mut OwnedWriteHalf, m: &HostMsg) -> Result<(), String> {
    let mut out = BytesMut::new();
    msg::encode_msg(m, &mut out).map_err(|e| e.to_string())?;
    wr.write_all(&out).await.map_err(|e| format!("write failed: {e}"))
}
