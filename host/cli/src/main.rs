//! traffic-police: watch an Android app's HTTP traffic from the terminal.

mod config;
mod doctor;
mod headless;

/// The attach-mode agent, when the build embedded it (`TRAFFIC_POLICE_EMBED_AGENT`, see build.rs).
mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded_agent.rs"));
}

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::sync::{mpsc, watch};
use traffic_police_backends::demo::{DemoConfig, DemoSession, demo_rules};
use traffic_police_backends::{AgentKit, DeviceTarget, Launch, run_device};
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
    #[arg(long, global = true, value_enum)]
    theme: Option<ThemeArg>,
    /// Draw images with half-blocks instead of asking the terminal for a graphics protocol.
    #[arg(long, global = true)]
    no_images: bool,
    /// Write the log here instead of the default location.
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<PathBuf>,
    /// The project whose .traffic-police/ (rules.toml, project.toml) to use; by default the
    /// nearest one at or above the working directory.
    #[arg(long, global = true, value_name = "DIR")]
    project: Option<PathBuf>,
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
    /// How capture gets into the app: the traffic-police library in its debug build (the
    /// default with --package), or attach (any debuggable app, no library needed). Without
    /// --package, the picker attaches to a process that has no library.
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    /// Attach mode: the directory with the agent (./gradlew :attach-agent:agentArtifacts). By
    /// default the one built into this binary, else the build output in the source tree.
    #[arg(long, value_name = "DIR", env = "TRAFFIC_POLICE_AGENT_DIR")]
    agent_dir: Option<PathBuf>,
    /// Start the app first. In attach mode it is restarted if it runs, and the agent loads as
    /// it starts (Android 8.1 and newer), so its start-up requests are captured.
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
    /// Open a saved session (.trafficpolice) or a HAR file.
    Open {
        /// The file to open.
        file: PathBuf,
    },
    /// Print each finished request as a line of JSON (PROTOCOL.md Appendix B), until Ctrl+C.
    #[command(after_help = "Example: traffic-police tail -p com.example.app status:4xx,5xx | jq .url")]
    Tail(TailArgs),
    /// Record a session file (.trafficpolice) without the UI, until Ctrl+C or --duration.
    #[command(after_help = "Example: traffic-police record -p com.example.app --out login.trafficpolice --duration 2m")]
    Record(RecordArgs),
    /// Check the host, the device and the app, and say how to fix what is wrong (nothing is
    /// changed). Exits with 1 when a check fails.
    #[command(after_help = "Example: traffic-police doctor -p com.example.app")]
    Doctor {
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Write requests as a HAR file: from a saved session, or captured live for --duration.
    #[command(
        after_help = "Examples:\n  traffic-police export --har out.har --input login.trafficpolice host:api.example.com\n  traffic-police export --har out.har -p com.example.app --duration 60s"
    )]
    Export(ExportArgs),
    /// Internal: run one jq filter over stdin and print the result as JSON.
    #[command(name = "__jq", hide = true)]
    Jq {
        #[arg(allow_hyphen_values = true)]
        filter: String,
    },
}

#[derive(Args)]
struct TailArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// One JSON object per line (PROTOCOL.md Appendix B) instead of text.
    #[arg(long)]
    json: bool,
    /// Print every captured message as the app sent it (JSON), instead of a line per request.
    #[arg(long, conflicts_with = "filter")]
    events: bool,
    /// With --json: include bodies (text, or base64). With --events: include body bytes (base64).
    #[arg(long)]
    bodies: bool,
    /// Stop after this long (like 90s, 5m or 1h); otherwise Ctrl+C stops.
    #[arg(long, value_name = "TIME", value_parser = headless::parse_duration)]
    duration: Option<Duration>,
    /// Watch the pretend app of `demo` instead of a device.
    #[arg(long, hide = true)]
    demo: bool,
    /// Only requests that match, in the filter bar's language (e.g. host:api.example.com status:5xx).
    #[arg(value_name = "FILTER", trailing_var_arg = true, allow_hyphen_values = true)]
    filter: Vec<String>,
}

#[derive(Args)]
struct RecordArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// The session file to write.
    #[arg(long, short = 'o', value_name = "FILE")]
    out: PathBuf,
    /// Stop after this long (like 90s, 5m or 1h); otherwise Ctrl+C stops.
    #[arg(long, value_name = "TIME", value_parser = headless::parse_duration)]
    duration: Option<Duration>,
    /// Keep only requests that match, in the filter bar's language (quote it: --filter "host:api.* status:5xx").
    #[arg(long, value_name = "FILTER")]
    filter: Option<String>,
    /// Record the pretend app of `demo` instead of a device.
    #[arg(long, hide = true)]
    demo: bool,
}

#[derive(Args)]
struct ExportArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// The HAR file to write.
    #[arg(long, value_name = "FILE")]
    har: PathBuf,
    /// A saved session (.trafficpolice) or HAR file to read, instead of capturing live.
    #[arg(long, short = 'i', value_name = "FILE")]
    input: Option<PathBuf>,
    /// With a live capture: how long to capture (like 60s).
    #[arg(long, value_name = "TIME", value_parser = headless::parse_duration, conflicts_with = "input")]
    duration: Option<Duration>,
    /// Keep only requests that match, in the filter bar's language (quote it: --filter "host:api.* status:5xx").
    #[arg(long, value_name = "FILTER")]
    filter: Option<String>,
    /// Capture the pretend app of `demo` instead of a device.
    #[arg(long, hide = true, conflicts_with = "input")]
    demo: bool,
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
    if let Some(Command::Jq { filter }) = &cli.command {
        return jq_child(filter);
    }
    let settings = config::load();
    settings.apply_storage();
    let theme_arg = cli.theme.or_else(|| settings.theme().and_then(|t| ThemeArg::from_str(t, true).ok()));
    let theme = theme(theme_arg.unwrap_or(ThemeArg::Auto));
    let images = !cli.no_images && settings.images();
    let log_file = cli.log_file.clone();
    match cli.command {
        Some(Command::Jq { .. }) => unreachable!("handled above"),
        Some(Command::Open { ref file }) => {
            ui_run(log_file.as_deref(), run_open(file, theme, images, &settings, cli.project.as_deref()))
        }
        Some(Command::Tail(args)) => headless_run(log_file.as_deref(), &settings, async {
            let filter = headless::parse_filter(&args.filter.join(" "))?;
            let source =
                headless_source(&args.target, &cli.target, args.demo, "tail", &settings, cli.project.as_deref())
                    .await?;
            let format = match (args.events, args.json) {
                (true, _) => headless::TailFormat::Events { bodies: args.bodies },
                (false, true) => headless::TailFormat::Json { bodies: args.bodies },
                (false, false) if args.bodies => bail!("--bodies needs --json (or --events)"),
                (false, false) => headless::TailFormat::Text,
            };
            let lines = std::sync::Arc::new(headless::Lines::new(Box::new(std::io::stdout())));
            headless::tail(source, filter, format, args.duration, lines).await
        }),
        Some(Command::Record(args)) => headless_run(log_file.as_deref(), &settings, async {
            let filter = headless::parse_filter(args.filter.as_deref().unwrap_or(""))?;
            let source =
                headless_source(&args.target, &cli.target, args.demo, "record", &settings, cli.project.as_deref())
                    .await?;
            headless::record(source, &args.out, args.duration, filter).await
        }),
        Some(Command::Export(args)) => headless_run(log_file.as_deref(), &settings, async {
            let filter = headless::parse_filter(args.filter.as_deref().unwrap_or(""))?;
            let input = match (&args.input, args.duration) {
                (Some(file), _) => {
                    if args.target.package.is_some() || cli.target.package.is_some() {
                        bail!("give either --input or a target (--package), not both");
                    }
                    headless::ExportInput::File(file)
                }
                (None, Some(d)) => headless::ExportInput::Live(
                    Box::new(
                        headless_source(
                            &args.target,
                            &cli.target,
                            args.demo,
                            "export",
                            &settings,
                            cli.project.as_deref(),
                        )
                        .await?,
                    ),
                    d,
                ),
                (None, None) => bail!("give --input FILE, or capture live with --package and --duration (like 60s)"),
            };
            headless::export_har(&args.har, input, filter).await
        }),
        Some(Command::Doctor { ref target }) => {
            let attach = target.mode.or(cli.target.mode) == Some(Mode::Attach);
            let t = doctor::Target {
                project: cli.project.clone(),
                serial: target.serial.clone().or_else(|| cli.target.serial.clone()),
                package: target.package.clone().or_else(|| cli.target.package.clone()),
                process: target.process.clone().or_else(|| cli.target.process.clone()),
                pid: target.pid.or(cli.target.pid),
                attach,
                agent: attach.then(|| load_agent_kit(target.agent_dir.as_deref().or(cli.target.agent_dir.as_deref()))),
            };
            let adb = settings.adb();
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            let report = rt.block_on(doctor::run(&t, &settings, &adb));
            print!("{}", report.text);
            std::process::exit(i32::from(report.failures > 0));
        }
        Some(Command::Demo(ref args)) => {
            if let Some(spec) = &args.dump_frame {
                return dump_frame(args, spec, theme);
            }
            ui_run(log_file.as_deref(), run_demo(args, theme, images, &settings, cli.project.as_deref()))
        }
        None => ui_run(
            log_file.as_deref(),
            run_device_mode(cli.target.clone(), theme, images, &settings, cli.project.as_deref()),
        ),
    }
}

/// Runs the UI: logging to the log file, spilled bodies cleaned up after.
fn ui_run(log_file: Option<&Path>, work: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    let _log = init_logging(log_file)?;
    traffic_police_core::fmt::set_local_offset_secs(traffic_police_tui::local_utc_offset_secs());
    spill::remove_stale();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = rt.block_on(work);
    spill::remove_all();
    result
}

/// Runs a command without the UI: logging to the log file, spilled bodies cleaned up after.
fn headless_run(
    log_file: Option<&Path>,
    settings: &config::Loaded,
    work: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    for p in &settings.problems {
        eprintln!("traffic-police: config.toml: {p}");
    }
    let _log = init_logging(log_file)?;
    traffic_police_core::fmt::set_local_offset_secs(traffic_police_tui::local_utc_offset_secs());
    spill::remove_stale();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = rt.block_on(async {
        headless::stop_on_signals();
        work.await
    });
    // the backend may still be saying goodbye; it has had its chance
    rt.shutdown_timeout(Duration::from_secs(1));
    spill::remove_all();
    result
}

/// What a command without the UI watches. It has no picker, so --package is needed; target
/// flags may come before the command too (`traffic-police -p com.example.app tail`).
async fn headless_source(
    args: &TargetArgs,
    outer: &TargetArgs,
    demo: bool,
    command: &str,
    settings: &config::Loaded,
    project: Option<&Path>,
) -> anyhow::Result<headless::Source> {
    if demo {
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        return Ok(headless::Source::Demo(DemoConfig { wall_start_ms: now_ms, ..DemoConfig::default() }, 1.0));
    }
    let Some(package) = args.package.clone().or_else(|| outer.package.clone()) else {
        bail!("{command} needs the app: --package com.example.app (the UI lets you pick instead)");
    };
    let rules = ProjectRules::find(project);
    if let Some(s) = rules.summary() {
        headless::note(s);
    }
    let agent_kit = if args.mode.or(outer.mode) == Some(Mode::Attach) {
        Some(load_agent_kit(args.agent_dir.as_deref().or(outer.agent_dir.as_deref()))?)
    } else {
        None
    };
    let target = DeviceTarget {
        serial: args.serial.clone().or_else(|| outer.serial.clone()),
        package,
        process: args.process.clone().or_else(|| outer.process.clone()),
        pid: args.pid.or(outer.pid),
        follow: args.follow || outer.follow,
        capture: settings.capture(),
        rules: rules.active(),
        attach: agent_kit,
        launch: (args.launch || outer.launch).then(Launch::default),
    };
    let adb = settings.adb();
    tracing::info!(command, package = %target.package, serial = ?target.serial, follow = target.follow, attach = target.attach.is_some(), "headless session");
    Ok(headless::Source::Device(Box::new(target), adb, rules.path()))
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

/// The project's rules (`.traffic-police/rules.toml`), read at start; the UI and the commands
/// without it then watch the file.
struct ProjectRules {
    dir: Option<PathBuf>,
    file: Option<traffic_police_core::rules::RulesFile>,
}

impl ProjectRules {
    fn find(project: Option<&Path>) -> ProjectRules {
        let dir = match project {
            Some(p) => Some(p.join(traffic_police_core::project::DIR)),
            None => std::env::current_dir().ok().and_then(|d| traffic_police_core::rules::find_dir(&d)),
        };
        let file = dir.as_ref().map(|d| traffic_police_core::rules::load(&d.join(traffic_police_core::rules::FILE)));
        ProjectRules { dir, file }
    }

    /// The file to watch, when the project has a `.traffic-police` directory.
    fn path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(traffic_police_core::rules::FILE))
    }

    /// What the app gets: the file's rules when they are valid, else none.
    fn active(&self) -> traffic_police_proto::msg::RuleSet {
        self.file.as_ref().filter(|f| f.is_valid()).map_or_else(
            || traffic_police_proto::msg::RuleSet { version: "none".into(), rules: Vec::new() },
            |f| f.set.clone(),
        )
    }

    /// For commands without the UI: what applies, or why nothing does.
    fn summary(&self) -> Option<String> {
        let f = self.file.as_ref().filter(|f| !f.entries.is_empty() || !f.problems.is_empty())?;
        Some(if f.is_valid() {
            let on = f.set.rules.iter().filter(|r| r.enabled).count();
            format!("rules: {on} of {} on, from {}", f.entries.len(), f.path.display())
        } else {
            let list: Vec<String> = f.problems.iter().map(ToString::to_string).collect();
            format!("rules: none apply; {} has problems:\n  {}", f.path.display(), list.join("\n  "))
        })
    }

    fn apply(self, app: &mut App) {
        app.rules = Some(self.active());
        if let Some(f) = self.file.as_ref().filter(|f| !f.is_valid()) {
            app.flash(format!("rules.toml: {} (no rules apply until it is fixed)", f.problems[0]));
        }
        app.rules_file = self.file;
        app.rules_dir = self.dir;
    }
}

/// The agent for attach mode: from `--agent-dir` (or `TRAFFIC_POLICE_AGENT_DIR`), else the one
/// built into this binary (release builds), else the Android build output of the source tree
/// this binary was built from, or under the working directory.
fn load_agent_kit(dir: Option<&Path>) -> anyhow::Result<Arc<AgentKit>> {
    if let Some(d) = dir {
        return AgentKit::from_dir(d).map(Arc::new).with_context(|| format!("the agent in {}", d.display()));
    }
    if let Some([arm64, arm, x86_64, boot, runtime]) = embedded::AGENT {
        return Ok(Arc::new(AgentKit::embedded(boot, runtime, [arm64, arm, x86_64])));
    }
    const OUTPUT: &str = "android/attach-agent/build/outputs/agent";
    let candidates = [
        // host/cli → the repository root
        Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(OUTPUT)),
        std::env::current_dir().ok().map(|d| d.join(OUTPUT)),
    ];
    for d in candidates.into_iter().flatten().filter_map(|d| d.canonicalize().ok()) {
        match AgentKit::from_dir(&d) {
            Ok(kit) => {
                tracing::info!(dir = %d.display(), "the agent for attach mode");
                return Ok(Arc::new(kit));
            }
            Err(e) => tracing::debug!(dir = %d.display(), "not a complete agent directory: {e}"),
        }
    }
    bail!(
        "attach mode needs the agent, and this build has none: build it with \
         `cd android && ./gradlew :attach-agent:agentArtifacts`, or give --agent-dir"
    )
}

/// Apply `.traffic-police/project.toml` (nearest one at or above the working directory).
fn load_project(app: &mut App, project: Option<&Path>) {
    let start = match project {
        Some(p) => p.to_path_buf(),
        None => match std::env::current_dir() {
            Ok(d) => d,
            Err(_) => return,
        },
    };
    match traffic_police_core::project::find(&start) {
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
async fn run_device_mode(
    args: TargetArgs,
    theme: Theme,
    detect_images: bool,
    settings: &config::Loaded,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let adb = settings.adb();
    adb.ensure_server().await.context("traffic-police talks to devices through the adb server")?;
    let rules = ProjectRules::find(project);
    let launch = args.launch.then(Launch::default);
    let target = match args.package.clone() {
        Some(package) => DeviceTarget {
            serial: args.serial.clone(),
            package,
            process: args.process.clone(),
            pid: args.pid,
            follow: args.follow,
            capture: settings.capture(),
            rules: rules.active(),
            attach: match args.mode {
                Some(Mode::Attach) => Some(load_agent_kit(args.agent_dir.as_deref())?),
                _ => None,
            },
            launch,
        },
        None => {
            // without --mode, a process with no library can be picked when the agent is there
            let agent_kit = match (args.mode, &args.agent_dir) {
                (Some(Mode::Library), _) => Err(
                    "Add it to the app's debug build (see the README), or start traffic-police without --mode library to attach the agent instead."
                        .to_string(),
                ),
                (Some(Mode::Attach), _) | (None, Some(_)) => Ok(load_agent_kit(args.agent_dir.as_deref())?),
                (None, None) => load_agent_kit(None).map_err(|_| {
                    "Add it to the app's debug build (see the README), or build the agent to attach to it: cd android && ./gradlew :attach-agent:agentArtifacts."
                        .to_string()
                }),
            };
            let offer = agent_kit.as_ref().map(|_| ()).map_err(Clone::clone);
            match traffic_police_tui::picker::pick(adb.clone(), theme.clone(), offer).await? {
                Some(p) => DeviceTarget {
                    serial: Some(p.serial),
                    package: p.package,
                    process: Some(p.process),
                    pid: None,
                    follow: args.follow,
                    capture: settings.capture(),
                    rules: rules.active(),
                    // a process that runs capture already is watched as it is, unless attach
                    // mode was asked for (then --follow attaches to its restarts)
                    attach: agent_kit.ok().filter(|_| !p.capturing || args.mode == Some(Mode::Attach)),
                    launch,
                },
                None => return Ok(()),
            }
        }
    };
    tracing::info!(package = %target.package, serial = ?target.serial, follow = target.follow, attach = target.attach.is_some(), "device session");

    let mut app = App::new(SessionStore::new(), theme);
    app.caps = Capabilities { pause: true, rules: true, live: true };
    settings.apply_ui(&mut app);
    load_project(&mut app, project);
    rules.apply(&mut app);
    app.jq_runner = jq_in_child;
    let ids = app.store.source_ids();
    let (event_tx, event_rx) = mpsc::channel(256);
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    app.commands = Some(command_tx);
    let (status_tx, status_rx) = watch::channel(ConnectionStatus::Waiting("looking for the device…".into()));
    let log = session_log();
    app.session_log = log.clone();
    let sink = log.map(|l| l as std::sync::Arc<dyn traffic_police_core::session::StreamSink>);
    let backend = tokio::spawn(run_device(adb, target, ids, event_tx, command_rx, status_tx, sink));
    let ui = catch_unwind(terminal::run(app, event_rx, RunOptions { detect_images, status: Some(status_rx) })).await;
    // the UI dropped its command sender (a panic too): the backend says goodbye and removes its
    // forward
    let _ = tokio::time::timeout(Duration::from_secs(3), backend).await;
    ui.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Runs `f` to its end or to a panic, so that what has to follow (the backend's goodbye) still
/// happens; the caller carries the panic on.
async fn catch_unwind<F: Future>(f: F) -> std::thread::Result<F::Output> {
    let mut f = std::pin::pin!(f);
    std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(v)) => std::task::Poll::Ready(Ok(v)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    })
    .await
}

async fn run_demo(
    args: &DemoArgs,
    theme: Theme,
    detect_images: bool,
    settings: &config::Loaded,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let cfg = demo_config(args, Some(now_ms))?;
    let mut app = demo_app(theme);
    settings.apply_ui(&mut app);
    load_project(&mut app, project);
    let ids = app.store.source_ids();
    let (event_tx, event_rx) = mpsc::channel(256);
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    app.commands = Some(command_tx);
    app.jq_runner = jq_in_child;
    let log = session_log();
    app.session_log = log.clone();
    let sink = log.map(|l| l as std::sync::Arc<dyn traffic_police_core::session::StreamSink>);
    let backend = tokio::spawn(traffic_police_backends::run_demo(cfg, args.speed, ids, event_tx, command_rx, sink));
    let result = terminal::run(app, event_rx, RunOptions { detect_images, status: None }).await;
    backend.abort();
    result
}

/// `open FILE`: a saved session, replayed into the UI.
async fn run_open(
    file: &Path,
    theme: Theme,
    detect_images: bool,
    settings: &config::Loaded,
    project: Option<&Path>,
) -> anyhow::Result<()> {
    let mut app = App::new(SessionStore::new(), theme);
    app.caps = Capabilities { pause: false, rules: false, live: false };
    settings.apply_ui(&mut app);
    load_project(&mut app, project);
    app.jq_runner = jq_in_child;
    let name = file.file_name().map_or_else(|| file.display().to_string(), |n| n.to_string_lossy().into_owned());
    let opened = traffic_police_core::import::open(file, &app.store.source_ids())
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", file.display()))?;
    for s in &opened.skipped {
        tracing::warn!("{}: {s}", file.display());
    }
    app.ingest(opened.events);
    for key in opened.pins {
        if let Some(i) = app.store.find(key) {
            app.store.set_pinned(i, true);
        }
    }
    app.replay = Some(match (opened.truncated, opened.skipped.len()) {
        (true, _) => format!("{name} (the recording was cut short)"),
        (false, 0) => name,
        (false, n) => format!("{name} ({n} entr{} left out; see the log)", if n == 1 { "y" } else { "ies" }),
    });
    // time stands still at the end of the recording
    app.now_override = Some(app.store.latest());
    let (_event_tx, event_rx) = mpsc::channel(1);
    terminal::run(app, event_rx, RunOptions { detect_images, status: None }).await
}

/// A recording of the captured stream in a private temporary directory, so the session can be
/// saved from the UI (`e`). Without one (no writable temp dir), saving a session is not offered.
fn session_log() -> Option<std::sync::Arc<traffic_police_core::session::SessionLog>> {
    match traffic_police_core::session::SessionLog::temporary() {
        Ok(log) => Some(std::sync::Arc::new(log)),
        Err(e) => {
            tracing::warn!("no session recording: {e}");
            None
        }
    }
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
    // a rendering tool: copying must not reach the real clipboard
    app.clipboard = traffic_police_tui::share::ClipboardMode::Off;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_panic_in_the_ui_still_lets_the_backend_say_goodbye() {
        let (commands, mut backend) = mpsc::unbounded_channel::<()>();
        let ui = catch_unwind(async move {
            let _commands = commands;
            tokio::task::yield_now().await;
            panic!("a bug in the UI");
        })
        .await;
        assert!(ui.is_err());
        // the command sender went with the UI, so the backend sees the end and says goodbye
        assert!(backend.recv().await.is_none());
    }
}
