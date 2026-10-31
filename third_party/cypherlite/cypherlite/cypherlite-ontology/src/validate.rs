//! Hand-rolled structural validation against the vendored main schema
//! (ADR-0364 §2): `schema/ontology-graph-spec.schema.json`
//! (`neo4j/graph-spec` @ `066515593ae59317d758bcf2cef54195e74bde39`).
//!
//! The checks below mirror that schema exactly: the required root fields
//! (`schema`, `version`, `id`) with `version` as a string; `name`/`description`
//! as strings; `additionalProperties: false` on every object main closes;
//! nullable display `name`s; id-keyed constraint maps with the uppercase
//! `type` enum; the closed 39-token [`Neo4jType`] enum; `must_exist` (the
//! pinned draft's `mustExist` and the property `aliases` member are
//! rejected as unknown); nullable flags, dimensions and endpoint counts
//! with no minimums; `one_of` items as strings only; and the closed
//! per-element extension envelopes, whose typed payloads are validated
//! deeply in [`ext`] over the retained root value tree (ADR-0364 §2).
//! `format: uri` /
//! `format: regex` are annotations and are not checked, as JSON Schema
//! prescribes.
//!
//! Main-valid constructs stay accepted even when the ADR-0226 converter
//! cannot express them: a missing identifier label, a label-only or empty
//! relationship target, an empty property name and an empty constraint
//! `properties` list are all validation-valid and fail later, at `apply`,
//! with [`OntologyError::Mapping`] (ADR-0364 §3). Two decisions are made
//! here, at the validation boundary: a node entry's `label` shorthand is
//! desugared into `labels.identifier` (a conflicting pair is rejected as
//! ambiguous, per ADR-0364 §3), and an endpoint's `count` may not be
//! combined with `min_count`/`max_count`.

mod ext;
mod read;

use indexmap::IndexMap;

use crate::error::{OntologyError, PINNED_SCHEMA_URI};
use crate::model::{
    Constraint, ConstraintType, Labels, NodeType, OntologyDocument, Property, RelationshipTarget,
    RelationshipType, CONSTRAINTS,
};
use crate::value::Value;

use read::{
    check_allowed, expect_object, join, kind_of, opt_list, opt_neo4j_type, opt_nullable_bool,
    opt_nullable_int, opt_nullable_string, opt_string, opt_string_list, opt_sub, req_string,
    req_string_list, req_sub, string_list, verr,
};

/// Allowed root keys (`additionalProperties: false` at the root).
const ROOT_KEYS: &[&str] = &[
    "schema",
    "id",
    "version",
    "name",
    "description",
    "nodes",
    "relationships",
    "extensions",
];

/// Allowed keys of a node entry.
const NODE_KEYS: &[&str] = &[
    "label",
    "labels",
    "properties",
    "constraints",
    "extensions",
    "name",
    "description",
    "aliases",
    "reference",
];

/// Allowed keys of a `labels` object.
const LABELS_KEYS: &[&str] = &["identifier", "implied", "optional", "extensions"];

/// Allowed keys of a property object. Main removed `aliases` and renamed
/// `mustExist` to `must_exist`; both old spellings fail as unknown members.
const PROPERTY_KEYS: &[&str] = &[
    "type",
    "dimension",
    "must_exist",
    "unique",
    "key",
    "extensions",
    "name",
    "description",
    "reference",
    "pattern",
    "one_of",
];

/// Allowed keys of a relationship entry.
const RELATIONSHIP_KEYS: &[&str] = &[
    "type",
    "from",
    "to",
    "properties",
    "constraints",
    "extensions",
    "name",
    "description",
    "aliases",
    "reference",
];

/// Allowed keys of a main `RelationshipTarget`.
const TARGET_KEYS: &[&str] = &["node", "label", "count", "min_count", "max_count"];

/// Allowed keys of a node constraint object.
const NODE_CONSTRAINT_KEYS: &[&str] = &["type", "label", "properties", "extensions", "name"];

/// Allowed keys of a relationship constraint object (no `label` in main).
const RELATIONSHIP_CONSTRAINT_KEYS: &[&str] = &["type", "properties", "extensions", "name"];

/// Validate a parsed value model into a typed [`OntologyDocument`].
///
/// The spec-version gate (ADR-0364 §1) runs before structural validation,
/// so a newer-version document fails with the version error rather than
/// incidental structural errors. A non-string `schema` is not a URI at all
/// and falls through to the structural type error on that field.
pub(crate) fn validate_document(root: Value) -> Result<OntologyDocument, OntologyError> {
    let obj = expect_object(&root, "/", "the document root")?;

    // ADR-0364 §1: the exact main URI is the only accepted gate value,
    // checked before structural validation.
    if let Some(Value::String(uri)) = obj.get("schema") {
        if uri != PINNED_SCHEMA_URI {
            return Err(OntologyError::UnsupportedSpecVersion { uri: uri.clone() });
        }
    }

    check_allowed(obj, ROOT_KEYS, "/")?;
    let schema = req_string(obj, "schema", "/")?;
    let version = req_string(obj, "version", "/")?;
    let id = req_string(obj, "id", "/")?;
    let name = opt_string(obj, "name", "/")?;
    let description = opt_string(obj, "description", "/")?;
    let nodes = opt_sub(obj, "nodes", "/", validate_node_entries)?;
    let relationships = opt_sub(obj, "relationships", "/", validate_relationship_entries)?;
    opt_sub(obj, "extensions", "/", ext::validate_document_extensions)?;

    Ok(OntologyDocument::new(
        root,
        schema,
        id,
        version,
        name,
        description,
        nodes,
        relationships,
    ))
}

fn validate_node_entries(value: &Value, path: &str) -> Result<Vec<NodeType>, OntologyError> {
    let obj = expect_object(value, path, "'nodes'")?;
    let mut out = Vec::with_capacity(obj.len());
    for (key, entry) in obj {
        let path = &join(path, key);
        let obj = expect_object(entry, path, "a node entry")?;
        check_allowed(obj, NODE_KEYS, path)?;

        let shorthand = opt_nullable_string(obj, "label", path)?;
        let labels = opt_sub(obj, "labels", path, |v, p| validate_labels(v, p).map(Some))?;
        let labels = desugar_labels(shorthand, labels, path)?;

        let name = opt_nullable_string(obj, "name", path)?;
        let properties = opt_sub(obj, "properties", path, validate_properties)?;
        let constraints = opt_sub(obj, "constraints", path, |v, p| {
            validate_constraints(v, p, ConstraintOwner::Node)
        })?;
        let aliases = opt_string_list(obj, "aliases", path)?;
        let reference = opt_string(obj, "reference", path)?;
        let description = opt_string(obj, "description", path)?;
        opt_sub(obj, "extensions", path, ext::validate_node_extensions)?;

        out.push(NodeType {
            key: key.clone(),
            labels,
            name,
            properties,
            constraints,
            aliases,
            reference,
            description,
        });
    }
    Ok(out)
}

/// Desugar main's `label` shorthand into `labels.identifier` (ADR-0364
/// §3). Both forms together must agree; a lone form is used as-is; neither
/// leaves the identifier null, which main allows and `apply` rejects.
fn desugar_labels(
    shorthand: Option<String>,
    labels: Option<Labels>,
    path: &str,
) -> Result<Labels, OntologyError> {
    let mut labels = labels.unwrap_or_default();
    let Some(shorthand) = shorthand else {
        return Ok(labels);
    };
    match &labels.identifier {
        Some(identifier) if *identifier != shorthand => Err(verr(
            path,
            format!(
                "a node entry's 'label' ('{shorthand}') and 'labels.identifier' \
                 ('{identifier}') must be equal"
            ),
        )),
        Some(_) => Ok(labels),
        None => {
            labels.identifier = Some(shorthand);
            Ok(labels)
        }
    }
}

fn validate_labels(value: &Value, path: &str) -> Result<Labels, OntologyError> {
    let obj = expect_object(value, path, "'labels'")?;
    check_allowed(obj, LABELS_KEYS, path)?;
    let identifier = opt_nullable_string(obj, "identifier", path)?;
    let implied = opt_string_list(obj, "implied", path)?;
    let optional = opt_string_list(obj, "optional", path)?;
    opt_sub(obj, "extensions", path, validate_custom_extensions)?;
    Ok(Labels {
        identifier,
        implied,
        optional,
    })
}

fn validate_relationship_entries(
    value: &Value,
    path: &str,
) -> Result<Vec<RelationshipType>, OntologyError> {
    let obj = expect_object(value, path, "'relationships'")?;
    let mut out = Vec::with_capacity(obj.len());
    for (key, entry) in obj {
        let path = &join(path, key);
        let obj = expect_object(entry, path, "a relationship entry")?;
        check_allowed(obj, RELATIONSHIP_KEYS, path)?;
        let rel_type = req_string(obj, "type", path)?;
        let from = req_sub(obj, "from", path, validate_relationship_target)?;
        let to = req_sub(obj, "to", path, validate_relationship_target)?;
        let name = opt_nullable_string(obj, "name", path)?;
        let properties = opt_sub(obj, "properties", path, validate_properties)?;
        let constraints = opt_sub(obj, "constraints", path, |v, p| {
            validate_constraints(v, p, ConstraintOwner::Relationship)
        })?;
        let aliases = opt_string_list(obj, "aliases", path)?;
        let reference = opt_string(obj, "reference", path)?;
        let description = opt_string(obj, "description", path)?;
        opt_sub(
            obj,
            "extensions",
            path,
            ext::validate_relationship_extensions,
        )?;
        out.push(RelationshipType {
            key: key.clone(),
            rel_type,
            from,
            to,
            name,
            properties,
            constraints,
            aliases,
            reference,
            description,
        });
    }
    Ok(out)
}

fn validate_relationship_target(
    value: &Value,
    path: &str,
) -> Result<RelationshipTarget, OntologyError> {
    let obj = expect_object(value, path, "a relationship target")?;
    check_allowed(obj, TARGET_KEYS, path)?;
    let node = opt_string(obj, "node", path)?;
    let label = opt_string(obj, "label", path)?;
    let count = opt_nullable_int(obj, "count", path)?;
    let min_count = opt_nullable_int(obj, "min_count", path)?;
    let max_count = opt_nullable_int(obj, "max_count", path)?;
    // ADR-0364 §3: the model represents one cardinality form; combining
    // the exact count with a range bound would silently drop one side.
    if count.is_some() && (min_count.is_some() || max_count.is_some()) {
        return Err(verr(
            path,
            "'count' cannot be combined with 'min_count' or 'max_count'",
        ));
    }
    Ok(RelationshipTarget {
        node,
        label,
        count,
        min_count,
        max_count,
    })
}

fn validate_properties(value: &Value, path: &str) -> Result<Vec<Property>, OntologyError> {
    let obj = expect_object(value, path, "'properties'")?;
    let mut out = Vec::with_capacity(obj.len());
    for (id, entry) in obj {
        let path = &join(path, id);
        let obj = expect_object(entry, path, "a property")?;
        check_allowed(obj, PROPERTY_KEYS, path)?;
        let property_type = opt_neo4j_type(obj, "type", path)?;
        let dimension = opt_nullable_int(obj, "dimension", path)?;
        let must_exist = opt_nullable_bool(obj, "must_exist", path)?;
        let unique = opt_nullable_bool(obj, "unique", path)?;
        let key = opt_nullable_bool(obj, "key", path)?;
        let one_of = opt_list(obj, "one_of", path, string_list)?;
        let pattern = opt_string(obj, "pattern", path)?;
        let name = opt_nullable_string(obj, "name", path)?;
        let reference = opt_string(obj, "reference", path)?;
        let description = opt_string(obj, "description", path)?;
        opt_sub(obj, "extensions", path, validate_custom_extensions)?;
        out.push(Property {
            id: id.clone(),
            property_type,
            dimension,
            must_exist,
            unique,
            key,
            one_of,
            pattern,
            name,
            reference,
            description,
        });
    }
    Ok(out)
}

/// Which closed constraint object shape an entry's `constraints` map uses
/// (main's `NodeConstraint` vs `RelationshipConstraint`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConstraintOwner {
    Node,
    Relationship,
}

/// The expected-token list for constraint `type` fields, derived from the
/// canonical [`CONSTRAINTS`] table [`ConstraintType::parse`] consults.
fn expected_constraint_types() -> String {
    CONSTRAINTS
        .iter()
        .map(|(token, _)| format!("'{token}'"))
        .collect::<Vec<_>>()
        .join(" or ")
}

fn validate_constraints(
    value: &Value,
    path: &str,
    owner: ConstraintOwner,
) -> Result<Vec<Constraint>, OntologyError> {
    let obj = expect_object(value, path, "'constraints'")?;
    let keys = match owner {
        ConstraintOwner::Node => NODE_CONSTRAINT_KEYS,
        ConstraintOwner::Relationship => RELATIONSHIP_CONSTRAINT_KEYS,
    };
    let mut out = Vec::with_capacity(obj.len());
    for (id, entry) in obj {
        let path = &join(path, id);
        let obj = expect_object(entry, path, "a constraint")?;
        check_allowed(obj, keys, path)?;
        let token = req_string(obj, "type", path)?;
        let constraint_type = ConstraintType::parse(&token).ok_or_else(|| {
            verr(
                &join(path, "type"),
                format!(
                    "invalid constraint type '{token}': expected {}",
                    expected_constraint_types()
                ),
            )
        })?;
        let properties = req_string_list(obj, "properties", path)?;
        let label = opt_nullable_string(obj, "label", path)?;
        let name = opt_nullable_string(obj, "name", path)?;
        opt_sub(obj, "extensions", path, validate_custom_extensions)?;
        out.push(Constraint {
            id: id.clone(),
            constraint_type,
            properties,
            label,
            name,
        });
    }
    Ok(out)
}

/// A `custom`-object-only extensions envelope: `custom`, when present,
/// must be an object.
fn validate_custom_extensions(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "'extensions'")?;
    check_allowed(obj, &["custom"], path)?;
    validate_custom_object(obj, path)
}

/// A present `custom` member must be an object; its members are free-form.
fn validate_custom_object(obj: &IndexMap<String, Value>, path: &str) -> Result<(), OntologyError> {
    match obj.get("custom") {
        None | Some(Value::Object(_)) => Ok(()),
        Some(v) => Err(verr(
            &join(path, "custom"),
            format!("'custom' must be an object, found {}", kind_of(v)),
        )),
    }
}

#[cfg(test)]
mod tests;
