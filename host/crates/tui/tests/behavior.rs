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
use traffic_police_tui::app::{Focus, Overlay, Target};
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
    let row = app.list_offset + 2;
    let r = app.hits.rect_of(Target::ListRow(row)).expect("the third visible row");
    click(&mut app, r.x + 3, r.y);
    click(&mut app, r.x + 3, r.y);
    assert!(app.detail_open);
    assert_eq!(app.list_cursor, row);
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
    // the detail box now starts at column 56, where its left border meets the list's right one
    let text = render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let row = text.lines().nth(14).unwrap();
    assert_eq!(row.chars().nth(55), Some('│'), "{text}");
    assert_eq!(row.chars().nth(56), Some('│'), "{text}");

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

#[test]
fn the_filter_applies_as_typed_and_esc_restores_it() {
    let mut app = app_at(40.0);
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let all = app.view_rows().matched();
    press(&mut app, MEDIUM, "/method:POST<Enter>");
    let posts = app.view_rows().matched();
    assert!(posts > 0 && posts < all, "{posts} of {all}");
    let store = app.view_store();
    assert!(app.view_rows().rows().iter().all(|r| store.txn(r.txn()).method == "POST"));
    // a broken edit keeps the previous filter; Esc goes back to it
    press(&mut app, MEDIUM, "/ status:9");
    press(&mut app, MEDIUM, "x");
    assert!(app.filter_error.is_some());
    press(&mut app, MEDIUM, "<Esc>");
    assert!(app.filter_error.is_none());
    assert_eq!(app.view_rows().filter.as_ref().map(|f| f.source.as_str()), Some("method:POST"));
    assert_eq!(app.view_rows().matched(), posts);
    // an empty filter clears it
    press(&mut app, MEDIUM, &format!("/{}<Enter>", "<BS>".repeat("method:POST".len())));
    assert_eq!(app.view_rows().matched(), all);
}

#[test]
fn pins_mark_rows_and_filter_with_is_pinned() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    let text = press(&mut app, MEDIUM, &format!("{to}m"));
    let init = app.selected.expect("selected");
    assert!(app.view_store().txn(init).pinned);
    assert!(text.contains("★ init"), "{text}");
    press(&mut app, MEDIUM, "/is:pinned<Enter>");
    assert_eq!(app.view_rows().matched(), 1);
    press(&mut app, MEDIUM, "m");
    assert!(!app.view_store().txn(init).pinned);
}

#[test]
fn d_on_two_requests_compares_them() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    let text = press(&mut app, MEDIUM, &format!("{to}d"));
    assert!(text.contains("◆ init"), "{text}");
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/monitor"));
    let text = press(&mut app, MEDIUM, &format!("{to}d"));
    assert_eq!(app.overlay, Overlay::Diff);
    assert!(text.contains("A POST https://deepid.example.app/api/sdk/init"), "{text}");
    assert!(text.contains("B GET https://deepid.example.app/api/sdk/monitor"), "{text}");
    assert!(text.contains("differs in request,"), "{text}");
    assert!(text.contains("- POST https://deepid.example.app/api/sdk/init"), "{text}");
    // sorted headers, then the next change
    let text = press(&mut app, MEDIUM, "sn");
    assert!(text.contains("headers compared as sets"), "{text}");
    assert!(app.diff.as_ref().unwrap().scroll.line > 0);
    let text = press(&mut app, MEDIUM, "<Esc>");
    assert_eq!(app.overlay, Overlay::None);
    assert!(!text.contains("◆"), "the mark is used up: {text}");
    // d twice on one request clears the mark
    press(&mut app, MEDIUM, "dd");
    assert_eq!((app.overlay, app.diff_mark), (Overlay::None, None));
}

#[test]
fn enter_on_a_value_decodes_it_or_filters_by_it() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    let init = app.selected.expect("selected");
    // the Overview shows the bearer token; Enter on its row decodes it
    let doc = traffic_police_tui::detail::build_doc(&mut app);
    let row = doc.tokens.first().expect("a token row").0;
    let text = press(&mut app, MEDIUM, &format!("{}<Enter>", "j".repeat(row)));
    assert_eq!(app.overlay, Overlay::Decoded);
    assert!(text.contains("signature not verified") && text.contains("Claims"), "{text}");
    press(&mut app, MEDIUM, "<Esc>");
    assert_eq!(app.overlay, Overlay::None);

    // Request tab: Enter on the Authorization header opens the value menu
    press(&mut app, MEDIUM, "<Right><Right>g");
    let doc = traffic_police_tui::detail::build_doc(&mut app);
    let row = (0..doc.len())
        .find(|&i| {
            traffic_police_tui::detail::row_text(&mut app, &doc, i, init)
                .is_some_and(|t| t.starts_with("Authorization: "))
        })
        .expect("the Authorization header");
    let text = press(&mut app, MEDIUM, &format!("{}<Enter>", "j".repeat(row)));
    assert_eq!(app.overlay, Overlay::Menu);
    assert!(text.contains("header Authorization") && text.contains("decode JWT"), "{text}");
    press(&mut app, MEDIUM, "f");
    assert!(app.filter_input.value().starts_with("header:\"authorization=Bearer ey"), "{}", app.filter_input.value());
    let store = app.view_store();
    let rows = app.view_rows().rows();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| store.txn(r.txn()).req_headers.iter().any(|(n, _)| n == "Authorization")));
}

#[test]
fn the_palette_finds_and_runs_commands() {
    let mut app = app_at(40.0);
    let text = press(&mut app, MEDIUM, ":");
    assert_eq!(app.overlay, Overlay::Palette);
    assert!(text.contains("commands") && text.contains("Export"), "{text}");
    // a setting without a key of its own
    press(&mut app, MEDIUM, "braille<Enter>");
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.graph_style, traffic_police_tui::graph::GraphStyle::Braille);
    // an action runs as its key would: pin the selected request
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("{to}:pin<Enter>"));
    assert!(app.view_store().txn(app.selected.unwrap()).pinned);
    // a command without a key, found by what people call it
    let shown = app.frames.visible;
    press(&mut app, MEDIUM, ":fps<Enter>");
    assert_ne!(app.frames.visible, shown, "fps finds the frame rate readout");
    // nothing matches: Enter says so; Esc closes without running anything
    press(&mut app, MEDIUM, ":zzzz<Enter>");
    assert_eq!(app.overlay, Overlay::None);
    press(&mut app, MEDIUM, ":pin<Esc>");
    assert!(app.view_store().txn(app.selected.unwrap()).pinned, "Esc ran nothing");
}

#[test]
fn the_body_explorer_moves_folds_and_switches_bodies() {
    use traffic_police_tui::explorer::BodyTab;
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    // the explorer sits above the tabs: Shift+Tab reaches it
    press(&mut app, MEDIUM, "<S-Tab>");
    assert_eq!(app.focus, Focus::Preview);
    // down to "config": { and fold it with Enter, then unfold it
    let text = press(&mut app, MEDIUM, "jjjj<Enter>");
    assert_eq!(app.explorer.cursor, 4);
    assert!(text.contains("\"config\": {…},  4 keys"), "{text}");
    let text = press(&mut app, MEDIUM, "<Enter>");
    assert!(!text.contains("4 keys"), "{text}");
    // Enter on a single value opens its value menu
    let text = press(&mut app, MEDIUM, "k<Enter>");
    assert_eq!(app.overlay, Overlay::Menu);
    assert!(text.contains("value at $.deepid"), "{text}");
    press(&mut app, MEDIUM, "<Esc>");
    // l goes to the request body and h back, as the tabs below are switched
    let text = press(&mut app, MEDIUM, "l");
    assert_eq!((app.explorer.tab, app.focus), (BodyTab::Request, Focus::Preview));
    assert!(text.contains("\"sdkVersion\": \"2.4.1\""), "the request's JSON: {text}");
    press(&mut app, MEDIUM, "h");
    assert_eq!(app.explorer.tab, BodyTab::Response);
    // Tab goes on to the tabs below, where h and l switch tabs too
    press(&mut app, MEDIUM, "<Tab>l");
    assert_eq!(app.focus, Focus::Detail);
    assert_eq!(app.detail.tab, traffic_police_tui::app::Tab::Response);
}

#[test]
fn rules_from_the_project_file_toggle_edit_and_grow() {
    let dir = std::env::temp_dir().join(format!("tp-rules-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tp = dir.join(".traffic-police");
    std::fs::create_dir_all(&tp).unwrap();
    let path = tp.join("rules.toml");
    std::fs::write(
        &path,
        "version = 1\n\n# the status poll\n[[rule]]\nid = \"slow\"\nname = \"Slow poll\"\n  [rule.match]\n  path = \"/api/sdk/*/status/**\"\n  [[rule.action]]\n  type = \"delay\"\n  ms = 800\n",
    )
    .unwrap();
    let mut app = app_at(40.0);
    let (tx, mut rx) = mpsc::unbounded_channel();
    app.commands = Some(tx);
    let f = traffic_police_core::rules::load(&path);
    app.rules = Some(f.set.clone());
    app.rules_file = Some(f);
    app.rules_dir = Some(tp.clone());
    let text = press(&mut app, MEDIUM, "3");
    assert!(text.contains("[x] slow") && text.contains("Slow poll") && text.contains("delay 800 ms"), "{text}");
    // Space turns it off in the file (the comment stays) and sends the new rules to the app
    press(&mut app, MEDIUM, "<Space>");
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("# the status poll\n[[rule]]\nenabled = false\nid = \"slow\""), "{written}");
    assert!(app.recording, "Space in the Rules view does not pause");
    match rx.try_recv() {
        Ok(BackendCommand::SetRules(r)) => assert!(!r.rules[0].enabled),
        other => panic!("expected SetRules, got {other:?}"),
    }
    // Enter opens the file at the rule
    press(&mut app, MEDIUM, "<Enter>");
    assert_eq!(app.editor_request.take(), Some((path.clone(), 4)));
    // r on a request appends a rule that matches it exactly, off, and opens it
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("1{to}r"));
    let f = traffic_police_core::rules::load(&path);
    assert!(f.is_valid(), "{:?}", f.problems);
    let new = f.set.rules.last().unwrap();
    assert_eq!((new.id.as_str(), new.enabled), ("init", false));
    assert_eq!(new.matcher.path, Some(traffic_police_proto::msg::Pattern::Exact("/api/sdk/init".into())));
    assert_eq!(app.editor_request.take().map(|(p, _)| p), Some(path.clone()));
    // a broken file keeps the active rules, and says where it breaks
    let active = app.rules.clone();
    std::fs::write(&path, "[[rule]]\nid = \"x\"\nbogus = true\n").unwrap();
    app.rules_reloaded(traffic_police_core::rules::load(&path));
    assert_eq!(app.rules, active);
    let text = press(&mut app, MEDIUM, "3");
    assert!(text.contains("rules.toml has 1 problem: line 3"), "{text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn body_filters_fill_in_from_the_background() {
    let mut app = app_at(40.0);
    press(&mut app, MEDIUM, "/body:\"simBinding\"<Enter>");
    // render_keys runs queued jobs inline, as the event loop would run them on workers
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let store = app.view_store();
    let paths: Vec<&str> = app.view_rows().rows().iter().map(|r| store.txn(r.txn()).url.path.as_str()).collect();
    assert!(!paths.is_empty());
    assert!(paths.iter().all(|p| *p == "/api/sdk/init"), "{paths:?}");
}

#[test]
fn search_in_the_detail_pane_steps_through_matches() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    // Response tab of init: its JSON mentions "feature" keys several times
    press(&mut app, MEDIUM, &format!("{to}<Enter>l/true<Enter>"));
    let n = app.search.matches.len();
    assert!(n >= 2, "{n} matches");
    let first = app.search.current.expect("jumped to a match");
    assert_eq!(app.detail.cursor, app.search.matches[first].row);
    press(&mut app, MEDIUM, "n");
    let second = app.search.current.unwrap();
    assert_eq!(second, (first + 1) % n);
    press(&mut app, MEDIUM, "N");
    assert_eq!(app.search.current, Some(first));
    // Esc while typing restores the previous search
    press(&mut app, MEDIUM, "/zz<Esc>");
    assert_eq!(app.search.query, "true");
}

#[test]
fn copy_as_curl_url_and_a_json_value() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("{to}yc"));
    let curl = app.copied.clone().expect("copied");
    assert!(curl.starts_with("curl"), "{curl}");
    assert!(curl.contains("--data-binary '{"), "the JSON request body goes inline: {curl}");
    assert!(!curl.contains("Content-Length"), "{curl}");
    press(&mut app, MEDIUM, "yu");
    assert!(app.copied.as_deref().unwrap().ends_with("/api/sdk/init"));
    // the value under the cursor in the response body
    press(&mut app, MEDIUM, "<Enter>l");
    press(&mut app, MEDIUM, "/pollIntervalMs<Enter>");
    press(&mut app, MEDIUM, "yv");
    assert_eq!(app.copied.as_deref(), Some("1500"));
}

#[test]
fn save_a_body_and_export_har() {
    use traffic_police_tui::app::Overlay;
    let dir = std::env::temp_dir().join(format!("tp-share-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("{to}w"));
    assert_eq!(app.overlay, Overlay::Prompt);
    let default = app.prompt.as_ref().unwrap().input.value().to_string();
    assert!(default.ends_with("init.json"), "{default}");
    let body = dir.join("init.json");
    app.prompt.as_mut().unwrap().input = tui_input::Input::new(body.display().to_string());
    app.finish_prompt();
    let saved = std::fs::read_to_string(&body).unwrap();
    assert!(saved.contains("\"sessionId\""), "{saved}");

    // all requests as HAR
    press(&mut app, MEDIUM, "ea");
    let har = dir.join("all.har");
    app.prompt.as_mut().unwrap().input = tui_input::Input::new(har.display().to_string());
    app.finish_prompt();
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(&har).unwrap()).unwrap();
    let entries = doc["log"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), app.view_store().len());
    let init = entries.iter().find(|e| e["request"]["url"].as_str().unwrap().ends_with("/api/sdk/init")).unwrap();
    assert_eq!(init["request"]["method"], "POST");
    assert!(init["request"]["postData"]["text"].as_str().unwrap().starts_with('{'));
    assert!(init["_trafficPolice"]["thread"]["name"].is_string());
    assert!(init["timings"]["wait"].as_f64().unwrap() >= 0.0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn going_to_the_bottom_follows_new_requests_with_a_request_open() {
    let mut app = app_at(5.0);
    let mut session = DemoSession::new(DemoConfig::default(), app.store.source_ids());
    for t in 1..=200 {
        app.ingest(session.advance(t * 25_000_000));
    }
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    // open a request above the newest: it stays while new ones arrive
    press(&mut app, MEDIUM, "kk<Enter>");
    let picked = app.selected;
    for t in 201..=300 {
        app.ingest(session.advance(t * 25_000_000));
    }
    app.now_override = Some(DemoSession::clock_at(7_500_000_000));
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    assert_eq!(app.selected, picked, "an open request stays put");
    // back to the list, to the bottom: the open request follows the newest from then on
    press(&mut app, MEDIUM, "<Tab><Tab>");
    assert_eq!(app.focus, Focus::List);
    press(&mut app, MEDIUM, "G");
    for t in 301..=400 {
        app.ingest(session.advance(t * 25_000_000));
    }
    app.now_override = Some(DemoSession::clock_at(10 * NS_PER_SEC));
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let last = app.view_rows().len() - 1;
    assert_eq!(app.list_cursor, last, "the cursor followed the new requests");
    assert_eq!(app.selected, Some(app.view_rows().rows()[last].txn()));
    assert!(app.detail_open);
    // moving away stops it
    press(&mut app, MEDIUM, "k");
    let picked = app.selected;
    for t in 401..=480 {
        app.ingest(session.advance(t * 25_000_000));
    }
    app.now_override = Some(DemoSession::clock_at(12 * NS_PER_SEC));
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    assert_eq!(app.selected, picked);
    // Ctrl+G goes to the bottom too
    press(&mut app, MEDIUM, "<C-g>");
    assert_eq!(app.list_cursor, app.view_rows().len() - 1);
}

#[test]
fn half_page_jumps_move_the_view_and_the_cursor() {
    let mut app = app_at(40.0);
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    press(&mut app, MEDIUM, "g");
    let half = app.list_height / 2;
    press(&mut app, MEDIUM, "<C-d>");
    assert_eq!((app.list_cursor, app.list_offset), (half, half), "like Neovim's Ctrl+D");
    press(&mut app, MEDIUM, "<C-u>");
    assert_eq!((app.list_cursor, app.list_offset), (0, 0));
    press(&mut app, MEDIUM, "<C-d><C-p>");
    assert_eq!(app.list_cursor, 0, "Ctrl+P goes up too");
    // [ui] scroll sets the jump
    app.prefs.scroll = 3;
    press(&mut app, MEDIUM, "<C-d>");
    assert_eq!(app.list_cursor, 3);

    // the body box: init's response body is 18 lines
    app.prefs.scroll = 0;
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    press(&mut app, MEDIUM, &format!("{to}<Enter><S-Tab>"));
    assert_eq!(app.focus, Focus::Preview);
    let half = app.explorer.height / 2;
    press(&mut app, MEDIUM, "<C-d>");
    assert_eq!(app.explorer.cursor, half);
    press(&mut app, MEDIUM, "<C-u>");
    assert_eq!(app.explorer.cursor, 0);

    // the detail tabs
    press(&mut app, MEDIUM, "<Tab>");
    assert_eq!(app.focus, Focus::Detail);
    let half = app.detail_height / 2;
    press(&mut app, MEDIUM, "<C-d>");
    assert_eq!(app.detail.cursor, half);
}

#[test]
fn the_body_box_shows_the_request_body_on_its_second_tab() {
    use traffic_police_tui::explorer::BodyTab;
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    let text = press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    assert!(text.contains("Response body") && text.contains("Request body"), "{text}");
    assert_eq!(app.explorer.tab, BodyTab::Response);
    // init posts {"sdkVersion":"2.4.1","platform":"android"}
    let text = press(&mut app, MEDIUM, "b");
    assert_eq!(app.explorer.tab, BodyTab::Request);
    assert!(text.contains("\"sdkVersion\": \"2.4.1\""), "{text}");
    // the tab stays when another request is opened
    press(&mut app, MEDIUM, "<Esc>j<Enter>");
    assert_eq!(app.explorer.tab, BodyTab::Request);
    // a click on the other tab
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let r = app.hits.rect_of(Target::ExplorerTab(BodyTab::Response)).expect("the Response body tab");
    click(&mut app, r.x + 1, r.y);
    assert_eq!(app.explorer.tab, BodyTab::Response);
    assert_eq!(app.focus, Focus::Preview);
}

#[test]
fn long_rows_wrap_and_enter_still_acts_on_the_whole_row() {
    use traffic_police_tui::detail;
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    let text = press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    let txn = app.selected.unwrap();
    let doc = detail::build_doc(&mut app);
    let (row, _) = doc.tokens.first().cloned().expect("a token row");
    // the token row is wider than the pane at this size: it goes on under its value
    let width = app.detail_width;
    assert!(detail::row_height(&mut app, &doc, row, txn, width) > 1);
    let y = text.lines().position(|l| l.contains("Token             JWT")).expect("the token row");
    let next = text.lines().nth(y + 1).unwrap();
    assert!(next.contains(&format!("││{}", " ".repeat(19))), "under the value: {next:?}");
    // Enter on it decodes the token
    let text = press(&mut app, MEDIUM, &format!("{}<Enter>", "j".repeat(row)));
    assert_eq!(app.overlay, Overlay::Decoded, "{text}");
    press(&mut app, MEDIUM, "<Esc>");
    // [ui] wrap = false: one row each again, cut at the edge
    app.prefs.wrap = false;
    assert_eq!(detail::row_height(&mut app, &doc, row, txn, width), 1);
}

#[test]
fn body_lines_wrap_in_the_body_box_and_the_tabs() {
    use traffic_police_core::model::BodyDir;
    use traffic_police_tui::detail;
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/oauth/token"));
    let text = press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    let txn = app.selected.unwrap();
    let body = app.body_view(txn, BodyDir::Response).unwrap().decoded.bytes.clone();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let token = json["access_token"].as_str().unwrap().to_string();
    let end = &token[token.len() - 16..];
    // the body box shows the token to its end, on the rows under its key
    assert_eq!(text.matches(end).count(), 1, "{text}");
    // the Response tab's body too, once the cursor is on it
    press(&mut app, MEDIUM, "l");
    let doc = detail::build_doc(&mut app);
    let row = (0..doc.len())
        .find(|&i| detail::row_text(&mut app, &doc, i, txn).is_some_and(|t| t.contains(&token)))
        .expect("the token's line");
    let width = app.detail_width;
    assert!(detail::row_height(&mut app, &doc, row, txn, width) > 2);
    let text = press(&mut app, MEDIUM, &"j".repeat(row));
    assert_eq!(text.matches(end).count(), 2, "{text}");
    // not wrapping: cut at the edge in both
    app.prefs.wrap = false;
    let text = render_text(&mut app, MEDIUM.0, MEDIUM.1);
    assert!(!text.contains(end), "{text}");
}

#[test]
fn search_finds_text_across_the_break_of_a_wrapped_row() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/sdk/init"));
    let text = press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    // the token row's first two rows on screen, from where the row starts
    let lines: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
    let y = lines.iter().position(|l| l.iter().collect::<String>().contains("Token             JWT")).unwrap();
    let x = (0..lines[y].len()).find(|&x| lines[y][x..].starts_with(&['T', 'o', 'k', 'e', 'n'])).unwrap();
    let part = |l: &[char]| l[x..].iter().collect::<String>().trim_end_matches('│').trim_end().to_string();
    let (one, two) = (part(&lines[y]), part(&lines[y + 1]));
    // the last word of the first and the first of the second are together only in the row
    let (a, b) = (one.split_whitespace().last().unwrap(), two.split_whitespace().next().unwrap());
    press(&mut app, MEDIUM, &format!("/{a} {b}<Enter>"));
    let row = app.detail.cursor;
    assert!(app.search.matches.iter().any(|m| m.row == row), "{:?}", app.search.matches);
    // both rows are painted as the current match, also with the body scrolled sideways
    app.detail.hscroll = 4;
    let mut term = Terminal::new(TestBackend::new(MEDIUM.0, MEDIUM.1)).unwrap();
    term.draw(|f| ui::draw(f, &mut app)).unwrap();
    let buf = term.backend().buffer();
    let hit = app.theme.search_hit(true).bg.unwrap();
    let painted = |y: usize, from: usize, n: usize| (from..from + n).all(|c| buf[((x + c) as u16, y as u16)].bg == hit);
    let a_at = one.chars().count() - a.chars().count();
    let b_at = two.chars().count() - two.trim_start().chars().count();
    assert!(painted(y, a_at, a.chars().count()), "{a:?} in {one:?}");
    assert!(painted(y + 1, b_at, b.chars().count()), "{b:?} in {two:?}");
}

#[test]
fn hidden_boxes_take_no_room_and_no_focus() {
    let mut app = app_at(12.0);
    app.prefs.graph_height = Some(0);
    app.prefs.body_height = 0;
    app.prefs.hints = false;
    let text = press(&mut app, MEDIUM, "<Enter>");
    assert!(!text.contains("Network"), "no graph: {text}");
    assert!(!text.contains("Request body"), "no body box: {text}");
    assert!(!text.contains("? help"), "no key hints: {text}");
    press(&mut app, MEDIUM, "<Tab>");
    assert_eq!(app.focus, Focus::List, "Tab skips the hidden graph and body box");
    press(&mut app, MEDIUM, "<Tab>");
    assert_eq!(app.focus, Focus::Detail);
}
