//! `traffic-police doctor` (ARCHITECTURE.md §5.12): checks what capture needs, in order, and
//! prints a fix for each failure. It only reads: nothing on the host or the device changes (the
//! adb server is not even started).

use std::fmt::Write as _;
use std::sync::Arc;

use traffic_police_adb::{Adb, AdbError, AppProcess, Device, RuntimeSocket, quote};
use traffic_police_backends::AgentKit;
use traffic_police_backends::attach::{self, ABIS};
use traffic_police_tui::share::describe;
use traffic_police_tui::theme::Depth;

use crate::config;

/// What doctor looks at: the same flags as capture.
pub struct Target {
    /// `--project`: where `.traffic-police/` is (else found from the working directory).
    pub project: Option<std::path::PathBuf>,
    pub serial: Option<String>,
    pub package: Option<String>,
    pub process: Option<String>,
    pub pid: Option<u32>,
    pub attach: bool,
    /// In attach mode: the agent it would use, or why there is none.
    pub agent: Option<anyhow::Result<Arc<AgentKit>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mark {
    Ok,
    Warn,
    Fail,
    Note,
}

/// The report as it is printed.
#[derive(Default)]
pub struct Report {
    pub text: String,
    pub failures: usize,
    pub warnings: usize,
}

impl Report {
    fn section(&mut self, title: &str) {
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        let _ = writeln!(self.text, "{title}");
    }

    fn check(&mut self, mark: Mark, what: impl AsRef<str>, fix: Option<&str>) {
        let sign = match mark {
            Mark::Ok => "✓",
            Mark::Warn => "!",
            Mark::Fail => "✗",
            Mark::Note => "·",
        };
        match mark {
            Mark::Warn => self.warnings += 1,
            Mark::Fail => self.failures += 1,
            _ => {}
        }
        let _ = writeln!(self.text, "  {sign} {}", what.as_ref());
        if let Some(fix) = fix {
            let _ = writeln!(self.text, "      fix: {fix}");
        }
    }

    fn summary(&mut self) {
        let line = match (self.failures, self.warnings) {
            (0, 0) => "All checks passed.".to_string(),
            (0, w) => format!("No problems; {w} warning{}.", plural(w)),
            (f, 0) => format!("{f} problem{} found.", plural(f)),
            (f, w) => format!("{f} problem{} and {w} warning{} found.", plural(f), plural(w)),
        };
        let _ = writeln!(self.text, "\n{line}");
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `Android Debug Bridge version 1.0.41` → 41.
fn client_version(out: &str) -> Option<u32> {
    let line = out.lines().find(|l| l.contains("Android Debug Bridge version"))?;
    line.rsplit('.').next()?.trim().parse().ok()
}

/// Runs the checks and returns the report.
pub async fn run(t: &Target, settings: &config::Loaded, adb: &Adb) -> Report {
    let mut r = Report::default();
    r.section("Host");
    let server = adb.server_version().await;
    match &server {
        Ok(v) => r.check(Mark::Ok, format!("adb server at {} answers (version {v})", adb.address()), None),
        Err(AdbError::NoServer(addr)) => r.check(
            Mark::Fail,
            format!("no adb server at {addr}"),
            Some("run `adb start-server` (traffic-police starts it when it captures), or check [adb] server and ADB_SERVER_SOCKET"),
        ),
        Err(e) => r.check(
            Mark::Fail,
            format!("the adb server at {} does not answer: {e}", adb.address()),
            Some("check [adb] server and ADB_SERVER_SOCKET; if adb itself hangs, restart it with `adb kill-server` then `adb start-server` (this also disconnects Android Studio)"),
        ),
    }
    let binary = adb.binary();
    match binary {
        Some(bin) => {
            let out = std::process::Command::new(&bin).arg("version").output();
            let version = out.as_ref().ok().and_then(|o| client_version(&String::from_utf8_lossy(&o.stdout)));
            match (version, &server) {
                (Some(c), Ok(s)) if c == *s => {
                    r.check(Mark::Ok, format!("adb binary {} is 1.0.{c}, the same as the server", bin.display()), None)
                }
                (Some(c), Ok(s)) => r.check(
                    Mark::Warn,
                    format!("adb binary {} is 1.0.{c}, the server is version {s}", bin.display()),
                    Some("an adb of another version restarts the server when used, which disconnects Android Studio and every other adb user; put the server's adb first on PATH, or set [adb] path"),
                ),
                (Some(c), Err(_)) => r.check(Mark::Ok, format!("adb binary {} is 1.0.{c}", bin.display()), None),
                (None, _) => r.check(Mark::Warn, format!("{} did not report its version", bin.display()), None),
            }
        }
        None => r.check(
            Mark::Warn,
            "no adb binary found (needed only to start the adb server)",
            Some("install the Android SDK platform-tools, or set [adb] path"),
        ),
    }
    match (&settings.path, settings.problems.is_empty()) {
        (Some(p), true) => r.check(Mark::Ok, format!("config {}", p.display()), None),
        (None, true) => {
            let where_ = config::path().map_or("(no config location)".into(), |p| p.display().to_string());
            r.check(Mark::Note, format!("no config file ({where_}); defaults apply"), None);
        }
        (path, false) => {
            let name = path.as_ref().map_or("config".into(), |p| p.display().to_string());
            r.check(
                Mark::Fail,
                format!(
                    "{name}: {} problem{}; defaults apply to them",
                    settings.problems.len(),
                    plural(settings.problems.len())
                ),
                None,
            );
            for p in &settings.problems {
                let _ = writeln!(r.text, "      {p}");
            }
        }
    }
    let start = t.project.clone().map_or_else(std::env::current_dir, Ok);
    match start.as_ref().map(|d| traffic_police_core::project::find(d)) {
        Ok(Ok(Some(p))) if p.warnings.is_empty() => {
            r.check(Mark::Ok, format!("project config in {}", p.root.join(".traffic-police").display()), None)
        }
        Ok(Ok(Some(p))) => {
            for w in &p.warnings {
                r.check(Mark::Warn, format!("project config: {w}"), None);
            }
        }
        Ok(Err(e)) => r.check(
            Mark::Fail,
            format!("project config: {e}"),
            Some("correct the file at the place named (README: Project config), or remove it"),
        ),
        _ => {}
    }
    let rules_dir = match &t.project {
        Some(p) => Some(p.join(traffic_police_core::project::DIR)),
        None => start.as_ref().ok().and_then(|d| traffic_police_core::rules::find_dir(d)),
    };
    if let Some(path) = rules_dir.map(|d| d.join(traffic_police_core::rules::FILE)).filter(|p| p.is_file()) {
        let f = traffic_police_core::rules::load(&path);
        if f.is_valid() {
            let on = f.set.rules.iter().filter(|x| x.enabled).count();
            r.check(
                Mark::Ok,
                format!("rules {}: {} rule{}, {on} on", path.display(), f.entries.len(), plural(f.entries.len())),
                None,
            );
        } else {
            r.check(
                Mark::Fail,
                format!("rules {}: {} problem{}", path.display(), f.problems.len(), plural(f.problems.len())),
                Some("fix the lines below; until then no rules apply (docs/PROTOCOL.md §8 has the format)"),
            );
            for p in &f.problems {
                let _ = writeln!(r.text, "      {p}");
            }
        }
    }
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    match crossterm::terminal::size().ok().filter(|_| tty).ok_or(()) {
        Ok((w, h)) if w >= traffic_police_tui::ui::MIN_WIDTH && h >= traffic_police_tui::ui::MIN_HEIGHT => {
            r.check(Mark::Ok, format!("terminal {w}×{h} · {}", depth_label(Depth::detect())), None)
        }
        Ok((w, h)) => r.check(
            Mark::Warn,
            format!(
                "terminal {w}×{h} is smaller than the {}×{} the UI needs",
                traffic_police_tui::ui::MIN_WIDTH,
                traffic_police_tui::ui::MIN_HEIGHT
            ),
            Some("make the window larger (headless commands work at any size)"),
        ),
        Err(_) => r.check(Mark::Note, format!("not a terminal · {}", depth_label(Depth::detect())), None),
    }
    let mode = settings.clipboard().unwrap_or_default();
    match describe(mode) {
        Ok(how) => r.check(Mark::Ok, format!("clipboard: {how}"), None),
        Err(e) => {
            r.check(Mark::Warn, format!("clipboard: {e}"), Some("install one, or set [ui] clipboard = \"osc52\""))
        }
    }
    if let Some(agent) = &t.agent {
        match agent {
            Ok(kit) => r.check(Mark::Ok, format!("agent for attach mode: {} ({})", kit.origin, ABIS.join(", ")), None),
            Err(e) => r.check(
                Mark::Fail,
                format!("agent for attach mode: {e:#}"),
                Some("build it (cd android && ./gradlew :attach-agent:agentArtifacts) or give --agent-dir; release binaries have it built in"),
            ),
        }
    }

    r.section("Devices");
    if server.is_err() {
        r.check(Mark::Note, "not checked without an adb server", None);
        r.summary();
        return r;
    }
    let devices = match adb.devices().await {
        Ok(d) => d,
        Err(e) => {
            r.check(
                Mark::Fail,
                format!("the device list failed: {e}"),
                Some("run `adb devices`; if it hangs too, restart adb with `adb kill-server` then `adb start-server` (this also disconnects Android Studio)"),
            );
            r.summary();
            return r;
        }
    };
    if devices.is_empty() {
        r.check(Mark::Fail, "no devices", Some("connect a device with USB debugging on, or start an emulator"));
    }
    let wanted: Vec<&Device> = devices.iter().filter(|d| t.serial.as_ref().is_none_or(|s| &d.serial == s)).collect();
    if let Some(s) = &t.serial
        && wanted.is_empty()
    {
        r.check(Mark::Fail, format!("no device {s}"), Some("check the serial with `adb devices`"));
    }
    let mut checked: Vec<(&Device, u32)> = Vec::new();
    for d in &wanted {
        match d.state.as_str() {
            "device" => {
                let props = adb
                    .shell(d.transport_id, "getprop ro.build.version.sdk; getprop ro.build.version.release")
                    .await
                    .map(|o| o.stdout_text())
                    .unwrap_or_default();
                let mut lines = props.lines();
                let api: Option<u32> = lines.next().and_then(|l| l.trim().parse().ok());
                let release = lines.next().unwrap_or("").trim().to_string();
                match api {
                    Some(a) if a >= 26 => {
                        r.check(Mark::Ok, format!("{} online · Android {release} (API {a})", d.label()), None);
                        checked.push((d, a));
                    }
                    Some(a) => r.check(
                        Mark::Fail,
                        format!("{} is Android {release} (API {a})", d.label()),
                        Some("traffic-police needs Android 8.0 (API 26) or newer"),
                    ),
                    None => r.check(Mark::Warn, format!("{} online; its API level did not read", d.label()), None),
                }
            }
            "unauthorized" => r.check(
                Mark::Fail,
                format!("{} is unauthorized", d.label()),
                Some("unlock the device and accept \"Allow USB debugging\""),
            ),
            "offline" => r.check(
                Mark::Fail,
                format!("{} is offline", d.label()),
                Some("reconnect the cable; for an emulator, restart it"),
            ),
            other => r.check(Mark::Fail, format!("{} is {other}", d.label()), Some("it must be in the `device` state")),
        }
    }

    let Some(package) = &t.package else {
        r.check(Mark::Note, "give --package to check an app", None);
        r.summary();
        return r;
    };
    let (device, api) = match checked.as_slice() {
        [one] => *one,
        [] => {
            r.summary();
            return r;
        }
        _ => {
            r.section(&format!("App {package}"));
            r.check(Mark::Fail, "several devices are online", Some("give --serial to choose one"));
            r.summary();
            return r;
        }
    };
    let id = device.transport_id;
    r.section(&format!("App {package} on {}", device.label()));
    let q = quote(package);
    let path = adb.shell(id, &format!("pm path {q}")).await.map(|o| o.stdout_text()).unwrap_or_default();
    if !path.contains("package:") {
        let fix = if t.attach {
            "install a debug build of the app"
        } else {
            "install a debug build of the app with the traffic-police library"
        };
        r.check(Mark::Fail, "not installed", Some(fix));
        r.summary();
        return r;
    }
    r.check(Mark::Ok, "installed", None);
    let run_as = adb.shell(id, &format!("run-as {q} id")).await;
    let disabled =
        adb.shell(id, "getprop ro.boot.disable_runas").await.map(|o| o.stdout_text().trim() == "1").unwrap_or(false);
    match &run_as {
        Ok(o) if o.exit == 0 => r.check(Mark::Ok, "debuggable (run-as works)", None),
        Ok(o) => {
            let why = format!("{}{}", String::from_utf8_lossy(&o.stderr), o.stdout_text()).trim().to_string();
            let flags = adb
                .shell(id, &format!("dumpsys package {q} | grep -m 1 -E 'pkgFlags=|flags=\\['"))
                .await
                .map(|o| o.stdout_text())
                .unwrap_or_default();
            if disabled && flags.contains("DEBUGGABLE") {
                if t.attach {
                    r.check(
                        Mark::Fail,
                        "debuggable, but run-as is disabled on this device (ro.boot.disable_runas)",
                        Some("attach mode copies the agent into the app with run-as: use another device, or library mode"),
                    );
                } else {
                    r.check(
                        Mark::Warn,
                        "debuggable, but run-as is disabled on this device (ro.boot.disable_runas)",
                        Some("library mode works without run-as; attach mode needs another device"),
                    );
                }
            } else {
                r.check(
                    Mark::Fail,
                    format!("not debuggable ({why})"),
                    Some("install a debug build (android:debuggable); capture never runs in release builds"),
                );
            }
        }
        Err(e) => r.check(Mark::Warn, format!("run-as did not run: {e}"), None),
    }
    let sockets = match adb.runtime_sockets(id).await {
        Ok(s) => {
            r.check(Mark::Ok, "/proc/net/unix is readable", None);
            s
        }
        Err(AdbError::Unreadable(why)) => {
            // the runtime's socket is found by its name instead (package and pid)
            match traffic_police_backends::device::package_sockets(adb, device, package, None).await {
                Ok(found) => {
                    r.check(
                        Mark::Ok,
                        format!("{why}: the capture runtime is looked for by its socket name instead"),
                        None,
                    );
                    found
                }
                Err(e) => {
                    r.check(
                        Mark::Fail,
                        format!("{why}, and its socket names could not be checked: {e}"),
                        Some("check that the device is still connected (adb devices), then run doctor again"),
                    );
                    Vec::new()
                }
            }
        }
        Err(e) => {
            r.check(
                Mark::Fail,
                format!("/proc/net/unix did not read: {e}"),
                Some("check that the device is still connected (adb devices), then run doctor again"),
            );
            Vec::new()
        }
    };
    let mine: Vec<u32> = sockets.iter().filter(|s| s.is_for(package)).map(|s| s.pid).collect();
    let features = adb.device_features(id).await.unwrap_or_default();
    let procs = adb.app_processes(id, &features).await.unwrap_or_default();
    let of_package: Vec<&AppProcess> = procs.iter().filter(|p| attach::of_package(p, package)).collect();
    let name_of = |pid: u32| {
        of_package.iter().find(|p| p.pid == pid).and_then(|p| p.process_name.clone()).unwrap_or_else(|| "?".into())
    };
    if t.attach {
        let run_as_works = run_as.as_ref().is_ok_and(|o| o.exit == 0);
        let running: Vec<&RuntimeSocket> = sockets.iter().filter(|s| s.is_for(package)).collect();
        attach_checks(&mut r, adb, device, api, package, t, &of_package, &running, run_as_works).await;
    } else if mine.is_empty() {
        let fix = if of_package.is_empty() {
            "start the app (or use --launch); its debug build needs the traffic-police library (README: Library mode)"
        } else {
            "the app runs, but no capture runtime answers: add the traffic-police library to its debug build (README: Library mode)"
        };
        r.check(Mark::Fail, "no capture runtime is listening", Some(fix));
    } else {
        let list: Vec<String> = mine.iter().map(|&pid| format!("{} (pid {pid})", name_of(pid))).collect();
        r.check(Mark::Ok, format!("capture runtime listening in {}", list.join(", ")), None);
        if let Some(want) = &t.process
            && !mine.iter().any(|&pid| name_of(pid) == *want)
        {
            r.check(
                Mark::Fail,
                format!("no capture runtime in process {want}"),
                Some("check --process, or start that process"),
            );
        }
        if let Some(want) = t.pid
            && !mine.contains(&want)
        {
            r.check(
                Mark::Fail,
                format!("no capture runtime in pid {want}"),
                Some("check --pid (it changes when the app restarts)"),
            );
        }
    }
    if !of_package.is_empty() {
        let list: Vec<String> = of_package
            .iter()
            .map(|p| {
                format!(
                    "{} {}{}",
                    p.process_name.as_deref().unwrap_or("?"),
                    p.pid,
                    if p.debuggable { "" } else { " (not debuggable)" }
                )
            })
            .collect();
        r.check(Mark::Note, format!("app processes: {}", list.join(", ")), None);
        let pids: Vec<u32> = of_package.iter().map(|p| p.pid).collect();
        for pid in adb.frozen_pids(id, &pids).await.unwrap_or_default() {
            r.check(
                Mark::Warn,
                format!("{} (pid {pid}) is frozen by the system (a cached app)", name_of(pid)),
                Some("bring the app to the foreground; capture resumes when it thaws"),
            );
        }
    }
    r.summary();
    r
}

/// Attach mode's checks on the device (ARCHITECTURE.md §5.12): the process's ABI, the page size,
/// `code_cache` through `run-as`, how capture from launch works here, startup agents left
/// behind, and the hooks of an agent that runs.
#[allow(clippy::too_many_arguments)]
async fn attach_checks(
    r: &mut Report,
    adb: &Adb,
    device: &Device,
    api: u32,
    package: &str,
    t: &Target,
    of_package: &[&AppProcess],
    running: &[&RuntimeSocket],
    run_as_works: bool,
) {
    let id = device.transport_id;
    let q = quote(package);
    // the Host section reported where the agent comes from
    let kit = t.agent.as_ref().and_then(|a| a.as_ref().ok());
    // the process's ABI (or the one the app will start with) needs its own agent
    let target = attach::pick_process(
        &of_package.iter().map(|p| (*p).clone()).collect::<Vec<_>>(),
        package,
        t.process.as_deref(),
        t.pid,
    )
    .cloned();
    let abi = attach::process_abi(adb, device, package, target.as_ref().and_then(|p| p.architecture.as_deref())).await;
    let who = match &target {
        Some(p) => format!("{} (pid {}) runs as {abi}", p.process_name.as_deref().unwrap_or(package), p.pid),
        None => format!("the app starts as {abi}"),
    };
    match kit.map(|k| k.agent_for_abi(&abi).is_some()) {
        Some(true) | None if ABIS.contains(&abi.as_str()) => {
            r.check(Mark::Ok, format!("{who}: the agent is built for it"), None)
        }
        _ => r.check(
            Mark::Fail,
            format!("{who}: the agent is built for {} only", ABIS.join(", ")),
            Some("use a device or emulator image with one of those ABIs, or library mode"),
        ),
    }
    let page = adb
        .shell(id, "getconf PAGE_SIZE 2>/dev/null")
        .await
        .ok()
        .and_then(|o| o.stdout_text().trim().parse::<u32>().ok())
        .unwrap_or(4096);
    r.check(Mark::Ok, format!("memory pages of {} KB: the agent is aligned for 4 and 16 KB pages", page / 1024), None);
    if run_as_works {
        let cache = adb.shell(id, &format!("run-as {q} sh -c 'test -d code_cache && test -w code_cache'")).await;
        match cache {
            Ok(o) if o.exit == 0 => r.check(Mark::Ok, "code_cache is writable through run-as", None),
            Ok(_) => r.check(
                Mark::Fail,
                "code_cache is not writable through run-as",
                Some("the agent goes into the app's code_cache: reinstall the app, or clear its data"),
            ),
            Err(e) => r.check(Mark::Warn, format!("code_cache did not check: {e}"), None),
        }
    }
    let launch = match api {
        30.. => "--launch and --follow load the agent as the app starts (a startup agent)",
        27..30 => {
            "--launch starts the app with the agent (best effort on Android 8.1 and 9); --follow attaches to restarts once they run"
        }
        _ => {
            "Android 8.0 cannot load an agent as the app starts: --launch attaches right after, so the first requests may be missed"
        }
    };
    r.check(Mark::Note, launch, None);
    if run_as_works {
        let listing = adb
            .shell(
                id,
                &format!(
                    "run-as {q} sh -c 'ls code_cache/startup_agents 2>/dev/null; echo ---; cat code_cache/traffic-police/agent.conf 2>/dev/null; echo ---; date +%s'"
                ),
            )
            .await
            .map(|o| o.stdout_text())
            .unwrap_or_default();
        let mut parts = listing.split("---\n");
        let agents: Vec<&str> = parts.next().unwrap_or("").lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let conf = parts.next().unwrap_or("");
        let now_ms = parts.next().and_then(|n| n.trim().parse::<i64>().ok()).map(|s| s * 1000);
        let expires =
            conf.lines().find_map(|l| l.strip_prefix("expires_at_ms=")).and_then(|v| v.trim().parse::<i64>().ok());
        let others: Vec<&str> = agents.iter().copied().filter(|a| *a != attach::AGENT_LIB).collect();
        if agents.contains(&attach::AGENT_LIB) {
            match (expires, now_ms) {
                (Some(e), Some(now)) if e > now => r.check(
                    Mark::Note,
                    format!(
                        "a startup agent is in the app for {} more minutes: a traffic-police with --launch or --follow runs (or ended moments ago)",
                        (e - now) / 60_000 + 1
                    ),
                    None,
                ),
                _ => r.check(
                    Mark::Warn,
                    "a startup agent is left in the app from a traffic-police that did not finish; it has expired and does nothing",
                    Some(&format!("adb -s {} shell run-as {package} rm code_cache/startup_agents/{}", device.serial, attach::AGENT_LIB)),
                ),
            }
        }
        if !others.is_empty() {
            r.check(Mark::Note, format!("other startup agents in the app (left alone): {}", others.join(", ")), None);
        }
    }
    if running.is_empty() {
        let what = if of_package.is_empty() {
            "the app is not running; start it (or use --launch) and traffic-police attaches the agent"
        } else {
            "no capture runs in the app yet; traffic-police attaches the agent when it starts"
        };
        r.check(Mark::Note, what, None);
        return;
    }
    let pids: Vec<u32> = running.iter().map(|s| s.pid).collect();
    let frozen = adb.frozen_pids(id, &pids).await.unwrap_or_default();
    for socket in running {
        if frozen.contains(&socket.pid) {
            r.check(
                Mark::Note,
                format!(
                    "capture runs in pid {}, which Android froze (cached): its hooks read once it runs",
                    socket.pid
                ),
                None,
            );
            continue;
        }
        match traffic_police_backends::peek_hello(adb, device, socket).await {
            Ok(hello) => {
                r.check(
                    Mark::Ok,
                    format!(
                        "capture runs in {} (pid {}): {} mode, runtime {}",
                        hello.app.process, hello.app.pid, hello.runtime.mode, hello.runtime.version
                    ),
                    None,
                );
                for h in &hello.hooks {
                    let hits = h.hits.unwrap_or(0);
                    let what = format!(
                        "hook {}: {}{}{}",
                        h.target.as_deref().unwrap_or(&h.id),
                        h.status.replace('_', " "),
                        if hits > 0 { format!(" · {hits} call{}", plural(hits as usize)) } else { String::new() },
                        h.detail.as_deref().map_or(String::new(), |d| format!(" ({d})")),
                    );
                    let mark = match h.status.as_str() {
                        "installed" => Mark::Ok,
                        "pending" => Mark::Note,
                        _ => Mark::Warn,
                    };
                    let fix = match h.status.as_str() {
                        "method_not_found" | "class_not_found" => Some(
                            "a minified (R8) build renames OkHttp; keep okhttp3.** in the debug build, or use library mode",
                        ),
                        "other_loader" => Some("a second OkHttp copy in another class loader is not captured"),
                        _ => None,
                    };
                    r.check(mark, what, fix);
                }
            }
            Err(e) => {
                r.check(Mark::Warn, format!("the capture socket in pid {} did not answer: {e}", socket.pid), None)
            }
        }
    }
}

fn depth_label(d: Depth) -> &'static str {
    match d {
        Depth::TrueColor => "24-bit color",
        Depth::Ansi256 => "256 colors",
        Depth::Ansi16 => "16 colors",
        Depth::Mono => "no color (NO_COLOR)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adb_client_versions() {
        let out = "Android Debug Bridge version 1.0.41\nVersion 35.0.2-12147458\nInstalled as /opt/adb\n";
        assert_eq!(client_version(out), Some(41));
        assert_eq!(client_version("nothing"), None);
    }

    #[test]
    fn marks_count_and_fixes_are_indented() {
        let mut r = Report::default();
        r.section("Host");
        r.check(Mark::Ok, "fine", None);
        r.check(Mark::Fail, "broken", Some("mend it"));
        r.check(Mark::Warn, "odd", None);
        r.summary();
        assert_eq!((r.failures, r.warnings), (1, 1));
        assert_eq!(
            r.text,
            "Host\n  ✓ fine\n  ✗ broken\n      fix: mend it\n  ! odd\n\n1 problem and 1 warning found.\n"
        );
    }
}
