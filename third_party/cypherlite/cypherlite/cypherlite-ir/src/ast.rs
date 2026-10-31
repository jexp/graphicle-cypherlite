//! OpenCypher AST (ADR-0007), with the pure `Expr`-shape recognizers
//! beside the type they recognize.

use crate::search::SearchSpec;

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub parts: Vec<SingleQuery>,
    /// `union_all[i]` applies between `parts[i]` and `parts[i+1]`.
    pub union_all: Vec<bool>,
    /// CIP-213 sequential statement composition.
    pub next: Option<Box<Query>>,
    /// CIP-150: the statement's `CYPHER <version>` preamble, lowered from
    /// `cypherlite_parser::Statement::cypher_version`. Set on the top-level
    /// query (and inherited by `NEXT` stages); nested subquery bodies
    /// (`CALL { }`, EXISTS/COUNT/COLLECT expression subqueries) keep `None`
    /// here and inherit the enclosing statement's version at evaluation
    /// time: the executor threads it through `QueryCtx`/`ExecCtx`/`EvalCtx`
    /// into `subquery_qctx` (`Query::cypher_version.or(enclosing)`).
    pub cypher_version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SingleQuery {
    pub clauses: Vec<Clause>,
}

/// A `CALL … YIELD` tail: no `YIELD` at all, `YIELD *`, or an explicit
/// list of (procedure output name, optional alias) pairs. The typed shape
/// replaces the old `"*"`-string sentinel, so a real column named `*`
/// cannot be confused with the wildcard.
#[derive(Debug, Clone, PartialEq)]
pub enum YieldItems {
    /// No `YIELD` clause: the procedure's default yields apply.
    Default,
    /// `YIELD *`: every output the procedure declares.
    Star,
    /// An explicit `YIELD a [AS x], b ...` list, in source order.
    Named(Vec<(String, Option<String>)>),
}

impl YieldItems {
    /// The explicit items, or an empty slice for `Default`/`Star`.
    pub fn items(&self) -> &[(String, Option<String>)] {
        match self {
            YieldItems::Named(items) => items,
            YieldItems::Default | YieldItems::Star => &[],
        }
    }

    /// Raw output names of an explicit list, in order (empty otherwise).
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.items().iter().map(|(name, _)| name.as_str())
    }

    /// Each explicit item's raw name and effective (alias-else-name) name.
    pub fn effective_pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.items()
            .iter()
            .map(|(name, alias)| (name.as_str(), alias.as_deref().unwrap_or(name)))
    }

    /// Effective (post-alias) output names of an explicit list.
    pub fn effective_names(&self) -> impl Iterator<Item = &str> {
        self.effective_pairs().map(|(_, effective)| effective)
    }

    /// Whether any `YIELD` tail was written (`Star` or `Named`).
    pub fn has_yield(&self) -> bool {
        !matches!(self, YieldItems::Default)
    }

    /// The yield tail resolved against a procedure's `default` output
    /// columns: `YIELD *` takes every default (the same as no `YIELD`),
    /// an explicit list is itself. The one classification for callers that
    /// need the resolved pairs — never a boolean collapsed to `items()`
    /// (Star's items are empty by definition).
    pub fn resolved_pairs(&self, defaults: &[&str]) -> Vec<(String, Option<String>)> {
        match self {
            YieldItems::Named(items) => items.to_vec(),
            YieldItems::Star | YieldItems::Default => defaults
                .iter()
                .map(|column| (column.to_string(), None))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    /// CypherLite's rich catalog/admin AST, retained losslessly by the
    /// CypherLite front-end and executed against CypherLite metadata.
    Catalog(Box<cypherlite_parser::Clause>),
    Match {
        optional: bool,
        match_mode: MatchMode,
        patterns: Vec<PathPattern>,
        where_clause: Option<Expr>,
        /// CIP-224/CIP-240 vector index search attached to this MATCH.
        search: Option<Box<SearchClause>>,
    },
    Create {
        patterns: Vec<PathPattern>,
    },
    Merge {
        pattern: PathPattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
        explicit: bool,
        yield_items: YieldItems,
        optional: bool,
        where_clause: Option<Expr>,
    },
    Delete {
        detach: bool,
        expressions: Vec<Expr>,
    },
    Set {
        items: Vec<SetItem>,
    },
    Remove {
        items: Vec<RemoveItem>,
    },
    With {
        distinct: bool,
        /// CIP-236: explicit grouping keys (Some(vec![]) = `GROUP BY ()`
        /// empty set; None = infer from the non-aggregating items).
        group_by: Option<Vec<Expr>>,
        items: Vec<Projection>,
        order: Vec<OrderItem>,
        skip: Option<Expr>,
        limit: Option<Expr>,
        where_clause: Option<Expr>,
        origin: WithOrigin,
    },
    Unwind {
        expression: Expr,
        alias: String,
    },
    Return {
        distinct: bool,
        /// CIP-236 explicit grouping keys (same encoding as `With`).
        group_by: Option<Vec<Expr>>,
        items: Vec<Projection>,
        order: Vec<OrderItem>,
        skip: Option<Expr>,
        limit: Option<Expr>,
    },
    /// Named `(label, property)` catalog entry (ADR-0040).
    CreateIndex {
        name: Option<String>,
        if_not_exists: bool,
        label: String,
        property: String,
    },
    DropIndex {
        if_exists: bool,
        target: DropIndexTarget,
    },
    ShowIndexes,
    /// CIP-140: terminal empty projection (0 columns).
    Finish,
    /// `LOAD CSV [WITH HEADERS] FROM … AS …`.
    LoadCsv {
        with_headers: bool,
        url: Expr,
        variable: String,
        field_terminator: Option<String>,
    },
    /// `CALL { … }` / `OPTIONAL CALL { … }` (CIP-141 / CIP-32).
    CallSubquery {
        query: Box<Query>,
        optional: bool,
        scope: CallScope,
        in_transactions: Option<InTransactions>,
    },
    /// CIP-135 top-level WHEN/THEN/ELSE.
    Conditional {
        branches: Vec<(Expr, Query)>,
        else_query: Option<Box<Query>>,
    },
    /// Updating FOREACH loop.
    Foreach {
        variable: String,
        list: Expr,
        clauses: Vec<Clause>,
    },
    /// Single-graph runtime: validates scope, then acts as a no-op.
    Use {
        graph: Expr,
    },
    /// Constraint catalog op recorded in WAL via create/set checks (wave D).
    CreateConstraint {
        name: Option<String>,
        if_not_exists: bool,
        relationship: bool,
        label: String,
        properties: Vec<String>,
        kind: String,
        property_type: Option<TypeExpr>,
    },
    DropConstraint {
        name: String,
        if_exists: bool,
    },
}

/// Native CypherLite representation of a SEARCH subclause (CIP-224, the
/// CIP-278 `<source specification clause>` extension).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchClause {
    pub binding_var: String,
    /// CIP-278 `<source specification clause>`: one index search, or a
    /// `COMBINE` of two or more weighted sources fused with WRRF.
    pub spec: SearchSpec,
    pub score_var: Option<String>,
}

/// `DROP INDEX name` or `DROP INDEX ON :Label(prop)` (ADR-0040).
#[derive(Debug, Clone, PartialEq)]
pub enum DropIndexTarget {
    Name(String),
    Schema { label: String, property: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub expr: Expr,
    pub alias: Option<String>,
    /// Verbatim source text of the unaliased projection expression, as
    /// captured by the parser. Used by [`Projection::output_name`] as the
    /// column name for expression shapes without a dedicated label
    /// (`count{()}`, list/map literals, `CASE`, etc.). Synthetic
    /// projections built after parsing leave this `None`.
    pub source_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WithOrigin {
    #[default]
    Explicit,
    Let,
    Filter,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallScope {
    None,
    Wildcard,
    Vars(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InTransactions {
    pub concurrency: Concurrency,
    pub batch_size: Option<Expr>,
    pub on_error: OnError,
    pub report_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Concurrency {
    Sequential,
    Default,
    Value(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum OnError {
    Fail,
    Continue,
    Break,
    Retry {
        duration: Option<Expr>,
        fallback: RetryFallback,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryFallback {
    Fail,
    Continue,
    Break,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub ascending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    Property {
        target: Expr,
        value: Expr,
    },
    MergeMap {
        target: Expr,
        value: Expr,
    },
    Labels {
        target: Expr,
        labels: Vec<LabelExpr>,
    },
    LabelsFunction {
        target: Expr,
        value: Expr,
        op: LabelsFunctionOp,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelsFunctionOp {
    Overwrite,
    Extend,
    Remove,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    Property {
        target: Expr,
    },
    Labels {
        target: Expr,
        labels: Vec<LabelExpr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// Named path: `MATCH p = (a)-[]->(b)`.
    pub variable: Option<String>,
    /// Lossless GPM element sequence. Empty only for legacy callers that
    /// construct the compact `nodes`/`rels` representation directly.
    pub elements: Vec<PatternElement>,
    pub nodes: Vec<NodePattern>,
    pub rels: Vec<RelPattern>,
    pub where_clause: Option<Box<Expr>>,
    pub mode: Option<PathMode>,
    pub selector: Option<PathSelector>,
    pub is_shortest_path_function: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelPattern),
    Quantified(QuantifiedPathPattern),
}

impl PathPattern {
    /// Whether this path must be matched by the GPM engine rather than the
    /// compact node/rel walker: either the compact `nodes`/`rels`
    /// representation was cleared at lowering (non-alternating or quantified
    /// element sequence), or GPM-only features are present (path mode, path
    /// selector, path-level or element-level predicates).
    pub fn needs_gpm(&self) -> bool {
        self.nodes.is_empty()
            || self.mode.is_some()
            || self.selector.is_some()
            || self.where_clause.is_some()
            || self.elements.iter().any(|element| match element {
                PatternElement::Node(node) => node.where_clause.is_some(),
                PatternElement::Relationship(rel) => rel.where_clause.is_some() || rel.quantified,
                PatternElement::Quantified(_) => true,
            })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuantifiedPathPattern {
    pub subpath_variable: Option<String>,
    pub elements: Vec<PatternElement>,
    pub where_clause: Option<Expr>,
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    #[default]
    DifferentRelationships,
    RepeatableElements,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    Walk,
    Trail,
    Acyclic,
    Simple,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PathCount {
    Literal(u32),
    Parameter(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PathSelector {
    All,
    Any(Option<PathCount>),
    AnyShortest,
    AllShortest,
    Shortest {
        count: Option<PathCount>,
        group: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub variable: Option<String>,
    /// Static conjunction, retained for catalog/index candidate selection.
    pub labels: Vec<String>,
    /// Complete CIP-14/CIP-156 label expression.
    pub label_expr: Option<LabelExpr>,
    pub properties: Option<Expr>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub variable: Option<String>,
    /// Static disjunction, retained for typed-CSR candidate selection.
    pub types: Vec<String>,
    /// Complete CIP-14/CIP-156 relationship type expression.
    pub type_expr: Option<LabelExpr>,
    pub direction: Direction,
    pub properties: Option<Expr>,
    pub var_length: bool,
    pub min_hops: u32,
    pub max_hops: Option<u32>,
    pub where_clause: Option<Expr>,
    pub quantified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Both,
}

impl Direction {
    /// The opposite traversal direction (`Both` flips to itself).
    pub fn flip(self) -> Direction {
        match self {
            Direction::Outgoing => Direction::Incoming,
            Direction::Incoming => Direction::Outgoing,
            Direction::Both => Direction::Both,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Executor-internal eager value; never produced by either parser.
    RuntimeValue(cypherlite_core::Value),
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Parameter(String),
    Variable(String),
    Star,
    Property {
        expr: Box<Expr>,
        key: String,
    },
    DynamicProperty {
        expr: Box<Expr>,
        key: Box<Expr>,
    },
    LabelPredicate {
        expr: Box<Expr>,
        label_expr: LabelExpr,
        strict: bool,
    },
    Map(Vec<(String, Expr)>),
    List(Vec<Expr>),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Function {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
    },
    CountStar,
    Case {
        operand: Option<Box<Expr>>,
        /// `(condition, result, is_predicate)`. Predicate branches are the
        /// generalized/simple-CASE `WHEN IS [NOT] LABELED ...` form and are
        /// evaluated as booleans rather than compared to `operand`.
        when_then: Vec<(Expr, Expr, bool)>,
        else_expr: Option<Box<Expr>>,
    },
    Subscript {
        expr: Box<Expr>,
        index: Box<Expr>,
    },
    Slice {
        expr: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
    },
    ListComp {
        ident: String,
        list: Box<Expr>,
        predicate: Option<Box<Expr>>,
        projection: Box<Expr>,
    },
    /// CIP-272 map comprehension (lowered from the parser's
    /// `MapComprehension`).
    MapComp {
        /// Key binding of a map source (`None` for a list source).
        key_ident: Option<String>,
        value_ident: String,
        source: Box<Expr>,
        predicate: Option<Box<Expr>>,
        key_expr: Box<Expr>,
        value_expr: Box<Expr>,
    },
    Quantifier {
        kind: QuantifierKind,
        ident: String,
        list: Box<Expr>,
        predicate: Box<Expr>,
    },
    Reduce {
        accumulator: String,
        initial: Box<Expr>,
        ident: String,
        list: Box<Expr>,
        expression: Box<Expr>,
    },
    AllReduce {
        accumulator: String,
        initial: Box<Expr>,
        ident: String,
        list: Box<Expr>,
        expression: Box<Expr>,
        predicate: Box<Expr>,
    },
    IsType {
        expr: Box<Expr>,
        type_expr: TypeExpr,
        negated: bool,
    },
    IsNormalized {
        expr: Box<Expr>,
        form: Option<String>,
        negated: bool,
    },
    /// Map projection `expr { key: value, .key, var, .* }`. Typed form of
    /// what used to be tunnelled through a `__map_projection` pseudo-call.
    MapProjection {
        expr: Box<Expr>,
        items: Vec<MapProjectionItem>,
    },
    /// CIP-214 distance-metric keyword (e.g. the `EUCLIDEAN` in
    /// `vector_distance(a, b, EUCLIDEAN)`), resolved at lowering from the
    /// bare identifier in the final argument of `vector_distance` /
    /// `vector_norm`. Only meaningful as an argument to those functions.
    DistanceMetric(VectorMetric),
    PatternPred {
        path: PathPattern,
    },
    PatternComp {
        path: PathPattern,
        predicate: Option<Box<Expr>>,
        projection: Box<Expr>,
    },
    ShortestPath {
        all: bool,
        path: PathPattern,
    },
    ExistsSubquery {
        query: Query,
    },
    CountSubquery {
        query: Query,
    },
    CollectSubquery {
        query: Query,
    },
}

/// The `<var>.<key>` property access shape: the variable's and the
/// property key's borrowed names, or `None` for any other expression.
/// The seek/fold/vector operand recognizers derive from here.
pub fn var_prop(expr: &Expr) -> Option<(&str, &str)> {
    let Expr::Property { expr, key } = expr else {
        return None;
    };
    let Expr::Variable(v) = expr.as_ref() else {
        return None;
    };
    Some((v, key))
}

/// The owned spelling of the `<var>.<key>` pair (filters and plan
/// items own their strings). A cross-crate contract, not a local
/// convenience: the planner passes it as the recognizer fn to
/// `orient_side` and the executor's `unwind` builds owned plan items
/// from it, so the public owned spelling stays named rather than
/// mapped inline at each call site.
pub fn var_prop_owned(expr: &Expr) -> Option<(String, String)> {
    var_prop(expr).map(|(var, key)| (var.to_string(), key.to_string()))
}

/// One entry of a map projection (`Expr::MapProjection`).
#[derive(Debug, Clone, PartialEq)]
pub enum MapProjectionItem {
    /// Explicit `key: value` entry.
    KeyValue { key: String, value: Expr },
    /// `.key` property shorthand, read from the projection target.
    Property { key: String },
    /// Bare variable entry: `var` shorthand for `var: var`.
    Variable { name: String },
    /// `.*`: every property of the target.
    AllProperties,
}

/// CIP-214 distance metric for `vector_distance` / `vector_norm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMetric {
    Euclidean,
    EuclideanSquared,
    Manhattan,
    Cosine,
    Dot,
    Hamming,
}

impl VectorMetric {
    /// Parse the bare metric keyword (case-insensitive). `None` when the
    /// identifier is not a known metric, in which case lowering leaves it as
    /// an ordinary variable reference for the binder to reject.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_uppercase().as_str() {
            "EUCLIDEAN" => Self::Euclidean,
            "EUCLIDEAN_SQUARED" => Self::EuclideanSquared,
            "MANHATTAN" => Self::Manhattan,
            "COSINE" => Self::Cosine,
            "DOT" => Self::Dot,
            "HAMMING" => Self::Hamming,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Euclidean => "EUCLIDEAN",
            Self::EuclideanSquared => "EUCLIDEAN_SQUARED",
            Self::Manhattan => "MANHATTAN",
            Self::Cosine => "COSINE",
            Self::Dot => "DOT",
            Self::Hamming => "HAMMING",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LabelExpr {
    Label(String),
    Wildcard,
    Dynamic {
        expr: Box<Expr>,
        quantifier: DynamicLabelQuantifier,
    },
    Not(Box<LabelExpr>),
    And(Vec<LabelExpr>),
    Or(Vec<LabelExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicLabelQuantifier {
    Default,
    All,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantifierKind {
    All,
    Any,
    None,
    Single,
}

impl LabelExpr {
    /// Static conjunction: a single label or an `And`-chain of static labels.
    pub fn is_static_conjunction(&self) -> bool {
        match self {
            LabelExpr::Label(_) => true,
            // An empty conjunction is degenerate, not static — and it
            // is constructible: the parser never builds one, but
            // exec/create.rs's post-resolution synthesis can when a
            // dynamic label list resolves empty. Callers (node_ok's
            // label arm, label_index_seekable) must not trust a
            // construction invariant, so `And([])` declines here and
            // routes to the dynamic/scan arms instead (PR #359
            // F-2-6/F-3-2).
            LabelExpr::And(items) => {
                !items.is_empty() && items.iter().all(LabelExpr::is_static_conjunction)
            }
            _ => false,
        }
    }

    /// Static disjunction: a single label or an `Or`-chain of static labels.
    pub fn is_static_disjunction(&self) -> bool {
        match self {
            LabelExpr::Label(_) => true,
            // The degenerate-case twin of the `And` arm above: an empty
            // `Or` is constructible the same way and declines here so
            // no caller trusts a construction invariant (PR #359
            // F-3-1/F-3-3).
            LabelExpr::Or(items) => {
                !items.is_empty() && items.iter().all(LabelExpr::is_static_disjunction)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    Predefined {
        kind: PredefinedType,
        not_null: bool,
    },
    List {
        element: Box<TypeExpr>,
        not_null: bool,
    },
    PropertyValue {
        not_null: bool,
    },
    Union(Vec<TypeExpr>),
    Vector {
        coord: Option<String>,
        dimension: Option<i64>,
        not_null: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredefinedType {
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
    /// CIP-246.
    Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Plus,
    Minus,
    IsNull,
    IsNotNull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    Xor,
    And,
    Eq,
    Neq,
    Lt,
    Gt,
    Lte,
    Gte,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    In,
    StartsWith,
    EndsWith,
    Contains,
    Regex,
}

impl Projection {
    /// Column name of this projection item.
    ///
    /// Resolution order (matches Neo4j):
    /// 1. explicit `AS alias`
    /// 2. [`canonical_name`] — a bare variable (`RETURN n` → `n`) or
    ///    `v.key` property access (`RETURN n.name` → `n.name`), the two
    ///    names Neo4j spells identically regardless of source formatting
    /// 3. the verbatim source text captured by the parser (`RETURN 1+2`
    ///    → `1+2`, `RETURN count{()}` → `count{()}`); covers every other
    ///    expression shape without a parser-synthesized name
    /// 4. `column_<idx>` for synthetic post-parse projections that have
    ///    no `source_text`
    pub fn output_name(&self, idx: usize) -> String {
        if let Some(a) = &self.alias {
            return a.clone();
        }
        if let Some(name) = canonical_name(&self.expr) {
            return name;
        }
        if let Some(src) = &self.source_text {
            return src.clone();
        }
        format!("column_{idx}")
    }
}

/// The canonical names Neo4j spells identically regardless of source
/// formatting: a bare variable (`n`), `*`, and a `v.key` property
/// access. `None` for any other expression shape.
fn canonical_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Star => Some("*".into()),
        Expr::Variable(v) => Some(v.clone()),
        other => var_prop(other).map(|(v, key)| format!("{v}.{key}")),
    }
}

impl Expr {
    /// Fold a seek predicate into a value. Literals fold directly.
    /// Parameters resolve through `params`. `RuntimeValue` is already a
    /// value. `None` means the expression is not a seek constant.
    pub fn seek_value(
        &self,
        params: &std::collections::BTreeMap<String, cypherlite_core::Value>,
    ) -> Option<cypherlite_core::Value> {
        use cypherlite_core::Value;
        match self {
            Expr::Null => Some(Value::Null),
            Expr::Bool(value) => Some(Value::Bool(*value)),
            Expr::Int(value) => Some(Value::Int(*value)),
            Expr::Float(value) => Some(Value::Float(*value)),
            Expr::String(value) => Some(Value::String(std::sync::Arc::from(value.as_str()))),
            Expr::Parameter(name) => params.get(name).cloned(),
            Expr::RuntimeValue(value) => Some(value.clone()),
            _ => None,
        }
    }

    /// A literal or parameter that a planner may store on a pattern
    /// property map. `RuntimeValue` folds through [`seek_value`] but is
    /// not a planner seek constant.
    pub fn is_seek_const(&self) -> bool {
        match self {
            Expr::Parameter(_) => true,
            Expr::RuntimeValue(_) => false,
            other => other
                .seek_value(&std::collections::BTreeMap::new())
                .is_some(),
        }
    }
}
