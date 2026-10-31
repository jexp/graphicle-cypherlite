//! Typed engine errors for CypherLite.
//!
//! This crate hosts [`EngineError`], the typed error channel that replaces the
//! engine's historical `Result<_, String>`. It depends on `cypherlite-parser`
//! (for [`ParseError`]). See `docs/adr/0003-tck-named-error-kinds.md`.
//!
//! # The prime invariant
//!
//! `Display` output is preserved verbatim against the pre-refactor stringly
//! errors: a parse-time failure renders exactly as `ParseError`'s
//! `"parse error: …"`, a storage failure exactly as `StorageError`'s message,
//! and a semantic/runtime failure exactly as the historical `"Kind: message"`
//! shape. Changing an error *message* is not a breaking change; changing a
//! *kind* is (per ADR 0003).

use std::fmt;

mod storage;

pub use cypherlite_parser::ParseError;
pub use storage::StorageError;

/// One of the eight openCypher TCK error *classes*. The TCK harness
/// asserts an expected class, while [`ErrorKind`] carries the finer-grained
/// detail. Every [`ErrorKind`] maps to exactly one class via
/// [`ErrorKind::tck_class`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TckErrorClass {
    SyntaxError,
    TypeError,
    ArgumentError,
    EntityNotFound,
    SemanticError,
    ProcedureError,
    ParameterMissing,
    ConstraintVerificationFailed,
}

impl TckErrorClass {
    /// The canonical openCypher class name.
    pub fn as_str(&self) -> &'static str {
        match self {
            TckErrorClass::SyntaxError => "SyntaxError",
            TckErrorClass::TypeError => "TypeError",
            TckErrorClass::ArgumentError => "ArgumentError",
            TckErrorClass::EntityNotFound => "EntityNotFound",
            TckErrorClass::SemanticError => "SemanticError",
            TckErrorClass::ProcedureError => "ProcedureError",
            TckErrorClass::ParameterMissing => "ParameterMissing",
            TckErrorClass::ConstraintVerificationFailed => "ConstraintVerificationFailed",
        }
    }

    /// Parses one of the eight canonical openCypher class names back into a
    /// [`TckErrorClass`]; the inverse of [`TckErrorClass::as_str`]. Returns
    /// `None` for any other string.
    ///
    /// The TCK harness uses this to decide whether a `should be raised`
    /// step's `<Class>` slot is one of the eight strict-matchable classes: a
    /// step whose class does not parse here (e.g. cip-eval's `GraphTypeError`,
    /// `SchemaError`, `ConstraintValidationFailed`) falls back to lenient
    /// any-error matching rather than a strict class assertion.
    pub fn from_name(name: &str) -> Option<TckErrorClass> {
        match name {
            "SyntaxError" => Some(TckErrorClass::SyntaxError),
            "TypeError" => Some(TckErrorClass::TypeError),
            "ArgumentError" => Some(TckErrorClass::ArgumentError),
            "EntityNotFound" => Some(TckErrorClass::EntityNotFound),
            "SemanticError" => Some(TckErrorClass::SemanticError),
            "ProcedureError" => Some(TckErrorClass::ProcedureError),
            "ParameterMissing" => Some(TckErrorClass::ParameterMissing),
            "ConstraintVerificationFailed" => Some(TckErrorClass::ConstraintVerificationFailed),
            _ => None,
        }
    }
}

impl fmt::Display for TckErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Generates [`ErrorKind`] plus its `as_str`/`from_name`/`tck_class` tables
/// from a single `Variant => TckClass` list, so the three stay in lockstep.
/// The catch-all `Uncategorized` variant is appended automatically; it is the
/// documented escape hatch for legacy strings that carry no recognized kind
/// prefix, and it renders with no `"Kind: "` prefix at all (see
/// [`EngineError`]'s `Display`).
macro_rules! error_kinds {
    ($($variant:ident => $class:ident),+ $(,)?) => {
        /// The specific, TCK-aligned name of an engine error. `kind()` returns
        /// the most specific name available; nested detail (e.g. the
        /// `UnsupportedIndexOrConstraint` in `"SyntaxError:
        /// UnsupportedIndexOrConstraint: …"`) stays in the message.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ErrorKind {
            $($variant,)+
            /// Escape hatch for legacy/opaque errors with no recognized kind
            /// prefix. Renders without a prefix so the raw message shows
            /// through byte-for-byte.
            Uncategorized,
        }

        impl ErrorKind {
            /// The canonical string form of this kind — the inverse of
            /// [`ErrorKind::from_name`] for every non-`Uncategorized` variant.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(ErrorKind::$variant => stringify!($variant),)+
                    ErrorKind::Uncategorized => "Uncategorized",
                }
            }

            /// Parses a kind name back into an [`ErrorKind`]. Returns `None`
            /// for unrecognized names (including `"Uncategorized"`, which is
            /// never produced by parsing).
            pub fn from_name(name: &str) -> Option<ErrorKind> {
                match name {
                    $(stringify!($variant) => Some(ErrorKind::$variant),)+
                    _ => None,
                }
            }

            /// The openCypher TCK error *class* this kind belongs to. Total
            /// over every variant.
            pub fn tck_class(&self) -> TckErrorClass {
                match self {
                    $(ErrorKind::$variant => TckErrorClass::$class,)+
                    ErrorKind::Uncategorized => TckErrorClass::SemanticError,
                }
            }
        }
    };
}

// The ~60 error-kind prefixes already emitted by the engine (derived by
// grepping the `"Word:"` error-literal pattern), plus a few kinds used to
// classify `StorageError` in `EngineError::kind`. The `=> Class` mapping to
// the eight TCK classes is a best-effort taxonomy refined
// against the real TCK error scenarios; it never affects `Display` bytes.
error_kinds! {
    // --- Syntax / structural / scope / aggregation validation ---
    SyntaxError => SyntaxError,
    UnexpectedSyntax => SyntaxError,
    IncompleteQuery => SyntaxError,
    InvalidPatternCharacter => SyntaxError,
    InvalidLabelExpression => SyntaxError,
    InvalidNodePatternPredicate => SyntaxError,
    InvalidClauseComposition => SyntaxError,
    InvalidAggregation => SyntaxError,
    AmbiguousAggregationExpression => SyntaxError,
    NestedAggregation => SyntaxError,
    NegativeIntegerArgument => SyntaxError,
    NoSingleRelationshipType => SyntaxError,
    DifferentColumnsInUnion => SyntaxError,
    VariableAlreadyBound => SyntaxError,
    VariableAlreadyDefined => SyntaxError,
    VariableTypeConflict => SyntaxError,
    VariableShadowing => SyntaxError,
    UndefinedVariable => SyntaxError,
    UnknownFunction => SyntaxError,
    NoExpressionAlias => SyntaxError,
    NoVariablesInScope => SyntaxError,
    RelationshipUniquenessViolation => SyntaxError,
    CreatingVarLength => SyntaxError,
    RequiresDirectedRelationship => SyntaxError,
    InvalidParameterUse => SyntaxError,
    ColumnNameConflict => SyntaxError,
    QuantifiedPathPatternNotSupported => SyntaxError,
    InvalidSelectivePathPatternDependency => SyntaxError,
    GroupVariableReuse => SyntaxError,
    SingletonReusedAsGroup => SyntaxError,
    NonLocalGroupVariableReference => SyntaxError,
    GroupVariableInNonAggregatingOperation => SyntaxError,
    InvalidSearchVariableReference => SyntaxError,
    SelfReferencingSearch => SyntaxError,
    InvalidUseOfCommand => SyntaxError,
    InvalidPlacementOfUseClause => SyntaxError,
    InvalidUpdatingClauseInCountSubquery => SyntaxError,
    InvalidUpdatingClauseInCollectSubquery => SyntaxError,
    NonConstantExpression => SyntaxError,
    InvalidShowColumn => SyntaxError,
    InvalidNumberOfArguments => SyntaxError,
    // Literal-lexing / literal-range failures raised by the parser and surfaced
    // through `EngineError::Syntax`'s `ParseError::detail` tag. All are
    // `SyntaxError`-class compile-time failures in the TCK error grammar
    // (Literals2/3/5/6, Mathematical3, Match4).
    IntegerOverflow => SyntaxError,
    FloatingPointOverflow => SyntaxError,
    InvalidNumberLiteral => SyntaxError,
    InvalidUnicodeLiteral => SyntaxError,
    InvalidUnicodeCharacter => SyntaxError,
    InvalidRelationshipPattern => SyntaxError,
    InvalidArgumentPassingMode => SyntaxError,
    // `DELETE`ing a non-entity that is specifically a label predicate
    // (`DELETE n:Person`, `DELETE r:T`) is a `SyntaxError: InvalidDelete`
    // (Delete1/Delete2); a non-label non-entity expression (`DELETE 1 + 1`)
    // stays `InvalidArgumentType` (Delete5).
    InvalidDelete => SyntaxError,
    // --- Type errors ---
    TypeError => TypeError,
    InvalidArgumentType => TypeError,
    ParameterWrongTypeException => TypeError,
    InvalidPropertyType => TypeError,
    InvalidPropertyValueType => TypeError,
    // Indexing a map with a non-string key (`{name: 'Apa'}[0]`) is a runtime
    // `TypeError: MapElementAccessByNonString` (Map2).
    MapElementAccessByNonString => TypeError,
    // --- Argument value / arithmetic errors ---
    ArgumentError => ArgumentError,
    // The `InvalidArgumentValue` detail is classified as TypeError by both the
    // openCypher TCK (expressions/graph/Graph3-4, typeConversion/TypeConversion1-4)
    // and the in-repo cip-eval suite (63 TypeError vs 0 ArgumentError occurrences).
    InvalidArgumentValue => TypeError,
    ArithmeticError => ArgumentError,
    // A numeric argument outside its permitted range: `range()` step 0,
    // `percentileCont`/`percentileDisc` percentile outside [0,1] — a runtime
    // `ArgumentError: NumberOutOfRange` (List11, Aggregation6).
    NumberOutOfRange => ArgumentError,
    // ADR-0395: a statement's nested evaluation (CALL bodies,
    // EXISTS/COUNT/COLLECT subqueries, FOREACH clause lists, NEXT
    // stages) exceeded the fixed per-statement pass budget — the runtime
    // twin of the compile-side cloned-node budget (ADR-0382). Raised as
    // an `Error::Execution` payload whose `Kind:` prefix names this row.
    NestedEvaluationBudgetExceeded => ArgumentError,
    // --- Parameters ---
    ParameterMissing => ParameterMissing,
    // --- Entity access ---
    EntityNotFound => EntityNotFound,
    DeletedEntityAccess => EntityNotFound,
    // --- Constraints ---
    ConstraintViolation => ConstraintVerificationFailed,
    // Deleting a still-connected node without `DETACH` is a runtime
    // `ConstraintVerificationFailed: DeleteConnectedNode` (Delete1). Distinct
    // from a general `ConstraintViolation` so the finer detail survives.
    DeleteConnectedNode => ConstraintVerificationFailed,
    // --- Procedures ---
    ProcedureError => ProcedureError,
    // Calling a procedure that is not registered is a
    // `ProcedureError: ProcedureNotFound` (Call1).
    ProcedureNotFound => ProcedureError,
    // --- Schema / database / graph-type / data / transaction (semantic) ---
    SchemaError => SemanticError,
    MergeReadOwnWrites => SemanticError,
    DataException => SemanticError,
    DataError => SemanticError,
    DatabaseNotFound => SemanticError,
    DatabaseExists => SemanticError,
    DatabaseError => SemanticError,
    SystemDatabaseRequired => SemanticError,
    GraphTypeNotFound => SemanticError,
    InternalError => SemanticError,
    TransactionError => SemanticError,
    // ADR-0116 §9: a database administration command outside the CE-supported
    // set (Neo4j's `Neo.ClientError.Statement.UnsupportedAdministrationCommand`,
    // GQLSTATUS 51N27).
    UnsupportedAdministrationCommand => SemanticError,
    // ADR-0116 §9: a write against the read-only `system_ontologies`
    // namespace (GQLSTATUS 42N18).
    WriteOnReadOnlyAccessDatabase => SemanticError,
}

/// The matcher-facing detail API. A handful of openCypher-TCK error *detail*
/// names differ from the corresponding [`ErrorKind`] variant name. Rather than
/// rename the variant (which would break `Display` byte-identity, per ADR 0003)
/// or thread a second name through the `error_kinds!` macro, the alias is
/// handled here so `as_str`/`from_name`/`tck_class` stay a canonical trio in
/// lockstep and the TCK harness uses this pair for detail matching.
///
/// Currently one alias exists:
///
/// | `ErrorKind` variant | Display / `as_str` | TCK detail (`tck_detail`) |
/// |---------------------|--------------------|---------------------------|
/// | [`ErrorKind::ParameterMissing`] | `ParameterMissing` | `MissingParameter` |
impl ErrorKind {
    /// The openCypher-TCK error *detail* name for this kind — what the harness
    /// compares against the `<Detail>` slot of `a <Class> should be raised at
    /// <phase>: <Detail>`. Equal to [`ErrorKind::as_str`] for every kind except
    /// the aliased ones above. Never used for `Display`.
    pub fn tck_detail(&self) -> &'static str {
        match self {
            // The TCK spells our `ParameterMissing` as `MissingParameter`
            // (clauses/call/Call1 scenario 11). `as_str` stays `ParameterMissing`
            // for Display/`from_name` round-trip compatibility.
            ErrorKind::ParameterMissing => "MissingParameter",
            other => other.as_str(),
        }
    }

    /// Parses a TCK error-*detail* name into an [`ErrorKind`] — the inverse of
    /// [`ErrorKind::tck_detail`]. Handles the aliases first, then delegates to
    /// [`ErrorKind::from_name`]. Returns `None` for unrecognized names.
    pub fn from_tck_detail(name: &str) -> Option<ErrorKind> {
        match name {
            "MissingParameter" => Some(ErrorKind::ParameterMissing),
            other => ErrorKind::from_name(other),
        }
    }
}

/// The two `COUNT { … }` statement-shape UNION rule message bodies (CIP-66),
/// single-sourced because two layers raise them byte-identically after
/// [`ErrorKind::DifferentColumnsInUnion`]'s name: the front's validate
/// battery (`validate_count_subquery_union_shapes`, rejecting before
/// lowering) and the executor's per-row defense-in-depth scan
/// (`eval_count_query`, for callers that bypass validation). The rendered
/// text is pinned contract (ADR-0003's message immutability rule).
pub const COUNT_UNION_MIX_MESSAGE: &str = "cannot mix explicit and omitted RETURN";
pub const COUNT_UNION_OMITTED_MESSAGE: &str = "omitted RETURN requires UNION ALL";

/// The typed engine error channel. Per ADR 0003 it has four variants:
///
/// - [`EngineError::Syntax`] wraps a parser [`ParseError`] verbatim.
/// - [`EngineError::Semantic`] is a validation-time failure carrying an
///   [`ErrorKind`] and message.
/// - [`EngineError::Runtime`] is a row-evaluation-time failure carrying an
///   [`ErrorKind`] and message.
/// - [`EngineError::Storage`] wraps a [`StorageError`] verbatim.
///
/// `Display` reproduces the pre-refactor strings byte-for-byte (see the crate
/// docs). `kind()` returns the finest TCK-aligned name; `tck_class()` returns
/// the coarser TCK class.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineError {
    /// A parse-time failure. Renders as `ParseError`'s `"parse error: …"`.
    Syntax(ParseError),
    /// A semantic (validation-time) failure.
    Semantic { kind: ErrorKind, message: String },
    /// A runtime (row-evaluation-time) failure.
    Runtime { kind: ErrorKind, message: String },
    /// A storage-layer failure. Renders as `StorageError`'s message.
    Storage(StorageError),
}

impl EngineError {
    /// Construct a semantic error.
    pub fn semantic(kind: ErrorKind, message: impl Into<String>) -> Self {
        EngineError::Semantic {
            kind,
            message: message.into(),
        }
    }

    /// Construct a runtime error.
    pub fn runtime(kind: ErrorKind, message: impl Into<String>) -> Self {
        EngineError::Runtime {
            kind,
            message: message.into(),
        }
    }

    /// The finest TCK-aligned [`ErrorKind`] for this error.
    ///
    /// A parse failure is a `SyntaxError` *class*, but if its [`ParseError`]
    /// carries a [`detail`](ParseError::detail) tag (e.g. `"IntegerOverflow"`),
    /// that finer kind is surfaced here so the TCK harness can match the
    /// TCK's `<Detail>` slot. The tag is never rendered, so `Display` is
    /// unaffected.
    pub fn kind(&self) -> ErrorKind {
        match self {
            // A parse failure with no finer `detail` tag is the openCypher TCK's
            // generic `UnexpectedSyntax` detail: the corpus has no
            // scenario pinning a bare `SyntaxError` *detail*, while it has 17
            // pinning `UnexpectedSyntax` for exactly these generic
            // "expected/unexpected token" parse errors (Literals7/8, Literals2
            // symbol case, …). Both kinds share the `SyntaxError` TCK *class*, so
            // class-only assertions are unaffected. A tagged parse error still
            // surfaces its finer kind (IntegerOverflow, InvalidNumberLiteral, …).
            EngineError::Syntax(e) => e
                .detail
                .and_then(ErrorKind::from_name)
                .unwrap_or(ErrorKind::UnexpectedSyntax),
            EngineError::Semantic { kind, .. } | EngineError::Runtime { kind, .. } => *kind,
            EngineError::Storage(e) => storage_kind(e),
        }
    }

    /// The openCypher TCK error *class* for this error.
    pub fn tck_class(&self) -> TckErrorClass {
        self.kind().tck_class()
    }
}

/// Maps a [`StorageError`] to the finest [`ErrorKind`] for reporting. `Display`
/// still delegates to `StorageError` itself, so this only affects
/// `kind()`/`tck_class()`, never the rendered message.
fn storage_kind(e: &StorageError) -> ErrorKind {
    match e {
        StorageError::NodeNotFound(_) | StorageError::RelationshipNotFound(_) => {
            ErrorKind::EntityNotFound
        }
        // Deleting a still-connected node without `DETACH` carries the finer
        // `DeleteConnectedNode` detail (TCK clauses/delete/Delete1 scenario 7);
        // a genuine constraint violation keeps `ConstraintViolation`. Both are
        // the `ConstraintVerificationFailed` TCK class.
        StorageError::NodeHasRelationships(_) => ErrorKind::DeleteConnectedNode,
        StorageError::ConstraintViolation(_) => ErrorKind::ConstraintViolation,
        StorageError::IndexAlreadyExists(_)
        | StorageError::IndexNotFound(_)
        | StorageError::ConstraintAlreadyExists(_)
        | StorageError::ConstraintNotFound(_) => ErrorKind::SchemaError,
        StorageError::TransactionsUnsupported
        | StorageError::TransactionAlreadyActive
        | StorageError::NoActiveTransaction
        // A `Conflict` today is the flush-during-transaction guard, which is a
        // transaction-state conflict.
        | StorageError::Conflict(_) => ErrorKind::TransactionError,
        // Filesystem I/O, on-disk corruption, a database already locked by
        // another process, and a write rejected against a read-only snapshot
        // (a historical MVCC checkout) are all database-level operational
        // failures.
        StorageError::Io { .. }
        | StorageError::Corruption(_)
        | StorageError::Locked(_)
        | StorageError::ReadOnly(_) => ErrorKind::DatabaseError,
        StorageError::PersistenceOutcomeIndeterminate(_) | StorageError::Other(_) => {
            ErrorKind::Uncategorized
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Syntax(e) => write!(f, "{e}"),
            EngineError::Storage(e) => write!(f, "{e}"),
            EngineError::Semantic { kind, message } | EngineError::Runtime { kind, message } => {
                if *kind == ErrorKind::Uncategorized {
                    // Escape hatch: reproduce the raw legacy string verbatim.
                    f.write_str(message)
                } else {
                    write!(f, "{}: {}", kind.as_str(), message)
                }
            }
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EngineError::Syntax(e) => Some(e),
            EngineError::Storage(e) => Some(e),
            EngineError::Semantic { .. } | EngineError::Runtime { .. } => None,
        }
    }
}

impl From<ParseError> for EngineError {
    fn from(e: ParseError) -> Self {
        EngineError::Syntax(e)
    }
}

impl From<StorageError> for EngineError {
    fn from(e: StorageError) -> Self {
        EngineError::Storage(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-`Uncategorized` kind used across the engine, for exhaustive
    /// round-trip coverage. Kept in sync with the `error_kinds!` list.
    const ALL_KINDS: &[ErrorKind] = &[
        ErrorKind::SyntaxError,
        ErrorKind::UnexpectedSyntax,
        ErrorKind::IncompleteQuery,
        ErrorKind::InvalidPatternCharacter,
        ErrorKind::InvalidLabelExpression,
        ErrorKind::InvalidNodePatternPredicate,
        ErrorKind::InvalidClauseComposition,
        ErrorKind::InvalidAggregation,
        ErrorKind::AmbiguousAggregationExpression,
        ErrorKind::NestedAggregation,
        ErrorKind::NegativeIntegerArgument,
        ErrorKind::NoSingleRelationshipType,
        ErrorKind::DifferentColumnsInUnion,
        ErrorKind::VariableAlreadyBound,
        ErrorKind::VariableAlreadyDefined,
        ErrorKind::VariableTypeConflict,
        ErrorKind::VariableShadowing,
        ErrorKind::UndefinedVariable,
        ErrorKind::UnknownFunction,
        ErrorKind::NoExpressionAlias,
        ErrorKind::NoVariablesInScope,
        ErrorKind::RelationshipUniquenessViolation,
        ErrorKind::CreatingVarLength,
        ErrorKind::RequiresDirectedRelationship,
        ErrorKind::InvalidParameterUse,
        ErrorKind::ColumnNameConflict,
        ErrorKind::QuantifiedPathPatternNotSupported,
        ErrorKind::InvalidSelectivePathPatternDependency,
        ErrorKind::GroupVariableReuse,
        ErrorKind::SingletonReusedAsGroup,
        ErrorKind::NonLocalGroupVariableReference,
        ErrorKind::GroupVariableInNonAggregatingOperation,
        ErrorKind::InvalidSearchVariableReference,
        ErrorKind::SelfReferencingSearch,
        ErrorKind::InvalidUseOfCommand,
        ErrorKind::InvalidPlacementOfUseClause,
        ErrorKind::InvalidUpdatingClauseInCountSubquery,
        ErrorKind::InvalidUpdatingClauseInCollectSubquery,
        ErrorKind::NonConstantExpression,
        ErrorKind::InvalidShowColumn,
        ErrorKind::InvalidNumberOfArguments,
        ErrorKind::IntegerOverflow,
        ErrorKind::FloatingPointOverflow,
        ErrorKind::InvalidNumberLiteral,
        ErrorKind::InvalidUnicodeLiteral,
        ErrorKind::InvalidUnicodeCharacter,
        ErrorKind::InvalidRelationshipPattern,
        ErrorKind::InvalidArgumentPassingMode,
        ErrorKind::InvalidDelete,
        ErrorKind::TypeError,
        ErrorKind::InvalidArgumentType,
        ErrorKind::ParameterWrongTypeException,
        ErrorKind::InvalidPropertyType,
        ErrorKind::InvalidPropertyValueType,
        ErrorKind::MapElementAccessByNonString,
        ErrorKind::ArgumentError,
        ErrorKind::InvalidArgumentValue,
        ErrorKind::ArithmeticError,
        ErrorKind::NumberOutOfRange,
        ErrorKind::ParameterMissing,
        ErrorKind::EntityNotFound,
        ErrorKind::DeletedEntityAccess,
        ErrorKind::ConstraintViolation,
        ErrorKind::DeleteConnectedNode,
        ErrorKind::ProcedureError,
        ErrorKind::ProcedureNotFound,
        ErrorKind::SchemaError,
        ErrorKind::MergeReadOwnWrites,
        ErrorKind::DataException,
        ErrorKind::DataError,
        ErrorKind::DatabaseNotFound,
        ErrorKind::DatabaseExists,
        ErrorKind::DatabaseError,
        ErrorKind::SystemDatabaseRequired,
        ErrorKind::GraphTypeNotFound,
        ErrorKind::InternalError,
        ErrorKind::TransactionError,
        ErrorKind::UnsupportedAdministrationCommand,
        ErrorKind::WriteOnReadOnlyAccessDatabase,
    ];

    #[test]
    fn kind_name_round_trips_through_from_name() {
        for kind in ALL_KINDS {
            let name = kind.as_str();
            assert_eq!(
                ErrorKind::from_name(name),
                Some(*kind),
                "from_name({name:?}) did not round-trip"
            );
        }
    }

    #[test]
    fn tck_error_class_from_name_round_trips() {
        use TckErrorClass as C;
        for class in [
            C::SyntaxError,
            C::TypeError,
            C::ArgumentError,
            C::EntityNotFound,
            C::SemanticError,
            C::ProcedureError,
            C::ParameterMissing,
            C::ConstraintVerificationFailed,
        ] {
            assert_eq!(TckErrorClass::from_name(class.as_str()), Some(class));
        }
        // Classes outside the eight (used by cip-eval error steps) do not parse,
        // which is what steers the harness to lenient fallback for them.
        assert_eq!(TckErrorClass::from_name("GraphTypeError"), None);
        assert_eq!(TckErrorClass::from_name("SchemaError"), None);
        assert_eq!(TckErrorClass::from_name("ConstraintValidationFailed"), None);
        assert_eq!(TckErrorClass::from_name(""), None);
    }

    #[test]
    fn uncategorized_is_not_parseable_and_has_a_name() {
        assert_eq!(ErrorKind::from_name("Uncategorized"), None);
        assert_eq!(ErrorKind::Uncategorized.as_str(), "Uncategorized");
    }

    #[test]
    fn from_name_rejects_unknown_prefixes() {
        assert_eq!(ErrorKind::from_name("storage is unavailable"), None);
        assert_eq!(ErrorKind::from_name("constraint violation"), None);
        assert_eq!(ErrorKind::from_name(""), None);
    }

    #[test]
    fn tck_class_is_total_and_stable() {
        // Total mapping: every kind (including Uncategorized) resolves.
        for kind in ALL_KINDS {
            let _ = kind.tck_class();
        }
        assert_eq!(
            ErrorKind::Uncategorized.tck_class(),
            TckErrorClass::SemanticError
        );
        // Spot-check a few representative mappings.
        assert_eq!(
            ErrorKind::SyntaxError.tck_class(),
            TckErrorClass::SyntaxError
        );
        assert_eq!(ErrorKind::TypeError.tck_class(), TckErrorClass::TypeError);
        assert_eq!(
            ErrorKind::InvalidArgumentValue.tck_class(),
            TckErrorClass::TypeError
        );
        assert_eq!(
            ErrorKind::ParameterMissing.tck_class(),
            TckErrorClass::ParameterMissing
        );
        assert_eq!(
            ErrorKind::EntityNotFound.tck_class(),
            TckErrorClass::EntityNotFound
        );
        assert_eq!(
            ErrorKind::ConstraintViolation.tck_class(),
            TckErrorClass::ConstraintVerificationFailed
        );
        assert_eq!(
            ErrorKind::ProcedureError.tck_class(),
            TckErrorClass::ProcedureError
        );
    }

    #[test]
    fn display_of_semantic_and_runtime_is_kind_colon_message() {
        for kind in ALL_KINDS {
            let sem = EngineError::semantic(*kind, "the message");
            let run = EngineError::runtime(*kind, "the message");
            let expected = format!("{}: the message", kind.as_str());
            assert_eq!(sem.to_string(), expected);
            assert_eq!(run.to_string(), expected);
            assert_eq!(sem.kind(), *kind);
            assert_eq!(run.kind(), *kind);
        }
    }

    #[test]
    fn display_of_uncategorized_has_no_prefix() {
        let err = EngineError::runtime(ErrorKind::Uncategorized, "raw legacy text: with colon");
        assert_eq!(err.to_string(), "raw legacy text: with colon");
        let err = EngineError::semantic(ErrorKind::Uncategorized, "plain");
        assert_eq!(err.to_string(), "plain");
    }

    #[test]
    fn syntax_display_matches_parse_error_verbatim() {
        let pe = ParseError::new("unexpected token", 3, 7);
        let flattened = pe.to_string();
        let err: EngineError = pe.into();
        assert_eq!(err.to_string(), flattened);
        // An untagged parse error is the generic `UnexpectedSyntax` detail,
        // still of the `SyntaxError` TCK class.
        assert_eq!(err.kind(), ErrorKind::UnexpectedSyntax);
        assert_eq!(err.tck_class(), TckErrorClass::SyntaxError);
        assert!(err.to_string().starts_with("parse error: "));
    }

    #[test]
    fn syntax_display_matches_parse_error_with_caret() {
        let pe = ParseError::new("boom", 1, 5).with_source("RETURN oops");
        let flattened = pe.to_string();
        let err: EngineError = pe.into();
        assert_eq!(err.to_string(), flattened);
    }

    #[test]
    fn storage_display_matches_storage_error_verbatim() {
        let cases = [
            StorageError::NodeNotFound(42),
            StorageError::RelationshipNotFound(7),
            StorageError::NodeHasRelationships(1),
            StorageError::ConstraintViolation("dup name".to_string()),
            StorageError::IndexAlreadyExists("idx".to_string()),
            StorageError::IndexNotFound("idx".to_string()),
            StorageError::ConstraintAlreadyExists("c".to_string()),
            StorageError::ConstraintNotFound("c".to_string()),
            StorageError::TransactionsUnsupported,
            StorageError::TransactionAlreadyActive,
            StorageError::NoActiveTransaction,
            StorageError::PersistenceOutcomeIndeterminate("boom".to_string()),
            StorageError::Io {
                kind: std::io::ErrorKind::NotFound,
                message: "failed to read /db: No such file or directory".to_string(),
            },
            StorageError::Corruption("failed to parse JSON: expected value".to_string()),
            StorageError::Conflict("cannot flush while a transaction is active".to_string()),
            StorageError::Locked("/tmp/graph.db".to_string()),
            StorageError::ReadOnly("database is checked out at version 1".to_string()),
            StorageError::Other("misc io".to_string()),
        ];
        for se in cases {
            let flattened = se.to_string();
            let err: EngineError = se.clone().into();
            assert_eq!(err.to_string(), flattened, "storage Display drifted");
        }
    }

    #[test]
    fn locked_display_names_the_path() {
        let err = StorageError::Locked("/tmp/graph.db".to_string());
        assert_eq!(
            err.to_string(),
            "database is locked by another process: /tmp/graph.db"
        );
    }

    #[test]
    fn storage_kind_and_class_mapping() {
        assert_eq!(
            EngineError::Storage(StorageError::NodeNotFound(1)).kind(),
            ErrorKind::EntityNotFound
        );
        assert_eq!(
            EngineError::Storage(StorageError::ConstraintViolation("x".into())).kind(),
            ErrorKind::ConstraintViolation
        );
        assert_eq!(
            EngineError::Storage(StorageError::NodeHasRelationships(1)).kind(),
            ErrorKind::DeleteConnectedNode
        );
        // Both the connected-node and generic-constraint storage errors carry
        // the ConstraintVerificationFailed TCK class, but distinct details.
        assert_eq!(
            EngineError::Storage(StorageError::NodeHasRelationships(1)).tck_class(),
            TckErrorClass::ConstraintVerificationFailed
        );
        assert_eq!(
            EngineError::Storage(StorageError::IndexNotFound("i".into())).kind(),
            ErrorKind::SchemaError
        );
        assert_eq!(
            EngineError::Storage(StorageError::NoActiveTransaction).kind(),
            ErrorKind::TransactionError
        );
        assert_eq!(
            EngineError::Storage(StorageError::Other("x".into())).kind(),
            ErrorKind::Uncategorized
        );
        // New structured storage variants.
        assert_eq!(
            EngineError::Storage(StorageError::Io {
                kind: std::io::ErrorKind::NotFound,
                message: "failed to read".into(),
            })
            .kind(),
            ErrorKind::DatabaseError
        );
        assert_eq!(
            EngineError::Storage(StorageError::Corruption("bad json".into())).kind(),
            ErrorKind::DatabaseError
        );
        assert_eq!(
            EngineError::Storage(StorageError::Locked("/db".into())).kind(),
            ErrorKind::DatabaseError
        );
        assert_eq!(
            EngineError::Storage(StorageError::Conflict("txn".into())).kind(),
            ErrorKind::TransactionError
        );
        // The database-operational kinds fall under the SemanticError TCK class.
        assert_eq!(
            EngineError::Storage(StorageError::Locked("/db".into())).tck_class(),
            TckErrorClass::SemanticError
        );
        assert_eq!(
            EngineError::Storage(StorageError::Conflict("txn".into())).tck_class(),
            TckErrorClass::SemanticError
        );
        assert_eq!(
            EngineError::Storage(StorageError::NodeNotFound(1)).tck_class(),
            TckErrorClass::EntityNotFound
        );
    }

    #[test]
    fn from_impls_wrap_verbatim() {
        let pe = ParseError::new("x", 1, 1);
        assert!(matches!(EngineError::from(pe), EngineError::Syntax(_)));
        let se = StorageError::Other("y".into());
        assert!(matches!(EngineError::from(se), EngineError::Storage(_)));
    }

    /// The Phase-6 detail gaps: every new ErrorKind variant maps to the TCK
    /// error *class* that the corresponding scenarios expect (verified against
    /// the `<Class>` slot of the `should be raised` steps in testdata/tck).
    #[test]
    fn phase6_detail_kinds_have_expected_tck_class() {
        use ErrorKind as K;
        use TckErrorClass as C;
        let cases: &[(ErrorKind, TckErrorClass)] = &[
            (K::IntegerOverflow, C::SyntaxError),
            (K::FloatingPointOverflow, C::SyntaxError),
            (K::InvalidNumberLiteral, C::SyntaxError),
            (K::InvalidUnicodeLiteral, C::SyntaxError),
            (K::InvalidUnicodeCharacter, C::SyntaxError),
            (K::InvalidRelationshipPattern, C::SyntaxError),
            (K::InvalidArgumentPassingMode, C::SyntaxError),
            (K::InvalidDelete, C::SyntaxError),
            (K::MapElementAccessByNonString, C::TypeError),
            (K::NumberOutOfRange, C::ArgumentError),
            (K::DeleteConnectedNode, C::ConstraintVerificationFailed),
            (K::ProcedureNotFound, C::ProcedureError),
        ];
        for (kind, class) in cases {
            assert_eq!(kind.tck_class(), *class, "{} class drifted", kind.as_str());
            // as_str/from_name round-trip is total for the new variants too.
            assert_eq!(ErrorKind::from_name(kind.as_str()), Some(*kind));
        }
    }

    /// The `ParameterMissing`/`MissingParameter` alias: the variant keeps its
    /// `as_str`/`Display`/`from_name` identity (byte-stable), while the
    /// matcher-facing `tck_detail`/`from_tck_detail` pair speaks the TCK's
    /// `MissingParameter` spelling.
    #[test]
    fn missing_parameter_alias_round_trips_without_touching_display() {
        // Canonical identity is unchanged.
        assert_eq!(ErrorKind::ParameterMissing.as_str(), "ParameterMissing");
        assert_eq!(
            ErrorKind::from_name("ParameterMissing"),
            Some(ErrorKind::ParameterMissing)
        );
        // `MissingParameter` is NOT a canonical name.
        assert_eq!(ErrorKind::from_name("MissingParameter"), None);
        // But it IS the TCK detail, reachable through the matcher-facing API.
        assert_eq!(ErrorKind::ParameterMissing.tck_detail(), "MissingParameter");
        assert_eq!(
            ErrorKind::from_tck_detail("MissingParameter"),
            Some(ErrorKind::ParameterMissing)
        );
        // Display is still the canonical spelling.
        assert_eq!(
            EngineError::semantic(ErrorKind::ParameterMissing, "boom").to_string(),
            "ParameterMissing: boom"
        );
    }

    /// `tck_detail` equals `as_str` for every non-aliased kind, and
    /// `from_tck_detail` is its inverse (delegating to `from_name`).
    #[test]
    fn tck_detail_matches_as_str_except_aliases() {
        for kind in ALL_KINDS {
            if *kind == ErrorKind::ParameterMissing {
                assert_eq!(kind.tck_detail(), "MissingParameter");
                continue;
            }
            assert_eq!(kind.tck_detail(), kind.as_str());
            assert_eq!(ErrorKind::from_tck_detail(kind.tck_detail()), Some(*kind));
        }
        assert_eq!(ErrorKind::from_tck_detail("NoSuchDetail"), None);
    }

    /// A parse error tagged with a detail surfaces that finer kind through
    /// `kind()`/`tck_class()` while `Display` stays byte-for-byte identical to
    /// the untagged parse error (the prime invariant).
    #[test]
    fn tagged_parse_error_surfaces_detail_without_changing_display() {
        // Integer overflow.
        let untagged = ParseError::new("integer overflow: 9223372036854775808", 1, 8);
        let tagged = ParseError::new("integer overflow: 9223372036854775808", 1, 8)
            .with_detail("IntegerOverflow");
        let untagged_err: EngineError = untagged.into();
        let tagged_err: EngineError = tagged.into();
        assert_eq!(untagged_err.to_string(), tagged_err.to_string());
        // Untagged → the generic `UnexpectedSyntax` detail.
        assert_eq!(untagged_err.kind(), ErrorKind::UnexpectedSyntax);
        assert_eq!(tagged_err.kind(), ErrorKind::IntegerOverflow);
        assert_eq!(tagged_err.tck_class(), TckErrorClass::SyntaxError);

        // Floating-point overflow.
        let fp: EngineError =
            ParseError::new("FloatingPointOverflow: float literal is too large", 1, 8)
                .with_detail("FloatingPointOverflow")
                .into();
        assert_eq!(fp.kind(), ErrorKind::FloatingPointOverflow);
        assert_eq!(fp.tck_class(), TckErrorClass::SyntaxError);

        // An unrecognized/garbage tag falls back to the generic
        // `UnexpectedSyntax` kind (SyntaxError class), never panics.
        let bogus: EngineError = ParseError::new("boom", 1, 1)
            .with_detail("NotARealKind")
            .into();
        assert_eq!(bogus.kind(), ErrorKind::UnexpectedSyntax);
        assert_eq!(bogus.tck_class(), TckErrorClass::SyntaxError);
    }
}
