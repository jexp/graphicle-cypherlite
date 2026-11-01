//! Dynamic label/property-key evaluation and label-expression matching.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{DynamicLabelQuantifier, Expr, LabelExpr};

use crate::overlay::ExecGraph;

use super::{eval, Row};

pub(crate) fn dynamic_property_key(value: Value) -> Result<String> {
    match value {
        Value::String(key) => Ok(String::from(&*key)),
        _ => Err(Error::invalid_argument_type(
            "dynamic property key must be of type STRING NOT NULL",
        )),
    }
}

pub(crate) fn dynamic_label_names(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    expr: &Expr,
    cypher_version: Option<u32>,
) -> Result<Vec<String>> {
    fn valid(name: &str) -> Result<String> {
        if name.is_empty() || name.contains('\0') {
            Err(Error::semantic(
                "InvalidLabelExpression: dynamic label/type names must be non-empty and contain no null bytes",
            ))
        } else {
            Ok(name.to_string())
        }
    }
    match eval(graph, row, expr, params, cypher_version)? {
        Value::String(name) => Ok(vec![valid(&name)?]),
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::String(name) => valid(name),
                _ => Err(Error::invalid_argument_type(
                    "label/type list elements must be STRING NOT NULL",
                )),
            })
            .collect(),
        _ => Err(Error::invalid_argument_type(
            "dynamic label/type must be STRING NOT NULL or LIST<STRING NOT NULL>",
        )),
    }
}

pub(crate) fn label_expr_matches(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    expr: &LabelExpr,
    labels: &[String],
    is_rel: bool,
    cypher_version: Option<u32>,
) -> Result<bool> {
    Ok(match expr {
        LabelExpr::Label(name) => labels.iter().any(|label| label == name),
        LabelExpr::Wildcard => is_rel || !labels.is_empty(),
        LabelExpr::Dynamic { expr, quantifier } => {
            let names = dynamic_label_names(graph, row, params, expr, cypher_version)?;
            match quantifier {
                DynamicLabelQuantifier::Any => names
                    .iter()
                    .any(|name| labels.iter().any(|label| label == name)),
                DynamicLabelQuantifier::Default | DynamicLabelQuantifier::All => names
                    .iter()
                    .all(|name| labels.iter().any(|label| label == name)),
            }
        }
        LabelExpr::Not(inner) => {
            !label_expr_matches(graph, row, params, inner, labels, is_rel, cypher_version)?
        }
        LabelExpr::And(items) => {
            let mut matched = true;
            for item in items {
                if !label_expr_matches(graph, row, params, item, labels, is_rel, cypher_version)? {
                    matched = false;
                    break;
                }
            }
            matched
        }
        LabelExpr::Or(items) => {
            let mut matched = false;
            for item in items {
                if label_expr_matches(graph, row, params, item, labels, is_rel, cypher_version)? {
                    matched = true;
                    break;
                }
            }
            matched
        }
    })
}

pub(crate) fn resolve_write_labels(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    items: &[LabelExpr],
    cypher_version: Option<u32>,
) -> Result<Vec<String>> {
    let mut labels = Vec::new();
    for item in items {
        match item {
            LabelExpr::Label(name) => labels.push(name.clone()),
            LabelExpr::Dynamic { expr, quantifier } => {
                if *quantifier == DynamicLabelQuantifier::Any {
                    return Err(Error::semantic(
                        "InvalidLabelExpression: $any() is not allowed in SET/REMOVE",
                    ));
                }
                labels.extend(dynamic_label_names(
                    graph,
                    row,
                    params,
                    expr,
                    cypher_version,
                )?);
            }
            _ => {
                return Err(Error::semantic(
                    "InvalidLabelExpression: SET/REMOVE only supports plain or dynamic labels",
                ))
            }
        }
    }
    let mut seen = HashSet::new();
    labels.retain(|label| seen.insert(label.clone()));
    Ok(labels)
}
