//! ADR-0371 preview-gate predicates for the CIP graph-type trio
//! (ADR-0388: CIP-283 optional properties, CIP-262 property default values, CIP-266
//! implied labels identifying node element types): the AST-level checks
//! `parse_and_lower_statement_with_preview` runs before anything of a
//! statement lowers or plans. Each predicate answers "does this statement's
//! own text use the gated construct" from the parsed AST alone;
//! constructs that only materialize against *stored* graph-type state
//! (an `ADD` whose new identifying label is already implied by a stored
//! node element type) stay the executor's to fail closed -- the same
//! authored error, produced where the state lives.
//!
//! The predicates are one family over the same body walk: a single
//! collector runs once per admitted statement and yields the
//! `PreviewFeature`s the statement's bodies use, so admission is one
//! loop over that set -- a later graph-type gate is one more arm here,
//! not a fourth tree walk and a fourth admission branch.

use cypherlite_core::PreviewFeature;
use cypherlite_error::EngineError;
use cypherlite_parser::type_normalize::{type_expr_to_canon, CanonKind};
use cypherlite_parser::{Clause, GraphTypeBody, GraphTypeNodeTypeSpec, Statement};

use crate::ast::tree::walk_statement_tree;

/// The preview features a statement's graph-type bodies use, in registry
/// order (the first arm that finds a construct records it), deduplicated
/// -- the set admission loops over, once per statement.
pub(crate) type GraphTypeGates = Vec<PreviewFeature>;

/// Records `feature` once (a construct found again by a later body of
/// the same statement stays one entry).
fn note(gates: &mut GraphTypeGates, feature: PreviewFeature) {
    if !gates.contains(&feature) {
        gates.push(feature);
    }
}

/// Walks the statement tree once and collects the preview features its
/// graph-type bodies use. A walk error propagates: the gate never
/// answers "none" over a tree it could not walk.
pub(crate) fn statement_graph_type_gates(stmt: &Statement) -> Result<GraphTypeGates, EngineError> {
    let mut gates: GraphTypeGates = Vec::new();
    let mut visit_clause = |clause: &Clause| -> Result<(), EngineError> {
        for body in clause_graph_type_bodies(clause) {
            inspect_body(body, &mut gates);
        }
        Ok(())
    };
    walk_statement_tree(stmt, &mut |_| Ok(()), &mut visit_clause, &mut |_| Ok(()))?;
    Ok(gates)
}

/// The graph-type bodies a clause carries: `CREATE GRAPH TYPE`'s own body
/// and `ALTER CURRENT GRAPH TYPE`'s inline body (the `Named` target has
/// no body of its own). Every other clause carries none.
fn clause_graph_type_bodies(clause: &Clause) -> Vec<&GraphTypeBody> {
    match clause {
        Clause::CreateGraphType(c) => vec![&c.body],
        Clause::AlterCurrentGraphType(c) => match &c.target {
            cypherlite_parser::AlterCurrentGraphTypeTarget::Body(body) => vec![body],
            cypherlite_parser::AlterCurrentGraphTypeTarget::Named(_) => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Folds one body's element types into `gates`: a bare `ANY` value type
/// or a `DEFAULT ON COMMIT` clause on any element-type property, and an
/// implied label naming another in-body node element type's identifying
/// label.
fn inspect_body(body: &GraphTypeBody, gates: &mut GraphTypeGates) {
    for node in &body.node_types {
        inspect_properties(&node.properties, gates);
    }
    for edge in &body.edge_types {
        inspect_properties(&edge.properties, gates);
    }
    let identifying: Vec<&str> = body
        .node_types
        .iter()
        .map(|n| n.identifying_label.as_str())
        .collect();
    for node in &body.node_types {
        if node_implied_labels_identify(body, node, &identifying) {
            note(gates, PreviewFeature::GraphTypeImpliedLabels);
        }
    }
}

/// One element type's property list: the two per-property constructs.
fn inspect_properties(
    props: &[cypherlite_parser::GraphTypePropertyType],
    gates: &mut GraphTypeGates,
) {
    for p in props {
        if p.default_value.is_some() {
            note(gates, PreviewFeature::GraphTypeDefaultValues);
        }
        let canon = type_expr_to_canon(&p.value_type);
        if matches!(
            canon.kind,
            CanonKind::Predefined(cypherlite_parser::PredefinedKind::Any)
        ) && !canon.not_null
        {
            note(gates, PreviewFeature::GraphTypeOptProps);
        }
    }
}

/// Whether one of `node`'s implied labels names another *in-body* node
/// element type's identifying label (CIP-266 inheritance). The implied
/// label's own node element type -- declared by this same statement or by
/// a later one -- is what makes the label identify an element type in the
/// merged graph type; the in-body half is what the AST can see.
fn node_implied_labels_identify(
    body: &GraphTypeBody,
    node: &GraphTypeNodeTypeSpec,
    identifying: &[&str],
) -> bool {
    node.implied_labels.iter().any(|implied| {
        identifying.contains(&implied.as_str())
            && body
                .node_types
                .iter()
                .any(|other| other.identifying_label == *implied && !std::ptr::eq(other, node))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_core::{Error, PreviewFeature, PreviewFeatures};
    use cypherlite_parser::parse;

    fn gates_of(query: &str) -> GraphTypeGates {
        let stmt = parse(query).expect("parses");
        statement_graph_type_gates(&stmt).expect("walks")
    }

    #[test]
    fn the_collector_finds_each_gated_construct() {
        // A plain body gates nothing.
        let gates = gates_of("ALTER CURRENT GRAPH TYPE SET { (:L => {p :: STRING}) }");
        assert!(gates.is_empty());
        // CIP-283: the bare ANY element-type property.
        let gates = gates_of("ALTER CURRENT GRAPH TYPE SET { (:L => {p :: ANY}) }");
        assert_eq!(gates, vec![PreviewFeature::GraphTypeOptProps]);
        // CIP-262: the DEFAULT ON COMMIT clause.
        let gates = gates_of(
            "ALTER CURRENT GRAPH TYPE SET { (:L => {p :: STRING DEFAULT ON COMMIT 'x'}) }",
        );
        assert_eq!(gates, vec![PreviewFeature::GraphTypeDefaultValues]);
        // CIP-266: an implied label naming an in-body identifying label.
        let gates = gates_of("ALTER CURRENT GRAPH TYPE SET { (:A => {p :: STRING}), (:B => :A) }");
        assert_eq!(gates, vec![PreviewFeature::GraphTypeImpliedLabels]);
        // One statement using two gated constructs records both, once.
        let gates = gates_of(
            "ALTER CURRENT GRAPH TYPE SET { (:A => {p :: ANY}), (:B => :A {p :: STRING}) }",
        );
        assert_eq!(
            gates,
            vec![
                PreviewFeature::GraphTypeOptProps,
                PreviewFeature::GraphTypeImpliedLabels,
            ]
        );
        // `ANY NOT NULL` is not the optional property; a plain implied
        // label gates nothing.
        let gates = gates_of("ALTER CURRENT GRAPH TYPE SET { (:A => {p :: ANY NOT NULL}) }");
        assert!(gates.is_empty());
        // An independent property type constraint with ANY is not the
        // element-type construct either (it stays its own rejection).
        let gates = gates_of(
            "ALTER CURRENT GRAPH TYPE SET { \
             CONSTRAINT FOR (n:L) REQUIRE n.p IS :: STRING }",
        );
        assert!(gates.is_empty());
    }

    #[test]
    fn admission_fails_closed_with_the_authored_error_ungated() {
        // The ADR-0371 authored error, one gate at a time: each construct
        // requires its own feature before anything of it lowers.
        let cases = [
            (
                "ALTER CURRENT GRAPH TYPE SET { (:L => {p :: ANY}) }",
                PreviewFeature::GraphTypeOptProps,
                "graph-type-opt-props",
            ),
            (
                "ALTER CURRENT GRAPH TYPE SET { \
                 (:L => {p :: STRING DEFAULT ON COMMIT 'x'}) }",
                PreviewFeature::GraphTypeDefaultValues,
                "graph-type-default-values",
            ),
            (
                "ALTER CURRENT GRAPH TYPE SET { (:A => {p :: STRING}), (:B => :A) }",
                PreviewFeature::GraphTypeImpliedLabels,
                "graph-type-implied-labels",
            ),
        ];
        for (query, feature, name) in cases {
            let error =
                match crate::parse_and_lower_statement_with_preview(query, PreviewFeatures::none())
                {
                    Ok(_) => panic!("the gated construct must fail closed ungated"),
                    Err(error) => error,
                };
            let rendered = error.to_string();
            assert!(
                rendered.contains(&format!(
                    "preview feature '{name}' is not enabled: \
                     set CYPHERLITE_PREVIEW={name}"
                )),
                "{rendered}"
            );
            assert!(matches!(error, Error::Semantic(_)), "{rendered}");
            // Enabled, the statement lowers past the gate.
            let enabled = PreviewFeatures::parse(name).expect("a registered feature name");
            crate::parse_and_lower_statement_with_preview(query, enabled)
                .expect("lowers behind its gate");
            let _ = feature;
        }
    }
}
