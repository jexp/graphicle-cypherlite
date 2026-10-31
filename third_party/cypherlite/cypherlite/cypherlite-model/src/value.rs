use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use indexmap::IndexMap;
use std::fmt;

/// ISO 8601 duration with separate month, day, second, and nanosecond components.
/// chrono::Duration does not support months/years, so we need a custom type.
#[derive(Debug, Clone, PartialEq)]
pub struct IsoDuration {
    pub months: i64,
    pub days: i64,
    pub seconds: i64,
    pub nanoseconds: i64,
}

impl IsoDuration {
    pub fn new(months: i64, days: i64, seconds: i64, nanoseconds: i64) -> Self {
        Self {
            months,
            days,
            seconds,
            nanoseconds,
        }
    }

    pub fn zero() -> Self {
        Self::new(0, 0, 0, 0)
    }
}

impl fmt::Display for IsoDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "P")?;
        let years = self.months / 12;
        let months = self.months % 12;
        if years != 0 {
            write!(f, "{years}Y")?;
        }
        if months != 0 {
            write!(f, "{months}M")?;
        }
        if self.days != 0 {
            write!(f, "{}D", self.days)?;
        }

        // Normalize seconds + nanoseconds: combine into total nanoseconds,
        // then split back out so signs are consistent.
        // Engine durations are i128-based (Temporal::Duration.nanos), and
        // seconds * 1e9 overflows i64 for extreme magnitudes (e.g.
        // duration({years: 1000000, ...}) scale values from Temporal10),
        // so the combined math is done in i128, which covers the full
        // range representable by the i64 fields.
        let total_nanos = i128::from(self.seconds) * 1_000_000_000 + i128::from(self.nanoseconds);
        let has_time = total_nanos != 0;

        if has_time || (self.months == 0 && self.days == 0) {
            write!(f, "T")?;

            let negative = total_nanos < 0;
            let abs_nanos = total_nanos.unsigned_abs();
            let abs_seconds = abs_nanos / 1_000_000_000;
            let frac_nanos = abs_nanos % 1_000_000_000;

            let hours = abs_seconds / 3600;
            let mins = (abs_seconds % 3600) / 60;
            let secs = abs_seconds % 60;

            let sign = if negative { "-" } else { "" };

            if hours != 0 {
                write!(f, "{sign}{hours}H")?;
            }
            if mins != 0 {
                // If hours were written with sign, don't repeat for mins
                // But Cypher format: each component can have its own sign
                write!(f, "{sign}{mins}M")?;
            }
            if secs != 0 || frac_nanos != 0 || (!has_time && self.months == 0 && self.days == 0) {
                if frac_nanos != 0 {
                    let frac = format!("{frac_nanos:09}");
                    let trimmed = frac.trim_end_matches('0');
                    write!(f, "{sign}{secs}.{trimmed}S")?;
                } else {
                    write!(f, "{sign}{secs}S")?;
                }
            }
        }
        Ok(())
    }
}

/// Cypher point value with SRID and 2D/3D coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct PointValue {
    pub srid: i32,
    pub x: f64,
    pub y: f64,
    pub z: Option<f64>,
}

impl PointValue {
    pub fn cartesian_2d(x: f64, y: f64) -> Self {
        Self {
            srid: 7203,
            x,
            y,
            z: None,
        }
    }

    pub fn cartesian_3d(x: f64, y: f64, z: f64) -> Self {
        Self {
            srid: 9157,
            x,
            y,
            z: Some(z),
        }
    }

    pub fn wgs84_2d(longitude: f64, latitude: f64) -> Self {
        Self {
            srid: 4326,
            x: longitude,
            y: latitude,
            z: None,
        }
    }

    pub fn wgs84_3d(longitude: f64, latitude: f64, height: f64) -> Self {
        Self {
            srid: 4979,
            x: longitude,
            y: latitude,
            z: Some(height),
        }
    }
}

impl fmt::Display for PointValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.z {
            Some(z) => write!(
                f,
                "point({{srid: {}, x: {}, y: {}, z: {}}})",
                self.srid, self.x, self.y, z
            ),
            None => write!(
                f,
                "point({{srid: {}, x: {}, y: {}}})",
                self.srid, self.x, self.y
            ),
        }
    }
}

/// A vector coordinate type (CIP-216/CIP-200): the six numeric types valid
/// inside `VECTOR<...>`/`VECTOR(...)`. `Int64`/`Float64` double as ordinary
/// plain property types (aliases for `INTEGER`/`FLOAT`) outside a vector
/// type; the other four (`Float32`, `Int8`/`Int16`/`Int32`) are valid only
/// as a vector coordinate type. Lives in `cypherlite-model` (rather than
/// the parser, where it originated) so the runtime `CypherValue::Vector`
/// variant can carry it without a circular crate dependency -- the parser
/// re-exports this same type as `cypherlite_parser::VectorCoordKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VectorCoordKind {
    Int8,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
}

impl VectorCoordKind {
    /// CIP-100 "Default type names" table, vector coordinate half: the
    /// canonical coordinate name rendered inside `VECTOR<...>` and by
    /// `toString`/Display. `Int64`/`Float64` render as the plain
    /// `INTEGER`/`FLOAT` names (CIP-216 S-6: they double as ordinary
    /// property type aliases), not `INTEGER64`/`FLOAT64`.
    pub fn canonical_name(&self) -> &'static str {
        match self {
            VectorCoordKind::Int8 => "INTEGER8",
            VectorCoordKind::Int16 => "INTEGER16",
            VectorCoordKind::Int32 => "INTEGER32",
            VectorCoordKind::Int64 => "INTEGER",
            VectorCoordKind::Float32 => "FLOAT32",
            VectorCoordKind::Float64 => "FLOAT",
        }
    }

    /// Reverse of `canonical_name`, understanding only the canonical
    /// (non-aliased) spelling -- used to round-trip a rendered type string.
    pub fn from_canonical_name(name: &str) -> Option<Self> {
        Some(match name {
            "INTEGER8" => VectorCoordKind::Int8,
            "INTEGER16" => VectorCoordKind::Int16,
            "INTEGER32" => VectorCoordKind::Int32,
            "INTEGER" => VectorCoordKind::Int64,
            "FLOAT32" => VectorCoordKind::Float32,
            "FLOAT" => VectorCoordKind::Float64,
            _ => return None,
        })
    }

    /// Resolves the full CIP-200 alias vocabulary accepted by the `vector()`
    /// constructor's `coordinateType` keyword argument (case-sensitive,
    /// already-uppercased input expected): every alias in the "Type Name
    /// Normalization" table, not just the canonical spelling.
    pub fn from_alias(name: &str) -> Option<Self> {
        Some(match name {
            "INT8" | "INTEGER8" => VectorCoordKind::Int8,
            "INT16" | "INTEGER16" => VectorCoordKind::Int16,
            "INT32" | "INTEGER32" => VectorCoordKind::Int32,
            "INT" | "INTEGER" | "INT64" | "INTEGER64" | "SIGNED INTEGER" => VectorCoordKind::Int64,
            "FLOAT32" => VectorCoordKind::Float32,
            "FLOAT" | "FLOAT64" => VectorCoordKind::Float64,
            _ => return None,
        })
    }

    /// Whether this coordinate kind stores its elements as `CypherValue::Int`
    /// (the four integer-family kinds) vs `CypherValue::Float` (the two
    /// floating-point kinds).
    pub fn is_integer(&self) -> bool {
        !matches!(self, VectorCoordKind::Float32 | VectorCoordKind::Float64)
    }

    /// The CIP-200 "Ordering of the Vector Type" coordinate-type rank:
    /// INTEGER8 < INTEGER16 < INTEGER32 < INTEGER64 < FLOAT32 < FLOAT64.
    pub fn order_rank(&self) -> u8 {
        match self {
            VectorCoordKind::Int8 => 0,
            VectorCoordKind::Int16 => 1,
            VectorCoordKind::Int32 => 2,
            VectorCoordKind::Int64 => 3,
            VectorCoordKind::Float32 => 4,
            VectorCoordKind::Float64 => 5,
        }
    }
}

/// Coordinate for geometry types (2D).
pub type Coord2D = [f64; 2];

/// Extended geometry types beyond Point.
#[derive(Debug, Clone, PartialEq)]
pub enum GeometryValue {
    LineString {
        coordinates: Vec<Coord2D>,
        srid: i32,
    },
    Polygon {
        /// Outer ring first, then holes.
        rings: Vec<Vec<Coord2D>>,
        srid: i32,
    },
    MultiPoint {
        points: Vec<Coord2D>,
        srid: i32,
    },
    MultiLineString {
        lines: Vec<Vec<Coord2D>>,
        srid: i32,
    },
    MultiPolygon {
        polygons: Vec<Vec<Vec<Coord2D>>>,
        srid: i32,
    },
    GeometryCollection {
        geometries: Vec<GeometryValue>,
        srid: i32,
    },
}

impl GeometryValue {
    /// Return the SRID of this geometry.
    pub fn srid(&self) -> i32 {
        match self {
            GeometryValue::LineString { srid, .. }
            | GeometryValue::Polygon { srid, .. }
            | GeometryValue::MultiPoint { srid, .. }
            | GeometryValue::MultiLineString { srid, .. }
            | GeometryValue::MultiPolygon { srid, .. }
            | GeometryValue::GeometryCollection { srid, .. } => *srid,
        }
    }

    /// Return the geometry type name (matching WKT type names).
    pub fn type_name(&self) -> &'static str {
        match self {
            GeometryValue::LineString { .. } => "LineString",
            GeometryValue::Polygon { .. } => "Polygon",
            GeometryValue::MultiPoint { .. } => "MultiPoint",
            GeometryValue::MultiLineString { .. } => "MultiLineString",
            GeometryValue::MultiPolygon { .. } => "MultiPolygon",
            GeometryValue::GeometryCollection { .. } => "GeometryCollection",
        }
    }
}

impl fmt::Display for GeometryValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GeometryValue::LineString { coordinates, srid } => {
                write!(f, "linestring({{srid: {srid}, coordinates: [")?;
                for (i, c) in coordinates.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "[{}, {}]", c[0], c[1])?;
                }
                write!(f, "]}})")
            }
            GeometryValue::Polygon { rings, srid } => {
                write!(f, "polygon({{srid: {srid}, coordinates: [")?;
                for (i, ring) in rings.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "[")?;
                    for (j, c) in ring.iter().enumerate() {
                        if j > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "[{}, {}]", c[0], c[1])?;
                    }
                    write!(f, "]")?;
                }
                write!(f, "]}})")
            }
            GeometryValue::MultiPoint { points, srid } => {
                write!(f, "multipoint({{srid: {srid}, coordinates: [")?;
                for (i, c) in points.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "[{}, {}]", c[0], c[1])?;
                }
                write!(f, "]}})")
            }
            GeometryValue::MultiLineString { lines: _, srid } => {
                write!(f, "multilinestring({{srid: {srid}, ...}})")
            }
            GeometryValue::MultiPolygon { polygons: _, srid } => {
                write!(f, "multipolygon({{srid: {srid}, ...}})")
            }
            GeometryValue::GeometryCollection { geometries, srid } => {
                write!(
                    f,
                    "geometrycollection({{srid: {}, count: {}}})",
                    srid,
                    geometries.len()
                )
            }
        }
    }
}

/// The core value type for CypherLite.
///
/// Every value flowing through the query engine is represented as a `CypherValue`.
/// This replaces Go's `interface{}` with a proper algebraic type.
#[derive(Debug, Clone)]
pub enum CypherValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<CypherValue>),
    Map(IndexMap<String, CypherValue>),
    Node {
        id: i64,
        labels: Vec<String>,
        properties: IndexMap<String, CypherValue>,
    },
    Relationship {
        id: i64,
        start_id: i64,
        end_id: i64,
        rel_type: String,
        properties: IndexMap<String, CypherValue>,
    },
    Path {
        nodes: Vec<CypherValue>,
        relationships: Vec<CypherValue>,
    },
    Date(NaiveDate),
    Time(NaiveTime, Option<FixedOffset>),
    /// DateTime with fixed offset and optional IANA timezone name (e.g., "Europe/Stockholm").
    DateTime(DateTime<FixedOffset>, Option<String>),
    LocalDateTime(NaiveDateTime),
    Duration(IsoDuration),
    Point(PointValue),
    Geometry(GeometryValue),
    /// CIP-200 VECTOR runtime value: a fixed-length, homogeneously-typed
    /// numeric vector distinct from `List`. `values` elements are always
    /// `CypherValue::Int` when `coord.is_integer()`, `CypherValue::Float`
    /// otherwise (never `Null` or any other variant -- enforced by the
    /// `vector()` constructor, the only way to produce one). `dim` is
    /// always `values.len()`.
    Vector {
        coord: VectorCoordKind,
        dim: usize,
        values: Vec<CypherValue>,
    },
}

impl CypherValue {
    /// Returns true if this value is Null.
    pub fn is_null(&self) -> bool {
        matches!(self, CypherValue::Null)
    }

    /// True for the five temporal variants (`Date`, `Time`, `DateTime`,
    /// `LocalDateTime`, `Duration`) — the one enumeration of the set, beside
    /// [`Self::is_null`] and `type_name`; the renderers that classify
    /// temporals (TOON's scalars-bare token policy) consult this instead of
    /// re-listing the variants, and a new variant is a compile error here.
    pub fn is_temporal(&self) -> bool {
        match self {
            CypherValue::Date(_)
            | CypherValue::Time(..)
            | CypherValue::DateTime(..)
            | CypherValue::LocalDateTime(_)
            | CypherValue::Duration(_) => true,
            CypherValue::Null
            | CypherValue::Bool(_)
            | CypherValue::Int(_)
            | CypherValue::Float(_)
            | CypherValue::Str(_)
            | CypherValue::List(_)
            | CypherValue::Map(_)
            | CypherValue::Node { .. }
            | CypherValue::Relationship { .. }
            | CypherValue::Path { .. }
            | CypherValue::Geometry(_)
            | CypherValue::Vector { .. }
            | CypherValue::Point(_) => false,
        }
    }

    /// Returns the type name as a string (for the `type()` function).
    pub fn type_name(&self) -> &'static str {
        match self {
            CypherValue::Null => "NULL",
            CypherValue::Bool(_) => "BOOLEAN",
            CypherValue::Int(_) => "INTEGER",
            CypherValue::Float(_) => "FLOAT",
            CypherValue::Str(_) => "STRING",
            CypherValue::List(_) => "LIST",
            CypherValue::Map(_) => "MAP",
            CypherValue::Node { .. } => "NODE",
            CypherValue::Relationship { .. } => "RELATIONSHIP",
            CypherValue::Path { .. } => "PATH",
            CypherValue::Date(_) => "DATE",
            CypherValue::Time(_, _) => "TIME",
            CypherValue::DateTime(..) => "DATE_TIME",
            CypherValue::LocalDateTime(_) => "LOCAL_DATE_TIME",
            CypherValue::Duration(_) => "DURATION",
            CypherValue::Point(_) => "POINT",
            CypherValue::Geometry(g) => g.type_name(),
            CypherValue::Vector { .. } => "VECTOR",
        }
    }

    /// Attempts to extract a boolean value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            CypherValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Attempts to extract an integer value.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            CypherValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Attempts to extract a float value, including int-to-float promotion.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            CypherValue::Float(f) => Some(*f),
            CypherValue::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// Attempts to extract a string reference.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            CypherValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Attempts to extract a list reference.
    pub fn as_list(&self) -> Option<&[CypherValue]> {
        match self {
            CypherValue::List(l) => Some(l),
            _ => None,
        }
    }

    /// Attempts to extract a map reference.
    pub fn as_map(&self) -> Option<&IndexMap<String, CypherValue>> {
        match self {
            CypherValue::Map(m) => Some(m),
            _ => None,
        }
    }

    /// Returns true if this value is truthy in Cypher's three-valued logic.
    /// Null returns None (unknown), true/false return Some(bool).
    pub fn to_bool_3vl(&self) -> Option<bool> {
        match self {
            CypherValue::Null => None,
            CypherValue::Bool(b) => Some(*b),
            _ => Some(true), // Non-null, non-bool values are truthy
        }
    }

    /// Coerce to a numeric type for arithmetic.
    /// Returns (value_as_f64, is_integer).
    pub fn as_numeric(&self) -> Option<(f64, bool)> {
        match self {
            CypherValue::Int(i) => Some((*i as f64, true)),
            CypherValue::Float(f) => Some((*f, false)),
            _ => None,
        }
    }
}

impl fmt::Display for CypherValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CypherValue::Null => write!(f, "null"),
            CypherValue::Bool(b) => write!(f, "{b}"),
            CypherValue::Int(i) => write!(f, "{i}"),
            CypherValue::Float(v) => {
                if v.is_nan() {
                    write!(f, "NaN")
                } else if v.is_infinite() {
                    write!(f, "{}Inf", if *v < 0.0 { "-" } else { "" })
                } else if v.fract() == 0.0 {
                    write!(f, "{v}.0")
                } else {
                    write!(f, "{v}")
                }
            }
            CypherValue::Str(s) => write!(f, "\"{s}\""),
            CypherValue::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            CypherValue::Map(map) => {
                write!(f, "{{")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            CypherValue::Node {
                labels, properties, ..
            } => {
                write!(f, "(:{}", labels.join(":"))?;
                if !properties.is_empty() {
                    write!(f, " {{")?;
                    for (i, (k, v)) in properties.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{k}: {v}")?;
                    }
                    write!(f, "}}")?;
                }
                write!(f, ")")
            }
            CypherValue::Relationship {
                rel_type,
                properties,
                ..
            } => {
                write!(f, "[:{rel_type}")?;
                if !properties.is_empty() {
                    write!(f, " {{")?;
                    for (i, (k, v)) in properties.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{k}: {v}")?;
                    }
                    write!(f, "}}")?;
                }
                write!(f, "]")
            }
            CypherValue::Path {
                nodes,
                relationships,
            } => {
                for (i, node) in nodes.iter().enumerate() {
                    if i > 0 {
                        if let Some(rel) = relationships.get(i - 1) {
                            write!(f, "-{rel}-")?;
                        } else {
                            write!(f, "--")?;
                        }
                    }
                    write!(f, "{node}")?;
                }
                Ok(())
            }
            CypherValue::Date(d) => write!(f, "{d}"),
            CypherValue::Time(t, tz) => {
                use chrono::Timelike;
                // Format the time part with trimmed fractional seconds
                let nanos = t.nanosecond() % 1_000_000_000;
                if nanos != 0 {
                    let frac = format!("{nanos:09}");
                    let trimmed = frac.trim_end_matches('0');
                    write!(
                        f,
                        "{:02}:{:02}:{:02}.{}",
                        t.hour(),
                        t.minute(),
                        t.second(),
                        trimmed
                    )?;
                } else if t.second() != 0 {
                    write!(f, "{}", t.format("%H:%M:%S"))?;
                } else {
                    write!(f, "{}", t.format("%H:%M"))?;
                }
                // Format the timezone part
                if let Some(offset) = tz {
                    let total_secs = offset.local_minus_utc();
                    if total_secs == 0 {
                        write!(f, "Z")
                    } else {
                        let sign = if total_secs >= 0 { '+' } else { '-' };
                        let abs_secs = total_secs.unsigned_abs();
                        let hours = abs_secs / 3600;
                        let mins = (abs_secs % 3600) / 60;
                        let secs = abs_secs % 60;
                        if secs != 0 {
                            write!(f, "{sign}{hours:02}:{mins:02}:{secs:02}")
                        } else {
                            write!(f, "{sign}{hours:02}:{mins:02}")
                        }
                    }
                } else {
                    Ok(())
                }
            }
            CypherValue::DateTime(dt, tz_name) => {
                // Use ISO 8601 format with 'T' separator, trimmed nanos, and compact timezone
                let nanos = dt.timestamp_subsec_nanos();
                let date_part = dt.date_naive();
                let time_part = dt.time();
                if nanos != 0 {
                    let frac = format!("{nanos:09}");
                    let trimmed = frac.trim_end_matches('0');
                    write!(
                        f,
                        "{}T{}.{}",
                        date_part,
                        time_part.format("%H:%M:%S"),
                        trimmed
                    )?;
                } else {
                    write!(f, "{}T{}", date_part, time_part.format("%H:%M:%S"))?;
                }
                // Timezone offset
                let offset = dt.offset();
                let total_secs = offset.local_minus_utc();
                if total_secs == 0 {
                    write!(f, "Z")?;
                } else {
                    let sign = if total_secs >= 0 { '+' } else { '-' };
                    let abs_secs = total_secs.unsigned_abs();
                    let hours = abs_secs / 3600;
                    let mins = (abs_secs % 3600) / 60;
                    let secs_rem = abs_secs % 60;
                    if secs_rem != 0 {
                        write!(f, "{sign}{hours:02}:{mins:02}:{secs_rem:02}")?;
                    } else {
                        write!(f, "{sign}{hours:02}:{mins:02}")?;
                    }
                }
                // Append IANA timezone name if present
                if let Some(tz) = tz_name {
                    write!(f, "[{tz}]")?;
                }
                Ok(())
            }
            CypherValue::LocalDateTime(dt) => {
                // Use ISO 8601 format with 'T' separator and trimmed trailing zeros
                let nanos = dt.and_utc().timestamp_subsec_nanos();
                if nanos != 0 {
                    let frac = format!("{nanos:09}");
                    let trimmed = frac.trim_end_matches('0');
                    write!(
                        f,
                        "{}T{}.{}",
                        dt.date(),
                        dt.time().format("%H:%M:%S"),
                        trimmed
                    )
                } else {
                    write!(f, "{}T{}", dt.date(), dt.time().format("%H:%M:%S"))
                }
            }
            CypherValue::Duration(d) => write!(f, "{d}"),
            CypherValue::Point(p) => write!(f, "{p}"),
            CypherValue::Geometry(g) => write!(f, "{g}"),
            CypherValue::Vector { coord, dim, values } => {
                write!(f, "vector([")?;
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "], {}, {} NOT NULL)", dim, coord.canonical_name())
            }
        }
    }
}

impl PartialEq for CypherValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (CypherValue::Null, CypherValue::Null) => true,
            (CypherValue::Bool(a), CypherValue::Bool(b)) => a == b,
            (CypherValue::Int(a), CypherValue::Int(b)) => a == b,
            (CypherValue::Float(a), CypherValue::Float(b)) => a == b,
            // Exact Int-vs-Float equality: a plain `(a as f64) == b` cast
            // rounds away low bits for `|a| > 2^53`, which made `=` (and every
            // scan/residual/MERGE path built on it) disagree with exact
            // numeric equivalence keys -- so an index's presence could
            // silently change a MATCH/MERGE result. See
            // `canonical_key::eq_i64_f64`.
            (CypherValue::Int(a), CypherValue::Float(b)) => {
                crate::canonical_key::eq_i64_f64(*a, *b)
            }
            (CypherValue::Float(a), CypherValue::Int(b)) => {
                crate::canonical_key::eq_i64_f64(*b, *a)
            }
            (CypherValue::Str(a), CypherValue::Str(b)) => a == b,
            (CypherValue::List(a), CypherValue::List(b)) => a == b,
            (CypherValue::Map(a), CypherValue::Map(b)) => a == b,
            (CypherValue::Node { id: a, .. }, CypherValue::Node { id: b, .. }) => a == b,
            (CypherValue::Relationship { id: a, .. }, CypherValue::Relationship { id: b, .. }) => {
                a == b
            }
            (CypherValue::Date(a), CypherValue::Date(b)) => a == b,
            (CypherValue::Time(a, ao), CypherValue::Time(b, bo)) => a == b && ao == bo,
            (CypherValue::DateTime(a, _), CypherValue::DateTime(b, _)) => a == b,
            (CypherValue::LocalDateTime(a), CypherValue::LocalDateTime(b)) => a == b,
            (CypherValue::Duration(a), CypherValue::Duration(b)) => a == b,
            (CypherValue::Point(a), CypherValue::Point(b)) => a == b,
            (CypherValue::Geometry(a), CypherValue::Geometry(b)) => a == b,
            (
                CypherValue::Vector {
                    coord: ca,
                    dim: da,
                    values: va,
                },
                CypherValue::Vector {
                    coord: cb,
                    dim: db,
                    values: vb,
                },
            ) => ca == cb && da == db && va == vb,
            (
                CypherValue::Path {
                    nodes: n1,
                    relationships: r1,
                },
                CypherValue::Path {
                    nodes: n2,
                    relationships: r2,
                },
            ) => n1 == n2 && r1 == r2,
            _ => false,
        }
    }
}

impl PartialOrd for CypherValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (CypherValue::Int(a), CypherValue::Int(b)) => a.partial_cmp(b),
            (CypherValue::Float(a), CypherValue::Float(b)) => a.partial_cmp(b),
            // Exact Int-vs-Float ordering, kept consistent with the equality
            // above (both route through the same `canonical_key` domain), so
            // `<`/`ORDER BY` and `=` never disagree past `2^53`.
            (CypherValue::Int(a), CypherValue::Float(b)) => {
                crate::canonical_key::cmp_i64_f64(*a, *b)
            }
            (CypherValue::Float(a), CypherValue::Int(b)) => {
                crate::canonical_key::cmp_i64_f64(*b, *a).map(std::cmp::Ordering::reverse)
            }
            (CypherValue::Str(a), CypherValue::Str(b)) => a.partial_cmp(b),
            (CypherValue::Bool(a), CypherValue::Bool(b)) => a.partial_cmp(b),
            (CypherValue::Date(a), CypherValue::Date(b)) => a.partial_cmp(b),
            (CypherValue::List(a), CypherValue::List(b)) => {
                // Lexicographic comparison of lists
                for (av, bv) in a.iter().zip(b.iter()) {
                    match av.partial_cmp(bv) {
                        Some(std::cmp::Ordering::Equal) => continue,
                        Some(ord) => return Some(ord),
                        None => return None,
                    }
                }
                a.len().partial_cmp(&b.len())
            }
            (CypherValue::Time(a, ao), CypherValue::Time(b, bo)) => {
                // Sort times by UTC-normalized value
                match (ao, bo) {
                    (Some(a_off), Some(b_off)) => {
                        use chrono::Timelike;
                        let a_utc_secs =
                            a.num_seconds_from_midnight() as i64 - a_off.local_minus_utc() as i64;
                        let b_utc_secs =
                            b.num_seconds_from_midnight() as i64 - b_off.local_minus_utc() as i64;
                        match a_utc_secs.partial_cmp(&b_utc_secs) {
                            Some(std::cmp::Ordering::Equal) => {
                                // Compare nanoseconds within the second
                                let a_nanos = a.nanosecond() % 1_000_000_000;
                                let b_nanos = b.nanosecond() % 1_000_000_000;
                                a_nanos.partial_cmp(&b_nanos)
                            }
                            other => other,
                        }
                    }
                    (None, None) => a.partial_cmp(b),
                    _ => None,
                }
            }
            (CypherValue::DateTime(a, _), CypherValue::DateTime(b, _)) => a.partial_cmp(b),
            (CypherValue::LocalDateTime(a), CypherValue::LocalDateTime(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}

impl From<bool> for CypherValue {
    fn from(b: bool) -> Self {
        CypherValue::Bool(b)
    }
}

impl From<i64> for CypherValue {
    fn from(i: i64) -> Self {
        CypherValue::Int(i)
    }
}

impl From<f64> for CypherValue {
    fn from(f: f64) -> Self {
        CypherValue::Float(f)
    }
}

impl From<String> for CypherValue {
    fn from(s: String) -> Self {
        CypherValue::Str(s)
    }
}

impl From<&str> for CypherValue {
    fn from(s: &str) -> Self {
        CypherValue::Str(s.to_string())
    }
}

impl<T: Into<CypherValue>> From<Vec<T>> for CypherValue {
    fn from(v: Vec<T>) -> Self {
        CypherValue::List(v.into_iter().map(|x| x.into()).collect())
    }
}

impl<T: Into<CypherValue>> From<Option<T>> for CypherValue {
    fn from(o: Option<T>) -> Self {
        match o {
            Some(v) => v.into(),
            None => CypherValue::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_type_names() {
        assert_eq!(CypherValue::Null.type_name(), "NULL");
        assert_eq!(CypherValue::Bool(true).type_name(), "BOOLEAN");
        assert_eq!(CypherValue::Int(42).type_name(), "INTEGER");
        assert_eq!(CypherValue::Float(3.14).type_name(), "FLOAT");
        assert_eq!(CypherValue::Str("hello".into()).type_name(), "STRING");
        assert_eq!(CypherValue::List(vec![]).type_name(), "LIST");
        assert_eq!(CypherValue::Map(IndexMap::new()).type_name(), "MAP");
    }

    #[test]
    fn test_int_float_equality() {
        assert_eq!(CypherValue::Int(1), CypherValue::Float(1.0));
        assert_eq!(CypherValue::Float(1.0), CypherValue::Int(1));
        assert_ne!(CypherValue::Int(1), CypherValue::Float(1.5));
    }

    #[test]
    fn test_partial_ord() {
        assert!(CypherValue::Int(1) < CypherValue::Int(2));
        assert!(CypherValue::Int(1) < CypherValue::Float(1.5));
        assert!(CypherValue::Str("a".into()) < CypherValue::Str("b".into()));
    }

    #[test]
    fn int_float_equality_is_exact_past_2_pow_53() {
        // `9007199254740993` (= 2^53 + 1) is an exact i64 but rounds to
        // `9007199254740992.0` (= 2^53) as an f64. A lossy `(a as f64) == b`
        // cast made these compare EQUAL, which disagreed with the exact
        // numeric equivalence keys (so an index's presence silently changed
        // MATCH/MERGE results). They denote different numbers, so `=` must
        // say NOT equal.
        let big_int = CypherValue::Int(9_007_199_254_740_993);
        let near_float = CypherValue::Float(9_007_199_254_740_992.0);
        assert_ne!(big_int, near_float);
        assert_ne!(near_float, big_int);

        // The float DOES still equal the integer it exactly represents.
        assert_eq!(
            CypherValue::Int(9_007_199_254_740_992),
            CypherValue::Float(9_007_199_254_740_992.0)
        );

        // Small integral floats keep the ordinary `1 = 1.0` behavior.
        assert_eq!(CypherValue::Int(1), CypherValue::Float(1.0));
        assert_eq!(CypherValue::Int(0), CypherValue::Float(-0.0));
    }

    #[test]
    fn int_float_ordering_stays_consistent_with_equality() {
        // Whenever `partial_cmp` reports `Equal`, `==` must agree, and vice
        // versa, across the whole large-magnitude boundary. A stray `Equal`
        // from a lossy ordering cast (while `=` became exact) would be a latent
        // inconsistency.
        let probes = [
            (9_007_199_254_740_993_i64, 9_007_199_254_740_992.0_f64),
            (9_007_199_254_740_992, 9_007_199_254_740_992.0),
            (i64::MAX, 9_223_372_036_854_775_808.0), // 2^63, out of exact range
            (i64::MIN, -9_223_372_036_854_775_808.0), // -2^63, in exact range
            (0, -0.0),
            (1, 1.0),
            (2, 1.5),
            (1, 1.5),
        ];
        for (i, f) in probes {
            let iv = CypherValue::Int(i);
            let fv = CypherValue::Float(f);
            let eq = iv == fv;
            let cmp_eq = iv.partial_cmp(&fv) == Some(std::cmp::Ordering::Equal);
            assert_eq!(eq, cmp_eq, "eq/ord disagree for Int({i}) vs Float({f})");
            // Reverse direction must be the mirror image.
            let rev = fv.partial_cmp(&iv);
            assert_eq!(
                iv.partial_cmp(&fv),
                rev.map(std::cmp::Ordering::reverse),
                "forward/reverse ordering not antisymmetric for {i}/{f}"
            );
        }
        // 2^53 + 1 (Int) sits strictly above 2^53 (Float).
        assert!(
            CypherValue::Int(9_007_199_254_740_993) > CypherValue::Float(9_007_199_254_740_992.0)
        );
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", CypherValue::Null), "null");
        assert_eq!(format!("{}", CypherValue::Bool(true)), "true");
        assert_eq!(format!("{}", CypherValue::Int(42)), "42");
        assert_eq!(format!("{}", CypherValue::Float(3.0)), "3.0");
        assert_eq!(format!("{}", CypherValue::Str("hi".into())), "\"hi\"");
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_as_float_promotion() {
        assert_eq!(CypherValue::Int(5).as_float(), Some(5.0));
        assert_eq!(CypherValue::Float(3.14).as_float(), Some(3.14));
        assert_eq!(CypherValue::Str("x".into()).as_float(), None);
    }

    #[test]
    fn test_three_valued_logic() {
        assert_eq!(CypherValue::Null.to_bool_3vl(), None);
        assert_eq!(CypherValue::Bool(true).to_bool_3vl(), Some(true));
        assert_eq!(CypherValue::Bool(false).to_bool_3vl(), Some(false));
        assert_eq!(CypherValue::Int(0).to_bool_3vl(), Some(true));
    }

    #[test]
    fn test_iso_duration_display() {
        assert_eq!(format!("{}", IsoDuration::new(0, 0, 0, 0)), "PT0S");
        assert_eq!(format!("{}", IsoDuration::new(14, 0, 0, 0)), "P1Y2M");
        assert_eq!(format!("{}", IsoDuration::new(0, 5, 3661, 0)), "P5DT1H1M1S");
        // Negative time components carry the sign per component.
        assert_eq!(
            format!("{}", IsoDuration::new(0, 0, -3661, -500_000_000)),
            "PT-1H-1M-1.5S"
        );
        // Mixed-sign seconds/nanoseconds normalize to a single sign.
        assert_eq!(
            format!("{}", IsoDuration::new(0, 0, 1, -1_500_000_000)),
            "PT-0.5S"
        );
    }

    #[test]
    fn test_iso_duration_display_extreme_magnitudes() {
        // Temporal10 [10] magnitude: duration({years: 1000000, ...}).
        assert_eq!(
            format!("{}", IsoDuration::new(12_000_000, 0, 0, 0)),
            "P1000000Y"
        );
        assert_eq!(
            format!("{}", IsoDuration::new(-12_000_000, 0, 0, 0)),
            "P-1000000Y"
        );
        // seconds * 1e9 overflows i64 at these magnitudes; the combined
        // seconds/nanos math must stay correct via i128 intermediates.
        assert_eq!(
            format!("{}", IsoDuration::new(0, 0, i64::MAX, 999_999_999)),
            "PT2562047788015215H30M7.999999999S"
        );
        assert_eq!(
            format!("{}", IsoDuration::new(0, 0, i64::MIN, -999_999_999)),
            "PT-2562047788015215H-30M-8.999999999S"
        );
        // Full engine-representable stack: 1e6-scale years plus extreme time.
        assert_eq!(
            format!("{}", IsoDuration::new(12_000_000, 0, i64::MAX, 999_999_999)),
            "P1000000YT2562047788015215H30M7.999999999S"
        );
        assert_eq!(
            format!(
                "{}",
                IsoDuration::new(-12_000_000, 0, i64::MIN, -999_999_999)
            ),
            "P-1000000YT-2562047788015215H-30M-8.999999999S"
        );
    }

    #[test]
    fn test_point_display() {
        let p = PointValue::wgs84_2d(-73.9857, 40.7484);
        assert_eq!(
            format!("{p}"),
            "point({srid: 4326, x: -73.9857, y: 40.7484})"
        );
    }

    #[test]
    fn test_from_conversions() {
        let v: CypherValue = 42i64.into();
        assert_eq!(v, CypherValue::Int(42));

        let v: CypherValue = "hello".into();
        assert_eq!(v, CypherValue::Str("hello".to_string()));

        let v: CypherValue = true.into();
        assert_eq!(v, CypherValue::Bool(true));

        let v: CypherValue = None::<i64>.into();
        assert_eq!(v, CypherValue::Null);
    }

    #[test]
    fn test_float_display_infinity_and_nan() {
        // CIP-55: the special float values render as `Inf`/`-Inf`/`NaN`,
        // matching Neo4j's `RETURN Infinity` result rendering (as opposed
        // to `toString()`, which spells them out as `'Infinity'`).
        assert_eq!(format!("{}", CypherValue::Float(f64::INFINITY)), "Inf");
        assert_eq!(format!("{}", CypherValue::Float(f64::NEG_INFINITY)), "-Inf");
        assert_eq!(format!("{}", CypherValue::Float(f64::NAN)), "NaN");
        // Ordinary finite floats are unaffected.
        assert_eq!(format!("{}", CypherValue::Float(2.0)), "2.0");
        assert_eq!(format!("{}", CypherValue::Float(1.5)), "1.5");
    }

    // ---- CIP-200 VECTOR runtime value type ----

    #[test]
    fn test_vector_display() {
        let v = CypherValue::Vector {
            coord: VectorCoordKind::Int64,
            dim: 3,
            values: vec![
                CypherValue::Int(1),
                CypherValue::Int(2),
                CypherValue::Int(3),
            ],
        };
        assert_eq!(format!("{v}"), "vector([1, 2, 3], 3, INTEGER NOT NULL)");

        let float_v = CypherValue::Vector {
            coord: VectorCoordKind::Float32,
            dim: 2,
            values: vec![CypherValue::Float(1.0), CypherValue::Float(2.5)],
        };
        assert_eq!(
            format!("{float_v}"),
            "vector([1.0, 2.5], 2, FLOAT32 NOT NULL)"
        );
    }

    #[test]
    fn test_vector_equality_compares_coord_dim_and_values() {
        let a = CypherValue::Vector {
            coord: VectorCoordKind::Int32,
            dim: 2,
            values: vec![CypherValue::Int(1), CypherValue::Int(2)],
        };
        let same = a.clone();
        assert_eq!(a, same);

        let different_coord = CypherValue::Vector {
            coord: VectorCoordKind::Int64,
            dim: 2,
            values: vec![CypherValue::Int(1), CypherValue::Int(2)],
        };
        assert_ne!(a, different_coord);

        let different_values = CypherValue::Vector {
            coord: VectorCoordKind::Int32,
            dim: 2,
            values: vec![CypherValue::Int(2), CypherValue::Int(1)],
        };
        assert_ne!(a, different_values);

        // A vector is never equal to a plain list, even with identical
        // element values.
        let list = CypherValue::List(vec![CypherValue::Int(1), CypherValue::Int(2)]);
        assert_ne!(a, list);
    }

    #[test]
    fn test_vector_type_name_and_ordering_is_unordered() {
        let a = CypherValue::Vector {
            coord: VectorCoordKind::Int32,
            dim: 1,
            values: vec![CypherValue::Int(1)],
        };
        let b = a.clone();
        assert_eq!(a.type_name(), "VECTOR");
        // `CypherValue::partial_cmp` deliberately has no Vector<->Vector
        // arm: the CIP-200 `</>/<=/>=` three-valued-logic special case
        // (always null except `<=`/`>=` on equal vectors) is handled at
        // the comparison-operator level, not via `PartialOrd`.
        assert_eq!(a.partial_cmp(&b), None);
    }

    #[test]
    fn test_vector_coord_kind_aliases_and_canonical_names() {
        assert_eq!(
            VectorCoordKind::from_alias("INT8"),
            Some(VectorCoordKind::Int8)
        );
        assert_eq!(
            VectorCoordKind::from_alias("INTEGER8"),
            Some(VectorCoordKind::Int8)
        );
        assert_eq!(
            VectorCoordKind::from_alias("SIGNED INTEGER"),
            Some(VectorCoordKind::Int64)
        );
        assert_eq!(
            VectorCoordKind::from_alias("FLOAT64"),
            Some(VectorCoordKind::Float64)
        );
        assert_eq!(VectorCoordKind::from_alias("STRING"), None);

        assert_eq!(VectorCoordKind::Int64.canonical_name(), "INTEGER");
        assert_eq!(VectorCoordKind::Float64.canonical_name(), "FLOAT");
        assert_eq!(VectorCoordKind::Float32.canonical_name(), "FLOAT32");

        // Coordinate-type ordering rank (CIP-200 "Ordering of the Vector
        // Type"): INTEGER8 < INTEGER16 < INTEGER32 < INTEGER64 < FLOAT32
        // < FLOAT64.
        assert!(VectorCoordKind::Int8.order_rank() < VectorCoordKind::Int16.order_rank());
        assert!(VectorCoordKind::Int16.order_rank() < VectorCoordKind::Int32.order_rank());
        assert!(VectorCoordKind::Int32.order_rank() < VectorCoordKind::Int64.order_rank());
        assert!(VectorCoordKind::Int64.order_rank() < VectorCoordKind::Float32.order_rank());
        assert!(VectorCoordKind::Float32.order_rank() < VectorCoordKind::Float64.order_rank());
    }
}
