//! Journal-recovery tests (ADR-0406 §5/§7): the crash-window contract —
//! roll forward when everything landed, roll back (node files and the
//! view class alike) when it did not.

use super::super::manifest::{read_manifest, FileStat};
use super::super::tests::{person_graph, signature};
use super::super::vault::crc;
use super::*;
use crate::codec::SnapshotCodecKind;

/// The crashed-checkpoint staging both rollback tests share: the
/// journal's copy of the old Ada content, the pre-image listing every
/// path that existed before the checkpoint began (the node files the
/// walk sees plus the on-disk views, exactly as the real [`stage`]
/// records them), and the intent naming the caller's `intent_files` —
/// a files map the disk will not match, so recovery must roll back.
fn stage_crashed_checkpoint(
    vfs: &dyn Vfs,
    ada_before: &[u8],
    old_stats: &BTreeMap<String, FileStat>,
    intent_files: BTreeMap<String, FileStat>,
) {
    atomic_write(vfs, &format!("{JOURNAL_DIR}/Person/Ada.md"), ada_before).unwrap();
    let mut preimage: Vec<String> = old_stats.keys().cloned().collect();
    preimage.extend(walk_view_paths(vfs).unwrap());
    preimage.sort();
    atomic_write(
        vfs,
        JOURNAL_PREIMAGE,
        &serde_json::to_vec(&preimage).unwrap(),
    )
    .unwrap();
    let intent = VaultManifest {
        codec: Some(SnapshotCodecKind::Markdown),
        files: intent_files,
        ..VaultManifest::default()
    };
    atomic_write(vfs, JOURNAL_INTENT, &serde_json::to_vec(&intent).unwrap()).unwrap();
}

#[test]
fn journal_recovery_rolls_back_a_partial_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    crate::checkpoint(&store, &person_graph(), None).unwrap();
    let vfs = store.vfs();
    let ada_before = read_to_vec(&*vfs, "Person/Ada.md").unwrap();

    // Simulate a crash mid-checkpoint: journal staged with the old
    // content, intent naming a new files map, one file clobbered.
    let old_stats = vault_stats(&walk_vault(&*vfs).unwrap());
    let mut intent_files = old_stats.clone();
    // The intent names the Ada file the crashed checkpoint *meant* to
    // write; the disk holds a half-updated "new" that matches neither
    // the intent nor the old state, so recovery must roll back.
    intent_files.insert(
        "Person/Ada.md".to_string(),
        FileStat {
            len: 8,
            crc: crc(b"intended"),
            mtime: None,
        },
    );
    stage_crashed_checkpoint(&*vfs, &ada_before, &old_stats, intent_files);
    atomic_write(&*vfs, "Person/Ada.md", b"new").unwrap();

    recover(&store).unwrap();
    assert_eq!(
        read_to_vec(&*vfs, "Person/Ada.md").unwrap(),
        ada_before,
        "the journaled old content is restored"
    );
    assert!(!vfs.exists(JOURNAL_DIR).unwrap());
    // The old manifest still governs: a plain open replays nothing.
    let (graph, _wal) = crate::open_store(&store).unwrap();
    assert_eq!(signature(&graph).0.len(), 3);
}

#[test]
fn journal_rollback_deletes_a_newly_created_view_file() {
    // A crashed checkpoint that rendered a view for a label directory
    // the pre-image never held: the view class joins the deletion
    // candidates — `intent.files` never names views (they are
    // regenerated wholesale, never guarded), so without the class walk
    // the orphaned view would survive the rollback.
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    crate::checkpoint(&store, &person_graph(), None).unwrap();
    let vfs = store.vfs();
    let ada_before = read_to_vec(&*vfs, "Person/Ada.md").unwrap();

    // The crashed run had already written a new Place label's node
    // file and its directory view; the journal holds the old Ada
    // content, the pre-image predates the Place label, and the intent
    // names a files map the disk does not match — recovery rolls back.
    let old_stats = vault_stats(&walk_vault(&*vfs).unwrap());
    let mut intent_files = old_stats.clone();
    intent_files.insert(
        "Place/Ithaca.md".to_string(),
        FileStat {
            len: 8,
            crc: crc(b"intended"),
            mtime: None,
        },
    );
    stage_crashed_checkpoint(&*vfs, &ada_before, &old_stats, intent_files);
    atomic_write(
        &*vfs,
        "Place/Ithaca.md",
        b"---\ntype: Place\nname: Ithaca\n---\n",
    )
    .unwrap();
    atomic_write(&*vfs, "Place/index.md", b"# Place\n\n").unwrap();
    atomic_write(&*vfs, "Person/Ada.md", b"new").unwrap();

    recover(&store).unwrap();
    assert_eq!(
        read_to_vec(&*vfs, "Person/Ada.md").unwrap(),
        ada_before,
        "the journaled old content is restored"
    );
    assert!(
        !vfs.exists("Place/Ithaca.md").unwrap(),
        "the new node file is deleted"
    );
    assert!(
        !vfs.exists("Place/index.md").unwrap(),
        "the new view file is deleted with the label it was rendered for"
    );
    assert!(
        vfs.exists("index.md").unwrap(),
        "pre-existing views survive"
    );
    assert!(!vfs.exists(JOURNAL_DIR).unwrap());
}

#[test]
fn journal_recovery_publishes_forward_when_everything_landed() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store =
        crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    crate::checkpoint(&store, &person_graph(), None).unwrap();
    let vfs = store.vfs();

    // Everything the intent names is on disk (stats match); the journal
    // only never got cleaned up.
    let stats = vault_stats(&walk_vault(&*vfs).unwrap());
    atomic_write(
        &*vfs,
        JOURNAL_INTENT,
        &serde_json::to_vec(&VaultManifest {
            codec: Some(SnapshotCodecKind::Markdown),
            checkpoint_lsn: 5,
            files: stats,
            ..VaultManifest::default()
        })
        .unwrap(),
    )
    .unwrap();
    vfs.create_dir_all(JOURNAL_DIR).unwrap();

    recover(&store).unwrap();
    assert!(!vfs.exists(JOURNAL_DIR).unwrap());
    let manifest = read_manifest(&store).unwrap().unwrap();
    assert_eq!(manifest.checkpoint_lsn, 5, "the intent was published");
}
