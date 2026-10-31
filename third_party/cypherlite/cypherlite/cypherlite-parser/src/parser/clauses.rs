use super::*;

impl Parser {
    /// Label-expression mixing (`InvalidLabelExpression`/
    /// `MixedIsAndColonConjunction` for pattern-position label exprs) is
    /// scoped to one *top-level clause* (CIP-40 Section 3.6) -- but a
    /// clause containing an `EXISTS {}`/`COUNT {}`/`CALL {}` subquery
    /// expression is one clause together with everything nested inside
    /// that subquery's own statement body: `statement_depth == 1` exactly
    /// while parsing a clause of the *outermost* statement, so only that
    /// outer call resets/checks the flags; a `parse_clause` call nested
    /// inside a subquery's own `parse_statement` (`statement_depth >= 2`)
    /// leaves them alone, letting the subquery's label usage bubble up and
    /// combine with the enclosing clause's own
    /// (label-expression-mixing.feature scenario 12: two MATCH clauses
    /// nested inside one `COUNT {}` combine and must error) -- while two
    /// genuinely separate top-level clauses do NOT combine
    /// (label-expression-mixing.feature scenario 19,
    /// orthogonal-features-and-match-modes.feature scenario 9). Quantifier
    /// mixing, unlike label mixing, is scoped strictly per literal clause
    /// regardless of nesting (quantifier-mixing.feature scenario 17's
    /// COUNT-subquery exception), so it always resets here.
    pub(super) fn parse_clause(&mut self) -> ParseResult<Clause> {
        // ADR-0257: nested FOREACH bodies recurse `parse_clause` ->
        // `parse_clause_inner` -> `parse_foreach` -> `parse_clause`,
        // bypassing `parse_statement`, so the `statement_depth` cap never
        // engages on that cycle; count clause nesting here instead
        // (LABS-69). Sequential clauses in a statement or FOREACH body do
        // not accumulate: each `parse_clause` call fully returns before the
        // enclosing loop makes the next one, so the counter only grows
        // with true nesting.
        self.with_clause_depth(Self::parse_clause_at_current_depth)
    }

    pub(super) fn parse_clause_at_current_depth(&mut self) -> ParseResult<Clause> {
        let is_outer_clause = self.statement_depth <= 1;
        if is_outer_clause {
            self.side_effects = PatternSideEffectFlags::default();
        } else {
            // Label-mixing state bubbles up out of subquery bodies (see the
            // `parse_clause` doc comment); only the quantifier-mixing flags
            // reset per literal clause regardless of nesting.
            self.side_effects.quantifier_saw_legacy_bracket = false;
            self.side_effects.quantifier_saw_gpm_form = false;
        }
        let clause = self.parse_clause_inner()?;
        if is_outer_clause {
            if self.side_effects.label_saw_legacy_conjunction
                && self.side_effects.label_saw_gql_symbol
            {
                return Err(self.error(
                    "InvalidLabelExpression: mixing label expression symbols (|, &, !, and %) with colon (:) between labels is not allowed",
                ));
            }
            if self.side_effects.label_saw_legacy_conjunction
                && self.side_effects.label_saw_is_introducer
            {
                return Err(self.error(
                    "MixedIsAndColonConjunction: cannot mix the IS introducer with colon-conjunction within the same clause",
                ));
            }
        }
        if self.side_effects.quantifier_saw_legacy_bracket
            && self.side_effects.quantifier_saw_gpm_form
        {
            return Err(self.error(
                "MixedQuantifiers: cannot mix the legacy bracket-syntax variable-length quantifier with a GPM postfix/QPP quantifier within the same clause",
            ));
        }
        Ok(clause)
    }

    pub(super) fn parse_clause_inner(&mut self) -> ParseResult<Clause> {
        let tok = self.peek().clone();
        match tok.ty {
            TokenType::MATCH => self.parse_match(false),
            // CIP-159: `OPTIONAL MATCH` (pre-existing) and `OPTIONAL CALL`
            // (inline subquery or named procedure call) -- `OPTIONAL` must
            // immediately precede one of these two clause keywords.
            TokenType::OPTIONAL => {
                self.next_token(); // consume OPTIONAL
                match self.peek_type() {
                    TokenType::MATCH => self.parse_match(true),
                    TokenType::CALL => self.parse_call(true),
                    _ => Err(self.error(format!(
                        "expected MATCH or CALL, got {:?}",
                        self.peek().literal
                    ))),
                }
            }
            TokenType::CREATE => self.parse_create(),
            TokenType::INSERT => self.parse_insert(),
            TokenType::MERGE => self.parse_merge(),
            TokenType::DELETE => self.parse_delete(false),
            TokenType::DETACH => {
                self.next_token(); // consume DETACH
                self.expect(TokenType::DELETE)?;
                self.backup();
                self.parse_delete(true)
            }
            // CIP-122: `NODETACH DELETE <delete item list>` -- an explicit
            // spelling of the (already-default) plain, non-detaching DELETE.
            // Not a reserved keyword (recognized by spelling, like
            // `SHOW`/`USE`/`LET`/`FILTER` above) so it stays usable as an
            // ordinary variable name everywhere except directly before
            // DELETE at clause start.
            TokenType::Identifier if tok.literal.eq_ignore_ascii_case("nodetach") => {
                self.next_token(); // consume NODETACH
                self.expect(TokenType::DELETE)?;
                self.backup();
                self.parse_delete(false)
            }
            TokenType::SET => self.parse_set(),
            TokenType::REMOVE => self.parse_remove(),
            TokenType::RETURN => self.parse_return(),
            TokenType::WITH => self.parse_with(),
            TokenType::UNWIND => self.parse_unwind(),
            // CIP-250 (GQ10): `FOR <variable> IN <expression>`, the
            // GQL-compliant spelling of UNWIND -- valid wherever UNWIND is
            // valid. `FOREACH` is its own token, so the two never collide.
            TokenType::FOR => self.parse_for_clause(),
            TokenType::FOREACH => self.parse_foreach(),
            TokenType::CALL => self.parse_call(false),
            TokenType::LOAD => self.parse_load_csv(),
            TokenType::DROP => self.parse_drop(),
            // The contextual (identifier-spelled) clause keywords --
            // single-sourced as `CONTEXTUAL_CLAUSE_WORDS` in token.rs so
            // `peek_starts_show_filter_expr` excludes exactly this set.
            // None of them is a reserved keyword (each is only meaningful
            // at clause start, e.g. `SHOW FUNCTIONS`); recognized by
            // spelling on a plain Identifier token, same technique as the
            // GQL special forms (trim/normalize/property_exists) further
            // down.
            TokenType::Identifier if is_contextual_clause_keyword(&tok.literal) => {
                self.parse_contextual_clause(&tok)
            }
            // ADR-0116 §2: `START DATABASE <name>` / `STOP DATABASE
            // <name>` -- recognized, then rejected with CE's
            // `UnsupportedAdministrationCommand`. `START`/`STOP` stay
            // ordinary identifiers everywhere else (`MATCH (start)
            // WHERE start.alias = 'stop' ...` keeps parsing).
            TokenType::Identifier
                if (tok.literal.eq_ignore_ascii_case("start")
                    || tok.literal.eq_ignore_ascii_case("stop"))
                    && self.peek_at_is_ident(1, "database") =>
            {
                let start = self.pos;
                self.unsupported_admin_clause(start)
            }
            // CIP-168: standalone `<order by and page statement>` -- a new
            // clause starting directly with ORDER BY / SKIP / OFFSET /
            // LIMIT rather than attached to WITH/RETURN/YIELD.
            TokenType::ORDER | TokenType::SKIP | TokenType::LIMIT => {
                self.parse_standalone_order_by_page()
            }
            TokenType::Identifier if tok.literal.eq_ignore_ascii_case("offset") => {
                self.parse_standalone_order_by_page()
            }
            _ => Err(self.error_at_token(&tok, format!("unexpected token {:?}", tok.literal))),
        }
    }

    // ---- Clause parsers ----

    /// Dispatch for the contextual (identifier-spelled) clause keywords
    /// ([`crate::token::CONTEXTUAL_CLAUSE_WORDS`]), reached from
    /// `parse_clause_inner`'s single consolidated guard. `NEXT`/`CYPHER`
    /// are in the set but never start a clause here (statement-block
    /// boundary / pre-parser preamble respectively), so they get the same
    /// `unexpected token` error any unrecognized clause start gets.
    fn parse_contextual_clause(&mut self, tok: &Token) -> ParseResult<Clause> {
        let word = tok.literal.to_ascii_lowercase();
        match word.as_str() {
            "show" => self.parse_show_dispatch(),
            // CIP-68: `TERMINATE TRANSACTION[S] <ids> [YIELD ...]`.
            "terminate" => self.parse_terminate_transactions(),
            // CIP-141: `USE <graph-expr>` graph-selector clause.
            "use" => self.parse_use().map(Clause::Use),
            // CIP-206: `LET <var> = <expr>, ...` mid-query projection.
            "let" => self.parse_let(),
            // CIP-203: `FILTER [WHERE] <search condition>`.
            "filter" => self.parse_filter(),
            // CIP-140: `FINISH` terminal clause.
            "finish" => self.parse_finish(),
            "alter" => {
                // CIP-170: `ALTER DATABASE ...` is distinguished by the word
                // directly after `ALTER` (`DATABASE` vs `CURRENT`).
                if self.peek_at_is_ident(1, "database") {
                    self.parse_alter_database()
                } else if self.peek_at_is_ident(1, "alias") {
                    // ADR-0116 §2: `ALTER ALIAS <name> SET DATABASE <target>`
                    // -- recognized, then rejected with CE's
                    // `UnsupportedAdministrationCommand`.
                    let start = self.pos;
                    self.unsupported_admin_clause(start)
                } else {
                    // CIP-182/CIP-187: `ALTER CURRENT GRAPH TYPE SET { ... }`.
                    self.parse_alter_current_graph_type()
                }
            }
            _ => Err(self.error_at_token(tok, format!("unexpected token {:?}", tok.literal))),
        }
    }

    pub(super) fn parse_match(&mut self, optional: bool) -> ParseResult<Clause> {
        self.next_token(); // consume MATCH
        let (match_mode, match_mode_explicit) = self.parse_match_mode_prefix()?;
        let pattern = self.parse_pattern()?;
        self.validate_path_modes(&pattern, match_mode)?;

        // CIP-40 Section 2.6 last paragraph: an explicit match-mode keyword
        // cannot combine with a `shortestPath()`/`allShortestPaths()`
        // legacy function pattern in the same MATCH clause (path-selector-
        // mixing.feature scenarios 8/9) -- but combines freely with an
        // *explicit* selector keyword pattern (scenario 11) or with a
        // shortestPath function in a *separate* MATCH clause (scenario 18).
        if match_mode_explicit && pattern.iter().any(|p| p.is_shortest_path_function) {
            return Err(self.error(
                "MixingShortestPathAndMatchMode: an explicit match mode cannot be combined with a shortestPath()/allShortestPaths() pattern",
            ));
        }
        // CIP-73 Section 3.3 / path-selector-mixing.feature scenario 14: a
        // MATCH clause with more than one path pattern cannot have a
        // *selective* selector (anything but the implicit/explicit `ALL`)
        // written as an explicit keyword on any of them -- but the legacy
        // `shortestPath()`/`allShortestPaths()` function forms are exempt
        // (scenario 13: two `shortestPath()` patterns combine freely, each
        // evaluated independently). CIP-60 5.4.4/05-selective-path-patterns
        // [2]/[4]: `REPEATABLE ELEMENTS` lifts this sole-path-pattern
        // restriction. The finer-grained CIP-60 5.4.4 rules for such a MATCH
        // (selective patterns may only share *boundary* variables, and their
        // own predicates can't reference a sibling pattern's variable) are
        // not currently enforced: the pinned TCK corpus has no
        // selective-path-pattern scenarios.
        if match_mode != MatchMode::RepeatableElements
            && pattern.len() > 1
            && pattern.iter().any(|p| {
                !p.is_shortest_path_function
                    && matches!(
                        p.selector,
                        Some(
                            PathSelector::Any(_)
                                | PathSelector::AnyShortest
                                | PathSelector::AllShortest
                                | PathSelector::Shortest { .. }
                        )
                    )
            })
        {
            return Err(self.error(
                "MultiplePathPatternsWithSelectiveSelector: a MATCH clause with more than one path pattern cannot have a selective path selector",
            ));
        }

        // CIP-79: `USING INDEX`/`USING INDEX SEEK`/`USING JOIN ON`/`USING
        // SCAN` query-planner hints, written directly after the pattern
        // and before WHERE. CypherLite has no cost-based planner to steer
        // (every query already always uses whatever index/scan strategy
        // its own evaluator picks), so a hint can never change results --
        // it's accepted and discarded rather than acted on, same as Neo4j
        // treats a hint that happens to be a no-op for its chosen plan.
        self.parse_using_hints()?;

        // CIP-224: the `<graph pattern where clause>` may appear either
        // before or after the `<search clause>` with no semantic
        // difference -- check for a leading WHERE first, then SEARCH, then
        // (only if no WHERE was seen yet) a trailing WHERE.
        let mut where_expr = if self.peek_type() == TokenType::WHERE {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let search = if self.is_search_clause_start() {
            Some(self.parse_search_clause()?)
        } else {
            None
        };

        if where_expr.is_none() && self.peek_type() == TokenType::WHERE {
            self.next_token();
            where_expr = Some(self.parse_expression()?);
        }

        Ok(Clause::Match(MatchClause {
            optional,
            match_mode,
            pattern,
            where_expr,
            search,
        }))
    }

    /// CIP-79: zero or more `USING INDEX <var>:<Label>(<prop>[, <prop>...])`
    /// / `USING INDEX SEEK ...` / `USING JOIN ON <var>[, <var>...]` /
    /// `USING SCAN <var>:<Label>` planner hints directly after a MATCH
    /// clause's pattern. `USING` is a non-reserved word (never added to
    /// `token.rs`'s keyword table), recognized by spelling like `SEARCH`/
    /// `FILTER` above. CypherLite has no cost-based planner for these to
    /// steer, so every hint is parsed (to keep queries that use them
    /// syntactically valid) and then discarded -- a hint never changes
    /// query results, only which plan produces them.
    pub(super) fn parse_using_hints(&mut self) -> ParseResult<()> {
        while self.peek_type() == TokenType::Identifier
            && self.peek().literal.eq_ignore_ascii_case("using")
        {
            self.next_token(); // consume USING
            let kind_tok = self.next_token();
            match kind_tok.literal.to_ascii_lowercase().as_str() {
                "index" => {
                    // Optional `SEEK` after `INDEX` (`USING INDEX SEEK
                    // n:Label(prop)`) -- same hint, just a stronger one.
                    if self.peek_type() == TokenType::Identifier
                        && self.peek().literal.eq_ignore_ascii_case("seek")
                    {
                        self.next_token();
                    }
                    self.parse_bare_name("identifier")?; // variable
                    self.expect(TokenType::Colon)?;
                    self.parse_label_name()?;
                    self.expect(TokenType::LeftParen)?;
                    loop {
                        self.parse_bare_name("identifier")?; // property
                        if self.peek_type() == TokenType::Comma {
                            self.next_token();
                            continue;
                        }
                        break;
                    }
                    self.expect(TokenType::RightParen)?;
                }
                "join" => {
                    if self.peek_type() == TokenType::ON {
                        self.next_token();
                    }
                    loop {
                        self.parse_bare_name("identifier")?; // variable
                        if self.peek_type() == TokenType::Comma {
                            self.next_token();
                            continue;
                        }
                        break;
                    }
                }
                "scan" => {
                    self.parse_bare_name("identifier")?; // variable
                    self.expect(TokenType::Colon)?;
                    self.parse_label_name()?;
                }
                _ => {
                    return Err(self.error_at_token(
                        &kind_tok,
                        format!(
                            "expected INDEX, JOIN, or SCAN after USING, got {:?}",
                            kind_tok.literal
                        ),
                    ))
                }
            }
        }
        Ok(())
    }

    /// Parses the optional CIP-51/CIP-60 match-mode prefix
    /// (`REPEATABLE ELEMENTS` / `DIFFERENT RELATIONSHIPS`) that may appear
    /// directly after `MATCH`. Both keyword pairs are contextual (plain
    /// identifiers everywhere else); since a graph pattern always begins
    /// with `(`, a bare identifier immediately after `MATCH` can only be one
    /// of these two phrases, so no lookahead ambiguity is possible.
    /// Returns `(mode, explicit)` -- `explicit` is true only when a
    /// `REPEATABLE ELEMENTS`/`DIFFERENT RELATIONSHIPS` keyword was actually
    /// written, distinguishing it from the identical-valued implicit
    /// default (needed for CIP-40's explicit-match-mode-vs-shortestPath-
    /// function mixing check).
    pub(super) fn parse_match_mode_prefix(&mut self) -> ParseResult<(MatchMode, bool)> {
        if self.peek_type() == TokenType::Identifier {
            let lower = self.peek().literal.to_ascii_lowercase();
            if lower == "repeatable" && self.peek_at(1).literal.eq_ignore_ascii_case("elements") {
                self.next_token();
                self.next_token();
                return Ok((MatchMode::RepeatableElements, true));
            }
            if lower == "different"
                && self
                    .peek_at(1)
                    .literal
                    .eq_ignore_ascii_case("relationships")
            {
                self.next_token();
                self.next_token();
                return Ok((MatchMode::DifferentRelationships, true));
            }
        }
        Ok((MatchMode::DifferentRelationships, false))
    }

    /// CIP-51 Section 3.1.3 rules 2-4, checked once a whole graph pattern
    /// (one MATCH clause's comma-separated path pattern list) has been
    /// parsed. Each `EXISTS { MATCH ... }` subquery gets its own independent
    /// call (a different "depth", rule 3), since it routes back through
    /// `parse_match`.
    pub(super) fn validate_path_modes(
        &self,
        pattern: &[PatternPath],
        match_mode: MatchMode,
    ) -> ParseResult<()> {
        // Rule 3: every path pattern at this depth shares one resolved path
        // mode (explicit keyword, or the Rule 1 default of WALK).
        let mut resolved: Option<PathMode> = None;
        for path in pattern {
            let m = path.mode.unwrap_or(PathMode::Walk);
            match resolved {
                None => resolved = Some(m),
                Some(prev) if prev != m => {
                    return Err(self.error(
                        "UnsupportedCombinationOfPathModes: a graph pattern cannot mix path modes",
                    ));
                }
                _ => {}
            }
        }

        if match_mode == MatchMode::RepeatableElements {
            // Rule 4: REPEATABLE ELEMENTS only combines with WALK (including
            // the implicit default).
            if let Some(m) = resolved {
                if m != PathMode::Walk {
                    return Err(self.error(
                        "UnsupportedCombinationOfMatchModeAndPathMode: REPEATABLE ELEMENTS only combines with WALK",
                    ));
                }
            }
            // Section 2.3.3.3 / Deviation D2: Neo4j 2026.05 rejects an
            // unbounded quantified path pattern under REPEATABLE ELEMENTS
            // unconditionally (42N53 "unsafe usage of repeatable elements"),
            // regardless of any path selector that might otherwise bound it
            // (CIP-73 Deviation #5/#6: confirmed for ALL, ANY, and SHORTEST
            // alike). The doc's own claimed SHORTEST relaxation (CIP-51
            // Section 2.3.3.3) is intentionally *not* implemented: without
            // real relationship-uniqueness pruning (which REPEATABLE
            // ELEMENTS disables), evaluating it would mean brute-force
            // enumerating every candidate up to the engine's iteration cap
            // before the SHORTEST selector gets a chance to narrow anything
            // down -- exponential in the cap on a cyclic graph.
            for path in pattern {
                if path_has_unbounded_quantifier(path) {
                    return Err(self.error(
                        "UnsafeRepeatableElements: REPEATABLE ELEMENTS cannot be combined with an unbounded quantified path pattern",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn parse_create(&mut self) -> ParseResult<Clause> {
        let start = self.pos;
        self.next_token(); // consume CREATE

        // CIP-170: `CREATE [OR REPLACE] DATABASE ...` / `CREATE [OR
        // REPLACE] COMPOSITE DATABASE ...`. Checked before the general
        // dispatch below -- a plain `CREATE (...)` node pattern can never
        // start with `OR`. `database`/`composite` aren't reserved words
        // though, so they *can* legitimately be a named-path variable
        // (`CREATE database = (a)-[:X]->(b)`, CIP-146's `<named path>`
        // form) -- disambiguated by checking the token immediately after
        // isn't `=`, which a `<create database>`'s `<name>` position can
        // never be followed by.
        //
        // ADR-0116 §2: with fixed namespaces, every `CREATE [OR REPLACE]
        // [COMPOSITE] DATABASE` form is recognized and then rejected with
        // CE's `UnsupportedAdministrationCommand` rather than executed.
        {
            let or_replace =
                self.peek_type() == TokenType::OR && self.peek_at_is_ident(1, "replace");
            let after = if or_replace { 2 } else { 0 };
            if self.peek_at_is_ident(after, "database")
                && self.peek_at(after + 1).ty != TokenType::Equals
            {
                return self.unsupported_admin_clause(start);
            }
            if self.peek_at_is_ident(after, "composite")
                && self.peek_at_is_ident(after + 1, "database")
                && self.peek_at(after + 2).ty != TokenType::Equals
            {
                return self.unsupported_admin_clause(start);
            }
            // ADR-0116 §2: `CREATE [OR REPLACE] ALIAS <name> FOR
            // DATABASE <target>`. Same named-path disambiguation as
            // above: an `ALIAS` variable is never followed by `<name>
            // FOR`.
            if self.peek_at_is_ident(after, "alias")
                && self.peek_at(after + 1).ty.is_ident_like()
                && self.peek_at(after + 2).ty == TokenType::FOR
            {
                return self.unsupported_admin_clause(start);
            }
        }

        match self.peek_type() {
            TokenType::INDEX => {
                return Ok(Clause::CreateIndex(
                    self.parse_create_index(IndexKind::Range)?,
                ));
            }
            TokenType::VECTOR => {
                self.next_token();
                self.expect(TokenType::INDEX)?;
                self.backup();
                return Ok(Clause::CreateIndex(
                    self.parse_create_index(IndexKind::Vector)?,
                ));
            }
            TokenType::POINT => {
                self.next_token();
                self.expect(TokenType::INDEX)?;
                self.backup();
                return Ok(Clause::CreateIndex(
                    self.parse_create_index(IndexKind::Point)?,
                ));
            }
            TokenType::CONSTRAINT => {
                return Ok(Clause::CreateConstraint(self.parse_create_constraint()?));
            }
            // CIP-178/CIP-180: `CREATE GRAPH TYPE <name> [AS] { ... }`.
            // `GRAPH`/`TYPE` aren't reserved tokens (like `RANGE`/`TEXT`/...
            // INDEX below), recognized by spelling only directly before
            // `TYPE` -- an ordinary `CREATE (graph:Type)`-style pattern
            // never has an identifier named `graph` immediately followed by
            // one named `type`.
            TokenType::Identifier
                if self.peek().literal.eq_ignore_ascii_case("graph")
                    && self.peek_at(1).ty == TokenType::Identifier
                    && self.peek_at(1).literal.eq_ignore_ascii_case("type") =>
            {
                return self.parse_create_graph_type();
            }
            // RANGE/TEXT/FULLTEXT/LOOKUP/BTREE aren't reserved tokens (they
            // lex as plain identifiers, like `SHOW`/`USE`/`LET` elsewhere in
            // this parser) so they're recognized by spelling, only directly
            // before INDEX -- an ordinary `CREATE p = (...)` named-path
            // pattern never has an identifier immediately followed by
            // INDEX, so there's no ambiguity with a path variable named
            // e.g. `range`.
            TokenType::Identifier if self.peek_at(1).ty == TokenType::INDEX => {
                let word = self.peek().literal.to_ascii_lowercase();
                let kind = match word.as_str() {
                    "range" => IndexKind::Range,
                    "text" => IndexKind::Text,
                    "fulltext" => IndexKind::Fulltext,
                    "lookup" => IndexKind::Lookup,
                    "btree" => {
                        let tok = self.peek().clone();
                        return Err(self.error_at_token(
                            &tok,
                            "UnsupportedIndexType: BTREE indexes were removed; use RANGE, POINT, TEXT, FULLTEXT, LOOKUP, or VECTOR"
                                .to_string(),
                        ));
                    }
                    _ => {
                        let pattern = self.parse_pattern()?;
                        return Ok(Clause::Create(CreateClause {
                            pattern,
                            is_insert: false,
                        }));
                    }
                };
                self.next_token(); // consume the kind word
                self.expect(TokenType::INDEX)?;
                self.backup();
                return Ok(Clause::CreateIndex(self.parse_create_index(kind)?));
            }
            _ => {}
        }

        let pattern = self.parse_pattern()?;
        Ok(Clause::Create(CreateClause {
            pattern,
            is_insert: false,
        }))
    }

    /// The GQL `INSERT` clause (CIP-40 Section 2.3.3 / Section 4.1 DML row):
    /// a legal alternate to `CREATE`, executed identically, but its label
    /// set specifications may not use the legacy colon-conjunction shorthand
    /// (`:A:B`) -- only `IS`-introduced (optionally `&`-composed) or a
    /// single bare `:Label` are allowed.
    pub(super) fn parse_insert(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume INSERT
        let pattern = self.parse_pattern()?;
        if self.side_effects.label_saw_legacy_conjunction {
            return Err(self.error(
                "42I54: Colon ':' conjunction is not allowed in INSERT. Use CREATE or conjunction with ampersand '&' instead.",
            ));
        }
        // CIP-114: parenthesized label expressions (`(A:B):C`) are a CREATE-
        // only grouping form -- INSERT's grammar doesn't accept `(` in
        // label-expression position at all.
        if self.side_effects.label_saw_paren_group {
            return Err(self.error("42I06: Invalid input '(', expected an identifier."));
        }
        // CIP-114: named path patterns (`INSERT p = (a)-[r]->(b)`) are not
        // allowed in INSERT -- only in CREATE.
        if pattern.iter().any(|p| p.variable.is_some()) {
            return Err(self.error(
                "42I54: Named patterns are not allowed in INSERT. Use CREATE or remove the name.",
            ));
        }
        Ok(Clause::Create(CreateClause {
            pattern,
            is_insert: true,
        }))
    }

    pub(super) fn parse_drop(&mut self) -> ParseResult<Clause> {
        let start = self.pos;
        self.next_token(); // consume DROP

        let tok = self.peek().clone();
        match tok.ty {
            TokenType::INDEX => {
                self.next_token();
                if self.peek_type() == TokenType::ON {
                    let target_tok = self.peek().clone();
                    let (label, properties) = self.parse_legacy_index_target()?;
                    let [property]: [String; 1] = properties.try_into().map_err(|_| {
                        self.error_at_token(
                            &target_tok,
                            "legacy DROP INDEX ON :Label(prop) takes exactly one property",
                        )
                    })?;
                    return Ok(Clause::DropIndex(DropIndexClause {
                        name: SchemaName::Literal(String::new()),
                        if_exists: false,
                        legacy_target: Some((label, property)),
                    }));
                }
                let (name, if_exists) = self.parse_drop_target("DROP INDEX")?;
                Ok(Clause::DropIndex(DropIndexClause {
                    name,
                    if_exists,
                    legacy_target: None,
                }))
            }
            TokenType::CONSTRAINT => {
                self.next_token();
                let (name, if_exists) = self.parse_drop_target("DROP CONSTRAINT")?;
                Ok(Clause::DropConstraint(DropConstraintClause {
                    name,
                    if_exists,
                }))
            }
            // ADR-0116 §2: `DROP DATABASE <name> [IF EXISTS] [<wait
            // clause>]`, `DROP COMPOSITE DATABASE ...` and `DROP ALIAS
            // <name> FOR DATABASE` are recognized (so they fail with CE's
            // `UnsupportedAdministrationCommand`, not a syntax error) but
            // never executed -- the namespaces are fixed. `DATABASE`/
            // `COMPOSITE`/`ALIAS` aren't reserved tokens, recognized by
            // spelling like `SHOW`/`USE` elsewhere in this parser.
            TokenType::Identifier if tok.literal.eq_ignore_ascii_case("database") => {
                self.unsupported_admin_clause(start)
            }
            TokenType::Identifier
                if tok.literal.eq_ignore_ascii_case("composite")
                    && self.peek_at_is_ident(1, "database") =>
            {
                self.unsupported_admin_clause(start)
            }
            TokenType::Identifier
                if tok.literal.eq_ignore_ascii_case("alias")
                    && self.peek_at(1).ty.is_ident_like()
                    && self.peek_at(2).ty == TokenType::FOR =>
            {
                self.unsupported_admin_clause(start)
            }
            _ => Err(self.error_at_token(
                &tok,
                format!(
                    "expected INDEX, CONSTRAINT, or DATABASE after DROP, got {:?}",
                    tok.literal
                ),
            )),
        }
    }

    /// The `[IF EXISTS] <name> [IF EXISTS]` target shared by `DROP INDEX`
    /// and `DROP CONSTRAINT`: `IF EXISTS` may precede or follow the name.
    /// Returns the name and whether `IF EXISTS` was written; `context`
    /// names the command in the missing-name error.
    pub(super) fn parse_drop_target(&mut self, context: &str) -> ParseResult<(SchemaName, bool)> {
        let leading_if_exists = self.parse_optional_if_exists()?;
        let name = self.parse_required_schema_name(context)?;
        let if_exists = leading_if_exists || self.parse_optional_if_exists()?;
        Ok((name, if_exists))
    }

    pub(super) fn parse_merge(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume MERGE

        let paths = self.parse_pattern()?;
        if paths.len() != 1 {
            return Err(self.error("MERGE requires exactly one pattern path"));
        }
        let pattern = paths.into_iter().next().unwrap();

        let mut on_create = Vec::new();
        let mut on_match = Vec::new();

        while self.peek_type() == TokenType::ON {
            self.next_token();
            let tok = self.next_token();
            match tok.ty {
                TokenType::CREATE => {
                    self.expect(TokenType::SET)?;
                    on_create = self.parse_set_items()?;
                }
                TokenType::MATCH => {
                    self.expect(TokenType::SET)?;
                    on_match = self.parse_set_items()?;
                }
                _ => {
                    return Err(self.error_at_token(
                        &tok,
                        format!("expected CREATE or MATCH after ON, got {:?}", tok.literal),
                    ));
                }
            }
        }

        Ok(Clause::Merge(MergeClause {
            pattern,
            on_create,
            on_match,
        }))
    }

    pub(super) fn parse_delete(&mut self, detach: bool) -> ParseResult<Clause> {
        self.next_token(); // consume DELETE
        let mut expressions = Vec::new();
        loop {
            let expr = self.parse_expression()?;
            expressions.push(expr);
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        Ok(Clause::Delete(DeleteClause {
            detach,
            expressions,
        }))
    }

    pub(super) fn parse_set(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume SET
        let items = self.parse_set_items()?;
        Ok(Clause::Set(SetClause { items }))
    }

    pub(super) fn parse_set_items(&mut self) -> ParseResult<Vec<SetItem>> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_set_item()?);
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        Ok(items)
    }

    /// Parses zero or more trailing SET/REMOVE-target accessors after a base
    /// expression: `.prop` (static) or `[<expr>]` (CIP-131 dynamic). Used by
    /// both `parse_set_item` and `parse_remove_item` so `n.a.b`, `n[a][b]`,
    /// `n.a[b]`, `(expr)[a].b`, etc. all parse uniformly -- all but the last
    /// accessor navigate to the entity ultimately mutated (see
    /// `build_property_access`).
    pub(super) fn parse_property_target_accessors(
        &mut self,
    ) -> ParseResult<Vec<PropertyTargetAccessor>> {
        let mut accessors = Vec::new();
        loop {
            match self.peek_type() {
                TokenType::Dot => {
                    self.next_token(); // consume .
                    let prop = self.parse_schema_name("property name")?;
                    accessors.push(PropertyTargetAccessor::Static(prop));
                }
                TokenType::LeftBracket => {
                    self.next_token(); // consume [
                    let key_expr = self.parse_expression()?;
                    self.expect(TokenType::RightBracket)?;
                    accessors.push(PropertyTargetAccessor::Dynamic(key_expr));
                }
                _ => break,
            }
        }
        Ok(accessors)
    }

    /// Folds a base expression and a non-empty accessor chain into the
    /// `PropertyAccess` a SET/REMOVE property item targets: all but the last
    /// accessor become part of the entity-navigation expression (nested
    /// `Expr::Property`/`Expr::Subscript`), and the last accessor becomes the
    /// actual static or dynamic (CIP-131) property target.
    pub(super) fn build_property_access(
        base: Expr,
        mut accessors: Vec<PropertyTargetAccessor>,
    ) -> PropertyAccess {
        let last = accessors.pop().expect("accessors must be non-empty");
        let mut entity_expr = base;
        for accessor in accessors {
            entity_expr = match accessor {
                PropertyTargetAccessor::Static(name) => Expr::Property(Box::new(PropertyAccess {
                    expr: entity_expr,
                    property: name,
                    key_expr: None,
                })),
                PropertyTargetAccessor::Dynamic(key_expr) => {
                    Expr::Subscript(Box::new(SubscriptExpr {
                        expr: entity_expr,
                        index: Some(key_expr),
                        from: None,
                        to: None,
                        slice: false,
                    }))
                }
            };
        }
        match last {
            PropertyTargetAccessor::Static(name) => PropertyAccess {
                expr: entity_expr,
                property: name,
                key_expr: None,
            },
            PropertyTargetAccessor::Dynamic(key_expr) => PropertyAccess {
                expr: entity_expr,
                property: String::new(),
                key_expr: Some(key_expr),
            },
        }
    }

    /// Parses a SET/REMOVE property target whose base is neither the plain
    /// `<variable>` form nor a parenthesized `(<expr>)` form -- e.g.
    /// `coalesce(n, m)[c]` (CIP-131 basic-syntax.feature [14]: an
    /// unparenthesized complex target is valid, unlike the doc's implication
    /// that parens are required -- see that CIP's NOTES.md deviation #1).
    /// Reuses the general postfix expression grammar (`parse_postfix`, the
    /// same one that parses `n.prop` / `n[expr]` on the read side, including
    /// chained accessors) so the whole target parses identically to the read
    /// path, then peels off the outermost accessor as the actual target
    /// (mirroring `build_property_access`'s last-accessor rule). Note
    /// `parse_postfix`'s `:` (HasLabel) handling only fires when its operand
    /// is *currently* a bare `Expr::Variable`, so it never fires here for a
    /// function-call/complex base -- no conflict with SET/REMOVE's own
    /// `n:Label` syntax, which is parsed by the caller before falling back
    /// to this method.
    pub(super) fn parse_general_property_target(&mut self) -> ParseResult<PropertyAccess> {
        let expr = self.parse_postfix()?;
        match expr {
            Expr::Property(pa) => Ok(*pa),
            Expr::Subscript(se) if !se.slice && se.index.is_some() => Ok(PropertyAccess {
                expr: se.expr,
                property: String::new(),
                key_expr: se.index,
            }),
            _ => Err(self.error(
                "expected a '.' or '[' property target after the SET/REMOVE target expression",
            )),
        }
    }

    pub(super) fn parse_set_item(&mut self) -> ParseResult<SetItem> {
        // Handle parenthesized expression: SET (n).prop = value / SET (n)[c] = value
        if self.peek_type() == TokenType::LeftParen {
            self.next_token(); // consume (
            let inner_expr = self.parse_expression()?;
            self.expect(TokenType::RightParen)?;

            let accessors = self.parse_property_target_accessors()?;
            if accessors.is_empty() {
                let next = self.peek().clone();
                return Err(self.error_at_token(
                    &next,
                    format!(
                        "expected '.' or '[' after parenthesized expression in SET, got {:?}",
                        next.literal
                    ),
                ));
            }
            let prop_access = Self::build_property_access(inner_expr, accessors);
            self.expect(TokenType::Equals)?;
            let val = self.parse_expression()?;
            return Ok(SetItem {
                property: Some(prop_access),
                variable: None,
                labels: Vec::new(),
                value: Some(val),
                merge: false,
                is_keyword: false,
                labels_function: None,
            });
        }

        // CIP-162 Alternatives: `SET labels(<expr>) = / += / -= <expr>`
        // (deferred by the CIP itself but implemented here as its stated
        // target semantics -- see `SetItem::labels_function`'s doc
        // comment). Must be tried before the CIP-131 general complex-target
        // branch below, since `labels(...)` also matches
        // `Identifier LeftParen` -- backtracks to that branch if the tail
        // after `labels(<expr>)` isn't actually `=`/`+=`/`-=` (e.g.
        // `labels(n)[0] = ...`, an ordinary dynamic-property-key target).
        if self.peek_type() == TokenType::Identifier
            && self.peek().literal.eq_ignore_ascii_case("labels")
            && self.peek_at(1).ty == TokenType::LeftParen
        {
            let saved = self.save_pos();
            self.next_token(); // consume 'labels'
            self.next_token(); // consume (
            let target = self.parse_expression()?;
            let op = if self.expect(TokenType::RightParen).is_ok() {
                match self.peek_type() {
                    TokenType::Equals => {
                        self.next_token();
                        Some(LabelsFunctionOp::Overwrite)
                    }
                    TokenType::Plus if self.peek_at(1).ty == TokenType::Equals => {
                        self.next_token();
                        self.next_token();
                        Some(LabelsFunctionOp::Extend)
                    }
                    TokenType::Dash if self.peek_at(1).ty == TokenType::Equals => {
                        self.next_token();
                        self.next_token();
                        Some(LabelsFunctionOp::Remove)
                    }
                    _ => None,
                }
            } else {
                None
            };
            match op {
                Some(op) => {
                    let val = self.parse_expression()?;
                    return Ok(SetItem {
                        property: None,
                        variable: None,
                        labels: Vec::new(),
                        value: Some(val),
                        merge: false,
                        is_keyword: false,
                        labels_function: Some((target, op)),
                    });
                }
                None => self.restore_pos(saved),
            }
        }

        // CIP-131: an unparenthesized complex target expression, e.g.
        // `SET coalesce(n, m)[c] = ...` -- the base isn't a plain variable
        // (it's a function call here), so it can't be the `n:Label` /
        // `n = ...` / `n += ...` / `n.prop = ...` / `n[...] = ...` forms
        // below, all of which start with a bare identifier NOT immediately
        // followed by `(`.
        if self.peek_type() == TokenType::Identifier && self.peek_at(1).ty == TokenType::LeftParen {
            let prop_access = self.parse_general_property_target()?;
            self.expect(TokenType::Equals)?;
            let val = self.parse_expression()?;
            return Ok(SetItem {
                property: Some(prop_access),
                variable: None,
                labels: Vec::new(),
                value: Some(val),
                merge: false,
                is_keyword: false,
                labels_function: None,
            });
        }

        // ADR-0191 (Decision point 7's follow-up probe): Neo4j 2026.09
        // accepts every keyword spelling as a SET target variable
        // (`SET MATCH.prop = 1`, `SET MATCH:Label`, `SET MATCH += {...}`),
        // so this reads through the shared symbolic-name read like every
        // other variable slot.
        let name = self.parse_schema_name("identifier in SET")?;

        // CIP-131: `SET n.prop = ...` / `SET n[<expr>] = ...`, including
        // chained accessors (`n[a][b]`, `n[a].b`, ...) where all but the
        // last accessor navigate to the entity being mutated.
        let accessors = self.parse_property_target_accessors()?;
        if !accessors.is_empty() {
            let prop_access = Self::build_property_access(Expr::Variable(name), accessors);
            self.expect(TokenType::Equals)?;
            let val = self.parse_expression()?;
            return Ok(SetItem {
                property: Some(prop_access),
                variable: None,
                labels: Vec::new(),
                value: Some(val),
                merge: false,
                is_keyword: false,
                labels_function: None,
            });
        }

        let next = self.peek().clone();
        match next.ty {
            TokenType::Colon | TokenType::IS => {
                let (labels, is_keyword) = self.parse_label_set_list()?;
                Ok(SetItem {
                    property: None,
                    variable: Some(name),
                    labels,
                    value: None,
                    merge: false,
                    is_keyword,
                    labels_function: None,
                })
            }
            TokenType::Equals => {
                self.next_token();
                let val = self.parse_expression()?;
                Ok(SetItem {
                    property: None,
                    variable: Some(name),
                    labels: Vec::new(),
                    value: Some(val),
                    merge: false,
                    is_keyword: false,
                    labels_function: None,
                })
            }
            TokenType::Plus => {
                self.next_token(); // consume +
                self.expect(TokenType::Equals)?;
                let val = self.parse_expression()?;
                Ok(SetItem {
                    property: None,
                    variable: Some(name),
                    labels: Vec::new(),
                    value: Some(val),
                    merge: true,
                    is_keyword: false,
                    labels_function: None,
                })
            }
            _ => Err(self.error_at_token(
                &next,
                format!("unexpected token {:?} in SET clause", next.literal),
            )),
        }
    }

    /// Parses a SET/REMOVE label-set item's labels after the variable
    /// (CIP-126): `:A` / `:A:B:C` (colon, chainable) or `IS A` (IS keyword,
    /// single label only). Each label atom may also be a CIP-162 dynamic
    /// label (`$(<expr>)`/`$all(<expr>)`/`$any(<expr>)`), chainable the same
    /// way (`n:$(a):$(b)`, `n IS $(a)`). Label-expression symbols (`&`, `|`,
    /// `!`, `%`) are never allowed here, and `IS` cannot be chained with `:`
    /// within one item. Returns the flat label-atom list and whether `IS`
    /// was used (for the CIP-40/CIP-126 statement-level IS/colon-conjunction
    /// mixing check).
    pub(super) fn parse_label_set_list(&mut self) -> ParseResult<(Vec<LabelExpr>, bool)> {
        let via_is = self.peek_type() == TokenType::IS;
        self.next_token(); // consume ':' or 'IS'
        if via_is && self.peek_is_labeled_keyword() {
            return Err(self.error(
                "invalid label specification context: IS LABELED is a predicate, not a label specification",
            ));
        }
        self.reject_label_spec_symbols()?;
        let mut labels = vec![self.parse_label_atom()?];
        if via_is {
            if self.peek_type() == TokenType::Colon {
                return Err(self.error(
                    "MixedIsAndColonConjunction: cannot mix IS and colon-conjunction within the same label item",
                ));
            }
        } else {
            while self.peek_type() == TokenType::Colon {
                self.next_token();
                self.reject_label_spec_symbols()?;
                labels.push(self.parse_label_atom()?);
            }
        }
        self.reject_label_spec_symbols()?;
        Ok((labels, via_is))
    }

    /// Fails if the current token is a label-expression symbol, which a
    /// SET/REMOVE label specification never allows (see
    /// `parse_label_set_list`).
    pub(super) fn reject_label_spec_symbols(&self) -> ParseResult<()> {
        if matches!(
            self.peek_type(),
            TokenType::Ampersand | TokenType::Pipe | TokenType::Bang | TokenType::Percent
        ) {
            return Err(self.error(
                "InvalidLabelExpression: label expression symbols (&, |, !, %) are not allowed in a SET/REMOVE label specification",
            ));
        }
        Ok(())
    }

    pub(super) fn parse_remove(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume REMOVE
        let mut items = Vec::new();
        loop {
            items.push(self.parse_remove_item()?);
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        Ok(Clause::Remove(RemoveClause { items }))
    }

    pub(super) fn parse_remove_item(&mut self) -> ParseResult<RemoveItem> {
        // Handle a parenthesized expression property target (CIP-118 static
        // `.prop`, CIP-131 dynamic `[<expr>]`): `REMOVE (<expr>).prop` /
        // `REMOVE (<expr>)[<expr>]`, e.g. `REMOVE (COLLECT {...}[0]).prop`.
        // Per the CIP's revision history, label removal still requires a
        // plain variable -- only property removal accepts a general
        // expression -- so `(expr):Label` is intentionally not accepted
        // here (falls through to a syntax error).
        if self.peek_type() == TokenType::LeftParen {
            self.next_token(); // consume (
            let inner_expr = self.parse_expression()?;
            self.expect(TokenType::RightParen)?;
            let accessors = self.parse_property_target_accessors()?;
            if accessors.is_empty() {
                let next = self.peek().clone();
                return Err(self.error_at_token(
                    &next,
                    format!(
                        "expected '.' or '[' after parenthesized expression in REMOVE, got {:?}",
                        next.literal
                    ),
                ));
            }
            return Ok(RemoveItem {
                property: Some(Self::build_property_access(inner_expr, accessors)),
                variable: None,
                labels: Vec::new(),
                is_keyword: false,
            });
        }

        // CIP-131: an unparenthesized complex target expression, e.g.
        // `REMOVE coalesce(n, m)[c]` (see `parse_set_item`'s matching
        // comment).
        if self.peek_type() == TokenType::Identifier && self.peek_at(1).ty == TokenType::LeftParen {
            let prop_access = self.parse_general_property_target()?;
            return Ok(RemoveItem {
                property: Some(prop_access),
                variable: None,
                labels: Vec::new(),
                is_keyword: false,
            });
        }

        // ADR-0191 (Decision point 7's follow-up probe): Neo4j 2026.09
        // accepts every keyword spelling as a REMOVE target variable
        // (`REMOVE MATCH.prop`, `REMOVE MATCH:Label`), matching
        // `parse_set_item`'s equivalent read.
        let name = self.parse_schema_name("identifier in REMOVE")?;

        // CIP-131: `REMOVE n.prop` / `REMOVE n[<expr>]`, including chained
        // accessors (see `parse_set_item`'s matching comment).
        let accessors = self.parse_property_target_accessors()?;
        if !accessors.is_empty() {
            let prop_access = Self::build_property_access(Expr::Variable(name), accessors);
            return Ok(RemoveItem {
                property: Some(prop_access),
                variable: None,
                labels: Vec::new(),
                is_keyword: false,
            });
        }

        let next = self.peek().clone();
        match next.ty {
            TokenType::Colon | TokenType::IS => {
                let (labels, is_keyword) = self.parse_label_set_list()?;
                Ok(RemoveItem {
                    property: None,
                    variable: Some(name),
                    labels,
                    is_keyword,
                })
            }
            _ => Err(self.error_at_token(
                &next,
                format!(
                    "expected . or : after identifier in REMOVE, got {:?}",
                    next.literal
                ),
            )),
        }
    }

    /// CIP-199: consumes an optional `ALL` projection modifier directly
    /// after `RETURN`/`WITH` (explicit non-DISTINCT; semantically identical
    /// to omitting it -- `distinct: false` is already the default this is
    /// called from). Only fires when `ALL` is *not* immediately followed by
    /// `(`, since `ALL(` starts the pre-existing `all(x IN ... WHERE ...)`
    /// quantifier expression (parsed as the first return item, e.g.
    /// `RETURN all(x IN list WHERE x > 1)`) -- that usage must be left
    /// completely untouched. `RETURN ALL DISTINCT ...` deliberately isn't
    /// special-cased here: after consuming `ALL`, `DISTINCT` is not a valid
    /// return item, so `parse_return_items` naturally raises the syntax
    /// error the CIP calls for (ALL and DISTINCT are mutually exclusive).
    pub(super) fn consume_return_all_modifier(&mut self) {
        if self.peek_type() == TokenType::ALL && self.peek_at(1).ty != TokenType::LeftParen {
            self.next_token();
        }
    }

    pub(super) fn parse_return(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume RETURN
        let distinct = if self.peek_type() == TokenType::DISTINCT {
            self.next_token();
            true
        } else {
            self.consume_return_all_modifier();
            false
        };

        let items = self.parse_return_items()?;

        // CIP-236: the GROUP BY subclause sits between the return items and
        // the order-by/page clauses.
        let group_by = self.parse_group_by()?;

        let order_by = if self.peek_type() == TokenType::ORDER {
            self.next_token();
            self.expect(TokenType::BY)?;
            self.parse_order_by()?
        } else {
            Vec::new()
        };

        let skip = if self.peek_type() == TokenType::SKIP || self.peek_is_offset() {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let limit = if self.peek_type() == TokenType::LIMIT {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        Ok(Clause::Return(ReturnClause {
            group_by,
            distinct,
            items,
            order_by,
            skip,
            limit,
        }))
    }

    pub(super) fn parse_with(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume WITH
        let distinct = if self.peek_type() == TokenType::DISTINCT {
            self.next_token();
            true
        } else {
            self.consume_return_all_modifier();
            false
        };

        let items = self.parse_return_items()?;

        // CIP-236 (canonical order: items, GROUP BY, order/page, WHERE).
        let group_by = self.parse_group_by()?;

        let order_by = if self.peek_type() == TokenType::ORDER {
            self.next_token();
            self.expect(TokenType::BY)?;
            self.parse_order_by()?
        } else {
            Vec::new()
        };

        let skip = if self.peek_type() == TokenType::SKIP || self.peek_is_offset() {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let limit = if self.peek_type() == TokenType::LIMIT {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        let where_expr = if self.peek_type() == TokenType::WHERE {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        Ok(Clause::With(WithClause {
            group_by,
            distinct,
            items,
            order_by,
            skip,
            limit,
            where_expr,
            origin: WithOrigin::Explicit,
        }))
    }

    /// CIP-236 `<group by clause>`: `GROUP BY <expr>...`, `GROUP BY ()`
    /// (empty grouping set), or `GROUP BY ALL`. `None` when the next token
    /// is not `GROUP`.
    pub(super) fn parse_group_by(&mut self) -> ParseResult<Option<GroupBySpec>> {
        if !self.peek_is_ident("GROUP") {
            return Ok(None);
        }
        self.next_token();
        // BY is a keyword token (ORDER BY), not an identifier.
        self.expect(TokenType::BY)?;
        // `()` is the empty grouping set only when it stands alone as the
        // clause tail -- `GROUP BY (expr)` is a parenthesized key, so require
        // the `()` pair *and* a clause/end token right after it.
        if self.peek_type() == TokenType::LeftParen
            && self.peek_at(1).ty == TokenType::RightParen
            && self.group_by_ends_clause(2)
        {
            self.next_token();
            self.next_token();
            return Ok(Some(GroupBySpec::Empty));
        }
        // ALL is a keyword token (RETURN ALL), not an identifier. Only when
        // it is *not* immediately followed by `(`, since `ALL(` is a function
        // call named `all` (an ordinary expression). Same rule as
        // `consume_return_all_modifier`.
        if self.peek_type() == TokenType::ALL && self.peek_at(1).ty != TokenType::LeftParen {
            self.next_token();
            return Ok(Some(GroupBySpec::All));
        }
        let mut keys = vec![self.parse_expression()?];
        while self.peek_type() == TokenType::Comma {
            self.next_token();
            keys.push(self.parse_expression()?);
        }
        Ok(Some(GroupBySpec::Keys(keys)))
    }

    /// True when the token `offset` ahead ends a `WITH`/`RETURN` clause (or
    /// the statement): nothing that could continue a grouping-key expression
    /// list follows. Disambiguates `GROUP BY ()` (the empty grouping set)
    /// from a parenthesized key expression.
    ///
    /// The clause-starter half of the vocabulary is the one shared table
    /// `CLAUSE_STARTER_TOKENS` (token.rs; consulted here instead of a
    /// second hand-maintained list, after the round-5 LOAD/DROP drift): a
    /// new clause-start dispatch arm in `parse_clause_inner` extends that
    /// one table and this walk sees it. The table's dispatch coverage is
    /// pinned by test (`clause_starter_table_covers_the_dispatch`).
    pub(super) fn group_by_ends_clause(&self, offset: usize) -> bool {
        matches!(
            self.peek_at(offset).ty,
            TokenType::EOF
                | TokenType::Semicolon
                | TokenType::RightParen
                | TokenType::RightBrace
                | TokenType::ORDER
                | TokenType::SKIP
                | TokenType::LIMIT
                | TokenType::WHERE
                | TokenType::RETURN
                | TokenType::WITH
        ) || CLAUSE_STARTER_TOKENS.contains(&self.peek_at(offset).ty)
    }

    pub(super) fn parse_unwind(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume UNWIND
        let expr = self.parse_expression()?;
        self.expect(TokenType::AS)?;
        let alias = self.parse_alias_after_as()?;
        Ok(Clause::Unwind(UnwindClause {
            expr,
            alias,
            for_syntax: false,
        }))
    }

    /// CIP-250 (GQ10): `FOR <binding variable> IN <expression>` -- the
    /// GQL-compliant successor spelling of `UNWIND <expression> AS
    /// <binding variable>`, with the binding variable *before* the
    /// expression. Normalized here into the same [`Clause::Unwind`] the
    /// UNWIND parse produces (`for_syntax: true`), so the clause validates,
    /// lowers, plans and executes identically to UNWIND -- the CIP's own
    /// "no new logical plan operators" rule. UNWIND is not deprecated; the
    /// two spellings coexist. The ADR-0371 `gql-for` preview gate (the
    /// front-end's statement admission) fails this spelling closed unless
    /// enabled.
    pub(super) fn parse_for_clause(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume FOR
                           // The binding variable is a symbolic name, so any keyword spelling
                           // works unquoted too (ADR-0191), exactly like the FOREACH loop
                           // variable and the UNWIND alias.
        let alias = self.parse_schema_name("binding variable in FOR")?;
        self.expect(TokenType::IN)?;
        let expr = self.parse_expression()?;
        Ok(Clause::Unwind(UnwindClause {
            expr,
            alias,
            for_syntax: true,
        }))
    }

    pub(super) fn parse_foreach(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume FOREACH
        self.expect(TokenType::LeftParen)?;
        // The loop variable is a symbolic name, so any keyword spelling
        // works unquoted too (ADR-0191).
        let variable = self.parse_schema_name("variable in FOREACH")?;
        self.expect(TokenType::IN)?;
        let list_expr = self.parse_expression()?;
        self.expect(TokenType::Pipe)?;

        let mut clauses = Vec::new();
        while self.peek_type() != TokenType::RightParen {
            clauses.push(self.parse_clause()?);
        }
        self.next_token(); // consume )

        Ok(Clause::Foreach(ForeachClause {
            variable,
            list_expr,
            clauses,
        }))
    }

    pub(super) fn parse_load_csv(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume LOAD
        let csv_tok = self.next_token();
        if !csv_tok.literal.eq_ignore_ascii_case("CSV") {
            return Err(self.error_at_token(
                &csv_tok,
                format!("expected CSV after LOAD, got {:?}", csv_tok.literal),
            ));
        }

        let with_headers = if self.peek_type() == TokenType::WITH {
            self.next_token();
            let headers_tok = self.next_token();
            if !headers_tok.literal.eq_ignore_ascii_case("HEADERS") {
                return Err(self.error_at_token(
                    &headers_tok,
                    format!("expected HEADERS after WITH, got {:?}", headers_tok.literal),
                ));
            }
            true
        } else {
            false
        };

        self.expect(TokenType::FROM)?;
        let source = self.parse_expression()?;
        self.expect(TokenType::AS)?;
        let alias = self.parse_alias_after_as()?;

        let field_terminator = if self.peek_type() == TokenType::Identifier
            && self.peek().literal.eq_ignore_ascii_case("FIELDTERMINATOR")
        {
            self.next_token();
            let ft_tok = self.next_token();
            if ft_tok.ty != TokenType::String {
                return Err(self.error_at_token(
                    &ft_tok,
                    format!(
                        "expected a string literal after FIELDTERMINATOR, got {:?}",
                        ft_tok.literal
                    ),
                ));
            }
            Some(ft_tok.literal)
        } else {
            None
        };

        Ok(Clause::LoadCSV(LoadCSVClause {
            with_headers,
            source,
            alias,
            field_terminator,
        }))
    }

    pub(super) fn parse_call(&mut self, optional: bool) -> ParseResult<Clause> {
        self.next_token(); // consume CALL

        // CIP-141 `CALL (...) { ... }` variable scope clause.
        if self.peek_type() == TokenType::LeftParen {
            let scope = self.parse_call_scope()?;
            self.expect(TokenType::LeftBrace)?;
            let stmt = self.parse_statement_block()?;
            self.expect(TokenType::RightBrace)?;
            let in_transactions = self.parse_in_transactions()?;
            return Ok(Clause::CallSubquery(CallSubqueryClause {
                statement: stmt,
                scope,
                optional,
                in_transactions,
            }));
        }

        // CALL { ... } subquery (naked, no scope clause -- deprecated)
        if self.peek_type() == TokenType::LeftBrace {
            self.next_token(); // consume {
            let stmt = self.parse_statement_block()?;
            self.expect(TokenType::RightBrace)?;
            // CIP-213 Rule 4: NEXT cannot be used inside a CALL subquery
            // that uses the (deprecated) importing WITH syntax -- a naked
            // `CALL { ... }` (no scope-clause parens) whose body's very
            // first clause is a `WITH` (the legacy "import these outer
            // variables" idiom) cannot also chain a NEXT stage.
            if stmt.next.is_some() && matches!(stmt.clauses.first(), Some(Clause::With(_))) {
                return Err(self.error(
                    "InvalidClauseComposition: 42N3C invalid use of 'CALL { ... }'. Not possible to enclose 'NEXT' in 'CALL { ... }'. Use 'CALL () { ... }' instead.",
                ));
            }
            let in_transactions = self.parse_in_transactions()?;
            return Ok(Clause::CallSubquery(CallSubqueryClause {
                statement: stmt,
                scope: CallScope::None,
                optional,
                in_transactions,
            }));
        }

        // CALL db.procedure(...) YIELD ...
        let mut name_parts = Vec::new();
        let tok = self.next_token();
        if tok.ty != TokenType::Identifier {
            return Err(self.error_at_token(
                &tok,
                format!("expected procedure name after CALL, got {:?}", tok.literal),
            ));
        }
        name_parts.push(tok.literal);
        while self.peek_type() == TokenType::Dot {
            self.next_token(); // consume .
            let next = self.next_token();
            // `is_schema_name`, not `is_ident_like`: after `CALL name.` the
            // grammar requires a name segment, so no keyword there can
            // start a clause — and procedure names may carry reserved words
            // as segments (`db.ontology.set`, `db.ontology.drop`;
            // ADR-0121/ADR-0227), as Neo4j's symbolic-name rule allows.
            if !next.ty.is_schema_name() {
                return Err(self.error_at_token(
                    &next,
                    format!(
                        "expected identifier in procedure name, got {:?}",
                        next.literal
                    ),
                ));
            }
            name_parts.push(next.literal);
        }
        let proc_name = name_parts.join(".");

        let explicit_args = self.peek_type() == TokenType::LeftParen;
        let args = if explicit_args {
            self.next_token(); // consume (
            self.parse_arg_list()?
        } else {
            Vec::new()
        };

        let yields = self.parse_show_yield_list()?;
        let saw_yield = yields.present();

        // CIP-159: `WHERE` inside a `YIELD` clause is part of the
        // `<procedure call>` production -- filtering the procedure's own
        // result rows (correlated to the outer row) before they're merged
        // in, distinct from a separate `WHERE` clause that would follow.
        let where_expr = if saw_yield && self.peek_type() == TokenType::WHERE {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        Ok(Clause::CallProcedure(CallProcedureClause {
            procedure: proc_name,
            args,
            explicit_args,
            yields,
            optional,
            where_expr,
        }))
    }

    /// `<scope clause> ::= "(" ")" | "(" "*" ")" | "(" <var> ("," <var>)* ")"`
    /// -- the CIP-141 `CALL (...)` variable scope clause. The leading `(`
    /// has not been consumed yet.
    pub(super) fn parse_call_scope(&mut self) -> ParseResult<CallScope> {
        self.next_token(); // consume (

        if self.peek_type() == TokenType::RightParen {
            self.next_token();
            return Ok(CallScope::Vars(Vec::new()));
        }

        if self.peek_type() == TokenType::Star {
            self.next_token();
            if self.peek_type() == TokenType::Comma {
                return Err(self.error(
                    "InvalidSyntax: a wildcard scope clause cannot be followed by further variable references",
                ));
            }
            self.expect(TokenType::RightParen)?;
            return Ok(CallScope::Wildcard);
        }

        let mut vars = Vec::new();
        loop {
            if self.peek_type() == TokenType::Star {
                return Err(self.error(
                    "InvalidSyntax: a wildcard cannot follow variable references in a scope clause",
                ));
            }
            // The import list names variables already in scope; the name
            // is a symbolic name, so any keyword spelling works unquoted
            // too (ADR-0191).
            let var = self.parse_schema_name("a variable reference in the CALL scope clause")?;
            if self.peek_type() == TokenType::AS {
                return Err(self.error(
                    "VariableDeclarationForbidden: a CALL scope clause may not contain a variable declaration",
                ));
            }
            vars.push(var);
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        self.expect(TokenType::RightParen)?;
        Ok(CallScope::Vars(vars))
    }

    /// CIP-32/CIP-34/CIP-198/CIP-30: `IN [<concExpr>] CONCURRENT?
    /// TRANSACTIONS [OF <n> ROW|ROWS] [ON ERROR ...] [REPORT STATUS AS
    /// <var>]`, trailing a `CALL { ... }` / `CALL (...) { ... }`
    /// subquery's closing brace. `TRANSACTIONS`/`CONCURRENT`/`OF`/`ROW`/
    /// `ROWS`/`ERROR`/`BREAK`/`CONTINUE`/`FAIL`/`RETRY`/`REPORT`/`STATUS`/
    /// `SECOND(S)`/`SEC` are all contextual identifiers matched
    /// case-insensitively, not reserved keywords -- `IN`/`ON`/`FOR`/
    /// `THEN`/`AS` are the only real tokens reused here (reserving new
    /// keywords for the rest would regress the TCK). Returns `None` when
    /// the clause has no `IN TRANSACTIONS` trailer at all (an ordinary
    /// CALL subquery). Only CIP-32's own base semantics (sequential
    /// batching, nesting/preceding-write restrictions) are enforced by the
    /// engine today; concurrency, retry, and status/error output are
    /// parsed into the AST here so later CIPs only need to add
    /// validation/execution on top of it.
    pub(super) fn parse_in_transactions(&mut self) -> ParseResult<Option<InTransactionsClause>> {
        if self.peek_type() != TokenType::IN {
            return Ok(None);
        }
        self.next_token(); // consume IN

        let concurrency = if self.peek_is_ident("transactions") {
            Concurrency::Sequential
        } else if self.peek_is_ident("concurrent") {
            self.next_token(); // consume CONCURRENT
            Concurrency::DefaultUnspecified
        } else {
            let expr = self.parse_expression()?;
            if !self.peek_is_ident("concurrent") {
                return Err(self.error(format!(
                    "expected CONCURRENT after the concurrency expression in 'IN ... TRANSACTIONS', got {:?}",
                    self.peek().literal
                )));
            }
            self.next_token(); // consume CONCURRENT
            Concurrency::Value(expr)
        };

        if !self.peek_is_ident("transactions") {
            return Err(self.error(format!(
                "expected TRANSACTIONS, got {:?}",
                self.peek().literal
            )));
        }
        self.next_token(); // consume TRANSACTIONS

        // CIP-30/CIP-34 verified examples (syntax [1]-[4]/[11]/[12],
        // concurrent-error-handling[7]): `OF <n> ROW|ROWS`, `ON ERROR ...`,
        // and `REPORT STATUS AS s` may appear in any of their 6
        // permutations, so all three are parsed in one loop rather than
        // `OF` being a fixed leading production.
        let mut batch_size: Option<Expr> = None;
        let mut on_error: Option<OnErrorMode> = None;
        let mut report_status: Option<String> = None;
        loop {
            if batch_size.is_none() && self.peek_is_ident("of") {
                self.next_token(); // consume OF
                let n = self.parse_expression()?;
                if self.peek_is_ident("row") || self.peek_is_ident("rows") {
                    self.next_token();
                } else {
                    return Err(self.error(format!(
                        "expected ROW or ROWS after 'OF <n>', got {:?}",
                        self.peek().literal
                    )));
                }
                batch_size = Some(n);
                continue;
            }
            if on_error.is_none() && self.peek_type() == TokenType::ON {
                self.next_token(); // consume ON
                if !self.peek_is_ident("error") {
                    return Err(self.error(format!(
                        "expected ERROR after ON, got {:?}",
                        self.peek().literal
                    )));
                }
                self.next_token(); // consume ERROR
                on_error = Some(if self.peek_is_ident("continue") {
                    self.next_token();
                    OnErrorMode::Continue
                } else if self.peek_is_ident("break") {
                    self.next_token();
                    OnErrorMode::Break
                } else if self.peek_is_ident("fail") {
                    self.next_token();
                    OnErrorMode::Fail
                } else if self.peek_is_ident("retry") {
                    self.next_token(); // consume RETRY
                    self.parse_on_error_retry()?
                } else {
                    return Err(self.error(format!(
                        "expected CONTINUE, BREAK, FAIL, or RETRY after ON ERROR, got {:?}",
                        self.peek().literal
                    )));
                });
                continue;
            }
            if report_status.is_none() && self.peek_is_ident("report") {
                self.next_token(); // consume REPORT
                if !self.peek_is_ident("status") {
                    return Err(self.error(format!(
                        "expected STATUS after REPORT, got {:?}",
                        self.peek().literal
                    )));
                }
                self.next_token(); // consume STATUS
                self.expect(TokenType::AS)?;
                // ADR-0191 (Decision point 7's follow-up probe): Neo4j
                // 2026.09 accepts every keyword spelling as the REPORT
                // STATUS AS variable, so this reads through the shared
                // symbolic-name read like every other alias slot.
                let var = self.parse_schema_name("a variable name after REPORT STATUS AS")?;
                report_status = Some(var);
                continue;
            }
            break;
        }
        let on_error = on_error.unwrap_or(OnErrorMode::Fail);

        Ok(Some(InTransactionsClause {
            concurrency,
            batch_size,
            on_error,
            report_status,
        }))
    }

    /// CIP-198 `RETRY [FOR]? [<dur> SEC|SECOND|SECONDS]? [THEN
    /// CONTINUE|BREAK|FAIL]?` -- the `RETRY` keyword itself has already
    /// been consumed. Whether a duration follows at all is genuinely
    /// ambiguous without `FOR` (`RETRY` alone, immediately followed by
    /// `THEN`/`REPORT`/the next clause, is valid), so the duration is
    /// parsed speculatively when `FOR` is absent: try an expression
    /// followed by a `SEC`/`SECOND`/`SECONDS` unit, and roll back to try
    /// `THEN`/end-of-clause instead if that doesn't pan out.
    pub(super) fn parse_on_error_retry(&mut self) -> ParseResult<OnErrorMode> {
        // `FOR` is a real reserved token (reused from e.g. `FOREACH`'s
        // desugaring elsewhere), not a contextual identifier.
        let has_for = self.peek_type() == TokenType::FOR;
        if has_for {
            self.next_token(); // consume FOR
        }
        let checkpoint = self.save_pos();
        let mut duration = None;
        let parsed_duration = self.parse_expression().ok().filter(|_| {
            self.peek_is_ident("sec")
                || self.peek_is_ident("second")
                || self.peek_is_ident("seconds")
        });
        if let Some(dur_expr) = parsed_duration {
            self.next_token(); // consume SEC/SECOND/SECONDS
            duration = Some(dur_expr);
        } else {
            self.restore_pos(checkpoint);
            if has_for {
                return Err(self.error(format!(
                    "expected a duration followed by SEC/SECOND/SECONDS after RETRY FOR, got {:?}",
                    self.peek().literal
                )));
            }
        }

        let fallback = if self.peek_type() == TokenType::THEN {
            self.next_token(); // consume THEN
            if self.peek_is_ident("continue") {
                self.next_token();
                RetryFallback::Continue
            } else if self.peek_is_ident("break") {
                self.next_token();
                RetryFallback::Break
            } else if self.peek_is_ident("fail") {
                self.next_token();
                RetryFallback::Fail
            } else {
                return Err(self.error(format!(
                    "expected CONTINUE, BREAK, or FAIL after THEN, got {:?}",
                    self.peek().literal
                )));
            }
        } else {
            // CIP-198 deviation: a bare RETRY (or RETRY FOR <dur>) with no
            // THEN clause falls back to FAIL, not CONTINUE.
            RetryFallback::Fail
        };

        Ok(OnErrorMode::Retry { duration, fallback })
    }

    /// CIP-168: `OFFSET` is a synonym for `SKIP`, usable everywhere SKIP
    /// is (standalone paging clause, WITH, RETURN, YIELD). Not a reserved
    /// keyword -- recognized by spelling, same technique as `SHOW`/`USE`,
    /// so existing uses like the property access `d.offset` (TCK
    /// Temporal5) keep working.
    pub(super) fn peek_is_offset(&self) -> bool {
        let tok = self.peek();
        tok.ty == TokenType::Identifier && tok.literal.eq_ignore_ascii_case("offset")
    }

    /// `USE <graph-expr>` (CIP-141 feature-interaction scenarios). `USE`
    /// is not a reserved keyword -- recognized by spelling at clause
    /// position, same technique as `SHOW`.
    pub(super) fn parse_use(&mut self) -> ParseResult<UseClause> {
        self.next_token(); // consume USE
        let graph_expr = self.parse_expression()?;
        Ok(UseClause { graph_expr })
    }

    /// `LET <var> = <expr> ("," <var> = <expr>)*` (CIP-206) -- sugar for
    /// `WITH *, <expr> AS <var>, ...`, desugared directly into a
    /// `WithClause` (tagged `WithOrigin::Let`) so it reuses the engine's
    /// existing WITH execution and scope-analysis machinery; the extra
    /// restrictions LET has that plain `WITH *` doesn't (no shadowing, no
    /// sibling/forward/self reference, no aggregation) are enforced on the
    /// lowered IR by `cypherlite-planner`'s binding pass (the
    /// `VariableAlreadyBound`/`InvalidAggregation` checks there), keyed on
    /// that tag.
    pub(super) fn parse_let(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume LET
        let mut items = vec![ReturnItem {
            expr: Expr::Star,
            alias: None,
            all: true,
            source_text: None,
        }];
        loop {
            // LET's grammar has no modifier position after the keyword
            // (unlike `WITH DISTINCT`/`RETURN ... AS`), so accepting any
            // word-shaped token -- including normally-reserved ones like
            // DISTINCT/AS -- as the binding variable is unambiguous.
            // CIP-206 PE6.1/PE6.2 rely on exactly this (`LET DISTINCT = 1`,
            // `LET AS = 1` are both valid).
            let name = self.parse_schema_name("a variable name after LET")?;
            self.expect(TokenType::Equals)?;
            let expr = self.parse_expression()?;
            items.push(ReturnItem {
                expr,
                alias: Some(name),
                all: false,
                source_text: None,
            });
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        Ok(Clause::With(WithClause {
            group_by: None,
            distinct: false,
            items,
            order_by: Vec::new(),
            skip: None,
            limit: None,
            where_expr: None,
            origin: WithOrigin::Let,
        }))
    }

    /// `FILTER [WHERE] <search condition>` (CIP-203) -- sugar for `WITH *
    /// WHERE <search condition>`, desugared directly into a `WithClause`
    /// (tagged `WithOrigin::Filter`) for the same reason `LET` is: reuse
    /// WITH's execution/scope machinery, with the extra Boolean-declared-
    /// type check (Rule 3) enforced on the lowered IR by
    /// `cypherlite-planner`'s predicate binding (`check_pred`: a statically
    /// non-Boolean search condition is `InvalidArgumentType`). `FILTER` is a
    /// non-reserved word -- recognized by spelling at clause position,
    /// same technique as `LET`/`FINISH`/`SHOW`/`USE`. A bare `WHERE` with
    /// no `FILTER` keyword (NE1) is already rejected: `WHERE` has no
    /// clause-start dispatch of its own.
    pub(super) fn parse_filter(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume FILTER
        if self.peek_type() == TokenType::WHERE {
            self.next_token();
        }
        let where_expr = self.parse_expression()?;
        Ok(Clause::With(WithClause {
            group_by: None,
            distinct: false,
            items: vec![ReturnItem {
                expr: Expr::Star,
                alias: None,
                all: true,
                source_text: None,
            }],
            order_by: Vec::new(),
            skip: None,
            limit: None,
            where_expr: Some(where_expr),
            origin: WithOrigin::Filter,
        }))
    }

    /// The `ORDER BY`/`SKIP`/`LIMIT` paging sub-grammar: loops consuming
    /// one stage per iteration until none of the three introducer tokens
    /// are next, so `LIMIT 3 OFFSET 1` and `ORDER BY x SKIP 1 LIMIT 2`
    /// both parse fully, in any combination and (per Neo4j's observed
    /// behavior, not the CIP's own fixed grammar order -- see `PageStage`)
    /// any order. Shared by the standalone paging clause (CIP-168,
    /// `parse_standalone_order_by_page`) and the SHOW family's YIELD
    /// paging (CIP-46 con[15]/CIP-74 filtering[8]/[9]) -- the one place
    /// the SKIP/OFFSET policy is decided: OFFSET is accepted everywhere
    /// SKIP is (`peek_is_offset`, CIP-168), including inside SHOW paging.
    pub(super) fn parse_page_stages(&mut self) -> ParseResult<Vec<PageStage>> {
        let mut stages = Vec::new();
        loop {
            if self.peek_type() == TokenType::ORDER {
                self.next_token();
                self.expect(TokenType::BY)?;
                stages.push(PageStage::OrderBy(self.parse_order_by()?));
            } else if self.peek_type() == TokenType::SKIP || self.peek_is_offset() {
                self.next_token();
                stages.push(PageStage::Skip(self.parse_expression()?));
            } else if self.peek_type() == TokenType::LIMIT {
                self.next_token();
                stages.push(PageStage::Limit(self.parse_expression()?));
            } else {
                break;
            }
        }
        Ok(stages)
    }

    /// CIP-168: a standalone `<order by and page statement>` at clause
    /// start -- `ORDER BY ...`, `SKIP|OFFSET ...`, or `LIMIT ...`; the
    /// stages themselves are `parse_page_stages`.
    pub(super) fn parse_standalone_order_by_page(&mut self) -> ParseResult<Clause> {
        let stages = self.parse_page_stages()?;
        Ok(Clause::StandaloneOrderBy(StandaloneOrderByClause {
            stages,
        }))
    }

    /// `FINISH` (CIP-140) -- a terminal clause. `FINISH` is not a reserved
    /// keyword, recognized by spelling at clause position like `SHOW`.
    pub(super) fn parse_finish(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume FINISH
        Ok(Clause::Finish)
    }

    // ---- Return items / Order by ----

    pub(super) fn parse_return_items(&mut self) -> ParseResult<Vec<ReturnItem>> {
        let mut items = Vec::new();
        loop {
            if self.peek_type() == TokenType::Star {
                self.next_token();
                items.push(ReturnItem {
                    expr: Expr::Star,
                    alias: None,
                    all: true,
                    source_text: None,
                });
                // CIP-101/CIP-118 usage: `WITH *, expr AS x` / `RETURN *,
                // expr` -- the star may be followed by further projection
                // items, not just terminate the list.
                if self.peek_type() != TokenType::Comma {
                    return Ok(items);
                }
                self.next_token();
                continue;
            }

            let start_pos = self.save_pos();
            let expr = self.parse_expression()?;
            let end_pos = self.save_pos();

            // Extract source text
            let source_text = self.extract_source_text(start_pos, end_pos);

            let alias = if self.peek_type() == TokenType::AS {
                self.next_token();
                // `RETURN 1 AS as` and `RETURN 1 AS union` included.
                Some(self.parse_alias_after_as()?)
            } else {
                None
            };

            items.push(ReturnItem {
                expr,
                alias,
                all: false,
                source_text,
            });

            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        Ok(items)
    }

    /// ADR-0116 §2: the parser recognizes every database administration
    /// command Neo4j CE rejects, so the engine answers with CE's
    /// `UnsupportedAdministrationCommand` (51N27) rather than a syntax
    /// error. Consumes the rest of the statement and fails the parse with a
    /// [`ParseError`] tagged [`UNSUPPORTED_ADMINISTRATION_DETAIL`] whose
    /// message is the verbatim statement text (sliced from `start_pos`, the
    /// command's first token, so any leading `CYPHER n` preamble is
    /// excluded) -- fail-closed: an unsupported administration command
    /// never produces an executable clause. `cypherlite-front` maps this
    /// parse error to CE's semantic error text.
    pub(super) fn unsupported_admin_clause(&mut self, start_pos: usize) -> ParseResult<Clause> {
        while !matches!(self.peek_type(), TokenType::EOF | TokenType::Semicolon) {
            self.next_token();
        }
        // Internal invariant, not user input: `start_pos` is the command's
        // first token, saved by the caller before any of the command was
        // consumed, so it always indexes a real token and the source slice
        // from it to the current position always exists. A violation is a
        // parser bug -- assert loudly instead of masking it with a fallback.
        let text = self.extract_source_text(start_pos, self.pos).expect(
            "invariant: unsupported_admin_clause start_pos indexes the command's first token, \
             so its source slice always exists",
        );
        let tok = &self.tokens[start_pos];
        Err(ParseError::new(text, tok.line, tok.col)
            .with_source(&self.source)
            .with_detail(UNSUPPORTED_ADMINISTRATION_DETAIL))
    }

    pub(super) fn extract_source_text(&self, start_pos: usize, end_pos: usize) -> Option<String> {
        if start_pos >= self.tokens.len() || end_pos == 0 {
            return None;
        }
        let start_tok = &self.tokens[start_pos];
        let start_offset = self.token_byte_offset(start_tok)?;

        let end_offset = if end_pos < self.tokens.len() && self.tokens[end_pos].ty != TokenType::EOF
        {
            self.token_byte_offset(&self.tokens[end_pos])
        } else {
            // Expression runs to end of input: the EOF token's position never
            // matches a source character, and token literals exclude quote
            // characters, so the only reliable end is the source length.
            Some(self.source.len())
        };

        match end_offset {
            Some(end) if end > start_offset => {
                Some(self.source[start_offset..end].trim().to_string())
            }
            _ => None,
        }
    }

    pub(super) fn token_byte_offset(&self, tok: &Token) -> Option<usize> {
        // The lexer counts columns in bytes (`Lexer::advance` does
        // `col += 1` per byte), so `line_start + col - 1` is exactly the
        // token's byte offset — no per-token source rescan, and no
        // char/byte-column mismatch after non-ASCII text.
        let line_idx = tok.line.checked_sub(1)?;
        let col_bytes = tok.col.checked_sub(1)?;
        let offset = self.line_starts.get(line_idx)? + col_bytes;
        (offset <= self.source.len()).then_some(offset)
    }

    pub(super) fn parse_order_by(&mut self) -> ParseResult<Vec<OrderByItem>> {
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expression()?;
            let descending = match self.peek_type() {
                TokenType::DESC => {
                    self.next_token();
                    true
                }
                TokenType::ASC => {
                    self.next_token();
                    false
                }
                _ => false,
            };
            items.push(OrderByItem { expr, descending });
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        Ok(items)
    }
}
