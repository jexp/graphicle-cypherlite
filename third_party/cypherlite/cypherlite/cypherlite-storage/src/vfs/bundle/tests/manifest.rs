use super::super::*;
use super::host_page;

#[test]
fn bundle_failed_manifest_publication_preserves_selected_manifest_and_wal() {
    for failure in ["contents_sync", "super_write"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("generation.cl");
        let vfs = Arc::new(BundleVfs::open_or_create(&path).unwrap());
        let old = vec![b'O'; PAGE * 2];
        let mut manifest = vfs
            .open("manifest.json", VfsOpen::create_truncate())
            .unwrap();
        manifest.write_all(&old).unwrap();
        manifest.sync_all().unwrap();
        let mut wal = crate::wal::Wal::open(vfs.clone(), "wal", 0).unwrap();
        wal.append_commit(1, &[]).unwrap();
        let mut replacement = vfs
            .open("next-manifest", VfsOpen::create_truncate())
            .unwrap();
        replacement.write_all(&vec![b'N'; PAGE * 2]).unwrap();
        replacement.sync_all().unwrap();

        let selected = {
            let b = vfs.lock();
            let manifest_page = b.inodes["manifest.json"].extents[0].page;
            let wal_page = b.inodes["wal/00000001.log"].extents[0].page;
            let mut file = b.file.try_clone().unwrap();
            [manifest_page, wal_page, b.catalog_root]
                .map(|page| (page, read_page(&mut file, page).unwrap()))
        };
        vfs.lock().fail_at = Some(failure);
        let error = vfs.rename("next-manifest", "manifest.json").unwrap_err();
        assert!(error.to_string().contains(failure), "{error}");
        // Both selected inode bytes AND the selected catalog survive the
        // allocator's attempts to find space for the new catalog.
        for (page, bytes) in selected {
            assert_eq!(host_page(&vfs, page), bytes, "{failure}");
        }
        assert!(vfs.open("another", VfsOpen::create_truncate()).is_err());
        assert!(replacement.write_all(b"more").is_err());
        drop((replacement, manifest, wal, vfs));

        let reopened = BundleVfs::open_or_create(&path).unwrap();
        assert_eq!(
            super::super::super::read_to_vec(&reopened, "manifest.json").unwrap(),
            old
        );
        let reopened = Arc::new(reopened);
        let replay = crate::wal::Wal::open(reopened, "wal", 0)
            .unwrap()
            .replay(0)
            .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!((replay[0].lsn, replay[0].commit_ts), (1, 1));
    }
}

#[test]
fn bundle_failed_remove_preserves_selected_inode_and_ambiguous_barrier_fences_writes() {
    for failure in ["super_write", "super_sync"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remove.cl");
        let vfs = BundleVfs::open_or_create(&path).unwrap();
        let mut file = vfs
            .open("manifest.json", VfsOpen::create_truncate())
            .unwrap();
        file.write_all(&vec![b'M'; PAGE]).unwrap();
        file.sync_all().unwrap();
        let page = vfs.lock().inodes["manifest.json"].extents[0].page;
        let old_bytes = host_page(&vfs, page);
        vfs.lock().fail_at = Some(failure);
        assert!(vfs.remove_file("manifest.json").is_err());
        let b = vfs.lock();
        assert!(b.publication_failed);
        assert!(b.retired.iter().any(|e| e.page == page));
        assert!(!b.freelist.iter().any(|e| e.page == page));
        drop(b);
        assert!(file.write_all(b"blocked").is_err());
        drop((file, vfs));
        let mut host = OpenOptions::new().read(true).open(&path).unwrap();
        assert_eq!(read_page(&mut host, page).unwrap(), old_bytes);

        // An error before the superblock write selects the old catalog;
        // an error after writing it is ambiguous, so reopen selects the
        // checksum-valid generation actually present on this host.
        let reopened = BundleVfs::open_or_create(&path).unwrap();
        assert_eq!(
            reopened.exists("manifest.json").unwrap(),
            failure == "super_write"
        );
    }
}

#[test]
fn bundle_repeated_manifest_replacement_recycles_pages_and_grows_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recycle.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    // Cross a catalog-page boundary; an allocation from the freelist
    // changes the serialized freelist count and must be encoded afterwards.
    for n in 0..180 {
        let name = format!("nodes/long-shard-name-{n:04}.parquet");
        vfs.open(&name, VfsOpen::create_truncate()).unwrap();
    }
    let mut manifest = vfs
        .open("manifest.json", VfsOpen::create_truncate())
        .unwrap();
    manifest.write_all(&vec![b'0'; PAGE * 2]).unwrap();
    manifest.sync_all().unwrap();
    let initial_pages = vfs.lock().next_page;
    for n in 1..25 {
        let mut replacement = vfs
            .open("next-manifest", VfsOpen::create_truncate())
            .unwrap();
        replacement.write_all(&vec![b'0' + n; PAGE * 2]).unwrap();
        replacement.sync_all().unwrap();
        vfs.rename("next-manifest", "manifest.json").unwrap();
    }
    assert!(
        vfs.lock().next_page < initial_pages + 16,
        "retired catalog or manifest pages leaked"
    );
    drop((manifest, vfs));
    let reopened = BundleVfs::open_or_create(&path).unwrap();
    assert!(reopened.lock().catalog_len as usize > PAGE);
    assert_eq!(
        super::super::super::read_to_vec(&reopened, "manifest.json").unwrap(),
        vec![b'0' + 24; PAGE * 2]
    );
    let before = reopened.lock().next_page;
    let mut extra = reopened.open("extra", VfsOpen::create_truncate()).unwrap();
    extra.write_all(&vec![b'E'; PAGE * 2]).unwrap();
    extra.sync_all().unwrap();
    assert!(reopened.lock().next_page <= before + 2);
}
#[test]
fn bundle_bulk_remove_publishes_one_catalog_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bulk-remove.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut names = Vec::new();
    for n in 0..4 {
        let name = format!("nodes/part-{n}.parquet");
        let mut f = vfs.open(&name, VfsOpen::create_truncate()).unwrap();
        f.write_all(&vec![b'x'; PAGE]).unwrap();
        f.sync_all().unwrap();
        names.push(name);
    }
    vfs.lock().io_counts = IoCounts::default();
    vfs.remove_files(&names).unwrap();
    let counts = vfs.lock().io_counts;
    assert_eq!(counts.barriers, 2, "one contents + one publication barrier");
    assert_eq!(counts.writes, 2, "one catalog page + one superblock");
    for name in &names {
        assert!(!vfs.exists(name).unwrap(), "{name} survived removal");
    }
    drop(vfs);
    let reopened = BundleVfs::open_or_create(&path).unwrap();
    for name in &names {
        assert!(!reopened.exists(name).unwrap(), "{name} after reopen");
    }
}

#[test]
fn bundle_catalog_writer_limit_keeps_old_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog-limit.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut original = vfs.open("old", VfsOpen::create_truncate()).unwrap();
    original.write_all(b"committed").unwrap();
    original.sync_all().unwrap();
    let huge_name = "x".repeat(MAX_CATALOG_LEN);
    let mut oversized = vfs.open(&huge_name, VfsOpen::create_truncate()).unwrap();
    assert!(oversized.sync_all().is_err());
    drop((oversized, original, vfs));
    let reopened = BundleVfs::open_or_create(&path).unwrap();
    assert_eq!(
        super::super::super::read_to_vec(&reopened, "old").unwrap(),
        b"committed"
    );
}
