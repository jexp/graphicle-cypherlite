//! CIP-262 (ADR-0388): applying graph-type property default values at
//! the statement boundary. A statement is this engine's transaction (and a
//! `CALL { ... } IN TRANSACTIONS` batch its own boundary inside the
//! statement), so the defaults land exactly where the CIP puts them: at
//! transaction commit -- after the statement's writes, before the
//! post-statement constraint validation, and *not* visible to the
//! statement's own RETURN (the property still reads null until the
//! statement finishes, CE1).
//!
//! The redo slice decides who is eligible: a node created in the
//! statement (all its labels count) or a node that *gained* a label (only
//! the gained labels count -- matching and updating an untouched label's
//! nodes never backfills, and removing a property in the same statement
//! as its creation or label-add is ignored: the property is null at the
//! boundary, so the default applies), plus the relationships created
//! with their type. A property already holding a non-null value is never
//! touched, and -- when more than one element type in the node's gained
//! labels declares a default for the same property (CIP-262 FE10) -- the
//! statement fails instead of picking one.

use std::collections::{BTreeMap, BTreeSet};

use cypherlite_core::{Error, NodeId, RelId, Result, Value};

use crate::graph_type::{
    body_has_default_values, effective_default_map, eval_lowered_default, lowered_default,
    DefaultValue, EffectiveDefault, Inheritance,
};
use crate::overlay::ExecGraph;
use crate::redo::RedoOp;

/// The slice of a statement's redo ops that property defaults can apply
/// to: the labels each touched node *gained* in the statement, and the
/// relationships created with their type.
#[derive(Debug, Default)]
struct DefaultsSlice {
    node_labels: BTreeMap<NodeId, BTreeSet<String>>,
    created_rels: BTreeSet<RelId>,
}

/// The one classifier over the redo ops: created nodes start with all
/// their labels, an `AddLabels` contributes only the labels it added
/// (duplicates across ops fold into one set), and a `CreateRel` is
/// eligible whole.
fn classify_defaults(ops: &[RedoOp]) -> DefaultsSlice {
    let mut slice = DefaultsSlice::default();
    for op in ops {
        match op {
            RedoOp::CreateNode { id, labels, .. } => {
                slice
                    .node_labels
                    .entry(*id)
                    .or_default()
                    .extend(labels.iter().cloned());
            }
            RedoOp::AddLabels { id, labels } => {
                slice
                    .node_labels
                    .entry(*id)
                    .or_default()
                    .extend(labels.iter().cloned());
            }
            RedoOp::CreateRel { id, .. } => {
                slice.created_rels.insert(*id);
            }
            _ => {}
        }
    }
    slice
}

/// Applies the current graph type's property default values at the
/// statement boundary (CIP-262): for every eligible touched entity whose
/// property is null at the boundary, evaluate the default (through the
/// engine's own evaluator -- `uuid()` and the no-argument temporals
/// produce a fresh value per entity) and write it through the overlay,
/// where the following constraint validation sees it like any other
/// write.
pub(crate) fn apply_property_defaults(graph: &ExecGraph<'_>) -> Result<()> {
    // The cheap scratch-aware gate runs first (round 8's rust Medium): the
    // redo classification costs a `String` clone per created/added label,
    // so a defaults-free store -- the common case -- must not pay it on
    // every write statement. The one yes/no answer over both vintages
    // (`ExecGraph::has_property_defaults`: the committed theater, or a
    // `SetCurrentGraphType` in this statement's redo slice declaring one)
    // -- the committed-theater half alone could miss a default this very
    // statement set, so both halves consult the same gate instead of
    // answering the fact twice over two vintages.
    if !graph.has_property_defaults() {
        return Ok(());
    }
    let (slice, _) = graph.unvalidated_redo(classify_defaults);
    if slice.node_labels.is_empty() && slice.created_rels.is_empty() {
        return Ok(());
    }
    let body = graph.show_schema()?.data.current_graph_type.to_body();
    if !body_has_default_values(&body) {
        return Ok(());
    }
    // The node side: the effective default map (own defaults plus
    // CIP-266-inherited ones). Validation already rejected the
    // multiple-default conflicts at `ALTER CURRENT GRAPH TYPE` time, so
    // resolution here fails only on a body that skipped validation --
    // fail-closed, never a silent skip.
    let inheritance = Inheritance::of(&body);
    let defaults =
        effective_default_map(&body, &inheritance).map_err(|e| Error::semantic(e.to_string()))?;
    // The lowered-expr memo (the round-10 Medium): keyed by the default's
    // canonical text, shared by the node and relationship halves of this
    // statement -- a default lowered once, evaluated per entity.
    let mut lowered: BTreeMap<String, cypherlite_ir::Expr> = BTreeMap::new();
    for (node, gained_labels) in &slice.node_labels {
        apply_node_defaults(
            graph,
            &defaults,
            &mut lowered,
            &inheritance,
            *node,
            gained_labels,
        )?;
    }
    // The relationship side: edge element types declare their own
    // defaults (no inheritance exists for edge types). The capture runs
    // once per edge type per statement (the round-9 rust Medium: the
    // per-relationship re-capture cost a clone and a re-lowering per
    // created relationship); only the evaluation runs per entity.
    for edge in &body.edge_types {
        let defaults = edge_property_defaults(edge)?;
        if defaults.is_empty() {
            continue;
        }
        for rel in &slice.created_rels {
            if graph
                .rel_type(*rel)
                .is_none_or(|rel_type| rel_type != edge.identifying_label)
            {
                continue;
            }
            apply_rel_defaults(graph, &defaults, &mut lowered, *rel)?;
        }
    }
    Ok(())
}
/// One touched node's gained labels' effective defaults: a property
/// already holding a non-null value is untouched; a default a *descendant*
/// of another gained label's type also declares wins over the ancestor's
/// (CE9: the child element type's re-specified default overrides the
/// parent's); two declaring element types that are unrelated both
/// defaulting the same property (CIP-262 FE10) fails the statement;
/// otherwise the one default is evaluated and written.
fn apply_node_defaults(
    graph: &ExecGraph<'_>,
    defaults: &BTreeMap<String, BTreeMap<String, EffectiveDefault>>,
    lowered: &mut BTreeMap<String, cypherlite_ir::Expr>,
    inheritance: &Inheritance,
    node: NodeId,
    gained_labels: &BTreeSet<String>,
) -> Result<()> {
    // Per property: the declaring labels and the default each gained
    // label's element type resolves to (a type's own effective map
    // already prefers its own declaration over an inherited one).
    // Values borrow the effective map (the round-11 rust Medium): no
    // DefaultValue deep-clone per declaring label per node -- the clone
    // was paid even when the node already holds a value.
    let mut candidates: BTreeMap<&str, BTreeMap<&str, &EffectiveDefault>> = BTreeMap::new();
    for label in gained_labels {
        let Some(map) = defaults.get(label) else {
            continue;
        };
        for (property, effective) in map {
            for declaring in &effective.declared_by {
                candidates
                    .entry(property.as_str())
                    .or_default()
                    .insert(declaring.as_str(), effective);
            }
        }
    }
    for (property, declaring) in &candidates {
        if holds_non_null(graph.node_prop(node, property)) {
            // Explicitly set in the statement: the default never applies
            // (CIP-262), whatever else competes for it.
            continue;
        }
        // An ancestor's declaration yields to a descendant's (CE9): drop
        // every declaring label that is an ancestor of another declaring
        // label.
        let declaring: BTreeMap<&str, &EffectiveDefault> = declaring
            .iter()
            .filter(|(label, _)| {
                !declaring
                    .keys()
                    .any(|other| *other != **label && inheritance.is_ancestor_of(label, other))
            })
            .map(|(label, effective)| (*label, *effective))
            .collect();
        if declaring.len() > 1 {
            return Err(Error::semantic(format!(
                "Node ({node}) has more than one default value for the property `{property}`, \
                 defined by the node element types {}",
                declaring
                    .keys()
                    .map(|label| format!("`{label}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        // A zero-survivor set is the same multiple-defaults/corruption
        // family as the >1 conflict above: a typed error, never an
        // asserted survivor (the rust review's High -- acyclicity is
        // enforced elsewhere, this boundary owns its own totality).
        let Some(default) = declaring.values().next() else {
            return Err(Error::semantic(format!(
                "Node ({node}) has no default value for the property `{property}` left \
                 after resolving its declaring node element types"
            )));
        };
        let value = eval_lowered_default(graph, lowered_expr(lowered, &default.value)?)?;
        graph.set_property_node(node, property, value);
    }
    Ok(())
}

/// One created relationship's type's own defaults (CIP-262: the same
/// rules as the node side, minus inheritance, which edge element types
/// do not have).
fn apply_rel_defaults(
    graph: &ExecGraph<'_>,
    defaults: &[(&cypherlite_parser::GraphTypePropertyType, DefaultValue)],
    lowered: &mut BTreeMap<String, cypherlite_ir::Expr>,
    rel: RelId,
) -> Result<()> {
    for (property, default) in defaults {
        if holds_non_null(graph.rel_prop(rel, &property.name)) {
            continue;
        }
        let value = eval_lowered_default(graph, lowered_expr(lowered, default)?)?;
        graph.set_property_rel(rel, &property.name, value);
    }
    Ok(())
}

/// The default's lowered expression through the statement memo: lowered
/// once per distinct default (keyed by its canonical text), evaluated per
/// entity.
fn lowered_expr<'a>(
    lowered: &'a mut BTreeMap<String, cypherlite_ir::Expr>,
    default: &DefaultValue,
) -> Result<&'a cypherlite_ir::Expr> {
    let key = default.render();
    if !lowered.contains_key(&key) {
        let expr = lowered_default(default).map_err(|e| Error::semantic(e.to_string()))?;
        lowered.insert(key.clone(), expr);
    }
    Ok(lowered.get(&key).expect("just inserted"))
}

/// The edge's own declared defaults captured once per statement (the
/// rel-side twin of the node side's effective_default_map, minus
/// inheritance): each property with a `DEFAULT ON COMMIT` value and its
/// captured `DefaultValue`. Fallible exactly like the node side -- a
/// body that skipped validation (the persisted paths install without
/// re-validating) fails the semantic mapping, never a panic.
fn edge_property_defaults(
    edge: &cypherlite_parser::GraphTypeEdgeTypeSpec,
) -> Result<Vec<(&cypherlite_parser::GraphTypePropertyType, DefaultValue)>> {
    let mut out = Vec::new();
    for property in &edge.properties {
        let Some(expr) = &property.default_value else {
            continue;
        };
        out.push((
            property,
            DefaultValue::from_expr(expr).map_err(|e| Error::semantic(e.to_string()))?,
        ));
    }
    Ok(out)
}

/// Whether a property read holds a non-null value (`None` is an absent
/// property; `Some(Null)` is an explicitly-nulled one -- both take the
/// default).
fn holds_non_null(read: Option<Value>) -> bool {
    read.is_some_and(|value| !value.is_null())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_or_absent_property_takes_the_default() {
        assert!(!holds_non_null(None));
        assert!(!holds_non_null(Some(Value::Null)));
        assert!(holds_non_null(Some(Value::Bool(true))));
    }
}
