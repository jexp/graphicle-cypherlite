//! `reset_uri` deletes only the store its URI names. A symlink, file
//! reparse point or junction alias is refused with the alias and its target
//! untouched, and a store replaced after its identity was taken is refused
//! rather than deleted.

use super::*;
#[cfg(windows)]
use cypherlite_core::file_identity;

#[cfg(unix)]
#[test]
fn reset_bundle_alias_never_unlinks_unrelated_or_valid_target() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let alias = dir.path().join("db.cl");
    let unrelated = dir.path().join("unrelated.bin");
    std::fs::write(&unrelated, b"not a bundle").unwrap();
    symlink(&unrelated, &alias).unwrap();
    let err = reset_uri(alias.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"not a bundle");
    assert_eq!(std::fs::read_link(&alias).unwrap(), unrelated);

    std::fs::remove_file(&alias).unwrap();
    let bundle = dir.path().join("bundle.cl");
    drop(BundleVfs::open_or_create(&bundle).unwrap());
    let before = std::fs::read(&bundle).unwrap();
    symlink(&bundle, &alias).unwrap();
    // Opening an alias remains supported; reset through it is destructive.
    drop(BundleVfs::open_or_create(&alias).unwrap());
    let err = reset_uri(alias.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read(&bundle).unwrap(), before);
    assert_eq!(std::fs::read_link(&alias).unwrap(), bundle);
    std::fs::remove_file(&alias).unwrap();
    symlink(dir.path().join("missing"), &alias).unwrap();
    let err = reset_uri(alias.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(std::fs::symlink_metadata(&alias).is_ok());
    std::fs::remove_file(&alias).unwrap();
    reset_uri(bundle.to_str().unwrap(), None).unwrap();
    assert!(!bundle.exists());
}

#[cfg(windows)]
#[test]
fn reset_bundle_file_reparse_alias_preserves_unrelated_bytes() {
    use std::os::windows::fs::symlink_file;
    let dir = tempfile::tempdir().unwrap();
    let unrelated = dir.path().join("unrelated.bin");
    let alias = dir.path().join("db.cl");
    std::fs::write(&unrelated, b"not bundle magic").unwrap();
    if let Err(err) = symlink_file(&unrelated, &alias) {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            return; // Windows developer mode / symlink privilege unavailable.
        }
        panic!("could not create file symlink: {err}");
    }
    let err = reset_uri(alias.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"not bundle magic");
    assert!(std::fs::symlink_metadata(&alias).is_ok());
}

#[cfg(unix)]
#[test]
fn reset_native_alias_preserves_target_manifest_wal_and_sentinel() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("physical");
    let alias = dir.path().join("alias");
    let unrelated = dir.path().join("unrelated");
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::write(unrelated.join("sentinel"), b"not a database").unwrap();
    symlink(&unrelated, &alias).unwrap();
    let err = reset_uri(alias.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read_link(&alias).unwrap(), unrelated);
    assert_eq!(
        std::fs::read(unrelated.join("sentinel")).unwrap(),
        b"not a database"
    );
    assert!(!unrelated.join(LOCK_MARKER).exists());
    std::fs::remove_file(&alias).unwrap();
    let store = parse_uri(root.to_str().unwrap()).unwrap();
    let (graph, _) = person_ada();
    checkpoint(&store, &graph, None).unwrap();
    let (vfs, prefix) = store.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    wal.append_commit(2, &[]).unwrap();
    drop((wal, store));
    std::fs::write(root.join("sentinel"), b"keep this").unwrap();
    let manifest = std::fs::read(root.join("manifest.json")).unwrap();
    let segment = std::fs::read(root.join("wal/00000001.log")).unwrap();
    symlink(&root, &alias).unwrap();
    drop(parse_uri(alias.to_str().unwrap()).unwrap());
    let err = reset_uri(alias.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read_link(&alias).unwrap(), root);
    assert_eq!(std::fs::read(root.join("sentinel")).unwrap(), b"keep this");
    assert_eq!(std::fs::read(root.join("manifest.json")).unwrap(), manifest);
    assert_eq!(
        std::fs::read(root.join("wal/00000001.log")).unwrap(),
        segment
    );
    assert!(!root.join(".reset-incomplete").exists());
    std::fs::remove_file(&alias).unwrap();
    reset_uri(root.to_str().unwrap(), None).unwrap();
    assert!(!root.exists());
}

#[cfg(windows)]
#[test]
fn reset_native_junction_refuses_reparse_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("physical");
    let alias = dir.path().join("alias");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("sentinel"), b"keep this").unwrap();
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&alias)
        .arg(&root)
        .status()
        .unwrap();
    assert!(status.success());
    let err = reset_uri(alias.to_str().unwrap(), None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(std::fs::read(root.join("sentinel")).unwrap(), b"keep this");
    assert!(std::fs::symlink_metadata(&alias).is_ok());
}

#[cfg(windows)]
#[test]
fn reset_identity_rejects_replaced_file_and_directory() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("db.cl");
    let other_bundle = dir.path().join("other.cl");
    std::fs::write(&bundle, b"original").unwrap();
    std::fs::write(&other_bundle, b"replacement").unwrap();
    let file_id = file_identity::of_path(&bundle).unwrap();
    std::fs::remove_file(&bundle).unwrap();
    std::fs::rename(&other_bundle, &bundle).unwrap();
    assert!(matches!(
        verify_reset_identity(&bundle, &bundle, file_id),
        Err(Error::Storage(_))
    ));
    assert_eq!(std::fs::read(&bundle).unwrap(), b"replacement");

    let root = dir.path().join("root");
    let other_root = dir.path().join("other-root");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&other_root).unwrap();
    let dir_id = file_identity::of_path(&root).unwrap();
    std::fs::remove_dir(&root).unwrap();
    std::fs::rename(&other_root, &root).unwrap();
    assert!(matches!(
        verify_reset_identity(&root, &root, dir_id),
        Err(Error::Storage(_))
    ));
}
