//! Attach orchestration (ARCHITECTURE.md §4.7.2): push the agent artifacts to the device, copy
//! them into the app's `code_cache`, and attach the JVMTI agent.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use traffic_police_adb::{Adb, AppProcess, Device, RuntimeSocket, quote};

/// The three ABIs the agent is built for.
const ABIS: [&str; 3] = ["arm64-v8a", "armeabi-v7a", "x86_64"];

const TMP_DIR: &str = "/data/local/tmp/traffic-police";
const CACHE_DIR: &str = "code_cache/traffic-police";
const AGENT_LIB: &str = "libtrafficpolice_agent.so";
const BOOT_DEX: &str = "traffic-police-boot.dex";
const RUNTIME_DEX: &str = "traffic-police-runtime.dex";
const AGENT_CONF: &str = "agent.conf";

/// How long to wait for the runtime socket after sending `cmd activity attach-agent`.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// The agent artifacts needed for attach mode.
#[derive(Clone)]
pub struct AgentKit {
    pub boot_dex: Vec<u8>,
    pub runtime_dex: Vec<u8>,
    pub agent_arm64: Vec<u8>,
    pub agent_arm: Vec<u8>,
    pub agent_x86_64: Vec<u8>,
}

impl std::fmt::Debug for AgentKit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentKit")
            .field("boot_dex", &self.boot_dex.len())
            .field("runtime_dex", &self.runtime_dex.len())
            .field("agent_arm64", &self.agent_arm64.len())
            .field("agent_arm", &self.agent_arm.len())
            .field("agent_x86_64", &self.agent_x86_64.len())
            .finish()
    }
}

impl AgentKit {
    /// Loads the artifacts from a directory that `agentArtifacts` produced.
    pub fn from_dir(dir: &Path) -> anyhow::Result<Self> {
        let read = |name: &str| {
            let p = dir.join(name);
            std::fs::read(&p).with_context(|| format!("reading {}", p.display()))
        };
        Ok(AgentKit {
            boot_dex: read(BOOT_DEX)?,
            runtime_dex: read(RUNTIME_DEX)?,
            agent_arm64: read(&format!("arm64-v8a/{AGENT_LIB}"))?,
            agent_arm: read(&format!("armeabi-v7a/{AGENT_LIB}"))?,
            agent_x86_64: read(&format!("x86_64/{AGENT_LIB}"))?,
        })
    }

    fn agent_for_abi(&self, abi: &str) -> Option<&[u8]> {
        match abi {
            "arm64-v8a" => Some(&self.agent_arm64),
            "armeabi-v7a" => Some(&self.agent_arm),
            "x86_64" => Some(&self.agent_x86_64),
            _ => None,
        }
    }
}

/// Maps an ISA name from `track-app` to an ABI directory name.
fn isa_to_abi(isa: &str) -> &str {
    match isa {
        "arm64" => "arm64-v8a",
        "arm" => "armeabi-v7a",
        "x86_64" => "x86_64",
        "x86" => "x86",
        other => other,
    }
}

/// Determines the ABI to use for the agent. Tries, in order: the ISA from `track-app`, the
/// package's `primaryCpuAbi` from `dumpsys package`, the device's primary ABI.
pub async fn resolve_abi(adb: &Adb, device: &Device, package: &str, arch: Option<&str>) -> String {
    if let Some(isa) = arch {
        let abi = isa_to_abi(isa);
        if ABIS.contains(&abi) {
            return abi.to_string();
        }
    }

    let q = quote(package);
    if let Ok(out) = adb.shell(device.transport_id, &format!("dumpsys package {q} | grep primaryCpuAbi")).await {
        for line in out.stdout_text().lines() {
            if let Some(abi) = line.trim().strip_prefix("primaryCpuAbi=") {
                let abi = abi.trim();
                if !abi.is_empty() && abi != "null" && ABIS.contains(&abi) {
                    return abi.to_string();
                }
            }
        }
    }

    if let Ok(out) = adb.shell(device.transport_id, "getprop ro.product.cpu.abi").await {
        let abi = out.stdout_text().trim().to_string();
        if ABIS.contains(&abi.as_str()) {
            return abi;
        }
    }

    "arm64-v8a".to_string()
}

/// The result of an attach attempt.
pub enum AttachOutcome {
    /// The agent was attached; the socket should appear shortly.
    Attached { pid: u32 },
    /// No matching debuggable process is running.
    NoProcess,
    /// The attach failed with a reportable error.
    Failed(String),
}

/// Finds the target process, pushes the agent, and attaches it.
pub async fn attach(
    adb: &Adb,
    device: &Device,
    package: &str,
    target_process: Option<&str>,
    target_pid: Option<u32>,
    kit: &AgentKit,
) -> AttachOutcome {
    let features = match adb.device_features(device.transport_id).await {
        Ok(f) => f,
        Err(e) => return AttachOutcome::Failed(format!("cannot read device features: {e}")),
    };
    let procs = match adb.app_processes(device.transport_id, &features).await {
        Ok(p) => p,
        Err(e) => return AttachOutcome::Failed(format!("cannot list processes: {e}")),
    };

    let proc = pick_process(&procs, package, target_process, target_pid);
    let Some(proc) = proc else {
        return AttachOutcome::NoProcess;
    };
    if !proc.debuggable {
        return AttachOutcome::Failed(format!(
            "{} (pid {}) is not debuggable; attach mode requires a debuggable app",
            proc.process_name.as_deref().unwrap_or(package),
            proc.pid,
        ));
    }

    let abi = resolve_abi(adb, device, package, proc.architecture.as_deref()).await;
    let agent_bytes = match kit.agent_for_abi(&abi) {
        Some(a) => a,
        None => return AttachOutcome::Failed(format!("no agent built for ABI {abi}")),
    };

    match push_install_attach(adb, device, package, proc.pid, &abi, agent_bytes, kit).await {
        Ok(()) => AttachOutcome::Attached { pid: proc.pid },
        Err(e) => AttachOutcome::Failed(format!("{e:#}")),
    }
}

fn pick_process<'a>(
    procs: &'a [AppProcess],
    package: &str,
    target_process: Option<&str>,
    target_pid: Option<u32>,
) -> Option<&'a AppProcess> {
    let candidates: Vec<&AppProcess> = procs
        .iter()
        .filter(|p| p.package_names.iter().any(|pkg| pkg == package) || p.process_name.as_deref() == Some(package))
        .collect();

    if let Some(pid) = target_pid {
        return candidates.into_iter().find(|p| p.pid == pid);
    }
    if let Some(name) = target_process {
        return candidates.into_iter().find(|p| p.process_name.as_deref() == Some(name));
    }
    // default: the package's main process (process name == package name), or the newest pid
    candidates
        .iter()
        .find(|p| p.process_name.as_deref() == Some(package))
        .copied()
        .or_else(|| candidates.into_iter().max_by_key(|p| p.pid))
}

async fn push_install_attach(
    adb: &Adb,
    device: &Device,
    package: &str,
    pid: u32,
    abi: &str,
    agent_bytes: &[u8],
    kit: &AgentKit,
) -> anyhow::Result<()> {
    let id = device.transport_id;

    // Ensure the tmp directory exists
    let mkdir = adb.shell(id, &format!("mkdir -p {TMP_DIR}")).await?;
    if mkdir.exit != 0 {
        bail!("cannot create {TMP_DIR}: {}", String::from_utf8_lossy(&mkdir.stderr).trim());
    }

    // Push the three files to /data/local/tmp/traffic-police/
    tracing::info!(pid, abi, dir = TMP_DIR, "pushing agent artifacts");
    adb.push(id, agent_bytes, &format!("{TMP_DIR}/{AGENT_LIB}"), 0o755).await.context("pushing the agent .so")?;
    adb.push(id, &kit.boot_dex, &format!("{TMP_DIR}/{BOOT_DEX}"), 0o644).await.context("pushing the boot dex")?;
    adb.push(id, &kit.runtime_dex, &format!("{TMP_DIR}/{RUNTIME_DEX}"), 0o644)
        .await
        .context("pushing the runtime dex")?;

    // Copy into the app's code_cache via run-as
    let q = quote(package);
    let install = format!(
        "run-as {q} sh -c '\
         mkdir -p {CACHE_DIR} && \
         cp {TMP_DIR}/{AGENT_LIB} {CACHE_DIR}/{AGENT_LIB} && \
         cp {TMP_DIR}/{BOOT_DEX} {CACHE_DIR}/{BOOT_DEX} && \
         cp {TMP_DIR}/{RUNTIME_DEX} {CACHE_DIR}/{RUNTIME_DEX} && \
         chmod 555 {CACHE_DIR}/{AGENT_LIB} && \
         chmod 444 {CACHE_DIR}/{BOOT_DEX} {CACHE_DIR}/{RUNTIME_DEX}'"
    );
    let out = adb.shell(id, &install).await.context("installing into code_cache")?;
    if out.exit != 0 {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("not debuggable") || err.contains("not allowed") {
            bail!("{package} is not debuggable (run-as refused)");
        }
        bail!("copying agent to code_cache failed: {}", err.trim());
    }

    // Get the app's data directory (run-as changes to it)
    let pwd_out = adb.shell(id, &format!("run-as {q} pwd")).await?;
    let data_dir = pwd_out.stdout_text().trim().to_string();
    if data_dir.is_empty() {
        bail!("cannot determine {package}'s data directory");
    }

    // Attach the agent
    let agent_path = format!("{data_dir}/{CACHE_DIR}/{AGENT_LIB}");
    let opts = format!("dir={data_dir}/{CACHE_DIR};package={package}");
    let attach_cmd = format!("cmd activity attach-agent {} {}", pid, quote(&format!("{agent_path}={opts}")));
    tracing::info!(pid, %agent_path, "sending attach-agent");
    let out = adb.shell(id, &attach_cmd).await.context("cmd activity attach-agent")?;
    if out.exit != 0 {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("attach-agent returned exit code {}", out.exit);
        }
        bail!("attach-agent: {stderr}");
    }

    Ok(())
}

/// Waits for the target's runtime socket to appear, polling up to `SOCKET_TIMEOUT`.
pub async fn wait_for_socket(adb: &Adb, device: &Device, package: &str, pid: u32) -> Option<RuntimeSocket> {
    let deadline = tokio::time::Instant::now() + SOCKET_TIMEOUT;
    loop {
        if let Ok(sockets) = adb.runtime_sockets(device.transport_id).await
            && let Some(s) = sockets.into_iter().find(|s| s.pid == pid && s.is_for(package))
        {
            return Some(s);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Sets up a startup agent for capture from launch on API 30+. Writes `agent.conf` with an
/// expiry stamp and copies the agent .so into `code_cache/startup_agents/`.
pub async fn install_startup_agent(
    adb: &Adb,
    device: &Device,
    package: &str,
    kit: &AgentKit,
    abi: &str,
    expiry_minutes: u32,
) -> anyhow::Result<()> {
    let agent_bytes = kit.agent_for_abi(abi).context("no agent for ABI")?;
    let id = device.transport_id;
    let q = quote(package);

    // Push agent to tmp
    adb.push(id, agent_bytes, &format!("{TMP_DIR}/{AGENT_LIB}"), 0o755).await.context("pushing agent for startup")?;

    // Write agent.conf with expiry
    let now_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0i64, |d| d.as_millis() as i64);
    let expires_at = now_ms + i64::from(expiry_minutes) * 60_000;
    let conf = format!("expires_at_ms={expires_at}\npackage={package}\n");
    adb.push(id, conf.as_bytes(), &format!("{TMP_DIR}/{AGENT_CONF}"), 0o644).await.context("pushing agent.conf")?;

    // Also ensure the main artifacts are installed (for the runtime dex)
    adb.push(id, &kit.boot_dex, &format!("{TMP_DIR}/{BOOT_DEX}"), 0o644).await?;
    adb.push(id, &kit.runtime_dex, &format!("{TMP_DIR}/{RUNTIME_DEX}"), 0o644).await?;

    let install = format!(
        "run-as {q} sh -c '\
         mkdir -p {CACHE_DIR} && \
         cp {TMP_DIR}/{BOOT_DEX} {CACHE_DIR}/{BOOT_DEX} && \
         cp {TMP_DIR}/{RUNTIME_DEX} {CACHE_DIR}/{RUNTIME_DEX} && \
         cp {TMP_DIR}/{AGENT_CONF} {CACHE_DIR}/{AGENT_CONF} && \
         chmod 444 {CACHE_DIR}/{BOOT_DEX} {CACHE_DIR}/{RUNTIME_DEX} {CACHE_DIR}/{AGENT_CONF} && \
         mkdir -p code_cache/startup_agents && \
         cp {TMP_DIR}/{AGENT_LIB} code_cache/startup_agents/{AGENT_LIB} && \
         chmod 555 code_cache/startup_agents/{AGENT_LIB}'"
    );
    let out = adb.shell(id, &install).await?;
    if out.exit != 0 {
        bail!("installing startup agent failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// Removes our startup agent from `code_cache/startup_agents/` (other tools' agents are left).
pub async fn remove_startup_agent(adb: &Adb, device: &Device, package: &str) {
    let q = quote(package);
    let cmd = format!("run-as {q} rm -f code_cache/startup_agents/{AGENT_LIB} {CACHE_DIR}/{AGENT_CONF}");
    let _ = adb.shell(device.transport_id, &cmd).await;
}

/// Gets the device's API level.
pub async fn api_level(adb: &Adb, device: &Device) -> anyhow::Result<u32> {
    let out = adb.shell(device.transport_id, "getprop ro.build.version.sdk").await.context("reading API level")?;
    out.stdout_text().trim().parse().context("parsing API level")
}
