//! A session's captures against the fake adb (ARCHITECTURE.md §5.5): another app picked in the
//! session takes over the capture, with the pause carried over, Logdawg's reader told, and one
//! forward at a time.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use traffic_police_backends::{DeviceTarget, LogdawgTarget, run_session};
use traffic_police_core::SessionEvent;
use traffic_police_core::backend::{BackendCommand, ConnectionStatus};
use traffic_police_core::store::SessionStore;
use traffic_police_fakeadb::{FakeAdb, FakeRuntime};

const SERIAL: &str = "fake-1";
const SHOP: &str = "com.example.shop";
const OTHER: &str = "com.example.other";

/// Applies events until `done` holds; fails after 10 s with the status and the requests.
async fn until(
    store: &mut SessionStore,
    events: &mut mpsc::Receiver<Vec<SessionEvent>>,
    status: &watch::Receiver<ConnectionStatus>,
    what: &str,
    done: impl Fn(&SessionStore) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done(store) {
        let batch = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for {what}; status {:?}; {:?}", *status.borrow(), paths(store))
            })
            .expect("the session runs");
        for e in batch {
            store.apply(e);
        }
    }
}

fn paths(store: &SessionStore) -> Vec<String> {
    store.txns().iter().map(|t| t.url.path.clone()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn another_app_picked_in_the_session_takes_over_the_capture() {
    let fake = FakeAdb::start().await;
    fake.add_device(SERIAL, 36);
    let shop = FakeRuntime::new(SHOP, SHOP, 4312);
    shop.request("GET", "http://api.example.com/shop", 200);
    fake.start_app(SERIAL, &shop);
    let other = FakeRuntime::new(OTHER, OTHER, 5100);
    other.request("GET", "http://api.example.com/other", 200);
    fake.start_app(SERIAL, &other);

    let mut store = SessionStore::new();
    let (event_tx, mut events) = mpsc::channel(256);
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (switch, switches) = mpsc::unbounded_channel();
    let (status_tx, status) = watch::channel(ConnectionStatus::Waiting(String::new()));
    let (log_tx, log_rx) = watch::channel(LogdawgTarget {
        serial: Some(SERIAL.into()),
        package: Some(SHOP.into()),
        ..LogdawgTarget::default()
    });
    let target = DeviceTarget { serial: Some(SERIAL.into()), package: SHOP.into(), ..DeviceTarget::default() };
    let task = tokio::spawn(run_session(
        fake.client(),
        target.clone(),
        store.source_ids(),
        event_tx,
        command_rx,
        switches,
        status_tx,
        None,
        Some(log_tx),
    ));
    until(&mut store, &mut events, &status, "the shop's request", |s| paths(s) == ["/shop"]).await;
    assert_eq!(fake.forwards().len(), 1);

    // paused, then another app: its capture starts paused too, and the shop's ends
    commands.send(BackendCommand::SetRecording(false)).unwrap();
    switch.send(DeviceTarget { package: OTHER.into(), process: Some(OTHER.into()), ..target }).unwrap();
    until(&mut store, &mut events, &status, "the other app's request", |s| {
        paths(s) == ["/shop", "/other"] && s.sources().any(|src| src.package == SHOP && src.ended.is_some())
    })
    .await;
    assert_eq!(other.acks()[0]["config"]["recording"], false, "the pause carried over");
    assert_eq!(log_rx.borrow().package.as_deref(), Some(OTHER), "the log follows the app");
    assert_eq!(fake.forwards().len(), 1, "the shop's forward went: {:?}", fake.forwards());
    let current = store.current_source().expect("a source");
    assert_eq!((current.package.as_str(), current.ended.is_none()), (OTHER, true));

    // the UI goes: the capture says goodbye and removes its forward
    drop(commands);
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the session ends").unwrap();
    assert!(fake.forwards().is_empty(), "{:?}", fake.forwards());
}
