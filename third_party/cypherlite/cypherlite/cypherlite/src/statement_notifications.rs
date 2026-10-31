//! Statement-scoped query notifications (ADR-0363 decisions 3, 5, 6 and 13).
//!
//! This module owns the kinds that have no operator to hang on: an unknown
//! label, relationship type or property key, a deprecated function or
//! procedure, and the scope-less `CALL { … }` subquery. Derivation runs only
//! where a plan tree is built — [`crate::explain::explain_result`] and
//! [`crate::explain::profile_result`] — and [`merge`] joins the result with
//! the tree's operator-scoped entries in the ADR-0363 decision 6 order:
//!
//! 1. the tree's pre-order entries (the `Id` column's order);
//! 2. statement-scoped entries by category (`UNRECOGNIZED` before
//!    `DEPRECATION`), then code, then description;
//! 3. identical `(kind, description)` pairs collapse to the first, through
//!    the one [`crate::notifications::dedup_identical`] helper.
//!
//! Unknown-name evidence is **read-side only** (ADR-0363 decision 13): the
//! labels, relationship types and pattern property keys of `MATCH` patterns
//! (including patterns nested in pattern predicates, comprehensions and
//! `shortestPath`) and read property accesses. A `CREATE`/`MERGE` target
//! pattern and a `SET`/`REMOVE` property target contribute nothing — those
//! names are the write's own output — while a read on a write's right-hand
//! side still counts.
//!
//! The sources are the facts the existing surfaces already expose — never a
//! second vocabulary:
//!
//! - **Labels and relationship types** read the view's catalog
//!   ([`ExecGraph::has_label`] / [`ExecGraph::has_rel_type`]): the committed
//!   store catalog plus the names an explicit transaction's scratch has
//!   written. Every write records a first-seen name, every restore path
//!   (checkpoint rows and WAL redo) replays it, and the catalog never
//!   removes a name — so a miss proves the name has never existed in this
//!   view and a warning can never fire for a known one.
//! - **Property keys** read the view's interner
//!   ([`ExecGraph::has_property_key`]) — the committed interner plus scratch
//!   writes — a **superset** of the keys
//!   `db.propertyKeys` reports. `db.propertyKeys` walks every stored node
//!   and relationship; the interner is fed by the same write path. Every
//!   stored key is interned by `CompactProps::from_parts` on CREATE/SET/
//!   MERGE, and the checkpoint/WAL restore paths reconstruct properties
//!   through the same interner, so a key the store has never held is
//!   absent and a **miss is a true miss**. The converse does not hold:
//!   labels, relationship types and the internal relationship `ts` key
//!   share the table, and a key removed from every entity stays interned.
//!   A hit therefore suppresses a warning (a false negative at worst) but
//!   can never produce a false `UnknownPropertyKeyWarning`. That one-sided
//!   guarantee is exactly what this diagnostic needs: it must never warn
//!   about a key the store holds.
//! - **Deprecated functions and procedures** read
//!   [`cypherlite_executor::builtin_function_deprecation`] and
//!   [`cypherlite_executor::builtin_procedure_deprecation`] — the
//!   `deprecated_by` replacements behind `SHOW FUNCTIONS` and
//!   `SHOW PROCEDURES` (`None` means not deprecated). A user-registered
//!   procedure of the same name shadows the built-in exactly as
//!   `validate_calls` does.
//! - **The scope-less subquery** reads `CallScope::None` in the lowered IR;
//!   the scoped forms (`CALL (n) { … }`, `CALL () { … }`) never fire.
//!
//! `DeprecatedFunction` and `DeprecatedProcedure` map to the vocabulary's
//! `FeatureDeprecationWarning` 01N01 rows; a catalog entry is deprecated
//! exactly when it names a `deprecated_by` replacement. The vocabulary has
//! no 01N02 row yet, so a replacement-less entry cannot be expressed
//! (ADR-0363 decision 5).

use std::collections::BTreeSet;

use cypherlite_executor::{
    builtin_function_deprecation, builtin_procedure_deprecation, DeprecatedBy, DeprecationForm,
    ExecGraph,
};
use cypherlite_ir::{
    walk_pattern_payloads, CallScope, Clause, Expr, ExprRole, LabelExpr, MapProjectionItem,
    NodePattern, PathPattern, PatternPayload, RelPattern,
};
use cypherlite_model::wire::{Notification, NotificationCategory, NotificationKind};
use cypherlite_planner::LogicalPlan;

use crate::notifications::dedup_identical;

/// Merge the tree's operator-scoped entries with this module's
/// statement-scoped entries (ADR-0363 decision 6): operator entries keep
/// their pre-order first, statement entries sort by category, code and
/// description, and identical `(kind, description)` pairs collapse to the
/// first occurrence. The tree's half is already deduplicated by
/// [`crate::plan_node::tree_notifications`], so only the statement half
/// takes the second pass. The one identity helper is
/// [`crate::notifications::dedup_identical`].
pub(crate) fn merge(
    operator: Vec<Notification>,
    mut statement: Vec<Notification>,
) -> Vec<Notification> {
    statement.sort_by(|a, b| statement_order_key(a).cmp(&statement_order_key(b)));
    let mut merged = operator;
    merged.extend(dedup_identical(statement));
    merged
}

/// The statement-scoped entries of one planned statement (ADR-0363
/// decision 5). The plan is walked through the planner's canonical
/// part/stage walk and the IR's exhaustive clause-tree walk, so `NEXT`
/// stages, `UNION` arms, `CALL { … }` bodies, `WHEN` branches, `FOREACH`
/// bodies and expression subqueries all contribute.
pub(crate) fn statement_notifications(
    plan: &LogicalPlan,
    view: &ExecGraph<'_>,
) -> Vec<Notification> {
    // The IR walk takes two callbacks; collect the visited clauses and
    // expression roles first so the one `Facts` accumulator is borrowed by
    // a single fold below (no duplicated traversal, no interior mutability
    // just to split the two callback borrows).
    let mut clauses = Vec::new();
    let mut exprs = Vec::new();
    cypherlite_planner::walk_plan_parts(plan, &mut |part| {
        cypherlite_ir::walk_clauses_with_roles(
            part,
            &mut |clause| clauses.push(clause),
            &mut |expr, role| exprs.push((expr, role)),
        );
    });
    let mut facts = Facts::default();
    for clause in clauses {
        facts.observe_clause(clause, view);
    }
    for (expr, role) in exprs {
        facts.observe_expr(expr, role);
    }
    facts.finish(view)
}

/// The ADR-0363 decision 6 statement-scoped sort key: category rank,
/// then code, then instance description.
fn statement_order_key(notification: &Notification) -> (u8, &'static str, &str) {
    (
        category_rank(notification.category()),
        notification.code(),
        notification.description(),
    )
}

/// `UNRECOGNIZED` before `DEPRECATION`. Performance kinds are
/// operator-scoped and never reach this list; ranking them last keeps the
/// order total and deterministic if a future classification does.
fn category_rank(category: NotificationCategory) -> u8 {
    match category {
        NotificationCategory::Unrecognized => 0,
        NotificationCategory::Deprecation => 1,
        NotificationCategory::Performance => 2,
    }
}

/// The names and deprecations one statement mentions, accumulated before
/// the catalog/interner lookups decide which names are unknown.
#[derive(Default)]
struct Facts {
    labels: BTreeSet<String>,
    rel_types: BTreeSet<String>,
    property_keys: BTreeSet<String>,
    /// The deprecation notifications, already formed; [`merge`] orders and
    /// deduplicates them.
    notifications: Vec<Notification>,
}

impl Facts {
    /// One clause's contributions: a `MATCH` pattern's read payloads, a
    /// `CALL` deprecation, and the scope-less `CALL { … }` subquery. A
    /// `CREATE`/`MERGE` target pattern contributes nothing (ADR-0363
    /// decision 13): its names are the write's own output.
    fn observe_clause(&mut self, clause: &Clause, view: &ExecGraph<'_>) {
        match clause {
            Clause::Match { patterns, .. } => {
                for pattern in patterns {
                    self.observe_pattern(pattern);
                }
            }
            Clause::Call { name, .. } => self.observe_call(view, name),
            Clause::CallSubquery { scope, .. } => {
                if matches!(scope, CallScope::None) {
                    self.notifications.push(Notification::new(
                        NotificationKind::CallSubqueryWithoutScope,
                        "The `CALL { … }` subquery has no variable scope clause.",
                    ));
                }
            }
            _ => {}
        }
    }

    /// One expression's contributions. Unknown-name evidence — property
    /// keys and nested read patterns — comes only from a read expression;
    /// a `SET`/`REMOVE` target or a `CREATE`/`MERGE` pattern payload is a
    /// write target (ADR-0363 decision 13). Deprecations and the scoped
    /// `CALL` fact are clause-shape facts, not name evidence, so they are
    /// observed in every role.
    fn observe_expr(&mut self, expr: &Expr, role: ExprRole) {
        match expr {
            Expr::Property { key, .. } if role == ExprRole::Read => {
                self.property_keys.insert(key.clone());
            }
            Expr::Function { name, .. } => {
                if let Some(replacement) = builtin_function_deprecation(name) {
                    self.notifications.push(
                        Notification::new(
                            NotificationKind::DeprecatedFunction,
                            format!("The function `{name}()` is deprecated."),
                        )
                        .with_remedy(deprecation_remedy(replacement)),
                    );
                }
            }
            // `expr { .key }` is a property access the expression walker
            // does not flatten into `Expr::Property`.
            Expr::MapProjection { items, .. } if role == ExprRole::Read => {
                for item in items {
                    if let MapProjectionItem::Property { key } = item {
                        self.property_keys.insert(key.clone());
                    }
                }
            }
            Expr::PatternPred { path }
            | Expr::PatternComp { path, .. }
            | Expr::ShortestPath { path, .. }
                if role == ExprRole::Read =>
            {
                self.observe_pattern(path)
            }
            _ => {}
        }
    }

    /// A built-in `CALL` deprecation. A registered procedure shadows the
    /// built-in for validation and execution, so it shadows its
    /// deprecation fact too; a non-deprecated or unknown name contributes
    /// nothing.
    fn observe_call(&mut self, view: &ExecGraph<'_>, name: &str) {
        if view.procedure(name).is_some() {
            return;
        }
        let Some(replacement) = builtin_procedure_deprecation(name) else {
            return;
        };
        self.notifications.push(
            Notification::new(
                NotificationKind::DeprecatedProcedure,
                format!("The procedure `{name}` is deprecated."),
            )
            .with_remedy(deprecation_remedy(replacement)),
        );
    }

    /// One read path pattern's node and relationship payloads, through the
    /// IR's one post-rewrite payload visitor: it owns the elements-vs-compact
    /// precedence (so a `WHERE` equality or label predicate the planner
    /// folded into the compact view is seen) and the quantified-group
    /// recursion, so no payload is visited twice.
    fn observe_pattern(&mut self, pattern: &PathPattern) {
        walk_pattern_payloads(pattern, &mut |payload| match payload {
            PatternPayload::Node(node) => self.observe_node(node),
            PatternPayload::Relationship(rel) => self.observe_rel(rel),
            PatternPayload::Quantified(_) => {}
        });
    }

    fn observe_node(&mut self, node: &NodePattern) {
        resolve_names(node.label_expr.as_ref(), &node.labels, &mut self.labels);
        collect_pattern_property_keys(node.properties.as_ref(), &mut self.property_keys);
    }

    fn observe_rel(&mut self, rel: &RelPattern) {
        resolve_names(rel.type_expr.as_ref(), &rel.types, &mut self.rel_types);
        collect_pattern_property_keys(rel.properties.as_ref(), &mut self.property_keys);
    }

    /// Turn the accumulated names into notifications: only names the view
    /// does not know, plus the already-formed deprecations. The three
    /// catalog-resolved classes are one table, so each class names its
    /// predicate, kind and noun once.
    fn finish(self, view: &ExecGraph<'_>) -> Vec<Notification> {
        let mut notifications = self.notifications;
        for (names, class) in [
            (self.labels, CatalogName::Label),
            (self.rel_types, CatalogName::RelationshipType),
            (self.property_keys, CatalogName::PropertyKey),
        ] {
            for name in names {
                if !class.exists(view, &name) {
                    notifications.push(Notification::new(
                        class.kind(),
                        format!(
                            "The {} `{name}` does not exist in this database.",
                            class.noun()
                        ),
                    ));
                }
            }
        }
        notifications
    }
}

/// The three name classes the unknown-name loop resolves against the view's
/// catalog: one row per predicate/kind/noun triple.
#[derive(Clone, Copy)]
enum CatalogName {
    Label,
    RelationshipType,
    PropertyKey,
}

impl CatalogName {
    fn exists(self, view: &ExecGraph<'_>, name: &str) -> bool {
        match self {
            CatalogName::Label => view.has_label(name),
            CatalogName::RelationshipType => view.has_rel_type(name),
            CatalogName::PropertyKey => view.has_property_key(name),
        }
    }

    fn kind(self) -> NotificationKind {
        match self {
            CatalogName::Label => NotificationKind::UnknownLabelWarning,
            CatalogName::RelationshipType => NotificationKind::UnknownRelationshipTypeWarning,
            CatalogName::PropertyKey => NotificationKind::UnknownPropertyKeyWarning,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            CatalogName::Label => "label",
            CatalogName::RelationshipType => "relationship type",
            CatalogName::PropertyKey => "property key",
        }
    }
}

/// The static names of a label/type slot: the label expression when the
/// rewrite produced one, else the flat list. One precedence rule for node
/// labels and relationship types.
fn resolve_names(expr: Option<&LabelExpr>, names: &[String], out: &mut BTreeSet<String>) {
    match expr {
        Some(expr) => collect_label_names(expr, out),
        None => out.extend(names.iter().cloned()),
    }
}

/// The instance remedy for a catalog replacement: an identifier/call is
/// rendered as code, a clause is named as a clause, and the catalog's
/// display prose never reaches the backticks.
fn deprecation_remedy(replacement: DeprecatedBy) -> String {
    match replacement.form() {
        DeprecationForm::Identifier(identifier) => format!("Use `{identifier}` instead."),
        DeprecationForm::Clause(clause) => format!("Use the `{clause}` clause instead."),
    }
}

/// Every static label name in a label/type expression, including the names
/// inside `And`/`Or`/`Not` and `NOT :Label` forms. Dynamic (`$(…)`) and
/// wildcard parts name no static label.
fn collect_label_names(expr: &LabelExpr, out: &mut BTreeSet<String>) {
    match expr {
        LabelExpr::Label(name) => {
            out.insert(name.clone());
        }
        LabelExpr::Wildcard | LabelExpr::Dynamic { .. } => {}
        LabelExpr::Not(inner) => collect_label_names(inner, out),
        LabelExpr::And(items) | LabelExpr::Or(items) => {
            for item in items {
                collect_label_names(item, out);
            }
        }
    }
}

/// The keys a pattern property map mentions. A parameter map (`$props`)
/// names no static key and contributes nothing.
fn collect_pattern_property_keys(properties: Option<&Expr>, out: &mut BTreeSet<String>) {
    if let Some(Expr::Map(items)) = properties {
        out.extend(items.iter().map(|(key, _)| key.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decision 6: operator entries keep their pre-order, statement
    /// entries follow in category/code/description order, and identical
    /// pairs collapse to the first. Only the statement half takes the
    /// dedup pass; the tree's half arrived already deduplicated.
    #[test]
    fn merge_orders_statement_entries_after_operator_entries_and_dedups() {
        let operator = vec![
            Notification::new(NotificationKind::CartesianProduct, "first").with_operator_id(0),
            Notification::new(NotificationKind::AllNodesScan, "second").with_operator_id(1),
        ];
        let statement = vec![
            Notification::new(
                NotificationKind::DeprecatedFunction,
                "The function `id()` is deprecated.",
            ),
            Notification::new(
                NotificationKind::UnknownPropertyKeyWarning,
                "The property key `b` does not exist in this database.",
            ),
            Notification::new(
                NotificationKind::UnknownLabelWarning,
                "The label `A` does not exist in this database.",
            ),
            // The same statement entry twice: one survives.
            Notification::new(
                NotificationKind::UnknownLabelWarning,
                "The label `A` does not exist in this database.",
            ),
        ];
        let merged = merge(operator, statement);
        assert_eq!(
            merged.iter().map(Notification::kind).collect::<Vec<_>>(),
            [
                NotificationKind::CartesianProduct,
                NotificationKind::AllNodesScan,
                NotificationKind::UnknownLabelWarning,
                NotificationKind::UnknownPropertyKeyWarning,
                NotificationKind::DeprecatedFunction,
            ]
        );
        assert_eq!(merged.len(), 5);
    }

    /// The merge deduplicates only the statement half: the tree's operator
    /// entries are already deduplicated by `tree_notifications`, so merge
    /// must not take a second pass over them.
    #[test]
    fn merge_leaves_the_operator_half_to_the_tree_dedup() {
        let duplicate = || Notification::new(NotificationKind::AllNodesScan, "first");
        let merged = merge(vec![duplicate(), duplicate()], Vec::new());
        assert_eq!(
            merged.len(),
            2,
            "operator entries arrive deduplicated and are not re-scanned"
        );
    }

    /// A replacement renders by its syntactic form: an identifier/call is
    /// code, a clause is named as a clause, and catalog prose never reaches
    /// the remedy.
    #[test]
    fn deprecation_remedy_renders_identifier_and_clause_forms() {
        assert_eq!(
            deprecation_remedy(DeprecatedBy::identified(
                "elementId",
                "elementId or consider using an application-generated id",
            )),
            "Use `elementId` instead."
        );
        assert_eq!(
            deprecation_remedy(DeprecatedBy::identifier("db.cdc.current")),
            "Use `db.cdc.current` instead."
        );
        assert_eq!(
            deprecation_remedy(DeprecatedBy::clause("SEARCH")),
            "Use the `SEARCH` clause instead."
        );
    }

    /// Every static label name is collected from `And`/`Or`/`Not`; the
    /// wildcard and dynamic forms contribute none.
    #[test]
    fn label_expression_names_are_collected_recursively() {
        let mut names = BTreeSet::new();
        collect_label_names(
            &LabelExpr::And(vec![
                LabelExpr::Label("A".into()),
                LabelExpr::Or(vec![
                    LabelExpr::Label("B".into()),
                    LabelExpr::Not(Box::new(LabelExpr::Label("C".into()))),
                    LabelExpr::Wildcard,
                ]),
            ]),
            &mut names,
        );
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["A", "B", "C"]);
    }
}
