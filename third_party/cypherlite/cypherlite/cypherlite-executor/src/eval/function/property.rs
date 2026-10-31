//! Property special forms: `property_exists`, whose property-name argument
//! is an identifier literal rather than an evaluated expression.

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::Expr;

use crate::overlay::ExecGraph;

use super::super::{eval, Row};

/// The `property_exists` special form. Returns `None` when `n` is not
/// `property_exists`.
pub(super) fn property_exists_form(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    n: &str,
    args: &[Expr],
    cypher_version: Option<u32>,
) -> Option<Result<Value>> {
    if n != "property_exists" {
        return None;
    }
    Some(property_exists(graph, row, params, args, cypher_version))
}

// PROPERTY_EXISTS uses an identifier as a property-name literal. It must
// not be evaluated as a row variable (CIP-247).
fn property_exists(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    args: &[Expr],
    cypher_version: Option<u32>,
) -> Result<Value> {
    if args.len() != 2 {
        return Err(Error::syntax(
            "InvalidNumberOfArguments: property_exists() requires two arguments",
        ));
    }
    let element = eval(graph, row, &args[0], params, cypher_version)?;
    let property = match &args[1] {
        Expr::Variable(name) => name.as_str(),
        Expr::Null => "null",
        Expr::String(name) => name.as_str(),
        _ => {
            return Err(Error::invalid_argument_type(
                "property name must be an identifier",
            ))
        }
    };
    Ok(match element {
        Value::Node(node) => Value::Bool(
            graph
                .node(node.id)
                .and_then(|node| node.properties.get(property).cloned())
                .is_some_and(|value| !value.is_null()),
        ),
        Value::Relationship(rel) => Value::Bool(
            graph
                .rel(rel.id)
                .and_then(|rel| rel.properties.get(property).cloned())
                .is_some_and(|value| !value.is_null()),
        ),
        Value::Null => Value::Null,
        _ => {
            return Err(Error::invalid_argument_type(
                "expected NODE or RELATIONSHIP",
            ))
        }
    })
}
