//! Single-file container VFS (ADR-0074). One host file presents the same
//! virtual tree as Native/Memory.

use std::collections::BTreeMap;
use std::fs::File;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cypherlite_core::{Error, Result};

mod alloc;
mod catalog;
mod host;

use super::host::host_durability_barrier;
use super::{Vfs, VfsFile, VfsMeta, VfsOpen};
use alloc::{alloc_pages, free_pages, merge_adjacent_freelist};
use catalog::{encode_catalog, pages_for, wal_tail_hint, write_superblock};
pub(crate) use host::HostAdmit;

// Container markers live in the store-version registry (ADR-0127), re-exported for `catalog`.
pub(crate) use crate::version::bundle::{CURRENT_MAGIC as MAGIC, CURRENT_VERSION as VERSION};
const PAGE: usize = 4096;
const PAGE_U64: u64 = PAGE as u64;
const MAX_CATALOG_LEN: usize = 16 * 1024 * 1024;
const SUPER_A: u32 = 0;
const SUPER_B: u32 = 1;

const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 8;
const OFF_PAGE_SIZE: usize = 12;
const OFF_GENERATION: usize = 16;
const OFF_CATALOG_ROOT: usize = 24;
const OFF_CATALOG_LEN: usize = 28;
const OFF_FILE_PAGES: usize = 32;
const OFF_WAL_INODE: usize = 36;
const OFF_WAL_LEN: usize = 40;
const OFF_CHECKSUM: usize = 48;

/// Whether a virtual path is the WAL stream itself (`wal`) or one of its
/// segment files (`wal/...`). The `.cl` superblock tail hint, catalog
/// ownership check, and WAL-length bookkeeping all key off this predicate.
pub(super) fn is_wal_path(path: &str) -> bool {
    path == "wal" || path.starts_with("wal/")
}

#[derive(Clone)]
struct Extent {
    page: u32,
    count: u32,
}

#[derive(Clone)]
struct Inode {
    id: u32,
    len: u64,
    extents: Vec<Extent>,
}

struct Bundle {
    file: File,
    generation: u64,
    next_page: u32,
    next_inode: u32,
    inodes: BTreeMap<String, Inode>,
    by_id: BTreeMap<u32, String>,
    freelist: Vec<Extent>,
    // Pages reachable through the last durably selected superblock. Retired
    // extents may not be allocated again until its replacement is durable.
    selected: Vec<Extent>,
    retired: Vec<Extent>,
    // A failed publication may have written either superblock. Only reopening
    // can establish which catalog was selected; do not publish again in place.
    publication_failed: bool,
    #[cfg(test)]
    fail_at: Option<&'static str>,
    #[cfg(test)]
    io_counts: IoCounts,
    catalog_root: u32,
    catalog_len: u32,
    catalog_dirty: bool,
    extents_dirty: bool,
}

impl Bundle {
    fn select_current(&mut self) {
        self.selected.clear();
        if self.catalog_len != 0 {
            self.selected.push(Extent {
                page: self.catalog_root,
                count: self.catalog_len.div_ceil(PAGE as u32),
            });
        }
        for inode in self.inodes.values() {
            self.selected.extend(inode.extents.iter().cloned());
        }
    }

    fn writable(&self) -> Result<()> {
        if self.publication_failed {
            Err(Error::storage(
                ".cl publication failed; reopen to select the durable generation",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    fn fail(&self, stage: &'static str) -> Result<()> {
        if self.fail_at == Some(stage) {
            Err(Error::storage(format!("injected {stage} failure")))
        } else {
            Ok(())
        }
    }

    fn data_page(&mut self, page: u32, data: &[u8], wal: bool) -> Result<()> {
        #[cfg(not(test))]
        let _ = wal;
        #[cfg(test)]
        {
            self.io_counts.writes += 1;
            if wal {
                self.io_counts.wal_writes += 1;
                let stage = format!("wal_tear_{}", self.io_counts.wal_writes);
                if self.fail_at == Some(stage.as_str()) {
                    self.file
                        .seek(SeekFrom::Start(u64::from(page) * PAGE_U64))?;
                    self.file.write_all(&data[..PAGE / 2])?;
                    return Err(Error::storage(format!("injected {stage} failure")));
                }
            }
        }
        write_page(&mut self.file, page, data)
    }

    fn contents_barrier(&mut self) -> Result<()> {
        #[cfg(test)]
        {
            self.io_counts.barriers += 1;
            self.fail("contents_sync")?;
        }
        host_durability_barrier(&self.file)?;
        #[cfg(test)]
        self.fail("contents_sync_after")?;
        Ok(())
    }

    fn publication_barrier(&mut self) -> Result<()> {
        #[cfg(test)]
        {
            self.io_counts.barriers += 1;
            self.fail("super_sync")?;
        }
        host_durability_barrier(&self.file)?;
        #[cfg(test)]
        self.fail("super_sync_after")?;
        Ok(())
    }
}

#[cfg(test)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct IoCounts {
    writes: usize,
    wal_writes: usize,
    barriers: usize,
    copies: usize,
}

impl Drop for Bundle {
    fn drop(&mut self) {
        // Match NativeLease's final handoff: on macOS closing the locked
        // descriptor alone can briefly leave an immediate reopen contended.
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

struct Inner {
    bundle: Mutex<Bundle>,
}

pub struct BundleVfs {
    inner: Arc<Inner>,
    path: PathBuf,
}

impl BundleVfs {
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bundle> {
        self.inner.bundle.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn commit_catalog(b: &mut Bundle) -> Result<Option<Extent>> {
    // Reserve against the largest possible serialized catalog. Allocation can
    // remove a freelist entry, but cannot increase the encoded size. Encode
    // again afterwards so the new catalog does not claim its own pages free.
    let npages = pages_for(encode_catalog(b)?.len() as u64)?.max(1);
    let old = (b.catalog_len != 0).then(|| Extent {
        page: b.catalog_root,
        count: b.catalog_len.div_ceil(PAGE as u32),
    });
    let root = alloc_pages(b, npages)?;
    let bytes = encode_catalog(b)?;
    let mut padded = bytes.clone();
    padded.resize(npages as usize * PAGE, 0);
    for i in 0..npages {
        let s = i as usize * PAGE;
        let page = root
            .checked_add(i)
            .ok_or_else(|| Error::storage(".cl catalog page overflow"))?;
        #[cfg(test)]
        {
            b.io_counts.writes += 1;
            if b.fail_at == Some("catalog_tear") {
                b.file.seek(SeekFrom::Start(u64::from(page) * PAGE_U64))?;
                b.file.write_all(&padded[s..s + PAGE / 2])?;
                return Err(Error::storage("injected catalog_tear failure"));
            }
            b.fail("catalog_write")?;
        }
        write_page(&mut b.file, page, &padded[s..s + PAGE])?;
    }
    b.catalog_root = root;
    b.catalog_len = bytes.len() as u32;
    b.catalog_dirty = false;
    b.extents_dirty = false;
    Ok(old)
}

fn inode_capacity(inode: &Inode) -> Result<u64> {
    inode.extents.iter().try_fold(0u64, |cap, e| {
        cap.checked_add(u64::from(e.count) * PAGE_U64)
            .ok_or_else(|| Error::storage(".cl inode capacity overflow"))
    })
}

fn ensure_len(b: &mut Bundle, path: &str, needed: u64) -> Result<()> {
    let cap = b
        .inodes
        .get(path)
        .ok_or_else(|| Error::io(format!("missing {path}")))
        .and_then(inode_capacity)?;
    if needed <= cap {
        let grew = if let Some(inode) = b.inodes.get_mut(path) {
            if needed > inode.len {
                inode.len = needed;
                true
            } else {
                false
            }
        } else {
            false
        };
        if grew {
            b.catalog_dirty = true;
        }
        return Ok(());
    }
    let extra = pages_for(needed - cap)?;
    let start = alloc_pages(b, extra)?;
    let inode = b.inodes.get_mut(path).unwrap();
    append_extent(&mut inode.extents, start, extra)?;
    inode.len = needed;
    b.catalog_dirty = true;
    Ok(())
}

fn map_offset(inode: &Inode, off: u64) -> Result<(u32, usize)> {
    let mut remain = off;
    for e in &inode.extents {
        let bytes = e.count as u64 * PAGE_U64;
        if remain < bytes {
            let page = e
                .page
                .checked_add(
                    u32::try_from(remain / PAGE_U64)
                        .map_err(|_| Error::storage(".cl offset page overflow"))?,
                )
                .ok_or_else(|| Error::storage(".cl offset page overflow"))?;
            let within = (remain % PAGE_U64) as usize;
            return Ok((page, within));
        }
        remain -= bytes;
    }
    Err(Error::storage(format!("offset {off} past inode extents")))
}

// Preserve logical order when one physical page inside a (possibly multi-page)
// extent is replaced. Adjacent physical ranges are coalesced to bound catalog
// growth for repeated commits and after reuse of freed pages.
fn append_extent(extents: &mut Vec<Extent>, page: u32, count: u32) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    if let Some(last) = extents.last_mut() {
        if last.page.checked_add(last.count) == Some(page) {
            last.count = last
                .count
                .checked_add(count)
                .ok_or_else(|| Error::storage(".cl extent overflow"))?;
            return Ok(());
        }
    }
    extents.push(Extent { page, count });
    Ok(())
}

fn remap_page(b: &mut Bundle, path: &str, logical_page: u64, old: u32, new: u32) -> Result<()> {
    let inode = b.inodes.get_mut(path).unwrap();
    let mut next = Vec::with_capacity(inode.extents.len() + 2);
    let mut cursor = 0u64;
    let mut found = false;
    for e in &inode.extents {
        let end = cursor + u64::from(e.count);
        if cursor <= logical_page && logical_page < end {
            let before = (logical_page - cursor) as u32;
            if e.page.checked_add(before) != Some(old) {
                return Err(Error::storage(".cl remap page mismatch"));
            }
            append_extent(&mut next, e.page, before)?;
            append_extent(&mut next, new, 1)?;
            append_extent(&mut next, old + 1, e.count - before - 1)?;
            found = true;
        } else {
            append_extent(&mut next, e.page, e.count)?;
        }
        cursor = end;
    }
    if !found {
        return Err(Error::storage(".cl remap outside inode"));
    }
    inode.extents = next;
    free_pages(b, old, 1);
    b.catalog_dirty = true;
    Ok(())
}

fn write_bytes(b: &mut Bundle, path: &str, pos: u64, buf: &[u8]) -> Result<u64> {
    if buf.is_empty() {
        return Ok(pos);
    }
    let inode = b
        .inodes
        .get(path)
        .ok_or_else(|| Error::io(format!("missing {path}")))?;
    let old_len = inode.len;
    let end = pos
        .checked_add(buf.len() as u64)
        .ok_or_else(|| Error::storage(".cl write offset overflow"))?;
    let start = pos.min(old_len);
    // Growing the host file must not precede a read of a live partial page:
    // if that read fails, even the physical bundle should remain unchanged.
    // At most the first page needs preflight: subsequent pages are fully
    // overwritten when a write crosses the current capacity. This includes
    // starting exactly at EOF within a partially live page: filling that
    // page's gap still needs a read of its existing live prefix.
    let mut first_page = if end > inode_capacity(inode)? && start % PAGE_U64 != 0 {
        let page = map_offset(inode, start)?.0;
        Some((page, read_page(&mut b.file, page)?))
    } else {
        None
    };
    let old_next_page = b.next_page;
    ensure_len(b, path, end)?;
    let mut cursor = start;
    while cursor < end {
        let inode = b.inodes.get(path).unwrap();
        let (page, within) = map_offset(inode, cursor)?;
        let n = (PAGE_U64 - within as u64).min(end - cursor) as usize;
        let gap = pos.saturating_sub(cursor).min(n as u64) as usize;
        // set_len zeroes fresh host pages already; only reused pages need
        // explicit writes for a full page of the gap.
        if gap == PAGE && page >= old_next_page {
            cursor += PAGE_U64;
            continue;
        }
        let mut pg = if within == 0 && n == PAGE {
            [0u8; PAGE]
        } else {
            // Reused freelist pages may contain deleted bytes. Only a page
            // containing old logical data is safe to read as-is.
            if cursor - within as u64 >= old_len {
                [0u8; PAGE]
            } else if first_page
                .as_ref()
                .is_some_and(|(cached, _)| *cached == page)
            {
                first_page.take().unwrap().1
            } else {
                read_page(&mut b.file, page)?
            }
        };
        pg[within..within + gap].fill(0);
        if gap < n {
            let source = (cursor + gap as u64 - pos) as usize;
            pg[within + gap..within + n].copy_from_slice(&buf[source..source + n - gap]);
        }
        let wal = is_wal_path(path);
        // Even bytes after the selected logical EOF share a physical page
        // with its committed prefix. Never issue a host write to that page.
        let selected = wal
            && b.selected
                .iter()
                .any(|e| page >= e.page && page < e.page + e.count);
        if selected {
            // A full overwrite still copies first: an unreadable selected
            // page must not be silently replaced with zeros.
            if within == 0 && n == PAGE {
                read_page(&mut b.file, page)?;
            }
            let new = alloc_pages(b, 1)?;
            #[cfg(test)]
            {
                b.io_counts.copies += 1;
                b.fail("copy_read")?;
            }
            b.data_page(new, &pg, true)?;
            remap_page(b, path, cursor / PAGE_U64, page, new)?;
        } else {
            b.data_page(page, &pg, wal)?;
        }
        cursor += n as u64;
    }
    if end > old_len {
        b.inodes.get_mut(path).unwrap().len = end;
        // WAL length is persisted in the superblock tail hint. Other inode
        // lengths need a catalog update even when no new pages were allocated.
        if !is_wal_path(path) || b.inodes[path].id != wal_tail_hint(b).0 {
            b.catalog_dirty = true;
        }
    }
    Ok(end)
}

fn read_bytes(b: &mut Bundle, path: &str, mut pos: u64, buf: &mut [u8]) -> Result<u64> {
    let inode = b
        .inodes
        .get(path)
        .ok_or_else(|| Error::io(format!("missing {path}")))?;
    if pos
        .checked_add(buf.len() as u64)
        .is_none_or(|end| end > inode.len)
    {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof").into());
    }
    let mut done = 0usize;
    while done < buf.len() {
        let inode = b.inodes.get(path).unwrap();
        let (page, within) = map_offset(inode, pos)?;
        let pg = read_page(&mut b.file, page)?;
        let n = (PAGE - within).min(buf.len() - done);
        buf[done..done + n].copy_from_slice(&pg[within..within + n]);
        done += n;
        pos += n as u64;
    }
    Ok(pos)
}

fn read_page(file: &mut File, page: u32) -> Result<[u8; PAGE]> {
    file.seek(SeekFrom::Start(page as u64 * PAGE_U64))?;
    let mut buf = [0u8; PAGE];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_page(file: &mut File, page: u32, data: &[u8]) -> Result<()> {
    debug_assert_eq!(data.len(), PAGE);
    file.seek(SeekFrom::Start(page as u64 * PAGE_U64))?;
    file.write_all(data)?;
    Ok(())
}

fn ensure_inode(b: &mut Bundle, path: &str, create: bool, truncate: bool) -> Result<()> {
    if create {
        if !b.inodes.contains_key(path) {
            let id = b.next_inode;
            b.next_inode = b
                .next_inode
                .checked_add(1)
                .ok_or_else(|| Error::storage(".cl inode id overflow"))?;
            b.inodes.insert(
                path.to_string(),
                Inode {
                    id,
                    len: 0,
                    extents: Vec::new(),
                },
            );
            b.by_id.insert(id, path.to_string());
            b.catalog_dirty = true;
        }
    } else if !b.inodes.contains_key(path) {
        return Err(Error::io(format!("missing {path}")));
    }
    if truncate {
        let extents = if let Some(inode) = b.inodes.get_mut(path) {
            inode.len = 0;
            std::mem::take(&mut inode.extents)
        } else {
            Vec::new()
        };
        for e in extents {
            free_pages(b, e.page, e.count);
        }
        b.catalog_dirty = true;
    }
    Ok(())
}

fn truncate_inode(b: &mut Bundle, path: &str, len: u64) -> Result<()> {
    let current = b
        .inodes
        .get(path)
        .ok_or_else(|| Error::io(format!("missing {path}")))?
        .len;
    if len >= current {
        return ensure_len(b, path, len);
    }
    let keep_pages = len.div_ceil(PAGE_U64) as u32;
    let extents = std::mem::take(&mut b.inodes.get_mut(path).unwrap().extents);
    let mut kept = Vec::new();
    let mut remaining = keep_pages;
    for extent in extents {
        let keep = remaining.min(extent.count);
        if keep > 0 {
            kept.push(Extent {
                page: extent.page,
                count: keep,
            });
            remaining -= keep;
        }
        if keep < extent.count {
            free_pages(b, extent.page + keep, extent.count - keep);
        }
    }
    let inode = b.inodes.get_mut(path).unwrap();
    inode.extents = kept;
    inode.len = len;
    b.catalog_dirty = true;
    Ok(())
}

fn drop_inode(b: &mut Bundle, path: &str) {
    if let Some(inode) = b.inodes.remove(path) {
        b.by_id.remove(&inode.id);
        for e in inode.extents {
            free_pages(b, e.page, e.count);
        }
        b.catalog_dirty = true;
    }
}

fn is_prefix(inodes: &BTreeMap<String, Inode>, path: &str) -> bool {
    let p = format!("{path}/");
    inodes.keys().any(|k| k.starts_with(&p))
}

fn persist(b: &mut Bundle, catalog: bool) -> Result<()> {
    b.writable()?;
    let result = (|| {
        let next_generation = b
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::storage(".cl generation overflow"))?;
        let old_catalog = if catalog && (b.catalog_dirty || b.extents_dirty) {
            let old = commit_catalog(b)?;
            b.contents_barrier()?;
            old
        } else {
            None
        };
        b.generation = next_generation;
        #[cfg(test)]
        b.fail("super_write")?;
        write_superblock(b)?;
        b.publication_barrier()?;

        // Only the successful replacement barrier makes the retired pages and
        // previous catalog reusable. A later open also finds them as orphans.
        b.select_current();
        b.freelist.append(&mut b.retired);
        if let Some(old) = old_catalog {
            b.freelist.push(old);
        }
        merge_adjacent_freelist(&mut b.freelist);
        Ok(())
    })();
    if result.is_err() {
        b.publication_failed = true;
    }
    result
}

impl Vfs for BundleVfs {
    fn durable(&self) -> bool {
        true
    }

    // Virtual directories are catalog prefixes in the locked host file.
    // Their persistence is handled by the bundle publication barrier.
    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        self.create_dir_all(path)
    }

    fn create_dir_all(&self, _path: &str) -> Result<()> {
        Ok(())
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        let b = self.lock();
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        let mut names = Vec::new();
        for key in b.inodes.keys() {
            let rest = if prefix.is_empty() {
                key.as_str()
            } else if let Some(r) = key.strip_prefix(&prefix) {
                r
            } else {
                continue;
            };
            let name = rest.split('/').next().unwrap_or(rest);
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    fn exists(&self, path: &str) -> Result<bool> {
        let b = self.lock();
        Ok(b.inodes.contains_key(path) || is_prefix(&b.inodes, path))
    }

    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        let b = self.lock();
        if let Some(inode) = b.inodes.get(path) {
            return Ok(VfsMeta {
                len: inode.len,
                is_dir: false,
            });
        }
        if is_prefix(&b.inodes, path) {
            return Ok(VfsMeta {
                len: 0,
                is_dir: true,
            });
        }
        Err(Error::io(format!("missing {path}")))
    }

    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        let mut b = self.lock();
        if opts.create || opts.create_new || opts.truncate || opts.write || opts.append {
            b.writable()?;
        }
        if opts.create_new && (b.inodes.contains_key(path) || is_prefix(&b.inodes, path)) {
            return Err(Error::io(format!("already exists: {path}")));
        }
        ensure_inode(&mut b, path, opts.create || opts.create_new, opts.truncate)?;
        let pos = if opts.append {
            b.inodes.get(path).map(|i| i.len).unwrap_or(0)
        } else {
            0
        };
        Ok(Box::new(BundleFile {
            inner: Arc::clone(&self.inner),
            path: path.to_string(),
            pos,
            append: opts.append,
        }))
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        let mut b = self.lock();
        b.writable()?;
        drop_inode(&mut b, path);
        persist(&mut b, true)?;
        Ok(())
    }

    /// Drop every inode in the batch, then publish once. One catalog rewrite,
    /// one contents barrier, one superblock and one publication barrier retire
    /// the whole set, instead of one persist per shard.
    fn remove_files(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut b = self.lock();
        b.writable()?;
        for path in paths {
            drop_inode(&mut b, path);
        }
        persist(&mut b, true)?;
        Ok(())
    }

    fn remove_dir_all(&self, path: &str) -> Result<()> {
        let mut b = self.lock();
        b.writable()?;
        if path.is_empty() {
            let names: Vec<_> = b.inodes.keys().cloned().collect();
            for n in names {
                drop_inode(&mut b, &n);
            }
        } else {
            let prefix = format!("{path}/");
            let names: Vec<_> = b
                .inodes
                .keys()
                .filter(|k| *k == path || k.starts_with(&prefix))
                .cloned()
                .collect();
            for n in names {
                drop_inode(&mut b, &n);
            }
        }
        persist(&mut b, true)?;
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let mut b = self.lock();
        b.writable()?;
        let inode = b
            .inodes
            .remove(from)
            .ok_or_else(|| Error::io(format!("missing {from}")))?;
        if let Some(old) = b.inodes.remove(to) {
            b.by_id.remove(&old.id);
            for e in old.extents {
                free_pages(&mut b, e.page, e.count);
            }
        }
        b.by_id.insert(inode.id, to.to_string());
        b.inodes.insert(to.to_string(), inode);
        b.catalog_dirty = true;
        persist(&mut b, true)?;
        Ok(())
    }

    fn sync_dir(&self, _path: &str) -> Result<()> {
        // The catalog/superblock barriers in rename persist the virtual entry.
        Ok(())
    }
}

struct BundleFile {
    inner: Arc<Inner>,
    path: String,
    pos: u64,
    append: bool,
}

impl BundleFile {
    fn lock(&self) -> std::sync::MutexGuard<'_, Bundle> {
        self.inner.bundle.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl VfsFile for BundleFile {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        let mut b = self.lock();
        b.writable()?;
        let path = self.path.clone();
        let mut pos = self.pos;
        if self.append {
            pos = b.inodes.get(&path).map(|i| i.len).unwrap_or(0);
        }
        let next = write_bytes(&mut b, &path, pos, buf).inspect_err(|_| {
            // A short host write may have changed an unselected page and the
            // in-memory mapping/length. Reopen before attempting publication.
            b.publication_failed = true;
        })?;
        drop(b);
        self.pos = next;
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        let mut b = self.lock();
        let path = self.path.clone();
        let pos = self.pos;
        let next = read_bytes(&mut b, &path, pos, buf)?;
        drop(b);
        self.pos = next;
        Ok(())
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        let b = self.lock();
        let len = b.inodes.get(&self.path).map(|i| i.len).unwrap_or(0);
        drop(b);
        let next = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => (len as i64).saturating_add(n).max(0) as u64,
            SeekFrom::Current(n) => (self.pos as i64).saturating_add(n).max(0) as u64,
        };
        self.pos = next;
        Ok(self.pos)
    }

    fn sync_all(&mut self) -> Result<()> {
        let mut b = self.lock();
        persist(&mut b, true)
    }

    fn durability_barrier(&mut self) -> Result<()> {
        let mut b = self.lock();
        let catalog = b.catalog_dirty || b.extents_dirty;
        persist(&mut b, catalog)
    }

    fn set_len(&mut self, len: u64) -> Result<()> {
        let mut b = self.lock();
        truncate_inode(&mut b, &self.path, len)?;
        drop(b);
        self.pos = self.pos.min(len);
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        let b = self.lock();
        Ok(b.inodes.get(&self.path).map(|i| i.len).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests;
