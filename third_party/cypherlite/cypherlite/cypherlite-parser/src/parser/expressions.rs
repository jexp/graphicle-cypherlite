use super::*;

/// `parse_reduce_head`'s result: the `( acc = init, var IN list | expr`
/// head shared by `reduce` and `allReduce`.
struct ReduceHead {
    accumulator: String,
    init_expr: Expr,
    variable: String,
    list_expr: Expr,
    expression: Expr,
}

impl Parser {
    // ---- Expression parsing (precedence climbing) ----

    pub(super) fn parse_expression(&mut self) -> ParseResult<Expr> {
        // ADR-0257: every nested expression descends through here, so this
        // single counter bounds all expression recursion (LABS-69).
        self.with_expr_depth(Self::parse_or)
    }

    /// The `<expr> (, <expr>)*` production shared by `SHOW SETTINGS`
    /// `<names>`, `SHOW TRANSACTIONS` `<id-list>` and `TERMINATE
    /// TRANSACTIONS` `<id-list>` (CIP-68/CIP-84). Parses the first
    /// expression itself; the caller checks
    /// `peek_starts_show_filter_expr` first when the list is optional.
    pub(super) fn parse_comma_separated_exprs(&mut self) -> ParseResult<Vec<Expr>> {
        let mut items = vec![self.parse_expression()?];
        while self.peek_type() == TokenType::Comma {
            self.next_token();
            items.push(self.parse_expression()?);
        }
        Ok(items)
    }

    pub(super) fn parse_or(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_xor()?;
        while self.peek_type() == TokenType::OR {
            self.next_token();
            let right = self.parse_xor()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: "OR".to_string(),
                right,
            }));
        }
        Ok(left)
    }

    pub(super) fn parse_xor(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_and()?;
        while self.peek_type() == TokenType::XOR {
            self.next_token();
            let right = self.parse_and()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: "XOR".to_string(),
                right,
            }));
        }
        Ok(left)
    }

    pub(super) fn parse_and(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_not()?;
        while self.peek_type() == TokenType::AND {
            self.next_token();
            let right = self.parse_not()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: "AND".to_string(),
                right,
            }));
        }
        Ok(left)
    }

    pub(super) fn parse_not(&mut self) -> ParseResult<Expr> {
        if self.peek_type() == TokenType::NOT {
            self.next_token();
            // ADR-0257: `NOT` chains recurse here directly, bypassing
            // `parse_expression`, so each step must count separately
            // (LABS-69).
            let expr = self.with_expr_depth(|p| p.parse_not())?;
            return Ok(Expr::Unary(Box::new(UnaryExpr {
                op: "NOT".to_string(),
                expr,
            })));
        }
        self.parse_comparison()
    }

    pub(super) fn parse_comparison(&mut self) -> ParseResult<Expr> {
        let left = self.parse_string_null_check()?;

        let op = comparison_op(self.peek_type());
        if op.is_none() {
            return Ok(left);
        }
        let op = op.unwrap();
        self.next_token();
        let mut right = self.parse_string_null_check()?;
        let mut result = Expr::Comparison(Box::new(ComparisonExpr {
            left,
            op: op.to_string(),
            right: right.clone(),
        }));

        // Chained comparisons
        loop {
            let next_op = comparison_op(self.peek_type());
            if next_op.is_none() {
                break;
            }
            let next_op = next_op.unwrap();
            self.next_token();
            let next_right = self.parse_string_null_check()?;
            result = Expr::Binary(Box::new(BinaryExpr {
                left: result,
                op: "AND".to_string(),
                right: Expr::Comparison(Box::new(ComparisonExpr {
                    left: right,
                    op: next_op.to_string(),
                    right: next_right.clone(),
                })),
            }));
            right = next_right;
        }

        Ok(result)
    }

    /// Parses `ComparisonExpression6`: an `AddOrSubtractExpression` followed
    /// by *at most one* of `IN`/`STARTS WITH`/`ENDS WITH`/`CONTAINS`/`IS
    /// [NOT] NULL`/`IS [NOT] [<normal form>] NORMALIZED`/the CIP-87 type
    /// predicate (`IS [NOT] ::`, `IS [NOT] TYPED`, bare `::`). These
    /// alternatives do not chain: per CIP-87's "Precedence of Type
    /// Predicate Expression" section, combining any two of them without
    /// parentheses (e.g. `"ab" STARTS WITH "a" IS :: BOOLEAN`, or two type
    /// predicates back to back) is a compile-time syntax error, so this is
    /// a single optional suffix rather than a loop.
    pub(super) fn parse_string_null_check(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_add_sub()?;
        let mut consumed_suffix = true;
        match self.peek_type() {
            TokenType::IN => {
                self.next_token();
                let right = self.parse_add_sub()?;
                left = Expr::In(Box::new(InExpr {
                    expr: left,
                    list: right,
                }));
            }
            TokenType::STARTS => {
                self.next_token();
                self.expect(TokenType::WITH)?;
                let right = self.parse_add_sub()?;
                left = Expr::StringMatch(Box::new(StringMatchExpr {
                    left,
                    op: "STARTS WITH".to_string(),
                    right,
                }));
            }
            TokenType::ENDS => {
                self.next_token();
                self.expect(TokenType::WITH)?;
                let right = self.parse_add_sub()?;
                left = Expr::StringMatch(Box::new(StringMatchExpr {
                    left,
                    op: "ENDS WITH".to_string(),
                    right,
                }));
            }
            TokenType::CONTAINS => {
                self.next_token();
                let right = self.parse_add_sub()?;
                left = Expr::StringMatch(Box::new(StringMatchExpr {
                    left,
                    op: "CONTAINS".to_string(),
                    right,
                }));
            }
            _ => {
                consumed_suffix = false;
            }
        }

        // Per CIP-87's precedence rules, a string/list predicate and an
        // IS-suffix (null/type/normalized check) do not combine without
        // parens: only try the IS-suffix if the above didn't already fire.
        if consumed_suffix {
            return Ok(left);
        }

        match self.peek_type() {
            TokenType::IS => {
                self.next_token();
                let negated = if self.peek_type() == TokenType::NOT {
                    self.next_token();
                    true
                } else {
                    false
                };

                if self.peek_type() == TokenType::DoubleColon {
                    // x IS [NOT] :: <value type>  (CIP-87 type predicate)
                    self.next_token();
                    let type_expr = self.parse_value_type()?;
                    left = Expr::IsType(Box::new(IsTypeExpr {
                        expr: left,
                        type_expr,
                        negated,
                    }));
                } else if self.peek_is_typed_keyword() {
                    // x IS [NOT] TYPED <value type>  (CIP-87 type predicate)
                    self.next_token();
                    let type_expr = self.parse_value_type()?;
                    left = Expr::IsType(Box::new(IsTypeExpr {
                        expr: left,
                        type_expr,
                        negated,
                    }));
                } else if self.peek_is_normal_form_or_normalized() {
                    // x IS [NOT] [<normal form>] NORMALIZED  (CIP-133)
                    let form = self.try_consume_normal_form_keyword();
                    self.expect_keyword_ident("NORMALIZED")?;
                    left = Expr::IsNormalized(Box::new(IsNormalizedExpr {
                        expr: left,
                        form,
                        negated,
                    }));
                } else if self.peek_is_labeled_keyword() {
                    // x IS [NOT] LABELED <label-expr>  (CIP-249)
                    self.next_token();
                    let label_expr = self.parse_label_expr_body(true, false)?;
                    left = Expr::HasLabel(Box::new(HasLabelExpr {
                        expr: left,
                        label_expr,
                        negated,
                        strict: true,
                    }));
                } else if self.peek_type() == TokenType::NULL {
                    // x IS [NOT] NULL
                    self.next_token();
                    left = Expr::IsNull(Box::new(IsNullExpr {
                        expr: left,
                        negated,
                    }));
                } else if self.peek_can_start_label_expr() {
                    // x IS <label-expr>  (CIP-14, non-negated, interchangeable
                    // with `:`) or x IS NOT <label-expr>  (CIP-249 shorthand
                    // for IS NOT LABELED <label-expr>).
                    let label_expr = self.parse_label_expr_body(true, false)?;
                    left = Expr::HasLabel(Box::new(HasLabelExpr {
                        expr: left,
                        label_expr,
                        negated,
                        strict: negated,
                    }));
                } else {
                    self.expect(TokenType::NULL)?;
                    left = Expr::IsNull(Box::new(IsNullExpr {
                        expr: left,
                        negated,
                    }));
                }
            }
            TokenType::DoubleColon => {
                // x :: <value type>  (CIP-87 bare shorthand for IS ::)
                self.next_token();
                let type_expr = self.parse_value_type()?;
                left = Expr::IsType(Box::new(IsTypeExpr {
                    expr: left,
                    type_expr,
                    negated: false,
                }));
            }
            TokenType::Colon => {
                // x:<label-expr>  (CIP-14, colon spelling of the label
                // predicate, interchangeable with `IS <label-expr>`).
                // CIP-190 moves this from `<postfix operator>` (tighter than
                // `+`) to here, `<advanced comparison predicate part 2>`
                // (looser than `+`): `[1,'abc',3] + n:A` now parses as
                // `([1,'abc',3] + n):A`, not `[1,'abc',3] + (n:A)`
                // (label-expression-precedence.feature [1]/[3]). Unlike the
                // pre-CIP-190 postfix version, `left` here is a whole
                // `<add or subtract expression>`, not necessarily a bare
                // variable -- but that's fine: SET/REMOVE's own `n:Label`
                // grammar is parsed by its own bespoke logic before ever
                // reaching general expression parsing, and
                // `parse_general_property_target` (SET's complex-target
                // fallback) calls `parse_postfix` directly, which no longer
                // handles `:` at all, so there's still no conflict.
                let label_expr = self.parse_label_spec(true)?;
                left = Expr::HasLabel(Box::new(HasLabelExpr {
                    expr: left,
                    label_expr,
                    negated: false,
                    strict: false,
                }));
            }
            _ => {}
        }

        Ok(left)
    }

    /// True if the current token is the contextual `TYPED` keyword (like
    /// `NORMALIZED`, not a reserved word in the lexer).
    pub(super) fn peek_is_typed_keyword(&self) -> bool {
        let tok = self.peek();
        tok.ty == TokenType::Identifier && tok.literal.eq_ignore_ascii_case("TYPED")
    }

    /// Returns the canonical uppercase form name ("NFC"/"NFD"/"NFKC"/"NFKD")
    /// if the current token is one of the normal-form keywords, else None.
    pub(super) fn peek_normal_form_keyword(&self) -> Option<String> {
        let tok = self.peek();
        if tok.ty == TokenType::Identifier {
            let upper = tok.literal.to_ascii_uppercase();
            if matches!(upper.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
                return Some(upper);
            }
        }
        None
    }

    /// True if the current token starts a `[<normal form>] NORMALIZED` tail
    /// (used to distinguish `IS NORMALIZED`/`IS <form> NORMALIZED` from `IS NULL`).
    pub(super) fn peek_is_normal_form_or_normalized(&self) -> bool {
        if self.peek_normal_form_keyword().is_some() {
            return true;
        }
        let tok = self.peek();
        tok.ty == TokenType::Identifier && tok.literal.eq_ignore_ascii_case("NORMALIZED")
    }

    /// Consumes and returns a normal-form keyword if present; otherwise leaves
    /// the token stream untouched and returns None.
    pub(super) fn try_consume_normal_form_keyword(&mut self) -> Option<String> {
        let form = self.peek_normal_form_keyword();
        if form.is_some() {
            self.next_token();
        }
        form
    }

    /// Expects the current token to be a case-insensitive identifier keyword
    /// -- one that is not a reserved word in the lexer, e.g. `NORMALIZED`,
    /// or a word of the multi-word DDL/SHOW forms (`CURRENT GRAPH TYPE`,
    /// `DEFAULT DATABASE`, `CURRENT USER`) where only one spelling is valid
    /// at this position. `keyword` is written uppercase, as the error shows
    /// it.
    pub(super) fn expect_keyword_ident(&mut self, keyword: &str) -> ParseResult<()> {
        let tok = self.peek().clone();
        if tok.ty == TokenType::Identifier && tok.literal.eq_ignore_ascii_case(keyword) {
            self.next_token();
            Ok(())
        } else {
            Err(self.error_at_token(&tok, format!("expected {}, got {:?}", keyword, tok.literal)))
        }
    }

    /// The consume-and-validate gate for a command's kind word: the next
    /// token must be the `singular` keyword token itself, or an identifier
    /// spelled `singular_ident` or `plural` (CIP-68's singular/plural KIND
    /// acceptance). Serves the SHOW family's kind word (`FUNCTION(S)`,
    /// `INDEX(ES)`, `CONSTRAINT(S)`, ...) and TERMINATE's `TRANSACTION(S)`.
    /// Pass `TokenType::Identifier` as `singular` when the word has no
    /// reserved token of its own (`FUNCTION(S)`, `DEFINED`) -- the
    /// identifier-spelling check is then the whole gate. Anything else gets
    /// an `expected <expected>, got <literal>` error.
    pub(super) fn expect_kind_word(
        &mut self,
        singular: TokenType,
        singular_ident: &str,
        plural: Option<&str>,
        expected: &str,
    ) -> ParseResult<()> {
        let tok = self.next_token();
        let is_singular_token = singular != TokenType::Identifier && tok.ty == singular;
        let is_ident_spelling = tok.ty == TokenType::Identifier
            && (tok.literal.eq_ignore_ascii_case(singular_ident)
                || plural.is_some_and(|p| tok.literal.eq_ignore_ascii_case(p)));
        if !(is_singular_token || is_ident_spelling) {
            return Err(
                self.error_at_token(&tok, format!("expected {expected}, got {:?}", tok.literal))
            );
        }
        Ok(())
    }

    /// Parses `normalize(input)` or `normalize(input, normalForm)`. The
    /// second argument (if present) must be a bare normal-form keyword
    /// (NFC/NFD/NFKC/NFKD) — a quoted string, parameter, or any other
    /// expression there is a compile-time error (CIP-132), so this bypasses
    /// the generic argument-list parser.
    pub(super) fn parse_normalize_call(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume (
        if self.peek_type() == TokenType::RightParen {
            return Err(self.error("normalize() requires at least 1 argument"));
        }
        let input = self.parse_expression()?;
        let mut args = vec![input];
        if self.peek_type() == TokenType::Comma {
            self.next_token();
            let form = self.try_consume_normal_form_keyword().ok_or_else(|| {
                let tok = self.peek().clone();
                self.error_at_token(
                    &tok,
                    format!(
                        "invalid normal form: expected NFC, NFD, NFKC, or NFKD, got {:?}",
                        tok.literal
                    ),
                )
            })?;
            args.push(Expr::Literal(LiteralValue::String(form)));
            if self.peek_type() == TokenType::Comma {
                return Err(self.error("normalize() accepts at most 2 arguments"));
            }
        }
        self.expect(TokenType::RightParen)?;
        Ok(Expr::FunctionCall(FunctionCallExpr {
            name: "normalize".to_string(),
            args,
            distinct: false,
        }))
    }

    /// Parses the GQL TRIM special form (CIP-149):
    /// `trim([[LEADING|TRAILING|BOTH] [trimCharacterString] FROM] input)`.
    /// `LEADING`/`TRAILING`/`BOTH` are not reserved keywords elsewhere, so
    /// they lex as plain identifiers and are recognized here by their
    /// literal spelling. The trim specification and character string
    /// selects which of the three underlying dispatch names ("trim" for
    /// BOTH/default, "trim_leading", "trim_trailing") the call compiles to;
    /// an omitted trim character string is distinguished from an explicit
    /// one by arg count (1 vs 2), letting the evaluator apply the correct
    /// default-whitespace vs. explicit-character-string semantics.
    ///
    /// Complexity: a cheap token-level gate (`trim_args_have_top_level_from`)
    /// decides up front whether a `FROM`-bearing spec form is even possible;
    /// without it the plain `trim(input)` form parses in a single pass. When
    /// the gate passes, the candidate trim-character-string expression is
    /// parsed exactly once and *committed to* — the old code restored the
    /// position and re-parsed the same tokens whenever no `FROM` followed,
    /// which made nested `trim(trim(...))` cost T(k) = 2·T(k-1) (k=28 took
    /// ~80 s). The only remaining backtrack is the error-only path where a
    /// leading BOTH/LEADING/TRAILING identifier was tentatively consumed as
    /// `spec` but turns out to be an ordinary variable (e.g. `trim(both)`
    /// inside a larger expression containing a top-level `FROM`).
    pub(super) fn parse_trim_call(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume (

        // The `[spec] [chars] FROM input` forms all require a `FROM` keyword
        // at this trim call's own paren depth. If none exists this is
        // necessarily the plain `trim(input)` form — parse it directly with
        // no speculation (this is what keeps deeply nested trim() linear).
        if !self.trim_args_have_top_level_from() {
            let input = self.parse_expression()?;
            self.expect(TokenType::RightParen)?;
            return Ok(Self::build_trim_call(None, vec![input]));
        }

        let start = self.save_pos();

        let mut spec: Option<&'static str> = None;
        let peek = self.peek().clone();
        if peek.ty == TokenType::Identifier {
            spec = match peek.literal.to_ascii_uppercase().as_str() {
                "BOTH" => Some("BOTH"),
                "LEADING" => Some("LEADING"),
                "TRAILING" => Some("TRAILING"),
                _ => None,
            };
            if spec.is_some() {
                self.next_token();
            }
        }

        // `[spec] FROM input` — no trim character string given.
        if self.peek_type() == TokenType::FROM {
            self.next_token(); // consume FROM
            let input = self.parse_expression()?;
            self.expect(TokenType::RightParen)?;
            return Ok(Self::build_trim_call(spec, vec![input]));
        }

        // `[spec] trimCharacterString FROM input` — or the plain form.
        match self.parse_expression() {
            Ok(chars_expr) if self.peek_type() == TokenType::FROM => {
                self.next_token(); // consume FROM
                let input = self.parse_expression()?;
                self.expect(TokenType::RightParen)?;
                Ok(Self::build_trim_call(spec, vec![input, chars_expr]))
            }
            Ok(chars_expr) if spec.is_none() => {
                // No spec word was consumed, so the already-parsed expression
                // *is* the plain trim(input) argument: a re-parse from
                // `start` would see the identical token stream, so reuse the
                // result instead of backtracking.
                self.expect(TokenType::RightParen)?;
                Ok(Self::build_trim_call(None, vec![chars_expr]))
            }
            Err(e) if spec.is_none() => {
                // Same reasoning: with nothing consumed since `start`, a
                // re-parse would fail identically — propagate directly.
                Err(e)
            }
            Ok(_) | Err(_) => {
                // A spec word (BOTH/LEADING/TRAILING) was tentatively
                // consumed but the spec forms didn't pan out: the word is an
                // ordinary variable (e.g. `trim(both)`), so backtrack and
                // parse the plain single-argument form including it. Only
                // reachable on invalid input — valid plain trims have no
                // top-level FROM and never enter the gated block — so this
                // re-parse is not on any well-formed hot path.
                self.restore_pos(start);
                let input = self.parse_expression()?;
                self.expect(TokenType::RightParen)?;
                Ok(Self::build_trim_call(None, vec![input]))
            }
        }
    }

    /// Lookahead from `self.pos` without consuming tokens: true when
    /// `target` appears at depth 0 before the closer that ends the
    /// construct opened just before `self.pos` (the caller has already
    /// consumed the opener). Only the bracket kinds in `brackets` count
    /// toward the depth, so a paren-only scan (inside a call's argument
    /// list) and an all-bracket scan (inside a comprehension brace, whose
    /// source expression can nest brackets and parens) are explicit
    /// per-site choices. The one scan behind
    /// [`Parser::trim_args_have_top_level_from`],
    /// [`Parser::pipe_before_closing_paren`], and
    /// [`Parser::has_pipe_before_matching_brace`].
    fn top_level_token_before_matching_close(
        &self,
        target: TokenType,
        brackets: BracketSet,
    ) -> bool {
        let mut depth = 0usize;
        let mut pos = self.pos;
        while pos < self.tokens.len() {
            let ty = self.tokens[pos].ty;
            if brackets.opens(ty) {
                depth += 1;
            } else if brackets.closes(ty) {
                if depth == 0 {
                    return false; // the construct's own closer
                }
                depth -= 1;
            } else if ty == TokenType::EOF {
                return false;
            } else if ty == target && depth == 0 {
                return true;
            }
            pos += 1;
        }
        false
    }

    /// Returns true if a `FROM` keyword token appears at this trim call's own
    /// paren depth before its closing `)` — i.e. whether the GQL
    /// `[spec] [chars] FROM input` forms are possible at all. Pure lookahead
    /// over the token stream (O(remaining tokens)); consumes nothing. A
    /// `FROM` nested inside a parenthesized sub-expression does not count;
    /// one inside brackets/braces at paren depth 0 may produce a harmless
    /// false positive (the caller then falls back to the speculative path,
    /// which behaves exactly as before) — never a false negative, since every
    /// real spec-form `FROM` sits at paren depth 0.
    pub(super) fn trim_args_have_top_level_from(&self) -> bool {
        self.top_level_token_before_matching_close(TokenType::FROM, BracketSet::Parens)
    }

    /// Parses `PROPERTY_EXISTS(<element variable reference>, <property name>)`
    /// (CIP-247). The GQL grammar restricts this to exactly two arguments: the
    /// first must be a variable (checked here, not a literal or compound
    /// expression), and the second is a bare property-name identifier — a
    /// distinct grammar slot, not an ordinary comma-separated expression, so a
    /// variable appearing there names the *literal* property (matching
    /// observed Neo4j behaviour) rather than being evaluated, and compound
    /// expressions (`a + b`, `toString(x)`) are rejected at parse time.
    /// Compiles down to a plain `property_exists(var, "propName")` call;
    /// evaluation lives in the engine (a generic property-access rewrite
    /// would wrongly accept maps, which this function must reject).
    pub(super) fn parse_property_exists_call(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume (
        let first = self.parse_expression()?;
        let var_name = match &first {
            Expr::Variable(v) => v.clone(),
            _ => {
                return Err(self.error(
                    "PROPERTY_EXISTS() first argument must be a variable referencing a node or relationship",
                ))
            }
        };
        self.expect(TokenType::Comma)?;
        let prop_tok = self.peek().clone();
        if !prop_tok.ty.is_ident_like() {
            return Err(self.error_at_token(
                &prop_tok,
                "PROPERTY_EXISTS() second argument must be a property name identifier",
            ));
        }
        self.next_token();
        if self.peek_type() != TokenType::RightParen {
            return Err(self.error(
                "PROPERTY_EXISTS() second argument must be a bare property name, not an expression",
            ));
        }
        self.next_token(); // consume )
        Ok(Expr::FunctionCall(FunctionCallExpr {
            name: "property_exists".to_string(),
            args: vec![
                Expr::Variable(var_name),
                Expr::Literal(LiteralValue::String(prop_tok.literal)),
            ],
            distinct: false,
        }))
    }

    /// Parses `vector(list)` or `vector(list, dimension, coordType)`
    /// (CIP-214), already having consumed the `VECTOR` keyword and
    /// confirmed the next token is `(`. The optional third argument is a
    /// bare coordinate-type keyword (`INT32`, `FLOAT`, ...), not an
    /// ordinary expression -- like `normalize()`'s normal form and
    /// `PROPERTY_EXISTS()`'s property name, it needs dedicated parsing so
    /// the static variable-scope validator doesn't reject it as an
    /// undefined variable reference. Only this construction shape is
    /// supported (as CIP-216 eval-suite test-data setup); full CIP-214
    /// `vector()` semantics are otherwise out of scope.
    pub(super) fn parse_vector_call(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume (
        if self.peek_type() == TokenType::RightParen {
            return Err(self.error("vector() requires at least 1 argument"));
        }
        let mut args = vec![self.parse_expression()?];
        if self.peek_type() == TokenType::Comma {
            self.next_token();
            args.push(self.parse_expression()?);
            self.expect(TokenType::Comma)?;
            let coord_tok = self.peek().clone();
            if !coord_tok.ty.is_ident_like() {
                return Err(self.error_at_token(
                    &coord_tok,
                    "vector() third argument must be a coordinate type keyword",
                ));
            }
            self.next_token();
            // "SIGNED INTEGER" (CIP-200's two-word alias for INTEGER) is the
            // only multi-word coordinate type keyword.
            let mut coord_name = coord_tok.literal.to_ascii_uppercase();
            if coord_name == "SIGNED" && self.peek_is_ident("INTEGER") {
                self.next_token();
                coord_name.push_str(" INTEGER");
            }
            args.push(Expr::Literal(LiteralValue::String(coord_name)));
        }
        self.expect(TokenType::RightParen)?;
        Ok(Expr::FunctionCall(FunctionCallExpr {
            name: "vector".to_string(),
            args,
            distinct: false,
        }))
    }

    pub(super) fn build_trim_call(spec: Option<&'static str>, args: Vec<Expr>) -> Expr {
        let name = match spec {
            Some("LEADING") => "trim_leading",
            Some("TRAILING") => "trim_trailing",
            _ => "trim",
        };
        Expr::FunctionCall(FunctionCallExpr {
            name: name.to_string(),
            args,
            distinct: false,
        })
    }

    pub(super) fn parse_add_sub(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_mul_div()?;
        loop {
            match self.peek_type() {
                TokenType::Plus => {
                    self.next_token();
                    let right = self.parse_mul_div()?;
                    left = Expr::Binary(Box::new(BinaryExpr {
                        left,
                        op: "+".to_string(),
                        right,
                    }));
                }
                TokenType::Dash => {
                    self.next_token();
                    let right = self.parse_mul_div()?;
                    left = Expr::Binary(Box::new(BinaryExpr {
                        left,
                        op: "-".to_string(),
                        right,
                    }));
                }
                TokenType::DoublePipe => {
                    self.next_token();
                    let right = self.parse_mul_div()?;
                    left = Expr::Binary(Box::new(BinaryExpr {
                        left,
                        op: "||".to_string(),
                        right,
                    }));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    pub(super) fn parse_mul_div(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_exponentiation()?;
        loop {
            match self.peek_type() {
                TokenType::Star => {
                    self.next_token();
                    let right = self.parse_exponentiation()?;
                    left = Expr::Binary(Box::new(BinaryExpr {
                        left,
                        op: "*".to_string(),
                        right,
                    }));
                }
                TokenType::Slash => {
                    self.next_token();
                    let right = self.parse_exponentiation()?;
                    left = Expr::Binary(Box::new(BinaryExpr {
                        left,
                        op: "/".to_string(),
                        right,
                    }));
                }
                TokenType::Percent => {
                    self.next_token();
                    let right = self.parse_exponentiation()?;
                    left = Expr::Binary(Box::new(BinaryExpr {
                        left,
                        op: "%".to_string(),
                        right,
                    }));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    pub(super) fn parse_exponentiation(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_unary()?;
        while self.peek_type() == TokenType::Caret {
            self.next_token();
            let right = self.parse_unary()?;
            left = Expr::Binary(Box::new(BinaryExpr {
                left,
                op: "^".to_string(),
                right,
            }));
        }
        Ok(left)
    }

    pub(super) fn parse_unary(&mut self) -> ParseResult<Expr> {
        if self.peek_type() == TokenType::Dash {
            // A double (or longer) unary minus applied directly to an
            // Infinity/Inf/NaN literal (no parens in between) is a compile-time
            // syntax error in Neo4j, even though `-Infinity` and
            // `-(-Infinity)` are both valid (CIP-55 deviation: `--Infinity` is
            // rejected while `--1` still parses). Detect a run of two or more
            // leading Dash tokens landing directly on such a literal.
            if self.pos + 1 < self.tokens.len() && self.tokens[self.pos + 1].ty == TokenType::Dash {
                let mut lookahead = self.pos + 1;
                while lookahead < self.tokens.len() && self.tokens[lookahead].ty == TokenType::Dash
                {
                    lookahead += 1;
                }
                if lookahead < self.tokens.len()
                    && matches!(
                        self.tokens[lookahead].ty,
                        TokenType::INFINITY | TokenType::NAN
                    )
                {
                    let bad_tok = self.tokens[lookahead].clone();
                    return Err(self.error_at_token(
                        &bad_tok,
                        format!("Invalid input '{}', expected: a number", bad_tok.literal),
                    ));
                }
            }
            self.next_token();
            // Special case: -<integer literal> for int64 min value
            if self.peek_type() == TokenType::Integer {
                let next = self.peek().clone();
                if parse_int_literal(&next.literal).is_err() {
                    let neg_literal = format!("-{}", next.literal);
                    if let Ok(val) = parse_int_literal(&neg_literal) {
                        self.next_token();
                        return Ok(Expr::Literal(LiteralValue::Integer(val)));
                    }
                }
            }
            // ADR-0257: unary `-` chains recurse here directly, bypassing
            // `parse_expression`, so each step must count separately
            // (LABS-69).
            let expr = self.with_expr_depth(|p| p.parse_unary())?;
            return Ok(Expr::Unary(Box::new(UnaryExpr {
                op: "-".to_string(),
                expr,
            })));
        }
        self.parse_postfix()
    }

    /// Walks the maximal `.ident` token chain starting at `from` (which must
    /// point at a `Dot`) and reports `(end, ends_in_call)`: `end` is the
    /// token index where the scan stopped — the `(` when the chain leads to
    /// a function call, otherwise the first token that broke the chain — and
    /// `ends_in_call` tells which. Pure lookahead over the token stream;
    /// consumes nothing. The result is constant for every dot position
    /// inside one maximal chain, which is what lets `parse_postfix` memoize
    /// it instead of rescanning the remaining tokens per dot.
    pub(super) fn scan_dot_chain_for_call(&self, from: usize) -> (usize, bool) {
        let mut pos = from;
        while pos < self.tokens.len() && self.tokens[pos].ty == TokenType::Dot {
            let ident = pos + 1;
            if ident >= self.tokens.len() || !self.tokens[ident].ty.is_schema_name() {
                return (ident, false);
            }
            let after = pos + 2;
            if after < self.tokens.len() && self.tokens[after].ty == TokenType::LeftParen {
                return (after, true);
            }
            if after >= self.tokens.len() || self.tokens[after].ty != TokenType::Dot {
                return (after, false);
            }
            pos = after;
        }
        (pos, false)
    }

    pub(super) fn parse_postfix(&mut self) -> ParseResult<Expr> {
        let primary = self.parse_primary()?;
        let ns = Self::extract_namespace(&primary);
        let mut base = PostfixBase::Built(primary, ns);
        // Memoized `scan_dot_chain_for_call` result: `(scan end, result)`.
        // Query positions (the dot after each consumed `.ident`) are strictly
        // increasing and the result is constant within one maximal chain, so
        // a cached `(end, res)` answers every later dot of the same chain
        // (`pos < end`) — each token is scanned at most once per
        // `parse_postfix` call rather than an O(remaining) lookahead per dot.
        let mut chain_scan: Option<(usize, bool)> = None;
        loop {
            match self.peek_type() {
                TokenType::Dot => {
                    self.next_token();
                    let prop_name = self.parse_schema_name("property name after '.'")?;
                    // Check for namespaced function call: expr.name(args) or expr.name.name(args)
                    if self.peek_type() == TokenType::LeftParen {
                        // Use the namespace path collected so far.
                        let path = match &mut base {
                            PostfixBase::Built(_, ns) => ns.take(),
                            PostfixBase::NsPath(path) => Some(std::mem::take(path)),
                        };
                        if let Some(mut func_name) = path {
                            func_name.push('.');
                            func_name.push_str(&prop_name);
                            self.next_token(); // consume (
                            let args = self.parse_arg_list()?;
                            base = PostfixBase::Built(
                                Expr::FunctionCall(FunctionCallExpr {
                                    name: func_name,
                                    args,
                                    distinct: false,
                                }),
                                None,
                            );
                            continue;
                        }
                    }
                    // Check for deeper nesting: if next is '.', this could be a longer namespace
                    // Only use namespace Variable if the chain eventually leads to a function call '('
                    if self.peek_type() == TokenType::Dot {
                        let carries_path = matches!(
                            &base,
                            PostfixBase::Built(_, Some(_)) | PostfixBase::NsPath(_)
                        );
                        if carries_path {
                            // Look ahead to see if this chain eventually leads to '('
                            let is_func_namespace = match chain_scan {
                                Some((end, res)) if self.pos < end => res,
                                _ => {
                                    let scanned = self.scan_dot_chain_for_call(self.pos);
                                    chain_scan = Some(scanned);
                                    scanned.1
                                }
                            };
                            if is_func_namespace {
                                // Accumulate the segment into the bare path,
                                // entering (or staying in) `NsPath`:
                                // materializing an `Expr` per segment cost
                                // Θ(n²) in string clones (a parse-time DoS on
                                // maximal inputs), and nothing can observe the
                                // intermediate value — the terminating `(` arm
                                // above rebuilds the callee name from the path.
                                match &mut base {
                                    PostfixBase::Built(_, ns) => {
                                        if let Some(mut path) = ns.take() {
                                            path.push('.');
                                            path.push_str(&prop_name);
                                            base = PostfixBase::NsPath(path);
                                            continue;
                                        }
                                    }
                                    PostfixBase::NsPath(path) => {
                                        path.push('.');
                                        path.push_str(&prop_name);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    // Plain property access; extend the running namespace path
                    // to keep it in sync (`extract_namespace` of a Property is
                    // the base namespace plus `.` plus the property).
                    base = match base {
                        PostfixBase::Built(expr, mut ns) => {
                            if let Some(ns_str) = ns.as_mut() {
                                ns_str.push('.');
                                ns_str.push_str(&prop_name);
                            }
                            PostfixBase::Built(
                                Expr::Property(Box::new(PropertyAccess {
                                    expr,
                                    property: prop_name,
                                    key_expr: None,
                                })),
                                ns,
                            )
                        }
                        // Unreachable for valid input: the scan admitted
                        // `NsPath` only for a chain ending in `(`, so
                        // mid-chain the token after `.ident` is always `.`
                        // or `(` — never a plain property hop. A violation
                        // is a typed internal error, never a panic.
                        PostfixBase::NsPath(_) => {
                            return Err(self.internal_error(
                                "a call-bound dot-chain only continues at '.' or '('",
                            ));
                        }
                    };
                }
                TokenType::LeftBracket => {
                    // Subscript results are never namespace paths. `NsPath`
                    // is unreachable here for valid input by the same scan
                    // invariant as the plain-property arm above; a violation
                    // is a typed internal error, never a panic.
                    let PostfixBase::Built(expr, _) = base else {
                        return Err(self.internal_error(
                            "a call-bound dot-chain only continues at '.' or '('",
                        ));
                    };
                    self.next_token(); // consume [
                                       // Check for [..to]
                    if self.peek_type() == TokenType::DotDot {
                        self.next_token(); // consume ..
                        let to = self.parse_expression()?;
                        self.expect(TokenType::RightBracket)?;
                        base = PostfixBase::Built(
                            Expr::Subscript(Box::new(SubscriptExpr {
                                expr,
                                index: None,
                                from: None,
                                to: Some(to),
                                slice: true,
                            })),
                            None,
                        );
                        continue;
                    }
                    let first = self.parse_expression()?;
                    if self.peek_type() == TokenType::DotDot {
                        self.next_token(); // consume ..
                        let to = if self.peek_type() != TokenType::RightBracket {
                            Some(self.parse_expression()?)
                        } else {
                            None
                        };
                        self.expect(TokenType::RightBracket)?;
                        base = PostfixBase::Built(
                            Expr::Subscript(Box::new(SubscriptExpr {
                                expr,
                                index: None,
                                from: Some(first),
                                to,
                                slice: true,
                            })),
                            None,
                        );
                    } else {
                        self.expect(TokenType::RightBracket)?;
                        base = PostfixBase::Built(
                            Expr::Subscript(Box::new(SubscriptExpr {
                                expr,
                                index: Some(first),
                                from: None,
                                to: None,
                                slice: false,
                            })),
                            None,
                        );
                    }
                }
                // Note: the `variable:<label-expr>` predicate used to be
                // parsed here (in `<postfix operator>`, tighter than `+`).
                // CIP-190 moves it to `parse_string_null_check`'s `<advanced
                // comparison predicate part 2>` level (looser than `+`,
                // alongside the `IS <label-expr>` spelling) -- see
                // label-expression-precedence.feature. `parse_postfix` no
                // longer consumes a bare `:` at all, so it correctly stops
                // and lets the caller's `<advanced comparison predicate part
                // 2>` check see it.
                TokenType::LeftBrace
                    if matches!(&base, PostfixBase::Built(Expr::Variable(_), _))
                        && self.peek_brace_is_map_projection() =>
                {
                    // `<variable> <map projection>` (openCypher's map
                    // projection is always rooted at a plain variable).
                    // Also how CIP-190's near-ambiguities resolve
                    // `exists {name: exists.name}` / `case {name:
                    // case.name}` in favor of the ordinary-variable
                    // reading once the EXISTS/COUNT/COLLECT/CASE primary
                    // handling has already decided the keyword isn't
                    // introducing a subquery/CASE expression here.
                    //
                    // Gated on the same `peek_brace_is_map_projection`
                    // lookahead used for EXISTS/COUNT/COLLECT: without it,
                    // this would also fire for CIP-146's `USE <graph-expr>
                    // { <query> }` top-level brace body (whose graph-expr
                    // parsing runs through this same general expression
                    // grammar) and try to parse the query body as map
                    // projection items, hard-erroring instead of leaving
                    // the `{` for `parse_braced_single_query`.
                    //
                    // A map projection is never a namespace path. The
                    // non-`Built` case is unreachable for valid input (the
                    // match guard above only matches a built variable); a
                    // violation is a typed internal error, never a panic.
                    let PostfixBase::Built(expr, _) = base else {
                        return Err(self.internal_error("the guard only matches a built variable"));
                    };
                    let items = self.parse_map_projection_items()?;
                    base = PostfixBase::Built(
                        Expr::MapProjection(Box::new(MapProjectionExpr { expr, items })),
                        None,
                    );
                }
                _ => break,
            }
        }
        match base {
            PostfixBase::Built(expr, _) => Ok(expr),
            // A call-bound dot-chain is exited only through its terminating
            // `(` arm, which leaves a `Built` base — so `NsPath` here is
            // unreachable for valid input; a violation is a typed internal
            // error, never a panic.
            PostfixBase::NsPath(_) => {
                Err(self.internal_error("a call-bound dot-chain ends at its '(' arm"))
            }
        }
    }

    /// Builds the one-clause `MATCH` `Statement` wrapping a subquery
    /// expression's bare-pattern form (`EXISTS { pattern [WHERE expr] }` /
    /// `COUNT { pattern [WHERE expr] }`), shared by `parse_exists_subquery`
    /// and `parse_count_subquery`.
    fn bare_pattern_match_statement(
        pattern: Vec<PatternPath>,
        where_expr: Option<Expr>,
    ) -> Statement {
        Statement {
            clauses: vec![Clause::Match(MatchClause {
                optional: false,
                match_mode: MatchMode::DifferentRelationships,
                pattern,
                where_expr,
                search: None,
            })],
            ..Statement::default()
        }
    }

    /// `EXISTS { ... }` subquery expression -- the two grammar forms (bare
    /// pattern, or a full statement of one-or-more clauses), mirrored by
    /// `parse_count_subquery` with row counting instead of the existence
    /// test. Kept out-of-line (not inlined into `parse_primary`) so its
    /// locals don't inflate that function's stack frame, which is on the
    /// hot recursive path for deeply nested expressions (e.g. TCK's 40-deep
    /// nested list literals). The `{` is the current token; the caller has
    /// already ruled out a map projection (`peek_brace_is_map_projection`).
    #[inline(never)]
    pub(super) fn parse_exists_subquery(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume {
        let peek = self.peek().clone();
        if peek.ty == TokenType::LeftParen {
            // Simple pattern: EXISTS { pattern }
            let pattern = self.parse_pattern()?;
            let where_expr = if self.peek_type() == TokenType::WHERE {
                self.next_token();
                Some(self.parse_expression()?)
            } else {
                None
            };
            self.expect(TokenType::RightBrace)?;
            let stmt = Self::bare_pattern_match_statement(pattern, where_expr);
            return Ok(Expr::ExistsSubquery(Box::new(ExistsSubqueryExpr {
                statement: stmt,
            })));
        }
        let stmt = self.parse_statement_block()?;
        self.expect(TokenType::RightBrace)?;
        // CIP-63: unlike COUNT, a distinct UNION inside EXISTS
        // may omit RETURN in every branch (see the shared
        // helper's doc comment).
        self.check_subquery_union_consistency(&stmt, false)?;
        Ok(Expr::ExistsSubquery(Box::new(ExistsSubqueryExpr {
            statement: stmt,
        })))
    }

    /// `COUNT { ... }` subquery expression -- mirrors `EXISTS { ... }`'s two
    /// grammar forms (bare pattern, or a full statement of one-or-more
    /// clauses), just counting rows instead of testing non-emptiness. Kept
    /// out-of-line (not inlined into `parse_primary`) so its locals don't
    /// inflate that function's stack frame, which is on the hot recursive
    /// path for deeply nested expressions (e.g. TCK's 40-deep nested list
    /// literals).
    #[inline(never)]
    pub(super) fn parse_count_subquery(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume {
        if self.peek_type() == TokenType::RightBrace {
            // CIP-31 count_standalone[6]: `COUNT { }` is a compile-time
            // syntax error, not a valid (empty) subquery -- unlike EXISTS
            // and COLLECT, which this batch leaves alone (out of scope).
            return Err(
                self.error("InvalidCountExpression: COUNT {} must contain a pattern or a query")
            );
        }
        let peek = self.peek().clone();
        if peek.ty == TokenType::LeftParen {
            let pattern = self.parse_pattern()?;
            let where_expr = if self.peek_type() == TokenType::WHERE {
                self.next_token();
                Some(self.parse_expression()?)
            } else {
                None
            };
            self.expect(TokenType::RightBrace)?;
            let stmt = Self::bare_pattern_match_statement(pattern, where_expr);
            return Ok(Expr::CountSubquery(Box::new(CountSubqueryExpr {
                statement: stmt,
            })));
        }
        let stmt = self.parse_statement_block()?;
        self.expect(TokenType::RightBrace)?;
        // CIP-66's own UNION rules (verified against Neo4j): a *distinct*
        // UNION inside COUNT can never omit RETURN in any branch (its
        // dedup needs comparable, named columns), unlike EXISTS below,
        // which only cares about row existence and has no such need.
        self.check_subquery_union_consistency(&stmt, true)?;
        Ok(Expr::CountSubquery(Box::new(CountSubqueryExpr {
            statement: stmt,
        })))
    }

    /// CIP-63 (EXISTS) / CIP-66 (COUNT): shared UNION-consistency check for
    /// a subquery body's `<RegularQuery>`. Now that `parse_regular_query`
    /// lets UNION parse inside these bodies at all, the semantic rules both
    /// CIPs spell out for RETURN omission across UNION branches need
    /// enforcing here, or a UNION that omission rules forbid would silently
    /// "succeed" instead of raising the compile-time error the doc requires:
    /// (1) if any branch has an explicit RETURN, every branch must have one
    /// -- RETURN omission is all-or-nothing "for consistency"; (2) when
    /// `reject_omitted_distinct` is set (COUNT, CIP-66: a distinct union
    /// needs named columns to compare rows for deduplication), a plain
    /// `UNION` (not `UNION ALL`) can never omit RETURN, even consistently
    /// -- CIP-63 verified against Neo4j that EXISTS has no such
    /// restriction, since existence doesn't care about column identity, so
    /// callers checking an EXISTS body pass `false`; (3) when every branch
    /// does have a RETURN, they must all project the same column names --
    /// the general (pre-existing) `DifferentColumnsInUnion` rule, which a
    /// UNION that failed to parse at all inside these subqueries had been
    /// trivially "satisfying" before.
    pub(super) fn check_subquery_union_consistency(
        &self,
        stmt: &Statement,
        reject_omitted_distinct: bool,
    ) -> ParseResult<()> {
        if stmt.union.is_none() {
            return Ok(());
        }

        fn has_return(s: &Statement) -> bool {
            matches!(tail_stage(s).clauses.last(), Some(Clause::Return(_)))
        }
        // `None` covers both "no RETURN" and "RETURN *" (whose columns
        // can't be compared by name); only `Some` sides are compared,
        // mirroring the engine's top-level `get_return_columns` leniency.
        fn return_columns(s: &Statement) -> Option<Vec<String>> {
            match tail_stage(s).clauses.last() {
                Some(Clause::Return(rc)) => {
                    let mut cols = Vec::with_capacity(rc.items.len());
                    for item in &rc.items {
                        if item.all {
                            return None;
                        }
                        if let Some(alias) = &item.alias {
                            cols.push(alias.clone());
                        } else if let Expr::Variable(v) = &item.expr {
                            cols.push(v.clone());
                        } else if let Some(src) = &item.source_text {
                            cols.push(src.clone());
                        } else {
                            cols.push(String::new());
                        }
                    }
                    Some(cols)
                }
                _ => None,
            }
        }

        let mut branches: Vec<&Statement> = vec![stmt];
        let mut distinct_link_seen = false;
        let mut current = &stmt.union;
        while let Some(u) = current {
            if !u.all {
                distinct_link_seen = true;
            }
            branches.push(&u.statement);
            current = &u.statement.union;
        }

        let any_missing = branches.iter().any(|s| !has_return(s));
        let any_present = branches.iter().any(|s| has_return(s));

        if any_missing && any_present {
            return Err(self.error(
                "DifferentColumnsInUnion: all UNION branches must consistently include or omit RETURN",
            ));
        }
        if any_missing && distinct_link_seen && reject_omitted_distinct {
            return Err(self.error(
                "InvalidUnionOmittedReturn: a distinct UNION requires an explicit RETURN in every branch",
            ));
        }
        if !any_missing {
            let mut reference: Option<Vec<String>> = None;
            for s in &branches {
                if let Some(cols) = return_columns(s) {
                    match &reference {
                        None => reference = Some(cols),
                        Some(r) if *r != cols => {
                            return Err(self.error(
                                "DifferentColumnsInUnion: all UNION branches must have the same column names",
                            ));
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// `COLLECT { <RegularQuery> } (CIP-71): mirrors `parse_count_subquery`'s
    /// full-statement path (no bare-pattern shorthand -- COLLECT always
    /// requires a full `<RegularQuery>` ending in `RETURN`). Enforces the
    /// doc's syntactic restrictions that are checkable on the parsed shape
    /// alone: every branch of the (possibly UNION-chained) inner query must
    /// end with a `RETURN` clause projecting exactly one non-`*` item.
    /// Restrictions that require scope-aware analysis (no updating clauses,
    /// no shadowing outer variables, inner variables not leaking outward)
    /// are deferred to the eval/validation batch that adds COLLECT
    /// evaluation -- pure re-use of `EvalContext`/`validate.rs` machinery,
    /// not a parsing concern.
    #[inline(never)]
    pub(super) fn parse_collect_subquery(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume {
        if self.peek_type() == TokenType::RightBrace {
            return Err(self.error("A COLLECT subquery must end with a single return column"));
        }
        let stmt = self.parse_statement_block()?;
        self.expect(TokenType::RightBrace)?;
        self.check_collect_return_shape(&stmt)?;
        Ok(Expr::CollectSubquery(Box::new(CollectSubqueryExpr {
            statement: stmt,
        })))
    }

    /// Checks that `stmt` and every UNION-chained branch after it ends in a
    /// `RETURN` clause with exactly one item and that the item isn't `*`.
    pub(super) fn check_collect_return_shape(&self, stmt: &Statement) -> ParseResult<()> {
        let bad_shape = || self.error("A COLLECT subquery must end with a single return column");
        let mut current = stmt;
        loop {
            // CIP-213: the "must end with a single return column" rule
            // applies to this arm's *true* terminal stage -- see
            // [`tail_stage`].
            let tail = tail_stage(current);
            match tail.clauses.last() {
                Some(Clause::Return(rc)) if rc.items.len() == 1 && !rc.items[0].all => {}
                _ => return Err(bad_shape()),
            }
            match &current.union {
                Some(next) => current = &next.statement,
                None => return Ok(()),
            }
        }
    }

    pub(super) fn parse_primary(&mut self) -> ParseResult<Expr> {
        let tok = self.peek().clone();

        match tok.ty {
            TokenType::Integer => {
                self.next_token();
                let val = parse_int_literal(&tok.literal).map_err(|_| {
                    self.error_at_token(&tok, format!("integer overflow: {}", tok.literal))
                        .with_detail("IntegerOverflow")
                })?;
                Ok(Expr::Literal(LiteralValue::Integer(val)))
            }
            TokenType::Float => {
                self.next_token();
                let val: f64 = tok.literal.parse().map_err(|_| {
                    self.error_at_token(&tok, format!("invalid float literal: {}", tok.literal))
                })?;
                // A numeric literal (as opposed to the Infinity/NaN keyword
                // literals, which are their own token types) that overflows
                // to +-infinity is a compile-time error, not a valid way to
                // spell Infinity (CIP-52 NOTES.md: `RETURN 1e400` is rejected
                // by live Neo4j with "numeric value out of range").
                if val.is_infinite() {
                    return Err(self
                        .error_at_token(
                            &tok,
                            "FloatingPointOverflow: float literal is too large".to_string(),
                        )
                        .with_detail("FloatingPointOverflow"));
                }
                Ok(Expr::Literal(LiteralValue::Float(val)))
            }
            TokenType::String => {
                self.next_token();
                Ok(Expr::Literal(LiteralValue::String(tok.literal)))
            }
            TokenType::InterpString => {
                self.next_token();
                let parts = parse_interp_body(&tok.literal)
                    .map_err(|e| self.error_at_token(&tok, e).with_detail("UnexpectedSyntax"))?;
                Ok(Expr::StringInterp(parts))
            }
            TokenType::TRUE => {
                self.next_token();
                Ok(Expr::Literal(LiteralValue::Bool(true)))
            }
            TokenType::FALSE => {
                self.next_token();
                Ok(Expr::Literal(LiteralValue::Bool(false)))
            }
            TokenType::NULL => {
                self.next_token();
                Ok(Expr::Literal(LiteralValue::Null))
            }
            TokenType::INFINITY => {
                self.next_token();
                Ok(Expr::Literal(LiteralValue::Float(f64::INFINITY)))
            }
            TokenType::NAN => {
                self.next_token();
                Ok(Expr::Literal(LiteralValue::Float(f64::NAN)))
            }
            TokenType::Parameter => {
                self.next_token();
                Ok(Expr::Parameter(tok.literal))
            }
            TokenType::LeftBracket => self.parse_list_literal(),
            TokenType::LeftBrace => {
                // CIP-272: `{k : v IN map [WHERE p] | k' : v'}` and
                // `{item IN list [WHERE p] | k' : v'}` are map comprehensions;
                // anything else (including `{k: v IN m}`, whose value is a
                // genuine IN expression) stays a map literal.
                if let Some(mc) = self.try_parse_map_comprehension()? {
                    return Ok(mc);
                }
                let pairs = self.parse_map_literal()?;
                Ok(Expr::Map(pairs))
            }
            TokenType::LeftParen => {
                if self.node_pattern_precedes_relationship() {
                    return self.parse_pattern_predicate();
                }
                self.next_token();
                let expr = self.parse_expression()?;
                self.expect(TokenType::RightParen)?;
                Ok(Expr::Paren(Box::new(expr)))
            }
            TokenType::COUNT => {
                self.next_token();
                if self.peek_type() == TokenType::LeftBrace && !self.peek_brace_is_map_projection()
                {
                    return self.parse_count_subquery();
                }
                if self.peek_type() != TokenType::LeftParen {
                    return Ok(Expr::Variable(tok.literal));
                }
                self.next_token(); // consume (
                if self.peek_type() == TokenType::Star {
                    self.next_token();
                    self.expect(TokenType::RightParen)?;
                    return Ok(Expr::CountStar);
                }
                let distinct = if self.peek_type() == TokenType::DISTINCT {
                    self.next_token();
                    true
                } else if self.peek_type() == TokenType::ALL {
                    // ALL is the explicit (default) set-quantifier; semantically
                    // equivalent to omitting the quantifier entirely.
                    self.next_token();
                    false
                } else {
                    false
                };
                let arg = self.parse_expression()?;
                self.expect(TokenType::RightParen)?;
                Ok(Expr::FunctionCall(FunctionCallExpr {
                    name: "count".to_string(),
                    args: vec![arg],
                    distinct,
                }))
            }
            TokenType::COLLECT => {
                self.next_token();
                if self.peek_type() == TokenType::LeftBrace && !self.peek_brace_is_map_projection()
                {
                    return self.parse_collect_subquery();
                }
                if self.peek_type() != TokenType::LeftParen {
                    return Ok(Expr::Variable(tok.literal));
                }
                self.next_token(); // consume (
                let distinct = if self.peek_type() == TokenType::DISTINCT {
                    self.next_token();
                    true
                } else if self.peek_type() == TokenType::ALL {
                    self.next_token();
                    false
                } else {
                    false
                };
                let args = self.parse_arg_list()?;
                Ok(Expr::FunctionCall(FunctionCallExpr {
                    name: tok.literal,
                    args,
                    distinct,
                }))
            }
            TokenType::CASE => {
                // `case` can be an ordinary bound variable (CIP-190
                // near-ambiguities.feature [1]/[3]): not actually ambiguous
                // with a real CASE expression, since the latter must
                // eventually reach a WHEN clause (bare `CASE` -> searched
                // form) or successfully parse a test-expression followed by
                // WHEN (simple form). Try the real grammar first (matching
                // "let the first successful alternative win"); if it fails
                // to parse, backtrack and treat the token as a plain
                // variable reference instead, letting `parse_postfix` see
                // any trailing `<map projection>` (`case {name:
                // case.name}`, near-ambiguities.feature [3]).
                let saved = self.save_pos();
                match self.parse_case_expr() {
                    Ok(expr) => Ok(expr),
                    Err(_) => {
                        self.restore_pos(saved);
                        self.next_token(); // consume CASE as an identifier
                        Ok(Expr::Variable(tok.literal))
                    }
                }
            }
            TokenType::ALL => self.parse_quantifier_or_function(),
            TokenType::Identifier => {
                self.next_token();
                let name_lower = tok.literal.to_ascii_lowercase();

                // Quantifiers
                if (name_lower == "any" || name_lower == "none" || name_lower == "single")
                    && self.peek_type() == TokenType::LeftParen
                {
                    self.backup();
                    return self.parse_quantifier_or_function();
                }

                // allReduce(acc = init, x IN list | expr, predicate)
                if name_lower == "allreduce" && self.peek_type() == TokenType::LeftParen {
                    return self.parse_allreduce_expr();
                }

                // shortestPath / allShortestPaths
                if (name_lower == "shortestpath" || name_lower == "allshortestpaths")
                    && self.peek_type() == TokenType::LeftParen
                {
                    self.next_token(); // consume (
                    let pattern = self.parse_pattern_path()?;
                    self.expect(TokenType::RightParen)?;
                    return Ok(Expr::ShortestPath(Box::new(ShortestPathExpr {
                        pattern,
                        all: name_lower == "allshortestpaths",
                    })));
                }

                // normalize(input [, normalForm]) — the optional second argument
                // is a bare normal-form keyword (NFC/NFD/NFKC/NFKD), not an
                // ordinary expression (CIP-132), so it needs dedicated parsing.
                if name_lower == "normalize" && self.peek_type() == TokenType::LeftParen {
                    return self.parse_normalize_call();
                }

                // trim([[LEADING|TRAILING|BOTH] [trimCharacterString] FROM] input) —
                // the GQL TRIM special form (CIP-149) is a distinct grammar production
                // from an ordinary comma-separated argument list.
                if name_lower == "trim" && self.peek_type() == TokenType::LeftParen {
                    return self.parse_trim_call();
                }

                // PROPERTY_EXISTS(<element variable>, <property name>) (CIP-247) —
                // the second argument is a bare property-name identifier, not an
                // ordinary expression, so it needs dedicated parsing.
                if name_lower == "property_exists" && self.peek_type() == TokenType::LeftParen {
                    return self.parse_property_exists_call();
                }

                // Function call
                if self.peek_type() == TokenType::LeftParen {
                    self.next_token(); // consume (
                    let distinct = if self.peek_type() == TokenType::DISTINCT {
                        self.next_token();
                        true
                    } else if self.peek_type() == TokenType::ALL {
                        // ALL is the explicit (default) set-quantifier; semantically
                        // equivalent to omitting the quantifier entirely.
                        self.next_token();
                        false
                    } else {
                        false
                    };
                    let args = self.parse_arg_list()?;
                    return Ok(Expr::FunctionCall(FunctionCallExpr {
                        name: tok.literal,
                        args,
                        distinct,
                    }));
                }

                Ok(Expr::Variable(tok.literal))
            }
            TokenType::VECTOR => {
                self.next_token();
                if self.peek_type() == TokenType::LeftParen {
                    return self.parse_vector_call();
                }
                Ok(Expr::Variable(tok.literal))
            }
            TokenType::NODE
            | TokenType::REL
            | TokenType::POINT
            | TokenType::INDEX
            | TokenType::SPATIAL
            | TokenType::FROM
            // CIP-187: `options` is a real SHOW INDEXES/SHOW CONSTRAINTS
            // YIELD column name (e.g. `SHOW CONSTRAINTS YIELD options
            // RETURN options`), so it must be usable as a bare variable
            // reference like the other non-reserved schema/DDL keywords
            // above, not just in alias/YIELD-name position.
            | TokenType::OPTIONS
            // CIP-123: `constraint` (and similarly-shaped schema keywords)
            // is a non-reserved word usable as an ordinary `YIELD ... AS
            // constraint` alias / variable reference, e.g. when composing
            // `SHOW CONSTRAINTS YIELD name AS constraint WHERE constraint
            // = ...` -- same treatment as `TokenType::INDEX` above.
            | TokenType::CONSTRAINT
            // CIP-150 NOTES.md Q2: `RETURN` is a non-reserved word usable
            // as an ordinary variable reference (e.g. `RETURN RETURN AS
            // r`, where the second token names the variable bound by an
            // earlier `WITH ... AS RETURN`) -- reached here only when a
            // primary expression was already expected, so a `RETURN`
            // starting a fresh top-level clause is never affected.
            | TokenType::RETURN => {
                self.next_token();
                if self.peek_type() == TokenType::LeftParen {
                    self.next_token(); // consume (
                    let args = self.parse_arg_list()?;
                    return Ok(Expr::FunctionCall(FunctionCallExpr {
                        name: tok.literal,
                        args,
                        distinct: false,
                    }));
                }
                Ok(Expr::Variable(tok.literal))
            }
            TokenType::EXISTS => {
                self.next_token();
                if self.peek_type() == TokenType::LeftBrace && !self.peek_brace_is_map_projection()
                {
                    return self.parse_exists_subquery();
                }
                if self.peek_type() == TokenType::LeftParen {
                    self.next_token();
                    let args = self.parse_arg_list()?;
                    return Ok(Expr::FunctionCall(FunctionCallExpr {
                        name: "exists".to_string(),
                        args,
                        distinct: false,
                    }));
                }
                Ok(Expr::Variable(tok.literal))
            }
            // ADR-0191: the reduce form only applies when `(` follows; a
            // bare `REDUCE` is a plain variable reference owned by the
            // schema-name fallthrough below. The form itself (and its
            // accumulator/loop variables) is parsed by `parse_reduce_expr`,
            // which consumes REDUCE.
            TokenType::REDUCE if self.peek_at(1).ty == TokenType::LeftParen => {
                self.parse_reduce_expr()
            }
            _ if tok.ty.is_schema_name() => {
                // ADR-0191: the primary-expression variable read -- any
                // keyword token that reached this fallthrough is a variable
                // reference, spelling kept as written. Every arm above has
                // priority: the TRUE/FALSE/NULL/INF/INFINITY/NAN literals,
                // prefix NOT, the CASE/COUNT/EXISTS/COLLECT/REDUCE/VECTOR
                // forms, the ALL quantifier/function, the contextual schema
                // words and the DISTINCT/ALL set quantifiers their callers
                // consume.
                self.next_token();
                Ok(Expr::Variable(tok.literal))
            }
            _ => Err(self.error_at_token(
                &tok,
                format!(
                    "unexpected token {:?} ({:?}) at line {}, col {}",
                    tok.literal, tok.ty, tok.line, tok.col
                ),
            )),
        }
    }

    pub(super) fn parse_quantifier_or_function(&mut self) -> ParseResult<Expr> {
        let tok = self.next_token();
        let name_lower = tok.literal.to_ascii_lowercase();

        if self.peek_type() == TokenType::LeftParen {
            self.next_token(); // consume (
            let var_tok = self.peek().clone();
            // The quantifier variable is a symbolic name, so any keyword
            // spelling works unquoted too (ADR-0191).
            if var_tok.ty.is_schema_name() {
                self.next_token(); // consume variable
                if self.peek_type() == TokenType::IN {
                    self.next_token(); // consume IN
                    let list_expr = self.parse_expression()?;
                    // LABS-201: the GQL pipe spelling `any(x IN list | pred)`
                    // -- openCypher binds the predicate with WHERE. The pipe
                    // used to fall through to `expect(RightParen)` below and
                    // surface as a bare `expected RightParen, got |`; name
                    // the fix instead.
                    if self.peek_type() == TokenType::Pipe {
                        return Err(self.error(format!(
                            "a list predicate introduces its predicate with WHERE, not '|': \
                             write {name_lower}(x IN list WHERE predicate)"
                        )));
                    }
                    let predicate = if self.peek_type() == TokenType::WHERE {
                        self.next_token();
                        Some(self.parse_expression()?)
                    } else {
                        None
                    };
                    self.expect(TokenType::RightParen)?;
                    return Ok(Expr::Quantifier(Box::new(QuantifierExpr {
                        quantifier: name_lower,
                        variable: var_tok.literal,
                        list: list_expr,
                        predicate,
                    })));
                }
                // Not IN - backup variable
                self.backup();
            }
            // Not a quantifier shape (no `var IN <list>` prefix): parse as a
            // regular function call. The cursor sits after `(` at the first
            // argument on this path, whichever backup ran above.
            // LABS-201: `any((a)-[:R]->(b) | pred)` -- a pattern used
            // directly inside a list predicate. The pattern argument parses
            // as a pattern predicate and then the pipe fails
            // `expect(RightParen)` with the bare `expected RightParen, got |`
            // and no guidance. Detect the shape up front (the argument starts
            // with a node pattern and a pipe follows before the closing
            // paren) and point at the EXISTS requirement. Without the pipe
            // (e.g. `any((a)-[:R]->(b))`) the argument parses on and the
            // validator already reports `pattern predicates are only valid
            // in WHERE`, so this lookahead must not hijack it.
            if self.peek_type() == TokenType::LeftParen
                && self.node_pattern_precedes_relationship()
                && self.pipe_before_closing_paren()
            {
                return Err(self.error(
                    "a pattern cannot be used directly inside a list predicate: wrap it in \
                     EXISTS { ... } (e.g. EXISTS { (a)-[:R]->(b) }) or bind a pattern \
                     comprehension first (any(x IN [(a)-[:R]->(b) | b.prop] WHERE predicate))",
                ));
            }
            let distinct = if self.peek_type() == TokenType::DISTINCT {
                self.next_token();
                true
            } else {
                false
            };
            let args = self.parse_arg_list()?;
            return Ok(Expr::FunctionCall(FunctionCallExpr {
                name: tok.literal,
                args,
                distinct,
            }));
        }
        Ok(Expr::Variable(tok.literal))
    }

    pub(super) fn parse_arg_list(&mut self) -> ParseResult<Vec<Expr>> {
        let mut args = Vec::new();
        if self.peek_type() == TokenType::RightParen {
            self.next_token();
            return Ok(args);
        }
        loop {
            args.push(self.parse_expression()?);
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        self.expect(TokenType::RightParen)?;
        Ok(args)
    }

    pub(super) fn parse_list_literal(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume [

        if self.peek_type() == TokenType::RightBracket {
            self.next_token();
            return Ok(Expr::List(Vec::new()));
        }

        let first = self.peek().clone();
        // A list/pattern comprehension's own loop/path variable is a
        // symbolic name (ADR-0191), so any keyword spelling works unquoted
        // too. This resolves `[true IN [1]]` in favour of the comprehension
        // (it returns `[1]`, matching Neo4j), not a one-element list
        // literal holding a boolean `IN` expression.
        if first.ty.is_schema_name() {
            self.next_token(); // consume identifier
            if self.peek_type() == TokenType::IN {
                // List comprehension
                let variable = first.literal;
                self.next_token(); // consume IN
                let list_expr = self.parse_expression()?;

                let where_expr = if self.peek_type() == TokenType::WHERE {
                    self.next_token();
                    Some(self.parse_comprehension_where_expr()?)
                } else {
                    None
                };

                let map_expr = if self.peek_type() == TokenType::Pipe {
                    self.next_token();
                    Some(self.parse_expression()?)
                } else {
                    None
                };

                self.expect(TokenType::RightBracket)?;
                return Ok(Expr::ListComprehension(Box::new(ListComprehensionExpr {
                    variable,
                    list_expr,
                    where_expr,
                    map_expr,
                })));
            }

            // Pattern comprehension with path variable: [p = (n)-->() | expr].
            // ADR-0191: a keyword spelling is a path variable too (`[MATCH =
            // (n)-->() | n]`). The pattern-comprehension reading only applies
            // when a pattern path really follows the `=` -- decided by
            // speculatively running the real `parse_pattern_path` reader and
            // keeping the reading only when a `|` or `WHERE` follows the
            // pattern (`pattern_comprehension_follows_equals`); otherwise the
            // list-literal reading wins, so `[true = x]`, `[null = x]`,
            // `[true = (1 = 1)]` and `[x = 1]` stay list literals (matching
            // Neo4j, where a pattern never follows a literal).
            if self.peek_type() == TokenType::Equals && self.pattern_comprehension_follows_equals()
            {
                let path_var = first.literal;
                self.next_token(); // consume =
                return self.parse_pattern_comprehension_body(Some(path_var));
            }

            // Not a list comprehension, backup and parse as regular list
            self.backup();
        }

        // Pattern comprehension without path variable: [(n)-->() | expr]
        if first.ty == TokenType::LeftParen {
            // Use lookahead to check if this looks like a pattern comprehension.
            // A pattern comprehension starts with a node pattern (a) followed by a relationship.
            if self.node_pattern_precedes_relationship() {
                return self.parse_pattern_comprehension_body(None);
            }
        }

        // Regular list literal
        let mut elements = Vec::new();
        loop {
            elements.push(self.parse_expression()?);
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        self.expect(TokenType::RightBracket)?;
        Ok(Expr::List(elements))
    }

    /// Speculative gate for the `=` of a would-be pattern comprehension
    /// (`[pathVar = <pattern> | expr]`, ADR-0191): true when the tokens
    /// after the `=` (the current token must be that `=`) parse as a
    /// complete pattern path through the real `parse_pattern_path` reader
    /// AND the token after the pattern is `|` or `WHERE` (the
    /// comprehension's own grammar). Because the gate runs the real reader,
    /// everything `parse_pattern_path` accepts is recognized here -- label
    /// expressions and GPM forms (`(n:A|B)-->()`, `(IS A&B)-->()`), bare
    /// node or juxtaposed bodies (`(n)`, `(a)(b)`), parenthesized subpaths
    /// (`((q = (a)-->(b)))`) and the `shortestPath(...)` wrapper alike.
    ///
    /// The probe is invisible when it does not commit: the token position
    /// and every clause-level side-effect flag the parse can set (the
    /// `label_saw_*`/`quantifier_saw_*` mixing flags) are restored before
    /// returning, so a negative verdict leaves the list-literal reading to
    /// run exactly as if the probe never happened. A positive verdict is
    /// re-parsed for real by the caller (`parse_pattern_comprehension_body`),
    /// which re-applies those side effects.
    ///
    /// The verdict is memoized per `=` token position
    /// (`Parser::pattern_comprehension_probe_memo`): it depends only on
    /// that position, and without the memo a nested
    /// `[v = (n {p: ...}) | ...]` shape re-probes the whole nested term at
    /// every level -- 2^k parse cost on a sub-KB query (LABS-183).
    fn pattern_comprehension_follows_equals(&mut self) -> bool {
        debug_assert_eq!(self.peek_type(), TokenType::Equals);
        let key = self.pos;
        if let Some(&verdict) = self.pattern_comprehension_probe_memo.get(&key) {
            return verdict;
        }
        let saved_pos = self.save_pos();
        let saved_flags = self.side_effects;
        self.next_token(); // consume =
        let probe = self.parse_pattern_path();
        let is_comprehension =
            probe.is_ok() && matches!(self.peek_type(), TokenType::Pipe | TokenType::WHERE);
        self.restore_pos(saved_pos);
        self.side_effects = saved_flags;
        self.pattern_comprehension_probe_memo
            .insert(key, is_comprehension);
        is_comprehension
    }

    /// Lookahead from a `(` without consuming tokens: true when the tokens at
    /// the current position form a node pattern `( var? (:label)* {…}? )`
    /// followed by `-` or `<-`. That relationship start is what tells a
    /// pattern (a pattern comprehension after `[`, a pattern predicate such
    /// as `WHERE (n)-->()`) apart from a bracketed or parenthesized
    /// expression.
    pub(super) fn node_pattern_precedes_relationship(&self) -> bool {
        self.node_pattern_precedes_relationship_at(0)
    }

    /// [`node_pattern_precedes_relationship`] starting `offset` tokens ahead
    /// of the current position (0 == the current token).
    fn node_pattern_precedes_relationship_at(&self, offset: usize) -> bool {
        let mut pos = self.pos + offset;

        // Must start with (
        if pos >= self.tokens.len() || self.tokens[pos].ty != TokenType::LeftParen {
            return false;
        }
        pos += 1;

        // Skip optional variable. ADR-0191: a keyword spelling is a node
        // variable too, so this mirrors the reader (`parse_node_pattern`),
        // including the `WHERE`/`IS` one-token lookahead that keeps their
        // inline-predicate / label-introducer meanings.
        // (`peek_pattern_name_is_variable` folds in the symbolic-name check.)
        if self.peek_pattern_name_is_variable(pos - self.pos) {
            pos += 1;
        }

        // Skip labels (a keyword is a label name too, ADR-0169)
        while pos < self.tokens.len() && self.tokens[pos].ty == TokenType::Colon {
            pos += 1;
            if pos < self.tokens.len() && self.tokens[pos].ty.is_schema_name() {
                pos += 1;
            } else {
                return false;
            }
        }

        // Skip optional properties { ... }
        if pos < self.tokens.len() && self.tokens[pos].ty == TokenType::LeftBrace {
            let mut depth = 1;
            pos += 1;
            while depth > 0 && pos < self.tokens.len() {
                match self.tokens[pos].ty {
                    TokenType::LeftBrace => depth += 1,
                    TokenType::RightBrace => depth -= 1,
                    TokenType::EOF => return false,
                    _ => {}
                }
                pos += 1;
            }
        }

        // Must see )
        if pos >= self.tokens.len() || self.tokens[pos].ty != TokenType::RightParen {
            return false;
        }
        pos += 1;

        // After ), must see - or <- (indicating a relationship/pattern)
        if pos < self.tokens.len() {
            matches!(self.tokens[pos].ty, TokenType::Dash | TokenType::ArrowLeft)
        } else {
            false
        }
    }

    /// Lookahead from the argument-start position without consuming tokens:
    /// true when a `|` appears at the argument's own paren depth before the
    /// paren that closes the current argument list. That pipe is the tail of
    /// the agent form `any((a)-[:R]->(b) | pred)` (LABS-201) -- a
    /// comprehension's map-expression separator misplaced outside any
    /// `[...]`, where no valid parse ever has one.
    fn pipe_before_closing_paren(&self) -> bool {
        self.top_level_token_before_matching_close(TokenType::Pipe, BracketSet::Parens)
    }

    pub(super) fn parse_pattern_comprehension_body(
        &mut self,
        path_var: Option<String>,
    ) -> ParseResult<Expr> {
        let pattern = self.parse_pattern_path()?;

        let where_expr = if self.peek_type() == TokenType::WHERE {
            self.next_token();
            Some(self.parse_comprehension_where_expr()?)
        } else {
            None
        };

        self.expect(TokenType::Pipe)?;
        let map_expr = self.parse_expression()?;
        self.expect(TokenType::RightBracket)?;

        Ok(Expr::PatternComprehension(Box::new(
            PatternComprehensionExpr {
                path_variable: path_var,
                pattern,
                where_expr,
                map_expr,
            },
        )))
    }

    /// Parses the `WHERE`-part expression of a list/pattern comprehension
    /// with `comprehension_where_ambiguous_pipe` set, so a trailing `|` that
    /// can't validly continue a label-expression disjunction is left for the
    /// comprehension's own map-separator grammar instead of being consumed
    /// as part of the expression. See `label_or_pipe_is_ambiguous`.
    pub(super) fn parse_comprehension_where_expr(&mut self) -> ParseResult<Expr> {
        let saved_flag = self.comprehension_where_ambiguous_pipe;
        self.comprehension_where_ambiguous_pipe = true;
        let result = self.parse_expression();
        self.comprehension_where_ambiguous_pipe = saved_flag;
        result
    }

    /// CIP-272 map comprehension, attempted from the `{` of a primary
    /// expression. Returns `Ok(None)` (position restored) when the tokens do
    /// not form one, so the caller falls back to the map-literal parse.
    /// Shapes: `{ident IN <src> [WHERE p] | <key> : <value>}` (list source)
    /// and `{ident : ident IN <src> [WHERE p] | <key> : <value>}` (map
    /// source, distinguished by the second ident before `IN`).
    pub(super) fn try_parse_map_comprehension(&mut self) -> ParseResult<Option<Expr>> {
        let start = self.save_pos();
        self.next_token(); // consume '{'

        let first = self.peek().clone();
        // The binding variable(s) are symbolic names, so any keyword
        // spelling works unquoted too (ADR-0191).
        if !first.ty.is_schema_name() {
            self.restore_pos(start);
            return Ok(None);
        }
        self.next_token(); // consume the binding identifier

        // List source: {item IN ...}
        if self.peek_type() == TokenType::IN {
            self.next_token();
            return self.finish_map_comprehension(start, None, first.literal);
        }

        // Map source: {k : v IN ...}
        if self.peek_type() == TokenType::Colon {
            self.next_token();
            let second = self.peek().clone();
            if second.ty.is_schema_name() {
                self.next_token();
                if self.peek_type() == TokenType::IN {
                    self.next_token();
                    return self.finish_map_comprehension(
                        start,
                        Some(first.literal),
                        second.literal,
                    );
                }
            }
        }

        self.restore_pos(start);
        Ok(None)
    }

    /// True when a `|` at the current brace nesting depth appears before the
    /// `}` that closes the comprehension `{`. Called with the current token
    /// immediately after the `IN` of a map comprehension (both CIP-272
    /// shapes), so `false` means no token run inside this brace can be the
    /// projection separator and the speculation is pointless. All bracket
    /// kinds nest (unlike the paren-only scans): the source expression
    /// inside the brace can itself contain brackets and parens.
    pub(super) fn has_pipe_before_matching_brace(&self) -> bool {
        self.top_level_token_before_matching_close(TokenType::Pipe, BracketSet::AllBrackets)
    }

    /// Shared tail of both CIP-272 shapes: the binding variable(s) and `IN`
    /// are consumed; parse source, optional WHERE, the `| key : value`
    /// projection, and the closing brace. Any structural mismatch restores
    /// the start position and yields `None` (the map-literal parse then
    /// reports its own error for genuinely malformed input).
    pub(super) fn finish_map_comprehension(
        &mut self,
        start: usize,
        key_var: Option<String>,
        value_var: String,
    ) -> ParseResult<Option<Expr>> {
        // Fail fast before the speculative parse below. The whole shape hinges on
        // a top-level `|` separating the source from the projection; when no such
        // token exists the speculative parse can only rewind and let the
        // map-literal fallback re-parse the same tokens, doubling the work per
        // nesting level for input such as `{a: b IN {a: b IN { ... }}}` -- i.e.
        // exponential parse time on untrusted input. One brace-depth-aware scan
        // rejects that shape up front, in O(tokens); a genuine comprehension
        // scans once and then parses once, exactly as before. A brace that does
        // carry a top-level `|` still takes the speculative path: the scan cannot
        // tell a well-formed projection from one with a malformed tail.
        if !self.has_pipe_before_matching_brace() {
            self.restore_pos(start);
            return Ok(None);
        }
        #[cfg(test)]
        SPECULATIVE_COMPREHENSION_PARSES.with(|c| c.set(c.get() + 1));

        // The source expression must stop at the comprehension's `|`: the
        // flag-guarded parse (same as a comprehension WHERE) refuses an
        // ambiguous trailing pipe, so `{k : v IN m | k : v}` does not try
        // to read `m | k` as a label disjunction.
        let source_expr = match self.parse_comprehension_where_expr() {
            Ok(e) => e,
            Err(_) => {
                self.restore_pos(start);
                return Ok(None);
            }
        };

        let where_expr = if self.peek_type() == TokenType::WHERE {
            self.next_token();
            match self.parse_comprehension_where_expr() {
                Ok(e) => Some(e),
                Err(_) => {
                    self.restore_pos(start);
                    return Ok(None);
                }
            }
        } else {
            None
        };

        if self.peek_type() != TokenType::Pipe {
            self.restore_pos(start);
            return Ok(None);
        }
        self.next_token();

        // The key expression is parsed at the arithmetic level
        // (`<arithmetic value expression>` in the CIP-272 grammar): a full
        // expression would swallow the `:` as the CIP-14 label predicate
        // (`'k': i` = HasLabel), which is exactly the ambiguity the CIP
        // resolves by restricting the key grammar.
        let key_expr = match self.parse_add_sub() {
            Ok(e) => e,
            Err(_) => {
                self.restore_pos(start);
                return Ok(None);
            }
        };
        if self.peek_type() != TokenType::Colon {
            self.restore_pos(start);
            return Ok(None);
        }
        self.next_token();
        let value_expr = match self.parse_expression() {
            Ok(e) => e,
            Err(_) => {
                self.restore_pos(start);
                return Ok(None);
            }
        };
        if self.peek_type() != TokenType::RightBrace {
            self.restore_pos(start);
            return Ok(None);
        }
        self.next_token();

        Ok(Some(Expr::MapComprehension(Box::new(
            MapComprehensionExpr {
                key_var,
                value_var,
                source_expr,
                where_expr,
                key_expr,
                value_expr,
            },
        ))))
    }

    pub(super) fn parse_map_literal(&mut self) -> ParseResult<Vec<MapPair>> {
        self.expect(TokenType::LeftBrace)?;
        let mut pairs = Vec::new();
        if self.peek_type() == TokenType::RightBrace {
            self.next_token();
            return Ok(pairs);
        }
        loop {
            let key_tok = self.next_token();
            // ADR-0191 point 5: an unquoted keyword is a map key too
            // (`{match: 1}`), so this reads through `is_schema_name()` --
            // the same predicate ADR-0169 uses for label/type names.
            if !key_tok.ty.is_schema_name() && key_tok.ty != TokenType::String {
                // A digit-leading key (`{1B2c3e67: 1}`) lexes as an
                // `InvalidNumberLiteral` error token, but in *map-key* position
                // the openCypher TCK classifies every invalid key as the generic
                // `UnexpectedSyntax` detail (Literals8 [19]–[27], SyntaxError
                // class), not `InvalidNumberLiteral`. Override the propagated
                // token detail with `UnexpectedSyntax` here; the same malformed
                // token in *expression* position (`RETURN 1B2c3e67`) keeps its
                // `InvalidNumberLiteral` detail because it never reaches this
                // key-parsing path.
                return Err(self
                    .error_at_token(
                        &key_tok,
                        format!(
                            "expected map key (identifier or string), got {:?}",
                            key_tok.literal
                        ),
                    )
                    .with_detail("UnexpectedSyntax"));
            }
            self.expect(TokenType::Colon)?;
            let val = self.parse_expression()?;
            pairs.push(MapPair {
                key: key_tok.literal,
                value: val,
            });
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        self.expect(TokenType::RightBrace)?;
        Ok(pairs)
    }

    /// Parses a `<map projection>` filler (the current token must be `{`):
    /// `{ .*, .prop, variable, key: expr, ... }`.
    pub(super) fn parse_map_projection_items(&mut self) -> ParseResult<Vec<MapProjectionItem>> {
        self.expect(TokenType::LeftBrace)?;
        let mut items = Vec::new();
        if self.peek_type() == TokenType::RightBrace {
            self.next_token();
            return Ok(items);
        }
        loop {
            if self.peek_type() == TokenType::Dot {
                self.next_token(); // consume .
                if self.peek_type() == TokenType::Star {
                    self.next_token();
                    items.push(MapProjectionItem::AllProperties);
                } else {
                    // ADR-0191 point 5: keywords are map-projection
                    // selectors too (`n{.match}`, `n{match}`).
                    let prop =
                        self.parse_schema_name("a property name after '.' in map projection")?;
                    items.push(MapProjectionItem::Property(prop));
                }
            } else {
                let name = self.parse_schema_name("a map projection item")?;
                if self.peek_type() == TokenType::Colon {
                    self.next_token();
                    let value = self.parse_expression()?;
                    items.push(MapProjectionItem::KeyValue(name, value));
                } else {
                    items.push(MapProjectionItem::Variable(name));
                }
            }
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        self.expect(TokenType::RightBrace)?;
        Ok(items)
    }

    /// Called with `{` as the current token, immediately after a
    /// subquery-introducing keyword (`EXISTS`/`COUNT`/`COLLECT`). Returns
    /// true if the filler looks like a `<map projection>` (the keyword
    /// being used as an ordinary bound variable) rather than subquery
    /// content: a subquery's `{...}` must start with `(` (a bare pattern)
    /// or a clause keyword (MATCH/WITH/RETURN/...), neither of which a map
    /// projection filler (`.prop`, `.*`, a bare selector variable, or
    /// `key: expr`) can also start with, so one token of extra lookahead
    /// past that disambiguates with no backtracking (CIP-190
    /// near-ambiguities.feature [2]).
    ///
    /// ADR-0191 point 5: the item read through `is_schema_name()`, so a
    /// keyword spelling is a map-projection item too (`n {match: 1}`,
    /// `n {match}`, `n {.match}`). A subquery body starting with a clause
    /// keyword (`EXISTS { MATCH ... }`, `COUNT { DELETE n }`) still fails
    /// the two-token lookahead (`(`/an operand after the keyword, not
    /// `:`/`,`/`}`), so subquery classification is unchanged.
    pub(super) fn peek_brace_is_map_projection(&self) -> bool {
        match self.peek_at(1).ty {
            TokenType::Dot => true,
            ty if ty.is_schema_name() => matches!(
                self.peek_at(2).ty,
                TokenType::Colon | TokenType::Comma | TokenType::RightBrace
            ),
            _ => false,
        }
    }

    pub(super) fn parse_case_expr(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume CASE

        let test = if self.peek_type() != TokenType::WHEN {
            Some(self.parse_expression()?)
        } else {
            None
        };

        let mut whens = Vec::new();
        while self.peek_type() == TokenType::WHEN {
            self.next_token();
            let operand_predicate = match &test {
                Some(operand) => self.parse_when_operand_predicate(operand)?,
                None => None,
            };
            let (condition, is_predicate) = match operand_predicate {
                Some(predicate) => (predicate, true),
                None => (self.parse_expression()?, false),
            };
            self.expect(TokenType::THEN)?;
            let result = self.parse_expression()?;
            whens.push(WhenClause {
                condition,
                result,
                is_predicate,
            });
        }

        let else_expr = if self.peek_type() == TokenType::ELSE {
            self.next_token();
            Some(self.parse_expression()?)
        } else {
            None
        };

        self.expect(TokenType::END)?;
        Ok(Expr::Case(Box::new(CaseExpr {
            test,
            whens,
            else_expr,
        })))
    }

    /// Parses a simple-CASE WHEN operand that is a predicate on the CASE
    /// `operand` (the `WHEN` already consumed), or returns `None` without
    /// consuming anything when the WHEN operand is a plain value compared
    /// for equality with it.
    pub(super) fn parse_when_operand_predicate(
        &mut self,
        operand: &Expr,
    ) -> ParseResult<Option<Expr>> {
        let predicate = match self.peek_type() {
            TokenType::IS => {
                // Simple-CASE `WHEN IS ...` (CIP-249 label predicate, plus
                // the CIP-143/CIP-190 `<when operand>` completion covering
                // the other IS-introduced `<advanced comparison predicate
                // part 2>` forms): evaluated directly against the CASE
                // operand, not compared for equality with it.
                self.next_token(); // consume IS
                let negated = if self.peek_type() == TokenType::NOT {
                    self.next_token();
                    true
                } else {
                    false
                };
                let operand = operand.clone();
                if self.peek_type() == TokenType::DoubleColon {
                    // WHEN IS [NOT] :: <value type>  (CIP-87/CIP-143 type predicate)
                    self.next_token();
                    let type_expr = self.parse_value_type()?;
                    Expr::IsType(Box::new(IsTypeExpr {
                        expr: operand,
                        type_expr,
                        negated,
                    }))
                } else if self.peek_is_typed_keyword() {
                    // WHEN IS [NOT] TYPED <value type>
                    self.next_token();
                    let type_expr = self.parse_value_type()?;
                    Expr::IsType(Box::new(IsTypeExpr {
                        expr: operand,
                        type_expr,
                        negated,
                    }))
                } else if self.peek_type() == TokenType::NULL {
                    // WHEN IS [NOT] NULL
                    self.next_token();
                    Expr::IsNull(Box::new(IsNullExpr {
                        expr: operand,
                        negated,
                    }))
                } else {
                    // WHEN IS [NOT] [LABELED] <label-expr>  (CIP-249)
                    if self.peek_is_labeled_keyword() {
                        self.next_token();
                    }
                    let label_expr = self.parse_label_expr_body(true, false)?;
                    Expr::HasLabel(Box::new(HasLabelExpr {
                        expr: operand,
                        label_expr,
                        negated,
                        strict: true,
                    }))
                }
            }
            TokenType::Colon => {
                // Generalized-CASE `WHEN :<label-expr>` (CIP-221): the colon
                // form of the same operand label predicate as `WHEN IS
                // <label-expr>` above.
                let label_expr = self.parse_label_spec(true)?;
                Expr::HasLabel(Box::new(HasLabelExpr {
                    expr: operand.clone(),
                    label_expr,
                    negated: false,
                    strict: false,
                }))
            }
            TokenType::IN => {
                // CIP-143/CIP-190 `<when operand>` completion: `WHEN IN
                // <list>` is `<operand> IN <list>` with the CASE operand as
                // the implicit LHS.
                self.next_token();
                let list = self.parse_add_sub()?;
                Expr::In(Box::new(InExpr {
                    expr: operand.clone(),
                    list,
                }))
            }
            TokenType::CONTAINS => {
                self.next_token();
                let right = self.parse_add_sub()?;
                Expr::StringMatch(Box::new(StringMatchExpr {
                    left: operand.clone(),
                    op: "CONTAINS".to_string(),
                    right,
                }))
            }
            TokenType::STARTS => {
                self.next_token();
                self.expect(TokenType::WITH)?;
                let right = self.parse_add_sub()?;
                Expr::StringMatch(Box::new(StringMatchExpr {
                    left: operand.clone(),
                    op: "STARTS WITH".to_string(),
                    right,
                }))
            }
            TokenType::ENDS => {
                self.next_token();
                self.expect(TokenType::WITH)?;
                let right = self.parse_add_sub()?;
                Expr::StringMatch(Box::new(StringMatchExpr {
                    left: operand.clone(),
                    op: "ENDS WITH".to_string(),
                    right,
                }))
            }
            _ => return Ok(None),
        };
        Ok(Some(predicate))
    }

    /// Shared head of `reduce` and `allReduce`: parses
    /// `( acc = init, var IN list | expr` with the function-name token
    /// already consumed by the caller, leaving the cursor just after the
    /// per-element expression. `function_name` only shapes the error text;
    /// the two forms differ solely in their tail after `| expr`.
    fn parse_reduce_head(&mut self, function_name: &str) -> ParseResult<ReduceHead> {
        self.expect(TokenType::LeftParen)?;

        // The accumulator and loop variables are symbolic names, so any
        // keyword spelling works unquoted too (ADR-0191).
        let accumulator =
            self.parse_schema_name(&format!("accumulator variable in {function_name}()"))?;
        self.expect(TokenType::Equals)?;
        let init_expr = self.parse_expression()?;
        self.expect(TokenType::Comma)?;

        let variable = self.parse_schema_name(&format!("variable in {function_name}()"))?;
        self.expect(TokenType::IN)?;
        let list_expr = self.parse_expression()?;
        self.expect(TokenType::Pipe)?;
        let expression = self.parse_expression()?;

        Ok(ReduceHead {
            accumulator,
            init_expr,
            variable,
            list_expr,
            expression,
        })
    }

    pub(super) fn parse_reduce_expr(&mut self) -> ParseResult<Expr> {
        self.next_token(); // consume REDUCE
        let ReduceHead {
            accumulator,
            init_expr,
            variable,
            list_expr,
            expression,
        } = self.parse_reduce_head("reduce")?;
        self.expect(TokenType::RightParen)?;

        Ok(Expr::Reduce(Box::new(ReduceExpr {
            accumulator,
            init_expr,
            variable,
            list_expr,
            expression,
        })))
    }

    pub(super) fn parse_allreduce_expr(&mut self) -> ParseResult<Expr> {
        // The 'allReduce' identifier token has already been consumed by the caller.
        let ReduceHead {
            accumulator,
            init_expr,
            variable,
            list_expr,
            expression,
        } = self.parse_reduce_head("allReduce")?;
        self.expect(TokenType::Comma)?;
        let predicate = self.parse_expression()?;
        self.expect(TokenType::RightParen)?;

        Ok(Expr::AllReduce(Box::new(AllReduceExpr {
            accumulator,
            init_expr,
            variable,
            list_expr,
            expression,
            predicate,
        })))
    }

    pub(super) fn parse_pattern_predicate(&mut self) -> ParseResult<Expr> {
        let path = self.parse_pattern_path()?;
        Ok(Expr::PatternPredicate(Box::new(PatternPredicateExpr {
            pattern: path,
        })))
    }
}

/// CIP-213: an arm's true terminal stage is the end of its own NEXT
/// chain, if any — not its own first stage's clause list. Shared by the
/// UNION-arm shape checks (`has_return`, `return_columns`) and the
/// COLLECT-subquery single-return-column check.
fn tail_stage(s: &Statement) -> &Statement {
    let mut t = s;
    while let Some(next) = &t.next {
        t = next;
    }
    t
}

/// The bracket kinds that count toward nesting in
/// [`Parser::top_level_token_before_matching_close`] — an explicit,
/// per-site choice, so a paren-only scan (a call's argument list) and an
/// all-bracket scan (a comprehension brace) cannot be silently confused.
#[derive(Clone, Copy)]
enum BracketSet {
    /// Only `(` / `)` nest: the scan runs inside a call's argument list.
    Parens,
    /// `{`, `[`, and `(` all nest: the scan runs inside a comprehension
    /// brace, whose source expression can contain brackets and parens.
    AllBrackets,
}

impl BracketSet {
    /// True when `ty` is an opening bracket of one of this set's kinds.
    fn opens(self, ty: TokenType) -> bool {
        match self {
            BracketSet::Parens => ty == TokenType::LeftParen,
            BracketSet::AllBrackets => matches!(
                ty,
                TokenType::LeftBrace | TokenType::LeftBracket | TokenType::LeftParen
            ),
        }
    }

    /// True when `ty` is a closing bracket of one of this set's kinds.
    fn closes(self, ty: TokenType) -> bool {
        match self {
            BracketSet::Parens => ty == TokenType::RightParen,
            BracketSet::AllBrackets => matches!(
                ty,
                TokenType::RightBrace | TokenType::RightBracket | TokenType::RightParen
            ),
        }
    }
}
