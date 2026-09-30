//! Read-only checks against the local adb server and any online device. Ignored by default:
//! `cargo test -p traffic-police-adb --test live -- --ignored --nocapture`

use traffic_police_adb::Adb;

#[tokio::test]
#[ignore = "needs a running adb server with an online device"]
async fn inspect_devices() {
    let adb = Adb::from_env();
    println!("server version {}", adb.server_version().await.unwrap());
    println!("host features {:?}", adb.host_features().await.unwrap());
    let mut tracker = adb.track_devices().await.unwrap();
    let devices = tracker.next().await.unwrap();
    for d in &devices {
        println!("{} state={} transport_id={}", d.label(), d.state, d.transport_id);
        if !d.is_online() {
            continue;
        }
        let features = adb.device_features(d.transport_id).await.unwrap();
        println!("  features: {} (track_app: {})", features.len(), features.iter().any(|f| f == "track_app"));
        let out = adb.shell(d.transport_id, "getprop ro.build.version.sdk").await.unwrap();
        println!("  sdk {} exit {}", out.stdout_text().trim(), out.exit);
        let sockets = adb.runtime_sockets(d.transport_id).await.unwrap();
        let pids: Vec<u32> = sockets.iter().map(|s| s.pid).collect();
        let frozen = adb.frozen_pids(d.transport_id, &pids).await.unwrap();
        for s in &sockets {
            println!(
                "  runtime socket @{} (pid {}){}",
                s.name,
                s.pid,
                if frozen.contains(&s.pid) { " frozen" } else { "" }
            );
        }
        for p in adb.app_processes(d.transport_id, &features).await.unwrap().iter().take(8) {
            println!("  process {} {:?} debuggable={} arch={:?}", p.pid, p.process_name, p.debuggable, p.architecture);
        }
    }
}
