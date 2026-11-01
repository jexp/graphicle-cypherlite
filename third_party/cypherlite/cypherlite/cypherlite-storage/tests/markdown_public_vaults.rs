//! Public Obsidian vault validation (ADR-0406 §7), env-gated: set
//! `CYPHERLITE_MARKDOWN_VAULTS=<dir>[,<dir>…]` (cloned vaults, e.g.
//! obsidianmd/obsidian-help, community-archive/obsidian-hub,
//! doctorfree/Obsidian-Books-Vault,
//! obsidian-pkm-vault/awesome-obsidian-vault) and this test opens each
//! as a markdown store, counts the graph, checkpoints, and verifies an
//! idempotent re-checkpoint. Unset, it passes vacuously (the loop below
//! runs zero times) — CI does not clone the vaults.

mod vault_support;

use std::path::PathBuf;

use cypherlite_storage::{checkpoint, open_store, parse_uri_with_options, SnapshotCodecKind};
use vault_support::{copy_dir, vault_signature};

fn vault_dirs() -> Vec<PathBuf> {
    // Unset (or empty) is the intended vacuous pass; a set but non-UTF-8
    // value is a broken setting and fails the test instead of silently
    // skipping the corpus.
    let dirs = match std::env::var("CYPHERLITE_MARKDOWN_VAULTS") {
        Ok(dirs) => dirs,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(std::env::VarError::NotUnicode(value)) => {
            panic!("CYPHERLITE_MARKDOWN_VAULTS is not valid UTF-8: {value:?}")
        }
    };
    dirs.split(',')
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .collect()
}

#[test]
fn public_vaults_load_checkpoint_and_recheckpoint_stably() {
    for dir in vault_dirs() {
        let workspace = tempfile::tempdir().unwrap();
        // Copy the vault: the test checkpoint writes into it, and the
        // original clone must stay pristine.
        let vault = workspace.path().join("vault");
        copy_dir(&dir, &vault);
        let uri = vault.to_str().unwrap();
        let store = parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
        let (graph, _wal) = open_store(&store).unwrap();
        let nodes = graph.all_node_ids_at(u64::MAX).len();
        assert!(nodes > 0, "{}: no nodes loaded", dir.display());
        checkpoint(&store, &graph, None).unwrap();
        let (reloaded, _wal) = open_store(&store).unwrap();
        assert_eq!(
            reloaded.all_node_ids_at(u64::MAX).len(),
            nodes,
            "{}: stable across checkpoint",
            dir.display()
        );
        // Idempotent re-checkpoint: the guarded-file signature is
        // unchanged, asserted whole (`BTreeMap` equality) like the
        // odyssey sibling test.
        let before = vault_signature(&store);
        checkpoint(&store, &reloaded, None).unwrap();
        let after = vault_signature(&store);
        assert_eq!(before, after, "{}: idempotent re-checkpoint", dir.display());
    }
}
