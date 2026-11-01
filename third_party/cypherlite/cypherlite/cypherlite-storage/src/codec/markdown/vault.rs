//! The vault filesystem walk (ADR-0406 §4): the one traversal that
//! classifies vault entries, refuses symlinked entries, and derives the
//! guard stats — plus the shared `vault_matches` predicate and its
//! stat-first form.
//!
//! Extracted from `mod.rs` (the no-1k-line rule, LABS-311).

use std::collections::BTreeMap;

use cypherlite_markdown as md;

use super::codec_error;
use super::manifest::{FileStat, VaultManifest};
use crate::vfs::{read_to_vec, Vfs, VfsMeta};
use cypherlite_core::Result;

/// One walked `.md` entry: its path, the stat the walk's own metadata
/// fetch captured, and whether it lives at the vault root (loaded under
/// the implicit `:Vault` label, ADR-0406 §2). The bytes-free shape —
/// [`walk_vault`] reads over it.
pub(super) struct WalkedPath {
    pub(super) path: String,
    /// The entry's stat as the walk saw it — length, mtime, shape —
    /// carried so the guard's stat-first comparison (ADR-0406 §5)
    /// consults the walk's own fetch instead of a second stat pass.
    pub(super) meta: VfsMeta,
    pub(super) root_file: bool,
}

impl WalkedPath {
    /// The file's name stem: the on-disk path minus `.md` (nested key
    /// paths keep their `/` separators) — the *filename* fact, distinct
    /// from the `Label/name` vault key load derives from it.
    pub(super) fn stem(&self) -> &str {
        self.path
            .strip_suffix(".md")
            .expect("walked vault paths end in .md")
    }
}

/// One `.md` file in the vault: the walked entry (path, stat, root
/// classification — the walked model's one field list) plus its bytes.
pub(super) struct VaultFile {
    pub(super) walked: WalkedPath,
    pub(super) bytes: Vec<u8>,
}

/// Every guarded `.md` entry, root files flagged: the entry point over
/// the one recursive walk below (which derives root classification at
/// its push site), plus the global path sort.
pub(super) fn walk_paths(vfs: &dyn Vfs) -> Result<Vec<WalkedPath>> {
    let mut paths = Vec::new();
    walk_dir(vfs, "", &mut paths)?;
    paths.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(paths)
}

/// Read every walked entry. The one read pass — no other vault walk
/// fetches bytes.
pub(super) fn walk_vault(vfs: &dyn Vfs) -> Result<Vec<VaultFile>> {
    let mut files = Vec::new();
    for walked in walk_paths(vfs)? {
        let bytes = read_to_vec(vfs, &walked.path)?;
        files.push(VaultFile { walked, bytes });
    }
    Ok(files)
}

/// One directory entry as a store path: the dir prefix joined onto the
/// bare name `list_dir` returns (the one path-joining policy, shared
/// with the journal walk).
pub(super) fn entry_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The path's final segment — the whole path at the vault root.
pub(super) fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The one entry enumeration both traversals consume: names sorted for
/// deterministic order, hidden entries skipped, symlinked entries
/// refused — yielding each surviving entry with its stat, so every
/// walker keeps only its own tail (node classification or directory
/// pruning) and the classification conditionals exist once.
pub(super) fn walked_entries(vfs: &dyn Vfs, dir: &str) -> Result<Vec<(String, VfsMeta)>> {
    let mut names = vfs.list_dir(dir)?;
    names.sort();
    let mut entries = Vec::new();
    for name in names {
        let entry = entry_path(dir, &name);
        if is_hidden(&entry) {
            continue;
        }
        let meta = refuse_symlink(vfs, &entry)?;
        entries.push((entry, meta));
    }
    Ok(entries)
}

/// Every generated-view file present on disk (the reserved names at
/// any depth, over the same one entry enumeration) — the view class
/// the checkpoint's deletion phase diffs against the views it
/// rendered, so a stale view is deleted by class membership, never by
/// re-deriving label directories from the guarded manifest.
pub(super) fn walk_view_paths(vfs: &dyn Vfs) -> Result<Vec<String>> {
    let mut views = Vec::new();
    walk_views(vfs, "", &mut views)?;
    Ok(views)
}

fn walk_views(vfs: &dyn Vfs, dir: &str, views: &mut Vec<String>) -> Result<()> {
    for (entry, meta) in walked_entries(vfs, dir)? {
        if meta.is_dir() {
            walk_views(vfs, &entry, views)?;
            continue;
        }
        if md::is_reserved_view_file_name(file_name(&entry)) {
            views.push(entry);
        }
    }
    Ok(())
}

/// The one recursive traversal: every non-hidden, non-reserved `.md`
/// is a node file; the only root/label difference — whether the file
/// loads under the implicit `:Vault` label (ADR-0406 §2) — is derived
/// where it is pushed, never threaded as a mode flag.
fn walk_dir(vfs: &dyn Vfs, dir: &str, paths: &mut Vec<WalkedPath>) -> Result<()> {
    for (entry, meta) in walked_entries(vfs, dir)? {
        if meta.is_dir() {
            walk_dir(vfs, &entry, paths)?;
            continue;
        }
        // Reserved view filenames are skipped at any hierarchy level —
        // judged through the case fold, so `Index.md` is the view the
        // same way `index.md` is.
        if md::is_reserved_view_file_name(file_name(&entry)) || !entry.ends_with(".md") {
            continue;
        }
        paths.push(WalkedPath {
            path: entry,
            meta,
            root_file: dir.is_empty(),
        });
    }
    Ok(())
}

/// A vault is a hand-authored folder of plain files and directories; a
/// symlinked entry would redirect the store's read, write, and delete
/// surface outside the vault root, so it is refused before any of the
/// three can reach it.
fn refuse_symlink(vfs: &dyn Vfs, entry: &str) -> Result<VfsMeta> {
    let meta = vfs.metadata(entry)?;
    if meta.is_symlink() {
        return Err(codec_error(format!(
            "vault entry {entry} is a symlink; \
             a markdown vault refuses symlinked entries"
        )));
    }
    // A FIFO, socket, or device entry is not vault content either —
    // refused at the walk with its own shape named, never deferred to
    // a later open's regular-file check.
    if meta.is_other() {
        return Err(codec_error(format!(
            "vault entry {entry} is not a plain file or directory; \
             a markdown vault holds only those"
        )));
    }
    Ok(meta)
}

fn is_hidden(path: &str) -> bool {
    file_name(path).starts_with('.')
}

pub(super) fn crc(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// ADR-0406 §5's one predicate — the vault's guarded files match the
/// manifest's expectation — consulted by the checkpoint external-write
/// guard and journal recovery, so both roles provably share one
/// definition. Every consumer lives inside the `codec::markdown`
/// subtree, so the grade is `pub(super)`, matching the sibling walk and
/// stat helpers.
pub(super) fn vault_matches(stats: &BTreeMap<String, FileStat>, manifest: &VaultManifest) -> bool {
    stats == &manifest.files
}

/// The stat-first form of the same predicate (ADR-0406 §5): a file
/// whose recorded length and mtime both match is unchanged without
/// reading a byte; only when the VFS reports no mtime (or the recorded
/// one is absent) does the comparison fall back to the crc.
pub(super) fn vault_matches_stat_first(vfs: &dyn Vfs, manifest: &VaultManifest) -> Result<bool> {
    // The file SET must match first: a file added to the vault shifts
    // every path-order node id, so replaying id-based redo over it is
    // the exact corruption ADR-0406 §5 refuses — the paths-only walk
    // detects it without reading a byte. Both sides are BTreeMap keys,
    // so the sorted iterators compare directly.
    let walked: BTreeMap<String, VfsMeta> = walk_paths(vfs)?
        .into_iter()
        .map(|walked| (walked.path, walked.meta))
        .collect();
    if walked.keys().ne(manifest.files.keys()) {
        return Ok(false);
    }
    for (path, stat) in &manifest.files {
        // The walk's own stat is the single classification source: a
        // file missing from the vault already showed up as the set
        // mismatch above (a legitimate "changed", never the guard's
        // error), and any other I/O failure is the walk's own error.
        let meta = walked
            .get(path)
            .expect("the set check proved every manifest path walked");
        if meta.len != stat.len {
            return Ok(false);
        }
        // The mtime is advisory, never a verdict: a mismatch — a fresh
        // write of identical bytes — falls through to the crc, the one
        // content-based definition of "changed" (FileStat::eq), shared
        // with the backends that report no mtime at all.
        if let (Some(recorded), Some(current)) = (stat.mtime, meta.mtime) {
            if recorded == current {
                continue;
            }
        }
        // No mtime to consult — or a mismatch to prove: the crc.
        let bytes = read_to_vec(vfs, path)?;
        if stat.differs_from(&bytes) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Reserved views are excluded (regenerated wholesale, never guarded).
pub(super) fn vault_stats(files: &[VaultFile]) -> BTreeMap<String, FileStat> {
    files
        .iter()
        .map(|file| {
            (
                file.walked.path.clone(),
                FileStat {
                    mtime: file.walked.meta.mtime,
                    ..FileStat::of(&file.bytes)
                },
            )
        })
        .collect()
}

/// Remove label directories that no longer hold node files or views.
/// Only empty directories go — a directory with attachments
/// (non-markdown files) is vault content the engine does not own.
pub(super) fn prune_empty_dirs(vfs: &dyn Vfs) -> Result<()> {
    prune_dir(vfs, "")
}

/// The pruning traversal, post-order: each directory is considered only
/// after its children, so a nested directory emptied by its own pruning
/// goes too — deepest-first for free, with no collected-then-sorted
/// directory list. The emptiness proof is a fresh listing: hidden
/// entries are skipped by the enumeration but are vault content that
/// keeps their directory alive.
fn prune_dir(vfs: &dyn Vfs, dir: &str) -> Result<()> {
    for (entry, meta) in walked_entries(vfs, dir)? {
        if meta.is_dir() {
            prune_dir(vfs, &entry)?;
        }
    }
    // The vault root itself is never a removal candidate.
    if !dir.is_empty() && vfs.list_dir(dir)?.is_empty() {
        vfs.remove_dir_all(dir)
            .map_err(|e| codec_error(format!("removing empty directory {dir}: {e}")))?;
    }
    Ok(())
}
