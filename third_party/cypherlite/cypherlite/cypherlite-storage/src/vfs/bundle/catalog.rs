//! On-disk v1 catalog/superblock selection, ownership and orphan recovery.
use super::*;

pub(super) fn init_empty(file: &mut File) -> Result<Bundle> {
    file.set_len(PAGE_U64 * 2)?;
    let mut b = Bundle {
        file: file.try_clone()?,
        generation: 1,
        next_page: 2,
        next_inode: 1,
        inodes: BTreeMap::new(),
        by_id: BTreeMap::new(),
        freelist: Vec::new(),
        selected: Vec::new(),
        retired: Vec::new(),
        publication_failed: false,
        #[cfg(test)]
        fail_at: None,
        #[cfg(test)]
        io_counts: IoCounts::default(),
        catalog_root: 0,
        catalog_len: 0,
        catalog_dirty: false,
        extents_dirty: false,
    };
    write_superblock(&mut b)?;
    host_durability_barrier(&b.file)?;
    Ok(b)
}

/// The locking, truncate-always load. Its only caller is the writable
/// [`BundleVfs::open_or_create`](super::BundleVfs::open_or_create), which
/// holds the exclusive lock and a writable fd. Read-only opens
/// ([`BundleVfs::open_read_only`](super::BundleVfs::open_read_only) and the
/// lock-free peek) call [`load_peek`] directly and never reach the truncate:
/// a stale uncommitted tail (a writer that crashed mid-extend, or a torn
/// partial page at EOF) leaves `host_len` past the committed length, and
/// `ftruncate` on a read-only fd would fail the open outright. Skipping the
/// truncate on a read-only open is safe: the tail pages have no committed
/// owner, ownership validation bounds every read to `file_pages`, a
/// read-only open never allocates (its fd refuses the growing `set_len` at
/// the OS), and the next writable open truncates the tail before any
/// allocation can map it.
pub(super) fn load_existing(file: &mut File) -> Result<Bundle> {
    let (bundle, _, host_len) = load_peek(file)?;
    // Pages beyond the selected superblock's file_pages have no committed
    // owner (including no WAL-tail owner). set_len on a later allocation does
    // not clear existing host bytes, so discard the uncommitted physical tail
    // before it can be mapped into a new inode and exposed by a sparse write.
    // `load_peek` leaves `next_page == file_pages` (reclaim_orphans sets it
    // unconditionally), so next_page is the committed length in pages. The
    // peek/existing distinction is enforced by the callee's name: only this
    // lock-holding caller truncates; a peek load holds no lock and must not
    // mutate the file.
    let committed_len = u64::from(bundle.next_page) * PAGE_U64;
    if host_len != committed_len {
        bundle.file.set_len(committed_len)?;
    }
    Ok(bundle)
}

/// Superblock selection and catalog validation shared by the locking
/// [`load_existing`] and the lock-free peek ([`BundleVfs::open_peek`],
/// ADR-0165). Never mutates the host file — a live writer may own it; the
/// physical-tail truncation is [`load_existing`]'s alone. Returns the
/// selected superblock's `VERSION` and the host file length (statted once,
/// here) alongside the catalog.
pub(super) fn load_peek(file: &mut File) -> Result<(Bundle, u32, u64)> {
    let host_len = file.metadata()?.len();
    let host_pages = host_len / PAGE_U64;
    if host_pages < 2 {
        // A short non-bundle is a missing-magic error, but a bundle with a
        // truncated superblock is a different kind of corruption. Propagate
        // read errors rather than treating an unreadable file as bad magic.
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0; MAGIC.len()];
        file.read_exact(&mut magic[..host_len.min(MAGIC.len() as u64) as usize])?;
        if host_len < MAGIC.len() as u64 || &magic != MAGIC {
            return Err(Error::storage(
                "not a CypherLite .cl bundle (missing magic MDBCLB01)",
            ));
        }
        return Err(Error::storage("truncated .cl superblock pages"));
    }
    let a = read_super(file, SUPER_A)?;
    let sb = read_super(file, SUPER_B)?;
    let chosen = match (a, sb) {
        (Some(a), Some(b)) => {
            if a.generation >= b.generation {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => {
            return Err(Error::storage(
                "not a CypherLite .cl bundle (missing magic MDBCLB01)",
            ));
        }
    };
    if chosen.file_pages < 2 || u64::from(chosen.file_pages) > host_pages {
        return Err(Error::storage(".cl file_pages outside physical file"));
    }
    let catalog = match (chosen.catalog_root, chosen.catalog_len) {
        (0, 0) => None,
        (root, len) if root >= 2 && (len as usize) <= MAX_CATALOG_LEN => {
            let count = pages_for(len as u64)?;
            checked_range(root, count, chosen.file_pages)?;
            Some((root, len))
        }
        _ => {
            return Err(Error::storage(
                "invalid .cl catalog root or length (16 MiB maximum)",
            ));
        }
    };
    let mut bundle = Bundle {
        file: file.try_clone()?,
        generation: chosen.generation,
        next_page: 2,
        next_inode: 1,
        inodes: BTreeMap::new(),
        by_id: BTreeMap::new(),
        freelist: Vec::new(),
        selected: Vec::new(),
        retired: Vec::new(),
        publication_failed: false,
        #[cfg(test)]
        fail_at: None,
        #[cfg(test)]
        io_counts: IoCounts::default(),
        catalog_root: chosen.catalog_root,
        catalog_len: chosen.catalog_len,
        catalog_dirty: false,
        extents_dirty: false,
    };
    if let Some((root, len)) = catalog {
        load_catalog(&mut bundle, root, len, chosen.file_pages)?;
    }
    validate_ownership(&bundle, chosen.file_pages)?;
    apply_wal_tail(
        &mut bundle,
        chosen.wal_inode,
        chosen.wal_len,
        chosen.file_pages,
    )?;
    validate_ownership(&bundle, chosen.file_pages)?;
    reclaim_orphans(&mut bundle, chosen.file_pages)?;
    // A catalog written before adjacent holes were folded, and an orphan
    // gap that touches a recorded hole, both become one extent here.
    super::merge_adjacent_freelist(&mut bundle.freelist);
    bundle.select_current();
    Ok((bundle, chosen.version, host_len))
}

pub(super) struct Super {
    generation: u64,
    version: u32,
    catalog_root: u32,
    catalog_len: u32,
    file_pages: u32,
    wal_inode: u32,
    wal_len: u64,
}

pub(super) fn read_super(file: &mut File, page: u32) -> Result<Option<Super>> {
    let buf = read_page(file, page)?;
    let magic: [u8; 8] = buf[OFF_MAGIC..OFF_MAGIC + 8].try_into().unwrap();
    if &magic != MAGIC {
        return Ok(None);
    }
    let checksum = u32::from_le_bytes(buf[OFF_CHECKSUM..OFF_CHECKSUM + 4].try_into().unwrap());
    if crc32fast::hash(&buf[..OFF_CHECKSUM]) != checksum {
        return Ok(None);
    }
    let version = u32::from_le_bytes(buf[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
    // The registry classifies magic and VERSION as one READABLE pair
    // (ADR-0127 Decision 2); a checksum-valid superblock of this container
    // magic with an unknown VERSION was written by a newer CypherLite.
    if matches!(
        crate::version::bundle::classify(&magic, version),
        crate::version::Classification::Unreadable(_)
    ) {
        return Err(Error::storage(format!(
            "unsupported .cl bundle version {version}"
        )));
    }
    let page_size = u32::from_le_bytes(buf[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4].try_into().unwrap());
    if page_size != PAGE as u32 {
        return Err(Error::storage(format!(
            "unsupported .cl page size {page_size}"
        )));
    }
    Ok(Some(Super {
        generation: u64::from_le_bytes(buf[OFF_GENERATION..OFF_GENERATION + 8].try_into().unwrap()),
        version,
        catalog_root: u32::from_le_bytes(
            buf[OFF_CATALOG_ROOT..OFF_CATALOG_ROOT + 4]
                .try_into()
                .unwrap(),
        ),
        catalog_len: u32::from_le_bytes(
            buf[OFF_CATALOG_LEN..OFF_CATALOG_LEN + 4]
                .try_into()
                .unwrap(),
        ),
        file_pages: u32::from_le_bytes(buf[OFF_FILE_PAGES..OFF_FILE_PAGES + 4].try_into().unwrap()),
        wal_inode: u32::from_le_bytes(buf[OFF_WAL_INODE..OFF_WAL_INODE + 4].try_into().unwrap()),
        wal_len: u64::from_le_bytes(buf[OFF_WAL_LEN..OFF_WAL_LEN + 8].try_into().unwrap()),
    }))
}

pub(super) fn write_superblock(b: &mut Bundle) -> Result<()> {
    let mut buf = [0u8; PAGE];
    buf[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(MAGIC);
    buf[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
    buf[OFF_PAGE_SIZE..OFF_PAGE_SIZE + 4].copy_from_slice(&(PAGE as u32).to_le_bytes());
    buf[OFF_GENERATION..OFF_GENERATION + 8].copy_from_slice(&b.generation.to_le_bytes());
    buf[OFF_CATALOG_ROOT..OFF_CATALOG_ROOT + 4].copy_from_slice(&b.catalog_root.to_le_bytes());
    buf[OFF_CATALOG_LEN..OFF_CATALOG_LEN + 4].copy_from_slice(&b.catalog_len.to_le_bytes());
    buf[OFF_FILE_PAGES..OFF_FILE_PAGES + 4].copy_from_slice(&b.next_page.to_le_bytes());
    let (wal_inode, wal_len) = wal_tail_hint(b);
    buf[OFF_WAL_INODE..OFF_WAL_INODE + 4].copy_from_slice(&wal_inode.to_le_bytes());
    buf[OFF_WAL_LEN..OFF_WAL_LEN + 8].copy_from_slice(&wal_len.to_le_bytes());
    let sum = crc32fast::hash(&buf[..OFF_CHECKSUM]);
    buf[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&sum.to_le_bytes());
    let page = if b.generation % 2 == 1 {
        SUPER_A
    } else {
        SUPER_B
    };
    #[cfg(test)]
    {
        b.io_counts.writes += 1;
        if b.fail_at == Some("super_tear") {
            b.file.seek(SeekFrom::Start(u64::from(page) * PAGE_U64))?;
            // Tear before the CRC field, leaving this slot checksum-invalid.
            b.file.write_all(&buf[..OFF_CHECKSUM])?;
            return Err(Error::storage("injected super_tear failure"));
        }
    }
    write_page(&mut b.file, page, &buf)
}

pub(super) fn wal_tail_hint(b: &Bundle) -> (u32, u64) {
    let mut best: Option<&Inode> = None;
    for (name, inode) in &b.inodes {
        if is_wal_path(name) && best.is_none_or(|i| inode.id >= i.id) {
            best = Some(inode);
        }
    }
    match best {
        Some(inode) => (inode.id, inode.len),
        None => (0, 0),
    }
}

pub(super) fn apply_wal_tail(
    b: &mut Bundle,
    wal_inode: u32,
    wal_len: u64,
    file_pages: u32,
) -> Result<()> {
    if file_pages < b.next_page {
        return Err(Error::storage(".cl catalog next_page exceeds file_pages"));
    }
    if wal_inode == 0 {
        if wal_len != 0 {
            return Err(Error::storage(".cl WAL length without inode"));
        }
        b.next_page = file_pages;
        return Ok(());
    }
    let path = b
        .by_id
        .get(&wal_inode)
        .cloned()
        .ok_or_else(|| Error::storage(".cl WAL inode missing"))?;
    if !is_wal_path(&path) {
        return Err(Error::storage(".cl WAL hint refers to non-WAL inode"));
    }
    let inode = b.inodes.get(&path).unwrap();
    let extra = file_pages - b.next_page;
    let cap = inode_capacity(inode)?
        .checked_add(u64::from(extra) * PAGE_U64)
        .ok_or_else(|| Error::storage(".cl WAL capacity overflow"))?;
    if wal_len < inode.len || wal_len > cap {
        return Err(Error::storage(".cl WAL length outside mapped pages"));
    }
    if extra > 0 {
        if let Some(inode) = b.inodes.get_mut(&path) {
            append_extent(&mut inode.extents, b.next_page, extra)?;
        }
        b.next_page = file_pages;
    }
    if let Some(inode) = b.inodes.get_mut(&path) {
        inode.len = wal_len;
    }
    Ok(())
}

pub(super) fn owned_ranges(b: &Bundle, file_pages: u32) -> Result<Vec<(u32, u32)>> {
    let mut ranges = vec![(0, 2)];
    if b.catalog_len > 0 {
        ranges.push(checked_range(
            b.catalog_root,
            pages_for(b.catalog_len as u64)?,
            file_pages,
        )?);
    }
    for inode in b.inodes.values() {
        let mut capacity = 0u64;
        for e in &inode.extents {
            ranges.push(checked_range(e.page, e.count, file_pages)?);
            capacity = capacity
                .checked_add(u64::from(e.count) * PAGE_U64)
                .ok_or_else(|| Error::storage(".cl inode capacity overflow"))?;
        }
        if inode.len > capacity {
            return Err(Error::storage(".cl inode length exceeds mapped extents"));
        }
    }
    for e in &b.freelist {
        ranges.push(checked_range(e.page, e.count, file_pages)?);
    }
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(Error::storage(
                "overlapping .cl catalog, inode or freelist pages",
            ));
        }
    }
    Ok(ranges)
}

pub(super) fn validate_ownership(b: &Bundle, file_pages: u32) -> Result<()> {
    if b.next_page > file_pages {
        return Err(Error::storage(".cl next_page exceeds file_pages"));
    }
    owned_ranges(b, file_pages)?;
    Ok(())
}

pub(super) fn reclaim_orphans(b: &mut Bundle, file_pages: u32) -> Result<()> {
    let ranges = owned_ranges(b, file_pages)?;
    let mut end = 2;
    for (start, next) in ranges.into_iter().skip(1) {
        if start > end {
            b.freelist.push(Extent {
                page: end,
                count: start - end,
            });
        }
        end = next;
    }
    if end < file_pages {
        b.freelist.push(Extent {
            page: end,
            count: file_pages - end,
        });
    }
    b.next_page = file_pages;
    Ok(())
}

pub(super) fn checked_range(page: u32, count: u32, limit: u32) -> Result<(u32, u32)> {
    let end = page
        .checked_add(count)
        .ok_or_else(|| Error::storage(".cl page range overflow"))?;
    if page < 2 || count == 0 || end > limit {
        return Err(Error::storage(".cl page range outside file_pages"));
    }
    Ok((page, end))
}

pub(super) fn pages_for(len: u64) -> Result<u32> {
    u32::try_from(len / PAGE_U64 + u64::from(!len.is_multiple_of(PAGE_U64)))
        .map_err(|_| Error::storage(".cl page count overflow"))
}

pub(super) fn encode_catalog(b: &Bundle) -> Result<Vec<u8>> {
    // Check the serialized size before allocating/copying names or extents.
    let mut size = 4usize + 4 + 4 + 4;
    for (name, inode) in &b.inodes {
        size = size
            .checked_add(20)
            .and_then(|n| n.checked_add(name.len()))
            .and_then(|n| n.checked_add(inode.extents.len().checked_mul(8)?))
            .ok_or_else(|| Error::storage(".cl catalog length overflow"))?;
        if size > MAX_CATALOG_LEN {
            return Err(Error::storage(".cl catalog exceeds 16 MiB"));
        }
    }
    size = size
        .checked_add(
            b.freelist
                .len()
                .checked_mul(8)
                .ok_or_else(|| Error::storage(".cl catalog length overflow"))?,
        )
        .ok_or_else(|| Error::storage(".cl catalog length overflow"))?;
    if size > MAX_CATALOG_LEN {
        return Err(Error::storage(".cl catalog exceeds 16 MiB"));
    }
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(
        &u32::try_from(b.inodes.len())
            .map_err(|_| Error::storage(".cl catalog entry count overflow"))?
            .to_le_bytes(),
    );
    for (name, inode) in &b.inodes {
        let nb = name.as_bytes();
        out.extend_from_slice(
            &u32::try_from(nb.len())
                .map_err(|_| Error::storage(".cl catalog name too long"))?
                .to_le_bytes(),
        );
        out.extend_from_slice(nb);
        out.extend_from_slice(&inode.id.to_le_bytes());
        out.extend_from_slice(&inode.len.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(inode.extents.len())
                .map_err(|_| Error::storage(".cl extent count overflow"))?
                .to_le_bytes(),
        );
        for e in &inode.extents {
            out.extend_from_slice(&e.page.to_le_bytes());
            out.extend_from_slice(&e.count.to_le_bytes());
        }
    }
    out.extend_from_slice(
        &u32::try_from(b.freelist.len())
            .map_err(|_| Error::storage(".cl freelist count overflow"))?
            .to_le_bytes(),
    );
    for e in &b.freelist {
        out.extend_from_slice(&e.page.to_le_bytes());
        out.extend_from_slice(&e.count.to_le_bytes());
    }
    out.extend_from_slice(&b.next_inode.to_le_bytes());
    out.extend_from_slice(&b.next_page.to_le_bytes());
    debug_assert_eq!(out.len(), size);
    Ok(out)
}

pub(super) fn take_u32(i: &mut usize, bytes: &[u8]) -> Result<u32> {
    let v = u32::from_le_bytes(
        bytes
            .get(
                *i..(*i)
                    .checked_add(4)
                    .ok_or_else(|| Error::storage(".cl catalog offset overflow"))?,
            )
            .ok_or_else(|| Error::storage("truncated .cl catalog"))?
            .try_into()
            .unwrap(),
    );
    *i += 4;
    Ok(v)
}

pub(super) fn take_u64(i: &mut usize, bytes: &[u8]) -> Result<u64> {
    let v = u64::from_le_bytes(
        bytes
            .get(
                *i..(*i)
                    .checked_add(8)
                    .ok_or_else(|| Error::storage(".cl catalog offset overflow"))?,
            )
            .ok_or_else(|| Error::storage("truncated .cl catalog"))?
            .try_into()
            .unwrap(),
    );
    *i += 8;
    Ok(v)
}

pub(super) fn load_catalog(b: &mut Bundle, root: u32, len: u32, file_pages: u32) -> Result<()> {
    let mut bytes = vec![0u8; len as usize];
    read_extent_bytes(&mut b.file, root, pages_for(len as u64)?, &mut bytes)?;
    let mut i = 0usize;
    let n = take_u32(&mut i, &bytes)?;
    // Each inode needs at least a name length, id, logical length and extent count.
    if u64::from(n) > (bytes.len() - i) as u64 / 20 {
        return Err(Error::storage(
            ".cl catalog inode count exceeds catalog bytes",
        ));
    }
    for _ in 0..n {
        let nlen = take_u32(&mut i, &bytes)? as usize;
        let name = String::from_utf8(
            bytes
                .get(
                    i..i.checked_add(nlen)
                        .ok_or_else(|| Error::storage(".cl catalog offset overflow"))?,
                )
                .ok_or_else(|| Error::storage("truncated .cl catalog"))?
                .to_vec(),
        )
        .map_err(|e| Error::storage(e.to_string()))?;
        i += nlen;
        let id = take_u32(&mut i, &bytes)?;
        let ilen = take_u64(&mut i, &bytes)?;
        let ne = take_u32(&mut i, &bytes)?;
        if name.is_empty()
            || id == 0
            || id == u32::MAX
            || b.inodes.contains_key(&name)
            || b.by_id.contains_key(&id)
        {
            return Err(Error::storage("duplicate or invalid .cl inode name/id"));
        }
        if u64::from(ne) > (bytes.len() - i) as u64 / 8 {
            return Err(Error::storage(".cl extent count exceeds catalog bytes"));
        }
        let mut extents = Vec::with_capacity(ne as usize);
        for _ in 0..ne {
            let page = take_u32(&mut i, &bytes)?;
            let count = take_u32(&mut i, &bytes)?;
            checked_range(page, count, file_pages)?;
            extents.push(Extent { page, count });
        }
        b.by_id.insert(id, name.clone());
        b.inodes.insert(
            name,
            Inode {
                id,
                len: ilen,
                extents,
            },
        );
        b.next_inode = b.next_inode.max(id + 1);
    }
    let nf = take_u32(&mut i, &bytes)?;
    if u64::from(nf) > (bytes.len() - i) as u64 / 8 {
        return Err(Error::storage(".cl freelist count exceeds catalog bytes"));
    }
    for _ in 0..nf {
        let page = take_u32(&mut i, &bytes)?;
        let count = take_u32(&mut i, &bytes)?;
        checked_range(page, count, file_pages)?;
        b.freelist.push(Extent { page, count });
    }
    let next_inode = take_u32(&mut i, &bytes)?;
    let next_page = take_u32(&mut i, &bytes)?;
    if i != bytes.len() || next_inode < b.next_inode || next_page < 2 || next_page > file_pages {
        return Err(Error::storage(
            "invalid .cl catalog counters or trailing bytes",
        ));
    }
    if b.inodes
        .values()
        .flat_map(|inode| &inode.extents)
        .chain(&b.freelist)
        .any(|e| e.page + e.count > next_page)
    {
        return Err(Error::storage(
            ".cl catalog next_page precedes mapped pages",
        ));
    }
    b.next_inode = next_inode;
    // Catalog allocation itself follows serialization of next_page in the writer.
    b.next_page = next_page.max(
        root.checked_add(pages_for(len as u64)?)
            .ok_or_else(|| Error::storage(".cl catalog range overflow"))?,
    );
    Ok(())
}

pub(super) fn read_extent_bytes(
    file: &mut File,
    root: u32,
    npages: u32,
    out: &mut [u8],
) -> Result<()> {
    if npages != pages_for(out.len() as u64)? || out.len() > MAX_CATALOG_LEN {
        return Err(Error::storage("invalid .cl catalog extent length"));
    }
    let mut done = 0usize;
    for i in 0..npages {
        let page = read_page(
            file,
            root.checked_add(i)
                .ok_or_else(|| Error::storage(".cl catalog page overflow"))?,
        )?;
        let n = PAGE.min(out.len() - done);
        out[done..done + n].copy_from_slice(&page[..n]);
        done += n;
        if done == out.len() {
            break;
        }
    }
    if done < out.len() {
        return Err(Error::storage("truncated .cl catalog"));
    }
    Ok(())
}
