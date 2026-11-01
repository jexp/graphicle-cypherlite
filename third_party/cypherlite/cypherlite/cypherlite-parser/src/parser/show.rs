use super::*;

impl Parser {
    // ---- SHOW command family (CIP-11/CIP-68/CIP-84 and friends) ----

    /// Parses the `YIELD col [AS alias] [, col [AS alias] ...] | YIELD *`
    /// production shared by every catalog-backed SHOW/TERMINATE command and
    /// by `CALL proc() YIELD ...`. The one parser for the YIELD tail; callers
    /// read [`YieldList::present`] when they need to know whether a `YIELD`
    /// was written at all.
    pub(super) fn parse_show_yield_list(&mut self) -> ParseResult<YieldList> {
        let mut items = Vec::new();
        let mut all = false;
        if self.peek_type() == TokenType::YIELD {
            self.next_token();
            if self.peek_type() == TokenType::Star {
                self.next_token();
                all = true;
            } else {
                loop {
                    let yield_tok = self.next_token();
                    if !yield_tok.ty.is_ident_like() {
                        return Err(self.error_at_token(
                            &yield_tok,
                            format!(
                                "expected identifier after YIELD, got {:?}",
                                yield_tok.literal
                            ),
                        ));
                    }
                    let alias = if self.peek_type() == TokenType::AS {
                        self.next_token();
                        Some(self.parse_alias_after_as()?)
                    } else {
                        None
                    };
                    items.push(YieldItem {
                        name: yield_tok.literal,
                        alias,
                    });
                    if self.peek_type() != TokenType::Comma {
                        break;
                    }
                    self.next_token();
                }
            }
        }
        Ok(YieldList { items, all })
    }

    /// The optional trailing `WHERE expr` shared by every `parse_show_*`
    /// parser (CIP-184 syntax[6]/[7]: permitted whether or not an explicit
    /// `YIELD` precedes it). Returns `None` when no `WHERE` was written.
    fn parse_show_where_tail(&mut self) -> ParseResult<Option<Expr>> {
        if self.peek_type() == TokenType::WHERE {
            self.next_token();
            Ok(Some(self.parse_expression()?))
        } else {
            Ok(None)
        }
    }

    /// Case-normalizing lookahead for the SHOW dispatch: the lowercased
    /// literal of the token `offset` ahead when it is a word -- an
    /// `Identifier` or a keyword token (`INDEX`/`CONSTRAINT`/`ALL`/
    /// `POINT`/`VECTOR`/`NODE`/`REL`/`UNIQUE` are reserved tokens; every
    /// other dispatch word is identifier-spelled) -- else `None`.
    fn word_at(&self, offset: usize) -> Option<String> {
        let tok = self.peek_at(offset);
        if tok.ty == TokenType::Identifier || tok.ty.is_keyword() {
            Some(tok.literal.to_ascii_lowercase())
        } else {
            None
        }
    }

    /// SHOW-command family dispatch: the current token is the `SHOW`
    /// identifier (a non-reserved word, recognized by spelling at clause
    /// position). Routes to the dedicated `parse_show_*` parser by
    /// lookahead on the kind word(s) following SHOW; the exact
    /// singular-vs-plural validation happens inside each parser.
    pub(super) fn parse_show_dispatch(&mut self) -> ParseResult<Clause> {
        // CIP-62/CIP-64/CIP-74: the `<type filter>` prefix (`[ALL]
        // [NODE|REL[ATIONSHIP]] [PROPERTY]?
        // (UNIQUE[NESS]|KEY|EXIST[ENCE]|TYPE)`) can push CONSTRAINT(S)
        // up to 4 tokens ahead of SHOW -- scan forward through only
        // recognized filter words looking for it, same lookahead
        // technique as the INDEX kind-word check below.
        let mut show_targets_constraints = false;
        for i in 1..=4 {
            match self.word_at(i).as_deref() {
                Some("constraint" | "constraints") => {
                    show_targets_constraints = true;
                    break;
                }
                Some(
                    "all" | "node" | "rel" | "unique" | "relationship" | "property" | "uniqueness"
                    | "key" | "type" | "exist" | "existence",
                ) => {}
                _ => break,
            }
        }
        if show_targets_constraints {
            return self.parse_show_constraints();
        }
        // CIP-68: the remaining SHOW-command families, each dispatched by
        // spelling on the KIND word right after SHOW (or, for
        // `DEFAULT`/`HOME DATABASE` and `[ALL|POPULATED] ROLE[S]`, one
        // word further ahead) -- same lookahead technique as the
        // INDEX/CONSTRAINT scan above. The exact singular-vs-plural (and
        // `DEFAULT`/`HOME`/`CURRENT` singular-only) validation happens
        // inside each dedicated parse function; this dispatch only needs
        // to recognize which family a scenario is naming.
        match (self.word_at(1).as_deref(), self.word_at(2).as_deref()) {
            (Some("index" | "indexes"), _)
            | (
                Some(
                    "all" | "point" | "vector" | "range" | "text" | "fulltext" | "lookup" | "btree",
                ),
                Some("index" | "indexes"),
            ) => self.parse_show_indexes(),
            (Some("database" | "databases" | "default" | "home"), _) => self.parse_show_databases(),
            (Some("alias" | "aliases"), _) => {
                // ADR-0116 §2: `SHOW ALIAS[ES] ... FOR DATABASE[S]` --
                // recognized, then rejected with CE's
                // `UnsupportedAdministrationCommand`.
                let start = self.pos;
                self.unsupported_admin_clause(start)
            }
            (Some("role" | "roles"), _) | (Some("all" | "populated"), Some("role" | "roles")) => {
                self.parse_show_roles()
            }
            (Some("server" | "servers"), _) => self.parse_show_servers(),
            // CIP-176/CIP-184: `SHOW CURRENT GRAPH TYPE`, checked before
            // the general `current` -> `SHOW CURRENT USER` fallback
            // below.
            (Some("current"), Some("graph")) => self.parse_show_current_graph_type(),
            // CIP-178/CIP-180: `SHOW GRAPH TYPE <name> [YIELD ...]`.
            (Some("graph"), Some("type")) => self.parse_show_graph_type(),
            // CIP-68: `SHOW USER[S]`. The `defined` guard is a
            // load-bearing negative lookahead: it lets `SHOW USER
            // DEFINED FUNCTIONS` fall through to the
            // `parse_show_functions` catch-all below (where it parses as
            // `FunctionScope::UserDefined`) instead of being swallowed
            // here as `SHOW USER[S]`.
            (Some("user" | "users"), second) if second != Some("defined") => {
                self.parse_show_users()
            }
            // CIP-68: `SHOW CURRENT USER`.
            (Some("current"), _) => self.parse_show_users(),
            (Some("procedure" | "procedures"), _) => self.parse_show_procedures(),
            (Some("setting" | "settings"), _) => self.parse_show_settings(),
            (Some("transaction" | "transactions"), _) => self.parse_show_transactions(),
            _ => self.parse_show_functions(),
        }
    }

    /// `SHOW [ALL | BUILT IN | USER DEFINED] FUNCTIONS [YIELD col [, col
    /// ...] | YIELD *] [WHERE expr]` (CIP-245/CIP-247/CIP-263/CIP-84/
    /// CIP-157). Only the function catalog is supported — not a general
    /// SHOW-command framework (indexes/constraints/databases are a
    /// separate, larger feature out of scope here).
    pub(super) fn parse_show_functions(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW

        let scope = if self.peek_type() == TokenType::ALL {
            self.next_token();
            FunctionScope::All
        } else if self.peek_type() == TokenType::Identifier
            && self.peek().literal.eq_ignore_ascii_case("built")
        {
            self.next_token(); // consume BUILT
            self.expect(TokenType::IN)?;
            FunctionScope::BuiltIn
        } else if self.peek_type() == TokenType::Identifier
            && self.peek().literal.eq_ignore_ascii_case("user")
        {
            self.next_token(); // consume USER
            self.expect_kind_word(TokenType::Identifier, "defined", None, "DEFINED after USER")?;
            FunctionScope::UserDefined
        } else {
            FunctionScope::All
        };

        // CIP-68: both FUNCTION and FUNCTIONS are accepted.
        self.expect_kind_word(
            TokenType::Identifier,
            "function",
            Some("functions"),
            "FUNCTIONS after SHOW",
        )?;

        let yields = self.parse_show_yield_list()?;

        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowFunctions(ShowFunctionsClause {
            yields,
            where_expr,
            scope,
        }))
    }

    /// `SHOW [<type filter>] CONSTRAINTS [YIELD col [, col ...] | YIELD *]
    /// [WHERE expr]` (CIP-85's observation surface for `propertyType`,
    /// needed by CIP-100; the `<type filter>` grammar is CIP-62/CIP-64/
    /// CIP-74's `[ALL] [NODE|REL[ATIONSHIP]] [PROPERTY]?
    /// (UNIQUE[NESS]|KEY|EXIST[ENCE]|TYPE)`). Mirrors `parse_show_functions`
    /// otherwise.
    pub(super) fn parse_show_constraints(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW

        let type_filter = self.parse_constraint_type_filter()?;

        self.expect_kind_word(
            TokenType::CONSTRAINT,
            "constraint",
            Some("constraints"),
            "CONSTRAINT(S)",
        )?;

        let yields = self.parse_show_yield_list()?;

        // CIP-74 filtering[8]/[9]: the YIELD's own `ORDER BY`/`SKIP`/
        // `LIMIT` paging sub-grammar, written directly after the YIELD
        // list and before any trailing `WHERE` -- distinct from a
        // `RETURN ... ORDER BY ...` (or a separate standalone
        // `ORDER BY`/`SKIP`/`LIMIT` clause) that follows this whole SHOW
        // clause instead. Shared with the standalone paging clause via
        // `parse_page_stages`.
        let stages = self.parse_page_stages()?;

        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowConstraints(ShowConstraintsClause {
            yields,
            where_expr,
            type_filter,
            stages,
        }))
    }

    /// Parses the `<type filter>` prefix of `SHOW <type filter> CONSTRAINTS`
    /// (CIP-62/CIP-64/CIP-74): `[ALL] [NODE|REL[ATIONSHIP]] [PROPERTY]?
    /// (UNIQUE[NESS]|KEY|EXIST[ENCE]|TYPE)`, or nothing at all (bare `SHOW
    /// CONSTRAINTS`, equivalent to `ALL`). `PROPERTY` is only meaningful
    /// directly before `UNIQUE[NESS]`/`EXIST[ENCE]`/`TYPE` -- it never
    /// combines with `KEY` and never stands alone (CIP-74 filtering[12]/
    /// [13]).
    pub(super) fn parse_constraint_type_filter(&mut self) -> ParseResult<ConstraintTypeFilter> {
        if self.peek_type() == TokenType::ALL {
            self.next_token();
            return Ok(ConstraintTypeFilter::all());
        }

        let mut entity = ConstraintEntityFilter::Any;
        if self.peek_type() == TokenType::NODE {
            self.next_token();
            entity = ConstraintEntityFilter::Node;
        } else if self.peek_type() == TokenType::REL || self.peek_is_ident("relationship") {
            self.next_token();
            entity = ConstraintEntityFilter::Relationship;
        }

        let had_property = if self.peek_is_ident("property") {
            self.next_token();
            true
        } else {
            false
        };

        // The kind half: read the kind word once, then finish with a
        // single match on `(kind, had_property)` -- `PROPERTY` never
        // combines with `KEY` and never stands alone (CIP-74
        // filtering[12]/[13]).
        let kind: Option<ConstraintKindFilter> = if self.peek_is_ident("key") {
            Some(ConstraintKindFilter::Key)
        } else if self.peek_type() == TokenType::UNIQUE || self.peek_is_ident("uniqueness") {
            Some(ConstraintKindFilter::Unique)
        } else if self.peek_is_ident("exist") || self.peek_is_ident("existence") {
            Some(ConstraintKindFilter::Existence)
        } else if self.peek_is_ident("type") {
            Some(ConstraintKindFilter::PropertyType)
        } else {
            None
        };

        let filter = match (kind, had_property) {
            (Some(ConstraintKindFilter::Key), true) => {
                return Err(self.error(
                    "InvalidConstraintTypeFilter: PROPERTY cannot combine with the KEY filter",
                ));
            }
            (Some(ConstraintKindFilter::Key), false) => ConstraintTypeFilter {
                entity,
                kind,
                property_only: false,
            },
            (Some(_), _) => ConstraintTypeFilter {
                entity,
                kind,
                property_only: had_property,
            },
            (None, true) => {
                return Err(self.error(
                    "InvalidConstraintTypeFilter: PROPERTY must be followed by UNIQUE[NESS], \
                     EXIST[ENCE], or TYPE",
                ));
            }
            (None, false) => ConstraintTypeFilter {
                entity,
                kind: None,
                property_only: false,
            },
        };
        if kind.is_some() {
            self.next_token(); // consume the kind word
        }
        Ok(filter)
    }

    /// `SHOW [ALL|RANGE|POINT|TEXT|FULLTEXT|VECTOR|LOOKUP] INDEXES [YIELD
    /// col [, col ...] | YIELD *] [WHERE expr]` (CIP-11). `SHOW BTREE
    /// INDEXES` is dispatched here too, purely to raise a clean
    /// `UnsupportedIndexType` error (CIP-11 SD[6]: BTREE was removed, not
    /// deprecated).
    pub(super) fn parse_show_indexes(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW

        let tok = self.peek().clone();
        let kind_filter = match tok.ty {
            TokenType::ALL => {
                self.next_token();
                None
            }
            TokenType::POINT => {
                self.next_token();
                Some(IndexKind::Point)
            }
            TokenType::VECTOR => {
                self.next_token();
                Some(IndexKind::Vector)
            }
            TokenType::Identifier => match tok.literal.to_ascii_lowercase().as_str() {
                "range" => {
                    self.next_token();
                    Some(IndexKind::Range)
                }
                "text" => {
                    self.next_token();
                    Some(IndexKind::Text)
                }
                "fulltext" => {
                    self.next_token();
                    Some(IndexKind::Fulltext)
                }
                "lookup" => {
                    self.next_token();
                    Some(IndexKind::Lookup)
                }
                "btree" => {
                    return Err(self.error_at_token(
                        &tok,
                        "UnsupportedIndexType: BTREE indexes are no longer supported".to_string(),
                    ));
                }
                _ => None,
            },
            _ => None,
        };

        // consume INDEX / INDEXES
        self.expect_kind_word(TokenType::INDEX, "index", Some("indexes"), "INDEXES")?;

        let yields = self.parse_show_yield_list()?;

        // CIP-46 con[15]/CIP-74 filtering[8]/[9]: the YIELD's own `ORDER
        // BY`/`SKIP`/`LIMIT` paging sub-grammar, written directly after the
        // YIELD list and before any trailing `WHERE` -- shared with
        // `parse_show_constraints` via `parse_page_stages`.
        let stages = self.parse_page_stages()?;

        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowIndexes(ShowIndexesClause {
            kind_filter,
            yields,
            where_expr,
            stages,
        }))
    }

    /// `SHOW GRAPH TYPE <name> [YIELD ...]` (CIP-180's SHOW surface).
    pub(super) fn parse_show_graph_type(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW
        self.expect_keyword_ident("GRAPH")?;
        self.expect_keyword_ident("TYPE")?;
        let name = self.parse_bare_name("identifier")?;
        let yields = self.parse_show_yield_list()?;
        Ok(Clause::ShowGraphType(ShowGraphTypeClause { name, yields }))
    }

    /// `SHOW CURRENT GRAPH TYPE [AS GRAPH [SINGLE | SEPARATE]] [YIELD ...]
    /// [WHERE ...]` (CIP-176 overview / CIP-184 / CIP-257's `AS GRAPH`
    /// visualization extension).
    pub(super) fn parse_show_current_graph_type(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW
        self.expect_keyword_ident("CURRENT")?;
        self.expect_keyword_ident("GRAPH")?;
        self.expect_keyword_ident("TYPE")?;
        // CIP-257: `AS GRAPH` is a contextual, non-reserved extension --
        // `AS` is a real keyword token, but the following `GRAPH` (and the
        // optional `SINGLE`/`SEPARATE` any-node keyword after it) are plain
        // identifiers, so this only commits once both `AS` and an
        // identifier literally spelled `GRAPH` are seen.
        let as_graph = if self.peek_type() == TokenType::AS && self.peek_at_is_ident(1, "graph") {
            self.next_token(); // AS
            self.next_token(); // GRAPH
            let mode = if self.peek_is_ident("single") {
                self.next_token();
                AnyNodeMode::Single
            } else if self.peek_is_ident("separate") {
                self.next_token();
                AnyNodeMode::Separate
            } else {
                AnyNodeMode::Separate
            };
            Some(mode)
        } else {
            None
        };
        let yields = self.parse_show_yield_list()?;
        // Same YIELD paging sub-grammar as the other SHOW parsers
        // (`parse_page_stages`) -- OFFSET is accepted as a SKIP synonym
        // here too (CIP-168: OFFSET is usable everywhere SKIP is).
        let stages = self.parse_page_stages()?;
        let where_expr = self.parse_show_where_tail()?;
        Ok(Clause::ShowCurrentGraphType(ShowCurrentGraphTypeClause {
            yields,
            stages,
            where_expr,
            as_graph,
        }))
    }

    /// `SHOW DATABASE[S] [<name>] [YIELD ...] [WHERE ...]` / `SHOW DEFAULT
    /// DATABASE` / `SHOW HOME DATABASE` (CIP-42's four new non-default
    /// columns; CIP-68's singular/plural KIND acceptance). `DEFAULT`/`HOME`
    /// are singular-only: requiring the literal `DATABASE` spelling right
    /// after them (rather than the shared FUNCTION[S]-style
    /// singular-or-plural check) naturally rejects `SHOW DEFAULT
    /// DATABASES`/`SHOW HOME DATABASES` with a parse error, matching live
    /// Neo4j's documented exception.
    pub(super) fn parse_show_databases(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW

        let filter = if self.peek_is_ident("default") {
            self.next_token();
            self.expect_keyword_ident("DATABASE")?;
            DatabaseFilter::Default
        } else if self.peek_is_ident("home") {
            self.next_token();
            self.expect_keyword_ident("DATABASE")?;
            DatabaseFilter::Home
        } else {
            self.next_token(); // consume DATABASE / DATABASES
            match self.parse_optional_schema_name() {
                Some(name) => DatabaseFilter::Named(name),
                None => DatabaseFilter::All,
            }
        };

        let yields = self.parse_show_yield_list()?;
        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowDatabases(ShowDatabasesClause {
            filter,
            yields,
            where_expr,
        }))
    }

    /// `SHOW [ALL|POPULATED] ROLE[S] [WITH USER[S]] [YIELD ...] [WHERE
    /// ...]` (CIP-68).
    pub(super) fn parse_show_roles(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW

        let modifier = if self.peek_type() == TokenType::ALL {
            self.next_token();
            RoleModifier::All
        } else if self.peek_is_ident("populated") {
            self.next_token();
            RoleModifier::Populated
        } else {
            RoleModifier::Any
        };

        self.next_token(); // consume ROLE / ROLES

        let with_users = if self.peek_type() == TokenType::WITH {
            self.next_token();
            let user_tok = self.next_token();
            if !(user_tok.ty == TokenType::Identifier
                && (user_tok.literal.eq_ignore_ascii_case("user")
                    || user_tok.literal.eq_ignore_ascii_case("users")))
            {
                return Err(self.error_at_token(
                    &user_tok,
                    format!("expected USER(S) after WITH, got {:?}", user_tok.literal),
                ));
            }
            true
        } else {
            false
        };

        let yields = self.parse_show_yield_list()?;
        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowRoles(ShowRolesClause {
            modifier,
            with_users,
            yields,
            where_expr,
        }))
    }

    /// `SHOW SERVER[S] [YIELD ...] [WHERE ...]` (CIP-68).
    pub(super) fn parse_show_servers(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW
        self.next_token(); // consume SERVER / SERVERS

        let yields = self.parse_show_yield_list()?;
        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowServers(ShowServersClause {
            yields,
            where_expr,
        }))
    }

    /// `SHOW USER[S] [YIELD ...] [WHERE ...]` / `SHOW CURRENT USER`
    /// (CIP-68). `CURRENT` is singular-only, same technique as `DEFAULT`/
    /// `HOME DATABASE`.
    pub(super) fn parse_show_users(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW

        let current_only = if self.peek_is_ident("current") {
            self.next_token();
            self.expect_keyword_ident("USER")?;
            true
        } else {
            self.next_token(); // consume USER / USERS
            false
        };

        let yields = self.parse_show_yield_list()?;
        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowUsers(ShowUsersClause {
            current_only,
            yields,
            where_expr,
        }))
    }

    /// `SHOW PROCEDURE[S] [YIELD ...] [WHERE ...]` (CIP-68 grammar
    /// foundation).
    pub(super) fn parse_show_procedures(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW
        self.next_token(); // consume PROCEDURE / PROCEDURES

        let yields = self.parse_show_yield_list()?;
        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowProcedures(ShowProceduresClause {
            yields,
            where_expr,
        }))
    }

    /// True if the parser is positioned at a token that can only be the
    /// start of the optional `<names>`/`<id-list>` filter expression on
    /// `SHOW SETTINGS`/`SHOW TRANSACTIONS`/`TERMINATE TRANSACTIONS`.
    /// `YIELD`/`WHERE` are reserved keywords with their own `TokenType`
    /// (never `Identifier`), so they never need excluding here. CIP-123/
    /// CIP-253: the filter expression is no longer restricted to a
    /// literal/parameter/list -- a composed `SHOW SETTINGS`/`SHOW
    /// TRANSACTIONS`/`TERMINATE TRANSACTIONS` may reference a variable
    /// bound by an earlier clause (`SHOW SETTINGS inputs`, `SHOW SETTING
    /// name`) or a parenthesized expression
    /// (`TERMINATE TRANSACTIONS (database + '-transaction-999999')`), so a
    /// bare `Identifier`/`LeftParen`/numeric/boolean-literal start must also
    /// be accepted. The one thing this must *not* swallow is a following
    /// bare `SHOW`/`TERMINATE` clause composed right after this one with no
    /// filter expression of its own (`SHOW SETTINGS` then a fresh `SHOW
    /// TRANSACTIONS ...` clause) -- so the contextual (non-reserved)
    /// `Identifier`-spelled clause keywords are excluded here even though
    /// they syntactically look like a plain identifier reference. The
    /// excluded set is single-sourced as `CONTEXTUAL_CLAUSE_WORDS` in
    /// token.rs, shared with `parse_clause_inner`'s dispatch.
    pub(super) fn peek_starts_show_filter_expr(&self) -> bool {
        match self.peek_type() {
            TokenType::String
            | TokenType::Parameter
            | TokenType::LeftBracket
            | TokenType::LeftParen
            | TokenType::NULL
            | TokenType::Integer
            | TokenType::Float
            | TokenType::TRUE
            | TokenType::FALSE
            | TokenType::Dash => true,
            TokenType::Identifier => !is_contextual_clause_keyword(&self.peek().literal),
            _ => false,
        }
    }

    /// `SHOW SETTING[S] [<names>] [YIELD ...] [WHERE ...]` (CIP-68 grammar
    /// foundation; CIP-84 `SHOW SETTINGS 'name1', 'name2', ...` name-list
    /// form). `names` reports the settings catalog CIP-84 scenarios pin.
    pub(super) fn parse_show_settings(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW
        self.next_token(); // consume SETTING / SETTINGS

        let names = if self.peek_starts_show_filter_expr() {
            // CIP-84: `SHOW SETTINGS 'name1', 'name2', ...` -- a bare
            // comma-separated list of names, distinct from the single-
            // expression (`$param`/`[...]`/`null`) forms. Collapses to a
            // single `List` expression so `ShowSettingsClause::names`
            // stays a single `Option<Expr>`.
            let mut items = self.parse_comma_separated_exprs()?;
            if items.len() == 1 {
                Some(items.pop().unwrap())
            } else {
                Some(Expr::List(items))
            }
        } else {
            None
        };

        let yields = self.parse_show_yield_list()?;
        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowSettings(ShowSettingsClause {
            names,
            yields,
            where_expr,
        }))
    }

    /// `SHOW TRANSACTION[S] [<id-list>] [YIELD ...] [WHERE ...]` (CIP-68
    /// grammar foundation). CypherLite has no live-transaction catalog, so
    /// `ids` is parsed but never changes the (always empty) result.
    pub(super) fn parse_show_transactions(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SHOW
        self.next_token(); // consume TRANSACTION / TRANSACTIONS

        let ids = if self.peek_starts_show_filter_expr() {
            self.parse_comma_separated_exprs()?
        } else {
            Vec::new()
        };

        let yields = self.parse_show_yield_list()?;
        let where_expr = self.parse_show_where_tail()?;

        Ok(Clause::ShowTransactions(ShowTransactionsClause {
            ids,
            yields,
            where_expr,
        }))
    }
}
