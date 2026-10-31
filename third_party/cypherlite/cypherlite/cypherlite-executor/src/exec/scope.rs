//! Row scope helpers: `RETURN *` column names, `CALL { … }` read rules,
//! and post-write row refresh.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::Value;
use cypherlite_ir::{CallScope, Clause, Expr, OrderItem, PathPattern, PatternElement, Projection};

use crate::eval::Row;
use crate::overlay::ExecGraph;

/// Internal/synthetic row column names start with `__` (e.g. the `__agg_*`
/// bindings aggregation seeds, or `anon_name` temporaries); they are never
/// projected to users.
pub(crate) fn is_internal_column(name: &str) -> bool {
    name.starts_with("__")
}

/// The parked sort-key column of ORDER BY item `index` — one spelling for
/// the sink writer (`SinkProjection::new`) and the reader (`parked_keys`).
pub(crate) fn parked_key_name(index: usize) -> String {
    format!("__ord{index}")
}

/// One projection's (`RETURN` / `WITH`) borrowed field bundle, so helpers
/// over a projection read named fields instead of positional parameters
/// (three of which are the identical `&Option<Expr>`). `where_clause` is
/// `None` for `RETURN`, the `WITH`'s own filter otherwise.
pub(crate) struct ProjectionFields<'a> {
    pub items: &'a [Projection],
    pub group_by: &'a Option<Vec<Expr>>,
    pub order: &'a [OrderItem],
    pub skip: &'a Option<Expr>,
    pub limit: &'a Option<Expr>,
    pub where_clause: &'a Option<Expr>,
}

pub(crate) fn push_scope_name(names: &mut Vec<String>, name: impl Into<String>) {
    let name = name.into();
    if !is_internal_column(&name) && !names.iter().any(|n| n == &name) {
        names.push(name);
    }
}

pub(crate) fn add_path_scope(names: &mut Vec<String>, p: &PathPattern) {
    if let Some(v) = &p.variable {
        push_scope_name(names, v.clone());
    }
    for n in &p.nodes {
        if let Some(v) = &n.variable {
            push_scope_name(names, v.clone());
        }
    }
    for r in &p.rels {
        if let Some(v) = &r.variable {
            push_scope_name(names, v.clone());
        }
    }
    fn add_elements(names: &mut Vec<String>, elements: &[PatternElement]) {
        for element in elements {
            match element {
                PatternElement::Node(node) => {
                    if let Some(name) = &node.variable {
                        push_scope_name(names, name.clone());
                    }
                }
                PatternElement::Relationship(rel) => {
                    if let Some(name) = &rel.variable {
                        push_scope_name(names, name.clone());
                    }
                }
                PatternElement::Quantified(qpp) => {
                    if let Some(name) = &qpp.subpath_variable {
                        push_scope_name(names, name.clone());
                    }
                    add_elements(names, &qpp.elements);
                }
            }
        }
    }
    add_elements(names, &p.elements);
}

/// The names a `RETURN *`/`WITH *` expands when there is no row to read them
/// off — the declared scope, so the empty table still has the right columns.
/// It walks the arms that bind a name before the projection: MATCH/MERGE/
/// CREATE patterns, `UNWIND` aliases, `CALL … YIELD` columns, and the
/// preceding `WITH`.
///
/// This is the executor's runtime stand-in for an empty table, not the
/// front's admission set: the front's `clause_introduced_names` also folds
/// the catalog commands' output columns, the `LOAD CSV` alias and a nested
/// `CALL { … }`'s exports, which this walk does not model.
pub(crate) fn star_scope_names(clauses: &[Clause]) -> Vec<String> {
    let mut names = Vec::new();
    for c in clauses {
        match c {
            Clause::Match { patterns, .. } | Clause::Create { patterns } => {
                for p in patterns {
                    add_path_scope(&mut names, p);
                }
            }
            Clause::Merge { pattern, .. } => add_path_scope(&mut names, pattern),
            Clause::Unwind { alias, .. } => push_scope_name(&mut names, alias.clone()),
            Clause::Call { yield_items, .. } => {
                for name in yield_items.effective_names() {
                    push_scope_name(&mut names, name.to_owned());
                }
            }
            Clause::With { items, .. } => {
                // A `WITH` replaces the scope; under `WITH *` it keeps the
                // names bound so far and adds the explicit items.
                let carried = std::mem::take(&mut names);
                names = return_columns(items, move || carried);
            }
            Clause::Return { .. } => break,
            _ => {}
        }
    }
    names.sort();
    names
}

/// Whether one projection item is the `*` item — the one spelling of the
/// per-item predicate every use site derives from.
pub(crate) fn is_star_item(item: &Projection) -> bool {
    matches!(item.expr, Expr::Star)
}

/// Whether a projection's item list carries a `*` item — the one spelling
/// of the list-level predicate, consulted by [`return_columns`] (which
/// expands it), the `WITH` arm of [`star_scope_names`] (which replaces the
/// carried scope) and the liveness walk ([`super::pushdown`], which gives
/// up on it).
pub(crate) fn has_star_item(items: &[Projection]) -> bool {
    items.iter().any(is_star_item)
}

/// The `__` namespace reservation (OPLANE_REQ-00328162): no user-authored
/// output name (alias) may fall in the internal `__` namespace on an
/// optimized path. The one site where such an alias can collide with
/// engine-internal state is the match sink, whose `SinkProjection` parks
/// its `__ord*` sort keys on the row — the sink declines a colliding alias
/// and the ordinary projection (which parks nothing) keeps the user column.
/// The aggregate folds synthesize no internal row names (their state lives
/// in group tables, caches, and slot vectors), but decline all the same so
/// every optimized path answers a user `__`-alias identically.
pub(crate) fn projection_collides_with_internal_names(items: &[Projection]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(index, item)| is_internal_column(&item.output_name(index)))
}

/// The result columns of a `RETURN`/`WITH` item list: the names its `*`
/// item expands, followed by every non-star item's output name. `star` is
/// called only when the list has a `*` item, so each caller picks the
/// expansion source (the first row's visible keys, the declared scope for
/// an empty table, the `WITH`'s carried-over scope). Dedup-preserving: the
/// first occurrence wins, so an explicit item repeating a star name keeps
/// the star's place.
///
/// The one implementation of that policy: the projection's result columns
/// ([`super::project::projection_result_columns`]), the `CALL { … }` export
/// set ([`super::call::call_export_columns`]) and a `WHEN`/`ELSE` clause's
/// declared columns ([`super::call::conditional_columns`]) all call it, so a
/// star can never silently drop its sibling items.
pub(crate) fn return_columns(
    items: &[Projection],
    star: impl FnOnce() -> Vec<String>,
) -> Vec<String> {
    let mut columns = if has_star_item(items) {
        star()
    } else {
        Vec::new()
    };
    for (index, item) in items.iter().enumerate() {
        if is_star_item(item) {
            continue;
        }
        let name = item.output_name(index);
        if !columns.contains(&name) {
            columns.push(name);
        }
    }
    columns
}

/// The names a `CALL { … }` clause reads from the enclosing row, or `None`
/// when its scope reads the whole row (the naked `CALL { … }` and
/// `CALL (*) { … }` forms). The one statement of that rule, at the
/// granularity both liveness walks need: the `UNWIND` source strip asks
/// whether a given column is among them ([`super::unwind`]), the expression
/// liveness pass takes the whole set ([`super::pushdown::live_from`]).
pub(crate) fn call_scope_reads(scope: &CallScope) -> Option<&[String]> {
    match scope {
        CallScope::Vars(vars) => Some(vars),
        CallScope::None | CallScope::Wildcard => None,
    }
}

pub(crate) fn visible_keys(row: &Row) -> Vec<String> {
    row.keys()
        .filter(|k| !is_internal_column(k))
        .cloned()
        .collect()
}

/// The seed row of a nested body that sees the enclosing row as its whole
/// scope — the implicit-import forms `CALL { … }`, `CALL (*) { … }` and the
/// expression subqueries `EXISTS { … }` / `COUNT { … }` / `COLLECT { … }`:
/// the enclosing row without the clause engine's internal columns. A
/// `__`-prefixed slot (the match sink's parked `__ord*` sort keys, the
/// `__agg_*` seeds, other clause temporaries) describes the *enclosing*
/// clause and never the body's own state; a body that inherited one reads
/// it as its own and skips items it never evaluated.
///
/// Not for the imperative bodies (`exec_conditional`'s CIP-135 `WHEN`/
/// `ELSE` branches, `exec_foreach`'s per-element body): they run on the
/// caller's row unchanged — a sink projection only ever feeds the clause
/// immediately after its MATCH, so a `Conditional`/`Foreach` body can never
/// receive sink-parked `__ord*`/`__agg_*` rows, and no strip is needed.
pub(crate) fn nested_seed_row(row: &Row) -> Row {
    let mut seed = row.clone();
    seed.retain(|name, _| !is_internal_column(name));
    seed
}

pub(crate) fn refresh_rows(graph: &ExecGraph<'_>, rows: &mut [Row]) {
    for row in rows {
        let keys: Vec<_> = row.keys().cloned().collect();
        for k in keys {
            match row.get(&k) {
                Some(Value::Node(n)) => {
                    if let Some(fresh) = graph.node(n.id) {
                        row.insert(k, Value::from_node(fresh));
                    } else if graph.is_deleted_node(n.id) {
                        row.insert(
                            k,
                            Value::from_node(cypherlite_core::Node {
                                id: n.id,
                                labels: Arc::new(Vec::new()),
                                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
                            }),
                        );
                    }
                }
                Some(Value::Relationship(r)) => {
                    if let Some(fresh) = graph.rel(r.id) {
                        row.insert(k, Value::from_relationship(fresh));
                    } else if graph.is_deleted_rel(r.id) {
                        row.insert(
                            k,
                            Value::from_relationship(cypherlite_core::Relationship {
                                id: r.id,
                                start: r.start,
                                end: r.end,
                                rel_type: r.rel_type.clone(),
                                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
                            }),
                        );
                    }
                }
                _ => {}
            }
        }
    }
}
