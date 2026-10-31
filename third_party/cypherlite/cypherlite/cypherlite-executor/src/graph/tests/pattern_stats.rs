use super::*;

#[test]
fn pattern_stats_follow_labels_indexes_and_rel_deletes() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let manila = g
        .create_node(
            vec!["Place".into()],
            BTreeMap::from([("name".into(), Value::String("Manila".into()))]),
        )
        .unwrap();
    let paris = g
        .create_node(
            vec!["Place".into()],
            BTreeMap::from([("name".into(), Value::String("Paris".into()))]),
        )
        .unwrap();
    let person = g
        .create_node(
            vec!["Person".into()],
            BTreeMap::from([("name".into(), Value::String("Ada".into()))]),
        )
        .unwrap();
    let tag = g
        .create_node(
            vec!["Tag".into()],
            BTreeMap::from([("name".into(), Value::String("Copernicus".into()))]),
        )
        .unwrap();
    g.create_rel(person, manila, "LIVES_IN".into(), BTreeMap::new())
        .unwrap();
    g.create_rel(person, paris, "LIVES_IN".into(), BTreeMap::new())
        .unwrap();
    let extra = g
        .create_rel(person, tag, "HAS".into(), BTreeMap::new())
        .unwrap();
    g.commit_write();

    assert_eq!(g.pattern_label_rows("Place"), 2);
    assert_eq!(g.pattern_label_rows("Missing"), 0);
    assert_eq!(g.pattern_rel_type_rows("LIVES_IN"), 2);
    assert_eq!(g.pattern_rel_type_rows("HAS"), 1);
    assert_eq!(g.pattern_rel_type_rows("NOPE"), 0);
    assert_eq!(
        g.pattern_equality_rows("Place", "name", &Value::String("Manila".into())),
        None
    );

    g.materialize_range_index("Place", "name");
    assert_eq!(
        g.pattern_equality_rows("Place", "name", &Value::String("Manila".into())),
        Some(1)
    );
    assert_eq!(
        g.pattern_equality_rows("Place", "name", &Value::String("Nope".into())),
        Some(0)
    );

    g.materialize_text_index("Tag", "name");
    assert_eq!(g.pattern_contains_rows("Tag", "name", "pernicus"), Some(1));
    assert_eq!(g.pattern_contains_rows("Tag", "name", "zzz"), Some(0));
    assert_eq!(g.pattern_contains_rows("Place", "name", "Manila"), None);

    g.begin_write().unwrap();
    g.delete_rel(extra);
    g.commit_write();
    assert_eq!(g.pattern_rel_type_rows("HAS"), 0);
    assert_eq!(g.pattern_rel_type_rows("LIVES_IN"), 2);

    g.begin_write().unwrap();
    g.create_rel(person, manila, "LIVES_IN".into(), BTreeMap::new())
        .unwrap();
    g.abort_write();
    assert_eq!(g.pattern_rel_type_rows("LIVES_IN"), 2);
}

/// LABS-351: the (label, rel-type) degree histogram's tail — the estimate
/// the mean degree cannot express. A label of 6 nodes where one carries
/// 20 outgoing rels has a mean degree of ~3.3 but a tail of 16 (the floor
/// of 20); the gate that must never miss the supernode consults the tail.
#[test]
fn degree_tail_reports_the_supernode_not_the_mean() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let outs: Vec<_> = (0..26)
        .map(|_| g.create_node(vec!["Out".into()], BTreeMap::new()).unwrap())
        .collect();
    // Five degree-1 nodes and one degree-20 supernode, all Source.
    for out in outs.iter().take(5) {
        let src = g
            .create_node(vec!["Source".into()], BTreeMap::new())
            .unwrap();
        g.create_rel(src, *out, "R".into(), BTreeMap::new())
            .unwrap();
    }
    let super_node = g
        .create_node(vec!["Source".into()], BTreeMap::new())
        .unwrap();
    for o in outs.iter().take(20) {
        g.create_rel(super_node, *o, "R".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();

    // The mean: 25 rels over 6 sources ~ 4.2 — the supernode is invisible.
    let mean = g.pattern_rel_type_rows("R") / g.pattern_label_rows("Source");
    assert!(mean < 5, "mean degree {mean} hides the supernode");
    // The tail: the top non-empty bucket floors the supernode's degree 20
    // at 16 (bucket 4); the degree-1 nodes sit in bucket 0.
    assert_eq!(g.pattern_degree_tail("Source", "R"), Some(16));
    // A pair never written answers None, not zero.
    assert_eq!(g.pattern_degree_tail("Source", "NEVER"), None);
    assert_eq!(g.pattern_degree_tail("NoLabel", "R"), None);
}

/// LABS-351: the high-water semantics — a deleted rel does not move the
/// bucket down (the typed adjacency keeps tombstoned entries; the floor
/// only ever overestimates), and the adjacency rebuild recalibrates the
/// tail to the honest live state.
#[test]
fn degree_tail_is_high_water_until_the_rebuild_recalibrates() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let outs: Vec<_> = (0..10)
        .map(|_| g.create_node(vec!["Out".into()], BTreeMap::new()).unwrap())
        .collect();
    let src = g
        .create_node(vec!["Source".into()], BTreeMap::new())
        .unwrap();
    let rels: Vec<_> = outs
        .iter()
        .map(|o| g.create_rel(src, *o, "R".into(), BTreeMap::new()).unwrap())
        .collect();
    g.commit_write();
    assert_eq!(g.pattern_degree_tail("Source", "R"), Some(8)); // floor(10)=8

    // Delete half: the high-water floor stays (the conservative direction).
    g.begin_write().unwrap();
    for r in rels.iter().take(5) {
        g.delete_rel(*r);
    }
    g.commit_write();
    assert_eq!(g.pattern_degree_tail("Source", "R"), Some(8));

    // The rebuild recomputes from the live rels: degree 5 floors at 4.
    g.rebuild_adj();
    assert_eq!(g.pattern_degree_tail("Source", "R"), Some(4));
}

/// LABS-351's blocker fix, pinned: a label added after the edges exist
/// inherits the node's degree buckets immediately (`SET n:Hub` on an
/// already-connected supernode lands in the (Hub, type) histogram), and a
/// removed label stops owning them. The never-miss invariant holds for
/// labels at any time, not only at node create.
#[test]
fn degree_tail_follows_label_mutation() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let outs: Vec<_> = (0..10)
        .map(|_| g.create_node(vec!["Out".into()], BTreeMap::new()).unwrap())
        .collect();
    // Connected first, labeled after: the reverse of the create order.
    let src = g
        .create_node(vec!["Plain".into()], BTreeMap::new())
        .unwrap();
    for o in &outs {
        g.create_rel(src, *o, "R".into(), BTreeMap::new()).unwrap();
    }
    g.commit_write();
    assert_eq!(g.pattern_degree_tail("Plain", "R"), Some(8)); // floor(10)=8
    assert_eq!(g.pattern_degree_tail("Hub", "R"), None); // never written

    g.begin_write().unwrap();
    g.add_labels(src, &["Hub".into()]);
    g.commit_write();
    // The added label inherits the buckets at once — the invariant.
    assert_eq!(g.pattern_degree_tail("Hub", "R"), Some(8));

    g.begin_write().unwrap();
    g.remove_labels(src, &["Hub".into()]);
    g.commit_write();
    // The removed label no longer owns them.
    assert_eq!(g.pattern_degree_tail("Hub", "R"), None);
}

/// LABS-370: the (start-label, rel-type, end-label) pair counts are
/// exact — creates add, deletes remove, and the counting operator can
/// answer from them byte-identically to a scan.
#[test]
fn pair_counts_follow_creates_and_deletes() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let person = g
        .create_node(vec!["Person".into()], BTreeMap::new())
        .unwrap();
    let city = g
        .create_node(vec!["City".into(), "Place".into()], BTreeMap::new())
        .unwrap();
    let r1 = g
        .create_rel(person, city, "LIVES_IN".into(), BTreeMap::new())
        .unwrap();
    g.create_rel(person, city, "LIVES_IN".into(), BTreeMap::new())
        .unwrap();
    g.commit_write();

    assert_eq!(g.pattern_pair_rows("Person", "LIVES_IN", "City"), Some(2));
    assert_eq!(g.pattern_pair_rows("Person", "LIVES_IN", "Place"), Some(2));
    assert_eq!(g.pattern_pair_rows("City", "LIVES_IN", "Person"), None);
    assert_eq!(g.pattern_pair_rows("Person", "NOPE", "City"), None);

    g.begin_write().unwrap();
    g.delete_rel(r1);
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("Person", "LIVES_IN", "City"), Some(1));
    assert_eq!(g.pattern_pair_rows("Person", "LIVES_IN", "Place"), Some(1));
}

/// LABS-370: a label mutation moves the node's rel contributions between
/// the pair counts — both endpoint sides, added and removed labels
/// (the #398 blocker's lesson applied to the pair map from day one).
#[test]
fn pair_counts_follow_label_mutations_both_sides() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    let c = g.create_node(vec!["C".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.create_rel(c, b, "R".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("A", "R", "B"), Some(1));
    assert_eq!(g.pattern_pair_rows("C", "R", "B"), Some(1));

    // Start-side addition: A gains "A2"; its rel counts under A2 too.
    g.begin_write().unwrap();
    g.add_labels(a, &["A2".into()]);
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("A", "R", "B"), Some(1));
    assert_eq!(g.pattern_pair_rows("A2", "R", "B"), Some(1));

    // End-side addition: B gains "B2"; every rel into B counts under B2.
    g.begin_write().unwrap();
    g.add_labels(b, &["B2".into()]);
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("A", "R", "B2"), Some(1));
    assert_eq!(g.pattern_pair_rows("A2", "R", "B2"), Some(1));
    assert_eq!(g.pattern_pair_rows("C", "R", "B2"), Some(1));

    // Removals: the actually-removed labels lose their contributions.
    g.begin_write().unwrap();
    g.remove_labels(a, &["A2".into()]);
    g.remove_labels(b, &["B2".into()]);
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("A2", "R", "B"), None);
    assert_eq!(g.pattern_pair_rows("A", "R", "B2"), None);
    assert_eq!(g.pattern_pair_rows("C", "R", "B2"), None);
    assert_eq!(g.pattern_pair_rows("A", "R", "B"), Some(1));
    assert_eq!(g.pattern_pair_rows("C", "R", "B"), Some(1));
}

/// LABS-370: the wholesale adjacency rebuild recalibrates the pair
/// counts from the live state (the high-water-free exact map, the
/// histogram's rebuild precedent).
#[test]
fn pair_counts_recalibrate_on_rebuild() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    g.rebuild_adj();
    assert_eq!(g.pattern_pair_rows("A", "R", "B"), Some(1));
}

/// LABS-370 round 3 (F-1-2): the incident pair walk must see rels
/// created earlier in the SAME batch — a ts-probe at `commit_ts` misses
/// them (their xmin > commit_ts inside the write window), so a label
/// added after an in-batch rel lost the rel's pair entries. The live
/// version (xmax == 0) sees them; the pair count stays exact with no
/// commit between the create and the label mutation.
#[test]
fn pair_counts_see_in_batch_rels_on_label_mutation() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    // Same batch, no commit between: the label lands after the rel.
    g.add_labels(a, &["A2".into()]);
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("A2", "R", "B"), Some(1));
    // The far side too: the end node's added label inherits the
    // in-batch rel.
    g.begin_write().unwrap();
    g.create_rel(a, b, "R2".into(), BTreeMap::new()).unwrap();
    g.add_labels(b, &["B2".into()]);
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("A", "R2", "B2"), Some(1));
    assert_eq!(g.pattern_pair_rows("A2", "R2", "B2"), Some(1));
}

/// The WAL proptest's persisted failure (replay_is_deterministic): a rel
/// id deleted and re-created with different endpoints keeps its old hop
/// in the old end's in-adjacency (older snapshots still traverse it),
/// but the rel's LIVE version ends elsewhere — the incident walk
/// ([`Graph::incident_rels`]) used to debug-assert the walked side and
/// mis-price the stale hop as a live incidence (in release builds,
/// silently). The live version's walked-side end decides: the stale hop
/// is history and is skipped, exactly like a hop with no live version.
#[test]
fn pair_counts_skip_a_reused_rel_ids_stale_hop() {
    let g = Graph::default();
    g.apply_redo(
        vec![
            RedoOp::create_node(1, vec!["A".to_string()], BTreeMap::new()),
            RedoOp::create_node(2, vec!["B".to_string()], BTreeMap::new()),
            RedoOp::create_node(3, vec!["C".to_string()], BTreeMap::new()),
        ],
        1,
    )
    .unwrap();
    // Rel 6 lives (1→2), dies, and its id is re-created (1→3): node 2's
    // in-adjacency keeps the first hop.
    g.apply_redo(
        vec![RedoOp::create_rel(
            6,
            1,
            2,
            "R".to_string(),
            BTreeMap::new(),
        )],
        2,
    )
    .unwrap();
    g.apply_redo(vec![RedoOp::delete_rel(6)], 3).unwrap();
    g.apply_redo(
        vec![
            RedoOp::create_rel(6, 1, 3, "R".to_string(), BTreeMap::new()),
            RedoOp::add_labels(2, vec!["B2".to_string()]),
        ],
        4,
    )
    .unwrap();
    // The re-created rel's live pair entry is (A, R, C) — and node 2's
    // label mutation must not charge the stale hop: no (A, R, B2), no
    // (B2, R, A), the diagonal only if a live rel actually rode node 2.
    assert_eq!(g.pattern_pair_rows("A", "R", "C"), Some(1));
    assert_eq!(g.pattern_pair_rows("A", "R", "B"), None);
    assert_eq!(g.pattern_pair_rows("A", "R", "B2"), None);
    assert_eq!(g.pattern_pair_rows("B2", "R", "A"), None);
    assert_eq!(g.pattern_pair_rows("B2", "R", "B2"), None);
}

/// The stale-hop rule's second axis (stellar boundaries round 1): a rel
/// id deleted and re-created with the SAME endpoints but a DIFFERENT
/// rel type keeps its old hop in the old type's adjacency chunk. The
/// endpoint guard alone lets that hop through — the live-identity match
/// must hold on the type axis too, or the stale hop prices pair-count
/// entries under a rel type the live rel no longer has.
#[test]
fn pair_counts_skip_a_reused_rel_ids_retyped_hop() {
    let g = Graph::default();
    g.apply_redo(
        vec![
            RedoOp::create_node(1, vec!["A".to_string()], BTreeMap::new()),
            RedoOp::create_node(2, vec!["B".to_string()], BTreeMap::new()),
        ],
        1,
    )
    .unwrap();
    // Rel 7 lives (1→2, "R"), dies, and its id comes back (1→2, "S"):
    // node 2's in-adjacency keeps the "R"-chunk hop.
    g.apply_redo(
        vec![RedoOp::create_rel(
            7,
            1,
            2,
            "R".to_string(),
            BTreeMap::new(),
        )],
        2,
    )
    .unwrap();
    g.apply_redo(vec![RedoOp::delete_rel(7)], 3).unwrap();
    g.apply_redo(
        vec![
            RedoOp::create_rel(7, 1, 2, "S".to_string(), BTreeMap::new()),
            RedoOp::add_labels(2, vec!["B2".to_string()]),
        ],
        4,
    )
    .unwrap();
    // The live rel's pair entry is (A, S, B) — the label mutation must
    // not charge the stale "R" hop: no (A, R, B2), no (B2, R, A).
    assert_eq!(g.pattern_pair_rows("A", "S", "B"), Some(1));
    assert_eq!(g.pattern_pair_rows("A", "R", "B"), None);
    assert_eq!(g.pattern_pair_rows("A", "R", "B2"), None);
    assert_eq!(g.pattern_pair_rows("B2", "R", "A"), None);
    assert_eq!(g.pattern_pair_rows("A", "S", "B2"), Some(1));
}

/// LABS-425 (round 5's self-loop fix, pinned on the round-6 pass split):
/// a self-loop rides both adjacency sides of its own node, and the
/// correct far set for the mutated node itself is mutation-aware on both
/// sides — the out side charges `mutated × (live ∪ mutated)`, the in side
/// the complement, so the (mutated, type, mutated) diagonal is charged
/// exactly once, by the out side, and drained exactly once on remove.
/// The far rel to the B node carries label A too, so the (A2, R, A) key
/// is charged by BOTH passes — the far-node pass and the self-loop pass
/// collide on one key, pinning that the split passes sum like the
/// interleaved walk did.
#[test]
fn pair_counts_charge_the_self_loop_diagonal_exactly_once() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    // One self-loop and one far rel whose end carries A — the cross-pass
    // key collision: both realize (A2, R, A) on the add below.
    g.create_rel(a, a, "R".into(), BTreeMap::new()).unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("A", "R", "A"), Some(2));

    // Add: the diagonal gains exactly one charge (not two — the rel rides
    // both adjacency sides), and the cross-pass key lands at 2.
    g.begin_write().unwrap();
    g.add_labels(a, &["A2".into()]);
    g.commit_write();
    // Far pass: mutated [A2] × far [A] -> (A2, R, A).
    // Self-loop out pass: [A2] × (live ∪ mutated) = [A, A2] ->
    // (A2, R, A) again (the collision) and the diagonal (A2, R, A2).
    // Self-loop in pass: (live ∪ mutated) \ mutated = [A] × [A2] ->
    // (A, R, A2).
    assert_eq!(g.pattern_pair_rows("A", "R", "A"), Some(2));
    assert_eq!(g.pattern_pair_rows("A2", "R", "A"), Some(2));
    assert_eq!(g.pattern_pair_rows("A2", "R", "A2"), Some(1));
    assert_eq!(g.pattern_pair_rows("A", "R", "A2"), Some(1));

    // Remove: the diagonal drains exactly once (the entry leaves the
    // map), the collision key drains through both passes' charges, and
    // the untouched (A, R, A) survives.
    g.begin_write().unwrap();
    g.remove_labels(a, &["A2".into()]);
    g.commit_write();
    assert_eq!(g.pattern_pair_rows("A", "R", "A"), Some(2));
    assert_eq!(g.pattern_pair_rows("A2", "R", "A"), None);
    assert_eq!(g.pattern_pair_rows("A2", "R", "A2"), None);
    assert_eq!(g.pattern_pair_rows("A", "R", "A2"), None);
}

/// LABS-370 round 3 (F-1-6): the AddLabels admission estimate simulates
/// the batch's in-batch rels — apply walks them (the incident walk and
/// the rebucket see the rel the batch created), so the estimate must
/// price their entries too or drift from the applied `used_bytes` on a
/// mixed [CreateRel, AddLabels] batch.
#[test]
fn stats_entry_growth_estimates_mixed_batches() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    g.commit_write();
    let ops = vec![
        RedoOp::create_rel(1, a, b, "R".to_string(), BTreeMap::new()),
        RedoOp::add_labels(a, vec!["A2".to_string()]),
    ];
    // CreateRel: the (A,R) histogram entry + the (A,R,B) pair entry.
    // AddLabels on the start node, same batch: the A2 label count, the
    // (A2,R) histogram entry (the in-batch rel's out-degree), and the
    // (A2,R,B) pair entry (the in-batch rel's incident contribution).
    let expected = (2 * Graph::HISTOGRAM_ENTRY_BYTES
        + 2 * Graph::PAIR_COUNT_ENTRY_BYTES
        + Graph::LABEL_COUNT_ENTRY_BYTES) as i64;
    assert_eq!(g.estimate_stats_entry_growth(&ops), expected);
}

/// LABS-425 (stellar round 1): the estimate's incident walk prices BOTH
/// sides of a committed self-loop — the apply path charges
/// `mutated × (live ∪ mutated)` on the out side and the complement on
/// the in side, so the estimate must price the same three pair entries
/// or an over-cap self-loop AddLabels batch slips past admission (the
/// old `seen_far` early-out keyed (type, far) and skipped the in side).
#[test]
fn stats_entry_growth_prices_a_committed_self_loop_on_both_sides() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, a, "R".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    let ops = vec![RedoOp::add_labels(a, vec!["A2".to_string()])];
    // The A2 label count, the (A2, R) histogram entry (the self-loop's
    // out-degree bucket), and the three pair entries: out side
    // A2 × (A ∪ A2) -> (A2,R,A) and the diagonal (A2,R,A2); in side
    // (A ∪ A2) \ A2 -> (A,R,A2).
    let expected = (Graph::HISTOGRAM_ENTRY_BYTES
        + 3 * Graph::PAIR_COUNT_ENTRY_BYTES
        + Graph::LABEL_COUNT_ENTRY_BYTES) as i64;
    assert_eq!(g.estimate_stats_entry_growth(&ops), expected);
}

/// LABS-425 (stellar round 1): a reciprocal same-type pair — a-[:R]->b
/// AND b-[:R]->a — is the other two-sided incidence the old `seen_far`
/// early-out under-priced: the (t, far) key matched across the two
/// sides, so the in-side keys ((far-label, t, added)) were never priced.
#[test]
fn stats_entry_growth_prices_a_reciprocal_pair_on_both_sides() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.create_rel(b, a, "R".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    let ops = vec![RedoOp::add_labels(a, vec!["A2".to_string()])];
    // The A2 label count, the (A2, R) histogram entry, and the two pair
    // entries: the out side's (A2,R,B) and the in side's (B,R,A2).
    let expected = (Graph::HISTOGRAM_ENTRY_BYTES
        + 2 * Graph::PAIR_COUNT_ENTRY_BYTES
        + Graph::LABEL_COUNT_ENTRY_BYTES) as i64;
    assert_eq!(g.estimate_stats_entry_growth(&ops), expected);
}

/// LABS-425 (stellar round 1): an IN-BATCH self-loop is two-sided too —
/// the CreateRel lands before the AddLabels applies, so the apply
/// incident walk sees the rel on both adjacency sides and charges both
/// crosses; the estimate's batch-rel walk prices both crosses for it.
#[test]
fn stats_entry_growth_prices_an_in_batch_self_loop_on_both_sides() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    g.commit_write();
    let ops = vec![
        RedoOp::create_rel(1, a, a, "R".to_string(), BTreeMap::new()),
        RedoOp::add_labels(a, vec!["A2".to_string()]),
    ];
    // CreateRel: the (A, R) histogram entry + the (A,R,A) pair entry.
    // AddLabels on the self-looped node: the A2 label count, the
    // (A2, R) histogram entry, and the three pair entries both sides
    // realize (out A2 × (A ∪ A2), in (A ∪ A2) \ A2 × A2).
    let expected = (2 * Graph::HISTOGRAM_ENTRY_BYTES
        + 4 * Graph::PAIR_COUNT_ENTRY_BYTES
        + Graph::LABEL_COUNT_ENTRY_BYTES) as i64;
    assert_eq!(g.estimate_stats_entry_growth(&ops), expected);
}
