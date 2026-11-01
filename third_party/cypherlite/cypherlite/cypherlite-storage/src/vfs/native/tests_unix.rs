//! Native VFS tests, extracted from `native.rs` (the no-1k-line rule,
//! LABS-311).

#[cfg(all(test, unix))]
use super::*;
use std::os::unix::fs::symlink;

#[test]
fn checked_existing_refuses_symlink_swapped_after_inspection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock");
    let victim = dir.path().join("victim");
    fs::write(&path, b"original").unwrap();
    fs::write(&victim, b"keep").unwrap();
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    nofollow(&mut options);
    assert!(NativeVfs::open_checked_existing_with(&path, &options, || {
        fs::remove_file(&path).unwrap();
        symlink(&victim, &path).unwrap();
    })
    .is_err());
    assert_eq!(fs::read(&victim).unwrap(), b"keep");

    let vfs = NativeVfs::new(dir.path());
    assert!(vfs.open("lock", VfsOpen::create_truncate()).is_err());
    assert_eq!(fs::read(victim).unwrap(), b"keep");
}

#[test]
fn create_dir_all_refuses_a_symlinked_directory_below_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let vfs = NativeVfs::new(dir.path());
    let elsewhere = dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    symlink(&elsewhere, dir.path().join("escape")).unwrap();
    // lstat semantics: the symlinked entry reports its own shape.
    let meta = vfs.metadata("escape").unwrap();
    assert!(meta.is_symlink());
    assert!(!meta.is_dir());
    // Creating beneath it fails closed, and nothing was written
    // through the symlink.
    let err = vfs.create_dir_all("escape/Person").unwrap_err();
    assert!(err.to_string().contains("symlinks refused"), "{err}");
    assert!(!elsewhere.join("Person").exists());
}

#[test]
fn metadata_reports_a_fifo_as_the_other_kind() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let vfs = NativeVfs::new(dir.path());
    let fifo = dir.path().join("pipe");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    // lstat semantics: a FIFO reports its own shape — a file, a directory,
    // and the symlink refusal kind are all wrong answers; the classifier's
    // fourth state is the one the metadata boundary must carry.
    let meta = vfs.metadata("pipe").unwrap();
    assert_eq!(meta.kind, crate::vfs::VfsEntryKind::Other);
    assert!(!meta.is_symlink());
    assert!(!meta.is_dir());
    // And it is never opened through: the regular-file open refuses it.
    assert!(vfs.open("pipe", VfsOpen::read_only()).is_err());
}
