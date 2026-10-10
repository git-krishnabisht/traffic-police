//! Event sources for traffic-police: the demo device and live devices over adb (library and
//! attach mode), all producing the same [`traffic_police_core::SessionEvent`] batches.

pub mod attach;
pub mod demo;
pub mod device;
pub mod flutter;
pub mod logdawg;
pub mod session;

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::BackendCommand;
use traffic_police_core::store::SourceIds;

pub use attach::AgentKit;
pub use demo::{DemoConfig, DemoSession};
pub use device::{DeviceTarget, Launch, peek_hello, run_device};
pub use flutter::run_flutter;
pub use logdawg::{LogdawgControl, LogdawgTarget, run_logdawg};
pub use session::run_session;

/// Run the demo device in real time (scaled by `speed`) until the event receiver is dropped or
/// `Shutdown` arrives.
pub async fn run_demo(
    cfg: DemoConfig,
    speed: f64,
    ids: SourceIds,
    events: mpsc::Sender<Vec<SessionEvent>>,
    mut commands: mpsc::UnboundedReceiver<BackendCommand>,
    log: Option<std::sync::Arc<dyn traffic_police_core::session::StreamSink>>,
) {
    let mut session = DemoSession::new(cfg, ids);
    session.log = log;
    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(25));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let elapsed = (start.elapsed().as_nanos() as f64 * speed) as u64;
                let batch = session.advance(elapsed);
                if !batch.is_empty() && events.send(batch).await.is_err() {
                    return;
                }
            }
            cmd = commands.recv() => match cmd {
                Some(BackendCommand::SetRecording(on)) => session.set_recording(on),
                Some(BackendCommand::Shutdown) | None => return,
                Some(_) => {}
            },
        }
    }
}
