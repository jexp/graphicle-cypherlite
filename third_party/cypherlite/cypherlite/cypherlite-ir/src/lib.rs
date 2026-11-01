mod aggregates;
mod ast;
mod recognize;
mod search;
mod walk;
mod walk_query;

pub use aggregates::{
    aggregate_arity, classify_aggregate, is_agg_expr, is_agg_fn, AggregateArity, AggregateKind,
    AGGREGATES, SINGLE_INPUT_AGGREGATES,
};
pub use ast::*;
// `var_prop` and `var_prop_owned` are re-exported by `pub use ast::*`
// above: they are pure `Expr`-shape recognizers and live in `ast`
// beside the type they recognize (one-way dependency, `recognize` ->
// `ast`).
pub use recognize::{key_of, orient, orient_side, var_key_side};
pub use search::*;
pub use walk::{expr_vars, label_expr_vars, path_pattern_vars};
pub use walk_query::{
    path_pattern_mentions, walk_clauses, walk_clauses_with_roles, walk_pattern_payloads,
    walk_query, ExprRole, PatternPayload,
};
