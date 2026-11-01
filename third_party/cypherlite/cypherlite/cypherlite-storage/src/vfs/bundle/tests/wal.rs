use super::super::*;
use super::{catalog, forged_bundle, host_bytes, host_page, reject_unchanged};

#[test]
fn bundle_wal_cow_faults_keep_selected_prefix_and_fence_failed_handle() {
    for (failure, during_append, expected) in [
        ("copy_read", true, 1),
        ("wal_tear_1", true, 1),
        ("wal_tear_2", true, 1),
        ("wal_tear_3", true, 1),
        ("catalog_write", false, 1),
        ("catalog_tear", false, 1),
        ("contents_sync", false, 1),
        ("contents_sync_after", false, 1),
        ("super_write", false, 1),
        ("super_tear", false, 1),
        ("super_sync", false, 2),
        ("super_sync_after", false, 2),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal-fault.cl");
        let vfs = Arc::new(BundleVfs::open_or_create(&path).unwrap());
        let mut wal = crate::wal::Wal::open(vfs.clone(), "wal", 0).unwrap();
        wal.append_commit(1, &[]).unwrap();
        let (old_page, old_bytes, catalog_page, catalog_bytes) = {
            let b = vfs.lock();
            let page = b.inodes["wal/00000001.log"].extents[0].page;
            let mut host = b.file.try_clone().unwrap();
            (
                page,
                read_page(&mut host, page).unwrap(),
                b.catalog_root,
                read_page(&mut host, b.catalog_root).unwrap(),
            )
        };
        {
            let mut b = vfs.lock();
            b.io_counts = IoCounts::default();
            b.fail_at = Some(failure);
        }
        if during_append {
            let err = wal.append_record(2, &[]).unwrap_err();
            assert!(err.to_string().contains(failure), "{failure}: {err}");
        } else {
            wal.append_record(2, &[]).unwrap();
            let err = wal.sync().unwrap_err();
            assert!(err.to_string().contains(failure), "{failure}: {err}");
        }
        let b = vfs.lock();
        assert!(b.publication_failed, "{failure}");
        assert!(
            b.freelist
                .iter()
                .all(|e| { old_page < e.page || old_page >= e.page + e.count }),
            "{failure}: selected page was made allocatable"
        );
        drop(b);
        assert!(wal.sync().is_err(), "{failure}: fenced sync");
        assert!(
            wal.append_record(3, &[]).is_err(),
            "{failure}: fenced append"
        );
        assert_eq!(host_page(&vfs, old_page), old_bytes, "{failure}");
        assert_eq!(host_page(&vfs, catalog_page), catalog_bytes, "{failure}");
        drop((wal, vfs));
        let reopened = Arc::new(BundleVfs::open_or_create(&path).unwrap());
        let records = crate::wal::Wal::open(reopened, "wal", 0)
            .unwrap()
            .replay(0)
            .unwrap();
        assert_eq!(records.len(), expected, "{failure}");
        assert_eq!(records[0].lsn, 1, "{failure}");
        if expected == 2 {
            assert_eq!(records[1].lsn, 2, "{failure}");
        }
    }
}

#[test]
fn bundle_wal_cow_read_failure_never_replaces_selected_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal-read-error.cl");
    let vfs = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    let mut wal = crate::wal::Wal::open(vfs.clone(), "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let before = host_bytes(&vfs);
    vfs.lock().file = OpenOptions::new().write(true).open(&path).unwrap();
    assert!(wal.append_record(2, &[]).is_err());
    assert!(vfs.lock().publication_failed);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop((wal, vfs));
    let reopened = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    assert_eq!(
        crate::wal::Wal::open(reopened, "wal", 0)
            .unwrap()
            .replay(0)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn bundle_wal_cow_counts_and_reuses_retired_pages() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal-count.cl");
    let vfs = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    let mut wal = crate::wal::Wal::open(vfs.clone(), "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let old_page = vfs.lock().inodes["wal/00000001.log"].extents[0].page;
    vfs.lock().io_counts = IoCounts::default();
    wal.append_commit(2, &[]).unwrap();
    let b = vfs.lock();
    assert_eq!(b.io_counts.wal_writes, 3);
    assert_eq!(b.io_counts.writes, 5, "three WAL + catalog + super");
    assert_eq!(b.io_counts.barriers, 2);
    assert_eq!(b.io_counts.copies, 1);
    assert_ne!(b.inodes["wal/00000001.log"].extents[0].page, old_page);
    assert!(b.freelist.iter().any(|e| e.page == old_page));
    drop(b);
    vfs.lock().io_counts = IoCounts::default();
    wal.append_record(3, &[]).unwrap();
    wal.append_record(4, &[]).unwrap();
    wal.sync().unwrap();
    let counts = vfs.lock().io_counts;
    assert_eq!(counts.wal_writes, 6);
    assert_eq!(counts.copies, 1, "one COW per selected page per group");
    assert_eq!(counts.writes, 8, "six WAL + catalog + super");
    assert_eq!(counts.barriers, 2);
    let initial_pages = vfs.lock().next_page;
    for ts in 5..30 {
        wal.append_commit(ts, &[]).unwrap();
    }
    assert!(
        vfs.lock().next_page < initial_pages + 5,
        "WAL COW leaked pages"
    );
    drop((wal, vfs));
    let reopened = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    let records = crate::wal::Wal::open(reopened, "wal", 0)
        .unwrap()
        .replay(0)
        .unwrap();
    assert_eq!(records.len(), 29);
    assert_eq!(records.last().unwrap().lsn, 29);
}

#[test]
fn bundle_wal_remaps_middle_page_and_crosses_boundary_after_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal-boundary.cl");
    let vfs = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    let mut middle = vfs
        .open("wal/middle.log", VfsOpen::create_truncate())
        .unwrap();
    middle.write_all(&vec![b'M'; PAGE * 3]).unwrap();
    middle.sync_all().unwrap();
    let old = vfs.lock().inodes["wal/middle.log"].extents[0].page + 1;
    middle.seek(SeekFrom::Start(PAGE_U64 + 30)).unwrap();
    middle.write_all(b"changed").unwrap();
    {
        let b = vfs.lock();
        let extents = &b.inodes["wal/middle.log"].extents;
        assert_eq!(extents.len(), 3);
        assert_eq!((extents[0].page, extents[0].count), (old - 1, 1));
        assert_eq!((extents[2].page, extents[2].count), (old + 1, 1));
        assert!(b.retired.iter().any(|e| e.page == old));
    }
    assert_eq!(host_page(&vfs, old), [b'M'; PAGE]);
    middle.sync_all().unwrap();
    let mut wal = crate::wal::Wal::open(vfs.clone(), "wal", 0).unwrap();
    for ts in 1..=110 {
        wal.append_commit(ts, &[]).unwrap();
    }
    let first_len = vfs.lock().inodes["wal/00000001.log"].len;
    assert!(first_len > PAGE_U64);
    wal.rotate().unwrap();
    wal.append_commit(111, &[]).unwrap();
    drop((middle, wal, vfs));
    let reopened = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    let bytes = super::super::super::read_to_vec(&*reopened, "wal/middle.log").unwrap();
    assert_eq!(&bytes[PAGE + 30..PAGE + 37], b"changed");
    assert_eq!(bytes[PAGE * 2], b'M');
    let records = crate::wal::Wal::open(reopened, "wal", 0)
        .unwrap()
        .replay(0)
        .unwrap();
    assert_eq!(records.len(), 111);
    assert_eq!(records.last().unwrap().lsn, 111);
}

#[test]
fn bundle_v1_stale_catalog_wal_length_reopens_and_cows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-tail.cl");
    let vfs = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    let mut wal = crate::wal::Wal::open(vfs.clone(), "wal", 0).unwrap();
    wal.append_commit(1, &[]).unwrap();
    let (catalog_page, old_page) = {
        let b = vfs.lock();
        (b.catalog_root, b.inodes["wal/00000001.log"].extents[0].page)
    };
    drop((wal, vfs));

    // Construct the old v1 tail-hint shortcut: the selected catalog has
    // the magic-only length while the superblock advertises the commit.
    let mut host = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let mut cat = read_page(&mut host, catalog_page).unwrap();
    let name = b"wal/00000001.log";
    let len_offset = 4 + 4 + name.len() + 4;
    assert_eq!(&cat[8..8 + name.len()], name);
    cat[len_offset..len_offset + 8].copy_from_slice(&(MAGIC.len() as u64).to_le_bytes());
    write_page(&mut host, catalog_page, &cat).unwrap();
    drop(host);

    let reopened = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    let mut wal = crate::wal::Wal::open(reopened.clone(), "wal", 0).unwrap();
    assert_eq!(wal.replay(0).unwrap().len(), 1);
    let old_bytes = host_page(&reopened, old_page);
    wal.append_commit(2, &[]).unwrap();
    assert_eq!(host_page(&reopened, old_page), old_bytes);
    assert_eq!(wal.replay(0).unwrap().len(), 2);
    drop((wal, reopened));
    let reopened = Arc::new(BundleVfs::open_or_create(&path).unwrap());
    assert_eq!(
        crate::wal::Wal::open(reopened, "wal", 0)
            .unwrap()
            .replay(0)
            .unwrap()
            .len(),
        2
    );
}
#[test]
fn bundle_wal_tail_checked_and_valid_tail_recovered() {
    let bytes = catalog(&[("wal/00000001.log", 1, PAGE_U64, &[(3, 1)])], &[], 4);
    for (wal_inode, wal_len, file_pages) in [
        (2, PAGE_U64, 5),
        (1, PAGE_U64 - 1, 5),
        (1, PAGE_U64 * 3, 5),
        (0, 1, 5),
        (1, PAGE_U64 + 1, 3),
    ] {
        let (_dir, path) = forged_bundle(
            &bytes,
            2,
            bytes.len() as u32,
            file_pages,
            5,
            wal_inode,
            wal_len,
        );
        reject_unchanged(&path);
    }
    let non_wal = catalog(&[("data", 1, PAGE_U64, &[(3, 1)])], &[], 4);
    let (_dir, path) = forged_bundle(&non_wal, 2, non_wal.len() as u32, 5, 5, 1, PAGE_U64);
    reject_unchanged(&path);
    // Page 4 belongs to the committed WAL tail; page 5 does not.
    let (_dir, path) = forged_bundle(&bytes, 2, bytes.len() as u32, 5, 6, 1, PAGE_U64 + 4);
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(4 * PAGE_U64)).unwrap();
    file.write_all(b"tail").unwrap();
    file.seek(SeekFrom::Start(5 * PAGE_U64)).unwrap();
    file.write_all(&[b'S'; PAGE]).unwrap();
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 5 * PAGE_U64);
    let mut f = vfs.open("wal/00000001.log", VfsOpen::read_only()).unwrap();
    assert_eq!(f.len().unwrap(), PAGE_U64 + 4);
    f.seek(SeekFrom::Start(PAGE_U64)).unwrap();
    let mut tail = [0; 4];
    f.read_exact(&mut tail).unwrap();
    assert_eq!(&tail, b"tail");
}
