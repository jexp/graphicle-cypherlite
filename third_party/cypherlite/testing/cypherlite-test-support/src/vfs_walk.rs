//! The one recursive traversal over a store [`Vfs`]: every file beneath
//! a directory, as `/`-joined store-relative paths, so Native
//! directories and `.cl` bundles enumerate the same way. The
//! golden-store suite and the markdown-vault suites consume it — the
//! traversal policy (panics on any VFS error, so a broken fixture fails
//! the test at the point of the walk) is spelled once here.

use cypherlite_storage::Vfs;

/// The store-relative path join: an empty prefix yields the bare name,
/// otherwise `/`-joined. The same policy as `cypherlite-storage`'s
/// crate-internal `vfs::virt_join` — test-support cannot link a
/// `pub(crate)` item, so the policy's one spelling in this crate lives
/// here and every consumer of the join (the walk below and the storage
/// integration fixtures' tree copier) consults it. When storage exports
/// `virt_join` publicly, this helper deletes in favor of the engine's
/// own.
pub fn virt_join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else if name.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Every file under `dir` in the VFS, as store-relative paths.
///
/// # Panics
///
/// Panics on any `list_dir`/`metadata` error — a store the walk cannot
/// enumerate is a broken fixture, never an empty answer.
pub fn walk_vfs(vfs: &dyn Vfs, dir: &str) -> Vec<String> {
    let mut out = Vec::new();
    for name in vfs.list_dir(dir).unwrap() {
        let path = virt_join(dir, &name);
        if vfs.metadata(&path).unwrap().is_dir() {
            out.extend(walk_vfs(vfs, &path));
        } else {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_storage::{atomic_write, MemoryVfs};

    #[test]
    fn walk_vfs_lists_every_file_as_store_relative_paths() {
        let vfs = MemoryVfs::new();
        atomic_write(&vfs, "wal/segment.log", b"1").unwrap();
        atomic_write(&vfs, "top.md", b"22").unwrap();
        let mut paths = walk_vfs(&vfs, "");
        paths.sort();
        assert_eq!(
            paths,
            vec!["top.md".to_string(), "wal/segment.log".to_string()]
        );
        assert_eq!(walk_vfs(&vfs, "wal"), vec!["wal/segment.log".to_string()]);
    }

    #[test]
    fn virt_join_spells_the_store_relative_join_policy() {
        // The engine's `vfs::virt_join` policy, spelled once for this
        // crate: empty prefix yields the bare name (the store root has
        // no leading `/`), and an empty name never truncates the
        // prefix.
        assert_eq!(virt_join("", "top.md"), "top.md");
        assert_eq!(virt_join("wal", "segment.log"), "wal/segment.log");
        assert_eq!(virt_join("wal", ""), "wal");
    }
}
