//! The typed, validated ontology document model.
//!
//! [`OntologyDocument`] is produced only by
//! [`crate::parse_ontology_document`], so every instance has passed the
//! spec-version gate and the structural validation of ADR-0364 §1/§2 (which
//! supersede ADR-0227 §6/§7). The typed views ([`NodeType`],
//! [`RelationshipType`], [`Property`], [`Constraint`]) preserve document
//! order everywhere — ADR-0226's deterministic id assignment depends on it.
//!
//! The document's root [`Value`] tree is retained alongside the typed view
//! ([`OntologyDocument::root`]) so canonical serialization
//! ([`OntologyDocument::to_json`], [`OntologyDocument::to_yaml`]) preserves
//! the document's exact source key order (ADR-0227 §3).
//!
//! Main's shape differs from the pinned draft in ways the model mirrors
//! (ADR-0364 §2): `version` is a string carried verbatim, node,
//! relationship and property entries carry a nullable display `name` while
//! the map key is the element identity, constraints are id-keyed maps whose
//! category is the uppercase `type` enum, endpoints are
//! [`RelationshipTarget`]s with no required members, and the extension
//! envelopes are closed per-element objects whose payloads stay on the
//! retained root value tree (validated in place, never converted —
//! ADR-0364 §5.1).

use crate::error::OntologyError;
use crate::serialize;
use crate::value::Value;

/// A validated ontology document (main Ontology Graph Spec 1.0.0,
/// ADR-0364).
#[derive(Debug, Clone, PartialEq)]
pub struct OntologyDocument {
    root: Value,
    schema: String,
    id: String,
    version: String,
    name: Option<String>,
    description: Option<String>,
    nodes: Vec<NodeType>,
    relationships: Vec<RelationshipType>,
}

impl OntologyDocument {
    /// Assemble the model from validated parts. Only [`crate::validate`]
    /// calls this.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        root: Value,
        schema: String,
        id: String,
        version: String,
        name: Option<String>,
        description: Option<String>,
        nodes: Vec<NodeType>,
        relationships: Vec<RelationshipType>,
    ) -> Self {
        Self {
            root,
            schema,
            id,
            version,
            name,
            description,
            nodes,
            relationships,
        }
    }

    /// The document's root value tree, in source key order.
    #[must_use]
    pub fn root(&self) -> &Value {
        &self.root
    }

    /// The `schema` URI (always the pinned main URI).
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// The ontology's unique identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The author-defined version string, carried verbatim (ADR-0364 §2).
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The human-readable name, when present.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The human-readable description, when present.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Node type entries, in document order.
    #[must_use]
    pub fn nodes(&self) -> &[NodeType] {
        &self.nodes
    }

    /// Relationship type entries, in document order.
    #[must_use]
    pub fn relationships(&self) -> &[RelationshipType] {
        &self.relationships
    }

    /// Canonical JSON serialization: compact UTF-8, keys in document order
    /// (ADR-0227 §3). Depth-capped at 128 levels — parsed documents are
    /// pre-bounded by the parse screen; the cap bounds hand-built values.
    pub fn to_json(&self) -> Result<String, OntologyError> {
        serialize::to_json_string(&self.root)
    }

    /// Canonical YAML serialization: block style with two-space
    /// indentation, keys in document order (ADR-0227 §3). Depth-capped at
    /// 128 levels like [`Self::to_json`].
    pub fn to_yaml(&self) -> Result<String, OntologyError> {
        serialize::to_yaml_string(&self.root)
    }
}

/// A node type entry from the document's `nodes` map.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeType {
    /// The map key — the identifier within the document, not the label.
    pub key: String,
    /// The labels object. The `label` shorthand is desugared into this
    /// one form at validation (`crate::validate`), so the model carries
    /// no dual representation.
    pub labels: Labels,
    /// Nullable display name (never compiled, ADR-0364 §5.1).
    pub name: Option<String>,
    /// Properties, in document order.
    pub properties: Vec<Property>,
    /// Constraints, keyed by id in document order.
    pub constraints: Vec<Constraint>,
    /// Alternative names for this node type.
    pub aliases: Vec<String>,
    /// Informational URI pointing at an external definition.
    pub reference: Option<String>,
    /// Human-readable description.
    pub description: Option<String>,
}

impl NodeType {
    /// The identifying label: `labels.identifier`, the one form (the
    /// `label` shorthand is desugared at validation, so there is no
    /// fallback). `None` when the document left the label null or absent —
    /// main-valid, and a `Mapping` failure at `apply`, not at validation
    /// (ADR-0364 §3).
    #[must_use]
    pub fn identifying_label(&self) -> Option<&str> {
        self.labels.identifier.as_deref()
    }
}

/// The full labels object of a node type: the nullable identifying label
/// plus implied and optional labels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Labels {
    /// The identifying (main) label, nullable in main's schema.
    pub identifier: Option<String>,
    /// Labels entailed by the identifying label.
    pub implied: Vec<String>,
    /// Labels that may be present on instances but are not guaranteed.
    pub optional: Vec<String>,
}

/// A relationship type entry from the document's `relationships` map.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipType {
    /// The map key — the intra-document identifier.
    pub key: String,
    /// The relationship type (`type` in the schema).
    pub rel_type: String,
    /// The start endpoint.
    pub from: RelationshipTarget,
    /// The end endpoint.
    pub to: RelationshipTarget,
    /// Nullable display name (never compiled, ADR-0364 §5.1).
    pub name: Option<String>,
    /// Properties, in document order.
    pub properties: Vec<Property>,
    /// Constraints, keyed by id in document order.
    pub constraints: Vec<Constraint>,
    /// Alternative names for this relationship type.
    pub aliases: Vec<String>,
    /// Informational URI pointing at an external definition.
    pub reference: Option<String>,
    /// Human-readable description.
    pub description: Option<String>,
}

/// One end of a relationship (main's `RelationshipTarget`).
///
/// Main requires no member: `node` and `label` are optional strings and the
/// cardinality fields are nullable integers with no minimums. A target with
/// no `node` is accepted at validation and fails at `apply` with a
/// `Mapping` error (ADR-0364 §3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RelationshipTarget {
    /// A node id from the `nodes` map; authoritative when present.
    pub node: Option<String>,
    /// A label at this end; descriptive, never used for resolution.
    pub label: Option<String>,
    /// Exact relationship count per endpoint instance, nullable.
    pub count: Option<i64>,
    /// Minimum relationship count per endpoint instance, nullable.
    pub min_count: Option<i64>,
    /// Maximum relationship count per endpoint instance, nullable.
    pub max_count: Option<i64>,
}

/// A property of a node or relationship type.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    /// The map key — the property's addressable id (never the display
    /// name, ADR-0364 §5.1).
    pub id: String,
    /// The `type` token from main's closed [`Neo4jType`] enum, when
    /// present.
    pub property_type: Option<Neo4jType>,
    /// The `VECTOR` companion dimension (null = unconstrained).
    pub dimension: Option<i64>,
    /// Shorthand for a single-property existence constraint (`must_exist`
    /// in main; the pinned draft's `mustExist` is gone).
    pub must_exist: Option<bool>,
    /// Shorthand for a single-property `unique` constraint.
    pub unique: Option<bool>,
    /// Shorthand for a single-property `key` constraint.
    pub key: Option<bool>,
    /// Allowed values (`one_of`), strings only in main.
    pub one_of: Option<Vec<String>>,
    /// Regular expression the property value must match.
    pub pattern: Option<String>,
    /// Nullable display name (never compiled).
    pub name: Option<String>,
    /// Informational URI pointing at an external definition.
    pub reference: Option<String>,
    /// Human-readable description.
    pub description: Option<String>,
}

/// A constraint object (the nameable alternative to the shorthand flags),
/// carried as an ordered id-keyed map entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    /// The map key — the constraint's stable identity, used in
    /// diagnostics and never compiled (ADR-0364 §5.1).
    pub id: String,
    /// The constraint kind (`type` in main).
    pub constraint_type: ConstraintType,
    /// Ids of properties on the same element this constraint covers
    /// (empty allowed at validation; `apply` rejects an empty list).
    pub properties: Vec<String>,
    /// Nullable node label; always `None` for relationship constraints
    /// (main has no `label` member there).
    pub label: Option<String>,
    /// Nullable display name, carried as the graph-type constraint name
    /// when present.
    pub name: Option<String>,
}

/// The constraint kind of a [`Constraint`] object (main's uppercased
/// `type` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintType {
    /// Must exist.
    Exists,
    /// Unique + must exist.
    Key,
    /// A property-type constraint (main's `PROPERTY_TYPE`; accepted at
    /// validation, a `Mapping` failure at `apply`).
    PropertyType,
    /// Unique.
    Unique,
}

impl ConstraintType {
    /// The schema token for this constraint kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        CONSTRAINTS
            .iter()
            .find_map(|(token, kind)| (*kind == self).then_some(*token))
            .expect("every ConstraintType variant is listed in CONSTRAINTS")
    }

    /// Parse a main `type` token.
    pub(crate) fn parse(token: &str) -> Option<ConstraintType> {
        CONSTRAINTS
            .iter()
            .find_map(|(candidate, kind)| (*candidate == token).then_some(*kind))
    }
}

/// The closed `ConstraintType` vocabulary in schema order. `pub(crate)` so
/// the validator's expected-token message and the fixture coverage test walk
/// the one canonical table instead of duplicating it.
pub(crate) const CONSTRAINTS: &[(&str, ConstraintType)] = &[
    ("EXISTS", ConstraintType::Exists),
    ("KEY", ConstraintType::Key),
    ("PROPERTY_TYPE", ConstraintType::PropertyType),
    ("UNIQUE", ConstraintType::Unique),
];

/// A property `type` token from main's closed 39-token `Neo4jType` enum
/// (ADR-0364 §2).
///
/// The enum is the document model's vocabulary, not the CIP-176 mapping:
/// the ADR-0226 converter's token table lives in the graph-type module and
/// is free to reject tokens the document schema accepts (ADR-0364 §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Neo4jType {
    /// `ANY`
    Any,
    /// `LIST<ANY>`
    ListAny,
    /// `BOOLEAN`
    Boolean,
    /// `LIST<BOOLEAN>`
    ListBoolean,
    /// `DATE`
    Date,
    /// `LIST<DATE>`
    ListDate,
    /// `DURATION`
    Duration,
    /// `LIST<DURATION>`
    ListDuration,
    /// `FLOAT32`
    Float32,
    /// `LIST<FLOAT32>`
    ListFloat32,
    /// `FLOAT`
    Float,
    /// `LIST<FLOAT>`
    ListFloat,
    /// `INTEGER8`
    Integer8,
    /// `LIST<INTEGER8>`
    ListInteger8,
    /// `INTEGER16`
    Integer16,
    /// `LIST<INTEGER16>`
    ListInteger16,
    /// `INTEGER32`
    Integer32,
    /// `LIST<INTEGER32>`
    ListInteger32,
    /// `INTEGER`
    Integer,
    /// `LIST<INTEGER>`
    ListInteger,
    /// `LOCAL DATETIME`
    LocalDatetime,
    /// `LIST<LOCAL DATETIME>`
    ListLocalDatetime,
    /// `LOCAL TIME`
    LocalTime,
    /// `LIST<LOCAL TIME>`
    ListLocalTime,
    /// `POINT`
    Point,
    /// `LIST<POINT>`
    ListPoint,
    /// `STRING`
    String,
    /// `LIST<STRING>`
    ListString,
    /// `VECTOR<FLOAT>`
    VectorFloat,
    /// `VECTOR<FLOAT32>`
    VectorFloat32,
    /// `VECTOR<INTEGER>`
    VectorInteger,
    /// `VECTOR<INTEGER32>`
    VectorInteger32,
    /// `VECTOR<INTEGER16>`
    VectorInteger16,
    /// `VECTOR<INTEGER8>`
    VectorInteger8,
    /// `ZONED DATETIME`
    ZonedDatetime,
    /// `LIST<ZONED DATETIME>`
    ListZonedDatetime,
    /// `ZONED TIME`
    ZonedTime,
    /// `LIST<ZONED TIME>`
    ListZonedTime,
    /// `UUID`
    Uuid,
}

impl Neo4jType {
    /// The schema token for this type.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        NEO4J_TYPES
            .iter()
            .find_map(|(token, ty)| (*ty == self).then_some(*token))
            .expect("every Neo4jType variant is listed in NEO4J_TYPES")
    }

    /// Parse a `Neo4jType` token. The table is the closed vocabulary: any
    /// other spelling (including the pinned draft's `TIME`, `DATETIME`,
    /// `LOCALTIME`, `LOCALDATETIME` and `BYTES`) is rejected.
    pub(crate) fn parse(token: &str) -> Option<Neo4jType> {
        NEO4J_TYPES
            .iter()
            .find_map(|(candidate, ty)| (*candidate == token).then_some(*ty))
    }
}

/// The closed `Neo4jType` vocabulary in schema order. `pub(crate)` so the
/// converter's vocabulary test can walk the one canonical table instead of
/// duplicating it.
pub(crate) const NEO4J_TYPES: &[(&str, Neo4jType)] = &[
    ("ANY", Neo4jType::Any),
    ("LIST<ANY>", Neo4jType::ListAny),
    ("BOOLEAN", Neo4jType::Boolean),
    ("LIST<BOOLEAN>", Neo4jType::ListBoolean),
    ("DATE", Neo4jType::Date),
    ("LIST<DATE>", Neo4jType::ListDate),
    ("DURATION", Neo4jType::Duration),
    ("LIST<DURATION>", Neo4jType::ListDuration),
    ("FLOAT32", Neo4jType::Float32),
    ("LIST<FLOAT32>", Neo4jType::ListFloat32),
    ("FLOAT", Neo4jType::Float),
    ("LIST<FLOAT>", Neo4jType::ListFloat),
    ("INTEGER8", Neo4jType::Integer8),
    ("LIST<INTEGER8>", Neo4jType::ListInteger8),
    ("INTEGER16", Neo4jType::Integer16),
    ("LIST<INTEGER16>", Neo4jType::ListInteger16),
    ("INTEGER32", Neo4jType::Integer32),
    ("LIST<INTEGER32>", Neo4jType::ListInteger32),
    ("INTEGER", Neo4jType::Integer),
    ("LIST<INTEGER>", Neo4jType::ListInteger),
    ("LOCAL DATETIME", Neo4jType::LocalDatetime),
    ("LIST<LOCAL DATETIME>", Neo4jType::ListLocalDatetime),
    ("LOCAL TIME", Neo4jType::LocalTime),
    ("LIST<LOCAL TIME>", Neo4jType::ListLocalTime),
    ("POINT", Neo4jType::Point),
    ("LIST<POINT>", Neo4jType::ListPoint),
    ("STRING", Neo4jType::String),
    ("LIST<STRING>", Neo4jType::ListString),
    ("VECTOR<FLOAT>", Neo4jType::VectorFloat),
    ("VECTOR<FLOAT32>", Neo4jType::VectorFloat32),
    ("VECTOR<INTEGER>", Neo4jType::VectorInteger),
    ("VECTOR<INTEGER32>", Neo4jType::VectorInteger32),
    ("VECTOR<INTEGER16>", Neo4jType::VectorInteger16),
    ("VECTOR<INTEGER8>", Neo4jType::VectorInteger8),
    ("ZONED DATETIME", Neo4jType::ZonedDatetime),
    ("LIST<ZONED DATETIME>", Neo4jType::ListZonedDatetime),
    ("ZONED TIME", Neo4jType::ZonedTime),
    ("LIST<ZONED TIME>", Neo4jType::ListZonedTime),
    ("UUID", Neo4jType::Uuid),
];
