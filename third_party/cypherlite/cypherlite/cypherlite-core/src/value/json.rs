//! JSON encode/decode for [`Value`]: the tagged `{"t":..., "v":...}` form
//! (WAL/checkpoint bytes) and the plain FFI form, plus the `Tagged*Ref` serde
//! impls behind the canonical tagged encoder.

use super::*;

impl Value {
    /// The tagged `{"t":..., "v":...}` JSON form. Built from the canonical
    /// [`serde::Serialize`] impl (via [`TaggedRef`]), so the tree here and the
    /// direct WAL/checkpoint bytes cannot diverge. Graph entities are not
    /// persistence properties: the encoder rejects them (see [`TaggedRef`]),
    /// and that internal error maps to `Null` here.
    pub fn to_json_tagged(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    pub fn from_json_tagged(v: &serde_json::Value) -> crate::Result<Value> {
        fn malformed(expected: &str) -> crate::Error {
            crate::Error::storage(format!("malformed tagged value: expected {expected}"))
        }
        let obj = v
            .as_object()
            .ok_or_else(|| crate::Error::storage("property JSON must be an object"))?;
        let t = obj
            .get("t")
            .and_then(|x| x.as_str())
            .ok_or_else(|| crate::Error::storage("missing type tag"))?;
        Ok(match t {
            "null" => Value::Null,
            "bool" => Value::Bool(
                obj.get("v")
                    .and_then(|x| x.as_bool())
                    .ok_or_else(|| malformed("\"bool\" with a boolean \"v\""))?,
            ),
            "int" => Value::Int(
                obj.get("v")
                    .and_then(|x| x.as_i64())
                    .ok_or_else(|| malformed("\"int\" with an integer \"v\""))?,
            ),
            "float" => Value::Float(match obj.get("v") {
                Some(serde_json::Value::String(s)) if s == "NaN" => f64::NAN,
                Some(serde_json::Value::String(s)) if s == "Infinity" => f64::INFINITY,
                Some(serde_json::Value::String(s)) if s == "-Infinity" => f64::NEG_INFINITY,
                other => other
                    .and_then(|x| x.as_f64())
                    .ok_or_else(|| malformed("\"float\" with a number \"v\""))?,
            }),
            "string" => Value::String(
                obj.get("v")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| malformed("\"string\" with a string \"v\""))?
                    .into(),
            ),
            "uuid" => {
                let s = obj
                    .get("v")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| malformed("\"uuid\" with a string \"v\""))?;
                Value::Uuid(
                    uuid_parse(s)
                        .ok_or_else(|| malformed("\"uuid\" with a canonical hyphenated \"v\""))?,
                )
            }
            "list" => {
                let arr = obj
                    .get("v")
                    .and_then(|x| x.as_array())
                    .ok_or_else(|| malformed("\"list\" with an array \"v\""))?;
                Value::List(
                    arr.iter()
                        .map(Value::from_json_tagged)
                        .collect::<crate::Result<Vec<_>>>()?
                        .into(),
                )
            }
            "map" => {
                let mut map = BTreeMap::new();
                let o = obj
                    .get("v")
                    .and_then(|x| x.as_object())
                    .ok_or_else(|| malformed("\"map\" with an object \"v\""))?;
                for (k, val) in o {
                    map.insert(k.clone(), Value::from_json_tagged(val)?);
                }
                Value::Map(map.into())
            }
            "date" | "time" | "localtime" | "datetime" | "localdatetime" | "duration" => {
                let s = obj
                    .get("v")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| crate::Error::storage("missing temporal value"))?;
                Value::Temporal(crate::temporal::parse_iso(t, s)?)
            }
            "point" => {
                let p = obj
                    .get("v")
                    .and_then(|x| x.as_object())
                    .ok_or_else(|| malformed("\"point\" with an object \"v\""))?;
                Value::Point(PointValue {
                    srid: p
                        .get("srid")
                        .and_then(|x| x.as_i64())
                        .and_then(|x| i32::try_from(x).ok())
                        .ok_or_else(|| malformed("\"point\" with an integer \"srid\""))?,
                    x: p.get("x")
                        .and_then(|x| x.as_f64())
                        .ok_or_else(|| malformed("\"point\" with a number \"x\""))?,
                    y: p.get("y")
                        .and_then(|x| x.as_f64())
                        .ok_or_else(|| malformed("\"point\" with a number \"y\""))?,
                    z: p.get("z").and_then(|x| x.as_f64()),
                })
            }
            "vector" => {
                let coord = obj
                    .get("coord")
                    .and_then(|x| x.as_str())
                    .and_then(VectorCoordKind::from_canonical_name)
                    .ok_or_else(|| crate::Error::storage("invalid vector coordinate type"))?;
                let xs = obj
                    .get("v")
                    .and_then(|x| x.as_array())
                    .ok_or_else(|| malformed("\"vector\" with an array \"v\""))?;
                let values = xs
                    .iter()
                    .map(Value::from_json_tagged)
                    .collect::<crate::Result<Vec<_>>>()?;
                Value::Vector {
                    coord,
                    values: values.into(),
                }
            }
            other => return Err(crate::Error::storage(format!("unknown type tag {other:?}"))),
        })
    }

    /// Best-effort Python/JSON-ish value for FFI clients.
    pub fn to_json_plain(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => json!(b),
            Value::Int(i) => json!(i),
            Value::Float(f) => json!(f),
            Value::String(s) => json!(&**s),
            Value::Uuid(u) => json!(uuid_hyphenated(*u)),
            // The byte values 0-255, the `value_to_cypher` shape (ADR-0327).
            Value::Bytes(b) => json!(&**b),
            Value::List(xs) => json!(xs.iter().map(Value::to_json_plain).collect::<Vec<_>>()),
            Value::Map(m) => map_to_json_plain(m.iter()),
            Value::Node(n) => node_json(n),
            Value::Relationship(r) => rel_json(r),
            Value::Path(p) => json!({
                "_type": "path",
                "nodes": p.nodes.iter().map(node_json).collect::<Vec<_>>(),
                "relationships": p.rels.iter().map(rel_json).collect::<Vec<_>>(),
            }),
            Value::Temporal(t) => json!({
                "_type": t.kind_name(),
                "value": t.iso_string(),
            }),
            Value::Point(p) => json!({
                "_type": "point", "srid": p.srid, "x": p.x, "y": p.y, "z": p.z
            }),
            Value::Vector { coord, values } => json!({
                "_type": "vector",
                "coordinateType": coord.canonical_name(),
                "values": values.iter().map(Value::to_json_plain).collect::<Vec<_>>()
            }),
        }
    }

    pub fn from_json_plain(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(f) = n.as_f64() {
                    Value::Float(f)
                } else {
                    Value::Null
                }
            }
            serde_json::Value::String(s) => Value::String(s.as_str().into()),
            serde_json::Value::Array(a) => Value::List(
                a.iter()
                    .map(Value::from_json_plain)
                    .collect::<Vec<_>>()
                    .into(),
            ),
            serde_json::Value::Object(o) => {
                // Probe the `_type` tag exactly once: the handled set
                // ("node", "point", "vector") and the fall-through to a
                // plain map are explicit in one place.
                match o.get("_type").and_then(|t| t.as_str()) {
                    Some("node") => {
                        return Value::Node(Arc::new(Node {
                            id: o.get("id").and_then(|x| x.as_i64()).unwrap_or(0),
                            labels: Arc::new(
                                o.get("labels")
                                    .and_then(|x| x.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default(),
                            ),
                            properties: match o.get("properties") {
                                Some(p) => match Value::from_json_plain(p) {
                                    Value::Map(m) => PropertyMap::share(Arc::unwrap_or_clone(m)),
                                    _ => PropertyMap::share(PropertyMap::new()),
                                },
                                None => PropertyMap::share(PropertyMap::new()),
                            },
                        }));
                    }
                    Some("point") => {
                        return Value::Point(PointValue {
                            srid: o
                                .get("srid")
                                .and_then(|x| x.as_i64())
                                .and_then(|x| i32::try_from(x).ok())
                                .unwrap_or(7203),
                            x: o.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            y: o.get("y").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            z: o.get("z").and_then(|x| x.as_f64()),
                        });
                    }
                    Some("vector") => {
                        if let Some(coord) = o
                            .get("coordinateType")
                            .and_then(|x| x.as_str())
                            .and_then(VectorCoordKind::from_canonical_name)
                        {
                            let values: Vec<Value> = o
                                .get("values")
                                .and_then(|x| x.as_array())
                                .map(|xs| xs.iter().map(Value::from_json_plain).collect())
                                .unwrap_or_default();
                            return Value::Vector {
                                coord,
                                values: values.into(),
                            };
                        }
                    }
                    _ => {}
                }
                let mut m = BTreeMap::new();
                for (k, val) in o {
                    m.insert(k.clone(), Value::from_json_plain(val));
                }
                Value::Map(m.into())
            }
        }
    }

    /// Decode JSON whose objects are always maps. A `_type` of `node`,
    /// `point`, or `vector` stays a string property. This is the decoder for
    /// untrusted property documents. [`from_json_plain`] is the inverse of
    /// [`to_json_plain`] and still recognizes those tags.
    pub fn from_json_scalars(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(f) = n.as_f64() {
                    Value::Float(f)
                } else {
                    Value::Null
                }
            }
            serde_json::Value::String(s) => Value::String(s.as_str().into()),
            serde_json::Value::Array(a) => Value::List(
                a.iter()
                    .map(Value::from_json_scalars)
                    .collect::<Vec<_>>()
                    .into(),
            ),
            serde_json::Value::Object(o) => {
                let mut m = BTreeMap::new();
                for (k, val) in o {
                    m.insert(k.clone(), Value::from_json_scalars(val));
                }
                Value::Map(m.into())
            }
        }
    }
}

/// Borrowing encoder of the tagged string `{"t":"string","v":...}`.
/// Construct it with [`tagged_str`]. [`Value`]'s tagged encoder writes the
/// same two entries.
pub struct TaggedStr<'a>(&'a str);

fn write_tagged_str<M: serde::ser::SerializeMap>(m: &mut M, s: &str) -> Result<(), M::Error> {
    m.serialize_entry("t", "string")?;
    m.serialize_entry("v", s)?;
    Ok(())
}

impl serde::Serialize for TaggedStr<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = serializer.serialize_map(Some(2))?;
        write_tagged_str(&mut m, self.0)?;
        m.end()
    }
}

/// The tagged string [`TaggedRef`] writes for [`Value::String`].
pub fn tagged_str(s: &str) -> TaggedStr<'_> {
    TaggedStr(s)
}

/// Serializes a [`Value`] as the tagged `{"t":..., ...}` JSON that both the
/// WAL and the Parquet checkpoint codec record — directly, without building an
/// intermediate `serde_json::Value` tree. This is the single canonical tagged
/// encoder; [`Value::to_json_tagged`] produces its `serde_json::Value` from it,
/// so the two cannot drift.
///
/// Graph entities ([`Value::Node`], [`Value::Relationship`],
/// [`Value::Path`]) are not persistence properties: encoding one is a
/// serialization error, mirroring the v2 persistence codec's "graph entity
/// is not a persistence property" rejection. Byte arrays are the second
/// not-a-persistence-property rejection class (ADR-0327: statement-scoped,
/// and the executor's property check rejects them with a typed `TypeError`
/// before persistence), with their own "byte array is not a persistence
/// property" error.
struct TaggedRef<'a>(&'a Value);

impl serde::Serialize for TaggedRef<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = serializer.serialize_map(Some(2))?;
        match self.0 {
            Value::Null => m.serialize_entry("t", "null")?,
            Value::Bool(b) => {
                m.serialize_entry("t", "bool")?;
                m.serialize_entry("v", b)?;
            }
            Value::Int(i) => {
                m.serialize_entry("t", "int")?;
                m.serialize_entry("v", i)?;
            }
            Value::Float(f) => {
                m.serialize_entry("t", "float")?;
                if f.is_nan() {
                    m.serialize_entry("v", "NaN")?;
                } else if *f == f64::INFINITY {
                    m.serialize_entry("v", "Infinity")?;
                } else if *f == f64::NEG_INFINITY {
                    m.serialize_entry("v", "-Infinity")?;
                } else {
                    m.serialize_entry("v", f)?;
                }
            }
            Value::String(s) => write_tagged_str(&mut m, s)?,
            Value::Uuid(u) => {
                m.serialize_entry("t", "uuid")?;
                m.serialize_entry("v", &uuid_hyphenated(*u))?;
            }
            Value::List(xs) => {
                m.serialize_entry("t", "list")?;
                m.serialize_entry("v", &TaggedListRef(xs))?;
            }
            Value::Map(map) => {
                m.serialize_entry("t", "map")?;
                m.serialize_entry("v", &TaggedMapRef(map))?;
            }
            // Invariant: this encoder records graph *property* values only,
            // and a property is never a graph entity -- `sanitize_props`
            // (cypherlite-executor) rejects Node/Relationship/Path before
            // persistence, and the documented v2 persistence codec
            // (cypherlite-model `serde_value`) errors with "graph entity is
            // not a persistence property". Fail loudly instead of silently
            // writing the entity as a null tag.
            Value::Node(_) | Value::Relationship(_) | Value::Path(_) => {
                return Err(serde::ser::Error::custom(
                    "graph entity is not a persistence property",
                ));
            }
            // Invariant, as for graph entities: a byte array is
            // statement-scoped (ADR-0327) and the executor's property check
            // rejects it with a typed TypeError before persistence. The
            // tagged codec has no byte-array tag; fail loudly instead.
            Value::Bytes(_) => {
                return Err(serde::ser::Error::custom(
                    "byte array is not a persistence property",
                ));
            }
            Value::Temporal(t) => {
                m.serialize_entry("t", t.kind_name())?;
                m.serialize_entry("v", &t.iso_string())?;
            }
            Value::Point(p) => {
                m.serialize_entry("t", "point")?;
                m.serialize_entry("v", &TaggedPointRef(p))?;
            }
            Value::Vector { coord, values } => {
                // serde_json's Map is a BTreeMap: keys serialize sorted, so
                // "coord" precedes "t". Match the historical bytes.
                m.serialize_entry("coord", coord.canonical_name())?;
                m.serialize_entry("t", "vector")?;
                m.serialize_entry("v", &TaggedListRef(values))?;
            }
        }
        m.end()
    }
}

struct TaggedListRef<'a>(&'a [Value]);
impl serde::Serialize for TaggedListRef<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for v in self.0 {
            seq.serialize_element(&TaggedRef(v))?;
        }
        seq.end()
    }
}

struct TaggedMapRef<'a>(&'a BTreeMap<String, Value>);
impl serde::Serialize for TaggedMapRef<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            m.serialize_entry(k, &TaggedRef(v))?;
        }
        m.end()
    }
}

struct TaggedPointRef<'a>(&'a PointValue);
impl serde::Serialize for TaggedPointRef<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let p = self.0;
        let mut m = s.serialize_map(Some(4))?;
        m.serialize_entry("srid", &p.srid)?;
        m.serialize_entry("x", &p.x)?;
        m.serialize_entry("y", &p.y)?;
        m.serialize_entry("z", &p.z)?;
        m.end()
    }
}

impl serde::Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        TaggedRef(self).serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = serde_json::Value::deserialize(deserializer)?;
        Value::from_json_tagged(&json).map_err(serde::de::Error::custom)
    }
}

fn map_to_json_plain<'a>(
    m: impl IntoIterator<Item = (&'a String, &'a Value)>,
) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    for (k, v) in m {
        o.insert(k.clone(), v.to_json_plain());
    }
    serde_json::Value::Object(o)
}

/// Plain-JSON form of a node, shared by the [`Value::Node`] arm and the
/// [`Value::Path`] arm of [`Value::to_json_plain`], which renders each path
/// element directly instead of cloning it into a [`Value`] and re-entering
/// the dispatcher one arm down.
fn node_json(n: &Node) -> serde_json::Value {
    serde_json::json!({
        "_type": "node",
        "id": n.id,
        "labels": n.labels,
        "properties": map_to_json_plain(n.properties.iter()),
    })
}

/// Plain-JSON form of a relationship (see [`node_json`]).
fn rel_json(r: &Relationship) -> serde_json::Value {
    serde_json::json!({
        "_type": "relationship",
        "id": r.id,
        "start": r.start,
        "end": r.end,
        "type": r.rel_type,
        "properties": map_to_json_plain(r.properties.iter()),
    })
}
