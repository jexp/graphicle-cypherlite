//! Path selectors (`ANY`/`SHORTEST`, per-endpoint partitioning and
//! groups) and path-count resolution.

use std::collections::{BTreeSet, HashMap};

use super::*;
use cypherlite_core::Error;
use cypherlite_ir::{PathCount, PathSelector};

pub(crate) fn resolve_gpm_count(
    count: Option<&PathCount>,
    default: u32,
    params: &BTreeMap<String, Value>,
) -> Result<usize> {
    match count {
        None => Ok(default as usize),
        Some(PathCount::Literal(value)) => Ok(*value as usize),
        Some(PathCount::Parameter(name)) => match params.get(name) {
            Some(Value::Int(value)) if *value > 0 => Ok(*value as usize),
            Some(Value::Int(_)) => Err(Error::ty(
                "NegativeIntegerArgument: path selector count must be positive",
            )),
            Some(_) => Err(Error::invalid_argument_type(
                "path selector count must be an integer",
            )),
            None => Err(Error::parameter_missing(format!(
                "path selector parameter ${name} was not supplied"
            ))),
        },
    }
}

/// One partitioned state (F-1-1: the selector's member tuple grew a
/// fourth positional field; a named struct keeps the fields honest).
#[derive(Clone)]
struct Member {
    /// Position in the input walk list (the enumeration order).
    index: usize,
    /// Walk length (rel count) — the primary selector key.
    rels_len: usize,
    /// The rel-type sequence as interned indices, for the same-length
    /// same-type tie. The interner assigns indices in sorted name order,
    /// so a `Vec<u32>` compares lexicographically exactly as the rel-type
    /// name sequence would (and the old NUL-joined key did), without
    /// cloning a `String` per relationship of every enumerated walk.
    type_key: Vec<u32>,
    /// The walk's node-id sequence, for the (cost, node-id) tie-break.
    node_seq: Vec<NodeId>,
}

/// The per-partition action, resolved once before the partition loop
/// (count parameters resolve once, and `All` — handled by the early
/// return in `apply_gpm_selector` — is unrepresentable here).
enum Plan {
    /// Keep the first `count` members in enumeration order.
    Any(usize),
    /// Keep the `count` shortest members.
    ShortestPaths(usize),
    /// Keep every member of the `count` shortest distinct lengths.
    ShortestGroups(usize),
}

pub(crate) fn apply_gpm_selector(
    states: Vec<GpmState>,
    selector: Option<&PathSelector>,
    params: &BTreeMap<String, Value>,
) -> Result<Vec<GpmState>> {
    let Some(selector) = selector else {
        return Ok(states);
    };
    // `All` keeps every state — no partition map at all (F-1-2: it used to
    // build the whole map, clone every state's node sequence, and then
    // return the input unchanged).
    let plan = match selector {
        PathSelector::All => return Ok(states),
        PathSelector::Any(count) => Plan::Any(resolve_gpm_count(count.as_ref(), 1, params)?),
        PathSelector::AnyShortest => Plan::ShortestPaths(1),
        PathSelector::AllShortest => Plan::ShortestGroups(1),
        PathSelector::Shortest { count, group } => {
            let count = resolve_gpm_count(count.as_ref(), 1, params)?;
            if *group {
                Plan::ShortestGroups(count)
            } else {
                Plan::ShortestPaths(count)
            }
        }
    };
    // Intern the rel-type names once per select call: a BTreeSet yields
    // them sorted, so the index order matches the name order and the
    // per-walk key is a cheap-to-copy Vec<u32> instead of cloned Strings.
    let names: BTreeSet<&str> = states
        .iter()
        .flat_map(|state| state.rels.iter().map(|rel| rel.rel_type.as_str()))
        .collect();
    let interner: HashMap<&str, u32> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (*name, index as u32))
        .collect();
    let mut partitions = BTreeMap::<(NodeId, NodeId), Vec<Member>>::new();
    for (index, state) in states.iter().enumerate() {
        let key = (
            state.nodes.first().map(|node| node.id).unwrap_or(0),
            state.nodes.last().map(|node| node.id).unwrap_or(0),
        );
        partitions.entry(key).or_default().push(Member {
            index,
            rels_len: state.rels.len(),
            type_key: state
                .rels
                .iter()
                .map(|relationship| interner[relationship.rel_type.as_str()])
                .collect(),
            node_seq: state.nodes.iter().map(|node| node.id).collect(),
        });
    }
    let mut keep = vec![false; states.len()];
    // Every plan only marks members into the shared flat `keep` vector, so
    // cross-partition iteration order cannot affect the result — plain
    // `values()` (BTreeMap key order) is fine.
    for members in partitions.values() {
        match plan {
            Plan::Any(count) => {
                for member in members.iter().take(count) {
                    keep[member.index] = true;
                }
            }
            Plan::ShortestPaths(count) => select_shortest_paths(members, count, &mut keep),
            Plan::ShortestGroups(count) => {
                select_shortest_length_groups(members, count, &mut keep);
            }
        }
    }
    Ok(states
        .into_iter()
        .zip(keep)
        .filter_map(|(state, keep)| keep.then_some(state))
        .collect())
}

/// The shared ordering for both selectors: by rel count, then the rel-type
/// tiebreak, then the walk's node-id sequence, then the stable position.
/// The node-sequence key reproduces the reference's equal-length tie order
/// (ADR-0203): Neo4j's stateful shortest path pops its priority queue by
/// cost and then by node id, so among equal-length, equal-type walks it is
/// the lexicographically smallest node-id sequence that is selected — e.g.
/// CIP-73's `SHORTEST 1 WALK` closed walk returns `[1, 2, 1]`, not the
/// walk the (newest-first) expansion enumerated first. Expansion order
/// still decides fully tied members (parallel relationships share the node
/// sequence), and `PathSelector::Any` keeps pure enumeration order.
fn sorted_members(members: &[Member]) -> Vec<&Member> {
    let mut sorted: Vec<&Member> = members.iter().collect();
    sorted.sort_by(|left, right| {
        left.rels_len
            .cmp(&right.rels_len)
            .then_with(|| left.type_key.cmp(&right.type_key))
            .then_with(|| left.node_seq.cmp(&right.node_seq))
            .then_with(|| left.index.cmp(&right.index))
    });
    sorted
}

/// Keep the `count` shortest members.
fn select_shortest_paths(members: &[Member], count: usize, keep: &mut [bool]) {
    for member in sorted_members(members).into_iter().take(count) {
        keep[member.index] = true;
    }
}

/// Keep every member of the `count` shortest distinct lengths.
fn select_shortest_length_groups(members: &[Member], count: usize, keep: &mut [bool]) {
    // sorted_members orders by rels_len, so the `count` shortest distinct
    // lengths are exactly the first `count` equal-length chunks; a zero
    // count or an empty input marks nothing.
    let sorted = sorted_members(members);
    for chunk in sorted
        .chunk_by(|left, right| left.rels_len == right.rels_len)
        .take(count)
    {
        for member in chunk {
            keep[member.index] = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(index: usize, len: usize, type_index: u32, nodes: &[NodeId]) -> Member {
        Member {
            index,
            rels_len: len,
            type_key: vec![type_index; len],
            node_seq: nodes.to_vec(),
        }
    }

    /// CIP-73 [14] (@neo4j-verified): equal-length, equal-type closed walks
    /// `[1,2,1]` / `[1,3,1]` / `[1,4,1]` — the reference selects the
    /// lexicographically smallest node-id sequence regardless of the
    /// (newest-first) enumeration order (ADR-0203, LABS-255/LABS-258).
    #[test]
    fn equal_length_ties_follow_the_node_id_sequence() {
        let members = vec![
            member(0, 2, 0, &[0, 3, 0]), // [1,4,1], enumerated first
            member(1, 2, 0, &[0, 2, 0]), // [1,3,1]
            member(2, 2, 0, &[0, 1, 0]), // [1,2,1]
        ];
        let mut keep = vec![false; 3];
        select_shortest_paths(&members, 1, &mut keep);
        assert_eq!(keep, vec![false, false, true]);

        // Length still leads: a longer path never wins the tie-break, and
        // fully tied members (parallel relationships: same nodes) keep
        // enumeration order, which is the newest-first expansion order.
        let members = vec![
            member(0, 2, 0, &[0, 1, 0]),
            member(1, 3, 0, &[0, 0, 0, 0]),
            member(2, 2, 0, &[0, 1, 0]),
        ];
        let mut keep = vec![false; 3];
        select_shortest_paths(&members, 1, &mut keep);
        assert_eq!(keep, vec![true, false, false]);
    }
}
