//! A stand-in for the adb server and the devices behind it, for integration tests
//! (ARCHITECTURE.md §8): it answers the smart-socket requests traffic-police makes (PROTOCOL.md
//! Appendix A), runs the shell commands the host sends against a model of each device, forwards
//! TCP ports to fake abstract sockets, and stops and starts like a real server (transport ids
//! begin at 1 again). [`FakeRuntime`] plays the capture runtime in an app process: `hello`, the
//! handshake, replay from `resume_after_seq`, events as they happen, pings, and its process dying.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use traffic_police_adb::Adb;
use traffic_police_proto::{Decoder, Frame, PROTOCOL_VERSION};

/// A fake adb server on 127.0.0.1, with its devices.
#[derive(Clone)]
pub struct FakeAdb {
    inner: Arc<Inner>,
}

struct Inner {
    port: u16,
    state: Mutex<State>,
    /// Bumped on every change of the device list (`track-devices` sends the list again).
    devices_changed: watch::Sender<u64>,
    running: Mutex<Option<Running>>,
}

struct Running {
    accept: JoinHandle<()>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

#[derive(Default)]
struct State {
    devices: Vec<Dev>,
    forwards: Vec<Forward>,
    next_transport: u64,
}

struct Forward {
    serial: String,
    port: u16,
    remote: String,
    task: JoinHandle<()>,
}

struct Dev {
    serial: String,
    transport_id: u64,
    state: String,
    model: String,
    api: u32,
    features: Vec<String>,
    processes: Vec<Process>,
    sockets: HashMap<String, FakeRuntime>,
    unix_readable: bool,
    commands: Vec<String>,
    /// Logcat lines by pid (`logcat -d --pid=`).
    logs: Vec<(u32, String)>,
    /// Device TCP ports and the host address that serves each (`forward tcp:0 tcp:<port>`).
    tcp: HashMap<u16, std::net::SocketAddr>,
}

#[derive(Clone)]
struct Process {
    pid: u32,
    name: String,
}

impl FakeAdb {
    /// Starts a server on a free port.
    pub async fn start() -> FakeAdb {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port for the fake adb server");
        let port = listener.local_addr().expect("local address").port();
        let (devices_changed, _) = watch::channel(0);
        let fake = FakeAdb {
            inner: Arc::new(Inner {
                port,
                state: Mutex::new(State { next_transport: 1, ..State::default() }),
                devices_changed,
                running: Mutex::new(None),
            }),
        };
        fake.serve(listener);
        fake
    }

    /// A client for this server, with short timeouts.
    pub fn client(&self) -> Adb {
        Adb::at("127.0.0.1", self.inner.port).with_timeout(Duration::from_secs(3))
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }

    /// Adds an online device; returns its transport id.
    pub fn add_device(&self, serial: &str, api: u32) -> u64 {
        let id = {
            let mut st = self.inner.state.lock().unwrap();
            let id = st.next_transport;
            st.next_transport += 1;
            st.devices.push(Dev {
                serial: serial.to_string(),
                transport_id: id,
                state: "device".into(),
                model: "Fake_Phone".into(),
                api,
                features: ["shell_v2", "cmd", "stat_v2", "ls_v2", "fixed_push_mkdir", "abb", "abb_exec"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                processes: Vec::new(),
                sockets: HashMap::new(),
                unix_readable: true,
                commands: Vec::new(),
                logs: Vec::new(),
                tcp: HashMap::new(),
            });
            id
        };
        self.devices_changed();
        id
    }

    /// Connects or disconnects a device (`device` or `offline`); going away drops its forwards.
    pub fn set_online(&self, serial: &str, online: bool) {
        {
            let mut st = self.inner.state.lock().unwrap();
            if let Some(d) = st.devices.iter_mut().find(|d| d.serial == serial) {
                d.state = if online { "device".into() } else { "offline".into() };
            }
            if !online {
                drop_forwards(&mut st, |f| f.serial == serial);
            }
        }
        self.devices_changed();
    }

    /// Whether the device's shell may read `/proc/net/unix` (some devices hide it).
    pub fn set_unix_readable(&self, serial: &str, readable: bool) {
        self.with_device(serial, |d| d.unix_readable = readable);
    }

    /// A debuggable app process (it shows in `track-jdwp`).
    pub fn start_process(&self, serial: &str, pid: u32, name: &str) {
        self.with_device(serial, |d| d.processes.push(Process { pid, name: name.to_string() }));
    }

    /// A line the process logs (what `logcat -d --pid=<pid>` shows).
    pub fn log(&self, serial: &str, pid: u32, line: &str) {
        self.with_device(serial, |d| d.logs.push((pid, line.to_string())));
    }

    /// A TCP port on the device, served by `to` on this computer (a Flutter app's VM service).
    pub fn listen_tcp(&self, serial: &str, port: u16, to: std::net::SocketAddr) {
        self.with_device(serial, |d| {
            d.tcp.insert(port, to);
        });
    }

    /// A process with a capture runtime listening on its socket.
    pub fn start_app(&self, serial: &str, runtime: &FakeRuntime) {
        let name = runtime.socket_name();
        self.with_device(serial, |d| {
            d.processes.push(Process { pid: runtime.pid(), name: runtime.process().to_string() });
            d.sockets.insert(name, runtime.clone());
        });
    }

    /// The process exits: its socket goes, and connections to it close.
    pub fn kill_process(&self, serial: &str, pid: u32) {
        let runtimes: Vec<FakeRuntime> = self.with_device(serial, |d| {
            d.processes.retain(|p| p.pid != pid);
            let gone: Vec<String> = d.sockets.iter().filter(|(_, r)| r.pid() == pid).map(|(n, _)| n.clone()).collect();
            gone.iter().filter_map(|n| d.sockets.remove(n)).collect()
        });
        for r in runtimes {
            r.die();
        }
    }

    /// Stops the server: every connection closes, forwards go (as when someone runs
    /// `adb kill-server`).
    pub fn stop(&self) {
        if let Some(r) = self.inner.running.lock().unwrap().take() {
            r.accept.abort();
            for c in r.connections.lock().unwrap().drain(..) {
                c.abort();
            }
        }
        let mut st = self.inner.state.lock().unwrap();
        drop_forwards(&mut st, |_| true);
    }

    /// Starts the server again on the same port; transport ids begin at 1 again.
    pub async fn restart(&self) {
        self.stop();
        {
            let mut st = self.inner.state.lock().unwrap();
            st.next_transport = 1;
            for i in 0..st.devices.len() {
                let id = st.next_transport;
                st.next_transport += 1;
                st.devices[i].transport_id = id;
            }
        }
        let mut tries = 0;
        let listener = loop {
            match TcpListener::bind(("127.0.0.1", self.inner.port)).await {
                Ok(l) => break l,
                Err(e) if tries < 50 => {
                    tries += 1;
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("rebinding the fake adb server's port: {e}"),
            }
        };
        self.serve(listener);
        self.devices_changed();
    }

    /// The forwards the server holds, as `(serial, local port, remote)`.
    pub fn forwards(&self) -> Vec<(String, u16, String)> {
        let st = self.inner.state.lock().unwrap();
        st.forwards.iter().map(|f| (f.serial.clone(), f.port, f.remote.clone())).collect()
    }

    /// The shell commands the device ran, in order.
    pub fn commands(&self, serial: &str) -> Vec<String> {
        self.with_device(serial, |d| d.commands.clone())
    }

    fn with_device<T>(&self, serial: &str, f: impl FnOnce(&mut Dev) -> T) -> T {
        let mut st = self.inner.state.lock().unwrap();
        let d = st.devices.iter_mut().find(|d| d.serial == serial).expect("no such fake device");
        f(d)
    }

    fn devices_changed(&self) {
        self.inner.devices_changed.send_modify(|n| *n += 1);
    }

    fn serve(&self, listener: TcpListener) {
        let connections: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let inner = self.inner.clone();
        let conns = connections.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else { return };
                let _ = s.set_nodelay(true);
                let inner = inner.clone();
                let task = tokio::spawn(async move { serve_connection(inner, s).await });
                let mut c = conns.lock().unwrap();
                c.retain(|t| !t.is_finished());
                c.push(task);
            }
        });
        *self.inner.running.lock().unwrap() = Some(Running { accept, connections });
    }
}

fn drop_forwards(st: &mut State, which: impl Fn(&Forward) -> bool) {
    let (gone, kept): (Vec<Forward>, Vec<Forward>) = st.forwards.drain(..).partition(|f| which(f));
    st.forwards = kept;
    for f in gone {
        f.task.abort();
    }
}

async fn read_request(s: &mut TcpStream) -> Option<String> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await.ok()?;
    let n = usize::from_str_radix(std::str::from_utf8(&len).ok()?, 16).ok()?;
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).await.ok()?;
    String::from_utf8(buf).ok()
}

fn hex4(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

async fn okay(s: &mut TcpStream) -> bool {
    s.write_all(b"OKAY").await.is_ok()
}

async fn fail(s: &mut TcpStream, message: &str) {
    let mut out = b"FAIL".to_vec();
    out.extend(hex4(message.as_bytes()));
    let _ = s.write_all(&out).await;
}

fn device_list(st: &State) -> String {
    st.devices
        .iter()
        .map(|d| {
            format!(
                "{}\t{} product:fake model:{} device:fake transport_id:{}\n",
                d.serial, d.state, d.model, d.transport_id
            )
        })
        .collect()
}

async fn serve_connection(inner: Arc<Inner>, mut s: TcpStream) {
    let Some(req) = read_request(&mut s).await else { return };
    match req.as_str() {
        "host:version" => {
            if okay(&mut s).await {
                let _ = s.write_all(&hex4(b"0029")).await;
            }
        }
        "host:host-features" => {
            if okay(&mut s).await {
                let _ = s.write_all(&hex4(b"shell_v2,cmd,stat_v2,ls_v2,fixed_push_mkdir,apex,abb,abb_exec")).await;
            }
        }
        "host:devices-l" => {
            let list = device_list(&inner.state.lock().unwrap());
            if okay(&mut s).await {
                let _ = s.write_all(&hex4(list.as_bytes())).await;
            }
        }
        "host:track-devices-l" => {
            if !okay(&mut s).await {
                return;
            }
            let mut changed = inner.devices_changed.subscribe();
            loop {
                let list = device_list(&inner.state.lock().unwrap());
                if s.write_all(&hex4(list.as_bytes())).await.is_err() || changed.changed().await.is_err() {
                    return;
                }
            }
        }
        "host:list-forward" => {
            let text: String = {
                let st = inner.state.lock().unwrap();
                st.forwards.iter().map(|f| format!("{} tcp:{} {}\n", f.serial, f.port, f.remote)).collect()
            };
            if okay(&mut s).await {
                let _ = s.write_all(&hex4(text.as_bytes())).await;
            }
        }
        r if r.starts_with("host-transport-id:") => {
            let rest = &r["host-transport-id:".len()..];
            let Some((id, command)) = rest.split_once(':') else { return fail(&mut s, "bad request").await };
            let id: u64 = id.parse().unwrap_or(0);
            let serial = {
                let st = inner.state.lock().unwrap();
                st.devices.iter().find(|d| d.transport_id == id && d.state == "device").map(|d| d.serial.clone())
            };
            let Some(serial) = serial else { return fail(&mut s, "").await };
            transport_command(&inner, &mut s, &serial, command).await;
        }
        r if r.starts_with("host:transport-id:") => {
            let id: u64 = r["host:transport-id:".len()..].parse().unwrap_or(0);
            let serial = {
                let st = inner.state.lock().unwrap();
                st.devices.iter().find(|d| d.transport_id == id && d.state == "device").map(|d| d.serial.clone())
            };
            let Some(serial) = serial else { return fail(&mut s, "device not found").await };
            if !okay(&mut s).await {
                return;
            }
            let Some(service) = read_request(&mut s).await else { return };
            device_service(&inner, s, &serial, &service).await;
        }
        _ => fail(&mut s, "unknown host service").await,
    }
}

async fn transport_command(inner: &Arc<Inner>, s: &mut TcpStream, serial: &str, command: &str) {
    if command == "features" {
        let features = {
            let st = inner.state.lock().unwrap();
            st.devices.iter().find(|d| d.serial == serial).map(|d| d.features.join(",")).unwrap_or_default()
        };
        if okay(s).await {
            let _ = s.write_all(&hex4(features.as_bytes())).await;
        }
    } else if let Some(remote) = command.strip_prefix("forward:tcp:0;") {
        let Ok(listener) = TcpListener::bind("127.0.0.1:0").await else { return fail(s, "cannot bind").await };
        let port = listener.local_addr().expect("local address").port();
        let task = {
            let inner = inner.clone();
            let serial = serial.to_string();
            let remote = remote.to_string();
            tokio::spawn(async move {
                // the forward's connections: they end with it (dropping the set aborts them)
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let Ok((stream, _)) = listener.accept().await else { return };
                    // a device TCP port: the host address that serves it
                    let tcp = remote.strip_prefix("tcp:").and_then(|p| p.parse::<u16>().ok()).and_then(|p| {
                        let st = inner.state.lock().unwrap();
                        st.devices
                            .iter()
                            .find(|d| d.serial == serial && d.state == "device")
                            .and_then(|d| d.tcp.get(&p).copied())
                    });
                    if let Some(to) = tcp {
                        let mut stream = stream;
                        connections.spawn(async move {
                            if let Ok(mut there) = TcpStream::connect(to).await {
                                let _ = tokio::io::copy_bidirectional(&mut stream, &mut there).await;
                            }
                        });
                        continue;
                    }
                    let runtime = remote.strip_prefix("localabstract:").and_then(|name| {
                        let st = inner.state.lock().unwrap();
                        st.devices
                            .iter()
                            .find(|d| d.serial == serial && d.state == "device")
                            .and_then(|d| d.sockets.get(name).cloned())
                    });
                    // a missing socket: adbd cannot connect, and the server closes the connection
                    if let Some(r) = runtime {
                        connections.spawn(async move { r.serve(stream).await });
                    }
                    while connections.try_join_next().is_some() {}
                }
            })
        };
        inner.state.lock().unwrap().forwards.push(Forward {
            serial: serial.to_string(),
            port,
            remote: remote.to_string(),
            task,
        });
        if okay(s).await && okay(s).await {
            let _ = s.write_all(&hex4(port.to_string().as_bytes())).await;
        }
    } else if let Some(port) = command.strip_prefix("killforward:tcp:") {
        let port: u16 = port.parse().unwrap_or(0);
        let found = {
            let mut st = inner.state.lock().unwrap();
            let before = st.forwards.len();
            drop_forwards(&mut st, |f| f.port == port && f.serial == serial);
            st.forwards.len() != before
        };
        if found {
            if okay(s).await {
                okay(s).await;
            }
        } else {
            fail(s, &format!("listener 'tcp:{port}' not found")).await;
        }
    } else {
        fail(s, "unknown transport command").await;
    }
}

async fn device_service(inner: &Arc<Inner>, mut s: TcpStream, serial: &str, service: &str) {
    if let Some(command) = service.strip_prefix("shell,v2,raw:") {
        if !okay(&mut s).await {
            return;
        }
        // the client closes stdin first (packet 4)
        let mut head = [0u8; 5];
        let _ = tokio::time::timeout(Duration::from_secs(1), s.read_exact(&mut head)).await;
        let (out, err, exit) = {
            let mut st = inner.state.lock().unwrap();
            match st.devices.iter_mut().find(|d| d.serial == serial) {
                Some(d) => run_shell(d, command),
                None => (String::new(), "device gone\n".into(), 255),
            }
        };
        let mut packets = Vec::new();
        for (id, data) in [(1u8, out.as_bytes()), (2u8, err.as_bytes())] {
            if !data.is_empty() {
                packets.push(id);
                packets.extend_from_slice(&(data.len() as u32).to_le_bytes());
                packets.extend_from_slice(data);
            }
        }
        packets.extend_from_slice(&[3, 1, 0, 0, 0, exit]);
        let _ = s.write_all(&packets).await;
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(1), s.read_to_end(&mut rest)).await;
    } else if service == "track-jdwp" {
        let pids: String = {
            let st = inner.state.lock().unwrap();
            st.devices
                .iter()
                .find(|d| d.serial == serial)
                .map(|d| d.processes.iter().map(|p| format!("{}\n", p.pid)).collect())
                .unwrap_or_default()
        };
        if okay(&mut s).await && s.write_all(&hex4(pids.as_bytes())).await.is_ok() {
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
        }
    } else {
        fail(&mut s, &format!("unknown service {service}")).await;
    }
}

/// The device's answer to a shell command: the commands traffic-police sends, against the model.
fn run_shell(d: &mut Dev, command: &str) -> (String, String, u8) {
    d.commands.push(command.to_string());
    if command == "cat /proc/net/unix" {
        if !d.unix_readable {
            return (String::new(), "cat: /proc/net/unix: Permission denied\n".into(), 1);
        }
        let mut out = "Num       RefCount Protocol Flags    Type St Inode Path\n".to_string();
        for (i, name) in d.sockets.keys().enumerate() {
            out.push_str(&format!("0000000000000000: 00000002 00000000 00010000 0001 01 {} @{name}\n", 26000 + i));
        }
        return (out, String::new(), 0);
    }
    if let Some(list) = command.strip_prefix("for p in ")
        && command.contains("cmdline")
    {
        let pids = list.split("; do").next().unwrap_or_default();
        let mut out = String::new();
        for pid in pids.split_whitespace().filter_map(|p| p.parse::<u32>().ok()) {
            out.push_str(&format!("{pid} "));
            if let Some(p) = d.processes.iter().find(|p| p.pid == pid) {
                out.push_str(&p.name);
            }
            out.push('\n');
        }
        return (out, String::new(), 0);
    }
    if command.contains("cgroup.events") {
        return (String::new(), String::new(), 0);
    }
    if let Some(rest) = command.strip_prefix("logcat -d -v brief --pid=") {
        let pid: u32 = rest.trim().parse().unwrap_or(0);
        let out: String = d.logs.iter().filter(|(p, _)| *p == pid).map(|(_, l)| format!("{l}\n")).collect();
        return (out, String::new(), 0);
    }
    if let Some(rest) = command.strip_prefix("test -d /proc/") {
        let pid: u32 = rest.split_whitespace().next().and_then(|p| p.parse().ok()).unwrap_or(0);
        return if d.processes.iter().any(|p| p.pid == pid) {
            ("up\n".into(), String::new(), 0)
        } else {
            (String::new(), String::new(), 1)
        };
    }
    if command.starts_with("getprop ") {
        let mut out = String::new();
        for part in command.split(';') {
            // the clocks, read with the properties (the Flutter backend's offset)
            match part.trim() {
                "cat /proc/uptime" => {
                    out.push_str("8283.09 18565.22\n");
                    continue;
                }
                "date +%s%N" => {
                    out.push_str("1790000000000000000\n");
                    continue;
                }
                _ => {}
            }
            let key = part.trim().strip_prefix("getprop ").unwrap_or("").trim();
            let value = match key {
                "ro.build.version.sdk" => d.api.to_string(),
                "ro.build.version.release" => "16".to_string(),
                "ro.product.model" => d.model.clone(),
                "ro.product.cpu.abi" => "arm64-v8a".to_string(),
                _ => String::new(),
            };
            out.push_str(&value);
            out.push('\n');
        }
        return (out, String::new(), 0);
    }
    if let Some(name) = command.strip_prefix("pidof ") {
        // as the shell reads it: 'quoted' (traffic_police_adb::quote) or bare
        let name = name.trim();
        let name = name
            .strip_prefix('\'')
            .and_then(|n| n.strip_suffix('\''))
            .map_or(name.to_string(), |n| n.replace("'\\''", "'"));
        let pids: Vec<String> = d.processes.iter().filter(|p| p.name == name).map(|p| p.pid.to_string()).collect();
        return if pids.is_empty() {
            (String::new(), String::new(), 1)
        } else {
            (pids.join(" ") + "\n", String::new(), 0)
        };
    }
    (String::new(), format!("/system/bin/sh: {command}: not found\n"), 127)
}

/// The capture runtime in one app process (PROTOCOL.md §6): it speaks first, waits for
/// `hello_ack`, replays what it buffered after the host's `resume_after_seq`, then sends events
/// as they happen and answers pings until its process dies.
#[derive(Clone)]
pub struct FakeRuntime {
    inner: Arc<Rt>,
}

struct Rt {
    package: String,
    process: String,
    pid: u32,
    instance: String,
    protocol: AtomicU32,
    /// Every event so far, encoded, with `seq` = index + 1.
    events: Mutex<Vec<Value>>,
    added: watch::Sender<usize>,
    alive: watch::Sender<bool>,
    next_txn: AtomicU64,
    started: Instant,
    /// The `hello_ack`s received, for tests to look at.
    acks: Mutex<Vec<Value>>,
}

/// The device clock at the fake process's start (nanoseconds).
const BASE_TS: u64 = 5_000_000_000_000;

impl FakeRuntime {
    pub fn new(package: &str, process: &str, pid: u32) -> FakeRuntime {
        let (added, _) = watch::channel(0);
        let (alive, _) = watch::channel(true);
        FakeRuntime {
            inner: Arc::new(Rt {
                package: package.to_string(),
                process: process.to_string(),
                pid,
                instance: format!("fake-{package}-{pid}"),
                protocol: AtomicU32::new(PROTOCOL_VERSION),
                events: Mutex::new(Vec::new()),
                added,
                alive,
                next_txn: AtomicU64::new(1),
                started: Instant::now(),
                acks: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Speaks another protocol version (for mismatch tests).
    pub fn with_protocol(self, version: u32) -> FakeRuntime {
        self.inner.protocol.store(version, Ordering::Relaxed);
        self
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    pub fn process(&self) -> &str {
        &self.inner.process
    }

    pub fn socket_name(&self) -> String {
        traffic_police_adb::socket_name(&self.inner.package, self.inner.pid)
    }

    /// The `hello_ack`s this runtime received.
    pub fn acks(&self) -> Vec<Value> {
        self.inner.acks.lock().unwrap().clone()
    }

    fn now(&self) -> u64 {
        BASE_TS + self.inner.started.elapsed().as_nanos() as u64
    }

    /// A finished request: `req`, `resp`, an empty response body, and `done`. Returns its id.
    pub fn request(&self, method: &str, url: &str, status: u16) -> u64 {
        let txn = self.inner.next_txn.fetch_add(1, Ordering::Relaxed);
        let ts = self.now();
        let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or_default().to_string();
        self.push(json!({"t": "req", "ts": ts, "txn": txn, "call": txn, "method": method, "url": url,
            "headers": [["Host", host]], "client": {"kind": "okhttp", "version": "5.5.0"},
            "thread": {"name": "main", "id": 1, "origin": "call"}}));
        self.push(json!({"t": "resp", "ts": ts + 1_000_000, "txn": txn, "status": status, "message": "",
            "protocol": "http/1.1", "headers": [["Content-Length", "0"]], "conn": null}));
        self.push(json!({"t": "body_end", "ts": ts + 2_000_000, "txn": txn, "dir": "response", "bytes": 0,
            "captured": 0, "state": "none"}));
        self.push(json!({"t": "done", "ts": ts + 2_000_000, "txn": txn}));
        txn
    }

    fn push(&self, mut event: Value) {
        let n = {
            let mut events = self.inner.events.lock().unwrap();
            event["seq"] = json!(events.len() + 1);
            events.push(event);
            events.len()
        };
        self.inner.added.send_replace(n);
    }

    /// The process is gone: connections close.
    pub fn die(&self) {
        self.inner.alive.send_replace(false);
    }

    fn hello(&self) -> Value {
        json!({"t": "hello", "protocol": self.inner.protocol.load(Ordering::Relaxed),
            "runtime": {"version": "0.1.0", "build": "fake", "mode": "library"},
            "instance": self.inner.instance,
            "app": {"package": self.inner.package, "process": self.inner.process, "pid": self.inner.pid,
                "debuggable": true},
            "device": {"api": 36, "model": "Fake_Phone", "abi": "arm64-v8a"},
            "clock": self.clock(), "capabilities": ["rules", "pause", "traffic"], "clients": {"okhttp": "5.5.0"}})
    }

    fn clock(&self) -> Value {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        json!({"ts": self.now(), "wall_ms": wall})
    }

    async fn serve(&self, mut s: TcpStream) {
        let mut alive = self.inner.alive.subscribe();
        if !*alive.borrow() {
            return;
        }
        if send(&mut s, &self.hello()).await.is_err() {
            return;
        }
        let mut decoder = Decoder::new();
        let mut buf = vec![0u8; 64 * 1024];
        let Some(ack) = next_json(&mut s, &mut decoder, &mut buf, Duration::from_secs(10)).await else { return };
        if ack["t"] != "hello_ack" {
            return;
        }
        self.inner.acks.lock().unwrap().push(ack.clone());
        let mine = self.inner.protocol.load(Ordering::Relaxed);
        if ack["protocol"] != json!(mine) {
            let _ = send(
                &mut s,
                &json!({"t": "bye", "reason": "protocol_mismatch",
                    "message": format!("host speaks protocol {}; this runtime supports {mine}", ack["protocol"]),
                    "supported": [mine]}),
            )
            .await;
            return;
        }
        let ack_id = ack["id"].as_u64().unwrap_or(0);
        if send(&mut s, &json!({"t": "rules_ack", "id": ack_id, "active": 0})).await.is_err() {
            return;
        }
        let mut sent = ack["resume_after_seq"].as_u64().unwrap_or(0) as usize;
        let mut added = self.inner.added.subscribe();
        loop {
            let batch: Vec<Value> = {
                let events = self.inner.events.lock().unwrap();
                events.get(sent.min(events.len())..).map(<[Value]>::to_vec).unwrap_or_default()
            };
            for e in batch {
                if send(&mut s, &e).await.is_err() {
                    return;
                }
                sent += 1;
            }
            tokio::select! {
                _ = added.changed() => {}
                _ = alive.changed() => {
                    if !*alive.borrow() {
                        return;
                    }
                }
                m = next_json(&mut s, &mut decoder, &mut buf, Duration::from_secs(3600)) => match m {
                    None => return,
                    Some(m) => match m["t"].as_str() {
                        Some("ping") => {
                            let pong = json!({"t": "pong", "id": m["id"], "clock": self.clock()});
                            if send(&mut s, &pong).await.is_err() {
                                return;
                            }
                        }
                        Some("set_rules") => {
                            let _ = send(&mut s, &json!({"t": "rules_ack", "id": m["id"], "active": 0})).await;
                        }
                        Some("set_config") => {
                            let config = json!({"recording": true, "body_cap": 10485760,
                                "capture_request_bodies": true, "capture_response_bodies": true, "stack_depth": 64});
                            let _ = send(&mut s, &json!({"t": "config_ack", "id": m["id"], "config": config})).await;
                        }
                        Some("bye") => return,
                        _ => {}
                    },
                },
            }
        }
    }
}

async fn send(s: &mut TcpStream, msg: &Value) -> std::io::Result<()> {
    let mut out = BytesMut::new();
    traffic_police_proto::frame::encode_json(msg.to_string().as_bytes(), &mut out);
    s.write_all(&out).await
}

/// The next JSON message from the host, or `None` at the end of the connection.
async fn next_json(s: &mut TcpStream, decoder: &mut Decoder, buf: &mut [u8], limit: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        match decoder.next_frame() {
            Ok(Some(Frame::Json(json))) => return serde_json::from_slice(&json).ok(),
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(_) => return None,
        }
        let n = tokio::time::timeout_at(deadline, s.read(buf)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        decoder.push(&buf[..n]);
    }
}
