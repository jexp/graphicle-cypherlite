//! Load-path tests (ADR-0406 §4/§7): bootstrap inference over an
//! external vault, stub materialization, closed-engine edit pickup,
//! the walk's fail-closed rules, and the fold and set semantics.

use super::super::tests::{int, node_by_name, person_graph, props, signature, string};
use super::*;
use crate::codec::SnapshotCodecKind;
use crate::vfs::atomic_write;
use cypherlite_core::Relationship;

fn write_external_vault(root: &std::path::Path) {
    let person = root.join("Person");
    let book = root.join("Book");
    let place = root.join("Place");
    std::fs::create_dir_all(&person).unwrap();
    std::fs::create_dir_all(&book).unwrap();
    std::fs::create_dir_all(&place).unwrap();
    std::fs::write(
        person.join("Ada.md"),
        "---\ntype: Person\nname: Ada\nKNOWS:\n  - \"[[Person/Grace]]\"\n  - target: \"[[Place/Ithaca]]\"\n    how: ship\n  - \"[[Place/Nowhere]]\"\n---\n\nThe wanderer.\n",
    ).unwrap();
    std::fs::write(
        person.join("Grace.md"),
        "---\ntype: Person\nname: Grace\nrecords:\n  - alpha\n  - beta\n---\n",
    )
    .unwrap();
    std::fs::write(book.join("Dune.md"), "---\ntype: Book\ntitle: Dune\n---\n").unwrap();
    std::fs::write(
        place.join("Ithaca.md"),
        "---\ntype: Place\nname: Ithaca\n---\n",
    )
    .unwrap();
    // A root-level `.md` file loads as a node under the implicit
    // :Vault label (ADR-0406 §2) — ordinary vault content, but a node.
    std::fs::write(root.join("README.md"), "# My vault\n").unwrap();
}

#[test]
fn a_root_level_wal_label_directory_is_user_content_not_the_direct_wal_slot() {
    // A `:wal` label's files land in a root-level `wal/` directory —
    // exactly the classic codecs' direct-WAL spelling. The vault's
    // engine WAL is the `.cypherlite/` sidecar, so the label directory
    // must never be misread as the direct-WAL slot (an ambiguity the
    // open would otherwise refuse): the engine must open the layout it
    // itself writes.
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("wal")).unwrap();
    std::fs::write(
        vault.join("wal/x.md"),
        "---\ntype: wal\nname: x\n---\ncontent",
    )
    .unwrap();
    std::fs::write(
        vault.join("Note.md"),
        "---\ntype: Person\nname: Note\n---\nbody",
    )
    .unwrap();
    let store = crate::parse_uri_with_options(
        vault.to_str().unwrap(),
        None,
        Some(SnapshotCodecKind::Markdown),
    )
    .unwrap();
    let (graph, _wal) = crate::open_store(&store).unwrap();
    let x = node_by_name(&graph, "x");
    assert_eq!(x.properties.get("name"), Some(&string("x")));
    // The engine re-writes the layout it just loaded: the round-trip
    // keeps the label directory and reopens over it.
    crate::checkpoint(&store, &graph, None).unwrap();
    let (reloaded, _wal) = crate::open_store(&store).unwrap();
    assert_eq!(
        node_by_name(&reloaded, "x").properties.get("name"),
        Some(&string("x"))
    );
}

#[test]
fn bootstrap_loads_an_external_vault() {
    let dir = tempfile::tempdir().unwrap();
    write_external_vault(dir.path());
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    let (graph, _wal) = crate::open_store(&store).unwrap();

    // 4 authored nodes + the root README under :Vault + the stub for
    // the unresolved [[Place/Nowhere]] (ADR-0406 §2, §4).
    let (nodes, rels) = signature(&graph);
    assert_eq!(nodes.len(), 6, "{nodes:?}");
    assert!(
        nodes.contains_key("Vault/README"),
        "root file under the implicit label"
    );
    assert!(nodes.contains_key("Place/Nowhere"), "stub materialized");
    let mut sorted = rels.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![
            ("Person/Ada".into(), "KNOWS".into(), "Person/Grace".into()),
            ("Person/Ada".into(), "KNOWS".into(), "Place/Ithaca".into()),
            ("Person/Ada".into(), "KNOWS".into(), "Place/Nowhere".into()),
        ],
        "map-form props decode; the unresolved link reaches its stub"
    );
    // `records` is not a rel type (no bare-link evidence): a property.
    let grace = node_by_name(&graph, "Grace");
    assert!(grace.properties.contains_key("records"));
    // The body became the body property; the `title` label inferred its
    // key from the 1:1 property (no `name` on Book).
    let ada = node_by_name(&graph, "Ada");
    assert_eq!(
        ada.properties.get("body").and_then(Value::as_string),
        Some("The wanderer.")
    );
}

#[test]
fn a_label_directory_folding_to_the_reserved_root_label_is_refused() {
    // The codec's one identity relation is the case fold: a `vault/` or
    // `VAULT/` directory collides with the root files' implicit
    // `:Vault` namespace exactly like an exact-case `Vault/` directory,
    // judged once at the label derivation site — never re-judged
    // downstream with a second, exact-`==` spelling.
    for dir_name in ["Vault", "vault", "VAULT"] {
        let dir = tempfile::tempdir().unwrap();
        let label_dir = dir.path().join(dir_name);
        std::fs::create_dir_all(&label_dir).unwrap();
        std::fs::write(
            label_dir.join("note.md"),
            format!("---\ntype: {dir_name}\nname: note\n---\n"),
        )
        .unwrap();
        let store = crate::parse_uri_with_options(
            dir.path().to_str().unwrap(),
            None,
            Some(SnapshotCodecKind::Markdown),
        )
        .unwrap();
        let err = match crate::open_store(&store) {
            Err(err) => err,
            Ok(_) => panic!("label directory '{dir_name}' must collide with the root label"),
        };
        assert!(
            err.to_string()
                .contains("collides with the reserved implicit root label"),
            "{dir_name}: {err}"
        );
    }
}

#[test]
fn a_sidecar_rel_id_at_or_above_the_recorded_next_id_is_refused() {
    // The stable-id contract's rel half (ADR-0406 §5) carries
    // `restore_ids`' discipline: a sidecar recording a relationship id
    // at or above the seeded next id would hand a fresh relationship an
    // id a restored one already owns, so the load fails closed with the
    // corrupt-sidecar error instead of silently colliding.
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    crate::checkpoint(&store, &person_graph(), None).unwrap();
    let mut manifest = read_manifest(&store).unwrap().unwrap();
    let seed = manifest.next_rel_id;
    assert!(
        !manifest.rel_ids.is_empty(),
        "the checkpoint recorded the stable rel ids"
    );
    for id in manifest.rel_ids.values_mut() {
        *id = seed;
    }
    write_manifest(&*store.vfs(), &manifest).unwrap();
    let err = crate::open_store(&store)
        .err()
        .expect("a sidecar rel id at the recorded next id must not load");
    assert!(err.to_string().contains("sidecar relationship id"), "{err}");
}

#[test]
fn bootstrap_rejects_undeclared_map_links() {
    let dir = tempfile::tempdir().unwrap();
    let person = dir.path().join("Ledger");
    std::fs::create_dir_all(&person).unwrap();
    std::fs::write(
        person.join("A.md"),
        "---\ntype: Ledger\nname: A\nLEDGER:\n  - target: \"[[Ledger/B]]\"\n    amount: 3\n---\n",
    )
    .unwrap();
    std::fs::write(person.join("B.md"), "---\ntype: Ledger\nname: B\n---\n").unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    let err = crate::open_store(&store)
        .err()
        .expect("undeclared map links must fail");
    assert!(
        err.to_string().contains("LEDGER"),
        "error names the ambiguous key: {err}"
    );
}

#[test]
fn external_edits_land_with_an_empty_wal() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    crate::checkpoint(&store, &person_graph(), None).unwrap();

    // Edit a file in "Obsidian" while the engine is closed.
    let ada_path = dir.path().join("Person/Ada.md");
    let edited = std::fs::read_to_string(&ada_path)
        .unwrap()
        .replace("age: 36", "age: 37");
    std::fs::write(&ada_path, edited).unwrap();

    let (graph, _wal) = crate::open_store(&store).unwrap();
    let ada = node_by_name(&graph, "Ada");
    assert_eq!(
        ada.properties.get("age"),
        Some(&Value::Int(37)),
        "closed-engine vault edits are picked up at open"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_vault_entry_is_refused_before_read_or_write() {
    use std::os::unix::fs::symlink;
    // A symlinked label directory would redirect the store's read,
    // write, and delete surface outside the vault root: the walk
    // refuses it before any of the three can reach the target.
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    write_external_vault(&vault);
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(
        elsewhere.join("Stowaway.md"),
        "---\ntype: Place\nname: Stowaway\n---\n",
    )
    .unwrap();
    symlink(&elsewhere, vault.join("Place2")).unwrap();
    let store = crate::parse_uri_with_options(
        vault.to_str().unwrap(),
        None,
        Some(SnapshotCodecKind::Markdown),
    )
    .unwrap();
    let err = match crate::open_store(&store) {
        Err(e) => e,
        Ok(_) => panic!("a symlinked vault entry must fail closed at open"),
    };
    assert!(err.to_string().contains("symlink"), "{err}");
    // Nothing was written through or replaced at the symlink: it is
    // still a symlink, and the target file is untouched.
    assert!(std::fs::symlink_metadata(vault.join("Place2"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(elsewhere.join("Stowaway.md").is_file());
}

#[test]
fn a_case_only_duplicate_key_loads_the_first_spelling_and_folds_the_loser() {
    // On the case-sensitive MemoryVfs — a host filesystem can be
    // case-insensitive (macOS APFS), where the two files cannot even
    // coexist.
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    let vfs = store.vfs();
    vfs.create_dir_all("Person").unwrap();
    atomic_write(
        &*vfs,
        "Person/Ada.md",
        b"---\ntype: Person\nname: Ada\n---\n\nThe first spelling wins.\n",
    )
    .unwrap();
    atomic_write(
        &*vfs,
        "Person/ada.md",
        b"---\ntype: Person\nname: ada\n---\n\nThe loser is deleted at checkpoint.\n",
    )
    .unwrap();
    let (graph, _) = crate::open_store(&store).unwrap();
    // One node — the first spelling in path order; the later row's
    // content is dropped, with the fold recorded as the warning.
    let (nodes, _) = signature(&graph);
    assert_eq!(nodes.len(), 1, "{nodes:?}");
    assert!(nodes.contains_key("Person/Ada"));
    crate::checkpoint(&store, &graph, None).unwrap();
    assert!(
        !vfs.exists("Person/ada.md").unwrap(),
        "the folded loser is deleted at checkpoint"
    );
    let manifest = read_manifest(&store).unwrap().unwrap();
    assert!(
        manifest.learned.folded_keys.contains("Person/ada.md"),
        "the fold is recorded in the learned schema"
    );
}

#[test]
fn set_semantics_dedupes_identical_relationships() {
    let graph = Graph::default();
    graph
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec!["Person".into()]),
            properties: props(&[("name", string("Ada"))]),
        })
        .unwrap();
    graph
        .load_checkpoint_node(Node {
            id: 2,
            labels: Arc::new(vec!["Person".into()]),
            properties: props(&[("name", string("Grace"))]),
        })
        .unwrap();
    for id in 1..=2 {
        graph
            .load_checkpoint_rel(Relationship {
                id,
                start: 1,
                end: 2,
                rel_type: "KNOWS".into(),
                properties: props(&[("since", int(1833))]),
            })
            .unwrap();
    }
    graph.set_next_node_id(3);
    graph.set_next_rel_id(3);
    graph.set_commit_ts(1);

    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    crate::checkpoint(&store, &graph, None).unwrap();
    let (loaded, _wal) = crate::open_store(&store).unwrap();
    let (_, rels) = signature(&loaded);
    assert_eq!(rels.len(), 1, "exact duplicates collapse: {rels:?}");
}
