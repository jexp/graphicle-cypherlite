//! The pair search: one shortest trail between two distinct nodes, by the
//! ADR-0037 shallow intersection or the ADR-0034 bidirectional BFS.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};

use cypherlite_core::{NodeId, RelId, Result};
use cypherlite_ir::Direction;

use super::{unique_rels, Search};

/// One shortest trail from `start` to `end`, start first, of length 1 to
/// `max`; `None` when there is none. The only home of the dispatch rule:
/// the shallow intersection for a directed pattern with `min` 1 and `max`
/// ≤ 3, the bidirectional BFS otherwise.
///
/// Precondition: `start` ≠ `end` and `min` ≤ 1. There the first meet at the
/// shortest length is always a trail, which the bidirectional search's stop
/// rule relies on; closed trails go to [`Search::closed`] and larger
/// minimums to enumeration (ADR-0189).
pub(crate) fn pair(s: &Search<'_, '_>, start: NodeId, end: NodeId) -> Result<Option<Vec<RelId>>> {
    debug_assert!(
        start != end && s.min <= 1,
        "pair search needs distinct ends and min_hops <= 1"
    );
    if s.max == 0 {
        return Ok(None);
    }
    if s.dir != Direction::Both && s.min >= 1 && s.max <= 3 {
        shallow(s, start, end)
    } else {
        bidir(s, start, end)
    }
}

/// ADR-0037: intersect the one-hop sets of both ends, expanding only the
/// smaller one a second step for three hops.
fn shallow(s: &Search<'_, '_>, start: NodeId, end: NodeId) -> Result<Option<Vec<RelId>>> {
    let s1 = s.hops(start)?;
    if let Some(&(_, r)) = s1.iter().find(|&&(n, _)| n == end) {
        return Ok(Some(vec![r]));
    }
    if s.max < 2 {
        return Ok(None);
    }
    let back = s.reversed();
    let t1 = back.hops(end)?;
    let t1_at: HashMap<NodeId, RelId> = t1.iter().copied().collect();
    let two = s1.iter().find_map(|&(mid, r1)| {
        let &r2 = t1_at.get(&mid)?;
        (r1 != r2).then(|| vec![r1, r2])
    });
    if two.is_some() || s.max < 3 {
        return Ok(two);
    }
    let s1_at: HashMap<NodeId, RelId> = s1.iter().copied().collect();
    // Expand the smaller 1-hop side; the selected order fn fixes the
    // [r1, r2, r3] path order at the hit site without a per-candidate flag.
    type TrailOrder = fn(RelId, RelId, RelId) -> [RelId; 3];
    let (near_search, near, far_at, order): (_, _, _, TrailOrder) = if s1.len() <= t1.len() {
        (*s, &s1, &t1_at, |a, m, b| [a, m, b])
    } else {
        (back, &t1, &s1_at, |a, m, b| [b, m, a])
    };
    let mut hit = None;
    for &(at, r_near) in near {
        near_search.each_hop(at, |n, r2| {
            if hit.is_none() {
                if let Some(&r_far) = far_at.get(&n) {
                    let trail = order(r_near, r2, r_far);
                    hit = unique_rels(&trail).then(|| trail.to_vec());
                }
            }
            Ok(())
        })?;
        if hit.is_some() {
            break;
        }
    }
    Ok(hit)
}

/// Depth and tree parent of each node one side reached; the origin has no
/// parent.
type Tree = HashMap<NodeId, (u32, Option<(NodeId, RelId)>)>;

/// One BFS tree of the bidirectional search.
struct Side<'a, 'g> {
    search: Search<'a, 'g>,
    q: VecDeque<(NodeId, u32)>,
    tree: Tree,
}

impl<'a, 'g> Side<'a, 'g> {
    fn new(search: Search<'a, 'g>, origin: NodeId) -> Self {
        Self {
            search,
            q: VecDeque::from([(origin, 0)]),
            tree: HashMap::from([(origin, (0, None))]),
        }
    }

    /// Depth of the layer to expand next.
    fn depth(&self) -> Option<u32> {
        self.q.front().map(|&(_, d)| d)
    }
}

/// The tree trail from the origin to `at`, origin first.
fn trail_to(tree: &Tree, mut at: NodeId) -> Vec<RelId> {
    let mut trail = Vec::new();
    while let Some(&(_, Some((prev, rid)))) = tree.get(&at) {
        trail.push(rid);
        at = prev;
    }
    trail.reverse();
    trail
}

/// The relationship `at`'s tree trail ends with. On a shortest-path tree
/// the trail is a simple path, so this is the only relationship of it that
/// touches `at`.
fn parent_rel(tree: &Tree, at: NodeId) -> Option<RelId> {
    tree.get(&at).and_then(|&(_, p)| p).map(|(_, rid)| rid)
}

/// ADR-0034: BFS from both ends, always expanding the side with the
/// smaller frontier layer.
fn bidir(s: &Search<'_, '_>, start: NodeId, end: NodeId) -> Result<Option<Vec<RelId>>> {
    let mut front = Side::new(*s, start);
    let mut back = Side::new(s.reversed(), end);
    let mut meet: Option<Vec<RelId>> = None;
    // Every layer below both heads is finished, so some split of every
    // trail of length ≤ df + db was already checked as a meet. With start
    // ≠ end and min_hops ≤ 1 the split of a shortest path is always
    // accepted (its two tree halves are node-disjoint), so once df + db + 1
    // reaches the meet no later layer can beat it (LABS-196).
    while let (Some(df), Some(db)) = (front.depth(), back.depth()) {
        // The next joining relationship makes a trail of df + db + 1. Past
        // max_hops it cannot be legal; do not walk the other max-hop ball.
        let next = u64::from(df) + u64::from(db) + 1;
        if next > u64::from(s.max) || meet.as_ref().is_some_and(|m| next >= m.len() as u64) {
            break;
        }
        let best = meet.as_ref().map(Vec::len);
        if front.q.len() <= back.q.len() {
            // Front expanded: its tree holds the start half of the trail.
            if let Some(m) = expand_layer(&mut front, &back, best)? {
                meet = Some(meet_trail(
                    &front.tree,
                    m.side_at,
                    m.rid,
                    &back.tree,
                    m.other_at,
                ));
            }
        } else {
            // Back expanded: its tree holds the end half of the trail.
            if let Some(m) = expand_layer(&mut back, &front, best)? {
                meet = Some(meet_trail(
                    &front.tree,
                    m.other_at,
                    m.rid,
                    &back.tree,
                    m.side_at,
                ));
            }
        }
    }
    Ok(meet)
}

/// A meet of the two searches, recorded un-oriented by the expansion that
/// found it: its length, the node of the expanded side it joins from, the
/// joining relationship, and the node of the other side it joins at.
struct Meet {
    len: usize,
    side_at: NodeId,
    rid: RelId,
    other_at: NodeId,
}

/// The start → end trail through a meet: the front tree's trail to
/// `front_at`, the joining relationship, then the back tree's trail to
/// `back_at` read backwards.
fn meet_trail(
    front: &Tree,
    front_at: NodeId,
    rid: RelId,
    back: &Tree,
    back_at: NodeId,
) -> Vec<RelId> {
    let mut trail = trail_to(front, front_at);
    trail.push(rid);
    let mut rest = trail_to(back, back_at);
    rest.reverse();
    trail.extend(rest);
    trail
}

/// Expand `side`'s next layer, returning the best meet it found: one
/// strictly shorter than `best` (the current best meet's length), recorded
/// un-oriented — the caller knows which side it expanded and assembles the
/// trail.
fn expand_layer(
    side: &mut Side<'_, '_>,
    other: &Side<'_, '_>,
    best: Option<usize>,
) -> Result<Option<Meet>> {
    let Some(layer) = side.depth() else {
        return Ok(None);
    };
    let max = side.search.max;
    let Side { search, q, tree } = side;
    let mut meet = None;
    while let Some(&(cur, d)) = q.front() {
        if d != layer {
            break;
        }
        q.pop_front();
        search.graph.cancel_tick()?;
        let cur_parent = parent_rel(tree, cur);
        search.each_hop(cur, |next, rid| {
            if cur_parent == Some(rid) {
                return Ok(());
            }
            if let Some(&(od, _)) = other.tree.get(&next) {
                let len = (layer + 1) as usize + od as usize;
                let gate = meet.as_ref().map_or(best, |m: &Meet| Some(m.len));
                if parent_rel(&other.tree, next) != Some(rid)
                    && len <= max as usize
                    && gate.is_none_or(|b| len < b)
                {
                    // Reordering keeps a trail's relationships, so the
                    // un-oriented halves settle uniqueness here.
                    let trail =
                        [trail_to(tree, cur), vec![rid], trail_to(&other.tree, next)].concat();
                    if unique_rels(&trail) {
                        meet = Some(Meet {
                            len,
                            side_at: cur,
                            rid,
                            other_at: next,
                        });
                    }
                }
            }
            if let Entry::Vacant(v) = tree.entry(next) {
                v.insert((layer + 1, Some((cur, rid))));
                q.push_back((next, layer + 1));
            }
            Ok(())
        })?;
    }
    Ok(meet)
}
