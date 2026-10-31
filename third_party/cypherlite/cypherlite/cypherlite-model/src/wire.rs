//! Canonical JSON **wire** encoding shared by every CypherLite surface.
//!
//! This module is the single source of truth for how a [`CypherValue`], a set
//! of mutation counters, and a query response are rendered as JSON on the wire
//! (the C FFI, the CLI, and the shapes the language bindings derive from
//! those). Every surface is expected to route its serialization
//! through here so that a drift in one surface becomes a failing golden test
//! (see the [`golden`] submodule).
//!
//! It is deliberately distinct from the persistence encoding in
//! [`crate::serde_value`] (the `__type`-tagged format used for on-disk
//! round-tripping and for `CypherValue`'s `serde::Serialize` impl). The wire
//! encoding is **self-describing and stable**: it never falls through to a
//! `Display` string for a structured value, and is documented field-by-field
//! below. Entity values (nodes and relationships) carry an explicit
//! `"kind"` discriminator so clients dispatch on a
//! documented field instead of sniffing key sets (ADR-0223, a breaking wire
//! change recorded in the CHANGELOG); the `__type`-tagged persistence
//! format remains a separate encoding.
//!
//! # Value encoding ([`value_to_wire_json`])
//!
//! | Variant | Wire JSON |
//! |---|---|
//! | `Null` | `null` |
//! | `Bool` | `true` / `false` |
//! | `Int` | JSON integer |
//! | `Float` | JSON number (non-finite `NaN`/`±Inf` → `"NaN"`/`"Inf"`/`"-Inf"`, see below) |
//! | `Str` | JSON string |
//! | `List` | JSON array (elements recursively encoded) |
//! | `Map` | JSON object (values recursively encoded) |
//! | `Node` | `{"kind":"node","id","labels","properties"}` |
//! | `Relationship` | `{"kind":"relationship","id","type","start_node","end_node","properties"}` |
//! | `Path` | JSON array alternating `[node, rel, node, rel, …, node]` (no tag of its own; elements carry their `kind`, ADR-0223) |
//! | `Date` | ISO-8601 date string `YYYY-MM-DD` |
//! | `Time` | ISO-8601 time string `HH:MM[:SS[.fff]][±HH:MM[:SS] or Z]` |
//! | `DateTime` | ISO-8601 `YYYY-MM-DDTHH:MM:SS[.fff]±HH:MM[[Zone/Id]]` |
//! | `LocalDateTime` | ISO-8601 `YYYY-MM-DDTHH:MM:SS[.fff]` |
//! | `Duration` | ISO-8601 duration `PnYnMnDTnHnMnS` |
//! | `Point` | coordinate-key object + `srid`/`crs` marker (below) |
//! | `Geometry` | GeoJSON-style `{"type", "coordinates" or "geometries"}` |
//! | `Vector` | `{"type","dim","values"}` (structured, **not** a string) |
//!
//! The five temporal renderings are exactly the canonical ISO-8601 `Display`
//! forms of the corresponding `CypherValue` (trailing-zero-trimmed fractional
//! seconds, compact offsets, `Z` for UTC, `[IANA/Zone]` suffix on a zoned
//! `DateTime`). They are unambiguous and parseable, and — unlike the
//! persistence format — carry no type tag.
//!
//! ## Point (`Point`)
//!
//! A coordinate-key object carrying an explicit CRS marker so the reader never
//! has to guess the coordinate space:
//! - geographic (`srid` 4326 / 4979): `{"srid","crs","longitude","latitude"[,"height"]}`
//! - cartesian (all other srids): `{"srid","crs","x","y"[,"z"]}`
//!
//! `crs` is the Neo4j CRS name (`wgs-84`, `wgs-84-3d`, `cartesian`,
//! `cartesian-3d`; unknown srids fall back to `cartesian` keys/name). The
//! `srid`/`crs` marker is new relative to the historical CLI/WASM point output,
//! which emitted the coordinate keys alone.
//!
//! ## Vector (`Vector`)
//!
//! `{"type": <coordinate-type name>, "dim": <usize>, "values": [<number>, …]}`.
//! `type` is the canonical coordinate-type name (`INTEGER8`, `INTEGER16`,
//! `INTEGER32`, `INTEGER`, `FLOAT32`, `FLOAT`); integer-kind vectors carry JSON
//! integers, float-kind vectors carry JSON numbers. This replaces the previous
//! bug where every surface stringified a vector via `Display`
//! (`vector([…], 3, FLOAT32 NOT NULL)`).
//!
//! ## Non-finite floats
//!
//! `NaN`, `+Inf`, and `-Inf` are not representable as standard JSON numbers
//! and are encoded as the JSON strings `"NaN"`, `"Inf"`, and `"-Inf"` —
//! exactly the `CypherValue` `Display` spellings, so the JSON surfaces agree
//! with the table/CSV modes and a non-finite float is never
//! indistinguishable from a genuine `null` (ADR-0280; a breaking wire
//! change from the previous `null` encoding, acceptable pre-1.0/alpha per
//! the precedent below, recorded in the CHANGELOG).
//!
//! # Stats encoding ([`stats_to_wire_json`])
//!
//! All **11** mutation counters, **snake_case**, **zeros included**:
//!
//! `nodes_created`, `nodes_deleted`, `relationships_created`,
//! `relationships_deleted`, `properties_set`, `labels_added`, `labels_removed`,
//! `indexes_added`, `indexes_removed`, `constraints_added`,
//! `constraints_removed`.
//!
//! [`stats_from_wire_json`] is the inverse: it decodes that object back into
//! a [`WireStatsSnapshot`], naming the first canonical counter that is
//! missing or not an unsigned integer.
//!
//! These are the canonical names (they match the engine's `Stats` field names
//! and Neo4j's `Added`/`Removed` semantics). The historical JSONL server
//! renamed four of them (`indexes_added`→`indexes_created`,
//! `indexes_removed`→`indexes_dropped`, and the two `constraints_*`
//! equivalents); the WASM surface used camelCase and omitted zeros; the FFI
//! surface emitted only six. Unifying on this set is a breaking wire change
//! (documented in the CHANGELOG; acceptable pre-1.0/alpha).
//!
//! # Response envelope ([`response_envelope`] / [`error_envelope`])
//!
//! ```json
//! { "columns": [ … ], "columnTypes": [ … ], "rows": [ { … }, … ], "stats": { … }, "notifications": [ … ], "elapsed_ms": 1.25, "plan": { … } | null, "error": null }
//! ```
//!
//! `notifications` is always present and empty on a plain run (ADR-0363
//! decision 9); its entries are the engine-native notification objects
//! ([`notification_to_wire_json`]). `plan` is always present too — the
//! `cypherlite.plan/v1` document (LABS-418, ADR-0381), `null` on a plain
//! run.
//!
//! The row container key is **`rows`**. The historical JSONL server and FFI
//! used `data`; migrating them to `rows` is part of the same unification.
//! `elapsed_ms` is the engine-reported statement elapsed in wall-clock
//! milliseconds (ADR-0221). `columnTypes` carries the engine-stamped
//! per-column type names of ADR-0224 (one of the 18 canonical names, beside
//! `columns`, same length and order; a column whose first-row value is
//! `Null`, or a result with no rows, stamps `ANY`, so a write-only envelope
//! carries `"columnTypes": []`). On error: `columns: []`, `rows: []`,
//! `stats: {}`, `error: <message string>` (no `elapsed_ms`: no statement
//! ran).

use std::borrow::Cow;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::value::{CypherValue, GeometryValue, PointValue};

/// The `kind` discriminator of a wire node object (ADR-0223). Decoder sides
/// match on these constants instead of re-typing the literals.
pub const WIRE_KIND_NODE: &str = "node";

/// The `kind` discriminator of a wire relationship object (ADR-0223).
pub const WIRE_KIND_RELATIONSHIP: &str = "relationship";

/// Encode a [`CypherValue`] as its canonical wire JSON.
///
/// Covers all 18 variants with structured, self-describing encodings; no
/// variant falls through to a `Display` string except the five temporal
/// variants, whose ISO-8601 string *is* the canonical encoding. See the module
/// docs for the full table.
pub fn value_to_wire_json(val: &CypherValue) -> Value {
    match val {
        CypherValue::Null => Value::Null,
        CypherValue::Bool(b) => Value::Bool(*b),
        CypherValue::Int(i) => Value::Number((*i).into()),
        CypherValue::Float(f) => json_f64(*f),
        CypherValue::Str(s) => Value::String(s.clone()),
        CypherValue::List(items) => Value::Array(items.iter().map(value_to_wire_json).collect()),
        CypherValue::Map(map) => Value::Object(map_to_wire_json(map)),
        CypherValue::Node {
            id,
            labels,
            properties,
        } => {
            let mut obj = Map::new();
            // ADR-0223: the `kind` discriminator lets clients dispatch on a
            // documented field instead of shape-sniffing.
            obj.insert("kind".into(), Value::String(WIRE_KIND_NODE.into()));
            obj.insert("id".into(), Value::Number((*id).into()));
            obj.insert(
                "labels".into(),
                Value::Array(labels.iter().map(|l| Value::String(l.clone())).collect()),
            );
            obj.insert(
                "properties".into(),
                Value::Object(map_to_wire_json(properties)),
            );
            Value::Object(obj)
        }
        CypherValue::Relationship {
            id,
            start_id,
            end_id,
            rel_type,
            properties,
        } => {
            let mut obj = Map::new();
            // ADR-0223: see the `Node` arm.
            obj.insert("kind".into(), Value::String(WIRE_KIND_RELATIONSHIP.into()));
            obj.insert("id".into(), Value::Number((*id).into()));
            obj.insert("type".into(), Value::String(rel_type.clone()));
            obj.insert("start_node".into(), Value::Number((*start_id).into()));
            obj.insert("end_node".into(), Value::Number((*end_id).into()));
            obj.insert(
                "properties".into(),
                Value::Object(map_to_wire_json(properties)),
            );
            Value::Object(obj)
        }
        CypherValue::Path {
            nodes,
            relationships,
        } => {
            // Alternating [node, rel, node, rel, …, node]. A well-formed path
            // has `nodes.len() == relationships.len() + 1`, so the trailing
            // node emits no following relationship.
            let mut items = Vec::with_capacity(nodes.len() + relationships.len());
            for (i, n) in nodes.iter().enumerate() {
                items.push(value_to_wire_json(n));
                if let Some(r) = relationships.get(i) {
                    items.push(value_to_wire_json(r));
                }
            }
            Value::Array(items)
        }
        // Temporal values: canonical ISO-8601 Display strings (documented,
        // stable, and tag-free). This is the one intentional "Display" arm.
        CypherValue::Date(_)
        | CypherValue::Time(_, _)
        | CypherValue::DateTime(_, _)
        | CypherValue::LocalDateTime(_)
        | CypherValue::Duration(_) => Value::String(val.to_string()),
        CypherValue::Point(p) => point_to_wire_json(p),
        CypherValue::Geometry(g) => geometry_to_wire_json(g),
        CypherValue::Vector { coord, dim, values } => {
            let mut obj = Map::new();
            obj.insert(
                "type".into(),
                Value::String(coord.canonical_name().to_string()),
            );
            obj.insert("dim".into(), Value::Number((*dim as u64).into()));
            obj.insert(
                "values".into(),
                Value::Array(values.iter().map(value_to_wire_json).collect()),
            );
            Value::Object(obj)
        }
    }
}

/// Encode a property/value map (used for node & relationship `properties` and
/// for `CypherValue::Map`).
fn map_to_wire_json(map: &IndexMap<String, CypherValue>) -> Map<String, Value> {
    map.iter()
        .map(|(k, v)| (k.clone(), value_to_wire_json(v)))
        .collect()
}

/// Encode an `f64` as wire JSON: a JSON number when finite, otherwise the
/// JSON string of its `CypherValue` `Display` spelling (`"NaN"`/`"Inf"`/
/// `"-Inf"`, ADR-0280).
pub fn json_f64(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(CypherValue::Float(f).to_string()))
}

/// Whether an SRID is geographic (longitude/latitude keys, `wgs-84*` CRS).
fn is_geographic(srid: i32) -> bool {
    matches!(srid, 4326 | 4979)
}

/// The Neo4j CRS name for an SRID (used as the `crs` point marker) — the
/// canonical mapping every surface shares (ADR-0280 wire shape).
pub fn crs_name(srid: i32) -> &'static str {
    match srid {
        4326 => "wgs-84",
        4979 => "wgs-84-3d",
        7203 => "cartesian",
        9157 => "cartesian-3d",
        _ => "cartesian",
    }
}

fn point_to_wire_json(p: &PointValue) -> Value {
    let mut obj = Map::new();
    obj.insert("srid".into(), Value::Number(p.srid.into()));
    obj.insert("crs".into(), Value::String(crs_name(p.srid).to_string()));
    // Geographic and cartesian differ only in the coordinate key names.
    let (x_key, y_key, z_key) = if is_geographic(p.srid) {
        ("longitude", "latitude", "height")
    } else {
        ("x", "y", "z")
    };
    obj.insert(x_key.into(), json_f64(p.x));
    obj.insert(y_key.into(), json_f64(p.y));
    if let Some(z) = p.z {
        obj.insert(z_key.into(), json_f64(z));
    }
    Value::Object(obj)
}

fn coords_to_wire_json(coords: &[[f64; 2]]) -> Value {
    Value::Array(
        coords
            .iter()
            .map(|c| Value::Array(vec![json_f64(c[0]), json_f64(c[1])]))
            .collect(),
    )
}

fn geometry_to_wire_json(g: &GeometryValue) -> Value {
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String(g.type_name().to_string()));
    match g {
        GeometryValue::LineString { coordinates, .. } => {
            obj.insert("coordinates".into(), coords_to_wire_json(coordinates));
        }
        GeometryValue::Polygon { rings, .. } => {
            obj.insert(
                "coordinates".into(),
                Value::Array(rings.iter().map(|r| coords_to_wire_json(r)).collect()),
            );
        }
        GeometryValue::MultiPoint { points, .. } => {
            obj.insert("coordinates".into(), coords_to_wire_json(points));
        }
        GeometryValue::MultiLineString { lines, .. } => {
            obj.insert(
                "coordinates".into(),
                Value::Array(lines.iter().map(|l| coords_to_wire_json(l)).collect()),
            );
        }
        GeometryValue::MultiPolygon { polygons, .. } => {
            obj.insert(
                "coordinates".into(),
                Value::Array(
                    polygons
                        .iter()
                        .map(|poly| {
                            Value::Array(poly.iter().map(|r| coords_to_wire_json(r)).collect())
                        })
                        .collect(),
                ),
            );
        }
        GeometryValue::GeometryCollection { geometries, .. } => {
            obj.insert(
                "geometries".into(),
                Value::Array(geometries.iter().map(geometry_to_wire_json).collect()),
            );
        }
    }
    Value::Object(obj)
}

/// Encode one result row (an ordered column→value map) as a JSON object.
///
/// Columns are taken from `columns` (preserving the query's column order in the
/// underlying ordered map); a column absent from `row` encodes as `null`.
pub fn row_to_wire_json(columns: &[String], row: &IndexMap<String, CypherValue>) -> Value {
    let mut obj = Map::new();
    for col in columns {
        let v = row.get(col).map(value_to_wire_json).unwrap_or(Value::Null);
        obj.insert(col.clone(), v);
    }
    Value::Object(obj)
}

/// Build the canonical wire `rows` array for a query result.
///
/// This is the single, shared assembly point for the row container so the
/// JSON surfaces (C FFI, CLI) cannot diverge on it. It applies
/// the **write-only rule**: a statement that produces no result columns (a
/// `CREATE`/`MERGE`/`SET`/… without `RETURN`) yields an empty `rows` array — not
/// a `[{}]` array of empty objects — even though the engine emits one driving
/// row per input row (seeded with a single empty row for a top-level write).
/// When there are columns, each row is encoded via [`row_to_wire_json`].
///
/// Historically only the JSONL server guarded the empty-columns case; the FFI
/// and WASM surfaces mapped every driving row unconditionally and so emitted
/// `[{}]` for a write-only statement. Routing all three through this helper
/// makes that drift structurally impossible (pinned by
/// [`golden::envelope_write_only`]).
pub fn rows_for_result(columns: &[String], rows: &[IndexMap<String, CypherValue>]) -> Vec<Value> {
    if columns.is_empty() {
        Vec::new()
    } else {
        rows.iter()
            .map(|row| row_to_wire_json(columns, row))
            .collect()
    }
}

/// The 11 canonical mutation counters, read as `u64`.
///
/// Implemented by [`WireStatsSnapshot`] here (built from the executor's
/// `QueryStats` via `cypherlite::query_stats_to_wire`), so
/// `stats_to_wire_json` can serialize either without `cypherlite-model`
/// depending on the executor.
pub trait WireStats {
    fn nodes_created(&self) -> u64;
    fn nodes_deleted(&self) -> u64;
    fn relationships_created(&self) -> u64;
    fn relationships_deleted(&self) -> u64;
    fn properties_set(&self) -> u64;
    fn labels_added(&self) -> u64;
    fn labels_removed(&self) -> u64;
    fn indexes_added(&self) -> u64;
    fn indexes_removed(&self) -> u64;
    fn constraints_added(&self) -> u64;
    fn constraints_removed(&self) -> u64;
}

/// A plain, model-owned carrier of the 11 counters — a [`WireStats`] value that
/// does not require the engine crate (used by the golden fixtures and by any
/// surface that already has counts in hand).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireStatsSnapshot {
    pub nodes_created: u64,
    pub nodes_deleted: u64,
    pub relationships_created: u64,
    pub relationships_deleted: u64,
    pub properties_set: u64,
    pub labels_added: u64,
    pub labels_removed: u64,
    pub indexes_added: u64,
    pub indexes_removed: u64,
    pub constraints_added: u64,
    pub constraints_removed: u64,
}

impl WireStats for WireStatsSnapshot {
    fn nodes_created(&self) -> u64 {
        self.nodes_created
    }
    fn nodes_deleted(&self) -> u64 {
        self.nodes_deleted
    }
    fn relationships_created(&self) -> u64 {
        self.relationships_created
    }
    fn relationships_deleted(&self) -> u64 {
        self.relationships_deleted
    }
    fn properties_set(&self) -> u64 {
        self.properties_set
    }
    fn labels_added(&self) -> u64 {
        self.labels_added
    }
    fn labels_removed(&self) -> u64 {
        self.labels_removed
    }
    fn indexes_added(&self) -> u64 {
        self.indexes_added
    }
    fn indexes_removed(&self) -> u64 {
        self.indexes_removed
    }
    fn constraints_added(&self) -> u64 {
        self.constraints_added
    }
    fn constraints_removed(&self) -> u64 {
        self.constraints_removed
    }
}

/// Encode the 11 canonical mutation counters as a JSON object: snake_case keys,
/// zeros included. See the module docs for the canonical name set.
pub fn stats_to_wire_json(stats: &dyn WireStats) -> Value {
    Value::Object(
        STAT_COUNTERS
            .iter()
            .map(|counter| (counter.key.to_string(), (counter.count)(stats).into()))
            .collect(),
    )
}

/// Decode an envelope's `stats` object into the 11 canonical counters — the
/// inverse of [`stats_to_wire_json`], reading the same [`STAT_COUNTERS`]
/// table the encoder writes.
///
/// Every canonical counter must be present as a JSON unsigned integer; the
/// first one that is missing or of another type is an error naming it.
/// Entries outside the canonical set are ignored — the encoder writes none,
/// and a reader must not fail on a recording that carries one.
pub fn stats_from_wire_json(stats: &Map<String, Value>) -> Result<WireStatsSnapshot, String> {
    let mut snapshot = WireStatsSnapshot::default();
    for counter in &STAT_COUNTERS {
        let count = stats
            .get(counter.key)
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                format!(
                    "stats entry `{}` is missing or not an unsigned integer",
                    counter.key
                )
            })?;
        (counter.set)(&mut snapshot, count);
    }
    Ok(snapshot)
}

/// A mutation counter: its key in an envelope's `stats` object, its key on
/// the Bolt wire (derived by [`StatCounter::bolt_key`]), and the
/// [`WireStats`] accessor it encodes — with that accessor's inverse, so
/// [`stats_to_wire_json`] and [`stats_from_wire_json`] read one table.
#[derive(Debug, Clone, Copy)]
pub struct StatCounter {
    /// The counter's key in the JSON envelope's `stats` object (snake_case,
    /// the canonical envelope vocabulary).
    pub key: &'static str,
    /// The [`WireStats`] accessor this counter encodes.
    pub count: fn(&dyn WireStats) -> u64,
    /// The inverse of `count`: writes this counter's value into a
    /// [`WireStatsSnapshot`], so the decoder assigns the field the encoder
    /// reads.
    pub set: fn(&mut WireStatsSnapshot, u64),
}

impl StatCounter {
    /// The counter's key in Bolt's `stats` map (ADR-0309): Neo4j's
    /// kebab-case wire spelling, derived from the envelope key so no second
    /// stored spelling can drift from it.
    pub fn bolt_key(&self) -> Cow<'static, str> {
        self.key.replace('_', "-").into()
    }
}

/// The mutation counters of an envelope's `stats` object, in canonical order.
pub const STAT_COUNTERS: [StatCounter; 11] = [
    StatCounter {
        key: "nodes_created",
        count: |s| s.nodes_created(),
        set: |s, n| s.nodes_created = n,
    },
    StatCounter {
        key: "nodes_deleted",
        count: |s| s.nodes_deleted(),
        set: |s, n| s.nodes_deleted = n,
    },
    StatCounter {
        key: "relationships_created",
        count: |s| s.relationships_created(),
        set: |s, n| s.relationships_created = n,
    },
    StatCounter {
        key: "relationships_deleted",
        count: |s| s.relationships_deleted(),
        set: |s, n| s.relationships_deleted = n,
    },
    StatCounter {
        key: "properties_set",
        count: |s| s.properties_set(),
        set: |s, n| s.properties_set = n,
    },
    StatCounter {
        key: "labels_added",
        count: |s| s.labels_added(),
        set: |s, n| s.labels_added = n,
    },
    StatCounter {
        key: "labels_removed",
        count: |s| s.labels_removed(),
        set: |s, n| s.labels_removed = n,
    },
    StatCounter {
        key: "indexes_added",
        count: |s| s.indexes_added(),
        set: |s, n| s.indexes_added = n,
    },
    StatCounter {
        key: "indexes_removed",
        count: |s| s.indexes_removed(),
        set: |s, n| s.indexes_removed = n,
    },
    StatCounter {
        key: "constraints_added",
        count: |s| s.constraints_added(),
        set: |s, n| s.constraints_added = n,
    },
    StatCounter {
        key: "constraints_removed",
        count: |s| s.constraints_removed(),
        set: |s, n| s.constraints_removed = n,
    },
];

/// The ADR-0224 per-column type classification: the closed 18-kind set the
/// engine stamps result columns with, one kind per first-row value. This
/// enum is the single source of the canonical name spellings — both value
/// taxonomies (the wire-side [`CypherValue`] here, the engine's core
/// `Value` in `cypherlite-core`'s `convert` module) classify into it, so a
/// renamed or extended kind fails to compile at every mapping site instead
/// of drifting into a second hand-maintained table.
///
/// `Uuid` and `Binary` are reserved kinds: neither value enum has a
/// distinct variant that classifies into them today (UUID values cross as
/// `Str`/`String` and stamp `STRING`), so the names exist only to keep a
/// future distinct value kind from renumbering the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnTypeKind {
    Any,
    String,
    Integer,
    Float,
    Boolean,
    Node,
    Relationship,
    Path,
    List,
    Map,
    Point,
    DateTime,
    Date,
    Time,
    Duration,
    Vector,
    Uuid,
    Binary,
}

impl ColumnTypeKind {
    /// The canonical ADR-0224 type-name spelling of this kind.
    pub fn name(self) -> &'static str {
        match self {
            Self::Any => "ANY",
            Self::String => "STRING",
            Self::Integer => "INTEGER",
            Self::Float => "FLOAT",
            Self::Boolean => "BOOLEAN",
            Self::Node => "NODE",
            Self::Relationship => "RELATIONSHIP",
            Self::Path => "PATH",
            Self::List => "LIST",
            Self::Map => "MAP",
            Self::Point => "POINT",
            Self::DateTime => "DATETIME",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::Duration => "DURATION",
            Self::Vector => "VECTOR",
            Self::Uuid => "UUID",
            Self::Binary => "BINARY",
        }
    }
}

/// Classify a first-row value into its canonical ADR-0224 kind.
///
/// `Null` classifies `Any` (the stamping rule's `ANY`). `Geometry` is a
/// distinct `CypherValue` variant but is not in the client-facing set; it
/// reports `Point` (the same coordinate family).
pub fn column_type_kind(value: &CypherValue) -> ColumnTypeKind {
    match value {
        CypherValue::Null => ColumnTypeKind::Any,
        CypherValue::Bool(_) => ColumnTypeKind::Boolean,
        CypherValue::Int(_) => ColumnTypeKind::Integer,
        CypherValue::Float(_) => ColumnTypeKind::Float,
        CypherValue::Str(_) => ColumnTypeKind::String,
        CypherValue::List(_) => ColumnTypeKind::List,
        CypherValue::Map(_) => ColumnTypeKind::Map,
        CypherValue::Node { .. } => ColumnTypeKind::Node,
        CypherValue::Relationship { .. } => ColumnTypeKind::Relationship,
        CypherValue::Path { .. } => ColumnTypeKind::Path,
        CypherValue::Date(_) => ColumnTypeKind::Date,
        CypherValue::Time(_, _) => ColumnTypeKind::Time,
        CypherValue::DateTime(_, _) => ColumnTypeKind::DateTime,
        CypherValue::LocalDateTime(_) => ColumnTypeKind::DateTime,
        CypherValue::Duration(_) => ColumnTypeKind::Duration,
        CypherValue::Point(_) => ColumnTypeKind::Point,
        // Distinct value kind, same coordinate family — see the enum docs.
        CypherValue::Geometry(_) => ColumnTypeKind::Point,
        CypherValue::Vector { .. } => ColumnTypeKind::Vector,
    }
}

/// One canonical ADR-0224 per-column type name for a first-row value — the
/// name spelling of [`column_type_kind`], for callers that want the string
/// directly. The engine stamps each result column with one of the 18
/// canonical names at result construction, from the first row only —
/// Cypher columns are heterogeneous, and the name makes no claim about
/// later rows. This mapping is the wire-side spelling of the engine's
/// `QueryResult::column_types` stamp.
pub fn column_type_name(value: &CypherValue) -> &'static str {
    column_type_kind(value).name()
}

/// Build the canonical success response envelope.
///
/// `{ "columns": [...], "columnTypes": [...], "rows": [...], "stats": {...},
/// "notifications": [...], "elapsed_ms": <f64>, "error": null }`.
/// The `plan` key (LABS-418, ADR-0381) is always present too: the
/// `cypherlite.plan/v1` document, `null` on a plain run.
/// `elapsed_ms` is the engine-reported statement elapsed in wall-clock
/// milliseconds (ADR-0221). `columnTypes` carries the engine-stamped
/// per-column type names (ADR-0224), beside `columns`, same length and order.
/// `notifications` is always present — empty on a plain run, populated by
/// `EXPLAIN`/`PROFILE` (ADR-0363 decision 9) — and its entries are the
/// engine-native objects [`notification_to_wire_json`] emits.
pub fn response_envelope(
    columns: &[String],
    column_types: &[String],
    rows: Vec<Value>,
    stats: Value,
    notifications: &[Notification],
    elapsed_ms: f64,
    plan: Option<Value>,
) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "columns".into(),
        Value::Array(columns.iter().map(|c| Value::String(c.clone())).collect()),
    );
    // ADR-0224: the engine-stamped per-column type names, beside `columns`.
    obj.insert(
        "columnTypes".into(),
        Value::Array(
            column_types
                .iter()
                .map(|t| Value::String(t.clone()))
                .collect(),
        ),
    );
    obj.insert("rows".into(), Value::Array(rows));
    obj.insert("stats".into(), stats);
    // ADR-0363 decision 9: always present, empty on a plain run.
    obj.insert(
        notification_keys::NOTIFICATIONS.into(),
        notifications_to_wire_json(notifications),
    );
    obj.insert("elapsed_ms".into(), json_f64(elapsed_ms));
    obj.insert("error".into(), Value::Null);
    // The structured plan document (LABS-418, ADR-0381): always present,
    // `null` when the statement carried no plan — the same presence policy
    // `notifications` (ADR-0363) and `columnTypes` (ADR-0224) use, owned by
    // the canonical builder. The HTTP surface's Neo4j-shaped envelope has
    // its own builder and its own plan keys.
    obj.insert("plan".into(), plan.unwrap_or(Value::Null));
    Value::Object(obj)
}

/// Build the canonical error response envelope: empty `columns`/`rows`, empty
/// `stats`, and the error message string under `error`.
pub fn error_envelope(message: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("columns".into(), Value::Array(Vec::new()));
    obj.insert("rows".into(), Value::Array(Vec::new()));
    obj.insert("stats".into(), Value::Object(Map::new()));
    obj.insert("error".into(), Value::String(message.to_string()));
    Value::Object(obj)
}

/// Shared golden fixtures: the cross-surface anti-drift substrate.
///
/// Every CypherLite surface (C FFI, CLI, and — transitively — the language
/// bindings) is expected to serialize through
/// [`value_to_wire_json`]. These fixtures pin the exact expected JSON for every
/// variant so that any surface diverging from the canonical encoding fails a
/// test. The module is public and always compiled so the CLI, FFI (and
/// any other) test suites can `use cypherlite_model::wire::golden;` and run
/// [`golden::assert_all_match`] against their own serializer.
pub mod golden;

#[cfg(test)]
mod tests;

// The one hand-built plan wire fixture — its own file so the wire
// accessor's and the agent document's test modules share one builder,
// never a second copy of the metric-args block.
#[cfg(test)]
#[path = "plan_test_fixtures.rs"]
pub(crate) mod plan_test_fixtures;

// ---------------------------------------------------------------------------
// Structured query plan wire form (ADR-0344/0345) — lives in `plan_wire`,
// re-exported here so `wire::plan_keys` paths keep working: the one
// public path stays under `wire`.
// ---------------------------------------------------------------------------

#[path = "plan_wire.rs"]
mod plan_wire;

pub use plan_wire::{
    metric_i64, metric_u64, plan_keys, plan_node_wire, plan_subtree_totals, profiled_facts,
    time_ns_ms, PlanMetricsWire, PlanNodeWire, PlanPhasesWire, PlanProfiledFacts, PlanReadsWire,
    CYPHER_VERSION, NEO4J_PLANNER_IMPL, PLAN_UNAVAILABLE,
};

// The typed plan-warning vocabulary (LABS-461: one warning type) — its
// own module beside the plan wire form it parses into, mirroring the
// notification vocabulary's placement; re-exported here so
// `wire::PlanWarning` is the one public path.

#[path = "plan_warning.rs"]
mod plan_warning;

pub use plan_warning::{estimation_miss, ratio_defined, warning_wire_json, PlanWarning};

// The agent-native plan projection (`cypherlite.plan/v1`, LABS-370) — our
// own API dialect beside the Neo4j-facing one, reading the same typed view.

#[path = "agent_plan.rs"]
mod agent_plan;

pub use agent_plan::{plan_document, SCHEMA as PLAN_SCHEMA};

// ---------------------------------------------------------------------------
// Query notifications (ADR-0363) — lives in `notification_wire`, re-exported
// here so `wire::Notification` is the one public path (the plan vocabulary's
// layout).
// ---------------------------------------------------------------------------

#[path = "notification_wire.rs"]
mod notification_wire;

pub use notification_wire::{
    notification_keys, notification_to_neo4j_entries, notification_to_neo4j_json,
    notification_to_wire_json, notifications_to_wire_json, Notification, NotificationCategory,
    NotificationKind, NotificationSeverity,
};
