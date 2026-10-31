//! ADR-0226 §2: the ontology → graph-type mapping behind
//! `db.ontology.apply`, re-based on main's document shapes by ADR-0364
//! §5.
//!
//! [`to_graph_type_body`] compiles a validated [`OntologyDocument`] into a
//! CIP-176 [`GraphTypeBody`] — the same parser model the
//! `ALTER CURRENT GRAPH TYPE SET` path consumes, so `db.ontology.apply`
//! can feed it to `apply_verb` / `derive_constraints_typed` and persist it
//! with `GraphTypeSpec::from_body` (both in `cypherlite-executor`, the
//! only layer that can see the data; ADR-0226 §3).
//!
//! The split follows the spec's enforcement table. **Enforceable** —
//! compiled into the body:
//!
//! - Node entries: one node type spec per entry with the identifying
//!   label (`labels.identifier`, the one form — validation desugars the
//!   `label` shorthand and leaves a null identifier as main allows), the
//!   local id as the element alias (endpoint and constraint references
//!   need it), implied labels from `labels.implied`, and the typed
//!   property declarations.
//! - Property `type` tokens map through [`Neo4jType::mapping`], the one
//!   exhaustive classification of main's closed 39-token enum: `ANY`,
//!   `LIST<ANY>` and an absent `type` declare nothing; the predefined
//!   scalars and temporal kinds map to the CIP-176 kind of the same name;
//!   `UUID` maps to CIP-246 `UUID`; `LIST<X>` becomes `LIST<X NOT NULL>`;
//!   the six `VECTOR<…>` tokens map to their coordinate kind with the
//!   property's `dimension`. `FLOAT32`, `INTEGER8`, `INTEGER16`,
//!   `INTEGER32` and their list forms have no plain CIP-176 counterpart
//!   and fail loudly — never a silent widening or a declare-nothing.
//! - `must_exist: true` → `NOT NULL` on the declaration, or a standalone
//!   `REQUIRE <el>.<p> IS NOT NULL` graph-type constraint when the
//!   property declares no type. `unique: true` / `key: true` →
//!   single-property unique / key constraints; `key` subsumes `unique`.
//! - Constraint objects → graph-type constraints of the same kind over the
//!   listed properties — composite when the list has more than one entry —
//!   keeping the object's `name` when present. A constraint object that
//!   restates a shorthand flag exactly (same kind, same single property)
//!   replaces the flag-derived constraint, so the object is what survives
//!   — with its name. `PROPERTY_TYPE` objects fail loudly: the body cannot
//!   express a property-type assertion and dropping it would under-enforce.
//! - Relationship entries → one edge type spec per entry, endpoints
//!   resolved through the referenced node entries' aliases. Entries
//!   sharing a `type` stay distinct specs (the ADR's documented caveat:
//!   graph-type enforcement keys on the type alone).
//!
//! **Descriptive** fields are never compiled: display `name`s,
//! `description`, `aliases`, `reference`, `labels.optional`, endpoint
//! labels and counts, `one_of`, `pattern`, every extension payload
//! (`indexes`, `display`, `tables`, `mappings` and `custom`), and the
//! document metadata (`schema`, `id`, `version`,
//! `name`, `description`). They remain available verbatim through
//! `db.ontology.get` — the stored document is the sole source of truth,
//! and no graph is derived from it (ADR-0229 §3).
//!
//! Main-valid constructs the graph-type body cannot express fail loudly
//! here rather than disappearing (ADR-0364 §5.4): a node with no
//! identifying label, a relationship target with no `node`, an endpoint
//! naming a missing `nodes` key, a token with no plain CIP-176
//! counterpart, a `PROPERTY_TYPE` constraint, an empty constraint
//! property list, a constraint naming a property its owner does not
//! declare, an empty property name that would be declared or constrained,
//! a node-constraint `label` outside the node's labels, and a non-positive
//! vector dimension are all [`OntologyError::Mapping`] errors naming the
//! owner and construct.

use std::collections::HashSet;

use cypherlite_parser::{
    GraphTypeBody, GraphTypeConstraintSpec, GraphTypeConstraintVerb, GraphTypeEdgeTypeSpec,
    GraphTypeElementRef, GraphTypeNodeTypeSpec, GraphTypePropertyType, PredefinedKind, TypeExpr,
    VectorCoordKind,
};

use crate::model::{
    Constraint, ConstraintType, Neo4jType, NodeType, OntologyDocument, Property,
    RelationshipTarget, RelationshipType,
};
use crate::OntologyError;

/// What one main `Neo4jType` token compiles to in a CIP-176 property type.
///
/// The match producing it is exhaustive over [`Neo4jType`]: a token added
/// to main's closed enum must be classified here before the crate
/// compiles, so no token can be silently skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenMapping {
    /// `ANY` and `LIST<ANY>`: the document says "unconstrained", so no
    /// declaration is compiled (ADR-0228). An absent `type` is handled
    /// separately and means the same.
    DeclaresNothing,
    /// A predefined CIP-176 leaf of the same name.
    Predefined(PredefinedKind),
    /// `LIST<X>`: CIP-176 property types are closed, so the element is
    /// always `NOT NULL`.
    List(PredefinedKind),
    /// `VECTOR<X>`: the coordinate kind, with the property's `dimension`.
    Vector(VectorCoordKind),
    /// A token with no plain CIP-176 counterpart. The converter fails
    /// loudly for it (ADR-0364 §5.2): widening would silently change the
    /// declared constraint, and declaring nothing would assert less than
    /// the document.
    Unsupported,
}

impl Neo4jType {
    /// The ONE token classification, read everywhere the mapping decides
    /// between a declaration, "declares nothing", and a loud failure.
    fn mapping(self) -> TokenMapping {
        use Neo4jType as T;
        match self {
            T::Any | T::ListAny => TokenMapping::DeclaresNothing,
            T::Boolean => TokenMapping::Predefined(PredefinedKind::Boolean),
            T::ListBoolean => TokenMapping::List(PredefinedKind::Boolean),
            T::Date => TokenMapping::Predefined(PredefinedKind::Date),
            T::ListDate => TokenMapping::List(PredefinedKind::Date),
            T::Duration => TokenMapping::Predefined(PredefinedKind::Duration),
            T::ListDuration => TokenMapping::List(PredefinedKind::Duration),
            T::Float32 | T::ListFloat32 => TokenMapping::Unsupported,
            T::Float => TokenMapping::Predefined(PredefinedKind::Float),
            T::ListFloat => TokenMapping::List(PredefinedKind::Float),
            T::Integer8
            | T::ListInteger8
            | T::Integer16
            | T::ListInteger16
            | T::Integer32
            | T::ListInteger32 => TokenMapping::Unsupported,
            T::Integer => TokenMapping::Predefined(PredefinedKind::Integer),
            T::ListInteger => TokenMapping::List(PredefinedKind::Integer),
            T::LocalDatetime => TokenMapping::Predefined(PredefinedKind::LocalDateTime),
            T::ListLocalDatetime => TokenMapping::List(PredefinedKind::LocalDateTime),
            T::LocalTime => TokenMapping::Predefined(PredefinedKind::LocalTime),
            T::ListLocalTime => TokenMapping::List(PredefinedKind::LocalTime),
            T::Point => TokenMapping::Predefined(PredefinedKind::Point),
            T::ListPoint => TokenMapping::List(PredefinedKind::Point),
            T::String => TokenMapping::Predefined(PredefinedKind::String),
            T::ListString => TokenMapping::List(PredefinedKind::String),
            T::VectorFloat => TokenMapping::Vector(VectorCoordKind::Float64),
            T::VectorFloat32 => TokenMapping::Vector(VectorCoordKind::Float32),
            T::VectorInteger => TokenMapping::Vector(VectorCoordKind::Int64),
            T::VectorInteger32 => TokenMapping::Vector(VectorCoordKind::Int32),
            T::VectorInteger16 => TokenMapping::Vector(VectorCoordKind::Int16),
            T::VectorInteger8 => TokenMapping::Vector(VectorCoordKind::Int8),
            T::ZonedDatetime => TokenMapping::Predefined(PredefinedKind::ZonedDateTime),
            T::ListZonedDatetime => TokenMapping::List(PredefinedKind::ZonedDateTime),
            T::ZonedTime => TokenMapping::Predefined(PredefinedKind::ZonedTime),
            T::ListZonedTime => TokenMapping::List(PredefinedKind::ZonedTime),
            T::Uuid => TokenMapping::Predefined(PredefinedKind::Uuid),
        }
    }

    /// Whether this token declares no CIP-176 property type — such a
    /// property gets no declaration, and its `must_exist: true` becomes a
    /// standalone existence constraint. `Unsupported` is not "nothing":
    /// its failure is raised before this classification is consulted.
    fn declares_nothing(self) -> bool {
        self.mapping() == TokenMapping::DeclaresNothing
    }
}

/// The entry that owns the properties and constraints being compiled, for
/// named [`OntologyError::Mapping`] diagnostics.
///
/// The variant is the owner kind — the one encoding of node-vs-relationship
/// — and carries the entry plus the identifying label a constraint targets
/// (`FOR (:Label =>)`).
enum Owner<'a> {
    /// A node entry and its identifying label.
    Node { entry: &'a NodeType, label: &'a str },
    /// A relationship entry and its type, the label its constraints target.
    Relationship {
        entry: &'a RelationshipType,
        label: &'a str,
    },
}

impl<'a> Owner<'a> {
    /// `node 'N'` / `relationship 'R'`, the prefix of every mapping error.
    fn describe(&self) -> String {
        match self {
            Self::Node { entry, .. } => format!("node '{}'", entry.key),
            Self::Relationship { entry, .. } => format!("relationship '{}'", entry.key),
        }
    }

    fn mapping(&self, detail: impl std::fmt::Display) -> OntologyError {
        OntologyError::Mapping(format!("{} {detail}", self.describe()))
    }

    /// This owner's properties, in document order.
    fn properties(&self) -> &'a [Property] {
        match self {
            Self::Node { entry, .. } => &entry.properties,
            Self::Relationship { entry, .. } => &entry.properties,
        }
    }

    /// This owner's constraint objects, in document order.
    fn constraints(&self) -> &'a [Constraint] {
        match self {
            Self::Node { entry, .. } => &entry.constraints,
            Self::Relationship { entry, .. } => &entry.constraints,
        }
    }

    /// The identifying label a constraint targets (`FOR (:Label =>)`).
    fn label(&self) -> &'a str {
        match self {
            Self::Node { label, .. } | Self::Relationship { label, .. } => label,
        }
    }

    /// Whether the owner compiles to an edge type spec.
    fn is_edge(&self) -> bool {
        match self {
            Self::Node { .. } => false,
            Self::Relationship { .. } => true,
        }
    }

    /// Whether `label` is one of a node's labels (identifying or implied);
    /// relationship constraints have no label member.
    fn knows_label(&self, label: &str) -> bool {
        match self {
            Self::Node { entry, .. } => {
                entry.labels.identifier.as_deref() == Some(label)
                    || entry.labels.implied.iter().any(|implied| implied == label)
            }
            Self::Relationship { .. } => false,
        }
    }
}

/// Compile a validated ontology document into the CIP-176 graph-type body
/// `db.ontology.apply` sets on the database (ADR-0226 §2, ADR-0364 §5).
///
/// # Errors
///
/// [`OntologyError::Mapping`] when the document is main-valid but the
/// graph-type body cannot express it: a node entry with no identifying
/// label, a relationship target with no `node`, an endpoint naming no
/// `nodes` entry, a property token with no plain CIP-176 counterpart, a
/// `PROPERTY_TYPE` constraint, an empty constraint property list, a
/// constraint property the owner does not declare, an empty property name
/// that would be declared or constrained, a node-constraint label outside
/// the node's labels, or a non-positive vector dimension (ADR-0364 §5.4).
pub fn to_graph_type_body(doc: &OntologyDocument) -> Result<GraphTypeBody, OntologyError> {
    // Node type specs, and the set of node keys an endpoint can resolve
    // to.
    let mut node_types = Vec::with_capacity(doc.nodes().len());
    let mut constraints = Vec::new();
    let mut node_keys: HashSet<&str> = HashSet::with_capacity(doc.nodes().len());
    for entry in doc.nodes() {
        node_keys.insert(entry.key.as_str());
        let label = entry.identifying_label().ok_or_else(|| {
            OntologyError::Mapping(format!("node '{}' has no identifying label", entry.key))
        })?;
        let owner = Owner::Node { entry, label };
        node_types.push(GraphTypeNodeTypeSpec {
            alias: Some(entry.key.clone()),
            identifying_label: label.to_owned(),
            implied_labels: entry.labels.implied.clone(),
            properties: property_decls(&owner)?,
        });
        constraints.extend(owner_constraints(&owner)?);
    }

    let mut edge_types = Vec::with_capacity(doc.relationships().len());
    for entry in doc.relationships() {
        let from = endpoint(&entry.from, "from", &entry.key, &node_keys)?;
        let to = endpoint(&entry.to, "to", &entry.key, &node_keys)?;
        let owner = Owner::Relationship {
            entry,
            label: &entry.rel_type,
        };
        edge_types.push(GraphTypeEdgeTypeSpec {
            alias: Some(entry.key.clone()),
            identifying_label: entry.rel_type.clone(),
            properties: property_decls(&owner)?,
            source: GraphTypeElementRef::AliasRef(from.to_owned()),
            target: GraphTypeElementRef::AliasRef(to.to_owned()),
        });
        constraints.extend(owner_constraints(&owner)?);
    }

    Ok(GraphTypeBody {
        node_types,
        edge_types,
        constraints,
        drop_constraint_names: Vec::new(),
    })
}

/// One resolved relationship endpoint: the node key its target names, after
/// checking that the target names a node at all and that the key is one of
/// the document's `nodes` entries. `side` is the target being resolved
/// (`from` / `to`), named in the diagnostic.
fn endpoint<'a>(
    target: &'a RelationshipTarget,
    side: &str,
    relationship: &str,
    node_keys: &HashSet<&str>,
) -> Result<&'a str, OntologyError> {
    let node = target.node.as_deref().ok_or_else(|| {
        OntologyError::Mapping(format!(
            "relationship '{relationship}' target '{side}' has no node"
        ))
    })?;
    if !node_keys.contains(node) {
        return Err(OntologyError::Mapping(format!(
            "relationship '{relationship}' endpoint '{side}' references unknown node key \
             '{node}'"
        )));
    }
    Ok(node)
}

/// An owner's typed property declarations, in document order. A property
/// that declares nothing contributes no declaration (and may still
/// contribute an existence constraint in [`owner_constraints`]).
fn property_decls(owner: &Owner<'_>) -> Result<Vec<GraphTypePropertyType>, OntologyError> {
    let mut declarations = Vec::with_capacity(owner.properties().len());
    for property in owner.properties() {
        if let Some(declaration) = property_decl(owner, property)? {
            declarations.push(declaration);
        }
    }
    Ok(declarations)
}

/// A property's type declaration: the CIP-176 value type of the declared
/// token, `NOT NULL` when `must_exist: true`. Returns `None` exactly when
/// the property declares nothing — no token at all, or `ANY`/`LIST<ANY>`.
///
/// # Errors
///
/// [`OntologyError::Mapping`] for an empty-named property that declares a
/// type, a token with no plain CIP-176 counterpart, or a non-positive
/// vector dimension (ADR-0364 §5.4).
fn property_decl(
    owner: &Owner<'_>,
    property: &Property,
) -> Result<Option<GraphTypePropertyType>, OntologyError> {
    let not_null = property.must_exist == Some(true);
    let Some(ty) = property.property_type else {
        return Ok(None);
    };
    if property.id.is_empty() {
        return Err(owner.mapping(format!(
            "property '' declares type '{}': a graph-type property name is non-empty",
            ty.as_str()
        )));
    }
    let value_type = match ty.mapping() {
        TokenMapping::DeclaresNothing => return Ok(None),
        TokenMapping::Unsupported => {
            return Err(owner.mapping(format!(
                "property '{}' has type '{}', which has no CIP-176 property type",
                property.id,
                ty.as_str()
            )));
        }
        TokenMapping::Predefined(kind) => TypeExpr::Predefined { kind, not_null },
        TokenMapping::List(kind) => TypeExpr::List {
            // CIP-176 property types are closed: a list's inner type is
            // mandatory NOT NULL, so the spec's `LIST<X>` maps to
            // `LIST<X NOT NULL>`.
            element: Box::new(TypeExpr::Predefined {
                kind,
                not_null: true,
            }),
            not_null,
        },
        TokenMapping::Vector(coord) => {
            if let Some(dimension) = property.dimension {
                if dimension <= 0 {
                    return Err(owner.mapping(format!(
                        "property '{}' has vector dimension {dimension}: a graph-type vector \
                         needs a positive dimension",
                        property.id
                    )));
                }
            }
            TypeExpr::Vector {
                coord: Some(coord),
                dimension: property.dimension,
                not_null,
            }
        }
    };
    Ok(Some(GraphTypePropertyType {
        name: property.id.clone(),
        value_type,
        default_value: None,
        inline_constraint: None,
    }))
}

/// One owner's standalone graph-type constraints, in document order: the
/// shorthand flags of its properties first (in property order), then its
/// constraint objects. A constraint object restating a flag exactly
/// suppresses the flag-derived constraint, so the named object survives.
///
/// # Errors
///
/// [`OntologyError::Mapping`] for an empty-named property asserted by a
/// shorthand flag, an empty constraint property list, a `PROPERTY_TYPE`
/// constraint, a listed property the owner does not declare, or a node
/// constraint `label` outside the node's labels (ADR-0364 §5.3/§5.4).
fn owner_constraints(owner: &Owner<'_>) -> Result<Vec<GraphTypeConstraintSpec>, OntologyError> {
    let mut out = Vec::new();
    for property in owner.properties() {
        if property.id.is_empty() {
            if property.must_exist == Some(true)
                || property.unique == Some(true)
                || property.key == Some(true)
            {
                return Err(owner.mapping(
                    "property '' is asserted by a shorthand flag: a graph-type property name \
                     is non-empty",
                ));
            }
            continue;
        }
        // `key` subsumes `unique` (it implies unique + must_exist per the
        // spec).
        let flag = if property.key == Some(true) {
            Some(GraphTypeConstraintVerb::Key)
        } else if property.unique == Some(true) {
            Some(GraphTypeConstraintVerb::Unique)
        } else {
            None
        };
        if let Some(verb) = flag {
            if !restated_by_object(&verb, &property.id, owner.constraints()) {
                out.push(constraint_spec(
                    None,
                    owner,
                    vec![property.id.clone()],
                    verb,
                ));
            }
        }
        // `must_exist` on a typed property is the declaration's
        // `NOT NULL` (`property_decl`), so only a property that declares
        // nothing gets a standalone existence constraint. The shorthand
        // gets the same object-replaces-flag rule as key/unique.
        if property
            .property_type
            .is_none_or(Neo4jType::declares_nothing)
            && property.must_exist == Some(true)
            && !restated_by_object(
                &GraphTypeConstraintVerb::Exists,
                &property.id,
                owner.constraints(),
            )
        {
            out.push(constraint_spec(
                None,
                owner,
                vec![property.id.clone()],
                GraphTypeConstraintVerb::Exists,
            ));
        }
    }
    for object in owner.constraints() {
        if object.properties.is_empty() {
            return Err(owner.mapping(format!(
                "constraint '{}' lists no properties: a graph-type constraint needs at least one",
                object.id
            )));
        }
        for property in &object.properties {
            if property.is_empty() {
                return Err(owner.mapping(format!(
                    "constraint '{}' lists the empty property name: a graph-type property name \
                     is non-empty",
                    object.id
                )));
            }
            if !owner.properties().iter().any(|p| p.id == *property) {
                return Err(owner.mapping(format!(
                    "constraint '{}' references property '{}', which {} does not declare",
                    object.id,
                    property,
                    owner.describe()
                )));
            }
        }
        if let Some(label) = object.label.as_deref() {
            if !owner.knows_label(label) {
                return Err(owner.mapping(format!(
                    "constraint '{}' label '{}' is not one of the node's labels",
                    object.id, label
                )));
            }
        }
        let Some(verb) = object.constraint_type.graph_type_verb() else {
            return Err(owner.mapping(format!(
                "constraint '{}' is of type PROPERTY_TYPE: the graph-type body cannot \
                 express a property-type constraint object",
                object.id
            )));
        };
        out.push(constraint_spec(
            object.name.clone(),
            owner,
            object.properties.clone(),
            verb,
        ));
    }
    Ok(out)
}

/// The ONE correspondence between main's constraint kind and the CIP-176
/// graph-type constraint verb, read by [`ConstraintType::graph_type_verb`].
/// `PROPERTY_TYPE` is deliberately absent: the body cannot express a
/// property-type assertion, so [`ConstraintType::graph_type_verb`] returns
/// `None` and the converter fails loudly.
const CONSTRAINT_VERBS: &[(ConstraintType, GraphTypeConstraintVerb)] = &[
    (ConstraintType::Exists, GraphTypeConstraintVerb::Exists),
    (ConstraintType::Key, GraphTypeConstraintVerb::Key),
    (ConstraintType::Unique, GraphTypeConstraintVerb::Unique),
];

impl ConstraintType {
    /// The graph-type verb of this constraint kind, read from
    /// [`CONSTRAINT_VERBS`]; `None` for `PROPERTY_TYPE`, which the
    /// graph-type body cannot express.
    fn graph_type_verb(self) -> Option<GraphTypeConstraintVerb> {
        CONSTRAINT_VERBS
            .iter()
            .find(|(kind, _)| *kind == self)
            .map(|(_, verb)| verb.clone())
    }
}

/// Whether a constraint object restates a single-property shorthand flag
/// exactly (same kind, same property) — the object-replaces-flag rule for
/// all three shorthands (`key`, `unique`, `must_exist`). No flag maps to
/// `PROPERTY_TYPE`, so it never restates one.
fn restated_by_object(
    verb: &GraphTypeConstraintVerb,
    property: &str,
    objects: &[Constraint],
) -> bool {
    objects.iter().any(|o| {
        o.constraint_type
            .graph_type_verb()
            .is_some_and(|v| &v == verb)
            && o.properties == [property]
    })
}

/// One standalone graph-type constraint, in the target shape the parser's
/// own inline-constraint rewrite produces (`FOR (:Label =>)`).
fn constraint_spec(
    name: Option<String>,
    owner: &Owner<'_>,
    properties: Vec<String>,
    kind: GraphTypeConstraintVerb,
) -> GraphTypeConstraintSpec {
    GraphTypeConstraintSpec {
        name,
        target: in_situ(owner.label()),
        is_edge: owner.is_edge(),
        properties,
        kind,
        property_type: None,
        options: Vec::new(),
    }
}

/// An in-situ element reference in the `(:Label =>)` shape — the ONE
/// construction of it, shared by the converter and the test modules.
fn in_situ(label: &str) -> GraphTypeElementRef {
    GraphTypeElementRef::InSitu {
        alias: None,
        label: label.to_owned(),
        implies: true,
    }
}

#[cfg(test)]
mod tests;
