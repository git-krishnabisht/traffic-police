//! A client for the adb server's smart-socket protocol (PROTOCOL.md Appendix A, verified in
//! docs/research/04-adb-protocol.md). One request per TCP connection, a timeout on every request,
//! devices addressed by transport id, and never anything that disturbs other adb users: no
//! `kill-server`, no `killforward-all`, only forwards we created are removed.

pub mod apps;
pub mod devices;
pub mod pb;
pub mod sockets;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;

pub use apps::AppProcess;
pub use devices::{Device, TransportId};
pub use sockets::{PREFIX, RuntimeSocket, socket_name};

#[derive(Debug, thiserror::Error)]
pub enum AdbError {
    #[error("the adb server is not running at {0} (start it with `adb start-server`)")]
    NoServer(String),
    /// A `FAIL` reply; the message may be empty (e.g. forward to a device that went away).
    #[error("adb: {}", if .0.is_empty() { "device not found or not online" } else { .0.as_str() })]
    Fail(String),
    #[error("adb protocol error: {0}")]
    Protocol(String),
    #[error("adb request timed out: {0}")]
    Timeout(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, AdbError>;

/// The latest device list from [`Adb::watch_devices`]: `None` until the first one arrives,
/// `Err` while the adb server cannot be reached.
pub type DeviceList = Option<std::result::Result<Vec<Device>, String>>;

/// Output of a `shell,v2` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Exit code (128 + signal); 255 if the stream ended without one (device gone).
    pub exit: u8,
}

impl ShellOutput {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Where the adb server listens, and how long requests may take.
#[derive(Debug, Clone)]
pub struct Adb {
    host: String,
    port: u16,
    timeout: Duration,
}

impl Default for Adb {
    fn default() -> Self {
        Adb::from_env()
    }
}

impl Adb {
    /// From `ADB_SERVER_SOCKET` (`tcp:<host>:<port>`), else `ANDROID_ADB_SERVER_ADDRESS` and
    /// `ANDROID_ADB_SERVER_PORT`, else `127.0.0.1:5037` (as the adb CLI does).
    pub fn from_env() -> Adb {
        let mut host = std::env::var("ANDROID_ADB_SERVER_ADDRESS").unwrap_or_else(|_| "127.0.0.1".into());
        let mut port = std::env::var("ANDROID_ADB_SERVER_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5037);
        if let Ok(spec) = std::env::var("ADB_SERVER_SOCKET")
            && let Some(rest) = spec.strip_prefix("tcp:")
            && let Some((h, p)) = rest.rsplit_once(':')
            && let Ok(p) = p.parse()
        {
            host = h.to_string();
            port = p;
        }
        Adb { host, port, timeout: Duration::from_secs(10) }
    }

    pub fn at(host: impl Into<String>, port: u16) -> Adb {
        Adb { host: host.into(), port, timeout: Duration::from_secs(10) }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Adb {
        self.timeout = timeout;
        self
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    async fn connect(&self) -> Result<TcpStream> {
        let addr = self.address();
        match tokio::time::timeout(self.timeout, TcpStream::connect(&addr)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                Ok(s)
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Err(AdbError::NoServer(addr)),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(AdbError::Timeout(format!("connecting to {addr}"))),
        }
    }

    /// Starts the server with `adb start-server` when nothing listens (never restarts a running
    /// one, which would disconnect every other adb user).
    pub async fn ensure_server(&self) -> Result<()> {
        match self.connect().await {
            Ok(_) => Ok(()),
            Err(AdbError::NoServer(addr)) => {
                let adb = find_adb_binary().ok_or_else(|| AdbError::NoServer(addr.clone()))?;
                tracing::info!(adb = %adb.display(), "starting the adb server");
                let status = tokio::process::Command::new(&adb)
                    .args(["-P", &self.port.to_string(), "start-server"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await?;
                if !status.success() {
                    return Err(AdbError::NoServer(addr));
                }
                // a freshly started server may hold connections for about 3 s
                for _ in 0..40 {
                    if self.connect().await.is_ok() {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(AdbError::NoServer(addr))
            }
            Err(e) => Err(e),
        }
    }

    /// Connects, sends one request, and reads its status; the stream is left for the reply.
    async fn request(&self, payload: &str) -> Result<TcpStream> {
        let mut s = self.connect().await?;
        self.timed(payload, async {
            send(&mut s, payload).await?;
            read_status(&mut s).await
        })
        .await?;
        Ok(s)
    }

    async fn timed<T>(&self, what: &str, f: impl Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(self.timeout, f).await {
            Ok(r) => r,
            Err(_) => Err(AdbError::Timeout(what.to_string())),
        }
    }

    pub async fn server_version(&self) -> Result<u32> {
        let mut s = self.request("host:version").await?;
        let v = self.timed("host:version", read_hex4_payload(&mut s)).await?;
        u32::from_str_radix(std::str::from_utf8(&v).unwrap_or(""), 16)
            .map_err(|_| AdbError::Protocol(format!("bad version reply {v:?}")))
    }

    pub async fn host_features(&self) -> Result<Vec<String>> {
        let mut s = self.request("host:host-features").await?;
        let v = self.timed("host:host-features", read_hex4_payload(&mut s)).await?;
        Ok(csv(&v))
    }

    /// The current device list (`host:devices-l`).
    pub async fn devices(&self) -> Result<Vec<Device>> {
        let mut s = self.request("host:devices-l").await?;
        let v = self.timed("host:devices-l", read_hex4_payload(&mut s)).await?;
        Ok(devices::parse_long(&String::from_utf8_lossy(&v)))
    }

    /// The device list, kept current by `host:track-devices` in a background task that
    /// reconnects if the adb server restarts. The task stops when every receiver is dropped.
    pub fn watch_devices(&self) -> watch::Receiver<DeviceList> {
        let (tx, rx) = watch::channel(None);
        let adb = self.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(250);
            loop {
                match adb.track_devices().await {
                    Ok(mut tracker) => {
                        backoff = Duration::from_millis(250);
                        loop {
                            tokio::select! {
                                r = tracker.next() => match r {
                                    Ok(list) => {
                                        if tx.send(Some(Ok(list))).is_err() {
                                            return;
                                        }
                                    }
                                    Err(e) => {
                                        let _ = tx.send(Some(Err(e.to_string())));
                                        break;
                                    }
                                },
                                _ = tx.closed() => return,
                            }
                        }
                    }
                    Err(e) => {
                        if tx.send(Some(Err(e.to_string()))).is_err() {
                            return;
                        }
                    }
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = tx.closed() => return,
                }
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        });
        rx
    }

    /// A tracker that yields the full device list on every change (binary proto when the
    /// server supports it, the long text format otherwise).
    pub async fn track_devices(&self) -> Result<DeviceTracker> {
        let proto =
            self.host_features().await.map(|f| f.iter().any(|x| x == "devicetracker_proto_format")).unwrap_or(false);
        let service = if proto { "host:track-devices-proto-binary" } else { "host:track-devices-l" };
        let stream = self.request(service).await?;
        Ok(DeviceTracker { stream, proto, last: None })
    }

    /// The device's feature list (only works while it is online).
    pub async fn device_features(&self, id: TransportId) -> Result<Vec<String>> {
        let req = format!("host-transport-id:{id}:features");
        let mut s = self.request(&req).await?;
        let v = self.timed(&req, read_hex4_payload(&mut s)).await?;
        Ok(csv(&v))
    }

    /// Switches to the device and opens one device service; the stream is then that service.
    pub async fn open_service(&self, id: TransportId, service: &str) -> Result<TcpStream> {
        let mut s = self.connect().await?;
        self.timed(service, async {
            send(&mut s, &format!("host:transport-id:{id}")).await?;
            read_status(&mut s).await?;
            send(&mut s, service).await?;
            read_status(&mut s).await
        })
        .await?;
        Ok(s)
    }

    /// Runs `command` under `sh -c` with the shell protocol, for its output and exit code.
    pub async fn shell(&self, id: TransportId, command: &str) -> Result<ShellOutput> {
        let mut s = self.open_service(id, &format!("shell,v2,raw:{command}")).await?;
        self.timed(command, async {
            // close stdin: nothing to send
            s.write_all(&[4, 0, 0, 0, 0]).await?;
            read_shell_packets(&mut s).await
        })
        .await
    }

    /// `adb forward tcp:0 <remote>`: returns the local port the server bound on 127.0.0.1.
    pub async fn forward(&self, id: TransportId, remote: &str) -> Result<u16> {
        let req = format!("host-transport-id:{id}:forward:tcp:0;{remote}");
        let mut s = self.request(&req).await?;
        self.timed(&req, async {
            read_status(&mut s).await?;
            let port = read_hex4_payload(&mut s).await?;
            std::str::from_utf8(&port)
                .ok()
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| AdbError::Protocol(format!("bad forward port {port:?}")))
        })
        .await
    }

    /// Removes a forward we created (by its local port). Gone already is fine.
    pub async fn kill_forward(&self, id: TransportId, port: u16) -> Result<()> {
        let req = format!("host-transport-id:{id}:killforward:tcp:{port}");
        match self.request(&req).await {
            Ok(mut s) => {
                let _ = self.timed(&req, read_status(&mut s)).await;
                Ok(())
            }
            Err(AdbError::Fail(m)) if m.contains("not found") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// All forwards the server holds, as `(serial, local, remote)`, e.g.
    /// `("emulator-5554", "tcp:51234", "localabstract:traffic-police_com.example_4312")`.
    pub async fn list_forwards(&self) -> Result<Vec<(String, String, String)>> {
        let mut s = self.request("host:list-forward").await?;
        let v = self.timed("host:list-forward", read_hex4_payload(&mut s)).await?;
        Ok(String::from_utf8_lossy(&v)
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                Some((f.next()?.to_string(), f.next()?.to_string(), f.next()?.to_string()))
            })
            .collect())
    }

    /// Listening capture-runtime sockets on the device.
    pub async fn runtime_sockets(&self, id: TransportId) -> Result<Vec<RuntimeSocket>> {
        let out = self.shell(id, "cat /proc/net/unix").await?;
        Ok(sockets::parse_proc_net_unix(&out.stdout_text()))
    }

    /// Debuggable (and profileable) processes once, with names: from `track-app` when the
    /// device has it, else `track-jdwp` plus `/proc/<pid>/cmdline`.
    pub async fn app_processes(&self, id: TransportId, features: &[String]) -> Result<Vec<AppProcess>> {
        let mut procs = if features.iter().any(|f| f == "track_app") {
            let mut s = self.open_service(id, "track-app").await?;
            let msg = self.timed("track-app", read_hex4_payload(&mut s)).await?;
            apps::parse_app_processes(&msg).ok_or_else(|| AdbError::Protocol("bad track-app message".into()))?
        } else {
            let mut s = self.open_service(id, "track-jdwp").await?;
            let msg = self.timed("track-jdwp", read_hex4_payload(&mut s)).await?;
            apps::parse_jdwp(&String::from_utf8_lossy(&msg))
                .into_iter()
                .map(|pid| AppProcess {
                    pid,
                    debuggable: true,
                    profileable: false,
                    architecture: None,
                    process_name: None,
                    package_names: Vec::new(),
                    uid: None,
                })
                .collect()
        };
        let unnamed: Vec<u32> = procs.iter().filter(|p| p.process_name.is_none()).map(|p| p.pid).collect();
        if !unnamed.is_empty() {
            let names = self.process_names(id, &unnamed).await.unwrap_or_default();
            for p in &mut procs {
                if p.process_name.is_none() {
                    p.process_name = names.iter().find(|(pid, _)| *pid == p.pid).map(|(_, n)| n.clone());
                }
            }
        }
        Ok(procs)
    }

    /// `(pid, argv[0])` for the given pids, in one shell command.
    pub async fn process_names(&self, id: TransportId, pids: &[u32]) -> Result<Vec<(u32, String)>> {
        if pids.is_empty() {
            return Ok(Vec::new());
        }
        let list: Vec<String> = pids.iter().map(u32::to_string).collect();
        // argv[0] is the process name for app processes; NULs separate arguments
        let cmd = format!(
            "for p in {}; do printf '%s ' $p; tr '\\0' '\\n' < /proc/$p/cmdline 2>/dev/null | head -n 1; echo; done",
            list.join(" ")
        );
        let out = self.shell(id, &cmd).await?;
        Ok(out
            .stdout_text()
            .lines()
            .filter_map(|l| {
                let (pid, name) = l.trim().split_once(' ')?;
                Some((pid.parse().ok()?, name.trim().to_string()))
            })
            .filter(|(_, n)| !n.is_empty())
            .collect())
    }

    /// Which of the given pids Android's cached-apps freezer has frozen (Android 11+, cgroup
    /// v2: `frozen 1` in the process cgroup's `cgroup.events`). A frozen process runs no code, so
    /// its capture runtime cannot answer until Android thaws it.
    pub async fn frozen_pids(&self, id: TransportId, pids: &[u32]) -> Result<Vec<u32>> {
        if pids.is_empty() {
            return Ok(Vec::new());
        }
        let list: Vec<String> = pids.iter().map(u32::to_string).collect();
        let cmd = format!(
            "for p in {}; do c=$(sed -n 's/^0:://p' /proc/$p/cgroup 2>/dev/null); \
             [ -n \"$c\" ] && grep -qs '^frozen 1' /sys/fs/cgroup$c/cgroup.events && echo $p; done; true",
            list.join(" ")
        );
        let out = self.shell(id, &cmd).await?;
        Ok(out.stdout_text().lines().filter_map(|l| l.trim().parse().ok()).collect())
    }
}

/// Where to find the adb binary to start a server: the SDK first, then PATH.
/// The adb binary: the SDK's platform-tools, then `PATH`.
pub fn find_adb_binary() -> Option<std::path::PathBuf> {
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    let mut candidates = Vec::new();
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(d) = std::env::var_os(var) {
            candidates.push(std::path::PathBuf::from(d).join("platform-tools").join(exe));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(std::path::PathBuf::from(&home).join("Library/Android/sdk/platform-tools").join(exe));
        candidates.push(std::path::PathBuf::from(&home).join("Android/Sdk/platform-tools").join(exe));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(std::path::PathBuf::from(local).join("Android/Sdk/platform-tools").join(exe));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Streams the device list: the full list on every change, identical repeats skipped.
pub struct DeviceTracker {
    stream: TcpStream,
    proto: bool,
    last: Option<Vec<u8>>,
}

impl DeviceTracker {
    /// The next distinct device list. An error means the server went away.
    pub async fn next(&mut self) -> Result<Vec<Device>> {
        loop {
            let msg = read_hex4_payload(&mut self.stream).await?;
            if self.last.as_deref() == Some(&msg[..]) {
                continue;
            }
            let devices = if self.proto {
                devices::parse_proto(&msg).ok_or_else(|| AdbError::Protocol("bad device list".into()))?
            } else {
                devices::parse_long(&String::from_utf8_lossy(&msg))
            };
            self.last = Some(msg);
            return Ok(devices);
        }
    }
}

/// Single-quotes an argument for `sh -c` (`'` becomes `'\''`), as adb's own escape_arg does.
pub fn quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn csv(v: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(v).split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

async fn send(s: &mut TcpStream, payload: &str) -> Result<()> {
    if payload.len() > 0xffff {
        return Err(AdbError::Protocol("request longer than 65535 bytes".into()));
    }
    let mut buf = format!("{:04x}", payload.len()).into_bytes();
    buf.extend_from_slice(payload.as_bytes());
    s.write_all(&buf).await?;
    Ok(())
}

/// `OKAY`, or `FAIL` + hex4 message.
async fn read_status(s: &mut TcpStream) -> Result<()> {
    let mut status = [0u8; 4];
    read_exact(s, &mut status).await?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let msg = read_hex4_payload(s).await?;
            Err(AdbError::Fail(String::from_utf8_lossy(&msg).trim().to_string()))
        }
        other => Err(AdbError::Protocol(format!("unexpected status {:?}", String::from_utf8_lossy(other)))),
    }
}

async fn read_hex4_payload(s: &mut TcpStream) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    read_exact(s, &mut len).await?;
    let n = usize::from_str_radix(std::str::from_utf8(&len).unwrap_or("x"), 16)
        .map_err(|_| AdbError::Protocol(format!("bad length {:?}", String::from_utf8_lossy(&len))))?;
    let mut buf = vec![0u8; n];
    read_exact(s, &mut buf).await?;
    Ok(buf)
}

async fn read_exact(s: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    match s.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(AdbError::Protocol("the adb server closed the connection".into()))
        }
        Err(e) => Err(e.into()),
    }
}

/// Shell protocol v2: `[u8 id][u32 LE length][payload]`; 1 stdout, 2 stderr, 3 exit.
async fn read_shell_packets(s: &mut TcpStream) -> Result<ShellOutput> {
    let mut out = ShellOutput { stdout: Vec::new(), stderr: Vec::new(), exit: 255 };
    loop {
        let mut head = [0u8; 5];
        match s.read_exact(&mut head).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(out),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let mut payload = vec![0u8; len];
        read_exact(s, &mut payload).await?;
        match head[0] {
            1 => out.stdout.extend_from_slice(&payload),
            2 => out.stderr.extend_from_slice(&payload),
            3 => {
                out.exit = payload.first().copied().unwrap_or(255);
                return Ok(out);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A scripted adb server: for each connection, expects requests and sends canned replies.
    async fn fake_server(script: Vec<Vec<(&'static str, Vec<u8>)>>) -> Adb {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            for conn in script {
                let (mut s, _) = listener.accept().await.unwrap();
                for (expect, reply) in conn {
                    let mut len = [0u8; 4];
                    s.read_exact(&mut len).await.unwrap();
                    let n = usize::from_str_radix(std::str::from_utf8(&len).unwrap(), 16).unwrap();
                    let mut req = vec![0u8; n];
                    s.read_exact(&mut req).await.unwrap();
                    assert_eq!(String::from_utf8(req).unwrap(), expect);
                    s.write_all(&reply).await.unwrap();
                }
            }
        });
        Adb::at("127.0.0.1", port).with_timeout(Duration::from_secs(2))
    }

    #[tokio::test]
    async fn version_features_and_forward() {
        let adb = fake_server(vec![
            vec![("host:version", b"OKAY00040029".to_vec())],
            vec![("host:host-features", b"OKAY001fshell_v2,track_app,app_info,cmd".to_vec())],
            vec![(
                "host-transport-id:14:forward:tcp:0;localabstract:traffic-police_app_77",
                b"OKAYOKAY000553603".to_vec(),
            )],
            vec![("host-transport-id:14:forward:tcp:0;localabstract:x", b"FAIL0000".to_vec())],
        ])
        .await;
        assert_eq!(adb.server_version().await.unwrap(), 41);
        assert_eq!(adb.host_features().await.unwrap(), vec!["shell_v2", "track_app", "app_info", "cmd"]);
        assert_eq!(adb.forward(14, "localabstract:traffic-police_app_77").await.unwrap(), 53603);
        let e = adb.forward(14, "localabstract:x").await.unwrap_err();
        assert!(e.to_string().contains("device not found"), "{e}");
    }

    #[tokio::test]
    async fn shell_v2_collects_output_and_exit_code() {
        let mut shell = b"OKAY".to_vec();
        shell.extend_from_slice(&[1, 6, 0, 0, 0]);
        shell.extend_from_slice(b"hello\n");
        shell.extend_from_slice(&[2, 3, 0, 0, 0]);
        shell.extend_from_slice(b"err");
        shell.extend_from_slice(&[3, 1, 0, 0, 0, 7]);
        let adb =
            fake_server(vec![vec![("host:transport-id:9", b"OKAY".to_vec()), ("shell,v2,raw:echo hello", shell)]])
                .await;
        let out = adb.shell(9, "echo hello").await.unwrap();
        assert_eq!(out.stdout, b"hello\n");
        assert_eq!(out.stderr, b"err");
        assert_eq!(out.exit, 7);
    }

    #[tokio::test]
    async fn no_server_is_reported_clearly() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let e = Adb::at("127.0.0.1", port).server_version().await.unwrap_err();
        assert!(matches!(e, AdbError::NoServer(_)), "{e}");
    }

    #[test]
    fn quoting_for_sh() {
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
