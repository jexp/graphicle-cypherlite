//! The generator: random, realizable multi-slot flush groups.
//!
//! The generated shape stays inside the configuration the engine's other
//! admission gate (`redo_conflicts_with_later`) accepts: slots own disjoint
//! entities and relationship endpoints are never deleted. ADR-0439 only
//! defines admission semantics for that shape ("two accepted slots never
//! touch the same entity"); generating outside it would test undefined
//! configurations. An in-group DDL gets a slot of its own, so the
//! statement-time validator's same-slot coverage stays out of the admission
//! contract.

use std::collections::{BTreeMap, HashSet};

use super::model::MValue;

const NODE_COUNT: usize = 5;
const REL_COUNT: usize = 3;

/// The keyable value alphabet: every `Int` the generator can emit and the
/// complete set of tuple components the reference model must enumerate.
/// Both sides read it: the generator's value pickers
/// ([`Counters::take`], [`random_value`]) and the model's
/// `tuples_over` in `model.rs`. A value added here is generated and checked
/// in the same change, so a checker that lags the generator cannot pass
/// vacuously.
pub(super) const VALUE_ALPHABET: [i64; 4] = [0, 1, 2, 3];

#[derive(Clone, Debug)]
pub(super) enum TestOp {
    CreateNode {
        id: i64,
        labels: Vec<&'static str>,
        props: Vec<(&'static str, MValue)>,
    },
    CreateRel {
        id: i64,
        start: i64,
        end: i64,
        rel_type: &'static str,
        props: Vec<(&'static str, MValue)>,
    },
    DeleteNode {
        id: i64,
    },
    DeleteRel {
        id: i64,
    },
    SetNodeProp {
        id: i64,
        key: &'static str,
        value: MValue,
    },
    RemoveNodeProp {
        id: i64,
        key: &'static str,
    },
    SetRelProp {
        id: i64,
        key: &'static str,
        value: MValue,
    },
    RemoveRelProp {
        id: i64,
        key: &'static str,
    },
    AddLabels {
        id: i64,
        labels: Vec<&'static str>,
    },
    RemoveLabels {
        id: i64,
        labels: Vec<&'static str>,
    },
    CreateConstraint {
        name: &'static str,
        relationship: bool,
        label: &'static str,
        properties: Vec<&'static str>,
    },
    DropConstraint {
        name: &'static str,
    },
}

/// What one generated op does — the reference model's own classification,
/// mirroring the engine's `ConstraintOp` shape. Hand-written over `TestOp`
/// so the model stays independent of the engine's classifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TestEffect {
    /// Can add a constrained tuple.
    Introduce,
    /// Can only remove a constrained tuple.
    Free,
    /// Changes the constraint set itself.
    Ddl,
}

impl TestOp {
    /// The one exhaustive classification of a generated op.
    pub(super) fn effect(&self) -> TestEffect {
        match self {
            Self::CreateNode { .. }
            | Self::CreateRel { .. }
            | Self::SetNodeProp { .. }
            | Self::SetRelProp { .. }
            | Self::AddLabels { .. } => TestEffect::Introduce,
            Self::DeleteNode { .. }
            | Self::DeleteRel { .. }
            | Self::RemoveNodeProp { .. }
            | Self::RemoveRelProp { .. }
            | Self::RemoveLabels { .. } => TestEffect::Free,
            Self::CreateConstraint { .. } | Self::DropConstraint { .. } => TestEffect::Ddl,
        }
    }

    /// The constraint description this `CreateConstraint` op names, `None`
    /// for every other op — the one crossing from the op vocabulary to the
    /// reference model's description (the inverse of
    /// [`ConstraintDesc::to_op`]).
    pub(super) fn constraint_desc(&self) -> Option<ConstraintDesc> {
        match self {
            Self::CreateConstraint {
                name,
                relationship,
                label,
                properties,
            } => Some(ConstraintDesc {
                name,
                relationship: *relationship,
                label,
                properties: properties.clone(),
            }),
            _ => None,
        }
    }

    pub(super) fn is_ddl(&self) -> bool {
        self.effect() == TestEffect::Ddl
    }

    pub(super) fn needs_admission(&self) -> bool {
        matches!(self.effect(), TestEffect::Introduce | TestEffect::Ddl)
    }
}

#[derive(Clone, Debug)]
pub(super) struct TestCase {
    pub(super) prefix: Vec<TestOp>,
    pub(super) slots: Vec<Vec<TestOp>>,
}

/// One generated constraint's identity, named rather than positional so the
/// generator's `(name, relationship, label, properties)` tuple cannot be
/// read in the wrong order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConstraintDesc {
    pub(super) name: &'static str,
    pub(super) relationship: bool,
    pub(super) label: &'static str,
    pub(super) properties: Vec<&'static str>,
}

impl ConstraintDesc {
    /// Whether two descriptions name the same schema rule (the reference
    /// model's spelling of the production `DdlSpec::same_schema`).
    pub(super) fn same_schema(&self, other: &Self) -> bool {
        self.relationship == other.relationship
            && self.label == other.label
            && self.properties == other.properties
    }

    fn to_op(&self) -> TestOp {
        TestOp::CreateConstraint {
            name: self.name,
            relationship: self.relationship,
            label: self.label,
            properties: self.properties.clone(),
        }
    }
}

/// One in-group DDL choice, typed instead of an integer code. A create
/// carries the exact schema it would mint, so the realizability probe and
/// the constructed op read one description.
#[derive(Clone)]
enum DdlChoice {
    /// Drop one of the committed constraints.
    Drop,
    /// Create the carried schema — only realizable when no committed
    /// constraint owns it.
    Create(ConstraintDesc),
}

/// The in-group node-key candidate (`d`, `P.k`).
fn node_key_candidate() -> ConstraintDesc {
    ConstraintDesc {
        name: "d",
        relationship: false,
        label: "P",
        properties: vec!["k"],
    }
}

/// The in-group relationship-key candidate (`dr`, `R.k`).
fn rel_key_candidate() -> ConstraintDesc {
    ConstraintDesc {
        name: "dr",
        relationship: true,
        label: "R",
        properties: vec!["k"],
    }
}

#[derive(Default)]
struct Counters {
    p: usize,
    t: usize,
    r: usize,
}

impl Counters {
    /// Distinct values per constrained label group: each element of
    /// [`VALUE_ALPHABET`] once, then `Null` (unkeyable). A committed prefix
    /// must never start with a duplicate tuple, or the property would be
    /// blamed for a pre-existing replay violation.
    fn take(counter: &mut usize) -> MValue {
        let value = match VALUE_ALPHABET.get(*counter) {
            Some(&value) => MValue::Int(value),
            None => MValue::Null,
        };
        *counter += 1;
        value
    }

    fn next_p(&mut self) -> MValue {
        Self::take(&mut self.p)
    }

    fn next_t(&mut self) -> MValue {
        Self::take(&mut self.t)
    }

    fn next_r(&mut self) -> MValue {
        Self::take(&mut self.r)
    }
}

/// A generated entity's lifecycle, one discriminant instead of the
/// exists/deleted pair (dead-before-live is unrepresentable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GenLife {
    /// Not yet created in this slot.
    New,
    /// Created and alive.
    Live,
    /// Deleted in this slot.
    Dead,
}

#[derive(Clone, Copy)]
struct NodeGen {
    life: GenLife,
    protected: bool,
}

#[derive(Clone, Copy)]
struct RelGen {
    life: GenLife,
}

#[derive(Clone, Copy, Debug)]
enum Cand {
    CreateNode(i64),
    AddLabel(i64, &'static str),
    RemoveLabel(i64, &'static str),
    SetProp(i64, &'static str),
    RemoveProp(i64, &'static str),
    DeleteNode(i64),
    CreateRel(i64),
    SetRelProp(i64),
    RemoveRelProp(i64),
    DeleteRel(i64),
}

struct Entropy<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Entropy<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        assert!(!bytes.is_empty(), "the strategy supplies at least one byte");
        Self { bytes, position: 0 }
    }

    fn next(&mut self) -> u8 {
        let byte = self.bytes[self.position % self.bytes.len()];
        self.position += 1;
        byte
    }

    fn below(&mut self, upper: usize) -> usize {
        self.next() as usize % upper
    }

    fn chance(&mut self, percent: u8) -> bool {
        self.next() % 100 < percent
    }
}

/// The two relationship types the generator emits: the constrained `R` and
/// the unconstrained `NR`. The kind owns the coin→name mapping and the key
/// property's value policy, so a site cannot pick one without the others.
#[derive(Clone, Copy)]
enum RelKind {
    Constrained,
    Other,
}

impl RelKind {
    fn pick(rng: &mut Entropy<'_>, constrained_percent: u8) -> Self {
        if rng.chance(constrained_percent) {
            Self::Constrained
        } else {
            Self::Other
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Constrained => "R",
            Self::Other => "NR",
        }
    }

    /// The committed prefix's key-property policy: the constrained kind takes
    /// the next distinct counter value (the prefix must never start with a
    /// duplicate tuple), the other kind writes one only on the caller's coin.
    fn prefix_key_prop(
        self,
        rng: &mut Entropy<'_>,
        percent: u8,
        counters: &mut Counters,
    ) -> Option<(&'static str, MValue)> {
        match self {
            Self::Constrained => Some(("k", counters.next_r())),
            Self::Other if rng.chance(percent) => Some(("k", random_value(rng))),
            Self::Other => None,
        }
    }
}

fn random_value(rng: &mut Entropy<'_>) -> MValue {
    let alphabet_len = VALUE_ALPHABET.len();
    match rng.below(alphabet_len + 2) {
        index if index < alphabet_len => MValue::Int(VALUE_ALPHABET[index]),
        index if index == alphabet_len => MValue::Null,
        _ => MValue::Nan,
    }
}

/// The committed prefix and the state the slot phase reads from it.
struct Prefix {
    ops: Vec<TestOp>,
    committed: [bool; NODE_COUNT],
    protected: HashSet<i64>,
    committed_rels: HashSet<i64>,
    constraints: Vec<ConstraintDesc>,
}

pub(super) fn build_case(entropy: &[u8]) -> TestCase {
    let mut rng = Entropy::new(entropy);
    let has_ddl = rng.chance(45);
    let slot_count = if has_ddl {
        2 + rng.below(4)
    } else {
        1 + rng.below(5)
    };

    let mut counters = Counters::default();
    let prefix = build_prefix(&mut rng, &mut counters);
    // The in-group DDL and its owning slot are one value, and `has_ddl` is
    // the one place that decides whether one exists.
    let ddl = has_ddl.then(|| choose_ddl(&mut rng, &prefix.constraints, slot_count));
    let slots = build_slots(&mut rng, slot_count, ddl, &prefix);

    TestCase {
        prefix: prefix.ops,
        slots,
    }
}

/// Phase 1: the committed nodes, relationships, and constraints.
fn build_prefix(rng: &mut Entropy<'_>, counters: &mut Counters) -> Prefix {
    let mut committed = [false; NODE_COUNT];
    let mut any_committed = false;
    for flag in &mut committed {
        *flag = rng.chance(55);
        any_committed |= *flag;
    }
    if !any_committed {
        committed[0] = true;
    }

    let mut ops = Vec::new();
    for (id, is_committed) in committed.iter().enumerate() {
        if !is_committed {
            continue;
        }
        let mut labels = Vec::new();
        let mut props = Vec::new();
        if rng.chance(50) {
            labels.push("P");
            props.push(("k", counters.next_p()));
        }
        if rng.chance(50) {
            labels.push("T");
            props.push(("t", counters.next_t()));
        }
        ops.push(TestOp::CreateNode {
            id: id as i64,
            labels,
            props,
        });
    }

    let committed_nodes: Vec<i64> = (0..NODE_COUNT)
        .filter(|id| committed[*id])
        .map(|id| id as i64)
        .collect();
    let mut protected: HashSet<i64> = HashSet::new();
    let mut committed_rels: HashSet<i64> = HashSet::new();
    for id in 0..REL_COUNT {
        if !rng.chance(35) {
            continue;
        }
        let kind = RelKind::pick(rng, 60);
        let rel_type = kind.name();
        let mut props = Vec::new();
        if let Some(pair) = kind.prefix_key_prop(rng, 60, counters) {
            props.push(pair);
        }
        let start = committed_nodes[rng.below(committed_nodes.len())];
        let end = committed_nodes[rng.below(committed_nodes.len())];
        protected.insert(start);
        protected.insert(end);
        committed_rels.insert(id as i64);
        ops.push(TestOp::CreateRel {
            id: id as i64,
            start,
            end,
            rel_type,
            props,
        });
    }

    let mut constraints: Vec<ConstraintDesc> = Vec::new();
    if rng.chance(45) {
        constraints.push(ConstraintDesc {
            name: "c",
            relationship: false,
            label: "P",
            properties: vec!["k"],
        });
    }
    if rng.chance(30) {
        constraints.push(ConstraintDesc {
            name: "ct",
            relationship: false,
            label: "T",
            properties: vec!["t"],
        });
    }
    if rng.chance(40) {
        constraints.push(ConstraintDesc {
            name: "cr",
            relationship: true,
            label: "R",
            properties: vec!["k"],
        });
    }
    ops.extend(constraints.iter().map(ConstraintDesc::to_op));

    Prefix {
        ops,
        committed,
        protected,
        committed_rels,
        constraints,
    }
}

/// Whether `constraints` already owns the schema `candidate` would mint.
fn owns(constraints: &[ConstraintDesc], candidate: &ConstraintDesc) -> bool {
    constraints.iter().any(|c| c.same_schema(candidate))
}

/// Phase 2: pick the single in-group DDL op and the slot that owns it.
fn choose_ddl(
    rng: &mut Entropy<'_>,
    constraints: &[ConstraintDesc],
    slot_count: usize,
) -> (usize, TestOp) {
    // The catalog rejects a second constraint with an equivalent schema
    // (`EquivalentSchemaRuleAlreadyExists`) before it ever becomes a redo
    // op, so an in-group create is only realizable for a schema no committed
    // constraint already owns.
    let mut choices = Vec::new();
    if !constraints.is_empty() {
        choices.push(DdlChoice::Drop);
    }
    for candidate in [node_key_candidate(), rel_key_candidate()] {
        if !owns(constraints, &candidate) {
            choices.push(DdlChoice::Create(candidate));
        }
    }
    debug_assert!(!choices.is_empty(), "a called choose_ddl has a choice");
    // The DDL owns a slot with no entity ops: admission never checks a
    // same-slot install against the slot's own writes (the statement-time
    // validator owns that), so keeping them apart keeps the property inside
    // admission's stated contract. The slot starts at 1 (never 0) for the
    // same reason: slot 0 would leave `check_install` no earlier accepted
    // slot to replay.
    let slot = 1 + rng.below(slot_count - 1);
    let op = match choices[rng.below(choices.len())].clone() {
        DdlChoice::Drop => TestOp::DropConstraint {
            name: constraints[rng.below(constraints.len())].name,
        },
        DdlChoice::Create(candidate) => candidate.to_op(),
    };
    (slot, op)
}

/// Phase 3: the entity ops, one disjoint slot per entity id.
fn build_slots(
    rng: &mut Entropy<'_>,
    slot_count: usize,
    mut ddl: Option<(usize, TestOp)>,
    prefix: &Prefix,
) -> Vec<Vec<TestOp>> {
    let mut slots = Vec::with_capacity(slot_count);
    for slot in 0..slot_count {
        let mut ops = Vec::new();
        match ddl.take_if(|(ddl_slot, _)| *ddl_slot == slot) {
            // The DDL owns its slot whole: place it here and skip the entity
            // ops this slot would otherwise generate.
            Some((_, op)) => ops.push(op),
            None => {
                let budget = rng.below(5);
                let node_ids: Vec<i64> = (0..NODE_COUNT)
                    .filter(|id| *id % slot_count == slot)
                    .map(|id| id as i64)
                    .collect();
                let rel_ids: Vec<i64> = (0..REL_COUNT)
                    .filter(|id| *id % slot_count == slot)
                    .map(|id| id as i64)
                    .collect();
                let mut node_states: BTreeMap<i64, NodeGen> = node_ids
                    .iter()
                    .map(|&id| {
                        (
                            id,
                            NodeGen {
                                life: if prefix.committed[id as usize] {
                                    GenLife::Live
                                } else {
                                    GenLife::New
                                },
                                protected: prefix.protected.contains(&id),
                            },
                        )
                    })
                    .collect();
                let mut rel_states: BTreeMap<i64, RelGen> = rel_ids
                    .iter()
                    .map(|&id| {
                        (
                            id,
                            RelGen {
                                life: if prefix.committed_rels.contains(&id) {
                                    GenLife::Live
                                } else {
                                    GenLife::New
                                },
                            },
                        )
                    })
                    .collect();
                for _ in 0..budget {
                    let candidates = candidates(&node_ids, &rel_ids, &node_states, &rel_states);
                    if candidates.is_empty() {
                        break;
                    }
                    let choice = candidates[rng.below(candidates.len())];
                    apply_choice(choice, &mut ops, &mut node_states, &mut rel_states, rng);
                }
            }
        }
        slots.push(ops);
    }
    slots
}

fn candidates(
    node_ids: &[i64],
    rel_ids: &[i64],
    nodes: &BTreeMap<i64, NodeGen>,
    rels: &BTreeMap<i64, RelGen>,
) -> Vec<Cand> {
    let mut out = Vec::new();
    let alive_nodes = node_ids
        .iter()
        .filter(|id| nodes[*id].life == GenLife::Live)
        .count();
    for &id in node_ids {
        let state = nodes[&id];
        match state.life {
            GenLife::Dead => continue,
            GenLife::New => {
                out.push(Cand::CreateNode(id));
                continue;
            }
            GenLife::Live => {}
        }
        out.push(Cand::AddLabel(id, "P"));
        out.push(Cand::AddLabel(id, "T"));
        out.push(Cand::RemoveLabel(id, "P"));
        out.push(Cand::RemoveLabel(id, "T"));
        // Key writes weighted up: they are the tuple-changing shapes.
        out.push(Cand::SetProp(id, "k"));
        out.push(Cand::SetProp(id, "k"));
        out.push(Cand::SetProp(id, "t"));
        out.push(Cand::RemoveProp(id, "k"));
        out.push(Cand::RemoveProp(id, "t"));
        if !state.protected {
            out.push(Cand::DeleteNode(id));
        }
    }
    for &id in rel_ids {
        match rels[&id].life {
            GenLife::Dead => {}
            GenLife::Live => {
                out.push(Cand::SetRelProp(id));
                out.push(Cand::RemoveRelProp(id));
                out.push(Cand::DeleteRel(id));
            }
            GenLife::New if alive_nodes > 0 => out.push(Cand::CreateRel(id)),
            GenLife::New => {}
        }
    }
    out
}

fn apply_choice(
    choice: Cand,
    ops: &mut Vec<TestOp>,
    nodes: &mut BTreeMap<i64, NodeGen>,
    rels: &mut BTreeMap<i64, RelGen>,
    rng: &mut Entropy<'_>,
) {
    match choice {
        Cand::CreateNode(id) => {
            let mut labels = Vec::new();
            let mut props = Vec::new();
            if rng.chance(50) {
                labels.push("P");
            }
            if rng.chance(50) {
                labels.push("T");
            }
            if rng.chance(60) {
                props.push(("k", random_value(rng)));
            }
            if rng.chance(50) {
                props.push(("t", random_value(rng)));
            }
            ops.push(TestOp::CreateNode { id, labels, props });
            nodes.get_mut(&id).expect("owned node").life = GenLife::Live;
        }
        Cand::AddLabel(id, label) => ops.push(TestOp::AddLabels {
            id,
            labels: vec![label],
        }),
        Cand::RemoveLabel(id, label) => ops.push(TestOp::RemoveLabels {
            id,
            labels: vec![label],
        }),
        Cand::SetProp(id, key) => ops.push(TestOp::SetNodeProp {
            id,
            key,
            value: random_value(rng),
        }),
        Cand::RemoveProp(id, key) => ops.push(TestOp::RemoveNodeProp { id, key }),
        Cand::DeleteNode(id) => {
            ops.push(TestOp::DeleteNode { id });
            nodes.get_mut(&id).expect("owned node").life = GenLife::Dead;
        }
        Cand::CreateRel(id) => {
            let endpoints: Vec<i64> = nodes
                .iter()
                .filter(|(_, state)| state.life == GenLife::Live)
                .map(|(&id, _)| id)
                .collect();
            let start = endpoints[rng.below(endpoints.len())];
            let end = endpoints[rng.below(endpoints.len())];
            let rel_type = RelKind::pick(rng, 60).name();
            let mut props = Vec::new();
            if rng.chance(70) {
                props.push(("k", random_value(rng)));
            }
            ops.push(TestOp::CreateRel {
                id,
                start,
                end,
                rel_type,
                props,
            });
            rels.get_mut(&id).expect("owned rel").life = GenLife::Live;
            for endpoint in [start, end] {
                if let Some(state) = nodes.get_mut(&endpoint) {
                    state.protected = true;
                }
            }
        }
        Cand::SetRelProp(id) => ops.push(TestOp::SetRelProp {
            id,
            key: "k",
            value: random_value(rng),
        }),
        Cand::RemoveRelProp(id) => ops.push(TestOp::RemoveRelProp { id, key: "k" }),
        Cand::DeleteRel(id) => {
            ops.push(TestOp::DeleteRel { id });
            rels.get_mut(&id).expect("owned rel").life = GenLife::Dead;
        }
    }
}
