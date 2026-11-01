//! The machine-native plan form (ADR-0345): the engine `Value` map in
//! Neo4j's structured-plan field shape, so agent and tooling consumers
//! that already read Neo4j plans read this engine's plans unmodified. The
//! Neo4j-facing root decoration (ADR-0357) builds on the same node and
//! args assembly, never on a re-parse of the serialized form.

use std::collections::BTreeMap;

use cypherlite_model::wire;
use cypherlite_model::wire::plan_keys;

use super::{ClauseMetrics, PlanNode, PlanWarning};
use crate::Value;

/// One metric arg, through the wire's one width policy (`metric_i64`):
/// saturate at the i64 boundary, never an unchecked cast — the insert
/// idiom every metric field shares.
fn put_metric(args: &mut BTreeMap<String, Value>, key: &str, value: u128) {
    args.insert(key.to_string(), Value::Int(wire::metric_i64(value)));
}

impl PlanNode {
    /// The Neo4j-facing plan value (ADR-0357) — the one boundary every
    /// surface calls before converting the value to its own wire type, so
    /// the plan metadata cannot drift between Bolt, the HTTP Query API and
    /// the CLI's machine modes. Called on a result's plan **root**: the
    /// decoration is root-only by intent —
    /// `string-representation` renders the whole statement's plan, and
    /// `planner`/`runtime`/`version`/`Memory` are root facts — and the
    /// callers hold that root as `QueryResult::plan_root()`, destructuring
    /// their own `Option` once (they need it anyway for the `is_profiled`
    /// key pick), so the signature never re-states it.
    ///
    /// `plan_text` is the surface's rendering of this plan (ADR-0359:
    /// the boundary renders through `cypherlite-formatter`); the engine
    /// only heads it with the identity block.
    ///
    /// The typed table/speedscope renderers walk the undecorated
    /// [`PlanNode::to_value`] form instead: this decoration's float
    /// `EstimatedRows` is Neo4j-facing and never reaches the engine-form
    /// wire accessor (ADR-0345's strict integer contract).
    pub fn to_neo4j_value(&self, plan_text: &str) -> Value {
        Value::Map(std::sync::Arc::new(
            self.node_map(self.neo4j_args(plan_text)),
        ))
    }

    /// The root's args with the Neo4j-facing additions (ADR-0357), built
    /// from the same fields the base args read — never a re-parse of the
    /// serialized form: `EstimatedRows` mirrors as Neo4j's float,
    /// `Memory` mirrors the row-materialization bytes under Neo4j's arg
    /// name, `string-representation` carries the engine's one rendering of
    /// `statement` (headed by the `Cypher`/`Planner`/`Runtime` lines
    /// tooling parses), and `version` plus the planner/runtime identity
    /// name this engine honestly (never Neo4j's `COST`/`SLOTTED`).
    fn neo4j_args(&self, plan_text: &str) -> BTreeMap<String, Value> {
        let mut args = self.base_args();
        // Neo4j's `EstimatedRows` is a float; the engine's own form keeps
        // the integer — the mirror converts it so float-parsing tooling
        // (cypher-shell's summary) reads it.
        if let Some(estimate) = self.estimated_rows {
            args.insert(
                plan_keys::ESTIMATED_ROWS.to_string(),
                Value::Float(estimate as f64),
            );
        }
        // Neo4j's root carries `Memory`; this engine's root metric is the
        // row-materialization bytes — the same number the `Memory (Bytes)`
        // column renders, mirrored under Neo4j's arg name.
        if let Some(metrics) = self.metrics {
            args.insert(
                plan_keys::MEMORY.to_string(),
                Value::Int(wire::metric_i64(metrics.memory_bytes as u128)),
            );
        }
        let planner = wire::NEO4J_PLANNER_IMPL;
        let version = env!("CARGO_PKG_VERSION");
        // The representation's header block is an engine fact (the honest
        // planner/runtime identity); the plan text under it is the
        // boundary's rendering (ADR-0359), passed in by the surface that
        // rendered it through `cypherlite-formatter`.
        let representation = format!(
            "Cypher {cypher_version}\n\nPlanner {planner}\n\nRuntime {planner}\n\nRuntime \
             version {version}\n\n{plan_text}",
            cypher_version = wire::CYPHER_VERSION,
        );
        args.insert(
            plan_keys::STRING_REPRESENTATION.to_string(),
            Value::String(representation.into()),
        );
        for (key, value) in [
            (plan_keys::VERSION, wire::CYPHER_VERSION.to_string()),
            (plan_keys::PLANNER, planner.to_string()),
            (plan_keys::PLANNER_IMPL, planner.to_string()),
            (plan_keys::PLANNER_VERSION, version.to_string()),
            (plan_keys::RUNTIME, planner.to_string()),
            (plan_keys::RUNTIME_IMPL, planner.to_string()),
            (plan_keys::RUNTIME_VERSION, version.to_string()),
        ] {
            args.insert(key.to_string(), Value::String(value.into()));
        }
        args
    }

    /// The machine-native form (ADR-0345): an engine `Value` map in
    /// Neo4j's structured-plan field shape — `operatorType`, `args` (the
    /// node's id, details, estimate), `identifiers`, `children` — so agent
    /// and tooling consumers that already read Neo4j plans read this
    /// engine's plans unmodified, and the later `PROFILE` phases add their
    /// metrics as further `args` entries. Surfaces convert to their wire
    /// value type (`value_to_cypher`) at their own boundary.
    pub fn to_value(&self) -> Value {
        Value::Map(std::sync::Arc::new(self.node_map(self.base_args())))
    }

    /// The node's base args (ADR-0345): `Id` (the pre-order number), the
    /// `Details` string, the estimate when the estimate phase filled it,
    /// and the PROFILE metrics set when the node carries one — one
    /// construction the undecorated and the Neo4j-facing forms both build
    /// from, so they cannot drift.
    fn base_args(&self) -> BTreeMap<String, Value> {
        let mut args: BTreeMap<String, Value> = [
            // One width policy for every plan integer (wire's
            // `metric_i64`): saturate at the i64 boundary, never an
            // unchecked cast.
            (
                plan_keys::ID.to_string(),
                Value::Int(wire::metric_i64(self.id as u128)),
            ),
            (
                plan_keys::DETAILS.to_string(),
                Value::String(self.details.as_str().into()),
            ),
        ]
        .into_iter()
        .collect();
        if let Some(estimate) = self.estimated_rows {
            args.insert(
                plan_keys::ESTIMATED_ROWS.to_string(),
                Value::Int(wire::metric_i64(estimate as u128)),
            );
        }
        if let Some(tokens) = self.token_estimate {
            args.insert(
                "PeakLogicalBytes".to_string(),
                Value::Int(wire::metric_i64(tokens.peak_logical_bytes as u128)),
            );
            args.insert(
                "TokenSchedule".to_string(),
                Value::String(tokens.token_schedule.into()),
            );
        }
        // PROFILE metrics (ADR-0344 phase 2 + metric fields): present
        // only on PROFILE trees, emitted together — through the one
        // emission helper every metrics feature extends.
        if let Some(metrics) = self.metrics {
            Self::put_metrics(&mut args, &metrics);
        }
        args
    }

    /// The PROFILE metrics' arg emission (ADR-0344 phase 2 + metric
    /// fields, LABS-369's kinds and start, LABS-448's phase times): one
    /// helper every metrics feature extends, so base_args reads as base
    /// fields + estimate + tokens + metrics.
    fn put_metrics(args: &mut BTreeMap<String, Value>, metrics: &ClauseMetrics) {
        // One width policy for every metric field (wire's
        // `metric_i64`): saturate, never an unchecked cast on one
        // field and a clamp on the other.
        put_metric(args, plan_keys::ROWS, metrics.rows as u128);
        put_metric(args, plan_keys::TIME_NS, metrics.time_ns);
        put_metric(args, plan_keys::DB_HITS, metrics.db_hits as u128);
        // LABS-369: the read-kind mix behind the one `DbHits` total,
        // and the walk's chosen start (element, direction, reason) —
        // the facts an opaque `Expand` hid. Emitted together with the
        // metric block, so a PROFILE tree carries them or none.
        args.insert(
            plan_keys::HOPS.to_string(),
            Value::Int(wire::metric_i64(metrics.reads.hops as u128)),
        );
        args.insert(
            plan_keys::NEIGHBOR_WALKS.to_string(),
            Value::Int(wire::metric_i64(metrics.reads.neighbor_walks as u128)),
        );
        args.insert(
            plan_keys::NODE_READS.to_string(),
            Value::Int(wire::metric_i64(metrics.reads.node_reads as u128)),
        );
        // LABS-448: the phase times the read kinds' store work spent —
        // the seek (candidate resolution and admission) and the walk
        // (relationship traversal), inclusive of a multi-element
        // pattern's nested phases. Emitted with the metric block like
        // the kinds.
        put_metric(args, plan_keys::SEEK_TIME_NS, metrics.phases.seek_ns);
        put_metric(args, plan_keys::WALK_TIME_NS, metrics.phases.walk_ns);
        if let Some(start) = metrics.start {
            put_metric(args, plan_keys::START_ELEMENT, start.element as u128);
            args.insert(
                plan_keys::START_BACKWARD.to_string(),
                Value::Bool(start.backward),
            );
            args.insert(
                plan_keys::START_REASON.to_string(),
                Value::String(start.reason.as_str().into()),
            );
        }
        put_metric(args, plan_keys::MEMORY_BYTES, metrics.memory_bytes as u128);
    }

    /// The node's map (ADR-0345) around `args`: `operatorType`, `args`,
    /// `identifiers`, the children as undecorated [`PlanNode::to_value`]
    /// subtrees (the decoration is root-only, ADR-0357) and the typed
    /// warnings. `to_value` and `to_neo4j_value` share this assembly, so
    /// the two forms cannot drift.
    fn node_map(&self, args: BTreeMap<String, Value>) -> BTreeMap<String, Value> {
        let mut node: BTreeMap<String, Value> = [
            (
                plan_keys::OPERATOR_TYPE.to_string(),
                Value::String(self.operator.as_str().into()),
            ),
            (
                plan_keys::ARGS.to_string(),
                Value::Map(std::sync::Arc::new(args)),
            ),
            (
                plan_keys::IDENTIFIERS.to_string(),
                Value::List(
                    self.identifiers
                        .iter()
                        .map(|name| Value::String(name.as_str().into()))
                        .collect::<Vec<_>>()
                        .into(),
                ),
            ),
        ]
        .into_iter()
        .collect();
        if !self.children.is_empty() {
            node.insert(
                plan_keys::CHILDREN.to_string(),
                Value::List(
                    self.children
                        .iter()
                        .map(|child| child.to_value())
                        .collect::<Vec<_>>()
                        .into(),
                ),
            );
        }
        // The typed warnings (typed node warnings): serialized as warning
        // maps — the shared vocabulary's name plus the warning's payload
        // (the `AllNodesScan` warning's suggested index) when it carries
        // one, present only when the node carries one, so the wire
        // consumer renders the engine's own message text.
        if !self.warnings.is_empty() {
            node.insert(
                plan_keys::WARNINGS.to_string(),
                Value::List(
                    self.warnings
                        .iter()
                        .map(warning_value)
                        .collect::<Vec<_>>()
                        .into(),
                ),
            );
        }
        node
    }
}

/// One warning's wire map (ADR-0345): the shared vocabulary's `name` plus
/// the warning's payload — the `AllNodesScan` warning's suggested index —
/// when the typed warning carries one. The one spelling of the warning
/// wire shape, beside [`PlanNode::node_map`] which assembles it.
fn warning_value(warning: &PlanWarning) -> Value {
    let mut fields: BTreeMap<String, Value> = [(
        plan_keys::WARNING_NAME.to_string(),
        Value::String(warning.name().into()),
    )]
    .into_iter()
    .collect();
    if let Some(index) = warning.suggested_index() {
        fields.insert(
            plan_keys::WARNING_INDEX.to_string(),
            Value::String(index.into()),
        );
    }
    Value::Map(std::sync::Arc::new(fields))
}

#[cfg(test)]
mod tests {
    use super::super::PlanWarning;
    use super::*;

    /// The wire warnings on a node carrying one: the typed vocabulary's
    /// names, by name — never a free-form string.
    #[test]
    fn to_value_serializes_warnings_by_name() {
        let node = PlanNode::new("AllNodesScan", "()".to_string(), Vec::new(), Vec::new())
            .with_warnings(vec![PlanWarning::AllNodesScan {
                index: ":Person(name)".to_string(),
            }]);
        let Value::Map(node) = node.to_value() else {
            panic!("the wire node is a map");
        };
        let warnings = match node.get(plan_keys::WARNINGS) {
            Some(Value::List(warnings)) => warnings,
            other => panic!("warnings serialize as a list, got {other:?}"),
        };
        // The warning map carries the name and the payload — the
        // suggested index — so the wire consumer renders the engine's
        // own message text.
        let all_nodes_scan = warnings
            .iter()
            .map(|w| match w {
                Value::Map(fields) => (
                    match fields.get(plan_keys::WARNING_NAME) {
                        Some(Value::String(name)) => name.to_string(),
                        other => panic!(
                            "a warning's {} is a string, got {other:?}",
                            plan_keys::WARNING_NAME
                        ),
                    },
                    match fields.get(plan_keys::WARNING_INDEX) {
                        Some(Value::String(index)) => Some(index.to_string()),
                        None => None,
                        other => panic!(
                            "a warning's {} is a string when present, got {other:?}",
                            plan_keys::WARNING_INDEX
                        ),
                    },
                ),
                other => panic!("a warning serializes as a map, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            all_nodes_scan,
            vec![(
                "AllNodesScan".to_string(),
                Some(":Person(name)".to_string())
            )],
            "the name and the payload are the wire spelling"
        );
        // A node with no warnings carries no `warnings` key — never an
        // empty list fabricated beside the absent ones.
        let clean = PlanNode::new(
            "ProduceResults",
            "1 item(s)".to_string(),
            Vec::new(),
            Vec::new(),
        );
        let Value::Map(clean) = clean.to_value() else {
            panic!("the wire node is a map");
        };
        assert!(clean.get(plan_keys::WARNINGS).is_none());
    }

    /// The Neo4j-facing entry is root-shaped (ADR-0357): the root carries
    /// the Neo4j-facing args while its children keep the engine's own form —
    /// `EstimatedRows` an integer on the child, a float only on the root.
    #[test]
    fn to_neo4j_value_decorates_the_root_and_keeps_children_undecorated() {
        let mut child = PlanNode::new("AllNodesScan", "(n)".to_string(), Vec::new(), Vec::new());
        child.estimated_rows = Some(7);
        let mut root = PlanNode::new(
            "ProduceResults",
            "1 item(s)".to_string(),
            Vec::new(),
            vec![child],
        );
        root.estimated_rows = Some(3);

        let Value::Map(root_map) = root.to_neo4j_value("Query: RETURN 1\nPlan:") else {
            panic!("a root decorates to a map");
        };
        let Some(Value::Map(root_args)) = root_map.get(plan_keys::ARGS) else {
            panic!("the decorated root carries an args map");
        };
        assert_eq!(
            root_args.get(plan_keys::ESTIMATED_ROWS),
            Some(&Value::Float(3.0)),
            "the root's estimate mirrors Neo4j's float"
        );
        assert!(
            matches!(root_args.get(plan_keys::PLANNER), Some(Value::String(name)) if name.as_ref() == "cypherlite"),
            "the root names this engine honestly: {root_args:?}"
        );
        let Some(Value::List(children)) = root_map.get(plan_keys::CHILDREN) else {
            panic!("the decorated root keeps its children");
        };
        let Some(Value::Map(child_map)) = children.first() else {
            panic!("a child is a map");
        };
        let Some(Value::Map(child_args)) = child_map.get(plan_keys::ARGS) else {
            panic!("the child carries an args map");
        };
        assert_eq!(
            child_args.get(plan_keys::ESTIMATED_ROWS),
            Some(&Value::Int(7)),
            "a child keeps the engine's integer estimate"
        );
        assert!(
            child_args.get(plan_keys::PLANNER).is_none(),
            "the decoration never reaches a child: {child_args:?}"
        );
    }
}
