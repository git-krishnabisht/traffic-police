//! HAR export against the HAR 1.2 JSON schema (`testdata/har-schema`, har-schema 2.0.0) and the
//! rules of the HAR 1.2 specification that the schema leaves out: timings that are never
//! negative except for -1, `time` as the sum of the timings, `ssl` inside `connect`, and query
//! strings that match the URL. The traffic is the demo's: redirects, failures, gzip, binary and
//! form bodies, a rule's rewrite, a cancelled call, a timeout, a WebSocket and gRPC calls.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;
use traffic_police_backends::demo::{DemoConfig, DemoSession};
use traffic_police_core::export::har::har;
use traffic_police_core::fmt::{NS_PER_MS, NS_PER_SEC};
use traffic_police_core::store::SessionStore;

/// The vendored schema files, by `$id` (`entry.json#`).
fn schemas() -> HashMap<String, Value> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../testdata/har-schema");
    let mut out = HashMap::new();
    for e in std::fs::read_dir(&dir).expect("testdata/har-schema") {
        let path = e.unwrap().path();
        if path.extension().is_some_and(|x| x == "json") {
            let v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            out.insert(v["$id"].as_str().expect("an $id").to_string(), v);
        }
    }
    assert!(out.len() >= 18, "{} schema files", out.len());
    out
}

/// A strict validator for the part of JSON Schema (draft 06) these files use. A keyword it does
/// not know fails the test, so nothing in the schema is skipped silently. (`min`, `optional` and
/// `unique` are not JSON Schema keywords, so validators ignore them; the spec checks below cover
/// what `min` meant.)
fn validate(schemas: &HashMap<String, Value>, schema: &Value, v: &Value, path: &str, problems: &mut Vec<String>) {
    let Some(s) = schema.as_object() else { return problems.push(format!("{path}: schema is not an object")) };
    for (k, rule) in s {
        match k.as_str() {
            "$id" | "$schema" | "optional" | "min" | "unique" | "description" => {}
            "$ref" => {
                let target = schemas.get(rule.as_str().unwrap()).unwrap_or_else(|| panic!("no schema {rule}"));
                validate(schemas, target, v, path, problems);
            }
            "type" => {
                let types: Vec<&str> = match rule {
                    Value::String(t) => vec![t.as_str()],
                    Value::Array(a) => a.iter().map(|t| t.as_str().unwrap()).collect(),
                    _ => panic!("type {rule}"),
                };
                let ok = types.iter().any(|t| match *t {
                    "object" => v.is_object(),
                    "array" => v.is_array(),
                    "string" => v.is_string(),
                    "number" => v.is_number(),
                    "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
                    "boolean" => v.is_boolean(),
                    "null" => v.is_null(),
                    other => panic!("type {other}"),
                });
                if !ok {
                    problems.push(format!("{path}: {v} is not {types:?}"));
                }
            }
            "required" => {
                if let Some(o) = v.as_object() {
                    for name in rule.as_array().unwrap() {
                        if !o.contains_key(name.as_str().unwrap()) {
                            problems.push(format!("{path}: {name} is missing"));
                        }
                    }
                }
            }
            "properties" => {
                if let Some(o) = v.as_object() {
                    for (name, sub) in rule.as_object().unwrap() {
                        if let Some(x) = o.get(name) {
                            validate(schemas, sub, x, &format!("{path}.{name}"), problems);
                        }
                    }
                }
            }
            "items" => {
                if let Some(a) = v.as_array() {
                    for (i, x) in a.iter().enumerate() {
                        validate(schemas, rule, x, &format!("{path}[{i}]"), problems);
                    }
                }
            }
            "oneOf" => {
                let passing = rule
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|sub| {
                        let mut p = Vec::new();
                        validate(schemas, sub, v, path, &mut p);
                        p.is_empty()
                    })
                    .count();
                if passing != 1 {
                    problems.push(format!("{path}: {v} matches {passing} of oneOf {rule}"));
                }
            }
            "pattern" => {
                if let Some(text) = v.as_str() {
                    let re = regex::Regex::new(rule.as_str().unwrap()).unwrap();
                    if !re.is_match(text) {
                        problems.push(format!("{path}: {text:?} does not match {rule}"));
                    }
                }
            }
            "format" => {
                if let Some(text) = v.as_str()
                    && !format_ok(rule.as_str().unwrap(), text)
                {
                    problems.push(format!("{path}: {text:?} is not a {rule}"));
                }
            }
            other => panic!("{path}: the schema uses {other:?}, which this validator does not know"),
        }
    }
}

fn format_ok(format: &str, text: &str) -> bool {
    match format {
        // RFC 3339
        "date-time" => {
            regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?(Z|[+-]\d\d:\d\d)$").unwrap().is_match(text)
        }
        // RFC 3986: a scheme, then no spaces or controls
        "uri" => {
            regex::Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*:").unwrap().is_match(text)
                && !text.chars().any(|c| c.is_whitespace() || c.is_control())
        }
        "ipv4" => text.parse::<std::net::Ipv4Addr>().is_ok(),
        "ipv6" => text.parse::<std::net::Ipv6Addr>().is_ok(),
        other => panic!("format {other}"),
    }
}

/// The HAR 1.2 specification's rules that the schema does not express.
fn spec_checks(entry: &Value, path: &str, problems: &mut Vec<String>) {
    let t = &entry["timings"];
    let num = |k: &str| t[k].as_f64();
    for k in ["blocked", "dns", "connect", "ssl"] {
        if let Some(x) = num(k)
            && x < 0.0
            && x != -1.0
        {
            problems.push(format!("{path}.timings.{k}: {x} (only -1 may be negative)"));
        }
    }
    for k in ["send", "wait", "receive"] {
        match num(k) {
            Some(x) if x >= 0.0 => {}
            other => problems.push(format!("{path}.timings.{k}: {other:?} (required, never negative)")),
        }
    }
    // ssl is part of connect (HAR 1.1 compatibility)
    if let (Some(ssl), Some(connect)) = (num("ssl"), num("connect"))
        && ssl >= 0.0
        && (connect < 0.0 || ssl > connect + 0.001)
    {
        problems.push(format!("{path}.timings: ssl {ssl} is not inside connect {connect}"));
    }
    // time is the sum of the timings that apply (ssl is counted in connect)
    let sum: f64 = ["blocked", "dns", "connect", "send", "wait", "receive"]
        .iter()
        .filter_map(|k| num(k))
        .filter(|x| *x >= 0.0)
        .sum();
    let time = entry["time"].as_f64().unwrap_or(-1.0);
    if (time - sum).abs() > 0.01 {
        problems.push(format!("{path}: time {time} is not the sum of its timings {sum} ({t})"));
    }
    // the query string is the URL's
    let url = entry["request"]["url"].as_str().unwrap_or_default();
    let has_query = url.split_once('?').is_some_and(|(_, q)| !q.split('#').next().unwrap_or("").is_empty());
    let pairs = entry["request"]["queryString"].as_array().map_or(0, Vec::len);
    if has_query != (pairs > 0) {
        problems.push(format!("{path}.request.queryString: {pairs} pairs for {url}"));
    }
    for which in ["request", "response"] {
        for k in ["headersSize", "bodySize"] {
            if let Some(x) = entry[which][k].as_i64()
                && x < -1
            {
                problems.push(format!("{path}.{which}.{k}: {x}"));
            }
        }
    }
    if let Some(c) = entry["response"]["content"]["compression"].as_i64()
        && c < 0
    {
        problems.push(format!("{path}.response.content.compression: {c} (bytes saved)"));
    }
}

fn demo_store(secs: u64) -> SessionStore {
    let mut store = SessionStore::new();
    let mut session = DemoSession::new(DemoConfig::default(), store.source_ids());
    let end = secs * NS_PER_SEC;
    let mut t = 0;
    while t < end {
        t = (t + 25 * NS_PER_MS).min(end);
        for e in session.advance(t) {
            store.apply(e);
        }
    }
    store
}

#[test]
fn the_har_export_follows_har_1_2() {
    let schemas = schemas();
    let store = demo_store(60);
    let all: Vec<u32> = (0..store.len() as u32).collect();
    assert!(all.len() > 30, "{} requests", all.len());
    let doc = har(&store, &all, store.latest());
    let mut problems = Vec::new();
    validate(&schemas, &schemas["har.json#"], &doc, "har", &mut problems);
    let entries = doc["log"]["entries"].as_array().unwrap();
    for (i, e) in entries.iter().enumerate() {
        spec_checks(e, &format!("entries[{i}]"), &mut problems);
    }
    // the cases worth having are in there
    let has = |f: &dyn Fn(&Value) -> bool| entries.iter().any(f);
    assert!(has(&|e| e["response"]["status"] == 302), "a redirect");
    assert!(has(&|e| e["response"]["content"]["encoding"] == "base64"), "a binary body");
    assert!(has(&|e| e["request"]["postData"].is_object()), "a request body");
    assert!(has(&|e| e["_trafficPolice"]["failure"].is_object()), "a failure");
    assert!(has(&|e| e["_trafficPolice"]["original"].is_object()), "a rule's rewrite");
    assert!(
        has(&|e| e["response"]["status"] == 101 && e["_webSocketMessages"].as_array().is_some_and(|m| m.len() == 8)),
        "a WebSocket and its messages"
    );
    assert!(
        has(&|e| e["_trafficPolice"]["grpc"]["status"] == "NOT_FOUND"
            && e["_trafficPolice"]["trailers"][0]["name"] == "grpc-status"),
        "a gRPC error with its trailers"
    );
    assert!(problems.is_empty(), "{} problems:\n{}", problems.len(), problems.join("\n"));
}
