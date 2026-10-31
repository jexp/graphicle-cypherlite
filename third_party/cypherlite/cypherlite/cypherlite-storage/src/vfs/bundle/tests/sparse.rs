use super::super::*;
use super::host_bytes;

#[test]
fn bundle_existing_page_read_error_preserves_host_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read-error.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
    f.write_all(b"original live bytes").unwrap();
    f.sync_all().unwrap();
    let before = host_bytes(&vfs);
    // Host file is still seekable/writable, but every read fails.
    vfs.lock().file = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(4)).unwrap();
    assert!(f.write_all(b"change").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn bundle_growth_read_error_does_not_extend_host_or_inode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("growth-read-error.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
    f.write_all(b"old bytes").unwrap();
    f.sync_all().unwrap();
    let before = host_bytes(&vfs);
    let next_page = vfs.lock().next_page;
    vfs.lock().file = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(3)).unwrap();
    assert!(f.write_all(&vec![b'X'; PAGE]).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(f.len().unwrap(), 9);
    assert_eq!(vfs.lock().next_page, next_page);
}

#[test]
fn bundle_crossing_eof_read_error_preserves_host_and_inode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crossing-eof.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
    f.write_all(b"old bytes").unwrap();
    f.sync_all().unwrap();
    let before = host_bytes(&vfs);
    let next_page = vfs.lock().next_page;
    vfs.lock().file = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(9)).unwrap();
    assert!(f.write_all(&vec![b'X'; PAGE]).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(f.len().unwrap(), 9);
    assert_eq!(vfs.lock().next_page, next_page);
}

#[test]
fn bundle_full_page_overwrite_does_not_require_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("full-overwrite.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
    f.write_all(&vec![b'A'; PAGE]).unwrap();
    f.sync_all().unwrap();
    vfs.lock().file = OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&vec![b'B'; PAGE]).unwrap();
    drop((f, vfs));
    let reopened = BundleVfs::open_or_create(&path).unwrap();
    assert_eq!(
        super::super::super::read_to_vec(&reopened, "data").unwrap(),
        vec![b'B'; PAGE]
    );
}

#[test]
fn bundle_uncommitted_physical_tail_cannot_leak_into_sparse_gap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("uncommitted-tail.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
    f.write_all(b"kept").unwrap();
    f.sync_all().unwrap();
    drop((f, vfs));

    let committed_len = std::fs::metadata(&path).unwrap().len();
    let mut host = OpenOptions::new().append(true).open(&path).unwrap();
    host.write_all(&vec![b'S'; PAGE * 3]).unwrap();
    host.sync_all().unwrap();
    drop(host);

    let reopened = BundleVfs::open_or_create(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), committed_len);
    let mut f = reopened
        .open(
            "data",
            VfsOpen {
                write: true,
                ..VfsOpen::read_only()
            },
        )
        .unwrap();
    f.seek(SeekFrom::Start(PAGE_U64 * 2)).unwrap();
    f.write_all(b"tail").unwrap();
    f.sync_all().unwrap();
    drop((f, reopened));

    let reopened = BundleVfs::open_or_create(&path).unwrap();
    let contents = super::super::super::read_to_vec(&reopened, "data").unwrap();
    assert_eq!(&contents[..4], b"kept");
    assert!(contents[4..PAGE * 2].iter().all(|b| *b == 0));
    assert_eq!(&contents[PAGE * 2..], b"tail");
}

#[test]
fn bundle_read_only_open_keeps_stale_uncommitted_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale-tail-readonly.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
    f.write_all(b"kept").unwrap();
    f.sync_all().unwrap();
    drop((f, vfs));

    // A writer that crashed mid-extend leaves host pages past the committed
    // length. A read-only open must not truncate them — ftruncate on its
    // read-only fd would fail the open — and must still read the committed
    // catalog: the tail has no committed owner and no read maps it.
    let committed_len = std::fs::metadata(&path).unwrap().len();
    let mut host = OpenOptions::new().append(true).open(&path).unwrap();
    host.write_all(&vec![b'S'; PAGE * 3]).unwrap();
    host.sync_all().unwrap();
    drop(host);
    let stale_len = committed_len + (PAGE * 3) as u64;

    let readonly = BundleVfs::open_read_only(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        stale_len,
        "read-only open must not mutate the host file"
    );
    assert_eq!(
        super::super::super::read_to_vec(&readonly, "data").unwrap(),
        b"kept"
    );
    drop(readonly);

    // The next writable open owns the truncation, before any allocation can
    // map the tail pages.
    let reopened = BundleVfs::open_or_create(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), committed_len);
    assert_eq!(
        super::super::super::read_to_vec(&reopened, "data").unwrap(),
        b"kept"
    );
}

#[test]
fn bundle_sparse_write_zeroes_reused_gap_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reused-gap.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut deleted = vfs.open("deleted", VfsOpen::create_truncate()).unwrap();
    deleted.write_all(&vec![b'S'; PAGE * 8]).unwrap();
    deleted.sync_all().unwrap();
    vfs.remove_file("deleted").unwrap();

    let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
    f.write_all(b"kept").unwrap();
    let end = PAGE_U64 * 3 + 17;
    f.seek(SeekFrom::Start(end)).unwrap();
    f.write_all(b"tail").unwrap();
    f.sync_all().unwrap();
    drop((f, deleted, vfs));

    let reopened = BundleVfs::open_or_create(&path).unwrap();
    let contents = super::super::super::read_to_vec(&reopened, "data").unwrap();
    assert_eq!(&contents[..4], b"kept");
    assert!(contents[4..end as usize].iter().all(|b| *b == 0));
    assert_eq!(&contents[end as usize..], b"tail");
}

#[test]
fn bundle_sparse_write_within_capacity_persists_zero_gap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("within-capacity-gap.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
    f.write_all(b"kept").unwrap();
    f.sync_all().unwrap();
    f.seek(SeekFrom::Start(PAGE_U64 - 4)).unwrap();
    f.write_all(b"tail").unwrap();
    f.sync_all().unwrap();
    drop((f, vfs));

    let reopened = BundleVfs::open_or_create(&path).unwrap();
    let contents = super::super::super::read_to_vec(&reopened, "data").unwrap();
    assert_eq!(&contents[..4], b"kept");
    assert!(contents[4..PAGE - 4].iter().all(|b| *b == 0));
    assert_eq!(&contents[PAGE - 4..], b"tail");
}
#[test]
fn file_set_len_grows_logical_length_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("set-len.cl");
    {
        let vfs = BundleVfs::open_or_create(&path).unwrap();
        let mut f = vfs.open("data", VfsOpen::create_truncate()).unwrap();
        f.write_all(b"abc").unwrap();
        // Grow within the already-allocated capacity: `ensure_len` must still
        // advance the logical length (regression: it used to no-op).
        f.set_len(PAGE_U64).unwrap();
        assert_eq!(f.len().unwrap(), PAGE_U64);
        // Grow beyond capacity: allocate extents and advance length again.
        f.set_len(PAGE_U64 * 4).unwrap();
        assert_eq!(f.len().unwrap(), PAGE_U64 * 4);
        // Shrinking back is still honoured.
        f.set_len(9).unwrap();
        assert_eq!(f.len().unwrap(), 9);
        f.sync_all().unwrap();
    }
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    assert_eq!(vfs.metadata("data").unwrap().len, 9);
}
