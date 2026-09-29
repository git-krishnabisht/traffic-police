//! Shared helpers: a demo session replayed in virtual time into an [`App`].

#![allow(dead_code)]

use traffic_police_backends::demo::{DemoConfig, DemoSession, demo_rules};
use traffic_police_core::backend::Capabilities;
use traffic_police_core::fmt::{NS_PER_MS, NS_PER_SEC};
use traffic_police_core::model::Transaction;
use traffic_police_core::store::SessionStore;
use traffic_police_tui::{App, Theme, parse_keys, render_keys, render_text};

pub const SMALL: (u16, u16) = (100, 30);
pub const MEDIUM: (u16, u16) = (140, 40);
pub const LARGE: (u16, u16) = (200, 50);

/// The demo after `secs` of simulated time, fed in 25 ms steps as the live runner does.
pub fn app_with(secs: f64, cfg: DemoConfig, theme: Theme) -> App {
    let mut app = App::new(SessionStore::new(), theme);
    app.caps = Capabilities { pause: true, rules: false, live: true };
    app.rules = Some(demo_rules());
    let mut session = DemoSession::new(cfg, app.store.source_ids());
    let end = (secs * NS_PER_SEC as f64) as u64;
    let mut t = 0;
    while t < end {
        t = (t + 25 * NS_PER_MS).min(end);
        app.ingest(session.advance(t));
    }
    app.now_override = Some(DemoSession::clock_at(end));
    app
}

pub fn app_at(secs: f64) -> App {
    app_with(secs, DemoConfig::default(), Theme::default())
}

/// Keys that move the list cursor to the first row matching `pred`.
pub fn goto(app: &mut App, size: (u16, u16), pred: impl Fn(&Transaction) -> bool) -> String {
    render_text(app, size.0, size.1);
    let store = app.view_store();
    let pos = app.view_rows().rows().iter().position(|r| pred(store.txn(r.txn()))).expect("no row matches");
    format!("g{}", "j".repeat(pos))
}

pub fn press(app: &mut App, size: (u16, u16), keys: &str) -> String {
    render_keys(app, size.0, size.1, &parse_keys(keys).expect("key script"))
}

/// Frame after `secs`, pressing `keys`.
pub fn frame(secs: f64, size: (u16, u16), keys: &str) -> String {
    let mut app = app_at(secs);
    press(&mut app, size, keys)
}

/// Frame after `secs` with the detail pane open on the first request matching `pred`, then
/// pressing `keys`.
pub fn detail(secs: f64, size: (u16, u16), pred: impl Fn(&Transaction) -> bool, keys: &str) -> String {
    let mut app = app_at(secs);
    let to = goto(&mut app, size, pred);
    press(&mut app, size, &format!("{to}<Enter>{keys}"))
}

pub fn path_is(p: &'static str) -> impl Fn(&Transaction) -> bool {
    move |t| t.url.path == p
}

pub fn path_ends(p: &'static str) -> impl Fn(&Transaction) -> bool {
    move |t| t.url.path.ends_with(p)
}
