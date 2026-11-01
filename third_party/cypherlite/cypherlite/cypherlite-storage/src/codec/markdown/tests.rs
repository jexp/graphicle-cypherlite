//! Markdown codec tests (ADR-0406 §7): the JSON roundtrip suite's
//! mirror on Native and Memory VFS, plus the guard/checkpoint/id
//! contracts — reserved-view refusals, fail-closed rules, the
//! stat-first guard's content-based verdict, and stable ids. The
//! pipeline-specific suites live beside their modules:
//! `load/tests.rs` (bootstrap, folds, edit pickup) and
//! `journal/tests.rs` (crash recovery). The shared graph builders and
//! oracles live here.

use super::*;
use crate::codec::SnapshotCodecKind;
use crate::vfs::read_to_vec;
use crate::CodecOpenConfig;
use cypherlite_core::Node;
use cypherlite_core::{PropertyMap, Relationship, Temporal, Value};
use cypherlite_executor::Graph;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) fn string(value: &str) -> Value {
    Value::String(Arc::from(value))
}

pub(super) fn int(value: i64) -> Value {
    Value::Int(value)
}

fn list(values: Vec<Value>) -> Value {
    Value::List(Arc::new(values))
}

pub(super) fn props(entries: &[(&str, Value)]) -> Arc<PropertyMap> {
    PropertyMap::share(
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn no_props() -> Arc<PropertyMap> {
    PropertyMap::share(BTreeMap::new())
}

pub(super) fn person_graph() -> Graph {
    let graph = Graph::default();
    graph
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec!["Person".into()]),
            properties: props(&[
                ("name", string("Ada")),
                ("age", int(36)),
                ("tags", list(vec![string("pioneer"), string("math")])),
                (
                    "born",
                    Value::Temporal(Temporal::Duration {
                        months: 0,
                        days: 1,
                        nanos: 0,
                    }),
                ),
                ("body", string("Analytical engine pioneer.")),
            ]),
        })
        .unwrap();
    graph
        .load_checkpoint_node(Node {
            id: 2,
            labels: Arc::new(vec!["Person".into()]),
            properties: props(&[("name", string("Grace"))]),
        })
        .unwrap();
    graph
        .load_checkpoint_node(Node {
            id: 3,
            labels: Arc::new(vec!["Company".into(), "Org".into()]),
            properties: props(&[("name", string("X")), ("founded", int(1911))]),
        })
        .unwrap();
    graph
        .load_checkpoint_rel(Relationship {
            id: 1,
            start: 1,
            end: 2,
            rel_type: "KNOWS".into(),
            properties: props(&[("since", int(1833))]),
        })
        .unwrap();
    graph
        .load_checkpoint_rel(Relationship {
            id: 2,
            start: 1,
            end: 3,
            rel_type: "PARTNERS".into(),
            properties: no_props(),
        })
        .unwrap();
    graph.set_next_node_id(4);
    graph.set_next_rel_id(3);
    graph.set_commit_ts(7);
    graph
}

/// Nodes keyed by vault identity, relationships keyed by their triple.
type GraphSignature = (BTreeMap<String, Vec<String>>, Vec<(String, String, String)>);

pub(super) fn signature(graph: &Graph) -> GraphSignature {
    let mut nodes = BTreeMap::new();
    // The node's vault key, recorded per id while the nodes map is
    // built, so the relationship pass reads it instead of re-deriving
    // the label/name key for every endpoint.
    let mut key_by_id: BTreeMap<i64, String> = BTreeMap::new();
    for id in graph.all_node_ids_at(u64::MAX) {
        let node = graph.node_at(id, u64::MAX).unwrap();
        // `name` is genuinely absent on nodes whose key property is
        // another string (Book keys on `title`): the empty name is the
        // oracle's honest fallback, not a fabricated default over an
        // invariant-bound value.
        let name = node
            .properties
            .get("name")
            .and_then(Value::as_string)
            .unwrap_or_default()
            .to_string();
        let mut labels: Vec<String> = node.labels.as_ref().clone();
        labels.sort();
        let key = format!("{}/{}", labels[0], name);
        key_by_id.insert(id, key.clone());
        nodes.insert(key, labels);
    }
    let mut rels = Vec::new();
    for id in graph.all_rel_ids_at(u64::MAX) {
        let rel = graph.rel_at(id, u64::MAX).unwrap();
        let endpoint = |node_id: i64| {
            key_by_id
                .get(&node_id)
                .expect("every relationship endpoint is a walked node")
                .clone()
        };
        rels.push((endpoint(rel.start), rel.rel_type.clone(), endpoint(rel.end)));
    }
    rels.sort();
    (nodes, rels)
}

/// The suite's one find-node-by-name scan.
pub(super) fn node_by_name(graph: &Graph, name: &str) -> Node {
    graph
        .all_node_ids_at(u64::MAX)
        .into_iter()
        .map(|id| graph.node_at(id, u64::MAX).unwrap())
        .find(|node| {
            node.properties
                .get("name")
                .and_then(Value::as_string)
                .is_some_and(|key| key == name)
        })
        .expect("the test graph holds the named node")
}

fn assert_markdown_roundtrip(store: &StoreTarget) {
    let graph = person_graph();
    crate::checkpoint(store, &graph, None).unwrap();
    let vfs = store.vfs();
    let ada = read_to_vec(&*vfs, "Person/Ada.md").unwrap();
    let ada = String::from_utf8(ada).unwrap();
    assert!(
        ada.starts_with("---\ntype: Person\nage: 36\n"),
        "deterministic frontmatter order: {ada}"
    );
    assert!(ada.contains("KNOWS:\n  - since: 1833\n    target: \"[[Person/Grace]]\"\n"));
    assert!(ada.contains("PARTNERS:\n  - \"[[Company/X]]\"\n"));
    assert!(ada.contains("---\n\nAnalytical engine pioneer.\n"));
    assert!(vfs.exists("index.md").unwrap());
    assert!(vfs.exists("log.md").unwrap());
    assert!(vfs.exists("Person/index.md").unwrap());

    let (loaded, _wal) = crate::open_store(store).unwrap();
    assert_eq!(signature(&loaded), signature(&graph));
    assert_eq!(loaded.commit_ts(), 7);
    // Temporal and list properties survive the tagged-encoding detour
    // (looked up by name: load assigns ids in path order, Company first).
    let ada = node_by_name(&loaded, "Ada");
    let original = node_by_name(&graph, "Ada");
    assert_eq!(ada.properties.get("tags"), original.properties.get("tags"));
    assert_eq!(ada.properties.get("born"), original.properties.get("born"));

    // Idempotence: a second checkpoint of the reloaded graph changes no
    // guarded vault byte (formatting is stable, not churning). log.md is
    // the changelog — the first checkpoint logs the initial adds, so the
    // byte comparison runs from the second checkpoint onward.
    crate::checkpoint(store, &loaded, None).unwrap();
    let before: BTreeMap<String, Vec<u8>> = walk_vault(&*vfs)
        .unwrap()
        .into_iter()
        .map(|f| (f.walked.path, f.bytes))
        .collect();
    let before_views = (
        read_to_vec(&*vfs, "index.md").unwrap(),
        read_to_vec(&*vfs, "log.md").unwrap(),
    );
    let second = crate::open_store(store).unwrap().0;
    crate::checkpoint(store, &second, None).unwrap();
    let after: BTreeMap<String, Vec<u8>> = walk_vault(&*vfs)
        .unwrap()
        .into_iter()
        .map(|f| (f.walked.path, f.bytes))
        .collect();
    let after_views = (
        read_to_vec(&*vfs, "index.md").unwrap(),
        read_to_vec(&*vfs, "log.md").unwrap(),
    );
    assert_eq!(before, after);
    assert_eq!(before_views, after_views, "views are deterministic");
}

#[test]
fn markdown_snapshot_roundtrip_native_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    assert_markdown_roundtrip(&store);
}

#[test]
fn an_explicit_markdown_open_over_an_envelope_only_store_is_refused_before_the_lease() {
    // The envelope-only classic state (its graph objects removed, the
    // root manifest.json envelope remaining) is evidence too: the
    // refusal must precede any engine-state creation, and a markdown
    // vault's own unrelated manifest.json (which does not parse as an
    // envelope) must never trip it.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("manifest.json"),
        r#"{"codec":"parquet","checkpoint_lsn":4}"#,
    )
    .unwrap();
    let err = match crate::parse_uri_with_options(
        dir.path().to_str().unwrap(),
        None,
        Some(SnapshotCodecKind::Markdown),
    ) {
        Err(err) => err,
        Ok(_) => panic!("expected refusal, the explicit markdown open succeeded"),
    };
    assert!(err.to_string().contains("mixed snapshot codecs"), "{err}");
    assert!(
        !dir.path().join(".cypherlite").exists(),
        "the refusal must precede any engine-state creation"
    );
    // (A root manifest.json is engine-reserved namespace in any codec:
    // `read_envelope` treats any parseable one as a classic envelope
    // and the resolver refuses the mismatch — this guard only fires
    // earlier, on the explicit-classic subset, never on a directory
    // the resolver would have accepted.)
}

#[test]
fn a_case_only_key_rename_survives_checkpoint() {
    // `SET n.name = 'Ada'` over `Person/ada.md` renames the node's file
    // by casing alone: on a case-insensitive filesystem the old and new
    // spellings are one physical file, so the checkpoint must delete
    // the stale spelling BEFORE writing the new one — after would
    // unlink the file the write just produced (silent node loss;
    // visible on the macOS leg of the matrix, invisible on Linux).
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("Person")).unwrap();
    std::fs::write(
        vault.join("Person/ada.md"),
        "---\ntype: Person\nname: ada\n---\nbody",
    )
    .unwrap();
    let store = crate::parse_uri_with_options(
        vault.to_str().unwrap(),
        None,
        Some(SnapshotCodecKind::Markdown),
    )
    .unwrap();
    let (graph, _wal) = crate::open_store(&store).unwrap();
    let node = node_by_name(&graph, "ada");
    graph
        .load_checkpoint_node(Node {
            id: node.id,
            labels: node.labels.clone(),
            properties: props(&[("name", string("Ada")), ("body", string("body"))]),
        })
        .unwrap();
    crate::checkpoint(&store, &graph, None).unwrap();
    // The renamed node's file holds the new spelling's content, and the
    // node survives a reopen.
    let bytes = read_to_vec(&*store.vfs(), "Person/Ada.md").unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("name: Ada"), "{text}");
    let (reloaded, _wal) = crate::open_store(&store).unwrap();
    let ada = node_by_name(&reloaded, "Ada");
    assert_eq!(ada.properties.get("name"), Some(&string("Ada")));
}

#[test]
fn an_explicit_markdown_open_over_a_classic_store_is_refused_before_the_lease() {
    // The directory already carries a classic codec's root evidence
    // (`graph.json`): an explicit markdown request must fail closed
    // before the default-WAL lease materializes `.cypherlite/wal` and
    // its role marker inside that store.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("graph.json"), "{}").unwrap();
    let err = match crate::parse_uri_with_options(
        dir.path().to_str().unwrap(),
        None,
        Some(SnapshotCodecKind::Markdown),
    ) {
        Err(err) => err,
        Ok(_) => panic!("expected refusal, the explicit markdown open succeeded"),
    };
    assert!(err.to_string().contains("mixed snapshot codecs"), "{err}");
    assert!(
        !dir.path().join(".cypherlite").exists(),
        "the refusal must precede any engine-state creation"
    );
}

#[test]
fn markdown_snapshot_roundtrip_memory_vfs() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    assert_markdown_roundtrip(&store);
}

#[test]
fn a_reserved_view_name_is_refused_at_checkpoint_not_silently_overwritten() {
    // A node whose file would land on `Person/index.md` is refused at
    // checkpoint: the regenerated view would otherwise overwrite it and
    // the node would silently disappear at the next open.
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    let graph = Graph::default();
    graph
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec!["Person".into()]),
            properties: props(&[("name", string("index"))]),
        })
        .unwrap();
    graph.set_commit_ts(1);
    let err = crate::checkpoint(&store, &graph, None).unwrap_err();
    assert!(err.to_string().contains("reserved view"), "{err}");
}

#[test]
fn a_nested_reserved_view_name_is_refused_at_checkpoint() {
    // A nested key (`sub/index`) lands on `Person/sub/index.md` — the
    // reserved name hides in the path's final segment, and the walk
    // skips it at every level, so the refusal judges the planned path,
    // never the whole filename string.
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    let graph = Graph::default();
    graph
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec!["Person".into()]),
            properties: props(&[("name", string("sub/index"))]),
        })
        .unwrap();
    graph.set_commit_ts(1);
    let err = crate::checkpoint(&store, &graph, None).unwrap_err();
    assert!(
        err.to_string()
            .contains("reserved view file 'Person/sub/index.md'"),
        "{err}"
    );
}

#[test]
fn a_case_only_duplicate_key_is_refused_at_checkpoint() {
    // `Ada` and `ada` of one label fold to one vault identity at load
    // (the loser drops at canonicalization), so the write side refuses
    // the same collision — a committed graph must round-trip to the
    // same node count, never silently N−1.
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    let graph = Graph::default();
    for (id, name) in [(1, "Ada"), (2, "ada")] {
        graph
            .load_checkpoint_node(Node {
                id,
                labels: Arc::new(vec!["Person".into()]),
                properties: props(&[("name", string(name))]),
            })
            .unwrap();
    }
    graph.set_commit_ts(1);
    let err = crate::checkpoint(&store, &graph, None).unwrap_err();
    assert!(err.to_string().contains("share the key value"), "{err}");
}

#[test]
fn a_byte_identical_rewrite_with_a_fresh_mtime_is_not_a_change() {
    // The stat-first guard's mtime is advisory, never a verdict: a
    // rewritten-but-byte-identical file carries a fresh mtime with
    // unchanged content, and "changed" keeps the one content-based
    // definition (length + crc) the re-baseline trigger uses.
    let dir = tempfile::tempdir().unwrap();
    let store = crate::parse_uri_with_options(
        dir.path().to_str().unwrap(),
        None,
        Some(SnapshotCodecKind::Markdown),
    )
    .unwrap();
    crate::checkpoint(&store, &person_graph(), None).unwrap();
    // Rewrite Ada with identical bytes, then force a distinct mtime —
    // a plain rewrite can land inside the filesystem's timestamp
    // granularity.
    let ada = dir.path().join("Person/Ada.md");
    let bytes = std::fs::read(&ada).unwrap();
    std::fs::write(&ada, &bytes).unwrap();
    let file = std::fs::File::options().write(true).open(&ada).unwrap();
    file.set_times(
        std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(9_999),
        ),
    )
    .unwrap();
    assert!(
        !vault_changed_since_checkpoint(&store).unwrap(),
        "a fresh mtime over identical bytes is not a change"
    );
    // A real content change still is.
    std::fs::write(&ada, b"changed").unwrap();
    assert!(vault_changed_since_checkpoint(&store).unwrap());
}

#[test]
fn an_open_engine_write_survives_its_own_checkpoint_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    crate::checkpoint(&store, &person_graph(), None).unwrap();
    let (graph, _wal) = crate::open_store(&store).unwrap();
    // A node added in-session (id above the loaded range).
    graph
        .load_checkpoint_node(Node {
            id: 9,
            labels: Arc::new(vec!["Person".into()]),
            properties: props(&[("name", string("Mary"))]),
        })
        .unwrap();
    graph.set_next_node_id(10);
    crate::checkpoint_with_lsn(&store, &graph, 1, None).unwrap();
    assert!(store.vfs().exists("Person/Mary.md").unwrap());
    let (reloaded, _wal) = crate::open_store(&store).unwrap();
    let (nodes, _) = signature(&reloaded);
    assert!(nodes.contains_key("Person/Mary"));
}

#[test]
fn fail_closed_rules() {
    // Label-less node.
    let graph = Graph::default();
    graph
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec![]),
            properties: props(&[("name", string("Ada"))]),
        })
        .unwrap();
    graph.set_next_node_id(2);
    graph.set_commit_ts(1);
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    let err = crate::checkpoint(&store, &graph, None).unwrap_err();
    assert!(err.to_string().contains("no label"), "{err}");

    // Missing key property.
    let graph = Graph::default();
    graph
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec!["Person".into()]),
            properties: no_props(),
        })
        .unwrap();
    graph.set_next_node_id(2);
    graph.set_commit_ts(1);
    let err = crate::checkpoint(&store, &graph, None).unwrap_err();
    assert!(
        err.to_string()
            .contains("requires a non-null unique 'name'"),
        "{err}"
    );

    // Duplicate key values within a label.
    let graph = Graph::default();
    for id in 1..=2 {
        graph
            .load_checkpoint_node(Node {
                id,
                labels: Arc::new(vec!["Person".into()]),
                properties: props(&[("name", string("Ada"))]),
            })
            .unwrap();
    }
    graph.set_next_node_id(3);
    graph.set_commit_ts(1);
    let err = crate::checkpoint(&store, &graph, None).unwrap_err();
    assert!(err.to_string().contains("share the key value"), "{err}");
}

#[test]
fn type_and_labels_properties_round_trip_through_demotion() {
    let graph = Graph::default();
    graph
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec!["Book".into(), "Read".into()]),
            properties: props(&[
                ("name", string("Dune")),
                ("type", string("novel")),
                ("labels", string("meta")),
            ]),
        })
        .unwrap();
    graph.set_next_node_id(2);
    graph.set_commit_ts(1);

    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    crate::checkpoint(&store, &graph, None).unwrap();
    let text = read_to_vec(&*store.vfs(), "Book/Dune.md").unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("type: Book\ntype_property: novel\nlabels_property: meta\n"));
    assert!(text.contains("labels:\n  - Read\n"));
    let (loaded, _wal) = crate::open_store(&store).unwrap();
    let node = loaded.node_at(1, u64::MAX).unwrap();
    assert_eq!(
        node.properties.get("type").and_then(Value::as_string),
        Some("novel")
    );
    assert_eq!(
        node.properties.get("labels").and_then(Value::as_string),
        Some("meta")
    );
    assert_eq!(
        node.labels.as_ref().clone(),
        vec!["Book".to_string(), "Read".to_string()]
    );
}

#[test]
fn an_in_session_vault_node_never_writes_to_the_vault_root() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    // A root file the vault already holds — under the old ambient-shape
    // guard its mere existence let any in-session :Vault node write a
    // NEW root file; provenance must refuse.
    std::fs::write(
        vault.join("readme.md"),
        "---\ntype: Vault\nname: readme\n---\n",
    )
    .unwrap();
    let store = crate::parse_uri_with_options(
        vault.to_str().unwrap(),
        None,
        Some(SnapshotCodecKind::Markdown),
    )
    .unwrap();
    let graph = Graph::default();
    graph
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec![VAULT_LABEL.to_string()]),
            properties: props(&[("name", string("readme"))]),
        })
        .unwrap();
    graph
        .load_checkpoint_node(Node {
            id: 2,
            labels: Arc::new(vec![VAULT_LABEL.to_string()]),
            properties: props(&[("name", string("Fresh"))]),
        })
        .unwrap();
    graph.set_commit_ts(1);
    let err = crate::checkpoint(&store, &graph, None).unwrap_err();
    assert!(
        err.to_string().contains("never writes to the vault root"),
        "{err}"
    );
}

#[test]
fn node_and_relationship_ids_are_stable_across_checkpoints() {
    // ADR-0406 §5's WAL-recovery contract: id-based WAL replay after a
    // checkpoint must address the same entities the writes named, so a
    // reload restores the ids the checkpoint recorded — proven by the
    // second checkpoint recording the identical id maps.
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    crate::checkpoint(&store, &person_graph(), None).unwrap();
    let first = read_manifest(&store).unwrap().unwrap();
    let (reloaded, _) = crate::open_store(&store).unwrap();
    crate::checkpoint(&store, &reloaded, None).unwrap();
    let second = read_manifest(&store).unwrap().unwrap();
    assert_eq!(
        first.node_ids, second.node_ids,
        "node ids drifted across reload"
    );
    assert_eq!(
        first.rel_ids, second.rel_ids,
        "relationship ids drifted across reload"
    );
}

/// The ADR-0460 body-properties suite, split from this file's shared
/// builders (the no-1k-line rule).
mod bodies;
