//! The typed plan-warning vocabulary (ADR-0344, typed node warnings;
//! LABS-461): the one warning type — the [`PlanWarning`] enum owning the
//! wire name, the message text, the payload variant, and the notification
//! projection once each — beside the estimate-vs-actual policy its
//! `EstimationMiss` variant derives from and the wire-shape serializer
//! the HTTP plan reads. It lives here, beside the notification vocabulary
//! it projects into and the wire parse it coordinates, so the engine's
//! detection, the wire parse, and every surface's rendering cannot drift
//! (not three review-coordinated layers).

use super::notification_wire::NotificationKind;
use super::plan_wire::plan_keys;

/// The typed warnings vocabulary's wire name, by variant — the one
/// spelling the engine's detection serializes into the wire form and
/// [`PlanWarning::from_wire_parts`] parses back; a name outside the
/// vocabulary is a contract break. In-crate today (the parse and the
/// model's tests); re-widened to `pub` in the same change as the first
/// cross-crate consumer naming a wire warning.
pub(crate) const WARNING_ESTIMATION_MISS: &str = "EstimationMiss";
/// See [`WARNING_ESTIMATION_MISS`].
pub(crate) const WARNING_CARTESIAN_PRODUCT: &str = "CartesianProduct";
/// See [`WARNING_ESTIMATION_MISS`].
pub(crate) const WARNING_ALL_NODES_SCAN: &str = "AllNodesScan";

/// The `EstimationMiss` overshoot factor (ADR-0344): the estimate misses
/// when it overshoots the actual rows by this many times or more — the
/// one spelling both the policy ([`estimation_miss`]) divides by and the
/// variant's message text names, so the factor and its "10x" cannot
/// drift apart.
const ESTIMATION_MISS_FACTOR: u64 = 10;

/// One typed warning a plan node carries (ADR-0344, typed node warnings):
/// the vocabulary's one definition — the wire name it serializes by, the
/// message text that renders beside the affected operator and in the
/// warnings footer, the payload variant that names the property index, and
/// the notification kind it projects to (ADR-0363 decision 1) — each owned
/// here once, beside the notification vocabulary it projects into, so the
/// engine's detection, the wire parse, and every surface's rendering
/// cannot drift (LABS-461: one warning type, not three coordinated
/// layers).
///
/// The engine constructs the variants at plan-tree construction (the
/// MATCH-shape detection in `cypherlite`'s `plan_node/warning.rs`); the
/// wire accessor parses the `{name, index}` maps back into them — the one
/// typed vocabulary in both directions.
///
/// ```rust
/// # use cypherlite_model::wire::PlanWarning;
/// let cartesian = PlanWarning::CartesianProduct;
/// assert_eq!(cartesian.name(), "CartesianProduct");
/// assert_eq!(
///     cartesian.message(),
///     "the MATCH patterns share no join anchor"
/// );
/// // The AllNodesScan warning carries the existing property index it
/// // suggests, so the text names it.
/// let scan = PlanWarning::AllNodesScan {
///     index: ":Person(name)".to_string(),
/// };
/// assert_eq!(scan.name(), "AllNodesScan");
/// assert_eq!(
///     scan.message(),
///     "the scan matches every node; consider the property index (:Person(name))"
/// );
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanWarning {
    /// The operator's estimate overshot its actual rows by 10x or more —
    /// the same policy the ratio cell's `!` marker reads. PROFILE only:
    /// it needs the actual rows, so it never fires on an EXPLAIN tree.
    EstimationMiss,
    /// A multi-pattern `MATCH` whose patterns share no join anchor (no
    /// node variable appears in more than one pattern) — the patterns
    /// multiply instead of joining.
    CartesianProduct,
    /// A label-less scan (`MATCH (n)`) while the store carries a property
    /// index: the warning carries the existing index's text
    /// (`:Label(property)`) as the suggestion.
    AllNodesScan {
        /// The existing property index the suggestion names.
        index: String,
    },
}

/// One variant's vocabulary row — the wire name, the base message text,
/// and the notification kind the warning projects to, each named so a
/// transposed read (`name()` reading the text) stops compiling, the
/// same boundary the notification vocabulary's own row struct draws.
struct VocabularyRow {
    name: &'static str,
    /// The base message text — `String` because the `EstimationMiss`
    /// row names [`ESTIMATION_MISS_FACTOR`], built here rather than
    /// spelled a second time.
    text: String,
    kind: NotificationKind,
}

impl PlanWarning {
    /// One exhaustive variant mapping: the wire name, the base message
    /// text, and the notification kind the warning projects to (ADR-0363
    /// decision 1) — a fourth warning variant must choose all three
    /// here rather than falling through a wildcard. Every projected kind
    /// is a plan warning in the model's partition
    /// ([`NotificationKind::is_plan_warning`]), so the plan table's
    /// warning text and the notification description are one spelling.
    fn vocabulary(&self) -> VocabularyRow {
        match self {
            PlanWarning::EstimationMiss => VocabularyRow {
                name: WARNING_ESTIMATION_MISS,
                text: format!(
                    "estimated rows {ESTIMATION_MISS_FACTOR}x or more above the actual rows"
                ),
                kind: NotificationKind::EstimationMiss,
            },
            PlanWarning::CartesianProduct => VocabularyRow {
                name: WARNING_CARTESIAN_PRODUCT,
                text: "the MATCH patterns share no join anchor".to_string(),
                kind: NotificationKind::CartesianProduct,
            },
            PlanWarning::AllNodesScan { .. } => VocabularyRow {
                name: WARNING_ALL_NODES_SCAN,
                text: "the scan matches every node; consider the property index".to_string(),
                kind: NotificationKind::AllNodesScan,
            },
        }
    }

    /// The warning's wire name — the one spelling the wire form
    /// serializes (`warnings` on the node) and
    /// [`Self::from_wire_parts`] validates against.
    pub fn name(&self) -> &'static str {
        self.vocabulary().name
    }

    /// The warning's index payload — the `AllNodesScan` warning's suggested
    /// property index, `None` on the payload-less variants. The one spelling
    /// of which warnings carry a payload, read by both the message text
    /// and the wire form's `index` field, so the vocabulary is matched once.
    pub fn suggested_index(&self) -> Option<&str> {
        match self {
            PlanWarning::AllNodesScan { index } => Some(index.as_str()),
            PlanWarning::EstimationMiss | PlanWarning::CartesianProduct => None,
        }
    }

    /// The notification kind this warning projects to (ADR-0363
    /// decision 1) — from the one exhaustive variant mapping [`Self::name`]
    /// reads, so a fourth warning variant must choose its vocabulary row
    /// there rather than falling through a wildcard.
    ///
    /// ```rust
    /// # use cypherlite_model::wire::PlanWarning;
    /// # use cypherlite_model::wire::NotificationKind;
    /// assert_eq!(
    ///     PlanWarning::CartesianProduct.notification_kind(),
    ///     NotificationKind::CartesianProduct
    /// );
    /// assert!(PlanWarning::EstimationMiss
    ///     .notification_kind()
    ///     .is_plan_warning());
    /// ```
    pub fn notification_kind(&self) -> NotificationKind {
        self.vocabulary().kind
    }

    /// The warning's message text — the vocabulary's base text
    /// ([`Self::vocabulary`]'s one spelling), with the payload appended in
    /// parentheses when the warning carries one (the `AllNodesScan`
    /// warning's suggested property index). The one spelling the engine
    /// renderer and every surface's table layout render, so the text
    /// cannot drift between the forms.
    pub fn message(&self) -> String {
        match self.suggested_index() {
            Some(index) => format!("{} ({index})", self.vocabulary().text),
            None => self.vocabulary().text.to_string(),
        }
    }

    /// The wire parse — the one validating match back into the typed
    /// vocabulary: a name from it with the payload shape its variant
    /// carries (`AllNodesScan` requires the index, the payload-less
    /// variants carry none). `None` names a warning outside the
    /// vocabulary or a payload shape its variant does not carry — a
    /// wire-contract break the accessor fails loudly on, never a
    /// silently dropped or reinterpreted warning. In-crate today (the
    /// accessor and the model's tests); re-widened to `pub` in the same
    /// change as the first cross-crate consumer parsing raw wire
    /// warning maps.
    pub(crate) fn from_wire_parts(name: &str, index: Option<&str>) -> Option<Self> {
        match (name, index) {
            (WARNING_ESTIMATION_MISS, None) => Some(PlanWarning::EstimationMiss),
            (WARNING_CARTESIAN_PRODUCT, None) => Some(PlanWarning::CartesianProduct),
            (WARNING_ALL_NODES_SCAN, Some(index)) => Some(PlanWarning::AllNodesScan {
                index: index.to_string(),
            }),
            _ => None,
        }
    }
}

/// Whether the estimated-vs-actual ratio is defined (ADR-0344 metric
/// fields): the operator produced rows. The one predicate both the
/// ratio cell and the overshoot policy read — never a second
/// `actual > 0` spelling at either site.
pub fn ratio_defined(actual: u64) -> bool {
    actual > 0
}

/// The one estimated-vs-actual overshoot policy (ADR-0344 metric
/// fields + typed node warnings): the estimate misses when it
/// overshoots the actual rows by 10x or more and the operator
/// produced rows (an estimate over zero actual rows has no ratio).
/// Both the ratio cell's warning marker and the `EstimationMiss`
/// warning derive from this one policy, never a second computation.
pub fn estimation_miss(estimated: u64, actual: u64) -> bool {
    // Integer comparison — exact for every saturated u64 (f64's 53-bit
    // mantissa would lose the boundary); floor division preserves the
    // "ten times or more" test for actual > 0.
    ratio_defined(actual) && estimated / ESTIMATION_MISS_FACTOR >= actual
}

/// The wire shape of one typed warning: `{name, index?}`, built from
/// the one [`PlanWarning`] vocabulary — the spelling the plan-wire
/// warning projections read (the HTTP v2 plan), never a re-assembled
/// `{name, index}` at each site. The agent-native document does not read
/// it: per ADR-0381 its node `warnings` derive from the result's
/// remedy-bearing notifications.
pub fn warning_wire_json(warning: &PlanWarning) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert(
        plan_keys::WARNING_NAME.to_string(),
        serde_json::Value::String(warning.name().to_string()),
    );
    if let Some(index) = warning.suggested_index() {
        out.insert(
            plan_keys::WARNING_INDEX.to_string(),
            serde_json::Value::String(index.to_string()),
        );
    }
    serde_json::Value::Object(out)
}
