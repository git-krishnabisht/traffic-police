//! jq-style filters over JSON bodies, using jaq.
//!
//! `halt` and runtime errors become [`JqError`]s (jaq's own `unwrap_valr` would exit the
//! process on `halt`). A filter can run forever or recurse deeply, so the UI runs this in a
//! child process with a time limit rather than in its own threads.

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data};
use jaq_json::Val;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JqError {
    #[error("filter: {message} (at column {column})")]
    Syntax { message: String, column: usize },
    #[error("filter: {0}")]
    Compile(String),
    #[error("input is not JSON: {0}")]
    Input(String),
    #[error("{0}")]
    Runtime(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JqOutput {
    /// Each output value as compact JSON text.
    pub values: Vec<String>,
    /// More values were produced than `max_outputs`.
    pub truncated: bool,
}

/// Run `filter` over `input` (one JSON document), keeping at most `max_outputs` results.
pub fn run(filter: &str, input: &[u8], max_outputs: usize) -> Result<JqOutput, JqError> {
    let input = jaq_json::read::parse_single(input).map_err(|e| JqError::Input(e.to_string()))?;
    let program = File { code: filter, path: () };
    let defs = jaq_core::defs().chain(jaq_std::defs()).chain(jaq_json::defs());
    let funs = jaq_core::funs().chain(jaq_std::funs()).chain(jaq_json::funs());
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader.load(&arena, program).map_err(|errs| {
        let mut first: Option<(String, usize)> = None;
        for (_file, err) in errs {
            let (message, part) = match err {
                jaq_core::load::Error::Io(v) => (v.first().map(|(_, m)| m.clone()).unwrap_or_default(), None),
                jaq_core::load::Error::Lex(v) => match v.first() {
                    Some((expect, part)) => (format!("expected {}", expect.as_str()), Some(*part)),
                    None => ("syntax error".into(), None),
                },
                jaq_core::load::Error::Parse(v) => match v.first() {
                    Some((expect, part)) => {
                        let found = if part.is_empty() {
                            "end of filter".to_string()
                        } else {
                            format!("`{}`", part.chars().take(12).collect::<String>())
                        };
                        (format!("expected {}, found {found}", expect.as_str()), Some(*part))
                    }
                    None => ("syntax error".into(), None),
                },
            };
            let column = part.map(|p| jaq_core::load::span(filter, p).start + 1).unwrap_or(1);
            first.get_or_insert((message, column));
        }
        let (message, column) = first.unwrap_or(("syntax error".into(), 1));
        JqError::Syntax { message, column }
    })?;
    let filter_c = Compiler::default().with_funs(funs).compile(modules).map_err(|errs| {
        let names: Vec<String> = errs
            .into_iter()
            .flat_map(|(_f, es)| {
                es.into_iter().map(|(name, undef)| format!("undefined {} `{name}`", undefined_kind(&undef)))
            })
            .collect();
        JqError::Compile(names.join("; "))
    })?;
    let ctx = Ctx::<data::JustLut<Val>>::new(&filter_c.lut, Vars::new([]));
    let mut values = Vec::new();
    let mut truncated = false;
    for out in filter_c.id.run((ctx, input)) {
        match out {
            Ok(v) => {
                if values.len() == max_outputs {
                    truncated = true;
                    break;
                }
                values.push(v.to_string());
            }
            Err(exn) => {
                let message = match exn.get_err() {
                    Ok(err) => err.to_string(),
                    Err(exn) => match exn.get_halt() {
                        Ok(code) => format!("halt with exit code {code}"),
                        Err(_) => "filter raised an exception".into(),
                    },
                };
                return Err(JqError::Runtime(message));
            }
        }
    }
    Ok(JqOutput { values, truncated })
}

fn undefined_kind(u: &jaq_core::compile::Undefined) -> &'static str {
    use jaq_core::compile::Undefined;
    match u {
        Undefined::Mod => "module",
        Undefined::Var => "variable",
        Undefined::Label => "label",
        Undefined::Filter(_) => "filter",
        _ => "name",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_filters_and_reports_errors() {
        let input = br#"{"data":{"items":[{"n":1},{"n":2},{"n":3}]},"big":12345678901234567890}"#;
        assert_eq!(run(".data.items[].n", input, 10).unwrap().values, vec!["1", "2", "3"]);
        assert_eq!(run(".big", input, 10).unwrap().values, vec!["12345678901234567890"]);
        let t = run(".data.items[]", input, 2).unwrap();
        assert!(t.truncated);
        assert_eq!(t.values.len(), 2);
        assert!(matches!(run(".data.items[", input, 10), Err(JqError::Syntax { .. })));
        assert!(matches!(run("nosuchfn", input, 10), Err(JqError::Compile(_))));
        assert!(matches!(run("error(\"boom\")", input, 10), Err(JqError::Runtime(m)) if m.contains("boom")));
        assert!(matches!(run("halt", input, 10), Err(JqError::Runtime(m)) if m.contains("halt")));
        assert!(matches!(run(".", b"{oops", 10), Err(JqError::Input(_))));
    }
}
