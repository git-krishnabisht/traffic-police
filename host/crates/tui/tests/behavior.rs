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
use traffic_police_core::backend::{BackendCommand, Capabilities};
use traffic_police_core::fmt::{NS_PER_MS, NS_PER_SEC};
use traffic_police_core::rows::Column;
use traffic_police_core::store::SessionStore;
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

/// A device session draws its first frame before the app has sent anything: an empty store and
/// no clock, so the graph's window is empty. In 0.2.0 that frame took tens of seconds and
/// gigabytes: the smoothing ran over columns a nanosecond wide.
#[test]
fn first_frame_before_the_app_sends_anything() {
    let mut app = App::new(SessionStore::new(), Theme::default());
    app.caps = Capabilities { pause: true, rules: true, live: true };
    let text = render_text(&mut app, MEDIUM.0, MEDIUM.1);
    assert!(text.contains("No requests yet"), "{text}");
    // the graph shows the zoom's first stretch, in the zoom's slices
    let (left, right) = app.graph.drawn.expect("the graph was drawn");
    assert_eq!(left, 0);
    assert!(app.graph.span - right < NS_PER_MS, "{right}");
}

/// An app that trickles data in while pouring it out, as a polling SDK does: 3 KB/s received
/// against 300 KB/s sent. On one shared scale the receiving line is flattened against the
/// baseline (the bottom row); in the mirror layout it climbs its own half.
#[test]
fn the_mirror_layout_keeps_a_trickle_readable_next_to_a_flood() {
    use traffic_police_core::SessionEvent;
    use traffic_police_core::model::SourceInfo;
    let tick = NS_PER_SEC / 2;
    let mut events = vec![SessionEvent::SourceUp(Box::new(SourceInfo {
        id: 1,
        device_label: "Pixel 8 [test]".into(),
        serial: None,
        package: "com.example.poller".into(),
        process: "com.example.poller".into(),
        pid: 4242,
        instance: "test".into(),
        mode: "library".into(),
        api: Some(35),
        runtime_version: None,
        capabilities: vec!["traffic".into()],
        hooks: vec![],
        okhttp_version: None,
        clock: Some((tick, 1_700_000_000_000)),
        started: tick,
        ended: None,
    }))];
    // the counters every half second for 20 s: 1.5 KB in and 150 KB out per tick
    for i in 1..=40u64 {
        events.push(SessionEvent::Traffic {
            source: 1,
            at: i * tick,
            since: (i > 1).then_some((i - 1) * tick),
            rx: i * 1536,
            tx: i * 153_600,
        });
    }
    let draw = |layout: traffic_police_tui::graph::GraphLayout| {
        let mut app = App::new(SessionStore::new(), Theme::default());
        app.caps = Capabilities { pause: true, rules: true, live: true };
        app.graph_layout = layout;
        // the curves carry their series' exact color; the solid areas shade their rows
        app.graph_style = traffic_police_tui::graph::GraphStyle::Curves;
        app.ingest(events.clone());
        app.now_override = Some(40 * tick);
        let mut term = Terminal::new(TestBackend::new(MEDIUM.0, MEDIUM.1)).unwrap();
        term.draw(|f| ui::draw(f, &mut app)).unwrap();
        let theme = app.theme.clone();
        let buf = term.backend().buffer().clone();
        // the rows (relative to the plot's top) each series reaches, from the colors of its dots
        let plot_top = 2u16; // header, then the panel's border
        let rows_of = |color: Color| -> Vec<u16> {
            let mut rows: Vec<u16> = (plot_top..plot_top + 20)
                .filter(|&y| (0..MEDIUM.0).any(|x| buf[(x, y)].fg == color && buf[(x, y)].symbol() != " "))
                .map(|y| y - plot_top)
                .collect();
            rows.dedup();
            rows
        };
        (rows_of(theme.recv()), rows_of(theme.send()))
    };
    use traffic_police_tui::graph::GraphLayout;
    let (recv, send) = draw(GraphLayout::Overlay);
    assert_eq!(recv.len(), 1, "on one scale the trickle is one flat row: {recv:?}");
    assert!(send.len() >= 2 && send[0] < recv[0], "the flood rises above it: {send:?} vs {recv:?}");
    let (recv, send) = draw(GraphLayout::Mirror);
    assert!(recv.len() >= 2, "in the mirror the trickle has a shape of its own: {recv:?}");
    assert!(send.len() >= 2, "{send:?}");
    assert!(recv.iter().all(|r| send.iter().all(|s| r < s)), "receiving above, sending below: {recv:?} {send:?}");
}

/// A 150 ms request on a 30 s window of 100 cells is 4 eighths wide. Scrolled across a cell an
/// eighth at a time, the bar keeps its width and its left edge follows within an eighth or two:
/// the old right-anchored set had only ⅛, ½ and full blocks, so the edge stood still and jumped.
#[test]
fn timeline_bars_keep_their_width_and_edge_while_scrolling() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use traffic_police_core::phases::Segments;
    let left_blocks = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
    let right_blocks = [" ", "▕", "🮇", "🮈", "▐", "🮉", "🮊", "🮋", "█"];
    // (first eighth, last eighth) a cell's glyph covers, from the cell's left edge
    let covered = |sym: &str| -> (u64, u64) {
        if let Some(k) = left_blocks.iter().position(|g| *g == sym) {
            (0, k as u64)
        } else if let Some(k) = right_blocks.iter().position(|g| *g == sym) {
            (8 - k as u64, 8)
        } else {
            panic!("not a block: {sym:?}")
        }
    };
    let theme = Theme::default();
    let cell_ns = 300 * NS_PER_MS;
    let (left, right) = (1_000 * NS_PER_SEC, 1_030 * NS_PER_SEC);
    let mut last_edge = 0;
    for step in 0..24u64 {
        let start = left + 10 * cell_ns + step * cell_ns / 8;
        let seg = Segments {
            start,
            sent: start + 10 * NS_PER_MS,
            first_byte: Some(start + 120 * NS_PER_MS),
            end: start + 150 * NS_PER_MS,
        };
        let area = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(area);
        ui::draw_bar(&mut buf, area, (left, right), seg, &theme, false);
        let cells: Vec<(u64, (u64, u64))> = (0..100u64)
            .filter(|&x| buf[(x as u16, 0)].symbol() != " ")
            .map(|x| (x, covered(buf[(x as u16, 0)].symbol())))
            .collect();
        let width: u64 = cells.iter().map(|(_, (lo, hi))| hi - lo).sum();
        assert_eq!(width, 4, "step {step}: {cells:?}");
        let edge = cells[0].0 * 8 + cells[0].1.0;
        let truth = 80 + step;
        assert!(edge.abs_diff(truth) <= 2, "step {step}: edge {edge} for {truth}: {cells:?}");
        assert!(edge >= last_edge, "step {step}: the edge went back from {last_edge} to {edge}");
        last_edge = edge;
    }
    // a bar starting 5 eighths into a cell: a 3-eighths block from the right, then one from the left
    let start = left + 10 * cell_ns + 5 * cell_ns / 8;
    let seg = Segments { start, sent: start, first_byte: None, end: start + 150 * NS_PER_MS };
    let area = Rect::new(0, 0, 100, 1);
    let mut buf = Buffer::empty(area);
    ui::draw_bar(&mut buf, area, (left, right), seg, &theme, false);
    assert_eq!((buf[(10, 0)].symbol(), buf[(11, 0)].symbol()), ("🮈", "▏"));
    // with the terminal's background known, the same edge in glyphs every font has: the empty
    // five eighths as a left block in the background color over a cell in the bar's color
    let mut theme = Theme::default();
    theme.set_color("background", (1, 2, 3));
    let mut buf = Buffer::empty(area);
    ui::draw_bar(&mut buf, area, (left, right), seg, &theme, false);
    let cell = &buf[(10, 0)];
    assert_eq!((cell.symbol(), cell.fg), ("▋", Color::Rgb(1, 2, 3)));
    assert_ne!(cell.bg, Color::Reset);
}

#[test]
fn only_colon_q_quits() {
    // `q` and Ctrl+C do not quit (as in Neovim): they say what does
    let mut app = app_at(5.0);
    let text = press(&mut app, MEDIUM, "q");
    assert!(!app.should_quit);
    assert!(text.contains("type :q and press Enter to quit"), "{text}");
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!app.should_quit, "Ctrl+C does not quit");
    press(&mut app, MEDIUM, ":q<Enter>");
    assert!(app.should_quit);
    // the footer and the help show it
    let mut app = app_at(5.0);
    let text = press(&mut app, LARGE, "");
    assert!(text.contains(":q quit"), "{text}");
    // with `quit = ["q"]` in [keymap], `q` quits again
    let mut app = app_at(5.0);
    let errors = app.keymap.apply(&[("quit".to_string(), vec!["q".to_string()])]);
    assert!(errors.is_empty(), "{errors:?}");
    press(&mut app, MEDIUM, "q");
    assert!(app.should_quit);
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
    // the list's right border is at column 55 now, and the detail box starts a blank column later
    let text = render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let row = text.lines().nth(14).unwrap();
    assert_eq!(row.chars().nth(55), Some('│'), "{text}");
    assert_eq!(row.chars().nth(56), Some(' '), "{text}");
    assert_eq!(row.chars().nth(57), Some('│'), "{text}");

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
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
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
    // SessionRepository.kt lives outside its package directory, as Kotlin allows
    let file = root.join("shop/session/SessionRepository.kt");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "class SessionRepository\n").unwrap();
    let mut app = app_at(12.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
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
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    let text = press(&mut app, MEDIUM, &format!("{to}m"));
    let init = app.selected.expect("selected");
    assert!(app.view_store().txn(init).pinned);
    assert!(text.contains("★ sessions"), "{text}");
    press(&mut app, MEDIUM, "/is:pinned<Enter>");
    assert_eq!(app.view_rows().matched(), 1);
    press(&mut app, MEDIUM, "m");
    assert!(!app.view_store().txn(init).pinned);
}

#[test]
fn d_on_two_requests_compares_them() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    let text = press(&mut app, MEDIUM, &format!("{to}d"));
    assert!(text.contains("◆ sessions"), "{text}");
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/notifications"));
    let text = press(&mut app, MEDIUM, &format!("{to}d"));
    assert_eq!(app.overlay, Overlay::Diff);
    assert!(text.contains("A POST http://localhost:8080/api/v1/sessions"), "{text}");
    assert!(text.contains("B GET http://localhost:8080/api/v1/notifications"), "{text}");
    assert!(text.contains("differs in request,"), "{text}");
    assert!(text.contains("- POST http://localhost:8080/api/v1/sessions"), "{text}");
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
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
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
    assert!(text.contains("commands") && text.contains("Connection View") && text.contains("view-logdawg"), "{text}");
    // a setting without a key of its own
    press(&mut app, MEDIUM, "braille<Enter>");
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.graph_style, traffic_police_tui::graph::GraphStyle::Braille);
    // an action runs as its key would: pin the selected request
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
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
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
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
    assert!(text.contains("value at $.userId"), "{text}");
    press(&mut app, MEDIUM, "<Esc>");
    // l goes to the request body and h back, as the tabs below are switched
    let text = press(&mut app, MEDIUM, "l");
    assert_eq!((app.explorer.tab, app.focus), (BodyTab::Request, Focus::Preview));
    assert!(text.contains("\"appVersion\": \"3.8.0\""), "the request's JSON: {text}");
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
        "version = 1\n\n# the status poll\n[[rule]]\nid = \"slow\"\nname = \"Slow poll\"\n  [rule.match]\n  path = \"/api/v1/*/status\"\n  [[rule.action]]\n  type = \"delay\"\n  ms = 800\n",
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
    // Enter opens the rule in the form (Esc leaves it), E the file at the rule
    press(&mut app, MEDIUM, "<Enter>");
    assert_eq!(app.form.as_ref().map(|f| f.draft.id.as_str()), Some("slow"));
    press(&mut app, MEDIUM, "<Esc>E");
    assert!(app.form.is_none());
    assert_eq!(app.editor_request.take(), Some((path.clone(), 4)));
    // r on a request opens a rule that matches it exactly in the form; saved, it is appended
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    press(&mut app, MEDIUM, &format!("1{to}r"));
    assert_eq!(app.view, traffic_police_tui::app::View::Rules);
    press(&mut app, MEDIUM, "<C-s>");
    assert!(app.form.is_none(), "saved");
    let f = traffic_police_core::rules::load(&path);
    assert!(f.is_valid(), "{:?}", f.problems);
    let new = f.set.rules.last().unwrap();
    assert_eq!((new.id.as_str(), new.enabled), ("sessions", true));
    assert_eq!(new.matcher.path, Some(traffic_police_proto::msg::Pattern::Exact("/api/v1/sessions".into())));
    assert_eq!(new.matcher.methods, ["POST"]);
    assert!(std::fs::read_to_string(&path).unwrap().contains("# the status poll"));
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
    press(&mut app, MEDIUM, "/body:\"darkMode\"<Enter>");
    // render_keys runs queued jobs inline, as the event loop would run them on workers
    render_text(&mut app, MEDIUM.0, MEDIUM.1);
    let store = app.view_store();
    let paths: Vec<&str> = app.view_rows().rows().iter().map(|r| store.txn(r.txn()).url.path.as_str()).collect();
    assert!(!paths.is_empty());
    assert!(paths.iter().all(|p| *p == "/api/v1/sessions"), "{paths:?}");
}

#[test]
fn search_in_the_detail_pane_steps_through_matches() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    // Response tab of the session request: its JSON has several `true` values
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
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    press(&mut app, MEDIUM, &format!("{to}yc"));
    let curl = app.copied.clone().expect("copied");
    assert!(curl.starts_with("curl"), "{curl}");
    assert!(curl.contains("--data-binary '{"), "the JSON request body goes inline: {curl}");
    assert!(!curl.contains("Content-Length"), "{curl}");
    press(&mut app, MEDIUM, "yu");
    assert!(app.copied.as_deref().unwrap().ends_with("/api/v1/sessions"));
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
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    press(&mut app, MEDIUM, &format!("{to}w"));
    assert_eq!(app.overlay, Overlay::Prompt);
    let default = app.prompt.as_ref().unwrap().input.value().to_string();
    assert!(default.ends_with("sessions.json"), "{default}");
    let body = dir.join("sessions.json");
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
    let init = entries.iter().find(|e| e["request"]["url"].as_str().unwrap().ends_with("/api/v1/sessions")).unwrap();
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

    // the body box: the session response is 18 lines
    app.prefs.scroll = 0;
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
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
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    let text = press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    assert!(text.contains("Response body") && text.contains("Request body"), "{text}");
    assert_eq!(app.explorer.tab, BodyTab::Response);
    // the session request posts {"appVersion":"3.8.0","platform":"android"}
    let text = press(&mut app, MEDIUM, "b");
    assert_eq!(app.explorer.tab, BodyTab::Request);
    assert!(text.contains("\"appVersion\": \"3.8.0\""), "{text}");
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
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    let text = press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    let txn = app.selected.unwrap();
    let doc = detail::build_doc(&mut app);
    let (row, _) = doc.tokens.first().cloned().expect("a token row");
    // the token row is wider than the pane at this size: it goes on under its value
    let width = app.detail_width;
    assert!(detail::row_height(&mut app, &doc, row, txn, width) > 1);
    let y = text.lines().position(|l| l.contains("Token             JWT")).expect("the token row");
    let next = text.lines().nth(y + 1).unwrap();
    assert!(next.contains(&format!("│ │{}", " ".repeat(19))), "under the value: {next:?}");
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
    // the request's side of the screen, its rows run together without borders and indents: a
    // wrapped line is whole there, a cut one is not
    let top = text.lines().find(|l| l.starts_with("╭─ Requests")).expect("the boxes");
    let x = top.chars().enumerate().filter(|(_, c)| *c == '╭').nth(1).expect("the request's boxes").0;
    let joined = |text: &str| -> String {
        let rows = text.lines().map(|l| l.chars().skip(x).collect::<String>());
        rows.map(|r| r.trim_matches(|c| c == '│' || c == ' ').to_string()).collect()
    };
    // the body box shows the whole token, on the rows under its key
    assert_eq!(joined(&text).matches(&token).count(), 1, "{text}");
    // the Response tab's body too, once the cursor is on it
    press(&mut app, MEDIUM, "l");
    let doc = detail::build_doc(&mut app);
    let row = (0..doc.len())
        .find(|&i| detail::row_text(&mut app, &doc, i, txn).is_some_and(|t| t.contains(&token)))
        .expect("the token's line");
    let width = app.detail_width;
    assert!(detail::row_height(&mut app, &doc, row, txn, width) > 2);
    let text = press(&mut app, MEDIUM, &"j".repeat(row));
    assert_eq!(joined(&text).matches(&token).count(), 2, "{text}");
    // not wrapping: cut at the edge in both
    app.prefs.wrap = false;
    let text = render_text(&mut app, MEDIUM.0, MEDIUM.1);
    assert_eq!(joined(&text).matches(&token).count(), 0, "{text}");
}

#[test]
fn search_finds_text_across_the_break_of_a_wrapped_row() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
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
    app.prefs.body_box = false;
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

/// `B` hides the body box, and the tabs take its room; `B` again brings it back (`[ui] body_box
/// = false` starts without it).
#[test]
fn shift_b_hides_the_body_box_and_shows_it_again() {
    let tabs_at = |text: &str| text.lines().position(|l| l.contains("╭─ Overview")).expect("the tabs");
    let mut app = app_at(12.0);
    let text = press(&mut app, MEDIUM, "<Enter>");
    assert!(text.contains("Response body") && text.contains("Request body"), "on by default: {text}");
    let with_box = tabs_at(&text);
    // from the body box: B hides it, and the focus goes to the tabs, now at the pane's top
    press(&mut app, MEDIUM, "<S-Tab>");
    assert_eq!(app.focus, Focus::Preview);
    let text = press(&mut app, MEDIUM, "B");
    assert!(!text.contains("Response body") && !text.contains("Request body"), "{text}");
    assert_eq!(app.focus, Focus::Detail);
    let top = text.lines().position(|l| l.contains("╭─ Requests")).expect("the list");
    assert_eq!(tabs_at(&text), top, "{text}");
    assert!(text.contains("body box hidden; B shows it"), "{text}");
    // b (the other body) says how to bring the box back, and the focus skips it
    app.message = None;
    let text = press(&mut app, MEDIUM, "b");
    assert!(text.contains("body box hidden; B shows it"), "{text}");
    press(&mut app, MEDIUM, "<S-Tab>");
    assert_eq!(app.focus, Focus::List);
    // B again, also from the list: the box is back where it was
    let text = press(&mut app, MEDIUM, "B");
    assert!(text.contains("Response body") && text.contains("showing the body box"), "{text}");
    assert_eq!(tabs_at(&text), with_box);
    // off from the start, and B still knows its key after [keymap] moves it
    let mut app = app_at(12.0);
    app.prefs.body_box = false;
    let text = press(&mut app, MEDIUM, "<Enter>");
    assert!(!text.contains("Response body"), "{text}");
    assert!(app.keymap.apply(&[("body-box".into(), vec![])]).is_empty());
    app.message = None;
    let text = press(&mut app, MEDIUM, "b");
    assert!(text.contains("body box hidden; :body-box shows it"), "{text}");
}

#[test]
fn boxes_are_the_same_distance_apart_both_ways() {
    for gap in [0usize, 1] {
        let mut app = app_at(12.0);
        app.prefs.gap = gap as u16;
        let text = press(&mut app, MEDIUM, "<Enter>");
        let rows: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
        let s = |y: usize| rows[y].iter().collect::<String>();
        // one above the other: the list and the request's boxes start `gap` rows below the graph's
        let graph_end = (0..rows.len()).find(|&y| rows[y].first() == Some(&'╰')).expect("the graph's box");
        let top = graph_end + 1 + gap;
        assert!(s(top).starts_with("╭─ Requests"), "gap {gap}: {:?}", s(top));
        assert!((graph_end + 1..top).all(|y| s(y).trim().is_empty()), "gap {gap}: {text}");
        // side by side: 2 × gap + 1 blank columns between them, the same space to the eye
        let between = " ".repeat(2 * gap + 1);
        assert!(s(top).contains(&format!("╮{between}╭")), "gap {gap}: {:?}", s(top));
        // the body box over the tabs, `gap` rows apart
        let x = (0..rows[top].len()).filter(|&x| rows[top][x] == '╭').nth(1).expect("the request's boxes");
        let tabs = (top..rows.len()).find(|&y| s(y).contains("╭─ Overview")).expect("the tabs");
        assert_eq!(rows[tabs - 1 - gap][x], '╰', "gap {gap}: {text}");
        let blank = |y: usize| rows[y].get(x..).is_none_or(|r| r.iter().all(|c| *c == ' '));
        assert!((tabs - gap..tabs).all(blank), "gap {gap}: {text}");
    }
}

/// A request opened in a short detail pane: the body box and the tabs share what is there. Up to
/// 0.3.1 the layout panicked (`clamp` with its bounds crossed) when the pane was under 13 rows
/// plus the gap, e.g. 120×30 with `graph_height = 14`, or `gap = 3` at 30 rows.
#[test]
fn a_short_detail_pane_never_breaks_the_layout() {
    for size in [SMALL, (120, 30), MEDIUM] {
        let mut app = app_at(12.0);
        press(&mut app, size, "<Enter>");
        assert!(app.selected.is_some(), "a request is open");
        for graph_height in [None, Some(0), Some(8), Some(14), Some(16), Some(20), Some(26), Some(40)] {
            for gap in 0..=4 {
                for (body_box, body_height) in [(false, 40), (true, 0), (true, 15), (true, 40), (true, 85)] {
                    app.prefs.graph_height = graph_height;
                    app.prefs.gap = gap;
                    (app.prefs.body_box, app.prefs.body_height) = (body_box, body_height);
                    let text = render_text(&mut app, size.0, size.1);
                    let what = format!(
                        "{size:?} graph_height {graph_height:?} gap {gap} body_box {body_box} body_height {body_height}"
                    );
                    assert!(text.contains("Overview"), "{what}: the tabs are drawn\n{text}");
                }
            }
        }
    }
}

/// A menu entry's letter runs it even when the letter also moves or closes: `q` (request body)
/// and `k` (one header) in the copy menu, `j` (decode JWT) in a value menu. Up to 0.3.1 those
/// letters closed the menu or moved its cursor instead.
#[test]
fn menu_letters_win_over_the_keys_that_move() {
    let mut app = app_at(40.0);
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    let init = app.selected.expect("selected");
    let store = app.view_store();
    let body = String::from_utf8(store.body_bytes(&store.txn(init).req_body).to_vec()).expect("a text request body");
    press(&mut app, MEDIUM, "yq");
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.copied.as_deref(), Some(body.as_str()), "q copies the request body");
    // on the Request tab, the cursor on the Authorization header: k copies it, and its value
    // menu decodes the JWT with j
    press(&mut app, MEDIUM, "<Right><Right>g");
    let doc = traffic_police_tui::detail::build_doc(&mut app);
    let row = (0..doc.len())
        .find(|&i| {
            traffic_police_tui::detail::row_text(&mut app, &doc, i, init)
                .is_some_and(|t| t.starts_with("Authorization: "))
        })
        .expect("the Authorization header");
    press(&mut app, MEDIUM, &format!("{}yk", "j".repeat(row)));
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.copied.as_deref().is_some_and(|c| c.starts_with("Bearer ey")), "{:?}", app.copied);
    press(&mut app, MEDIUM, "<Enter>");
    assert_eq!(app.overlay, Overlay::Menu);
    let text = press(&mut app, MEDIUM, "j");
    assert_eq!(app.overlay, Overlay::Decoded, "j decodes the JWT");
    assert!(text.contains("Claims"), "{text}");
}

/// Remapped keys work inside menus, the column chooser and the palette too.
#[test]
fn remapped_keys_move_in_menus_and_the_palette() {
    let mut app = app_at(40.0);
    let errors = app.keymap.apply(&[
        ("down".into(), vec!["ctrl+j".into()]),
        ("up".into(), vec!["ctrl+k".into()]),
        ("back".into(), vec!["ctrl+x".into()]),
        ("open".into(), vec!["ctrl+o".into()]),
    ]);
    assert!(errors.is_empty(), "{errors:?}");
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    // the copy menu: the second entry with the new down key, run with the new open key
    press(&mut app, MEDIUM, "y<C-j><C-j><C-k>");
    let menu = app.menu.as_ref().expect("the copy menu");
    assert_eq!(menu.cursor, 1);
    let second = menu.items[1].label.clone();
    press(&mut app, MEDIUM, "<C-o>");
    assert_eq!(app.overlay, Overlay::None, "{second} ran");
    assert!(app.copied.is_some());
    // the new back key closes a menu; the old one no longer does
    press(&mut app, MEDIUM, "y<Esc>");
    assert_eq!(app.overlay, Overlay::Menu, "Esc is not back any more");
    press(&mut app, MEDIUM, "<C-x>");
    assert_eq!(app.overlay, Overlay::None);
    // the columns
    press(&mut app, MEDIUM, "C<C-j><C-j>");
    assert_eq!(app.overlay, Overlay::Columns { cursor: 2 });
    press(&mut app, MEDIUM, "<C-x>");
    // the palette: letters type, the new keys move
    press(&mut app, MEDIUM, ":graph<C-j>");
    let p = app.palette.as_ref().expect("the palette");
    assert_eq!((p.input.value(), p.cursor), ("graph", 1));
    press(&mut app, MEDIUM, "<C-x>");
    assert_eq!(app.overlay, Overlay::None);
}

/// A rules file in a scratch project, read into the app as the watcher would.
fn with_rules(app: &mut App, name: &str, text: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("tp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tp = dir.join(".traffic-police");
    std::fs::create_dir_all(&tp).unwrap();
    let path = tp.join("rules.toml");
    std::fs::write(&path, text).unwrap();
    let f = traffic_police_core::rules::load(&path);
    app.rules = Some(f.set.clone());
    app.rules_file = Some(f);
    app.rules_dir = Some(tp);
    (dir, path)
}

/// The form's cursor on a line.
fn form_at(app: &mut App, row: traffic_police_tui::ruleform::Row) {
    let f = app.form.as_mut().expect("the form is open");
    f.cursor = f.rows().iter().position(|r| *r == row).unwrap_or_else(|| panic!("no line {row:?}"));
}

const ONE_RULE: &str = "version = 1\n\n# keep me\n[[rule]]\nid = \"slow\"   # the poll\n\n  [rule.match]\n  path = \"/api/v1/*/status\"\n\n  # not too slow\n  [[rule.action]]\n  type = \"delay\"\n  ms = 800\n";

/// Every action type is made and set in the form; the file keeps its comments, and the saved rule
/// reads back into the same form (ARCHITECTURE.md §5.11.1's "done when").
#[test]
fn the_rule_form_makes_every_action_and_keeps_the_file() {
    use traffic_police_core::ruleform::ActionDraft;
    use traffic_police_proto::msg::RuleAction as A;
    use traffic_police_tui::ruleform::{Key as K, Row};
    let mut app = app_at(40.0);
    let (dir, path) = with_rules(&mut app, "form-actions", ONE_RULE);
    press(&mut app, MEDIUM, "3<Enter>");
    assert!(app.form.is_some());
    form_at(&mut app, Row::ActionField(0, K::Ms));
    press(&mut app, MEDIUM, "<Enter><C-u>1500<Enter>");
    for letter in ['f', 's', 'h', 'b', 'r'] {
        form_at(&mut app, Row::AddAction);
        press(&mut app, MEDIUM, &format!("<Enter>{letter}"));
    }
    form_at(&mut app, Row::ActionField(1, K::Exception));
    press(&mut app, MEDIUM, "l");
    form_at(&mut app, Row::ActionField(1, K::Message));
    press(&mut app, MEDIUM, "<Enter>gone<Enter>");
    form_at(&mut app, Row::ActionField(2, K::Code));
    press(&mut app, MEDIUM, "<Enter><C-u>418<Enter>");
    form_at(&mut app, Row::ActionField(3, K::Name));
    press(&mut app, MEDIUM, "<Enter>X-Test<Enter>");
    form_at(&mut app, Row::ActionField(3, K::Value));
    press(&mut app, MEDIUM, "<Enter>yes<Enter>");
    form_at(&mut app, Row::ActionField(4, K::Body));
    press(&mut app, MEDIUM, "<Enter>{\"ok\":true}<Enter>");
    form_at(&mut app, Row::ActionField(5, K::Find));
    press(&mut app, MEDIUM, "<Enter>pend(ing)?<Enter>");
    form_at(&mut app, Row::ActionField(5, K::With));
    press(&mut app, MEDIUM, "<Enter>done<Enter>");
    form_at(&mut app, Row::ActionField(5, K::Regex));
    press(&mut app, MEDIUM, "<Space>");
    let form = app.form.clone().unwrap();
    assert!(form.problems.is_empty(), "{:?}", form.problems);
    let text = press(&mut app, MEDIUM, "<C-s>");
    assert!(app.form.is_none(), "saved and closed: {text}");
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# keep me\n[[rule]]\nid = \"slow\"   # the poll"), "{saved}");
    assert!(saved.contains("  # not too slow\n  [[rule.action]]\n  type = \"delay\"\n  ms = 1500"), "{saved}");
    let f = traffic_police_core::rules::load(&path);
    assert!(f.is_valid(), "{:?}\n{saved}", f.problems);
    assert_eq!(
        f.set.rules[0].actions,
        vec![
            A::Delay { ms: 1500 },
            A::Fail { exception: "io".into(), message: Some("gone".into()) },
            A::Status { code: 418, reason: None },
            A::Header { op: "set".into(), name: "X-Test".into(), value: Some("yes".into()) },
            A::Body { text: Some("{\"ok\":true}".into()), base64: None, content_type: Some("application/json".into()) },
            A::Replace { find: "pend(ing)?".into(), with: "done".into(), regex: true },
        ]
    );
    assert_eq!(app.rules.as_ref().unwrap().rules[0].actions.len(), 6, "the saved rules are active");
    // read back into the same form
    press(&mut app, MEDIUM, "<Enter>");
    let back = app.form.as_ref().unwrap();
    let kinds = |d: &traffic_police_core::ruleform::RuleDraft| {
        d.actions.iter().map(|a| a.action.clone()).collect::<Vec<ActionDraft>>()
    };
    assert_eq!(kinds(&back.draft), kinds(&form.draft));
    assert_eq!(back.draft.path, form.draft.path);
    std::fs::remove_dir_all(dir).unwrap();
}

/// A problem shows at its field and keeps the rule from being saved; Esc with changes asks first.
#[test]
fn the_rule_form_checks_as_you_type_and_asks_before_leaving() {
    use traffic_police_tui::ruleform::{Key as K, Row};
    let mut app = app_at(40.0);
    let (dir, path) = with_rules(&mut app, "form-checks", ONE_RULE);
    press(&mut app, MEDIUM, "3<Enter>a");
    // the menu's s: a status action; then a code no status has
    press(&mut app, MEDIUM, "s");
    form_at(&mut app, Row::ActionField(1, K::Code));
    let text = press(&mut app, MEDIUM, "<Enter><C-u>999");
    assert!(text.contains("✗ status code 999: use 100 to 599"), "{text}");
    press(&mut app, MEDIUM, "<Enter><C-s>");
    assert!(app.form.is_some(), "not saved with a problem");
    assert!(app.current_message().unwrap_or_default().contains("problem"), "{:?}", app.current_message());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), ONE_RULE);
    // the port must be a port
    form_at(&mut app, Row::Port);
    let text = press(&mut app, MEDIUM, "<Enter>99999<Enter>");
    assert!(text.contains("✗ port 99999: use 1 to 65535"), "{text}");
    // Esc asks; n stays, y leaves the file as it was
    press(&mut app, MEDIUM, "<Esc>");
    assert_eq!(app.overlay, Overlay::ConfirmDiscard);
    press(&mut app, MEDIUM, "n");
    assert!(app.form.is_some());
    press(&mut app, MEDIUM, "<Esc>y");
    assert!(app.form.is_none());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), ONE_RULE);
    std::fs::remove_dir_all(dir).unwrap();
}

/// K and J move a rule in the file (rules apply in file order), comments with it.
#[test]
fn rules_move_up_and_down_in_the_file() {
    let mut app = app_at(40.0);
    let two = format!(
        "{ONE_RULE}\n# second\n[[rule]]\nid = \"stub\"\n  [[rule.action]]\n  type = \"status\"\n  code = 503\n"
    );
    let (dir, path) = with_rules(&mut app, "rules-move", &two);
    press(&mut app, MEDIUM, "3jK");
    let ids = |app: &App| app.rules_file.as_ref().unwrap().entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&app), ["stub", "slow"]);
    assert_eq!(app.rules_cursor, 0, "the cursor stays on the moved rule");
    let moved = std::fs::read_to_string(&path).unwrap();
    assert!(moved.find("# second").unwrap() < moved.find("# keep me").unwrap(), "{moved}");
    press(&mut app, MEDIUM, "J");
    assert_eq!(ids(&app), ["slow", "stub"]);
    std::fs::remove_dir_all(dir).unwrap();
}

/// The form tries its match on what was captured, and lists those requests.
#[test]
fn the_rule_form_lists_what_its_match_selects() {
    let mut app = app_at(40.0);
    let (dir, _) = with_rules(&mut app, "form-matches", ONE_RULE);
    let to = goto(&mut app, MEDIUM, path_is("/api/v1/sessions"));
    let text = press(&mut app, MEDIUM, &format!("{to}r"));
    let (n, of) = app.form.as_ref().unwrap().matches.clone().expect("the match runs here");
    assert!(n >= 1 && n < of, "{n} of {of}");
    assert!(text.contains(&format!("the match selects {n} of the {of} captured requests")), "{text}");
    press(&mut app, MEDIUM, "G<Enter>");
    assert_eq!(app.view, traffic_police_tui::app::View::Connections);
    let store = app.view_store();
    let rows = app.view_rows().rows();
    assert_eq!(rows.len(), n);
    assert!(rows.iter().all(|r| store.txn(r.txn()).url.path == "/api/v1/sessions"));
    // 3 goes back to the form, as it was
    press(&mut app, MEDIUM, "3");
    assert!(app.form.is_some());
    std::fs::remove_dir_all(dir).unwrap();
}

/// A WebSocket: its row says `ws`, the Overview counts what went each way, the Response tab lists
/// every message after the handshake's headers, and Enter on one opens its payload.
#[test]
fn a_websocket_lists_its_messages_and_enter_opens_one() {
    use traffic_police_tui::app::Tab;
    let mut app = app_at(60.0);
    let to = goto(&mut app, MEDIUM, path_is("/ws/orders"));
    let text = press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    let txn = app.selected.expect("selected");
    {
        let t = app.view_store().txn(txn);
        assert_eq!(t.type_label(), "ws");
        assert_eq!(t.status(), Some(101));
        assert_eq!(t.ws.len(), 8, "{:?}", t.ws);
    }
    assert!(text.contains("closed · 2 sent (60 B)") && text.contains("6 received (374 B)"), "{text}");
    assert!(!text.contains("Response size"), "{text}");

    let text = press(&mut app, MEDIUM, "<Right>");
    assert_eq!(app.detail.tab, Tab::Response);
    assert!(text.contains("101 Switching Protocols"), "{text}");
    assert!(text.contains("Messages (8 · 2 sent, 6 received"), "{text}");
    let doc = traffic_police_tui::detail::build_doc(&mut app);
    let lines: Vec<String> =
        (0..doc.len()).filter_map(|i| traffic_police_tui::detail::row_text(&mut app, &doc, i, txn)).collect();
    let has = |dir: &str, what: &[&str]| lines.iter().any(|l| l.starts_with(dir) && what.iter().all(|w| l.contains(w)));
    assert!(has("↑ +", &["text", "{\"type\":\"subscribe\""]), "{lines:#?}");
    assert!(has("↓ +", &["text", "\"state\":\"delivered\""]), "{lines:#?}");
    assert!(has("↑ +", &["close", "1000 left the orders screen"]), "{lines:#?}");
    assert!(has("↓ +", &["close", "1000 bye"]), "{lines:#?}");
    assert_eq!(doc.messages.len(), 8);

    // Enter on the first message (the subscription the app sent) opens its payload
    let (row, i) = doc.messages[0];
    assert_eq!(i, 0);
    let text = press(&mut app, MEDIUM, &format!("g{}<Enter>", "j".repeat(row)));
    assert_eq!(app.overlay, Overlay::Decoded, "{text}");
    assert!(text.contains("message 1 · sent text"), "{text}");
    assert!(text.contains("\"channel\":\"orders\""), "{text}");
    press(&mut app, MEDIUM, "<Esc>");
    assert_eq!(app.overlay, Overlay::None);
}

/// gRPC: the Status column shows the call's status name, the Overview its code and message, the
/// Response tab the trailers after the body, and `grpc:` filters by status.
#[test]
fn a_grpc_error_shows_its_status_and_trailers() {
    use traffic_police_tui::app::Tab;
    let mut app = app_at(52.0);
    let to = goto(&mut app, MEDIUM, |t| t.grpc.as_ref().is_some_and(|g| g.code == 5));
    let text = press(&mut app, MEDIUM, &format!("{to}<Enter>"));
    let txn = app.selected.expect("selected");
    assert_eq!(app.view_store().txn(txn).status_text(), "NOT_FOUND");
    assert!(text.contains("NOT_FOUND"), "{text}");
    assert!(text.contains("gRPC status") && text.contains("NOT_FOUND (5): sku sku_9999"), "{text}");

    press(&mut app, MEDIUM, "<Right>");
    assert_eq!(app.detail.tab, Tab::Response);
    let doc = traffic_police_tui::detail::build_doc(&mut app);
    let lines: Vec<String> =
        (0..doc.len()).filter_map(|i| traffic_police_tui::detail::row_text(&mut app, &doc, i, txn)).collect();
    let at = |s: &str| lines.iter().position(|l| l == s).unwrap_or_else(|| panic!("{s:?} in {lines:#?}"));
    assert!(at("Trailers (2)") < at("grpc-status: 5"), "{lines:#?}");
    assert!(lines.iter().any(|l| l == "grpc-message: sku sku_9999 is not in warehouse blr-1"), "{lines:#?}");

    // the request: one gRPC message, decoded as protobuf
    press(&mut app, MEDIUM, "<Right>");
    let doc = traffic_police_tui::detail::build_doc(&mut app);
    let all: String = (0..doc.len())
        .filter_map(|i| traffic_police_tui::detail::row_text(&mut app, &doc, i, txn))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("sku_9999") && all.contains("blr-1"), "{all}");

    // the filter
    press(&mut app, MEDIUM, "<Esc>/grpc:not_found<Enter>");
    let store = app.view_store();
    let rows = app.view_rows().rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(store.txn(rows[0].txn()).grpc.as_ref().unwrap().name, "NOT_FOUND");
}

/// Neovim's quit commands after `:` quit too (`q` alone still does): the palette puts Quit first
/// for them, so Enter quits; another entry can still be chosen, and Esc goes back.
#[test]
fn colon_q_quits_like_neovim() {
    for keys in [":q<Enter>", ":q!<Enter>", ":qa<Enter>", ":wq<Enter>", ":x<Enter>", ":quit<Enter>"] {
        let mut app = app_at(5.0);
        press(&mut app, MEDIUM, keys);
        assert!(app.should_quit, "{keys} quits");
    }
    let mut app = app_at(5.0);
    let text = press(&mut app, MEDIUM, ":q");
    assert_eq!(app.overlay, Overlay::Palette);
    let first = text.lines().find(|l| l.contains("Quit")).expect("Quit is listed");
    assert!(first.contains(":q"), "{first}");
    press(&mut app, MEDIUM, "<Down><Enter>");
    assert!(!app.should_quit, "another entry was chosen");
    let mut app = app_at(5.0);
    press(&mut app, MEDIUM, ":q<Esc>");
    assert!(!app.should_quit);
    assert_eq!(app.overlay, Overlay::None);
    // the help says so
    let text = press(&mut app, MEDIUM, "?");
    assert!(text.contains(":q quit"), "{text}");
}

/// Logdawg (4): the app's lines by default, a filter typed (and one that does not parse), a line
/// opened and copied, following, the focus, and `x` clearing the log alone.
#[test]
fn logdawg_lists_the_apps_log_filters_opens_copies_and_clears() {
    use traffic_police_core::logdawg::Level;
    use traffic_police_tui::app::View;
    let mut app = app_at(40.0);
    let text = press(&mut app, MEDIUM, "4");
    assert_eq!(app.view, View::Logdawg);
    assert!(text.contains("╭─ Log ") && text.contains("OkHttp"), "{text}");
    let logs = app.view_store().logs();
    let (all, mine) = (logs.len(), app.logdawg.len());
    assert!(mine > 0 && mine < all, "{mine} of {all}");
    let line = |app: &App, i: usize| app.view_store().logs().get(app.logdawg.id(i).unwrap()).unwrap().uid;
    assert!((0..mine).all(|i| line(&app, i) == Some(10_234)), "package:mine is the app's uid");
    assert!(app.logdawg.follow && app.logdawg.cursor == mine - 1, "following, on the newest line");
    // a filter: every process's warnings and up
    press(&mut app, MEDIUM, "/<C-u>level:w<Enter>");
    assert_eq!(app.overlay, Overlay::None);
    let logs = app.view_store().logs();
    assert!(!app.logdawg.is_empty());
    assert!((0..app.logdawg.len()).all(|i| logs.get(app.logdawg.id(i).unwrap()).unwrap().level >= Level::Warn));
    // one that does not parse is marked, and Esc puts back the one before
    press(&mut app, MEDIUM, "/<C-u>level:loud");
    assert!(app.log_filter_error.as_ref().is_some_and(|e| e.message.contains("not a level")));
    press(&mut app, MEDIUM, "<Esc>");
    assert_eq!((app.log_filter_input.value(), app.log_filter_error.is_none()), ("level:w", true));
    // up a line: no longer following; Enter opens it, y copies it as logcat prints it
    press(&mut app, MEDIUM, "k");
    assert!(!app.logdawg.follow);
    let id = app.logdawg.id(app.logdawg.cursor).unwrap();
    let (tag, message) = {
        let l = app.view_store().logs().get(id).unwrap();
        (l.tag.to_string(), l.message.to_string())
    };
    let text = press(&mut app, MEDIUM, "<Enter>");
    assert_eq!(app.overlay, Overlay::Decoded);
    assert!(text.contains(&tag), "{text}");
    press(&mut app, MEDIUM, "<Esc>y");
    let copied = app.copied.clone().unwrap();
    assert!(copied.contains(&format!(" {tag}: {}", message.lines().next().unwrap())), "{copied}");
    press(&mut app, MEDIUM, "G");
    assert!(app.logdawg.follow, "G follows again");
    // Tab: the graph and the log (the request's boxes wait)
    press(&mut app, MEDIUM, "<Tab>");
    assert_eq!(app.focus, Focus::Graph);
    press(&mut app, MEDIUM, "<Tab>");
    assert_eq!(app.focus, Focus::List);
    // x clears the log alone
    let requests = app.view_store().len();
    press(&mut app, MEDIUM, "xy");
    assert_eq!((app.view_store().logs().len(), app.logdawg.len()), (0, 0));
    assert_eq!(app.view_store().len(), requests, "the requests stay");
    let text = press(&mut app, MEDIUM, "1");
    assert!(text.contains("╭─ Requests "), "{text}");
}

/// New lines move the cursor along only while it is on the newest; a frozen view keeps them
/// waiting; a click puts the cursor on a line and the wheel scrolls.
#[test]
fn logdawg_follows_new_lines_freezes_and_takes_the_mouse() {
    use traffic_police_core::SessionEvent;
    use traffic_police_core::logdawg::{Level, LogLine};
    let mut app = app_at(12.0);
    press(&mut app, MEDIUM, "4");
    let line = |ts, msg: &str| LogLine {
        ts,
        wall_ms: 0,
        pid: 4312,
        tid: 4312,
        uid: Some(10_234),
        level: Level::Info,
        buffer: 0,
        tag: "Test".into(),
        message: msg.into(),
    };
    let message_at =
        |app: &App, i: usize| app.view_store().logs().get(app.logdawg.id(i).unwrap()).unwrap().message.to_string();
    let ts = app.now();
    app.ingest(vec![SessionEvent::Logs(vec![line(ts, "one")])]);
    press(&mut app, MEDIUM, "");
    assert_eq!(message_at(&app, app.logdawg.cursor), "one", "the cursor came along");
    // up a line: a new one does not move it
    press(&mut app, MEDIUM, "k");
    let at = app.logdawg.cursor;
    app.ingest(vec![SessionEvent::Logs(vec![line(ts + 1, "two")])]);
    press(&mut app, MEDIUM, "");
    assert_eq!(app.logdawg.cursor, at);
    // frozen: new lines wait until it thaws
    press(&mut app, MEDIUM, "F");
    let shown = app.logdawg.len();
    app.ingest(vec![SessionEvent::Logs(vec![line(ts + 2, "three")])]);
    press(&mut app, MEDIUM, "");
    assert_eq!(app.logdawg.len(), shown, "frozen");
    press(&mut app, MEDIUM, "F");
    assert_eq!(app.logdawg.len(), shown + 1, "thawed");
    // a click on a line, and the wheel
    press(&mut app, MEDIUM, "");
    let r = app.hits.rect_of(Target::LogLine(app.logdawg.len() - 3)).expect("a line on screen");
    click(&mut app, r.x + 3, r.y);
    assert_eq!((app.logdawg.cursor, app.logdawg.follow), (app.logdawg.len() - 3, false));
    let top = app.logdawg.top;
    app.handle_mouse(mouse(MouseEventKind::ScrollUp, r.x + 3, r.y));
    assert!(app.logdawg.top < top, "the wheel moved the view up");
}
