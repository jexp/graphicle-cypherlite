//! The layout a checkpoint plans over (ADR-0183 Decision 1). A kind's
//! previous parts become sorted, disjoint [`Slot`]s: each group of
//! overlapping legacy parts is healed into one window, and every live id no
//! part covers gets a new window.

use super::*;

use std::ops::Range;

/// One id range of the layout rewrite units are planned over. Only a kept
/// slot has a previous file, so only kept slots supply copy sources and
/// only their paths are retired when a unit replaces them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Slot {
    /// A previous manifest part and its file.
    Kept(ShardPart),
    /// An id range with no previous file, whose live rows are encoded: a
    /// healed overlap group, or a new window over ids no part covers.
    Window(Range<i64>),
}

impl Slot {
    pub(super) fn span(&self) -> Range<i64> {
        match self {
            Self::Kept(part) => part.id_lo..part.id_hi,
            Self::Window(range) => range.clone(),
        }
    }

    /// The previous part of a kept slot.
    pub(super) fn kept(&self) -> Option<&ShardPart> {
        match self {
            Self::Kept(part) => Some(part),
            Self::Window(_) => None,
        }
    }
}

/// Lay out `parts`, a kind's previous manifest parts in any order, over its
/// sorted live `ids`: sort them, heal overlapping groups and cover each id
/// no part holds with a window `w` ids wide. Returns the slots, sorted and
/// disjoint with every id in one, and the healed parts' paths to retire.
pub(super) fn lay_out(mut parts: Vec<ShardPart>, ids: &[i64], w: i64) -> (Vec<Slot>, Vec<String>) {
    parts.sort_by_key(|part| part.id_lo);
    let (slots, retired) = heal_overlaps(parts);
    (cover_ids(slots, ids, w), retired)
}

/// Replace each group of mutually overlapping `parts`, which are sorted by
/// `id_lo`, with one window over the group's union range, which may span
/// more than `MAX_WINDOW` ids, and keep every other part; returns the
/// slots, which are disjoint, and the replaced parts' paths. Stores written
/// by v0.4.0–v0.11.x, before [`cover_ids`] clipped new windows to the gaps
/// between parts, can hold overlapping parts that both hold an id, and
/// v0.4.0–v0.9.x could list one fixed-name part twice. Which part holds the
/// live version is unknown, so the group's files are not copied from: its
/// window encodes each live row once (ADR-0183 Decision 1).
fn heal_overlaps(parts: Vec<ShardPart>) -> (Vec<Slot>, Vec<String>) {
    let mut slots = Vec::with_capacity(parts.len());
    let mut retired = Vec::new();
    let mut sorted = parts.into_iter().peekable();
    while let Some(first) = sorted.next() {
        let mut hi = first.id_hi;
        let mut overlapping = Vec::new();
        // An earlier member can reach past a later one.
        while let Some(part) = sorted.next_if(|p| p.id_lo < hi) {
            hi = hi.max(part.id_hi);
            overlapping.push(part.path);
        }
        if overlapping.is_empty() {
            slots.push(Slot::Kept(first));
            continue;
        }
        slots.push(Slot::Window(first.id_lo..hi));
        retired.push(first.path);
        retired.append(&mut overlapping);
    }
    (slots, retired)
}

/// Give each of the sorted `ids` no slot covers a window: its `w`-aligned
/// window clipped to the gap between the slots on either side. `slots` must
/// be sorted and disjoint, and stay so. `w` follows the measured density,
/// so it can have grown since the parts were written; clipping keeps new
/// windows disjoint from them, and none spans more than `w` ids.
pub(super) fn cover_ids(slots: Vec<Slot>, ids: &[i64], w: i64) -> Vec<Slot> {
    let mut covered = Vec::with_capacity(slots.len());
    let mut ahead = slots.into_iter().peekable();
    // The end of the last slot passed: slots are disjoint, so no earlier
    // one reaches further.
    let mut reach = i64::MIN;
    for &id in ids {
        while let Some(slot) = ahead.next_if(|s| s.span().start <= id) {
            reach = slot.span().end;
            covered.push(slot);
        }
        if id < reach {
            continue;
        }
        let aligned = (id / w) * w;
        let end = aligned.saturating_add(w);
        let hi = ahead.peek().map_or(end, |s| end.min(s.span().start));
        covered.push(Slot::Window(aligned.max(reach)..hi));
        reach = hi;
    }
    covered.extend(ahead);
    covered
}
