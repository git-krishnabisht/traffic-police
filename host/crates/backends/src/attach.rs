//! Attach mode on the host (ARCHITECTURE.md §4.7): the agent and its two dex files go into the
//! app's `code_cache` through `run-as`; then the agent is attached to the running process, or
//! the app is started with it (`--launch`), or a startup agent loads it into every start of the
//! app while `--follow` watches. When the agent's socket does not appear, the app's log says why.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context;
use tokio::time::Instant;
use traffic_police_adb::{Adb, AdbError, AppProcess, Device, TransportId, quote};

use crate::device::{DeviceTarget, Launch};

/// The ABIs the agent is built for, in [`AgentKit`]'s order.
pub const ABIS: [&str; 3] = ["arm64-v8a", "armeabi-v7a", "x86_64"];
pub const AGENT_LIB: &str = "libtrafficpolice_agent.so";
pub const BOOT_DEX: &str = "traffic-police-boot.dex";
pub const RUNTIME_DEX: &str = "traffic-police-runtime.dex";

/// Where the files wait on the device between the push and the copy into the app.
const STAGING: &str = "/data/local/tmp/traffic-police";
/// In the app's data directory. The startup agent finds the dex files here too, since its only
/// option is the data directory (§4.7.4).
const CACHE_DIR: &str = "code_cache/traffic-police";
const STARTUP_DIR: &str = "code_cache/startup_agents";
const AGENT_CONF: &str = "agent.conf";

/// How long an agent that was sent may take to open its socket before the app's log is read.
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a process that loads the agent itself at start may take before a runtime attach.
const START_GRACE: Duration = Duration::from_secs(5);
/// After `am start --attach-agent`, how long a new process counts as the one it started.
const LAUNCH_WINDOW: Duration = Duration::from_secs(10);
/// A startup agent's expiry stamp: renewed while traffic-police runs, so one that crashed or was
/// killed leaves an agent that stays inactive once the stamp has passed.
const STARTUP_TTL: Duration = Duration::from_secs(300);
const STARTUP_RENEW: Duration = Duration::from_secs(60);
/// How often to look again while an attach is under way, and otherwise.
const FAST: Duration = Duration::from_millis(200);
const POLL: Duration = Duration::from_secs(1);

/// The agent and its dex files (`./gradlew :attach-agent:agentArtifacts`), from a directory or
/// embedded in the binary.
#[derive(Clone)]
pub struct AgentKit {
    boot_dex: Cow<'static, [u8]>,
    runtime_dex: Cow<'static, [u8]>,
    /// One per ABI, in the order of [`ABIS`].
    agents: [Cow<'static, [u8]>; 3],
    /// Where the files came from, for messages: a directory, or "embedded in this binary".
    pub origin: String,
}

impl std::fmt::Debug for AgentKit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentKit")
            .field("origin", &self.origin)
            .field("boot_dex", &self.boot_dex.len())
            .field("runtime_dex", &self.runtime_dex.len())
            .field("agents", &self.agents.iter().map(|a| a.len()).collect::<Vec<_>>())
            .finish()
    }
}

impl AgentKit {
    /// The paths in an agent directory, relative to it.
    pub fn files() -> [String; 5] {
        [
            format!("{}/{AGENT_LIB}", ABIS[0]),
            format!("{}/{AGENT_LIB}", ABIS[1]),
            format!("{}/{AGENT_LIB}", ABIS[2]),
            BOOT_DEX.to_string(),
            RUNTIME_DEX.to_string(),
        ]
    }

    /// Reads a directory that `agentArtifacts` produced.
    pub fn from_dir(dir: &Path) -> anyhow::Result<Self> {
        let read = |name: &str| -> anyhow::Result<Cow<'static, [u8]>> {
            let p = dir.join(name);
            Ok(Cow::Owned(std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?))
        };
        let [a, b, c, boot, runtime] = Self::files();
        Ok(AgentKit {
            agents: [read(&a)?, read(&b)?, read(&c)?],
            boot_dex: read(&boot)?,
            runtime_dex: read(&runtime)?,
            origin: dir.display().to_string(),
        })
    }

    /// Files built into the binary; `agents` in the order of [`ABIS`].
    pub fn embedded(boot_dex: &'static [u8], runtime_dex: &'static [u8], agents: [&'static [u8]; 3]) -> Self {
        AgentKit {
            boot_dex: Cow::Borrowed(boot_dex),
            runtime_dex: Cow::Borrowed(runtime_dex),
            agents: agents.map(Cow::Borrowed),
            origin: "embedded in this binary".to_string(),
        }
    }

    pub fn agent_for_abi(&self, abi: &str) -> Option<&[u8]> {
        ABIS.iter().position(|a| *a == abi).map(|i| &*self.agents[i])
    }
}

/// Why an attach step did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// Waiting will not change it (the app is not debuggable, no agent for its ABI, ...).
    Fatal(String),
    /// It may pass (the process was still starting, or has just exited; adb hiccuped).
    Passing(String),
}

impl Problem {
    pub fn text(&self) -> &str {
        match self {
            Problem::Fatal(s) | Problem::Passing(s) => s,
        }
    }
}

impl From<AdbError> for Problem {
    fn from(e: AdbError) -> Self {
        Problem::Passing(e.to_string())
    }
}

/// The ABI directory for an ISA name from `track-app`.
fn isa_to_abi(isa: &str) -> &str {
    match isa {
        "arm64" => "arm64-v8a",
        "arm" => "armeabi-v7a",
        other => other,
    }
}

/// `primaryCpuAbi=arm64-v8a` from `dumpsys package`: set when the app has native code.
fn primary_cpu_abi(dumpsys: &str) -> Option<&str> {
    dumpsys
        .lines()
        .find_map(|l| l.trim().strip_prefix("primaryCpuAbi="))
        .map(str::trim)
        .filter(|abi| !abi.is_empty() && *abi != "null")
}

/// The ABI the process runs with, which the agent's must match (ART loads no agent through a
/// native bridge): the ISA from `track-app` (Android 12+), else the package's `primaryCpuAbi`,
/// else the device's first ABI. It may be one no agent is built for (x86).
pub async fn process_abi(adb: &Adb, device: &Device, package: &str, isa: Option<&str>) -> String {
    if let Some(isa) = isa {
        return isa_to_abi(isa).to_string();
    }
    let q = quote(package);
    if let Ok(out) = adb.shell(device.transport_id, &format!("dumpsys package {q} | grep primaryCpuAbi")).await
        && let Some(abi) = primary_cpu_abi(&out.stdout_text())
    {
        return abi.to_string();
    }
    match adb.shell(device.transport_id, "getprop ro.product.cpu.abi").await {
        Ok(out) if !out.stdout_text().trim().is_empty() => out.stdout_text().trim().to_string(),
        _ => ABIS[0].to_string(),
    }
}

/// The device's API level.
pub async fn api_level(adb: &Adb, device: &Device) -> anyhow::Result<u32> {
    let out = adb.shell(device.transport_id, "getprop ro.build.version.sdk").await.context("reading the API level")?;
    out.stdout_text().trim().parse().context("parsing the API level")
}

/// Whether a process belongs to the package: by the package list (Android 15+ adbd), else by its
/// name (`com.app`, `com.app:sync`).
pub fn of_package(p: &AppProcess, package: &str) -> bool {
    p.package_names.iter().any(|n| n == package)
        || p.process_name
            .as_deref()
            .is_some_and(|n| n == package || n.strip_prefix(package).is_some_and(|r| r.starts_with(':')))
}

/// The process to attach to: `--pid`, else `--process`, else the package's main process (named
/// like the package), else its newest.
pub fn pick_process<'a>(
    procs: &'a [AppProcess],
    package: &str,
    process: Option<&str>,
    pid: Option<u32>,
) -> Option<&'a AppProcess> {
    let mut candidates = procs.iter().filter(|p| of_package(p, package));
    if let Some(pid) = pid {
        return candidates.find(|p| p.pid == pid);
    }
    let wanted = process.unwrap_or(package);
    let all: Vec<&AppProcess> = candidates.collect();
    all.iter()
        .find(|p| p.process_name.as_deref() == Some(wanted))
        .copied()
        .or_else(|| if process.is_some() { None } else { all.into_iter().max_by_key(|p| p.pid) })
}

/// The agent in the app's `code_cache`, as `install` left it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The app's data directory, where `run-as` starts.
    pub data_dir: String,
    pub abi: String,
}

impl Installed {
    /// `path=options` for `attach-agent` and `--attach-agent`: the options name the directory
    /// with the dex files and the package (§4.7.2).
    pub fn agent_arg(&self, package: &str) -> String {
        let dir = format!("{}/{CACHE_DIR}", self.data_dir);
        format!("{dir}/{AGENT_LIB}=dir={dir};package={package}")
    }
}

/// A distinct staging directory per install, so two traffic-police sessions never push over
/// each other's files.
fn staging_dir() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("{STAGING}/{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The shell script `run-as` runs in the app's data directory. A file that is already there with
/// the same bytes is kept (a running process of the app may have it mapped); a different one is
/// replaced by renaming a copy over it, never by writing into it, and the copies are owner-only
/// and read-only (the dex files must not be writable, §4.7.1). Copies are named after the shell's
/// pid (`$$`), so two sessions installing into one app at once (two of its processes) do not
/// collide. With `stamp`, the agent also goes into `code_cache/startup_agents`, after an
/// `agent.conf` with its expiry. Prints the data directory.
fn install_script(staging: &str, stamp: Option<(i64, &str)>) -> String {
    let mut s = format!(
        "umask 077; pwd; mkdir -p {CACHE_DIR} || exit 1; \
         put() {{ cmp -s \"$1\" \"$2\" || {{ rm -f \"$2.$$\" && cp \"$1\" \"$2.$$\" && chmod \"$3\" \"$2.$$\" && mv -f \"$2.$$\" \"$2\"; }}; }}; \
         put {staging}/{AGENT_LIB} {CACHE_DIR}/{AGENT_LIB} 500 && \
         put {staging}/{BOOT_DEX} {CACHE_DIR}/{BOOT_DEX} 400 && \
         put {staging}/{RUNTIME_DEX} {CACHE_DIR}/{RUNTIME_DEX} 400 || exit 1"
    );
    if let Some((expires_at_ms, package)) = stamp {
        s.push_str(&format!(
            "; {} && mkdir -p {STARTUP_DIR} && put {staging}/{AGENT_LIB} {STARTUP_DIR}/{AGENT_LIB} 500 || exit 1",
            conf_script(expires_at_ms, package)
        ));
    }
    s
}

/// Writes `agent.conf` (read by a startup agent, §4.7.4) by renaming a new copy over it.
fn conf_script(expires_at_ms: i64, package: &str) -> String {
    format!(
        "printf 'expires_at_ms=%s\\npackage=%s\\n' {expires_at_ms} {package} > {CACHE_DIR}/.{AGENT_CONF}.$$ && mv -f {CACHE_DIR}/.{AGENT_CONF}.$$ {CACHE_DIR}/{AGENT_CONF}"
    )
}

/// What `run-as` (or the script it ran) said when it failed.
fn run_as_problem(package: &str, out: &traffic_police_adb::ShellOutput) -> Problem {
    let text = format!("{}{}", String::from_utf8_lossy(&out.stderr), out.stdout_text()).trim().to_string();
    if text.contains("not debuggable") {
        Problem::Fatal(format!("{package} is not debuggable (run-as refused); attach mode needs a debuggable build"))
    } else if text.contains("unknown package") {
        Problem::Fatal(format!("{package} is not installed"))
    } else if text.starts_with("run-as:") {
        // e.g. "run-as: run-as is disabled from the kernel commandline"
        Problem::Fatal(format!("run-as does not work on this device, and attach mode needs it: {}", first_line(&text)))
    } else {
        Problem::Fatal(format!("copying the agent into {package} failed: {}", first_line(&text)))
    }
}

/// Attach mode works only in debuggable apps (ARCHITECTURE.md §7), and needs `run-as`: both
/// are checked before anything is copied.
pub async fn check_run_as(adb: &Adb, device: &Device, package: &str) -> Result<(), Problem> {
    let out = adb.shell(device.transport_id, &format!("run-as {} true", quote(package))).await?;
    if out.exit != 0 {
        return Err(run_as_problem(package, &out));
    }
    Ok(())
}

/// The device's clock (the expiry stamp is compared with it, not with this computer's).
async fn device_now_ms(adb: &Adb, id: TransportId) -> Result<i64, Problem> {
    let out = adb.shell(id, "date +%s").await?;
    let secs: i64 =
        out.stdout_text().trim().parse().map_err(|_| {
            Problem::Passing(format!("the device's clock did not read ({:?})", out.stdout_text().trim()))
        })?;
    Ok(secs * 1000)
}

/// Copies the agent for `abi` and the dex files into the app (through a staging directory in
/// `/data/local/tmp`, since only `run-as` can write the app's directory). With `startup`, the
/// agent also goes into `code_cache/startup_agents` with a fresh expiry stamp.
pub async fn install(
    adb: &Adb,
    device: &Device,
    package: &str,
    abi: &str,
    kit: &AgentKit,
    startup: bool,
) -> Result<Installed, Problem> {
    let agent = kit.agent_for_abi(abi).ok_or_else(|| {
        Problem::Fatal(format!("{package} runs as {abi}; the agent is built for {} only", ABIS.join(", ")))
    })?;
    let id = device.transport_id;
    let stamp = if startup { Some(device_now_ms(adb, id).await? + STARTUP_TTL.as_millis() as i64) } else { None };
    let staging = staging_dir();
    let made = adb.shell(id, &format!("umask 022; mkdir -p {staging}")).await?;
    if made.exit != 0 {
        return Err(Problem::Passing(format!(
            "cannot create {staging} on the device: {}",
            String::from_utf8_lossy(&made.stderr).trim()
        )));
    }
    tracing::info!(package, abi, %staging, startup, "installing the agent");
    let pushed = async {
        adb.push(id, agent, &format!("{staging}/{AGENT_LIB}"), 0o644).await?;
        adb.push(id, &kit.boot_dex, &format!("{staging}/{BOOT_DEX}"), 0o644).await?;
        adb.push(id, &kit.runtime_dex, &format!("{staging}/{RUNTIME_DEX}"), 0o644).await
    }
    .await;
    let copied = match pushed {
        Ok(()) => {
            let script = install_script(&staging, stamp.map(|s| (s, package)));
            adb.shell(id, &format!("run-as {} sh -c {}", quote(package), quote(&script))).await.map_err(Problem::from)
        }
        Err(e) => Err(Problem::Passing(format!("pushing the agent to the device failed: {e}"))),
    };
    let _ = adb.shell(id, &format!("rm -rf {staging}")).await;
    let out = copied?;
    if out.exit != 0 {
        return Err(run_as_problem(package, &out));
    }
    let data_dir = out.stdout_text().lines().next().unwrap_or("").trim().to_string();
    if !data_dir.starts_with('/') {
        return Err(Problem::Fatal(format!("{package}'s data directory did not read ({data_dir:?})")));
    }
    Ok(Installed { data_dir, abi: abi.to_string() })
}

/// Renews the startup agent's expiry stamp.
async fn renew_stamp(adb: &Adb, id: TransportId, package: &str) -> Result<(), Problem> {
    let stamp = device_now_ms(adb, id).await? + STARTUP_TTL.as_millis() as i64;
    let script = format!("[ -f {STARTUP_DIR}/{AGENT_LIB} ] || exit 0; {}", conf_script(stamp, package));
    let out = adb.shell(id, &format!("run-as {} sh -c {}", quote(package), quote(&script))).await?;
    if out.exit != 0 {
        return Err(run_as_problem(package, &out));
    }
    Ok(())
}

/// Removes our startup agent and its stamp; other files in `startup_agents` (another tool's
/// agents) stay, and the directory goes only when it is empty.
pub async fn remove_startup_agent(adb: &Adb, device: &Device, package: &str) {
    let script =
        format!("rm -f {STARTUP_DIR}/{AGENT_LIB} {CACHE_DIR}/{AGENT_CONF}; rmdir {STARTUP_DIR} 2>/dev/null; true");
    match adb.shell(device.transport_id, &format!("run-as {} sh -c {}", quote(package), quote(&script))).await {
        Ok(_) => tracing::info!(package, "removed the startup agent"),
        Err(e) => tracing::warn!(package, "could not remove the startup agent: {e}"),
    }
}

/// `cmd activity attach-agent`: a one-way call to the app, so success here means only that the
/// app got the request; its socket says the rest.
pub async fn attach_agent(
    adb: &Adb,
    device: &Device,
    pid: u32,
    installed: &Installed,
    package: &str,
) -> Result<(), Problem> {
    let cmd = format!("cmd activity attach-agent {pid} {}", quote(&installed.agent_arg(package)));
    tracing::info!(pid, package, "attach-agent");
    let out = adb.shell(device.transport_id, &cmd).await?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stderr), out.stdout_text()).trim().to_string();
    if out.exit == 0 && !text.contains("Exception") {
        return Ok(());
    }
    Err(if text.contains("Unknown process") || text.contains("disappeared") {
        // not attached to the system yet (just forked), or exited meanwhile
        Problem::Passing(format!("pid {pid} is not ready for the agent yet"))
    } else if text.contains("not debuggable") {
        Problem::Fatal(format!("{package} (pid {pid}) is not debuggable; attach mode needs a debuggable build"))
    } else {
        Problem::Fatal(format!("attach-agent failed: {}", first_line(&text)))
    })
}

fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(no output)")
}

/// The launcher activity, e.g. `com.example.app/.MainActivity`.
async fn launcher_activity(adb: &Adb, device: &Device, package: &str) -> Result<String, Problem> {
    let out = adb
        .shell(
            device.transport_id,
            &format!("cmd package resolve-activity --brief -c android.intent.category.LAUNCHER {}", quote(package)),
        )
        .await?;
    let component = out.stdout_text().lines().last().unwrap_or("").trim().to_string();
    if component.contains('/') {
        Ok(component)
    } else {
        Err(Problem::Fatal(format!("{package} has no launcher activity on {} (is it installed?)", device.label())))
    }
}

/// `am start`: with `restart`, a running app is stopped first (`-S`), so the start is a new
/// process; `agent` is `path=options` for `--attach-agent` (API 27+); `extras` go last.
fn start_command(component: &str, restart: bool, agent: Option<&str>, extras: &[String]) -> String {
    let mut cmd = String::from("am start");
    if restart {
        cmd.push_str(" -S");
    }
    cmd.push_str(" -n ");
    cmd.push_str(&quote(component));
    if let Some(a) = agent {
        cmd.push_str(" --attach-agent ");
        cmd.push_str(&quote(a));
    }
    for e in extras {
        cmd.push(' ');
        cmd.push_str(&quote(e));
    }
    cmd
}

/// Starts the app's launcher activity (`--launch`).
pub async fn start_app(
    adb: &Adb,
    device: &Device,
    package: &str,
    restart: bool,
    agent: Option<&str>,
    extras: &[String],
) -> Result<(), Problem> {
    let component = launcher_activity(adb, device, package).await?;
    let cmd = start_command(&component, restart, agent, extras);
    tracing::info!(%cmd, "starting the app");
    let out = adb.shell(device.transport_id, &cmd).await?;
    let text = format!("{}{}", out.stdout_text(), String::from_utf8_lossy(&out.stderr));
    if out.exit != 0 || text.lines().any(|l| l.starts_with("Error")) {
        let error = text.lines().find(|l| l.starts_with("Error")).unwrap_or_else(|| first_line(&text));
        return Err(Problem::Fatal(format!("could not start {component}: {}", error.trim())));
    }
    Ok(())
}

/// Log lines that explain an agent that did not start: its own warnings and errors, ART's
/// agent messages, the framework's, and the first exception printed after the agent loaded
/// (the cause its own lines name only by stage).
fn agent_trouble(log: &str) -> Vec<String> {
    let lines: Vec<&str> = log.lines().map(str::trim).collect();
    let message = |l: &str| l.split_once(": ").map_or(l, |(_, m)| m).trim().to_string();
    let mut out: Vec<String> = lines
        .iter()
        .filter(|l| {
            let (level, rest) = l.split_at(l.find('/').unwrap_or(0));
            let ours = rest.starts_with("/TrafficPoliceAgent") || rest.starts_with("/TrafficPolice:");
            (ours && matches!(level, "W" | "E" | "F"))
                || l.contains("Agent attach failed")
                || l.contains("Unable to dlopen")
                || l.contains("dlopen failed")
                || l.contains("Could not load plugin")
                || (rest.starts_with("/ActivityThread") && l.contains("agent"))
        })
        .map(|l| message(l))
        .collect();
    let start = lines.iter().position(|l| l.contains("/TrafficPoliceAgent")).unwrap_or(lines.len());
    let cause = lines[start..].iter().map(|l| message(l)).find(|m| {
        let name = m.split(':').next().unwrap_or("");
        name.contains('.') && !name.contains(' ') && (name.ends_with("Exception") || name.ends_with("Error"))
    });
    if !out.is_empty()
        && let Some(c) = cause
    {
        out.push(c);
    }
    out
}

/// Log lines of a process that died: Java's fatal exception, a fatal signal.
fn crash_lines(log: &str) -> Vec<String> {
    let lines: Vec<&str> = log.lines().map(str::trim).collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if l.starts_with("F/") || l.contains("FATAL EXCEPTION") {
            out.push(l.split_once(": ").map_or(*l, |(_, m)| m).trim().to_string());
            // the exception itself follows "FATAL EXCEPTION: main" and "Process: ..."
            if l.contains("FATAL EXCEPTION")
                && let Some(e) = lines[i + 1..].iter().find(|n| !n.contains("Process:") && !n.contains("FATAL"))
            {
                out.push(e.split_once(": ").map_or(*e, |(_, m)| m).trim().to_string());
            }
        }
    }
    out
}

fn summary(lines: Vec<String>) -> Option<String> {
    let mut lines = lines;
    lines.dedup();
    let n = lines.len();
    (n > 0).then(|| lines[n.saturating_sub(3)..].join(" · "))
}

/// Why the agent sent to `pid` has no socket, from the app's log; `None` when the log says
/// nothing (the app may simply be busy: the agent loads on its main thread).
pub async fn diagnose(adb: &Adb, device: &Device, pid: u32) -> Option<String> {
    let out = adb.shell(device.transport_id, &format!("logcat -d -v tag -t 2000 --pid={pid}")).await.ok()?;
    summary(agent_trouble(&out.stdout_text()))
}

/// Why a process died, from its log; `None` when it did not crash (it was stopped, or closed).
async fn crashed(adb: &Adb, device: &Device, pid: u32) -> Option<String> {
    let out =
        adb.shell(device.transport_id, &format!("logcat -d -b main -b crash -v tag -t 2000 --pid={pid}")).await.ok()?;
    summary(crash_lines(&out.stdout_text()))
}

/// What the device backend does next, in attach mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Show this and look again after the duration.
    Wait(String, Duration),
    /// Stop: waiting cannot help.
    Fail(String),
}

impl From<Problem> for Step {
    fn from(p: Problem) -> Self {
        match p {
            Problem::Fatal(s) => Step::Fail(s),
            Problem::Passing(s) => Step::Wait(s, POLL),
        }
    }
}

/// The agent was sent to this pid; its socket has not appeared yet.
struct Sent {
    pid: u32,
    at: Instant,
    diagnosed: bool,
}

/// Our agent in `code_cache/startup_agents` (API 30+): every start of the app loads it before
/// the app's own code runs, while its stamp is fresh.
struct Startup {
    serial: String,
    package: String,
    /// Kept after the first connection only with `--follow`.
    keep: bool,
    renewer: tokio::task::JoinHandle<()>,
}

/// Attach mode inside the device backend (ARCHITECTURE.md §4.7.2, §4.7.4): starts the app when
/// asked, keeps a startup agent while `--launch` or `--follow` needs one, attaches the agent to
/// the target process once per pid, and reads the app's log when the socket does not appear.
pub struct Attacher {
    kit: Arc<AgentKit>,
    launch: Option<Launch>,
    follow: bool,
    /// Whether the start-of-session work (launch, startup agent) is done.
    prepared: bool,
    device: Option<TransportId>,
    api: u32,
    sent: Option<Sent>,
    startup: Option<Startup>,
    /// When each pid was first seen, and whether it loads the agent itself at start (a startup
    /// agent, or the process `--attach-agent` started): those get a moment before a runtime
    /// attach, so a process never gets two copies.
    seen: HashMap<u32, (Instant, bool)>,
    /// Until when a new process is the one `am start --attach-agent` started.
    launched_until: Option<Instant>,
}

impl Attacher {
    pub fn new(kit: Arc<AgentKit>, target: &DeviceTarget) -> Attacher {
        Attacher {
            kit,
            launch: target.launch.clone(),
            follow: target.follow,
            prepared: false,
            device: None,
            api: 0,
            sent: None,
            startup: None,
            seen: HashMap::new(),
            launched_until: None,
        }
    }

    /// A new device (or the same one after a reconnect, with a new transport id).
    async fn on_device(&mut self, adb: &Adb, device: &Device) {
        if self.device != Some(device.transport_id) {
            self.device = Some(device.transport_id);
            self.api = api_level(adb, device).await.unwrap_or(0);
            self.sent = None;
        }
    }

    /// The pids running now: a startup agent or a launch that comes next reaches none of them.
    async fn note_running(&mut self, adb: &Adb, device: &Device) {
        let features = adb.device_features(device.transport_id).await.unwrap_or_default();
        let now = Instant::now();
        for p in adb.app_processes(device.transport_id, &features).await.unwrap_or_default() {
            self.seen.entry(p.pid).or_insert((now, false));
        }
    }

    /// Before looking for the app, once: the startup agent (API 30+, with `--launch` or
    /// `--follow`) and the launch. `None` when there is nothing to report.
    pub async fn prepare(&mut self, adb: &Adb, device: &Device, target: &DeviceTarget) -> Option<Step> {
        if self.prepared {
            return None;
        }
        self.on_device(adb, device).await;
        let package = target.package.as_str();
        if let Err(p) = check_run_as(adb, device, package).await {
            return Some(p.into());
        }
        let startup = self.api >= 30 && (self.launch.is_some() || self.follow);
        let with_flag = self.launch.is_some() && (27..30).contains(&self.api);
        let mut installed = None;
        if startup || with_flag {
            self.note_running(adb, device).await;
            let abi = process_abi(adb, device, package, None).await;
            match install(adb, device, package, &abi, &self.kit, startup).await {
                Ok(i) => installed = Some(i),
                Err(p) => return Some(p.into()),
            }
            if startup && self.startup.is_none() {
                self.startup = Some(Startup {
                    serial: device.serial.clone(),
                    package: package.to_string(),
                    keep: self.follow,
                    renewer: tokio::spawn(renew(adb.clone(), device.serial.clone(), package.to_string())),
                });
            }
        }
        let mut text = None;
        if let Some(launch) = self.launch.take() {
            if !startup && !with_flag {
                self.note_running(adb, device).await;
            }
            let agent = installed.as_ref().filter(|_| with_flag).map(|i| i.agent_arg(package));
            if let Err(p) = start_app(adb, device, package, true, agent.as_deref(), &launch.extras).await {
                if matches!(p, Problem::Passing(_)) {
                    self.launch = Some(launch);
                }
                return Some(p.into());
            }
            if with_flag {
                self.launched_until = Some(Instant::now() + LAUNCH_WINDOW);
            }
            text = Some(match self.api {
                30.. => format!("started {package}; the agent loads with it"),
                29 => format!("started {package} with the agent"),
                27..29 => format!("started {package} with the agent (best effort on Android 8.1 and 9)"),
                _ => format!(
                    "started {package}; Android 8.0 cannot load the agent at launch, so requests from its first moments may be missed"
                ),
            });
        }
        self.prepared = true;
        text.map(|t| Step::Wait(t, FAST))
    }

    /// The target has no socket yet: find its process, and attach the agent once per pid.
    /// `restarted`: the watched process exited (`--follow`).
    pub async fn step(&mut self, adb: &Adb, device: &Device, target: &DeviceTarget, restarted: bool) -> Step {
        self.on_device(adb, device).await;
        let package = target.package.as_str();
        let name = target.process.as_deref().unwrap_or(package);
        // just sent: the main loop's socket scan is what matters now
        if let Some(sent) = self.sent.as_ref().filter(|s| s.at.elapsed() < SOCKET_TIMEOUT) {
            return Step::Wait(format!("agent sent to {name} (pid {}); waiting for its socket…", sent.pid), FAST);
        }
        let features = match adb.device_features(device.transport_id).await {
            Ok(f) => f,
            Err(e) => return Step::Wait(format!("{}: {e}", device.label()), POLL),
        };
        let procs = match adb.app_processes(device.transport_id, &features).await {
            Ok(p) => p,
            Err(e) => return Step::Wait(format!("{}: {e}", device.label()), POLL),
        };
        // the process the agent was sent to died before its socket appeared
        if let Some(sent) = self.sent.take_if(|s| !procs.iter().any(|p| p.pid == s.pid))
            && let Some(why) = crashed(adb, device, sent.pid).await
        {
            return Step::Fail(format!("{name} (pid {}) crashed after the agent was attached: {why}", sent.pid));
        }
        let Some(proc) = pick_process(&procs, package, target.process.as_deref(), target.pid) else {
            let text = if restarted {
                format!("{package} exited; waiting for it to start again (--follow)")
            } else {
                format!("waiting for {name} on {} (start the app, or use --launch)", device.label())
            };
            return Step::Wait(text, POLL);
        };
        let pid = proc.pid;
        let now = Instant::now();
        let at_start = self.startup.is_some() || self.launched_until.is_some_and(|t| now < t);
        let (first_seen, loads_itself) = *self.seen.entry(pid).or_insert((now, at_start));
        if !proc.debuggable {
            return Step::Fail(format!("{name} (pid {pid}) is not debuggable; attach mode needs a debuggable build"));
        }
        if adb.frozen_pids(device.transport_id, &[pid]).await.is_ok_and(|f| f.contains(&pid)) {
            return Step::Wait(crate::device::frozen_text(name, pid), POLL);
        }
        if let Some(sent) = self.sent.as_mut().filter(|s| s.pid == pid) {
            if !sent.diagnosed {
                sent.diagnosed = true;
                if let Some(why) = diagnose(adb, device, pid).await {
                    return Step::Fail(format!(
                        "the agent did not start in {name} (pid {pid}): {why}. An agent stays in a process until it exits: restart the app before trying again"
                    ));
                }
            }
            return Step::Wait(
                format!(
                    "the agent was sent to {name} (pid {pid}) {} s ago but has not opened its socket; the app's main thread may be busy (adb logcat --pid={pid} may say why)",
                    sent.at.elapsed().as_secs()
                ),
                POLL,
            );
        }
        if loads_itself && first_seen.elapsed() < START_GRACE {
            return Step::Wait(format!("{name} (pid {pid}) is starting with the agent…"), FAST);
        }
        if loads_itself {
            tracing::warn!(pid, "the agent did not load at start; attaching it at run time");
        }
        let abi = process_abi(adb, device, package, proc.architecture.as_deref()).await;
        let installed = match install(adb, device, package, &abi, &self.kit, false).await {
            Ok(i) => i,
            Err(p) => return p.into(),
        };
        match attach_agent(adb, device, pid, &installed, package).await {
            Ok(()) => {
                self.sent = Some(Sent { pid, at: Instant::now(), diagnosed: false });
                Step::Wait(format!("agent sent to {name} (pid {pid}); waiting for its socket…"), FAST)
            }
            Err(Problem::Passing(text)) => Step::Wait(text, FAST),
            Err(p) => p.into(),
        }
    }

    /// The target's socket is there: the agent sent to it started. A startup agent that was
    /// there only for `--launch` has done its work.
    pub async fn found(&mut self, adb: &Adb, device: &Device, pid: u32) {
        if self.sent.as_ref().is_some_and(|s| s.pid == pid) {
            self.sent = None;
        }
        self.seen.entry(pid).or_insert((Instant::now(), false));
        if self.startup.as_ref().is_some_and(|s| !s.keep)
            && let Some(s) = self.startup.take()
        {
            s.renewer.abort();
            remove_startup_agent(adb, device, &s.package).await;
        }
    }

    /// The session ends: the startup agent goes.
    pub async fn finish(&mut self, adb: &Adb) {
        let Some(s) = self.startup.take() else { return };
        s.renewer.abort();
        match adb.devices().await {
            Ok(list) => match list.into_iter().find(|d| d.serial == s.serial && d.is_online()) {
                Some(device) => remove_startup_agent(adb, &device, &s.package).await,
                None => tracing::warn!(
                    "{} is offline; its startup agent stays until its stamp passes, and does nothing after",
                    s.serial
                ),
            },
            Err(e) => tracing::warn!("could not remove the startup agent: {e}"),
        }
    }
}

/// Keeps the startup agent's stamp fresh while the session runs (the device may reconnect with
/// a new transport id, so it is found by serial each time).
async fn renew(adb: Adb, serial: String, package: String) {
    loop {
        tokio::time::sleep(STARTUP_RENEW).await;
        let device = match adb.devices().await {
            Ok(list) => list.into_iter().find(|d| d.serial == serial && d.is_online()),
            Err(_) => None,
        };
        if let Some(d) = device
            && let Err(p) = renew_stamp(&adb, d.transport_id, &package).await
        {
            tracing::warn!("renewing the startup agent's stamp: {}", p.text());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, name: &str, packages: &[&str]) -> AppProcess {
        AppProcess {
            pid,
            debuggable: true,
            profileable: false,
            architecture: None,
            process_name: Some(name.to_string()),
            package_names: packages.iter().map(|p| p.to_string()).collect(),
            uid: None,
        }
    }

    #[test]
    fn the_main_process_unless_another_is_named() {
        // before Android 15 adbd lists no packages: the names decide
        let procs = [proc(10, "com.app:sync", &[]), proc(20, "com.app", &[]), proc(30, "com.apple", &[])];
        assert_eq!(pick_process(&procs, "com.app", None, None).map(|p| p.pid), Some(20));
        assert_eq!(pick_process(&procs, "com.app", Some("com.app:sync"), None).map(|p| p.pid), Some(10));
        assert_eq!(pick_process(&procs, "com.app", Some("com.app:gone"), None), None);
        assert_eq!(pick_process(&procs, "com.app", None, Some(10)).map(|p| p.pid), Some(10));
        // not a process of com.app
        assert_eq!(pick_process(&procs, "com.app", None, Some(30)), None);
        // only a secondary process runs: the newest one
        let only = [proc(10, "com.app:sync", &[]), proc(11, "com.app:push", &[])];
        assert_eq!(pick_process(&only, "com.app", None, None).map(|p| p.pid), Some(11));
        // a process named otherwise, listed under the package (Android 15+)
        let named = [proc(40, "custom.name", &["com.app"])];
        assert_eq!(pick_process(&named, "com.app", None, None).map(|p| p.pid), Some(40));
    }

    #[test]
    fn abis() {
        assert_eq!(isa_to_abi("arm64"), "arm64-v8a");
        assert_eq!(isa_to_abi("arm"), "armeabi-v7a");
        assert_eq!(isa_to_abi("x86_64"), "x86_64");
        assert_eq!(isa_to_abi("x86"), "x86");
        let dump = "  Package [com.app] (4f2):\n    primaryCpuAbi=armeabi-v7a\n    secondaryCpuAbi=null\n";
        assert_eq!(primary_cpu_abi(dump), Some("armeabi-v7a"));
        assert_eq!(primary_cpu_abi("    primaryCpuAbi=null\n"), None);
        let kit = AgentKit::embedded(b"boot", b"runtime", [b"arm64", b"arm", b"x64"]);
        assert_eq!(kit.agent_for_abi("armeabi-v7a"), Some(&b"arm"[..]));
        assert_eq!(kit.agent_for_abi("x86"), None);
    }

    #[test]
    fn the_agent_argument_names_the_directory_and_package() {
        let i = Installed { data_dir: "/data/user/0/com.app".into(), abi: "arm64-v8a".into() };
        assert_eq!(
            i.agent_arg("com.app"),
            "/data/user/0/com.app/code_cache/traffic-police/libtrafficpolice_agent.so=dir=/data/user/0/com.app/code_cache/traffic-police;package=com.app"
        );
    }

    #[test]
    fn launch_commands() {
        assert_eq!(start_command("com.app/.Main", false, None, &[]), "am start -n 'com.app/.Main'");
        assert_eq!(
            start_command(
                "com.app/.Main",
                true,
                Some("/d/a.so=dir=/d;package=com.app"),
                &["--es".into(), "run".into(), "all".into()]
            ),
            "am start -S -n 'com.app/.Main' --attach-agent '/d/a.so=dir=/d;package=com.app' '--es' 'run' 'all'"
        );
    }

    /// The install script as the device's shell (mksh) runs it; checked for the parts that
    /// matter, and run for real by `sh` in `install_script_runs` on Unix.
    #[test]
    fn install_script_replaces_by_renaming() {
        let s = install_script("/data/local/tmp/traffic-police/1-0", None);
        assert!(s.starts_with("umask 077; pwd; "), "{s}");
        assert!(s.contains("mv -f \"$2.$$\" \"$2\""), "{s}");
        assert!(!s.contains("startup_agents"), "{s}");
        let s = install_script("/tmp/x", Some((1_790_000_000_000, "com.app")));
        assert!(s.contains("expires_at_ms=%s\\npackage=%s\\n' 1790000000000 com.app"), "{s}");
        assert!(
            s.contains("put /tmp/x/libtrafficpolice_agent.so code_cache/startup_agents/libtrafficpolice_agent.so 500"),
            "{s}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_script_runs() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("tp-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (staging, app) = (root.join("staging"), root.join("app"));
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        for (f, b) in [(AGENT_LIB, "so1"), (BOOT_DEX, "boot1"), (RUNTIME_DEX, "rt1")] {
            std::fs::write(staging.join(f), b).unwrap();
        }
        let run = |script: String| {
            let out = std::process::Command::new("sh").arg("-c").arg(&script).current_dir(&app).output().unwrap();
            assert!(out.status.success(), "{script}\n{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap()
        };
        let staging_s = staging.to_str().unwrap();
        let printed = run(install_script(staging_s, None));
        assert_eq!(Path::new(printed.trim()).canonicalize().unwrap(), app.canonicalize().unwrap());
        let so = app.join(CACHE_DIR).join(AGENT_LIB);
        assert_eq!(std::fs::read(&so).unwrap(), b"so1");
        assert_eq!(std::fs::metadata(&so).unwrap().permissions().mode() & 0o777, 0o500);
        // a second install over read-only files: the same bytes stay (same inode), new ones replace
        let inode = |p: &Path| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(p).unwrap());
        let (so_inode, boot) = (inode(&so), app.join(CACHE_DIR).join(BOOT_DEX));
        std::fs::write(staging.join(BOOT_DEX), "boot2").unwrap();
        run(install_script(staging_s, Some((42, "com.app"))));
        assert_eq!(inode(&so), so_inode, "an identical file is left alone");
        assert_eq!(std::fs::read(&boot).unwrap(), b"boot2");
        assert_eq!(std::fs::metadata(&boot).unwrap().permissions().mode() & 0o777, 0o400);
        assert_eq!(
            std::fs::read_to_string(app.join(CACHE_DIR).join(AGENT_CONF)).unwrap(),
            "expires_at_ms=42\npackage=com.app\n"
        );
        assert_eq!(std::fs::read(app.join(STARTUP_DIR).join(AGENT_LIB)).unwrap(), b"so1");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trouble_from_the_log() {
        let log = "\
I/TrafficPoliceSample: backend on 127.0.0.1:35641
W/System.err: java.lang.IllegalStateException: the app's own, before the agent
I/TrafficPoliceAgent: agent attached; initialization continues on a private thread
E/System  : Unable to load dex files
E/System  : java.io.IOException: Failed to open dex files from memory: Bad file size (1000, expected 147236)
E/System  : \tat dalvik.system.DexFile.openInMemoryDexFilesNative(Native Method)
W/System.err: java.lang.ClassNotFoundException: io.trafficpolice.internal.AttachEntry
E/TrafficPoliceAgent: runtime loading failed during loading the runtime entry point
W/com.app: Unable to dlopen /x/libtrafficpolice_agent.so: dlopen failed: \"/x/libtrafficpolice_agent.so\" is 64-bit instead of 32-bit
E/ActivityThread: Attaching agent with dalvik.system.PathClassLoader[...] failed: /x/libtrafficpolice_agent.so=dir=/x
I/TrafficPolice: attach capture started
D/Something: unrelated agent talk
";
        let t = agent_trouble(log);
        assert_eq!(t.len(), 4, "{t:?}");
        assert_eq!(t[0], "runtime loading failed during loading the runtime entry point");
        assert!(t[1].contains("is 64-bit instead of 32-bit"));
        assert!(t[2].starts_with("Attaching agent with"));
        assert_eq!(
            t[3],
            "java.io.IOException: Failed to open dex files from memory: Bad file size (1000, expected 147236)"
        );
        // nothing from the agent: nothing to say (the app may just be busy)
        assert!(agent_trouble("W/System.err: java.lang.IllegalStateException: boom\n").is_empty());
        assert_eq!(summary(Vec::new()), None);
        let crash = "\
E/AndroidRuntime: FATAL EXCEPTION: main
E/AndroidRuntime: Process: com.app, PID: 4242
E/AndroidRuntime: java.lang.IllegalStateException: boom
E/AndroidRuntime: \tat com.app.Main.onCreate(Main.java:10)
";
        assert_eq!(
            summary(crash_lines(crash)).unwrap(),
            "FATAL EXCEPTION: main · java.lang.IllegalStateException: boom"
        );
        assert_eq!(
            summary(crash_lines("F/libc: Fatal signal 11 (SIGSEGV), code 1 in tid 42 (main)\n")).unwrap(),
            "Fatal signal 11 (SIGSEGV), code 1 in tid 42 (main)"
        );
        assert_eq!(summary(crash_lines("I/ActivityManager: Killing 42:com.app/u0a1 (adj 0): stop com.app\n")), None);
    }
}
