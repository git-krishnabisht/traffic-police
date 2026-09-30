//! traffic-police: watch an Android app's HTTP traffic from the terminal.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::sync::{mpsc, watch};
use traffic_police_adb::Adb;
use traffic_police_backends::demo::{DemoConfig, DemoSession, demo_rules};
use traffic_police_backends::{DeviceTarget, run_device};
use traffic_police_core::backend::{Capabilities, ConnectionStatus};
use traffic_police_core::fmt::{NS_PER_MS, NS_PER_SEC};
use traffic_police_core::store::SessionStore;
use traffic_police_core::store::spill;
use traffic_police_tui::app::{App, JqResult};
use traffic_police_tui::terminal::{self, RunOptions};
use traffic_police_tui::theme::{Depth, Palette, Theme};

/// How long a jq filter may run before it is stopped.
const JQ_TIME_LIMIT: Duration = Duration::from_secs(3);
/// Most results a filter may produce, and their total size.
const JQ_MAX_OUTPUTS: usize = 200;
const JQ_MAX_OUTPUT_BYTES: usize = 16 << 20;

#[derive(Parser)]
#[command(
    name = "traffic-police",
    version,
    about = "Watch an Android app's HTTP traffic from the terminal",
    after_help = "Without --package, traffic-police lets you choose the device and the app process."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    target: TargetArgs,
    /// Color theme.
    #[arg(long, global = true, value_enum, default_value_t = ThemeArg::Auto)]
    theme: ThemeArg,
    /// Draw images with half-blocks instead of asking the terminal for a graphics protocol.
    #[arg(long, global = true)]
    no_images: bool,
    /// Write the log here instead of the default location.
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

/// Which app to watch.
#[derive(Args, Clone)]
struct TargetArgs {
    /// Device serial, as `adb devices` lists it (optional with one device).
    #[arg(long, short = 's')]
    serial: Option<String>,
    /// The app's package name, e.g. com.example.app.
    #[arg(long, short = 'p')]
    package: Option<String>,
    /// A process name, for apps with several processes (e.g. com.example.app:sync).
    #[arg(long, conflicts_with = "pid")]
    process: Option<String>,
    /// A process id.
    #[arg(long)]
    pid: Option<u32>,
    /// How capture gets into the app: the traffic-police library in its debug build, or attach
    /// (no library; arrives in Phase 4).
    #[arg(long, value_enum, default_value_t = Mode::Library)]
    mode: Mode,
    /// Start the app first.
    #[arg(long)]
    launch: bool,
    /// Keep watching when the app restarts; each run appears as a new segment.
    #[arg(long)]
    follow: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    Library,
    Attach,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ThemeArg {
    Auto,
    Dark,
    Light,
}

#[derive(Subcommand)]
enum Command {
    /// Show the inspector with simulated traffic from a pretend app (no device needed).
    Demo(DemoArgs),
    /// Internal: run one jq filter over stdin and print the result as JSON.
    #[command(name = "__jq", hide = true)]
    Jq {
        #[arg(allow_hyphen_values = true)]
        filter: String,
    },
}

#[derive(Args)]
struct DemoArgs {
    /// Seed for the simulated traffic; the same seed gives the same traffic.
    #[arg(long, default_value_t = 7)]
    seed: u64,
    /// Speed of simulated time (2 = twice as fast).
    #[arg(long, default_value_t = 1.0)]
    speed: f64,
    /// Make the pretend app die after this many seconds and start again, to show detach and
    /// re-attach.
    #[arg(long, value_name = "SECS")]
    restart_after: Option<f64>,
    /// Print one frame as text after SECS of simulated time instead of starting the UI.
    #[arg(long, hide = true, value_name = "WxH@SECS")]
    dump_frame: Option<String>,
    /// With --dump-frame: keys to press first, e.g. "jj<Enter>l" (see traffic_police_tui::parse_keys).
    #[arg(long, hide = true, value_name = "KEYS", requires = "dump_frame")]
    keys: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Jq { filter }) => jq_child(&filter),
        Some(Command::Demo(ref args)) => {
            let theme = theme(cli.theme);
            if let Some(spec) = &args.dump_frame {
                return dump_frame(args, spec, theme);
            }
            let _log = init_logging(cli.log_file.as_deref())?;
            traffic_police_core::fmt::set_local_offset_secs(traffic_police_tui::local_utc_offset_secs());
            spill::remove_stale();
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let result = rt.block_on(run_demo(args, theme, !cli.no_images));
            spill::remove_all();
            result
        }
        None => {
            if cli.target.mode == Mode::Attach {
                eprintln!(
                    "Attach mode arrives in Phase 4. Use library mode: add the traffic-police library to the app's debug build (see the README)."
                );
                std::process::exit(2);
            }
            let theme = theme(cli.theme);
            let _log = init_logging(cli.log_file.as_deref())?;
            traffic_police_core::fmt::set_local_offset_secs(traffic_police_tui::local_utc_offset_secs());
            spill::remove_stale();
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let result = rt.block_on(run_device_mode(cli.target, theme, !cli.no_images));
            spill::remove_all();
            result
        }
    }
}

fn theme(arg: ThemeArg) -> Theme {
    let palette = match arg {
        ThemeArg::Auto => Palette::detect(),
        ThemeArg::Dark => Palette::Dark,
        ThemeArg::Light => Palette::Light,
    };
    Theme::new(palette, Depth::detect())
}

/// Log to `$XDG_STATE_HOME/traffic-police/traffic-police.log` (default
/// `~/.local/state/traffic-police`; `%LOCALAPPDATA%\traffic-police` on Windows). Nothing is
/// written to the terminal while the UI runs.
fn init_logging(path: Option<&std::path::Path>) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => default_log_dir().join("traffic-police.log"),
    };
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating the log directory {}", dir.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening the log file {}", path.display()))?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = tracing_subscriber::EnvFilter::try_from_env("TRAFFIC_POLICE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(writer).with_ansi(false).init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "traffic-police starting");
    Ok(guard)
}

fn default_log_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(d).join("traffic-police");
        }
    } else if let Some(d) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join("traffic-police");
    } else if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/traffic-police");
    }
    std::env::temp_dir().join("traffic-police")
}

fn demo_config(args: &DemoArgs, wall_start_ms: Option<i64>) -> anyhow::Result<DemoConfig> {
    if !(args.speed > 0.0 && args.speed <= 100.0) {
        bail!("--speed must be between 0 and 100");
    }
    let restart_after_ns = match args.restart_after {
        Some(s) if s > 0.0 && s < 86_400.0 => Some((s * NS_PER_SEC as f64) as u64),
        Some(_) => bail!("--restart-after must be between 0 and 86400 seconds"),
        None => None,
    };
    let mut cfg = DemoConfig { seed: args.seed, restart_after_ns, ..DemoConfig::default() };
    if let Some(w) = wall_start_ms {
        cfg.wall_start_ms = w;
    }
    Ok(cfg)
}

fn demo_app(theme: Theme) -> App {
    let mut app = App::new(SessionStore::new(), theme);
    app.caps = Capabilities { pause: true, rules: false, live: true };
    app.rules = Some(demo_rules());
    app
}

/// Apply `.traffic-police/project.toml` (nearest one at or above the working directory).
fn load_project(app: &mut App) {
    let Ok(cwd) = std::env::current_dir() else { return };
    match traffic_police_core::project::find(&cwd) {
        Ok(Some(p)) => {
            tracing::info!(root = %p.root.display(), roots = p.source_roots.len(), "project config");
            for w in &p.warnings {
                tracing::warn!("{w}");
            }
            if let Some(w) = p.warnings.first() {
                app.flash(w.clone());
            }
            app.source_roots = p.source_roots;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("{e}");
            app.flash(e.to_string());
        }
    }
}

/// Watch a real app: pick it (or take it from the flags), then stream it into the UI.
async fn run_device_mode(args: TargetArgs, theme: Theme, detect_images: bool) -> anyhow::Result<()> {
    let adb = Adb::from_env();
    adb.ensure_server().await.context("traffic-police talks to devices through the adb server")?;
    let target = match args.package.clone() {
        Some(package) => DeviceTarget {
            serial: args.serial.clone(),
            package,
            process: args.process.clone(),
            pid: args.pid,
            follow: args.follow,
        },
        None => match traffic_police_tui::picker::pick(adb.clone(), theme.clone()).await? {
            Some(p) => DeviceTarget {
                serial: Some(p.serial),
                package: p.package,
                process: Some(p.process),
                pid: None,
                follow: args.follow,
            },
            None => return Ok(()),
        },
    };
    if args.launch {
        launch_app(&adb, target.serial.as_deref(), &target.package).await?;
    }
    tracing::info!(package = %target.package, serial = ?target.serial, follow = target.follow, "device session");

    let mut app = App::new(SessionStore::new(), theme);
    app.caps = Capabilities { pause: true, rules: false, live: true };
    load_project(&mut app);
    app.jq_runner = jq_in_child;
    let ids = app.store.source_ids();
    let (event_tx, event_rx) = mpsc::channel(256);
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    app.commands = Some(command_tx);
    let (status_tx, status_rx) = watch::channel(ConnectionStatus::Waiting("looking for the device…".into()));
    let backend = tokio::spawn(run_device(adb, target, ids, event_tx, command_rx, status_tx));
    let result = terminal::run(app, event_rx, RunOptions { detect_images, status: Some(status_rx) }).await;
    // the UI dropped its command sender: the backend says goodbye and removes its forward
    let _ = tokio::time::timeout(Duration::from_secs(3), backend).await;
    result
}

/// `--launch`: start the app's launcher activity.
async fn launch_app(adb: &Adb, serial: Option<&str>, package: &str) -> anyhow::Result<()> {
    let devices = adb.devices().await?;
    let device = devices
        .iter()
        .filter(|d| d.is_online())
        .find(|d| serial.is_none_or(|s| d.serial == s))
        .context("no online device to launch the app on")?;
    let q = traffic_police_adb::quote(package);
    let resolved = adb
        .shell(
            device.transport_id,
            &format!("cmd package resolve-activity --brief -c android.intent.category.LAUNCHER {q}"),
        )
        .await?;
    let component = resolved.stdout_text().lines().last().unwrap_or("").trim().to_string();
    if !component.contains('/') {
        bail!("{package} has no launcher activity on {} (is it installed?)", device.label());
    }
    let started =
        adb.shell(device.transport_id, &format!("am start -n {}", traffic_police_adb::quote(&component))).await?;
    if started.exit != 0 {
        bail!("could not start {component}: {}", String::from_utf8_lossy(&started.stderr).trim());
    }
    Ok(())
}

async fn run_demo(args: &DemoArgs, theme: Theme, detect_images: bool) -> anyhow::Result<()> {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let cfg = demo_config(args, Some(now_ms))?;
    let mut app = demo_app(theme);
    load_project(&mut app);
    let ids = app.store.source_ids();
    let (event_tx, event_rx) = mpsc::channel(256);
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    app.commands = Some(command_tx);
    app.jq_runner = jq_in_child;
    let backend = tokio::spawn(traffic_police_backends::run_demo(cfg, args.speed, ids, event_tx, command_rx));
    let result = terminal::run(app, event_rx, RunOptions { detect_images, status: None }).await;
    backend.abort();
    result
}

/// `--dump-frame WxH@SECS`: run the demo in virtual time and print one frame.
fn dump_frame(args: &DemoArgs, spec: &str, theme: Theme) -> anyhow::Result<()> {
    let (size, secs) = spec.split_once('@').context("expected WxH@SECS, e.g. 140x40@12.5")?;
    let (w, h) = size.split_once('x').context("expected WxH@SECS, e.g. 140x40@12.5")?;
    let (w, h): (u16, u16) = (w.parse()?, h.parse()?);
    let secs: f64 = secs.parse()?;
    if !(0.0..=86_400.0).contains(&secs) || w == 0 || h == 0 {
        bail!("frame size and time must be positive");
    }
    let keys = match &args.keys {
        Some(k) => traffic_police_tui::parse_keys(k).map_err(anyhow::Error::msg)?,
        None => Vec::new(),
    };
    let mut app = demo_app(theme);
    let mut session = DemoSession::new(demo_config(args, None)?, app.store.source_ids());
    let end = (secs * NS_PER_SEC as f64) as u64;
    let step = 25 * NS_PER_MS;
    let mut t = 0;
    while t < end {
        t = (t + step).min(end);
        app.ingest(session.advance(t));
    }
    app.now_override = Some(DemoSession::clock_at(end));
    let text = traffic_police_tui::render_keys(&mut app, w, h, &keys);
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(())
}

// --- jq in a child process ----------------------------------------------------------------------
//
// A filter can loop forever or allocate without bound, and a thread cannot be stopped, so the UI
// runs each filter in a child process (`traffic-police __jq FILTER`) with a time limit.

fn jq_child(filter: &str) -> anyhow::Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let reply = match traffic_police_core::jq::run(filter, &input, JQ_MAX_OUTPUTS) {
        Ok(out) => {
            let mut total = 0;
            let mut values = Vec::new();
            let mut truncated = out.truncated;
            for v in out.values {
                total += v.len();
                if total > JQ_MAX_OUTPUT_BYTES {
                    truncated = true;
                    break;
                }
                values.push(v);
            }
            serde_json::json!({ "values": values, "truncated": truncated })
        }
        Err(e) => serde_json::json!({ "error": e.to_string() }),
    };
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &reply)?;
    out.flush()?;
    Ok(())
}

fn jq_in_child(filter: &str, input: &[u8]) -> JqResult {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the traffic-police executable: {e}"))?;
    let mut child = Process::new(exe)
        .arg("__jq")
        .arg("--")
        .arg(filter)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start the jq worker: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(JQ_MAX_OUTPUT_BYTES as u64 * 2 + 4096).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + JQ_TIME_LIMIT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the filter ran longer than {} s and was stopped", JQ_TIME_LIMIT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(format!("jq worker: {e}")),
        }
    }
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();
    let reply: serde_json::Value =
        serde_json::from_slice(&out).map_err(|_| "the jq worker stopped without an answer".to_string())?;
    if let Some(e) = reply.get("error").and_then(|e| e.as_str()) {
        return Err(e.to_string());
    }
    let values = reply
        .get("values")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let truncated = reply.get("truncated").and_then(|t| t.as_bool()).unwrap_or(false);
    Ok((values, truncated))
}
