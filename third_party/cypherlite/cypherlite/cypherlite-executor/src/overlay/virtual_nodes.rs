//! Virtual-node side table owned by the graph layer.
//!
//! The CIP-257 graph-type visualization (`graph_type_visualize`) and the
//! `db.schema.visualization` row producer synthesize nodes/relationships
//! that were never inserted into the store; every synthesized entity gets a
//! **negative** id (real, store-backed ids start at 0 and count up), so a
//! negative id unambiguously marks "no real backing".
//! `startNode()`/`endNode()` (`eval::function`) still need to resolve a
//! virtual relationship's endpoints, so the producers register each
//! synthesized node here and [`super::ExecGraph::node_value`] consults this
//! table as a fallback once the primary store/overlay lookup misses. The
//! table is thread-local: statement execution is single-threaded per query
//! worker, and ids are re-registered (overwritten) on every visualization
//! call.
//!
//! Only the value-materialization read path ([`super::ExecGraph::node_value`])
//! consults the fallback. `ExecGraph::node`/`node_exists`/… deliberately do
//! not: CIP-257's "Write behavior on virtual entities" relies on a write
//! against a virtual id never finding its target in the store.

use std::cell::RefCell;
use std::collections::HashMap;

use cypherlite_model::CypherValue;

thread_local! {
    static VIRTUAL_NODES: RefCell<HashMap<i64, CypherValue>> = RefCell::new(HashMap::new());
}

/// Registers a virtual node's full value under its id, for later
/// `startNode()`/`endNode()` resolution (see module doc comment). Called by
/// the visualization producers (`graph_type_visualize`,
/// `exec::procedure_rows::schema::visualization`) as they synthesize rows.
pub(crate) fn register_virtual_node(id: i64, value: CypherValue) {
    VIRTUAL_NODES.with(|cell| {
        cell.borrow_mut().insert(id, value);
    });
}

/// Looks up a previously-registered virtual node by id -- the fallback
/// [`super::ExecGraph::node_value`] uses once a primary store/overlay
/// lookup misses.
pub(crate) fn lookup_virtual_node(id: i64) -> Option<CypherValue> {
    VIRTUAL_NODES.with(|cell| cell.borrow().get(&id).cloned())
}

#[cfg(test)]
mod tests {
    use super::register_virtual_node;
    use crate::graph::Graph;
    use crate::overlay::ExecGraph;
    use cypherlite_core::{NodeId, Value};
    use cypherlite_model::CypherValue;
    use indexmap::IndexMap;

    #[test]
    fn virtual_node_temporal_and_map_list_properties_survive_conversion() {
        let date = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let mut attrs = IndexMap::new();
        attrs.insert("nested".to_string(), CypherValue::Int(7));
        let mut properties = IndexMap::new();
        properties.insert("created".to_string(), CypherValue::Date(date));
        properties.insert("attrs".to_string(), CypherValue::Map(attrs));
        properties.insert(
            "tags".to_string(),
            CypherValue::List(vec![CypherValue::Str("a".into()), CypherValue::Int(2)]),
        );
        let id: NodeId = -1;
        register_virtual_node(
            id,
            CypherValue::Node {
                id,
                labels: vec!["V".to_string()],
                properties,
            },
        );

        let g = Graph::default();
        let graph = ExecGraph::read(&g);
        let Value::Node(node) = graph.node_value(id) else {
            panic!("expected virtual node, got non-node")
        };
        assert!(matches!(
            node.properties.get("created"),
            Some(Value::Temporal(cypherlite_core::temporal::Temporal::Date(d))) if *d == date
        ));
        assert!(
            matches!(node.properties.get("attrs"), Some(Value::Map(m)) if m.get("nested") == Some(&Value::Int(7)))
        );
        match node.properties.get("tags") {
            Some(Value::List(items)) => {
                assert_eq!(items[0], Value::String("a".into()));
                assert_eq!(items[1], Value::Int(2));
            }
            other => panic!("expected list, got {other:?}"),
        }
    }
}
