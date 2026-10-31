//! Canonical built-in procedure signature registry: the single table of
//! argument/return-column names and types for every procedure the engine
//! executes or lists.
//!
//! Five consumers share this table so they cannot drift apart (e.g. the
//! vector `query` argument typed `ANY` in one place and `LIST<FLOAT>` in
//! another):
//!
//! - `exec::procedure_rows::builtin_procedure` — the executor's `CALL`
//!   dispatch (arg names/types, yield columns, arity);
//! - `cypherlite::Database`'s compile-time `validate_calls` (same shapes);
//! - `cypherlite`'s namespace write classification (`writes`, ADR-0116 §4);
//! - `show_catalog::procedure_catalog_rows` — the `SHOW PROCEDURES` catalog
//!   (CIP-84/CIP-113/CIP-157 display, which additionally reads the
//!   description/mode/deprecation fields);
//! - `procedure_gate::procedure_preview_feature` — the preview-feature
//!   mapping of the `db.ontology.*` family (ADR-0378).

use crate::catalog_meta::{DeprecatedBy, ProcArgMeta};

/// Static signature and catalog metadata of a built-in procedure.
pub struct ProcSignature {
    pub name: &'static str,
    pub description: &'static str,
    pub mode: &'static str,
    pub works_on_system: bool,
    pub admin: bool,
    /// Formal parameters. Required parameters come first; a parameter with
    /// a `default` may be omitted by the caller (CIP-113's
    /// `name = default :: type` rendering). Crate-internal (the parameter
    /// type is); a cross-crate caller reads [`Self::parameters`].
    pub(crate) args: &'static [ProcArgMeta],
    /// Return columns (`YIELD` names/types). Crate-internal; a cross-crate
    /// caller reads [`Self::return_columns`].
    pub(crate) returns: &'static [ProcArgMeta],
    /// The replacement when the procedure is deprecated (CIP-157's
    /// `deprecatedBy`); `None` means not deprecated. One fact, so a bool and
    /// its replacement cannot disagree.
    pub deprecated_by: Option<DeprecatedBy>,
    /// True when `CALL` can actually execute it (as opposed to a
    /// catalog-only entry pinned by CIP-84/CIP-113/CIP-157 scenarios that no
    /// scenario calls).
    pub executable: bool,
    /// True when `SHOW PROCEDURES` lists it.
    pub listed: bool,
    /// True when a `CALL` mutates the graph or catalog (ADR-0116 §4) — the
    /// write classification `cypherlite`'s namespace rules read, so there is
    /// one table (`mode` does not encode this: DBMS-mode entries such as
    /// `dbms.components` and `tx.setMetaData` are not graph/catalog writes).
    pub writes: bool,
    /// True when the procedure's commit is a store-level commit of its own
    /// (ADR-0120 §3) — the `db.ontology.set` / `db.ontology.drop` store ops,
    /// which push a store-level redo and require the implicit-only /
    /// no-mixed-writes gates; the in-overlay `db.ontology.apply` write is
    /// not one. The store-op classification reads this table the same way
    /// the write classification reads `writes`.
    pub store_level: bool,
}

impl ProcSignature {
    /// The common entry: a listed, executable read procedure with no
    /// description, catalog flags or deprecation. The builders below name
    /// only what a row changes from it, so the table states each entry's
    /// deviations instead of restating ten fields per row.
    pub(crate) const fn read(name: &'static str) -> Self {
        Self {
            name,
            description: "",
            mode: "READ",
            works_on_system: false,
            admin: false,
            args: &[],
            returns: &[],
            deprecated_by: None,
            executable: true,
            listed: true,
            writes: false,
            store_level: false,
        }
    }

    /// A listed, executable DBMS procedure: mode `"DBMS"`, works on the
    /// system database.
    pub(crate) const fn dbms(name: &'static str) -> Self {
        let mut sig = Self::read(name);
        sig.mode = "DBMS";
        sig.works_on_system = true;
        sig
    }

    /// A listed, executable write procedure: mode `"WRITE"`.
    pub(crate) const fn write(name: &'static str) -> Self {
        let mut sig = Self::read(name);
        sig.mode = "WRITE";
        sig
    }

    /// The procedure works on the system database.
    pub(crate) const fn works_on_system(mut self) -> Self {
        self.works_on_system = true;
        self
    }

    /// A description for the catalog row.
    pub(crate) const fn described(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// The procedure requires admin privileges.
    pub(crate) const fn admin(mut self) -> Self {
        self.admin = true;
        self
    }

    /// A `CALL` mutates the graph or catalog (ADR-0116 §4).
    pub(crate) const fn writes(mut self) -> Self {
        self.writes = true;
        self
    }

    /// The procedure's commit is a store-level commit of its own (ADR-0120 §3).
    pub(crate) const fn store_level(mut self) -> Self {
        self.store_level = true;
        self
    }

    /// The procedure's formal parameters.
    pub(crate) const fn args(mut self, args: &'static [ProcArgMeta]) -> Self {
        self.args = args;
        self
    }

    /// The procedure's return columns.
    pub(crate) const fn returns(mut self, returns: &'static [ProcArgMeta]) -> Self {
        self.returns = returns;
        self
    }

    /// The replacement when the procedure is deprecated (CIP-157).
    pub(crate) const fn deprecated_by(mut self, by: DeprecatedBy) -> Self {
        self.deprecated_by = Some(by);
        self
    }

    /// Whether `CALL` can actually execute it.
    pub(crate) const fn executable(mut self, executable: bool) -> Self {
        self.executable = executable;
        self
    }

    /// Whether `SHOW PROCEDURES` lists it.
    pub(crate) const fn listed(mut self, listed: bool) -> Self {
        self.listed = listed;
        self
    }
}

impl ProcSignature {
    /// Number of leading parameters without a default — the minimum arity a
    /// `CALL` may pass. (`args` lists required parameters first.)
    pub fn required_args(&self) -> usize {
        self.args.iter().take_while(|a| a.default.is_none()).count()
    }

    /// The formal parameters as `(name, declared type)` pairs, required
    /// parameters first — a caller outside this crate's view of `args`
    /// (`ProcArgMeta` is crate-internal). The compile-time `CALL`
    /// validation reads this.
    pub fn parameters(&self) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
        self.args.iter().map(|arg| (arg.name, arg.type_str))
    }

    /// The `YIELD` column names, in signature order — a caller outside this
    /// crate's view of `returns`.
    pub fn return_columns(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.returns.iter().map(|ret| ret.name)
    }
}

mod signatures;

pub use signatures::BUILTIN_PROCEDURE_SIGNATURES;

/// Looks up a built-in procedure signature by name, case-sensitively.
/// CIP-230 ("Casing sensitivity"): built-in *function* names are
/// case-insensitive, but built-in *procedure* names are case-sensitive --
/// `CALL DB.LABELS()` is an UnknownProcedure error, only the exact declared
/// casing (`CALL db.labels()`) resolves. The validator must agree.
pub fn builtin_procedure_signature(name: &str) -> Option<&'static ProcSignature> {
    BUILTIN_PROCEDURE_SIGNATURES
        .iter()
        .find(|sig| sig.name == name)
}

/// Whether `name` names one of the `db.ontology.*` procedures (ADR-0121 as
/// amended by ADR-0227). Derived from the one table so the facade's
/// compile-time gates (implicit-only, no-mixed-writes, target namespace)
/// and the executor's `CALL` dispatch cannot drift apart.
pub fn is_ontology_procedure(name: &str) -> bool {
    builtin_procedure_signature(name)
        .is_some_and(|signature| signature.name.starts_with("db.ontology."))
}

/// Whether `name` names a `db.ontology.*` write procedure (`set`, `drop`,
/// `apply` — ADR-0121 as amended by ADR-0227): registered with the
/// prefix AND flagged `writes`, both read from the one table.
pub fn is_ontology_write_procedure(name: &str) -> bool {
    builtin_procedure_signature(name)
        .is_some_and(|signature| signature.writes && signature.name.starts_with("db.ontology."))
}

/// Whether `name` names a store-level ontology op (`db.ontology.set` /
/// `db.ontology.drop`, ADR-0120 §3) as opposed to the in-overlay `apply`
/// write or the `get` read. Derived from the one table the way
/// [`is_ontology_write_procedure`] is: the procedure's signature is flagged
/// [`ProcSignature::store_level`].
pub fn is_ontology_store_op(name: &str) -> bool {
    builtin_procedure_signature(name).is_some_and(|signature| signature.store_level)
}

/// Whether `CALL name` **could** write — the namespace gate's question: a
/// store-registered procedure or a built-in flagged `writes` in the
/// canonical signature registry (ADR-0116 §4). The gate must never admit a
/// callable to a read view, so registration alone counts.
///
/// Its consumer is the facade's namespace write gate (`cypherlite::gate`,
/// which injects this as the `is_write_procedure` closure the planner's
/// `is_write` and `plan` take — the planner cannot depend on this registry).
/// The ADR-0402 effect asks the narrower question, `call_writes_store`.
pub fn call_is_write(graph: &crate::graph::Graph, name: &str) -> bool {
    graph.has_procedure(name)
        || builtin_procedure_signature(name).is_some_and(|signature| signature.writes)
}

/// Whether `CALL name` writes the store — the ADR-0402 effect question. Only
/// a writes-flagged built-in does; a store-registered procedure
/// ([`crate::graph::Procedure`]) is a static row table and cannot mutate the
/// store, so calling it is no write attempt.
pub(crate) fn call_writes_store(name: &str) -> bool {
    builtin_procedure_signature(name).is_some_and(|signature| signature.writes)
}

/// The replacement when `name` resolves to a deprecated built-in procedure,
/// `None` otherwise; the procedure-side twin of
/// [`crate::function_registry::builtin_function_deprecation`] and the one
/// lookup the query-notification detector reads. The registry's
/// `deprecated_by` field is the whole deprecation fact — `None` means not
/// deprecated — so the lookup is a field read, never a bridge between two
/// flags.
pub fn builtin_procedure_deprecation(name: &str) -> Option<DeprecatedBy> {
    builtin_procedure_signature(name)?.deprecated_by
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lookup reads the one registry: a deprecated entry yields its
    /// replacement (display text and syntactic form), a current entry and an
    /// unknown name yield `None`.
    #[test]
    fn procedure_deprecation_lookup_reads_the_registry() {
        assert_eq!(
            builtin_procedure_deprecation("db.index.vector.queryNodes"),
            Some(DeprecatedBy::clause("SEARCH"))
        );
        assert_eq!(
            builtin_procedure_deprecation("cdc.current"),
            Some(DeprecatedBy::identifier("db.cdc.current"))
        );
        assert_eq!(builtin_procedure_deprecation("db.labels"), None);
        assert_eq!(builtin_procedure_deprecation("no.such.procedure"), None);
    }

    #[test]
    fn every_listed_procedure_has_a_signature() {
        // The catalog placeholder path is gone: every `SHOW PROCEDURES`
        // entry renders from this one table.
        let listed = BUILTIN_PROCEDURE_SIGNATURES
            .iter()
            .filter(|s| s.listed)
            .count();
        assert!(listed >= 27, "expected the full listed set, got {listed}");
    }

    #[test]
    fn vector_query_argument_is_list_of_float() {
        for name in [
            "db.index.vector.queryNodes",
            "db.index.vector.queryRelationships",
        ] {
            let sig = builtin_procedure_signature(name).unwrap();
            assert!(sig.executable);
            assert_eq!(sig.args[2].name, "query");
            assert_eq!(sig.args[2].type_str, "LIST<FLOAT>");
        }
    }

    #[test]
    fn lookup_is_case_sensitive() {
        // CIP-230: built-in procedure names are case-sensitive (unlike
        // function names) -- only the exact declared casing resolves, so
        // `CALL DB.LABELS()`/`CALL Db.Labels()` are UnknownProcedure errors.
        assert!(builtin_procedure_signature("db.labels").is_some());
        assert!(builtin_procedure_signature("DB.LABELS").is_none());
        assert!(builtin_procedure_signature("Db.Labels").is_none());
        assert!(builtin_procedure_signature("no.such.procedure").is_none());
    }

    #[test]
    fn required_args_stop_at_first_defaulted_parameter() {
        let await_indexes = builtin_procedure_signature("db.awaitIndexes").unwrap();
        assert_eq!(await_indexes.args.len(), 1);
        assert_eq!(await_indexes.required_args(), 0);
        let labels = builtin_procedure_signature("db.labels").unwrap();
        assert_eq!(labels.required_args(), 0);
        let create_label = builtin_procedure_signature("db.createLabel").unwrap();
        assert_eq!(create_label.required_args(), 1);
    }

    /// ADR-0121/ADR-0227 (CIP-287): the four ontology procedures — no
    /// database argument, `get` with the `"yaml"` default on `format`.
    #[test]
    fn is_ontology_procedure_matches_exactly_the_four() {
        for name in [
            "db.ontology.set",
            "db.ontology.get",
            "db.ontology.drop",
            "db.ontology.apply",
        ] {
            assert!(is_ontology_procedure(name), "{name}");
        }
        for name in [
            "db.labels",
            "db.ontology",
            "DB.ONTOLOGY.SET",
            "db.ontology.getx",
        ] {
            assert!(!is_ontology_procedure(name), "{name}");
        }
    }

    /// The write/store-op predicates derive from the one table: `get` is
    /// not a write, `apply` is a write but not a store op.
    #[test]
    fn ontology_write_and_store_op_predicates_split_the_four() {
        assert!(is_ontology_write_procedure("db.ontology.set"));
        assert!(is_ontology_write_procedure("db.ontology.drop"));
        assert!(is_ontology_write_procedure("db.ontology.apply"));
        assert!(!is_ontology_write_procedure("db.ontology.get"));
        assert!(!is_ontology_write_procedure("db.labels"));

        assert!(is_ontology_store_op("db.ontology.set"));
        assert!(is_ontology_store_op("db.ontology.drop"));
        assert!(!is_ontology_store_op("db.ontology.apply"));
        assert!(!is_ontology_store_op("db.ontology.get"));
        assert!(!is_ontology_store_op("db.createLabel"));
    }

    #[test]
    fn ontology_procedures_follow_cip287() {
        let set = builtin_procedure_signature("db.ontology.set").unwrap();
        assert!(set.executable && set.listed && set.writes);
        assert_eq!(set.mode, "WRITE");
        assert_eq!(
            set.args
                .iter()
                .map(|a| (a.name, a.type_str))
                .collect::<Vec<_>>(),
            [("ontology", "STRING")]
        );
        assert!(set.returns.is_empty());

        let get = builtin_procedure_signature("db.ontology.get").unwrap();
        assert!(get.executable && get.listed && !get.writes);
        assert_eq!(get.mode, "READ");
        assert_eq!(get.args.len(), 1);
        assert_eq!(get.args[0].name, "format");
        assert_eq!(get.args[0].default, Some("\"yaml\""));
        assert_eq!(get.required_args(), 0);
        assert_eq!(
            get.returns
                .iter()
                .map(|r| (r.name, r.type_str))
                .collect::<Vec<_>>(),
            [("ontology", "STRING")]
        );

        for (name, mode) in [
            ("db.ontology.drop", "WRITE"),
            ("db.ontology.apply", "WRITE"),
        ] {
            let sig = builtin_procedure_signature(name).unwrap();
            assert!(sig.executable && sig.listed && sig.writes, "{name}");
            assert_eq!(sig.mode, mode, "{name}");
            assert!(sig.args.is_empty() && sig.returns.is_empty(), "{name}");
        }
    }

    #[test]
    fn writes_flag_marks_exactly_the_graph_or_catalog_mutating_builtins() {
        // ADR-0116 §4: this set is the single source of truth for the
        // write-procedure classification the `system_ontologies` read-only
        // check reads — a name added to or dropped from the table must flip
        // here, not in a second list.
        let writes: Vec<&str> = BUILTIN_PROCEDURE_SIGNATURES
            .iter()
            .filter(|s| s.writes)
            .map(|s| s.name)
            .collect();
        assert_eq!(
            writes,
            [
                "db.createLabel",
                "db.ontology.set",
                "db.ontology.drop",
                "db.ontology.apply",
                "dbms.setConfigValue"
            ]
        );
    }

    /// ADR-0143: the four Browser/Desktop-compat `dbms.*` procedures are
    /// callable but unlisted, with the exact pinned return columns.
    #[test]
    fn adr0143_procedures_are_callable_unlisted_with_pinned_columns() {
        let show_current_user =
            builtin_procedure_signature("dbms.showCurrentUser").expect("registered");
        assert!(show_current_user.executable);
        assert!(!show_current_user.listed);
        assert!(show_current_user.args.is_empty());
        assert_eq!(
            show_current_user
                .returns
                .iter()
                .map(|r| (r.name, r.type_str))
                .collect::<Vec<_>>(),
            [
                ("username", "STRING"),
                ("roles", "LIST<STRING>"),
                ("flags", "LIST<STRING>")
            ]
        );

        // dbms.clientConfig and dbms.listConfig deliberately share one
        // return-column list: same names, same types, same order, no args.
        let client_config = builtin_procedure_signature("dbms.clientConfig").expect("registered");
        let list_config = builtin_procedure_signature("dbms.listConfig").expect("registered");
        assert!(client_config.executable && !client_config.listed);
        assert!(list_config.executable && !list_config.listed);
        assert!(client_config.args.is_empty() && list_config.args.is_empty());
        assert!(std::ptr::eq(client_config.returns, list_config.returns));
        assert_eq!(
            client_config
                .returns
                .iter()
                .map(|r| (r.name, r.type_str))
                .collect::<Vec<_>>(),
            [
                ("name", "STRING"),
                ("description", "STRING"),
                ("value", "STRING"),
                ("dynamic", "BOOLEAN"),
                ("defaultValue", "STRING"),
                ("startupValue", "STRING"),
                ("explicitlySet", "BOOLEAN"),
                ("validValues", "STRING")
            ]
        );

        let license =
            builtin_procedure_signature("dbms.licenseAgreementDetails").expect("registered");
        assert!(license.executable);
        assert!(!license.listed);
        assert!(license.args.is_empty());
        assert_eq!(
            license
                .returns
                .iter()
                .map(|r| (r.name, r.type_str))
                .collect::<Vec<_>>(),
            [
                ("status", "STRING"),
                ("daysLeftOnTrial", "INTEGER"),
                ("totalTrialDays", "INTEGER")
            ]
        );
    }
}
