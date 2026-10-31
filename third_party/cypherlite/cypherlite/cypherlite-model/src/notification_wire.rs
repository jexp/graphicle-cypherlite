//! The query-notification vocabulary and its two wire forms (ADR-0363):
//! one table of kinds (code, GQLSTATUS, severity, category, title and
//! canonical remedy), the notification record every surface projects from,
//! and the native and Neo4j-shaped serializers.
//!
//! Lives beside the plan vocabulary in `plan_wire` — whose presentation
//! model lives with its one consumer in the formatter (ADR-0400) — and is
//! re-exported through [`super`] so `wire::Notification` is the one public
//! path, matching that layout.
//!
//! Two shapes exist by design (ADR-0363 decisions 9/11/12):
//!
//! - the **native** form carries `code`, `gql_status`, `severity`,
//!   `category`, `title`, `description`, `remedy` and — on operator-scoped
//!   kinds — `operator_id`, with the diagnosis and the action as separate
//!   fields;
//! - the **Neo4j-shaped** form (Bolt and the HTTP Query API v2) carries
//!   exactly `code`, `title`, `description`, `severity`, `category`; the
//!   remedy folds into `description` as ` (remedy: <remedy>)` and no
//!   `remedy` or `position` key ever appears.

use serde_json::{Map, Value};

/// The severity spellings (ADR-0363 decision 1): Neo4j's two severities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationSeverity {
    /// An informational hint (`INFORMATION`).
    Information,
    /// A warning (`WARNING`).
    Warning,
}

impl NotificationSeverity {
    /// The wire spelling — the one severity vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationSeverity::Information => "INFORMATION",
            NotificationSeverity::Warning => "WARNING",
        }
    }
}

impl std::fmt::Display for NotificationSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The category spellings (ADR-0363 decision 1): the categories in scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationCategory {
    /// A performance hint (`PERFORMANCE`).
    Performance,
    /// A name the catalog does not know (`UNRECOGNIZED`).
    Unrecognized,
    /// A deprecated feature (`DEPRECATION`).
    Deprecation,
}

impl NotificationCategory {
    /// The wire spelling — the one category vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationCategory::Performance => "PERFORMANCE",
            NotificationCategory::Unrecognized => "UNRECOGNIZED",
            NotificationCategory::Deprecation => "DEPRECATION",
        }
    }
}

impl std::fmt::Display for NotificationCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The notification kinds (ADR-0363 decision 1) — the row identities of the
/// one vocabulary below. `NotificationKind::row` binds each kind to its row
/// through an exhaustive `match`, so a new kind fails the build until it
/// names its metadata; no row is reachable by position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NotificationKind {
    /// A multi-pattern `MATCH` whose patterns share no join anchor.
    CartesianProduct,
    /// A variable-length relationship pattern with no upper bound.
    UnboundedVariableLengthPattern,
    /// A label-less node scan while a property index exists.
    AllNodesScan,
    /// A label/property equality with no range index on the pair.
    MissingIndex,
    /// An aggregation whose input estimate reaches the eager threshold.
    EagerAggregation,
    /// `PROFILE` only: an estimate at least 10x the actual rows.
    EstimationMiss,
    /// A node pattern naming a label absent from the catalog.
    UnknownLabelWarning,
    /// A relationship pattern naming a type absent from the catalog.
    UnknownRelationshipTypeWarning,
    /// A predicate or property access naming an absent property key.
    UnknownPropertyKeyWarning,
    /// A function call resolving to a deprecated catalog entry.
    DeprecatedFunction,
    /// A `CALL` resolving to a deprecated procedure.
    DeprecatedProcedure,
    /// A `CALL { … }` subquery with no variable scope clause.
    CallSubqueryWithoutScope,
}

/// One row of the notification vocabulary (ADR-0363 decision 1). The
/// trigger column of the ADR's table is documentation for the detector
/// nodes; the runtime needs the metadata below.
struct NotificationRow {
    code: &'static str,
    gql_status: &'static str,
    severity: NotificationSeverity,
    category: NotificationCategory,
    title: &'static str,
    remedy: &'static str,
    /// Whether the plan table renders this kind in its `Warnings` column
    /// (ADR-0363 decision 8) — the one membership fact `is_plan_warning`
    /// reads.
    plan_warning: bool,
}

// The one notification vocabulary (ADR-0363 decision 1): every kind's code,
// GQLSTATUS, severity, category, title and canonical remedy, one named row
// per `NotificationKind`. Engine-own kinds use the
// `CypherLite.Notification.*` namespace and a `C` in GQLSTATUS position 3; a
// kind whose diagnostic matches Neo4j's keeps Neo4j's code and status.
const CARTESIAN_PRODUCT_ROW: NotificationRow = NotificationRow {
    code: "Neo.ClientNotification.Statement.CartesianProduct",
    gql_status: "03N90",
    severity: NotificationSeverity::Information,
    category: NotificationCategory::Performance,
    title: "Cartesian product",
    remedy: "Give the patterns a join anchor: reuse a node variable across them, or add a \
             pattern that relates their endpoints; otherwise run them as separate queries.",
    plan_warning: true,
};

const UNBOUNDED_VARIABLE_LENGTH_PATTERN_ROW: NotificationRow = NotificationRow {
    code: "Neo.ClientNotification.Statement.UnboundedVariableLengthPattern",
    gql_status: "03N91",
    severity: NotificationSeverity::Information,
    category: NotificationCategory::Performance,
    title: "Unbounded variable-length pattern",
    remedy: "Bound the expansion (`[:R*1..5]`) where arbitrary length is not required, or add \
             a selective endpoint/label predicate and a `LIMIT`.",
    plan_warning: false,
};

const ALL_NODES_SCAN_ROW: NotificationRow = NotificationRow {
    code: "CypherLite.Notification.Statement.AllNodesScan",
    gql_status: "03C90",
    severity: NotificationSeverity::Information,
    category: NotificationCategory::Performance,
    title: "All nodes scan",
    remedy: "Name the label the scan intends (`MATCH (n:<Label>)`); on the index's label the \
             scan becomes an index-backed lookup.",
    plan_warning: true,
};

const MISSING_INDEX_ROW: NotificationRow = NotificationRow {
    code: "CypherLite.Notification.Statement.MissingIndex",
    gql_status: "03C91",
    severity: NotificationSeverity::Information,
    category: NotificationCategory::Performance,
    title: "Missing index",
    remedy: "`CREATE INDEX cypherlite_Person__email FOR (n:Person) ON (n.email)` — the exact \
             runnable statement, with the name derived deterministically from label and \
             property.",
    plan_warning: false,
};

const EAGER_AGGREGATION_ROW: NotificationRow = NotificationRow {
    code: "CypherLite.Notification.Statement.EagerAggregation",
    gql_status: "03C92",
    severity: NotificationSeverity::Information,
    category: NotificationCategory::Performance,
    title: "Eager aggregation",
    remedy: "Reduce the rows reaching the aggregation with a selective label/property \
             predicate or a `LIMIT` before it; profile again to confirm the buffered input \
             shrank.",
    plan_warning: false,
};

const ESTIMATION_MISS_ROW: NotificationRow = NotificationRow {
    code: "CypherLite.Notification.Statement.EstimationMiss",
    gql_status: "03C93",
    severity: NotificationSeverity::Information,
    category: NotificationCategory::Performance,
    title: "Estimation miss",
    remedy: "Rework the operator's input so the estimate can see the filter (add the \
             selective predicate at the scan, or reorder the join); the estimate is a planner \
             assumption, not a runtime fact.",
    plan_warning: true,
};

const UNKNOWN_LABEL_WARNING_ROW: NotificationRow = NotificationRow {
    code: "Neo.ClientNotification.Statement.UnknownLabelWarning",
    gql_status: "01N50",
    severity: NotificationSeverity::Warning,
    category: NotificationCategory::Unrecognized,
    title: "Unknown label",
    remedy: "Check the spelling; the label exists after the first write that creates it, and \
             no index or constraint uses the unknown name.",
    plan_warning: false,
};

const UNKNOWN_RELATIONSHIP_TYPE_WARNING_ROW: NotificationRow = NotificationRow {
    code: "Neo.ClientNotification.Statement.UnknownRelationshipTypeWarning",
    gql_status: "01N51",
    severity: NotificationSeverity::Warning,
    category: NotificationCategory::Unrecognized,
    title: "Unknown relationship type",
    remedy: "Check the spelling; the type exists after the first write that creates it.",
    plan_warning: false,
};

const UNKNOWN_PROPERTY_KEY_WARNING_ROW: NotificationRow = NotificationRow {
    code: "Neo.ClientNotification.Statement.UnknownPropertyKeyWarning",
    gql_status: "01N52",
    severity: NotificationSeverity::Warning,
    category: NotificationCategory::Unrecognized,
    title: "Unknown property key",
    remedy: "Check the spelling; the key exists after the first write that sets it.",
    plan_warning: false,
};

const DEPRECATED_FUNCTION_ROW: NotificationRow = NotificationRow {
    code: "Neo.ClientNotification.Statement.FeatureDeprecationWarning",
    gql_status: "01N01",
    severity: NotificationSeverity::Warning,
    category: NotificationCategory::Deprecation,
    title: "Deprecated function",
    remedy: "Replace the call with the catalog's `deprecated_by` (`id(n)` → `elementId(n)`).",
    plan_warning: false,
};

const DEPRECATED_PROCEDURE_ROW: NotificationRow = NotificationRow {
    code: "Neo.ClientNotification.Statement.FeatureDeprecationWarning",
    gql_status: "01N01",
    severity: NotificationSeverity::Warning,
    category: NotificationCategory::Deprecation,
    title: "Deprecated procedure",
    remedy: "Use the registry's `deprecated_by` replacement instead; the replacement is the \
             remedy.",
    plan_warning: false,
};

const CALL_SUBQUERY_WITHOUT_SCOPE_ROW: NotificationRow = NotificationRow {
    code: "Neo.ClientNotification.Statement.FeatureDeprecationWarning",
    gql_status: "01N00",
    severity: NotificationSeverity::Warning,
    category: NotificationCategory::Deprecation,
    title: "CALL subquery without scope",
    remedy: "Add the scope clause the body imports: `CALL (n) { … }`, or `CALL () { … }` when \
             it imports nothing.",
    plan_warning: false,
};

impl NotificationKind {
    /// The vocabulary row for this kind — the one lookup every metadata
    /// accessor reads, so no accessor re-spells a code or status. The
    /// `match` is exhaustive, so a new kind is a compile error until it
    /// names its row.
    fn row(self) -> &'static NotificationRow {
        match self {
            NotificationKind::CartesianProduct => &CARTESIAN_PRODUCT_ROW,
            NotificationKind::UnboundedVariableLengthPattern => {
                &UNBOUNDED_VARIABLE_LENGTH_PATTERN_ROW
            }
            NotificationKind::AllNodesScan => &ALL_NODES_SCAN_ROW,
            NotificationKind::MissingIndex => &MISSING_INDEX_ROW,
            NotificationKind::EagerAggregation => &EAGER_AGGREGATION_ROW,
            NotificationKind::EstimationMiss => &ESTIMATION_MISS_ROW,
            NotificationKind::UnknownLabelWarning => &UNKNOWN_LABEL_WARNING_ROW,
            NotificationKind::UnknownRelationshipTypeWarning => {
                &UNKNOWN_RELATIONSHIP_TYPE_WARNING_ROW
            }
            NotificationKind::UnknownPropertyKeyWarning => &UNKNOWN_PROPERTY_KEY_WARNING_ROW,
            NotificationKind::DeprecatedFunction => &DEPRECATED_FUNCTION_ROW,
            NotificationKind::DeprecatedProcedure => &DEPRECATED_PROCEDURE_ROW,
            NotificationKind::CallSubqueryWithoutScope => &CALL_SUBQUERY_WITHOUT_SCOPE_ROW,
        }
    }

    /// Whether the plan table already renders this kind (ADR-0363
    /// decision 8): `CartesianProduct`, `AllNodesScan` and `EstimationMiss`
    /// are the three `PlanWarning` variants, and their one text rendering is
    /// the plan table's `Warnings` column plus footer (ADR-0344/0345). The
    /// other operator-scoped kinds (`UnboundedVariableLengthPattern`,
    /// `MissingIndex`, `EagerAggregation`) do not join that column, so a
    /// text block carries them. The membership is the row's `plan_warning`
    /// column, so a new kind states its side where it names its metadata.
    pub fn is_plan_warning(self) -> bool {
        self.row().plan_warning
    }

    /// The diagnostic code (`Neo.ClientNotification.*` for a Neo4j analog,
    /// `CypherLite.Notification.*` for an engine-own kind).
    pub fn code(self) -> &'static str {
        self.row().code
    }

    /// The GQLSTATUS value (`03N90`, `03C90`, `01N50`, …).
    pub fn gql_status(self) -> &'static str {
        self.row().gql_status
    }

    /// The severity (INFORMATION or WARNING).
    pub fn severity(self) -> NotificationSeverity {
        self.row().severity
    }

    /// The category (PERFORMANCE, UNRECOGNIZED or DEPRECATION).
    pub fn category(self) -> NotificationCategory {
        self.row().category
    }

    /// The one canonical human title for this kind — defined here, never
    /// invented by a caller.
    pub fn title(self) -> &'static str {
        self.row().title
    }

    /// The kind's canonical remedy, used when a notification carries no
    /// instance-specific one.
    pub fn remedy(self) -> &'static str {
        self.row().remedy
    }
}

/// One query notification (ADR-0363 decision 1): the record
/// `{ kind, description, remedy, operator_id }`.
///
/// The kind supplies the metadata — code, GQLSTATUS, severity, category,
/// title and the canonical remedy — so the record stores no copy of it and
/// the two cannot drift. `description` carries the instance detail (the
/// unknown name, the index text, the procedure name); an instance-specific
/// `remedy` overrides the kind's canonical text where the detector can name
/// the precise action (the exact `CREATE INDEX`, the `deprecated_by`
/// replacement). `operator_id` is the raising operator's `PlanNode.id`,
/// present only on operator-scoped kinds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    kind: NotificationKind,
    description: String,
    remedy: Option<String>,
    operator_id: Option<usize>,
}

impl Notification {
    /// One notification of `kind` with its instance `description`; no
    /// instance remedy (the kind's canonical remedy applies) and no
    /// operator association (statement-scoped until [`Self::with_operator_id`]).
    pub fn new(kind: NotificationKind, description: impl Into<String>) -> Self {
        Notification {
            kind,
            description: description.into(),
            remedy: None,
            operator_id: None,
        }
    }

    /// Attach an operator association — the raising `PlanNode.id` — the one
    /// way an operator-scoped notification gains `operator_id`.
    pub fn with_operator_id(mut self, operator_id: usize) -> Self {
        self.operator_id = Some(operator_id);
        self
    }

    /// Override the kind's canonical remedy with the instance-specific
    /// action (the exact runnable statement, the replacement name).
    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }

    /// The notification's kind — the vocabulary row identity.
    pub fn kind(&self) -> NotificationKind {
        self.kind
    }

    /// The instance detail — the unknown name, index text, procedure name
    /// or warning message.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The kind's diagnostic code.
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// The kind's GQLSTATUS value.
    pub fn gql_status(&self) -> &'static str {
        self.kind.gql_status()
    }

    /// The kind's canonical title.
    pub fn title(&self) -> &'static str {
        self.kind.title()
    }

    /// The kind's severity.
    pub fn severity(&self) -> NotificationSeverity {
        self.kind.severity()
    }

    /// The kind's category.
    pub fn category(&self) -> NotificationCategory {
        self.kind.category()
    }

    /// The effective remedy: the instance-specific one when the detector
    /// supplied it, else the kind's canonical remedy. Every vocabulary kind
    /// carries a remedy, so this is never absent.
    pub fn remedy(&self) -> &str {
        self.remedy.as_deref().unwrap_or_else(|| self.kind.remedy())
    }

    /// The raising operator's `PlanNode.id`, on operator-scoped kinds only.
    pub fn operator_id(&self) -> Option<usize> {
        self.operator_id
    }
}

/// The engine-native notification keys (ADR-0363 decision 9) — the
/// vocabulary [`notification_to_wire_json`] emits and every consumer
/// reads, single-sourced like [`super::plan_keys`].
pub mod notification_keys {
    /// The diagnostic code.
    pub const CODE: &str = "code";
    /// The GQLSTATUS value.
    pub const GQL_STATUS: &str = "gql_status";
    /// The severity spelling.
    pub const SEVERITY: &str = "severity";
    /// The category spelling.
    pub const CATEGORY: &str = "category";
    /// The canonical title.
    pub const TITLE: &str = "title";
    /// The instance diagnosis.
    pub const DESCRIPTION: &str = "description";
    /// The effective remedy (always present on the native form).
    pub const REMEDY: &str = "remedy";
    /// The raising operator's id (operator-scoped kinds only).
    pub const OPERATOR_ID: &str = "operator_id";
    /// The response envelope's always-present notification list.
    pub const NOTIFICATIONS: &str = "notifications";
}

/// The seven diagnostic fields of one notification's engine-native wire
/// object, keyed by the [`notification_keys`] vocabulary — the core
/// [`notification_to_wire_json`] layers the operator id onto, and the exact
/// object the agent-native plan document projects for a node's `warnings`
/// (the node already scopes the operator, so the join key is not repeated
/// there). One spelling of the field set, so a field added to the canonical
/// notification reaches every projection instead of drifting in a second
/// hand-rolled one.
pub(crate) fn notification_fields(notification: &Notification) -> Map<String, Value> {
    let mut obj = Map::new();
    obj.insert(
        notification_keys::CODE.into(),
        Value::String(notification.code().into()),
    );
    obj.insert(
        notification_keys::GQL_STATUS.into(),
        Value::String(notification.gql_status().into()),
    );
    obj.insert(
        notification_keys::SEVERITY.into(),
        Value::String(notification.severity().as_str().into()),
    );
    obj.insert(
        notification_keys::CATEGORY.into(),
        Value::String(notification.category().as_str().into()),
    );
    obj.insert(
        notification_keys::TITLE.into(),
        Value::String(notification.title().into()),
    );
    obj.insert(
        notification_keys::DESCRIPTION.into(),
        Value::String(notification.description().into()),
    );
    obj.insert(
        notification_keys::REMEDY.into(),
        Value::String(notification.remedy().into()),
    );
    obj
}

/// One notification's engine-native wire object (ADR-0363 decision 9):
/// `code`, `gql_status`, `severity`, `category`, `title`, `description`,
/// `remedy` and — on operator-scoped kinds — `operator_id`.
pub fn notification_to_wire_json(notification: &Notification) -> Value {
    let mut obj = notification_fields(notification);
    if let Some(operator_id) = notification.operator_id() {
        obj.insert(
            notification_keys::OPERATOR_ID.into(),
            Value::from(operator_id),
        );
    }
    Value::Object(obj)
}

/// A notification list in the engine-native form — the array the shared
/// success envelope always carries (empty on a plain run).
pub fn notifications_to_wire_json(notifications: &[Notification]) -> Value {
    Value::Array(
        notifications
            .iter()
            .map(notification_to_wire_json)
            .collect(),
    )
}

/// One notification's Neo4j-shaped entries (ADR-0363 decisions 11/12), in
/// Bolt's entry order: `code`, `title`, `description`, `severity`,
/// `category`. The effective remedy is folded into `description` as
/// ` (remedy: <remedy>)`; no `remedy` and no `position` entry is ever
/// emitted. This is the ordered form Bolt packs directly, without building
/// or re-reading a JSON object; [`notification_to_neo4j_json`] is the JSON
/// projection of the same entries.
pub fn notification_to_neo4j_entries(notification: &Notification) -> Vec<(&'static str, String)> {
    vec![
        (notification_keys::CODE, notification.code().to_string()),
        (notification_keys::TITLE, notification.title().to_string()),
        (
            notification_keys::DESCRIPTION,
            format!(
                "{} (remedy: {})",
                notification.description(),
                notification.remedy()
            ),
        ),
        (
            notification_keys::SEVERITY,
            notification.severity().as_str().to_string(),
        ),
        (
            notification_keys::CATEGORY,
            notification.category().as_str().to_string(),
        ),
    ]
}

/// One notification's Neo4j-shaped wire object (ADR-0363 decisions 11/12):
/// exactly `code`, `title`, `description`, `severity`, `category`, with the
/// remedy folded into `description` as ` (remedy: <remedy>)` and no `remedy`
/// or `position` key. Built from [`notification_to_neo4j_entries`].
///
/// `serde_json::Map` is a sorted map in this workspace (it does not enable
/// `preserve_order`), so the object's key order is sorted, not Bolt's.
/// Callers that emit ordered entries — Bolt — read
/// [`notification_to_neo4j_entries`] instead.
pub fn notification_to_neo4j_json(notification: &Notification) -> Value {
    let obj: Map<String, Value> = notification_to_neo4j_entries(notification)
        .into_iter()
        .map(|(key, value)| (key.to_owned(), Value::String(value)))
        .collect();
    Value::Object(obj)
}

#[cfg(test)]
#[path = "notification_wire/tests.rs"]
mod tests;
