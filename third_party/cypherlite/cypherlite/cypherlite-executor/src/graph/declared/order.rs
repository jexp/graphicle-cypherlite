//! RANGE index order keys and postings.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::sync::Arc;

use chrono::{Datelike, NaiveDateTime, NaiveTime, Timelike};
use cypherlite_core::{NodeId, Temporal, Value};
use cypherlite_ir::BinaryOp;
use cypherlite_model::canonical_key::f64_exact_i64;

use crate::group_key::GroupKey;

use super::DrainState;

/// Node ids posted under one key. A unique value stays a single id; a
/// second distinct id promotes the arm to a set.
#[derive(Clone, Debug)]
pub(super) enum Postings {
    One(NodeId),
    Many(HashSet<NodeId>),
}

impl Postings {
    fn insert(&mut self, id: NodeId) {
        match self {
            Postings::One(existing) if *existing == id => {}
            Postings::One(existing) => {
                let mut set = HashSet::with_capacity(2);
                set.insert(*existing);
                set.insert(id);
                *self = Postings::Many(set);
            }
            Postings::Many(set) => {
                set.insert(id);
            }
        }
    }

    /// `true` when the posting is now empty and the caller should drop the key.
    fn remove(&mut self, id: NodeId) -> bool {
        let demote = match self {
            Postings::One(existing) if *existing == id => return true,
            Postings::One(_) => return false,
            Postings::Many(set) => {
                set.remove(&id);
                match set.len() {
                    0 => return true,
                    1 => set.iter().copied().next(),
                    _ => None,
                }
            }
        };
        if let Some(only) = demote {
            *self = Postings::One(only);
        }
        false
    }

    #[cfg(test)]
    pub(super) fn contains(&self, id: NodeId) -> bool {
        match self {
            Postings::One(existing) => *existing == id,
            Postings::Many(set) => set.contains(&id),
        }
    }

    fn len(&self) -> usize {
        match self {
            Postings::One(_) => 1,
            Postings::Many(set) => set.len(),
        }
    }

    fn extend_into(&self, out: &mut HashSet<NodeId>) {
        match self {
            Postings::One(id) => {
                out.insert(*id);
            }
            Postings::Many(set) => out.extend(set.iter().copied()),
        }
    }
}

/// Keys one node was posted under. One key stays inline; a second version
/// of the property promotes the arm to a vec.
#[derive(Clone, Debug)]
pub(super) enum RevKeys<T> {
    One(T),
    Many(Vec<T>),
}

impl<T: PartialEq + Clone> RevKeys<T> {
    fn insert(&mut self, key: T) {
        match self {
            RevKeys::One(existing) if existing == &key => {}
            RevKeys::One(existing) => {
                let first = existing.clone();
                *self = RevKeys::Many(vec![first, key]);
            }
            RevKeys::Many(keys) => {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
    }

    fn into_vec(self) -> Vec<T> {
        match self {
            RevKeys::One(key) => vec![key],
            RevKeys::Many(keys) => keys,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct RangeIndex {
    pub(super) order: BTreeMap<OrderKey, Postings>,
    /// Order keys this node was posted under, so a drain removes it without
    /// scanning every key in the index.
    pub(super) by_node: HashMap<NodeId, RevKeys<OrderKey>>,
    /// Equality postings for values that have no [`OrderKey`] (lists, maps,
    /// points, uuids, and temporals outside the order window).
    eq_only: HashMap<GroupKey, Postings>,
    eq_by_node: HashMap<NodeId, RevKeys<GroupKey>>,
    /// Nodes whose temporal value does not fit an `OrderKey`. A comparison
    /// seek must scan while this is non-empty: serving the index would miss
    /// them.
    pub(super) unbounded: HashSet<NodeId>,
    pub(super) dirty: HashSet<NodeId>,
}

impl DrainState for RangeIndex {
    fn dirty(&self) -> &HashSet<NodeId> {
        &self.dirty
    }
    fn dirty_mut(&mut self) -> &mut HashSet<NodeId> {
        &mut self.dirty
    }
    fn insert(&mut self, id: NodeId, value: &Value) {
        insert_order(self, value, id);
    }
    fn remove_node(&mut self, id: NodeId) {
        order_remove_node(self, id);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum OrderKey {
    Bool(bool),
    Int(i64),
    Float(u64),
    String(Arc<str>),
    Date(i32),
    CivilDate(i64, u32, u32),
    LocalTime(i64),
    Time(i64),
    LocalDateTime(i64),
    DateTime(i64),
    Duration(i64, i64, i128),
    End,
}

#[cfg(test)]
impl RevKeys<OrderKey> {
    pub(super) fn first(&self) -> Option<&OrderKey> {
        match self {
            RevKeys::One(key) => Some(key),
            RevKeys::Many(keys) => keys.first(),
        }
    }
}

/// Ordered comparison, equality, or `IN` against one property.
#[derive(Clone, Debug)]
pub(crate) enum RangeSeek {
    Cmp { op: BinaryOp, bound: Value },
    Eq(Value),
    In(Arc<Vec<Value>>),
}

/// Distance or bbox probe against one point property.
#[derive(Clone, Debug)]
pub(crate) enum SpatialSeek {
    Distance {
        origin: Value,
        max: f64,
        inclusive: bool,
    },
    BBox {
        lo: Value,
        hi: Value,
    },
}

pub(super) fn ord_f64(f: f64) -> u64 {
    let bits = f.to_bits();
    if bits & (1u64 << 63) == 0 {
        bits | (1u64 << 63)
    } else {
        !bits
    }
}

pub(super) fn time_nanos(t: NaiveTime) -> i64 {
    t.num_seconds_from_midnight() as i64 * 1_000_000_000 + t.nanosecond() as i64
}

pub(super) fn dt_nanos(dt: NaiveDateTime) -> Option<i64> {
    dt.and_utc().timestamp_nanos_opt()
}

pub(super) fn order_key(v: &Value) -> Option<OrderKey> {
    match v {
        Value::Bool(b) => Some(OrderKey::Bool(*b)),
        Value::Int(i) => Some(OrderKey::Int(*i)),
        // An exact integral float shares the integer key, so `1` and `1.0`
        // (and `0.0` / `-0.0`) are one point lookup. Non-integral floats
        // stay in the float variant.
        Value::Float(f) if !f.is_nan() => Some(match f64_exact_i64(*f) {
            Some(i) => OrderKey::Int(i),
            None => OrderKey::Float(ord_f64(*f)),
        }),
        Value::String(s) => Some(OrderKey::String(Arc::clone(s))),
        Value::Temporal(Temporal::Date(d)) => Some(OrderKey::Date(d.num_days_from_ce())),
        Value::Temporal(Temporal::CivilDate { year, month, day }) => {
            Some(OrderKey::CivilDate(*year, *month, *day))
        }
        Value::Temporal(Temporal::LocalTime(t)) => Some(OrderKey::LocalTime(time_nanos(*t))),
        Value::Temporal(Temporal::Time { time, offset }) => Some(OrderKey::Time(
            time_nanos(*time) - i64::from(*offset) * 1_000_000_000,
        )),
        Value::Temporal(Temporal::LocalDateTime(dt)) => dt_nanos(*dt).map(OrderKey::LocalDateTime),
        Value::Temporal(Temporal::DateTime { utc, .. }) => dt_nanos(*utc).map(OrderKey::DateTime),
        Value::Temporal(Temporal::Duration {
            months,
            days,
            nanos,
        }) => Some(OrderKey::Duration(*months, *days, *nanos)),
        _ => None,
    }
}

// Payload-free lower-bound constructor per `OrderKey` variant: the single
// definition of each variant boundary sentinel. `variant_lower`,
// `variant_upper` and the range scans below all build on these, so no call
// site re-states a sentinel inline.
fn bool_lower() -> OrderKey {
    OrderKey::Bool(false)
}

fn int_lower() -> OrderKey {
    OrderKey::Int(i64::MIN)
}

fn float_lower() -> OrderKey {
    OrderKey::Float(ord_f64(f64::NEG_INFINITY))
}

fn string_lower() -> OrderKey {
    OrderKey::String(Arc::from(""))
}

fn date_lower() -> OrderKey {
    OrderKey::Date(i32::MIN)
}

fn civil_date_lower() -> OrderKey {
    OrderKey::CivilDate(i64::MIN, 0, 0)
}

fn local_time_lower() -> OrderKey {
    OrderKey::LocalTime(i64::MIN)
}

fn time_lower() -> OrderKey {
    OrderKey::Time(i64::MIN)
}

fn local_date_time_lower() -> OrderKey {
    OrderKey::LocalDateTime(i64::MIN)
}

fn date_time_lower() -> OrderKey {
    OrderKey::DateTime(i64::MIN)
}

fn duration_lower() -> OrderKey {
    OrderKey::Duration(i64::MIN, i64::MIN, i128::MIN)
}

pub(super) fn variant_lower(k: &OrderKey) -> OrderKey {
    match k {
        OrderKey::Bool(_) => bool_lower(),
        OrderKey::Int(_) => int_lower(),
        OrderKey::Float(_) => float_lower(),
        OrderKey::String(_) => string_lower(),
        OrderKey::Date(_) => date_lower(),
        OrderKey::CivilDate(_, _, _) => civil_date_lower(),
        OrderKey::LocalTime(_) => local_time_lower(),
        OrderKey::Time(_) => time_lower(),
        OrderKey::LocalDateTime(_) => local_date_time_lower(),
        OrderKey::DateTime(_) => date_time_lower(),
        OrderKey::Duration(_, _, _) => duration_lower(),
        OrderKey::End => OrderKey::End,
    }
}

/// First key past `k`'s variant: the next variant's lower bound.
pub(super) fn variant_upper(k: &OrderKey) -> OrderKey {
    match k {
        OrderKey::Bool(_) => int_lower(),
        OrderKey::Int(_) => float_lower(),
        OrderKey::Float(_) => string_lower(),
        OrderKey::String(_) => date_lower(),
        OrderKey::Date(_) => civil_date_lower(),
        OrderKey::CivilDate(_, _, _) => local_time_lower(),
        OrderKey::LocalTime(_) => time_lower(),
        OrderKey::Time(_) => local_date_time_lower(),
        OrderKey::LocalDateTime(_) => date_time_lower(),
        OrderKey::DateTime(_) => duration_lower(),
        OrderKey::Duration(_, _, _) | OrderKey::End => OrderKey::End,
    }
}

pub(super) fn prefix_successor(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(c) = chars.pop() {
        if let Some(next) = char::from_u32(u32::from(c).saturating_add(1)) {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

pub(super) fn unorderable_temporal(value: &Value) -> bool {
    matches!(value, Value::Temporal(_)) && order_key(value).is_none()
}

fn remember<T: PartialEq + Clone>(map: &mut HashMap<NodeId, RevKeys<T>>, id: NodeId, key: T) {
    match map.entry(id) {
        std::collections::hash_map::Entry::Occupied(mut entry) => entry.get_mut().insert(key),
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(RevKeys::One(key));
        }
    }
}

trait ForwardPostings {
    type Key;
    fn unpost(&mut self, key: &Self::Key, id: NodeId);
}

impl<K: Ord> ForwardPostings for BTreeMap<K, Postings> {
    type Key = K;
    fn unpost(&mut self, key: &K, id: NodeId) {
        let empty = self
            .get_mut(key)
            .is_some_and(|postings| postings.remove(id));
        if empty {
            self.remove(key);
        }
    }
}

impl<K: Eq + std::hash::Hash> ForwardPostings for HashMap<K, Postings> {
    type Key = K;
    fn unpost(&mut self, key: &K, id: NodeId) {
        let empty = self
            .get_mut(key)
            .is_some_and(|postings| postings.remove(id));
        if empty {
            self.remove(key);
        }
    }
}

fn forget<K, M>(reverse: &mut HashMap<NodeId, RevKeys<K>>, forward: &mut M, id: NodeId)
where
    K: PartialEq + Clone,
    M: ForwardPostings<Key = K>,
{
    if let Some(keys) = reverse.remove(&id) {
        for key in keys.into_vec() {
            forward.unpost(&key, id);
        }
    }
}

pub(super) fn insert_order(st: &mut RangeIndex, value: &Value, id: NodeId) {
    if let Some(k) = order_key(value) {
        remember(&mut st.by_node, id, k.clone());
        st.order.entry(k).or_insert(Postings::One(id)).insert(id);
        return;
    }
    if unorderable_temporal(value) {
        st.unbounded.insert(id);
    }
    if let Some(gk) = GroupKey::for_property_index(value) {
        remember(&mut st.eq_by_node, id, gk.clone());
        st.eq_only.entry(gk).or_insert(Postings::One(id)).insert(id);
    }
}

pub(super) fn order_remove_node(st: &mut RangeIndex, id: NodeId) {
    st.unbounded.remove(&id);
    forget(&mut st.by_node, &mut st.order, id);
    forget(&mut st.eq_by_node, &mut st.eq_only, id);
}

/// Ids equal to `value` under the range index: one order-key probe for
/// values that sort, otherwise the side map. Exact integral floats share
/// the integer key, so this is a point lookup for `1` and `1.0`.
pub(super) fn equality_ids(st: &RangeIndex, value: &Value) -> HashSet<NodeId> {
    let mut out = HashSet::new();
    if let Some(postings) = equality_postings(st, value) {
        postings.extend_into(&mut out);
    }
    out
}

/// Posting length for one equality value. Zero when the value is not posted.
/// This does not allocate the id set.
pub(super) fn equality_count(st: &RangeIndex, value: &Value) -> usize {
    equality_postings(st, value).map(Postings::len).unwrap_or(0)
}

fn equality_postings<'a>(st: &'a RangeIndex, value: &Value) -> Option<&'a Postings> {
    if let Some(k) = order_key(value) {
        return st.order.get(&k);
    }
    let gk = GroupKey::for_property_index(value)?;
    st.eq_only.get(&gk)
}

pub(super) fn collect_range(
    order: &BTreeMap<OrderKey, Postings>,
    start: Bound<OrderKey>,
    end: Bound<OrderKey>,
    out: &mut HashSet<NodeId>,
) {
    for (_, ids) in order.range((start, end)) {
        ids.extend_into(out);
    }
}

pub(super) fn numeric_cross_start(bound: f64, greater: bool) -> OrderKey {
    let edge = if greater {
        bound.next_down()
    } else {
        bound.next_up()
    };
    OrderKey::Float(ord_f64(if edge.is_finite() { edge } else { bound }))
}

/// Superset of values on the far side of a numeric bound, covering both
/// ints and floats. Residual comparison drops the extra ids.
pub(super) fn numeric_ids(
    order: &BTreeMap<OrderKey, Postings>,
    op: BinaryOp,
    bound: &Value,
) -> HashSet<NodeId> {
    let mut out = HashSet::new();
    visit_numeric_postings(order, op, bound, |ids| ids.extend_into(&mut out));
    out
}

/// Upper bound on [`numeric_ids`], summed from posting lengths. A node posted
/// under more than one visited key is counted once per key. No id set is built.
pub(super) fn numeric_count(
    order: &BTreeMap<OrderKey, Postings>,
    op: BinaryOp,
    bound: &Value,
) -> usize {
    let mut count = 0usize;
    visit_numeric_postings(order, op, bound, |ids| {
        count = count.saturating_add(ids.len());
    });
    count
}

fn visit_numeric_postings(
    order: &BTreeMap<OrderKey, Postings>,
    op: BinaryOp,
    bound: &Value,
    mut visit: impl FnMut(&Postings),
) {
    let Some(key) = order_key(bound) else {
        return;
    };
    let (greater, inclusive) = match op {
        BinaryOp::Gt => (true, false),
        BinaryOp::Gte => (true, true),
        BinaryOp::Lt => (false, false),
        BinaryOp::Lte => (false, true),
        _ => return,
    };
    let start = if greater {
        if inclusive {
            Bound::Included(key.clone())
        } else {
            Bound::Excluded(key.clone())
        }
    } else {
        Bound::Included(variant_lower(&key))
    };
    let end = if greater {
        Bound::Excluded(variant_upper(&key))
    } else if inclusive {
        Bound::Included(key.clone())
    } else {
        Bound::Excluded(key.clone())
    };
    let mut scan = |start, end| {
        for (_, ids) in order.range((start, end)) {
            visit(ids);
        }
    };
    scan(start, end);

    let as_f = match bound {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) if f.is_finite() => Some(*f),
        _ => None,
    };
    if let Some(f) = as_f {
        if greater {
            let cross_start = numeric_cross_start(f, true);
            let float_end = Bound::Excluded(variant_upper(&cross_start));
            scan(Bound::Included(cross_start), float_end);
        } else {
            let cross_end = numeric_cross_start(f, false);
            let float_min = Bound::Included(variant_lower(&cross_end));
            scan(float_min, Bound::Included(cross_end));
        }
        if matches!(bound, Value::Float(_)) {
            if greater {
                let start_i = (f.floor() as i64).saturating_sub(1);
                let int_start = OrderKey::Int(start_i);
                let int_end = Bound::Excluded(variant_upper(&int_start));
                scan(Bound::Included(int_start), int_end);
            } else {
                let end_i = (f.ceil() as i64).saturating_add(1);
                let int_end = OrderKey::Int(end_i);
                let int_min = Bound::Included(variant_lower(&int_end));
                scan(int_min, Bound::Included(int_end));
            }
        }
    }
}

pub(super) fn prefix_ids(order: &BTreeMap<OrderKey, Postings>, prefix: &str) -> HashSet<NodeId> {
    let mut out = HashSet::new();
    let start = OrderKey::String(Arc::from(prefix));
    let end = match prefix_successor(prefix) {
        Some(s) => Bound::Excluded(OrderKey::String(Arc::from(s))),
        // No successor prefix: scan to the end of the String variant.
        None => Bound::Excluded(variant_upper(&start)),
    };
    collect_range(order, Bound::Included(start), end, &mut out);
    out
}

pub(super) fn cmp_holds(v: &Value, op: BinaryOp, bound: &Value) -> bool {
    let Some(ord) = v.compare(bound) else {
        return false;
    };
    match op {
        BinaryOp::Lt => ord.is_lt(),
        BinaryOp::Gt => ord.is_gt(),
        BinaryOp::Lte => ord.is_le(),
        BinaryOp::Gte => ord.is_ge(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_without_successor_scans_to_string_variant_end() {
        let mut st = RangeIndex::default();
        insert_order(&mut st, &Value::String("\u{10FFFF}".into()), 10);
        insert_order(&mut st, &Value::String("\u{10FFFF}z".into()), 11);
        insert_order(&mut st, &Value::String("abc".into()), 12);
        let date = Value::Temporal(Temporal::Date(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
        ));
        insert_order(&mut st, &date, 13);
        // Every char is at the max code point, so no successor prefix
        // exists: the scan runs to the end of the String variant.
        assert!(prefix_successor("\u{10FFFF}").is_none());
        let ids = prefix_ids(&st.order, "\u{10FFFF}");
        assert_eq!(ids, HashSet::from([10, 11]));
    }

    #[test]
    fn numeric_cross_type_scan_stays_within_numeric_variants() {
        let mut st = RangeIndex::default();
        insert_order(&mut st, &Value::Int(1), 1);
        insert_order(&mut st, &Value::Int(3), 2);
        insert_order(&mut st, &Value::Float(0.5), 3);
        insert_order(&mut st, &Value::Float(2.5), 4);
        insert_order(&mut st, &Value::Float(3.5), 5);
        insert_order(&mut st, &Value::String("s".into()), 6);
        insert_order(&mut st, &Value::Int(-100), 7);

        // Float bound: the int side of the cross scan runs up to the Float
        // variant lower bound and never reaches the String variant.
        let gt = numeric_ids(&st.order, BinaryOp::Gt, &Value::Float(2.5));
        assert!(gt.is_superset(&HashSet::from([2, 5])));
        assert!(!gt.contains(&6));
        assert!(!gt.contains(&7));

        // Int bound, other direction: the float side runs from the Float
        // variant lower bound and never reaches the String variant.
        let lt = numeric_ids(&st.order, BinaryOp::Lt, &Value::Int(3));
        assert_eq!(lt, HashSet::from([1, 3, 4, 7]));
    }

    #[test]
    fn unique_key_stays_one_posting_until_a_second_node() {
        let mut st = RangeIndex::default();
        insert_order(&mut st, &Value::Int(1), 10);
        assert!(matches!(
            st.order.get(&OrderKey::Int(1)),
            Some(Postings::One(10))
        ));
        assert!(matches!(st.by_node.get(&10), Some(RevKeys::One(_))));
        insert_order(&mut st, &Value::Int(1), 11);
        assert!(matches!(
            st.order.get(&OrderKey::Int(1)),
            Some(Postings::Many(_))
        ));
        assert!(st.order.get(&OrderKey::Int(1)).unwrap().contains(10));
        assert!(st.order.get(&OrderKey::Int(1)).unwrap().contains(11));
    }

    #[test]
    fn second_value_on_one_node_promotes_the_reverse_key() {
        let mut st = RangeIndex::default();
        insert_order(&mut st, &Value::Int(1), 10);
        insert_order(&mut st, &Value::Int(2), 10);
        assert!(matches!(st.by_node.get(&10), Some(RevKeys::Many(keys)) if keys.len() == 2));
        order_remove_node(&mut st, 10);
        assert!(st.order.is_empty());
        assert!(st.by_node.is_empty());
    }

    #[test]
    fn exact_integral_float_shares_the_int_bucket() {
        let mut st = RangeIndex::default();
        insert_order(&mut st, &Value::Int(1), 10);
        insert_order(&mut st, &Value::Float(1.0), 11);
        insert_order(&mut st, &Value::Float(-0.0), 12);
        assert_eq!(
            equality_ids(&st, &Value::Float(1.0)),
            HashSet::from([10, 11])
        );
        assert_eq!(equality_ids(&st, &Value::Int(0)), HashSet::from([12]));
        insert_order(&mut st, &Value::Float(1.5), 13);
        assert!(matches!(
            order_key(&Value::Float(1.5)),
            Some(OrderKey::Float(_))
        ));
        assert_eq!(equality_ids(&st, &Value::Float(1.5)), HashSet::from([13]));
    }

    #[test]
    fn unordered_value_is_found_by_equality() {
        let list = Value::List(vec![Value::Int(1)].into());
        let mut st = RangeIndex::default();
        insert_order(&mut st, &list, 20);
        assert!(st.order.is_empty());
        assert_eq!(equality_ids(&st, &list), HashSet::from([20]));
        order_remove_node(&mut st, 20);
        assert!(equality_ids(&st, &list).is_empty());
    }

    #[test]
    fn numeric_count_sums_posting_lengths() {
        let mut st = RangeIndex::default();
        insert_order(&mut st, &Value::Int(50), 1);
        for id in [2, 3, 4] {
            insert_order(&mut st, &Value::Int(150), id);
        }
        for id in [5, 6] {
            insert_order(&mut st, &Value::Int(250), id);
        }
        let bound = Value::Int(100);
        let count = numeric_count(&st.order, BinaryOp::Gt, &bound);
        let summed: usize = st
            .order
            .range((
                Bound::Excluded(OrderKey::Int(100)),
                Bound::Excluded(variant_upper(&OrderKey::Int(100))),
            ))
            .map(|(_, postings)| postings.len())
            .sum();
        assert_eq!(count, 5);
        assert_eq!(count, summed);
        assert_eq!(numeric_ids(&st.order, BinaryOp::Gt, &bound).len(), count);
        assert_eq!(equality_count(&st, &Value::Int(150)), 3);
    }
}
