use std::borrow::Cow;

use cypherlite_error::ErrorKind;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("syntax error: {0}")]
    Syntax(String),
    #[error("semantic error: {0}")]
    Semantic(String),
    /// A uniqueness-constraint violation raised by constraint validation,
    /// carrying the violated constraint's name typed, so consumers match the
    /// class and read the name without parsing the rendered message. The
    /// Display text is byte-identical to the historical
    /// `Semantic("ConstraintValidationFailed: `<name>` uniqueness")`
    /// rendering — stored text, bindings classification and pinned messages
    /// do not move.
    #[error("semantic error: {}", uniqueness_violation_detail(name))]
    UniquenessViolation { name: String },
    /// A call to a procedure the engine does not register (ADR-0138),
    /// carrying the missing procedure's name typed, so consumers match the
    /// class without a prefix check on the rendered message. The Display
    /// text is byte-identical to the historical
    /// `Syntax("ProcedureNotFound: <name>")` rendering — pinned messages do
    /// not move.
    #[error("syntax error: ProcedureNotFound: {name}")]
    ProcedureNotFound { name: String },
    /// An argument of the wrong static or runtime type, carrying the detail
    /// text typed, so consumers match the class without scanning the
    /// rendered message. The Display text is byte-identical to the
    /// historical `Type("InvalidArgumentType: <detail>")` rendering.
    #[error("{}", invalid_argument_type_display(.0))]
    InvalidArgumentType(String),
    /// A runtime type failure raised under the legacy `TypeError` detail
    /// name, carrying the detail text typed. The Display text is
    /// byte-identical to the historical `Type("TypeError: <detail>")`
    /// rendering.
    #[error("{}", type_error_variant_display(.0))]
    TypeError(String),
    /// A reference to a query parameter that was not supplied, carrying the
    /// detail text typed. The Display text is byte-identical to the
    /// historical `Semantic("ParameterMissing: <detail>")` rendering.
    #[error("semantic error: ParameterMissing: {0}")]
    ParameterMissing(String),
    /// A selected database or `USE` target that is not one of the fixed
    /// namespaces (ADR-0146, ADR-0307), carrying the name typed. The
    /// Display text is byte-identical to the historical
    /// `Semantic("DatabaseNotFound: Graph not found: <name>")` rendering.
    #[error("semantic error: {}", database_not_found_detail(name))]
    DatabaseNotFound { name: String },
    #[error("{}", type_error_display(.0))]
    Type(String),
    /// An arithmetic failure raised while evaluating an expression
    /// (ADR-0307). The Display text is the historical rendering of each
    /// [`ArithmeticFailure`], composed here from the shared Type-class
    /// prefix (ADR-0338).
    #[error("{}", arithmetic_detail(.0))]
    Arithmetic(ArithmeticFailure),
    /// Failure raised while evaluating an already parsed and planned query.
    /// The wrapped text preserves the executor's typed detail.
    #[error("{0}")]
    Execution(String),
    /// A statement stopped through its [`crate::CancelToken`] (ADR-0111,
    /// ADR-0307). The Display text is byte-identical to the historical
    /// `Execution("query cancelled")` rendering.
    #[error("query cancelled")]
    Cancelled,
    #[error("write-write conflict")]
    Conflict,
    #[error("capacity exceeded: used={used} limit={limit}")]
    Capacity { used: u64, limit: u64 },
    /// A processing control refused work. `name` is
    /// [`Error::CONCURRENT_LOGICAL_WORKSPACE_LIMIT`].
    /// `retryable` is false when the same call will fail again. `receipt`
    /// is the logical bytes counted when admission produced the error.
    #[error("resource limit exceeded: {name} limit={limit}")]
    ResourceLimit {
        name: &'static str,
        limit: u64,
        retryable: bool,
        receipt: Option<crate::tokens::ExecutionReceipt>,
    },
    #[error("storage error: {0}")]
    Storage(String),
    /// A persistent database is already owned by another independent opener.
    #[error("storage locked: {0}")]
    Locked(String),
    #[error("io error: {0}")]
    Io(String),
    /// Engine-internal failure that is not the query's fault: worker-thread
    /// spawn failure or a panic observed across the worker join (ADR-0259).
    /// Always recoverable by the caller; never an abort.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// `ResourceLimit::name` for the concurrent logical-workspace admission
    /// control. `limit` is the pool capacity in logical bytes.
    pub const CONCURRENT_LOGICAL_WORKSPACE_LIMIT: &'static str =
        "concurrent-logical-workspace-limit";

    /// `ResourceLimit::name` for a writer mid-`demote` (ADR-0440): new
    /// writes are refused while commits already past admission finish and
    /// the handle checkpoints before releasing the writer lease. Always
    /// `retryable: true` — the caller retries once the follower that is
    /// about to be promoted takes over.
    pub const WRITER_DRAINING: &'static str = "writer-draining";

    /// `ResourceLimit::name` for a follower that yielded across a checkpoint
    /// gap (ADR-0441). Always `retryable: true`. The host drops this engine
    /// and opens a new one; this process does not load a second graph.
    pub const FOLLOW_GAP: &'static str = "follow-gap";

    pub fn syntax(msg: impl Into<String>) -> Self {
        Self::Syntax(msg.into())
    }

    pub fn semantic(msg: impl Into<String>) -> Self {
        Self::Semantic(msg.into())
    }

    /// A uniqueness violation naming the violated constraint — the typed
    /// counterpart of the pinned `ConstraintValidationFailed` message text.
    pub fn uniqueness_violation(name: impl Into<String>) -> Self {
        Self::UniquenessViolation { name: name.into() }
    }

    /// A call to an unregistered procedure, naming it — the typed
    /// counterpart of the pinned `ProcedureNotFound: <name>` message text
    /// (ADR-0138).
    pub fn procedure_not_found(name: impl Into<String>) -> Self {
        Self::ProcedureNotFound { name: name.into() }
    }

    /// An argument of the wrong type, with the detail text after the pinned
    /// `InvalidArgumentType:` token — the typed counterpart of the legacy
    /// `Type("InvalidArgumentType: <detail>")` string.
    pub fn invalid_argument_type(detail: impl Into<String>) -> Self {
        Self::InvalidArgumentType(detail.into())
    }

    /// A runtime type failure under the legacy `TypeError` detail name, with
    /// the detail text after the pinned `TypeError:` token.
    pub fn type_error(detail: impl Into<String>) -> Self {
        Self::TypeError(detail.into())
    }

    /// A query parameter that was not supplied, with the detail text after
    /// the pinned `ParameterMissing:` token — the typed counterpart of the
    /// legacy `Semantic("ParameterMissing: <detail>")` string.
    pub fn parameter_missing(detail: impl Into<String>) -> Self {
        Self::ParameterMissing(detail.into())
    }

    /// A statement that references parameters the caller did not supply
    /// (ADR-0308): the `Expected parameter(s): <names>` detail, the names
    /// joined by `, ` in the order given. The one spelling of the message.
    pub fn parameters_missing<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let names: Vec<&str> = names.into_iter().collect();
        Self::ParameterMissing(format!("Expected parameter(s): {}", names.join(", ")))
    }

    /// A database or `USE` target that is not a fixed namespace, naming it
    /// (ADR-0307) — the typed counterpart of the pinned
    /// `DatabaseNotFound: Graph not found: <name>` text.
    pub fn database_not_found(name: impl Into<String>) -> Self {
        Self::DatabaseNotFound { name: name.into() }
    }

    /// A reference to a variable the evaluated row does not bind (ADR-0335)
    /// — the executor-side counterpart of the front-end's
    /// `UndefinedVariable` semantic error, for expressions the plan-time
    /// binding walk cannot see (catalog predicates lowered at run time).
    /// Renders byte-identically to the front-end's error:
    /// `syntax error: UndefinedVariable: variable '<name>' not defined`.
    pub fn undefined_variable(name: &str) -> Self {
        Self::syntax(format!(
            "{}: variable '{name}' not defined",
            ErrorKind::UndefinedVariable.as_str()
        ))
    }

    pub fn ty(msg: impl Into<String>) -> Self {
        Self::Type(msg.into())
    }

    pub fn arithmetic(failure: ArithmeticFailure) -> Self {
        Self::Arithmetic(failure)
    }

    pub fn execution(error: impl Into<String>) -> Self {
        Self::Execution(error.into())
    }

    /// The one spelling of statement cancellation (see `crate::cancel`):
    /// [`Error::Cancelled`], whose Display is the documented
    /// `"query cancelled"` contract string.
    pub fn cancelled() -> Self {
        Self::Cancelled
    }

    pub fn conflict() -> Self {
        Self::Conflict
    }

    pub fn capacity(used: u64, limit: u64) -> Self {
        Self::Capacity { used, limit }
    }

    /// A processing control refused work. The display text names the
    /// control and the configured limit.
    pub fn resource_limit(name: &'static str, limit: u64, retryable: bool) -> Self {
        Self::ResourceLimit {
            name,
            limit,
            retryable,
            receipt: None,
        }
    }

    /// [`Self::resource_limit`] plus the token receipt counted so far.
    pub fn resource_limit_with_receipt(
        name: &'static str,
        limit: u64,
        retryable: bool,
        receipt: crate::tokens::ExecutionReceipt,
    ) -> Self {
        Self::ResourceLimit {
            name,
            limit,
            retryable,
            receipt: Some(receipt),
        }
    }

    /// The token receipt attached to a resource-limit error, when the
    /// refusal came from token admission.
    pub fn receipt(&self) -> Option<&crate::tokens::ExecutionReceipt> {
        match self {
            Self::ResourceLimit { receipt, .. } => receipt.as_ref(),
            _ => None,
        }
    }

    pub fn storage(msg: impl Into<String>) -> Self {
        Self::Storage(msg.into())
    }

    pub fn io(msg: impl Into<String>) -> Self {
        Self::Io(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// The pinned class token and detail payload of the six typed detail
    /// variants ([`Error::UniquenessViolation`],
    /// [`Error::ProcedureNotFound`], [`Error::InvalidArgumentType`],
    /// [`Error::TypeError`], [`Error::ParameterMissing`],
    /// [`Error::DatabaseNotFound`]), single-sourced
    /// beside the Display attributes that own the same tokens so consumers
    /// classify and re-render the legacy bare text without re-spelling
    /// them. The detail borrows where the variant stores it verbatim and is
    /// owned where Display composes it (the uniqueness violation's
    /// `` `<name>` uniqueness ``, the CE `Graph not found: <name>`).
    /// `None` for every other class.
    pub fn typed_detail(&self) -> Option<(&'static str, Cow<'_, str>)> {
        match self {
            Error::UniquenessViolation { name } => Some((
                CONSTRAINT_VALIDATION_FAILED,
                Cow::Owned(uniqueness_detail(name)),
            )),
            Error::DatabaseNotFound { name } => Some((
                ErrorKind::DatabaseNotFound.as_str(),
                Cow::Owned(graph_not_found_text(name)),
            )),
            Error::ProcedureNotFound { name } => {
                Some(("ProcedureNotFound", Cow::Borrowed(name.as_str())))
            }
            Error::InvalidArgumentType(detail) => {
                Some(("InvalidArgumentType", Cow::Borrowed(detail.as_str())))
            }
            Error::TypeError(detail) => Some(("TypeError", Cow::Borrowed(detail.as_str()))),
            Error::ParameterMissing(detail) => {
                Some(("ParameterMissing", Cow::Borrowed(detail.as_str())))
            }
            _ => None,
        }
    }

    /// The legacy bare detail text — `<Token>: <detail>` — of the typed
    /// detail variants, exactly as the stringly `Type`/`Semantic` payloads
    /// spelled it. Built on [`Error::typed_detail`]; `None` for every
    /// other class.
    pub fn typed_detail_text(&self) -> Option<String> {
        let (token, detail) = self.typed_detail()?;
        Some(format!("{token}: {detail}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// The one spelling of the poisoned-lock policy (AGENTS.md: one canonical
/// helper per policy): a panicked lock holder fences the operation with an
/// internal error instead of silently proceeding on possibly torn state.
/// Every `.lock()?` / `.read()?` on a store mutex or lock resolves through
/// this impl.
impl<T> From<std::sync::PoisonError<T>> for Error {
    fn from(err: std::sync::PoisonError<T>) -> Self {
        Self::internal(err.to_string())
    }
}

/// The pinned class token of a uniqueness-constraint violation, shared by
/// [`Error::UniquenessViolation`]'s Display composition and
/// [`Error::typed_detail`] so both spell it from one place.
const CONSTRAINT_VALIDATION_FAILED: &str = "ConstraintValidationFailed";

/// The Type-class Display prefix, shared by [`Error::Type`]'s attribute and
/// the other Type-class details composed beside it (ADR-0338), so none of
/// them can spell the class differently.
const TYPE_ERROR_PREFIX: &str = "type error";

/// [`Error::Type`]'s Display: the shared Type-class prefix and the detail.
fn type_error_display(detail: &str) -> String {
    format!("{TYPE_ERROR_PREFIX}: {detail}")
}

/// [`Error::InvalidArgumentType`]'s Display: [`type_error_display`] with the
/// variant's pinned detail token, so the class prefix comes from the one
/// [`TYPE_ERROR_PREFIX`].
fn invalid_argument_type_display(detail: &str) -> String {
    format!(
        "{}: {detail}",
        type_error_display(ErrorKind::InvalidArgumentType.as_str())
    )
}

/// [`Error::TypeError`]'s Display: [`type_error_display`] with the
/// variant's pinned detail token, on the same one [`TYPE_ERROR_PREFIX`].
fn type_error_variant_display(detail: &str) -> String {
    format!(
        "{}: {detail}",
        type_error_display(ErrorKind::TypeError.as_str())
    )
}

/// [`Error::Arithmetic`]'s Display: the historical rendering of each
/// [`ArithmeticFailure`] — [`type_error_display`] for the two division
/// failures, the bare `ArithmeticError: …` payload for rounding (ADR-0307,
/// ADR-0338).
fn arithmetic_detail(failure: &ArithmeticFailure) -> String {
    match failure {
        ArithmeticFailure::DivisionByZero | ArithmeticFailure::ModuloByZero => {
            type_error_display(failure.detail())
        }
        ArithmeticFailure::RoundingNecessary => failure.detail().to_string(),
    }
}

/// The pinned `` `<name>` uniqueness `` detail of a uniqueness violation,
/// shared by the Display composition and [`Error::typed_detail`].
fn uniqueness_detail(name: &str) -> String {
    format!("`{name}` uniqueness")
}

/// The bare `ConstraintValidationFailed` detail of a uniqueness violation —
/// [`Error::UniquenessViolation`]'s Display without the `semantic error: `
/// prefix, for error composition that embeds the violation's message.
pub fn uniqueness_violation_detail(name: &str) -> String {
    format!(
        "{CONSTRAINT_VALIDATION_FAILED}: {}",
        uniqueness_detail(name)
    )
}

/// The CE text of an unknown graph, `Graph not found: <name>` — the one
/// place it lives; [`Error::DatabaseNotFound`]'s Display and typed detail
/// compose it (moved here from `ce_messages` so the dependency stays
/// one-way `ce_messages -> error.rs`).
fn graph_not_found_text(name: &str) -> String {
    format!("Graph not found: {name}")
}

/// [`Error::DatabaseNotFound`]'s Display without the `semantic error: `
/// prefix — its [`Error::typed_detail_text`], composed from the typed
/// kind token and the CE text (ADR-0149).
fn database_not_found_detail(name: &str) -> String {
    format!(
        "{}: {}",
        ErrorKind::DatabaseNotFound.as_str(),
        graph_not_found_text(name)
    )
}

/// The arithmetic failures the executor raises (ADR-0307), typed so a
/// surface maps them without reading the message. Each renders exactly as
/// the stringly error it replaced did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithmeticFailure {
    /// Integer or duration division by zero: `type error: / by zero`.
    DivisionByZero,
    /// Integer modulo by zero: `type error: % by zero`.
    ModuloByZero,
    /// `round(x, precision, 'UNNECESSARY')` on a value that needs rounding:
    /// `ArithmeticError: rounding necessary`.
    RoundingNecessary,
}

impl ArithmeticFailure {
    /// The payload the replaced stringly error carried — `/ by zero` and
    /// `% by zero` (historically `Type`), `ArithmeticError: rounding
    /// necessary` (historically `Execution`) — for consumers that embed or
    /// classify that legacy text.
    pub fn detail(self) -> &'static str {
        match self {
            ArithmeticFailure::DivisionByZero => "/ by zero",
            ArithmeticFailure::ModuloByZero => "% by zero",
            ArithmeticFailure::RoundingNecessary => "ArithmeticError: rounding necessary",
        }
    }
}

impl std::fmt::Display for ArithmeticFailure {
    /// The bare legacy payload ([`Self::detail`]); the enclosing
    /// [`Error::Arithmetic`] composes the Type-class prefix (ADR-0338).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_display_is_stable() {
        let e = Error::capacity(100, 64);
        assert_eq!(e.to_string(), "capacity exceeded: used=100 limit=64");
        assert!(matches!(
            e,
            Error::Capacity {
                used: 100,
                limit: 64
            }
        ));
    }

    #[test]
    fn resource_limit_display_names_the_flag() {
        let e = Error::resource_limit("max-active-queries", 4, false);
        assert_eq!(
            e.to_string(),
            "resource limit exceeded: max-active-queries limit=4"
        );
        assert!(matches!(
            e,
            Error::ResourceLimit {
                retryable: false,
                ..
            }
        ));
        let retry = Error::resource_limit("max-active-queries", 8, true);
        assert!(matches!(
            retry,
            Error::ResourceLimit {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn internal_display_is_stable() {
        let e = Error::internal("query worker thread panicked");
        assert_eq!(
            e.to_string(),
            "internal error: query worker thread panicked"
        );
        assert!(matches!(e, Error::Internal(_)));
    }

    #[test]
    fn locked_is_distinct_from_storage_and_io() {
        let error = Error::Locked("/db/.lock".into());
        assert_eq!(error.to_string(), "storage locked: /db/.lock");
        assert!(matches!(error, Error::Locked(_)));
    }

    #[test]
    fn uniqueness_violation_display_matches_the_legacy_semantic_rendering() {
        let typed = Error::uniqueness_violation("p_id");
        assert_eq!(
            typed.to_string(),
            Error::semantic("ConstraintValidationFailed: `p_id` uniqueness").to_string()
        );
        assert_eq!(
            typed.to_string(),
            "semantic error: ConstraintValidationFailed: `p_id` uniqueness"
        );
        assert!(matches!(
            &typed,
            Error::UniquenessViolation { name } if name == "p_id"
        ));
        assert_eq!(
            uniqueness_violation_detail("p_id"),
            "ConstraintValidationFailed: `p_id` uniqueness"
        );
    }

    #[test]
    fn typed_detail_variants_display_matches_the_legacy_renderings() {
        let typed = Error::invalid_argument_type("round value must be numeric");
        assert_eq!(
            typed.to_string(),
            Error::ty("InvalidArgumentType: round value must be numeric").to_string()
        );
        assert_eq!(
            typed.to_string(),
            "type error: InvalidArgumentType: round value must be numeric"
        );
        assert!(
            matches!(&typed, Error::InvalidArgumentType(d) if d == "round value must be numeric")
        );

        let typed = Error::type_error("schema name must be STRING");
        assert_eq!(
            typed.to_string(),
            Error::ty("TypeError: schema name must be STRING").to_string()
        );
        assert_eq!(
            typed.to_string(),
            "type error: TypeError: schema name must be STRING"
        );
        assert!(matches!(&typed, Error::TypeError(d) if d == "schema name must be STRING"));

        let typed = Error::parameter_missing("parameter $n was not supplied");
        assert_eq!(
            typed.to_string(),
            Error::semantic("ParameterMissing: parameter $n was not supplied").to_string()
        );
        assert_eq!(
            typed.to_string(),
            "semantic error: ParameterMissing: parameter $n was not supplied"
        );
        assert!(
            matches!(&typed, Error::ParameterMissing(d) if d == "parameter $n was not supplied")
        );
    }

    /// ADR-0307: the three typed variants render exactly as the stringly
    /// errors they replace.
    #[test]
    fn adr_0307_variants_display_matches_the_legacy_renderings() {
        let cases = [
            (
                Error::arithmetic(ArithmeticFailure::DivisionByZero),
                Error::ty("/ by zero"),
            ),
            (
                Error::arithmetic(ArithmeticFailure::ModuloByZero),
                Error::ty("% by zero"),
            ),
            (
                Error::arithmetic(ArithmeticFailure::RoundingNecessary),
                Error::execution("ArithmeticError: rounding necessary"),
            ),
            (
                Error::database_not_found("nosuchdb"),
                Error::semantic("DatabaseNotFound: Graph not found: nosuchdb"),
            ),
            (Error::cancelled(), Error::execution("query cancelled")),
        ];
        for (typed, legacy) in cases {
            assert_eq!(typed.to_string(), legacy.to_string(), "{typed:?}");
        }
        assert_eq!(Error::cancelled(), Error::Cancelled);
        assert_eq!(Error::cancelled().to_string(), "query cancelled");
        assert_eq!(
            Error::database_not_found("x").to_string(),
            "semantic error: DatabaseNotFound: Graph not found: x"
        );
        assert_eq!(ArithmeticFailure::DivisionByZero.detail(), "/ by zero");
        assert_eq!(ArithmeticFailure::ModuloByZero.detail(), "% by zero");
        assert_eq!(
            ArithmeticFailure::RoundingNecessary.detail(),
            "ArithmeticError: rounding necessary"
        );
        assert_eq!(
            Error::database_not_found("x")
                .typed_detail_text()
                .as_deref(),
            Some("DatabaseNotFound: Graph not found: x")
        );
        assert_eq!(Error::cancelled().typed_detail(), None);
        assert_eq!(
            Error::arithmetic(ArithmeticFailure::DivisionByZero).typed_detail(),
            None
        );
    }

    /// ADR-0338: the inner type renders its bare payload; the `Error`
    /// variant composes the legacy full text from the shared Type prefix.
    #[test]
    fn arithmetic_failure_display_is_the_bare_payload() {
        assert_eq!(ArithmeticFailure::DivisionByZero.to_string(), "/ by zero");
        assert_eq!(ArithmeticFailure::ModuloByZero.to_string(), "% by zero");
        assert_eq!(
            ArithmeticFailure::RoundingNecessary.to_string(),
            "ArithmeticError: rounding necessary"
        );
        assert_eq!(
            Error::arithmetic(ArithmeticFailure::DivisionByZero).to_string(),
            "type error: / by zero"
        );
        assert_eq!(
            Error::ty("anything").to_string(),
            format!("{TYPE_ERROR_PREFIX}: anything")
        );
    }

    /// ADR-0335: the one spelling of the unbound-variable error.
    #[test]
    fn undefined_variable_renders_like_the_front_end_error() {
        let error = Error::undefined_variable("x");
        assert_eq!(
            error.to_string(),
            "syntax error: UndefinedVariable: variable 'x' not defined"
        );
        assert!(matches!(error, Error::Syntax(_)));
    }

    /// ADR-0308: the one spelling of the missing-parameter message.
    #[test]
    fn parameters_missing_names_every_parameter_in_order() {
        let one = Error::parameters_missing(["p"]);
        assert_eq!(one, Error::parameter_missing("Expected parameter(s): p"));
        assert_eq!(
            one.to_string(),
            "semantic error: ParameterMissing: Expected parameter(s): p"
        );
        assert_eq!(
            Error::parameters_missing(["b", "a"]).to_string(),
            "semantic error: ParameterMissing: Expected parameter(s): b, a"
        );
    }

    #[test]
    fn procedure_not_found_display_matches_the_legacy_syntax_rendering() {
        let typed = Error::procedure_not_found("dbms.noSuchProc");
        assert_eq!(
            typed.to_string(),
            Error::syntax("ProcedureNotFound: dbms.noSuchProc").to_string()
        );
        assert_eq!(
            typed.to_string(),
            "syntax error: ProcedureNotFound: dbms.noSuchProc"
        );
        assert!(matches!(
            &typed,
            Error::ProcedureNotFound { name } if name == "dbms.noSuchProc"
        ));
    }

    #[test]
    fn typed_detail_single_sources_the_class_tokens() {
        fn detail_text(error: &Error) -> Option<(String, String)> {
            error
                .typed_detail()
                .map(|(token, detail)| (token.to_string(), detail.into_owned()))
        }
        let typed = Error::invalid_argument_type("round value must be numeric");
        assert_eq!(
            detail_text(&typed),
            Some((
                "InvalidArgumentType".to_string(),
                "round value must be numeric".to_string()
            ))
        );
        assert_eq!(
            typed.typed_detail_text().as_deref(),
            Some("InvalidArgumentType: round value must be numeric")
        );
        let typed = Error::type_error("schema name must be STRING");
        assert_eq!(
            typed.typed_detail_text().as_deref(),
            Some("TypeError: schema name must be STRING")
        );
        let typed = Error::parameter_missing("parameter $n was not supplied");
        assert_eq!(
            typed.typed_detail_text().as_deref(),
            Some("ParameterMissing: parameter $n was not supplied")
        );
        // The two structured-field variants fit the same (token, detail)
        // shape; the uniqueness detail is the owned `` `<name>` uniqueness ``
        // composition the Display attribute spells inline.
        let typed = Error::procedure_not_found("dbms.noSuchProc");
        assert_eq!(
            detail_text(&typed),
            Some((
                "ProcedureNotFound".to_string(),
                "dbms.noSuchProc".to_string()
            ))
        );
        assert_eq!(
            typed.typed_detail_text().as_deref(),
            Some("ProcedureNotFound: dbms.noSuchProc")
        );
        let typed = Error::uniqueness_violation("p_id");
        assert_eq!(
            detail_text(&typed),
            Some((
                "ConstraintValidationFailed".to_string(),
                "`p_id` uniqueness".to_string()
            ))
        );
        assert_eq!(
            typed.typed_detail_text().as_deref(),
            Some(uniqueness_violation_detail("p_id").as_str())
        );
        assert_eq!(Error::ty("anything").typed_detail(), None);
        assert_eq!(Error::syntax("anything").typed_detail_text(), None);
    }
}
