use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

pub use cypherlite_model::value::PointValue;
pub use cypherlite_model::VectorCoordKind;

use crate::temporal::Temporal;
use crate::{NodeId, RelId};

mod json;
mod props;
pub use json::{tagged_str, TaggedStr};
pub use props::PropertyMap;

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: NodeId,
    pub labels: Arc<Vec<String>>,
    pub properties: Arc<PropertyMap>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relationship {
    pub id: RelId,
    pub start: NodeId,
    pub end: NodeId,
    pub rel_type: String,
    pub properties: Arc<PropertyMap>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    pub nodes: Vec<Node>,
    pub rels: Vec<Relationship>,
}

/// OpenCypher runtime value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(Arc<str>),
    List(Arc<Vec<Value>>),
    Map(Arc<BTreeMap<String, Value>>),
    Node(Arc<Node>),
    Relationship(Arc<Relationship>),
    Path(Arc<Path>),
    Temporal(Temporal),
    Point(PointValue),
    /// CIP-246 UUID: 128-bit value, rendered as the canonical lowercase
    /// hyphenated string (`to_display_string`/`iso`-style bare form) and
    /// constructible from a string or two 64-bit halves.
    Uuid(u128),
    Vector {
        coord: VectorCoordKind,
        values: Arc<Vec<Value>>,
    },
    /// A byte array (ADR-0327). Statement-scoped: it enters only as a query
    /// parameter (Bolt PackStream Bytes) and flows through expressions,
    /// lists and maps into results; storing it as a property fails with a
    /// typed `TypeError`. Shared payload like `String` (ADR-0170).
    Bytes(Arc<[u8]>),
}

// Query results cross worker threads, so the shared payloads must keep
// `Value: Send + Sync` (ADR-0170).
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Value>();
};

/// Whether the summed logical bytes (ADR-0042, as
/// [`Value::logical_bytes`]) of `values` stay within `limit`, or `None`
/// once the running total exceeds it. The one early-stopping logical-bytes
/// walk (ADR-0341): the total stops being computed as soon as it exceeds
/// `limit` instead of walking every value.
///
/// Shared payloads (ADR-0170) still count once per reference: many rows
/// holding one large list are small in memory but large in logical bytes,
/// and an unbounded sum over them does work proportional to the product.
/// The Bolt server sums its open results against one connection-wide cap
/// with it (ADR-0311).
pub fn logical_bytes_within<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    limit: u64,
) -> Option<u64> {
    let mut total = 0;
    let over = values
        .into_iter()
        .try_for_each(|v| v.add_logical_bytes(&mut total, limit))
        .is_break();
    (!over).then_some(total)
}

/// Adds `bytes` to `acc` (saturating) and breaks once `acc` exceeds `budget`.
/// Three-valued AND over element-wise `cypher_eq` results: any false pair
/// yields false; otherwise any null pair yields null. Shared by the List
/// and Map arms of `cypher_eq`.
fn cypher_eq_pairs<'a>(pairs: impl Iterator<Item = (&'a Value, &'a Value)>) -> Value {
    let mut saw_null = false;
    for (x, y) in pairs {
        match x.cypher_eq(y) {
            Value::Bool(false) => return Value::Bool(false),
            Value::Null => saw_null = true,
            _ => {}
        }
    }
    if saw_null {
        Value::Null
    } else {
        Value::Bool(true)
    }
}

fn add_bytes(acc: &mut u64, bytes: u64, budget: u64) -> ControlFlow<()> {
    *acc = acc.saturating_add(bytes);
    if *acc > budget {
        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

/// Add one string's logical size — [`str_logical_bytes`] is the one
/// spelling of the size; this is only its budgeted accumulation.
fn add_str_bytes(s: &str, acc: &mut u64, budget: u64) -> ControlFlow<()> {
    add_bytes(acc, str_logical_bytes(s), budget)
}

/// Add one byte payload's logical size (`Value::Bytes`): header 16 plus
/// its length — the same shape [`str_logical_bytes`] spells for a
/// string's UTF-8 bytes.
fn add_payload_bytes(payload: &[u8], acc: &mut u64, budget: u64) -> ControlFlow<()> {
    add_bytes(acc, 16 + payload.len() as u64, budget)
}

fn add_property_bytes<'a>(
    props: impl IntoIterator<Item = (&'a String, &'a Value)>,
    acc: &mut u64,
    budget: u64,
) -> ControlFlow<()> {
    props.into_iter().try_for_each(|(k, v)| {
        add_str_bytes(k, acc, budget)?;
        v.add_logical_bytes(acc, budget)
    })
}

fn add_node_bytes(n: &Node, acc: &mut u64, budget: u64) -> ControlFlow<()> {
    n.labels
        .iter()
        .try_for_each(|l| add_str_bytes(l, acc, budget))?;
    add_property_bytes(n.properties.iter(), acc, budget)
}

fn add_rel_bytes(r: &Relationship, acc: &mut u64, budget: u64) -> ControlFlow<()> {
    add_str_bytes(&r.rel_type, acc, budget)?;
    add_property_bytes(r.properties.iter(), acc, budget)
}

/// A string's documented logical size (ADR-0042): the 16-byte header
/// plus its UTF-8 bytes — the one spelling every string-size sum reads
/// (row keys, node and relationship property keys), beside
/// [`Value::logical_bytes`].
pub fn str_logical_bytes(s: &str) -> u64 {
    16 + s.len() as u64
}

impl Value {
    /// Share one node across every row that clones this value.
    pub fn from_node(node: Node) -> Self {
        Value::Node(Arc::new(node))
    }

    /// Share one relationship across every row that clones this value.
    pub fn from_relationship(rel: Relationship) -> Self {
        Value::Relationship(Arc::new(rel))
    }

    /// Share one path across every row that clones this value.
    pub fn from_path(path: Path) -> Self {
        Value::Path(Arc::new(path))
    }

    /// Documented logical payload size (ADR-0042) — the per-variant
    /// accumulation of the sizes [`str_logical_bytes`] spells for
    /// strings.
    pub fn logical_bytes(&self) -> u64 {
        let mut total = 0;
        let _ = self.add_logical_bytes(&mut total, u64::MAX);
        total
    }

    /// The single implementation of the ADR-0042 formula, shared by
    /// [`Value::logical_bytes`] (budget `u64::MAX`, never breaks) and
    /// [`logical_bytes_within`]: adds this value's logical bytes to `acc`
    /// and breaks as soon as `acc` exceeds `budget`.
    fn add_logical_bytes(&self, acc: &mut u64, budget: u64) -> ControlFlow<()> {
        match self {
            Value::Null | Value::Bool(_) => add_bytes(acc, 1, budget),
            Value::Int(_) | Value::Float(_) => add_bytes(acc, 8, budget),
            Value::String(s) => add_str_bytes(s, acc, budget),
            Value::Bytes(b) => add_payload_bytes(b, acc, budget),
            Value::List(xs) | Value::Vector { values: xs, .. } => {
                add_bytes(acc, 16, budget)?;
                xs.iter().try_for_each(|x| x.add_logical_bytes(acc, budget))
            }
            Value::Map(m) => {
                add_bytes(acc, 16, budget)?;
                add_property_bytes(m.iter(), acc, budget)
            }
            Value::Node(n) => add_node_bytes(n, acc, budget),
            Value::Relationship(r) => add_rel_bytes(r, acc, budget),
            Value::Path(p) => {
                p.nodes
                    .iter()
                    .try_for_each(|n| add_node_bytes(n, acc, budget))?;
                p.rels
                    .iter()
                    .try_for_each(|r| add_rel_bytes(r, acc, budget))
            }
            Value::Temporal(_) => add_bytes(acc, 24, budget),
            Value::Point(_) => add_bytes(acc, 32, budget),
            Value::Uuid(_) => add_bytes(acc, 16, budget),
        }
    }

    pub fn as_temporal(&self) -> Option<&Temporal> {
        match self {
            Value::Temporal(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(&**s),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Cypher toBoolean-ish: true/false stay, null stays none.
    pub fn truthy(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Null => None,
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f)
                if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 =>
            {
                Some(*f as i64)
            }
            _ => None,
        }
    }

    /// Cypher equality: numbers compare across int/float; null = x is null.
    /// Lists and maps are three-valued: any `false` pair yields false; otherwise
    /// any `null` pair yields null.
    pub fn cypher_eq(&self, other: &Value) -> Value {
        if self.is_null() || other.is_null() {
            return Value::Null;
        }
        match (self, other) {
            (Value::List(a), Value::List(b)) => {
                if a.len() != b.len() {
                    return Value::Bool(false);
                }
                cypher_eq_pairs(a.iter().zip(b.iter()))
            }
            (Value::Map(a), Value::Map(b)) => {
                if a.len() != b.len() || a.keys().any(|k| !b.contains_key(k)) {
                    return Value::Bool(false);
                }
                cypher_eq_pairs(a.iter().map(|(k, v)| (v, b.get(k).expect("key checked"))))
            }
            _ => Value::Bool(self.eq_non_null(other)),
        }
    }

    pub fn eq_non_null(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            // Exact Int-vs-Float equality: a plain `(a as f64) == b` cast
            // rounds away low bits for `|a| > 2^53`, so `9007199254740993`
            // spuriously equaled `9007199254740992.0` here while the exact
            // equivalence keys (DISTINCT/GROUP BY/property index) did not --
            // an index's presence could change a MATCH/MERGE answer.
            (Value::Int(a), Value::Float(b)) => cypherlite_model::canonical_key::eq_i64_f64(*a, *b),
            (Value::Float(a), Value::Int(b)) => cypherlite_model::canonical_key::eq_i64_f64(*b, *a),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::Uuid(a), Value::Uuid(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.eq_non_null(y))
            }
            (Value::Map(a), Value::Map(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(k, v)| b.get(k).is_some_and(|o| v.eq_non_null(o)))
            }
            (Value::Node(a), Value::Node(b)) => a.id == b.id,
            (Value::Relationship(a), Value::Relationship(b)) => a.id == b.id,
            (Value::Path(a), Value::Path(b)) => a == b,
            (Value::Temporal(a), Value::Temporal(b)) => {
                a.compare(b) == Some(std::cmp::Ordering::Equal)
            }
            (Value::Point(a), Value::Point(b)) => a == b,
            (
                Value::Vector {
                    coord: ac,
                    values: av,
                },
                Value::Vector {
                    coord: bc,
                    values: bv,
                },
            ) => {
                // Elementwise-exact, like the List arm: a derived `av == bv`
                // is variant-strict (`Int(1)` vs `Float(1.0)` differs), which
                // disagreed with the equivalence keys' numeric normalization
                // of vector elements.
                ac == bc
                    && av.len() == bv.len()
                    && av.iter().zip(bv.iter()).all(|(x, y)| x.eq_non_null(y))
            }
            _ => false,
        }
    }

    pub fn compare(&self, other: &Value) -> Option<Ordering> {
        if self.is_null() || other.is_null() {
            return None;
        }
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            // Exact Int-vs-Float ordering, consistent with the equality above
            // (both route through `cypherlite_model::canonical_key`), so `<`/
            // `ORDER BY` and `=` never disagree past `2^53`.
            (Value::Int(a), Value::Float(b)) => {
                cypherlite_model::canonical_key::cmp_i64_f64(*a, *b)
            }
            (Value::Float(a), Value::Int(b)) => {
                cypherlite_model::canonical_key::cmp_i64_f64(*b, *a).map(Ordering::reverse)
            }
            (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
            // Byte-wise, like the List arm: element by element, then length.
            (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            // CIP-246: UUIDs compare by their 128-bit numeric value so `<`/
            // `>` agree with ORDER BY / min / max (the rank puts UUID
            // between BOOLEAN and TEMPORAL). Without this arm `compare`
            // returned None -- i.e. NULL for ordering operators and, via
            // order_by_cmp's `unwrap_or(Equal)`, "all UUIDs equal" for
            // ORDER BY, min, and max.
            (Value::Uuid(a), Value::Uuid(b)) => Some(a.cmp(b)),
            (Value::Temporal(a), Value::Temporal(b)) => a.compare(b),
            (Value::List(a), Value::List(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    match x.compare(y) {
                        Some(Ordering::Equal) => {}
                        other => return other,
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            _ => None,
        }
    }

    /// Total order for ORDER BY / min / max. Comparison operators still use
    /// `compare`, which is three-valued and incomparable across types.
    pub fn order_by_cmp(&self, other: &Value) -> Ordering {
        let ra = self.order_type_rank();
        let rb = other.order_type_rank();
        if ra != rb {
            return ra.cmp(&rb);
        }
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::List(a), Value::List(b)) => {
                for (x, y) in a.iter().zip(b.iter()) {
                    match x.order_by_cmp(y) {
                        Ordering::Equal => {}
                        other => return other,
                    }
                }
                a.len().cmp(&b.len())
            }
            (
                Value::Vector {
                    coord: ac,
                    values: av,
                },
                Value::Vector {
                    coord: bc,
                    values: bv,
                },
            ) => {
                let type_order = ac
                    .order_rank()
                    .cmp(&bc.order_rank())
                    .then_with(|| av.len().cmp(&bv.len()));
                if type_order != Ordering::Equal {
                    return type_order;
                }
                for (a, b) in av.iter().zip(bv.iter()) {
                    let order = a.order_by_cmp(b);
                    if order != Ordering::Equal {
                        return order;
                    }
                }
                Ordering::Equal
            }
            _ => self.compare(other).unwrap_or(Ordering::Equal),
        }
    }

    /// OpenCypher ORDER BY type order (ascending):
    /// MAP < NODE < RELATIONSHIP < LIST < BYTES < PATH < VECTOR < POINT <
    /// STRING < BOOLEAN < UUID < TEMPORAL < NUMBER < NaN < NULL (UUID
    /// between BOOLEAN and TEMPORAL per the CIP-246 amendment; BYTES, a
    /// sequence value in Neo4j, directly after LIST, ADR-0327)
    fn order_type_rank(&self) -> u8 {
        match self {
            Value::Map(_) => 0,
            Value::Node(_) => 1,
            Value::Relationship(_) => 2,
            Value::List(_) => 3,
            Value::Bytes(_) => 4,
            Value::Path(_) => 5,
            Value::Vector { .. } => 6,
            Value::Point(_) => 7,
            Value::String(_) => 8,
            Value::Bool(_) => 9,
            // CIP-246: UUID orders between BOOLEAN and TEMPORAL (live
            // Neo4j 2026.09: true < uuid < 1).
            Value::Uuid(_) => 10,
            Value::Temporal(_) => 11,
            Value::Int(_) => 12,
            Value::Float(f) if f.is_nan() => 14,
            Value::Float(_) => 12,
            Value::Null => 15,
        }
    }

    pub fn to_cypher_literal(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Bool(true) => "true".into(),
            Value::Bool(false) => "false".into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                if f.is_nan() {
                    "NaN".into()
                } else if *f == f64::INFINITY {
                    "Infinity".into()
                } else if *f == f64::NEG_INFINITY {
                    "-Infinity".into()
                } else {
                    let s = format!("{f}");
                    if s.contains('.') || s.contains('e') || s.contains('E') {
                        s
                    } else {
                        format!("{s}.0")
                    }
                }
            }
            Value::String(s) => format!("'{}'", escape_cypher_string(s)),
            Value::Bytes(b) => bytes_as_int_list(b),
            Value::List(xs) => {
                let inner: Vec<_> = xs.iter().map(Value::to_cypher_literal).collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Map(m) => map_to_cypher_literal(m.iter()),
            Value::Node(n) => n.to_cypher_literal(),
            Value::Relationship(r) => r.to_cypher_literal(),
            Value::Path(p) => p.to_cypher_literal(),
            Value::Temporal(t) => format!("'{}'", escape_cypher_string(&t.iso_string())),
            // Round-trippable form like VECTOR: uuid('...') (CIP-246).
            Value::Uuid(u) => format!("uuid('{}')", uuid_hyphenated(*u)),
            Value::Point(p) => p.to_string(),
            Value::Vector { coord, values } => {
                let inner = values
                    .iter()
                    .map(|value| match value {
                        Value::Float(f) if f.abs() >= 1.0e16 => {
                            let rendered = format!("{f:.15e}");
                            rendered.replacen('e', "e+", 1)
                        }
                        _ => value.to_cypher_literal(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "vector([{inner}], {}, {} NOT NULL)",
                    values.len(),
                    coord.canonical_name()
                )
            }
        }
    }

    /// The CIP-271 `toString()` rendering: the cypher-shell display form
    /// rather than the round-trippable literal form — strings are unquoted,
    /// temporals are bare ISO strings, and maps/lists render recursively
    /// with `, ` separators (verified against Neo4j 2026.09.0).
    pub fn to_display_string(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => Value::Float(*f).to_cypher_literal(),
            // Unquoted: Neo4j 2026.09.0 renders toString({a:'x y'}) as
            // "{a: x y}".
            Value::String(s) => String::from(&**s),
            Value::Bytes(b) => bytes_as_int_list(b),
            Value::List(xs) => {
                let inner: Vec<_> = xs.iter().map(Value::to_display_string).collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Map(m) => {
                let inner: Vec<_> = m
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", v.to_display_string()))
                    .collect();
                format!("{{{}}}", inner.join(", "))
            }
            // Labels only, no properties: Neo4j 2026.09.0 renders
            // toString(node-with-props) as "(:X)".
            Value::Node(n) => {
                let labels: String = n.labels.iter().map(|l| format!(":{l}")).collect();
                format!("({labels})")
            }
            Value::Relationship(r) => format!("[:{}]", r.rel_type),
            Value::Path(p) => p.to_cypher_literal(),
            Value::Temporal(t) => t.iso_string(),
            // Bare hyphenated string (verified on 2026.09.0).
            Value::Uuid(u) => uuid_hyphenated(*u),
            Value::Point(pt) => pt.to_string(),
            Value::Vector { coord, values } => {
                let inner: Vec<_> = values.iter().map(Value::to_display_string).collect();
                format!(
                    "vector([{}], {}, {} NOT NULL)",
                    inner.join(", "),
                    values.len(),
                    coord.canonical_name()
                )
            }
        }
    }
}

impl Node {
    pub fn to_cypher_literal(&self) -> String {
        let labels: String = self.labels.iter().map(|l| format!(":{l}")).collect();
        let props = if self.properties.is_empty() {
            String::new()
        } else {
            format!(" {}", map_to_cypher_literal(self.properties.iter()))
        };
        format!("({labels}{props})")
    }
}

impl Relationship {
    pub fn to_cypher_literal(&self) -> String {
        let props = if self.properties.is_empty() {
            String::new()
        } else {
            format!(" {}", map_to_cypher_literal(self.properties.iter()))
        };
        format!("[:{}{}]", self.rel_type, props)
    }
}

impl Path {
    pub fn to_cypher_literal(&self) -> String {
        let mut s = String::from("<");
        for (i, n) in self.nodes.iter().enumerate() {
            s.push_str(&n.to_cypher_literal());
            if let Some(r) = self.rels.get(i) {
                let next = self.nodes.get(i + 1);
                let outgoing = next.is_some_and(|m| r.start == n.id && r.end == m.id);
                let incoming = next.is_some_and(|m| r.end == n.id && r.start == m.id);
                if outgoing {
                    s.push_str(&format!("-{}->", r.to_cypher_literal()));
                } else if incoming {
                    s.push_str(&format!("<-{}-", r.to_cypher_literal()));
                } else {
                    s.push_str(&format!("-{}-", r.to_cypher_literal()));
                }
            }
        }
        s.push('>');
        s
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_cypher_literal())
    }
}

/// Cypher has no byte-array literal: a [`Value::Bytes`] renders as the list
/// of its byte values 0-255 (`[0, 1, 255]`), the same shape it crosses the
/// `value_to_cypher` bridge in (ADR-0327).
fn bytes_as_int_list(bytes: &[u8]) -> String {
    let inner: Vec<_> = bytes.iter().map(u8::to_string).collect();
    format!("[{}]", inner.join(", "))
}

fn escape_cypher_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn map_to_cypher_literal<'a>(m: impl IntoIterator<Item = (&'a String, &'a Value)>) -> String {
    let inner: Vec<_> = m
        .into_iter()
        .map(|(k, v)| format!("{}: {}", k, v.to_cypher_literal()))
        .collect();
    format!("{{{}}}", inner.join(", "))
}

/// Render a 128-bit UUID value as the canonical lowercase hyphenated
/// string (8-4-4-4-12, CIP-246 / RFC 4122).
pub fn uuid_hyphenated(u: u128) -> String {
    let hex = format!("{u:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Parse an RFC-4122-shaped UUID string (case-insensitive, hyphens
/// required, 8-4-4-4-12 groups) into its 128-bit value. `None` on any
/// non-conforming input, mirroring Java's `UUID.fromString` strictness
/// (CIP-246).
pub fn uuid_parse(s: &str) -> Option<u128> {
    // Allocation-free single pass over the original string: `to_digit(16)`
    // accepts both hex cases, so no lowercasing or concatenation is needed.
    // Strictness is unchanged -- exactly five groups of 8-4-4-4-12 hex digits
    // separated by four hyphens, with nothing before or after.
    const GROUP_LEN: [usize; 5] = [8, 4, 4, 4, 12];
    let mut chars = s.chars();
    let mut u: u128 = 0;
    for (i, &len) in GROUP_LEN.iter().enumerate() {
        if i > 0 && chars.next()? != '-' {
            return None;
        }
        for _ in 0..len {
            u = (u << 4) | chars.next()?.to_digit(16)? as u128;
        }
    }
    if chars.next().is_some() {
        return None;
    }
    Some(u)
}

#[cfg(test)]
mod tests;
