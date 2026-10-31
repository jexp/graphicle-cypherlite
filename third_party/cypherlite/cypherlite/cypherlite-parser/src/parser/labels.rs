use super::*;

impl Parser {
    // ---- Label expression parsing (CIP-14 / CIP-249) ----

    /// Flattens a label expression produced inside a legacy colon-chain's
    /// parenthesized segment (`(A:B)` inside `(A:B):C`) into a flat label
    /// list. `Label`/`Dynamic` atoms are pushed as-is; a segment's own
    /// `And` nesting (a nested `:`-chain) is flattened recursively,
    /// keeping each item as a full `LabelExpr` (CIP-156: a colon-chain
    /// segment may be a dynamic label/type atom, not just a plain label
    /// name) rather than reducing to a bare `String`.
    ///
    /// A segment body built from a GQL label-expression symbol (`Or`,
    /// `Not`, `Wildcard` -- reachable through `parse_label_expr_body`,
    /// e.g. `(n:(A|B):C)`) is rejected with the colon-conjunction mixing
    /// error, matching Neo4j (probe against neo4j:2026.09-community:
    /// `EXPLAIN MATCH (n:(A|B):C) RETURN n` fails with 42I10 "Mixing
    /// label expression symbols (`|`, `&`, `!`, and `%`) with colon
    /// (`:`) between labels is not allowed").
    fn flatten_and_segments(&self, expr: &LabelExpr, out: &mut Vec<LabelExpr>) -> ParseResult<()> {
        match expr {
            LabelExpr::Label(_) | LabelExpr::Dynamic(_) => out.push(expr.clone()),
            LabelExpr::And(parts) => {
                for p in parts {
                    self.flatten_and_segments(p, out)?;
                }
            }
            LabelExpr::Wildcard | LabelExpr::Not(_) | LabelExpr::Or(_) => {
                return Err(self.error(
                    "InvalidLabelExpression: cannot mix colon-conjunction with label expression symbols",
                ));
            }
        }
        Ok(())
    }

    /// Looks past the balanced `(...)` group opening at `peek_at(start)`
    /// (consuming no tokens) and returns the type of the token immediately
    /// following it (`EOF` when the group never closes). Used to detect a
    /// `(<label-expr>):C` legacy colon-chain continuation (CIP-114) without
    /// full backtracking. Defined over `skip_balanced_parens`, the one
    /// canonical balanced-paren walker.
    pub(super) fn peek_type_after_balanced_parens(&self, start: usize) -> TokenType {
        self.skip_balanced_parens(self.pos + start)
            .map_or(TokenType::EOF, |end| self.token_type_at(end))
    }

    /// Parses one segment of a legacy colon-chain label list (`:A:B:C`, or
    /// the CIP-114 CREATE baseline `(A:B):C`) -- a parenthesized label
    /// expression (whose labels are flattened into the chain), or a label
    /// atom (`parse_label_atom`: a label name, or a CIP-156 dynamic atom).
    pub(super) fn parse_colon_chain_segment(
        &mut self,
        segments: &mut Vec<LabelExpr>,
    ) -> ParseResult<()> {
        if self.peek_type() == TokenType::LeftParen {
            self.side_effects.label_saw_paren_group = true;
            self.next_token(); // consume (
            let inner = self.parse_label_expr_body(false, false)?;
            self.expect(TokenType::RightParen)?;
            self.flatten_and_segments(&inner, segments)?;
            return Ok(());
        }
        // ADR-0169: a keyword is a label name here too (`:A:Order`).
        if !self.peek_starts_label_atom() {
            return Err(self.error(
                "InvalidLabelExpression: cannot mix colon-conjunction with label expression symbols",
            ));
        }
        segments.push(self.parse_label_atom()?);
        Ok(())
    }

    /// Parses a full label/type spec introduced by `:` or `IS` (the
    /// introducer must still be the current token) in pattern position,
    /// honoring the CIP-14 legacy colon-conjunction shorthand. The
    /// clause-level colon/GQL mixing flags are set directly at each
    /// symbol-consumption site (see `parse_label_expr_body`); the `IS`
    /// introducer flag is set here once the spec parses. `pipe_is_legacy`
    /// is true for relationship type lists, where a bare top-level `|`
    /// predates CIP-14.
    pub(super) fn parse_label_spec(&mut self, pipe_is_legacy: bool) -> ParseResult<LabelExpr> {
        let via_is = self.peek_type() == TokenType::IS;
        self.next_token(); // consume ':' or 'IS'
        if via_is && self.peek_is_labeled_keyword() {
            return Err(self.error(
                "invalid label specification context: IS LABELED is a predicate, not a label specification",
            ));
        }
        let expr = self.parse_label_expr_body(via_is, pipe_is_legacy)?;
        if via_is {
            self.side_effects.label_saw_is_introducer = true;
        }
        Ok(expr)
    }

    /// Parses the body of a label/type expression (after the introducer has
    /// already been consumed). The clause-level mixing flags are set
    /// directly at each symbol-consumption site:
    /// `label_saw_legacy_conjunction` here on the legacy colon-chain path
    /// (a chain always has more than one segment), `label_saw_gql_symbol`
    /// in `parse_label_or`/`parse_label_and`/`parse_label_unary`/
    /// `parse_label_primary` at each GQL-only operator symbol.
    pub(super) fn parse_label_expr_body(
        &mut self,
        via_is: bool,
        pipe_is_legacy: bool,
    ) -> ParseResult<LabelExpr> {
        // Legacy colon-chain: `:A:B:C`, or (CIP-114's CREATE baseline)
        // `:(A:B):C` -- a parenthesized label expression may itself be one
        // segment of the chain, flattened into the resulting flat label set
        // (`(A:B):C` produces {A, B, C}, matching Neo4j). Only entered via
        // the ':' introducer, when the first segment is immediately
        // followed by another ':' -- a keyword first segment included
        // (`:Order:A`, ADR-0169).
        let starts_ident_colon_chain =
            self.peek_type().is_schema_name() && self.peek_at(1).ty == TokenType::Colon;
        let starts_paren_colon_chain = self.peek_type() == TokenType::LeftParen
            && self.peek_type_after_balanced_parens(0) == TokenType::Colon;
        // CIP-156: `:$(a):$(b)` -- a dynamic label/type atom may itself be a
        // colon-chain segment, mixed freely with plain labels. The atom
        // opens at the current token (the `$` Parameter token, with its
        // `(` one token ahead), so the token after its balanced `(...)`
        // group is `peek_type_after_balanced_parens(1)`.
        let starts_dynamic_colon_chain = self.dynamic_label_quantifier().is_some()
            && self.peek_type_after_balanced_parens(1) == TokenType::Colon;
        if !via_is
            && (starts_ident_colon_chain || starts_paren_colon_chain || starts_dynamic_colon_chain)
        {
            let mut segments = Vec::new();
            self.parse_colon_chain_segment(&mut segments)?;
            while self.peek_type() == TokenType::Colon {
                self.next_token();
                self.parse_colon_chain_segment(&mut segments)?;
            }
            if matches!(
                self.peek_type(),
                TokenType::Ampersand | TokenType::Bang | TokenType::Percent
            ) || (self.peek_type() == TokenType::Pipe && !pipe_is_legacy)
            {
                return Err(self.error(
                    "InvalidLabelExpression: cannot mix colon-conjunction with label expression symbols",
                ));
            }
            if segments.len() > 1 {
                self.side_effects.label_saw_legacy_conjunction = true;
            }
            return Ok(LabelExpr::And(segments));
        }
        self.parse_label_or(pipe_is_legacy)
    }

    pub(super) fn parse_label_or(&mut self, pipe_is_legacy: bool) -> ParseResult<LabelExpr> {
        let mut parts = vec![self.parse_label_and()?];
        while self.peek_type() == TokenType::Pipe {
            if self.comprehension_where_ambiguous_pipe && self.label_or_pipe_is_ambiguous() {
                break;
            }
            if !pipe_is_legacy {
                self.side_effects.label_saw_gql_symbol = true;
            }
            self.next_token();
            if pipe_is_legacy && self.peek_type() == TokenType::Colon {
                // Legacy relationship type list allows an optional repeated
                // colon after `|`, e.g. `[:T|:T]` / `[:A|:B]`.
                self.next_token();
            }
            parts.push(self.parse_label_and()?);
        }
        if parts.len() == 1 {
            Ok(parts.pop().unwrap())
        } else {
            Ok(LabelExpr::Or(parts))
        }
    }

    /// Speculatively checks whether the `|` at the current position, inside
    /// a list/pattern comprehension's `WHERE`-part, should be treated as the
    /// comprehension's map-expression separator rather than a
    /// label-expression disjunction continuation. Only called when
    /// `comprehension_where_ambiguous_pipe` is set. Pure token-level
    /// lookahead (no speculative parse): scans one label-`and` chain past
    /// the `|`; if there isn't one, or there is but a `.` (property access,
    /// which can never validly follow a label expression) immediately
    /// follows it, the `|` is ambiguous and is left for the comprehension
    /// grammar instead. Consumes no input and sets no flags.
    pub(super) fn label_or_pipe_is_ambiguous(&mut self) -> bool {
        debug_assert_eq!(self.peek_type(), TokenType::Pipe);
        match self.scan_label_and_extent(self.pos + 1, self.expr_depth) {
            Some(end) => self.token_type_at(end) == TokenType::Dot,
            None => true,
        }
    }

    /// Type of the token at absolute index `pos` (`EOF` past the end) --
    /// the absolute-index counterpart of `peek_at` for token-level scans,
    /// in the idiom of `scan_dot_chain_for_call`.
    fn token_type_at(&self, pos: usize) -> TokenType {
        match self.tokens.get(pos) {
            Some(t) => t.ty,
            None => TokenType::EOF,
        }
    }

    /// Token-level extent scan of one label-`and` chain (`unary (&
    /// unary)*`, the scan counterpart of `parse_label_and`) starting at
    /// absolute token index `pos`: returns the index one past the chain's
    /// end, or `None` when the tokens there do not form one. `depth`
    /// mirrors the `with_expr_depth` budget (ADR-0257): entering a `!`
    /// step or a group consumes one nesting level, and exhausting the
    /// budget fails the scan exactly as the depth guard fails the parse,
    /// so the scan itself never recurses past [`MAX_NESTING_DEPTH`].
    fn scan_label_and_extent(&mut self, pos: usize, depth: u32) -> Option<usize> {
        let mut pos = self.scan_label_unary_extent(pos, depth)?;
        while self.token_type_at(pos) == TokenType::Ampersand {
            pos = self.scan_label_unary_extent(pos + 1, depth)?;
        }
        Some(pos)
    }

    /// [`Parser::scan_label_and_extent`] for one `unary` term: a `!` chain
    /// bottoming out in a primary.
    fn scan_label_unary_extent(&mut self, mut pos: usize, mut depth: u32) -> Option<usize> {
        while self.token_type_at(pos) == TokenType::Bang {
            if depth >= MAX_NESTING_DEPTH {
                return None;
            }
            depth += 1;
            pos += 1;
        }
        self.scan_label_primary_extent(pos, depth)
    }

    /// [`Parser::scan_label_and_extent`] for one primary: the `%`
    /// wildcard, a `(`/`[` group (whose label-`or` body is scanned
    /// recursively, pipe-ambiguity rule included, mirroring
    /// `parse_label_primary`'s recursion into `parse_label_or`), or a
    /// label atom (a schema-name token, or a `$(...)`/`$all(...)`/
    /// `$any(...)` dynamic atom whose balanced `(...)` group is skipped
    /// without validating the inner expression -- a malformed one still
    /// fails the real parse that a not-ambiguous verdict lets run).
    fn scan_label_primary_extent(&mut self, pos: usize, depth: u32) -> Option<usize> {
        match self.token_type_at(pos) {
            TokenType::Percent => Some(pos + 1),
            opener @ (TokenType::LeftParen | TokenType::LeftBracket) => {
                if depth >= MAX_NESTING_DEPTH {
                    return None;
                }
                if let Some(&extent) = self.label_group_extent_memo.get(&(pos, depth)) {
                    return extent;
                }
                let closer = if opener == TokenType::LeftParen {
                    TokenType::RightParen
                } else {
                    TokenType::RightBracket
                };
                let extent = match self.scan_label_or_extent(pos + 1, depth + 1) {
                    Some(inner_end) if self.token_type_at(inner_end) == closer => {
                        Some(inner_end + 1)
                    }
                    _ => None,
                };
                self.label_group_extent_memo.insert((pos, depth), extent);
                extent
            }
            ty if ty.is_schema_name() => Some(pos + 1),
            // Scan positions always stay at or past `self.pos`, so the
            // relative-offset quantifier check applies.
            _ if self.dynamic_label_quantifier_at(pos - self.pos).is_some() => {
                // The `(` follows the `$` parameter token.
                self.skip_balanced_parens(pos + 1)
            }
            _ => None,
        }
    }

    /// [`Parser::scan_label_and_extent`] for a group's label-`or` body
    /// (`label_and (| label_and)*`, `parse_label_or`'s scan counterpart).
    /// Each `|` joins the disjunction only when a full label-`and` chain
    /// follows it that does not end in front of a `.` -- the same
    /// ambiguity rule `label_or_pipe_is_ambiguous` applies at the top
    /// level, which `parse_label_or` also applies to nested groups while
    /// `comprehension_where_ambiguous_pipe` is set.
    fn scan_label_or_extent(&mut self, pos: usize, depth: u32) -> Option<usize> {
        let mut pos = self.scan_label_and_extent(pos, depth)?;
        while self.token_type_at(pos) == TokenType::Pipe {
            let Some(end) = self.scan_label_and_extent(pos + 1, depth) else {
                break;
            };
            if self.token_type_at(end) == TokenType::Dot {
                break;
            }
            pos = end;
        }
        Some(pos)
    }

    /// Skips the balanced `(...)` group opening at absolute index `open`
    /// (which must be a `LeftParen`), returning the index one past the
    /// matching `RightParen`, or `None` when it never closes. The one
    /// canonical balanced-paren walker: `peek_type_after_balanced_parens`
    /// (relative offset, token-type result) is defined over it.
    fn skip_balanced_parens(&self, open: usize) -> Option<usize> {
        let mut depth = 0i32;
        let mut i = open;
        loop {
            match self.token_type_at(i) {
                TokenType::LeftParen => depth += 1,
                TokenType::RightParen => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                TokenType::EOF => return None,
                _ => {}
            }
            i += 1;
        }
    }

    pub(super) fn parse_label_and(&mut self) -> ParseResult<LabelExpr> {
        let mut parts = vec![self.parse_label_unary()?];
        while self.peek_type() == TokenType::Ampersand {
            self.side_effects.label_saw_gql_symbol = true;
            self.next_token();
            parts.push(self.parse_label_unary()?);
        }
        if parts.len() == 1 {
            Ok(parts.pop().unwrap())
        } else {
            Ok(LabelExpr::And(parts))
        }
    }

    pub(super) fn parse_label_unary(&mut self) -> ParseResult<LabelExpr> {
        if self.peek_type() == TokenType::Bang {
            self.side_effects.label_saw_gql_symbol = true;
            self.next_token();
            // ADR-0257: `!` chains recurse here directly, bypassing
            // `parse_expression`, so each step must count separately
            // (LABS-69).
            let inner = self.with_expr_depth(|p| p.parse_label_unary())?;
            return Ok(LabelExpr::Not(Box::new(inner)));
        }
        self.parse_label_primary()
    }

    pub(super) fn parse_label_primary(&mut self) -> ParseResult<LabelExpr> {
        match self.peek_type() {
            TokenType::Percent => {
                self.side_effects.label_saw_gql_symbol = true;
                self.next_token();
                Ok(LabelExpr::Wildcard)
            }
            opener @ (TokenType::LeftParen | TokenType::LeftBracket) => {
                let closer = if opener == TokenType::LeftParen {
                    TokenType::RightParen
                } else {
                    TokenType::RightBracket
                };
                self.side_effects.label_saw_gql_symbol = true;
                self.side_effects.label_saw_paren_group = true;
                self.next_token();
                // ADR-0257: nested label groups recurse through
                // `parse_label_or` -> ... -> `parse_label_primary` without
                // passing through `parse_expression`; count each level
                // (LABS-69).
                let inner = self.with_expr_depth(|p| p.parse_label_or(false))?;
                self.expect(closer)?;
                Ok(inner)
            }
            // A label name, or a CIP-249/156/221 dynamic atom -- which, not
            // being a CIP-14 mixing symbol, doesn't set
            // `label_saw_gql_symbol`.
            _ => self.parse_label_atom(),
        }
    }

    /// True if the current token is the contextual `LABELED` keyword (like
    /// `TYPED`, not a reserved word in the lexer).
    pub(super) fn peek_is_labeled_keyword(&self) -> bool {
        let tok = self.peek();
        tok.ty == TokenType::Identifier && tok.literal.eq_ignore_ascii_case("LABELED")
    }

    /// True if the current token could begin a label-expression primary
    /// (used to detect the bare `IS <label-expr>` form vs. `IS NULL`/other
    /// IS-suffixes at the general expression-grammar level). Any keyword
    /// can start one (`x IS Order`, ADR-0169): the caller consumes `NOT`
    /// and tries the `::`/`TYPED`/`NORMALIZED`/`LABELED`/`NULL` suffixes
    /// first, so those keep their predicate meaning.
    pub(super) fn peek_can_start_label_expr(&self) -> bool {
        self.peek_can_start_label_expr_at(0)
    }

    /// True if the token `offset` ahead could begin a label-expression
    /// primary: a label name (any keyword, ADR-0169), the `%` wildcard, a
    /// `!` negation, a `(`/`[` grouping, or a `$`/`$all`/`$any` dynamic
    /// atom opener (CIP-249/221). One offset-aware truth table shared by the
    /// expression-level reader above and the pattern-name lookahead's `IS`
    /// arm (`peek_pattern_name_is_variable`), which must agree on when a
    /// leading `IS` introduces a label spec rather than binding the element
    /// variable.
    pub(super) fn peek_can_start_label_expr_at(&self, offset: usize) -> bool {
        matches!(
            self.peek_at(offset).ty,
            TokenType::Percent | TokenType::Bang | TokenType::LeftParen | TokenType::LeftBracket
        ) || self.peek_at(offset).ty.is_schema_name()
            || self.dynamic_label_quantifier_at(offset).is_some()
    }
}
