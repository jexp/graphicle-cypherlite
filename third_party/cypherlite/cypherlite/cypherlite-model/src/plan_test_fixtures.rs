//! The one hand-built plan wire fixture (LABS-448's review round): the
//! shape the engine emits, every key spelled through the `plan_keys`
//! vocabulary — one builder for the sibling test modules (the wire
//! accessor's and the agent document's), never a second copy of the
//! metric-args block.
#![allow(dead_code)]

use crate::wire::plan_keys;
use crate::{CypherValue, IndexMap};

/// A hand-built plan wire node: the operator, its id, PROFILE metric args
/// when `time_ns` carries one (the read kinds along with them, as the
/// engine emits), and the children composed in.
pub(crate) fn plan_wire_node(
    operator: &str,
    id: i64,
    time_ns: Option<i64>,
    children: Vec<CypherValue>,
) -> CypherValue {
    let mut args = IndexMap::new();
    args.insert(plan_keys::ID.to_string(), CypherValue::Int(id));
    args.insert(plan_keys::DETAILS.to_string(), CypherValue::Str("x".into()));
    if let Some(time_ns) = time_ns {
        args.insert(plan_keys::ROWS.to_string(), CypherValue::Int(1));
        args.insert(plan_keys::TIME_NS.to_string(), CypherValue::Int(time_ns));
        args.insert(plan_keys::DB_HITS.to_string(), CypherValue::Int(0));
        args.insert(plan_keys::MEMORY_BYTES.to_string(), CypherValue::Int(0));
        args.insert(plan_keys::HOPS.to_string(), CypherValue::Int(0));
        args.insert(plan_keys::NEIGHBOR_WALKS.to_string(), CypherValue::Int(0));
        args.insert(plan_keys::NODE_READS.to_string(), CypherValue::Int(0));
        args.insert(plan_keys::SEEK_TIME_NS.to_string(), CypherValue::Int(0));
        args.insert(plan_keys::WALK_TIME_NS.to_string(), CypherValue::Int(0));
    }
    let mut map = IndexMap::new();
    map.insert(
        plan_keys::OPERATOR_TYPE.to_string(),
        CypherValue::Str(operator.into()),
    );
    map.insert(plan_keys::ARGS.to_string(), CypherValue::Map(args));
    map.insert(
        plan_keys::IDENTIFIERS.to_string(),
        CypherValue::List(vec![CypherValue::Str("name".into())]),
    );
    if !children.is_empty() {
        map.insert(plan_keys::CHILDREN.to_string(), CypherValue::List(children));
    }
    CypherValue::Map(map)
}
