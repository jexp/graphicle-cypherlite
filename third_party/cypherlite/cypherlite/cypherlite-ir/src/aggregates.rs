//! The aggregate vocabulary and its arity/kind contracts (ADR-0217,
//! ADR-0225, LABS-276): one table, canonical lowercased names, with the
//! derived name lists and typed classifications. This is the single
//! definition for the whole workspace, living in the lowest layer that
//! needs aggregation detection; `cypherlite-front::agg` re-exports it (and
//! adds the CIP-128 alias/display spellings on top), and the planner and
//! executor match against it.

use crate::Expr;

/// The canonical aggregate function set: lowercase names (Cypher's
/// `count`/`sum`/`avg`/`min`/`max`/`collect`/`stDev`/`stDevP`/
/// `percentileCont`/`percentileDisc`, lowercased).
///
/// This is the single definition for the whole workspace: the names derive
/// from the [`AGGREGATE_CONTRACTS`] table (an aggregate without an arity
/// contract is unrepresentable), it lives in the lowest layer that needs
/// aggregation detection, `cypherlite-front::agg` re-exports it (and adds
/// the CIP-128 alias/display spellings on top), and the planner and
/// executor match against it. Matching is always done case-insensitively —
/// see [`is_agg_fn`].
pub const AGGREGATES: &[&str] = &aggregate_names();

/// The single-input aggregates (ADR-0225): each takes exactly one input
/// expression. The names derive from the [`AGGREGATE_CONTRACTS`] table —
/// the `SingleInput` class minus `count`, which the parser's count keyword
/// path already guarantees one argument or `CountStar` — so the class
/// cannot drift from the vocabulary. The single definition of this class
/// (LABS-276): `cypherlite-front` validates arity against it and the
/// `agg_eval` backstop in `cypherlite-executor` enforces the same
/// contract, so a future aggregate lands here once, in both layers.
pub const SINGLE_INPUT_AGGREGATES: &[&str] = &single_input_names();

/// The arity contract of an aggregate (ADR-0217): the class owns both the
/// exact argument count and the shape of the rejection, so callers build
/// the error from the contract instead of re-deriving it from the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateArity {
    /// Exactly one input expression (`count` and the
    /// [`SINGLE_INPUT_AGGREGATES`]): the rejection is the 42I13-shaped
    /// `InvalidNumberOfArguments` naming the signature.
    SingleInput,
    /// Exactly two arguments (`percentileCont`/`percentileDisc`): the
    /// rejection keeps the historical "percentile needs two arguments"
    /// message.
    PercentilePair,
}

impl AggregateArity {
    /// The exact argument count the class requires.
    pub fn required(self) -> usize {
        match self {
            AggregateArity::SingleInput => 1,
            AggregateArity::PercentilePair => 2,
        }
    }

    /// The rejection message for a call spelled `name` with `got`
    /// arguments.
    pub fn rejection(self, name: &str, got: usize) -> String {
        match self {
            AggregateArity::SingleInput => format!(
                "InvalidNumberOfArguments: {name}(input) requires exactly one argument, got {got}"
            ),
            AggregateArity::PercentilePair => "percentile needs two arguments".to_string(),
        }
    }
}

/// The typed kind of an aggregate (ADR-0217): the [`AGGREGATE_CONTRACTS`]
/// table row as a variant, so consumers match the class instead of
/// re-sniffing name strings — the `count`/`sum` literals live once,
/// beside the table. The arity contract derives from the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    StDev,
    StDevP,
    PercentileCont,
    PercentileDisc,
}

impl AggregateKind {
    /// The arity contract of the kind (ADR-0217). Every kind is spelled:
    /// a kind added later fails to compile here until its arity is decided.
    pub const fn arity(self) -> AggregateArity {
        match self {
            AggregateKind::Count
            | AggregateKind::Sum
            | AggregateKind::Avg
            | AggregateKind::Min
            | AggregateKind::Max
            | AggregateKind::Collect
            | AggregateKind::StDev
            | AggregateKind::StDevP => AggregateArity::SingleInput,
            AggregateKind::PercentileCont | AggregateKind::PercentileDisc => {
                AggregateArity::PercentilePair
            }
        }
    }
}

/// The aggregate vocabulary with its kind (ADR-0217): one table,
/// canonical lowercased names. [`AGGREGATES`] derives from it, so an
/// aggregate without a contract is unrepresentable.
const AGGREGATE_CONTRACTS: &[(&str, AggregateKind)] = &[
    ("count", AggregateKind::Count),
    ("sum", AggregateKind::Sum),
    ("avg", AggregateKind::Avg),
    ("min", AggregateKind::Min),
    ("max", AggregateKind::Max),
    ("collect", AggregateKind::Collect),
    ("stdev", AggregateKind::StDev),
    ("stdevp", AggregateKind::StDevP),
    ("percentilecont", AggregateKind::PercentileCont),
    ("percentiledisc", AggregateKind::PercentileDisc),
];

const AGGREGATE_NAMES_LEN: usize = AGGREGATE_CONTRACTS.len();

const fn aggregate_names() -> [&'static str; AGGREGATE_NAMES_LEN] {
    let mut names = [""; AGGREGATE_NAMES_LEN];
    let mut i = 0;
    while i < AGGREGATE_CONTRACTS.len() {
        names[i] = AGGREGATE_CONTRACTS[i].0;
        i += 1;
    }
    names
}

/// Const `str` equality (string-literal `PartialEq` is not const).
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether the table entry belongs to the front-checked single-input
/// class: `SingleInput` minus `count` (see [`SINGLE_INPUT_AGGREGATES`]).
const fn is_single_input_entry(entry: &(&str, AggregateKind)) -> bool {
    matches!(entry.1.arity(), AggregateArity::SingleInput) && !str_eq(entry.0, "count")
}

const fn single_input_names_len() -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < AGGREGATE_CONTRACTS.len() {
        if is_single_input_entry(&AGGREGATE_CONTRACTS[i]) {
            n += 1;
        }
        i += 1;
    }
    n
}

const SINGLE_INPUT_NAMES_LEN: usize = single_input_names_len();

const fn single_input_names() -> [&'static str; SINGLE_INPUT_NAMES_LEN] {
    let mut names = [""; SINGLE_INPUT_NAMES_LEN];
    let mut i = 0;
    let mut j = 0;
    while i < AGGREGATE_CONTRACTS.len() {
        if is_single_input_entry(&AGGREGATE_CONTRACTS[i]) {
            names[j] = AGGREGATE_CONTRACTS[i].0;
            j += 1;
        }
        i += 1;
    }
    names
}

/// The arity contract of an aggregate name, or `None` for a
/// non-aggregate. Matching is case-insensitive, mirroring [`is_agg_fn`].
/// This is the single home of aggregate arity (ADR-0217): the `agg_eval`
/// backstop in `cypherlite-executor` enforces it for every member of
/// [`AGGREGATES`], so a construction that bypasses front validation —
/// e.g. a backtick-escaped `` `count`() `` that slips past the parser's
/// count keyword path — is rejected before the row loop instead of
/// panicking on the missing argument or silently dropping extra ones.
pub fn aggregate_arity(name: &str) -> Option<AggregateArity> {
    classify_aggregate(name).map(AggregateKind::arity)
}

/// The kind of an aggregate name, or `None` for a non-aggregate. Matching
/// is case-insensitive, mirroring [`is_agg_fn`]. Consumers that need the
/// aggregate's class (not just its arity) match this typed variant
/// instead of re-sniffing the name string.
pub fn classify_aggregate(name: &str) -> Option<AggregateKind> {
    AGGREGATE_CONTRACTS
        .iter()
        .find(|(n, _)| name.eq_ignore_ascii_case(n))
        .map(|(_, kind)| *kind)
}

/// Aggregation function names (case-insensitive) — the bool derived from
/// the typed classification, one scan over the contracts table.
pub fn is_agg_fn(name: &str) -> bool {
    classify_aggregate(name).is_some()
}

/// Whether the expression contains an aggregation anywhere inside it.
pub fn is_agg_expr(expr: &Expr) -> bool {
    expr.any(&mut |e| {
        matches!(e, Expr::CountStar) || matches!(e, Expr::Function { name, .. } if is_agg_fn(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str) -> Expr {
        Expr::Variable(name.into())
    }

    // ADR-0217: the contracts table is the single home of the aggregate
    // vocabulary and its arity — AGGREGATES derives from it, and the
    // single-input class is the SingleInput contracts minus count (the
    // parser's keyword path keeps count out of the front check).
    #[test]
    fn aggregate_contracts_cover_the_vocabulary() {
        assert_eq!(AGGREGATES.len(), AGGREGATE_CONTRACTS.len());
        for (name, kind) in AGGREGATE_CONTRACTS {
            assert!(AGGREGATES.contains(name), "{name} in AGGREGATES");
            assert_eq!(classify_aggregate(name), Some(*kind), "{name} kind");
            assert_eq!(aggregate_arity(name), Some(kind.arity()), "{name} arity");
            // Matching is case-insensitive, mirroring is_agg_fn.
            assert_eq!(
                classify_aggregate(&name.to_ascii_uppercase()),
                Some(*kind),
                "{name} kind, uppercased"
            );
            let single_input = kind.arity() == AggregateArity::SingleInput && *name != "count";
            assert_eq!(
                SINGLE_INPUT_AGGREGATES.contains(name),
                single_input,
                "{name} in SINGLE_INPUT_AGGREGATES"
            );
        }
        assert_eq!(classify_aggregate("not_an_aggregate"), None);
        assert_eq!(aggregate_arity("not_an_aggregate"), None);
    }

    #[test]
    fn agg_detection_is_uniform() {
        assert!(is_agg_fn("count"));
        assert!(is_agg_fn("COUNT"));
        assert!(is_agg_fn("percentileCont"));
        assert!(!is_agg_fn("abs"));
        let expr = Expr::Binary {
            op: crate::BinaryOp::Add,
            left: Box::new(Expr::Function {
                name: "sum".into(),
                distinct: false,
                args: vec![var("n")],
            }),
            right: Box::new(Expr::Int(1)),
        };
        assert!(is_agg_expr(&expr));
        assert!(!is_agg_expr(&var("n")));
        assert!(is_agg_expr(&Expr::CountStar));
    }
}
