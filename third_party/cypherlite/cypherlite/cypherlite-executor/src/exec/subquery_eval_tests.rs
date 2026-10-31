//! The COUNT expression-subquery engine's two statement-shape UNION scans
//! (`eval_count_query`): the explicit-vs-omitted RETURN mix and the
//! omitted-RETURN-requires-UNION-ALL rule. Both compositions are rejected
//! before the executor ever sees them for parsed text — the parser's
//! COUNT-site check first, then the front's validate battery
//! (`cypherlite-front`'s `validate_count_subquery_union_shapes`, the
//! statically decidable home of the rules); these per-row scans stay as
//! defense-in-depth for callers that bypass validation, raising the
//! identical messages.

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Clause, Query, SingleQuery};

use super::subquery_eval::eval_count_query;
use crate::eval::Row;
use crate::graph::Graph;
use crate::overlay::ExecGraph;

/// A COUNT body query with `parts`' clause lists as its UNION branches,
/// joined by `union_all` — the shape the classification scan reads. The
/// queries are built by hand for exactly the reason the scans exist: no
/// product path can produce them anymore (the scans fire before any part
/// runs, so the branch clause lists only need the tails the
/// classification reads).
fn run_count(parts: Vec<Vec<Clause>>, union_all: Vec<bool>) -> Result<Value> {
    let query = Query {
        parts: parts
            .into_iter()
            .map(|clauses| SingleQuery { clauses })
            .collect(),
        union_all,
        next: None,
        cypher_version: None,
    };
    let g = Graph::default();
    let view = ExecGraph::read(&g);
    eval_count_query(&view, &Row::new(), &BTreeMap::new(), &query, None)
}

/// The classification scan still rejects a query whose parts mix an
/// explicit RETURN tail with omitted ones, handed to it directly —
/// defense-in-depth for a caller that bypasses front validation. The
/// message is byte-identical to the one the front battery raises (which
/// the lowering boundary wraps in the same `syntax error:` prefix).
#[test]
fn the_mix_scan_rejects_a_mixed_query_defense_in_depth() {
    let err: Error = run_count(
        vec![
            vec![Clause::Return {
                distinct: false,
                group_by: None,
                items: Vec::new(),
                order: Vec::new(),
                skip: None,
                limit: None,
            }],
            Vec::new(),
        ],
        vec![true],
    )
    .expect_err("the mix scan rejects the query");
    assert_eq!(
        err.to_string(),
        "syntax error: DifferentColumnsInUnion: cannot mix explicit and omitted RETURN"
    );
}

/// The same for the second rule: an all-omitted body whose branches join
/// with a plain UNION (not UNION ALL).
#[test]
fn the_omitted_scan_rejects_a_distinct_join_defense_in_depth() {
    let err: Error = run_count(vec![Vec::new(), Vec::new()], vec![false])
        .expect_err("the omitted scan rejects the query");
    assert_eq!(
        err.to_string(),
        "syntax error: DifferentColumnsInUnion: omitted RETURN requires UNION ALL"
    );
}
