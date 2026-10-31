use super::*;
use std::sync::Arc;

mod bundle;
#[cfg(parquet_codec)]
mod parquet;
mod snapshot_gate;
mod uri;

#[cfg(not(target_arch = "wasm32"))]
mod exclusive_create;
#[cfg(not(target_arch = "wasm32"))]
mod migrate;
#[cfg(not(target_arch = "wasm32"))]
mod reset;

#[test]
fn storage_context_names_the_object_of_storage_and_io_messages_only() {
    // One variant decision, `map_message`'s, under both context helpers:
    // a prefix here, the version module's suffix there.
    let named = |error| storage_context(error, format_args!("manifest part nodes/p.parquet"));
    assert_eq!(
        named(Error::storage("id column")),
        Error::storage("manifest part nodes/p.parquet: id column")
    );
    assert_eq!(
        named(Error::io("short read")),
        Error::io("manifest part nodes/p.parquet: short read")
    );
    for verbatim in [Error::Conflict, Error::internal("worker panicked")] {
        assert_eq!(named(verbatim.clone()), verbatim);
    }
    assert_eq!(
        map_message(Error::storage("a"), |m| format!("{m}b")),
        Error::storage("ab")
    );
}

/// Sorted listing of a WAL directory plus the bytes of every segment in it:
/// the shared "WAL unchanged" snapshot for failed-open assertions.
pub(crate) fn wal_dir_state(vfs: &dyn Vfs, prefix: &str) -> Vec<(String, Vec<u8>)> {
    // Every caller has a WAL directory; a missing one must fail loudly
    // rather than compare two empty snapshots as "unchanged".
    let mut names = vfs.list_dir(prefix).unwrap();
    names.sort();
    names
        .into_iter()
        // The ownership lock marker is not a WAL segment. The still-open
        // store holds an exclusive lock on it, and Windows LockFileEx
        // blocks reads of the locked range, so never read it here.
        .filter(|name| name != LOCK_MARKER && name != WRITER_LEASE)
        .map(|name| {
            let path = crate::vfs::virt_join(prefix, &name);
            (name, vfs::read_to_vec(vfs, &path).unwrap())
        })
        .collect()
}

/// A legacy JSON snapshot body with no `value_encoding` marker: encoding 1
/// (ADR-0127 Decision 1), one `:Person` node named "Ada".
pub(crate) const LEGACY_JSON_GRAPH: &str = r#"{"nodes":[{"id":1,"labels":["Person"],"properties":{"name":"Ada"}}],"relationships":[],"indexes":[],"constraints":[],"next_node_id":2,"next_rel_id":1}"#;

/// A Parquet-codec `Manifest` at `version` with no shard parts, index or
/// constraint definitions, or theater; at a pre-v3 version, the legacy
/// monolith manifest (ADR-0127). The one place the multi-field literal is
/// constructed; shared by the migrate-on-open tests and
/// `snapshot::native::tests`.
#[cfg(parquet_codec)]
pub(crate) fn bare_manifest(
    version: u32,
    next_node_id: i64,
    next_rel_id: i64,
    checkpoint_lsn: u64,
    commit_ts: u64,
) -> Manifest {
    Manifest {
        format_version: version,
        next_node_id,
        next_rel_id,
        checkpoint_lsn,
        commit_ts,
        node_parts: vec![],
        rel_parts: vec![],
        bytes_per_node: 0.0,
        bytes_per_rel: 0.0,
        codec: SnapshotCodecKind::Parquet,
        indexes: vec![],
        constraints: vec![],
        theater: None,
        ontology_path: None,
    }
}

pub(crate) fn person_ada() -> (Graph, Wal) {
    let mut wal = Wal::memory();
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["Person".into()],
        std::collections::BTreeMap::from([(
            "name".into(),
            cypherlite_core::Value::String("Ada".into()),
        )]),
    )
    .unwrap();
    let ops = g.take_redo();
    let ts = g.pending_commit_ts();
    g.abort_write();
    wal.append_commit(ts, &ops).unwrap();
    g.apply_redo(ops.clone(), ts).unwrap();
    (g, wal)
}

/// The checkpoint-image object of `codec`, the one that carries the
/// theater: the manifest for Parquet, the graph body for JSON.
fn checkpoint_image(codec: SnapshotCodecKind) -> &'static str {
    match codec {
        SnapshotCodecKind::Parquet => crate::snapshot::MANIFEST_JSON,
        SnapshotCodecKind::Json => crate::codec::json::GRAPH_JSON,
    }
}

#[test]
fn snapshot_is_wal_applied_state() {
    let store = memory_store();
    let (g, mut wal) = person_ada();
    checkpoint_with_lsn(&store, &g, wal.lsn, None).unwrap();
    let cut = wal.rotate().unwrap();
    wal.recycle(cut).unwrap();
    let loaded = load_with_wal(&store, &wal).unwrap();
    assert_eq!(loaded.live_node_count(), 1);
    assert_eq!(
        loaded.node(0).unwrap().properties["name"],
        cypherlite_core::Value::String("Ada".into())
    );
}

#[test]
fn legacy_wal_label_constraint_replays_with_typed_marker_after_recycle() {
    use cypherlite_executor::RedoOp;
    for &codec in SnapshotCodecKind::compiled() {
        let store = memory_store_with_codec(codec).unwrap();
        let (vfs, prefix) = store.wal_vfs();
        let mut wal = Wal::open(vfs, prefix, 0).unwrap();
        let old = RedoOp::CreateConstraint {
            name: "old_rule".into(),
            relationship: false,
            label: "Person".into(),
            properties: vec![],
            kind: "nodeLabelExistence".into(),
            property_type: None,
        };
        wal.append_commit(1, &[old]).unwrap();
        wal.sync().unwrap();
        let graph = load_with_wal(&store, &wal).unwrap();
        assert_eq!(graph.named_constraints().len(), 1);
        checkpoint_with_lsn(&store, &graph, wal.lsn, None).unwrap();
        let cut = wal.rotate().unwrap();
        wal.recycle(cut).unwrap();
        let recovered = load_with_wal(&store, &wal).unwrap();
        assert_eq!(recovered.named_constraints(), graph.named_constraints());
        assert_eq!(
            recovered.checkpoint_theater().enforced_label("old_rule"),
            None
        );
        let bytes = crate::snapshot::get_object(&store, checkpoint_image(codec)).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["theater"]["legacy_unknown_constraints"]["old_rule"]["name"], "old_rule",
            "{codec:?}"
        );
    }
}

#[test]
fn direct_graph_incomplete_label_constraint_fails_before_checkpoint_writes() {
    for &codec in SnapshotCodecKind::compiled() {
        let store = memory_store_with_codec(codec).unwrap();
        let graph = Graph::default();
        checkpoint_with_lsn(&store, &graph, 0, None).unwrap();
        let old_manifest = crate::snapshot::get_object(&store, "manifest.json").unwrap();
        let old_graph = store
            .vfs
            .exists("graph.json")
            .unwrap()
            .then(|| crate::snapshot::get_object(&store, "graph.json").unwrap());
        graph
            .install_named_constraint(cypherlite_catalog::ConstraintDef {
                name: "new_rule".into(),
                relationship: false,
                label: "Person".into(),
                properties: vec![],
                kind: "nodeLabelExistence".into(),
                property_type: None,
            })
            .unwrap();
        let error = checkpoint_with_lsn(&store, &graph, 1, None).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{codec:?}: {error}");
        assert_eq!(
            crate::snapshot::get_object(&store, "manifest.json").unwrap(),
            old_manifest
        );
        assert_eq!(
            store
                .vfs
                .exists("graph.json")
                .unwrap()
                .then(|| crate::snapshot::get_object(&store, "graph.json").unwrap()),
            old_graph
        );
        assert!(load(&store).unwrap().named_constraints().is_empty());
    }
}

#[test]
fn legacy_omission_never_masks_a_new_incomplete_definition() {
    use cypherlite_executor::RedoOp;
    let graph = Graph::default();
    let old = cypherlite_catalog::ConstraintDef {
        name: "old".into(),
        relationship: false,
        label: "Old".into(),
        properties: vec![],
        kind: "nodeLabelExistence".into(),
        property_type: None,
    };
    graph.install_named_constraint(old.clone()).unwrap();
    graph.mark_legacy_unenforced();
    assert!(graph.checkpoint_theater_for_write().is_ok());
    let new = cypherlite_catalog::ConstraintDef {
        name: "new".into(),
        relationship: false,
        label: "New".into(),
        properties: vec![],
        kind: "nodeLabelExistence".into(),
        property_type: None,
    };
    graph.install_named_constraint(new).unwrap();
    assert!(matches!(
        graph.checkpoint_theater_for_write(),
        Err(Error::Storage(_))
    ));
    graph
        .apply_redo(vec![RedoOp::DropConstraint { name: "new".into() }], 1)
        .unwrap();
    graph
        .apply_redo(
            vec![RedoOp::CatalogDerivedConstraint {
                constraint: cypherlite_model::Constraint {
                    name: "old".into(),
                    label: "Old".into(),
                    properties: vec![],
                    constraint_type: cypherlite_model::ConstraintType::NodeLabelExistence,
                    property_type: None,
                    entity_type: "NODE".into(),
                    enforced_label: Some("Known".into()),
                },
            }],
            2,
        )
        .unwrap();
    assert_eq!(
        graph
            .checkpoint_theater_for_write()
            .unwrap()
            .enforced_label("old"),
        Some("Known")
    );
    graph
        .apply_redo(vec![RedoOp::DropConstraint { name: "old".into() }], 3)
        .unwrap();
    graph.install_named_constraint(old).unwrap();
    assert!(matches!(
        graph.checkpoint_theater_for_write(),
        Err(Error::Storage(_))
    ));
    graph
        .apply_redo(vec![RedoOp::DropConstraint { name: "old".into() }], 4)
        .unwrap();
    graph
        .apply_redo(
            vec![
                RedoOp::CreateConstraint {
                    name: "old".into(),
                    relationship: false,
                    label: "Old".into(),
                    properties: vec![],
                    kind: "nodeLabelExistence".into(),
                    property_type: None,
                },
                RedoOp::CatalogDerivedConstraint {
                    constraint: cypherlite_model::Constraint {
                        name: "old".into(),
                        label: "Old".into(),
                        properties: vec![],
                        constraint_type: cypherlite_model::ConstraintType::NodeLabelExistence,
                        property_type: None,
                        entity_type: "NODE".into(),
                        enforced_label: Some("Known".into()),
                    },
                },
            ],
            5,
        )
        .unwrap();
    let image = serde_json::to_value(graph.checkpoint_theater_for_write().unwrap()).unwrap();
    assert!(image.get("legacy_unknown_constraints").is_none());
    assert_eq!(
        image["derived_constraints"]["old"]["enforced_label"],
        "Known"
    );
}

#[test]
fn typed_checkpoint_rejects_forged_legacy_unknown_markers_in_both_codecs() {
    use cypherlite_executor::RedoOp;
    for &codec in SnapshotCodecKind::compiled() {
        let store = memory_store_with_codec(codec).unwrap();
        let graph = Graph::default();
        graph
            .apply_redo(
                vec![
                    RedoOp::CreateConstraint {
                        name: "known".into(),
                        relationship: false,
                        label: "Person".into(),
                        properties: vec![],
                        kind: "nodeLabelExistence".into(),
                        property_type: None,
                    },
                    RedoOp::CatalogDerivedConstraint {
                        constraint: cypherlite_model::Constraint {
                            name: "known".into(),
                            label: "Person".into(),
                            properties: vec![],
                            constraint_type: cypherlite_model::ConstraintType::NodeLabelExistence,
                            property_type: None,
                            entity_type: "NODE".into(),
                            enforced_label: Some("Verified".into()),
                        },
                    },
                ],
                1,
            )
            .unwrap();
        checkpoint_with_lsn(&store, &graph, 1, None).unwrap();
        let image = checkpoint_image(codec);
        let original: serde_json::Value =
            serde_json::from_slice(&crate::snapshot::get_object(&store, image).unwrap()).unwrap();
        let definition = serde_json::to_value(&graph.named_constraints()[0]).unwrap();
        for (name, def, omit_derived) in [
            ("not_named", definition.clone(), true),
            (
                "known",
                serde_json::json!({"name":"known", "relationship":false,
                "label":"Other", "properties":[], "kind":"nodeLabelExistence",
                "property_type":null}),
                true,
            ),
            ("known", definition, false),
        ] {
            let mut forged = original.clone();
            // A matching marker is also invalid while its derived metadata exists.
            if omit_derived {
                forged["theater"]["derived_constraints"]
                    .as_object_mut()
                    .unwrap()
                    .remove("known");
            }
            forged["theater"]["legacy_unknown_constraints"][name] = def;
            crate::snapshot::put_object(&store, image, &serde_json::to_vec(&forged).unwrap())
                .unwrap();
            assert!(
                matches!(load(&store), Err(Error::Storage(_))),
                "{codec:?}: {name}"
            );
        }
    }
}

#[test]
fn materialize_replays_wal_without_snapshot() {
    let store = memory_store();
    let (_g, wal) = person_ada();
    let loaded = load_with_wal(&store, &wal).unwrap();
    assert_eq!(loaded.live_node_count(), 1);
}

#[test]
fn startup_streams_many_frames_across_segments_and_preserves_pull_redo() {
    use cypherlite_executor::{PullKind, RedoOp};

    let dir = tempfile::tempdir().unwrap();
    for store in [
        memory_store(),
        parse_uri(dir.path().join("many").to_str().unwrap()).unwrap(),
    ] {
        let (vfs, prefix) = store.wal_vfs();
        let mut wal = Wal::open(vfs, prefix, 0).unwrap();
        let graph = Graph::default();
        let seed = vec![RedoOp::CreateNode {
            id: 0,
            labels: vec!["Seed".into()].into(),
            properties: Default::default(),
        }];
        wal.append_commit(1, &seed).unwrap();
        graph.apply_redo(seed, 1).unwrap();
        checkpoint_with_lsn(&store, &graph, 1, None).unwrap();

        for id in 1..=1_500 {
            let ops = [
                RedoOp::CreateNode {
                    id,
                    labels: vec![].into(),
                    properties: Default::default(),
                },
                RedoOp::PullIdentityPut {
                    source: "upstream".into(),
                    kind: PullKind::Node,
                    remote_id: id.to_string(),
                    local_id: id,
                },
            ];
            wal.append_commit(id as u64 + 1, &ops).unwrap();
            if id == 750 {
                wal.rotate().unwrap();
            }
        }
        drop(wal);

        let (loaded, mut reopened) = open_store(&store).unwrap();
        assert_eq!(reopened.lsn, 1_501);
        assert_eq!(reopened.commits, 750);
        assert_eq!(reopened.checkpoint_pin(), (751, 751));
        assert_eq!(loaded.commit_ts(), 1_501);
        assert_eq!(loaded.live_node_count(), 1_501);
        assert!(loaded.node(0).is_some(), "checkpoint row must survive");
        for id in [1, 750, 1_500] {
            assert_eq!(
                loaded.pull_identity_get("upstream", PullKind::Node, &id.to_string()),
                Some(id)
            );
        }

        let page = reopened.records_after(1, 37).unwrap();
        assert_eq!(page.len(), 37);
        assert_eq!(page[0].lsn, 2);
        assert_eq!(page[36].lsn, 38);
        assert_eq!(reopened.records_after(38, 1).unwrap()[0].lsn, 39);

        let records = reopened.replay(1).unwrap();
        assert_eq!(records.len(), 1_500);
        drop((records, loaded, reopened, store));
    }
}

#[test]
fn streaming_recovery_trims_corrupt_active_tail_and_fails_without_publishing_interior_corruption() {
    use cypherlite_executor::RedoOp;

    let node = |id: i64| RedoOp::CreateNode {
        id,
        labels: vec![].into(),
        properties: Default::default(),
    };

    // A complete final frame on the active segment with a bad checksum is a
    // relaxed append tail (ADR-0077): recovery keeps the committed prefix
    // and trims it rather than failing closed.
    {
        let store = memory_store();
        let (vfs, prefix) = store.wal_vfs();
        let mut wal = Wal::open(Arc::clone(&vfs), prefix, 0).unwrap();
        wal.append_commit(1, &[node(0)]).unwrap();
        let path = "wal/00000001.log";
        let committed = vfs::read_to_vec(&*vfs, path).unwrap();
        let mut file = vfs.open(path, vfs::VfsOpen::append_create()).unwrap();
        file.write_all(&0u32.to_le_bytes()).unwrap(); // deliberately incorrect CRC
        let payload = br#"{"lsn":2,"commit_ts":2,"ops":[]}"#;
        file.write_all(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(payload).unwrap();
        drop(file);
        assert!(
            vfs::read_to_vec(&*vfs, path).unwrap().len() > committed.len(),
            "test must leave a damaged suffix"
        );

        // Streaming recovery of the already-open handle tolerates the
        // bad-EOF frame and publishes the committed prefix.
        let graph = load_with_wal(&store, &wal).unwrap();
        assert_eq!(graph.commit_ts(), 1);
        assert!(graph.node(0).is_some());
        drop(wal);

        // Reopen trims the damaged suffix; the committed prefix survives.
        let (graph, reopened) = open_store(&store).unwrap();
        assert_eq!(graph.commit_ts(), 1);
        assert!(graph.node(0).is_some());
        assert_eq!(reopened.lsn, 1);
        assert_eq!(vfs::read_to_vec(&*vfs, path).unwrap(), committed);
    }

    // Interior corruption (a bad frame with a valid frame after it) is not
    // a tail: open fails without publishing a partial graph or touching the
    // WAL bytes.
    {
        let store = memory_store();
        let (vfs, prefix) = store.wal_vfs();
        let mut wal = Wal::open(Arc::clone(&vfs), prefix, 0).unwrap();
        wal.append_commit(1, &[node(0)]).unwrap();
        let path = "wal/00000001.log";
        let mut file = vfs.open(path, vfs::VfsOpen::append_create()).unwrap();
        file.write_all(&0u32.to_le_bytes()).unwrap(); // deliberately incorrect CRC
        let bad = br#"{"lsn":2,"commit_ts":2,"ops":[]}"#;
        file.write_all(&(bad.len() as u32).to_le_bytes()).unwrap();
        file.write_all(bad).unwrap();
        drop(file);
        let good = serde_json::to_vec(&WalRecord {
            lsn: 3,
            commit_ts: 3,
            ops: vec![],
        })
        .unwrap();
        let mut file = vfs.open(path, vfs::VfsOpen::append_create()).unwrap();
        file.write_all(&crc32fast::hash(&good).to_le_bytes())
            .unwrap();
        file.write_all(&(good.len() as u32).to_le_bytes()).unwrap();
        file.write_all(&good).unwrap();
        drop(file);
        let before = vfs::read_to_vec(&*vfs, path).unwrap();

        assert!(matches!(
            load_with_wal(&store, &wal),
            Err(Error::Storage(_))
        ));
        assert_eq!(vfs::read_to_vec(&*vfs, path).unwrap(), before);
        drop(wal);
        assert!(matches!(open_store(&store), Err(Error::Storage(_))));
        assert_eq!(vfs::read_to_vec(&*vfs, path).unwrap(), before);
    }
}

#[test]
fn atomic_write_overwrites_and_leaves_no_tmp() {
    // The Windows-critical property of `atomic_write` — handle
    // closed before rename, write-mode handle for sync_all — is
    // structural (a lexical scope): POSIX can't observe a regression
    // because fsync(O_RDONLY) and rename-while-open both succeed there.
    // What POSIX *can* pin is the observable contract: dest content,
    // overwrite of an existing dest, and no leftover `.tmp`.
    let dir = tempfile::tempdir().unwrap();
    let vfs = NativeVfs::new(dir.path());
    atomic_write(&vfs, "obj.bin", b"one").unwrap();
    assert_eq!(std::fs::read(dir.path().join("obj.bin")).unwrap(), b"one");
    atomic_write(&vfs, "obj.bin", b"two").unwrap();
    assert_eq!(std::fs::read(dir.path().join("obj.bin")).unwrap(), b"two");
    assert!(!dir.path().join("obj.bin.tmp").exists());
}

#[test]
fn recycle_deletes_closed_segment_file() {
    let dir = tempfile::tempdir().unwrap();
    let wal_dir = dir.path().join("wal");
    let vfs = Arc::new(NativeVfs::new(&wal_dir));
    let mut wal = Wal::open(vfs, "", 0).unwrap();
    wal.append_commit(7, &[]).unwrap();
    let lsn = wal.rotate().unwrap();
    let pin = wal.checkpoint_pin();
    assert_eq!(pin, (lsn, 7));
    assert!(wal_dir.join("00000001.log").exists());
    assert!(wal_dir.join("00000002.log").exists());
    wal.recycle(lsn).unwrap();
    assert!(!wal_dir.join("00000001.log").exists());
    assert!(wal_dir.join("00000002.log").exists());
}

#[test]
fn json_codec_full_rewrite_roundtrip() {
    let store = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let (g, mut wal) = person_ada();
    checkpoint_with_lsn(&store, &g, wal.lsn, None).unwrap();
    let cut = wal.rotate().unwrap();
    wal.recycle(cut).unwrap();
    assert!(snapshot::object_exists(&store, "graph.json").unwrap());
    let loaded = load_with_wal(&store, &wal).unwrap();
    assert_eq!(loaded.live_node_count(), 1);
    assert_eq!(
        loaded.node(0).unwrap().properties["name"],
        cypherlite_core::Value::String("Ada".into())
    );
    let raw = snapshot::get_object(&store, "graph.json").unwrap();
    let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(v["nodes"][0]["properties"]["name"], "Ada");
    assert!(v.get("relationships").is_some());
    assert!(snapshot_data_bytes(&store).unwrap() > 0);
}

#[test]
fn json_codec_mixed_types_match_cypherlite_graphdata() {
    let store = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["T".into()],
        std::collections::BTreeMap::from([
            ("n".into(), cypherlite_core::Value::Int(7)),
            (
                "pt".into(),
                cypherlite_core::Value::Point(cypherlite_core::PointValue {
                    srid: 4326,
                    x: 1.5,
                    y: 2.5,
                    z: None,
                }),
            ),
        ]),
    )
    .unwrap();
    let ops = g.take_redo();
    let ts = g.pending_commit_ts();
    g.abort_write();
    let mut wal = Wal::memory();
    wal.append_commit(ts, &ops).unwrap();
    g.apply_redo(ops.clone(), ts).unwrap();
    checkpoint_with_lsn(&store, &g, wal.lsn, None).unwrap();
    let raw = String::from_utf8(snapshot::get_object(&store, "graph.json").unwrap()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let props = &v["nodes"][0]["properties"];
    assert_eq!(props["n"], 7);
    assert_eq!(props["pt"]["__type"], "point");
    let loaded = load(&store).unwrap();
    assert_eq!(
        loaded.node(0).unwrap().properties["n"],
        cypherlite_core::Value::Int(7)
    );
}

#[test]
fn json_import_graphdata_without_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    std::fs::write(dir.path().join("graph.json"), LEGACY_JSON_GRAPH).unwrap();
    let target = parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json)).unwrap();
    assert_eq!(target.snapshot_codec(), SnapshotCodecKind::Json);
    let (g, _wal) = open_store(&target).unwrap();
    assert_eq!(g.live_node_count(), 1);
    assert_eq!(
        g.node(1).unwrap().properties["name"],
        cypherlite_core::Value::String("Ada".into())
    );
}

#[test]
fn codec_mix_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    std::fs::write(dir.path().join("graph.json"), "{}").unwrap();
    std::fs::write(dir.path().join("nodes.parquet"), b"not parquet").unwrap();
    let err = parse_uri(uri).err().expect("mixed files");
    assert!(err.to_string().contains("mixed"), "{err}");
}
