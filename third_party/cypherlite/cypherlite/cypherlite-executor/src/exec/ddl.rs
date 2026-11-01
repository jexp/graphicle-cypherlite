//! Index and constraint DDL (`CREATE`/`DROP INDEX`/`CONSTRAINT`).

use std::collections::BTreeMap;

use cypherlite_catalog::ConstraintDef;
use cypherlite_core::{Error, Result};
use cypherlite_ir::DropIndexTarget;

use crate::overlay::ExecGraph;
use crate::redo::{IndexDetailSpec, IndexKindSpec, RedoOp};

pub(crate) fn default_index_name(label: &str, property: &str) -> String {
    format!("{label}_{property}")
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

/// The one creation half of both constraint paths: install the named
/// constraint, push the derived theater record the statement path carries
/// (between the constraint row and its backing index), and, when the kind is
/// index-backed, its range backing index (an index already holding the name
/// is left alone). `CREATE CONSTRAINT` reaches it through the
/// `Clause::Catalog` passthrough's catalog handler;
/// [`sync_derived_constraints`] calls it directly with no derived record.
pub(crate) fn create_constraint_core(
    graph: &ExecGraph<'_>,
    def: ConstraintDef,
    derived: Option<cypherlite_model::Constraint>,
) -> Result<()> {
    let index_backed = def.is_index_backed();
    // The backing index needs the def's fields after it moves.
    let name = def.name.clone();
    let relationship = def.relationship;
    let label = def.label.clone();
    let properties = def.properties.clone();
    graph.create_named_constraint(def)?;
    if let Some(constraint) = derived {
        graph.push_catalog_redo(RedoOp::CatalogDerivedConstraint { constraint });
    }
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
    for derived_constraint in derived {
        let crate::graph_type::DerivedConstraint { constraint, entity } = derived_constraint;
        let def = ConstraintDef::from_model(&constraint, entity);
        // A committed constraint that is schema-equivalent to the derived
        // one survives the purge; the derived constraint is skipped, not
        // re-created (no `CREATE CONSTRAINT` error behaviors here). The
        // enforced label joins the equivalence (CIP-266's multi-parent
        // `(:A => :B&C)`): without it, a node element type's second implied
        // label looked schema-equivalent to its first and was silently
        // skipped. The effective view resolves a legacy row's label first
        // (ADR-0450), so an upgraded store keeps its old skip behavior.
        if graph
            .effective_constraints()?
            .iter()
            .any(|existing| existing.schema_equivalent(&def))
        {
            continue;
        }
        create_constraint_core(graph, def, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use cypherlite_core::Value;

    use crate::exec::test_support::{exec_autocommit, exec_cypher};
    use crate::graph::Graph;
    use crate::overlay::ExecGraph;

    /// The equivalence gate's two outcomes, pinned for the `exists` kind too:
    /// a duplicate schema rule is rejected whatever the kind, and
    /// `IF NOT EXISTS` accepts the existing rule unchanged.
    #[test]
    fn equivalent_constraint_schemas_are_rejected_and_if_not_exists_accepts() {
        let g = Graph::default();
        exec_autocommit(
            &g,
            "CREATE CONSTRAINT e1 FOR (n:P) REQUIRE n.name IS NOT NULL",
        )
        .unwrap();
        let error = match exec_autocommit(
            &g,
            "CREATE CONSTRAINT e2 FOR (n:P) REQUIRE n.name IS NOT NULL",
        ) {
            Ok(_) => panic!("an equivalent exists rule must be rejected"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "semantic error: EquivalentSchemaRuleAlreadyExists"
        );
        exec_autocommit(
            &g,
            "CREATE CONSTRAINT e3 IF NOT EXISTS FOR (n:P) REQUIRE n.name IS NOT NULL",
        )
        .unwrap();
        assert_eq!(g.named_constraints().len(), 1);
        exec_autocommit(&g, "CREATE CONSTRAINT u1 FOR (n:Q) REQUIRE n.id IS UNIQUE").unwrap();
        let error =
            match exec_autocommit(&g, "CREATE CONSTRAINT u2 FOR (n:Q) REQUIRE n.id IS UNIQUE") {
                Ok(_) => panic!("a duplicate unique rule must be rejected"),
                Err(error) => error,
            };
        assert_eq!(
            error.to_string(),
            "semantic error: EquivalentSchemaRuleAlreadyExists"
        );
    }

    /// The one `CREATE CONSTRAINT` path: the front-end lowers the clause into
    /// the `Clause::Catalog` passthrough (the IR has no constraint clause),
    /// and the catalog handler creates it. Pins the path the removed dead IR
    /// family could otherwise suggest exists.
    #[test]
    fn create_constraint_lowers_to_the_catalog_passthrough() {
        let query = cypherlite_front::parse_and_lower(
            "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.id IS UNIQUE",
        )
        .expect("lowers");
        let clauses = &query.parts[0].clauses;
        assert!(matches!(
            clauses.as_slice(),
            [cypherlite_ir::Clause::Catalog(command)]
                if matches!(
                    command.as_ref(),
                    cypherlite_parser::Clause::CreateConstraint(_)
                )
        ));
        let g = Graph::default();
        exec_autocommit(&g, "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.id IS UNIQUE").unwrap();
        assert!(g.named_constraints().iter().any(|def| def.name == "c"));
    }

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
    /// CIP-266: two label-existence rules on one label with different
    /// enforced labels are two schema rules (a multi-parent element type),
    /// so both create cleanly; a repeat of one enforced label is the same
    /// rule and is rejected. The catalog row carries the enforced label, so
    /// the identity survives the round trip through the redo op.
    #[test]
    fn enforced_label_joins_the_constraint_identity() {
        let g = Graph::default();
        exec_autocommit(&g, "CREATE CONSTRAINT q FOR (n:P) REQUIRE n:Q").unwrap();
        exec_autocommit(&g, "CREATE CONSTRAINT r FOR (n:P) REQUIRE n:R").unwrap();
        let names: Vec<String> = g
            .named_constraints()
            .iter()
            .map(|def| def.name.clone())
            .collect();
        assert_eq!(names, vec!["q", "r"]);
        assert_eq!(
            g.named_constraints()[0].enforced_label.as_deref(),
            Some("Q")
        );
        let error = match exec_autocommit(&g, "CREATE CONSTRAINT q_again FOR (n:P) REQUIRE n:Q") {
            Ok(_) => panic!("a repeat of one enforced label is the same schema rule"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "semantic error: EquivalentSchemaRuleAlreadyExists"
        );
    }

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
