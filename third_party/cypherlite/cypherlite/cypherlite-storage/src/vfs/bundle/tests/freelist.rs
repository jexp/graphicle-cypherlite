use super::super::catalog::checked_range;
use super::super::*;
use super::{catalog, forged_bundle, freelist_spans, reject_unchanged, write_pages};

#[test]
fn bundle_page_count_and_allocation_overflow() {
    assert!(pages_for(u64::MAX).is_err());
    assert!(checked_range(u32::MAX, 2, u32::MAX).is_err());
    let dir = tempfile::tempdir().unwrap();
    let vfs = BundleVfs::open_or_create(dir.path().join("overflow.cl")).unwrap();
    let mut bundle = vfs.lock();
    bundle.next_page = u32::MAX;
    assert!(alloc_pages(&mut bundle, 1).is_err());
    assert_eq!(bundle.next_page, u32::MAX);
}
#[test]
fn adjacent_free_extents_merge_into_one_hole_that_a_later_write_reuses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("merge.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let a = write_pages(&vfs, "a", 2);
    let mut b = write_pages(&vfs, "b", 2);
    b.sync_all().unwrap();
    let (a_page, a_count, b_count, published) = {
        let bundle = vfs.lock();
        let a = &bundle.inodes["a"].extents;
        let b = &bundle.inodes["b"].extents;
        assert_eq!((a.len(), b.len()), (1, 1));
        assert_eq!(a[0].count, 2);
        assert_eq!(b[0].count, 2);
        assert_eq!(
            a[0].page + a[0].count,
            b[0].page,
            "the two inodes must be neighbours"
        );
        (a[0].page, a[0].count, b[0].count, bundle.next_page)
    };
    drop((a, b));
    vfs.remove_files(&["a".into(), "b".into()]).unwrap();
    let next_after_free = {
        let bundle = vfs.lock();
        let spans = freelist_spans(&bundle);
        assert!(
            spans
                .iter()
                .any(|(page, count)| *page == a_page && *count >= a_count + b_count),
            "adjacent holes stayed split: {spans:?}"
        );
        bundle.next_page
    };
    // The replacement catalog may extend the file. The write below must not.
    assert!(next_after_free >= published);
    let mut c = write_pages(&vfs, "c", 3);
    c.sync_all().unwrap();
    let bundle = vfs.lock();
    assert_eq!(bundle.next_page, next_after_free);
    let c = &bundle.inodes["c"].extents;
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].page, a_page);
    assert!(c[0].count >= 3);
}

#[test]
fn a_live_inode_between_free_extents_still_appends() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("split.cl");
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let a = write_pages(&vfs, "a", 2);
    let b = write_pages(&vfs, "b", 2);
    let mut c = write_pages(&vfs, "c", 2);
    c.sync_all().unwrap();
    let (a_page, b_page) = {
        let bundle = vfs.lock();
        let a = &bundle.inodes["a"].extents[0];
        let b = &bundle.inodes["b"].extents[0];
        let c = &bundle.inodes["c"].extents[0];
        assert_eq!(a.page + a.count, b.page);
        assert_eq!(b.page + b.count, c.page);
        (a.page, b.page)
    };
    drop((a, b, c));
    vfs.remove_files(&["a".into(), "c".into()]).unwrap();
    let (max_hole, next_after_free) = {
        let bundle = vfs.lock();
        assert!(bundle.inodes.contains_key("b"));
        let spans = freelist_spans(&bundle);
        assert!(
            spans
                .iter()
                .all(|(page, count)| page + count <= b_page || *page >= b_page + 2),
            "holes on either side of b were merged: {spans:?}"
        );
        let max_hole = spans.iter().map(|(_, count)| *count).max().unwrap();
        let sum: u32 = spans.iter().map(|(_, count)| *count).sum();
        assert!(max_hole < sum);
        assert!(a_page < b_page);
        (max_hole, bundle.next_page)
    };
    let mut d = write_pages(&vfs, "d", max_hole as usize + 1);
    d.sync_all().unwrap();
    assert!(vfs.lock().next_page > next_after_free);
}

#[test]
fn reopen_merges_adjacent_freelist_extents_and_keeps_a_split_hole() {
    let adjacent = catalog(&[("keep", 1, PAGE_U64, &[(3, 1)])], &[(5, 1), (4, 1)], 6);
    let (_dir, path) = forged_bundle(&adjacent, 2, adjacent.len() as u32, 6, 6, 0, 0);
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    assert_eq!(freelist_spans(&vfs.lock()), vec![(4, 2)]);

    let split = catalog(
        &[
            ("pad", 1, PAGE_U64, &[(3, 1)]),
            ("mid", 2, PAGE_U64, &[(5, 1)]),
        ],
        &[(4, 1), (6, 1)],
        7,
    );
    let (_dir, path) = forged_bundle(&split, 2, split.len() as u32, 7, 7, 0, 0);
    let vfs = BundleVfs::open_or_create(&path).unwrap();
    let mut free = freelist_spans(&vfs.lock());
    free.sort();
    assert_eq!(free, vec![(4, 1), (6, 1)]);

    let overlap = catalog(&[], &[(4, 2), (5, 1)], 6);
    let (_dir, path) = forged_bundle(&overlap, 2, overlap.len() as u32, 6, 6, 0, 0);
    reject_unchanged(&path);
}
