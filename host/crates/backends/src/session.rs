//! A device session's captures (ARCHITECTURE.md §5.5): the first app's, then each app picked in
//! the session (`A`). One capture runs at a time and the UI's commands go to it; the latest pause
//! and rules carry over to the next app's. The next capture starts once the one before has said
//! goodbye and removed its forward, and Logdawg's reader is told the new app, whose uid is then
//! `package:mine`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use traffic_police_adb::Adb;
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::{BackendCommand, ConnectionStatus};
use traffic_police_core::session::StreamSink;
use traffic_police_core::store::SourceIds;

use crate::{DeviceTarget, LogdawgTarget, run_device};

/// How long a capture has to say goodbye before the next app's starts anyway.
const GOODBYE: Duration = Duration::from_secs(3);

/// Runs the session's captures until the UI goes (its command sender drops) or says
/// `Shutdown`. `switches` brings the apps picked in the session; `logdawg`, when the log is read,
/// is told each one.
#[allow(clippy::too_many_arguments)]
pub async fn run_session(
    adb: Adb,
    mut target: DeviceTarget,
    ids: SourceIds,
    events: mpsc::Sender<Vec<SessionEvent>>,
    mut commands: mpsc::UnboundedReceiver<BackendCommand>,
    mut switches: mpsc::UnboundedReceiver<DeviceTarget>,
    status: watch::Sender<ConnectionStatus>,
    log: Option<Arc<dyn StreamSink>>,
    logdawg: Option<watch::Sender<LogdawgTarget>>,
) {
    // the latest pause and rules, for the next app's capture
    let mut carried: Vec<BackendCommand> = Vec::new();
    let mut switching = true;
    loop {
        let (tx, rx) = mpsc::unbounded_channel();
        for c in &carried {
            let _ = tx.send(c.clone());
        }
        let mut capture = tokio::spawn(run_device(
            adb.clone(),
            target.clone(),
            ids.clone(),
            events.clone(),
            rx,
            status.clone(),
            log.clone(),
        ));
        let mut running = true;
        let next = loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    // the UI went: the capture says goodbye and removes its forward
                    None | Some(BackendCommand::Shutdown) => {
                        let _ = tx.send(BackendCommand::Shutdown);
                        drop(tx);
                        if running {
                            let _ = capture.await;
                        }
                        return;
                    }
                    Some(c) => {
                        if matches!(c, BackendCommand::SetRecording(_) | BackendCommand::SetRules(_)) {
                            carried.retain(|k| std::mem::discriminant(k) != std::mem::discriminant(&c));
                            carried.push(c.clone());
                        }
                        let _ = tx.send(c);
                    }
                },
                t = switches.recv(), if switching => match t {
                    Some(t) => break t,
                    None => switching = false,
                },
                // the capture ended by itself (the app exited, a failure): the UI shows why, and
                // another app can still be picked
                _ = &mut capture, if running => running = false,
            }
        };
        tracing::info!(from = %target.package, to = %next.package, serial = ?next.serial, "another app");
        let _ = tx.send(BackendCommand::Shutdown);
        drop(tx);
        if running && tokio::time::timeout(GOODBYE, &mut capture).await.is_err() {
            capture.abort();
        }
        if let Some(l) = &logdawg {
            l.send_modify(|l| {
                l.serial = next.serial.clone();
                l.package = Some(next.package.clone());
            });
        }
        target = next;
    }
}
