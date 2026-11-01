//! TEXT index postings (ADR-0207).
//!
//! A string of `C` characters posts its `C - 2` raw width-3 windows and
//! nothing shorter. Each window is a packed `u64`. The node keeps the
//! source `Arc<str>` (shared with the property) so a later removal
//! recomputes those keys instead of storing a second copy of every gram.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cypherlite_core::{NodeId, Value};

use super::DrainState;

/// Sorted, deduplicated node ids posted under one trigram.
#[derive(Debug, Default)]
pub(super) struct PostingList {
    ids: Vec<NodeId>,
    /// False while a bulk build is appending. [`PostingList::ensure_sorted`]
    /// sorts and dedups once; later edits keep that order.
    sorted: bool,
}

impl PostingList {
    fn push_bulk(&mut self, id: NodeId) {
        self.ids.push(id);
        self.sorted = false;
    }

    fn ensure_sorted(&mut self) {
        if self.sorted {
            return;
        }
        self.ids.sort_unstable();
        self.ids.dedup();
        self.sorted = true;
    }

    fn insert_one(&mut self, id: NodeId) {
        self.ensure_sorted();
        if let Err(at) = self.ids.binary_search(&id) {
            self.ids.insert(at, id);
        }
    }

    fn remove_one(&mut self, id: NodeId) {
        self.ensure_sorted();
        if let Ok(at) = self.ids.binary_search(&id) {
            self.ids.remove(at);
        }
    }

    /// Used by the gram-removal test. Release builds do not call it.
    #[cfg(test)]
    pub(super) fn holds(&self, id: NodeId) -> bool {
        if self.sorted {
            self.ids.binary_search(&id).is_ok()
        } else {
            self.ids.contains(&id)
        }
    }
}

#[derive(Debug)]
pub(crate) struct GramIndex {
    pub(super) grams: HashMap<u64, PostingList>,
    /// Source strings posted for this node. Removal recomputes trigrams
    /// from these arcs.
    by_node: HashMap<NodeId, Vec<Arc<str>>>,
    pub(super) dirty: HashSet<NodeId>,
    /// True until [`GramIndex::seal`]. Bulk insert appends; after seal,
    /// each edit binary-searches a sorted list.
    bulk: bool,
}

impl Default for GramIndex {
    fn default() -> Self {
        Self {
            grams: HashMap::new(),
            by_node: HashMap::new(),
            dirty: HashSet::new(),
            bulk: true,
        }
    }
}

impl GramIndex {
    /// Sort every posting list and leave incremental mode. Idempotent.
    pub(super) fn seal(&mut self) {
        self.bulk = false;
        for list in self.grams.values_mut() {
            list.ensure_sorted();
        }
    }

    fn insert_id(&mut self, gram: u64, id: NodeId) {
        let list = self.grams.entry(gram).or_default();
        if self.bulk {
            list.push_bulk(id);
        } else {
            list.insert_one(id);
        }
    }
}

impl DrainState for GramIndex {
    fn dirty(&self) -> &HashSet<NodeId> {
        &self.dirty
    }
    fn dirty_mut(&mut self) -> &mut HashSet<NodeId> {
        &mut self.dirty
    }
    fn insert(&mut self, id: NodeId, value: &Value) {
        if let Value::String(s) = value {
            gram_insert(self, s, id);
        }
    }
    fn remove_node(&mut self, id: NodeId) {
        gram_remove_node(self, id);
    }
}

pub(super) fn gram_remove_node(st: &mut GramIndex, id: NodeId) {
    let Some(sources) = st.by_node.remove(&id) else {
        return;
    };
    st.seal();
    let mut keys = Vec::new();
    for source in &sources {
        keys.extend(trigrams(source));
    }
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let empty = match st.grams.get_mut(&key) {
            Some(list) => {
                list.remove_one(id);
                list.ids.is_empty()
            }
            None => false,
        };
        if empty {
            st.grams.remove(&key);
        }
    }
}

pub(super) fn gram_insert(st: &mut GramIndex, s: &Arc<str>, id: NodeId) {
    let keys = trigrams(s);
    if keys.is_empty() {
        return;
    }
    let sources = st.by_node.entry(id).or_default();
    if !sources
        .iter()
        .any(|existing| Arc::ptr_eq(existing, s) || existing.as_ref() == s.as_ref())
    {
        sources.push(Arc::clone(s));
    }
    for key in keys {
        st.insert_id(key, id);
    }
}

/// Packed width-3 key. Each Unicode scalar fits in 21 bits, so three of
/// them fit in a `u64` with the top bit clear.
pub(super) fn pack_trigram(a: char, b: char, c: char) -> u64 {
    ((a as u64) << 42) | ((b as u64) << 21) | (c as u64)
}

#[cfg(test)]
fn unpack_trigram(key: u64) -> (char, char, char) {
    let mask = (1u64 << 21) - 1;
    let c = char::from_u32((key & mask) as u32).unwrap();
    let b = char::from_u32(((key >> 21) & mask) as u32).unwrap();
    let a = char::from_u32(((key >> 42) & mask) as u32).unwrap();
    (a, b, c)
}

fn trigrams(s: &str) -> Vec<u64> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        return Vec::new();
    }
    chars
        .windows(3)
        .map(|w| pack_trigram(w[0], w[1], w[2]))
        .collect()
}

/// How many trigram postings a string of `char_len` characters contributes.
/// Strings shorter than 3 characters post nothing.
pub(crate) fn trigram_count(char_len: u64) -> u64 {
    char_len.saturating_sub(2)
}

/// Width-3 windows of `needle`, or `None` when the needle is too short to
/// seek. The caller scans in that case. Duplicate windows are dropped so
/// the intersection probes each key once.
pub(super) fn query_grams(needle: &str) -> Option<Vec<u64>> {
    let chars: Vec<char> = needle.chars().collect();
    if chars.len() < 3 {
        return None;
    }
    let mut grams: Vec<u64> = chars
        .windows(3)
        .map(|w| pack_trigram(w[0], w[1], w[2]))
        .collect();
    grams.sort_unstable();
    grams.dedup();
    Some(grams)
}

/// Intersect the posting lists of `grams`, probing from the shortest list.
/// A missing gram matches nothing. Seals the index first.
pub(super) fn intersect_grams(st: &mut GramIndex, grams: &[u64]) -> HashSet<NodeId> {
    st.seal();
    let mut lists: Vec<&[NodeId]> = Vec::with_capacity(grams.len());
    for gram in grams {
        match st.grams.get(gram) {
            Some(list) => lists.push(&list.ids),
            None => return HashSet::new(),
        }
    }
    let Some(shortest) = lists
        .iter()
        .enumerate()
        .min_by_key(|(_, list)| list.len())
        .map(|(index, _)| index)
    else {
        return HashSet::new();
    };
    lists.swap(0, shortest);
    let Some((first, rest)) = lists.split_first() else {
        return HashSet::new();
    };
    first
        .iter()
        .copied()
        .filter(|id| rest.iter().all(|list| list.binary_search(id).is_ok()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_round_trip_covers_non_bmp() {
        let key = pack_trigram('a', '😀', 'b');
        assert_eq!(unpack_trigram(key), ('a', '😀', 'b'));
    }

    #[test]
    fn abcd_posts_only_width_three() {
        let mut st = GramIndex::default();
        gram_insert(&mut st, &Arc::from("abcd"), 1);
        st.seal();
        let keys: HashSet<u64> = st.grams.keys().copied().collect();
        assert_eq!(
            keys,
            HashSet::from([pack_trigram('a', 'b', 'c'), pack_trigram('b', 'c', 'd')])
        );
    }

    #[test]
    fn short_string_posts_nothing() {
        let mut st = GramIndex::default();
        gram_insert(&mut st, &Arc::from("ab"), 1);
        assert!(st.grams.is_empty());
        assert!(st.by_node.is_empty());
    }

    #[test]
    fn source_arc_is_shared_with_the_property() {
        let payload: Arc<str> = Arc::from("abcd");
        let mut st = GramIndex::default();
        gram_insert(&mut st, &payload, 1);
        assert!(Arc::ptr_eq(&st.by_node[&1][0], &payload));
    }

    #[test]
    fn second_insert_does_not_duplicate_the_id() {
        let mut st = GramIndex::default();
        let text: Arc<str> = Arc::from("abcd");
        gram_insert(&mut st, &text, 7);
        gram_insert(&mut st, &text, 7);
        st.seal();
        assert!(st.grams.values().all(|list| list.ids == vec![7]));
        assert_eq!(st.by_node[&7].len(), 1);
    }

    #[test]
    fn query_shorter_than_three_declines() {
        assert!(query_grams("").is_none());
        assert!(query_grams("a").is_none());
        assert!(query_grams("ab").is_none());
        assert_eq!(
            query_grams("abc").unwrap(),
            vec![pack_trigram('a', 'b', 'c')]
        );
        assert_eq!(trigram_count(0), 0);
        assert_eq!(trigram_count(2), 0);
        assert_eq!(trigram_count(10), 8);
    }
}
