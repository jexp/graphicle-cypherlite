//! Native VFS tests, extracted from `native.rs` (the no-1k-line rule,
//! LABS-311).

#[cfg(all(test, windows))]
use super::*;

#[test]
fn checked_existing_refuses_swapped_reparse_point() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock");
    let victim = dir.path().join("victim");
    fs::write(&path, b"original").unwrap();
    fs::write(&victim, b"keep").unwrap();
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    nofollow(&mut options);
    let mut symlink_supported = true;
    let result = NativeVfs::open_checked_existing_with(&path, &options, || {
        fs::remove_file(&path).unwrap();
        if let Err(e) = std::os::windows::fs::symlink_file(&victim, &path) {
            // Some Windows runners do not grant the symlink privilege.
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
            symlink_supported = false;
        }
    });
    if symlink_supported {
        assert!(result.is_err());
        let vfs = NativeVfs::new(dir.path());
        assert!(vfs.open("lock", VfsOpen::create_truncate()).is_err());
    }
    assert_eq!(fs::read(victim).unwrap(), b"keep");
}
