//! Post-statement constraint validation, incremental per ADR-0172, over the
//! redo slice the overlay hands out and marks validated (ADR-0173), with a
//! full re-check of any constraint the committed catalog gained or changed
//! since this scratch last validated (ADR-0174). The recheck walks every
//! live entity of a constraint's label or relationship type; a uniqueness
//! probe reads one equality bucket per touched tuple — committed postings
//! plus the scratch's twin (ADR-0185 for relationships) — and both loops
//! sample the statement's cancellation token per entity (ADR-0111).

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;

use cypherlite_catalog::ConstraintDef;
use cypherlite_core::{Error, NodeId, RelId, Result, Value};
use cypherlite_ir::{Direction, TypeExpr};
use cypherlite_model::{ConstraintType, EnforcedLabelSite};

use crate::group_key::GroupKey;
use crate::overlay::{ExecGraph, ValidatedCatalog};
use crate::redo::RedoOp;

thread_local! {
    /// Entities constraint validation inspected (ADR-0172 Tests): each
    /// re-validated entity, each probe candidate verified, and each entity
    /// a full re-validation reads.
    static CONSTRAINT_INSPECTS: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
fn take_constraint_inspects() -> u64 {
    CONSTRAINT_INSPECTS.with(|c| c.replace(0))
}

fn note_inspect() {
    CONSTRAINT_INSPECTS.with(|c| c.set(c.get() + 1));
}

/// Post-statement constraint validation, incremental per ADR-0172.
///
/// Runs after every write statement and every `CALL { … } IN TRANSACTIONS`
/// batch. It re-checks only what the redo ops appended since the previous
/// validation (the *slice*, §1) could have broken, plus a full re-check of
/// every constraint created or re-derived in the slice (§5) and of every
/// constraint the committed catalog gained or changed since this scratch
/// last validated (ADR-0174):
///
/// - node constraints re-validate the touched nodes carrying the label;
///   `unique`/`key` probe each one for conflicting partners through the
///   committed equality postings plus the scratch's `prop_ix` (§3);
/// - relationship constraints re-validate the touched relationships of the
///   type, plus, for the endpoint-label kinds, the incident relationships of
///   every node that lost a label; `unique`/`key` probe each touched
///   relationship the same way, through the committed relationship equality
///   postings plus the scratch's relationship index (ADR-0185).
///
/// Whenever the state before the slice satisfied every constraint, the
/// accept/reject decision and the error text are the ones the full scan
/// (every live entity, constraints in name order, ids ascending, first
/// failure wins) would produce (§6). An empty slice with an unchanged
/// catalog inspects no entity (§8); the catalog diff itself is
/// O(constraints) per validation (ADR-0174).
///
/// The slice is marked validated, and the catalog fingerprint remembered,
/// only after the validation returned `Ok` (ADR-0173, ADR-0174): a failure
/// leaves both untouched, so validating again fails again.
pub(crate) fn validate_constraints(graph: &ExecGraph<'_>) -> Result<()> {
    let (slice, end) = graph.unvalidated_redo(ValidationSlice::classify);
    validate_slice(graph, &slice)?;
    graph.mark_redo_validated(end);
    Ok(())
}

fn validate_slice(graph: &ExecGraph<'_>, slice: &ValidationSlice) -> Result<()> {
    let constraints = graph.named_constraints();
    if constraints.is_empty() {
        // Remember the empty catalog: a constraint the committed catalog
        // gains later is new against it and joins the recheck set
        // (ADR-0174).
        graph.remember_validated_catalog(ValidatedCatalog::default());
        return Ok(());
    }
    let show = graph.show_schema()?;
    let enforced_labels: Vec<Option<String>> = constraints
        .iter()
        .map(|def| {
            show.data
                .derived_constraints
                .get(&def.name)
                .and_then(|constraint| constraint.enforced_label.clone())
        })
        .collect();
    let catalog = ValidatedCatalog::new(
        constraints
            .iter()
            .cloned()
            .zip(enforced_labels.iter().cloned())
            .collect(),
    );
    // ADR-0174: a constraint new or changed in the committed catalog since
    // this scratch last validated joins the slice's recheck set. The first
    // validation of a scratch diffs nothing: its slice holds every write
    // the scratch has made, each checked against the full current catalog
    // below.
    let drifted = graph
        .validated_catalog()
        .map(|remembered| catalog.new_or_changed_since(&remembered))
        .unwrap_or_default();
    if slice.ops == 0 && drifted.is_empty() {
        graph.remember_validated_catalog(catalog);
        return Ok(());
    }
    for (def, enforced_label) in constraints.iter().zip(enforced_labels) {
        let check = ConstraintCheck::new(def, enforced_label)?;
        if drifted.contains(&def.name) || slice.rechecks(&check) {
            check.recheck(graph)?;
        } else {
            check.validate_slice(graph, slice)?;
        }
    }
    graph.remember_validated_catalog(catalog);
    Ok(())
}

/// ADR-0226 §3 — `db.ontology.apply`'s validate-first scan: check the
/// calling transaction's captured `neo4j` snapshot (the overlay view, so
/// the transaction's own writes count too) against every constraint the
/// resulting graph type derives, BEFORE any catalog redo is written. The
/// first violation — constraints in derivation order, entities in
/// ascending id order — fails the statement with a data-violation error
/// naming the constraint and the offending element's element id. On
/// success the caller runs the ordinary `ALTER CURRENT GRAPH TYPE SET`
/// catalog sync unchanged.
pub(crate) fn validate_derived_constraints(
    graph: &ExecGraph<'_>,
    derived: &[crate::graph_type::DerivedConstraint],
) -> Result<()> {
    for derived_constraint in derived {
        let crate::graph_type::DerivedConstraint { constraint, entity } = derived_constraint;
        let def = ConstraintDef {
            name: constraint.name.clone(),
            relationship: entity.is_relationship(),
            label: constraint.label.clone(),
            properties: constraint.properties.clone(),
            kind: constraint.constraint_type.as_str().to_string(),
            property_type: constraint.property_type.clone(),
        };
        let check = ConstraintCheck::new(&def, constraint.enforced_label.clone())?;
        check.recheck_naming_elements(graph)?;
    }
    Ok(())
}

/// The redo ops one validation re-checks, classified (ADR-0172 §1).
#[derive(Default)]
struct ValidationSlice {
    /// Number of ops in the slice; zero means nothing to validate.
    ops: usize,
    nodes: BTreeSet<NodeId>,
    rels: BTreeSet<RelId>,
    /// Nodes that lost a label: the start/end of relationships whose
    /// endpoint-label constraint they may now break.
    label_loss: BTreeSet<NodeId>,
    /// Constraints created (or given an enforced label) in the slice.
    recheck: HashSet<String>,
    /// A `SetCurrentGraphType` re-derived every enforced label.
    recheck_enforced_label_kinds: bool,
}

impl ValidationSlice {
    fn classify(ops: &[RedoOp]) -> Self {
        let mut slice = Self {
            ops: ops.len(),
            ..Self::default()
        };
        for op in ops {
            match op {
                RedoOp::CreateNode { id, .. }
                | RedoOp::AddLabels { id, .. }
                | RedoOp::SetNodeProp { id, .. }
                | RedoOp::RemoveNodeProp { id, .. } => {
                    slice.nodes.insert(*id);
                }
                RedoOp::RemoveLabels { id, .. } => {
                    slice.nodes.insert(*id);
                    slice.label_loss.insert(*id);
                }
                RedoOp::CreateRel { id, .. }
                | RedoOp::SetRelProp { id, .. }
                | RedoOp::RemoveRelProp { id, .. } => {
                    slice.rels.insert(*id);
                }
                RedoOp::CreateConstraint { name, .. } => {
                    slice.recheck.insert(name.clone());
                }
                RedoOp::CatalogDerivedConstraint { constraint } => {
                    slice.recheck.insert(constraint.name.clone());
                }
                RedoOp::SetCurrentGraphType { .. } => slice.recheck_enforced_label_kinds = true,
                // A deletion can never introduce a violation; the other
                // catalog ops and pull identities touch no entity.
                RedoOp::DeleteNode { .. }
                | RedoOp::DeleteRel { .. }
                | RedoOp::CreateIndex { .. }
                | RedoOp::DropIndex { .. }
                | RedoOp::DropConstraint { .. }
                | RedoOp::CatalogIndexDetail { .. }
                | RedoOp::PutDatabase { .. }
                | RedoOp::DropDatabase { .. }
                | RedoOp::PutGraphType { .. }
                | RedoOp::PullIdentityPut { .. }
                | RedoOp::OntologySet { .. }
                | RedoOp::OntologyDrop { .. } => {}
            }
        }
        slice
    }

    fn rechecks(&self, check: &ConstraintCheck<'_>) -> bool {
        self.recheck.contains(&check.def.name)
            || (self.recheck_enforced_label_kinds && check.kind.is_enforced_label_kind())
    }
}

/// What one constraint checks, decided once from its entity kind and its
/// kind's enforced-label site. The enforced label E is absent when the
/// constraint has no derived enforced label (legacy-unenforced definitions).
enum Target {
    /// Nodes carrying the constraint's label; each must also carry
    /// `enforced` (`labelExistence`, `nodeLabelExistence`).
    Node { enforced: Option<String> },
    /// Relationships of the constraint's type; the node at `endpoint`'s end
    /// must carry its label (the source- and target-label kinds).
    Rel {
        endpoint: Option<(Endpoint, String)>,
    },
}

impl Target {
    fn new(
        relationship: bool,
        site: Option<EnforcedLabelSite>,
        enforced_label: Option<String>,
    ) -> Self {
        match (relationship, site, enforced_label) {
            (false, Some(EnforcedLabelSite::Node), enforced) => Target::Node { enforced },
            (false, _, _) => Target::Node { enforced: None },
            (true, Some(EnforcedLabelSite::Start), Some(label)) => Target::Rel {
                endpoint: Some((Endpoint::Start, label)),
            },
            (true, Some(EnforcedLabelSite::End), Some(label)) => Target::Rel {
                endpoint: Some((Endpoint::End, label)),
            },
            (true, _, _) => Target::Rel { endpoint: None },
        }
    }
}

/// The end of a relationship whose node an endpoint-label kind checks.
#[derive(Clone, Copy)]
enum Endpoint {
    Start,
    End,
}

impl Endpoint {
    /// The direction in which this end's node reaches the relationship.
    fn direction(self) -> Direction {
        match self {
            Endpoint::Start => Direction::Outgoing,
            Endpoint::End => Direction::Incoming,
        }
    }

    /// This end's node of a relationship's `(start, end)`.
    fn pick(self, (start, end): (NodeId, NodeId)) -> NodeId {
        match self {
            Endpoint::Start => start,
            Endpoint::End => end,
        }
    }
}

/// One entity's per-entity outcome under one constraint.
enum Visit {
    /// Not live, or not of the constraint's label/type.
    Skip,
    Fail(Error),
    /// Passed; the constrained values, a missing property read as `Null`.
    Pass(Vec<Value>),
}

/// Which check failed at a position: at one id the full scan raises a
/// per-entity failure before a uniqueness conflict (ADR-0172 §6).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// The label/endpoint, required or property-type check of one entity.
    Entity,
    /// A uniqueness conflict, raised at the later id of the pair.
    Conflict,
}

/// The lowest `(position, rank)` failure of one constraint (ADR-0172 §6):
/// the failure the full scan would raise first.
#[derive(Default)]
struct FirstFailure(Option<(i64, Rank, Error)>);

impl FirstFailure {
    fn offer(&mut self, position: i64, rank: Rank, error: Error) {
        if self
            .0
            .as_ref()
            .is_none_or(|(best, best_rank, _)| (position, rank) < (*best, *best_rank))
        {
            self.0 = Some((position, rank, error));
        }
    }

    /// No failure raised at `id` or later can beat the one found: every
    /// failure an entity yields sits at its own id or a later one.
    fn settled_before(&self, id: i64) -> bool {
        self.0.as_ref().is_some_and(|(best, ..)| id > *best)
    }

    fn into_result(self) -> Result<()> {
        self.0.map_or(Ok(()), |(_, _, error)| Err(error))
    }
}

/// The kind-specific pieces of one uniqueness probe (ADR-0172 §3,
/// ADR-0185) are methods on [`ConstraintCheck`], each a two-arm match on
/// the entity kind, mirroring the `visit`/`visit_node`/`visit_rel` split.
struct ConstraintCheck<'c> {
    def: &'c ConstraintDef,
    kind: ConstraintType,
    property_type: Option<TypeExpr>,
    target: Target,
}

impl<'c> ConstraintCheck<'c> {
    /// Parses the stored kind tag and property-type rendering up front, even
    /// when no entity is re-validated: either one failing to parse fails the
    /// validation closed.
    fn new(def: &'c ConstraintDef, enforced_label: Option<String>) -> Result<Self> {
        let kind = ConstraintType::from_str(&def.kind)
            .map_err(|reason| unknown_kind_error(&def.name, &reason))?;
        let property_type = def
            .property_type
            .as_deref()
            .map(|ty| parse_constraint_type(ty).ok_or_else(|| property_type_error(&def.name)))
            .transpose()?;
        Ok(Self {
            def,
            kind,
            property_type,
            target: Target::new(def.relationship, kind.enforced_label_site(), enforced_label),
        })
    }

    /// Re-validate the slice's entities (ADR-0172 §2, §3, §6, ADR-0185).
    fn validate_slice(&self, graph: &ExecGraph<'_>, slice: &ValidationSlice) -> Result<()> {
        let mut first = FirstFailure::default();
        let mut tuples: Vec<(i64, Vec<Value>)> = Vec::new();
        for id in self.revalidated_ids(graph, slice) {
            if first.settled_before(id) {
                break;
            }
            match self.visit(graph, id) {
                Visit::Skip => {}
                Visit::Fail(error) => first.offer(id, Rank::Entity, error),
                Visit::Pass(values) => {
                    if self.unique_tuple(&values) {
                        tuples.push((id, values));
                    }
                }
            }
        }
        for partners in self.partners(graph, &tuples) {
            let (id, partners) = partners?;
            if first.settled_before(id) {
                break;
            }
            if let Some(position) = conflict_position(id, &partners) {
                first.offer(
                    position,
                    Rank::Conflict,
                    Error::uniqueness_violation(&self.def.name),
                );
            }
        }
        first.into_result()
    }

    /// The slice's entities this constraint re-validates, ascending: touched
    /// nodes, or touched relationships plus — for the endpoint-label kinds —
    /// the relationships of the type at the checked end of every node that
    /// lost a label. The label/type filter happens in [`Self::visit`].
    fn revalidated_ids(&self, graph: &ExecGraph<'_>, slice: &ValidationSlice) -> Vec<i64> {
        match &self.target {
            Target::Node { .. } => slice.nodes.iter().copied().collect(),
            Target::Rel { endpoint } => {
                let mut ids = slice.rels.clone();
                if let Some((end, _)) = endpoint {
                    let types = std::slice::from_ref(&self.def.label);
                    for &node in &slice.label_loss {
                        graph.for_each_neighbor(node, end.direction(), types, |rel| {
                            ids.insert(rel);
                        });
                    }
                }
                ids.into_iter().collect()
            }
        }
    }

    fn visit(&self, graph: &ExecGraph<'_>, id: i64) -> Visit {
        match &self.target {
            Target::Node { enforced } => self.visit_node(graph, id, enforced.as_deref()),
            Target::Rel { endpoint } => self.visit_rel(graph, id, endpoint.as_ref()),
        }
    }

    fn visit_node(&self, graph: &ExecGraph<'_>, id: NodeId, enforced: Option<&str>) -> Visit {
        if graph.node_has_label(id, &self.def.label) != Some(true) {
            return Visit::Skip;
        }
        note_inspect();
        if let Some(enforced) = enforced {
            if graph.node_has_label(id, enforced) != Some(true) {
                return Visit::Fail(node_label_error(&self.def.label, enforced));
            }
        }
        self.check_values(self.values_or_null(|key| graph.node_prop(id, key)))
    }

    fn visit_rel(
        &self,
        graph: &ExecGraph<'_>,
        id: RelId,
        endpoint: Option<&(Endpoint, String)>,
    ) -> Visit {
        if graph
            .rel_type(id)
            .is_none_or(|rel_type| rel_type != self.def.label)
        {
            return Visit::Skip;
        }
        note_inspect();
        if let Some((end, enforced)) = endpoint {
            let node = graph.rel_ends(id).map(|ends| end.pick(ends));
            if node.and_then(|node| graph.node_has_label(node, enforced)) == Some(false) {
                return Visit::Fail(endpoint_label_error(enforced));
            }
        }
        self.check_values(self.values_or_null(|key| graph.rel_prop(id, key)))
    }

    /// The constrained values read through `prop`, a missing one as `Null`.
    fn values_or_null(&self, prop: impl Fn(&str) -> Option<Value>) -> Vec<Value> {
        self.def
            .properties
            .iter()
            .map(|key| prop(key).unwrap_or(Value::Null))
            .collect()
    }

    /// The constrained values read through `prop`; `None` when one is
    /// missing.
    fn complete_values(&self, prop: impl Fn(&str) -> Option<Value>) -> Option<Vec<Value>> {
        self.def.properties.iter().map(|key| prop(key)).collect()
    }

    /// The required check, then the property-type check (on the first
    /// constrained property), in the full scan's order.
    fn check_values(&self, values: Vec<Value>) -> Visit {
        if self.kind.enforces_property_presence() && values.iter().any(Value::is_null) {
            return Visit::Fail(requires_error(&self.def.name));
        }
        if let Some(ty) = &self.property_type {
            if values
                .first()
                .is_some_and(|value| !crate::eval::value_matches_type(value, ty))
            {
                return Visit::Fail(property_type_error(&self.def.name));
            }
        }
        Visit::Pass(values)
    }

    /// A tuple takes part in uniqueness only when every value is non-null.
    fn unique_tuple(&self, values: &[Value]) -> bool {
        self.kind.enforces_uniqueness() && values.iter().all(|value| !value.is_null())
    }

    /// The conflict partners of each touched tuple, in the tuples' order:
    /// one probe per entity, run only once the caller asks for it. Nodes
    /// and relationships share the probe (ADR-0172 §3, ADR-0185); the loop
    /// samples the statement's cancellation token per candidate
    /// (ADR-0111).
    fn partners<'s>(
        &'s self,
        graph: &'s ExecGraph<'_>,
        tuples: &'s [(i64, Vec<Value>)],
    ) -> impl Iterator<Item = Result<(i64, Vec<i64>)>> + 's {
        tuples
            .iter()
            .map(|(x, values)| self.probe_partners(graph, *x, values).map(|p| (*x, p)))
    }

    /// The constrained property whose committed-plus-scratch equality
    /// bucket is smallest, ties to the lowest index; the composite-probe
    /// choice of ADR-0184. `len_of` maps a property index to its bucket
    /// size. A single-property constraint selects 0 without consulting the
    /// lookup.
    fn select_property(&self, len_of: impl Fn(usize) -> usize) -> usize {
        let count = self.def.properties.len();
        if count <= 1 {
            return 0;
        }
        // `min_by_key` returns the first of equal minima — the
        // ties-to-the-lowest-index contract.
        (0..count).min_by_key(|&i| len_of(i)).unwrap_or(0)
    }

    /// Live entities other than `x` of the constraint's label or type whose
    /// constrained tuple equals `values` (ADR-0172 §3, ADR-0185): candidates
    /// from the equality postings of the constrained property with the
    /// smallest committed-plus-scratch bucket (ADR-0184) — the same property
    /// on both the committed and the scratch side — each verified with
    /// [`tuples_equal`]. The entity kind picks the node or the relationship
    /// pairs, one two-arm match per piece, mirroring `visit`/`visit_node`/
    /// `visit_rel`.
    fn probe_partners(&self, graph: &ExecGraph<'_>, x: i64, values: &[Value]) -> Result<Vec<i64>> {
        let properties = &self.def.properties;
        if properties.is_empty() || values.len() != properties.len() {
            return Ok(Vec::new());
        }
        let i = self.select_property(|i| self.bucket_len(graph, i, values));
        let mut partners = Vec::new();
        for y in self.candidates(graph, i, values) {
            graph.cancel_tick()?;
            if y == x {
                continue;
            }
            note_inspect();
            if self.verify(graph, y, values) {
                partners.push(y);
            }
        }
        Ok(partners)
    }

    /// The committed-plus-scratch equality bucket size of the i'th
    /// constrained property: the node pairs or the lazy relationship pairs
    /// (ADR-0185), by entity kind.
    fn bucket_len(&self, graph: &ExecGraph<'_>, i: usize, values: &[Value]) -> usize {
        match &self.target {
            Target::Node { .. } => {
                graph.equality_bucket_len(&self.def.label, &self.def.properties[i], &values[i])
            }
            Target::Rel { .. } => {
                graph.rel_equality_bucket_len(&self.def.label, &self.def.properties[i], &values[i])
            }
        }
    }

    /// The candidate ids of the i'th constrained property, by entity kind.
    fn candidates(&self, graph: &ExecGraph<'_>, i: usize, values: &[Value]) -> Vec<i64> {
        match &self.target {
            Target::Node { .. } => {
                graph.equality_candidates(&self.def.label, &self.def.properties[i], &values[i])
            }
            Target::Rel { .. } => {
                graph.rel_equality_candidates(&self.def.label, &self.def.properties[i], &values[i])
            }
        }
    }

    /// Candidate verification, by entity kind: `y` carries the constraint's
    /// label or type and its complete constrained tuple equals `values`.
    fn verify(&self, graph: &ExecGraph<'_>, y: i64, values: &[Value]) -> bool {
        match &self.target {
            Target::Node { .. } => {
                graph.node_has_label(y, &self.def.label) == Some(true)
                    && self
                        .complete_values(|key| graph.node_prop(y, key))
                        .is_some_and(|candidate| tuples_equal(values, &candidate))
            }
            Target::Rel { .. } => {
                graph
                    .rel_type(y)
                    .is_some_and(|rel_type| rel_type == self.def.label)
                    && self
                        .complete_values(|key| graph.rel_prop(y, key))
                        .is_some_and(|candidate| tuples_equal(values, &candidate))
            }
        }
    }

    /// Full re-validation over every live entity of the label/type in
    /// ascending id order, first failure wins (ADR-0172 §5). Uniqueness
    /// buckets tuples by key; a tuple conflicts only with an earlier tuple
    /// of its own bucket that is `eq_non_null` on every component.
    /// `report` maps a rule failure at element `id` to the returned
    /// error — the one difference between the plain recheck and the
    /// apply scan.
    fn recheck_scan(
        &self,
        graph: &ExecGraph<'_>,
        report: impl Fn(&ExecGraph<'_>, i64, Error) -> Error,
    ) -> Result<()> {
        let ids = match self.target {
            Target::Node { .. } => graph.live_node_ids_with_label(&self.def.label),
            Target::Rel { .. } => graph.live_rel_ids_of_type(&self.def.label),
        };
        let mut buckets: HashMap<Vec<GroupKey>, Vec<Vec<Value>>> = HashMap::new();
        for id in ids {
            graph.cancel_tick()?;
            let values = match self.visit(graph, id) {
                Visit::Skip => continue,
                Visit::Fail(error) => return Err(report(graph, id, error)),
                Visit::Pass(values) => values,
            };
            if !self.unique_tuple(&values) {
                continue;
            }
            // A NaN-bearing tuple has no key: it equals nothing.
            let Some(key) = tuple_key(&values) else {
                continue;
            };
            // A bucket holds key-equal tuples only: past its first entry,
            // only tuples with nulls inside lists or maps (key-equal but
            // never `eq_non_null`) can join it, so this is no label scan.
            let bucket = buckets.entry(key).or_default();
            if bucket.iter().any(|earlier| tuples_equal(earlier, &values)) {
                return Err(report(
                    graph,
                    id,
                    Error::uniqueness_violation(&self.def.name),
                ));
            }
            bucket.push(values);
        }
        Ok(())
    }

    /// The plain full re-validation: rule failures returned as raised.
    fn recheck(&self, graph: &ExecGraph<'_>) -> Result<()> {
        self.recheck_scan(graph, |_, _, error| error)
    }

    /// The apply scan's full re-validation (ADR-0226 §3): the same scan
    /// [`Self::recheck`] runs, but every failure names the constraint AND
    /// the offending element's element id, as the ADR pins for
    /// `db.ontology.apply`'s data-violation error.
    fn recheck_naming_elements(&self, graph: &ExecGraph<'_>) -> Result<()> {
        self.recheck_scan(graph, |graph, id, error| {
            self.apply_violation(graph, id, &error)
        })
    }

    /// ADR-0226 §3's data-violation error: names the constraint and the
    /// offending element's element id, and carries the underlying rule
    /// failure's message.
    fn apply_violation(&self, graph: &ExecGraph<'_>, id: i64, cause: &Error) -> Error {
        let namespace = graph.namespace();
        let element_id = match self.target {
            Target::Node { .. } => cypherlite_core::node_element_id(id, namespace),
            Target::Rel { .. } => cypherlite_core::rel_element_id(id, namespace),
        }
        .unwrap_or_else(|| id.to_string());
        let detail = match cause {
            Error::UniquenessViolation { name } => {
                cypherlite_core::uniqueness_violation_detail(name)
            }
            // The typed detail variants contribute the legacy bare inner
            // text, token included, as their stringly predecessors did.
            Error::InvalidArgumentType(detail) => format!("InvalidArgumentType: {detail}"),
            Error::TypeError(detail) => format!("TypeError: {detail}"),
            Error::ParameterMissing(detail) => format!("ParameterMissing: {detail}"),
            // ADR-0307: the legacy `Type`/`Execution` payload.
            Error::Arithmetic(failure) => failure.detail().to_string(),
            Error::Semantic(message)
            | Error::Execution(message)
            | Error::Type(message)
            | Error::Syntax(message) => message.clone(),
            // Every other class — the typed `DatabaseNotFound` included,
            // whose `typed_detail_text` is its bare token-and-detail text —
            // falls back to the rendered message, the same collapse
            // `transaction_error_message` applies.
            other => other
                .typed_detail_text()
                .unwrap_or_else(|| other.to_string()),
        };
        Error::semantic(format!(
            "ConstraintValidationFailed: `{}` violated by element `{element_id}`: {detail}",
            self.def.name
        ))
    }
}

/// Parse a constraint's stored property-type rendering (the canonical
/// `render_canon` string persisted on the constraint definition) back into
/// the structured type, once per constraint validation. Values are matched
/// against the result typed, via `eval::value_matches_type` — never with
/// string heuristics. An unparseable string yields `None`; callers fail
/// closed.
fn parse_constraint_type(ty: &str) -> Option<TypeExpr> {
    // A CIP-262 graph-type-derived property-type string carries its
    // `DEFAULT ON COMMIT <value>` suffix after the type; the check
    // validates the type prefix and ignores the suffix (the default is
    // applied before this check ever runs).
    let ty = ty
        .split_once(crate::graph_type::DEFAULT_ON_COMMIT_SEPARATOR)
        .map_or(ty, |(ty, _)| ty);
    cypherlite_parser::type_normalize::parse_canon_type_str(ty.trim())
        .map(|ty| cypherlite_front::lower_type_expr(&ty))
}

/// The property-index keys of a constrained tuple; `None` when a value
/// contains a NaN, so the tuple can equal nothing.
fn tuple_key(values: &[Value]) -> Option<Vec<GroupKey>> {
    values.iter().map(GroupKey::for_property_index).collect()
}

/// Uniqueness equality: every component `eq_non_null`, so `1` and `1.0`
/// conflict and a NaN never does.
fn tuples_equal(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(l, r)| l.eq_non_null(r))
}

/// Where the full scan raises `x`'s uniqueness conflict: at the later id of
/// a conflicting pair — `x` itself when a partner precedes it, otherwise
/// its smallest partner (ADR-0172 §6).
fn conflict_position(x: i64, partners: &[i64]) -> Option<i64> {
    partners.iter().copied().min().map(|y| y.max(x))
}

// The validation errors, byte-identical to the full scan's (ADR-0172 §6).
// The uniqueness violation is the typed `Error::UniquenessViolation`
// variant, constructed via `Error::uniqueness_violation` at the raise
// sites: the violated constraint's name travels as data, never as text to
// parse back — consumers match the variant, never the rendered message.

fn requires_error(name: &str) -> Error {
    Error::semantic(format!(
        "ConstraintValidationFailed: `{name}` requires properties"
    ))
}

fn property_type_error(name: &str) -> Error {
    Error::semantic(format!(
        "ConstraintValidationFailed: `{name}` property type"
    ))
}

fn node_label_error(label: &str, enforced: &str) -> Error {
    Error::execution(format!(
        "ConstraintValidationFailed: node with label {label} must also have label {enforced}"
    ))
}

fn endpoint_label_error(enforced: &str) -> Error {
    Error::execution(format!(
        "ConstraintValidationFailed: endpoint label presence verification failed: expected {enforced}"
    ))
}

/// A stored kind tag `ConstraintType::from_str` rejects fails closed
/// rather than leaving the constraint neither required nor unique.
fn unknown_kind_error(name: &str, reason: &str) -> Error {
    Error::semantic(format!("ConstraintValidationFailed: `{name}` {reason}"))
}

#[cfg(test)]
mod tests;
