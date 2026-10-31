/// AST node types for the Cypher parser.
///
/// The AST uses enums for Clause, Expr, and PatternElement to avoid trait objects.
/// Literal values use native Rust types (not CypherValue) since the AST is a compile-time concept.
///
/// Top-level AST node for a Cypher statement.
#[derive(Debug, Clone, Default)]
pub struct Statement {
    pub clauses: Vec<Clause>,
    pub union: Option<Box<UnionPart>>,
    pub explain: bool,
    pub profile: bool,
    /// CIP-213 `<statement block> ::= <statement> [<next statement>...]`,
    /// `<next statement> ::= NEXT <statement>`. `next` chains this
    /// statement (which may itself carry its own `.union` chain, or be a
    /// sole `Clause::Conditional`) to the following NEXT stage's own
    /// statement (which may in turn have a `.next` of its own). `None`
    /// means this is either not part of a NEXT chain, or is its final
    /// stage. Deliberately a *sibling* field to `union`/`Conditional`
    /// rather than nested inside them: per the CIP grammar, NEXT operates
    /// on a whole `<statement>` (union-chain-or-conditional), one level
    /// above where UNION/WHEN operate, so it's threaded through the
    /// outermost `Statement` of each stage only -- a stage's own `.union`
    /// arms never carry their own independent `.next` chain.
    pub next: Option<Box<Statement>>,
    /// CIP-150: the effective Cypher language version explicitly requested
    /// by a `CYPHER <version>` statement preamble (`5` or `25`), or `None`
    /// if the query had no version-bearing preamble clause at all (a
    /// version-less `CYPHER <options>` clause with no `<cypher version>`
    /// token also leaves this `None`). Only ever set on the outermost
    /// `Statement` a top-level `parse()` call returns -- the preamble is
    /// parsed once, before any clause, so nested statements (UNION arms,
    /// `CALL`/`EXISTS`/`COUNT` subquery bodies, NEXT stages) never carry
    /// their own value here and consumers must read it off the top-level
    /// statement. `None` means "use the session/database default
    /// language", which CypherLite always resolves to 25 (its only
    /// implemented dialect) since it has no CIP-170 default-language
    /// configuration surface.
    pub cypher_version: Option<u32>,
    /// CIP-146/CIP-242: true when this `SingleQuery` was written as
    /// `[ USE <graph-expr> ] '{' <RegularQuery> '}'` -- top-level braces --
    /// rather than the plain unbraced form. The brace form's *content* is
    /// otherwise indistinguishable from an unbraced statement (same clause
    /// list, same `.union` chain) once parsed, but CIP-242's SHOW/TERMINATE
    /// composability rule ("Composing a SHOW command without YIELD fails")
    /// treats an explicitly brace-wrapped bare `SHOW` (e.g. `{ SHOW INDEXES
    /// }`) as *composed*, requiring `YIELD`, even though the identical
    /// clause list unbraced (`SHOW INDEXES` alone) is the lenient
    /// "canonical standalone" form that doesn't need one -- so this needs
    /// its own marker distinct from clause shape. Set only by
    /// `parse_braced_single_query`; every other `Statement` constructor
    /// leaves it `false` (via `#[derive(Default)]` field-by-field
    /// construction elsewhere).
    pub brace_wrapped: bool,
    /// CIP-231: the `DEFINE <procedure definition> | <function
    /// definition>` block parsed at the head of this statement block (only
    /// the top-level statement and braced nested bodies carry definitions;
    /// every statement-block parse site funnels through
    /// `parse_statement_block`, where they are parsed). Rewritten away --
    /// definitions and their call sites eliminated -- by the front-end
    /// resolver (`cypherlite-front`'s `local_callables`) before lowering;
    /// the planner/executor never see local callables. `Default`-empty so
    /// the derived and hand-written constructors of un-gated statements
    /// are unchanged.
    pub definitions: Vec<LocalDefinition>,
}

/// Manual `PartialEq`: deliberately ignores `brace_wrapped`. CIP-146's own
/// "Semantics Step 3, Unwrap Case 1" -- and the parser tests pinning it
/// (`test_brace_enclosed_top_level_query_is_transparent` and friends) --
/// treat a brace-wrapped `SingleQuery` as producing an AST indistinguishable
/// from its unbraced spelling; `brace_wrapped` is out-of-band provenance a
/// couple of CIP-242 composability checks read directly off the field
/// (`validate_show_composability`), not part of that observable-content
/// equivalence.
impl PartialEq for Statement {
    fn eq(&self, other: &Self) -> bool {
        self.clauses == other.clauses
            && self.union == other.union
            && self.explain == other.explain
            && self.profile == other.profile
            && self.next == other.next
            && self.cypher_version == other.cypher_version
            && self.definitions == other.definitions
    }
}

/// CIP-231 `DEFINE` block entry: one local callable definition, parsed at
/// the head of a `<statement block>` (top level or a braced nested body)
/// and rewritten away by the front-end resolver
/// (`cypherlite-front`'s `local_callables`) before lowering -- the
/// planner/executor never see local callables.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalDefinition {
    /// `PROCEDURE` or `FUNCTION`.
    pub kind: LocalCallableKind,
    /// The fully-qualified callable name as written (`foo.bar`), namespace
    /// segments joined on `.`. Matched against call-site names verbatim.
    pub name: String,
    /// The `<callable input signature>`: parameters in declaration order.
    pub params: Vec<LocalParam>,
    /// The declared `<procedure output type>` (`(field :: T, ...)`) or
    /// `<function output type>` (`:: T`), when written.
    pub output: Option<LocalOutput>,
    /// `= <value expression>` (simple function body) or a braced
    /// `<procedure specification>` (procedure body or subquery function
    /// body).
    pub body: LocalDefinitionBody,
}

/// `DEFINE PROCEDURE` vs `DEFINE FUNCTION` (CIP-231). The two namespaces
/// are independent (CIP-230: a procedure and a function may share a name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalCallableKind {
    Procedure,
    Function,
}

/// One `<input field signature>` of a local callable (CIP-231): a field
/// name, an optional `[<typed>] <value type>`, and an optional
/// `= <value expression>` default (which makes the parameter optional).
#[derive(Debug, Clone, PartialEq)]
pub struct LocalParam {
    pub name: String,
    /// The written value type, `None` when omitted (implicitly ANY).
    /// Parsed and syntactically validated, deliberately not enforced at
    /// runtime (ADR-0382, NOTES.md).
    pub type_expr: Option<TypeExpr>,
    /// `= <value expression>`; `None` for a mandatory parameter. Defaults
    /// may not reference variables (checked at parse time).
    pub default: Option<Expr>,
}

/// A declared output type of a local callable (CIP-231): the procedure
/// form `:: (a :: INT, ...)` or the function form `:: STRING`.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalOutput {
    /// `:: (<mandatory field signature>, ...)` on a procedure.
    Fields(Vec<LocalOutputField>),
    /// `:: <value type>` on a function.
    Type(TypeExpr),
}

/// One field of a procedure's `:: (a :: INT, ...)` output signature.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalOutputField {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
}

/// The body of a local callable definition (CIP-231).
#[derive(Debug, Clone, PartialEq)]
pub enum LocalDefinitionBody {
    /// `FUNCTION f(...) = <value expression>` (simple function body).
    Expression(Expr),
    /// A braced `<procedure specification>`: a procedure body, or a
    /// subquery function body.
    Query(Statement),
}

/// Represents a UNION or UNION ALL between statements.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionPart {
    pub all: bool,
    pub statement: Statement,
}

/// All clause types in the Cypher language.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match(MatchClause),
    Create(CreateClause),
    Merge(MergeClause),
    Delete(DeleteClause),
    Set(SetClause),
    Remove(RemoveClause),
    Return(ReturnClause),
    With(WithClause),
    Unwind(UnwindClause),
    CreateIndex(CreateIndexClause),
    DropIndex(DropIndexClause),
    CreateConstraint(CreateConstraintClause),
    DropConstraint(DropConstraintClause),
    Foreach(ForeachClause),
    LoadCSV(LoadCSVClause),
    CallSubquery(CallSubqueryClause),
    CallProcedure(CallProcedureClause),
    ShowFunctions(ShowFunctionsClause),
    ShowConstraints(ShowConstraintsClause),
    ShowIndexes(ShowIndexesClause),
    /// `SHOW DATABASE[S] [<name>] [YIELD ...] [WHERE ...]` / `SHOW DEFAULT
    /// DATABASE` / `SHOW HOME DATABASE` (CIP-68's singular/plural KIND
    /// acceptance; CIP-42's four non-default columns). A minimal virtual
    /// catalog -- CypherLite is a single-database engine, so this reports a
    /// small set of deterministic built-in databases rather than modeling
    /// real multi-database administration.
    ShowDatabases(ShowDatabasesClause),
    /// `SHOW [ALL|POPULATED] ROLE[S] [WITH USER[S]] [YIELD ...] [WHERE ...]`
    /// (CIP-68). A minimal virtual catalog of built-in roles.
    ShowRoles(ShowRolesClause),
    /// `SHOW SERVER[S] [YIELD ...] [WHERE ...]` (CIP-68). Reports a single
    /// row, matching a single-process embedded engine.
    ShowServers(ShowServersClause),
    /// `SHOW USER[S] [YIELD ...] [WHERE ...]` / `SHOW CURRENT USER`
    /// (CIP-68). A minimal virtual catalog of a single built-in user.
    ShowUsers(ShowUsersClause),
    /// `SHOW PROCEDURE[S] [YIELD ...] [WHERE ...]` (CIP-68 grammar
    /// foundation). Lists the built-in procedure names the engine's `CALL`
    /// handler recognizes.
    ShowProcedures(ShowProceduresClause),
    /// `SHOW SETTING[S] [<names>] [YIELD ...] [WHERE ...]` (CIP-68 grammar
    /// foundation). CypherLite has no configurable settings surface, so
    /// this always reports an empty catalog.
    ShowSettings(ShowSettingsClause),
    /// `SHOW TRANSACTION[S] [<ids>] [YIELD ...] [WHERE ...]` (CIP-68
    /// grammar foundation). CypherLite executes each statement to
    /// completion without exposing concurrent transaction identifiers, so
    /// this always reports an empty catalog.
    ShowTransactions(ShowTransactionsClause),
    /// `TERMINATE TRANSACTION[S] <ids> [YIELD ...]` (CIP-68 grammar
    /// foundation). Since `ShowTransactions` never reports a live
    /// transaction, every requested id is reported as not found.
    TerminateTransactions(TerminateTransactionsClause),
    /// `USE <graph-expr>` (CIP-141 feature-interaction scenarios) -- a
    /// graph-selector clause. CypherLite is a single-database engine, so
    /// this is parsed and scope-checked (its expression's variables must
    /// be in scope, same as any other clause) but has no runtime effect
    /// beyond that: there is no composite/multi-graph database to switch
    /// to.
    Use(UseClause),
    /// `FINISH` (CIP-140) -- a terminal clause producing a truly empty
    /// result (0 rows, 0 columns) instead of RETURN's projected rows,
    /// while still running any earlier updating clauses. Only the minimal
    /// semantics needed by CIP-218's `CALL (a) { FINISH }` scenario are
    /// implemented: cardinality is preserved (one output row per input
    /// row) but that row carries no columns, so it contributes nothing
    /// when merged back into an enclosing CALL's outer row.
    Finish,
    /// CIP-168: a standalone `<order by and page statement>` -- e.g.
    /// `ORDER BY n.name SKIP 1 LIMIT 3` appearing as its own clause, not
    /// attached to WITH/RETURN/YIELD. Acts as an implicit `WITH *`
    /// horizon clause: changes row order/count but never the column set,
    /// and may sit between any two other clauses (including two write
    /// clauses). See `StandaloneOrderByClause` for why this is a sequence
    /// of stages rather than three fixed fields.
    StandaloneOrderBy(StandaloneOrderByClause),
    /// CIP-135: a standalone conditional clause chain --
    /// `<when query> ::= <when>, {<when>}, [<else>]`. This is a *sibling*
    /// alternative to `<union query>`/`<single query>` at the `<regular
    /// query>` grammar level (see `parse_regular_query`), not an ordinary
    /// mid-statement clause -- so this variant only ever appears as the
    /// sole element of a `Statement.clauses` vec, never alongside other
    /// clauses in the same statement.
    Conditional(ConditionalClause),
    /// `ALTER CURRENT GRAPH TYPE SET/ADD/ALTER/DROP { <graph type
    /// specification body> }` (CIP-176/182/183's mechanism; CIP-187's
    /// constraint derivation builds on it). `body` is the full CIP-178
    /// `<graph type specification body>` grammar (node/edge type specs,
    /// property types, and constraint specs) -- see `GraphTypeBody`.
    AlterCurrentGraphType(AlterCurrentGraphTypeClause),
    /// `CREATE GRAPH TYPE <name> [AS] { <graph type specification body> }`
    /// (CIP-178 grammar; the `AS` keyword is CIP-180's spelling of the same
    /// body-introducing form). Registers a *named* graph type, independent
    /// of the database's single unnamed "current" graph type that `ALTER
    /// CURRENT GRAPH TYPE` manages.
    CreateGraphType(CreateGraphTypeClause),
    /// `SHOW GRAPH TYPE <name> [YIELD ...]` (CIP-180's SHOW surface): a
    /// single-row catalog whose `name`/`body` columns report the named
    /// graph type's own name and its CIP-180 canonical specification
    /// string.
    ShowGraphType(ShowGraphTypeClause),
    /// `SHOW CURRENT GRAPH TYPE [YIELD ...]` (CIP-176 overview / CIP-184):
    /// the database's persistent current graph type, reported as a single
    /// `specification` column (the CIP-180 canonical body string wrapped in
    /// `{ }`, or the literal `{}` when empty).
    ShowCurrentGraphType(ShowCurrentGraphTypeClause),
    /// `ALTER DATABASE <name> [IF EXISTS] SET DEFAULT LANGUAGE CYPHER
    /// <n>` (CIP-170). Under ADR-0116 §2's fixed namespaces this is the
    /// only supported `ALTER DATABASE` form; every other form is
    /// recognized and rejected at parse time with CE's
    /// `UnsupportedAdministrationCommand`.
    AlterDatabase(AlterDatabaseClause),
}

/// See `Clause::AlterDatabase`.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterDatabaseClause {
    pub name: SchemaName,
    pub if_exists: bool,
    /// The `SET DEFAULT LANGUAGE CYPHER <n>` digits (e.g. `5`, `25`, or an
    /// unsupported value like `6`), unvalidated at parse time -- CIP-170
    /// NOTES.md Deviations #2: an out-of-range version here is a *runtime*
    /// data exception (`22N04`), unlike the CIP-150 statement-preamble
    /// `CYPHER <version>` prefix, which rejects at compile time.
    pub default_language: u32,
}

/// See `Clause::AlterCurrentGraphType`.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterCurrentGraphTypeClause {
    pub verb: AlterGraphTypeVerb,
    pub target: AlterCurrentGraphTypeTarget,
}

/// What an `ALTER CURRENT GRAPH TYPE` verb applies: an inline `{ <body> }`,
/// or — ADR-0211's `ALTER CURRENT GRAPH TYPE SET GRAPH TYPE <name>` — a
/// reference to a `CREATE GRAPH TYPE`-declared named type, promoted to
/// current. The invalid combinations (a verb body *and* a named reference)
/// are unrepresentable; no parser placeholder and no consumer-side doc
/// invariant.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterCurrentGraphTypeTarget {
    Body(GraphTypeBody),
    Named(String),
}

/// One `YIELD <name> [AS <alias>]` item of a procedure call or catalog
/// command. `alias` is `None` when no `AS` was written, so "no alias" is
/// represented by the optional itself rather than an empty-string sentinel
/// in a parallel array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YieldItem {
    pub name: String,
    pub alias: Option<String>,
}

impl YieldItem {
    /// The name this item binds in the enclosing scope: the alias when one
    /// was written, else the yielded column name.
    pub fn effective_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// A clause's `YIELD` tail, modeled once: the explicit items in source
/// order plus whether `YIELD *` was written. Empty `items` with `all ==
/// false` means no `YIELD` at all (the grammar never reads an empty
/// explicit list).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct YieldList {
    pub items: Vec<YieldItem>,
    pub all: bool,
}

/// A clause's yield mode, derived once from its [`YieldList`]: `YIELD *`
/// projects every column, a bare clause its default table, and an explicit
/// list its named (post-alias) columns. Consumers select their behavior
/// from this instead of each re-testing `(all, is_empty)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YieldMode {
    /// `YIELD *`: every column of the command.
    Star,
    /// No `YIELD` at all: the command's default table.
    Default,
    /// An explicit `YIELD` list: the named, post-alias columns.
    Named,
}

impl YieldList {
    /// The clause's yield mode — the one derivation.
    pub fn mode(&self) -> YieldMode {
        if self.all {
            YieldMode::Star
        } else if self.items.is_empty() {
            YieldMode::Default
        } else {
            YieldMode::Named
        }
    }

    /// Whether any `YIELD` tail was written (an explicit list or `*`).
    pub fn present(&self) -> bool {
        !matches!(self.mode(), YieldMode::Default)
    }

    /// Each item's raw yielded name, in source order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|item| item.name.as_str())
    }

    /// Each item's raw name, optional alias, and effective (post-alias)
    /// name, in source order. Consumers that need both sides of the alias
    /// rule read them from this one iterator instead of re-deriving the
    /// precedence.
    pub fn pairs(&self) -> impl Iterator<Item = (&str, Option<&str>, &str)> {
        self.items.iter().map(|item| {
            (
                item.name.as_str(),
                item.alias.as_deref(),
                item.effective_name(),
            )
        })
    }

    /// Each item's effective (post-alias) name, in source order.
    pub fn effective_names(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(YieldItem::effective_name)
    }
}

/// See `Clause::ShowCurrentGraphType`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShowCurrentGraphTypeClause {
    pub yields: YieldList,
    /// Paging stages following YIELD, preserved in source order.
    pub stages: Vec<PageStage>,
    /// CIP-184: an optional trailing `WHERE <expr>`, permitted with or
    /// without an explicit `YIELD` (mirrors `ShowDatabasesClause`).
    pub where_expr: Option<Expr>,
    /// CIP-257: `AS GRAPH [SINGLE | SEPARATE]` -- renders the current graph
    /// type as `nodes`/`relationships` columns instead of the plain
    /// `specification` string column. `None` when `AS GRAPH` wasn't
    /// written at all.
    pub as_graph: Option<AnyNodeMode>,
}

/// CIP-257's "Future improvements" any-node keyword, controlling how many
/// synthetic `(:AnyLabel {})` nodes an unconstrained relationship endpoint
/// gets: `Separate` (the default, whether or not the keyword is written) --
/// one any-node per relationship that needs one; `Single` -- every
/// unconstrained endpoint across the whole graph type shares one any-node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnyNodeMode {
    #[default]
    Separate,
    Single,
}

/// The four `ALTER CURRENT GRAPH TYPE` verbs (CIP-176/CIP-182 overview
/// scope): `SET` replaces the whole current graph type (and, as observed
/// live, every constraint in the database -- CIP-187 NOTES.md); `ADD`
/// extends it; `ALTER` replaces an existing element type (matched by
/// identifying label) with a new declaration; `DROP` removes the element
/// types named in its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterGraphTypeVerb {
    Set,
    Add,
    Alter,
    Drop,
}

/// See `Clause::CreateGraphType`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateGraphTypeClause {
    pub name: String,
    pub body: GraphTypeBody,
    /// Whether the `AS` spelling was used (CIP-180's own scaffolding form,
    /// `CREATE GRAPH TYPE <name> AS { <body> }`, vs. CIP-178's plain
    /// `CREATE GRAPH TYPE <name> { <body> }`). CIP-180's own worked
    /// example (`()-[:REL =>]->()`, no properties, both endpoints the any
    /// type) violates CIP-178 Rule 9 (an ineffectual single-label edge
    /// type) -- the `AS` form exists purely to exercise the CIP-180
    /// canonicalizer, not to be a validated graph-type declaration, so the
    /// engine's `CreateGraphType` handler skips `validate_graph_type_body`
    /// for it.
    pub via_as: bool,
}

/// See `Clause::ShowGraphType`.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowGraphTypeClause {
    pub name: String,
    pub yields: YieldList,
}

/// A fully parsed `<graph type specification body>` (CIP-178): the `{
/// ... }` contents shared by `CreateGraphTypeClause` and
/// `AlterCurrentGraphTypeClause`. Parsing is grammar-only (always succeeds
/// once the tokens match); the CIP-178 "shall" rules (alias/label
/// uniqueness, reference resolution, ...) are enforced separately by
/// `cypherlite-executor`'s graph-type validator (`validate_graph_type_body`),
/// which only `CREATE GRAPH
/// TYPE` invokes -- `ALTER CURRENT GRAPH TYPE` bodies legitimately reference
/// element types declared by an *earlier* statement (e.g. `ADD`), which a
/// single-body validation pass cannot see.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphTypeBody {
    pub node_types: Vec<GraphTypeNodeTypeSpec>,
    pub edge_types: Vec<GraphTypeEdgeTypeSpec>,
    pub constraints: Vec<GraphTypeConstraintSpec>,
    /// CIP-182 `<constraint drop specification>` = `CONSTRAINT <name>` (bare,
    /// no `FOR ...`) -- only allowed in an `ALTER CURRENT GRAPH TYPE DROP {
    /// ... }` body. The parser reads it in any body, and every other verb
    /// rejects a non-empty list as a parse error
    /// (`Parser::reject_bare_constraint_drops`), so this is always empty for
    /// `CREATE GRAPH TYPE`/`SET`/`ADD`/`ALTER` bodies.
    pub drop_constraint_names: Vec<String>,
}

/// `'(' [alias] (':'|IS) <identifying label> ('=>'|IMPLIES) [(':'|IS)
/// <implied label> ('&' <implied label>)*] ['{' <property types> '}'] ')'`
/// (CIP-178 `<node type specification>`).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphTypeNodeTypeSpec {
    pub alias: Option<String>,
    pub identifying_label: String,
    pub implied_labels: Vec<String>,
    pub properties: Vec<GraphTypePropertyType>,
}

/// `<source endpoint> '-' '[' [alias] (':'|IS) <identifying label>
/// ('=>'|IMPLIES) ['{' <property types> '}'] ']' '->' <destination
/// endpoint>` (CIP-178 `<edge type specification>`).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphTypeEdgeTypeSpec {
    pub alias: Option<String>,
    pub identifying_label: String,
    pub properties: Vec<GraphTypePropertyType>,
    pub source: GraphTypeElementRef,
    pub target: GraphTypeElementRef,
}

/// One `<property type>`: a name, its declared value type, an optional
/// `DEFAULT ON COMMIT <value expression>` (CIP-262), and an optional
/// inline `IS [NODE|REL|RELATIONSHIP] (KEY|UNIQUE)` constraint (CIP-178
/// Rules 10-12 rewrite these into standalone `GraphTypeConstraintSpec`s
/// during registration).
///
/// The default value is parsed as a full expression here (grammar-only,
/// like the rest of the body): the CIP-262 shape restriction -- a literal
/// or an allowed type constructor, never `null` -- is enforced by the
/// graph-type semantic layer, and the construct itself sits behind the
/// ADR-0371 `graph-type-default-values` preview gate at statement
/// admission (the trio's decision: ADR-0388). `None` is the no-default
/// case.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphTypePropertyType {
    pub name: String,
    pub value_type: TypeExpr,
    pub default_value: Option<Expr>,
    pub inline_constraint: Option<GraphTypeInlineConstraint>,
}

/// An inline property-type constraint (`{prop :: TYPE IS [qualifier]
/// KEY|UNIQUE}`). `qualifier` records which (if any) `NODE`/`REL`/
/// `RELATIONSHIP` keyword was written, so the graph-type validator can
/// enforce CIP-178 Rule 2 (a node type must not contain `REL`/
/// `RELATIONSHIP`; an edge type must not contain `NODE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphTypeInlineConstraint {
    pub verb: GraphTypeInlineVerb,
    pub qualifier: Option<GraphTypeEntityQualifier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphTypeInlineVerb {
    Key,
    Unique,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphTypeEntityQualifier {
    Node,
    Rel,
}

/// A `<node type reference>` / `<edge type reference>` (CIP-178): how a
/// node/edge type spec's endpoint, or a constraint specification's `FOR`
/// target, names the element type it applies to.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphTypeElementRef {
    /// `()` -- the any node/edge type.
    Any,
    /// `(alias)` / `()-[alias]->()` -- resolves to a previously declared
    /// node/edge type alias (Rule 1.1.3: must already be declared).
    AliasRef(String),
    /// `[alias] (':'|IS) <label> ['=>'|IMPLIES]` -- an in-situ reference.
    /// `implies` (`=>`/`IMPLIES` present) requires `label` to name an
    /// already-declared identifying label (Rule 1.2.3); without it, the
    /// label need not be declared. `alias` is only legal in constraint
    /// `FOR`-clause context (Rules 3/4 forbid it on an edge type's own
    /// endpoint references).
    InSitu {
        alias: Option<String>,
        label: String,
        implies: bool,
    },
}

/// `[CONSTRAINT [<name>]] FOR <node/edge type reference> REQUIRE
/// <property name spec> <constraint verb> [OPTIONS <map>]` (CIP-178
/// `<constraint specification>`) -- also the target of a node/edge type
/// spec's trailing inline `REQUIRE ...` list and a property's own inline
/// `IS KEY`/`IS UNIQUE` (both rewritten to this same shape at parse time,
/// per Rules 10-12).
#[derive(Debug, Clone, PartialEq)]
pub struct GraphTypeConstraintSpec {
    pub name: Option<String>,
    pub target: GraphTypeElementRef,
    /// True when `target` is an edge type reference (`FOR ()-[...]->()`);
    /// false for a node type reference (`FOR (...)`).
    pub is_edge: bool,
    pub properties: Vec<String>,
    pub kind: GraphTypeConstraintVerb,
    /// The parsed `<value type>` for a `PropertyType`-kind constraint,
    /// `None` for every other kind.
    pub property_type: Option<TypeExpr>,
    pub options: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphTypeConstraintVerb {
    Key,
    Unique,
    Exists,
    PropertyType,
}

/// CIP-135 conditional clause: `WHEN <pred> THEN { <body> } [WHEN <pred>
/// THEN { <body> }]* [ELSE { <body> }]`. Each branch's `body`/`else_body`
/// is itself a `<single-query>` (own `Statement`, possibly carrying its own
/// `.union` chain).
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionalClause {
    pub branches: Vec<WhenBranch>,
    pub else_body: Option<Statement>,
}

/// One `WHEN <predicate> THEN <single-query>` branch of a `ConditionalClause`.
#[derive(Debug, Clone, PartialEq)]
pub struct WhenBranch {
    pub predicate: Expr,
    pub body: Statement,
}

/// MATCH clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    pub optional: bool,
    /// CIP-51/CIP-60 match mode: whether relationship uniqueness (the
    /// default `DIFFERENT RELATIONSHIPS`) is enforced across the whole
    /// comma-separated graph pattern, or relaxed (`REPEATABLE ELEMENTS`).
    pub match_mode: MatchMode,
    pub pattern: Vec<PatternPath>,
    pub where_expr: Option<Expr>,
    /// CIP-224 `<search clause>`: `SEARCH <binding var ref> IN (VECTOR
    /// INDEX <index name> FOR <expr> LIMIT <expr>) [SCORE AS <var>]`, a
    /// subclause of `<match statement>`. The grammar allows the `<graph
    /// pattern where clause>` to appear either before or after the search
    /// clause with no semantic difference (CIP-224 "Canonical syntax"), so
    /// the parser folds both orderings into this same `where_expr` field --
    /// the source order isn't tracked in the AST.
    pub search: Option<SearchClause>,
}

/// CIP-224 `<search clause>`: the `SEARCH ... IN (<source specification
/// clause>) [SCORE AS ...]` subclause of `MatchClause`.
///
/// `binding_var`/`score_var` (and each nested `index_name`) are plain
/// identifiers (never a general expression, a parameter, or a property
/// access -- Rules 1, 4, 5) rather than `Expr`, matching the grammar's own
/// `<binding variable reference>` / `<index name specification>` /
/// `<binding variable>` productions.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchClause {
    /// `<binding variable reference>`: must name a variable declared by the
    /// same `<match statement>`'s own pattern (Rule 4).
    pub binding_var: String,
    /// CIP-278 `<source specification clause>`: one index search, or a
    /// `COMBINE` of two or more weighted sources fused with WRRF.
    pub spec: SearchSpec,
    /// `[SCORE AS <binding variable>]` -- `None` when the score column is
    /// omitted (results are still ordered by descending score either way).
    pub score_var: Option<String>,
}

/// CIP-278 `<source specification clause>`:
/// `<index specification clause> | <combined specification clause>`.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchSpec {
    Index(SearchIndexSpec),
    Combine(SearchCombineSpec),
}

/// CIP-225 `<index specification clause>`: `{ VECTOR | FULLTEXT } INDEX
/// <index name> <index for and control clause>`.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchIndexSpec {
    /// CIP-225: which index kind the specification names.
    pub kind: SearchKind,
    /// `<index name specification>`: the literal name of an existing index.
    pub index_name: String,
    /// `<for clause>`'s `<value expression>` -- the search query.
    pub for_expr: Expr,
    /// CIP-225 `WITH ANALYZER <expr>` (fulltext only).
    pub analyzer: Option<Expr>,
    /// CIP-240 "Vector Search with Single-stage Filtering": the optional
    /// inner `WHERE <search condition>` between `FOR <expr>` and `LIMIT
    /// <expr>`. Its `<search condition>` must decompose into one or more
    /// `<binding variable reference> <static property reference> <simple
    /// comp op> <arithmetic value expression>` predicates joined by `AND`
    /// (or a form rewritable into that, e.g. `NOT x.prop` or flipped
    /// operands) -- enforced by `cypherlite_front::search_filter`, not here;
    /// the parser accepts any general `<expression>` here, exactly like
    /// `MatchClause.where_expr`.
    pub inner_where: Option<Expr>,
    /// CIP-225 `<offset clause>` (`SKIP`/`OFFSET`).
    pub offset_expr: Option<Expr>,
    /// `<limit clause>`'s `<value expression>` (mandatory for an index
    /// source, in and out of a COMBINE).
    pub limit_expr: Expr,
}

/// CIP-225 `<index specification clause>`: `{ VECTOR | FULLTEXT } INDEX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Vector,
    Fulltext,
}

/// CIP-278 `<combined specification clause>`: `COMBINE ( <weighted source
/// clause>, ... ) USING WRRF [ WITH CONSTANT <expr> ] [ LIMIT <expr> ]`.
///
/// Rule 5: `sources` holds two or more weighted source clauses (fewer is a
/// parse error, raised where the list is parsed). A source may itself be a
/// nested `<combined specification clause>` (Example P3).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchCombineSpec {
    pub sources: Vec<SearchWeightedSource>,
    /// `<constant specification clause>`'s `<value expression>` -- the WRRF
    /// dampening constant; `None` means the default 60 (Rule 4).
    pub constant_expr: Option<Expr>,
    /// The optional `<limit clause>` of the combined specification -- the n
    /// highest-scoring results kept; `None` keeps all (Rule 7).
    pub limit_expr: Option<Expr>,
}

/// CIP-278 `<weighted source clause>`:
/// `<source specification clause> [ WEIGHT <value expression> ]`.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchWeightedSource {
    pub spec: SearchSpec,
    /// Rule 2: `None` means the default weight 1.
    pub weight_expr: Option<Expr>,
}

/// CIP-51/CIP-60 match mode: governs the default relationship-uniqueness
/// pool that spans one MATCH clause's entire comma-separated graph pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// `MATCH DIFFERENT RELATIONSHIPS ...` (also the implicit default): no
    /// two relationship slots anywhere in the graph pattern may bind to the
    /// same physical relationship.
    #[default]
    DifferentRelationships,
    /// `MATCH REPEATABLE ELEMENTS ...`: relaxes the relationship-uniqueness
    /// pool entirely -- nodes and relationships may repeat freely, subject
    /// only to each path's own path mode (which must resolve to `WALK`
    /// under this match mode, CIP-51 Section 3.1.3 rule 4).
    RepeatableElements,
}

/// CIP-51 path mode: an explicit restrictor on one path pattern's candidate
/// bindings. `PatternPath::mode` is `None` when no explicit keyword was
/// written -- Rule 1: the resolved default is `Walk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    /// Nodes and relationships may repeat freely (subject to the enclosing
    /// match mode's own relationship-uniqueness pool, which under the
    /// default `DIFFERENT RELATIONSHIPS` match mode still applies).
    Walk,
    /// No two relationship bindings in this one path may bind the same
    /// relationship (already the effective default under `DIFFERENT
    /// RELATIONSHIPS`, since that pool subsumes a single path's own).
    Trail,
    /// No two node bindings in this one path may bind the same node, except
    /// where two connectors are juxtaposed at a shared boundary node.
    Acyclic,
    /// Like `Acyclic`, but the very first and very last node of the path may
    /// coincide (an equijoin on the endpoints).
    Simple,
}

/// The `<number of paths>`/`<number of groups>` non-terminal (CIP-73 Rule 0,
/// extended by CIP-191 to accept a `$parameter`). A literal is validated
/// (`> 0`) at parse time; a parameter is validated at runtime once its value
/// is known (CIP-191 "Non-negative integer" rule).
#[derive(Debug, Clone, PartialEq)]
pub enum PathCount {
    Literal(u32),
    Parameter(String),
}

/// CIP-73 path selector prefix (`<path search prefix>`). `PatternPath` has
/// no selector when this is `None` -- Rule 3: the path pattern is then
/// non-selective, equivalent to an implicit `ALL`.
#[derive(Debug, Clone, PartialEq)]
pub enum PathSelector {
    /// `ALL [path mode] [PATH|PATHS]` -- also the implicit default.
    All,
    /// `ANY [<number of paths>] [path mode] [PATH|PATHS]`; `None` count
    /// defaults to 1 (Rule 0).
    Any(Option<PathCount>),
    /// `ANY SHORTEST [path mode] [PATH|PATHS]` -- normalizes to `SHORTEST 1
    /// [path mode] PATH` (Rule 2).
    AnyShortest,
    /// `ALL SHORTEST [path mode] [PATH|PATHS]` -- normalizes to `SHORTEST 1
    /// [path mode] GROUP` (Rule 2).
    AllShortest,
    /// `SHORTEST <number of paths> [path mode] [PATH|PATHS]` (`group` false)
    /// or `SHORTEST [<number of groups>] [path mode] [PATH|PATHS]
    /// (GROUP|GROUPS)` (`group` true); `count` is `None` only in the GROUP
    /// form (defaults to 1, Rule 0).
    Shortest {
        count: Option<PathCount>,
        group: bool,
    },
}

/// CREATE clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateClause {
    pub pattern: Vec<PatternPath>,
    /// True when this clause was written as `INSERT` (CIP-114) rather than
    /// `CREATE` -- executed identically in every other respect, but the
    /// `+labels` side-effect stat is *not* subject to CREATE's row-scoped
    /// same-label-name dedup (empirically, INSERT counts every label
    /// application on every node it creates, unlike CREATE/MERGE).
    pub is_insert: bool,
}

/// MERGE clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClause {
    pub pattern: PatternPath,
    pub on_create: Vec<SetItem>,
    pub on_match: Vec<SetItem>,
}

/// DELETE clause.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteClause {
    pub detach: bool,
    pub expressions: Vec<Expr>,
}

/// SET clause.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    pub items: Vec<SetItem>,
}

/// A single item in a SET clause.
#[derive(Debug, Clone, PartialEq)]
pub struct SetItem {
    pub property: Option<PropertyAccess>,
    pub variable: Option<String>,
    /// Each element is a `LabelExpr::Label` (static, `SET n:A`) or
    /// `LabelExpr::Dynamic` (CIP-162, `SET n:$(<expr>)`/`SET n IS $(<expr>)`)
    /// atom -- the parser never produces any other `LabelExpr` variant here
    /// (no `&`/`|`/`!`/`%`/grouping). See `RemoveItem::labels`.
    pub labels: Vec<LabelExpr>,
    pub value: Option<Expr>,
    pub merge: bool,
    /// True if this label item used the CIP-126 `IS` keyword (`SET n IS A`)
    /// rather than `:` (`SET n:A`). Only meaningful when `labels` is
    /// non-empty. Used for the CIP-40/CIP-126 statement-level rule that
    /// forbids mixing `IS` with a colon-conjunction (`:A:B`) anywhere in the
    /// same statement.
    pub is_keyword: bool,
    /// CIP-162 "Alternatives" section: `SET labels(<expr>) = / += / -=
    /// <expr>`. Per the CIP's own text this was never shipped in Neo4j
    /// ("we have decided to implement the rest first, and leave this as a
    /// potential future extension") -- CypherLite implements it anyway as
    /// the CIP's stated target semantics. `Some((target_expr, op))` when
    /// this item is that form, with `target_expr` the node expression
    /// inside `labels(...)` and `value` (above) holding the RHS list
    /// expression.
    pub labels_function: Option<(Expr, LabelsFunctionOp)>,
}

/// The operator of a CIP-162 `SET labels(<expr>) <op> <expr>` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelsFunctionOp {
    /// `=` -- replace the full label set.
    Overwrite,
    /// `+=` -- add every listed label, keep existing ones.
    Extend,
    /// `-=` -- remove every listed label that is present.
    Remove,
}

/// REMOVE clause.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveClause {
    pub items: Vec<RemoveItem>,
}

/// A single item in a REMOVE clause.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveItem {
    pub property: Option<PropertyAccess>,
    pub variable: Option<String>,
    /// See `SetItem::labels` (CIP-162 dynamic-label atoms allowed here too).
    pub labels: Vec<LabelExpr>,
    /// See `SetItem::is_keyword`.
    pub is_keyword: bool,
}

/// RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub group_by: Option<GroupBySpec>,
    pub distinct: bool,
    pub items: Vec<ReturnItem>,
    pub order_by: Vec<OrderByItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// A single item in a RETURN clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    pub expr: Expr,
    pub alias: Option<String>,
    pub all: bool,
    pub source_text: Option<String>,
}

/// An item in ORDER BY.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub descending: bool,
}

/// Which surface syntax produced a `WithClause`. Plain `WITH`, CIP-206's
/// `LET`, and CIP-203's `FILTER` all desugar into the same `WithClause` so
/// they share WITH's projection/aggregation/ORDER BY/SKIP/LIMIT/WHERE
/// execution machinery and its generic variable-scope tracking -- but LET
/// and FILTER each carry restrictions ordinary WITH does not (LET: no
/// shadowing of an in-scope variable, no sibling/forward/self reference
/// within one LET's item list, no aggregation function in a value
/// expression; FILTER: its search condition's declared type must be
/// Boolean). The origin is recorded so the validation passes enforcing
/// those extra rules only fire for the syntax that actually carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WithOrigin {
    #[default]
    Explicit,
    Let,
    Filter,
}

/// CIP-236 `<group by clause>` on RETURN/WITH.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupBySpec {
    /// `GROUP BY <expr> [, <expr>...]`
    Keys(Vec<Expr>),
    /// `GROUP BY ()` -- the empty grouping set: one global group.
    Empty,
    /// `GROUP BY ALL` -- every non-aggregating return item is a key.
    All,
}

/// WITH clause.
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    pub group_by: Option<GroupBySpec>,
    pub distinct: bool,
    pub items: Vec<ReturnItem>,
    pub order_by: Vec<OrderByItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
    pub where_expr: Option<Expr>,
    pub origin: WithOrigin,
}

/// One stage of a CIP-168 standalone `<order by and page statement>`.
/// Neo4j 2026.05 accepts `ORDER BY`/`OFFSET`/`SKIP`/`LIMIT` in any order
/// at the start of the clause and applies them sequentially in the order
/// written -- not the fixed ORDER-BY-then-OFFSET-then-LIMIT semantic
/// order the CIP text describes (see CIP-168 NOTES.md "Deviations" #1,
/// e.g. `LIMIT 3 OFFSET 1` keeps 3 rows then drops the first 1 of those,
/// rather than offsetting first). A `Vec<PageStage>` records exactly the
/// written sequence so execution can replay it faithfully.
#[derive(Debug, Clone, PartialEq)]
pub enum PageStage {
    OrderBy(Vec<OrderByItem>),
    Skip(Expr),
    Limit(Expr),
}

/// CIP-168: a standalone `<order by and page statement>`. See
/// `Clause::StandaloneOrderBy`.
#[derive(Debug, Clone, PartialEq)]
pub struct StandaloneOrderByClause {
    pub stages: Vec<PageStage>,
}

/// UNWIND clause. CIP-250 (GQ10): a `FOR <variable> IN <expression>`
/// clause normalizes here at parse time -- the same expr/alias pair with
/// [`UnwindClause::for_syntax`] recording the source spelling -- so every
/// downstream consumer (validation, lowering, planning, execution) treats
/// the two spellings identically, exactly as the CIP requires (no new
/// operators). The flag is what the ADR-0371 `gql-for` preview gate
/// detects and what a canonical rendering restores (`FOR x IN e`, never
/// `UNWIND e AS x`).
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    pub expr: Expr,
    pub alias: String,
    /// True when the clause was written `FOR <variable> IN <expression>`
    /// (CIP-250) rather than `UNWIND <expression> AS <variable>`.
    pub for_syntax: bool,
}

/// The DDL index kind (CIP-11 RANGE/POINT/TEXT, CIP-38 FULLTEXT, the
/// pre-existing VECTOR, and the label/type-token LOOKUP index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Range,
    Point,
    Text,
    Fulltext,
    Lookup,
    Vector,
}

impl IndexKind {
    /// The canonical lowercase tag persisted in
    /// `cypherlite_storage::Index::index_type` -- also the tag the engine
    /// switches on to decide which (if any) in-memory acceleration
    /// structure to build for a given index (`Engine::rebuild_indexes`).
    /// Unchanged for `Point`/`Vector` from before CIP-11 so existing
    /// vector/point index acceleration keeps working untouched; `Range`
    /// replaces the old ad hoc `"secondary"` tag (still accepted as a
    /// legacy alias when reading it back).
    pub fn storage_tag(&self) -> &'static str {
        match self {
            IndexKind::Range => "range",
            IndexKind::Point => "point",
            IndexKind::Text => "text",
            IndexKind::Fulltext => "fulltext",
            IndexKind::Lookup => "lookup",
            IndexKind::Vector => "vector",
        }
    }

    /// The upper-case display name Neo4j's `SHOW INDEXES` `type` column
    /// uses.
    pub fn display_name(&self) -> &'static str {
        match self {
            IndexKind::Range => "RANGE",
            IndexKind::Point => "POINT",
            IndexKind::Text => "TEXT",
            IndexKind::Fulltext => "FULLTEXT",
            IndexKind::Lookup => "LOOKUP",
            IndexKind::Vector => "VECTOR",
        }
    }
}

/// Which kind of graph element a CREATE INDEX / CREATE CONSTRAINT `FOR`
/// pattern targets (CIP-11's relationship-pattern generalization: `FOR
/// (n:Label)` vs. `FOR ()-[r:TYPE]-()` in all four direction spellings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaEntityType {
    Node,
    Relationship,
}

impl SchemaEntityType {
    /// The upper-case display value Neo4j's `SHOW INDEXES`/`SHOW
    /// CONSTRAINTS` `entityType` column uses.
    pub fn as_str(&self) -> &'static str {
        match self {
            SchemaEntityType::Node => "NODE",
            SchemaEntityType::Relationship => "RELATIONSHIP",
        }
    }
}

/// The name of an index or constraint in CREATE INDEX / CREATE CONSTRAINT /
/// DROP INDEX / DROP CONSTRAINT: either a literal identifier or a `$param`
/// reference resolved against the query's parameter map at execution time
/// (CIP-134).
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaName {
    Literal(String),
    Parameter(String),
}

/// CREATE INDEX clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexClause {
    pub name: Option<SchemaName>,
    pub entity_type: SchemaEntityType,
    /// Node labels, or (for a relationship index) relationship types, the
    /// index applies to. More than one entry only for the `A|B` disjunctive
    /// form (CIP-38/CIP-235); empty for a label-less/type-less LOOKUP index
    /// (`FOR (n)` / `FOR ()-[r]-()`).
    pub labels: Vec<String>,
    pub properties: Vec<String>,
    pub if_not_exists: bool,
    pub kind: IndexKind,
    /// True when the property list was written as `ON EACH [...]` (CIP-38
    /// FULLTEXT) rather than a plain `ON (...)` / paren-less `ON ...` list.
    pub on_each: bool,
    /// `WITH [var.prop1, var.prop2, ...]` (CIP-235): additional properties
    /// available for single-stage filtering on a VECTOR index, following
    /// the `ON` vector property. Always empty for every other index kind
    /// and for a vector index that doesn't use the clause.
    pub with_properties: Vec<String>,
    pub options: Vec<(String, Expr)>,
    /// `OPTIONS $paramName` (CIP-116 CR[4]): resolved at execution time by
    /// looking up the parameter, which must evaluate to a MAP. `None` when
    /// `options` (a literal map, possibly empty) was used instead, or no
    /// `OPTIONS` clause was given at all.
    pub options_param: Option<String>,
}

/// DROP INDEX clause.
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexClause {
    pub name: SchemaName,
    pub if_exists: bool,
    /// Legacy `DROP INDEX ON :Label(property)` target.
    pub legacy_target: Option<(String, String)>,
}

/// `SHOW [ALL|RANGE|POINT|TEXT|FULLTEXT|VECTOR|LOOKUP] INDEXES [YIELD ...]
/// [WHERE ...]` (CIP-11). Mirrors `ShowConstraintsClause`; `kind_filter`
/// is `None` for bare `SHOW INDEXES`/`SHOW ALL INDEXES` (every index kind).
#[derive(Debug, Clone, PartialEq)]
pub struct ShowIndexesClause {
    pub kind_filter: Option<IndexKind>,
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
    /// `ORDER BY`/`SKIP`/`LIMIT` written directly on the `YIELD` (before
    /// any trailing `WHERE`), same paging sub-grammar and rationale as
    /// `ShowConstraintsClause::stages`.
    pub stages: Vec<PageStage>,
}

/// CREATE CONSTRAINT clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateConstraintClause {
    pub name: Option<SchemaName>,
    pub entity_type: SchemaEntityType,
    pub label: String,
    pub variable: String,
    pub properties: Vec<String>,
    pub constraint_type: String, // "unique", "exists", "key", "propertyType"
    pub if_not_exists: bool,
    /// The parsed `<value type>` for a `REQUIRE (n.prop) IS :: <type>` /
    /// `IS TYPED <type>` property type constraint (CIP-85), `None` for
    /// every other `constraint_type`.
    pub property_type: Option<TypeExpr>,
    pub options: Vec<(String, Expr)>,
    /// The target label for a CIP-177 `REQUIRE <alias>:<Label>` label
    /// existence constraint (`constraint_type` one of `"labelExistence"`/
    /// `"sourceLabelExistence"`/`"targetLabelExistence"`), `None` for
    /// every other `constraint_type`.
    pub enforced_label: Option<String>,
}

/// DROP CONSTRAINT clause.
#[derive(Debug, Clone, PartialEq)]
pub struct DropConstraintClause {
    pub name: SchemaName,
    pub if_exists: bool,
}

/// FOREACH clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeachClause {
    pub variable: String,
    pub list_expr: Expr,
    pub clauses: Vec<Clause>,
}

/// LOAD CSV clause.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadCSVClause {
    pub with_headers: bool,
    pub source: Expr,
    pub alias: String,
    pub field_terminator: Option<String>,
}

/// CALL { ... } subquery clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CallSubqueryClause {
    pub statement: Statement,
    /// CIP-141 variable scope clause: `None` for a naked `CALL { ... }`
    /// (the deprecated legacy form, where an `IMPORTING WITH` as the
    /// subquery's own first clause is the only way to correlate outer
    /// variables in); `Some(CallScope::...)` for `CALL (...) { ... }`.
    pub scope: CallScope,
    /// CIP-159: `OPTIONAL CALL { ... }` -- when the inline subquery yields
    /// zero rows for a given outer row, that outer row is null-padded
    /// (every column the subquery would export set to `NULL`) instead of
    /// being dropped.
    pub optional: bool,
    /// CIP-32/CIP-34/CIP-198/CIP-30 trailing `IN [<concExpr>] CONCURRENT?
    /// TRANSACTIONS [OF <n> ROW(S)] [ON ERROR ...] [REPORT STATUS AS
    /// <var>]` production. `None` for a plain `CALL { ... }` subquery.
    pub in_transactions: Option<InTransactionsClause>,
}

/// The full `IN [<concExpr>] CONCURRENT? TRANSACTIONS [OF <n> ROW|ROWS] [ON
/// ERROR ...] [REPORT STATUS AS <var>]` production trailing a CALL
/// subquery. Parsed in its entirety (CIP-32 base batching, CIP-34
/// concurrency, CIP-198 retry, CIP-30 status/error) so later CIPs only need
/// to add validation/execution on top of this AST -- CIP-32 itself only
/// implements sequential batching plus the nesting/preceding-write
/// restrictions; concurrency and retry are parsed but currently execute as
/// plain sequential batching (see `Engine`'s `Clause::CallSubquery` arm).
#[derive(Debug, Clone, PartialEq)]
pub struct InTransactionsClause {
    /// `IN TRANSACTIONS` (no `CONCURRENT`) vs `IN [<expr>] CONCURRENT
    /// TRANSACTIONS` (CIP-34).
    pub concurrency: Concurrency,
    /// `OF <n> ROW|ROWS`. `None` means the batch size is
    /// implementation-chosen (batching[6]).
    pub batch_size: Option<Expr>,
    /// `ON ERROR CONTINUE|BREAK|FAIL|RETRY ...`. Defaults to `Fail` when
    /// `ON ERROR` is omitted.
    pub on_error: OnErrorMode,
    /// `REPORT STATUS AS <var>` (CIP-30).
    pub report_status: Option<String>,
}

/// CIP-34 `IN [<concExpr>] CONCURRENT? TRANSACTIONS` concurrency modifier.
#[derive(Debug, Clone, PartialEq)]
pub enum Concurrency {
    /// No `CONCURRENT` keyword -- sequential (base CIP-32 form).
    Sequential,
    /// `CONCURRENT` with no explicit value -- implementation-chosen
    /// concurrency.
    DefaultUnspecified,
    /// `<expr> CONCURRENT` -- an explicit concurrency value.
    Value(Expr),
}

/// CIP-30/CIP-198 `ON ERROR ...` mode.
#[derive(Debug, Clone, PartialEq)]
pub enum OnErrorMode {
    /// The default when `ON ERROR` is omitted entirely.
    Fail,
    Continue,
    Break,
    /// CIP-198 `RETRY [FOR]? [<dur> SEC|SECOND|SECONDS]? [THEN
    /// CONTINUE|BREAK|FAIL]?`.
    Retry {
        /// `FOR <dur> SEC[OND[S]]`; `None` means the retry duration is
        /// implementation-chosen.
        duration: Option<Expr>,
        /// The mode to fall back to once retries are exhausted. Defaults to
        /// `Fail` when no `THEN` clause is given (CIP-198's documented
        /// deviation from a naive "defaults to CONTINUE" reading).
        fallback: RetryFallback,
    },
}

/// The `THEN CONTINUE|BREAK|FAIL` fallback mode of `ON ERROR RETRY`.
#[derive(Debug, Clone, PartialEq)]
pub enum RetryFallback {
    Fail,
    Continue,
    Break,
}

/// The `(...)` variable scope clause on a `CALL` subquery (CIP-141).
#[derive(Debug, Clone, PartialEq)]
pub enum CallScope {
    /// Naked `CALL { ... }` -- no scope clause at all.
    None,
    /// `CALL (*) { ... }` -- import every variable currently in scope.
    Wildcard,
    /// `CALL (a, b) { ... }` / `CALL () { ... }` -- import exactly these
    /// variables (the list may be empty).
    Vars(Vec<String>),
}

/// `USE <graph-expr>` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct UseClause {
    pub graph_expr: Expr,
}

/// `SHOW FUNCTIONS [YIELD ...] [WHERE ...]` — the built-in function catalog
/// (CIP-245/CIP-247/CIP-263 discoverability scenarios). A minimal, dedicated
/// clause rather than a generic SHOW-command framework: only the function
/// catalog is modeled, not indexes/constraints/databases (out of this
/// batch's scope). `where_expr` is embedded here (rather than requiring a
/// standalone `WHERE` clause, which this AST has no variant for — WHERE is
/// always embedded in the clause that produces its rows, matching
/// `MatchClause`) so `SHOW FUNCTIONS YIELD name WHERE name = 'x' RETURN
/// name` parses as a single clause producing already-filtered rows.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowFunctionsClause {
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
    /// `[ALL | BUILT IN | USER DEFINED]` scope filter (CIP-84/CIP-157
    /// `SHOW BUILT IN FUNCTIONS` / `SHOW USER DEFINED FUNCTIONS`
    /// scenarios). `All` (the default) and `BuiltIn` are equivalent for
    /// CypherLite, which ships only built-in functions; `UserDefined`
    /// always yields an empty catalog (no way to register a UDF).
    pub scope: FunctionScope,
}

/// Scope filter for `SHOW [ALL | BUILT IN | USER DEFINED] FUNCTIONS`
/// (CIP-84/CIP-157). See `ShowFunctionsClause::scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FunctionScope {
    #[default]
    All,
    BuiltIn,
    UserDefined,
}

/// `SHOW [<type filter>] CONSTRAINTS [YIELD ...] [WHERE ...]` (CIP-85's
/// observation surface, needed by CIP-100 to expose a property type
/// constraint's normalized `propertyType`; the `<type filter>` grammar is
/// CIP-62/CIP-64/CIP-74's `[ALL] [NODE|REL[ATIONSHIP]] [PROPERTY]?
/// (UNIQUE[NESS]|KEY|EXIST[ENCE]|TYPE)` surface). Mirrors
/// `ShowFunctionsClause` otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowConstraintsClause {
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
    pub type_filter: ConstraintTypeFilter,
    /// `ORDER BY`/`SKIP`/`LIMIT` written directly on the `YIELD` (before
    /// any trailing `WHERE`) as part of the `YIELD`'s own paging
    /// sub-grammar (CIP-74 filtering[8]/[9]), e.g. `YIELD name ORDER BY
    /// name WHERE ... RETURN ...` -- distinct from a `SHOW ... YIELD ...`
    /// followed by a separate top-level `ORDER BY`/`RETURN ... ORDER BY`
    /// clause, which parses as its own `StandaloneOrderBy`/`Return` clause
    /// instead and never populates this field.
    pub stages: Vec<PageStage>,
}

/// The entity half of a `SHOW <type filter> CONSTRAINTS` filter -- `Any`
/// for bare `SHOW CONSTRAINTS`/`SHOW ALL CONSTRAINTS` or a kind-only
/// filter with no `NODE`/`REL[ATIONSHIP]` prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintEntityFilter {
    Any,
    Node,
    Relationship,
}

/// The constraint-kind half of a `SHOW <type filter> CONSTRAINTS` filter.
/// `None` (on `ConstraintTypeFilter::kind`) means every kind (bare `SHOW
/// CONSTRAINTS`/`SHOW ALL CONSTRAINTS`/`SHOW NODE CONSTRAINTS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKindFilter {
    /// `UNIQUE`/`UNIQUENESS`, optionally `PROPERTY`-prefixed (CIP-74) --
    /// matches `constraint_type == "unique"`.
    Unique,
    /// `KEY` -- matches `constraint_type == "key"`. Never combines with
    /// `PROPERTY` (CIP-74 SD[12]/SD[13]).
    Key,
    /// `TYPE`, optionally `PROPERTY`-prefixed -- matches `constraint_type
    /// == "propertyType"` (CIP-85/CIP-100).
    PropertyType,
    /// `EXIST`/`EXISTENCE`, optionally `PROPERTY`-prefixed -- matches
    /// `constraint_type == "exists"`.
    Existence,
}

/// `SHOW <type filter> CONSTRAINTS` -- `entity` narrows by `entityType`,
/// `kind` narrows by constraint kind. `ConstraintTypeFilter::all()` (empty
/// entity, no kind) is the default for bare `SHOW CONSTRAINTS`/`SHOW ALL
/// CONSTRAINTS`, matching every constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstraintTypeFilter {
    pub entity: ConstraintEntityFilter,
    pub kind: Option<ConstraintKindFilter>,
    /// Whether an explicit `PROPERTY` prefix was written directly before
    /// `EXIST[ENCE]` (CIP-74's `PROPERTY`-prefixed-existence spelling).
    /// Only meaningful when `kind == Some(Existence)`: CIP-187's "SHOW
    /// EXISTENCE CONSTRAINTS updates" filtering table has `PROPERTY
    /// EXIST[ENCE]` match only property existence constraints, while a
    /// bare `EXIST[ENCE]`/`NODE EXIST[ENCE]` (this `false`) additionally
    /// matches `NODE_LABEL_EXISTENCE`. `false` for every other kind (or no
    /// kind at all), where it has no effect.
    pub property_only: bool,
}

impl ConstraintTypeFilter {
    pub fn all() -> Self {
        ConstraintTypeFilter {
            entity: ConstraintEntityFilter::Any,
            kind: None,
            property_only: false,
        }
    }

    /// The `entityType` value this filter restricts to, or `None` for
    /// `ConstraintEntityFilter::Any`.
    pub fn entity_str(&self) -> Option<&'static str> {
        match self.entity {
            ConstraintEntityFilter::Any => None,
            ConstraintEntityFilter::Node => Some("NODE"),
            ConstraintEntityFilter::Relationship => Some("RELATIONSHIP"),
        }
    }
}

/// `SHOW DATABASE[S] [<filter>] [YIELD ...] [WHERE ...]` (CIP-42/CIP-68).
#[derive(Debug, Clone, PartialEq)]
pub struct ShowDatabasesClause {
    pub filter: DatabaseFilter,
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
}

/// Which database(s) a `SHOW DATABASE[S]` clause reports (CIP-68's
/// `DEFAULT`/`HOME` singular-only forms).
#[derive(Debug, Clone, PartialEq)]
pub enum DatabaseFilter {
    /// Bare `SHOW DATABASE[S]` -- every database.
    All,
    /// `SHOW DEFAULT DATABASE` -- singular only, CIP-68 rejects the plural
    /// `SHOW DEFAULT DATABASES`.
    Default,
    /// `SHOW HOME DATABASE` -- singular only, same restriction as `Default`.
    Home,
    /// `SHOW DATABASE[S] <name>` -- one named database.
    Named(SchemaName),
}

/// `SHOW [ALL|POPULATED] ROLE[S] [WITH USER[S]] [YIELD ...] [WHERE ...]`
/// (CIP-68).
#[derive(Debug, Clone, PartialEq)]
pub struct ShowRolesClause {
    pub modifier: RoleModifier,
    /// `WITH USER[S]` -- when true, the catalog carries one row per
    /// (role, member) pair (adding the `member` column) instead of one row
    /// per role.
    pub with_users: bool,
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
}

/// The `[ALL|POPULATED]` modifier of `SHOW ROLE[S]` -- `Any` (bare `SHOW
/// ROLE[S]`) and `All` (`SHOW ALL ROLE[S]`) report the identical row set
/// (every role); `Populated` narrows to roles with at least one member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleModifier {
    Any,
    All,
    Populated,
}

/// `SHOW SERVER[S] [YIELD ...] [WHERE ...]` (CIP-68).
#[derive(Debug, Clone, PartialEq)]
pub struct ShowServersClause {
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
}

/// `SHOW USER[S] [YIELD ...] [WHERE ...]` / `SHOW CURRENT USER` (CIP-68).
#[derive(Debug, Clone, PartialEq)]
pub struct ShowUsersClause {
    /// `SHOW CURRENT USER` -- singular only, CIP-68 rejects the plural
    /// `SHOW CURRENT USERS`.
    pub current_only: bool,
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
}

/// `SHOW PROCEDURE[S] [YIELD ...] [WHERE ...]` (CIP-68 grammar
/// foundation).
#[derive(Debug, Clone, PartialEq)]
pub struct ShowProceduresClause {
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
}

/// `SHOW SETTING[S] [<names>] [YIELD ...] [WHERE ...]` (CIP-68 grammar
/// foundation). `names` is the optional expression naming the setting(s) to
/// filter to (a string, a list of strings, or `null`/a parameter resolving
/// to either) -- CypherLite has no settings catalog, so it's parsed but
/// never changes the (always empty) result.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowSettingsClause {
    pub names: Option<Expr>,
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
}

/// `SHOW TRANSACTION[S] [<id-list>] [YIELD ...] [WHERE ...]` (CIP-68
/// grammar foundation). `ids` is the optional comma-separated list of
/// transaction-id expressions to filter to (empty = no filter) --
/// CypherLite has no live-transaction catalog, so it's parsed but never
/// changes the (always empty) result.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowTransactionsClause {
    pub ids: Vec<Expr>,
    pub yields: YieldList,
    pub where_expr: Option<Expr>,
}

/// `TERMINATE TRANSACTION[S] <id-list> [YIELD ...]` (CIP-68 grammar
/// foundation). Since `ShowTransactions` never reports a live transaction,
/// every requested id is reported as not found.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminateTransactionsClause {
    pub ids: Vec<Expr>,
    pub yields: YieldList,
}

/// CALL db.procedure() YIELD ... clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CallProcedureClause {
    pub procedure: String,
    pub args: Vec<Expr>,
    /// Whether the procedure name was followed by an explicit argument list,
    /// including an empty `()`. CypherLite needs this to distinguish `CALL p`
    /// (implicit arguments) from `CALL p()` (no arguments).
    pub explicit_args: bool,
    pub yields: YieldList,
    /// CIP-159: `OPTIONAL CALL proc(...)` -- when the procedure call yields
    /// zero rows (after any `WHERE` in `YIELD`) for a given outer row, that
    /// outer row is null-padded instead of being dropped. Void-procedure
    /// and zero-outer-row semantics are handled by the engine.
    pub optional: bool,
    /// CIP-159: `WHERE` inside the `YIELD` clause -- part of the
    /// `<procedure call>` production, filtering the procedure's own result
    /// rows (may reference the outer row's variables) before they're
    /// merged into the pipeline.
    pub where_expr: Option<Expr>,
}

// -- Pattern nodes --

/// A path pattern like (a)-[r]->(b), optionally named: p = (a)-[r]->(b).
#[derive(Debug, Clone, PartialEq)]
pub struct PatternPath {
    pub variable: Option<String>,
    pub elements: Vec<PatternElement>,
    /// The trailing `WHERE` of a parenthesized path pattern, e.g.
    /// `((n)-[r]->(m) WHERE n.p > m.q AND r:S)` (CIP-14 "parenthesized path
    /// pattern" location). Evaluated once every element in the path is
    /// bound, ANDed with the pattern match itself.
    pub where_expr: Option<Expr>,
    /// CIP-51 explicit path mode prefix (`WALK`/`TRAIL`/`ACYCLIC`/`SIMPLE`),
    /// written directly or nested inside a `selector`'s own `[path mode]`
    /// slot. `None` means no explicit keyword (Rule 1: resolves to `Walk`).
    pub mode: Option<PathMode>,
    /// CIP-73/CIP-191 path selector prefix (`ALL`/`ANY`/`SHORTEST`/...).
    /// `None` means no explicit selector (equivalent to an implicit `ALL`).
    pub selector: Option<PathSelector>,
    /// True if this path pattern is sugar for the pre-GPM
    /// `shortestPath(...)`/`allShortestPaths(...)` function (CIP-40/CIP-73
    /// Section 3.3), rather than an explicit `SHORTEST`/`ALL SHORTEST`
    /// selector keyword -- even though both normalize to the same
    /// `selector` value. The two are NOT interchangeable for CIP-40's
    /// mixing rules: the legacy function forms are exempt from the
    /// explicit-match-mode and multiple-selective-path-pattern
    /// restrictions that apply to the keyword selector syntax (see
    /// `Parser::parse_match`).
    pub is_shortest_path_function: bool,
}

/// A pattern element is either a node pattern, a relationship pattern, or a
/// quantified path pattern (CIP-28).
#[derive(Debug, Clone, PartialEq)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelPattern),
    Quantified(QuantifiedPathPattern),
}

/// The `{min, max}` bounds of a CIP-28 quantifier (`*`, `+`, `{m,n}`, `{m,}`,
/// `{,n}`, `{,}`, `{n}`). `max = None` means unbounded. Bounds are already
/// normalized and validated by the parser: Rule 1 (`{,}` -> `{0,}`) and
/// Rule 2 (upper bound > 0, lower bound <= upper bound) are enforced at
/// parse time, so every `PathQuantifier` that reaches the AST is legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathQuantifier {
    pub min: u32,
    pub max: Option<u32>,
}

/// A quantified path pattern (CIP-28 Section 3.1): a parenthesized subpath
/// (`elements`), optionally assigned a subpath variable (CIP-28 Section 3.3,
/// e.g. `(p2 = (x)-->(y))*`) and/or a trailing `WHERE` evaluated once per
/// iteration, followed by a quantifier. Appears as a `PatternElement`
/// alongside plain `Node`/`Relationship` elements so it composes freely with
/// them by concatenation (CIP-28 Section 3.1.7).
///
/// A bare quantified relationship (`-[r]->*`, Form 5) does *not* produce this
/// variant: it normalizes directly into the ordinary variable-length
/// `RelPattern` (`min_hops`/`max_hops`), since it declares no element
/// variables of its own to expose as a group and its matching semantics are
/// identical to the legacy `-[*min..max]->` matcher (only the quantifier's
/// own zero-inclusive defaults differ, e.g. bare `*` means 0 or more here
/// versus 1 or more for legacy bare `*`). See `Parser::parse_rel_pattern`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantifiedPathPattern {
    pub subpath_variable: Option<String>,
    pub elements: Vec<PatternElement>,
    pub where_expr: Option<Expr>,
    pub quantifier: PathQuantifier,
}

/// A node pattern like (n:Label {prop: value}).
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub label_expr: Option<LabelExpr>,
    pub properties: Vec<(String, Expr)>,
    pub has_properties: bool,
    /// The `$paramName` whole-property-map shorthand (an alternative to
    /// `{...}`), e.g. `CREATE (n:Label $props)`. Mutually exclusive with
    /// `properties`/`has_properties` -- the parser only ever populates one
    /// property-map form per pattern element.
    pub properties_param: Option<Expr>,
    /// The element-pattern `WHERE` of `(n WHERE <expr>)` (CIP-14 "node
    /// element pattern" location), evaluated once `variable` is bound.
    pub where_expr: Option<Expr>,
}

/// A relationship pattern like -[r:TYPE {prop: value}]->.
#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub variable: Option<String>,
    pub type_expr: Option<LabelExpr>,
    pub properties: Vec<(String, Expr)>,
    /// The `$paramName` whole-property-map shorthand -- see
    /// `NodePattern::properties_param`.
    pub properties_param: Option<Expr>,
    pub direction: Direction,
    pub min_hops: Option<u32>,
    pub max_hops: Option<u32>,
    /// The element-pattern `WHERE` of `[r WHERE <expr>]`.
    pub where_expr: Option<Expr>,
    /// True when `min_hops`/`max_hops` came from a CIP-28 postfix quantifier
    /// (`-[r]->*`, `-->+`, `-[]->{1,6}`, Form 5) rather than the legacy
    /// bracket-internal `[r*min..max]` syntax. Distinguishes the two so the
    /// engine can apply `where_expr` per traversed edge for the CIP-28 form
    /// without changing the legacy matcher's (TCK-tested) behavior.
    pub quantified: bool,
}

/// A GQL label/relationship-type expression (CIP-14): `&` (conjunction),
/// `|` (disjunction), `!` (negation), `%` (wildcard -- matches any non-empty
/// label set, or any relationship since every relationship has exactly one
/// type), parenthesized grouping (folded directly into the tree, so no
/// explicit `Group` variant is needed), and CIP-249's dynamic label name
/// `$(expr)`. The legacy colon-conjunction shorthand `:A:B:C` lowers to
/// `And([Label(A), Label(B), Label(C)])`, and the legacy relationship-type
/// disjunction shorthand `:A|B` lowers to `Or([Label(A), Label(B)])`,
/// preserving pre-CIP-14 behavior exactly.
#[derive(Debug, Clone, PartialEq)]
pub enum LabelExpr {
    /// A single label/type name.
    Label(String),
    /// `%`
    Wildcard,
    /// `$(expr)` / `$all(expr)` / `$any(expr)` -- a label/type name (or, for
    /// list-valued `expr`, a set of label/type names) resolved at runtime
    /// (CIP-249 / CIP-156 / CIP-221).
    Dynamic(DynamicLabelExpr),
    /// `!<label-expr>` -- negates only the following primary.
    Not(Box<LabelExpr>),
    /// `<label-expr> & <label-expr> & ...`
    And(Vec<LabelExpr>),
    /// `<label-expr> | <label-expr> | ...`
    Or(Vec<LabelExpr>),
}

/// A dynamic label/type expression's quantifier: `$(expr)` (`Default`),
/// `$all(expr)` (`All`), or `$any(expr)` (`Any`). `Default` and `All` share
/// conjunction semantics for list-valued `expr` (and are otherwise
/// interchangeable); `Any` is disjunction. Kept distinct from `All` so
/// callers (e.g. `CREATE`/`MERGE` label resolution) can reject the explicit
/// `$any(...)` form while still accepting the implicit-all `$(...)` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicLabelQuantifier {
    Default,
    All,
    Any,
}

/// The payload of `LabelExpr::Dynamic`: the value expression plus its
/// quantifier.
#[derive(Debug, Clone, PartialEq)]
pub struct DynamicLabelExpr {
    pub expr: Box<Expr>,
    pub quantifier: DynamicLabelQuantifier,
}

impl LabelExpr {
    /// Returns the flat list of label names if this expression is a plain
    /// label list: a single label, or an `And` of only plain labels (as
    /// produced by legacy colon-conjunction `:A:B:C`, or a comma-separated
    /// SET/REMOVE label item). Returns `None` for anything using `|`, `!`,
    /// `%`, or a dynamic label -- those aren't valid label lists (CREATE /
    /// MERGE / SET / REMOVE label specifications, per CIP-14's "DML
    /// statements" exclusion and CIP-126).
    pub fn as_label_list(&self) -> Option<Vec<String>> {
        match self {
            LabelExpr::Label(name) => Some(vec![name.clone()]),
            LabelExpr::And(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        LabelExpr::Label(name) => out.push(name.clone()),
                        _ => return None,
                    }
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// True if this expression contains `!` (negation) or `%` (wildcard)
    /// anywhere, recursively. Used to reject a variable-length quantifier
    /// combined with negation/wildcard (CIP-14).
    pub fn contains_not_or_wildcard(&self) -> bool {
        match self {
            LabelExpr::Label(_) | LabelExpr::Dynamic(_) => false,
            LabelExpr::Wildcard | LabelExpr::Not(_) => true,
            LabelExpr::And(items) | LabelExpr::Or(items) => {
                items.iter().any(LabelExpr::contains_not_or_wildcard)
            }
        }
    }

    /// Returns the flat list of label/type names if this expression is a
    /// plain label or `|`-disjunction list: a single label, or an `Or` of
    /// only plain labels (as produced by `A|B|C`). Returns `None` for
    /// anything using `&`, `!`, `%`, or a dynamic label. Used by the
    /// CREATE INDEX / CREATE CONSTRAINT `FOR` target (CIP-11/CIP-38/
    /// CIP-235's `FOR (n:A|B)` multi-label DDL surface) -- a narrower
    /// grammar than general MATCH label expressions.
    pub fn as_ddl_list(&self) -> Option<Vec<String>> {
        match self {
            LabelExpr::Label(name) => Some(vec![name.clone()]),
            LabelExpr::Or(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        LabelExpr::Label(name) => out.push(name.clone()),
                        _ => return None,
                    }
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// True if this expression contains a `$(...)`/`$all(...)`/`$any(...)`
    /// dynamic label/type atom anywhere, recursively (CIP-156). Used by
    /// CREATE/MERGE's static "exactly one relationship type" check to defer
    /// to runtime resolution when the count can't be known until the
    /// dynamic expression is evaluated.
    pub fn contains_dynamic(&self) -> bool {
        match self {
            LabelExpr::Label(_) | LabelExpr::Wildcard => false,
            LabelExpr::Dynamic(_) => true,
            LabelExpr::Not(inner) => inner.contains_dynamic(),
            LabelExpr::And(items) | LabelExpr::Or(items) => {
                items.iter().any(LabelExpr::contains_dynamic)
            }
        }
    }
}

/// Direction of a relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Right, // (a)-[]->(b)
    Left,  // (a)<-[]-(b)
    Both,  // (a)-[]-(b)
}

// -- Expression nodes --

/// One segment of a CIP-261 interpolated string literal.
#[derive(Debug, Clone, PartialEq)]
pub enum InterpPart {
    /// Literal text (escaped braces already resolved to plain braces).
    Text(String),
    /// An `{expression}` embed.
    Interp(Box<Expr>),
}

/// All expression types.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(LiteralValue),
    List(Vec<Expr>),
    Map(Vec<MapPair>),
    /// CIP-261 interpolated string literal: text segments interleaved with
    /// `{expression}` embeds, lowered to the internal `string.interp`
    /// function (see `cypherlite/cypherlite-front/src/lower.rs`).
    StringInterp(Vec<InterpPart>),
    Variable(String),
    Parameter(String),
    Property(Box<PropertyAccess>),
    FunctionCall(FunctionCallExpr),
    Binary(Box<BinaryExpr>),
    Unary(Box<UnaryExpr>),
    Comparison(Box<ComparisonExpr>),
    In(Box<InExpr>),
    IsNull(Box<IsNullExpr>),
    IsType(Box<IsTypeExpr>),
    IsNormalized(Box<IsNormalizedExpr>),
    StringMatch(Box<StringMatchExpr>),
    CountStar,
    Star,
    Case(Box<CaseExpr>),
    ExistsSubquery(Box<ExistsSubqueryExpr>),
    CountSubquery(Box<CountSubqueryExpr>),
    CollectSubquery(Box<CollectSubqueryExpr>),
    ListComprehension(Box<ListComprehensionExpr>),
    MapComprehension(Box<MapComprehensionExpr>),
    PatternComprehension(Box<PatternComprehensionExpr>),
    Reduce(Box<ReduceExpr>),
    AllReduce(Box<AllReduceExpr>),
    ShortestPath(Box<ShortestPathExpr>),
    Subscript(Box<SubscriptExpr>),
    Quantifier(Box<QuantifierExpr>),
    HasLabel(Box<HasLabelExpr>),
    Paren(Box<Expr>),
    PatternPredicate(Box<PatternPredicateExpr>),
    MapProjection(Box<MapProjectionExpr>),
}

/// A literal value in the AST.
#[derive(Debug, Clone, PartialEq)]
pub enum LiteralValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
}

/// A key-value pair in a map literal.
#[derive(Debug, Clone, PartialEq)]
pub struct MapPair {
    pub key: String,
    pub value: Expr,
}

/// `<variable> <map projection>` (`n {.foo, bar, baz: qux}`): openCypher's
/// map projection, syntactically always rooted at a plain variable
/// (`expr` is always an `Expr::Variable`). Needed for CIP-190's
/// near-ambiguity coverage: `exists {name: exists.name}` etc. must parse
/// as a map projection on a variable named `exists`, not as
/// `EXISTS { <subquery> }`, when the filler between the braces looks like
/// projection items rather than subquery content (near-ambiguities.feature).
#[derive(Debug, Clone, PartialEq)]
pub struct MapProjectionExpr {
    pub expr: Expr,
    pub items: Vec<MapProjectionItem>,
}

/// One item of a `<map projection>`.
#[derive(Debug, Clone, PartialEq)]
pub enum MapProjectionItem {
    /// `.*` -- every property of the base entity.
    AllProperties,
    /// `.prop` -- shorthand for `prop: <base>.prop`.
    Property(String),
    /// A bare `variable` selector -- shorthand for `variable: variable`,
    /// pulling a same-named variable from the surrounding scope (*not* a
    /// property of the base expression).
    Variable(String),
    /// `key: <expr>` -- a literal entry.
    KeyValue(String, Expr),
}

/// Property access like n.prop.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyAccess {
    pub expr: Expr,
    pub property: String,
    /// CIP-131: when a SET/REMOVE target uses the dynamic-property bracket
    /// form (`n[<expr>]`) instead of the static `n.prop` form, the property
    /// key is computed at runtime from this expression rather than being the
    /// statically-known `property` string (which is left empty). `None` for
    /// every static `.prop` access, keeping `property` back-compatible for
    /// all existing (non-SET/REMOVE) uses of `PropertyAccess`.
    pub key_expr: Option<Expr>,
}

/// Function call like count(x).
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionCallExpr {
    pub name: String,
    pub args: Vec<Expr>,
    pub distinct: bool,
}

/// Binary expression like a + b.
#[derive(Debug, Clone, PartialEq)]
pub struct BinaryExpr {
    pub left: Expr,
    pub op: String,
    pub right: Expr,
}

/// Unary expression like NOT x or -x.
#[derive(Debug, Clone, PartialEq)]
pub struct UnaryExpr {
    pub op: String,
    pub expr: Expr,
}

/// Comparison expression like a > b.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonExpr {
    pub left: Expr,
    pub op: String,
    pub right: Expr,
}

/// x IN list.
#[derive(Debug, Clone, PartialEq)]
pub struct InExpr {
    pub expr: Expr,
    pub list: Expr,
}

/// x IS NULL or x IS NOT NULL.
#[derive(Debug, Clone, PartialEq)]
pub struct IsNullExpr {
    pub expr: Expr,
    pub negated: bool,
}

/// x IS [NOT] :: <value type> (CIP-86/CIP-87 type predicate expression);
/// also produced by the `IS [NOT] TYPED <value type>` and bare `x :: <value
/// type>` surface forms, which are equivalent.
#[derive(Debug, Clone, PartialEq)]
pub struct IsTypeExpr {
    pub expr: Expr,
    pub type_expr: TypeExpr,
    pub negated: bool,
}

/// A parsed `<value type>` per the CIP-86 grammar: the vocabulary used by
/// the CIP-87 type predicate expression (`IS [NOT] :: <type>`) and, once
/// CIP-90 ships, by `valueType()`.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    /// A predefined leaf type: a scalar, temporal, reference, point, map,
    /// nothing/null, or the open `ANY` type. `not_null` reflects an
    /// explicit `NOT NULL` suffix directly on this leaf's own grammar
    /// production (e.g. `INTEGER NOT NULL`).
    Predefined {
        kind: PredefinedKind,
        not_null: bool,
    },
    /// `LIST<T>` / `T LIST` / `ARRAY<T>` / `T ARRAY`, with an optional
    /// `NOT NULL` on the list value itself (not on its elements -- element
    /// nullability lives in `element`).
    List {
        element: Box<TypeExpr>,
        not_null: bool,
    },
    /// `[ANY] PROPERTY VALUE [NOT NULL]`: the dynamic union of every
    /// property-storable scalar type and lists thereof.
    PropertyValue { not_null: bool },
    /// A closed dynamic union -- `ANY [VALUE] <A | B | ...>` or the bare
    /// `A | B | ...` form (equivalent). The grammar gives closed unions no
    /// `[NOT NULL]` production of their own, so the parser never attaches
    /// one here; a trailing `NOT NULL` after a union is a syntax error.
    Union(Vec<TypeExpr>),
    /// `VECTOR<TYPE [NOT NULL]>(DIM)` (Cypher style) or `VECTOR(DIM, TYPE
    /// [NOT NULL])` (GQL style) (CIP-216). Either the coordinate type or
    /// the dimension may be grammatically omitted (`VECTOR(3)`,
    /// `VECTOR<INTEGER32>`) -- both parse fine but are rejected at
    /// constraint-creation time, since a property type constraint requires
    /// both. The coordinate type's own `NOT NULL` is mandatory/implicit
    /// (always rendered NOT NULL in canonical form) and so isn't tracked
    /// separately here; `not_null` is the *outer* NOT NULL on the whole
    /// vector type (valid grammatically, but rejected for independent
    /// property type constraints -- CIP-216 doc, "for dependent property
    /// type constraints only").
    Vector {
        coord: Option<VectorCoordKind>,
        dimension: Option<i64>,
        not_null: bool,
    },
}

impl TypeExpr {
    /// The `NOT NULL` flag attached directly to this node's own grammar
    /// production. Used only to check that a closed union's components all
    /// agree on nullability (CIP-86 "Not Null Uses"); closed unions have no
    /// such production of their own, so they report `false`.
    pub fn direct_not_null(&self) -> bool {
        match self {
            TypeExpr::Predefined { not_null, .. } => *not_null,
            TypeExpr::List { not_null, .. } => *not_null,
            TypeExpr::PropertyValue { not_null } => *not_null,
            TypeExpr::Vector { not_null, .. } => *not_null,
            TypeExpr::Union(_) => false,
        }
    }
}

/// A vector coordinate type (CIP-216): the six numeric types valid inside
/// `VECTOR<...>`/`VECTOR(...)`. `Int64`/`Float64` double as ordinary plain
/// property types (aliases for `INTEGER`/`FLOAT`, see `PredefinedKind`)
/// outside a vector type; the other four (`Float32`, `Int8`/`Int16`/
/// `Int32`) are valid only as a vector coordinate type.
///
/// Defined in `cypherlite-model` (so the runtime `CypherValue::Vector`
/// variant can carry it without a circular crate dependency) and
/// re-exported here under its original name/location.
pub use cypherlite_model::VectorCoordKind;

/// The predefined leaf type names from CIP-86's `<value type>` grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredefinedKind {
    Nothing,
    Null,
    Any,
    Boolean,
    String,
    Integer,
    Float,
    Date,
    Duration,
    Point,
    LocalTime,
    ZonedTime,
    LocalDateTime,
    ZonedDateTime,
    Node,
    Relationship,
    Path,
    Map,
    /// CIP-246 `UUID [NOT NULL]`.
    Uuid,
}

/// x IS [NOT] [<normal form>] NORMALIZED (CIP-133). `form` is one of
/// "NFC"/"NFD"/"NFKC"/"NFKD", or None when the form is omitted (defaults to
/// NFC at evaluation time).
#[derive(Debug, Clone, PartialEq)]
pub struct IsNormalizedExpr {
    pub expr: Expr,
    pub form: Option<String>,
    pub negated: bool,
}

/// STARTS WITH, ENDS WITH, CONTAINS.
#[derive(Debug, Clone, PartialEq)]
pub struct StringMatchExpr {
    pub left: Expr,
    pub op: String,
    pub right: Expr,
}

/// CASE expression.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseExpr {
    pub test: Option<Expr>,
    pub whens: Vec<WhenClause>,
    pub else_expr: Option<Expr>,
}

/// WHEN ... THEN ... part of a CASE.
#[derive(Debug, Clone, PartialEq)]
pub struct WhenClause {
    pub condition: Expr,
    pub result: Expr,
    /// True when this is a simple-CASE `WHEN IS [NOT] LABELED <LE>` branch
    /// (CIP-249): `condition` is a `HasLabelExpr` evaluated directly as a
    /// boolean predicate against the CASE operand, rather than being
    /// compared for equality with it like an ordinary simple-CASE value.
    pub is_predicate: bool,
}

/// EXISTS { ... } subquery expression.
#[derive(Debug, Clone, PartialEq)]
pub struct ExistsSubqueryExpr {
    pub statement: Statement,
}

/// COUNT { ... } subquery expression: evaluates to the number of rows the
/// subquery produces against the current row, run independently per row
/// (like `EXISTS { ... }`, just returning a count instead of a boolean).
#[derive(Debug, Clone, PartialEq)]
pub struct CountSubqueryExpr {
    pub statement: Statement,
}

/// `COLLECT { <RegularQuery> }` subquery expression (CIP-71): evaluates the
/// inner `<RegularQuery>` once per outer row and collects the single
/// projected column's value from every inner row into a list. Unlike
/// `EXISTS`/`COUNT`, `COLLECT` has no bare-pattern shorthand -- the inner
/// query is always a full `<RegularQuery>` (any read clause, including
/// UNION) and must end in a `RETURN` with exactly one non-`*` return item.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectSubqueryExpr {
    pub statement: Statement,
}

/// CIP-272 map comprehension: `{k : v IN map [WHERE p] | keyExpr : valueExpr}`
/// (map source) or `{item IN list [WHERE p] | keyExpr : valueExpr}` (list
/// source).
#[derive(Debug, Clone, PartialEq)]
pub struct MapComprehensionExpr {
    /// The key binding variable of a map source (`None` for a list source).
    pub key_var: Option<String>,
    /// The value binding variable (map source) or the item variable (list
    /// source).
    pub value_var: String,
    pub source_expr: Expr,
    pub where_expr: Option<Expr>,
    pub key_expr: Expr,
    pub value_expr: Expr,
}

/// [x IN list WHERE pred | expr]
#[derive(Debug, Clone, PartialEq)]
pub struct ListComprehensionExpr {
    pub variable: String,
    pub list_expr: Expr,
    pub where_expr: Option<Expr>,
    pub map_expr: Option<Expr>,
}

/// [p = (n)-->() | expr] or [(n)-->() | expr]
#[derive(Debug, Clone, PartialEq)]
pub struct PatternComprehensionExpr {
    pub path_variable: Option<String>,
    pub pattern: PatternPath,
    pub where_expr: Option<Expr>,
    pub map_expr: Expr,
}

/// reduce(accumulator = init, x IN list | expression)
#[derive(Debug, Clone, PartialEq)]
pub struct ReduceExpr {
    pub accumulator: String,
    pub init_expr: Expr,
    pub variable: String,
    pub list_expr: Expr,
    pub expression: Expr,
}

/// allReduce(accumulator = init, x IN list | expression, predicate)
#[derive(Debug, Clone, PartialEq)]
pub struct AllReduceExpr {
    pub accumulator: String,
    pub init_expr: Expr,
    pub variable: String,
    pub list_expr: Expr,
    pub expression: Expr,
    pub predicate: Expr,
}

/// shortestPath() / allShortestPaths()
#[derive(Debug, Clone, PartialEq)]
pub struct ShortestPathExpr {
    pub pattern: PatternPath,
    pub all: bool,
}

/// expr[index] or expr[from..to]
#[derive(Debug, Clone, PartialEq)]
pub struct SubscriptExpr {
    pub expr: Expr,
    pub index: Option<Expr>,
    pub from: Option<Expr>,
    pub to: Option<Expr>,
    pub slice: bool,
}

/// all/any/none/single(x IN list WHERE pred)
#[derive(Debug, Clone, PartialEq)]
pub struct QuantifierExpr {
    pub quantifier: String,
    pub variable: String,
    pub list: Expr,
    pub predicate: Option<Expr>,
}

/// A label/type predicate: `n:Label`, `n IS Label` (CIP-14), or
/// `n IS [NOT] LABELED <label-expr>` / the `n IS NOT <label-expr>` shorthand
/// (CIP-249).
#[derive(Debug, Clone, PartialEq)]
pub struct HasLabelExpr {
    pub expr: Expr,
    pub label_expr: LabelExpr,
    /// True for `IS NOT LABELED <LE>` and its `IS NOT <LE>` shorthand, which
    /// negate the *whole* expression (unlike an inline `!` inside `label_expr`,
    /// which negates only the following primary).
    pub negated: bool,
    /// True for the CIP-249 `IS [NOT] LABELED <LE>` forms (and the `IS NOT
    /// <LE>` shorthand), which raise a runtime type error for a non-node/
    /// relationship LHS. False for the pre-existing `:`/bare `IS <LE>`
    /// forms (CIP-14), which fall back to `false` for a non-entity LHS,
    /// preserving their original behavior.
    pub strict: bool,
}

/// Pattern used as boolean predicate in WHERE, e.g. (n)-[:REL]->(m)
#[derive(Debug, Clone, PartialEq)]
pub struct PatternPredicateExpr {
    pub pattern: PatternPath,
}
