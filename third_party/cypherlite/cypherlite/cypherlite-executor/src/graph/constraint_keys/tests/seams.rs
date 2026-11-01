//! The committed map and its charge: the `apply_redo` seams, the wholesale
//! rebuild triggers (open/abort/graph type), and the entry-growth estimate
//! the capacity check prices — each priced against the bytes the same ops
//! actually charge.

use cypherlite_core::Value;

use crate::graph::Graph;
use crate::redo::RedoOp;

use super::super::ConstraintKeyScope;
use super::{
    create_node, create_rel, key, seeded, seeded_constrained, seeded_two_p_emails, tuple, unique,
    unique_rel,
};

#[test]
fn install_backfills_live_entities_and_charges_entries() {
    let graph = seeded();
    assert_eq!(graph.constraint_keys_structure_bytes(), 0);
    graph
        .apply_redo(vec![unique("c", "P", &["email"])], 2)
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(1)])),
        1
    );
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(2)])),
        1
    );
    // Q is outside the constraint's label.
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(3)])),
        0
    );
    assert!(graph.constraint_keys_structure_bytes() > 0);
    // The recount oracle includes the map term exactly: after the backfill
    // the incremental charge matches the live walk.
    assert_eq!(graph.used_bytes(), graph.accounted_bytes());
}

#[test]
fn delete_and_property_removal_free_and_refund() {
    let graph = seeded_constrained();
    let charged = graph.constraint_keys_structure_bytes();
    graph
        .apply_redo(vec![RedoOp::DeleteNode { id: 0 }], 3)
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(1)])),
        0
    );
    assert!(graph.constraint_keys_structure_bytes() < charged);
    // A `SET x = null` is a RemoveNodeProp at the primitive and frees too.
    graph
        .apply_redo(
            vec![RedoOp::RemoveNodeProp {
                id: 1,
                key: "email".into(),
            }],
            4,
        )
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(2)])),
        0
    );
    graph
        .apply_redo(vec![RedoOp::DropConstraint { name: "c".into() }], 5)
        .unwrap();
    assert_eq!(graph.constraint_keys_structure_bytes(), 0);
}

#[test]
fn set_property_swaps_the_tuple() {
    let graph = seeded_constrained();
    graph
        .apply_redo(
            vec![RedoOp::SetNodeProp {
                id: 0,
                key: "email".into(),
                value: Value::Int(9),
            }],
            3,
        )
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(1)])),
        0
    );
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(9)])),
        1
    );
}

#[test]
fn label_add_and_remove_introduce_and_free() {
    let graph = seeded();
    graph
        .apply_redo(vec![unique("c", "Q", &["email"])], 2)
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(1)])),
        0
    );
    graph
        .apply_redo(
            vec![RedoOp::AddLabels {
                id: 0,
                labels: vec!["Q".into()],
            }],
            3,
        )
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(1)])),
        1
    );
    graph
        .apply_redo(
            vec![RedoOp::RemoveLabels {
                id: 0,
                labels: vec!["Q".into()],
            }],
            4,
        )
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(1)])),
        0
    );
}

#[test]
fn rebuild_carries_replay_duplicates_forward() {
    let graph = Graph::default();
    // A legacy image/WAL records the duplicate; apply is a state recorder.
    graph
        .apply_redo(
            vec![
                create_node(0, "P", &[("email", Value::Int(1))]),
                create_node(1, "P", &[("email", Value::Int(1))]),
                unique("c", "P", &["email"]),
            ],
            1,
        )
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(1)])),
        2
    );
    let charged = graph.constraint_keys_structure_bytes();
    // The rebuild re-tallies `used_bytes` from the live walk, so the oracle
    // that matters is that the re-tally does not MOVE it: a drifted
    // incremental charge would surface as a difference here.
    let used_before_rebuild = graph.used_bytes();
    graph.rebuild_indexes();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(1)])),
        2
    );
    assert_eq!(graph.constraint_keys_structure_bytes(), charged);
    assert_eq!(graph.used_bytes(), used_before_rebuild);
}

/// ADR-0439 Consequences: the recount oracle matches the incremental charge
/// after an abort. `abort_write` drops the aborted versions, then rebuilds
/// the map and re-tallies `used_bytes`; an aborted tuple left in the map or
/// an unrecomputed charge would surface here.
#[test]
fn abort_write_rebuilds_the_map_and_the_recount_agrees() {
    let graph = seeded_constrained();
    // The committed charge the abort must return to: the rebuild re-tallies
    // `used_bytes`, so the oracle is that the aborted write leaves it where
    // the committed state had it (a leaked incremental charge would move it).
    let committed_used = graph.used_bytes();
    graph.begin_write().unwrap();
    graph
        .create_node(
            vec!["P".into()],
            std::collections::BTreeMap::from([("email".to_string(), Value::Int(7))]),
        )
        .unwrap();
    graph.abort_write();
    assert_eq!(
        graph.constraint_key_holder_count("c", &tuple([Value::Int(7)])),
        0,
        "the aborted holder is gone from the rebuilt map"
    );
    assert_eq!(graph.used_bytes(), committed_used);
}

/// ADR-0439: the graph-type arm re-derives the derived constraint set from
/// the persisted body, so a replay record that carries only
/// `SetCurrentGraphType` -- the legacy shape, without the `CreateConstraint`
/// rows a current statement emits first -- must still install the derived
/// uniqueness constraints into the maintained key map. The arm's wholesale
/// rebuild is that backstop: the per-op installs cannot see a constraint
/// whose only record is the re-derived body.
#[test]
fn bare_graph_type_replay_installs_the_derived_key_map() {
    let g = Graph::default();
    g.apply_redo(vec![create_node(1, "Site", &[("name", Value::Int(7))])], 1)
        .unwrap();
    let cgt = site_key_graph_type();
    let spec =
        crate::graph_type::GraphTypeSpec::from_body(&cgt.body).expect("shape-validated body");
    // No `CreateConstraint` row: the legacy record shape.
    g.apply_redo(vec![RedoOp::SetCurrentGraphType { body: spec }], 2)
        .unwrap();
    let key = crate::graph_type::derive_constraints(&cgt.body)
        .expect("a validated stored body derives")
        .into_iter()
        .find(|constraint| constraint.constraint_type.enforces_uniqueness())
        .expect("the body derives a key constraint");
    assert_eq!(
        g.constraint_key_holder_count(&key.name, &tuple([Value::Int(7)])),
        1,
        "the derived key constraint's committed tuple is in the map"
    );
    assert!(g.constraint_keys_structure_bytes() > 0);
}

/// ADR-0439, current-statement shape: `sync_derived_constraints` skips a
/// derived constraint that is schema-equivalent to a catalog constraint
/// surviving the purge, so that derived constraint gets no
/// `CreateConstraint` row -- while the persisted body's derived set still
/// names it, and the map covers catalog ∪ derived. The arm's rebuild is
/// what installs the skipped name; the per-op installs alone leave it out.
#[test]
fn graph_type_add_installs_a_skipped_derived_key_spec() {
    use crate::test_support::exec_autocommit;
    let g = Graph::default();
    exec_autocommit(&g, "CREATE (:Site {name: 1})").unwrap();
    exec_autocommit(
        &g,
        "CREATE CONSTRAINT my_key FOR (s:Site) REQUIRE s.name IS KEY",
    )
    .unwrap();
    exec_autocommit(
        &g,
        "ALTER CURRENT GRAPH TYPE ADD { (s:Site => {name :: INTEGER IS KEY}) }",
    )
    .unwrap();
    let catalog: Vec<String> = g
        .named_constraints()
        .iter()
        .map(|constraint| constraint.name.clone())
        .collect();
    let derived: Vec<String> = g
        .theater
        .lock()
        .unwrap()
        .derived_constraints
        .keys()
        .cloned()
        .collect();
    // The sync skipped the derived key against the surviving equivalent:
    // only the user's constraint is a catalog row.
    assert!(catalog.iter().any(|name| name == "my_key"));
    assert!(!catalog
        .iter()
        .any(|name| name == "constraint_key_Site_name"));
    assert!(derived
        .iter()
        .any(|name| name == "constraint_key_Site_name"));
    let specs: Vec<String> = g
        .constraint_key_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    // The map covers both names; without the arm's rebuild only `my_key`
    // would be present.
    assert!(specs.iter().any(|name| name == "my_key"));
    assert!(specs.iter().any(|name| name == "constraint_key_Site_name"));
    assert_eq!(
        g.constraint_key_holder_count("constraint_key_Site_name", &tuple([Value::Int(1)])),
        1
    );
    assert_eq!(
        g.constraint_key_holder_count("my_key", &tuple([Value::Int(1)])),
        1
    );
    // Symmetric with the replay test: the rebuild charged both names'
    // entry sets (a charge regression in the skipped-name install would
    // otherwise pass unnoticed).
    assert!(g.constraint_keys_structure_bytes() > 0);
}

#[test]
fn relationship_constraint_tracks_relationship_tuples() {
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![
                create_node(0, "N", &[]),
                create_node(1, "N", &[]),
                unique_rel("r", "R", &["k"]),
            ],
            1,
        )
        .unwrap();
    assert!(graph
        .constraint_key_spec("r")
        .is_some_and(|spec| spec.is_relationship()));
    graph
        .apply_redo(vec![create_rel(0, 0, 1, "R", &[("k", Value::Int(1))])], 2)
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("r", &tuple([Value::Int(1)])),
        1
    );
    graph
        .apply_redo(vec![RedoOp::DeleteRel { id: 0 }], 3)
        .unwrap();
    assert_eq!(
        graph.constraint_key_holder_count("r", &tuple([Value::Int(1)])),
        0
    );
}

#[test]
fn scope_stays_the_union_of_specs_across_install_drop_and_rebuild() {
    fn assert_scope_matches_specs(graph: &Graph) {
        let specs = graph.constraint_key_specs();
        let expected = ConstraintKeyScope::from_specs(&specs);
        assert_eq!(
            graph.constraint_keys_scope(),
            expected,
            "scope drifted from the map's spec set"
        );
    }
    let graph = seeded();
    assert_scope_matches_specs(&graph);
    graph
        .apply_redo(
            vec![unique("c", "P", &["email"]), unique_rel("r", "R", &["k"])],
            2,
        )
        .unwrap();
    assert_scope_matches_specs(&graph);
    graph
        .apply_redo(vec![RedoOp::DropConstraint { name: "c".into() }], 3)
        .unwrap();
    assert_scope_matches_specs(&graph);
    graph.rebuild_indexes();
    assert_scope_matches_specs(&graph);
}

/// The one `Site` graph-type fixture: a node element type whose `name` is a
/// key, parsed and shape-checked on demand.
fn site_key_graph_type() -> cypherlite_parser::CreateGraphTypeClause {
    let stmt =
        cypherlite_parser::parse("CREATE GRAPH TYPE gt { (s:Site => {name :: STRING IS KEY}) }")
            .expect("parses");
    match stmt.clauses.into_iter().next() {
        Some(cypherlite_parser::Clause::CreateGraphType(cgt)) => cgt,
        _ => panic!("expected CreateGraphType"),
    }
}

// ---------------------------------------------------------------------------
// Entry-growth estimate exactness (the capacity-admission pricing).
//
// `estimate_constraint_key_entry_growth` is the constraint-key term of
// `estimate_redo_delta`; measuring the map's own charge before/after the
// same ops isolates it, so equality is byte-exact rather than drowned in
// the node/adjacency/stats terms.
// ---------------------------------------------------------------------------

/// Apply `ops` after pricing them: returns `(estimate, actual map charge
/// delta)` and pins the recount oracle on the applied state.
fn estimate_and_apply(graph: &Graph, ops: Vec<RedoOp>) -> (i64, i64) {
    let before = graph.constraint_keys_structure_bytes();
    let estimate = graph.estimate_constraint_key_entry_growth(&ops);
    graph.apply_redo(ops, graph.commit_ts() + 1).unwrap();
    assert_eq!(
        graph.used_bytes(),
        graph.accounted_bytes(),
        "the recount oracle agrees with the incremental map charge"
    );
    let actual = graph.constraint_keys_structure_bytes() as i64 - before as i64;
    (estimate, actual)
}

/// One entry with one holder: the map slot floor (112) plus one holder id
/// (`EntityId`, 16 bytes) plus one 32-byte component.
const ONE_ENTRY: i64 = 160;

/// (a) A `CreateConstraint` and a write in one batch: the install prices the
/// committed backfill, the write swaps one tuple for another (net zero), and
/// the estimate prices exactly that.
#[test]
fn estimate_is_exact_for_create_constraint_with_writes() {
    let graph = seeded_two_p_emails();
    let ops = vec![
        unique("c", "P", &["email"]),
        RedoOp::SetNodeProp {
            id: 0,
            key: "email".into(),
            value: key(3),
        },
    ];
    let (estimate, actual) = estimate_and_apply(&graph, ops);
    assert_eq!(estimate, actual);
    assert_eq!(actual, 2 * ONE_ENTRY);
}

/// (b) A same-name schema change: the drop refunds the committed entries
/// exactly and the re-install reprices the backfill, so the net is the
/// batch's one new entity.
#[test]
fn estimate_is_exact_for_same_name_schema_reinstall() {
    let graph = seeded_two_p_emails();
    graph
        .apply_redo(
            vec![
                create_node(2, "P", &[("email", key(3))]),
                unique("c", "P", &["email"]),
            ],
            2,
        )
        .unwrap();
    let ops = vec![
        RedoOp::DropConstraint { name: "c".into() },
        unique("c", "P", &["email"]),
        create_node(3, "P", &[("email", key(4))]),
    ];
    let (estimate, actual) = estimate_and_apply(&graph, ops);
    assert_eq!(estimate, actual);
    // -3 entries (drop) + 3 entries (re-backfill) + 1 introduced entity.
    assert_eq!(actual, ONE_ENTRY);
}

/// (c) A duplicate identical install twice in one batch: applying the second
/// refunds the first and re-backfills, so the net is ONE install; the
/// estimate must not double-charge.
#[test]
fn estimate_is_exact_for_duplicate_identical_installs() {
    let graph = seeded_two_p_emails();
    let ops = vec![unique("c", "P", &["email"]), unique("c", "P", &["email"])];
    let (estimate, actual) = estimate_and_apply(&graph, ops);
    assert_eq!(estimate, actual);
    assert_eq!(actual, 2 * ONE_ENTRY);
}

/// (d) A `SetCurrentGraphType` carrying a derived `IS KEY` constraint: the
/// install seam derives the same spec the graph-type arm rebuilds from, so
/// the estimate prices the backfill the arm actually charges.
#[test]
fn estimate_is_exact_for_in_batch_graph_type_install() {
    let graph = Graph::default();
    graph
        .apply_redo(vec![create_node(1, "Site", &[("name", key(7))])], 1)
        .unwrap();
    let cgt = site_key_graph_type();
    let spec =
        crate::graph_type::GraphTypeSpec::from_body(&cgt.body).expect("shape-validated body");
    let ops = vec![RedoOp::SetCurrentGraphType { body: spec }];
    let (estimate, actual) = estimate_and_apply(&graph, ops);
    assert_eq!(estimate, actual);
    assert_eq!(actual, ONE_ENTRY);
}

/// (e) A `DropConstraint` and entity frees in one batch: the drop refunds
/// the committed entries, and the frees of an already-dropped constraint
/// price nothing more (the batch's final effective set no longer names it).
#[test]
fn estimate_is_exact_for_drop_constraint_with_entity_frees() {
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![
                create_node(0, "P", &[("email", key(1))]),
                create_node(1, "P", &[("email", key(2))]),
                unique("c", "P", &["email"]),
            ],
            1,
        )
        .unwrap();
    let ops = vec![
        RedoOp::DropConstraint { name: "c".into() },
        RedoOp::DeleteNode { id: 0 },
    ];
    let (estimate, actual) = estimate_and_apply(&graph, ops);
    assert_eq!(estimate, actual);
    assert_eq!(actual, -(2 * ONE_ENTRY));
}
