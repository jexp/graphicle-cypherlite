//! The engine's public query-result type and its construction-time
//! metadata stamps: the resolved namespace (ADR-0117 §7), the plan-derived
//! query type (ADR-0162), the wall-clock elapsed (ADR-0221), and the
//! per-column type names (ADR-0224).

use std::time::Duration;

use cypherlite_executor::{QueryResult as ExecResult, QueryStats, StatementEffect};
use cypherlite_model::wire::Notification;
use cypherlite_planner::QueryType;

use crate::{ExecutionReceipt, Namespace, Value};

/// Duration → wall-clock milliseconds (ADR-0221): the single conversion
/// behind [`QueryResult::elapsed_ms`], also used by engine-internal
/// renderers (PROFILE's `_profile_time` cell) that hold a bare `Duration`.
pub(crate) fn duration_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The result metadata every statement announces, finished or failed
/// (ADR-0319): one home for the vocabulary both [`QueryResult`] and
/// [`ExecutionFailure`] carry, so the two can never drift apart.
///
/// [`ExecutionFailure`]: crate::ExecutionFailure
#[derive(Debug)]
pub struct ResultHeader {
    /// The result's columns: the RETURN aliases / generated names.
    pub columns: Vec<String>,
    /// Per-column result types (ADR-0224): one of the 18 canonical type
    /// names (`STRING`, `INTEGER`, `FLOAT`, `BOOLEAN`, `NODE`,
    /// `RELATIONSHIP`, `PATH`, `LIST`, `MAP`, `POINT`, `DATETIME`, `DATE`,
    /// `TIME`, `DURATION`, `VECTOR`, `UUID`, `BINARY`, `ANY`) per column —
    /// same length and order as [`ResultHeader::columns`] — derived from the
    /// FIRST row. A column whose first-row value is Null, or a result with
    /// no rows, stamps `ANY`; the name describes the first row only.
    pub column_types: Vec<String>,
    /// The statement's query type (ADR-0162): the Neo4j Query API v2
    /// `queryType` classification — [`QueryType::Read`] (`"r"`),
    /// [`QueryType::Write`] (`"w"`), [`QueryType::ReadWrite`] (`"rw"`) or
    /// [`QueryType::Schema`] (`"s"`) — derived from the logical plan
    /// before execution. A write splits on its returned columns
    /// (ADR-0332): no returned columns → `"w"` (`MATCH … SET` without
    /// RETURN), returned columns → `"rw"` (`CREATE (n) RETURN n`). Plain
    /// runs, EXPLAIN and PROFILE alike carry it; EXPLAIN and PROFILE
    /// report the wrapped statement's type.
    pub query_type: QueryType,
    /// The namespace the statement ran in (ADR-0117 §7): the statement's
    /// resolved `USE` target, or the session namespace when it carries no
    /// `USE` (ADR-0116 §3).
    pub namespace: Namespace,
    /// The statement's wall-clock elapsed time (ADR-0221): parse, plan,
    /// gates and dispatch (execution or EXPLAIN/PROFILE rendering),
    /// measured once around `Connection::run_query` — never around row
    /// serialization or surface-side output. Stamped at construction;
    /// there is no placeholder value.
    pub elapsed: Duration,
}

impl ResultHeader {
    /// The one construction every result-metadata site builds: `columns` and
    /// the result's own `rows` go in, the ADR-0224 per-column stamp derives
    /// from the first row here, so `QueryResult::from_exec`,
    /// `explain_result` and `ExecutionFailure::staged` cannot hand-spell the
    /// derivation (and its `column_types` length) apart. `rows` is `&[]`
    /// when the statement has none — a staged failure — which stamps every
    /// column `ANY`.
    pub(crate) fn new(
        columns: Vec<String>,
        rows: &[Vec<Value>],
        query_type: QueryType,
        namespace: Namespace,
        elapsed: Duration,
    ) -> Self {
        let column_types = stamp_column_types(columns.len(), rows);
        Self {
            columns,
            column_types,
            query_type,
            namespace,
            elapsed,
        }
    }
}

#[derive(Debug)]
pub struct QueryResult {
    /// The statement's result metadata (ADR-0319).
    pub header: ResultHeader,
    pub rows: Vec<Vec<Value>>,
    pub stats: QueryStats,
    /// The engine's truthful write effect (ADR-0402 Decision 2): whether a
    /// mutation operator reached execution and whether the engine applied a
    /// change. An execution fact of the same class as [`Self::receipt`] — not
    /// announced result metadata ([`ResultHeader`]) and not carried by any
    /// wire envelope: a surface that wants to report it reads it here. The
    /// CLI's batch zero-effect write note is the one consumer.
    ///
    /// ```
    /// use cypherlite::Database;
    ///
    /// let db = Database::open(":memory:")?;
    /// let mut conn = db.connect()?;
    ///
    /// // A read ran no mutation operator: nothing to report.
    /// assert!(!conn.query("MATCH (n:Missing) RETURN n")?.effect.changed_nothing());
    ///
    /// // A write whose row pipeline was empty applied nothing.
    /// let noop = conn.query("MATCH (n:Missing) DELETE n")?;
    /// assert!(noop.effect.attempted());
    /// assert!(!noop.effect.applied());
    /// assert!(noop.effect.changed_nothing());
    ///
    /// // An effective write, and a catalog change that leaves every
    /// // mutation counter at zero, both applied a change.
    /// assert!(conn.query("CREATE (:Kept)")?.effect.applied());
    /// assert!(conn.query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5")?.effect.applied());
    /// # Ok::<(), cypherlite::Error>(())
    /// ```
    pub effect: StatementEffect,
    /// The plan tree and the query notifications derived with it, as one
    /// artifact (ADR-0344/0345, ADR-0363): `Some` when the statement was
    /// `EXPLAIN` or `PROFILE`, `None` on every plain run — so a
    /// non-explained query never constructs a [`crate::PlanNode`] (ADR-0344) and
    /// a result cannot carry notifications without the tree they derive
    /// from. A `PROFILE` whose post-run estimates read is refused (the
    /// store was poisoned while the statement ran) also leaves it `None`:
    /// the executed rows survive that refusal un-enriched, never as an
    /// error. Boxed: the artifact is heavy and rarely populated, and
    /// surfaces buffer whole `QueryResult`s (the Bolt row cap) — `None`
    /// stays pointer-sized.
    ///
    /// ```
    /// use cypherlite::{Database, NotificationKind};
    ///
    /// let db = Database::open(":memory:")?;
    /// let mut conn = db.connect()?;
    /// conn.query("CREATE (:Person)")?;
    /// conn.query("CREATE (:City)")?;
    ///
    /// let explained = conn.query("EXPLAIN MATCH (a:Person), (b:City) RETURN a, b")?;
    /// let [notification] = explained.notifications() else {
    ///     panic!("the disconnected MATCH raises exactly one notification");
    /// };
    /// assert_eq!(notification.kind(), NotificationKind::CartesianProduct);
    /// assert_eq!(notification.gql_status(), "03N90");
    /// assert_eq!(notification.severity().as_str(), "INFORMATION");
    /// assert_eq!(notification.category().as_str(), "PERFORMANCE");
    /// assert!(notification.operator_id().is_some());
    /// assert!(notification.remedy().contains("join anchor"));
    ///
    /// // A plain run builds no plan artifact: no tree, so no
    /// // notifications, never a re-derivation of the EXPLAIN list.
    /// let plain = conn.query("MATCH (a:Person), (b:City) RETURN a, b")?;
    /// assert!(plain.plan.is_none());
    /// assert!(plain.notifications().is_empty());
    /// # Ok::<(), cypherlite::Error>(())
    /// ```
    pub plan: Option<Box<crate::PlanArtifacts>>,
    /// Actual tokens when token admission is on. `None` when it is off,
    /// and on `EXPLAIN` (the estimate rides [`crate::PlanNode::token_estimate`]).
    pub receipt: Option<ExecutionReceipt>,
    /// Keeps the result's peak-token reservation until the buffered rows are
    /// released by the consuming surface. Boxed so a retained ledger does not
    /// enlarge every `Executed::Rows` value.
    #[doc(hidden)]
    pub _retained_ledger: Option<Box<cypherlite_core::tokens::TokenLedger>>,
}

impl QueryResult {
    /// Wrap an executor result, stamped with the namespace the statement
    /// ran in, the statement's plan-derived query type, and the
    /// statement's measured elapsed (ADR-0221). Construction is the
    /// stamp: a `QueryResult` always carries its resolved namespace
    /// (ADR-0117 §7), its query type (ADR-0162) and its plan-plus-run
    /// elapsed — there is no placeholder value.
    pub(crate) fn from_exec(
        r: ExecResult,
        namespace: Namespace,
        query_type: QueryType,
        elapsed: Duration,
    ) -> Self {
        let header = ResultHeader::new(r.columns, &r.rows, query_type, namespace, elapsed);
        Self {
            header,
            rows: r.rows,
            stats: r.stats,
            effect: r.effect,
            plan: None,
            receipt: None,
            _retained_ledger: None,
        }
    }

    /// A result with no execution behind it — `EXPLAIN`'s plan-only output:
    /// no rows, no stats, no receipt, and the zero effect, because no mutation
    /// operator ran (ADR-0402 Decision 2's plan-producing exclusion). The one
    /// construction for a plan-producing result, so the explain path spells
    /// neither an empty result nor a placeholder effect at a construction site;
    /// the plan artifact attaches with [`Self::with_plan`].
    pub(crate) fn not_executed(header: ResultHeader) -> Self {
        Self {
            header,
            rows: Vec::new(),
            stats: QueryStats::default(),
            effect: StatementEffect::None,
            plan: None,
            receipt: None,
            _retained_ledger: None,
        }
    }

    /// Stamp the plan tree and the notifications derived with it
    /// (ADR-0344/0345, ADR-0363) as one artifact. The only construction of
    /// a plan-carrying result, so the tree and its list are always attached
    /// together — `EXPLAIN` and `PROFILE` both call it, a plain run never
    /// does.
    pub(crate) fn with_plan(
        mut self,
        root: crate::PlanNode,
        notifications: Vec<Notification>,
    ) -> Self {
        self.plan = Some(Box::new(crate::PlanArtifacts {
            root,
            notifications,
        }));
        self
    }

    /// The structured plan tree, `None` on a plain run — the read half of
    /// [`QueryResult::plan`] for every surface that renders a tree.
    pub fn plan_root(&self) -> Option<&crate::PlanNode> {
        self.plan.as_deref().map(|artifacts| &artifacts.root)
    }

    /// The query notifications derived with the plan tree (ADR-0363): the
    /// operator-scoped entries in the tree's pre-order — each carrying its
    /// operator's [`crate::PlanNode::id`] — then the statement-scoped entries by
    /// category, code and description. Identical `(kind, description)`
    /// pairs collapse to the first. Empty on a plain run, because no tree
    /// is built there (ADR-0363 decision 3); each entry's code, GQLSTATUS,
    /// title, severity, category and canonical remedy come from its
    /// [`crate::NotificationKind`].
    pub fn notifications(&self) -> &[Notification] {
        self.plan
            .as_deref()
            .map_or(&[], |artifacts| &artifacts.notifications)
    }

    /// The engine-reported statement elapsed in wall-clock milliseconds
    /// (ADR-0221) — the one Duration→ms conversion every surface uses, so
    /// the unit convention lives here and nowhere else.
    pub fn elapsed_ms(&self) -> f64 {
        duration_ms(self.header.elapsed)
    }
}

/// The ADR-0224 per-column stamp behind [`ResultHeader::new`] — the one
/// derivation, never re-spelled at a construction site: `column_count` names
/// derived from the first row, `ANY` for a Null cell or a result with no
/// rows. The name describes the first row only — Cypher columns are
/// heterogeneous. The classification is the single-sourced ADR-0224 kind set
/// (`cypherlite_core::column_type_name`); the `&'static str` names are
/// copied exactly once at the `Vec<String>` boundary below.
///
/// Engine rows are rectangular by contract — one value per column, in
/// column order (the CLI's exec path asserts the same) — so a first row
/// short of `column_count` is a contract break and fails loudly here,
/// not a column stamped `ANY`.
fn stamp_column_types(column_count: usize, rows: &[Vec<Value>]) -> Vec<String> {
    let first = rows.first();
    (0..column_count)
        .map(|i| {
            let cell = match first {
                Some(row) => row
                    .get(i)
                    .expect("engine rows are rectangular: one value per column"),
                None => &Value::Null,
            };
            cypherlite_core::column_type_name(cell)
        })
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::Database;

    /// A first-row `Value::Uuid` stamps `STRING`, not the reserved `UUID`
    /// name (ADR-0224: the wire taxonomy cannot distinguish a UUID from a
    /// string — `value_to_cypher` crosses it as `Str`). Pinned end to end
    /// through a uuid-producing query: the engine stamp and the wire
    /// envelope agree.
    #[test]
    fn uuid_first_row_stamps_string_and_matches_the_wire_envelope() {
        let db = Database::open(":memory:").unwrap();
        let mut conn = db.connect().unwrap();
        let result = conn.query("RETURN uuid() AS id").unwrap();
        assert_eq!(result.header.columns, ["id"]);
        assert_eq!(result.header.column_types, ["STRING"]);

        let envelope = crate::query_result_to_wire(&result);
        assert_eq!(envelope["columnTypes"], serde_json::json!(["STRING"]));
    }
}
