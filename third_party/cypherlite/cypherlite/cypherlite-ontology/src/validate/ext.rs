//! Deep validation of the main schema's typed extension payloads
//! (ADR-0364 §2), in place over the retained root [`Value`] tree.
//!
//! Main defines node and relationship indexes, the index-type tokens and
//! their tagged `options` union, node display, tables with columns and
//! foreign-key references, and the `NODE`/`QUERY`/`RELATIONSHIP` mapping
//! union with property and target mappings. The checks here mirror those
//! definitions — closed members, required fields, enum and scalar types —
//! so a malformed payload that main rejects does not pass `set`. `custom`
//! members are the one exception: main defines them as arbitrary JSON
//! objects, so only the object shape is checked.
//!
//! Nothing validated here is ever converted (ADR-0364 §5.1), so there is
//! no typed extension tree: the document's root value tree is the one
//! carrier, and these checks only prove the payloads main-shaped.

use indexmap::IndexMap;

use super::read::{
    check_allowed, expect_object, join, kind_of, number_list, opt_bool, opt_int, opt_list,
    opt_neo4j_type, opt_neo4j_type_list, opt_nullable_int, opt_nullable_number,
    opt_nullable_string, opt_nullable_sub, opt_string, opt_string_list, opt_sub, req_number,
    req_string, req_string_list, req_sub, verr,
};
use super::validate_custom_object;
use crate::error::OntologyError;
use crate::value::Value;

const NODE_EXTENSION_KEYS: &[&str] = &["indexes", "display", "custom"];
const RELATIONSHIP_EXTENSION_KEYS: &[&str] = &["indexes", "custom"];
const DOCUMENT_EXTENSION_KEYS: &[&str] = &["tables", "mappings", "custom"];

const NODE_INDEX_KEYS: &[&str] = &["type", "labels", "properties", "options", "custom", "name"];
const RELATIONSHIP_INDEX_KEYS: &[&str] = &["type", "properties", "options", "custom", "name"];

const FULLTEXT_KEYS: &[&str] = &[
    "type",
    "fulltext.default_analyzer",
    "fulltext.analyzer",
    "fulltext.eventually_consistent",
    "fulltext.eventually_consistent_apply_parallelism",
    "fulltext.eventually_consistent_refresh_interval",
    "fulltext.eventually_consistent_refresh_parallelism",
    "fulltext.eventually_consistent_index_update_queue_max_length",
];
const POINT_KEYS: &[&str] = &[
    "type",
    "spatial.cartesian.min",
    "spatial.cartesian.max",
    "spatial.cartesian-3d.min",
    "spatial.cartesian-3d.max",
    "spatial.wgs-84.min",
    "spatial.wgs-84.max",
    "spatial.wgs-84-3d.min",
    "spatial.wgs-84-3d.max",
];
const VECTOR_KEYS: &[&str] = &[
    "type",
    "vector.dimensions",
    "vector.similarity_function",
    "vector.default_search_expansion_factor",
    "vector.quantization.enabled",
    "vector.quantization.type",
    "vector.hnsw.m",
    "vector.hnsw.ef_construction",
];

/// The validator a table row pairs with its `options.type` tag: it receives
/// the whole options object (with `type` among its keys) and the options
/// path.
type IndexOptionValidator = fn(&IndexMap<String, Value>, &str) -> Result<(), OntologyError>;

/// The closed `options.type` vocabulary shared by node and relationship
/// indexes, in schema order, each row pairing the tag with the validator
/// that checks its option shape. [`validate_index_option`] looks a
/// submitted tag up here and calls the matching validator;
/// [`expected_index_option_types`] renders the same rows for the error
/// message — the pattern [`CONSTRAINTS`](crate::model::CONSTRAINTS) and
/// `ConstraintType::parse` follow.
pub(super) const INDEX_OPTION_TYPES: &[(&str, IndexOptionValidator)] = &[
    ("FULLTEXT", validate_fulltext_option),
    ("POINT", validate_point_option),
    ("VECTOR", validate_vector_option),
];

const DISPLAY_KEYS: &[&str] = &["x", "y", "colour", "custom"];
const TABLE_KEYS: &[&str] = &[
    "source",
    "columns",
    "primary_keys",
    "foreign_keys",
    "custom",
];
const TABLE_COLUMN_KEYS: &[&str] = &[
    "name",
    "type",
    "size",
    "suggested",
    "supported",
    "dimension",
    "custom",
];
const FOREIGN_KEY_KEYS: &[&str] = &["columns", "references", "custom"];
const FOREIGN_KEY_REFERENCE_KEYS: &[&str] = &["table", "columns", "custom"];

const NODE_MAPPING_KEYS: &[&str] = &[
    "type",
    "node",
    "table",
    "properties",
    "mode",
    "match_label",
    "key",
    "depends_on",
    "id",
];
const QUERY_MAPPING_KEYS: &[&str] = &["type", "table", "query", "depends_on", "id"];
const RELATIONSHIP_MAPPING_KEYS: &[&str] = &[
    "type",
    "relationship",
    "table",
    "from_node",
    "to_node",
    "match_label",
    "properties",
    "mode",
    "key",
    "depends_on",
    "id",
];
const PROPERTY_MAPPING_KEYS: &[&str] = &["column"];
const TARGET_MAPPING_KEYS: &[&str] = &["label", "properties"];

/// One id-keyed entry map: `what` must be an object and every member is
/// validated at `<path>/<id>`. The simple entry maps — node and
/// relationship `indexes`, document `tables`, `foreign_keys` and property
/// mappings — share it instead of open-coding the loop. `entry` receives
/// one member value and that member's path.
fn validate_entry_map(
    value: &Value,
    path: &str,
    what: &str,
    entry: impl Fn(&Value, &str) -> Result<(), OntologyError>,
) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, what)?;
    for (id, entry_value) in obj {
        entry(entry_value, &join(path, id))?;
    }
    Ok(())
}

/// A document's `extensions`: `tables`, `mappings` and a `custom` object.
pub(super) fn validate_document_extensions(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "'extensions'")?;
    check_allowed(obj, DOCUMENT_EXTENSION_KEYS, path)?;
    opt_sub(obj, "tables", path, validate_tables)?;
    opt_sub(obj, "mappings", path, validate_mappings)?;
    validate_custom_object(obj, path)?;
    Ok(())
}

/// A node entry's `extensions`: `indexes`, optional `display` and `custom`.
pub(super) fn validate_node_extensions(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "'extensions'")?;
    check_allowed(obj, NODE_EXTENSION_KEYS, path)?;
    opt_sub(obj, "indexes", path, validate_node_indexes)?;
    opt_nullable_sub(obj, "display", path, validate_node_display)?;
    validate_custom_object(obj, path)?;
    Ok(())
}

/// A relationship entry's `extensions`: `indexes` and `custom`.
pub(super) fn validate_relationship_extensions(
    value: &Value,
    path: &str,
) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "'extensions'")?;
    check_allowed(obj, RELATIONSHIP_EXTENSION_KEYS, path)?;
    opt_sub(obj, "indexes", path, validate_relationship_indexes)?;
    validate_custom_object(obj, path)?;
    Ok(())
}

fn validate_node_indexes(value: &Value, path: &str) -> Result<(), OntologyError> {
    validate_entry_map(value, path, "'indexes'", validate_node_index)
}

fn validate_node_index(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "a node index")?;
    check_allowed(obj, NODE_INDEX_KEYS, path)?;
    validate_index_type(obj, path)?;
    req_string_list(obj, "labels", path)?;
    req_string_list(obj, "properties", path)?;
    opt_nullable_sub(obj, "options", path, validate_index_option)?;
    validate_custom_object(obj, path)?;
    opt_nullable_string(obj, "name", path)?;
    Ok(())
}

fn validate_relationship_indexes(value: &Value, path: &str) -> Result<(), OntologyError> {
    validate_entry_map(value, path, "'indexes'", validate_relationship_index)
}

fn validate_relationship_index(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "a relationship index")?;
    check_allowed(obj, RELATIONSHIP_INDEX_KEYS, path)?;
    validate_index_type(obj, path)?;
    req_string_list(obj, "properties", path)?;
    opt_nullable_sub(obj, "options", path, validate_index_option)?;
    validate_custom_object(obj, path)?;
    opt_nullable_string(obj, "name", path)?;
    Ok(())
}

fn validate_index_type(obj: &IndexMap<String, Value>, path: &str) -> Result<(), OntologyError> {
    let token = req_string(obj, "type", path)?;
    match token.as_str() {
        "FULLTEXT" | "POINT" | "RANGE" | "TEXT" | "VECTOR" | "LOOKUP" => Ok(()),
        _ => Err(verr(
            &join(path, "type"),
            format!(
                "invalid index type '{token}': expected 'FULLTEXT', 'POINT', 'RANGE', \
                 'TEXT', 'VECTOR' or 'LOOKUP'"
            ),
        )),
    }
}

/// The expected-token list for index `options.type` fields, derived from
/// [`INDEX_OPTION_TYPES`] in the message's `'A', 'B' or 'C'` form.
fn expected_index_option_types() -> String {
    let (last, rest) = INDEX_OPTION_TYPES
        .split_last()
        .expect("the index option vocabulary is never empty");
    let head = rest
        .iter()
        .map(|(token, _)| format!("'{token}'"))
        .collect::<Vec<_>>()
        .join(", ");
    if head.is_empty() {
        format!("'{}'", last.0)
    } else {
        format!("{head} or '{}'", last.0)
    }
}

/// A node or relationship index's `options`: an object tagged by
/// `options.type`, whose members are decided by that tag's
/// [`INDEX_OPTION_TYPES`] row.
pub(super) fn validate_index_option(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "'options'")?;
    let tag = req_string(obj, "type", path)?;
    let validate = INDEX_OPTION_TYPES
        .iter()
        .find_map(|(candidate, validate)| (*candidate == tag).then_some(*validate))
        .ok_or_else(|| {
            verr(
                &join(path, "type"),
                format!(
                    "invalid index options type '{tag}': expected {}",
                    expected_index_option_types()
                ),
            )
        })?;
    validate(obj, path)
}

/// `options.type: 'FULLTEXT'` — the `fulltext.*` members (main's
/// `FulltextIndexOptions`).
fn validate_fulltext_option(
    obj: &IndexMap<String, Value>,
    path: &str,
) -> Result<(), OntologyError> {
    check_allowed(obj, FULLTEXT_KEYS, path)?;
    opt_string(obj, "fulltext.default_analyzer", path)?;
    opt_nullable_string(obj, "fulltext.analyzer", path)?;
    opt_bool(obj, "fulltext.eventually_consistent", path)?;
    opt_int(
        obj,
        "fulltext.eventually_consistent_apply_parallelism",
        path,
    )?;
    opt_string(obj, "fulltext.eventually_consistent_refresh_interval", path)?;
    opt_int(
        obj,
        "fulltext.eventually_consistent_refresh_parallelism",
        path,
    )?;
    opt_int(
        obj,
        "fulltext.eventually_consistent_index_update_queue_max_length",
        path,
    )?;
    Ok(())
}

/// `options.type: 'POINT'` — the `spatial.*` members (main's
/// `PointIndexOptions`).
fn validate_point_option(obj: &IndexMap<String, Value>, path: &str) -> Result<(), OntologyError> {
    check_allowed(obj, POINT_KEYS, path)?;
    opt_list(obj, "spatial.cartesian.min", path, number_list)?;
    opt_list(obj, "spatial.cartesian.max", path, number_list)?;
    opt_list(obj, "spatial.cartesian-3d.min", path, number_list)?;
    opt_list(obj, "spatial.cartesian-3d.max", path, number_list)?;
    opt_list(obj, "spatial.wgs-84.min", path, number_list)?;
    opt_list(obj, "spatial.wgs-84.max", path, number_list)?;
    opt_list(obj, "spatial.wgs-84-3d.min", path, number_list)?;
    opt_list(obj, "spatial.wgs-84-3d.max", path, number_list)?;
    Ok(())
}

/// `options.type: 'VECTOR'` — the `vector.*` members (main's
/// `VectorIndexOptions`).
fn validate_vector_option(obj: &IndexMap<String, Value>, path: &str) -> Result<(), OntologyError> {
    check_allowed(obj, VECTOR_KEYS, path)?;
    opt_nullable_int(obj, "vector.dimensions", path)?;
    opt_string(obj, "vector.similarity_function", path)?;
    opt_nullable_number(obj, "vector.default_search_expansion_factor", path)?;
    opt_bool(obj, "vector.quantization.enabled", path)?;
    opt_string(obj, "vector.quantization.type", path)?;
    opt_int(obj, "vector.hnsw.m", path)?;
    opt_int(obj, "vector.hnsw.ef_construction", path)?;
    Ok(())
}

fn validate_node_display(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "'display'")?;
    check_allowed(obj, DISPLAY_KEYS, path)?;
    req_number(obj, "x", path)?;
    req_number(obj, "y", path)?;
    opt_nullable_string(obj, "colour", path)?;
    validate_custom_object(obj, path)?;
    Ok(())
}

fn validate_tables(value: &Value, path: &str) -> Result<(), OntologyError> {
    validate_entry_map(value, path, "'tables'", validate_table)
}

fn validate_table(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "a table")?;
    check_allowed(obj, TABLE_KEYS, path)?;
    req_string(obj, "source", path)?;
    opt_sub(obj, "columns", path, validate_table_columns)?;
    opt_string_list(obj, "primary_keys", path)?;
    opt_sub(obj, "foreign_keys", path, validate_foreign_keys)?;
    validate_custom_object(obj, path)?;
    Ok(())
}

fn validate_table_columns(value: &Value, path: &str) -> Result<(), OntologyError> {
    let items = value.as_array().ok_or_else(|| {
        verr(
            path,
            format!("'columns' must be an array, found {}", kind_of(value)),
        )
    })?;
    for (i, item) in items.iter().enumerate() {
        validate_table_column(item, &format!("{path}/{i}"))?;
    }
    Ok(())
}

fn validate_table_column(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "a table column")?;
    check_allowed(obj, TABLE_COLUMN_KEYS, path)?;
    opt_string(obj, "name", path)?;
    opt_string(obj, "type", path)?;
    opt_int(obj, "size", path)?;
    opt_neo4j_type(obj, "suggested", path)?;
    opt_neo4j_type_list(obj, "supported", path)?;
    opt_nullable_int(obj, "dimension", path)?;
    validate_custom_object(obj, path)?;
    Ok(())
}

fn validate_foreign_keys(value: &Value, path: &str) -> Result<(), OntologyError> {
    validate_entry_map(value, path, "'foreign_keys'", |entry, path| {
        let obj = expect_object(entry, path, "a foreign key")?;
        check_allowed(obj, FOREIGN_KEY_KEYS, path)?;
        req_string_list(obj, "columns", path)?;
        req_sub(obj, "references", path, validate_foreign_key_reference)?;
        validate_custom_object(obj, path)
    })
}

fn validate_foreign_key_reference(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "'references'")?;
    check_allowed(obj, FOREIGN_KEY_REFERENCE_KEYS, path)?;
    req_string(obj, "table", path)?;
    opt_string_list(obj, "columns", path)?;
    validate_custom_object(obj, path)?;
    Ok(())
}

fn validate_mappings(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "'mappings'")?;
    for (name, entry) in obj {
        let path = &join(path, name);
        let items = entry.as_array().ok_or_else(|| {
            verr(
                path,
                format!(
                    "'mappings' entries must be arrays, found {}",
                    kind_of(entry)
                ),
            )
        })?;
        for (i, item) in items.iter().enumerate() {
            validate_mapping(item, &format!("{path}/{i}"))?;
        }
    }
    Ok(())
}

fn validate_mapping(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "a mapping")?;
    match req_string(obj, "type", path)?.as_str() {
        "NODE" => {
            check_allowed(obj, NODE_MAPPING_KEYS, path)?;
            req_string(obj, "node", path)?;
            req_string(obj, "table", path)?;
            req_sub(obj, "properties", path, validate_property_mappings)?;
            validate_mapping_mode(obj, path)?;
            opt_nullable_string(obj, "match_label", path)?;
            opt_string_list(obj, "key", path)?;
            opt_string(obj, "depends_on", path)?;
            opt_string(obj, "id", path)?;
            Ok(())
        }
        "QUERY" => {
            check_allowed(obj, QUERY_MAPPING_KEYS, path)?;
            req_string(obj, "table", path)?;
            req_string(obj, "query", path)?;
            opt_string(obj, "depends_on", path)?;
            opt_string(obj, "id", path)?;
            Ok(())
        }
        "RELATIONSHIP" => {
            check_allowed(obj, RELATIONSHIP_MAPPING_KEYS, path)?;
            req_string(obj, "relationship", path)?;
            req_string(obj, "table", path)?;
            req_sub(obj, "from_node", path, validate_target_mapping)?;
            req_sub(obj, "to_node", path, validate_target_mapping)?;
            opt_nullable_string(obj, "match_label", path)?;
            opt_sub(obj, "properties", path, validate_property_mappings)?;
            validate_mapping_mode(obj, path)?;
            opt_string_list(obj, "key", path)?;
            opt_string(obj, "depends_on", path)?;
            opt_string(obj, "id", path)?;
            Ok(())
        }
        other => Err(verr(
            &join(path, "type"),
            format!("invalid mapping type '{other}': expected 'NODE', 'QUERY' or 'RELATIONSHIP'"),
        )),
    }
}

fn validate_mapping_mode(obj: &IndexMap<String, Value>, path: &str) -> Result<(), OntologyError> {
    let Some(token) = opt_string(obj, "mode", path)? else {
        return Ok(());
    };
    match token.as_str() {
        "MERGE" | "CREATE" => Ok(()),
        _ => Err(verr(
            &join(path, "mode"),
            format!("invalid mapping mode '{token}': expected 'MERGE' or 'CREATE'"),
        )),
    }
}

fn validate_property_mappings(value: &Value, path: &str) -> Result<(), OntologyError> {
    validate_entry_map(value, path, "'properties'", |entry, path| {
        let obj = expect_object(entry, path, "a property mapping")?;
        check_allowed(obj, PROPERTY_MAPPING_KEYS, path)?;
        req_string(obj, "column", path)?;
        Ok(())
    })
}

fn validate_target_mapping(value: &Value, path: &str) -> Result<(), OntologyError> {
    let obj = expect_object(value, path, "a target mapping")?;
    check_allowed(obj, TARGET_MAPPING_KEYS, path)?;
    opt_string(obj, "label", path)?;
    opt_sub(obj, "properties", path, validate_property_mappings)?;
    Ok(())
}
