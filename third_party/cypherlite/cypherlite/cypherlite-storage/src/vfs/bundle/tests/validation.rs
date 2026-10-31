use super::super::catalog::load_existing;
use super::super::*;
use super::{catalog, forged_bundle, reject_unchanged};

#[test]
fn bundle_short_non_bundle_reports_missing_magic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("garbage.cl");
    std::fs::write(&path, b"not a bundle").unwrap();
    let err = BundleVfs::open_or_create(&path).err().unwrap();
    assert!(err.to_string().contains("MDBCLB01"), "{err}");
    std::fs::write(&path, MAGIC).unwrap();
    let err = BundleVfs::open_or_create(&path).err().unwrap();
    assert!(
        err.to_string().contains("truncated .cl superblock"),
        "{err}"
    );
    let mut unreadable = OpenOptions::new().write(true).open(&path).unwrap();
    let err = load_existing(&mut unreadable).err().unwrap();
    assert!(
        !err.to_string().contains("truncated .cl superblock"),
        "{err}"
    );
    assert!(!err.to_string().contains("MDBCLB01"), "{err}");
}

#[test]
fn bundle_superblock_read_error_is_not_a_bad_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("super-read-error.cl");
    drop(BundleVfs::open_or_create(&path).unwrap());
    let before = std::fs::read(&path).unwrap();
    let mut unreadable = OpenOptions::new().write(true).open(&path).unwrap();
    assert!(load_existing(&mut unreadable).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn bundle_rejects_forged_superblock_and_catalog_lengths() {
    let good = catalog(&[], &[], 3);
    for (root, len, declared, physical) in [
        (0, 0, 0, 3),
        (0, 0, 4, 3),
        (0, 1, 3, 3),
        (1, good.len() as u32, 3, 3),
        (2, (MAX_CATALOG_LEN + 1) as u32, 3, 3),
        (2, PAGE as u32 + 1, 3, 3),
        (u32::MAX, good.len() as u32, 3, 3),
        (2, good.len() as u32, 3, 1),
    ] {
        let (_dir, path) = forged_bundle(&good, root, len, declared, physical, 0, 0);
        reject_unchanged(&path);
    }
}

#[test]
fn bundle_rejects_catalog_count_bombs_before_reserve() {
    let mut inode_count = catalog(&[], &[], 3);
    inode_count[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut extent_count = catalog(&[("a", 1, 0, &[])], &[], 3);
    let extent_count_off = 4 + 4 + 1 + 4 + 8;
    extent_count[extent_count_off..extent_count_off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut free_count = catalog(&[], &[], 3);
    free_count[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    for bytes in [inode_count, extent_count, free_count] {
        let (_dir, path) = forged_bundle(&bytes, 2, bytes.len() as u32, 3, 3, 0, 0);
        reject_unchanged(&path);
    }
    let bytes = catalog(&[("a", u32::MAX, 0, &[])], &[], 3);
    let (_dir, path) = forged_bundle(&bytes, 2, bytes.len() as u32, 3, 3, 0, 0);
    reject_unchanged(&path);
}

#[test]
fn bundle_rejects_aliases_invalid_extents_and_inode_lengths() {
    let cases = [
        catalog(&[("a", 1, 1, &[(2, 1)])], &[], 4), // catalog alias
        catalog(&[("a", 1, 1, &[(1, 1)])], &[], 4), // superblock alias
        catalog(&[("a", 1, 1, &[(3, 1)]), ("b", 2, 1, &[(3, 1)])], &[], 4),
        catalog(&[("a", 1, 1, &[(3, 1)])], &[(3, 1)], 4),
        catalog(&[], &[(3, 1), (3, 1)], 4),
        catalog(&[], &[(1, 1)], 4),
        catalog(&[("a", 1, 1, &[(5, 1)])], &[], 6), // outside physical file
        catalog(&[("a", 1, 1, &[(u32::MAX, 2)])], &[], 4),
        catalog(&[("a", 1, 1, &[(3, 0)])], &[], 4),
        catalog(&[("a", 1, PAGE_U64 + 1, &[(3, 1)])], &[], 4),
        catalog(&[("a", 1, 1, &[(3, 1)]), ("a", 2, 1, &[(4, 1)])], &[], 5),
        catalog(&[("a", 1, 1, &[(3, 1)]), ("b", 1, 1, &[(4, 1)])], &[], 5),
        catalog(&[("a", 1, 1, &[(3, 1)])], &[], 2), // next_page before extent
    ];
    for bytes in cases {
        let (_dir, path) = forged_bundle(&bytes, 2, bytes.len() as u32, 5, 5, 0, 0);
        reject_unchanged(&path);
    }
}
