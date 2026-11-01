//! Page allocation and freelist coalescing for the `.cl` bundle.
use super::*;

/// Fold freelist extents that share a boundary into one extent.
///
/// Overlapping extents are left unchanged so ownership validation still
/// reports them. `retired` is not part of this list: those pages stay
/// unreachable until the publication that appended them has completed.
pub(super) fn merge_adjacent_freelist(freelist: &mut Vec<Extent>) {
    if freelist.len() < 2 {
        return;
    }
    freelist.sort_by_key(|extent| extent.page);
    let mut merged: Vec<Extent> = Vec::with_capacity(freelist.len());
    for extent in freelist.drain(..) {
        let absorbed = merged.last_mut().is_some_and(|prev| {
            let Some(end) = prev.page.checked_add(prev.count) else {
                return false;
            };
            if end != extent.page {
                return false;
            }
            match prev.count.checked_add(extent.count) {
                Some(count) => {
                    prev.count = count;
                    true
                }
                None => false,
            }
        });
        if !absorbed {
            merged.push(extent);
        }
    }
    *freelist = merged;
}

pub(super) fn alloc_pages(b: &mut Bundle, n: u32) -> Result<u32> {
    if n == 0 {
        return Ok(0);
    }
    // The freelist is already folded: `free_pages` merges an extent that is
    // reusable immediately, and `persist` folds `retired` once the previous
    // superblock no longer names those pages.
    if let Some((idx, e)) = b
        .freelist
        .iter()
        .enumerate()
        .find(|(_, e)| e.count >= n)
        .map(|(i, e)| (i, e.clone()))
    {
        let page = e.page;
        if e.count == n {
            b.freelist.remove(idx);
        } else {
            b.freelist[idx] = Extent {
                page: e
                    .page
                    .checked_add(n)
                    .ok_or_else(|| Error::storage(".cl page allocation overflow"))?,
                count: e.count - n,
            };
        }
        b.extents_dirty = true;
        return Ok(page);
    }
    let page = b.next_page;
    let next = page
        .checked_add(n)
        .ok_or_else(|| Error::storage(".cl page allocation overflow"))?;
    b.file.set_len(PAGE_U64 * u64::from(next))?;
    b.next_page = next;
    b.extents_dirty = true;
    Ok(page)
}

pub(super) fn free_pages(b: &mut Bundle, page: u32, count: u32) {
    if count == 0 {
        return;
    }
    let end = u64::from(page) + u64::from(count);
    let selected = b.selected.iter().any(|e| {
        u64::from(page) < u64::from(e.page) + u64::from(e.count) && u64::from(e.page) < end
    });
    if selected {
        b.retired.push(Extent { page, count });
    } else {
        b.freelist.push(Extent { page, count });
        merge_adjacent_freelist(&mut b.freelist);
    }
    b.catalog_dirty = true;
}
