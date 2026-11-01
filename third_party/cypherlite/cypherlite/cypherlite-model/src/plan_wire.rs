//! The structured query plan wire form (ADR-0344/0345): the typed plan
//! node accessor and the plan contract the presentation and the engine's
//! warning construction read (ADR-0400 — the table layout itself lives
//! with its one consumer in the formatter) — one spelling so no surface
//! can drift from the wire form it reads. The typed warning vocabulary
//! itself lives in its own module beside this one (LABS-461:
//! `plan_warning.rs`), parsed here by the node accessor.

use super::plan_warning::PlanWarning;
use crate::CypherValue;

// ---------------------------------------------------------------------------
// Structured query plan wire form (ADR-0344/0345)
// ---------------------------------------------------------------------------

/// The one spelling of a plan column's unavailable value, shared by every
/// presentation of the plan table (the engine renderer and the surfaces'
/// table layout) so "column not produced" renders identically everywhere.
pub const PLAN_UNAVAILABLE: &str = "-";

// The typed warning vocabulary lives in its own module beside this one
// (LABS-461: one warning type — the enum, its estimate policy, and its
// wire-shape serializer in `plan_warning.rs`, re-exported through
// `wire`); the node wire form below reads it here.

/// The PROFILE metric set (ADR-0344 phase 2 + metric fields) as named
/// fields: which number is rows, nanoseconds, store accesses, or
/// row-materialization bytes is stated by the names, not by a position
/// convention.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlanMetricsWire {
    pub rows: i64,
    pub time_ns: i64,
    pub db_hits: i64,
    pub memory_bytes: i64,
    /// The store-read mix behind `db_hits` (LABS-369).
    pub reads: PlanReadsWire,
    /// The phase times behind the read kinds (LABS-448): the seek's and
    /// the walk's elapsed nanoseconds — emitted with the metric block,
    /// like the kinds, so a half-present pair is a wire-contract break.
    pub phases: PlanPhasesWire,
    /// The walk's chosen start, when the clause walked one (LABS-369).
    pub start: Option<PlanStartWire>,
}

/// A metrics-carrying node's presentation bundle (cumulative vs self):
/// the PROFILE metric set and its subtree total — the derived total
/// travels with the metrics it sums, present together or not at all, so
/// no presentation can render a total beside unavailable metrics or a
/// metric set without one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlanProfiledFacts {
    /// The node's PROFILE metric set.
    pub metrics: PlanMetricsWire,
    /// The subtree total of the node's elapsed time: its own `time_ns`
    /// plus every descendant's — the one fact the `Total (ms)` cell and
    /// the document's `total_time_ms` render.
    pub total_time_ns: i64,
}

/// The store-read mix a PROFILE node reports (LABS-369): the kinds the
/// one `Db Hits` total blends — relationship hops, neighbor walks, and
/// node-store reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanReadsWire {
    pub hops: i64,
    pub neighbor_walks: i64,
    pub node_reads: i64,
}

/// The phase times a PROFILE node reports (LABS-448): the elapsed time
/// the read kinds' store work spent — the seek (candidate resolution and
/// admission, the `Nodes` kind's work) and the walk (relationship
/// traversal, the `Rels`/`Degree` kinds' work), inclusive of a
/// multi-element pattern's nested phases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanPhasesWire {
    /// The seek phase's nanoseconds.
    pub seek_ns: i64,
    /// The walk phase's nanoseconds.
    pub walk_ns: i64,
}

/// How a PROFILE node's walk chose its start (LABS-369): the pattern
/// element index, the direction, and the reason class the executor
/// recorded (`bound-pivot`, `cheapest`, `seek-anchored`, `index-0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanStartWire {
    pub element: i64,
    pub backward: bool,
    /// The reason class, one of the `plan_keys::REASONS` vocabulary — a
    /// borrowed static, like the warning names.
    pub reason: &'static str,
}

/// The typed view of one structured-plan wire node (ADR-0344/0345): the
/// producer (`cypherlite`'s `PlanNode::to_value`) emits exactly this
/// shape, so this accessor states the invariants instead of tolerating
/// misses — a malformed plan fails loudly rather than silently dropping
/// a row or shifting cells into the wrong column.
pub struct PlanNodeWire<'a> {
    pub operator_type: &'a str,
    pub id: i64,
    pub details: &'a str,
    pub estimated_rows: Option<i64>,
    /// PROFILE metrics (ADR-0344 phase 2 + metric fields): rows
    /// produced, elapsed time, store accesses, and row-materialization
    /// bytes — present only on PROFILE trees: the producer always emits
    /// the four `args` metric fields together, so a half-present set is
    /// a wire-contract break that fails loudly.
    pub metrics: Option<PlanMetricsWire>,
    /// The node's args map, validated by the accessor (a map, present): a
    /// consumer reshaping the node (the HTTP Query API v2 plan) reads its
    /// display and identity entries from here rather than re-matching the
    /// raw map, so the invariant is stated once.
    pub args: &'a crate::IndexMap<String, CypherValue>,
    /// The identifier names the operator introduces, validated as strings
    /// at the accessor: the producer always emits the key as a list of
    /// strings, so a miss, a non-list or a non-string element is a
    /// wire-contract break that fails loudly here — the one place the
    /// invariant lives, never a per-consumer element check.
    pub identifiers: Vec<&'a str>,
    /// The child nodes' raw wire values — the consumer recurses through
    /// [`plan_node_wire`] itself, so the typed view stays shallow.
    pub children: &'a [CypherValue],
    /// The typed warnings the node carries (typed node warnings): parsed
    /// into the one [`PlanWarning`] vocabulary — an entry outside it (or
    /// a payload shape its variant does not carry) is a wire-contract
    /// break that fails loudly at the accessor, never a silently
    /// dropped warning.
    pub warnings: Vec<PlanWarning>,
}

impl PlanNodeWire<'_> {
    /// The operator's id as the width the notification side stamps
    /// (`Notification::operator_id`): the wire carries the engine's `usize`
    /// `PlanNode.id` as an `i64` (narrowed by the saturating `metric_i64`
    /// on serialize), so this is the one place the join key is widened
    /// back. A negative id is a wire-contract break and fails loudly, like
    /// the accessor's other non-negative integer fields — never a silent
    /// wrap that drops the operator's warnings.
    pub(crate) fn operator_id(&self) -> usize {
        usize::try_from(self.id).expect("plan wire Id is non-negative (ADR-0345)")
    }
}

/// Key names of the plan wire form — the vocabulary `PlanNode::to_value`
/// produces and every consumer reads, single-sourced here so drift between
/// producer and consumer is a compile error, not a silent mismatch.
pub mod plan_keys {
    pub const OPERATOR_TYPE: &str = "operatorType";
    pub const ARGS: &str = "args";
    pub const IDENTIFIERS: &str = "identifiers";
    pub const CHILDREN: &str = "children";
    pub const ID: &str = "Id";
    pub const DETAILS: &str = "Details";
    pub const ESTIMATED_ROWS: &str = "EstimatedRows";
    /// PROFILE metrics (ADR-0344 phase 2).
    pub const ROWS: &str = "Rows";
    pub const TIME_NS: &str = "TimeNs";
    /// PROFILE metric fields (ADR-0344 metric fields).
    pub const DB_HITS: &str = "DbHits";
    pub const MEMORY_BYTES: &str = "MemoryBytes";
    /// The store-read mix behind `DbHits` (LABS-369): the kinds the one
    /// total blends — relationship hops, neighbor walks, node-store reads.
    pub const HOPS: &str = "Hops";
    pub const NEIGHBOR_WALKS: &str = "NeighborWalks";
    pub const NODE_READS: &str = "NodeReads";
    /// The phase times the read kinds' store work spent (LABS-448): the
    /// seek (candidate resolution and admission — the `Nodes` kind's
    /// work) and the walk (relationship traversal — the `Rels`/`Degree`
    /// kinds' work), inclusive of a multi-element pattern's nested
    /// phases. Emitted together with the metric block and the kinds.
    pub const SEEK_TIME_NS: &str = "SeekTimeNs";
    /// See [`SEEK_TIME_NS`].
    pub const WALK_TIME_NS: &str = "WalkTimeNs";
    /// The walk's chosen start (LABS-369): the pattern element index, its
    /// direction, and the reason class (`bound-pivot`, `cheapest`,
    /// `seek-anchored`, `index-0`) — the fact an opaque `Expand` hid.
    pub const START_ELEMENT: &str = "StartElement";
    pub const START_BACKWARD: &str = "StartBackward";
    pub const START_REASON: &str = "StartReason";
    /// The start-reason vocabulary (LABS-369), the executor's
    /// `StartReason::as_str` names — single-sourced here so a renamed
    /// reason fails loudly at the wire boundary, like the warning names.
    pub const REASONS: [&str; 4] = ["bound-pivot", "cheapest", "seek-anchored", "index-0"];
    /// The typed warnings the node carries (typed node warnings): a list
    /// of warning maps from the [`PlanWarning`](crate::wire::PlanWarning)
    /// vocabulary, present only when the node carries one. Each map
    /// carries the warning's name and, when the warning has a payload
    /// (the `AllNodesScan` warning's suggested index), that payload —
    /// one spelling for the producer and every consumer.
    pub const WARNINGS: &str = "warnings";
    /// The warning map's name field (typed node warnings).
    pub const WARNING_NAME: &str = "name";
    /// The warning map's payload field (typed node warnings): the
    /// `AllNodesScan` warning's suggested property index text.
    pub const WARNING_INDEX: &str = "index";
    /// The Neo4j-facing root args (ADR-0357): the plan's rendered text —
    /// Neo4j's plan root carries it and tooling (Browser's Code tab,
    /// strict clients) renders from it. The engine's own args stay as
    /// they are; the one decoration lives on `PlanNode` (the
    /// `cypherlite` crate's `to_neo4j_value`, this vocabulary's only
    /// writer) and each surface calls it at its boundary.
    pub const STRING_REPRESENTATION: &str = "string-representation";
    /// The planner and runtime identity args (ADR-0357): `planner` names
    /// the planner, `planner-impl`/`planner-version` its implementation
    /// and version; `runtime`/`runtime-impl`/`runtime-version` the same
    /// for the runtime; `version` is the Cypher language version — Neo4j's
    /// plan root carries all of them and tooling (cypher-shell's summary
    /// block, Browser's footer) reads them.
    pub const PLANNER_IMPL: &str = "planner-impl";
    /// See [`PLANNER_IMPL`].
    pub const PLANNER: &str = "planner";
    /// See [`PLANNER_IMPL`].
    pub const PLANNER_VERSION: &str = "planner-version";
    /// See [`PLANNER_IMPL`].
    pub const RUNTIME: &str = "runtime";
    /// See [`PLANNER_IMPL`].
    pub const RUNTIME_IMPL: &str = "runtime-impl";
    /// See [`PLANNER_IMPL`].
    pub const RUNTIME_VERSION: &str = "runtime-version";
    /// See [`PLANNER_IMPL`]: the Cypher language version the plan ran
    /// under.
    pub const VERSION: &str = "version";
    /// See [`PLANNER_IMPL`]: Neo4j's `Memory` arg — the root mirrors this
    /// engine's row-materialization bytes under it (ADR-0357).
    pub const MEMORY: &str = "Memory";
}

/// The `planner-impl` value every surface sends (ADR-0357): this engine's
/// own planner, named honestly — never Neo4j's `IDP`/`COST`, which would
/// claim a planner this engine does not run.
pub const NEO4J_PLANNER_IMPL: &str = "cypherlite";

/// The Cypher language version the plan's `version` arg reports (ADR-0357):
/// this engine's front-end is openCypher 5-compatible (ADR-0046), so the
/// Neo4j-shaped `version` arg says `5` — the language version, not a
/// product version.
pub const CYPHER_VERSION: &str = "5";

/// Read one plan wire node (ADR-0344/0345). Panics on a malformed node —
/// the wire form is engine-produced and invariant-bound, so a deviation
/// is an engine contract break, kept loud like every other boundary here.
pub fn plan_node_wire(value: &CypherValue) -> PlanNodeWire<'_> {
    let CypherValue::Map(node) = value else {
        panic!("plan wire node is a map, got {value:?}")
    };
    let operator_type = match node.get(plan_keys::OPERATOR_TYPE) {
        Some(CypherValue::Str(op)) => op.as_str(),
        other => panic!(
            "plan wire node carries a string {} (ADR-0345), got {other:?}",
            plan_keys::OPERATOR_TYPE
        ),
    };
    let args = match node.get(plan_keys::ARGS) {
        Some(CypherValue::Map(args)) => args,
        other => panic!(
            "plan wire node carries a map {} (ADR-0345), got {other:?}",
            plan_keys::ARGS
        ),
    };
    let id = match args.get(plan_keys::ID) {
        Some(CypherValue::Int(id)) => *id,
        other => panic!(
            "plan wire node carries an integer {} (ADR-0345), got {other:?}",
            plan_keys::ID
        ),
    };
    let details = match args.get(plan_keys::DETAILS) {
        Some(CypherValue::Str(details)) => details.as_str(),
        other => panic!(
            "plan wire node carries a string {} (ADR-0345), got {other:?}",
            plan_keys::DETAILS
        ),
    };
    let estimated_rows = optional_int(args, plan_keys::ESTIMATED_ROWS);
    let rows = optional_int(args, plan_keys::ROWS);
    let time_ns = optional_int(args, plan_keys::TIME_NS);
    let db_hits = optional_int(args, plan_keys::DB_HITS);
    let memory_bytes = optional_int(args, plan_keys::MEMORY_BYTES);
    let has_metrics =
        rows.is_some() && time_ns.is_some() && db_hits.is_some() && memory_bytes.is_some();
    // LABS-448: the phase times ride the metric set with the kinds,
    // through the one group policy.
    let phases = metric_set_ints(
        args,
        &[plan_keys::SEEK_TIME_NS, plan_keys::WALK_TIME_NS],
        has_metrics,
    )
    .map(|values| PlanPhasesWire {
        seek_ns: values[0],
        walk_ns: values[1],
    })
    .unwrap_or_default();
    // LABS-369: the read-kind mix rides the metric set, through the one
    // group policy the phase times share — one spelling, never a
    // hand-rolled copy per field family.
    let reads = metric_set_ints(
        args,
        &[
            plan_keys::HOPS,
            plan_keys::NEIGHBOR_WALKS,
            plan_keys::NODE_READS,
        ],
        has_metrics,
    )
    .map(|values| PlanReadsWire {
        hops: values[0],
        neighbor_walks: values[1],
        node_reads: values[2],
    })
    .unwrap_or_default();
    // The walk's chosen start (LABS-369): the element, direction and
    // reason class, present only when the clause walked a pattern —
    // emitted together, like the metric set.
    let start = {
        let element = optional_int(args, plan_keys::START_ELEMENT);
        let backward = optional_bool(args, plan_keys::START_BACKWARD);
        let reason = match args.get(plan_keys::START_REASON) {
            None => None,
            Some(CypherValue::Str(reason)) => Some(
                match plan_keys::REASONS.iter().find(|r| **r == reason.as_str()) {
                    Some(name) => *name,
                    None => panic!(
                        "plan wire {} is one of {:?} (LABS-369), got {reason:?}",
                        plan_keys::START_REASON,
                        plan_keys::REASONS
                    ),
                },
            ),
            other => panic!(
                "plan wire {} is a string when present (LABS-369), got {other:?}",
                plan_keys::START_REASON
            ),
        };
        match (has_metrics, element, backward, reason) {
            (_, None, None, None) => None,
            (true, Some(element), Some(backward), Some(reason)) if element >= 0 => {
                Some(PlanStartWire {
                    element,
                    backward,
                    reason,
                })
            }
            (has_metrics, element, backward, reason) => panic!(
                "plan wire {}, {} and {} are emitted together, only on a \
                 PROFILE node (LABS-369), got metrics={has_metrics} \
                 element={element:?} backward={backward:?} reason={reason:?}",
                plan_keys::START_ELEMENT,
                plan_keys::START_BACKWARD,
                plan_keys::START_REASON
            ),
        }
    };
    // The producer emits the four metric fields together under one
    // metrics value: a half-present set is a contract break, kept loud.
    // The producer saturates at the i64 boundary, never negative: a
    // negative cell is a wire-contract break, kept loud like the
    // half-present set (ADR-0344 phase 2).
    let metrics = match (rows, time_ns, db_hits, memory_bytes) {
        (None, None, None, None) => None,
        (Some(rows), Some(time_ns), Some(db_hits), Some(memory_bytes))
            if rows < 0 || time_ns < 0 || db_hits < 0 || memory_bytes < 0 =>
        {
            panic!(
                "plan wire {}, {}, {} and {} are non-negative when present \
                 (ADR-0344 phase 2 + metric fields), got rows={rows} \
                 time_ns={time_ns} db_hits={db_hits} memory={memory_bytes}",
                plan_keys::ROWS,
                plan_keys::TIME_NS,
                plan_keys::DB_HITS,
                plan_keys::MEMORY_BYTES
            )
        }
        (Some(rows), Some(time_ns), Some(db_hits), Some(memory_bytes)) => Some(PlanMetricsWire {
            rows,
            time_ns,
            db_hits,
            memory_bytes,
            reads,
            phases,
            start,
        }),
        (rows, time_ns, db_hits, memory_bytes) => panic!(
            "plan wire {}, {}, {} and {} are emitted together (ADR-0344 \
             metric fields), got rows={rows:?} time={time_ns:?} \
             db_hits={db_hits:?} memory={memory_bytes:?}",
            plan_keys::ROWS,
            plan_keys::TIME_NS,
            plan_keys::DB_HITS,
            plan_keys::MEMORY_BYTES
        ),
    };
    let children = match node.get(plan_keys::CHILDREN) {
        None => &[],
        Some(CypherValue::List(children)) => children.as_slice(),
        other => panic!(
            "plan wire {} is a list when present (ADR-0345), got {other:?}",
            plan_keys::CHILDREN
        ),
    };
    // The typed warnings (typed node warnings): warning maps — the
    // vocabulary name and, when the warning carries one, its payload —
    // anything else is a wire-contract break kept loud.
    let warnings = match node.get(plan_keys::WARNINGS) {
        None => Vec::new(),
        Some(CypherValue::List(items)) => items
            .iter()
            .map(|item| {
                let CypherValue::Map(warning) = item else {
                    panic!(
                        "plan wire {} entries are warning maps, got {item:?}",
                        plan_keys::WARNINGS
                    )
                };
                let name = match warning.get(plan_keys::WARNING_NAME) {
                    Some(CypherValue::Str(name)) => name.as_str(),
                    other => panic!(
                        "plan wire warning {} is a string, got {other:?}",
                        plan_keys::WARNING_NAME
                    ),
                };
                let index = match warning.get(plan_keys::WARNING_INDEX) {
                    None => None,
                    Some(CypherValue::Str(index)) => Some(index.as_str()),
                    other => panic!(
                        "plan wire warning {} is a string when present, got {other:?}",
                        plan_keys::WARNING_INDEX
                    ),
                };
                // The one validating match into the typed vocabulary: the
                // name must come from it, carrying the payload shape its
                // variant carries. A miss is a contract break kept loud,
                // never a silently dropped or reinterpreted warning.
                PlanWarning::from_wire_parts(name, index).unwrap_or_else(|| {
                    panic!(
                        "plan wire warnings come from the typed vocabulary \
                         (ADR-0344 typed node warnings) with the payload shape \
                         their variant carries, got {name:?}"
                    )
                })
            })
            .collect(),
        other => panic!(
            "plan wire {} is a list of warning maps when present, got {other:?}",
            plan_keys::WARNINGS
        ),
    };
    // The producer always writes the identifiers key as a list: a miss or
    // a non-list is a wire-contract break, kept loud like the node's own
    // shape (the consumer reshaping the node reads them from the typed
    // view, never re-matching the raw map).
    let identifiers: Vec<&str> = match node.get(plan_keys::IDENTIFIERS) {
        Some(CypherValue::List(identifiers)) => identifiers
            .iter()
            .map(|identifier| match identifier {
                CypherValue::Str(name) => name.as_str(),
                other => panic!(
                    "a plan wire node's {} element is a string (ADR-0345), got {other:?}",
                    plan_keys::IDENTIFIERS
                ),
            })
            .collect(),
        other => panic!(
            "plan wire node carries a list {} (ADR-0345), got {other:?}",
            plan_keys::IDENTIFIERS
        ),
    };
    PlanNodeWire {
        operator_type,
        id,
        details,
        estimated_rows,
        metrics,
        args,
        identifiers,
        children,
        warnings,
    }
}

/// The one subtree-total pass (cumulative vs self): every node's own
/// elapsed time plus every descendant's, saturated through the metric
/// width policy — ONE walk over the tree, keyed by the node's pre-order
/// id, so the table's walk and the agent document's walk read the same
/// map instead of each re-folding its subtree (one policy, one spelling).
/// Reads the typed accessor (the same validation every other consumer
/// takes), so a malformed tree fails loudly here rather than silently
/// summing partial subtrees.
pub fn plan_subtree_totals(plan: &CypherValue) -> crate::IndexMap<i64, i64> {
    let mut totals = crate::IndexMap::new();
    subtree_total_into(plan, &mut totals);
    totals
}

/// The pass's recursion: a node's own time plus its children's totals,
/// inserted under the node's id and returned as the parent's summand —
/// the saturation lands once per node (`metric_i64` into the map), never
/// an i64→u64→u128 round-trip on the way up.
fn subtree_total_into(plan: &CypherValue, totals: &mut crate::IndexMap<i64, i64>) -> u128 {
    let node = plan_node_wire(plan);
    let own = node
        .metrics
        .map_or(0u128, |m| metric_u64(m.time_ns) as u128);
    let children = node
        .children
        .iter()
        .map(|child| subtree_total_into(child, totals))
        .fold(0u128, u128::saturating_add);
    let sum = own.saturating_add(children);
    totals.insert(node.id, metric_i64(sum));
    sum
}

/// The wire form's one width policy for metric integers: saturate at
/// `i64::MAX` — one policy for every metric field (rows and time
/// alike), never an unchecked cast on one and a clamp on the other.
pub fn metric_i64(value: u128) -> i64 {
    value.min(i64::MAX as u128) as i64
}
/// The consumer side of the metric width policy: a wire metric is
/// non-negative when present (the producer saturates), so the
/// one conversion every metric cell reads goes through here — never a
/// bare `as` cast at a call site.
pub fn metric_u64(value: i64) -> u64 {
    value.max(0) as u64
}
/// The one nanoseconds-to-milliseconds conversion, as a float: every
/// millisecond presentation reads it — the table cell's `{:.3}` render,
/// the HTTP v2 node's `time` field, the agent document's `time_ms` and
/// `total_time_ms` — so the spellings cannot drift (and none truncates
/// a sub-millisecond operator to 0).
pub fn time_ns_ms(time_ns: i64) -> f64 {
    metric_u64(time_ns) as f64 / 1e6
}

/// A metrics-carrying node's presentation bundle, read from the totals
/// map — the ONE spelling of the pairing's contract ("the map covers
/// every walked node"), so the table's walk and the agent document's
/// walk cannot each re-state the lookup, and a third consumer inherits
/// it instead of cloning the panic.
pub fn profiled_facts(
    metrics: PlanMetricsWire,
    totals: &crate::IndexMap<i64, i64>,
    id: i64,
) -> PlanProfiledFacts {
    PlanProfiledFacts {
        metrics,
        total_time_ns: *totals
            .get(&id)
            .unwrap_or_else(|| panic!("the totals map covers every walked node, id {id}")),
    }
}

/// The rides-the-metric-set group policy (one loud spelling, never a
/// hand-rolled copy per field family): the keys' values are an
/// all-or-none group of non-negative integers, present only when the
/// metric set is — a partial group, a group without metrics, or a
/// negative value is a wire-contract break kept loud. `None` when the
/// whole group is absent (an EXPLAIN node).
fn metric_set_ints(
    args: &crate::IndexMap<String, CypherValue>,
    keys: &[&str],
    has_metrics: bool,
) -> Option<Vec<i64>> {
    let values: Vec<Option<i64>> = keys.iter().map(|k| optional_int(args, k)).collect();
    let any_present = values.iter().any(Option::is_some);
    let all_present = values.iter().all(Option::is_some);
    match (has_metrics, any_present, all_present) {
        (false, false, _) => None,
        (true, true, true) if !values.iter().any(|v| v.is_some_and(|n| n < 0)) => Some(
            values
                .into_iter()
                .map(|v| v.expect("the group is all-present"))
                .collect(),
        ),
        (has_metrics, any_present, all_present) => panic!(
            "plan wire {keys:?} are emitted together with the metric fields \
             (all-or-none, non-negative), got metrics={has_metrics} \
             any_present={any_present} all_present={all_present} values={values:?}"
        ),
    }
}

impl PlanNodeWire<'_> {
    /// The node's input-row count (LABS-343's structural fact): the first
    /// child's produced rows — the bottom-up nesting puts each earlier
    /// clause under the one it feeds — `None` when the node has no input
    /// child (the statement's one seed row). The one spelling, read by
    /// the plan walk and the flag fold alike.
    pub fn input_rows(&self) -> Option<u64> {
        self.children
            .first()
            .and_then(|first| plan_node_wire(first).metrics.map(|m| metric_u64(m.rows)))
    }
}

/// The optional-integer arg — one extraction policy for every optional
/// metric key: absent or an integer, a malformed value fails loudly.
fn optional_int(args: &crate::IndexMap<String, CypherValue>, key: &str) -> Option<i64> {
    match args.get(key) {
        None => None,
        Some(CypherValue::Int(n)) => Some(*n),
        other => panic!("plan wire {key} is an integer when present (ADR-0345), got {other:?}"),
    }
}

/// The optional-boolean arg — the same policy as [`optional_int`] for the
/// one boolean fact (LABS-369's walk direction).
fn optional_bool(args: &crate::IndexMap<String, CypherValue>, key: &str) -> Option<bool> {
    match args.get(key) {
        None => None,
        Some(CypherValue::Bool(flag)) => Some(*flag),
        other => panic!("plan wire {key} is a boolean when present (LABS-369), got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::plan_test_fixtures::plan_wire_node;

    /// The one subtree-total pass (cumulative vs self): a node's own time
    /// plus every descendant's — the leaf equals its own time, the root
    /// sums the pipeline, an inner node's total lands under its id, and an
    /// unprofiled subtree contributes zero.
    #[test]
    fn subtree_totals_map_the_tree() {
        // root(10) -> [scan(3), filter(2) -> [expand(5)]]: the root's
        // total is 10 + 3 + 2 + 5 = 20; the filter heads the expand.
        let expand = plan_wire_node("Expand", 3, Some(5), vec![]);
        let filter = plan_wire_node("Filter", 2, Some(2), vec![expand]);
        let scan = plan_wire_node("NodeByLabelScan", 1, Some(3), vec![]);
        let root = plan_wire_node("ProduceResults", 0, Some(10), vec![scan, filter]);
        let totals = plan_subtree_totals(&root);
        assert_eq!(totals[&0], 20, "the root's total covers the pipeline");
        assert_eq!(totals[&2], 7, "the filter's total heads the expand");
        assert_eq!(totals[&1], 3, "a leaf's total equals its own time");
        // An unprofiled node contributes nothing — an EXPLAIN tree's
        // totals are zero, never a fabricated number.
        let explain = plan_subtree_totals(&plan_wire_node("ProduceResults", 0, None, vec![]));
        assert_eq!(explain[&0], 0);
    }
}
