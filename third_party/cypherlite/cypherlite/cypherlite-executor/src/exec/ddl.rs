//! Index and constraint DDL (`CREATE`/`DROP INDEX`/`CONSTRAINT`).

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result};
use cypherlite_ir::DropIndexTarget;

use crate::overlay::ExecGraph;
use crate::redo::{IndexDetailSpec, IndexKindSpec, RedoOp};

pub(crate) fn default_index_name(label: &str, property: &str) -> String {
    format!("{label}_{property}")
}

/// Default name of a `CREATE CONSTRAINT` without an explicit name — the
/// one place the `{label}_{props}_cstr` rule lives.
pub(crate) fn default_constraint_name(label: &str, properties: &[String]) -> String {
    format!("{label}_{}_cstr", properties.join("_"))
}

/// Create the range backing index of an index-backed (`unique`/`key`)
/// constraint: the index shares the constraint's name and label and
/// encodes the first constrained property with the shared legacy encoder.
/// The one place this backing-index rule lives. The recorded detail
/// carries **every** constrained property — SHOW INDEXES reports the
/// composite backing index with the constraint's full property list
/// (CIP-11/46/62), and `covered_pairs` materializes a range pair for
/// each, so an equality seek on any constrained property is served. Only
/// the legacy `CreateIndex` redo stays first-property-only (the
/// single-property legacy encoding).
///
/// `relationship` is the constraint's entity kind: a relationship
/// constraint's backing detail records the flag truthfully (SHOW INDEXES
/// reports it as a relationship index), and `covered_pairs` — the
/// postings machinery being node-only today — yields no pairs for it, so
/// nothing is materialized, exactly as before this fix.
pub(crate) fn create_constraint_backing_index(
    graph: &ExecGraph<'_>,
    name: &str,
    relationship: bool,
    label: &str,
    properties: &[String],
) -> Result<()> {
    // Only index-backed (`unique`/`key`) constraints reach here, and the
    // parser guarantees those carry at least one property
    // (`parse_create_constraint` and the graph-type constraint forms all
    // require a non-empty property spec) — an empty list would encode a
    // malformed `"#RANGE#<name>"`, so the invariant is an expect, never a
    // silent default.
    let property = properties
        .first()
        .expect("index-backed constraints carry at least one property (parser guarantee)");
    // The catalog detail is what materializes the backing index's seek
    // postings (`RedoOp::CatalogIndexDetail` → `materialize_detail` — the
    // same path `exec_create_index` takes through
    // [`push_range_index_detail`]). Without it the constraint's backing
    // index is a catalog ghost: SHOW INDEXES lists it, but every equality
    // lookup on the constrained property label-scans instead of seeking —
    // the LABS-328 quadratic merge/match. The encoded `CreateIndex` redo
    // below keeps the catalog's `IndexDef` (whose property consumers split
    // back via `split_legacy_property_encoding`).
    push_range_index_detail(graph, name, relationship, label, properties);
    // The constraint's backing index is part of the constraint, so it is
    // not counted in `QueryStats::indexes_added` (ADR-0309) — the branch's
    // `create_backing_index` call, kept beside main's LABS-328 detail push.
    graph.create_backing_index(
        name.to_string(),
        label.to_string(),
        crate::redo::legacy_property_encoding(property, crate::redo::IndexKindSpec::Range, name),
    )
}

/// The one construction of the single-label range index detail redo —
/// the push that puts the index's seek postings into the floor
/// (ADR-0202) and materializes them at commit. Both the `CREATE INDEX`
/// IR path and the index-backed constraints' backing indexes record the
/// identical detail through this one place; `properties` is the full
/// property list (`covered_pairs` materializes one range pair each).
fn push_range_index_detail(
    graph: &ExecGraph<'_>,
    name: &str,
    relationship: bool,
    label: &str,
    properties: &[String],
) {
    graph.push_catalog_redo(RedoOp::CatalogIndexDetail {
        name: name.to_string(),
        detail: IndexDetailSpec {
            kind: IndexKindSpec::Range,
            relationship,
            labels: vec![label.to_string()],
            properties: properties.to_vec(),
            with_properties: vec![],
        },
        options: crate::redo::props_json(&BTreeMap::new()),
    });
}

pub(crate) fn exec_create_index(
    graph: &ExecGraph<'_>,
    name: Option<&str>,
    if_not_exists: bool,
    label: &str,
    property: &str,
) -> Result<()> {
    let name = name
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_index_name(label, property));
    if let Some(existing) = graph.named_index(&name) {
        // One fall-through error for both the schema-equal and the
        // schema-different case: the two branches used to return
        // byte-identical errors (ADR-0339's dedup rule).
        if existing.label == label && existing.property == property && if_not_exists {
            return Ok(());
        }
        return Err(Error::semantic(format!(
            "IndexAlreadyExists: `{name}` already exists"
        )));
    }
    if let Some(other) = graph.named_index_by_schema(label, property) {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::semantic(format!(
            "EquivalentSchemaRuleAlreadyExists: (:{label} {{{property}}}) already indexed as `{}`",
            other.name
        )));
    }
    // ADR-0372: the serving pair conflicts — this fast path creates a
    // RANGE index (the IR path is the single-label range index), so an
    // index-backed constraint on the same label/property conflicts (the
    // backing would duplicate); non-backed constraints (property type,
    // existence) coexist, matching the catalog direction's narrowed rule.
    // `IF NOT EXISTS` treats the existing rule as satisfying, and the error
    // is byte-identical to the catalog direction's (ADR-0339's dedup).
    // ADR-0372: the serving-pair conflict, the fast path's form. This path
    // is the node-only single-label range index (the IR path), so the index
    // side is loop-invariant and built once above the scan; the closure
    // builds only the per-constraint side.
    let node_label = label.to_string();
    let node_property = property.to_string();
    let index_side = crate::redo::ServingPairIndex {
        kind: crate::redo::IndexKindSpec::Range,
        relationship: false,
        labels: std::slice::from_ref(&node_label),
        properties: std::slice::from_ref(&node_property),
    };
    if graph.named_constraints().iter().any(|constraint| {
        crate::redo::serving_pair_conflicts(
            index_side,
            crate::redo::ServingPairConstraint {
                backed: constraint.is_index_backed(),
                relationship: constraint.relationship,
                label: &constraint.label,
                properties: &constraint.properties,
            },
        )
    }) {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::semantic("IndexWithNameAlreadyExists"));
    }
    // The IR path is the single-label range index. Record the same detail the
    // catalog path records for TEXT and POINT, so the range posting is in
    // the floor (ADR-0202) and drop removes it by name.
    let detail_properties = [property.to_string()];
    push_range_index_detail(graph, &name, false, label, &detail_properties);
    graph.create_named_index(name, label.to_string(), property.to_string())
}

pub(crate) fn exec_drop_index(
    graph: &ExecGraph<'_>,
    if_exists: bool,
    target: &DropIndexTarget,
) -> Result<()> {
    let name = match target {
        DropIndexTarget::Name(n) => n.clone(),
        DropIndexTarget::Schema { label, property } => {
            match graph.named_index_by_schema(label, property) {
                Some(d) => d.name,
                None if if_exists => return Ok(()),
                None => {
                    return Err(Error::semantic(format!(
                        "NoSuchIndex: (:{label} {{{property}}})"
                    )))
                }
            }
        }
    };
    if graph.named_index(&name).is_none() {
        if if_exists {
            return Ok(());
        }
        return Err(Error::semantic(format!("NoSuchIndex: `{name}`")));
    }
    graph.drop_named_index(name)
}

/// Field references of a `CREATE CONSTRAINT` clause.
#[derive(Clone, Copy)]
pub(crate) struct ConstraintSpec<'a> {
    pub name: Option<&'a str>,
    pub if_not_exists: bool,
    pub relationship: bool,
    pub label: &'a str,
    pub properties: &'a [String],
    pub kind: &'a str,
    pub property_type: Option<&'a cypherlite_ir::TypeExpr>,
}

pub(crate) fn exec_create_constraint(
    graph: &ExecGraph<'_>,
    spec: &ConstraintSpec<'_>,
) -> Result<()> {
    let ConstraintSpec {
        name,
        if_not_exists,
        relationship,
        label,
        properties,
        kind,
        property_type,
    } = *spec;
    let name = name
        .map(|s| s.to_string())
        .unwrap_or_else(|| default_constraint_name(label, properties));
    let property_type = property_type.map(type_expr_name);
    let constraints = graph.named_constraints();
    if constraints.iter().any(|d| d.name == name) {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::semantic(format!(
            "ConstraintAlreadyExists: `{name}`"
        )));
    }
    if constraints.iter().any(|d| {
        d.schema_equivalent(
            relationship,
            label,
            properties,
            kind,
            property_type.as_deref(),
        )
    }) {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::semantic(
            "EquivalentSchemaRuleAlreadyExists: equivalent constraint exists",
        ));
    }
    create_constraint_core(
        graph,
        name,
        relationship,
        label.to_string(),
        properties.to_vec(),
        kind.to_string(),
        property_type,
    )
}

/// The creation half of [`exec_create_constraint`]: insert the named
/// constraint and, when its kind is index-backed, its range backing index
/// (an index already holding the name is left alone). Shared with
/// [`sync_derived_constraints`], which layers its own skip policy
/// (schema-equivalent survivors stay) in front instead of `CREATE
/// CONSTRAINT`'s error behaviors.
fn create_constraint_core(
    graph: &ExecGraph<'_>,
    name: String,
    relationship: bool,
    label: String,
    properties: Vec<String>,
    kind: String,
    property_type: Option<String>,
) -> Result<()> {
    // Index-backedness is a function of the in-scope kind tag alone —
    // derive it here rather than re-reading the catalog row after the
    // insert.
    let index_backed = cypherlite_catalog::constraint_kind_is_index_backed(&kind);
    graph.create_named_constraint(
        name.clone(),
        relationship,
        label.clone(),
        properties.clone(),
        kind,
        property_type,
    )?;
    // An index-backed constraint (unique/key) owns a range backing index
    // under the same effective name; an index already holding the name is
    // left alone.
    if index_backed && graph.named_index(&name).is_none() {
        create_constraint_backing_index(graph, &name, relationship, &label, &properties)?;
    }
    Ok(())
}

pub(crate) fn exec_drop_constraint(
    graph: &ExecGraph<'_>,
    name: &str,
    if_exists: bool,
) -> Result<()> {
    if !graph.named_constraints().iter().any(|d| d.name == name) {
        if if_exists {
            return Ok(());
        }
        return Err(Error::semantic(format!("NoSuchConstraint: `{name}`")));
    }
    graph.drop_named_constraint(name.to_string())?;
    if graph.named_index(name).is_some() {
        graph.drop_backing_index(name.to_string())?;
    }
    Ok(())
}

pub(crate) fn type_expr_name(ty: &cypherlite_ir::TypeExpr) -> String {
    match ty {
        cypherlite_ir::TypeExpr::Predefined { kind, not_null } => {
            use cypherlite_ir::PredefinedType as T;
            let name = match kind {
                T::Nothing => "NOTHING",
                T::Null => "NULL",
                T::Any => "ANY",
                T::Boolean => "BOOLEAN",
                T::String => "STRING",
                T::Uuid => "UUID",
                T::Integer => "INTEGER",
                T::Float => "FLOAT",
                T::Date => "DATE",
                T::Duration => "DURATION",
                T::Point => "POINT",
                T::LocalTime => "LOCAL TIME",
                T::ZonedTime => "ZONED TIME",
                T::LocalDateTime => "LOCAL DATETIME",
                T::ZonedDateTime => "ZONED DATETIME",
                T::Node => "NODE",
                T::Relationship => "RELATIONSHIP",
                T::Path => "PATH",
                T::Map => "MAP",
            };
            format!("{name}{}", if *not_null { " NOT NULL" } else { "" })
        }
        cypherlite_ir::TypeExpr::List { element, not_null } => format!(
            "LIST<{}>{}",
            type_expr_name(element),
            if *not_null { " NOT NULL" } else { "" }
        ),
        cypherlite_ir::TypeExpr::PropertyValue { not_null } => {
            format!("PROPERTY VALUE{}", if *not_null { " NOT NULL" } else { "" })
        }
        cypherlite_ir::TypeExpr::Union(items) => items
            .iter()
            .map(type_expr_name)
            .collect::<Vec<_>>()
            .join(" | "),
        cypherlite_ir::TypeExpr::Vector {
            coord,
            dimension,
            not_null,
        } => {
            let coord = coord.as_deref().unwrap_or("FLOAT");
            let element = format!("{coord} NOT NULL");
            let dimension = dimension
                .map(|value| format!("({value})"))
                .unwrap_or_default();
            format!(
                "VECTOR<{element}>{dimension}{}",
                if *not_null { " NOT NULL" } else { "" }
            )
        }
    }
}

/// Which constraints an `ALTER CURRENT GRAPH TYPE` purges before the
/// derived set is re-synced (ADR-0211). `SET` replaces the whole type, so
/// every committed named constraint goes; every other verb purges only
/// the constraints the previous graph type derived.
pub(crate) fn constraints_to_purge(
    committed: Vec<String>,
    derived_before: &BTreeMap<String, cypherlite_model::Constraint>,
    verb: cypherlite_parser::AlterGraphTypeVerb,
) -> Vec<String> {
    if matches!(verb, cypherlite_parser::AlterGraphTypeVerb::Set) {
        committed
    } else {
        derived_before.keys().cloned().collect()
    }
}

/// Sync the physical catalog to a graph type's derived constraint set:
/// drop the purged constraints (backing indexes go with them, via the
/// canonical [`exec_drop_constraint`]) and create the derived ones,
/// skipping schema-equivalent survivors. The one place the
/// constraint-plus-backing-index invariant is applied to a bulk
/// re-sync, next to the single-statement DDL it mirrors.
pub(crate) fn sync_derived_constraints(
    graph: &ExecGraph<'_>,
    purge: Vec<String>,
    derived: Vec<crate::graph_type::DerivedConstraint>,
) -> Result<()> {
    for name in purge {
        exec_drop_constraint(graph, &name, true)?;
    }
    // The enforced label of each surviving constraint (the theater's
    // derived-constraint index, overlaid with this scratch's catalog
    // ops), read once: a label/endpoint-label existence constraint's
    // identity includes it -- two implied labels of one node element
    // type are two constraints (CIP-266's multi-parent `(:A => :B&C)`),
    // not a schema-equivalent duplicate of the first.
    // A borrowed index over the bound show_schema() snapshot (the
    // round-11 rust Low): no owned HashMap of cloned names and enforced
    // labels per DDL sync.
    let schema = graph.show_schema()?;
    let enforced_of = |name: &str| -> Option<&str> {
        schema
            .data
            .derived_constraints
            .get(name)
            .and_then(|constraint| constraint.enforced_label.as_deref())
    };
    for derived_constraint in derived {
        let crate::graph_type::DerivedConstraint { constraint, entity } = derived_constraint;
        let cypherlite_model::Constraint {
            name,
            label,
            properties,
            constraint_type,
            property_type,
            enforced_label,
            ..
        } = constraint;
        let relationship = entity.is_relationship();
        // A committed constraint that is schema-equivalent to the derived
        // one survives the purge; the derived constraint is skipped, not
        // re-created (no `CREATE CONSTRAINT` error behaviors here). The
        // enforced label joins the equivalence: without it, a node
        // element type's second implied label looked schema-equivalent to
        // its first and was silently skipped (never enforced).
        if graph.named_constraints().iter().any(|existing| {
            existing.schema_equivalent(
                relationship,
                &label,
                &properties,
                constraint_type.as_str(),
                property_type.as_deref(),
            ) && enforced_of(&existing.name) == enforced_label.as_deref()
        }) {
            continue;
        }
        create_constraint_core(
            graph,
            name,
            relationship,
            label,
            properties,
            constraint_type.as_str().to_string(),
            property_type,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use cypherlite_core::Value;

    use crate::exec::test_support::{exec_autocommit, exec_cypher};
    use crate::graph::Graph;
    use crate::overlay::ExecGraph;

    /// LABS-328: an index-backed constraint's backing range index is
    /// materialized (present in the schema's `index_details`), not a
    /// catalog ghost. Before the fix, `CREATE CONSTRAINT … REQUIRE n.id IS
    /// UNIQUE` recorded only the catalog `IndexDef` — SHOW INDEXES listed
    /// the backing range index, but no `CatalogIndexDetail` ever reached
    /// `materialize_detail`, so the seek postings never existed and every
    /// equality lookup on the constrained property label-scanned instead
    /// of seeking (the quadratic merge/match: 5k MERGE lookups took 5.5s;
    /// with postings, 0.047s).
    #[test]
    fn constraint_backing_index_is_materialized_not_a_catalog_ghost() {
        let g = Graph::default();
        exec_autocommit(
            &g,
            "UNWIND range(1, 100) AS id CREATE (p:Person {name: 'p'+id, id: id})",
        )
        .unwrap();
        exec_autocommit(
            &g,
            "CREATE CONSTRAINT bulk_person_id FOR (p:Person) REQUIRE p.id IS UNIQUE",
        )
        .unwrap();
        let view = ExecGraph::read(&g);
        let details = view.show_schema().unwrap().data.index_details;
        let backing = details.values().any(|detail| {
            detail.kind == crate::redo::IndexKindSpec::Range
                && !detail.relationship
                && detail.labels == ["Person"]
                && detail.properties == ["id"]
        });
        assert!(
            backing,
            "the constraint's backing range index must carry a materialized index detail"
        );
        // And the materialized postings serve equality: the lookup finds
        // the constrained node.
        let found = exec_cypher(&g, "MATCH (p:Person {id: 42}) RETURN p.id AS id");
        assert_eq!(
            found.rows.first().map(|row| row.first().cloned()),
            Some(Some(Value::Int(42))),
            "the seek path still answers correctly"
        );
    }
    /// ADR-0372: the schema-rule conflict is narrowed to serving pairs —
    /// only a RANGE index conflicts with an index-backed constraint on the
    /// same label/property. A property-type/existence constraint coexists
    /// with a FULLTEXT index in both creation orders (the Stack Overflow
    /// notebook case, LABS-275): before, the constraint-first order failed
    /// with `IndexWithNameAlreadyExists` and the declared schema was less
    /// expressive than the data's real one.
    #[test]
    fn fulltext_index_and_existence_constraint_coexist_both_orders() {
        assert_both_orders(
            "CREATE CONSTRAINT q_title_exists FOR (q:Question) REQUIRE q.title IS NOT NULL",
            "CREATE FULLTEXT INDEX ft1 FOR (q:Question) ON EACH [q.title]",
            true,
        );
    }

    /// The ADR-0372 both-orders verdict, the one canonical scaffold: run
    /// the pair in each order on a fresh graph and assert the expectation
    /// inside, naming the order and the pair that broke it. `coexists`
    /// false also asserts the conflict carries an AlreadyExists-class
    /// error (the ADR's vocabulary), so the collapsing of the second
    /// statement's `Result` to its outcome still pins the error class.
    fn assert_both_orders(first: &str, second: &str, coexists: bool) {
        for (order, (f, s)) in [(first, second), (second, first)].into_iter().enumerate() {
            let g = Graph::default();
            exec_autocommit(&g, f).unwrap();
            // One match on the outcome: the Ok/Err arm carries both the
            // coexistence verdict and (for conflicts) the error class, so
            // no arm re-derives what the other proved.
            match exec_autocommit(&g, s) {
                Ok(_) => assert!(
                    coexists,
                    "order {order} (`{f}` then `{s}`): expected a conflict"
                ),
                Err(e) => {
                    assert!(
                        !coexists,
                        "order {order} (`{f}` then `{s}`): expected coexistence"
                    );
                    assert!(
                        e.to_string().contains("AlreadyExists"),
                        "order {order}: the conflict must be an AlreadyExists-class error, got {e}"
                    );
                }
            }
        }
    }

    /// ADR-0372: the serving pair still conflicts in both orders — a RANGE
    /// index and an index-backed (uniqueness) constraint on the same
    /// label/property never coexist, because the constraint's backing
    /// (ADR-0339) would duplicate the range index.
    #[test]
    fn range_index_and_backed_constraint_conflict_both_orders() {
        assert_both_orders(
            "CREATE INDEX r1 FOR (q:Question) ON (q.title)",
            "CREATE CONSTRAINT q_title_unique FOR (q:Question) REQUIRE q.title IS UNIQUE",
            false,
        );
    }

    /// ADR-0372 (the blocker pin): a RANGE index coexists with a
    /// NON-backed constraint (existence) in both orders — the serving pair
    /// needs both halves (kind RANGE AND constraint backed); a
    /// property-type/existence constraint is not backed, so even a RANGE
    /// index coexists with it. Before the canonical-helper fix, the catalog
    /// direction gated on the kind alone and wrongly rejected this pair.
    #[test]
    fn range_index_and_existence_constraint_coexist_both_orders() {
        assert_both_orders(
            "CREATE INDEX r1 FOR (q:Question) ON (q.title)",
            "CREATE CONSTRAINT q_title_exists FOR (q:Question) REQUIRE q.title IS NOT NULL",
            true,
        );
    }

    /// ADR-0372: a FULLTEXT index coexists with an index-backed (uniqueness)
    /// constraint too — non-serving kinds never conflict — and SHOW INDEXES
    /// lists both: the full-text index and the constraint's own backing
    /// range index (`owningConstraint` set).
    #[test]
    fn fulltext_index_and_backed_constraint_coexist() {
        let g = Graph::default();
        exec_autocommit(
            &g,
            "CREATE FULLTEXT INDEX ft1 FOR (q:Question) ON EACH [q.title]",
        )
        .unwrap();
        exec_autocommit(
            &g,
            "CREATE CONSTRAINT q_title_unique FOR (q:Question) REQUIRE q.title IS UNIQUE",
        )
        .unwrap();
        let view = ExecGraph::read(&g);
        let details = view.show_schema().unwrap().data.index_details;
        let fulltext = details.values().any(|detail| {
            detail.kind == crate::redo::IndexKindSpec::Fulltext
                && detail.labels == ["Question"]
                && detail.properties == ["title"]
        });
        let backing = details.values().any(|detail| {
            detail.kind == crate::redo::IndexKindSpec::Range
                && detail.labels == ["Question"]
                && detail.properties == ["title"]
        });
        assert!(fulltext, "the FULLTEXT index must coexist");
        assert!(backing, "the constraint's backing range index must exist");
    }
}
