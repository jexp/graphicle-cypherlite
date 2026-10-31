//! URI parsing: an `s3://`, `gs://` or `opfs://` URI, or the single-slash
//! `s3:/` form, is refused by its scheme (an OPFS store takes a
//! caller-supplied VFS instead), while drive-letter paths and paths with a
//! colon in a later component open as filesystem databases.

use super::*;

#[test]
fn opfs_uri_is_rejected_without_a_vfs() {
    let err = parse_uri("opfs://playground").err().expect("opfs rejected");
    assert!(err.to_string().contains("opfs://"), "{err}");
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let store = StoreTarget::from_vfs(Arc::clone(&vfs));
    assert!(store.wal_is_memory());
    assert!(store.filesystem_dir().is_none());
    assert_eq!(store.snapshot_codec(), SnapshotCodecKind::default());
}

#[test]
fn s3_uri_is_rejected() {
    let err = parse_uri("s3://bucket/prefix").err().expect("s3 rejected");
    assert!(err.to_string().contains("s3://"), "{err}");
    assert!(err.to_string().contains("memorydb-cloud"), "{err}");
    let err = reset_uri("s3://bucket/prefix", None).unwrap_err();
    assert!(err.to_string().contains("s3://"), "{err}");
    let err = parse_uri("gs://bucket/prefix").err().expect("gs rejected");
    assert!(err.to_string().contains("gs://"), "{err}");
}

#[test]
fn single_slash_remote_uri_is_rejected_without_touching_disk() {
    // Regression: `s3:/bucket/prefix` (single slash) used to parse as a
    // relative filesystem path and a test committed the resulting
    // `s3:` directory tree. It must now be rejected. No cwd juggling:
    // the parse fails at scheme validation, before any disk I/O, so a
    // literal `s3:` directory can never appear relative to the cwd.
    let err = parse_uri("s3:/bucket/prefix").err().expect("s3:/ rejected");
    assert!(err.to_string().contains("s3://"), "{err}");
    let err = reset_uri("s3:/bucket/prefix", None).unwrap_err();
    assert!(err.to_string().contains("s3://"), "{err}");
    assert!(!Path::new("s3:").exists());
}

#[test]
fn windows_drive_letter_paths_are_filesystem_not_scheme() {
    // Windows absolute paths (`C:\db`, `C:/db`) start with a drive
    // letter, not a URI scheme; the single-slash strictness must not
    // reject them (ci.yml runs the lib tests on windows-latest, where
    // tempdir paths have this shape). Asserted through `remote_scheme`
    // so the check is disk-free and platform-independent.
    for uri in ["C:\\db", "C:/db", "d:\\Data\\graphs", "z:/"] {
        assert!(
            remote_scheme(uri).is_none(),
            "{uri} is a filesystem path, not a URI scheme"
        );
    }
    // A scheme head that is not a drive letter stays rejected.
    assert!(remote_scheme("s3:/bucket/prefix").is_some());
    assert!(remote_scheme("https:/example.com/db").is_some());
}

#[test]
#[cfg(windows)]
fn windows_tempdir_path_parses_as_filesystem() {
    // End-to-end on Windows: a real tempdir path carries a drive
    // letter and must open as a filesystem database.
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let target = parse_uri(uri).unwrap();
    assert_eq!(
        target.filesystem_dir().unwrap(),
        std::fs::canonicalize(dir.path()).unwrap()
    );
}

#[test]
#[cfg(unix)]
fn absolute_path_with_colon_component_is_filesystem() {
    // A colon inside a later path component (e.g. a volume named
    // "My Drive: Backup") is not a URI scheme; only a scheme in the
    // *first* component is rejected.
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("with:colon").join("db");
    let uri = uri.to_str().unwrap();
    let target = parse_uri(uri).unwrap();
    assert!(target.filesystem_dir().is_some());
    drop(target);
    reset_uri(uri, None).unwrap();
    assert!(!dir.path().join("with:colon").join("db").exists());
}
