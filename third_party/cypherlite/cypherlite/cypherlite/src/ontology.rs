//! ADR-0120 store integration: the ontology document as store-level
//! facade state, and the commit/open/checkpoint wiring the
//! `db.ontology.*` procedures (ADR-0227, on main's document shapes per
//! ADR-0364) call into.
//!
//! The document is the source of truth and is kept verbatim; no
//! `system_ontologies` graph is derived from it — the namespace is
//! listed but not queryable, reachable only through the procedures
//! (ADR-0229 §3).

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use cypherlite_executor::{OntologyOp, RedoOp};
use cypherlite_storage::Wal;

use crate::commit::try_wal_commit;
use crate::{Database, Error, Result, Shared};

/// The database every stored ontology belongs to (ADR-0121 §2).
const ONTOLOGY_DATABASE: &str = "neo4j";

/// The store's ontology document slot (ADR-0120 §1): the verbatim document
/// behind a lock, shared as an `Arc<str>` handle so the per-statement
/// admission capture (`Connection::dispatch`, `commit_optimistic`,
/// `Connection::begin`) is a pointer clone, never a document-sized copy.
/// Written under the commit's publication boundary (after the record is
/// durable and applied) and at open; read by `Database::ontology_document`
/// and checkpoints.
pub(crate) struct OntologySlot(RwLock<Option<Arc<str>>>);

impl OntologySlot {
    pub(crate) fn new(doc: Option<String>) -> Self {
        Self(RwLock::new(doc.map(Arc::from)))
    }

    /// The current document handle (a cheap `Arc` clone). A poisoned lock
    /// is recovered, not propagated: the slot always holds a complete
    /// document state (the swap is a single atomic store).
    pub(crate) fn get(&self) -> Option<Arc<str>> {
        self.0.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Record a committed ontology document state (ADR-0120 §1). Called
    /// from the commit pipeline after the record is durable and applied,
    /// and from the open path with the loaded state.
    pub(crate) fn set(&self, doc: Option<String>) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = doc.map(Arc::from);
    }
}

impl Database {
    /// The store's current ontology document, verbatim as it was committed
    /// (ADR-0120 §1, ADR-0227). `None` when the store holds no ontology.
    /// Store-level state shared by every namespace; `db.ontology.get` and
    /// the checkpoint path read it here.
    #[doc(hidden)]
    pub fn ontology_document(&self) -> Result<Option<String>> {
        let _read = self.inner.read_guard()?;
        Ok(self.inner.ontology.get().map(|doc| doc.to_string()))
    }

    /// Validate `document` and commit it as the store's ontology for
    /// `neo4j` (ADR-0120 §3, ADR-0227), replacing any current one. The
    /// `db.ontology.set` procedure does NOT call here: it pushes the same
    /// [`RedoOp::OntologySet`] into the statement's write overlay and the
    /// group-commit pipeline converges both paths in
    /// [`prepare_committed`]/[`install_committed`] (the procedure's
    /// implicit-only and no-mixed-writes rules are compile-time gates in
    /// `Connection::ontology_gate_error`). After the commit is durable and
    /// applied, the stored document and the WAL's `MDBWAL02` segment-magic
    /// state are updated atomically with the commit (ADR-0120 §2/§4).
    ///
    /// The document is validated before any WAL write; an invalid
    /// document is a semantic error naming the violated rule.
    #[doc(hidden)]
    pub fn commit_ontology_set(&self, document: String) -> Result<()> {
        validate_ontology_document(&document)?;
        self.commit_ontology_op(RedoOp::OntologySet {
            database: ONTOLOGY_DATABASE.to_string(),
            spec: document,
        })
    }

    /// Drop the store's ontology (ADR-0120 §3, ADR-0227). Like
    /// [`Self::commit_ontology_set`], this is the direct facade; the
    /// `db.ontology.drop` procedure reaches the same commit pipeline
    /// through the statement's redo. After the commit, the document
    /// state is cleared and new WAL segments are `MDBWAL01` again.
    #[doc(hidden)]
    pub fn commit_ontology_drop(&self) -> Result<()> {
        self.commit_ontology_op(RedoOp::OntologyDrop {
            database: ONTOLOGY_DATABASE.to_string(),
        })
    }

    /// One store-level ontology op through the group-commit pipeline
    /// (auto-commit: the op is the whole record). The post-commit state
    /// update — document, segment-magic flag — runs inside
    /// [`crate::commit::commit_preflighted`], before the publication
    /// boundary opens, so no checkpoint can publish or recycle past the
    /// commit without seeing it.
    fn commit_ontology_op(&self, op: RedoOp) -> Result<()> {
        let shared = &self.inner;
        let txn_id = shared.next_txn_id.fetch_add(1, Ordering::SeqCst);
        // The commit timestamp through `Shared::commit_ts()`: the
        // publication-guarded read the `GraphCell` deref contract requires
        // (ADR-0137) — the guard lives inside that helper, scoped to the
        // one read and released before `try_wal_commit` (whose flush takes
        // the publication WRITE lock; the `commit_optimistic`
        // scoped-guard rule, commit.rs). Every graph deref inside the
        // commit pipeline itself runs under that write-side publication
        // boundary.
        let snapshot_ts = shared.commit_ts()?;
        try_wal_commit(shared, snapshot_ts, txn_id, vec![op], 0, 0)?;
        Ok(())
    }
}

/// Entry-point validation for [`Database::commit_ontology_set`]: the
/// document parses against main's vendored schema — the exact-URI gate
/// plus the structural validation of ADR-0364 §1/§2 (the set-time policy
/// of ADR-0364 §4). Semantic errors name the violated rule for the
/// procedure caller. (ADR-0229 §3: only the derived-graph build check is
/// gone; no graph is derived from a set.)
fn validate_ontology_document(doc: &str) -> Result<()> {
    cypherlite_ontology::parse_ontology_document(doc.as_bytes())
        .map_err(|err| Error::semantic(err.to_string()))?;
    Ok(())
}

/// Screen a document the open path folded out of a snapshot or the WAL
/// (ADR-0120 §1). ADR-0364 §4 makes the version gate and structural
/// validation a `set`-time policy, not a property of documents at rest:
/// open keeps only the syntax/value-model screen (UTF-8, one document,
/// the JSON-equivalent value model, the depth cap), so a pre-bump
/// document is expected here and opens. A failure is therefore a corrupt
/// stored record — syntactically broken, not merely from an older spec —
/// which open must surface instead of deferring to the first
/// `db.ontology.get`/`apply` that touches it.
pub(crate) fn validate_stored_document(doc: Option<&str>) -> Result<()> {
    // A store with no document is trivially valid — the `None` policy lives
    // here, so no caller carries its own `if let` guard.
    let Some(doc) = doc else {
        return Ok(());
    };
    cypherlite_ontology::parse_document_value(doc.as_bytes())
        .map_err(|err| Error::internal(format!("stored ontology document: {err}")))?;
    Ok(())
}

/// The ontology outcome of one accepted commit group: the last ontology
/// op in commit order wins (ADR-0120 §3). `doc` is the verbatim document
/// (`None` for a drop) — the sole stored source of truth (ADR-0229 §3);
/// no graph is derived from it.
pub(crate) struct CommittedOntology {
    doc: Option<String>,
}

/// Scan one group's accepted records for ontology ops (the last one wins,
/// ADR-0120 §3). Runs in [`crate::commit::commit_preflighted`] before the
/// first append. Infallible: every fallible check happens elsewhere — the
/// document was validated at set time, before the redo was pushed
/// (`validate_ontology_document` here, `validate_document` in the
/// executor's procedure path), and the 16 MiB envelope is the WAL
/// preflight's `RedoOp` validation (ADR-0120 §3). Nothing is built from
/// the document (ADR-0229 §3), so there is nothing left to fail here.
pub(crate) fn prepare_committed<'a>(
    records: impl IntoIterator<Item = &'a [RedoOp]>,
) -> Option<CommittedOntology> {
    let last = records
        .into_iter()
        .flat_map(|ops| ops.iter())
        .filter_map(RedoOp::ontology_op)
        .last();
    let doc = match last {
        Some(OntologyOp::Set(spec)) => Some(spec.to_string()),
        Some(OntologyOp::Drop) => None,
        None => return None,
    };
    Some(CommittedOntology { doc })
}

/// Install a committed ontology outcome on the store: the verbatim
/// document state and the WAL segment-magic flag (ADR-0120 §4) — after
/// the record is durable and applied. Runs under the commit's WAL mutex
/// and publication boundary, before any checkpoint can be scheduled, so a
/// checkpoint always observes the committed document. Infallible: every
/// fallible step happened before the record was ever accepted.
pub(crate) fn install_committed(shared: &Shared, wal: &mut Wal, committed: CommittedOntology) {
    install_ontology_state(shared, wal, committed.doc);
}

/// The one step that installs the store-level ontology state: the verbatim
/// document and the WAL's `ontology_held` segment-magic flag (ADR-0120
/// §4). Every path that changes the state — the commit pipeline and a
/// Follow-mode gap reload — takes it, so the two halves cannot drift.
pub(crate) fn install_ontology_state(shared: &Shared, wal: &mut Wal, doc: Option<String>) {
    let held = doc.is_some();
    shared.ontology.set(doc);
    wal.set_ontology_held(held);
}
