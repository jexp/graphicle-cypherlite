//! Canonical equivalence key for `DISTINCT`, `GROUP BY`, `UNION`
//! deduplication, aggregate `DISTINCT` (`count(DISTINCT x)`, ...),
//! `coll.distinct`, and the property index (`graph.rs::prop_index`).
//!
//! Cypher's *equivalence* relation is deliberately **not** the same relation
//! as `=` equality (see `Value::eq_non_null` / `Value::cypher_eq`): `null` is
//! reflexive here (`null ≡ null`) while `null = null` is `null`, and every
//! NaN is its own equivalence class while `NaN = NaN` is false. This module is
//! the executor's single source of truth for that relation: one typed
//! [`GroupKey`] keys the five dedup/grouping sites plus the property index,
//! so `RETURN DISTINCT`, `GROUP BY`, `UNION`, `count(DISTINCT ...)`,
//! `coll.distinct(...)`, and an indexed `{prop: value}` seek can never
//! disagree -- the divergence the old `Value::to_cypher_literal()` string
//! keys had (they rendered `Int(1)` as `"1"` and `Float(1.0)` as `"1.0"`,
//! splitting groups and missing index seeks, while rendering every NaN as
//! `"NaN"`, merging groups).
//!
//! The Int/Float half of the numeric normalization is the one exact rule
//! owned by [`cypherlite_model::canonical_key`] (ground-truthed against live
//! Neo4j) — `norm_f64` normalizes through its [`f64_exact_i64`], the same
//! primitive `=`/`<` evaluation (`Value::eq_non_null`/`compare`) routes
//! through, so an indexed `{prop: value}` seek, a `WHERE` residual, and a
//! `DISTINCT` bucket can never disagree:
//!
//! * numbers compare by **mathematical value**, ignoring the Int/Float
//!   distinction, recursively inside lists, maps, and vectors:
//!   `1 ≡ 1.0`, `0.0 ≡ -0.0`, `{a:[1]} ≡ {a:[1.0]}`.
//! * **every NaN is its own distinct key** -- never equivalent, even to
//!   itself (`UNWIND [0.0/0.0, 0.0/0.0] AS x RETURN DISTINCT x` yields two
//!   rows). This holds for a NaN nested arbitrarily deep inside a
//!   list/map/point too.
//! * strings are byte-identical only; a string never collides with a list
//!   (`'a,b' ≢ ['a','b']`).
//! * nodes/relationships compare by element id; two structurally identical
//!   nodes with different ids stay distinct. Paths key on their node/rel ids.
//! * temporal instants key by instant, mirroring `Temporal::compare`
//!   (`time('12:00+01:00') ≡ time('13:00+02:00')`; a zoned DateTime keys on
//!   its UTC instant, ignoring the zone name).
//! * the **first-seen** occurrence's representation is what a dedup caller
//!   emits -- the key only decides set membership (`[1, 1.0]` → `1`;
//!   `[1.0, 1]` → `1.0`; `RETURN 1 UNION RETURN 1.0` → a single row `1`).
//!
//! The key is a fully-owned, normalized tree (`Norm`) whose `Hash`/`Eq` are
//! derived, so the two can never fall out of sync -- the classic hazard of a
//! hand-written `Hash` paired with a hand-written `Eq`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};

use cypherlite_core::{Temporal, Value, VectorCoordKind};
use cypherlite_model::canonical_key::f64_exact_i64;

/// Process-global monotonically increasing nonce source for NaN keys. Every
/// NaN encountered while a key is built claims a fresh nonce, so two NaNs
/// never collide -- including NaNs nested inside lists/maps/points. The
/// counter's actual values are irrelevant (equal values never reach this
/// path); only the guaranteed uniqueness matters, so `Relaxed` ordering is
/// sufficient.
static NAN_NONCE: AtomicU64 = AtomicU64::new(0);

fn next_nan_nonce() -> u64 {
    NAN_NONCE.fetch_add(1, Ordering::Relaxed)
}

/// A number normalized so that Int/Float and `+0.0`/`-0.0` collapse while
/// every NaN stays unique. The Int/Float collapse is the one exact rule from
/// [`cypherlite_model::canonical_key`]; the NaN nonce is GroupKey-specific.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NormNum {
    /// An exact integer value -- integral floats within `i64` range land here
    /// too, so `Int(1)` and `Float(1.0)` share one key.
    Int(i64),
    /// A non-integral (or out-of-`i64`-range) finite float, or an infinity,
    /// keyed by its IEEE-754 bit pattern. `-0.0`/`+0.0` never reach this arm
    /// (they normalize to `Int(0)`), so distinct bit patterns here always mean
    /// distinct mathematical values.
    Bits(u64),
    /// A NaN, tagged with a per-occurrence nonce so it is equivalent to
    /// nothing, not even another NaN.
    Nan(u64),
}

fn norm_f64(f: f64) -> NormNum {
    if f.is_nan() {
        return NormNum::Nan(next_nan_nonce());
    }
    // The exact Int/Float rule, shared with `=`/`<` evaluation: `-0.0` and
    // `+0.0` are both integral and in range, so both land in `Int(0)` --
    // giving `0.0 ≡ -0.0` for free. Anything without an exact `i64`
    // (non-integral, or at/beyond ±2^63) keys by bit pattern and therefore
    // never spuriously collides with the genuine integer
    // `i64::MAX`/`i64::MIN`.
    match f64_exact_i64(f) {
        Some(i) => NormNum::Int(i),
        None => NormNum::Bits(f.to_bits()),
    }
}

/// Normalized temporal, mirroring `Temporal::compare` (which is what
/// `Value::eq_non_null` uses): `Time` is normalized to nanos-since-midnight
/// UTC so equivalent instants with different offsets share a key, and
/// `DateTime` keys on its UTC instant, ignoring the zone name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NormTemporal {
    Date(NaiveDate),
    CivilDate {
        year: i64,
        month: u32,
        day: u32,
    },
    LocalTime(NaiveTime),
    /// Nanoseconds since midnight UTC (`local time - offset`).
    Time(i64),
    LocalDateTime(NaiveDateTime),
    CivilDateTime {
        year: i64,
        month: u32,
        day: u32,
        time: NaiveTime,
    },
    /// UTC instant (matches `Temporal::compare`, which ignores zone/offset).
    DateTime(NaiveDateTime),
    Duration {
        months: i64,
        days: i64,
        nanos: i128,
    },
}

/// Nanoseconds since midnight, mirroring `Temporal::compare`'s private
/// `time_to_nanos`.
fn time_to_nanos(t: NaiveTime) -> i64 {
    t.num_seconds_from_midnight() as i64 * 1_000_000_000 + t.nanosecond() as i64
}

fn norm_temporal(t: &Temporal) -> NormTemporal {
    match t {
        Temporal::Date(d) => NormTemporal::Date(*d),
        Temporal::CivilDate { year, month, day } => NormTemporal::CivilDate {
            year: *year,
            month: *month,
            day: *day,
        },
        Temporal::LocalTime(tm) => NormTemporal::LocalTime(*tm),
        Temporal::Time { time, offset } => {
            NormTemporal::Time(time_to_nanos(*time) - i64::from(*offset) * 1_000_000_000)
        }
        Temporal::LocalDateTime(dt) => NormTemporal::LocalDateTime(*dt),
        Temporal::CivilDateTime {
            year,
            month,
            day,
            time,
        } => NormTemporal::CivilDateTime {
            year: *year,
            month: *month,
            day: *day,
            time: *time,
        },
        Temporal::DateTime { utc, .. } => NormTemporal::DateTime(*utc),
        Temporal::Duration {
            months,
            days,
            nanos,
        } => NormTemporal::Duration {
            months: *months,
            days: *days,
            nanos: *nanos,
        },
    }
}

/// The normalized equivalence tree. Distinct enum variants carry an implicit
/// type tag, so cross-type collisions (a string vs a list, a bool vs an int)
/// are impossible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Norm {
    /// All nulls share this one key (reflexive), the sole divergence from `=`.
    Null,
    Bool(bool),
    Num(NormNum),
    Str(Arc<str>),
    /// A byte array keys byte-wise (ADR-0327): its own variant, so bytes
    /// never collide with a list of the same integers.
    Bytes(Arc<[u8]>),
    List(Vec<Norm>),
    /// Map entries sorted by key so insertion order does not affect the key;
    /// values keyed recursively, so `{a:1} ≡ {a:1.0}`.
    Map(Vec<(String, Norm)>),
    Node(i64),
    Rel(i64),
    Path(Vec<Norm>, Vec<Norm>),
    Temporal(NormTemporal),
    Point {
        srid: i32,
        coords: Vec<NormNum>,
    },
    Uuid(u128),
    Vector {
        coord: VectorCoordKind,
        values: Vec<Norm>,
    },
}

fn norm(v: &Value) -> Norm {
    match v {
        Value::Null => Norm::Null,
        Value::Bool(b) => Norm::Bool(*b),
        Value::Int(i) => Norm::Num(NormNum::Int(*i)),
        Value::Float(f) => Norm::Num(norm_f64(*f)),
        Value::String(s) => Norm::Str(s.clone()),
        Value::Bytes(b) => Norm::Bytes(b.clone()),
        Value::Uuid(u) => Norm::Uuid(*u),
        Value::List(items) => Norm::List(items.iter().map(norm).collect()),
        Value::Map(m) => {
            let mut entries: Vec<(String, Norm)> =
                m.iter().map(|(k, val)| (k.clone(), norm(val))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Norm::Map(entries)
        }
        Value::Node(n) => Norm::Node(n.id),
        Value::Relationship(r) => Norm::Rel(r.id),
        Value::Path(p) => Norm::Path(
            p.nodes.iter().map(|n| Norm::Node(n.id)).collect(),
            p.rels.iter().map(|r| Norm::Rel(r.id)).collect(),
        ),
        Value::Temporal(t) => Norm::Temporal(norm_temporal(t)),
        Value::Point(p) => {
            let mut coords = vec![norm_f64(p.x), norm_f64(p.y)];
            if let Some(z) = p.z {
                coords.push(norm_f64(z));
            }
            Norm::Point {
                srid: p.srid,
                coords,
            }
        }
        Value::Vector { coord, values } => Norm::Vector {
            // The coordinate kind tag keeps an INT8 vector distinct from a
            // FLOAT32 vector even when their element values coincide
            // numerically (matching `Value::eq_non_null`'s vector arm, which
            // compares `coord`). Elements are normalized numerically, so
            // `vector([1], ...)` ≡ `vector([1.0], ...)` — the same
            // elementwise-exact rule `eq_non_null` applies.
            coord: *coord,
            values: values.iter().map(norm).collect(),
        },
    }
}

/// A canonical equivalence key for a single `Value`, per the module-level
/// contract. `Hash + Eq` implement Cypher's equivalence relation, which is
/// distinct from `=` equality.
///
/// The encoding is **injective** over the equivalence classes: two values map
/// to equal keys **iff** they are Cypher-equivalent (with the deliberate NaN
/// exception, where every occurrence is its own class). This is precisely the
/// property the old `to_cypher_literal`-string key lacked -- there
/// `Int(1)`/`Float(1.0)` collided-wrong-way while `"1,2"`/`[1,2]` and
/// quote-bearing strings aliased.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GroupKey(Norm);

impl GroupKey {
    /// Build the equivalence key for a value. A NaN anywhere inside `value`
    /// claims a fresh nonce at this call, so the resulting key is unique and
    /// two separately-built keys over NaN-containing values never compare
    /// equal.
    pub(crate) fn new(value: &Value) -> Self {
        GroupKey(norm(value))
    }

    /// Key for the property index: the same normalized equivalence as
    /// [`GroupKey::new`], except that a value containing a NaN anywhere is
    /// unindexable (`None`). A NaN never satisfies an equality seek
    /// (`NaN = NaN` is false), so indexing it could only create a bucket no
    /// seek key can ever address -- and, because every NaN key claims a
    /// fresh nonce, one the GC drop path could never recompute to remove.
    pub(crate) fn for_property_index(value: &Value) -> Option<Self> {
        if value_has_nan(value) {
            return None;
        }
        Some(GroupKey::new(value))
    }

    /// Estimated heap footprint of the key, for the ADR-0042 logical index
    /// accounting in `memory.rs` (which charges per index map entry).
    pub(crate) fn logical_bytes(&self) -> u64 {
        norm_bytes(&self.0)
    }
}

/// True if a NaN occurs anywhere inside `v` (recursively, mirroring `norm`).
fn value_has_nan(v: &Value) -> bool {
    match v {
        Value::Float(f) => f.is_nan(),
        Value::List(items) => items.iter().any(value_has_nan),
        Value::Map(m) => m.values().any(value_has_nan),
        Value::Point(p) => p.x.is_nan() || p.y.is_nan() || p.z.is_some_and(|z| z.is_nan()),
        Value::Vector { values, .. } => values.iter().any(value_has_nan),
        _ => false,
    }
}

/// Rough heap size of a normalized key tree: an 8-byte enum tag plus payload.
fn norm_bytes(n: &Norm) -> u64 {
    const TAG: u64 = 8;
    const HDR: u64 = 16; // String/Vec header.
    match n {
        Norm::Null | Norm::Bool(_) => TAG + 1,
        Norm::Num(_) => TAG + 16,
        Norm::Str(s) => TAG + HDR + s.len() as u64,
        Norm::Bytes(b) => TAG + HDR + b.len() as u64,
        Norm::List(items) => TAG + HDR + items.iter().map(norm_bytes).sum::<u64>(),
        Norm::Map(entries) => {
            TAG + HDR
                + entries
                    .iter()
                    .map(|(k, v)| HDR + k.len() as u64 + norm_bytes(v))
                    .sum::<u64>()
        }
        Norm::Node(_) | Norm::Rel(_) => TAG + 8,
        Norm::Path(ns, rs) => TAG + 2 * HDR + (ns.len() + rs.len()) as u64 * (TAG + 8),
        Norm::Temporal(_) => TAG + 24,
        Norm::Uuid(_) => TAG + 16,
        Norm::Point { coords, .. } => TAG + 4 + (TAG + 8) * coords.len() as u64,
        Norm::Vector { values, .. } => TAG + 1 + HDR + values.iter().map(norm_bytes).sum::<u64>(),
    }
}

/// Build the equivalence key for a whole row: its column values in column
/// order. Used by RETURN/WITH `DISTINCT` and `UNION` deduplication, where two
/// rows are the same iff every corresponding column value is equivalent.
pub(crate) fn row_key(vals: &[Value]) -> Vec<GroupKey> {
    vals.iter().map(GroupKey::new).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_core::{Node, PointValue, VectorCoordKind};
    use std::collections::{BTreeMap, HashSet};

    fn key(v: Value) -> GroupKey {
        GroupKey::new(&v)
    }

    fn nan() -> Value {
        Value::Float(f64::NAN)
    }

    #[test]
    fn int_and_integral_float_share_a_key() {
        assert_eq!(key(Value::Int(1)), key(Value::Float(1.0)));
        assert_eq!(key(Value::Int(0)), key(Value::Float(0.0)));
    }

    #[test]
    fn keys_agree_with_eq_non_null_past_2_pow_53() {
        // The equivalence key and `=` must give the same verdict on the
        // boundary where a lossy `as f64` cast goes wrong.
        let big = Value::Int(9_007_199_254_740_993);
        let near = Value::Float(9_007_199_254_740_992.0);
        assert!(!big.eq_non_null(&near));
        assert_ne!(key(big.clone()), key(near.clone()));
        // ... and the float DOES share a key with the integer it denotes.
        let exact = Value::Int(9_007_199_254_740_992);
        assert!(exact.eq_non_null(&near));
        assert_eq!(key(exact), key(near));
    }

    #[test]
    fn vector_keys_agree_with_vector_eq_both_directions() {
        // `=` and the equivalence key must give the same verdict on vectors:
        // numerically equal elements collapse (equal), different values or
        // coordinate kinds do not (unequal).
        let int_backed = Value::Vector {
            coord: VectorCoordKind::Float64,
            values: vec![Value::Int(1)].into(),
        };
        let float_backed = Value::Vector {
            coord: VectorCoordKind::Float64,
            values: vec![Value::Float(1.0)].into(),
        };
        assert!(int_backed.eq_non_null(&float_backed));
        assert_eq!(key(int_backed.clone()), key(float_backed));
        let two = Value::Vector {
            coord: VectorCoordKind::Float64,
            values: vec![Value::Float(2.0)].into(),
        };
        assert!(!int_backed.eq_non_null(&two));
        assert_ne!(key(int_backed.clone()), key(two));
        let other_kind = Value::Vector {
            coord: VectorCoordKind::Int32,
            values: vec![Value::Int(1)].into(),
        };
        assert!(!int_backed.eq_non_null(&other_kind));
        assert_ne!(key(int_backed), key(other_kind));
    }

    #[test]
    fn positive_and_negative_zero_share_a_key() {
        assert_eq!(key(Value::Float(0.0)), key(Value::Float(-0.0)));
        assert_eq!(key(Value::Int(0)), key(Value::Float(-0.0)));
    }

    #[test]
    fn non_integral_floats_key_by_value() {
        assert_eq!(key(Value::Float(1.5)), key(Value::Float(1.5)));
        assert_ne!(key(Value::Float(1.5)), key(Value::Float(2.5)));
        assert_ne!(key(Value::Int(1)), key(Value::Float(1.5)));
    }

    #[test]
    fn every_nan_is_its_own_key() {
        assert_ne!(key(nan()), key(nan()));
        // Two lists each holding a NaN are also never equivalent.
        assert_ne!(
            key(Value::List(vec![nan()].into())),
            key(Value::List(vec![nan()].into()))
        );
    }

    #[test]
    fn float_beyond_i64_range_does_not_collide_with_i64_extremes() {
        // 2^63 is i64::MAX + 1; it must not collapse onto Int(i64::MAX).
        let two_pow_63 = Value::Float(9_223_372_036_854_775_808.0);
        assert_ne!(key(two_pow_63.clone()), key(Value::Int(i64::MAX)));
        // But an in-range integral float (2^62, exactly representable) still
        // collapses onto its integer.
        let two_pow_62: i64 = 4_611_686_018_427_387_904;
        assert_eq!(
            key(Value::Float(two_pow_62 as f64)),
            key(Value::Int(two_pow_62))
        );
    }

    #[test]
    fn strings_never_collide_with_lists() {
        let s = Value::String("a,b".into());
        let l = Value::List(vec![Value::String("a".into()), Value::String("b".into())].into());
        assert_ne!(key(s), key(l));
    }

    #[test]
    fn strings_with_quotes_and_escapes_stay_distinct() {
        // The old to_cypher_literal key rendered these ambiguously; the
        // canonical key keeps the byte content verbatim.
        assert_ne!(
            key(Value::String("a\"b".into())),
            key(Value::String("a'b".into()))
        );
        assert_ne!(
            key(Value::String("a\\b".into())),
            key(Value::String("ab".into()))
        );
    }

    #[test]
    fn bools_never_collide_with_ints() {
        assert_ne!(key(Value::Bool(true)), key(Value::Int(1)));
        assert_ne!(key(Value::Bool(false)), key(Value::Int(0)));
    }

    #[test]
    fn nested_numeric_equivalence_in_lists_and_maps() {
        let a = Value::List(vec![Value::Int(1)].into());
        let b = Value::List(vec![Value::Float(1.0)].into());
        assert_eq!(key(a), key(b));

        let mut m1 = BTreeMap::new();
        m1.insert("a".to_string(), Value::List(vec![Value::Int(1)].into()));
        let mut m2 = BTreeMap::new();
        m2.insert("a".to_string(), Value::List(vec![Value::Float(1.0)].into()));
        assert_eq!(key(Value::Map(m1.into())), key(Value::Map(m2.into())));
    }

    #[test]
    fn map_key_order_does_not_matter() {
        let mut m1 = BTreeMap::new();
        m1.insert("a".to_string(), Value::Int(1));
        m1.insert("b".to_string(), Value::Int(2));
        let mut m2 = BTreeMap::new();
        m2.insert("b".to_string(), Value::Int(2));
        m2.insert("a".to_string(), Value::Int(1));
        assert_eq!(key(Value::Map(m1.into())), key(Value::Map(m2.into())));
    }

    #[test]
    fn nodes_key_by_id() {
        let n1 = Node {
            id: 1,
            labels: Arc::new(vec!["P".to_string()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        };
        let n2 = Node {
            id: 2,
            labels: Arc::new(vec!["P".to_string()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        };
        // Structurally identical but different ids -> distinct.
        assert_ne!(key(Value::from_node(n1.clone())), key(Value::from_node(n2)));
        assert_eq!(key(Value::from_node(n1.clone())), key(Value::from_node(n1)));
    }

    #[test]
    fn time_keys_by_instant_ignoring_offset() {
        // Matches `Temporal::compare` / `eq_non_null`: 12:00+01:00 and
        // 13:00+02:00 denote the same instant.
        let a = Temporal::Time {
            time: NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
            offset: 3600,
        };
        let b = Temporal::Time {
            time: NaiveTime::from_hms_opt(13, 0, 0).unwrap(),
            offset: 7200,
        };
        assert_eq!(
            key(Value::Temporal(a.clone())),
            key(Value::Temporal(b.clone()))
        );
        assert_eq!(a.compare(&b), Some(std::cmp::Ordering::Equal));
    }

    #[test]
    fn datetime_keys_by_utc_instant_ignoring_zone() {
        let utc = NaiveDate::from_ymd_opt(2026, 1, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let a = Temporal::DateTime {
            utc,
            offset: 0,
            zone: Some("UTC".to_string()),
        };
        let b = Temporal::DateTime {
            utc,
            offset: 0,
            zone: Some("Europe/Stockholm".to_string()),
        };
        assert_eq!(key(Value::Temporal(a)), key(Value::Temporal(b)));
    }

    #[test]
    fn vector_elements_normalize_numerically() {
        let a = Value::Vector {
            coord: VectorCoordKind::Float64,
            values: vec![Value::Int(1)].into(),
        };
        let b = Value::Vector {
            coord: VectorCoordKind::Float64,
            values: vec![Value::Float(1.0)].into(),
        };
        assert_eq!(key(a), key(b.clone()));
        // The coordinate kind stays part of the key.
        let c = Value::Vector {
            coord: VectorCoordKind::Int32,
            values: vec![Value::Int(1)].into(),
        };
        assert_ne!(key(b), key(c));
    }

    #[test]
    fn points_equivalent_by_coordinate_value() {
        let p1 = Value::Point(PointValue {
            srid: 7203,
            x: 1.0,
            y: 2.0,
            z: None,
        });
        let p2 = Value::Point(PointValue {
            srid: 7203,
            x: 1.0,
            y: 2.0,
            z: None,
        });
        assert_eq!(key(p1), key(p2.clone()));
        let p3 = Value::Point(PointValue {
            srid: 7203,
            x: 1.0,
            y: 2.5,
            z: None,
        });
        assert_ne!(key(p2), key(p3));
    }

    #[test]
    fn null_is_reflexive() {
        assert_eq!(key(Value::Null), key(Value::Null));
    }

    #[test]
    fn hashset_dedups_by_equivalence() {
        let mut set: HashSet<GroupKey> = HashSet::new();
        set.insert(key(Value::Int(1)));
        set.insert(key(Value::Float(1.0)));
        set.insert(key(Value::Int(2)));
        // 1 and 1.0 collapse; 2 stands alone.
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn row_key_compares_columnwise_by_equivalence() {
        let r1 = vec![Value::Int(1), Value::Float(2.0)];
        let r2 = vec![Value::Float(1.0), Value::Int(2)];
        assert_eq!(row_key(&r1), row_key(&r2));
        let r3 = vec![Value::Int(1), Value::Float(3.0)];
        assert_ne!(row_key(&r1), row_key(&r3));
    }

    #[test]
    fn property_index_key_shares_the_numeric_normalization() {
        // The one canonical numeric key: Int(1) and Float(1.0) collide.
        assert_eq!(
            GroupKey::for_property_index(&Value::Int(1)),
            GroupKey::for_property_index(&Value::Float(1.0))
        );
        assert_eq!(
            GroupKey::for_property_index(&Value::Float(-0.0)),
            GroupKey::for_property_index(&Value::Int(0))
        );
        // ... and it matches the grouping key for NaN-free values.
        assert_eq!(
            GroupKey::for_property_index(&Value::Float(1.0)),
            Some(GroupKey::new(&Value::Int(1)))
        );
    }

    #[test]
    fn property_index_rejects_nan_anywhere() {
        assert_eq!(GroupKey::for_property_index(&nan()), None);
        assert_eq!(
            GroupKey::for_property_index(&Value::List(vec![Value::Int(1), nan()].into())),
            None
        );
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), nan());
        assert_eq!(GroupKey::for_property_index(&Value::Map(m.into())), None);
        let p = Value::Point(PointValue {
            srid: 7203,
            x: f64::NAN,
            y: 2.0,
            z: None,
        });
        assert_eq!(GroupKey::for_property_index(&p), None);
        let v = Value::Vector {
            coord: VectorCoordKind::Float64,
            values: vec![nan()].into(),
        };
        assert_eq!(GroupKey::for_property_index(&v), None);
        // NaN-free values stay indexable.
        assert!(GroupKey::for_property_index(&Value::Int(1)).is_some());
    }

    #[test]
    fn logical_bytes_tracks_payload() {
        let small = GroupKey::new(&Value::Int(1)).logical_bytes();
        let big = GroupKey::new(&Value::String("x".repeat(100).into())).logical_bytes();
        assert!(small > 0);
        assert!(big >= 100);
    }
}
