//! Core values, errors, and identifiers for CypherLite.

mod cancel;
pub mod ce_messages;
mod convert;
mod error;
pub mod file_identity;
mod memory_limit;
mod namespace;
mod preview;
pub mod temporal;
pub mod tokens;
mod value;

pub use cancel::CancelToken;
pub use convert::{column_type_name, cypher_to_value, value_to_cypher};
pub use error::{uniqueness_violation_detail, ArithmeticFailure, Error, Result};
pub use memory_limit::parse_memory_limit;
pub use namespace::{
    is_graph_selector_function_name, node_element_id, parse_node_element_id, parse_rel_element_id,
    rel_element_id, ElementIdRef, Namespace, GRAPH_SELECTOR_FUNCTIONS,
};
pub use preview::{
    preview_feature_disabled_text, PreviewFeature, PreviewFeatures, PREVIEW_FEATURES,
};
pub use temporal::{QueryClockGuard, Temporal};
pub use tokens::{ExecutionReceipt, TokenEstimate, TokenOutcome};
pub use value::{
    logical_bytes_within, str_logical_bytes, tagged_str, uuid_hyphenated, uuid_parse, Node, Path,
    PointValue, PropertyMap, Relationship, TaggedStr, Value, VectorCoordKind,
};

pub type NodeId = i64;
pub type RelId = i64;

/// The single acceptance rule for a persisted local node or relationship ID.
///
/// The bound `0..i64::MAX` rejects a negative ID before it can seed a
/// next-write counter and reserves `i64::MAX` as the unsupported sentinel,
/// because installing a row advances the ID allocator. `what` labels the
/// caller's context in the returned storage error. See ADR-0096.
pub fn validate_persisted_id(id: i64, what: &str) -> Result<()> {
    if id < 0 || id == i64::MAX {
        return Err(Error::storage(format!(
            "{what} ID {id} is outside 0..i64::MAX"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_id_predicate_accepts_in_range_and_rejects_zero_nine_six() {
        for id in [0, 1, 42, i64::MAX - 1] {
            validate_persisted_id(id, "node").unwrap();
        }
        for id in [-1, i64::MIN, i64::MAX] {
            let error = validate_persisted_id(id, "node").unwrap_err();
            assert!(matches!(error, Error::Storage(_)), "{error}");
            assert_eq!(
                error.to_string(),
                format!("storage error: node ID {id} is outside 0..i64::MAX")
            );
        }
    }
}
