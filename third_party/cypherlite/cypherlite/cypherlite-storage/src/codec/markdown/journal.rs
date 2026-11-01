//! The copy-on-write checkpoint journal (ADR-0406 §5).
//!
//! Before a checkpoint overwrites or deletes the first vault file, the
//! current content of every to-be-touched file is copied under
//! `.cypherlite/checkpoint-journal/<relpath>` and the *intent* — the
//! full new manifest — is written atomically into the journal. An open
//! that finds a journal therefore knows the mismatch it may see is an
//! engine crash, not an external edit, and either rolls forward (the
//! vault already matches the intent: publish it) or back (restore the
//! journaled contents, remove the new files, and let the WAL replay as
//! normal over the old base).

use super::codec_error;
use super::manifest::{
    write_manifest, VaultManifest, JOURNAL_DIR, JOURNAL_INTENT, JOURNAL_PREIMAGE,
};
use super::vault::{
    entry_path, file_name, vault_matches, vault_stats, walk_vault, walk_view_paths, walked_entries,
};
use crate::vfs::{atomic_write, read_to_vec, Vfs};
use crate::StoreTarget;
use cypherlite_core::Result;
use std::collections::{BTreeMap, BTreeSet};

/// Recover a pending journaled checkpoint, if any (ADR-0406 §5).
pub(super) fn recover(target: &StoreTarget) -> Result<()> {
    let vfs = target.vfs();
    if !vfs.exists(JOURNAL_DIR)? {
        return Ok(());
    }
    if vfs.exists(JOURNAL_INTENT)? {
        let bytes = read_to_vec(&*vfs, JOURNAL_INTENT)?;
        let intent: VaultManifest = serde_json::from_slice(&bytes)
            .map_err(|e| codec_error(format!("invalid checkpoint intent: {e}")))?;
        let files = walk_vault(&*vfs)?;
        if vault_matches(&vault_stats(&files), &intent) {
            // Everything landed; publish the intent forward.
            write_manifest(&*vfs, &intent)?;
        } else {
            rollback(&*vfs, &intent)?;
        }
    }
    // A journal without an intent crashed before any vault write: the
    // vault is untouched and the old manifest still governs. The journal
    // leaves through the same removal a successful checkpoint's clear
    // performs — the one journal-removal spelling.
    clear(target)
}

/// Restore the journaled old contents, remove files the intent would
/// have created, and leave the old manifest in charge. A file is
/// "newly created" only if it is absent from the journal's pre-image —
/// the paths that existed before the checkpoint began: a file that
/// predates the checkpoint was journaled (or untouched) and is
/// restored, never deleted. The view class joins the deletion
/// candidates with the same class-diff the apply tail's stale-view
/// prune performs: every view file on disk absent from the pre-image
/// is one the crashed checkpoint created — views are regenerated
/// wholesale, never guarded, so `intent.files` never names them.
fn rollback(vfs: &dyn Vfs, intent: &VaultManifest) -> Result<()> {
    let mut journaled: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    collect_journaled(vfs, JOURNAL_DIR, "", &mut journaled)?;
    let preimage = read_preimage(vfs)?;
    for (relpath, old) in &journaled {
        atomic_write(vfs, relpath, old)?;
    }
    // One deletion predicate for both candidate classes — the intent's
    // guarded files (which may name never-written paths, hence the
    // exists check) and the on-disk view files the walk just proved
    // exist: a path the crashed checkpoint created (absent from the
    // journal and the pre-image) is removed; a file that predates the
    // checkpoint is restored above, never deleted.
    let remove_created = |path: String| -> Result<bool> {
        Ok(!journaled.contains_key(&path) && !preimage.contains(&path) && vfs.exists(&path)?)
    };
    for path in intent.files.keys().cloned().chain(walk_view_paths(vfs)?) {
        if remove_created(path.clone())? {
            vfs.remove_file(&path)
                .map_err(|e| codec_error(format!("rolling back new file {path}: {e}")))?;
        }
    }
    Ok(())
}

/// The staged pre-image path set. A journal with an intent but no
/// pre-image is internally inconsistent (the intent is written last)
/// and fails closed rather than guessing which files are new.
fn read_preimage(vfs: &dyn Vfs) -> Result<BTreeSet<String>> {
    let bytes = read_to_vec(vfs, JOURNAL_PREIMAGE)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| codec_error(format!("invalid checkpoint pre-image: {e}")))
}

/// Collect every journaled file, keyed by its vault-relative path. The
/// recursion carries the journal-relative directory alongside the
/// on-disk one, so the vault path is joined from the entry name —
/// never re-derived by stripping the journal prefix off the walked
/// path. The traversal itself is the vault walk's one entry
/// enumeration (`walked_entries`: sorted, hidden skipped, symlinks
/// refused) — the journal directory is engine-authored, so its entries
/// are all restorable vault content.
fn collect_journaled(
    vfs: &dyn Vfs,
    dir: &str,
    rel_dir: &str,
    out: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for (full, meta) in walked_entries(vfs, dir)? {
        let rel = entry_path(rel_dir, file_name(&full));
        if meta.is_dir() {
            collect_journaled(vfs, &full, &rel, out)?;
            continue;
        }
        // The journal's own metadata never restores into the vault —
        // rolling either back would write an engine-owned file into the
        // user's vault root.
        if full == JOURNAL_INTENT || full == JOURNAL_PREIMAGE {
            continue;
        }
        out.insert(rel, read_to_vec(vfs, &full)?);
    }
    Ok(())
}

/// Stage the journal: copy the current content of every existing file
/// the checkpoint will overwrite or delete, then write the intent
/// atomically (its presence is the journal's commit point).
pub(super) fn stage(
    target: &StoreTarget,
    existing: &BTreeMap<String, Vec<u8>>,
    manifest: &VaultManifest,
) -> Result<()> {
    let vfs = target.vfs();
    // atomic_write creates each written path's parent chain, so the
    // journal directory materializes with the first staged file (the
    // pre-image at the latest) — no separate create_dir_all pass.
    for (path, old) in existing {
        let touched = match manifest.files.get(path) {
            Some(stat) => stat.differs_from(old),
            None => true, // about to be deleted
        };
        if !touched {
            continue;
        }
        let journal_path = format!("{JOURNAL_DIR}/{path}");
        atomic_write(&*vfs, &journal_path, old)?;
    }
    // The pre-image precedes the intent (the commit point): every
    // existing path at the moment the checkpoint began.
    let preimage: Vec<&String> = existing.keys().collect();
    let preimage = serde_json::to_vec(&preimage)
        .map_err(|e| codec_error(format!("serializing checkpoint pre-image: {e}")))?;
    atomic_write(&*vfs, JOURNAL_PREIMAGE, &preimage)?;
    let intent = serde_json::to_vec(manifest)
        .map_err(|e| codec_error(format!("serializing checkpoint intent: {e}")))?;
    atomic_write(&*vfs, JOURNAL_INTENT, &intent)?;
    Ok(())
}

/// Remove the journal after a successful checkpoint.
pub(super) fn clear(target: &StoreTarget) -> Result<()> {
    let vfs = target.vfs();
    if vfs.exists(JOURNAL_DIR)? {
        vfs.remove_dir_all(JOURNAL_DIR)
            .map_err(|e| codec_error(format!("removing checkpoint journal: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
