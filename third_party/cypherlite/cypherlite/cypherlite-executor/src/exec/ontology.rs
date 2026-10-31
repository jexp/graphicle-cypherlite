//! The `db.ontology.*` procedures (ADR-0121 as amended by ADR-0227, the
//! CIP-287 surface): `set`, `get`, `drop`, `apply`. No database argument —
//! each acts on the session/`USE`-target database, which the facade's
//! plan-time gates restrict to `neo4j` before execution.
//!
//! Chosen seams (the executor's `ExecCtx`/`ExecRequest` carry no store
//! handle):
//!
//! - `set`/`drop` push the store-level [`RedoOp::OntologySet`] /
//!   [`RedoOp::OntologyDrop`] into the statement's write overlay, exactly
//!   like a catalog op. The facade's compile-time gates (implicit-only,
//!   no-mixed-writes 42N03, namespace) guarantee the statement commits
//!   nothing else, and the group-commit pipeline
//!   (`cypherlite::ontology::prepare_committed` / `install_committed`)
//!   already turns those ops into the store-level document swap before and
//!   after the WAL append — the same path the doc-hidden
//!   `Database::commit_ontology_set`/`commit_ontology_drop` facades use.
//!   Validation happens here, at execution time, so an invalid document
//!   fails the statement before any WAL write (the WAL preflight's 16 MiB
//!   envelope check backstops it).
//! - `get` is an ordinary read row producer; the facade pins the
//!   document the calling transaction sees on the graph view
//!   (`ExecGraph::with_ontology_document` — begin-time capture in explicit
//!   transactions, admission-time capture auto-commit), so `YIELD`/`RETURN`
//!   compose as for any procedure.
//! - `apply` is a normal in-overlay write procedure: it converts the
//!   stored document with [`cypherlite_ontology::to_graph_type_body`],
//!   runs ADR-0226 §3's validate-first scan against the captured snapshot,
//!   and only then pushes the `ALTER CURRENT GRAPH TYPE SET` redo through
//!   the existing pipeline (`apply_verb` → `derive_constraints_typed` →
//!   `sync_derived_constraints` → `RedoOp::SetCurrentGraphType`).

use std::borrow::Cow;
use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ontology::DocumentFormat;

use crate::ctx::ExecCtx;
use crate::eval::{eval, Row};
use crate::overlay::ExecGraph;
use crate::redo::RedoOp;

use super::procedures::CallSpec;

/// `db.ontology.get`'s one output column, used both as the default-yield
/// name and as the produced row's key so the declared and produced column
/// cannot drift.
const ONTOLOGY_COLUMN: &str = "ontology";

/// The database every stored ontology belongs to (ADR-0121 §2) — the
/// `database` payload the store-level redo ops carry.
const ONTOLOGY_DATABASE: &str = "neo4j";

/// The four `db.ontology.*` procedures (ADR-0121/ADR-0227), classified
/// ONCE at the `exec_call` divert by the single `OntologyProc::try_from`
/// classification below — the divert site's one name-matching scheme —
/// so every downstream dispatch matches this enum exhaustively instead of
/// re-matching name strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OntologyProc {
    Get,
    Set,
    Drop,
    Apply,
}

impl OntologyProc {
    /// Every dispatch arm, in this enum's order (`get`, `set`, `drop`,
    /// `apply`) — deliberately not the registry table's `set`, `get`,
    /// `drop`, `apply` order — the one name list `try_from` matches (each
    /// arm's [`Self::name`]), and the dispatch side of the both-ways pin
    /// against the registry's ontology family.
    const ALL: [Self; 4] = [Self::Get, Self::Set, Self::Drop, Self::Apply];

    /// The registered procedure name this arm dispatches. This is the
    /// dispatch vocabulary's only spelling of the name; the registry
    /// signature of the same name is the runtime contract the divert
    /// checks, so the two cannot drift (pinned by test).
    fn name(self) -> &'static str {
        match self {
            Self::Get => "db.ontology.get",
            Self::Set => "db.ontology.set",
            Self::Drop => "db.ontology.drop",
            Self::Apply => "db.ontology.apply",
        }
    }
}

impl TryFrom<&str> for OntologyProc {
    type Error = ();

    /// Classify a registered `db.ontology.*` name — the divert site's
    /// single classification step. The [`Self::ALL`] scan is the only
    /// membership decision: a name matching one arm's [`Self::name`] is
    /// that arm, and the arm's own name is the dispatch key. `Err(())` for
    /// anything else: the call falls to the generic registry path, where an
    /// unregistered name fails with `ProcedureNotFound` and a
    /// user-registered one runs. The both-ways set test pins `ALL` against
    /// the registry's ontology family, so a registry row added or dropped
    /// without the matching arm fails the suite.
    fn try_from(name: &str) -> std::result::Result<Self, ()> {
        Self::ALL
            .into_iter()
            .find(|proc| proc.name() == name)
            .ok_or(())
    }
}

/// Execute one `db.ontology.*` call over the input rows, keeping the
/// `exec_call` row contract: `get` produces its `ontology` column per row
/// (with YIELD aliasing, WHERE and OPTIONAL handled as the generic path
/// does); the void procedures (`set`/`drop`/`apply`) run their op once per
/// input row and pass the rows through unchanged. The divert site
/// (`exec_call`) classified the name once; this consumer enforces the
/// runtime contract it shares with the generic `CALL` path from the one
/// registry signature.
pub(crate) fn exec_ontology_call(
    ctx: ExecCtx<'_>,
    rows: Vec<Row>,
    call: CallSpec<'_>,
    proc: OntologyProc,
) -> Result<Vec<Row>> {
    // The runtime contract (passing mode, arity) ran at the divert site in
    // `exec_call` (`check_ontology_call_contract`) — the same checks, from
    // the same registry signature, the generic path applies; nothing is
    // re-checked here.
    match proc {
        OntologyProc::Get => exec_ontology_get(ctx, rows, call),
        OntologyProc::Set | OntologyProc::Drop | OntologyProc::Apply => {
            let CallSpec {
                name,
                args,
                explicit,
                optional,
                ..
            } = call;
            // The generic void-procedure contract: an OPTIONAL call
            // over no input yields one unit row.
            if super::procedures::void_optional_unit_row(rows.is_empty(), optional) {
                return Ok(vec![Row::new()]);
            }
            for row in &rows {
                exec_ontology_write(ctx, row, proc, name, args, explicit)?;
            }
            Ok(rows)
        }
    }
}

/// One void write op (`set`/`drop`/`apply`) for one input row.
fn exec_ontology_write(
    ctx: ExecCtx<'_>,
    row: &Row,
    proc: OntologyProc,
    name: &str,
    args: &[cypherlite_ir::Expr],
    explicit: bool,
) -> Result<()> {
    let graph = ctx.graph;
    if !graph.has_write_overlay() {
        // Unreachable by construction: the facade routes write-procedure
        // statements to the write path and rejects set/drop in explicit
        // transactions. Fail closed rather than panic on the read-only
        // view.
        return Err(Error::internal(format!(
            "{name} executed on a read-only graph view"
        )));
    }
    match proc {
        OntologyProc::Set => {
            let document = string_arg(ctx, row, args, explicit, 0, "ontology", name)?;
            // ADR-0227 §4/§6/§7: validate the document against the pinned
            // schema — before anything is committed. (ADR-0229 §3: only the
            // derived-graph build check is gone with the derived graph; the
            // 16 MiB ADR-0120 §3 envelope is `RedoOp::validate_ids`, checked
            // by the commit preflight and replay paths.)
            validate_document(&document)?;
            graph.push_catalog_redo(RedoOp::OntologySet {
                database: ONTOLOGY_DATABASE.to_string(),
                spec: document,
            });
            Ok(())
        }
        OntologyProc::Drop => {
            graph.push_catalog_redo(RedoOp::OntologyDrop {
                database: ONTOLOGY_DATABASE.to_string(),
            });
            Ok(())
        }
        OntologyProc::Apply => exec_ontology_apply(graph, graph.ontology_document()),
        OntologyProc::Get => Err(Error::internal(format!(
            "{name} is a read procedure routed to the write path"
        ))),
    }
}

/// `db.ontology.apply` (ADR-0226 §2/§3): convert the stored document to
/// the graph-type body, then run the shared `ALTER CURRENT GRAPH TYPE
/// SET` pipeline with the validate-first scan (ADR-0226 §3) spliced in
/// before the first catalog redo.
fn exec_ontology_apply(graph: &ExecGraph<'_>, ontology_document: Option<&str>) -> Result<()> {
    let document = ontology_document.ok_or_else(|| {
        Error::semantic("OntologyNotFound: no ontology is set for database 'neo4j'")
    })?;
    // ADR-0364 §4: `apply` is the one operation that must interpret the
    // stored document into the typed model. The stored bytes were accepted
    // by `set` at the time they were committed, but a document written by a
    // pre-bump binary never passed today's gate — so a parse failure here
    // is a loud semantic failure (the gate error names the supported URI),
    // never an internal one, and `get` keeps returning the bytes unchanged.
    let doc = cypherlite_ontology::parse_ontology_document(document.as_bytes())
        .map_err(|err| Error::semantic(err.to_string()))?;
    let body = cypherlite_ontology::to_graph_type_body(&doc)
        .map_err(|err| Error::semantic(err.to_string()))?;
    super::catalog_schema::set_current_graph_type_validate_first(
        graph,
        cypherlite_parser::AlterGraphTypeVerb::Set,
        &body,
    )
}

/// `db.ontology.get(format = "yaml") :: (ontology :: STRING)` (ADR-0227
/// §3): the stored document unchanged when the requested format matches
/// the stored one (strict-JSON sniff), else parsed and canonically
/// re-serialized; `null` when no ontology is set. Row composition (YIELD
/// defaulting with the typed star/default modes, aliasing, WHERE, OPTIONAL padding) is
/// the shared [`super::procedures::compose_rows`] scaffold.
fn exec_ontology_get(ctx: ExecCtx<'_>, rows: Vec<Row>, call: CallSpec<'_>) -> Result<Vec<Row>> {
    let CallSpec {
        args,
        explicit,
        yield_items,
        optional,
        where_clause,
        ..
    } = call;
    super::procedures::compose_rows(
        ctx,
        rows,
        &[ONTOLOGY_COLUMN],
        yield_items,
        optional,
        where_clause,
        |input| {
            let format = optional_format_arg(&ctx, input, args, explicit)?;
            let value = match ctx.graph.ontology_document() {
                None => Value::Null,
                Some(document) => Value::String(render_document(document, format)?.into()),
            };
            Ok(vec![Cow::Owned(BTreeMap::from([(
                ONTOLOGY_COLUMN.to_string(),
                value,
            )]))])
        },
    )
}

/// The `format` argument of `db.ontology.get`: `"yaml"` (the default) or
/// `"json"`; anything else is a typed argument error naming the two
/// (ADR-0227 §3). `null` falls back to the default.
fn optional_format_arg(
    ctx: &ExecCtx<'_>,
    row: &Row,
    args: &[cypherlite_ir::Expr],
    explicit: bool,
) -> Result<DocumentFormat> {
    let Some(value) = arg_value(ctx, row, args, explicit, 0, "format")? else {
        return Ok(DocumentFormat::Yaml);
    };
    match value {
        Value::Null => Ok(DocumentFormat::Yaml),
        Value::String(format) if format.as_ref() == "yaml" => Ok(DocumentFormat::Yaml),
        Value::String(format) if format.as_ref() == "json" => Ok(DocumentFormat::Json),
        Value::String(format) => Err(Error::ty(format!(
            "InvalidArgumentValue: unknown db.ontology.get format '{format}'; \
             expected 'yaml' or 'json'"
        ))),
        _ => Err(Error::invalid_argument_type(
            "db.ontology.get argument `format` expects STRING",
        )),
    }
}

/// The document in the requested format (ADR-0227 §3): byte-identical
/// when the stored syntax matches (a strict JSON parse sniffs the stored
/// format), else the canonical re-serialization with the document's key
/// order preserved.
fn render_document(document: &str, format: DocumentFormat) -> Result<Cow<'_, str>> {
    if cypherlite_ontology::detect_format(document.as_bytes()) == format {
        // Identity: the stored bytes are returned with no copy — the one
        // `Value::String` conversion happens at the call site.
        return Ok(Cow::Borrowed(document));
    }
    // ADR-0364 §4: a cross-format `get` is syntax-only — parse the stored
    // bytes into the value model and serialize canonically, with no version
    // gate and no structural validator, so a pre-bump document still
    // converts between JSON and YAML. A failure here is a corrupt stored
    // record: the open path already screened the syntax.
    let (value, _) = cypherlite_ontology::parse_document_value(document.as_bytes())
        .map_err(|err| Error::internal(format!("stored ontology document: {err}")))?;
    Ok(match format {
        DocumentFormat::Json => Cow::Owned(
            cypherlite_ontology::to_json_string(&value)
                .map_err(|err| Error::internal(format!("stored ontology document: {err}")))?,
        ),
        DocumentFormat::Yaml => Cow::Owned(
            cypherlite_ontology::to_yaml_string(&value)
                .map_err(|err| Error::internal(format!("stored ontology document: {err}")))?,
        ),
    })
}

/// Entry-point validation of `db.ontology.set`'s document (ADR-0227 §6/§7):
/// parses against the pinned schema (the version gate's error names 1.0.0 as
/// the highest supported version) and runs the structural validation. The
/// typed error names the violated field. No derived-graph build check
/// remains — no graph is derived from a set (ADR-0229 §3).
fn validate_document(document: &str) -> Result<()> {
    cypherlite_ontology::parse_ontology_document(document.as_bytes())
        .map_err(|err| Error::semantic(err.to_string()))?;
    Ok(())
}

/// Resolve one procedure argument's value — the shared resolution branch
/// of every `db.ontology.*` argument reader: the explicit positional
/// `args[index]` evaluated against the row, or (implicit passing) the
/// parameter/row column `arg_name`. `None` when the caller passed nothing
/// for it; the callers layer their required-vs-default policies on top.
fn arg_value(
    ctx: &ExecCtx<'_>,
    row: &Row,
    args: &[cypherlite_ir::Expr],
    explicit: bool,
    index: usize,
    arg_name: &str,
) -> Result<Option<Value>> {
    if explicit {
        args.get(index)
            .map(|expr| eval(ctx.graph, row, expr, ctx.params, ctx.cypher_version))
            .transpose()
    } else {
        Ok(ctx
            .params
            .get(arg_name)
            .cloned()
            .or_else(|| row.get(arg_name).cloned()))
    }
}

/// Evaluate a required STRING argument (explicit position `index` or the
/// implicit parameter/row column `arg_name`) for procedure `name`.
fn string_arg(
    ctx: ExecCtx<'_>,
    row: &Row,
    args: &[cypherlite_ir::Expr],
    explicit: bool,
    index: usize,
    arg_name: &str,
    name: &str,
) -> Result<String> {
    match arg_value(&ctx, row, args, explicit, index, arg_name)? {
        Some(Value::String(document)) => Ok(document.to_string()),
        Some(_) => Err(Error::invalid_argument_type(format!(
            "{name} argument `{arg_name}` expects STRING"
        ))),
        None => Err(Error::syntax(format!(
            "InvalidNumberOfArguments: {name} argument `{arg_name}` is required"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0227 §3: the stored-format sniff and the canonical conversion.
    #[test]
    fn render_document_round_trips_and_converts() {
        let json = "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\",\"id\":\"t\",\"version\":\"1\"}";
        // Stored JSON, requested JSON: unchanged bytes.
        assert_eq!(render_document(json, DocumentFormat::Json).unwrap(), json);
        // Stored JSON, requested YAML: canonical block-style conversion.
        let yaml = render_document(json, DocumentFormat::Yaml).unwrap();
        assert!(yaml.contains("schema:"), "{yaml}");
        assert!(yaml.contains("id: t"), "{yaml}");
        // And the YAML converts back to the canonical JSON.
        let back = render_document(&yaml, DocumentFormat::Json).unwrap();
        assert_eq!(back, json);
        // Stored YAML, requested YAML: unchanged bytes (comments survive).
        let commented = "# a comment\nschema: https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\nid: t\nversion: \"1\"\n";
        assert_eq!(
            render_document(commented, DocumentFormat::Yaml).unwrap(),
            commented
        );
        // ADR-0364 §4: a pre-bump document still converts across formats —
        // the conversion is syntax-only and runs neither the gate nor the
        // structural validator.
        let pre_bump = "{\"schema\":\"https://neo4j.com/ontology-graph-spec/1.0.0/schema.json\",\"id\":\"old\",\"version\":1}";
        let converted = render_document(pre_bump, DocumentFormat::Yaml).unwrap();
        assert!(
            converted.contains("https://neo4j.com/ontology-graph-spec/1.0.0/schema.json"),
            "{converted}"
        );
        assert!(converted.contains("version: 1"), "{converted}");
    }

    /// The dispatch vocabulary, spelled literally once in test: each arm is
    /// reachable from exactly its registered procedure name. (`Self::name`
    /// is the implementation's key; this independent pin catches an arm and
    /// a name drifting apart.)
    #[test]
    fn ontology_proc_classifies_its_registered_names() {
        let dispatch_names = [
            (OntologyProc::Get, "db.ontology.get"),
            (OntologyProc::Set, "db.ontology.set"),
            (OntologyProc::Drop, "db.ontology.drop"),
            (OntologyProc::Apply, "db.ontology.apply"),
        ];
        for (proc, name) in dispatch_names {
            assert_eq!(proc.name(), name);
            assert_eq!(OntologyProc::try_from(name), Ok(proc), "{name}");
            assert!(
                crate::procedure_registry::builtin_procedure_signature(name).is_some(),
                "{name} must be registered"
            );
        }
        assert!(OntologyProc::try_from("db.ontology.nope").is_err());
        assert!(OntologyProc::try_from("db.labels").is_err());
    }

    /// The divert's single classification and the registry's ontology
    /// predicate name the same set, in both directions: every registered
    /// `db.ontology.*` signature has a dispatch arm, and `try_from` accepts
    /// no name the registry does not register (the facade's gates consume
    /// the predicate, the executor's divert consumes the classification —
    /// they cannot drift apart).
    #[test]
    fn classification_and_registry_name_the_same_set_both_ways() {
        use std::collections::BTreeSet;

        let registered: BTreeSet<&str> = crate::procedure_registry::BUILTIN_PROCEDURE_SIGNATURES
            .iter()
            .filter(|signature| crate::procedure_registry::is_ontology_procedure(signature.name))
            .map(|signature| signature.name)
            .collect();
        let classified: BTreeSet<&str> = OntologyProc::ALL.iter().map(|proc| proc.name()).collect();
        assert_eq!(classified, registered);

        for signature in crate::procedure_registry::BUILTIN_PROCEDURE_SIGNATURES {
            assert_eq!(
                OntologyProc::try_from(signature.name).is_ok(),
                registered.contains(signature.name),
                "{}",
                signature.name
            );
        }
    }

    /// The generic path's passing-mode backstop covers the ontology divert
    /// too: an implicit in-query `db.ontology.get` — the one ontology
    /// procedure with an argument — with a YIELD fails exactly like the
    /// generic path's guard does for a registered procedure with an
    /// argument. (The facade's compile-time `validate_calls` raises the
    /// same error earlier on every facade-routed statement; this drives
    /// `exec_call` directly to pin the executor-side backstop.)
    #[test]
    fn implicit_get_with_defaulted_arg_succeeds_but_required_arg_is_a_passing_mode_error() {
        let graph = crate::graph::Graph::default();
        let params = std::collections::BTreeMap::new();
        let view = ExecGraph::read(&graph);
        let ctx = ExecCtx::new(&view, &params, None);
        // Through the divert: `exec_call` classifies once, the ontology
        // executor's registry-signature contract raises the error.
        let call = CallSpec {
            name: "db.ontology.get",
            args: &[],
            explicit: false,
            yield_items: &cypherlite_ir::YieldItems::Named(vec![("ontology".to_string(), None)]),
            optional: false,
            where_clause: None,
        };
        // LABS-389 (the paren-less follow-up): `db.ontology.get`'s one
        // parameter carries a default (format = yaml), so the implicit
        // in-query call with a YIELD is allowed -- required args key the
        // guard, not declared ones. The call succeeds with the default.
        let rows = super::super::procedures::exec_call(ctx, vec![Row::new()], call).unwrap();
        assert!(!rows.is_empty());
        // The mirror on the generic path: a user-registered procedure with
        // an argument, called implicitly in-query with a YIELD, hits the
        // same backstop there.
        graph.register_procedure(crate::graph::Procedure {
            name: "custom.one".to_string(),
            arg_names: vec!["x".to_string()],
            arg_types: vec!["ANY".to_string()],
            yield_names: vec!["out".to_string()],
            rows: Vec::new(),
        });
        let call = CallSpec {
            name: "custom.one",
            args: &[],
            explicit: false,
            yield_items: &cypherlite_ir::YieldItems::Named(vec![("out".to_string(), None)]),
            optional: false,
            where_clause: None,
        };
        // The mirror (a REQUIRED argument): the implicit in-query call
        // with a YIELD stays rejected -- required args key the guard.
        let err = super::super::procedures::exec_call(ctx, vec![Row::new()], call).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("InvalidArgumentPassingMode")
                && text.contains("cannot pass arguments implicitly"),
            "{text}"
        );
    }

    #[test]
    fn unknown_format_is_an_argument_error_naming_both() {
        let graph = crate::graph::Graph::default();
        let view = ExecGraph::read(&graph);
        let params = std::collections::BTreeMap::new();
        let ctx = ExecCtx::new(&view, &params, None);
        let row = Row::new();
        let err = optional_format_arg(
            &ctx,
            &row,
            &[cypherlite_ir::Expr::String("xml".to_string())],
            true,
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("unknown db.ontology.get format 'xml'; expected 'yaml' or 'json'"),
            "{text}"
        );
        // A non-string argument is a type error.
        let err =
            optional_format_arg(&ctx, &row, &[cypherlite_ir::Expr::Int(1)], true).unwrap_err();
        assert!(err.to_string().contains("InvalidArgumentType"), "{err}");
        // Absent and null both default to yaml.
        assert_eq!(
            optional_format_arg(&ctx, &row, &[], true).unwrap(),
            DocumentFormat::Yaml
        );
        assert_eq!(
            optional_format_arg(&ctx, &row, &[cypherlite_ir::Expr::Null], true).unwrap(),
            DocumentFormat::Yaml
        );
    }
}
