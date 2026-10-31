use crate::unicode_identifier::{is_identifier_part, is_identifier_start};

/// Token types for the Cypher lexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenType {
    // Special tokens
    EOF,
    Error,
    Identifier,
    Integer,
    Float,
    String,
    /// CIP-261 interpolated string literal `s'...'` / `s"..."`. Payload is
    /// the body between the quotes with standard escapes resolved; `\{` and
    /// `\}` stay as two characters so the parser can tell an escaped literal
    /// brace from an expression delimiter, and raw `{`/`}` are kept for the
    /// parser to split into text/expression parts.
    InterpString,
    Parameter, // $param

    // Punctuation
    LeftParen,    // (
    RightParen,   // )
    LeftBracket,  // [
    RightBracket, // ]
    LeftBrace,    // {
    RightBrace,   // }
    Colon,        // :
    DoubleColon,  // ::
    Comma,        // ,
    Dot,          // .
    Semicolon,    // ;
    Pipe,         // |
    DoublePipe,   // ||
    Dash,         // -
    ArrowRight,   // ->
    ArrowLeft,    // <-
    FatArrow,     // => (CIP-176/CIP-187 graph type declaration syntax)
    Star,         // *
    Plus,         // +
    Slash,        // /
    Percent,      // %
    Caret,        // ^
    DotDot,       // ..
    Bang,         // ! (CIP-102 NOT NULL type shorthand; also GQL label negation, CIP-14)
    Ampersand,    // & (GQL label expression conjunction, CIP-14)

    // Comparison operators
    Equals,       // =
    NotEquals,    // <>
    LessThan,     // <
    GreaterThan,  // >
    LessEqual,    // <=
    GreaterEqual, // >=
    RegexMatch,   // =~

    // Keywords
    MATCH,
    OPTIONAL,
    CREATE,
    INSERT,
    MERGE,
    DELETE,
    DETACH,
    SET,
    REMOVE,
    RETURN,
    WITH,
    UNWIND,
    WHERE,
    ORDER,
    BY,
    ASC,
    DESC,
    LIMIT,
    SKIP,
    AS,
    DISTINCT,
    AND,
    OR,
    NOT,
    XOR,
    IN,
    STARTS,
    ENDS,
    CONTAINS,
    IS,
    NULL,
    TRUE,
    FALSE,
    CASE,
    WHEN,
    THEN,
    ELSE,
    END,
    FOREACH,
    CALL,
    YIELD,
    UNION,
    ALL,
    ON,
    INDEX,
    CONSTRAINT,
    FOR,
    REQUIRE,
    UNIQUE,
    EXISTS,
    IF,
    DROP,
    VECTOR,
    POINT,
    OPTIONS,
    NODE,
    REL,
    COUNT,
    COLLECT,
    LOAD,
    FROM,
    CSV,
    REDUCE,
    SPATIAL,
    EXPLAIN,
    PROFILE,
    INFINITY, // Infinity/Inf literal (CIP-55)
    NAN,      // NaN literal (CIP-55)
}

impl TokenType {
    /// Returns true if this token type is a keyword (MATCH..PROFILE range).
    pub fn is_keyword(self) -> bool {
        matches!(
            self,
            TokenType::MATCH
                | TokenType::OPTIONAL
                | TokenType::CREATE
                | TokenType::INSERT
                | TokenType::MERGE
                | TokenType::DELETE
                | TokenType::DETACH
                | TokenType::SET
                | TokenType::REMOVE
                | TokenType::RETURN
                | TokenType::WITH
                | TokenType::UNWIND
                | TokenType::WHERE
                | TokenType::ORDER
                | TokenType::BY
                | TokenType::ASC
                | TokenType::DESC
                | TokenType::LIMIT
                | TokenType::SKIP
                | TokenType::AS
                | TokenType::DISTINCT
                | TokenType::AND
                | TokenType::OR
                | TokenType::NOT
                | TokenType::XOR
                | TokenType::IN
                | TokenType::STARTS
                | TokenType::ENDS
                | TokenType::CONTAINS
                | TokenType::IS
                | TokenType::NULL
                | TokenType::TRUE
                | TokenType::FALSE
                | TokenType::CASE
                | TokenType::WHEN
                | TokenType::THEN
                | TokenType::ELSE
                | TokenType::END
                | TokenType::FOREACH
                | TokenType::CALL
                | TokenType::YIELD
                | TokenType::UNION
                | TokenType::ALL
                | TokenType::ON
                | TokenType::INDEX
                | TokenType::CONSTRAINT
                | TokenType::FOR
                | TokenType::REQUIRE
                | TokenType::UNIQUE
                | TokenType::EXISTS
                | TokenType::IF
                | TokenType::DROP
                | TokenType::VECTOR
                | TokenType::POINT
                | TokenType::OPTIONS
                | TokenType::NODE
                | TokenType::REL
                | TokenType::COUNT
                | TokenType::COLLECT
                | TokenType::LOAD
                | TokenType::FROM
                | TokenType::CSV
                | TokenType::REDUCE
                | TokenType::SPATIAL
                | TokenType::EXPLAIN
                | TokenType::PROFILE
                | TokenType::INFINITY
                | TokenType::NAN
        )
    }

    /// Returns true if the token can be used as an identifier (variable,
    /// alias, map key, ...): `Identifier` or any keyword except the
    /// [`CLAUSE_KEYWORDS`].
    pub fn is_ident_like(self) -> bool {
        self.is_schema_name() && !CLAUSE_KEYWORDS.contains(&self)
    }

    /// Returns true if the token is `Identifier` or any keyword: openCypher's
    /// `SchemaName ::= SymbolicName | ReservedWord`, for positions where the
    /// grammar already requires a name, so no keyword there starts a clause
    /// -- property keys after `.`, the LET binding variable, and label and
    /// relationship-type names. ADR-0169: a strict superset of
    /// `is_ident_like()`, so `(o:Order)` and `-[:ORDER]->` parse unquoted
    /// with the name kept as written.
    ///
    /// This token class is unrelated to, and deliberately wider than, the
    /// narrower thing this crate also calls a schema name: the
    /// index/constraint name `ast::SchemaName` that
    /// `parse_optional_schema_name` / `parse_required_schema_name` read
    /// (`Identifier`, `INDEX`, `CONSTRAINT` or a `$param` only).
    pub fn is_schema_name(self) -> bool {
        self == TokenType::Identifier || self.is_keyword()
    }
}

/// The clause keywords: keyword tokens that start or continue a clause
/// (`MATCH`, `ORDER BY`, `AS`, `DISTINCT`, ...). `is_ident_like()` rejects
/// them as variables, aliases and map keys, so `(WHERE ...)` does not read
/// `WHERE` as the element variable. ADR-0169: they are still label and
/// relationship-type names (`is_schema_name()`).
///
/// `RETURN` is not one of them. CIP-150 NOTES.md Q2 (`@neo4j-verified`
/// against live Neo4j 2026.05, both CYPHER 5 and CYPHER 25): `RETURN` is a
/// non-reserved word usable as an ordinary variable/alias name (e.g.
/// `WITH 1 AS RETURN RETURN RETURN AS r`) -- `is_ident_like()` is only
/// checked in positions the grammar already knows unambiguously expect an
/// identifier (after AS, as a pattern/FOREACH variable, ...), so it never
/// shadows RETURN's role starting a new clause.
pub const CLAUSE_KEYWORDS: [TokenType; 22] = [
    TokenType::MATCH,
    TokenType::OPTIONAL,
    TokenType::CREATE,
    TokenType::INSERT,
    TokenType::MERGE,
    TokenType::DELETE,
    TokenType::DETACH,
    TokenType::SET,
    TokenType::REMOVE,
    TokenType::WITH,
    TokenType::UNWIND,
    TokenType::WHERE,
    TokenType::ORDER,
    TokenType::BY,
    TokenType::LIMIT,
    TokenType::SKIP,
    TokenType::AS,
    TokenType::DISTINCT,
    TokenType::FOREACH,
    TokenType::UNION,
    TokenType::REQUIRE,
    TokenType::DROP,
];

/// The contextual (non-reserved, identifier-spelled) clause keywords:
/// words that start or continue a clause when they appear as a plain
/// `Identifier` at clause position, recognized by spelling -- the same
/// role as [`CLAUSE_KEYWORDS`], but these stay usable as ordinary
/// variable/label names everywhere else (`MATCH (show) RETURN show`).
/// Single-sourced here so the clause dispatcher
/// (`Parser::parse_clause_inner`) and the `SHOW SETTINGS`-style
/// filter-expression starter (`Parser::peek_starts_show_filter_expr`,
/// which excludes exactly this set so a following composed clause is not
/// swallowed as a filter expression) can never drift apart.
///
/// `NEXT` (CIP-213) and `CYPHER` (preamble) are members even though they
/// never start a clause inside `parse_clause_inner` -- they are
/// recognized at statement-block boundaries and in the pre-parser
/// preamble respectively. `START`/`STOP`/`NODETACH`/`OFFSET` are
/// deliberately *not* members: they only act clause-like before a
/// specific following word (`DATABASE`/`DELETE`/paging), so a bare
/// `SHOW SETTINGS start` still reads `start` as a name filter.
/// The reserved-keyword clause-starter tokens: the keyword tokens that
/// start a new clause at clause position (`MATCH`, `CREATE`, `UNWIND`,
/// `FOR` (CIP-250), ...). The grammar authority is the clause dispatch
/// (`Parser::parse_clause_inner`); this table is the derived vocabulary
/// every *lookahead* over the same fact consults instead of re-spelling a
/// second hand-maintained list -- today `Parser::group_by_ends_clause`
/// (whose round-5 comment records the LOAD/DROP drift that motivated it)
/// and the dispatch-coverage pin test. A new clause-start dispatch arm
/// extends this one table. `OPTIONAL` is a member: it starts a clause
/// only immediately before `MATCH`/`CALL` (the arm that checks what
/// follows). The contextual (identifier-spelled) starters are the
/// separate [`CONTEXTUAL_CLAUSE_WORDS`] below.
pub const CLAUSE_STARTER_TOKENS: &[TokenType] = &[
    TokenType::MATCH,
    TokenType::OPTIONAL,
    TokenType::CREATE,
    TokenType::INSERT,
    TokenType::MERGE,
    TokenType::DELETE,
    TokenType::DETACH,
    TokenType::SET,
    TokenType::REMOVE,
    TokenType::UNWIND,
    // CIP-250 (GQ10): `FOR <variable> IN <expression>`.
    TokenType::FOR,
    TokenType::FOREACH,
    TokenType::CALL,
    TokenType::UNION,
    TokenType::LOAD,
    TokenType::DROP,
];

/// The contextual (non-reserved, identifier-spelled) clause keywords:
/// words that start or continue a clause when they appear as a plain
/// `Identifier` at clause position, recognized by spelling -- the same
/// role as [`CLAUSE_KEYWORDS`], but these stay usable as ordinary
/// variable/label names everywhere else (`MATCH (show) RETURN show`).
/// Single-sourced here so the clause dispatcher
/// (`Parser::parse_clause_inner`) and the `SHOW SETTINGS`-style
/// filter-expression starter (`Parser::peek_starts_show_filter_expr`,
/// which excludes exactly this set so a following composed clause is not
/// swallowed as a filter expression) can never drift apart.
///
/// `NEXT` (CIP-213) and `CYPHER` (preamble) are members even though they
/// never start a clause inside `parse_clause_inner` -- they are
/// recognized at statement-block boundaries and in the pre-parser
/// preamble respectively. `START`/`STOP`/`NODETACH`/`OFFSET` are
/// deliberately *not* members: they only act clause-like before a
/// specific following word (`DATABASE`/`DELETE`/paging), so a bare
/// `SHOW SETTINGS start` still reads `start` as a name filter.
pub const CONTEXTUAL_CLAUSE_WORDS: [&str; 9] = [
    "alter",
    "cypher",
    "filter",
    "finish",
    "let",
    "next",
    "show",
    "terminate",
    "use",
];

/// True if `literal` (any case) spells one of the
/// [`CONTEXTUAL_CLAUSE_WORDS`].
pub fn is_contextual_clause_keyword(literal: &str) -> bool {
    CONTEXTUAL_CLAUSE_WORDS
        .iter()
        .any(|w| literal.eq_ignore_ascii_case(w))
}

/// A lexical token with its type, literal value, and source position.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub ty: TokenType,
    pub literal: String,
    pub line: usize,
    pub col: usize,
    /// Optional symbolic openCypher-TCK error-*detail* name carried by a
    /// [`TokenType::Error`] token (e.g. `"InvalidNumberLiteral"`,
    /// `"InvalidUnicodeLiteral"`). The parser propagates it onto the
    /// [`crate::ParseError`] it raises when it surfaces the error token, so a
    /// malformed number/unicode literal reports its specific TCK detail rather
    /// than the generic `UnexpectedSyntax`. `None` for every non-error token.
    pub detail: Option<&'static str>,
}

impl Token {
    pub fn new(ty: TokenType, literal: impl Into<String>, line: usize, col: usize) -> Self {
        Token {
            ty,
            literal: literal.into(),
            line,
            col,
            detail: None,
        }
    }

    /// Build a [`TokenType::Error`] token tagged with a TCK error-detail name
    /// (see [`Token::detail`]).
    pub fn error_with_detail(
        literal: impl Into<String>,
        line: usize,
        col: usize,
        detail: &'static str,
    ) -> Self {
        Token {
            ty: TokenType::Error,
            literal: literal.into(),
            line,
            col,
            detail: Some(detail),
        }
    }
}

/// Look up a keyword from an identifier string (case-insensitive uppercase).
pub fn lookup_keyword(ident: &str) -> TokenType {
    match ident.to_ascii_uppercase().as_str() {
        "MATCH" => TokenType::MATCH,
        "OPTIONAL" => TokenType::OPTIONAL,
        "CREATE" => TokenType::CREATE,
        "INSERT" => TokenType::INSERT,
        "MERGE" => TokenType::MERGE,
        "DELETE" => TokenType::DELETE,
        "DETACH" => TokenType::DETACH,
        "SET" => TokenType::SET,
        "REMOVE" => TokenType::REMOVE,
        "RETURN" => TokenType::RETURN,
        "WITH" => TokenType::WITH,
        "UNWIND" => TokenType::UNWIND,
        "WHERE" => TokenType::WHERE,
        "ORDER" => TokenType::ORDER,
        "BY" => TokenType::BY,
        "ASC" | "ASCENDING" => TokenType::ASC,
        "DESC" | "DESCENDING" => TokenType::DESC,
        "LIMIT" => TokenType::LIMIT,
        "SKIP" => TokenType::SKIP,
        "AS" => TokenType::AS,
        "DISTINCT" => TokenType::DISTINCT,
        "AND" => TokenType::AND,
        "OR" => TokenType::OR,
        "NOT" => TokenType::NOT,
        "XOR" => TokenType::XOR,
        "IN" => TokenType::IN,
        "STARTS" => TokenType::STARTS,
        "ENDS" => TokenType::ENDS,
        "CONTAINS" => TokenType::CONTAINS,
        "IS" => TokenType::IS,
        "NULL" => TokenType::NULL,
        "TRUE" => TokenType::TRUE,
        "FALSE" => TokenType::FALSE,
        "CASE" => TokenType::CASE,
        "WHEN" => TokenType::WHEN,
        "THEN" => TokenType::THEN,
        "ELSE" => TokenType::ELSE,
        "END" => TokenType::END,
        "FOREACH" => TokenType::FOREACH,
        "CALL" => TokenType::CALL,
        "YIELD" => TokenType::YIELD,
        "UNION" => TokenType::UNION,
        "ALL" => TokenType::ALL,
        "ON" => TokenType::ON,
        "INDEX" => TokenType::INDEX,
        "CONSTRAINT" => TokenType::CONSTRAINT,
        "FOR" => TokenType::FOR,
        "REQUIRE" => TokenType::REQUIRE,
        "UNIQUE" => TokenType::UNIQUE,
        "EXISTS" => TokenType::EXISTS,
        "IF" => TokenType::IF,
        "DROP" => TokenType::DROP,
        "VECTOR" => TokenType::VECTOR,
        "POINT" => TokenType::POINT,
        "OPTIONS" => TokenType::OPTIONS,
        "NODE" => TokenType::NODE,
        "REL" => TokenType::REL,
        "COUNT" => TokenType::COUNT,
        "COLLECT" => TokenType::COLLECT,
        "LOAD" => TokenType::LOAD,
        "FROM" => TokenType::FROM,
        "CSV" => TokenType::CSV,
        "REDUCE" => TokenType::REDUCE,
        "SPATIAL" => TokenType::SPATIAL,
        "EXPLAIN" => TokenType::EXPLAIN,
        "PROFILE" => TokenType::PROFILE,
        "INFINITY" | "INF" => TokenType::INFINITY,
        "NAN" => TokenType::NAN,
        _ => TokenType::Identifier,
    }
}

/// True when `name` has the plain ASCII identifier shape
/// `[A-Za-z_][A-Za-z0-9_]*` — the shape-only half of the quoting policy.
///
/// Reads the lexer's canonical ASCII predicates (`is_identifier_start` /
/// `is_identifier_part`, ADR-0182) behind an explicit `is_ascii()` gate. That
/// gate is deliberately narrower than what the lexer accepts: unquoted
/// identifiers may use the Unicode 13.0.0 classes (so `café` parses bare),
/// but rendering quotes every non-ASCII name. Backticks keep generated Cypher
/// ASCII — readable by older CypherLite versions that predate Unicode
/// identifiers, and independent of the pinned Unicode table version — and the
/// quoted form round-trips exactly in every name position, so the narrower
/// gate is a rendering choice, not a dialect claim.
fn has_plain_identifier_shape(name: &str) -> bool {
    let mut chars = name.chars();
    // ASCII-only: see the doc comment. The predicates accept non-ASCII too.
    chars
        .next()
        .is_some_and(|c| c.is_ascii() && is_identifier_start(c))
        && chars.all(|c| c.is_ascii() && is_identifier_part(c))
}

/// True when `name` must be backtick-quoted to round-trip as a Cypher name.
///
/// The DDL / canonical identifier-quoting policy: a name is bare only when it
/// has the plain identifier shape ([`has_plain_identifier_shape`]) **and**
/// [`lookup_keyword`] does not classify it as a keyword.
///
/// Keyword names need quotes even in positions that accept them bare (a
/// property key after `.`, a label or relationship type per ADR-0169) because
/// the same spelling is a keyword in the positions that call
/// [`TokenType::is_ident_like`] — a variable, alias, map key, or the
/// `ON (n.prop)` property name of `CREATE INDEX` — so the unquoted form
/// would not parse everywhere. Quoting every keyword keeps one rendered
/// statement runnable in every position instead of deciding per site.
pub fn identifier_needs_quotes(name: &str) -> bool {
    !has_plain_identifier_shape(name) || lookup_keyword(name).is_keyword()
}

/// Renders `name` as a Cypher identifier for generated Cypher text: bare
/// when [`identifier_needs_quotes`] is false, otherwise through
/// [`quote_identifier_always`].
pub fn quote_identifier(name: &str) -> String {
    if identifier_needs_quotes(name) {
        quote_identifier_always(name)
    } else {
        name.to_string()
    }
}

/// Renders `name` as a Cypher identifier for **display** text (CIP-257's
/// `SHOW CURRENT GRAPH TYPE AS GRAPH` node/relationship properties and
/// constraints): bare when [`has_plain_identifier_shape`] holds, otherwise
/// through [`quote_identifier_always`].
///
/// This is the shape-only half of the policy: keyword spellings stay bare
/// (`when`, `order`), matching CypherLite's CIP-257 display corpus (a
/// hyphenated `` `prop-3` `` is quoted, a keyword `when` is not). Display text
/// is not re-parsed as a statement, so it owes no keyword-position guarantee;
/// a name that must round-trip as generated Cypher goes through
/// [`quote_identifier`] instead.
pub fn quote_identifier_shaped(name: &str) -> String {
    if has_plain_identifier_shape(name) {
        name.to_string()
    } else {
        quote_identifier_always(name)
    }
}

/// Renders `name` in the always-backtick-quoted form: wrapped in backticks
/// with every embedded backtick doubled (the Cypher escape rule), whatever
/// [`identifier_needs_quotes`] would say. The one spelling of the quoting
/// escape: [`quote_identifier`]'s quoting arm, [`quote_identifier_shaped`]'s
/// quoting arm, and every caller that needs a forced-quoted name
/// (SHOW-rendered Cypher, canonical graph-type text) call it instead of
/// re-spelling the `format!`/`replace`.
pub fn quote_identifier_always(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_keyword() {
        assert_eq!(lookup_keyword("MATCH"), TokenType::MATCH);
        assert_eq!(lookup_keyword("match"), TokenType::MATCH);
        assert_eq!(lookup_keyword("Match"), TokenType::MATCH);
        assert_eq!(lookup_keyword("RETURN"), TokenType::RETURN);
        assert_eq!(lookup_keyword("foo"), TokenType::Identifier);
        assert_eq!(lookup_keyword("ASCENDING"), TokenType::ASC);
        assert_eq!(lookup_keyword("DESCENDING"), TokenType::DESC);
    }

    #[test]
    fn test_is_ident_like() {
        assert!(TokenType::Identifier.is_ident_like());
        // CIP-150 NOTES.md Q2 (`@neo4j-verified`): `RETURN` is a
        // non-reserved word usable as an ordinary variable/alias name.
        for ty in [
            TokenType::RETURN,
            TokenType::NODE,
            TokenType::COUNT,
            TokenType::ALL,
            TokenType::NOT,
            TokenType::IN,
            TokenType::IS,
            TokenType::NULL,
            TokenType::TRUE,
            TokenType::INFINITY,
        ] {
            assert!(ty.is_ident_like(), "{ty:?}");
        }
        for ty in CLAUSE_KEYWORDS {
            assert!(ty.is_keyword() && !ty.is_ident_like(), "{ty:?}");
        }
        assert!(!TokenType::LeftParen.is_ident_like());
    }

    #[test]
    fn test_is_schema_name() {
        // ADR-0169: every clause keyword is a label name.
        for ty in CLAUSE_KEYWORDS {
            assert!(ty.is_schema_name(), "{ty:?}");
        }
        assert!(TokenType::Identifier.is_schema_name());
        assert!(TokenType::RETURN.is_schema_name());
        for ty in [
            TokenType::LeftParen,
            TokenType::Colon,
            TokenType::Pipe,
            TokenType::Parameter,
            TokenType::String,
            TokenType::Integer,
        ] {
            assert!(!ty.is_schema_name(), "{ty:?}");
        }
    }

    #[test]
    fn test_is_keyword() {
        assert!(TokenType::MATCH.is_keyword());
        assert!(TokenType::RETURN.is_keyword());
        assert!(TokenType::PROFILE.is_keyword());
        assert!(!TokenType::Identifier.is_keyword());
        assert!(!TokenType::LeftParen.is_keyword());
    }

    #[test]
    fn test_identifier_needs_quotes_shape() {
        for bare in ["a", "_", "_a1", "Person", "ship_date", "x9"] {
            assert!(
                !identifier_needs_quotes(bare),
                "{bare} is a bare identifier"
            );
            assert_eq!(quote_identifier(bare), bare);
        }
        for quoted in ["", "1abc", "a-b", "a b", "n.prop", "Ünïcode", "a`b"] {
            assert!(
                identifier_needs_quotes(quoted),
                "{quoted:?} must be backtick-quoted"
            );
        }
        assert_eq!(
            quote_identifier("we`ird"),
            "`we``ird`",
            "an embedded backtick is doubled"
        );
    }

    /// The display policy is shape-only: a keyword spelling renders bare (the
    /// CIP-257 display corpus writes `when :: STRING`), while a name that
    /// fails the plain identifier shape keeps its backticks. The DDL/canonical
    /// policy stays keyword-aware: `quote_identifier("when")` is `` `when` ``,
    /// which is what a generated statement needs in variable/alias positions.
    #[test]
    fn test_quote_identifier_shaped_is_shape_only() {
        assert_eq!(quote_identifier_shaped("when"), "when");
        assert_eq!(quote_identifier_shaped("order"), "order");
        assert_eq!(quote_identifier_shaped("prop-3"), "`prop-3`");
        assert_eq!(quote_identifier_shaped("a b"), "`a b`");
        assert_eq!(quote_identifier_shaped(""), "``");
        assert_eq!(
            quote_identifier_shaped("we`ird"),
            "`we``ird`",
            "the display quoter reuses the one escape"
        );
        assert_eq!(
            quote_identifier_shaped("prop-3"),
            quote_identifier_always("prop-3")
        );
        assert_eq!(
            quote_identifier("when"),
            "`when`",
            "the DDL/canonical policy stays keyword-aware"
        );
    }

    /// The forced-quoted constructor always quotes, and shares the one
    /// escape rule with [`quote_identifier`]'s quoting arm: an embedded
    /// backtick is doubled in both spellings.
    #[test]
    fn test_quote_identifier_always_forces_the_quoting_arm() {
        assert_eq!(quote_identifier_always("Person"), "`Person`");
        assert_eq!(quote_identifier_always("ship_date"), "`ship_date`");
        assert_eq!(quote_identifier_always("we`ird"), "`we``ird`");
        assert_eq!(
            quote_identifier_always("we`ird"),
            quote_identifier("we`ird")
        );
    }

    /// The rendering gate is ASCII-only even though the lexer accepts Unicode
    /// identifiers (ADR-0182): a non-ASCII name that parses bare still renders
    /// backticked, keeping generated Cypher ASCII and independent of the
    /// pinned Unicode table version. The quoted form round-trips exactly.
    #[test]
    fn test_identifier_needs_quotes_is_ascii_only() {
        for name in ["café", "Ünïcode", "名前"] {
            assert_eq!(one_identifier(name), name, "{name} lexes bare");
            assert!(identifier_needs_quotes(name), "{name} must render quoted");
            let rendered = quote_identifier(name);
            assert_eq!(rendered, format!("`{name}`"));
            assert_eq!(one_identifier(&rendered), name);
        }
    }

    /// The policy is keyword-aware: a keyword spelling is quoted even where
    /// the grammar would take it bare (a property key after `.`, a label per
    /// ADR-0169), because `CREATE INDEX ... ON (n.<prop>)` reads the property
    /// through `is_ident_like()` and rejects a clause keyword. Every quoted
    /// form lexes back to one `Identifier` token with the original spelling.
    #[test]
    fn test_identifier_quoting_is_keyword_aware_and_round_trips() {
        for name in ["order", "Order", "match", "WHERE", "return", "count"] {
            let rendered = quote_identifier(name);
            assert_eq!(rendered, format!("`{name}`"), "keyword names are quoted");
            assert_eq!(one_identifier(&rendered), name);
        }
        for ty in CLAUSE_KEYWORDS {
            let spelling = format!("{ty:?}");
            assert!(
                identifier_needs_quotes(&spelling),
                "{spelling} is a clause keyword"
            );
            assert_eq!(one_identifier(&quote_identifier(&spelling)), spelling);
        }
    }

    /// The single `Identifier` token a rendered name lexes to, with its
    /// literal as written — the round-trip half of the quoting policy.
    fn one_identifier(rendered: &str) -> String {
        let tokens = crate::lexer::tokenize(rendered);
        let [token] = tokens.as_slice() else {
            panic!("{rendered} must lex to one identifier token, got {tokens:?}");
        };
        assert_eq!(token.ty, TokenType::Identifier, "{rendered}");
        token.literal.clone()
    }
}
