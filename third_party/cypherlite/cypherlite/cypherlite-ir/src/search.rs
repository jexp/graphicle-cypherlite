//! The SEARCH clause's specification family (CIP-225/CIP-278/CIP-240):
//! index and combined source specs, their expression-slot enumeration,
//! and the decomposed single-stage filter predicates.

use crate::ast::Expr;

use crate::BinaryOp;

/// CIP-278 `<source specification clause>`:
/// `<index specification clause> | <combined specification clause>`.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchSpec {
    /// Boxed to keep the variant sizes close (the flat index
    /// specification carries five `Expr` slots).
    Index(Box<SearchIndexSpec>),
    Combine(SearchCombineSpec),
}

/// CIP-225 `<index specification clause>`: `{ VECTOR | FULLTEXT } INDEX
/// <index name> <index for and control clause>`.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchIndexSpec {
    /// CIP-225: which index kind the `<index specification clause>` names.
    pub kind: SearchKind,
    pub index_name: String,
    pub for_expr: Expr,
    /// CIP-225 `WITH ANALYZER <expr>` (fulltext only; parsed, accepted, and
    /// not differentiated -- CypherLite has one tokenizer). Boxed to keep
    /// the enclosing `Clause` enum variant small.
    pub analyzer: Option<Box<Expr>>,
    /// CIP-240: the inner `WHERE <search condition>` decomposed once, at
    /// the front's lowering boundary, into its filter predicates (the
    /// `NOT <property>` and flipped-operand rewrites applied); empty when
    /// the clause carries no inner WHERE. The raw where expression does
    /// not travel past lowering.
    pub filters: Vec<SearchFilterPred>,
    /// CIP-225 `<offset clause>` (`SKIP`/`OFFSET`), shared with vector.
    pub offset_expr: Option<Box<Expr>>,
    pub limit_expr: Expr,
}

/// CIP-240: one decomposed inner-WHERE filter predicate —
/// `<binding variable> <property> <comp op> <rhs>` after the `NOT
/// <property>` -> `= FALSE` and flipped-operand rewrites. The binding
/// variable is the clause's own (Rule 1, enforced at lowering), so it is
/// not carried per predicate; the front decomposes the grammar once at
/// lowering and the executor consumes this list without re-walking it.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchFilterPred {
    pub property: String,
    pub op: SearchOp,
    pub rhs: Expr,
}

/// The five `<simple comp op>`s CIP-240 Rule 0 admits (Rule 7 bans the
/// rest) — the typed subset the filter predicates carry, so the op
/// vocabulary is stated once and every downstream match is exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOp {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl SearchOp {
    /// The mirror for Rule 0's flipped-operand rewrite — total over the
    /// subset, panic-free (`flip_cmp` serves the wider `BinaryOp`
    /// vocabulary, where the operand-asymmetry decline is real).
    pub fn flip(self) -> Self {
        match self {
            SearchOp::Eq => SearchOp::Eq,
            SearchOp::Lt => SearchOp::Gt,
            SearchOp::Gt => SearchOp::Lt,
            SearchOp::Lte => SearchOp::Gte,
            SearchOp::Gte => SearchOp::Lte,
        }
    }

    /// The one construction site: where a `BinaryOp` narrows into the
    /// simple subset. `None` is Rule 7's decline.
    pub fn from_binary(op: BinaryOp) -> Option<Self> {
        Some(match op {
            BinaryOp::Eq => SearchOp::Eq,
            BinaryOp::Lt => SearchOp::Lt,
            BinaryOp::Lte => SearchOp::Lte,
            BinaryOp::Gt => SearchOp::Gt,
            BinaryOp::Gte => SearchOp::Gte,
            _ => return None,
        })
    }
}

impl From<SearchOp> for BinaryOp {
    fn from(op: SearchOp) -> Self {
        match op {
            SearchOp::Eq => BinaryOp::Eq,
            SearchOp::Lt => BinaryOp::Lt,
            SearchOp::Lte => BinaryOp::Lte,
            SearchOp::Gt => BinaryOp::Gt,
            SearchOp::Gte => BinaryOp::Gte,
        }
    }
}

/// CIP-225 `<index specification clause>`: `{ VECTOR | FULLTEXT } INDEX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Vector,
    Fulltext,
}

/// CIP-278 `<combined specification clause>`: `COMBINE ( <weighted source
/// clause>, ... ) USING WRRF [ WITH CONSTANT <expr> ] [ LIMIT <expr> ]`.
/// Rule 5: two or more weighted sources (the parser rejects fewer).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchCombineSpec {
    pub sources: Vec<SearchWeightedSource>,
    /// Rule 4: `None` means the default WRRF constant 60.
    pub constant_expr: Option<Box<Expr>>,
    /// Rule 7: `None` keeps all fused results.
    pub limit_expr: Option<Box<Expr>>,
}

/// CIP-278 `<weighted source clause>`:
/// `<source specification clause> [ WEIGHT <value expression> ]`.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchWeightedSource {
    pub spec: SearchSpec,
    /// Rule 2: `None` means the default weight 1.
    pub weight_expr: Option<Box<Expr>>,
}

impl SearchSpec {
    /// Every value expression of this source specification tree, in
    /// source order: the index control expressions (FOR, ANALYZER, the
    /// CIP-240 decomposed predicates' RHS, OFFSET, LIMIT) and the combine
    /// tree's WEIGHT/CONSTANT/LIMIT expressions.
    pub fn exprs(&self) -> Vec<&Expr> {
        let mut out = Vec::new();
        push_spec_exprs(self, &mut out);
        out
    }
}

fn push_spec_exprs<'a>(spec: &'a SearchSpec, out: &mut Vec<&'a Expr>) {
    match spec {
        SearchSpec::Index(index) => {
            out.push(&index.for_expr);
            if let Some(expr) = &index.analyzer {
                out.push(expr);
            }
            for pred in &index.filters {
                out.push(&pred.rhs);
            }
            if let Some(expr) = &index.offset_expr {
                out.push(expr);
            }
            out.push(&index.limit_expr);
        }
        SearchSpec::Combine(combine) => {
            for source in &combine.sources {
                push_spec_exprs(&source.spec, out);
                if let Some(expr) = &source.weight_expr {
                    out.push(expr);
                }
            }
            if let Some(expr) = &combine.constant_expr {
                out.push(expr);
            }
            if let Some(expr) = &combine.limit_expr {
                out.push(expr);
            }
        }
    }
}
