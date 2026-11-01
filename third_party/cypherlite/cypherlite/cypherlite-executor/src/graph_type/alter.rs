//! CIP-176/182/183: the `ALTER CURRENT GRAPH TYPE` per-verb body algebra.
//!
//! Each op is a pure `GraphTypeBody -> GraphTypeBody` transformation with
//! the verb's validation folded in; the executor's catalog handler
//! (`exec::catalog_schema::alter_current_graph_type`) keeps only the
//! snapshot/redo orchestration and the derived-constraint catalog sync.
//! Error variants and messages are byte-identical to the historical
//! inline implementation (the prime invariant).

use std::collections::{HashMap, HashSet};

use cypherlite_core::{Error, PreviewFeatures, Result};
use cypherlite_parser::{AlterGraphTypeVerb, GraphTypeBody, GraphTypeElementRef};

use super::{
    edge_type_matches_exactly, is_bare_edge_type_ref, is_bare_node_type_ref,
    node_type_matches_exactly, validate_graph_type_body,
};

/// Applies one `ALTER CURRENT GRAPH TYPE` verb's body to the current body,
/// returning the resulting body. Pure body algebra: no catalog access, no
/// redo. `preview` is the store's `CYPHERLITE_PREVIEW`-resolved set
/// (ADR-0371): the merged-body validation threads it, so the graph-type
/// trio's stored-state gates (an `ADD` whose identifying label a stored
/// node element type already implies) fail closed with the same authored
/// error statement admission produces.
pub fn apply_verb(
    verb: AlterGraphTypeVerb,
    current: GraphTypeBody,
    body: &GraphTypeBody,
    preview: PreviewFeatures,
) -> Result<GraphTypeBody> {
    match verb {
        AlterGraphTypeVerb::Set => apply_set(body, preview),
        AlterGraphTypeVerb::Add => apply_add(current, body, preview),
        AlterGraphTypeVerb::Alter => apply_alter(current, body, preview),
        AlterGraphTypeVerb::Drop => apply_drop(current, body),
    }
}

/// `SET`: the new body replaces the current one wholesale (CIP-176), after
/// passing the CIP-178 "shall" rules.
pub fn apply_set(body: &GraphTypeBody, preview: PreviewFeatures) -> Result<GraphTypeBody> {
    validate_graph_type_body(body, preview).map_err(|e| Error::semantic(e.to_string()))?;
    Ok(body.clone())
}

/// `ADD`: extends the current body with the new element types and
/// constraints, then validates the union.
pub fn apply_add(
    mut current: GraphTypeBody,
    body: &GraphTypeBody,
    preview: PreviewFeatures,
) -> Result<GraphTypeBody> {
    current.node_types.extend(body.node_types.iter().cloned());
    current.edge_types.extend(body.edge_types.iter().cloned());
    current.constraints.extend(body.constraints.iter().cloned());
    validate_graph_type_body(&current, preview).map_err(|e| Error::semantic(e.to_string()))?;
    Ok(current)
}

/// `ALTER`: replaces existing element types (matched by identifying label)
/// with their new declarations (CIP-183); constraints are not alterable.
pub fn apply_alter(
    current: GraphTypeBody,
    body: &GraphTypeBody,
    preview: PreviewFeatures,
) -> Result<GraphTypeBody> {
    if !body.constraints.is_empty() {
        return Err(Error::semantic(
            "SchemaError: constraints are not supported in ALTER",
        ));
    }
    let mut merged = current;
    upsert_element_type(
        &mut merged.node_types,
        &body.node_types,
        |node| &node.identifying_label,
        "SchemaError: node element type does not exist",
    )?;
    upsert_element_type(
        &mut merged.edge_types,
        &body.edge_types,
        |edge| &edge.identifying_label,
        "SchemaError: relationship element type does not exist",
    )?;
    validate_graph_type_body(&merged, preview).map_err(|e| Error::semantic(e.to_string()))?;
    Ok(merged)
}

/// The `ALTER` upsert shared by node and edge element types: every incoming
/// spec must name an existing identifying label, then replaces it.
fn upsert_element_type<T: Clone>(
    existing: &mut Vec<T>,
    incoming: &[T],
    identifying_label: impl Fn(&T) -> &str,
    missing_error: &'static str,
) -> Result<()> {
    for spec in incoming {
        let label = identifying_label(spec);
        if !existing.iter().any(|old| identifying_label(old) == label) {
            return Err(Error::semantic(missing_error));
        }
        existing.retain(|old| identifying_label(old) != label);
        existing.push(spec.clone());
    }
    Ok(())
}

/// `DROP`: removes the element types and constraints named in the drop body
/// (CIP-182). A dropped element type must exist and -- when given in more
/// than the bare reference form -- match the stored definition exactly; a
/// dropped node type must not still be referenced by a surviving edge type.
pub fn apply_drop(mut current: GraphTypeBody, body: &GraphTypeBody) -> Result<GraphTypeBody> {
    let aliases = current
        .node_types
        .iter()
        .filter_map(|node| {
            node.alias
                .as_deref()
                .map(|alias| (alias, node.identifying_label.as_str()))
        })
        .collect::<HashMap<_, _>>();
    let ref_is_label = |reference: &GraphTypeElementRef, sought: &str| match reference {
        GraphTypeElementRef::Any => false,
        GraphTypeElementRef::AliasRef(alias) => aliases
            .get(alias.as_str())
            .is_some_and(|value| *value == sought),
        GraphTypeElementRef::InSitu { label, .. } => label == sought,
    };
    for name in &body.drop_constraint_names {
        if !current
            .constraints
            .iter()
            .any(|constraint| constraint.name.as_ref() == Some(name))
        {
            return Err(Error::execution("SchemaError: constraint does not exist"));
        }
    }
    let dropping_edges: HashSet<_> = body
        .edge_types
        .iter()
        .map(|edge| edge.identifying_label.as_str())
        .collect();
    let node_names: HashSet<_> = body
        .node_types
        .iter()
        .map(|n| n.identifying_label.as_str())
        .collect();
    for node in &body.node_types {
        let Some(existing) = current
            .node_types
            .iter()
            .find(|old| old.identifying_label == node.identifying_label)
        else {
            return Err(Error::semantic(
                "SchemaError: node element type does not exist",
            ));
        };
        if !is_bare_node_type_ref(node) && !node_type_matches_exactly(node, existing) {
            return Err(Error::execution(
                "GraphTypeError: node element type specified incorrectly",
            ));
        }
        if current.edge_types.iter().any(|edge| {
            !dropping_edges.contains(edge.identifying_label.as_str())
                && (ref_is_label(&edge.source, &node.identifying_label)
                    || ref_is_label(&edge.target, &node.identifying_label))
        }) {
            return Err(Error::execution(
                "GraphTypeError: node element type is still referenced and cannot be dropped",
            ));
        }
        // CIP-266 NAGTE4: a node element type still *implied* by a
        // surviving node element type is referenced the same way a
        // relationship endpoint references it -- it cannot be dropped
        // without also dropping (or re-declaring) the child.
        if current.node_types.iter().any(|survivor| {
            !node_names.contains(survivor.identifying_label.as_str())
                && survivor.implied_labels.contains(&node.identifying_label)
        }) {
            return Err(Error::execution(
                "GraphTypeError: node element type is still referenced and cannot be dropped",
            ));
        }
    }
    for edge in &body.edge_types {
        let Some(existing) = current
            .edge_types
            .iter()
            .find(|old| old.identifying_label == edge.identifying_label)
        else {
            return Err(Error::semantic(
                "SchemaError: relationship element type does not exist",
            ));
        };
        if !is_bare_edge_type_ref(edge) && !edge_type_matches_exactly(edge, existing, &aliases) {
            return Err(Error::execution(
                "GraphTypeError: edge element type specified incorrectly",
            ));
        }
    }
    let edge_names: HashSet<_> = body
        .edge_types
        .iter()
        .map(|e| e.identifying_label.as_str())
        .collect();
    current
        .node_types
        .retain(|n| !node_names.contains(n.identifying_label.as_str()));
    current
        .edge_types
        .retain(|e| !edge_names.contains(e.identifying_label.as_str()));
    let constraint_names: HashSet<_> = body
        .drop_constraint_names
        .iter()
        .map(String::as_str)
        .collect();
    current.constraints.retain(|constraint| {
        !constraint
            .name
            .as_deref()
            .is_some_and(|n| constraint_names.contains(n))
    });
    Ok(current)
}
