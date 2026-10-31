//! `USE` target resolution (ADR-0116 §3): lift the statement's graph target
//! out of its `USE` clauses before execution.
//!
//! A statement targets the namespace named by its `USE` clause, or else the
//! connection's session namespace. Resolution is eager (no lazy failure):
//!
//! - A static name (`USE neo4j`, dotted `USE a.b` spellings included) must
//!   be one of the fixed namespaces; anything else fails with
//!   `DatabaseNotFound: Graph not found: <name>` (42N00).
//! - `graph.byElementId('<kind>:<segment>:<id>')` with a string-literal id
//!   resolves through the element-id middle segment (ADR-0116 §7); a
//!   segment naming no namespace fails with 42NA7.
//! - `graph.byName`, `graph.names`, `graph.propertiesByName` and any dynamic
//!   target (a non-literal `graph.byElementId` argument, a parameter, any
//!   other expression) fail with 42N72: graph selection beyond a static name
//!   is only available on composite databases.
//! - Several DISTINCT targets in one statement — across `UNION` arms, `CALL`
//!   subqueries, `WHEN` branches or `NEXT` stages — fail with 42NA5
//!   (`Multiple graphs in the same query not allowed here. …`). As in Neo4j
//!   CE, the distinct-targets check runs before name resolution, so
//!   `USE neo4j … USE foo …` is 42NA5 even though `foo` does not exist,
//!   while a lone `USE foo` is 42N00. The same target twice is fine.

use cypherlite_core::{ce_messages, ElementIdRef, Namespace, Result};
use cypherlite_ir::{Clause, Expr};

use crate::LogicalPlan;

/// One `USE` clause's target. Unresolved references keep their text so the
/// not-found error is deferred until after the distinct-targets check (see
/// the module doc).
enum UseTarget {
    /// A fixed namespace (static name or resolved element-id segment).
    Namespace(Namespace),
    /// A static name that is not a fixed namespace (deferred 42N00).
    UnknownName(String),
    /// An element id whose middle segment names no namespace (deferred
    /// 42NA7); keeps the full id for CE's echo.
    UnknownSegment(String),
}

impl UseTarget {
    /// Same-graph identity: two `USE` clauses target the same graph when
    /// this holds. Namespace names match case-insensitively (ADR-0116 §1),
    /// and a resolved element id compares on its namespace so
    /// `USE neo4j … USE graph.byElementId('4:cypherlite:n1') …` is one
    /// target. The three variants can never collide.
    fn same_target(&self, other: &UseTarget) -> bool {
        match (self, other) {
            (UseTarget::Namespace(a), UseTarget::Namespace(b)) => a == b,
            (UseTarget::UnknownName(a), UseTarget::UnknownName(b)) => a.eq_ignore_ascii_case(b),
            (UseTarget::UnknownSegment(a), UseTarget::UnknownSegment(b)) => a == b,
            _ => false,
        }
    }

    /// The target as CE echoes it on the `Attempted to access graph …` line
    /// of the 42NA5 text.
    fn attempted_text(&self) -> String {
        match self {
            UseTarget::Namespace(ns) => ns.as_str().to_string(),
            UseTarget::UnknownName(name) => name.clone(),
            UseTarget::UnknownSegment(id) => id.clone(),
        }
    }
}

/// Resolve the statement's single `USE` target, or `None` when the
/// statement carries no `USE` clause (the session namespace applies).
pub(crate) fn resolve_use_target(plan: &LogicalPlan) -> Result<Option<Namespace>> {
    let mut distinct: Vec<UseTarget> = Vec::new();
    for graph in use_graph_exprs(plan) {
        let target = resolve_graph_expr(graph)?;
        if !distinct.iter().any(|t| t.same_target(&target)) {
            distinct.push(target);
        }
    }
    match distinct.len() {
        0 => Ok(None),
        1 => match distinct.into_iter().next().expect("len checked") {
            UseTarget::Namespace(ns) => Ok(Some(ns)),
            UseTarget::UnknownName(name) => Err(ce_messages::graph_not_found(&name)),
            UseTarget::UnknownSegment(id) => Err(ce_messages::element_id_no_namespace(&id)),
        },
        // CE names the second distinct target on the "Attempted to access
        // graph …" line (testdata/ce-errors/neo4j-ce.json multi_graph_*).
        _ => Err(ce_messages::multiple_graphs(&distinct[1].attempted_text())),
    }
}

/// Resolve one `USE` graph expression. Composite-only forms fail here; a
/// static name or element-id segment is classified, with not-found errors
/// deferred to [`resolve_use_target`].
fn resolve_graph_expr(expr: &Expr) -> Result<UseTarget> {
    if let Some(name) = static_graph_name(expr) {
        return Ok(match Namespace::parse(&name) {
            Some(ns) => UseTarget::Namespace(ns),
            None => UseTarget::UnknownName(name),
        });
    }
    if let Expr::Function { name, args, .. } = expr {
        if cypherlite_core::is_graph_selector_function_name(name) {
            if name.eq_ignore_ascii_case("graph.byelementid") {
                return match args.as_slice() {
                    [Expr::String(id)] => Ok(resolve_element_id(id)),
                    // A non-literal argument is a dynamic graph lookup
                    // (CE 42N72).
                    _ => Err(ce_messages::composite_only_graph_function(
                        &graph_call_text(name, args),
                    )),
                };
            }
            return Err(ce_messages::composite_only_graph_function(
                &graph_call_text(name, args),
            ));
        }
    }
    // Parameters, in-scope variables and every other expression are a
    // dynamic graph lookup: composite-only (CE 42N72).
    Err(ce_messages::composite_only_graph_function(
        &graph_expr_text(expr),
    ))
}

/// Render a `graph.*` call as written for CE's `Attempted to access graph
/// …` line (`graph.byName('neo4j')`).
pub(crate) fn graph_call_text(name: &str, args: &[Expr]) -> String {
    let inner = args
        .iter()
        .map(graph_expr_text)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({inner})")
}

/// Best-effort source spelling of a graph expression for CE's `Attempted to
/// access graph …` line; non-literal expressions render as `<dynamic>`.
fn graph_expr_text(expr: &Expr) -> String {
    match expr {
        Expr::String(s) => format!("'{s}'"),
        Expr::Variable(v) => v.clone(),
        Expr::Parameter(p) => format!("${p}"),
        Expr::Function { name, args, .. } => graph_call_text(name, args),
        Expr::Property { expr, key } => format!("{}.{key}", graph_expr_text(expr)),
        _ => "<dynamic>".to_string(),
    }
}

/// The static dotted-name form: `USE neo4j`, `USE composite.constituent` or
/// a string literal. Property chains of plain variables are catalog name
/// spellings, not row-variable access.
///
/// This is the one static-name test for `USE` targets: `use_target` uses it
/// to resolve the target, and `validate`'s placement check uses it to skip
/// scope-checking a static name (a variable, string literal or dotted chain
/// carries no row-variable references to check).
pub(crate) fn static_graph_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Variable(name) | Expr::String(name) => Some(name.clone()),
        Expr::Property { expr, key } => Some(format!("{}.{}", static_graph_name(expr)?, key)),
        _ => None,
    }
}

/// ADR-0116 §7: an element id is `<kind>:cypherlite[.<namespace>]:<n|r><id>`;
/// the middle segment names the namespace. An id naming no namespace keeps
/// its full text so the 42NA7 error can echo it as CE does.
fn resolve_element_id(id: &str) -> UseTarget {
    match ElementIdRef::parse(id).and_then(|parsed| parsed.namespace()) {
        Some(ns) => UseTarget::Namespace(ns),
        None => UseTarget::UnknownSegment(id.to_string()),
    }
}

/// The argument list of a composite-database `graph.*` selector call
/// ([`cypherlite_core::GRAPH_SELECTOR_FUNCTIONS`], shared with the
/// executor's `SHOW FUNCTIONS` catalog so the two lists cannot drift), or
/// `None` for any other expression. The binder treats these as `USE`
/// selectors, not row expressions: their argument variables are still
/// scope-checked, but the call itself is resolved (and rejected) by
/// [`resolve_graph_expr`], never evaluated.
pub(crate) fn graph_selector_args(expr: &Expr) -> Option<&[Expr]> {
    match expr {
        Expr::Function { name, args, .. }
            if cypherlite_core::is_graph_selector_function_name(name) =>
        {
            Some(args)
        }
        _ => None,
    }
}

/// Every `USE` clause's graph expression, in statement order.
/// [`crate::walk_plan_clauses`] descends into UNION arms, CALL subqueries,
/// WHEN branches, FOREACH bodies, expression subqueries and NEXT stages,
/// so a `USE` in any of those positions is found and a new clause variant
/// cannot drift out of coverage silently.
fn use_graph_exprs(plan: &LogicalPlan) -> Vec<&Expr> {
    let mut graphs = Vec::new();
    crate::walk_plan_clauses(plan, &mut |clause| {
        if let Clause::Use { graph } = clause {
            graphs.push(graph);
        }
    });
    graphs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(cypher: &str) -> Result<Option<Namespace>> {
        let plan = crate::plan(cypherlite_front::parse_cypher(cypher).unwrap(), &mut |_| {
            false
        })?;
        Ok(plan.use_target)
    }

    #[test]
    fn static_names_resolve_case_insensitively() {
        assert_eq!(
            resolve("USE neo4j RETURN 1").unwrap(),
            Some(Namespace::Neo4j)
        );
        assert_eq!(
            resolve("USE SYSTEM RETURN 1").unwrap(),
            Some(Namespace::System)
        );
        assert_eq!(
            resolve("USE System_Ontologies RETURN 1").unwrap(),
            Some(Namespace::SystemOntologies)
        );
        assert_eq!(resolve("RETURN 1").unwrap(), None);
    }

    #[test]
    fn unknown_static_name_is_42n00() {
        let err = resolve("USE movies RETURN 1").unwrap_err().to_string();
        assert!(err.ends_with("Graph not found: movies"), "{err}");
        // Dotted composite-style names are unknown names too.
        let err = resolve("USE composite.movies RETURN 1")
            .unwrap_err()
            .to_string();
        assert!(err.ends_with("Graph not found: composite.movies"), "{err}");
    }

    #[test]
    fn composite_only_functions_are_42n72() {
        for (cypher, attempted) in [
            (
                "USE graph.byName('neo4j') RETURN 1",
                "graph.byName('neo4j')",
            ),
            ("USE graph.names() RETURN 1", "graph.names()"),
            (
                "USE graph.propertiesByName('neo4j') RETURN 1",
                "graph.propertiesByName('neo4j')",
            ),
        ] {
            let err = resolve(cypher).unwrap_err().to_string();
            assert!(
                err.ends_with(&format!(
                    "Dynamic graph lookup not allowed here. This feature is only available on composite databases.\nAttempted to access graph {attempted}"
                )),
                "{cypher}: {err}"
            );
        }
    }

    #[test]
    fn graph_selectors_in_expression_position_are_42n72() {
        // ADR-0116 §3: every selector in `GRAPH_SELECTOR_FUNCTIONS` —
        // `graph.byElementId` included — fails with CE's 42N72 text when
        // used as a row expression; only the `USE`-target path resolves
        // element ids.
        for (cypher, attempted) in [
            ("RETURN graph.byName('neo4j') AS g", "graph.byName('neo4j')"),
            ("RETURN graph.names() AS g", "graph.names()"),
            (
                "RETURN graph.propertiesByName('neo4j') AS g",
                "graph.propertiesByName('neo4j')",
            ),
            (
                "RETURN graph.byElementId('4:cypherlite:n1') AS g",
                "graph.byElementId('4:cypherlite:n1')",
            ),
        ] {
            let err = resolve(cypher).unwrap_err().to_string();
            assert!(
                err.ends_with(&format!(
                    "Dynamic graph lookup not allowed here. This feature is only available on composite databases.\nAttempted to access graph {attempted}"
                )),
                "{cypher}: {err}"
            );
        }
    }

    #[test]
    fn by_element_id_resolves_the_middle_segment() {
        assert_eq!(
            resolve("USE graph.byElementId('4:cypherlite:n1') RETURN 1").unwrap(),
            Some(Namespace::Neo4j)
        );
        assert_eq!(
            resolve("USE graph.byElementId('4:cypherlite.system_ontologies:n1') RETURN 1").unwrap(),
            Some(Namespace::SystemOntologies)
        );
    }

    #[test]
    fn by_element_id_without_a_namespace_is_42na7() {
        for id in [
            "4:00000000-0000-0000-0000-000000000000:n1",
            "4:cypherlite.system:n1",
            "garbage",
        ] {
            let err = resolve(&format!("USE graph.byElementId('{id}') RETURN 1"))
                .unwrap_err()
                .to_string();
            assert!(
                err.ends_with(&format!(
                    "No database is corresponding to `graph.byElementId({id})`. Verify that the elementId is correct."
                )),
                "{id}: {err}"
            );
        }
    }

    #[test]
    fn distinct_targets_across_union_call_and_next_are_42na5() {
        let suffix = "Multiple graphs in the same query not allowed here. This feature is only available on composite databases.\nAttempted to access graph ";
        for (cypher, attempted) in [
            (
                "USE neo4j RETURN 1 AS x UNION USE system RETURN 2 AS x",
                "system",
            ),
            (
                "USE neo4j CALL { USE system RETURN 2 AS x } RETURN x",
                "system",
            ),
            (
                "USE neo4j RETURN 1 AS x NEXT USE system RETURN 2 AS x",
                "system",
            ),
            (
                "WHEN true THEN { USE neo4j RETURN 1 AS x } ELSE { USE system RETURN 2 AS x }",
                "system",
            ),
            // CE checks distinctness before existence: an unknown second
            // target is still 42NA5, not 42N00.
            ("USE neo4j CALL { USE foo RETURN 1 AS x } RETURN x", "foo"),
        ] {
            let err = resolve(cypher).unwrap_err().to_string();
            assert!(
                err.ends_with(&format!("{suffix}{attempted}")),
                "{cypher}: {err}"
            );
        }
    }

    #[test]
    fn same_target_twice_is_fine() {
        assert_eq!(
            resolve("USE neo4j CALL { USE neo4j RETURN 1 AS x } RETURN x").unwrap(),
            Some(Namespace::Neo4j)
        );
        assert_eq!(
            resolve("USE neo4j RETURN 1 AS x UNION USE NEO4J RETURN 2 AS x").unwrap(),
            Some(Namespace::Neo4j)
        );
        // An element id naming the same namespace is the same target.
        assert_eq!(
            resolve("USE neo4j CALL { USE graph.byElementId('4:cypherlite:n1') RETURN 1 AS x } RETURN x")
                .unwrap(),
            Some(Namespace::Neo4j)
        );
        // Two distinct spellings of one unknown name are one target: 42N00.
        let err = resolve("USE foo RETURN 1 AS x UNION USE FOO RETURN 2 AS x")
            .unwrap_err()
            .to_string();
        assert!(err.ends_with("Graph not found: foo"), "{err}");
    }

    #[test]
    fn use_inside_expression_subqueries_counts() {
        let err = resolve("RETURN COUNT { USE foo MATCH (n) } AS c")
            .unwrap_err()
            .to_string();
        assert!(err.ends_with("Graph not found: foo"), "{err}");
        assert_eq!(
            resolve("RETURN COUNT { USE system MATCH (n) } AS c").unwrap(),
            Some(Namespace::System)
        );
        // EXISTS/COLLECT expression subqueries ride the same walker.
        let err = resolve("MATCH (n) WHERE EXISTS { USE foo MATCH (m) } RETURN n")
            .unwrap_err()
            .to_string();
        assert!(err.ends_with("Graph not found: foo"), "{err}");
        let err = resolve("RETURN COLLECT { USE foo MATCH (n) RETURN n } AS c")
            .unwrap_err()
            .to_string();
        assert!(err.ends_with("Graph not found: foo"), "{err}");
    }

    #[test]
    fn dynamic_targets_are_42n72() {
        for (cypher, attempted) in [
            ("USE $db RETURN 1", "$db"),
            (
                "WITH 1 AS g USE graph.byElementId(g) RETURN 1",
                "graph.byElementId(g)",
            ),
        ] {
            let err = resolve(cypher).unwrap_err().to_string();
            assert!(
                err.ends_with(&format!(
                    "Dynamic graph lookup not allowed here. This feature is only available on composite databases.\nAttempted to access graph {attempted}"
                )),
                "{cypher}: {err}"
            );
        }
    }
}
