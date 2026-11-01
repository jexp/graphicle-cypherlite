//! The one toolkit the markdown-vault integration tests share (the
//! Odyssey e2e and the public-vault corpus): the fixture locator, the
//! guarded-file vault signature (over the shared test-support VFS
//! walk), and the fixture-copy helpers. Each including test crate is
//! its own crate and uses a subset, so unused items would warn — allow
//! it module-wide.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cypherlite_markdown::is_reserved_view_file_name;
use cypherlite_storage::{StoreTarget, Vfs};
use cypherlite_test_support::vfs_walk::{virt_join, walk_vfs};

/// The vendored Odyssey vault fixture (ADR-0406 §7).
pub fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/odyssey-vault")
}

/// Every guarded `.md` file of the vault mapped to its byte length —
/// the idempotent-re-checkpoint signature. The reserved-view rule is
/// the grammar crate's one predicate, consulted like every sibling
/// site (whole file names at any depth, through the case fold — a
/// suffix check would also drop ordinary notes like `Blog.md`).
pub fn vault_signature(store: &StoreTarget) -> BTreeMap<String, u64> {
    let vfs = store.vfs();
    walk_vfs(&*vfs, "")
        .into_iter()
        .filter(|path| {
            let name = path.rsplit_once('/').map(|(_, name)| name).unwrap_or(path);
            path.ends_with(".md") && !is_reserved_view_file_name(name)
        })
        .map(|path| {
            let bytes = cypherlite_storage::read_to_vec(&*vfs, &path).unwrap();
            (path, bytes.len() as u64)
        })
        .collect()
}

/// The one recursive fixture traversal with one entry policy, for both
/// destinations: dot entries are skipped — `.git` and friends, and any
/// `.cypherlite/` sidecar an earlier run left in a source vault — so a
/// copied fixture is the vault's own files only, wherever it lands.
/// `sink` receives each file's store-relative path (joined through the
/// one store-relative join, `cypherlite_test_support::vfs_walk::virt_join`)
/// and bytes.
fn copy_tree(from: &Path, rel: &str, sink: &dyn Fn(&str, &[u8])) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let rel = virt_join(rel, &name);
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &rel, sink);
        } else {
            let bytes = std::fs::read(entry.path()).unwrap();
            sink(&rel, &bytes);
        }
    }
}

/// Copy a host directory tree to another host directory.
pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    copy_tree(from, "", &|rel, bytes| {
        let target = to.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&target, bytes).unwrap();
    });
}

/// Copy a host directory tree into a VFS at its root.
pub fn copy_dir_into_vfs(from: &Path, vfs: &dyn Vfs) {
    copy_tree(from, "", &|rel, bytes| {
        cypherlite_storage::atomic_write(vfs, rel, bytes).unwrap();
    });
}
