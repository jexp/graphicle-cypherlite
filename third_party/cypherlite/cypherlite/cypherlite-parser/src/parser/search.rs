//! The CIP-224/225/278 SEARCH clause grammar: `SEARCH <binding var ref>
//! IN ( <source specification clause> ) [SCORE AS <var>]`, where the source
//! is an index specification or a CIP-278 combined (COMBINE) one -- one
//! self-contained grammar family, split out of the general clause parser
//! (`super::clauses`).

use super::*;

impl Parser {
    /// True if the parser is positioned at the start of a CIP-224 `<search
    /// clause>` (a MATCH subclause). SEARCH is a non-reserved word --
    /// deliberately never added to `token.rs`'s keyword table -- so this is
    /// a plain case-insensitive literal check on an `Identifier` token,
    /// exactly like the `FILTER`/`FINISH`/match-mode-prefix contextual
    /// keywords elsewhere in this parser.
    pub(super) fn is_search_clause_start(&self) -> bool {
        self.peek_type() == TokenType::Identifier
            && self.peek().literal.eq_ignore_ascii_case("search")
    }

    /// Parses a CIP-224 `<search clause>`:
    /// `SEARCH <binding var ref> IN ( <source specification clause> )
    /// [SCORE AS <var>]` -- the source is a CIP-225 index specification or,
    /// per CIP-278, a combined (COMBINE) specification.
    pub(super) fn parse_search_clause(&mut self) -> ParseResult<SearchClause> {
        self.next_token(); // consume SEARCH

        // Rule 4's `<binding variable reference>` is a variable name
        // reference to a variable already in scope. ADR-0191 (Decision
        // point 7's follow-up probe): Neo4j 2026.09 reads every keyword
        // spelling here as a variable name -- `NULL` included (`SEARCH
        // NULL IN (...)` parses and fails semantically, "Variable `NULL`
        // not defined", exactly what CIP-224 binding-variable scenario
        // [4] expects; an unbound reference here is rejected by
        // cypherlite-front's `InvalidSearchVariableReference` check, not
        // by this read). It is never a property access (Example B2:
        // `SEARCH m.title IN (...)` is rejected, caught below by
        // requiring `IN`/`,` right after).
        let binding_var =
            self.parse_schema_name("a single binding variable reference after SEARCH")?;

        // Example 11 / basic-syntax[6]: exactly one variable after SEARCH.
        if self.peek_type() == TokenType::Comma {
            return Err(
                self.error("InvalidSyntax: SEARCH accepts exactly one binding variable reference")
            );
        }

        self.expect(TokenType::IN)?;
        self.expect(TokenType::LeftParen)?;
        let spec = self.parse_search_spec()?;
        self.expect(TokenType::RightParen)?;

        let score_var = if self.peek_type() == TokenType::Identifier
            && self.peek().literal.eq_ignore_ascii_case("score")
        {
            self.next_token(); // consume SCORE
            self.expect(TokenType::AS)?;
            // ADR-0191 (Decision point 7's follow-up probe): Neo4j 2026.09
            // accepts every keyword spelling as the SCORE AS variable, so
            // this reads through the shared symbolic-name read like every
            // other alias slot.
            let var = self.parse_schema_name("a binding variable after SCORE AS")?;
            Some(var)
        } else {
            None
        };

        Ok(SearchClause {
            binding_var,
            spec,
            score_var,
        })
    }

    /// Parses a CIP-278 `<source specification clause>`: a CIP-225
    /// `<index specification clause>` or a `<combined specification
    /// clause>`. `COMBINE` is a new non-reserved word (never added to
    /// `token.rs`'s keyword table), recognized by spelling like `SEARCH`
    /// above.
    /// `<keyword> <non-reserved word>` lookahead: `WITH` is the keyword
    /// token, the second word an ident-like non-reserved one (`ANALYZER`,
    /// `CONSTANT` -- the set grows with the grammar, so the shape lives
    /// once).
    fn peek_with_word(&self, word: &str) -> bool {
        self.peek_type() == TokenType::WITH
            && self.peek_at(1).ty.is_ident_like()
            && self.peek_at(1).literal.eq_ignore_ascii_case(word)
    }

    fn parse_search_spec(&mut self) -> ParseResult<SearchSpec> {
        // The nested-COMBINE cycle is a recursion family like the label
        // and expression cycles: bound it with the shared depth budget
        // (ADR-0079) so a crafted deep nesting is a clean parse error,
        // not a stack overflow.
        self.with_expr_depth(Self::parse_search_spec_at_current_depth)
    }

    fn parse_search_spec_at_current_depth(&mut self) -> ParseResult<SearchSpec> {
        if self.peek_is_ident("COMBINE") {
            self.parse_search_combine_spec().map(SearchSpec::Combine)
        } else {
            self.parse_search_index_spec().map(SearchSpec::Index)
        }
    }

    /// Parses a CIP-225 `<index specification clause>`:
    /// `{ VECTOR | FULLTEXT } INDEX <index name> FOR <expr>
    /// [WITH ANALYZER <expr>] [WHERE <expr>] [SKIP/OFFSET <expr>]
    /// LIMIT <expr>` -- everything between the SEARCH (or COMBINE) parens
    /// except the parens themselves, so the same body serves a top-level
    /// source and a weighted source inside a COMBINE.
    fn parse_search_index_spec(&mut self) -> ParseResult<SearchIndexSpec> {
        // CIP-225: `{ VECTOR | FULLTEXT } INDEX <index name specification>`.
        // FULLTEXT lexes as an identifier (a new non-reserved word per the
        // CIP); VECTOR is a keyword token.
        let kind = if self.peek_is_ident("FULLTEXT") {
            self.next_token();
            SearchKind::Fulltext
        } else {
            self.expect(TokenType::VECTOR)?;
            SearchKind::Vector
        };
        self.expect(TokenType::INDEX)?;

        // Rule 1: the index name must be a literal identifier, not a
        // general expression (Example IN3) and not a parameter (Example
        // IN8). `is_ident_like()` (rather than plain `Identifier`) lets it
        // be written as a keyword-shaped name too, e.g. `VECTOR INDEX null`
        // (Example IN7), matching how index/variable names elsewhere in
        // this grammar accept non-reserved keywords.
        let index_tok = self.peek().clone();
        if !index_tok.ty.is_ident_like() {
            return Err(self.error_at_token(
                &index_tok,
                "InvalidSyntax: the index specification requires a literal index name",
            ));
        }
        self.next_token();
        let index_name = index_tok.literal;

        self.expect(TokenType::FOR)?;
        let for_expr = self.parse_expression()?;

        // CIP-225: the optional `WITH ANALYZER <value expression>` after
        // the FOR clause (fulltext only). ANALYZER is a new non-reserved
        // word; WITH is the keyword token.
        let analyzer = if self.peek_with_word("analyzer") {
            self.next_token();
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        // CIP-240: the optional inner `WHERE <search condition>` between
        // `FOR <expr>` and `LIMIT <expr>`. The rewrite/shape rules (Rule 0,
        // 3, 6, 7) are enforced by the engine, not here -- the grammar
        // slot accepts any general expression, exactly like the ordinary
        // `<graph pattern where clause>` above.
        let inner_where = if self.peek_type() == TokenType::WHERE {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        // CIP-225: the optional `<offset clause>` (`SKIP` or `OFFSET`)
        // before the mandatory LIMIT, shared by both index kinds.
        let offset_expr = if self.peek_type() == TokenType::SKIP || self.peek_is_offset() {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        self.expect(TokenType::LIMIT)?;
        let limit_expr = self.parse_expression()?;

        Ok(SearchIndexSpec {
            kind,
            index_name,
            for_expr,
            analyzer,
            inner_where,
            offset_expr,
            limit_expr,
        })
    }

    /// Parses a CIP-278 `<combined specification clause>`:
    /// `COMBINE ( <weighted source clause> [, <weighted source clause>]... )
    /// USING WRRF [ WITH CONSTANT <expr> ] [ LIMIT <expr> ]`.
    /// `COMBINE`/`WRRF`/`CONSTANT`/`WEIGHT` are new non-reserved words
    /// (never added to `token.rs`'s keyword table), recognized by spelling;
    /// `USING`/`WITH`/`LIMIT` are the existing keyword/contextual reads.
    /// Rule 5: the body must hold two or more weighted source clauses --
    /// fewer is a parse error here, at the list itself.
    fn parse_search_combine_spec(&mut self) -> ParseResult<SearchCombineSpec> {
        self.next_token(); // consume COMBINE
        self.expect(TokenType::LeftParen)?;
        let mut sources = vec![self.parse_search_weighted_source()?];
        while self.peek_type() == TokenType::Comma {
            self.next_token();
            sources.push(self.parse_search_weighted_source()?);
        }
        if sources.len() < 2 {
            return Err(self.error(
                "InvalidSyntax: missing source: The COMBINE clause requires two or more sources \
                 provided",
            ));
        }
        self.expect(TokenType::RightParen)?;
        // `<result fusion specification>`: `USING WRRF` -- WRRF is the only
        // fusion method (a later method extends this read).
        if !self.peek_is_ident("USING") {
            return Err(self.error("InvalidSyntax: expected USING WRRF after the COMBINE sources"));
        }
        self.next_token(); // consume USING
        if !self.peek_is_ident("WRRF") {
            return Err(self.error("InvalidSyntax: expected the WRRF fusion method after USING"));
        }
        self.next_token(); // consume WRRF
                           // `<constant specification clause>`: `WITH CONSTANT <expr>`
                           // (CONSTANT is a non-reserved word; WITH the keyword token).
        let constant_expr = if self.peek_with_word("CONSTANT") {
            self.next_token();
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };
        // The combined specification's own optional `<limit clause>`: the n
        // highest-scoring fused results kept (Rule 7).
        let limit_expr = if self.peek_type() == TokenType::LIMIT {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(SearchCombineSpec {
            sources,
            constant_expr,
            limit_expr,
        })
    }

    /// Parses a CIP-278 `<weighted source clause>`:
    /// `<source specification clause> [ WEIGHT <value expression> ]` -- a
    /// source may itself be a nested combined specification (Example P3),
    /// and the optional WEIGHT follows it either way.
    fn parse_search_weighted_source(&mut self) -> ParseResult<SearchWeightedSource> {
        let spec = self.parse_search_spec()?;
        let weight_expr = if self.peek_is_ident("WEIGHT") {
            self.next_token(); // consume WEIGHT
            Some(self.parse_expression()?)
        } else {
            None
        };
        Ok(SearchWeightedSource { spec, weight_expr })
    }
}
