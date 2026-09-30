//! Whole-frame snapshots of every view, tab and state, at the three reference sizes, rendered
//! from the demo in virtual time (deterministic: fixed seed, fixed clock, half-block images).
//!
//! Update after an intended change with `INSTA_UPDATE=always cargo test -p traffic-police-tui`,
//! then review the diff of `tests/snapshots/`.

mod common;

use common::*;
use traffic_police_backends::demo::DemoConfig;
use traffic_police_tui::Theme;

macro_rules! snap {
    ($name:literal, $frame:expr) => {
        insta::assert_snapshot!($name, $frame);
    };
}

// --- Connection View -------------------------------------------------------------------------

#[test]
fn connections_at_each_size() {
    snap!("connections_small", frame(12.0, SMALL, ""));
    snap!("connections_medium", frame(12.0, MEDIUM, ""));
    snap!("connections_large", frame(40.0, LARGE, ""));
}

#[test]
fn connections_empty_before_traffic() {
    snap!("connections_empty", frame(0.0, MEDIUM, ""));
}

#[test]
fn connections_collapsed_and_expanded() {
    snap!("connections_collapsed", frame(40.0, MEDIUM, "c"));
    let mut app = app_at(40.0);
    press(&mut app, MEDIUM, "c");
    let group = app
        .view_rows()
        .rows()
        .iter()
        .rposition(|r| matches!(r, traffic_police_core::rows::Row::Group { .. }))
        .expect("a group of repeated calls");
    let up = app.view_rows().len() - 1 - group;
    snap!("connections_group_expanded", press(&mut app, MEDIUM, &format!("G{}l", "k".repeat(up))));
}

#[test]
fn connections_sorted_and_extra_columns() {
    snap!("connections_sorted_by_size_desc", frame(12.0, MEDIUM, "ssS"));
    snap!("connections_extra_columns", frame(12.0, LARGE, "C<Enter>j<Enter>jjj<Enter><Esc>"));
    snap!("columns_menu", frame(12.0, MEDIUM, "C<Enter>j"));
}

// --- graph -----------------------------------------------------------------------------------

#[test]
fn graph_states() {
    snap!("graph_captured_requests", frame(12.0, MEDIUM, "T"));
    snap!("graph_wall_clock_zoomed_in", frame(12.0, MEDIUM, "t++"));
    snap!("graph_zoomed_out", frame(40.0, MEDIUM, "--"));
    snap!("graph_range_selected", frame(40.0, MEDIUM, "vhhhhhhhhhhv"));
    snap!("graph_range_in_progress", frame(40.0, MEDIUM, "vhhhhh"));
}

// --- Thread View and Rules -------------------------------------------------------------------

#[test]
fn thread_view() {
    snap!("threads_medium", frame(12.0, MEDIUM, "2"));
    snap!("threads_large", frame(40.0, LARGE, "2"));
    snap!("threads_bar_selected_with_detail", frame(12.0, MEDIUM, "2jl<Enter>"));
}

#[test]
fn rules_view() {
    snap!("rules", frame(40.0, MEDIUM, "3"));
    snap!("rules_second", frame(40.0, MEDIUM, "3j"));
}

// --- detail pane -----------------------------------------------------------------------------

#[test]
fn detail_tabs_for_json_post() {
    let init = || path_is("/api/sdk/init");
    snap!("detail_overview_json", detail(12.0, MEDIUM, init(), ""));
    snap!("detail_response_json", detail(12.0, MEDIUM, init(), "l"));
    snap!("detail_response_json_source", detail(12.0, MEDIUM, init(), "lp"));
    snap!(
        "detail_response_json_folded",
        detail(
            12.0,
            MEDIUM,
            init(),
            "l[<Down><Down><Down><Down><Down><Down><Down><Down><Down><Down><Down><Down><Down><Down><Enter>"
        )
    );
    snap!("detail_request_json", detail(12.0, MEDIUM, init(), "ll"));
    snap!("detail_call_stack", detail(12.0, MEDIUM, init(), "lll"));
    snap!("detail_call_stack_expanded", detail(12.0, MEDIUM, init(), "lll<Down><Down><Down><Enter>"));
}

#[test]
fn detail_small_covers_list() {
    snap!("detail_small", detail(12.0, SMALL, path_is("/api/sdk/init"), "l"));
}

#[test]
fn detail_body_viewers() {
    snap!("detail_image_overview", detail(12.0, MEDIUM, path_ends(".png"), ""));
    snap!("detail_image_response", detail(12.0, MEDIUM, path_ends(".png"), "l"));
    snap!("detail_protobuf_request", detail(12.0, MEDIUM, path_is("/v1/metrics"), "ll"));
    snap!("detail_protobuf_response", detail(12.0, MEDIUM, path_is("/v1/metrics"), "l"));
    snap!("detail_html_500", detail(12.0, MEDIUM, |t| t.status() == Some(500), "l"));
    snap!("detail_404", detail(12.0, MEDIUM, |t| t.status() == Some(404), "l"));
    snap!("detail_binary_hex", detail(40.0, MEDIUM, path_ends(".tflite"), "l"));
    snap!("detail_query_params", detail(40.0, MEDIUM, |t| t.url.query.is_some(), "ll"));
}

#[test]
fn detail_failures_and_hops() {
    snap!("detail_timeout", detail(40.0, MEDIUM, path_ends("/threshold"), ""));
    snap!("detail_timeout_response", detail(40.0, MEDIUM, path_ends("/threshold"), "l"));
    snap!("detail_canceled", detail(12.0, MEDIUM, path_ends("fonts.json"), ""));
    snap!("detail_redirect_hop", detail(12.0, MEDIUM, |t| t.hop > 0, ""));
    snap!("detail_in_flight", detail(12.0, MEDIUM, |t| t.state.is_open(), ""));
}

#[test]
fn detail_rule_rewritten_response() {
    let modified = || |t: &traffic_police_core::model::Transaction| t.rule_modified();
    snap!("rule_delivered", detail(40.0, MEDIUM, modified(), "l"));
    snap!("rule_original", detail(40.0, MEDIUM, modified(), "lo"));
    snap!("rule_overview", detail(40.0, LARGE, modified(), "G"));
}

#[test]
fn jq_filter() {
    let init = || path_is("/api/sdk/init");
    snap!("jq_prompt", detail(12.0, MEDIUM, init(), "l|.config"));
    snap!("jq_result", detail(12.0, MEDIUM, init(), "l|.config.features<Enter>"));
    snap!("jq_error", detail(12.0, MEDIUM, init(), "l|.config[<Enter>"));
}

// --- session states and overlays -------------------------------------------------------------

#[test]
fn session_states() {
    snap!("paused", frame(12.0, MEDIUM, "<Space>"));
    snap!("frozen", frame(12.0, MEDIUM, "F"));
    let restart = DemoConfig { restart_after_ns: Some(20_000_000_000), ..DemoConfig::default() };
    let mut detached = app_with(21.0, restart.clone(), Theme::default());
    snap!("detached", press(&mut detached, MEDIUM, ""));
    let mut reattached = app_with(30.0, restart, Theme::default());
    snap!("reattached", press(&mut reattached, MEDIUM, ""));
}

#[test]
fn overlays() {
    snap!("help", frame(12.0, MEDIUM, "?"));
    snap!("help_small", frame(12.0, SMALL, "?"));
    snap!("confirm_clear", frame(12.0, MEDIUM, "x"));
    snap!("cleared", frame(12.0, MEDIUM, "xy"));
}

#[test]
fn terminal_too_small() {
    snap!("too_small", frame(12.0, (99, 30), ""));
    snap!("too_short", frame(12.0, (140, 29), ""));
}
