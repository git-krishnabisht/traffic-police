//! Input handling and state transitions: keys, mouse, commands to the backend, freeze, clear,
//! colors under NO_COLOR, and a random-input smoke test.

mod common;

use common::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use tokio::sync::mpsc;
use traffic_police_backends::demo::{DemoConfig, DemoSession};
use traffic_police_core::backend::BackendCommand;
use traffic_police_core::fmt::NS_PER_SEC;
use traffic_police_core::rows::Column;
use traffic_police_tui::app::{Focus, Target};
use traffic_police_tui::theme::{Depth, Palette};
use traffic_police_tui::{App, Theme, render_text, ui};

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE }
}

fn click(app: &mut App, x: u16, y: u16) {
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x, y));
}

#[test]
fn quit_keys_work_everywhere() {
    let mut app = app_at(5.0);
    press(&mut app, MEDIUM, "q");
    assert!(app.should_quit);
    let mut app = app_at(5.0);
    press(&mut app, MEDIUM, "?");
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.should_quit, "Ctrl+C quits even with an overlay open");
}

#[test]
fn focus_cycles_through_open_panes() {
    let mut app = app_at(5.0);
    assert_eq!(app.focus, Focus::List);
    press(&mut app, MEDIUM, "<Tab>");
    assert_eq!(app.focus, Focus::Graph);
    press(&mut app, MEDIUM, "<Tab>");
    assert_eq!(app.focus, Focus::List);
    press(&mut app, MEDIUM, "<Enter>");
    assert_eq!(app.focus, Focus::Detail);
    press(&mut app, MEDIUM, "<Tab>");
    assert_eq!(app.focus, Focus::Graph);
    press(&mut app, MEDIUM, "<S-Tab>");
    assert_eq!(app.focus, Focus::Detail);
}

#[test]
fn esc_closes_the_detail_then_clears_the_range() {
    let mut app = app_at(40.0);
    press(&mut app, MEDIUM, "vhhhhhv");
    assert!(app.graph.selection.is_some());
    press(&mut app, MEDIUM, "j<Enter>");
    assert!(app.detail_open);
    press(&mut app, MEDIUM, "<Esc>");
    assert!(!app.detail_open);
    assert!(app.graph.selection.is_some());
    press(&mut app, MEDIUM, "<Esc>");
    assert!(app.graph.selection.is_none());
}

#[test]
fn range_selection_filters_rows() {
    let mut app = app_at(40.0);
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let all = app.view_rows().len();
    press(&mut app, MEDIUM, "vhhhhhhhhhhv");
    let (a, b) = app.graph.selection.expect("range");
    let rows = app.view_rows().len();
    assert!(rows > 0 && rows < all, "{rows} of {all}");
    let store = app.view_store();
    for r in app.view_rows().rows() {
        let t = store.txn(r.txn());
        assert!(t.start <= b && t.end.unwrap_or(u64::MAX) >= a);
    }
}

#[test]
fn space_pauses_and_resumes_the_device() {
    let mut app = app_at(5.0);
    let (tx, mut rx) = mpsc::unbounded_channel();
    app.commands = Some(tx);
    let markers = app.store.markers().len();
    press(&mut app, MEDIUM, "<Space>");
    assert!(!app.recording);
    assert!(matches!(rx.try_recv(), Ok(BackendCommand::SetRecording(false))));
    press(&mut app, MEDIUM, "<Space>");
    assert!(matches!(rx.try_recv(), Ok(BackendCommand::SetRecording(true))));
    assert_eq!(app.store.markers().len(), markers + 2);
}

#[test]
fn freeze_keeps_the_view_while_capture_continues() {
    let mut app = app_at(5.0);
    let mut session = DemoSession::new(DemoConfig::default(), app.store.source_ids());
    for t in 1..=200 {
        app.ingest(session.advance(t * 25_000_000));
    }
    press(&mut app, MEDIUM, "F");
    let shown = app.view_store().len();
    for t in 201..=600 {
        app.ingest(session.advance(t * 25_000_000));
    }
    assert!(app.store.len() > shown, "capture continued");
    assert_eq!(app.view_store().len(), shown, "view frozen");
    assert!(app.frozen_events().unwrap() > 0);
    press(&mut app, MEDIUM, "F");
    assert_eq!(app.view_store().len(), app.store.len());
}

#[test]
fn clearing_asks_first_and_keeps_the_connection() {
    let mut app = app_at(12.0);
    let n = app.store.len();
    press(&mut app, MEDIUM, "xn");
    assert_eq!(app.store.len(), n);
    press(&mut app, MEDIUM, "xy");
    assert_eq!(app.store.len(), 0);
    assert!(app.store.current_source().is_some(), "the app is still connected");
}

#[test]
fn list_follows_new_rows_until_the_user_moves() {
    let mut app = app_at(5.0);
    let mut session = DemoSession::new(DemoConfig::default(), app.store.source_ids());
    // re-create the same timeline so later advances add new rows
    for t in 1..=200 {
        app.ingest(session.advance(t * 25_000_000));
    }
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let last = app.view_rows().len() - 1;
    assert_eq!(app.list_cursor, last);
    press(&mut app, MEDIUM, "kk");
    let picked = app.selected;
    for t in 201..=400 {
        app.ingest(session.advance(t * 25_000_000));
    }
    app.now_override = Some(DemoSession::clock_at(10 * NS_PER_SEC));
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    assert_eq!(app.selected, picked, "selection stays put");
    press(&mut app, MEDIUM, "L");
    assert_eq!(app.list_cursor, app.view_rows().len() - 1);
}

#[test]
fn clicking_a_header_sorts_and_double_click_opens() {
    let mut app = app_at(12.0);
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let h = app.hits.rect_of(Target::ListHeader(Column::Size)).expect("Size header");
    click(&mut app, h.x + 1, h.y);
    assert_eq!(app.rows.sort.column, Column::Size);
    click(&mut app, h.x + 1, h.y);
    assert!(app.rows.sort.descending);
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let r = app.hits.rect_of(Target::ListRow(2)).expect("row 2");
    click(&mut app, r.x + 3, r.y);
    click(&mut app, r.x + 3, r.y);
    assert!(app.detail_open);
    assert_eq!(app.list_cursor, 2);
}

#[test]
fn dragging_the_divider_and_the_graph() {
    let mut app = app_at(40.0);
    press(&mut app, MEDIUM, "<Enter>");
    let d = app.hits.rect_of(Target::Divider).expect("divider");
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), d.x, d.y + 2));
    app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 56, d.y + 2));
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 56, d.y + 2));
    assert_eq!(app.split_pct, 40);
    let text = render_text(&mut app, MEDIUM.0, MEDIUM.1);
    assert!(text.lines().nth(13).unwrap().chars().nth(56) == Some('│'), "{text}");

    let g = app.hits.rect_of(Target::Graph).expect("graph");
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), g.x + 20, g.y + 2));
    app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), g.x + 60, g.y + 2));
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), g.x + 60, g.y + 2));
    let (a, b) = app.graph.selection.expect("range");
    assert!(b > a);
}

#[test]
fn wheel_scrolls_the_list() {
    let mut app = app_at(40.0);
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let r = app.hits.rect_of(Target::ListRow(app.list_offset)).expect("row");
    let before = app.list_offset;
    app.handle_mouse(mouse(MouseEventKind::ScrollUp, r.x + 2, r.y));
    assert_eq!(app.list_offset, before - 3);
}

#[test]
fn stale_jq_results_are_ignored() {
    let mut app = app_at(12.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("{to}<Enter>l|.ok"));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let jobs = app.take_jq_jobs();
    assert_eq!(jobs.len(), 1);
    // the user edits the filter before the result arrives
    press(&mut app, MEDIUM, "|<BS><BS>");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let job = jobs.into_iter().next().unwrap();
    app.finish_jq(job, Ok((vec!["true".into()], false)));
    let text = render_text(&mut app, MEDIUM.0, MEDIUM.1);
    assert!(!text.contains("jq filter:"), "{text}");
}

#[test]
fn monochrome_theme_emits_no_colors() {
    let mut app = app_with(40.0, DemoConfig::default(), Theme::new(Palette::Dark, Depth::Mono));
    let mut term = Terminal::new(TestBackend::new(MEDIUM.0, MEDIUM.1)).unwrap();
    for keys in ["", "<Enter>", "<Enter>l", "2", "3", "?"] {
        let mut a = app_with(12.0, DemoConfig::default(), Theme::new(Palette::Light, Depth::Mono));
        press(&mut a, MEDIUM, keys);
        term.draw(|f| ui::draw(f, &mut a)).unwrap();
        for cell in &term.backend().buffer().content {
            assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset), "keys {keys:?}: {cell:?}");
        }
    }
    term.draw(|f| ui::draw(f, &mut app)).unwrap();
}

#[test]
fn tiny_terminals_do_not_panic() {
    for (w, h) in [(1, 1), (2, 1), (20, 5), (99, 29), (100, 30), (400, 120)] {
        let mut app = app_at(12.0);
        render_text(&mut app, w, h);
        press(&mut app, (w, h), "<Enter>l2?<Esc>3");
    }
}

#[test]
fn random_input_does_not_panic() {
    // xorshift, fixed seed: the same sequence every run
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let keys: Vec<KeyEvent> = "jkhlgG123cCsSvtT+-0LFx?pPo[]|<>Rq"
        .chars()
        .filter(|&c| c != 'q')
        .map(key)
        .chain(
            [
                KeyCode::Enter,
                KeyCode::Esc,
                KeyCode::Tab,
                KeyCode::BackTab,
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::PageUp,
                KeyCode::PageDown,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Backspace,
            ]
            .into_iter()
            .map(|k| KeyEvent::new(k, KeyModifiers::NONE)),
        )
        .collect();
    for size in [SMALL, MEDIUM] {
        let mut app = app_at(40.0);
        let mut session = DemoSession::new(DemoConfig::default(), app.store.source_ids());
        let mut t = 0u64;
        render_text(&mut app, size.0, size.1);
        for step in 0..3000 {
            let r = next();
            if r % 5 == 0 {
                let (x, y) = ((next() % u64::from(size.0)) as u16, (next() % u64::from(size.1)) as u16);
                let kind = match next() % 5 {
                    0 => MouseEventKind::Down(MouseButton::Left),
                    1 => MouseEventKind::Up(MouseButton::Left),
                    2 => MouseEventKind::Drag(MouseButton::Left),
                    3 => MouseEventKind::ScrollDown,
                    _ => MouseEventKind::ScrollUp,
                };
                app.handle_mouse(mouse(kind, x, y));
            } else {
                app.handle_key(keys[(r % keys.len() as u64) as usize]);
            }
            app.run_jobs_inline();
            if step % 7 == 0 {
                t += 100_000_000;
                app.ingest(session.advance(t));
            }
            render_text(&mut app, size.0, size.1);
        }
    }
}

#[test]
fn incremental_sort_matches_a_full_sort() {
    use traffic_police_core::rows::Sort;
    use traffic_police_core::store::SessionStore;
    for (column, descending) in
        [(Column::Size, true), (Column::Time, false), (Column::Name, false), (Column::Status, true)]
    {
        let mut app = App::new(SessionStore::new(), Theme::default());
        let mut session = DemoSession::new(DemoConfig::default(), app.store.source_ids());
        app.rows.set_sort(Sort { column, descending });
        for step in 1..=160u64 {
            let t = step * 250_000_000; // 40 s in quarter seconds
            app.ingest(session.advance(t));
            let now = DemoSession::clock_at(t);
            app.now_override = Some(now);
            app.refresh();
            let store = app.view_store();
            let mut expected: Vec<u32> = (0..store.len() as u32).collect();
            let key = |i: u32| {
                let x = store.txn(i);
                let k: (u64, String) = match column {
                    Column::Size => (x.response_size(), String::new()),
                    Column::Time => (x.duration(now), String::new()),
                    Column::Name => (0, x.url.name().to_lowercase()),
                    _ => (
                        x.status().map(u64::from).unwrap_or(if x.failure.is_some() { 1000 } else { 999 }),
                        String::new(),
                    ),
                };
                (k, x.start, x.key.txn)
            };
            expected.sort_by(|&a, &b| if descending { key(b).cmp(&key(a)) } else { key(a).cmp(&key(b)) });
            let got: Vec<u32> = app.view_rows().rows().iter().map(|r| r.txn()).collect();
            assert_eq!(got, expected, "{column:?} at step {step}");
        }
    }
}

#[test]
fn keys_between_frames_see_the_new_order() {
    // sort, then jump to the top, with no frame drawn in between
    let mut app = app_at(40.0);
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    for c in ['s', 's', 'S', 'g'] {
        app.handle_key(key(c));
    }
    let top = app
        .store
        .txns()
        .iter()
        .enumerate()
        .max_by_key(|(i, t)| (t.response_size(), t.start, std::cmp::Reverse(*i)))
        .unwrap()
        .0;
    assert_eq!(app.selected, Some(top as u32), "the largest response is selected");
}

#[test]
fn enter_on_an_app_frame_asks_for_the_editor() {
    let root = std::env::temp_dir().join(format!("tp-src-test-{}", std::process::id()));
    // SessionManager.kt lives outside its package directory, as Kotlin allows
    let file = root.join("sdk/session/SessionManager.kt");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "class SessionManager\n").unwrap();
    let mut app = app_at(12.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("{to}<Enter>lll<Down><Down><Down><Down><Enter>"));
    assert!(app.editor_request.is_none(), "no source roots configured yet");
    app.source_roots = vec![root.clone()];
    press(&mut app, MEDIUM, "<Enter>");
    let (path, line) = app.editor_request.take().expect("editor request");
    assert_eq!(path, file);
    assert!(line > 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn shift_wheel_pans_the_graph_and_returns_to_live() {
    let mut app = app_at(40.0);
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let g = app.hits.rect_of(Target::Graph).expect("graph");
    let wheel = |kind| MouseEvent { kind, column: g.x + 5, row: g.y + 1, modifiers: KeyModifiers::SHIFT };
    app.handle_mouse(wheel(MouseEventKind::ScrollUp));
    assert!(!app.is_live(), "moved back in time");
    let (_, right) = app.window();
    assert!(right < app.now());
    app.handle_mouse(mouse(MouseEventKind::ScrollRight, g.x + 5, g.y + 1));
    assert!(app.is_live(), "back at the live edge");
    // the wheel alone still zooms
    let span = app.graph.span;
    app.handle_mouse(mouse(MouseEventKind::ScrollDown, g.x + 5, g.y + 1));
    assert!(app.graph.span > span);
}
