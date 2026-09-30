//! Event sources for traffic-police. Phase 0 has the demo device; device connections, session
//! files and HAR import follow in later phases, all producing the same
//! [`traffic_police_core::SessionEvent`] batches.

pub mod demo;
pub mod device;

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::BackendCommand;
use traffic_police_core::store::SourceIds;

pub use demo::{DemoConfig, DemoSession};
pub use device::{DeviceStatus, DeviceTarget, run_device};

/// Run the demo device in real time (scaled by `speed`) until the event receiver is dropped or
/// `Shutdown` arrives.
pub async fn run_demo(
    cfg: DemoConfig,
    speed: f64,
    ids: SourceIds,
    events: mpsc::Sender<Vec<SessionEvent>>,
    mut commands: mpsc::UnboundedReceiver<BackendCommand>,
) {
    let mut session = DemoSession::new(cfg, ids);
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
