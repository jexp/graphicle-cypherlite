//! Shared test helpers for the `procedure_rows` test modules (`mod.rs` and
//! the `schema` module): run a built-in procedure against a graph and
//! compare its rows.

use std::collections::BTreeMap;

use cypherlite_core::Value;

use super::builtin_procedure;
use crate::graph::Graph;
use crate::overlay::ExecGraph;

pub(super) fn string(value: &str) -> Value {
    Value::String(value.into())
}

pub(super) fn map(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(key, value)| (key.to_string(), value.clone()))
        .collect()
}

pub(super) fn rows(graph: &Graph, name: &str) -> Vec<BTreeMap<String, Value>> {
    rows_with_principal(graph, name, None)
}

pub(super) fn rows_with_principal(
    graph: &Graph,
    name: &str,
    principal: Option<&str>,
) -> Vec<BTreeMap<String, Value>> {
    let view = ExecGraph::read(graph);
    builtin_procedure(&view, name, principal)
        .expect("schema procedure is executable")
        .rows
}
