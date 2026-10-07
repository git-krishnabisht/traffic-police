//! The device backend against a fake adb server and fake capture runtimes (ARCHITECTURE.md §8):
//! connecting and streaming, protocol mismatch, `--follow` across an app restart, resuming the
//! same process after the adb server restarts, a device that hides its socket list, and stale
//! forwards. No device or adb binary is needed.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use traffic_police_backends::{DeviceTarget, run_device};
use traffic_police_core::backend::{BackendCommand, ConnectionStatus};
use traffic_police_core::model::SourceId;
use traffic_police_core::store::SessionStore;
use traffic_police_fakeadb::{FakeAdb, FakeRuntime};
use traffic_police_proto::PROTOCOL_VERSION;

const SERIAL: &str = "fake-1";
const PACKAGE: &str = "com.example.shop";

struct Harness {
    store: SessionStore,
    events: mpsc::Receiver<Vec<traffic_police_core::SessionEvent>>,
    status: watch::Receiver<ConnectionStatus>,
    commands: Option<mpsc::UnboundedSender<BackendCommand>>,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    fn start(fake: &FakeAdb, target: DeviceTarget) -> Harness {
        let store = SessionStore::new();
        let (event_tx, events) = mpsc::channel(256);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (status_tx, status) = watch::channel(ConnectionStatus::Waiting(String::new()));
        let task =
            tokio::spawn(run_device(fake.client(), target, store.source_ids(), event_tx, cmd_rx, status_tx, None));
        Harness { store, events, status, commands: Some(cmd_tx), task }
    }

    /// Applies events until `done` holds; fails after 10 s with the status and what arrived.
    async fn until(&mut self, what: &str, done: impl Fn(&SessionStore, &ConnectionStatus) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if done(&self.store, &self.status.borrow()) {
                return;
            }
            tokio::select! {
                batch = self.events.recv() => match batch {
                    Some(batch) => {
                        for e in batch {
                            self.store.apply(e);
                        }
                    }
                    None => {
                        assert!(done(&self.store, &self.status.borrow()), "the backend ended before {what}: {:?}", *self.status.borrow());
                        return;
                    }
                },
                _ = self.status.changed() => {}
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timed out waiting for {what}; status {:?}; {} requests", *self.status.borrow(), self.store.len());
                }
            }
        }
    }

    /// Quits as the UI does, and waits for the backend to finish.
    async fn quit(mut self) -> SessionStore {
        if let Some(c) = self.commands.take() {
            let _ = c.send(BackendCommand::Shutdown);
        }
        tokio::time::timeout(Duration::from_secs(5), &mut self.task).await.expect("the backend did not stop").unwrap();
        while let Ok(batch) = self.events.try_recv() {
            for e in batch {
                self.store.apply(e);
            }
        }
        self.store
    }
}

fn target() -> DeviceTarget {
    DeviceTarget { serial: Some(SERIAL.into()), package: PACKAGE.into(), ..DeviceTarget::default() }
}

fn paths(store: &SessionStore, source: Option<SourceId>) -> Vec<String> {
    store.txns().iter().filter(|t| source.is_none_or(|s| t.key.source == s)).map(|t| t.url.path.clone()).collect()
}

/// Every request so far has finished (its last event arrived).
fn finished(store: &SessionStore, n: usize) -> bool {
    store.len() == n && store.txns().iter().all(|t| !t.state.is_open())
}

fn live(s: &ConnectionStatus) -> bool {
    matches!(s, ConnectionStatus::Live(_))
}

#[tokio::test(flavor = "multi_thread")]
async fn connects_streams_and_cleans_up() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 36);
    let app = FakeRuntime::new(PACKAGE, PACKAGE, 4312);
    app.request("GET", "http://api.example.com/before", 200);
    fake.start_app(SERIAL, &app);
    let mut h = Harness::start(&fake, target());
    // what the app did before the connection is replayed, what it does after streams
    h.until("the replayed request", |s, st| live(st) && paths(s, None) == ["/before"]).await;
    app.request("POST", "http://api.example.com/after", 201);
    h.until("the live request", |s, _| finished(s, 2)).await;
    assert_eq!(fake.forwards().len(), 1, "one forward while connected");
    let store = h.quit().await;
    assert_eq!(paths(&store, None), ["/before", "/after"]);
    assert!(store.txns().iter().all(|t| t.resp.is_some() && !t.state.is_open()));
    assert!(fake.forwards().is_empty(), "the forward is removed at the end: {:?}", fake.forwards());
    let ack = &app.acks()[0];
    assert_eq!(ack["protocol"], PROTOCOL_VERSION);
    assert_eq!(ack["resume_after_seq"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_of_another_protocol_fails_with_both_versions() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 36);
    let app = FakeRuntime::new(PACKAGE, PACKAGE, 4312).with_protocol(PROTOCOL_VERSION + 1);
    fake.start_app(SERIAL, &app);
    let mut h = Harness::start(&fake, target());
    h.until("the failure", |_, st| matches!(st, ConnectionStatus::Failed(_))).await;
    let ConnectionStatus::Failed(msg) = h.status.borrow().clone() else { unreachable!() };
    assert!(msg.contains(&PROTOCOL_VERSION.to_string()) && msg.contains(&(PROTOCOL_VERSION + 1).to_string()), "{msg}");
    h.quit().await;
    assert!(fake.forwards().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn follows_the_app_across_a_restart() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 36);
    let first = FakeRuntime::new(PACKAGE, PACKAGE, 100);
    first.request("GET", "http://api.example.com/one", 200);
    fake.start_app(SERIAL, &first);
    let mut h = Harness::start(&fake, DeviceTarget { follow: true, ..target() });
    h.until("the first run", |s, _| s.len() == 1).await;
    fake.kill_process(SERIAL, 100);
    h.until("the first run to end", |s, _| s.sources().any(|x| x.pid == 100 && x.ended.is_some())).await;
    let second = FakeRuntime::new(PACKAGE, PACKAGE, 200);
    second.request("GET", "http://api.example.com/two", 200);
    fake.start_app(SERIAL, &second);
    h.until("the second run", |s, st| live(st) && s.len() == 2).await;
    let store = h.quit().await;
    let sources: Vec<(u32, Vec<String>)> = store.sources().map(|s| (s.pid, paths(&store, Some(s.id)))).collect();
    assert_eq!(sources, [(100, vec!["/one".to_string()]), (200, vec!["/two".to_string()])]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_follow_an_exit_detaches_and_keeps_the_data() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 36);
    let app = FakeRuntime::new(PACKAGE, PACKAGE, 100);
    app.request("GET", "http://api.example.com/one", 200);
    fake.start_app(SERIAL, &app);
    let mut h = Harness::start(&fake, target());
    h.until("the request", |s, _| s.len() == 1).await;
    fake.kill_process(SERIAL, 100);
    h.until("detached", |_, st| matches!(st, ConnectionStatus::Detached(_))).await;
    let store = h.quit().await;
    assert_eq!(paths(&store, None), ["/one"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_process_resumes_after_the_adb_server_restarts() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 36);
    let app = FakeRuntime::new(PACKAGE, PACKAGE, 4312);
    app.request("GET", "http://api.example.com/a", 200);
    fake.start_app(SERIAL, &app);
    let mut h = Harness::start(&fake, target());
    h.until("the first request", |s, st| live(st) && finished(s, 1)).await;
    // every connection drops and transport ids start again; the app keeps running
    fake.stop();
    tokio::time::sleep(Duration::from_millis(400)).await;
    app.request("GET", "http://api.example.com/b", 200);
    fake.restart().await;
    h.until("the request made while adb was down", |s, st| live(st) && finished(s, 2)).await;
    let store = h.quit().await;
    // nothing twice: the host asked to resume after what it had
    assert_eq!(paths(&store, None), ["/a", "/b"]);
    let acks = app.acks();
    assert_eq!(acks.len(), 2, "{acks:?}");
    assert_eq!(acks[1]["resume_after_seq"], 4, "the first request's four events were not sent again");
    assert!(fake.forwards().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_hides_its_socket_list_is_probed_by_name() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 36);
    fake.set_unix_readable(SERIAL, false);
    // a debuggable process of another app, and two of this app: one without the runtime
    fake.start_process(SERIAL, 50, "com.example.other");
    fake.start_process(SERIAL, 99, PACKAGE);
    let app = FakeRuntime::new(PACKAGE, &format!("{PACKAGE}:sync"), 4312);
    app.request("GET", "http://api.example.com/found", 200);
    fake.start_app(SERIAL, &app);
    let mut h = Harness::start(&fake, DeviceTarget { process: Some(format!("{PACKAGE}:sync")), ..target() });
    h.until("the request", |s, st| live(st) && s.len() == 1).await;
    let store = h.quit().await;
    assert_eq!(paths(&store, None), ["/found"]);
    assert!(fake.commands(SERIAL).iter().any(|c| c == "cat /proc/net/unix"));
    assert!(fake.forwards().is_empty(), "probes remove their forwards: {:?}", fake.forwards());

    // the probe itself: a missing socket closes at once, a runtime speaks first
    let adb = fake.client();
    let id = adb.devices().await.unwrap()[0].transport_id;
    assert!(adb.probe_abstract(id, &app.socket_name()).await.unwrap());
    assert!(!adb.probe_abstract(id, &traffic_police_adb::socket_name(PACKAGE, 99)).await.unwrap());
    assert!(fake.forwards().is_empty());
    let e = adb.runtime_sockets(id).await.unwrap_err();
    assert!(e.to_string().contains("Permission denied"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_left_by_a_killed_traffic_police_are_removed() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 36);
    let adb = fake.client();
    let id = adb.devices().await.unwrap()[0].transport_id;
    // a forward to a capture socket that is gone, and one that is not ours
    adb.forward(id, &format!("localabstract:{}", traffic_police_adb::socket_name(PACKAGE, 7))).await.unwrap();
    adb.forward(id, "localabstract:AndroidStudioTransport").await.unwrap();
    let app = FakeRuntime::new(PACKAGE, PACKAGE, 4312);
    fake.start_app(SERIAL, &app);
    let mut h = Harness::start(&fake, target());
    h.until("the connection", |_, st| live(st)).await;
    let remotes: Vec<String> = fake.forwards().into_iter().map(|(_, _, r)| r).collect();
    assert!(!remotes.iter().any(|r| r.ends_with("_7")), "{remotes:?}");
    assert!(remotes.iter().any(|r| r == "localabstract:AndroidStudioTransport"), "{remotes:?}");
    h.quit().await;
    let remotes: Vec<String> = fake.forwards().into_iter().map(|(_, _, r)| r).collect();
    assert_eq!(remotes, ["localabstract:AndroidStudioTransport"], "only the forward that is not ours stays");
}

#[tokio::test(flavor = "multi_thread")]
async fn waits_for_a_device_and_for_the_app() {
    let fake = FakeAdb::start().await;
    let mut h = Harness::start(&fake, target());
    h.until(
        "waiting for the device",
        |_, st| matches!(st, ConnectionStatus::Waiting(m) if m.contains("not connected")),
    )
    .await;
    fake.add_device(SERIAL, 36);
    h.until("waiting for the app", |_, st| matches!(st, ConnectionStatus::Waiting(m) if m.contains(PACKAGE))).await;
    let app = FakeRuntime::new(PACKAGE, PACKAGE, 4312);
    app.request("GET", "http://api.example.com/late", 200);
    fake.start_app(SERIAL, &app);
    h.until("the app", |s, st| live(st) && s.len() == 1).await;
    h.quit().await;
}
