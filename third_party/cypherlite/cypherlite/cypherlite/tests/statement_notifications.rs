//! Statement-scoped query notifications (ADR-0363 decisions 3, 5 and 6):
//! unknown labels, relationship types and property keys, deprecated
//! functions and procedures, and the scope-less `CALL { … }` subquery.
//! Every detector runs only where an `EXPLAIN`/`PROFILE` tree is built, so
//! the same statement without a prefix returns an empty list.

mod common;

use common::{connection, has_notification, run, two_label_store};
use cypherlite::Database;
use cypherlite_model::wire::NotificationKind;

/// The notification kinds of one result, in list order.
fn kinds(result: &cypherlite::QueryResult) -> Vec<NotificationKind> {
    result
        .notifications()
        .iter()
        .map(cypherlite::Notification::kind)
        .collect()
}

/// Acceptance 1/ADR-0363 decision 5: an absent label warns; the first
/// write that creates it clears the warning immediately.
#[test]
fn unknown_label_warning_clears_after_the_label_exists() {
    let mut conn = connection();
    let explained = conn.query("EXPLAIN MATCH (n:Nope) RETURN n").unwrap();
    assert_eq!(kinds(&explained), [NotificationKind::UnknownLabelWarning]);
    let notification = &explained.notifications()[0];
    assert_eq!(
        notification.code(),
        "Neo.ClientNotification.Statement.UnknownLabelWarning"
    );
    assert_eq!(notification.gql_status(), "01N50");
    assert_eq!(notification.severity().as_str(), "WARNING");
    assert_eq!(notification.category().as_str(), "UNRECOGNIZED");
    assert_eq!(notification.title(), "Unknown label");
    assert!(
        notification.description().contains("Nope"),
        "the description names the unknown label: {}",
        notification.description()
    );
    assert!(notification.remedy().contains("first write"));
    assert_eq!(
        notification.operator_id(),
        None,
        "the unknown-name kinds are statement-scoped"
    );

    run(&mut conn, "CREATE (:Nope)");
    let explained = conn.query("EXPLAIN MATCH (n:Nope) RETURN n").unwrap();
    assert!(
        explained.notifications().is_empty(),
        "the label is known after its first write: {:?}",
        explained.notifications()
    );
}

/// Acceptance 2: the relationship-type twin of the label pair.
#[test]
fn unknown_relationship_type_warning_clears_after_the_type_exists() {
    let mut conn = connection();
    let explained = conn
        .query("EXPLAIN MATCH ()-[r:NOPE]->() RETURN r")
        .unwrap();
    assert_eq!(
        kinds(&explained),
        [NotificationKind::UnknownRelationshipTypeWarning]
    );
    let notification = &explained.notifications()[0];
    assert_eq!(
        notification.code(),
        "Neo.ClientNotification.Statement.UnknownRelationshipTypeWarning"
    );
    assert_eq!(notification.gql_status(), "01N51");
    assert!(notification.description().contains("NOPE"));

    run(&mut conn, "CREATE (:A)-[:NOPE]->(:B)");
    let explained = conn
        .query("EXPLAIN MATCH ()-[r:NOPE]->() RETURN r")
        .unwrap();
    assert!(
        explained.notifications().is_empty(),
        "the type is known after its first write: {:?}",
        explained.notifications()
    );
}

/// Acceptance 2: a `WHERE` equality or label predicate the planner folds
/// into the compact pattern payload is still read evidence — every detector
/// must use the IR's one post-rewrite payload view. Each warning clears
/// after a write creates the name.
#[test]
fn folded_where_predicates_reach_the_unknown_name_detectors() {
    let mut conn = connection();

    // `fold_where` moves `n.nope = 1` into the compact node's property map.
    let explained = conn
        .query("EXPLAIN MATCH (n) WHERE n.nope = 1 RETURN n")
        .unwrap();
    assert_eq!(
        kinds(&explained),
        [NotificationKind::UnknownPropertyKeyWarning]
    );
    run(&mut conn, "CREATE (:K {nope: 1})");
    let explained = conn
        .query("EXPLAIN MATCH (n) WHERE n.nope = 1 RETURN n")
        .unwrap();
    assert!(
        !has_notification(&explained, NotificationKind::UnknownPropertyKeyWarning),
        "the folded key is known after its first write: {:?}",
        explained.notifications()
    );

    // The label predicate folds into the same compact view.
    let explained = conn
        .query("EXPLAIN MATCH (n) WHERE n:Nope RETURN n")
        .unwrap();
    assert_eq!(kinds(&explained), [NotificationKind::UnknownLabelWarning]);
    run(&mut conn, "CREATE (:Nope)");
    let explained = conn
        .query("EXPLAIN MATCH (n) WHERE n:Nope RETURN n")
        .unwrap();
    assert!(
        explained.notifications().is_empty(),
        "the folded label is known after its first write: {:?}",
        explained.notifications()
    );

    // The relationship predicate folds into the compact rel payload.
    run(&mut conn, "CREATE (:A)-[:R]->(:B)");
    let statement = "EXPLAIN MATCH ()-[r:R]->() WHERE r.rel_key = 1 RETURN r";
    let explained = conn.query(statement).unwrap();
    assert_eq!(
        kinds(&explained),
        [NotificationKind::UnknownPropertyKeyWarning]
    );
    run(&mut conn, "MATCH ()-[r:R]->() SET r.rel_key = 1");
    let explained = conn.query(statement).unwrap();
    assert!(
        !has_notification(&explained, NotificationKind::UnknownPropertyKeyWarning),
        "the folded relationship key is known after its first write: {:?}",
        explained.notifications()
    );

    // The folded equality still feeds MissingIndex when no range index
    // serves the pair.
    run(&mut conn, "CREATE (:Person {name: 'ada'})");
    let explained = conn
        .query("EXPLAIN MATCH (n:Person) WHERE n.name = 'ada' RETURN n")
        .unwrap();
    assert_eq!(kinds(&explained), [NotificationKind::MissingIndex]);
}

/// ADR-0363 decision 13: a `CREATE`/`MERGE` target pattern and a
/// `SET`/`REMOVE` property target name the write's own output, so they never
/// raise 01N50/01N51/01N52; a read in the same statement still does.
#[test]
fn write_targets_do_not_warn_but_a_read_in_the_same_statement_does() {
    let mut conn = connection();

    let explained = conn
        .query("EXPLAIN CREATE (:Person {name: 'Ada'})")
        .unwrap();
    assert!(
        explained.notifications().is_empty(),
        "a CREATE target names its own output: {:?}",
        explained.notifications()
    );
    let explained = conn
        .query("EXPLAIN CREATE (:Person)-[:KNOWS]->(:Person)")
        .unwrap();
    assert!(
        explained.notifications().is_empty(),
        "a CREATE label, type and pattern property never warn: {:?}",
        explained.notifications()
    );
    let explained = conn
        .query("EXPLAIN MERGE (p:Persn {name: 'Ada'}) RETURN p")
        .unwrap();
    assert!(
        explained.notifications().is_empty(),
        "a MERGE target names its own output: {:?}",
        explained.notifications()
    );

    // The read half still warns; the SET target key is silent.
    let explained = conn
        .query("EXPLAIN MATCH (n:Persn) SET n.name = 'Ada'")
        .unwrap();
    assert_eq!(kinds(&explained), [NotificationKind::UnknownLabelWarning]);

    // A read on the right-hand side is read evidence even though the
    // target key is not; a REMOVE target stays silent.
    run(&mut conn, "CREATE (:K)");
    let explained = conn
        .query("EXPLAIN MATCH (n:K) SET n.name = n.other")
        .unwrap();
    assert_eq!(
        kinds(&explained),
        [NotificationKind::UnknownPropertyKeyWarning],
        "only the RHS read warns: {:?}",
        explained.notifications()
    );
    assert!(
        explained.notifications()[0].description().contains("other"),
        "the warning names the read key: {}",
        explained.notifications()[0].description()
    );
    let explained = conn.query("EXPLAIN MATCH (n:K) REMOVE n.nope").unwrap();
    assert!(
        explained.notifications().is_empty(),
        "a REMOVE target names nothing read: {:?}",
        explained.notifications()
    );
}

/// Acceptance 2: a property-key access warns while no stored key carries
/// the name; the first write that sets it clears the warning.
#[test]
fn unknown_property_key_warning_clears_after_the_key_is_written() {
    let mut conn = connection();
    let explained = conn.query("EXPLAIN MATCH (n) RETURN n.nope").unwrap();
    assert_eq!(
        kinds(&explained),
        [NotificationKind::UnknownPropertyKeyWarning]
    );
    let notification = &explained.notifications()[0];
    assert_eq!(
        notification.code(),
        "Neo.ClientNotification.Statement.UnknownPropertyKeyWarning"
    );
    assert_eq!(notification.gql_status(), "01N52");
    assert!(notification.description().contains("nope"));

    run(&mut conn, "CREATE (:K {nope: 1})");
    let explained = conn.query("EXPLAIN MATCH (n) RETURN n.nope").unwrap();
    assert!(
        !has_notification(&explained, NotificationKind::UnknownPropertyKeyWarning),
        "the key is known after its first write: {:?}",
        explained.notifications()
    );
}

/// Acceptance 3: keys written by CREATE, SET and MERGE never false-positive,
/// including across a checkpoint + WAL reopen. The checkpoint holds the
/// CREATE key; SET/MERGE keys ride the WAL tail (`leak_without_checkpoint`).
#[test]
fn property_keys_from_create_set_merge_never_warn_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let uri = path.to_str().unwrap();
    {
        let db = Database::open(uri).unwrap();
        let mut conn = db.connect().unwrap();
        run(&mut conn, "CREATE (:Created {from_create: 1})");
        db.checkpoint().unwrap();
        run(&mut conn, "MATCH (n:Created) SET n.from_set = 2");
        run(&mut conn, "MERGE (:Merged {from_merge: 3})");
        drop(conn);
        db.leak_without_checkpoint();
    }

    let db = Database::open(uri).unwrap();
    let mut conn = db.connect().unwrap();
    for key in ["from_create", "from_set", "from_merge"] {
        let explained = conn
            .query(&format!("EXPLAIN MATCH (n) RETURN n.{key}"))
            .unwrap();
        assert!(
            !has_notification(&explained, NotificationKind::UnknownPropertyKeyWarning),
            "`{key}` is stored after the reopen: {:?}",
            explained.notifications()
        );
    }
    // The same reopen proves the catalog's label set survived both the
    // checkpoint and the WAL tail.
    let explained = conn.query("EXPLAIN MATCH (n:Created) RETURN n").unwrap();
    assert!(
        explained.notifications().is_empty(),
        "the label is known after the reopen: {:?}",
        explained.notifications()
    );
}

/// Acceptance 4: `id()` is deprecated, read from the executor's function
/// catalog (`deprecated`/`deprecated_by`, the `SHOW FUNCTIONS` facts), and
/// the remedy names the catalog's `elementId` replacement.
#[test]
fn deprecated_function_reads_the_function_catalog() {
    let mut conn = connection();
    let explained = conn.query("EXPLAIN MATCH (n) RETURN id(n)").unwrap();
    assert_eq!(kinds(&explained), [NotificationKind::DeprecatedFunction]);
    let notification = &explained.notifications()[0];
    assert_eq!(
        notification.code(),
        "Neo.ClientNotification.Statement.FeatureDeprecationWarning"
    );
    assert_eq!(notification.gql_status(), "01N01");
    assert_eq!(notification.category().as_str(), "DEPRECATION");
    assert_eq!(notification.title(), "Deprecated function");
    assert!(
        notification.description().contains("id()"),
        "the description names the deprecated function: {}",
        notification.description()
    );

    let replacement = cypherlite_executor::builtin_function_deprecation("id")
        .expect("the catalog marks id deprecated");
    assert!(
        replacement.display().contains("elementId"),
        "the catalog's display names elementId: {}",
        replacement.display()
    );
    assert_eq!(
        notification.remedy(),
        "Use `elementId` instead.",
        "the instance remedy renders the function identifier, never the catalog's prose sentence"
    );
    assert!(
        !notification.remedy().contains("consider using"),
        "the catalog's prose never reaches the remedy: {}",
        notification.remedy()
    );

    let replacement = conn.query("EXPLAIN MATCH (n) RETURN elementId(n)").unwrap();
    assert!(
        replacement.notifications().is_empty(),
        "elementId is not deprecated: {:?}",
        replacement.notifications()
    );
}

/// Acceptance 5: the registry's deprecated `db.index.vector.queryNodes`
/// signature produces the procedure deprecation naming `SEARCH`.
#[test]
fn deprecated_procedure_reads_the_procedure_registry() {
    let mut conn = connection();
    let statement = "CALL db.index.vector.queryNodes('vidx', 5, [1.0, 0.0]) YIELD node RETURN node";
    let explained = conn.query(&format!("EXPLAIN {statement}")).unwrap();
    assert_eq!(kinds(&explained), [NotificationKind::DeprecatedProcedure]);
    let notification = &explained.notifications()[0];
    assert_eq!(
        notification.code(),
        "Neo.ClientNotification.Statement.FeatureDeprecationWarning"
    );
    assert_eq!(notification.gql_status(), "01N01");
    assert_eq!(notification.title(), "Deprecated procedure");
    assert!(notification
        .description()
        .contains("db.index.vector.queryNodes"));

    let signature = cypherlite_executor::builtin_procedure_signature("db.index.vector.queryNodes")
        .expect("the registry lists the procedure");
    let replacement = signature
        .deprecated_by
        .expect("a deprecated registry entry names its replacement");
    assert_eq!(replacement.display(), "SEARCH");
    assert_eq!(
        notification.remedy(),
        "Use the `SEARCH` clause instead.",
        "SEARCH is a clause, so the remedy names it as one"
    );
}

/// Acceptance 6: the scope-less `CALL { … }` fires `01N00`; both scoped
/// forms (`CALL (n)`, `CALL ()`) stay silent.
#[test]
fn call_subquery_without_scope_fires_and_scoped_forms_do_not() {
    let mut conn = connection();
    let scope_less = "EXPLAIN MATCH (n) CALL { WITH n RETURN 1 AS x } RETURN x";
    let explained = conn.query(scope_less).unwrap();
    assert_eq!(
        kinds(&explained),
        [NotificationKind::CallSubqueryWithoutScope]
    );
    let notification = &explained.notifications()[0];
    assert_eq!(
        notification.code(),
        "Neo.ClientNotification.Statement.FeatureDeprecationWarning"
    );
    assert_eq!(notification.gql_status(), "01N00");
    assert!(notification.remedy().contains("CALL (n)"));

    for scoped in [
        "EXPLAIN MATCH (n) CALL (n) { RETURN 1 AS x } RETURN x",
        "EXPLAIN MATCH (n) CALL () { RETURN 1 AS x } RETURN x",
    ] {
        let explained = conn.query(scoped).unwrap();
        assert!(
            explained.notifications().is_empty(),
            "a scoped CALL subquery does not fire: {scoped}: {:?}",
            explained.notifications()
        );
    }
}

/// Decision 6: the tree's operator-scoped entries come first, then the
/// statement-scoped ones (`UNRECOGNIZED` before `DEPRECATION`).
#[test]
fn operator_notifications_precede_statement_notifications() {
    let mut conn = two_label_store();
    let explained = conn
        .query("EXPLAIN MATCH (a:Person), (b:City) RETURN a.missing")
        .unwrap();
    assert_eq!(
        kinds(&explained),
        [
            NotificationKind::CartesianProduct,
            NotificationKind::UnknownPropertyKeyWarning,
        ]
    );
    assert!(explained.notifications()[0].operator_id().is_some());
    assert_eq!(explained.notifications()[1].operator_id(), None);
}

/// PROFILE derives the same statement-scoped list as EXPLAIN.
#[test]
fn profile_carries_the_statement_scoped_notifications() {
    let mut conn = connection();
    let profiled = conn.query("PROFILE MATCH (n:Nope) RETURN n").unwrap();
    assert_eq!(kinds(&profiled), [NotificationKind::UnknownLabelWarning]);
}

/// Decision 5: the notification facts read the view the transaction's
/// statement runs against — the committed base plus the transaction's
/// scratch writes. A name the scratch just created is known to EXPLAIN and
/// PROFILE before COMMIT.
#[test]
fn transaction_scratch_names_are_known_to_explain_and_profile() {
    let mut conn = connection();
    conn.begin().unwrap();

    // The exact sequence: the transaction's CREATE is visible to the
    // statement that EXPLAINs it, before COMMIT.
    run(&mut conn, "CREATE (:Person)");
    for prefixed in [
        "EXPLAIN MATCH (p:Person) RETURN p",
        "PROFILE MATCH (p:Person) RETURN p",
    ] {
        let result = conn
            .query(prefixed)
            .unwrap_or_else(|error| panic!("{prefixed}: {error}"));
        assert!(
            !has_notification(&result, NotificationKind::UnknownLabelWarning),
            "the scratch-created label is known under {prefixed}: {:?}",
            result.notifications()
        );
    }

    // The relationship type and property keys a scratch write names are
    // visible to the same view too.
    run(
        &mut conn,
        "CREATE (:Person {name: 'ada'})-[:KNOWS]->(:Person {name: 'bob'})",
    );

    let statement = "MATCH (p:Person)-[:KNOWS]->(q) WHERE p.name = 'ada' RETURN q";
    for prefixed in [
        format!("EXPLAIN {statement}"),
        format!("PROFILE {statement}"),
    ] {
        let result = conn
            .query(&prefixed)
            .unwrap_or_else(|error| panic!("{prefixed}: {error}"));
        for kind in [
            NotificationKind::UnknownLabelWarning,
            NotificationKind::UnknownRelationshipTypeWarning,
            NotificationKind::UnknownPropertyKeyWarning,
        ] {
            assert!(
                !has_notification(&result, kind),
                "scratch writes are known to {prefixed}: expected no {kind:?}: {:?}",
                result.notifications()
            );
        }
    }
}

/// Decision 5: a name absent from both the base graph and the transaction
/// scratch still warns, in both the EXPLAIN and PROFILE forms.
#[test]
fn transaction_scratch_absent_names_still_warn() {
    let mut conn = connection();
    conn.begin().unwrap();
    run(&mut conn, "CREATE (:Person)");

    let statement = "MATCH (a:Absent)-[:NOPE]->(b) WHERE a.missing = 1 RETURN b";
    for prefixed in [
        format!("EXPLAIN {statement}"),
        format!("PROFILE {statement}"),
    ] {
        let result = conn
            .query(&prefixed)
            .unwrap_or_else(|error| panic!("{prefixed}: {error}"));
        for expected in [
            NotificationKind::UnknownLabelWarning,
            NotificationKind::UnknownRelationshipTypeWarning,
            NotificationKind::UnknownPropertyKeyWarning,
        ] {
            assert!(
                has_notification(&result, expected),
                "a name absent from base and scratch warns under {prefixed}: \
                 expected {expected:?}: {:?}",
                result.notifications()
            );
        }
    }
}

/// Decision 5: the scratch's catalog facts cover the DDL names too, not only
/// data writes. An index created inside the transaction names its property
/// key (and its label) before COMMIT publishes either, so an `EXPLAIN`/
/// `PROFILE` of a statement reading that key raises no 01N52; a key neither
/// the base nor the scratch knows still warns.
#[test]
fn transaction_scratch_index_ddl_names_are_known_to_explain() {
    let mut conn = connection();
    conn.begin().unwrap();
    run(
        &mut conn,
        "CREATE INDEX ddl_key FOR (n:Indexed) ON (n.ddl_key)",
    );

    for prefixed in [
        "EXPLAIN MATCH (n) RETURN n.ddl_key",
        "PROFILE MATCH (n) RETURN n.ddl_key",
    ] {
        let result = conn
            .query(prefixed)
            .unwrap_or_else(|error| panic!("{prefixed}: {error}"));
        assert!(
            !has_notification(&result, NotificationKind::UnknownPropertyKeyWarning),
            "the index DDL names `ddl_key` under {prefixed}: {:?}",
            result.notifications()
        );
    }

    // The index's label is a scratch catalog name as well.
    let explained = conn.query("EXPLAIN MATCH (n:Indexed) RETURN n").unwrap();
    assert!(
        !has_notification(&explained, NotificationKind::UnknownLabelWarning),
        "the index DDL names `Indexed` under EXPLAIN: {:?}",
        explained.notifications()
    );

    // Neither the base graph nor the scratch names this key: it still warns.
    let explained = conn
        .query("EXPLAIN MATCH (n) RETURN n.not_anywhere")
        .unwrap();
    assert!(
        has_notification(&explained, NotificationKind::UnknownPropertyKeyWarning),
        "an absent key still warns: {:?}",
        explained.notifications()
    );
}

/// Decision 5: a DDL name is a committed catalog fact, not only a scratch
/// one. The label an index names is known inside the transaction and, because
/// COMMIT publishes the same fact to the committed catalog, after it too —
/// the 01N50 must not flip across COMMIT. The relationship-type twin covers
/// a relationship index and a relationship constraint; a name no DDL or data
/// write mentions still warns after the commit.
#[test]
fn transaction_ddl_schema_names_survive_commit() {
    let mut conn = connection();
    conn.begin().unwrap();
    run(
        &mut conn,
        "CREATE INDEX ddl_label FOR (n:Indexed) ON (n.ddl_key)",
    );
    run(
        &mut conn,
        "CREATE INDEX ddl_rel FOR ()-[r:IndexedRel]-() ON (r.ddl_key)",
    );
    run(
        &mut conn,
        "CREATE CONSTRAINT ddl_node FOR (n:IndexedConstraint) REQUIRE n.id IS UNIQUE",
    );
    run(
        &mut conn,
        "CREATE CONSTRAINT ddl_type FOR ()-[r:IndexedType]-() REQUIRE r.id IS UNIQUE",
    );

    // Inside the transaction: the scratch knows every DDL name.
    for statement in [
        "EXPLAIN MATCH (n:Indexed) RETURN n",
        "EXPLAIN MATCH ()-[r:IndexedRel]->() RETURN r",
        "EXPLAIN MATCH (n:IndexedConstraint) RETURN n",
        "EXPLAIN MATCH ()-[r:IndexedType]->() RETURN r",
    ] {
        let explained = conn
            .query(statement)
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
        assert!(
            explained.notifications().is_empty(),
            "the scratch DDL names are known under {statement}: {:?}",
            explained.notifications()
        );
    }

    conn.commit().unwrap();

    // After COMMIT the same statements read the committed catalog: no flip.
    for statement in [
        "EXPLAIN MATCH (n:Indexed) RETURN n",
        "EXPLAIN MATCH ()-[r:IndexedRel]->() RETURN r",
        "EXPLAIN MATCH (n:IndexedConstraint) RETURN n",
        "EXPLAIN MATCH ()-[r:IndexedType]->() RETURN r",
    ] {
        let explained = conn
            .query(statement)
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
        assert!(
            explained.notifications().is_empty(),
            "the committed catalog keeps the DDL names under {statement}: {:?}",
            explained.notifications()
        );
    }

    // A name neither DDL nor data ever mentioned still warns.
    for (statement, kind) in [
        (
            "EXPLAIN MATCH (n:NeverMentioned) RETURN n",
            NotificationKind::UnknownLabelWarning,
        ),
        (
            "EXPLAIN MATCH ()-[r:NEVER_MENTIONED]->() RETURN r",
            NotificationKind::UnknownRelationshipTypeWarning,
        ),
    ] {
        let explained = conn.query(statement).unwrap();
        assert!(
            has_notification(&explained, kind),
            "a never-mentioned name still warns under {statement}: {:?}",
            explained.notifications()
        );
    }
}

/// Decision 5 durability: the DDL names are committed catalog facts, so a
/// store that took a checkpoint and reopened keeps them — the checkpoint
/// rows carry only data labels, so the theater image's typed index details
/// are what restores a relationship index's type.
#[test]
fn transaction_ddl_schema_names_survive_checkpoint_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("ddl-names").to_str().unwrap().to_string();
    {
        let db = Database::open(&uri).unwrap();
        let mut conn = db.connect().unwrap();
        conn.begin().unwrap();
        run(
            &mut conn,
            "CREATE INDEX ddl_label FOR (n:Indexed) ON (n.ddl_key)",
        );
        run(
            &mut conn,
            "CREATE INDEX ddl_rel FOR ()-[r:IndexedRel]-() ON (r.ddl_key)",
        );
        run(
            &mut conn,
            "CREATE CONSTRAINT ddl_node FOR (n:IndexedConstraint) REQUIRE n.id IS UNIQUE",
        );
        run(
            &mut conn,
            "CREATE CONSTRAINT ddl_type FOR ()-[r:IndexedType]-() REQUIRE r.id IS UNIQUE",
        );
        conn.commit().unwrap();
        db.checkpoint().unwrap();
    }

    let db = Database::open(&uri).unwrap();
    let mut conn = db.connect().unwrap();
    for statement in [
        "EXPLAIN MATCH (n:Indexed) RETURN n",
        "EXPLAIN MATCH ()-[r:IndexedRel]->() RETURN r",
        "EXPLAIN MATCH (n:IndexedConstraint) RETURN n",
        "EXPLAIN MATCH ()-[r:IndexedType]->() RETURN r",
    ] {
        let explained = conn
            .query(statement)
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
        assert!(
            explained.notifications().is_empty(),
            "the reopened catalog keeps the DDL names under {statement}: {:?}",
            explained.notifications()
        );
    }
}

/// Acceptance 7/decision 3: the same statements without an EXPLAIN or
/// PROFILE prefix build no tree and return an empty list.
#[test]
fn plain_runs_carry_no_notifications() {
    let mut conn = connection();
    // The deprecated procedure executes against a real vector index.
    run(
        &mut conn,
        "CREATE VECTOR INDEX vidx FOR (p:Person) ON p.embed",
    );
    run(&mut conn, "CREATE (:Person {embed: [1.0, 0.0]})");
    let statements = [
        "MATCH (n:Nope) RETURN n",
        "MATCH ()-[r:NOPE]->() RETURN r",
        "MATCH (n) RETURN n.nope",
        "MATCH (n) RETURN id(n)",
        "CALL db.index.vector.queryNodes('vidx', 5, [1.0, 0.0]) YIELD node RETURN node",
        "MATCH (n) CALL { WITH n RETURN 1 AS x } RETURN x",
        "MATCH (n) CALL (n) { RETURN 1 AS x } RETURN x",
        "MATCH (n) CALL () { RETURN 1 AS x } RETURN x",
    ];
    for statement in statements {
        let result = conn
            .query(statement)
            .unwrap_or_else(|error| panic!("{statement}: {error}"));
        assert!(
            result.notifications().is_empty(),
            "a plain run derives no notifications: {statement}: {:?}",
            result.notifications()
        );
    }
}
