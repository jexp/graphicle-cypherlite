//! Query notifications from a built plan tree (ADR-0363): the operator-scoped
//! kinds derived from the tree's pattern, operator and estimate facts, in the
//! tree's pre-order (the `Id` column's order), with identical
//! `(kind, description)` entries collapsed to the first.
//!
//! Derivation runs only where a `PlanNode` tree is built — the `EXPLAIN` and
//! `PROFILE` result builders ([`crate::explain`]) — never on the plain
//! execution path (ADR-0363 decision 3). The three `PlanWarning` kinds
//! project from their per-node warnings; `UnboundedVariableLengthPattern`
//! and `MissingIndex` are derived here beside them and never join the plan
//! table's `Warnings` column (decision 8). `EagerAggregation` is staged at
//! tree construction, where the typed grouping fact and the input estimate
//! are both in hand ([`eager_aggregation_notification`]). The
//! statement-scoped kinds are detected in
//! [`crate::statement_notifications`]; both halves append through the one
//! identity/order policy in [`crate::notifications`].

use cypherlite_ir::{walk_pattern_payloads, Expr, PathPattern, PatternPayload, RelPattern};
use cypherlite_model::wire::{Notification, NotificationKind};
use cypherlite_parser::token::quote_identifier;

use super::PlanWarning;
use crate::notifications::dedup_identical;
use crate::stats::PlanStats;

/// The eager-aggregation input threshold (ADR-0363 decision 1): an
/// `EagerAggregation` whose input child's estimate reaches this many rows
/// gets a notification. The one comparison against it lives in
/// [`eager_aggregation_notification`].
const EAGER_INPUT_NOTIFICATION_ROWS: u64 = 100_000;

/// The operator-scoped notifications of a built plan tree (ADR-0363
/// decision 6): the typed warnings and the pattern/estimate-derived kinds in
/// pre-order, each carrying its node's `id` as `operator_id`, with identical
/// `(kind, description)` pairs collapsed to the first.
///
/// The derivation is idempotent: it reads the tree and the side map the
/// construction walk staged, so deriving twice from the same [`PlanTree`]
/// yields the same list (a handed-out EXPLAIN tree never changes what a
/// later read returns).
///
/// [`PlanTree`]: super::PlanTree
pub(crate) fn tree_notifications(tree: &super::PlanTree) -> Vec<Notification> {
    let mut notifications = Vec::new();
    collect(&tree.root, tree, &mut notifications);
    dedup_identical(notifications)
}

/// The pre-order walk behind [`tree_notifications`]: the node's own warnings
/// and staged notifications before its children's, matching the `Id`
/// column's numbering.
fn collect(node: &super::PlanNode, tree: &super::PlanTree, out: &mut Vec<Notification>) {
    out.extend(
        node.warnings
            .iter()
            .map(|warning| projection(warning, node.id)),
    );
    out.extend(
        tree.staged(node.id)
            .iter()
            .cloned()
            .map(|notification| notification.with_operator_id(node.id)),
    );
    for child in &node.children {
        collect(child, tree, out);
    }
}

/// The `MATCH` pattern notifications that are not plan warnings (ADR-0363
/// decision 4): an unbounded variable-length expansion, and a label/property
/// equality with no range index serving it. Both attach to the MATCH
/// clause's own operator (the `Expand` for a pattern with relationships, the
/// scan for a lone node).
pub(super) fn match_notifications(
    patterns: &[PathPattern],
    stats: &dyn PlanStats,
) -> Vec<Notification> {
    let mut notifications = unbounded_expansion_notifications(patterns);
    notifications.extend(missing_index_notifications(patterns, stats));
    notifications
}

/// One `UnboundedVariableLengthPattern` per pattern whose relationship or
/// quantified group has no upper bound. The description names the pattern
/// text (the `Expand` subject); the kind's canonical remedy asks for a
/// bound or a selective predicate plus `LIMIT`.
fn unbounded_expansion_notifications(patterns: &[PathPattern]) -> Vec<Notification> {
    patterns
        .iter()
        .filter(|pattern| pattern_has_unbounded_expansion(pattern))
        .map(|pattern| {
            Notification::new(
                NotificationKind::UnboundedVariableLengthPattern,
                format!(
                    "the variable-length pattern {} has no upper bound",
                    super::subject::pattern_text(pattern)
                ),
            )
        })
        .collect()
}

/// Whether any relationship or quantified group in the pattern (compact or
/// element form, nested groups included) is an unbounded expansion. The IR
/// visitor owns the post-rewrite view precedence and the group recursion.
fn pattern_has_unbounded_expansion(pattern: &PathPattern) -> bool {
    let mut unbounded = false;
    walk_pattern_payloads(pattern, &mut |payload| match payload {
        PatternPayload::Relationship(rel) => unbounded |= rel_is_unbounded(rel),
        PatternPayload::Quantified(quantified) => unbounded |= quantified.max.is_none(),
        PatternPayload::Node(_) => {}
    });
    unbounded
}

/// A variable-length relationship pattern with an open upper bound. A plain
/// relationship (`var_length`/`quantified` false) carries no quantifier, and
/// a bounded `[*1..3]` (`max_hops: Some(3)`) is bounded.
fn rel_is_unbounded(rel: &RelPattern) -> bool {
    (rel.var_length || rel.quantified) && rel.max_hops.is_none()
}

/// One `MissingIndex` per equality-predicated `(label, property)` pair that
/// no range index serves. The description names the label and property; the
/// instance remedy is the exact runnable `CREATE INDEX` statement (the ADR
/// table's remedy shape, with a deterministic name).
fn missing_index_notifications(
    patterns: &[PathPattern],
    stats: &dyn PlanStats,
) -> Vec<Notification> {
    let indexed = stats.range_indexed_properties();
    let mut notifications = Vec::new();
    for pattern in patterns {
        walk_pattern_payloads(pattern, &mut |payload| {
            let PatternPayload::Node(node) = payload else {
                return;
            };
            let Some(label) = super::subject::single_static_label(node) else {
                return;
            };
            let Some(Expr::Map(pairs)) = node.properties.as_ref() else {
                return;
            };
            for (property, _) in pairs {
                if indexed
                    .iter()
                    .any(|indexed| indexed.label == label && &indexed.property == property)
                {
                    continue;
                }
                notifications.push(
                    Notification::new(
                        NotificationKind::MissingIndex,
                        format!(
                            "the equality predicate on {property} filters :{label} \
                             without a range index on (:{label} {{{property}}})"
                        ),
                    )
                    .with_remedy(create_index_statement(label, property)),
                );
            }
        });
    }
    notifications
}

/// The runnable `CREATE INDEX` remedy for one missing `(label, property)`
/// pair: the deterministic index name from [`index_name`], and the
/// label/property rendered by the parser's one identifier-quoting policy
/// ([`quote_identifier`]): bare when plain and non-keyword, else
/// backtick-quoted with embedded backticks doubled. The keyword arm matters
/// here: `CREATE INDEX ... ON (n.<prop>)` reads the property name through
/// `is_ident_like()`, so a bare clause-keyword property (`n.order`) would not
/// re-parse.
fn create_index_statement(label: &str, property: &str) -> String {
    format!(
        "CREATE INDEX {} FOR (n:{}) ON (n.{})",
        index_name(label, property),
        quote_identifier(label),
        quote_identifier(property)
    )
}

/// The deterministic index name `cypherlite_<label>__<property>`, injective
/// for every distinct `(label, property)` pair because index names are
/// global: case is preserved (so `Person` and `person` stay apart) and each
/// part escapes every byte:
///
/// - an ASCII alphanumeric byte stays as-is;
/// - `_` becomes `_5f`;
/// - every other byte becomes `_` plus its two lowercase hex digits
///   (`-` → `_2d`, a space → `_20`, each byte of a non-ASCII character its
///   own `_xx`).
///
/// This is decodable and the **double**-underscore separator keeps it
/// injective. An escaped part never contains `__`: every `_` in a part
/// starts an escape and is followed by exactly two hex digits, so two
/// underscores can never be adjacent (the part is alphanumerics plus
/// three-character `_hh` escapes, and no part can end in `_`). After the
/// fixed `cypherlite_` prefix, the first `__` is therefore the separator;
/// split there and each side decodes uniquely (an `_` is always followed by
/// exactly two lowercase hex digits). Distinct pairs thus cannot fold:
/// `(A_, x)` → `cypherlite_A_5f__x`, `(A, _x)` →
/// `cypherlite_A___5fx`, `(A, B_x)` → `cypherlite_A__B_5fx`, `(A_B, x)` →
/// `cypherlite_A_5fB__x`, and `(Person, email)` →
/// `cypherlite_Person__email`, with `(person, email)` staying distinct. The
/// fixed prefix keeps the name a legal unquoted identifier whatever the
/// label starts with.
fn index_name(label: &str, property: &str) -> String {
    let part = |text: &str| {
        let mut escaped = String::with_capacity(text.len());
        for byte in text.bytes() {
            if byte.is_ascii_alphanumeric() {
                escaped.push(char::from(byte));
            } else if byte == b'_' {
                escaped.push_str("_5f");
            } else {
                escaped.push_str(&format!("_{byte:02x}"));
            }
        }
        escaped
    };
    format!("cypherlite_{}__{}", part(label), part(property))
}

/// The `EagerAggregation` notification (ADR-0363 decision 4): an aggregation
/// operator whose input child's estimate reaches
/// [`EAGER_INPUT_NOTIFICATION_ROWS`]. The description names the threshold and
/// the input estimate. `None` for an input with no estimate or an input below
/// the threshold. Tree construction calls this where the typed grouping fact
/// ([`super::ClauseOperator::EagerAggregation`]) and the child estimate are in
/// hand, so the decision never compares rendered operator names.
pub(super) fn eager_aggregation_notification(
    input_estimate: Option<u64>,
    operator_id: usize,
) -> Option<Notification> {
    let input = input_estimate?;
    if input < EAGER_INPUT_NOTIFICATION_ROWS {
        return None;
    }
    Some(
        Notification::new(
            NotificationKind::EagerAggregation,
            format!(
                "the aggregation buffers an estimated {input} input rows, at or above the \
                 {EAGER_INPUT_NOTIFICATION_ROWS}-row eager threshold"
            ),
        )
        .with_operator_id(operator_id),
    )
}

/// One typed plan warning's notification (ADR-0363 decision 1): the
/// warning's kind is its vocabulary row ([`PlanWarning::notification_kind`],
/// the one exhaustive variant-to-kind mapping), its message is the instance
/// description, and the raising node's id is the operator association.
fn projection(warning: &PlanWarning, operator_id: usize) -> Notification {
    Notification::new(warning.notification_kind(), warning.message()).with_operator_id(operator_id)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cypherlite_ir::PatternElement;
    use cypherlite_model::wire::NotificationKind;

    use super::super::fixtures::{compact, node, rel};
    use super::super::{PlanNode, PlanTree};
    use super::*;

    /// A hand-built tree with no staged clause facts.
    fn tree(root: PlanNode) -> PlanTree {
        PlanTree::from_parts(root, BTreeMap::new())
    }

    /// A warning-carrying node with a caller-chosen id, no children.
    fn warned(id: usize, operator: &str, warnings: Vec<PlanWarning>) -> PlanNode {
        let mut node =
            PlanNode::new(operator, String::new(), Vec::new(), Vec::new()).with_warnings(warnings);
        node.id = id;
        node
    }

    /// The three plan-warning variants project to their vocabulary rows in
    /// pre-order, each naming its operator, and identical pairs collapse.
    #[test]
    fn warnings_project_in_pre_order_and_dedup_identical_pairs() {
        let mut root = PlanNode::new(
            "ProduceResults",
            String::new(),
            Vec::new(),
            vec![warned(
                1,
                "AllNodesScan",
                vec![PlanWarning::AllNodesScan {
                    index: ":Person(name)".to_string(),
                }],
            )],
        )
        .with_warnings(vec![PlanWarning::CartesianProduct]);
        root.id = 0;
        let notifications = tree_notifications(&tree(root));
        assert_eq!(
            notifications
                .iter()
                .map(|n| (n.kind(), n.operator_id()))
                .collect::<Vec<_>>(),
            [
                (NotificationKind::CartesianProduct, Some(0)),
                (NotificationKind::AllNodesScan, Some(1)),
            ]
        );
        assert_eq!(
            notifications[1].description(),
            "the scan matches every node; consider the property index (:Person(name))"
        );

        // Two nodes carrying the same warning: one entry, the first node's
        // operator id.
        let duplicated = PlanNode::new(
            "Union",
            String::new(),
            Vec::new(),
            vec![
                warned(
                    1,
                    "AllNodesScan",
                    vec![PlanWarning::AllNodesScan {
                        index: ":Person(name)".to_string(),
                    }],
                ),
                warned(
                    2,
                    "AllNodesScan",
                    vec![PlanWarning::AllNodesScan {
                        index: ":Person(name)".to_string(),
                    }],
                ),
            ],
        );
        let deduped = tree_notifications(&tree(duplicated));
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].operator_id(), Some(1));
    }

    /// `EstimationMiss` is a `PROFILE` fact: a tree without it — an
    /// EXPLAIN tree — derives no notification for it, so the same statement
    /// yields the miss only where actual rows exist.
    #[test]
    fn estimation_miss_projects_only_when_the_profiled_tree_carries_it() {
        let explained = warned(2, "NodeByLabelScan", Vec::new());
        assert!(tree_notifications(&tree(explained)).is_empty());
        let profiled = warned(2, "NodeByLabelScan", vec![PlanWarning::EstimationMiss]);
        assert_eq!(
            tree_notifications(&tree(profiled))
                .iter()
                .map(|n| n.kind())
                .collect::<Vec<_>>(),
            [NotificationKind::EstimationMiss]
        );
    }

    /// The unbounded detector reads the relationship quantifier, not the
    /// pattern text: `[*]`/`[*1..]` fire, a bounded `[*1..3]` does not, and a
    /// quantified group with an open `max` fires too.
    #[test]
    fn unbounded_detection_reads_the_quantifier_bounds() {
        let unbounded = compact(
            vec![node(Some("a"), &["A"]), node(Some("b"), &["B"])],
            vec![rel(&["R"], 1, None)],
        );
        let mut open = rel(&["R"], 1, None);
        open.var_length = true;
        let open = compact(
            vec![node(Some("a"), &["A"]), node(Some("b"), &["B"])],
            vec![open],
        );
        let bounded = compact(
            vec![node(Some("a"), &["A"]), node(Some("b"), &["B"])],
            vec![rel(&["R"], 1, Some(3))],
        );
        // A plain relationship carries no quantifier.
        let plain = compact(
            vec![node(Some("a"), &["A"]), node(Some("b"), &["B"])],
            vec![rel(&["R"], 1, None)],
        );
        assert!(!pattern_has_unbounded_expansion(&unbounded));
        assert!(pattern_has_unbounded_expansion(&open));
        assert!(!pattern_has_unbounded_expansion(&bounded));
        assert!(!pattern_has_unbounded_expansion(&plain));
        let patterns = unbounded_expansion_notifications(&[open, bounded]);
        assert_eq!(patterns.len(), 1);
        assert_eq!(
            patterns[0].kind(),
            NotificationKind::UnboundedVariableLengthPattern
        );
        assert!(
            patterns[0].description().contains("(a:A)-[:R*1..]->(b:B)"),
            "the description names the pattern: {}",
            patterns[0].description()
        );

        // A quantified path group with no upper bound fires; a bounded one
        // does not.
        let group = |max: Option<u32>| PathPattern {
            variable: None,
            elements: vec![PatternElement::Quantified(
                cypherlite_ir::QuantifiedPathPattern {
                    subpath_variable: None,
                    elements: Vec::new(),
                    where_clause: None,
                    min: 1,
                    max,
                },
            )],
            nodes: Vec::new(),
            rels: Vec::new(),
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        assert!(pattern_has_unbounded_expansion(&group(None)));
        assert!(!pattern_has_unbounded_expansion(&group(Some(4))));
    }

    /// The missing-index remedy's name is the deterministic
    /// `cypherlite_<label>__<property>` with case preserved and every byte
    /// escaped injectively (`_` → `_5f`, any other non-alphanumeric byte →
    /// `_` plus two hex digits), so distinct pairs keep distinct names: the
    /// old `_`-fold collapsed `A-B`/`A_B`, and the old single-`_` separator
    /// collapsed `(A_, x)`/`(A, _x)`. An escaped part never contains `__`,
    /// so the first `__` after the prefix is the separator. Unusual or
    /// keyword identifiers are backtick-quoted so the statement re-parses.
    #[test]
    fn missing_index_remedy_names_are_deterministic_and_runnable() {
        assert_eq!(index_name("Person", "name"), "cypherlite_Person__name");
        assert_eq!(index_name("person", "name"), "cypherlite_person__name");
        assert_eq!(
            index_name("Order Item", "ship-date"),
            "cypherlite_Order_20Item__ship_2ddate"
        );
        assert_eq!(
            index_name("Order_Item", "ship_date"),
            "cypherlite_Order_5fItem__ship_5fdate"
        );
        assert_ne!(
            index_name("A-B", "x"),
            index_name("A_B", "x"),
            "a hyphen and an underscore must not fold to the same byte"
        );
        assert_ne!(
            index_name("Order Item", "ship-date"),
            index_name("Order_Item", "ship_date")
        );
        // The separator boundary: an escaped `_` (`_5f`) is not the
        // separator and cannot be confused with one, in either part.
        assert_eq!(index_name("A_", "x"), "cypherlite_A_5f__x");
        assert_eq!(index_name("A", "_x"), "cypherlite_A___5fx");
        assert_ne!(
            index_name("A_", "x"),
            index_name("A", "_x"),
            "a trailing label underscore and a leading property underscore must not fold"
        );
        assert_eq!(index_name("A", "B_x"), "cypherlite_A__B_5fx");
        assert_eq!(index_name("A_B", "x"), "cypherlite_A_5fB__x");
        assert_ne!(
            index_name("A", "B_x"),
            index_name("A_B", "x"),
            "an escaped underscore in either part must not shift the separator"
        );
        assert_eq!(
            create_index_statement("Person", "name"),
            "CREATE INDEX cypherlite_Person__name FOR (n:Person) ON (n.name)"
        );
        assert_eq!(
            create_index_statement("Order Item", "ship-date"),
            "CREATE INDEX cypherlite_Order_20Item__ship_2ddate \
             FOR (n:`Order Item`) ON (n.`ship-date`)"
        );
        assert_eq!(
            create_index_statement("Person", "order"),
            "CREATE INDEX cypherlite_Person__order FOR (n:Person) ON (n.`order`)",
            "a clause-keyword property is quoted: `ON (n.order)` would not re-parse"
        );
        assert_eq!(
            create_index_statement("we`ird", "kw`d"),
            "CREATE INDEX cypherlite_we_60ird__kw_60d FOR (n:`we``ird`) ON (n.`kw``d`)",
            "an embedded backtick is doubled in the identifier and hex-escaped in the name"
        );
    }

    /// The eager-aggregation threshold with a stub `PlanStats` source: a
    /// MATCH feeding an aggregating RETURN notifies at
    /// [`EAGER_INPUT_NOTIFICATION_ROWS`] and not one row below it.
    #[test]
    fn eager_aggregation_fires_at_the_threshold_and_not_one_row_below() {
        for (rows, expected) in [
            (EAGER_INPUT_NOTIFICATION_ROWS, true),
            (EAGER_INPUT_NOTIFICATION_ROWS - 1, false),
        ] {
            let plan = aggregating_plan();
            let stats = LabelStats { rows };
            let built = super::super::plan_tree(&plan, &stats);
            let notifications = tree_notifications(&built);
            let eager: Vec<&Notification> = notifications
                .iter()
                .filter(|n| n.kind() == NotificationKind::EagerAggregation)
                .collect();
            assert_eq!(
                eager.len(),
                usize::from(expected),
                "rows {rows}: notifications {notifications:?}"
            );
            if expected {
                let aggregation = built
                    .root
                    .children
                    .first()
                    .expect("the aggregation is the root, MATCH its child");
                assert_eq!(
                    built.root.operator, "EagerAggregation",
                    "the tree's root is the aggregating operator"
                );
                assert_eq!(eager[0].operator_id(), Some(built.root.id));
                assert_eq!(
                    eager[0].description(),
                    "the aggregation buffers an estimated 100000 input rows, at or above the \
                     100000-row eager threshold"
                );
                assert!(!eager[0].remedy().is_empty());
                assert!(
                    aggregation.estimated_rows.is_some(),
                    "the stub stats supplied the child estimate"
                );
            }
        }
    }

    /// The eager notification is driven by the typed grouping fact stamped
    /// at construction, never by the rendered operator name: a hand-built
    /// tree whose node merely renders `EagerAggregation` — with a
    /// threshold-crossing child estimate — carries no such fact and derives
    /// no notification.
    #[test]
    fn eager_notification_is_not_driven_by_the_rendered_operator_name() {
        let scan = PlanNode::new("AllNodesScan", String::new(), Vec::new(), Vec::new())
            .with_estimated_rows(Some(EAGER_INPUT_NOTIFICATION_ROWS));
        let root = PlanNode::new("EagerAggregation", String::new(), Vec::new(), vec![scan]);
        assert!(
            tree_notifications(&tree(root))
                .iter()
                .all(|notification| notification.kind() != NotificationKind::EagerAggregation),
            "the rendered name is a display detail, not the aggregation fact"
        );
    }

    /// A MATCH scanning `:Person` feeding `RETURN count(*)` — the tree shape
    /// whose input estimate the eager threshold reads.
    fn aggregating_plan() -> cypherlite_planner::LogicalPlan {
        use cypherlite_ir::{Clause, Expr, MatchMode, Projection};
        cypherlite_planner::LogicalPlan {
            parts: vec![vec![
                Clause::Match {
                    optional: false,
                    match_mode: MatchMode::DifferentRelationships,
                    patterns: vec![compact(vec![node(Some("n"), &["Person"])], Vec::new())],
                    where_clause: None,
                    search: None,
                },
                Clause::Return {
                    distinct: false,
                    group_by: None,
                    items: vec![Projection {
                        expr: Expr::CountStar,
                        alias: None,
                        source_text: None,
                    }],
                    order: Vec::new(),
                    skip: None,
                    limit: None,
                },
            ]],
            union_all: Vec::new(),
            next: None,
            cypher_version: None,
            use_target: None,
        }
    }

    /// Stats stub: one label count, no indexes.
    struct LabelStats {
        rows: u64,
    }

    impl PlanStats for LabelStats {
        fn property_index_texts(&self) -> Vec<String> {
            Vec::new()
        }
        fn range_indexed_properties(&self) -> Vec<crate::stats::RangeIndexedProperty> {
            Vec::new()
        }
    }

    impl cypherlite_planner::PatternStats for LabelStats {
        fn label_rows(&self, label: &str) -> Option<u64> {
            (label == "Person").then_some(self.rows)
        }
        fn equality_rows(&self, _: &str, _: &str, _: &cypherlite_ir::Expr) -> Option<u64> {
            None
        }
        fn contains_rows(&self, _: &str, _: &str, _: &cypherlite_ir::Expr) -> Option<u64> {
            None
        }
        fn rel_type_rows(&self, _: &str) -> Option<u64> {
            None
        }
    }

    /// The derivation is idempotent: it reads the staged facts beside the
    /// tree, so a second read of the same handed-out `PlanTree` returns the
    /// same list with the same operator association.
    #[test]
    fn deriving_twice_yields_the_same_list() {
        let mut root = PlanNode::new("NodeByLabelScan", String::new(), Vec::new(), Vec::new());
        root.id = 3;
        let mut staged = BTreeMap::new();
        staged.insert(
            3,
            vec![Notification::new(
                NotificationKind::MissingIndex,
                "the equality predicate on name filters :Person without a range index",
            )],
        );
        let tree = PlanTree::from_parts(root, staged);

        let first = tree_notifications(&tree);
        let second = tree_notifications(&tree);

        assert_eq!(first, second, "a second derivation reads the same facts");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].operator_id(), Some(3));
        assert!(
            !tree.staged(3).is_empty(),
            "the walk reads the staged facts instead of consuming them"
        );
    }

    /// The dedup identity is a hash set, so a tree carrying 50,000 distinct
    /// unknown names — with every entry duplicated — deduplicates linearly
    /// and returns exactly the distinct list.
    #[test]
    fn fifty_thousand_distinct_notifications_dedup_to_the_distinct_set() {
        let distinct = 50_000usize;
        let notification = |index: usize| {
            Notification::new(
                NotificationKind::UnknownPropertyKeyWarning,
                format!("The property key `k{index}` does not exist in this database."),
            )
        };
        let root = PlanNode::new("Union", String::new(), Vec::new(), Vec::new());
        let staged = BTreeMap::from([(
            0,
            (0..distinct)
                .map(notification)
                .chain((0..distinct).map(notification))
                .collect(),
        )]);
        let tree = PlanTree::from_parts(root, staged);

        let notifications = tree_notifications(&tree);
        assert_eq!(
            notifications.len(),
            distinct,
            "one entry per distinct (kind, description) pair"
        );
        assert_eq!(
            notifications,
            tree_notifications(&tree),
            "the derivation is idempotent"
        );
    }
}
