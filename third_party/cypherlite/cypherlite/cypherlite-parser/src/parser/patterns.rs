use super::*;

impl Parser {
    // ---- Pattern parsing ----

    pub(super) fn parse_pattern(&mut self) -> ParseResult<Vec<PatternPath>> {
        let mut paths = Vec::new();
        loop {
            paths.push(self.parse_pattern_path()?);
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        Ok(paths)
    }

    /// True when the token `offset` ahead is a symbolic name directly
    /// followed by `=` -- the named-path prefix `name = ...` (ADR-0191: a
    /// keyword spelling works too). The callers pass `offset == 0` at a
    /// pattern position and `offset == 1` past a leading `(` (the
    /// parenthesized subpath group).
    pub(super) fn peek_named_path_prefix(&self, offset: usize) -> bool {
        self.peek_at(offset).ty.is_schema_name() && self.peek_at(offset + 1).ty == TokenType::Equals
    }

    pub(super) fn parse_pattern_path(&mut self) -> ParseResult<PatternPath> {
        // Optional named-path prefix: `p = ...`. Consumed up front so that a
        // path variable assigned to a top-level parenthesized path pattern
        // (`p = ((a)-[r]->(b))`, CIP-19 basic-structure #15) is recognized
        // before `parse_pattern_path_body`'s own inner double-paren handling.
        // The variable is a symbolic name, so any keyword spelling works
        // unquoted too (ADR-0191).
        let mut outer_var: Option<String> = None;
        if self.peek_named_path_prefix(0) {
            let tok = self.next_token(); // consume the path variable
            self.next_token(); // consume =
            outer_var = Some(tok.literal);
        }

        // Pre-GPM `shortestPath(...)`/`allShortestPaths(...)` used as a
        // top-level MATCH pattern (CIP-40/CIP-73 Section 3.3): sugar for
        // `SHORTEST 1 PATH (...)` / `ALL SHORTEST (...)` respectively.
        // Checked before `parse_path_prefix` -- a bare `shortestPath`
        // identifier can never otherwise start a pattern (every path
        // pattern begins with `(`), so there is no ambiguity with the
        // `ALL`/`ANY`/`SHORTEST` selector keywords.
        if self.peek_type() == TokenType::Identifier {
            let lower = self.peek().literal.to_ascii_lowercase();
            if (lower == "shortestpath" || lower == "allshortestpaths")
                && self.peek_at(1).ty == TokenType::LeftParen
            {
                self.next_token(); // consume shortestPath/allShortestPaths
                self.next_token(); // consume (
                let mut path = self.parse_pattern_path_body()?;
                self.expect(TokenType::RightParen)?;
                if contains_gpm_quantification(&path.elements) {
                    return Err(self.error(
                        "QuantifiedPathPatternInShortestPath: a quantified path pattern cannot be used inside shortestPath()/allShortestPaths()",
                    ));
                }
                if let Some(var) = outer_var {
                    path.variable = Some(var);
                }
                path.mode = None;
                path.selector = Some(if lower == "allshortestpaths" {
                    PathSelector::AllShortest
                } else {
                    PathSelector::Shortest {
                        count: Some(PathCount::Literal(1)),
                        group: false,
                    }
                });
                path.is_shortest_path_function = true;
                return self.finish_pattern_path(path);
            }
        }

        let (selector, mode) = self.parse_path_prefix()?;

        // CIP-51 Section 3.1.2 / Deviation D1: the path variable declaration
        // must precede the mode/selector prefix. If another `name =`
        // follows the prefix we just consumed, the input was actually
        // `MODE p = ...` (mode before the variable) -- reject it explicitly
        // rather than silently reinterpreting it. A keyword spelling is a
        // path variable too (ADR-0191), so it must be rejected here the
        // same way.
        if self.peek_named_path_prefix(0) {
            return Err(self.error(
                "InvalidPathModePosition: the path variable declaration must precede the path mode/selector prefix",
            ));
        }

        let mut path = self.parse_pattern_path_body()?;
        if let Some(var) = outer_var {
            path.variable = Some(var);
        }
        path.mode = mode;
        path.selector = selector;

        self.finish_pattern_path(path)
    }

    /// Shared tail of `parse_pattern_path`: CIP-28 Section 4.2 Rule 6, a
    /// top-level path pattern must have a minimum node count > 0 -- it
    /// cannot consist entirely of zero-lower-bound quantified path patterns
    /// (which could all collapse to zero iterations, leaving no node at
    /// all). A plain `Node` element always guarantees at least one node; a
    /// `Quantified` element guarantees one only if its own quantifier
    /// requires at least one iteration.
    pub(super) fn finish_pattern_path(&self, path: PatternPath) -> ParseResult<PatternPath> {
        let has_guaranteed_node = path.elements.iter().any(|e| match e {
            PatternElement::Node(_) => true,
            PatternElement::Quantified(q) => q.quantifier.min >= 1,
            PatternElement::Relationship(_) => false,
        });
        if !has_guaranteed_node {
            return Err(self.error(
                "MinimumNodeCountZero: a path pattern must have a minimum node count greater than zero",
            ));
        }

        Ok(path)
    }

    /// Parses one path pattern's body: an optional named-path prefix
    /// (`ident = `, recognized both at the top level and for a subpath
    /// variable inside a quantification, e.g. `(p2 = (x)-->(y))*`) followed
    /// by a sequence of node, relationship, and quantified-path-pattern
    /// factors (CIP-28 Section 3.1.7 concatenation). Node and quantified
    /// factors may be juxtaposed directly (no relationship between them,
    /// e.g. `(x)((a)-->(b))*` or `((a)-->(b))*((c)-->(d))*`); a relationship
    /// factor must always be preceded by an already-parsed element and
    /// followed by a node or quantified factor.
    pub(super) fn parse_pattern_path_body(&mut self) -> ParseResult<PatternPath> {
        let mut path = PatternPath {
            variable: None,
            elements: Vec::new(),
            where_expr: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };

        // Check for named path: symbolic name = (...) -- a keyword spelling
        // included (ADR-0191), e.g. a subpath variable inside a quantified
        // path pattern (`(MATCH = (x)-->(y))*`).
        if self.peek_named_path_prefix(0) {
            let tok = self.next_token();
            self.next_token(); // consume =
            path.variable = Some(tok.literal);
        }

        loop {
            match self.peek_type() {
                TokenType::LeftParen if self.peek_is_parenthesized_subpath_start() => {
                    // Parenthesized subpath group: `((...) [WHERE ...])`
                    // (CIP-14). Either quantified into a `Quantified`
                    // element (CIP-28), or -- if unquantified -- legal only
                    // as the sole factor of the whole pattern.
                    self.next_token(); // consume outer (
                                       // ADR-0257: nested parenthesized subpaths recurse here
                                       // directly, bypassing `parse_expression`, so each level
                                       // must count separately (LABS-69).
                    let inner = self.with_expr_depth(|p| p.parse_pattern_path_body())?;
                    let where_expr = if self.peek_type() == TokenType::WHERE {
                        self.next_token();
                        Some(self.parse_expression()?)
                    } else {
                        None
                    };
                    self.expect(TokenType::RightParen)?;

                    if matches!(
                        self.peek_type(),
                        TokenType::Star | TokenType::Plus | TokenType::LeftBrace
                    ) {
                        self.side_effects.quantifier_saw_gpm_form = true;
                        let quantifier = self.parse_quantifier()?;
                        // Rule 3: the operand's minimum path length must be
                        // > 0 -- a single node pattern cannot be quantified.
                        let has_relationship = inner
                            .elements
                            .iter()
                            .any(|e| matches!(e, PatternElement::Relationship(_)));
                        if !has_relationship {
                            return Err(self.error(
                                "MinimumPathLengthZero: the operand of a quantified path pattern must have a minimum path length greater than zero",
                            ));
                        }
                        // Rule 4: quantified path patterns cannot be nested,
                        // directly or through concatenation.
                        if contains_quantification(&inner.elements) {
                            return Err(self.error(
                                "NestedQuantification: quantified path patterns cannot be nested",
                            ));
                        }
                        path.elements
                            .push(PatternElement::Quantified(QuantifiedPathPattern {
                                subpath_variable: inner.variable,
                                elements: inner.elements,
                                where_expr,
                                quantifier,
                            }));
                    } else {
                        // Unquantified parenthesized pattern: only legal as
                        // the entire pattern path (CIP-14 pre-existing
                        // feature) -- nothing may precede or follow it.
                        if !path.elements.is_empty()
                            || matches!(
                                self.peek_type(),
                                TokenType::LeftParen | TokenType::Dash | TokenType::ArrowLeft
                            )
                        {
                            return Err(self.error(
                                "UnquantifiedParenthesizedPattern: an unquantified parenthesized path pattern cannot be juxtaposed with other pattern elements",
                            ));
                        }
                        path.elements = inner.elements;
                        path.where_expr = where_expr;
                        if let Some(v) = inner.variable {
                            path.variable = Some(v);
                        }
                        return Ok(path);
                    }
                }
                TokenType::LeftParen => {
                    let node = self.parse_node_pattern()?;
                    path.elements.push(PatternElement::Node(node));
                }
                TokenType::Dash | TokenType::ArrowLeft => {
                    if path.elements.is_empty() {
                        return Err(self.error("a pattern must start with a node pattern"));
                    }
                    let rel = self.parse_rel_pattern()?;
                    path.elements.push(PatternElement::Relationship(rel));
                    if self.peek_type() != TokenType::LeftParen {
                        return Err(self.error("expected node pattern after relationship"));
                    }
                }
                _ => break,
            }
        }

        if path.elements.is_empty() {
            return Err(self.error("expected a pattern"));
        }

        Ok(path)
    }

    /// True if the parser is positioned at the start of a parenthesized
    /// subpath group (CIP-14 `((...))`) -- either the plain `((` form, or
    /// Consumes the current token if it is the contextual identifier `s`
    /// (see `peek_is_ident`); returns whether it did. Used for the
    /// CIP-51/CIP-73/CIP-191 contextual keywords (WALK, TRAIL, ACYCLIC,
    /// SIMPLE, ANY, SHORTEST, GROUP, GROUPS, PATH, PATHS), which stay
    /// ordinary identifiers everywhere else in the grammar (so they never
    /// shadow the `all()`/`any()`/`none()`/`single()` predicate functions or
    /// a variable of the same name).
    pub(super) fn consume_ident(&mut self, s: &str) -> bool {
        if self.peek_is_ident(s) {
            self.next_token();
            true
        } else {
            false
        }
    }

    /// Parses an optional CIP-51 path mode keyword (`WALK`/`TRAIL`/
    /// `ACYCLIC`/`SIMPLE`).
    pub(super) fn parse_path_mode_keyword(&mut self) -> Option<PathMode> {
        if self.consume_ident("walk") {
            Some(PathMode::Walk)
        } else if self.consume_ident("trail") {
            Some(PathMode::Trail)
        } else if self.consume_ident("acyclic") {
            Some(PathMode::Acyclic)
        } else if self.consume_ident("simple") {
            Some(PathMode::Simple)
        } else {
            None
        }
    }

    /// Consumes an optional CIP-51/CIP-73 `PATH`/`PATHS` noise keyword
    /// (`<path keywords>` / `<path or paths>`); has no semantic effect.
    pub(super) fn consume_path_or_paths_noise(&mut self) {
        if !self.consume_ident("path") {
            self.consume_ident("paths");
        }
    }

    /// Parses the CIP-73 `<number of paths>`/`<number of groups>`
    /// non-terminal, extended by CIP-191's `<dynamic parameter
    /// specification>`: an unsigned integer literal (validated `> 0` here,
    /// Rule 0) or a bare `$parameter` (validated at runtime, once its value
    /// is known). Returns `None` if neither is present.
    pub(super) fn parse_number_of_paths(&mut self) -> ParseResult<Option<PathCount>> {
        if self.peek_type() == TokenType::Integer {
            let tok = self.next_token();
            // Checked conversion via the shared bound helper, not a silent
            // clamp (LABS-81): `ANY 4294967296 PATHS` used to wrap to
            // selector 0.
            let n = self.parse_u32_bound(&tok, "InvalidNumberOfPaths: number of paths/groups")?;
            if n == 0 {
                return Err(self.error_at_token(
                    &tok,
                    "InvalidNumberOfPaths: the number of paths/groups must be greater than 0",
                ));
            }
            return Ok(Some(PathCount::Literal(n)));
        }
        if self.peek_type() == TokenType::Parameter {
            let tok = self.next_token();
            return Ok(Some(PathCount::Parameter(tok.literal)));
        }
        Ok(None)
    }

    /// Parses the optional CIP-51 path-mode-only prefix or CIP-73/CIP-191
    /// path-search (selector) prefix that may precede a path pattern's body
    /// (`<path pattern prefix>`, CIP-51 Section 3.1.2). Returns
    /// `(selector, mode)`; both `None` when neither is written.
    pub(super) fn parse_path_prefix(
        &mut self,
    ) -> ParseResult<(Option<PathSelector>, Option<PathMode>)> {
        if self.peek_type() == TokenType::ALL {
            self.next_token(); // consume ALL
            if self.consume_ident("shortest") {
                let mode = self.parse_path_mode_keyword();
                self.consume_path_or_paths_noise();
                return Ok((Some(PathSelector::AllShortest), mode));
            }
            let mode = self.parse_path_mode_keyword();
            self.consume_path_or_paths_noise();
            return Ok((Some(PathSelector::All), mode));
        }
        if self.peek_is_ident("any") {
            self.next_token(); // consume ANY
            if self.consume_ident("shortest") {
                let mode = self.parse_path_mode_keyword();
                self.consume_path_or_paths_noise();
                return Ok((Some(PathSelector::AnyShortest), mode));
            }
            let count = self.parse_number_of_paths()?;
            let mode = self.parse_path_mode_keyword();
            self.consume_path_or_paths_noise();
            return Ok((Some(PathSelector::Any(count)), mode));
        }
        if self.peek_is_ident("shortest") {
            self.next_token(); // consume SHORTEST
            let count = self.parse_number_of_paths()?;
            let mode = self.parse_path_mode_keyword();
            self.consume_path_or_paths_noise();
            let group = self.consume_ident("group") || self.consume_ident("groups");
            if !group && count.is_none() {
                return Err(self.error(
                    "InvalidNumberOfPaths: SHORTEST requires a number of paths unless followed by GROUP/GROUPS",
                ));
            }
            return Ok((Some(PathSelector::Shortest { count, group }), mode));
        }
        // No selector: a bare path-mode-only prefix, if any.
        let mode = self.parse_path_mode_keyword();
        if mode.is_some() {
            self.consume_path_or_paths_noise();
        }
        Ok((None, mode))
    }

    /// `(name = (` when the group itself carries a subpath variable
    /// (CIP-28 Section 3.3, e.g. `(p2 = (x)-->(y))*`). Distinguishes this
    /// from a plain node pattern, whose own label/property spec never
    /// starts with `(`. The subpath variable is a symbolic name, so any
    /// keyword spelling works unquoted too (ADR-0191).
    pub(super) fn peek_is_parenthesized_subpath_start(&self) -> bool {
        if self.peek_type() != TokenType::LeftParen {
            return false;
        }
        if self.peek_at(1).ty == TokenType::LeftParen {
            return true;
        }
        self.peek_named_path_prefix(1) && self.peek_at(3).ty == TokenType::LeftParen
    }

    /// Parses a CIP-28 quantifier: `*`, `+`, `{m,n}`, `{m,}`, `{,n}`, `{,}`,
    /// or `{n}`. The current token must be `Star`, `Plus`, or `LeftBrace`.
    /// Applies Rule 1 normalization (`{,}` -> `{0,}`, an absent lower bound
    /// defaults to 0) and Rule 2 validation (upper bound > 0 if present,
    /// lower bound <= upper bound).
    pub(super) fn parse_quantifier(&mut self) -> ParseResult<PathQuantifier> {
        match self.peek_type() {
            TokenType::Star => {
                self.next_token();
                Ok(PathQuantifier { min: 0, max: None })
            }
            TokenType::Plus => {
                self.next_token();
                Ok(PathQuantifier { min: 1, max: None })
            }
            TokenType::LeftBrace => {
                self.next_token(); // consume {
                let lower = if self.peek_type() == TokenType::Integer {
                    let tok = self.next_token();
                    Some(self.parse_u32_bound(&tok, "quantifier lower bound")?)
                } else {
                    None
                };
                let (min, max) = if self.peek_type() == TokenType::Comma {
                    self.next_token(); // consume ,
                    let upper = if self.peek_type() == TokenType::Integer {
                        let tok = self.next_token();
                        Some(self.parse_u32_bound(&tok, "quantifier upper bound")?)
                    } else {
                        None
                    };
                    (lower.unwrap_or(0), upper)
                } else {
                    // No comma: a fixed quantifier `{n}`, equivalent to `{n,n}`.
                    let n = lower
                        .ok_or_else(|| self.error("expected an integer or ',' in quantifier"))?;
                    (n, Some(n))
                };
                self.expect(TokenType::RightBrace)?;
                if let Some(upper) = max {
                    if upper == 0 {
                        return Err(self.error(
                            "ZeroUpperBound: a quantifier's upper bound must be greater than zero",
                        ));
                    }
                    if min > upper {
                        return Err(self.error(
                            "LowerBoundGreaterThanUpperBound: a quantifier's lower bound cannot exceed its upper bound",
                        ));
                    }
                }
                Ok(PathQuantifier { min, max })
            }
            _ => Err(self.error("expected a quantifier")),
        }
    }

    /// ADR-0191 Decision point 4: true when the token `offset` ahead can be
    /// read as a node or relationship pattern's element variable. Only a
    /// symbolic name (`is_schema_name()`: an `Identifier` or any keyword)
    /// can be one; the `WHERE` and `IS` spellings additionally keep their
    /// existing meanings in front of the variable read, decided by a
    /// one-token lookahead:
    ///
    /// - a leading `WHERE` is the variable only when the next token cannot
    ///   start the inline element predicate (`)`, `]`, `:`, `IS`, `$`, `-`,
    ///   `<-`, `WHERE`); otherwise the inline-predicate meaning wins, so
    ///   `(WHERE 1 = 1)` stays an anonymous element with a predicate while
    ///   `(WHERE)` and `(where WHERE true)` read `WHERE` as the variable;
    /// - a leading `IS` is the variable only when the next token cannot
    ///   start a label spec; otherwise the variable-less GPM
    ///   label-introducer meaning wins, so `(IS)`/`[IS]` read `IS` as the
    ///   variable while `(IS A&B)`/`-[IS R]->` keep `IS` as the introducer.
    ///   The label-spec test is the shared offset-aware
    ///   `peek_can_start_label_expr_at`, so this lookahead and the
    ///   expression-level label reader cannot drift apart.
    ///
    /// Also used by the pattern-comprehension/predicate lookahead
    /// (`node_pattern_precedes_relationship`), which must skip exactly the
    /// tokens this reader would consume as the variable.
    pub(super) fn peek_pattern_name_is_variable(&self, offset: usize) -> bool {
        let ty = self.peek_at(offset).ty;
        if !ty.is_schema_name() {
            return false;
        }
        match ty {
            TokenType::WHERE => matches!(
                self.peek_at(offset + 1).ty,
                TokenType::RightParen
                    | TokenType::RightBracket
                    | TokenType::Colon
                    | TokenType::IS
                    | TokenType::Parameter
                    | TokenType::Dash
                    | TokenType::ArrowLeft
                    | TokenType::WHERE
            ),
            TokenType::IS => !self.peek_can_start_label_expr_at(offset + 1),
            _ => true,
        }
    }

    pub(super) fn parse_node_pattern(&mut self) -> ParseResult<NodePattern> {
        self.expect(TokenType::LeftParen)?;
        let mut node = NodePattern {
            variable: None,
            label_expr: None,
            properties: Vec::new(),
            has_properties: false,
            properties_param: None,
            where_expr: None,
        };

        let tok = self.peek().clone();
        // The element variable is a symbolic name (ADR-0191), so any
        // keyword spelling works unquoted. `WHERE` and `IS` are read as the
        // variable only when the one-token lookahead shows their other
        // meaning cannot apply (`peek_pattern_name_is_variable`, which folds
        // in the symbolic-name check): a leading `WHERE` otherwise starts
        // the inline element predicate and a leading `IS` otherwise starts a
        // variable-less GPM label spec (`(IS A&B)`, CIP-40 Section 2.3.3).
        if self.peek_pattern_name_is_variable(0) {
            node.variable = Some(tok.literal.clone());
            self.next_token();
        }

        if matches!(self.peek_type(), TokenType::Colon | TokenType::IS) {
            node.label_expr = Some(self.parse_label_spec(false)?);
        }

        // Property spec comes before the element-pattern WHERE in the
        // grammar: `(var:LE {props} WHERE ...)` (CIP-17). A bare parameter
        // reference here (`(n:Label $props)`) is the whole-map shorthand
        // rather than a `{...}` literal.
        if self.peek_type() == TokenType::LeftBrace {
            node.has_properties = true;
            let pairs = self.parse_map_literal()?;
            node.properties = pairs.into_iter().map(|p| (p.key, p.value)).collect();
        } else if self.peek_type() == TokenType::Parameter {
            node.has_properties = true;
            let tok = self.next_token();
            node.properties_param = Some(Expr::Parameter(tok.literal));
        }

        if self.peek_type() == TokenType::WHERE {
            self.next_token();
            let where_expr = self.parse_expression()?;
            self.check_element_pattern_predicate_type(&where_expr)?;
            node.where_expr = Some(where_expr);
        }

        self.expect(TokenType::RightParen)?;
        Ok(node)
    }

    /// A node/relationship pattern's inline `WHERE` predicate (CIP-17/18)
    /// must be Boolean-typed. A bare map literal (`{...}`) is statically
    /// known to be a `MAP`, never a `BOOLEAN`, regardless of what the
    /// pattern matches at runtime -- Neo4j's semantic-analysis phase
    /// rejects this before execution even runs (CIP-190 `where-variable`
    /// scenario [1]: `MATCH (where {}) RETURN 1` errors even against an
    /// empty graph, where the predicate would otherwise never be
    /// evaluated). Only the statically-known-non-Boolean literal case is
    /// checked here; anything that could resolve to a boolean at runtime
    /// (a variable, property access, function call, ...) is left to the
    /// normal runtime type check.
    pub(super) fn check_element_pattern_predicate_type(&self, expr: &Expr) -> ParseResult<()> {
        if let Expr::Map(_) = expr {
            return Err(
                self.error("TypeError: Type mismatch: expected Boolean but was Map".to_string())
            );
        }
        Ok(())
    }

    /// Parses the legacy bracket-syntax variable-length hop bounds
    /// (`*min..max`, including a bare `*`) of a relationship pattern; the
    /// current token must be `Star`. Returns `(min, max)` -- an absent
    /// upper bound is `None`, a bare `*` is `(1, None)`, and a single
    /// number `*n` is the exact hop count `(n, Some(n))`. Sets
    /// `quantifier_saw_legacy_bracket` for the clause-level
    /// quantifier-mixing check.
    fn parse_legacy_hop_bounds(&mut self) -> ParseResult<(u32, Option<u32>)> {
        debug_assert_eq!(self.peek_type(), TokenType::Star);
        self.side_effects.quantifier_saw_legacy_bracket = true;
        self.next_token();
        let lower = if self.peek_type() == TokenType::Integer {
            let tok = self.next_token();
            Some(self.parse_u32_bound(&tok, "relationship hop lower bound")?)
        } else {
            None
        };
        if self.peek_type() != TokenType::DotDot {
            // `*` (unbounded) or `*n` (exact hop count).
            return Ok(lower.map_or((1, None), |n| (n, Some(n))));
        }
        self.next_token();
        let upper = if self.peek_type() == TokenType::Integer {
            let tok = self.next_token();
            Some(self.parse_u32_bound(&tok, "relationship hop upper bound")?)
        } else {
            None
        };
        Ok((lower.unwrap_or(1), upper))
    }

    pub(super) fn parse_rel_pattern(&mut self) -> ParseResult<RelPattern> {
        let mut rel = RelPattern {
            variable: None,
            type_expr: None,
            properties: Vec::new(),
            properties_param: None,
            direction: Direction::Both,
            min_hops: None,
            max_hops: None,
            where_expr: None,
            quantified: false,
        };

        let tok = self.next_token();
        let left_arrow = tok.ty == TokenType::ArrowLeft;
        if !left_arrow && tok.ty != TokenType::Dash {
            return Err(
                self.error_at_token(&tok, format!("expected - or <-, got {:?}", tok.literal))
            );
        }

        if self.peek_type() == TokenType::LeftBracket {
            self.next_token(); // consume [

            // Variable. As with node patterns, a keyword spelling is a
            // relationship variable too (ADR-0191); a bare `IS` here is the
            // variable-less GPM label introducer (`-[IS R]->`) only when
            // the lookahead does not prove it is the variable.
            let peek = self.peek().clone();
            if self.peek_pattern_name_is_variable(0) {
                rel.variable = Some(peek.literal.clone());
                self.next_token();
            }

            // Types (relationship `|` disjunction predates CIP-14, so a
            // bare top-level `|` here doesn't itself count as a GQL symbol
            // for the clause-level mixing check).
            if matches!(self.peek_type(), TokenType::Colon | TokenType::IS) {
                rel.type_expr = Some(self.parse_label_spec(true)?);
            }

            // Variable length *min..max
            if self.peek_type() == TokenType::Star {
                let (min, max) = self.parse_legacy_hop_bounds()?;
                rel.min_hops = Some(min);
                rel.max_hops = max;

                // CIP-14: negation or wildcard combined with a variable-length
                // quantifier is a SyntaxError; legacy single-type/pipe-disjunction
                // and parenthesized single-type quantified patterns remain valid.
                if let Some(type_expr) = &rel.type_expr {
                    if type_expr.contains_not_or_wildcard() {
                        return Err(self.error(
                            "VariableLengthRelationshipWithTypeExpression: variable length relationships cannot be used in combination with relationship type expressions containing negation or the wildcard",
                        ));
                    }
                }
            }

            // Properties. A bare parameter reference here (`[r:TYPE $props]`)
            // is the whole-map shorthand rather than a `{...}` literal.
            if self.peek_type() == TokenType::LeftBrace {
                let pairs = self.parse_map_literal()?;
                rel.properties = pairs.into_iter().map(|p| (p.key, p.value)).collect();
            } else if self.peek_type() == TokenType::Parameter {
                let tok = self.next_token();
                rel.properties_param = Some(Expr::Parameter(tok.literal));
            }

            // The element-pattern WHERE comes after the property spec in the
            // grammar: `[r:LE {props} WHERE ...]` (CIP-18).
            if self.peek_type() == TokenType::WHERE {
                let where_tok = self.peek().clone();
                self.next_token();
                let where_expr = self.parse_expression()?;
                self.check_element_pattern_predicate_type(&where_expr)?;
                rel.where_expr = Some(where_expr);
                // CIP-40 Deviation (quantifier-mixing.feature scenario 14):
                // the CIP text allows an inline WHERE on the legacy
                // bracket-syntax variable-length form (`[r*1..5 WHERE ...]`)
                // just like the GPM curly-brace form, but current Neo4j
                // rejects it -- only the GPM form (Form 5 postfix
                // quantifier, parsed after the closing bracket) is legal.
                if rel.min_hops.is_some() || rel.max_hops.is_some() {
                    return Err(self.error_at_token(
                        &where_tok,
                        "RelationshipPatternPredicateInVariableLength: an inline WHERE predicate cannot be used with the legacy bracket-syntax variable-length relationship pattern".to_string(),
                    ));
                }
            }

            // A malformed relationship bracket (a stray `..`/`-`/token where the
            // closing `]` is expected — Match4 [9] `[:LIKES..]`, [10]
            // `[:LIKES*-2]`) is `SyntaxError: InvalidRelationshipPattern`,
            // not the generic `UnexpectedSyntax`.
            self.expect_detail(TokenType::RightBracket, "InvalidRelationshipPattern")?;
        }

        // Right arrow or dash
        let right_tok = self.next_token();
        match right_tok.ty {
            TokenType::ArrowRight => {
                rel.direction = if left_arrow {
                    Direction::Both
                } else {
                    Direction::Right
                };
            }
            TokenType::Dash => {
                rel.direction = if left_arrow {
                    Direction::Left
                } else {
                    Direction::Both
                };
            }
            _ => {
                return Err(self.error_at_token(
                    &right_tok,
                    format!("expected -> or -, got {:?}", right_tok.literal),
                ));
            }
        }

        // CIP-28 Section 3.1.3 Form 5 / Rule 5: a quantifier directly
        // following the relationship pattern (`-[r]->*`, `-->+`,
        // `-[]->{1,6}`, `<-[:R]-{1,3}`, `-[:R]-{2}`) normalizes to an
        // ordinary variable-length relationship pattern with implicit
        // boundary nodes: it declares no element variables of its own to
        // expose as a group, so it reuses the existing var-length matcher
        // rather than becoming a `Quantified` element. `quantified` marks
        // this origin so the engine knows to honor an inline element-pattern
        // WHERE per traversed edge (CIP-28 semantics), unlike the legacy
        // `[r*min..max]` form which this grammar position never touches.
        if matches!(
            self.peek_type(),
            TokenType::Star | TokenType::Plus | TokenType::LeftBrace
        ) {
            if rel.min_hops.is_some() || rel.max_hops.is_some() {
                return Err(self.error(
                    "NestedQuantification: a relationship pattern cannot be quantified twice",
                ));
            }
            let quantifier = self.parse_quantifier()?;
            rel.min_hops = Some(quantifier.min);
            rel.max_hops = quantifier.max;
            rel.quantified = true;
            self.side_effects.quantifier_saw_gpm_form = true;
        }

        Ok(rel)
    }
}
