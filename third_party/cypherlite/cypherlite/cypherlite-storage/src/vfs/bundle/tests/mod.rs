use super::*;

type CatalogInode<'a> = (&'a str, u32, u64, &'a [(u32, u32)]);

fn catalog(inodes: &[CatalogInode<'_>], free: &[(u32, u32)], next_page: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(inodes.len() as u32).to_le_bytes());
    for (name, id, len, extents) in inodes {
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&(extents.len() as u32).to_le_bytes());
        for &(page, count) in *extents {
            bytes.extend_from_slice(&page.to_le_bytes());
            bytes.extend_from_slice(&count.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&(free.len() as u32).to_le_bytes());
    for &(page, count) in free {
        bytes.extend_from_slice(&page.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    bytes.extend_from_slice(&100u32.to_le_bytes());
    bytes.extend_from_slice(&next_page.to_le_bytes());
    bytes
}

// All forged inputs have a valid superblock checksum: rejection must come
// from the physical/ownership checks, not from a bad magic or CRC.
fn forged_bundle(
    bytes: &[u8],
    root: u32,
    catalog_len: u32,
    declared_pages: u32,
    physical_pages: u32,
    wal_inode: u32,
    wal_len: u64,
) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forged.cl");
    drop(BundleVfs::open_or_create(&path).unwrap());
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.set_len(u64::from(physical_pages) * PAGE_U64).unwrap();
    if root >= 2 && root < physical_pages && !bytes.is_empty() {
        file.seek(SeekFrom::Start(u64::from(root) * PAGE_U64))
            .unwrap();
        file.write_all(bytes).unwrap();
    }
    let mut superblock = read_page(&mut file, 0).unwrap();
    superblock[OFF_CATALOG_ROOT..OFF_CATALOG_ROOT + 4].copy_from_slice(&root.to_le_bytes());
    superblock[OFF_CATALOG_LEN..OFF_CATALOG_LEN + 4].copy_from_slice(&catalog_len.to_le_bytes());
    superblock[OFF_FILE_PAGES..OFF_FILE_PAGES + 4].copy_from_slice(&declared_pages.to_le_bytes());
    superblock[OFF_WAL_INODE..OFF_WAL_INODE + 4].copy_from_slice(&wal_inode.to_le_bytes());
    superblock[OFF_WAL_LEN..OFF_WAL_LEN + 8].copy_from_slice(&wal_len.to_le_bytes());
    let checksum = crc32fast::hash(&superblock[..OFF_CHECKSUM]);
    superblock[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&checksum.to_le_bytes());
    write_page(&mut file, 0, &superblock).unwrap();
    (dir, path)
}

/// Read the whole host `.cl` through a duplicate of the VFS's own locked
/// handle. Windows byte-range locks are enforced against any *other* handle,
/// so a fresh `File::open` of the raw path fails with `os error 33` while the
/// bundle lease is held; a handle duplicated with `try_clone` shares the lock
/// and can still read.
fn host_bytes(vfs: &BundleVfs) -> Vec<u8> {
    let mut file = vfs.lock().file.try_clone().unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

/// Read one physical host page through the VFS's locked handle (see above).
fn host_page(vfs: &BundleVfs, page: u32) -> [u8; PAGE] {
    let mut file = vfs.lock().file.try_clone().unwrap();
    read_page(&mut file, page).unwrap()
}

fn write_pages(vfs: &BundleVfs, name: &str, pages: usize) -> Box<dyn VfsFile> {
    let mut file = vfs.open(name, VfsOpen::create_truncate()).unwrap();
    file.write_all(&vec![b'x'; pages * PAGE]).unwrap();
    file
}

fn freelist_spans(bundle: &Bundle) -> Vec<(u32, u32)> {
    bundle
        .freelist
        .iter()
        .map(|extent| (extent.page, extent.count))
        .collect()
}

fn reject_unchanged(path: &Path) {
    let before = std::fs::read(path).unwrap();
    assert!(BundleVfs::open_or_create(path).is_err());
    assert_eq!(
        std::fs::read(path).unwrap(),
        before,
        "failed open mutated bundle"
    );
}

mod freelist;
mod manifest;
mod sparse;
mod validation;
mod wal;
