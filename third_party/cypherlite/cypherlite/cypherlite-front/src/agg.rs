//! Aggregation name aliases.
//!
//! The canonical aggregate function set (`AGGREGATES`) is owned by
//! `cypherlite-ir` — the lowest layer that needs aggregation detection —
//! and re-exported here so every consumer shares one definition. This
//! module adds the aggregate alias table (`AGGREGATE_ALIASES`) on top.
//! Consumers: the front validators (`validate`), the lowering rename in
//! `lower.rs` (which emits the display spelling), the planner, and the
//! executor's aggregation machinery / `SHOW FUNCTIONS` catalog.

pub use cypherlite_ir::{AGGREGATES, SINGLE_INPUT_AGGREGATES};

use crate::ast::tree::walk_expr_with;
use cypherlite_parser::Expr;

/// Aggregate alias table (CIP-128's snake_case GQL/SQL spellings): each
/// entry maps an accepted alternate lowercase spelling to its canonical
/// lowercase name and to the canonical *display* spelling the lowering
/// emits into IR (so an unaliased `RETURN percentile_cont(x)` keeps the
/// Cypher-canonical column name `percentileCont(x)`).
pub const AGGREGATE_ALIASES: &[(&str, &str, &str)] = &[
    ("collect_list", "collect", "collect"),
    ("stdev_samp", "stdev", "stDev"),
    ("stddev_samp", "stdev", "stDev"),
    ("stdev_pop", "stdevp", "stDevP"),
    ("stddev_pop", "stdevp", "stDevP"),
    ("percentile_cont", "percentilecont", "percentileCont"),
    ("percentile_disc", "percentiledisc", "percentileDisc"),
];

/// The canonical lowercase aggregate name for an accepted lowercase
/// spelling, or the input unchanged when it is not an alias.
pub fn canonical_agg_name(name: &str) -> &str {
    for (alias, canonical, _) in AGGREGATE_ALIASES {
        if *alias == name {
            return canonical;
        }
    }
    name
}

/// The display spelling (IR name) for an accepted lowercase aggregate
/// alias, or `None` when `name` is not an alias.
pub fn display_agg_alias(name: &str) -> Option<&'static str> {
    AGGREGATE_ALIASES
        .iter()
        .find(|(alias, _, _)| *alias == name)
        .map(|(_, _, display)| *display)
}

/// True when `name` (any casing, any accepted spelling) is an aggregate
/// function.
pub fn is_aggregate_name(name: &str) -> bool {
    AGGREGATES.contains(&canonical_agg_name(&name.to_lowercase()))
}

/// Whether an expression contains an aggregation function call, through the
/// historical reach: function arguments, binary/unary/comparison operands,
/// parentheses and property values, plus the postfix base of a subscript or
/// slice. Aggregates inside list literals, CASE arms, IN operands,
/// comprehensions or dynamic keys are not "contained" (the consumers that
/// must look there keep their own traversal; see
/// [`crate::validate::aggregation`]).
pub(crate) fn contains_aggregation(expr: &Expr) -> bool {
    let mut found = false;
    let mut visit = |node: &Expr| {
        if matches!(node, Expr::CountStar)
            || matches!(node, Expr::FunctionCall(fc) if is_aggregate_name(&fc.name))
        {
            found = true;
        }
        Ok(())
    };
    let enter_child = |parent: &Expr, child: &Expr| match parent {
        Expr::Property(p) => std::ptr::eq(child, &p.expr),
        Expr::Subscript(s) => std::ptr::eq(child, &s.expr),
        Expr::FunctionCall(_)
        | Expr::Binary(_)
        | Expr::Unary(_)
        | Expr::Paren(_)
        | Expr::Comparison(_) => true,
        // CIP-261: embeds are full expressions and must be walked.
        Expr::StringInterp(_) => true,
        _ => false,
    };
    // Infallible for the same reason as `collect_expr_variables`.
    let result = walk_expr_with(expr, &mut visit, enter_child);
    debug_assert!(
        result.is_ok(),
        "contains_aggregation walked a tree that already passed the depth cap"
    );
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_canonical_names() {
        assert_eq!(canonical_agg_name("percentile_cont"), "percentilecont");
        assert_eq!(canonical_agg_name("stddev_samp"), "stdev");
        assert_eq!(canonical_agg_name("collect_list"), "collect");
        assert_eq!(canonical_agg_name("count"), "count");
        assert_eq!(canonical_agg_name("abs"), "abs");
    }

    #[test]
    fn display_spellings_keep_cypher_casing() {
        assert_eq!(display_agg_alias("percentile_cont"), Some("percentileCont"));
        assert_eq!(display_agg_alias("stddev_pop"), Some("stDevP"));
        assert_eq!(display_agg_alias("count"), None);
    }

    #[test]
    fn aggregate_name_detection_covers_every_spelling() {
        for name in [
            "count",
            "SUM",
            "stDev",
            "stdev",
            "stdevp",
            "stDevP",
            "percentileCont",
            "percentilecont",
            "percentile_disc",
            "collect_list",
        ] {
            assert!(is_aggregate_name(name), "{name} must be aggregate");
        }
        assert!(!is_aggregate_name("abs"));
    }

    #[test]
    fn alias_targets_are_members_of_the_canonical_set() {
        // The alias table may only point at members of the canonical set
        // (re-exported from `cypherlite-ir`), and each display spelling must
        // be exactly its canonical target re-cased.
        for (alias, canonical, display) in AGGREGATE_ALIASES {
            assert!(
                AGGREGATES.contains(canonical),
                "alias {alias} targets {canonical}, which is not in the canonical set"
            );
            assert_eq!(
                display.to_lowercase(),
                *canonical,
                "alias {alias} display spelling {display} drifted from {canonical}"
            );
            assert!(
                !AGGREGATES.contains(alias),
                "alias {alias} collides with a canonical name"
            );
            assert!(is_aggregate_name(alias), "alias {alias} must match");
        }
    }

    #[test]
    fn ir_and_front_matchers_recognize_exactly_the_canonical_set() {
        use std::collections::BTreeSet;
        // Set-equality guard between the ir-owned canonical set and the two
        // matchers built on it. Probe pool: every canonical name, every
        // alias spelling, mixed-case variants, and near-miss non-aggregates.
        let probes = [
            "count",
            "sum",
            "avg",
            "min",
            "max",
            "collect",
            "stdev",
            "stdevp",
            "percentilecont",
            "percentiledisc",
            "COUNT",
            "stDev",
            "stDevP",
            "percentileCont",
            "percentileDisc",
            "collect_list",
            "stdev_samp",
            "stddev_samp",
            "stdev_pop",
            "stddev_pop",
            "percentile_cont",
            "percentile_disc",
            "abs",
            "rand",
            "stdevx",
            "percentile",
            "percentiledis",
            "collects",
        ];
        let canonical: BTreeSet<&str> = AGGREGATES.iter().copied().collect();
        // Pin the exact membership, so perturbing the ir-owned definition
        // itself (not just a matcher built on it) fails this test.
        let expected: BTreeSet<&str> = [
            "avg",
            "collect",
            "count",
            "max",
            "min",
            "percentilecont",
            "percentiledisc",
            "stdev",
            "stdevp",
            "sum",
        ]
        .into_iter()
        .collect();
        assert_eq!(canonical, expected, "AGGREGATES membership changed");
        // The ir matcher recognizes exactly the canonical set — the CIP-128
        // alias spellings are a front-layer concern and must NOT match here.
        let ir_hits: BTreeSet<String> = probes
            .iter()
            .filter(|n| cypherlite_ir::is_agg_fn(n))
            .map(|n| n.to_lowercase())
            .collect();
        assert_eq!(
            ir_hits,
            canonical.iter().map(|n| n.to_string()).collect(),
            "cypherlite_ir::is_agg_fn drifted from the canonical set"
        );
        // The front matcher recognizes the canonical set plus the alias
        // spellings, resolving to exactly the canonical set.
        let front_hits: BTreeSet<String> = probes
            .iter()
            .filter(|n| is_aggregate_name(n))
            .map(|n| canonical_agg_name(&n.to_lowercase()).to_string())
            .collect();
        assert_eq!(
            front_hits,
            canonical.iter().map(|n| n.to_string()).collect(),
            "front matcher resolved to names outside the canonical set"
        );
    }
}
