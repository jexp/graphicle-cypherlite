//! The statement-position type the whole-tree walkers hand to their
//! statement visitor: a real statement, a UNION arm, a NEXT stage, or a
//! FOREACH body (a clause list that is not a `Statement`).

use cypherlite_parser::{Clause, LocalDefinition, Statement, UnionPart};

/// A statement position in the tree walk: a real [`Statement`] of the AST
/// (the top-level statement or a nested body — `CALL { }` / `WHEN` / `ELSE`
/// / `EXISTS` etc. — that starts its own chain context), a chain *link*
/// reached by following a `union` or `next` edge from the chain's head, or
/// a FOREACH body — a clause list that is not a `Statement` in the AST but
/// is presented to the statement visitor as a statement one nesting level
/// deeper, so per-statement validators (and the depth cap) cover it without
/// cloning the body into a synthetic `Statement`.
///
/// The link variants let the chain-aware validators
/// (`crate::validate::structure::validate_union_columns` /
/// `crate::validate::structure::validate_use_placement` /
/// `crate::validate::structure::validate_next_no_admin_commands`) run their
/// whole-chain suffix scan at
/// the chain's head only: the walk visits every link, so a link's own
/// re-scan of its suffix was pure redundancy — it could only re-report a
/// violation the head's earlier scan had already reported first — and it
/// made validating a length-L chain O(L²).
#[derive(Clone, Copy)]
pub enum StmtRef<'a> {
    Real(&'a Statement),
    /// A UNION arm: a statement reached via a `union` edge, so the head of
    /// its chain has already been visited (and has already scanned the
    /// whole chain) before this arm.
    UnionArm(&'a Statement),
    /// A CIP-213 NEXT stage: a statement reached via a `next` edge, so the
    /// head of its chain has already been visited before this stage.
    NextStage(&'a Statement),
    /// A FOREACH body: a bare clause list with no UNION/NEXT chain of its
    /// own.
    Body(&'a [Clause]),
}

impl<'a> StmtRef<'a> {
    pub(crate) fn clauses(self) -> &'a [Clause] {
        self.parts().1
    }

    pub(crate) fn union(self) -> Option<&'a UnionPart> {
        self.statement().and_then(|stmt| stmt.union.as_deref())
    }

    pub(crate) fn next(self) -> Option<&'a Statement> {
        self.statement().and_then(|stmt| stmt.next.as_deref())
    }

    /// The one place the `Real`/`UnionArm`/`NextStage` classification is
    /// spelled: the statement behind those variants (`None` for a FOREACH
    /// `Body`, which carries none) and this position's clause list.
    fn parts(self) -> (Option<&'a Statement>, &'a [Clause]) {
        match self {
            StmtRef::Real(stmt) | StmtRef::UnionArm(stmt) | StmtRef::NextStage(stmt) => {
                (Some(stmt), &stmt.clauses)
            }
            StmtRef::Body(clauses) => (None, clauses),
        }
    }

    fn statement(self) -> Option<&'a Statement> {
        self.parts().0
    }

    /// The CIP-231 `DEFINE` block of this statement (empty for a FOREACH
    /// `Body`, which is a bare clause list that cannot carry definitions).
    pub(crate) fn definitions(self) -> &'a [LocalDefinition] {
        self.statement().map_or(&[], |stmt| &stmt.definitions)
    }

    /// True for a statement reached via a `union` edge: a non-head link of
    /// a UNION chain.
    pub(crate) fn is_union_arm(self) -> bool {
        matches!(self, StmtRef::UnionArm(_))
    }

    /// True for a statement reached via a `next` edge: a non-head link of
    /// a CIP-213 NEXT chain.
    pub(crate) fn is_next_stage(self) -> bool {
        matches!(self, StmtRef::NextStage(_))
    }
}
