use crate::*;
use std::io::Write;

use cypherlite_executor::{Graph, RedoOp};
use std::sync::Arc;

fn write_wal(uri: &std::path::Path, ops: Vec<RedoOp>) {
    let wal_dir = uri.join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::write(
        uri.join(cypherlite_storage::LOCK_MARKER),
        cypherlite_storage::ROOT_ROLE,
    )
    .unwrap();
    std::fs::write(
        wal_dir.join(cypherlite_storage::LOCK_MARKER),
        cypherlite_storage::WAL_ROLE,
    )
    .unwrap();
    let payload = serde_json::to_vec(&WalRecord {
        lsn: 1,
        commit_ts: 1,
        ops,
    })
    .unwrap();
    let mut file = std::fs::File::create(wal_dir.join("00000001.log")).unwrap();
    file.write_all(cypherlite_storage::MAGIC).unwrap();
    file.write_all(&crc32fast::hash(&payload).to_le_bytes())
        .unwrap();
    file.write_all(&(payload.len() as u32).to_le_bytes())
        .unwrap();
    file.write_all(&payload).unwrap();
}

fn node(id: i64) -> RedoOp {
    RedoOp::CreateNode {
        id,
        labels: Arc::new(vec!["High".into()]),
        properties: std::sync::Arc::new(cypherlite_core::PropertyMap::new()),
    }
}

fn rel(id: i64, start: i64, end: i64) -> RedoOp {
    RedoOp::CreateRel {
        id,
        start,
        end,
        rel_type: "R".into(),
        properties: std::sync::Arc::new(cypherlite_core::PropertyMap::new()),
    }
}

fn json_store(graph: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    std::fs::create_dir_all(&uri).unwrap();
    std::fs::write(uri.join("graph.json"), graph).unwrap();
    (tmp, uri)
}

fn json_node_graph(id: i64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "nodes": [{"id": id, "labels": ["High"], "properties": {}}],
        "relationships": [],
        "next_node_id": i64::MAX,
        "next_rel_id": 0
    }))
    .unwrap()
}

fn json_rel_graph(id: i64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "nodes": [{"id": 0, "labels": ["High"], "properties": {}}],
        "relationships": [{"id": id, "type": "R", "start_node": 0, "end_node": 0, "properties": {}}],
        "next_node_id": 1,
        "next_rel_id": i64::MAX
    }))
    .unwrap()
}

#[test]
fn invalid_replicated_ids_leave_existing_wal_and_graph_reopenable() {
    use cypherlite_executor::PullKind;

    for (name, invalid) in [
        ("negative node", node(-1)),
        ("max node", node(i64::MAX)),
        ("negative rel", rel(-1, 0, 1)),
        ("max rel", rel(i64::MAX, 0, 1)),
        ("negative start", rel(1, -1, 1)),
        ("max start", rel(1, i64::MAX, 1)),
        ("negative end", rel(1, 0, -1)),
        ("max end", rel(1, 0, i64::MAX)),
        (
            "negative mutation",
            RedoOp::SetNodeProp {
                id: -1,
                key: "k".into(),
                value: Value::Int(1),
            },
        ),
        ("max delete", RedoOp::DeleteRel { id: i64::MAX }),
        (
            "negative pull local",
            RedoOp::PullIdentityPut {
                source: "upstream".into(),
                kind: PullKind::Node,
                remote_id: "-1".into(),
                local_id: -1,
            },
        ),
        (
            "max pull local",
            RedoOp::PullIdentityPut {
                source: "upstream".into(),
                kind: PullKind::Rel,
                remote_id: i64::MAX.to_string(),
                local_id: i64::MAX,
            },
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let uri = tmp.path().join("db");
        let db = Database::open(uri.to_str().unwrap()).unwrap();
        db.connect().unwrap().query("CREATE (:Prior)").unwrap();
        let wal_file = uri.join("wal/00000001.log");
        let before = std::fs::read(&wal_file).unwrap();
        let (lsn, bytes, commits) = {
            let wal = db.inner.wal.lock().unwrap();
            (wal.lsn, wal.bytes, wal.commits)
        };

        // Even the earlier valid create in this same record must not be logged.
        let err = db
            .apply_replicated(&WalRecord {
                lsn: 999, // The destination assigns its own LSN.
                commit_ts: 2,
                ops: vec![node(1), invalid],
            })
            .unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{name}: {err}");
        assert_eq!(
            {
                let wal = db.inner.wal.lock().unwrap();
                (wal.lsn, wal.bytes, wal.commits)
            },
            (lsn, bytes, commits),
            "{name}"
        );
        assert_eq!(std::fs::read(&wal_file).unwrap(), before, "{name}");
        assert!(db.inner.graph.node(1).is_none(), "{name}");
        drop(db);

        let reopened = Database::open(uri.to_str().unwrap()).unwrap();
        assert_eq!(
            reopened
                .connect()
                .unwrap()
                .query("MATCH (n) RETURN count(n)")
                .unwrap()
                .rows[0][0],
            Value::Int(1),
            "{name}"
        );
    }
}

#[cfg(feature = "parquet")]
#[test]
fn valid_replicated_create_survives_reopen_but_not_the_identity_map() {
    use cypherlite_executor::PullKind;

    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    let db = Database::open(uri.to_str().unwrap()).unwrap();
    let ops = vec![
        node(0),
        node(1),
        rel(0, 0, 1),
        RedoOp::PullIdentityPut {
            source: "origin".into(),
            kind: PullKind::Node,
            remote_id: "remote-1".into(),
            local_id: 1,
        },
    ];
    assert_eq!(
        db.apply_replicated(&WalRecord {
            lsn: 42,
            commit_ts: 7,
            ops: ops.clone(),
        })
        .unwrap(),
        1
    );
    assert_eq!(db.cdc_records_after(0, 10).unwrap()[0].ops.len(), ops.len());
    drop(db);

    let reopened = Database::open(uri.to_str().unwrap()).unwrap();
    assert_eq!(reopened.inner.graph.commit_ts(), 7);
    assert_eq!(reopened.inner.graph.all_node_ids(), vec![0, 1]);
    assert!(reopened.inner.graph.rel(0).is_some());
    // The identity map is load-scoped memory (ADR-0158): the data survives
    // the reopen, the map does not.
    assert_eq!(
        reopened
            .inner
            .graph
            .pull_identity_get("origin", PullKind::Node, "remote-1"),
        None
    );
}

#[test]
fn replicated_missing_or_previously_deleted_endpoint_does_not_append() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    let db = Database::open(uri.to_str().unwrap()).unwrap();
    db.connect().unwrap().query("CREATE (:Prior)").unwrap();
    let wal_file = uri.join("wal/00000001.log");
    let prior = std::fs::read(&wal_file).unwrap();
    for ops in [
        vec![node(1), rel(0, 1, 99)],
        vec![node(1), RedoOp::DeleteNode { id: 1 }, rel(0, 0, 1)],
    ] {
        let error = db
            .apply_replicated(&WalRecord {
                lsn: 999,
                commit_ts: 2,
                ops,
            })
            .unwrap_err();
        assert!(matches!(error, Error::Semantic(_)), "{error}");
        assert_eq!(db.inner.wal.lock().unwrap().lsn, 1);
        assert_eq!(std::fs::read(&wal_file).unwrap(), prior);
        assert_eq!(db.inner.graph.all_node_ids(), vec![0]);
    }
    db.leak_without_checkpoint();
    let reopened = Database::open(uri.to_str().unwrap()).unwrap();
    assert_eq!(reopened.inner.graph.all_node_ids(), vec![0]);
}

#[test]
fn replicated_incomplete_label_constraint_does_not_append() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    let db = Database::open(uri.to_str().unwrap()).unwrap();
    db.connect().unwrap().query("CREATE (:Prior)").unwrap();
    let wal_file = uri.join("wal/00000001.log");
    let before = std::fs::read(&wal_file).unwrap();
    let constraint = RedoOp::CreateConstraint {
        name: "rule".into(),
        relationship: false,
        label: "Person".into(),
        properties: vec![],
        kind: "nodeLabelExistence".into(),
        property_type: None,
    };
    let mismatched = RedoOp::CatalogDerivedConstraint {
        constraint: cypherlite_model::Constraint {
            name: "rule".into(),
            label: "Other".into(),
            properties: vec![],
            constraint_type: cypherlite_model::ConstraintType::NodeLabelExistence,
            property_type: None,
            entity_type: "NODE".into(),
            enforced_label: Some("Verified".into()),
        },
    };
    for ops in [
        vec![constraint.clone()],
        vec![node(1), constraint.clone()],
        vec![constraint, mismatched],
    ] {
        let error = db
            .apply_replicated(&WalRecord {
                lsn: 90,
                commit_ts: 2,
                ops,
            })
            .unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        assert_eq!(db.inner.wal.lock().unwrap().lsn, 1);
        assert_eq!(std::fs::read(&wal_file).unwrap(), before);
        assert!(db.inner.graph.named_constraints().is_empty());
        assert!(db.inner.graph.node(1).is_none());
    }
    db.leak_without_checkpoint();
    let reopened = Database::open(uri.to_str().unwrap()).unwrap();
    assert!(reopened.inner.graph.named_constraints().is_empty());
}

#[test]
fn replicated_post_sync_apply_failure_fences_and_replays_on_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    let db = Database::open(uri.to_str().unwrap()).unwrap();
    db.inner.fail_after_sync.store(true, Ordering::SeqCst);
    let err = db
        .apply_replicated(&WalRecord {
            lsn: 999,
            commit_ts: 1,
            ops: vec![node(0)],
        })
        .unwrap_err();
    assert!(err.to_string().contains("uncertain WAL outcome"), "{err}");
    assert_eq!(db.inner.safe_lsn.load(Ordering::Acquire), 0);
    assert!(db.cdc_records_after(0, 1).is_err());
    assert!(db.checkpoint().is_err());
    drop(db);
    assert!(!uri.join("manifest.json").exists());
    let reopened = Database::open(uri.to_str().unwrap()).unwrap();
    assert_eq!(reopened.inner.graph.all_node_ids(), [0]);
}

#[cfg(feature = "parquet")]
#[test]
fn checkpoint_rejects_forged_part_metrics_without_replacing_old_files() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    let store = cypherlite_storage::parse_uri(uri.to_str().unwrap()).unwrap();
    let graph = Graph::default();
    graph.begin_write().unwrap();
    graph
        .create_node(vec!["A".into()], Default::default())
        .unwrap();
    graph.set_next_node_id(300_000);
    graph
        .create_node(vec!["B".into()], Default::default())
        .unwrap();
    let ops = graph.take_redo();
    let ts = graph.pending_commit_ts();
    graph.abort_write();
    graph.apply_redo(ops, ts).unwrap();
    cypherlite_storage::checkpoint_at(&store, &graph, 0, ts, None).unwrap();
    drop(store);

    let db = Database::open(uri.to_str().unwrap()).unwrap();
    let path = uri.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let parts = manifest["node_parts"].as_array_mut().unwrap();
    // The default target coalesces both rows' windows into one part: they
    // span 524,288 ids together, within MAX_WINDOW (ADR-0183).
    assert_eq!(parts.len(), 1);
    let part_paths: Vec<_> = parts
        .iter()
        .map(|p| uri.join(p["path"].as_str().unwrap()))
        .collect();
    let original_parts: Vec<_> = part_paths
        .iter()
        .map(|p| std::fs::read(p).unwrap())
        .collect();
    for part in parts {
        part["bytes"] = serde_json::Value::from(u64::MAX);
    }
    let forged = serde_json::to_vec(&manifest).unwrap();
    std::fs::write(&path, &forged).unwrap();

    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| db.checkpoint()))
        .expect("checkpoint must return a typed error rather than panic")
        .unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), forged);
    for (part, bytes) in part_paths.iter().zip(original_parts) {
        assert_eq!(std::fs::read(part).unwrap(), bytes);
    }
    assert_eq!(
        std::fs::read_dir(uri.join("nodes")).unwrap().count(),
        part_paths.len()
    );
}

#[test]
fn public_open_rejects_crc_valid_max_entity_ids_without_panic() {
    for ops in [
        vec![node(i64::MAX)],
        vec![node(0), rel(i64::MAX, 0, 0)],
        vec![node(0), rel(1, i64::MAX, 0)],
        vec![node(0), rel(1, 0, i64::MAX)],
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let uri = tmp.path().join("db");
        write_wal(&uri, ops);
        let wal_file = uri.join("wal/00000001.log");
        let before = std::fs::read(&wal_file).unwrap();
        let err = Database::open(uri.to_str().unwrap()).err().unwrap();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert!(err.to_string().contains("i64::MAX"), "{err}");
        assert_eq!(std::fs::read(&wal_file).unwrap(), before);
    }
}

#[test]
fn last_valid_wal_ids_readable_but_new_creates_do_not_append() {
    type LastValidReadable = fn(&Graph) -> bool;
    let scenarios: [(Vec<RedoOp>, LastValidReadable, &str); 2] = [
        (
            vec![node(i64::MAX - 1)],
            |graph| graph.node(i64::MAX - 1).is_some(),
            "CREATE (:Extra)",
        ),
        (
            vec![node(0), rel(i64::MAX - 1, 0, 0)],
            |graph| graph.rel(i64::MAX - 1).is_some(),
            "MATCH (n:High) CREATE (n)-[:EXTRA]->(n)",
        ),
    ];
    for (ops, last_valid_readable, query) in scenarios {
        let tmp = tempfile::tempdir().unwrap();
        let uri = tmp.path().join("db");
        write_wal(&uri, ops);
        let wal_file = uri.join("wal/00000001.log");
        let original_wal = std::fs::read(&wal_file).unwrap();
        let db = Database::open(uri.to_str().unwrap()).unwrap();
        assert!(last_valid_readable(&db.inner.graph));
        let mut conn = db.connect().unwrap();
        let err = conn.query(query).unwrap_err();
        assert!(matches!(err, Error::Execution(_)), "{err}");
        assert!(err.to_string().contains("ID exhausted"), "{err}");
        assert_eq!(db.inner.wal.lock().unwrap().lsn, 1);
        assert_eq!(std::fs::read(&wal_file).unwrap(), original_wal);
        drop(conn);
        drop(db);
        let reopened = Database::open(uri.to_str().unwrap()).unwrap();
        let result = reopened
            .connect()
            .unwrap()
            .query("MATCH (n:High) RETURN count(n)")
            .unwrap();
        assert_eq!(result.rows[0][0], Value::Int(1));
        assert_eq!(reopened.inner.wal.lock().unwrap().lsn, 1);
    }
}

#[test]
fn multi_create_exhaustion_does_not_commit_partial_redo() {
    for (ops, query, count_query) in [
        (
            vec![node(i64::MAX - 2)],
            "CREATE (:A), (:B), (:C)",
            "MATCH (n) RETURN count(n)",
        ),
        (
            vec![node(0), rel(i64::MAX - 2, 0, 0)],
            "MATCH (n:High) CREATE (n)-[:A]->(n), (n)-[:B]->(n), (n)-[:C]->(n)",
            "MATCH ()-[r]->() RETURN count(r)",
        ),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let uri = tmp.path().join("db");
        write_wal(&uri, ops);
        let wal_file = uri.join("wal/00000001.log");
        let original_wal = std::fs::read(&wal_file).unwrap();
        let db = Database::open(uri.to_str().unwrap()).unwrap();
        let err = db.connect().unwrap().query(query).unwrap_err();
        assert!(matches!(err, Error::Execution(_)), "{err}");
        assert!(err.to_string().contains("ID exhausted"), "{err}");
        assert_eq!(db.inner.wal.lock().unwrap().lsn, 1);
        assert_eq!(std::fs::read(&wal_file).unwrap(), original_wal);
        assert_eq!(
            db.connect().unwrap().query(count_query).unwrap().rows[0][0],
            Value::Int(1)
        );
        drop(db);
        let reopened = Database::open(uri.to_str().unwrap()).unwrap();
        assert_eq!(
            reopened.connect().unwrap().query(count_query).unwrap().rows[0][0],
            Value::Int(1)
        );
    }
}

#[test]
fn public_open_rejects_json_max_ids() {
    for graph in [json_node_graph(i64::MAX), json_rel_graph(i64::MAX)] {
        let (_tmp, uri) = json_store(&graph);
        let err = Database::open(uri.to_str().unwrap()).err().unwrap();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert_eq!(std::fs::read(uri.join("graph.json")).unwrap(), graph);
    }
}

#[test]
fn public_open_reads_last_valid_json_ids() {
    for (graph, query) in [
        (json_node_graph(i64::MAX - 1), "CREATE (:Extra)"),
        (
            json_rel_graph(i64::MAX - 1),
            "MATCH (n:High) CREATE (n)-[:EXTRA]->(n)",
        ),
    ] {
        let (_tmp, uri) = json_store(&graph);
        let db = Database::open(uri.to_str().unwrap()).unwrap();
        let mut conn = db.connect().unwrap();
        assert!(matches!(conn.query(query), Err(Error::Execution(_))));
        assert_eq!(db.inner.wal.lock().unwrap().lsn, 0);
        drop(conn);
        drop(db);
        let reopened = Database::open(uri.to_str().unwrap()).unwrap();
        assert_eq!(crate::test_opts::count(&reopened, "High"), 1);
    }
}

#[test]
fn public_open_rejects_json_max_relationship_endpoints() {
    for (start, end) in [(i64::MAX, 0), (0, i64::MAX)] {
        let data = serde_json::json!({
            "nodes": [{"id": 0, "labels": [], "properties": {}}],
            "relationships": [{"id": 0, "type": "R", "start_node": start, "end_node": end, "properties": {}}],
            "next_node_id": 1,
            "next_rel_id": 1
        });
        let (_tmp, uri) = json_store(&serde_json::to_vec(&data).unwrap());
        let err = Database::open(uri.to_str().unwrap()).err().unwrap();
        assert!(matches!(err, Error::Storage(_)), "{err}");
    }
}

#[test]
fn public_open_rejects_max_json_manifest_timestamp_even_with_valid_wal() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    write_wal(&uri, vec![node(0)]);
    let graph_path = uri.join("graph.json");
    let manifest_path = uri.join("manifest.json");
    let wal_path = uri.join("wal/00000001.log");
    let graph = br#"{"nodes":[],"relationships":[],"next_node_id":0,"next_rel_id":0}"#;
    std::fs::write(&graph_path, graph).unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "codec": "json", "checkpoint_lsn": 0, "commit_ts": u64::MAX
    }))
    .unwrap();
    std::fs::write(&manifest_path, &manifest).unwrap();
    let wal = std::fs::read(&wal_path).unwrap();

    let err = Database::open(uri.to_str().unwrap()).err().unwrap();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(err.to_string().contains("timestamp"), "{err}");
    assert_eq!(std::fs::read(&graph_path).unwrap(), graph);
    assert_eq!(std::fs::read(&manifest_path).unwrap(), manifest);
    assert_eq!(std::fs::read(&wal_path).unwrap(), wal);
}

#[test]
fn public_open_returns_storage_error_for_crc_valid_max_wal_timestamp() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    let wal_dir = uri.join("wal");
    std::fs::create_dir_all(&wal_dir).unwrap();
    std::fs::write(
        uri.join(cypherlite_storage::LOCK_MARKER),
        cypherlite_storage::ROOT_ROLE,
    )
    .unwrap();
    std::fs::write(
        wal_dir.join(cypherlite_storage::LOCK_MARKER),
        cypherlite_storage::WAL_ROLE,
    )
    .unwrap();
    let path = wal_dir.join("00000001.log");
    for ts in [u64::MAX, u64::MAX - 1] {
        let payload = format!(r#"{{"lsn":1,"commit_ts":{ts},"ops":[]}}"#);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(cypherlite_storage::MAGIC).unwrap();
        file.write_all(&crc32fast::hash(payload.as_bytes()).to_le_bytes())
            .unwrap();
        file.write_all(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(payload.as_bytes()).unwrap();
        drop(file);
        if ts == u64::MAX {
            let err = Database::open(uri.to_str().unwrap()).err().unwrap();
            assert!(matches!(err, cypherlite_core::Error::Storage(_)), "{err}");
        } else {
            let db = Database::open(uri.to_str().unwrap()).unwrap();
            assert_eq!(db.inner.graph.commit_ts(), ts);
            drop(db);
        }
    }
}

#[cfg(feature = "parquet")]
#[test]
fn public_open_restores_last_supported_parquet_node_id() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    let target = cypherlite_storage::parse_uri(uri.to_str().unwrap()).unwrap();
    let graph = cypherlite_executor::Graph::default();
    graph.set_next_node_id(i64::MAX - 1);
    graph.begin_write().unwrap();
    graph
        .create_node(vec!["High".into()], Default::default())
        .unwrap();
    let ops = graph.take_redo();
    let ts = graph.pending_commit_ts();
    graph.abort_write();
    graph.apply_redo(ops, ts).unwrap();
    cypherlite_storage::checkpoint_at(&target, &graph, 0, ts, None).unwrap();
    drop(target);
    let db = Database::open(uri.to_str().unwrap()).unwrap();
    assert_eq!(db.inner.graph.next_node_id(), i64::MAX);
    assert!(db.inner.graph.node(i64::MAX - 1).is_some());
    let err = db.connect().unwrap().query("CREATE (:Extra)").unwrap_err();
    assert!(matches!(err, Error::Execution(_)), "{err}");
    assert_eq!(db.inner.wal.lock().unwrap().lsn, 0);
    drop(db);
    let reopened = Database::open(uri.to_str().unwrap()).unwrap();
    assert_eq!(reopened.inner.graph.next_node_id(), i64::MAX);
    assert!(reopened.inner.graph.node(i64::MAX - 1).is_some());
}

#[cfg(feature = "parquet")]
#[test]
fn public_open_restores_last_supported_parquet_rel_id() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = tmp.path().join("db");
    let target = cypherlite_storage::parse_uri(uri.to_str().unwrap()).unwrap();
    let graph = cypherlite_executor::Graph::default();
    graph.begin_write().unwrap();
    let n = graph
        .create_node(vec!["High".into()], Default::default())
        .unwrap();
    graph.set_next_rel_id(i64::MAX - 1);
    graph
        .create_rel(n, n, "R".into(), Default::default())
        .unwrap();
    let ops = graph.take_redo();
    let ts = graph.pending_commit_ts();
    graph.abort_write();
    graph.apply_redo(ops, ts).unwrap();
    cypherlite_storage::checkpoint_at(&target, &graph, 0, ts, None).unwrap();
    drop(target);
    let db = Database::open(uri.to_str().unwrap()).unwrap();
    assert!(db.inner.graph.rel(i64::MAX - 1).is_some());
    let err = db
        .connect()
        .unwrap()
        .query("MATCH (n:High) CREATE (n)-[:EXTRA]->(n)")
        .unwrap_err();
    assert!(matches!(err, Error::Execution(_)), "{err}");
    assert_eq!(db.inner.wal.lock().unwrap().lsn, 0);
    drop(db);
    let reopened = Database::open(uri.to_str().unwrap()).unwrap();
    assert!(reopened.inner.graph.rel(i64::MAX - 1).is_some());
}
