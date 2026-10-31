//! CIP-176/177/178/180: the graph-type specification sublanguage's
//! semantic layer -- everything that operates on a parsed
//! `cypherlite_parser::GraphTypeBody` beyond grammar: the CIP-178 "shall"
//! rules (`validate_graph_type_body`, invoked only by `CREATE GRAPH TYPE`
//! -- see `GraphTypeBody`'s doc comment for why `ALTER CURRENT GRAPH TYPE`
//! skips it), the CIP-180 canonical string transformation
//! (`canonicalize_graph_type_body`), the CIP-176/182/183 `ALTER CURRENT
//! GRAPH TYPE` per-verb body algebra (`apply_set` / `apply_add` /
//! `apply_alter` / `apply_drop`, dispatched by `apply_verb`), and the
//! CIP-176/187 compilation of a graph type into the flat constraint set it
//! enforces (`derive_constraints`).
//!
//! The graph-type *model* itself (`GraphTypeBody` and friends) lives in
//! `cypherlite-parser` since it's produced by parsing alone; this module is
//! the engine-side behavior built on top of that model.

use std::collections::{BTreeMap, HashMap, HashSet};

use cypherlite_parser::token::quote_identifier_always;

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_model::{Constraint, ConstraintType, EntityKind};
use cypherlite_parser::type_normalize::{render_canon, type_expr_to_canon, CanonKind};
use cypherlite_parser::{
    Expr, GraphTypeBody, GraphTypeConstraintSpec, GraphTypeConstraintVerb, GraphTypeEdgeTypeSpec,
    GraphTypeElementRef, GraphTypeEntityQualifier, GraphTypeInlineConstraint, GraphTypeInlineVerb,
    GraphTypeNodeTypeSpec, GraphTypePropertyType, LiteralValue, PredefinedKind, TypeExpr,
    VectorCoordKind,
};
use serde::{Deserialize, Serialize};

/// Every CIP-178 validation failure in this module renders as `"SyntaxError:
/// <message>"`, matching the historical stringly errors byte-for-byte (the
/// prime invariant). This one-line constructor carries the
/// [`ErrorKind::SyntaxError`] kind natively while keeping the message the
/// bare detail text.
fn syn(msg: impl Into<String>) -> EngineError {
    EngineError::semantic(ErrorKind::SyntaxError, msg)
}

mod alter;
mod canon;
mod canonicalize;
mod default_value;
mod derive;
mod inherit;
mod storage_constraint;
mod validate;

pub(crate) use alter::*;
pub(crate) use canon::{canon_allows, canon_intersection};
pub(crate) use canonicalize::*;
pub use default_value::DefaultValue;
pub(crate) use default_value::{
    canonical_default_clause, eval_lowered_default, lowered_default, DEFAULT_ON_COMMIT_SEPARATOR,
};
pub(crate) use derive::*;
pub(crate) use inherit::*;
pub(crate) use storage_constraint::{
    render_storage_constraint, storage_constraint_rank, StorageConstraintStyle,
};
pub(crate) use validate::*;

/// Maps a [`derive_constraints`] failure over persisted (deserialized)
/// graph-type bytes -- a deserialized `DEFAULT ON COMMIT` constructor
/// whose name the CIP-262 table never admitted (a gated construct stored
/// by an enabled follower, then replayed or checkpoint-verified here) --
/// to the storage-corruption error family the committed redo paths use
/// for a corrupt spec. The one mapping every persisted-bytes derivation
/// site (redo preflight, redo replay, checkpoint verification) consults,
/// so a poisoned persisted default surfaces as a typed `Error::storage`
/// at `Database::open`, never a panic.
pub(crate) fn persisted_derive_error(error: cypherlite_core::Error) -> cypherlite_core::Error {
    cypherlite_core::Error::storage(format!(
        "persisted graph type carries an invalid default value: {error}"
    ))
}

/// The same persisted-spec admission failure, from the validator's
/// `EngineError` vantage (the JSON-snapshot restore sibling): the one
/// message, mapped for the caller whose admission runs the validator.
pub(crate) fn persisted_validation_error(
    error: cypherlite_error::EngineError,
) -> cypherlite_core::Error {
    cypherlite_core::Error::storage(format!(
        "persisted graph type carries an invalid default value: {error}"
    ))
}

// ---------------------------------------------------------------------
// Persisted graph-type model (serde)
// ---------------------------------------------------------------------

/// The typed, serde-serializable form of a parsed
/// [`GraphTypeBody`] (CIP-176/178/180). Graph-type redo ops
/// (`RedoOp::PutGraphType` / `RedoOp::SetCurrentGraphType`) carry this
/// model and `TheaterData` stores it: parsing happens once at
/// DDL-execution time ([`GraphTypeSpec::from_body`]), and redo replay
/// converts back with [`GraphTypeSpec::to_body`] -- an infallible,
/// mechanical conversion, never a re-parse of Cypher text.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GraphTypeSpec {
    #[serde(default)]
    pub node_types: Vec<NodeTypeModel>,
    #[serde(default)]
    pub edge_types: Vec<EdgeTypeModel>,
    #[serde(default)]
    pub constraints: Vec<GraphTypeConstraintModel>,
    #[serde(default)]
    pub drop_constraint_names: Vec<String>,
}

impl GraphTypeSpec {
    /// Captures a parsed [`GraphTypeBody`] into its persisted model. A
    /// property default value whose expression is not one of the
    /// CIP-262 allowed shapes fails here (the same shape the body
    /// validator rejects) rather than being captured lossily -- the
    /// capture stays fail-closed even for a caller that skipped
    /// validation.
    pub fn from_body(body: &GraphTypeBody) -> Result<Self, EngineError> {
        Ok(Self {
            node_types: body
                .node_types
                .iter()
                .map(NodeTypeModel::capture)
                .collect::<Result<_, EngineError>>()?,
            edge_types: body
                .edge_types
                .iter()
                .map(EdgeTypeModel::capture)
                .collect::<Result<_, EngineError>>()?,
            constraints: body
                .constraints
                .iter()
                .map(GraphTypeConstraintModel::from)
                .collect(),
            drop_constraint_names: body.drop_constraint_names.clone(),
        })
    }

    /// Rebuilds the parser model for the semantic layer
    /// (`validate_graph_type_body`, `derive_constraints`,
    /// `canonicalize_graph_type_body`, ...). Infallible.
    pub fn to_body(&self) -> GraphTypeBody {
        GraphTypeBody {
            node_types: self
                .node_types
                .iter()
                .map(GraphTypeNodeTypeSpec::from)
                .collect(),
            edge_types: self
                .edge_types
                .iter()
                .map(GraphTypeEdgeTypeSpec::from)
                .collect(),
            constraints: self
                .constraints
                .iter()
                .map(GraphTypeConstraintSpec::from)
                .collect(),
            drop_constraint_names: self.drop_constraint_names.clone(),
        }
    }
}

/// Persisted form of [`GraphTypeNodeTypeSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeTypeModel {
    pub alias: Option<String>,
    pub identifying_label: String,
    #[serde(default)]
    pub implied_labels: Vec<String>,
    #[serde(default)]
    pub properties: Vec<PropertyTypeModel>,
}

impl NodeTypeModel {
    /// Captures one parsed node type specification, fail-closed on a
    /// property default value whose expression is not a CIP-262 allowed
    /// shape (the same shape the body validator rejects).
    pub fn capture(s: &GraphTypeNodeTypeSpec) -> Result<Self, EngineError> {
        Ok(Self {
            alias: s.alias.clone(),
            identifying_label: s.identifying_label.clone(),
            implied_labels: s.implied_labels.clone(),
            properties: s
                .properties
                .iter()
                .map(PropertyTypeModel::capture)
                .collect::<Result<_, EngineError>>()?,
        })
    }
}

impl From<&NodeTypeModel> for GraphTypeNodeTypeSpec {
    fn from(m: &NodeTypeModel) -> Self {
        Self {
            alias: m.alias.clone(),
            identifying_label: m.identifying_label.clone(),
            implied_labels: m.implied_labels.clone(),
            properties: m
                .properties
                .iter()
                .map(GraphTypePropertyType::from)
                .collect(),
        }
    }
}

/// Persisted form of [`GraphTypeEdgeTypeSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeTypeModel {
    pub alias: Option<String>,
    pub identifying_label: String,
    #[serde(default)]
    pub properties: Vec<PropertyTypeModel>,
    pub source: ElementRefModel,
    pub target: ElementRefModel,
}

impl EdgeTypeModel {
    /// Captures one parsed edge type specification, fail-closed on a
    /// property default value whose expression is not a CIP-262 allowed
    /// shape (the same shape the body validator rejects).
    pub fn capture(s: &GraphTypeEdgeTypeSpec) -> Result<Self, EngineError> {
        Ok(Self {
            alias: s.alias.clone(),
            identifying_label: s.identifying_label.clone(),
            properties: s
                .properties
                .iter()
                .map(PropertyTypeModel::capture)
                .collect::<Result<_, EngineError>>()?,
            source: ElementRefModel::from(&s.source),
            target: ElementRefModel::from(&s.target),
        })
    }
}

impl From<&EdgeTypeModel> for GraphTypeEdgeTypeSpec {
    fn from(m: &EdgeTypeModel) -> Self {
        Self {
            alias: m.alias.clone(),
            identifying_label: m.identifying_label.clone(),
            properties: m
                .properties
                .iter()
                .map(GraphTypePropertyType::from)
                .collect(),
            source: GraphTypeElementRef::from(&m.source),
            target: GraphTypeElementRef::from(&m.target),
        }
    }
}

/// Persisted form of [`GraphTypePropertyType`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PropertyTypeModel {
    pub name: String,
    pub value_type: ValueTypeModel,
    /// The property's `DEFAULT ON COMMIT` value (CIP-262), in its
    /// restricted, serde-shaped form.
    #[serde(default)]
    pub default_value: Option<DefaultValue>,
    pub inline_constraint: Option<InlineConstraintModel>,
}

impl PropertyTypeModel {
    /// Captures one parsed property type, fail-closed on a default value
    /// expression that is not a CIP-262 allowed shape.
    pub fn capture(s: &GraphTypePropertyType) -> Result<Self, EngineError> {
        Ok(Self {
            name: s.name.clone(),
            value_type: ValueTypeModel::from(&s.value_type),
            default_value: s
                .default_value
                .as_ref()
                .map(DefaultValue::from_expr)
                .transpose()?,
            inline_constraint: s
                .inline_constraint
                .as_ref()
                .map(InlineConstraintModel::from),
        })
    }
}

impl From<&PropertyTypeModel> for GraphTypePropertyType {
    fn from(m: &PropertyTypeModel) -> Self {
        Self {
            name: m.name.clone(),
            value_type: TypeExpr::from(&m.value_type),
            default_value: m.default_value.as_ref().map(DefaultValue::to_expr),
            inline_constraint: m
                .inline_constraint
                .as_ref()
                .map(GraphTypeInlineConstraint::from),
        }
    }
}

/// Persisted form of [`GraphTypeInlineConstraint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineConstraintModel {
    pub verb: InlineVerbModel,
    pub qualifier: Option<EntityQualifierModel>,
}

impl From<&GraphTypeInlineConstraint> for InlineConstraintModel {
    fn from(s: &GraphTypeInlineConstraint) -> Self {
        Self {
            verb: InlineVerbModel::from(s.verb),
            qualifier: s.qualifier.map(EntityQualifierModel::from),
        }
    }
}

impl From<&InlineConstraintModel> for GraphTypeInlineConstraint {
    fn from(m: &InlineConstraintModel) -> Self {
        Self {
            verb: GraphTypeInlineVerb::from(m.verb),
            qualifier: m.qualifier.map(GraphTypeEntityQualifier::from),
        }
    }
}

/// Persisted form of [`GraphTypeInlineVerb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InlineVerbModel {
    Key,
    Unique,
}

impl From<GraphTypeInlineVerb> for InlineVerbModel {
    fn from(v: GraphTypeInlineVerb) -> Self {
        match v {
            GraphTypeInlineVerb::Key => Self::Key,
            GraphTypeInlineVerb::Unique => Self::Unique,
        }
    }
}

impl From<InlineVerbModel> for GraphTypeInlineVerb {
    fn from(m: InlineVerbModel) -> Self {
        match m {
            InlineVerbModel::Key => Self::Key,
            InlineVerbModel::Unique => Self::Unique,
        }
    }
}

/// Persisted form of [`GraphTypeEntityQualifier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityQualifierModel {
    Node,
    Rel,
}

impl From<GraphTypeEntityQualifier> for EntityQualifierModel {
    fn from(q: GraphTypeEntityQualifier) -> Self {
        match q {
            GraphTypeEntityQualifier::Node => Self::Node,
            GraphTypeEntityQualifier::Rel => Self::Rel,
        }
    }
}

impl From<EntityQualifierModel> for GraphTypeEntityQualifier {
    fn from(m: EntityQualifierModel) -> Self {
        match m {
            EntityQualifierModel::Node => Self::Node,
            EntityQualifierModel::Rel => Self::Rel,
        }
    }
}

/// Persisted form of [`GraphTypeElementRef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ElementRefModel {
    Any,
    AliasRef(String),
    InSitu {
        alias: Option<String>,
        label: String,
        implies: bool,
    },
}

impl From<&GraphTypeElementRef> for ElementRefModel {
    fn from(r: &GraphTypeElementRef) -> Self {
        match r {
            GraphTypeElementRef::Any => Self::Any,
            GraphTypeElementRef::AliasRef(a) => Self::AliasRef(a.clone()),
            GraphTypeElementRef::InSitu {
                alias,
                label,
                implies,
            } => Self::InSitu {
                alias: alias.clone(),
                label: label.clone(),
                implies: *implies,
            },
        }
    }
}

impl From<&ElementRefModel> for GraphTypeElementRef {
    fn from(m: &ElementRefModel) -> Self {
        match m {
            ElementRefModel::Any => Self::Any,
            ElementRefModel::AliasRef(a) => Self::AliasRef(a.clone()),
            ElementRefModel::InSitu {
                alias,
                label,
                implies,
            } => Self::InSitu {
                alias: alias.clone(),
                label: label.clone(),
                implies: *implies,
            },
        }
    }
}

/// Persisted form of [`TypeExpr`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ValueTypeModel {
    Predefined {
        kind: PredefinedTypeModel,
        not_null: bool,
    },
    List {
        element: Box<ValueTypeModel>,
        not_null: bool,
    },
    PropertyValue {
        not_null: bool,
    },
    Union(Vec<ValueTypeModel>),
    Vector {
        coord: Option<VectorCoordModel>,
        dimension: Option<i64>,
        not_null: bool,
    },
}

impl From<&TypeExpr> for ValueTypeModel {
    fn from(t: &TypeExpr) -> Self {
        match t {
            TypeExpr::Predefined { kind, not_null } => Self::Predefined {
                kind: PredefinedTypeModel::from(*kind),
                not_null: *not_null,
            },
            TypeExpr::List { element, not_null } => Self::List {
                element: Box::new(ValueTypeModel::from(element.as_ref())),
                not_null: *not_null,
            },
            TypeExpr::PropertyValue { not_null } => Self::PropertyValue {
                not_null: *not_null,
            },
            TypeExpr::Union(parts) => Self::Union(parts.iter().map(ValueTypeModel::from).collect()),
            TypeExpr::Vector {
                coord,
                dimension,
                not_null,
            } => Self::Vector {
                coord: coord.map(VectorCoordModel::from),
                dimension: *dimension,
                not_null: *not_null,
            },
        }
    }
}

impl From<&ValueTypeModel> for TypeExpr {
    fn from(m: &ValueTypeModel) -> Self {
        match m {
            ValueTypeModel::Predefined { kind, not_null } => Self::Predefined {
                kind: PredefinedKind::from(*kind),
                not_null: *not_null,
            },
            ValueTypeModel::List { element, not_null } => Self::List {
                element: Box::new(TypeExpr::from(element.as_ref())),
                not_null: *not_null,
            },
            ValueTypeModel::PropertyValue { not_null } => Self::PropertyValue {
                not_null: *not_null,
            },
            ValueTypeModel::Union(parts) => Self::Union(parts.iter().map(TypeExpr::from).collect()),
            ValueTypeModel::Vector {
                coord,
                dimension,
                not_null,
            } => Self::Vector {
                coord: coord.map(VectorCoordKind::from),
                dimension: *dimension,
                not_null: *not_null,
            },
        }
    }
}

/// Persisted form of [`PredefinedKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredefinedTypeModel {
    Nothing,
    Null,
    Any,
    Boolean,
    String,
    Integer,
    Float,
    Date,
    Duration,
    Point,
    LocalTime,
    ZonedTime,
    LocalDateTime,
    ZonedDateTime,
    Node,
    Relationship,
    Path,
    Map,
    /// CIP-246 UUID, carried since CIP-262: the property-default showcase
    /// is a UUID property defaulting to `uuid()`, and the model's
    /// historical narrowing to STRING broke the SHOW/DROP roundtrip for
    /// it (a stored `UUID` re-rendered as `STRING` never matched a DROP's
    /// restated `UUID`). Old persisted images deserialize their
    /// `String`s unchanged.
    Uuid,
}

impl From<PredefinedKind> for PredefinedTypeModel {
    fn from(k: PredefinedKind) -> Self {
        match k {
            PredefinedKind::Nothing => Self::Nothing,
            PredefinedKind::Null => Self::Null,
            PredefinedKind::Any => Self::Any,
            PredefinedKind::Boolean => Self::Boolean,
            PredefinedKind::String => Self::String,
            PredefinedKind::Integer => Self::Integer,
            PredefinedKind::Float => Self::Float,
            PredefinedKind::Date => Self::Date,
            PredefinedKind::Duration => Self::Duration,
            PredefinedKind::Point => Self::Point,
            PredefinedKind::LocalTime => Self::LocalTime,
            PredefinedKind::ZonedTime => Self::ZonedTime,
            PredefinedKind::LocalDateTime => Self::LocalDateTime,
            PredefinedKind::ZonedDateTime => Self::ZonedDateTime,
            PredefinedKind::Node => Self::Node,
            PredefinedKind::Relationship => Self::Relationship,
            PredefinedKind::Path => Self::Path,
            PredefinedKind::Map => Self::Map,
            PredefinedKind::Uuid => Self::Uuid,
        }
    }
}

impl From<PredefinedTypeModel> for PredefinedKind {
    fn from(m: PredefinedTypeModel) -> Self {
        match m {
            PredefinedTypeModel::Nothing => Self::Nothing,
            PredefinedTypeModel::Null => Self::Null,
            PredefinedTypeModel::Any => Self::Any,
            PredefinedTypeModel::Boolean => Self::Boolean,
            PredefinedTypeModel::String => Self::String,
            PredefinedTypeModel::Integer => Self::Integer,
            PredefinedTypeModel::Float => Self::Float,
            PredefinedTypeModel::Date => Self::Date,
            PredefinedTypeModel::Duration => Self::Duration,
            PredefinedTypeModel::Point => Self::Point,
            PredefinedTypeModel::LocalTime => Self::LocalTime,
            PredefinedTypeModel::ZonedTime => Self::ZonedTime,
            PredefinedTypeModel::LocalDateTime => Self::LocalDateTime,
            PredefinedTypeModel::ZonedDateTime => Self::ZonedDateTime,
            PredefinedTypeModel::Node => Self::Node,
            PredefinedTypeModel::Relationship => Self::Relationship,
            PredefinedTypeModel::Path => Self::Path,
            PredefinedTypeModel::Map => Self::Map,
            PredefinedTypeModel::Uuid => Self::Uuid,
        }
    }
}

/// Persisted form of [`VectorCoordKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VectorCoordModel {
    Int8,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
}

impl From<VectorCoordKind> for VectorCoordModel {
    fn from(k: VectorCoordKind) -> Self {
        match k {
            VectorCoordKind::Int8 => Self::Int8,
            VectorCoordKind::Int16 => Self::Int16,
            VectorCoordKind::Int32 => Self::Int32,
            VectorCoordKind::Int64 => Self::Int64,
            VectorCoordKind::Float32 => Self::Float32,
            VectorCoordKind::Float64 => Self::Float64,
        }
    }
}

impl From<VectorCoordModel> for VectorCoordKind {
    fn from(m: VectorCoordModel) -> Self {
        match m {
            VectorCoordModel::Int8 => Self::Int8,
            VectorCoordModel::Int16 => Self::Int16,
            VectorCoordModel::Int32 => Self::Int32,
            VectorCoordModel::Int64 => Self::Int64,
            VectorCoordModel::Float32 => Self::Float32,
            VectorCoordModel::Float64 => Self::Float64,
        }
    }
}

/// Persisted form of [`GraphTypeConstraintSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphTypeConstraintModel {
    pub name: Option<String>,
    pub target: ElementRefModel,
    pub is_edge: bool,
    #[serde(default)]
    pub properties: Vec<String>,
    pub kind: ConstraintVerbModel,
    pub property_type: Option<ValueTypeModel>,
    #[serde(default)]
    pub options: Vec<(String, OptionValueModel)>,
}

impl From<&GraphTypeConstraintSpec> for GraphTypeConstraintModel {
    fn from(s: &GraphTypeConstraintSpec) -> Self {
        Self {
            name: s.name.clone(),
            target: ElementRefModel::from(&s.target),
            is_edge: s.is_edge,
            properties: s.properties.clone(),
            kind: ConstraintVerbModel::from(&s.kind),
            property_type: s.property_type.as_ref().map(ValueTypeModel::from),
            options: s
                .options
                .iter()
                .map(|(k, v)| (k.clone(), OptionValueModel::from(v)))
                .collect(),
        }
    }
}

impl From<&GraphTypeConstraintModel> for GraphTypeConstraintSpec {
    fn from(m: &GraphTypeConstraintModel) -> Self {
        Self {
            name: m.name.clone(),
            target: GraphTypeElementRef::from(&m.target),
            is_edge: m.is_edge,
            properties: m.properties.clone(),
            kind: GraphTypeConstraintVerb::from(m.kind),
            property_type: m.property_type.as_ref().map(TypeExpr::from),
            options: m
                .options
                .iter()
                .map(|(k, v)| (k.clone(), Expr::from(v)))
                .collect(),
        }
    }
}

/// Persisted form of [`GraphTypeConstraintVerb`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConstraintVerbModel {
    Key,
    Unique,
    Exists,
    PropertyType,
}

impl From<&GraphTypeConstraintVerb> for ConstraintVerbModel {
    fn from(v: &GraphTypeConstraintVerb) -> Self {
        match v {
            GraphTypeConstraintVerb::Key => Self::Key,
            GraphTypeConstraintVerb::Unique => Self::Unique,
            GraphTypeConstraintVerb::Exists => Self::Exists,
            GraphTypeConstraintVerb::PropertyType => Self::PropertyType,
        }
    }
}

impl From<ConstraintVerbModel> for GraphTypeConstraintVerb {
    fn from(m: ConstraintVerbModel) -> Self {
        match m {
            ConstraintVerbModel::Key => Self::Key,
            ConstraintVerbModel::Unique => Self::Unique,
            ConstraintVerbModel::Exists => Self::Exists,
            ConstraintVerbModel::PropertyType => Self::PropertyType,
        }
    }
}

/// A graph-type constraint `OPTIONS` value. The canonical renderer
/// (`canonical_option_value`) maps the five literal kinds to text and
/// every other expression to `?`; the model mirrors exactly that
/// distinction, so [`GraphTypeSpec::to_body`] reproduces the same
/// canonical rendering as the parsed source body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OptionValueModel {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    /// A non-literal OPTIONS expression: its canonical rendering is `?`,
    /// byte-identical to what the source expression canonicalized to.
    NonLiteral,
}

impl From<&Expr> for OptionValueModel {
    fn from(e: &Expr) -> Self {
        match e {
            Expr::Literal(LiteralValue::Null) => Self::Null,
            Expr::Literal(LiteralValue::Bool(b)) => Self::Bool(*b),
            Expr::Literal(LiteralValue::Integer(i)) => Self::Integer(*i),
            Expr::Literal(LiteralValue::Float(f)) => Self::Float(*f),
            Expr::Literal(LiteralValue::String(s)) => Self::String(s.clone()),
            _ => Self::NonLiteral,
        }
    }
}

impl From<&OptionValueModel> for Expr {
    fn from(v: &OptionValueModel) -> Self {
        match v {
            OptionValueModel::Null => Expr::Literal(LiteralValue::Null),
            OptionValueModel::Bool(b) => Expr::Literal(LiteralValue::Bool(*b)),
            OptionValueModel::Integer(i) => Expr::Literal(LiteralValue::Integer(*i)),
            OptionValueModel::Float(f) => Expr::Literal(LiteralValue::Float(*f)),
            OptionValueModel::String(s) => Expr::Literal(LiteralValue::String(s.clone())),
            // Any non-literal expression renders as `?` in
            // `canonical_option_value`, just like the source expression.
            OptionValueModel::NonLiteral => Expr::Variable(String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_parser::parse;

    fn body_of(query: &str) -> GraphTypeBody {
        let stmt = parse(query).expect("should parse");
        match stmt.clauses.into_iter().next().unwrap() {
            cypherlite_parser::Clause::CreateGraphType(cgt) => cgt.body,
            cypherlite_parser::Clause::AlterCurrentGraphType(agt) => match agt.target {
                cypherlite_parser::AlterCurrentGraphTypeTarget::Body(body) => body,
                cypherlite_parser::AlterCurrentGraphTypeTarget::Named(name) => {
                    panic!("expected an inline body, got named type {name}")
                }
            },
            other => panic!("expected a graph-type clause, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_the_cip178_explanatory_example() {
        let body = body_of(
            "CREATE GRAPH TYPE gt {
                (s:Student => :Person { name :: STRING NOT NULL, birthday :: DATE, studId :: INTEGER IS KEY })
                REQUIRE (s.name, s.birthday) IS UNIQUE OPTIONS {indexProvider: 'range-1.0'},
                (:City => :Location { name :: STRING IS KEY }),
                (:Site => :Location { name :: STRING }),
                (s)-[:LIVES_IN =>]->(:City =>),
                (s)-[:VISITED =>]->(:Location),
                CONSTRAINT FOR ()-[x:LegacyRel]->() REQUIRE x.foo IS UNIQUE,
                CONSTRAINT mySiteConstraint FOR (st:Site =>) REQUIRE st.name IS KEY,
                CONSTRAINT FOR (p:Person) REQUIRE p.age :: INTEGER
            }",
        );
        assert!(validate_graph_type_body(&body, cypherlite_core::PreviewFeatures::none()).is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_identifying_label() {
        let body = body_of(
            "CREATE GRAPH TYPE gt {
                (p:Person => {name :: STRING}),
                (q:Person => {age :: INTEGER})
            }",
        );
        assert!(validate_graph_type_body(&body, cypherlite_core::PreviewFeatures::none()).is_err());
    }

    #[test]
    fn validate_rejects_ineffectual_edge_type() {
        let body = body_of("CREATE GRAPH TYPE gt { ()-[l:RELATED =>]->() }");
        assert!(validate_graph_type_body(&body, cypherlite_core::PreviewFeatures::none()).is_err());
    }

    #[test]
    fn validate_accepts_ineffectual_looking_edge_with_a_labelled_source() {
        let body = body_of(
            "CREATE GRAPH TYPE gt {
                (p:Person => {name :: STRING}),
                (p)-[l:RELATED =>]->()
            }",
        );
        assert!(validate_graph_type_body(&body, cypherlite_core::PreviewFeatures::none()).is_ok());
    }

    #[test]
    fn derive_constraints_produces_property_type_and_endpoint_constraints() {
        let body = body_of(
            "ALTER CURRENT GRAPH TYPE SET {
                (p :Person => {name :: STRING, age :: INTEGER}),
                (c :City => {name :: STRING}),
                (p)-[:LIVES_IN => {since :: DATE}]->(c)
            }",
        );
        let derived = derive_constraints(&body).expect("validated body derives");
        assert_eq!(derived.len(), 6);
        assert!(derived
            .iter()
            .any(|c| c.constraint_type == "sourceLabel"
                && c.enforced_label.as_deref() == Some("Person")));
        assert!(derived
            .iter()
            .any(|c| c.constraint_type == "targetLabel"
                && c.enforced_label.as_deref() == Some("City")));
        assert!(derived.iter().any(|c| c.constraint_type == "propertyType"
            && c.label == "LIVES_IN"
            && c.properties == vec!["since".to_string()]));
    }

    #[test]
    fn canonicalize_empty_body_is_empty_string() {
        let body = GraphTypeBody::default();
        assert_eq!(
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes"),
            ""
        );
    }

    #[test]
    fn canonicalize_simple_node_type() {
        let body = body_of("CREATE GRAPH TYPE gt AS { (:City => :Location { name :: STRING }) }");
        assert_eq!(
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes"),
            "(:`City` => :`Location` {`name` :: STRING})"
        );
    }

    #[test]
    fn canonicalize_int_and_bool_normalize() {
        let body = body_of("CREATE GRAPH TYPE gt AS { (:A => :Zebra&:Mango { p :: BOOL }) }");
        assert_eq!(
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes"),
            "(:`A` => :`Mango`&`Zebra` {`p` :: BOOLEAN})"
        );
    }

    #[test]
    fn canonicalize_orders_node_types_by_identifying_label() {
        let body = body_of(
            "CREATE GRAPH TYPE gt AS {
                (:Student => :Person { name :: STRING }),
                (:City => :Location { name :: STRING })
            }",
        );
        assert_eq!(
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes"),
            "(:`City` => :`Location` {`name` :: STRING}), (:`Student` => :`Person` {`name` :: STRING})"
        );
    }

    #[test]
    fn canonicalize_unnamed_unidentified_key_constraint() {
        let body = body_of("CREATE GRAPH TYPE gt AS { CONSTRAINT FOR (p:Foo) REQUIRE p.a IS KEY }");
        assert_eq!(
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes"),
            "CONSTRAINT FOR (`n`:`Foo`) REQUIRE (`n`.`a`) IS KEY"
        );
    }

    #[test]
    fn canonicalize_identified_key_constraint_from_inline_rewrite() {
        let body =
            body_of("CREATE GRAPH TYPE gt AS { (:City => :Location { name :: STRING IS KEY }) }");
        assert_eq!(
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes"),
            "(:`City` => :`Location` {`name` :: STRING}), CONSTRAINT FOR (`n`:`City` =>) REQUIRE (`n`.`name`) IS KEY"
        );
    }

    #[test]
    fn canonicalize_empty_options_map_is_omitted() {
        let body = body_of(
            "CREATE GRAPH TYPE gt AS { CONSTRAINT FOR (p:Foo) REQUIRE p.a IS KEY OPTIONS {} }",
        );
        assert_eq!(
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes"),
            "CONSTRAINT FOR (`n`:`Foo`) REQUIRE (`n`.`a`) IS KEY"
        );
    }

    #[test]
    fn canonicalize_rank_orders_key_unique_notnull_type() {
        let body = body_of(
            "CREATE GRAPH TYPE gt AS {
                CONSTRAINT FOR (p:Foo) REQUIRE p.a :: INT,
                CONSTRAINT FOR (p:Foo) REQUIRE p.a IS NOT NULL,
                CONSTRAINT FOR (p:Foo) REQUIRE p.a IS UNIQUE,
                CONSTRAINT FOR (p:Foo) REQUIRE p.a IS KEY
            }",
        );
        assert_eq!(
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes"),
            "CONSTRAINT FOR (`n`:`Foo`) REQUIRE (`n`.`a`) IS KEY, CONSTRAINT FOR (`n`:`Foo`) REQUIRE (`n`.`a`) IS UNIQUE, CONSTRAINT FOR (`n`:`Foo`) REQUIRE (`n`.`a`) IS NOT NULL, CONSTRAINT FOR (`n`:`Foo`) REQUIRE (`n`.`a`) IS :: INTEGER"
        );
    }

    #[test]
    fn graph_type_spec_roundtrips_through_the_parser_model() {
        let body = body_of(
            "CREATE GRAPH TYPE gt {
                (s:Student => :Person { name :: STRING NOT NULL, birthday :: DATE, studId :: INTEGER IS KEY })
                REQUIRE (s.name, s.birthday) IS UNIQUE OPTIONS {indexProvider: 'range-1.0'},
                (:City => :Location { name :: STRING IS KEY }),
                (:Site => :Location { name :: STRING }),
                (s)-[:LIVES_IN => {since :: DATE}]->(:City =>),
                (s)-[:VISITED =>]->(:Location),
                ()-[:Legacy]->(),
                CONSTRAINT FOR ()-[x:LegacyRel]->() REQUIRE x.foo IS UNIQUE,
                CONSTRAINT mySiteConstraint FOR (st:Site =>) REQUIRE st.name IS KEY,
                CONSTRAINT FOR (p:Person) REQUIRE p.age :: INTEGER,
                CONSTRAINT FOR (p:Person) REQUIRE p.nick IS NOT NULL OPTIONS {a: 1, b: 2.5, c: true, d: null}
            }",
        );
        let spec = GraphTypeSpec::from_body(&body).expect("shape-valid body");
        assert_eq!(spec.to_body(), body);
        let json = serde_json::to_string(&spec).expect("spec serializes");
        let decoded: GraphTypeSpec = serde_json::from_str(&json).expect("spec deserializes");
        assert_eq!(decoded, spec);
        assert_eq!(decoded.to_body(), body);
    }

    #[test]
    fn graph_type_spec_roundtrips_alter_drop_bodies() {
        let body = body_of(
            "ALTER CURRENT GRAPH TYPE DROP {
                (:City), ()-[:LIVES_IN]->(), CONSTRAINT mySiteConstraint
            }",
        );
        let spec = GraphTypeSpec::from_body(&body).expect("shape-valid body");
        assert_eq!(spec.to_body(), body);
        let json = serde_json::to_string(&spec).expect("spec serializes");
        let decoded: GraphTypeSpec = serde_json::from_str(&json).expect("spec deserializes");
        assert_eq!(decoded.to_body(), body);
    }

    #[test]
    fn graph_type_spec_keeps_nonliteral_options_rendering() {
        let body = body_of(
            "CREATE GRAPH TYPE gt AS {
                CONSTRAINT FOR (p:Foo) REQUIRE p.a IS KEY OPTIONS {x: 1 + 1}
            }",
        );
        let spec = GraphTypeSpec::from_body(&body).expect("shape-valid body");
        // The ASTs differ (a non-literal expression becomes the NonLiteral
        // marker), but the canonical rendering is byte-identical.
        assert_eq!(
            canonicalize_graph_type_body(&spec.to_body()).expect("a validated body canonicalizes"),
            canonicalize_graph_type_body(&body).expect("a validated body canonicalizes")
        );
    }
}
