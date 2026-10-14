//! Whatever is typed into a filter bar: view 1's filter (`filter.rs`) and Logdawg's
//! (`logdawg/filter.rs`) parse it, or say where it goes wrong at a place in the text (the
//! commands without the UI underline that place, slicing the text there), and never panic. The
//! `filters` fuzz target (host/fuzz) goes further, matching what parses against requests and log
//! lines.

use proptest::prelude::*;
use traffic_police_core::filter::{Filter, ParseError};
use traffic_police_core::logdawg::filter::LogFilter;

fn at_a_place(text: &str, e: &ParseError) {
    assert!(text.get(e.span.clone()).is_some(), "{text:?}: the error is at {:?}", e.span);
}

fn parse_both(text: &str) {
    if let Err(e) = Filter::parse(text) {
        at_a_place(text, &e);
    }
    if let Err(e) = LogFilter::parse(text) {
        at_a_place(text, &e);
    }
}

/// The languages' own pieces, split at the spaces: keys, words, operators, quotes, regexes, sizes
/// and times, wide and odd characters.
const PIECES: &str = "method: status: grpc: host: path: type: thread: rule: is: header: body: size time package: \
    process: tag: tag=: tag~: message: msg~: line: level: age: pid: tid: uid: name: mine crash stacktrace 4xx >= \
    < = 10k 2mb 500ms 1.5s 5m 1d - \" ' \\ / /i * ** ? ( ) | & , : é 日本 \u{a0} \u{0}";

/// Text made of those pieces, of spaces, and of anything else, so that much of it gets past the
/// first term.
fn filter_text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        prop::sample::select(PIECES.split(' ').map(str::to_string).collect::<Vec<_>>()),
        prop::sample::select(vec![" ".to_string(), "  ".to_string(), "\t".to_string(), "\\ ".to_string()]),
        "[a-z0-9.]{1,6}",
        "\\PC{1,3}",
        any::<char>().prop_map(String::from),
    ];
    prop::collection::vec(piece, 0..16).prop_map(|pieces| pieces.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn filter_language_pieces_never_panic(text in filter_text()) {
        parse_both(&text);
    }

    #[test]
    fn arbitrary_strings_never_panic(text in any::<String>()) {
        parse_both(&text);
    }
}

/// A few that a fuzzer might take a while to reach: an error at the very end, after a character
/// of several bytes, and in an unclosed group.
#[test]
fn errors_at_the_end_and_after_wide_characters() {
    for text in ["日本 \"", "tag:é (", "(((", "status:4x日", "é -", "size>", "level:", "/(/", "a | ", "\u{a0}|"] {
        parse_both(text);
    }
}
