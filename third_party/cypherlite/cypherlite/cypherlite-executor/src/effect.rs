//! The engine's truthful statement write effect (ADR-0402 Decision 2): the
//! fact the CLI's zero-effect write note is allowed to read, recorded by the
//! execution itself instead of inferred from the plan's conservative
//! query-type classification or from matching clause names at a surface.
//!
//! A statement's plan-derived query type is deliberately conservative
//! (ADR-0332): a top-level `CALL { … }` and every `LOAD CSV` count as writes
//! so the namespace write gate cannot be bypassed, even when the body only
//! reads. It also cannot see catalog changes — `ALTER DATABASE`, a graph-type
//! change, `db.ontology.set` — which leave every ADR-0309 mutation counter at
//! zero while the store did change. Neither fact can answer "did this
//! statement write and apply nothing", so the executor records the answer
//! where it is known: the statement's plan holds a mutation operator, and the
//! statement's write redo took a change.

/// What one statement's execution did to the store: one of exactly three
/// states, never a pair of flags whose fourth combination has to be explained
/// away.
///
/// Both facts are recorded by the statement boundary itself, never inferred
/// from the statement's surface spelling:
///
/// - the *attempt* fact is the plan walk's answer — the statement's plan holds
///   a mutation operator (a data-write clause, schema DDL, a catalog write, or
///   a writes-flagged procedure call) — recorded once at statement entry
///   through the canonical walk, which descends into `NEXT` stages and every
///   nested body. It is a fact about the statement, not about rows, so a write
///   whose pipeline feeds it nothing — the zero-effect write's whole shape —
///   still counts as an attempt;
/// - the *applied* fact is the write scratch's own applied-change count, which
///   the statement entry reset zeroes and every applied change — entity,
///   schema or catalog — grows: one redo funnel, so a catalog-only change
///   counts.
///
/// The zero state has one spelling, [`Self::None`] — there is no constant
/// alias and no derived default beside it — and the join
/// ([`Self::merge`]) is an explicit three-state table, so no variant order is
/// semantic. There is no "applied but never attempted" state either: every
/// state that carries the applied fact ([`Self::Applied`]) also answers
/// [`Self::attempted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementEffect {
    /// The statement's plan holds no mutation operator, so nothing was
    /// applied: a read (and every pre-execution construction such as an
    /// `EXPLAIN` result). The policy stays silent however the planner
    /// classified the statement.
    None,
    /// The statement's plan holds a mutation operator and the engine applied
    /// nothing: the ADR-0402 "zero-effect write" (the LABS-313 shape, an
    /// idempotent `MERGE` that matched, a `MATCH … SET` whose `MATCH` bound no
    /// rows).
    Attempted,
    /// The statement's plan holds a mutation operator and at least one change
    /// reached the write redo — an entity, schema or catalog change alike.
    Applied,
}

impl StatementEffect {
    /// The effect of a statement from its two execution facts: `attempted` —
    /// the statement's plan holds a mutation operator (the boundary's one plan
    /// walk) — and `applied` — at least one change reached the write redo since
    /// the statement's entry reset.
    ///
    /// `applied` implies `attempted` for every boundary that takes both facts
    /// from one execution, and the state model has no fourth state to hold the
    /// contrary pair. A caller that passes it anyway still gets
    /// [`Self::Applied`] rather than a silently downgraded [`Self::None`]:
    /// under-reporting an applied change is the one error the note policy must
    /// never make.
    pub fn from_facts(attempted: bool, applied: bool) -> Self {
        match (attempted, applied) {
            (false, false) => Self::None,
            (true, false) => Self::Attempted,
            (false, true) | (true, true) => Self::Applied,
        }
    }

    /// The effect of a structural write boundary (the ADR-0209 bulk batch),
    /// where the attempt is implied and only the applied fact is observed.
    pub fn from_applied(applied: bool) -> Self {
        Self::from_facts(true, applied)
    }

    /// Whether the statement's plan holds a mutation operator.
    pub fn attempted(self) -> bool {
        self != Self::None
    }

    /// Whether the statement applied at least one change (an entity, schema
    /// or catalog change — every applied change funnels through the redo).
    pub fn applied(self) -> bool {
        self == Self::Applied
    }

    /// The ADR-0402 "zero-effect write" criterion: the statement's plan holds
    /// a mutation operator and the engine applied nothing. A read fails the
    /// first half, an effective write (catalog changes included) the second.
    pub fn changed_nothing(self) -> bool {
        self == Self::Attempted
    }

    /// Fold another statement's effect into this one — the join of the
    /// three-state lattice, which is how a transaction's ledger accumulates
    /// its statements: attempted-if-any-attempted, applied-if-any-applied.
    ///
    /// The table is spelled out variant by variant, so the declaration order
    /// of the enum carries no meaning and a fourth state cannot be added
    /// without deciding every join here.
    pub fn merge(&mut self, other: Self) {
        *self = match (*self, other) {
            (Self::None, Self::None) => Self::None,
            (Self::None, Self::Attempted)
            | (Self::Attempted, Self::None)
            | (Self::Attempted, Self::Attempted) => Self::Attempted,
            (Self::None, Self::Applied)
            | (Self::Applied, Self::None)
            | (Self::Attempted, Self::Applied)
            | (Self::Applied, Self::Attempted)
            | (Self::Applied, Self::Applied) => Self::Applied,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ADR-0402 criterion's truth table, exactly one row per state.
    #[test]
    fn changed_nothing_is_the_attempted_without_application_state() {
        assert!(
            !StatementEffect::None.changed_nothing(),
            "a read never reports, however the planner classified it"
        );
        assert!(
            StatementEffect::Attempted.changed_nothing(),
            "a mutation operator that applied nothing is the note's subject"
        );
        assert!(
            !StatementEffect::Applied.changed_nothing(),
            "an applied change — entity, schema or catalog — is not zero-effect"
        );
    }

    /// The three accessors read the state, and only the applied state
    /// answers both facts.
    #[test]
    fn the_accessors_read_the_state() {
        for (effect, attempted, applied) in [
            (StatementEffect::None, false, false),
            (StatementEffect::Attempted, true, false),
            (StatementEffect::Applied, true, true),
        ] {
            assert_eq!(effect.attempted(), attempted, "{effect:?}");
            assert_eq!(effect.applied(), applied, "{effect:?}");
        }
    }

    /// The join's explicit truth table — all nine pairs, spelled out rather
    /// than restated through the implementation (a `max` or a merged
    /// assertion would pass while the table was wrong).
    #[test]
    fn merge_is_the_three_state_join() {
        use StatementEffect::{Applied, Attempted, None};
        let table = [
            (None, None, None),
            (None, Attempted, Attempted),
            (None, Applied, Applied),
            (Attempted, None, Attempted),
            (Attempted, Attempted, Attempted),
            (Attempted, Applied, Applied),
            (Applied, None, Applied),
            (Applied, Attempted, Applied),
            (Applied, Applied, Applied),
        ];
        for (left, right, expected) in table {
            let mut joined = left;
            joined.merge(right);
            assert_eq!(joined, expected, "{left:?} join {right:?}");
            // The join is commutative, so a ledger folds in any order.
            let mut reversed = right;
            reversed.merge(left);
            assert_eq!(reversed, expected, "{right:?} join {left:?}");
        }
    }

    /// The facts constructor joins its two facts: applied implies attempted,
    /// so the pair the state model forbids cannot be built.
    #[test]
    fn from_facts_joins_the_two_facts() {
        for (attempted, applied, expected) in [
            (false, false, StatementEffect::None),
            (true, false, StatementEffect::Attempted),
            (true, true, StatementEffect::Applied),
            (false, true, StatementEffect::Applied),
        ] {
            assert_eq!(
                StatementEffect::from_facts(attempted, applied),
                expected,
                "attempted={attempted} applied={applied}"
            );
        }
    }
}
