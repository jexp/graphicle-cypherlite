//! The `<value type>` grammar (CIP-86/CIP-102): component type lists,
//! type terms and leaves, predefined types, vector and time-zone
//! suffixes, and the ANY<...> bracket forms — split out of
//! `expressions.rs` so the expression grammar file stops accreting past
//! every other parser module's size.
use super::*;

impl Parser {
    /// Parses a `<value type>` (CIP-86 grammar): a `<component type list>`,
    /// i.e. one or more `|`-separated type terms. A single term is returned
    /// as-is; two or more become a closed dynamic union (CIP-86 "bare
    /// component list", equivalent to `ANY<A | B | ...>`). This is the
    /// entry point used everywhere a `<value type>` production appears: the
    /// top-level type predicate right-hand side, and (recursively) inside
    /// `LIST<...>`/`ANY<...>` brackets.
    pub(super) fn parse_value_type(&mut self) -> ParseResult<TypeExpr> {
        // ADR-0257: `LIST<...>`/`ARRAY<...>` and `ANY<...>` type brackets
        // recurse back into this function through `parse_type_term` ->
        // `parse_type_leaf` (and `parse_any_type`) without passing through
        // `parse_expression`, so deeply nested value types
        // (`1::LIST<LIST<...>>`) would otherwise overflow the stack
        // (LABS-69). Count each level, same balanced guard as the
        // label/subpath sites.
        self.with_expr_depth(Self::parse_value_type_at_current_depth)
    }

    fn parse_value_type_at_current_depth(&mut self) -> ParseResult<TypeExpr> {
        let first = self.parse_type_term()?;
        if self.peek_type() != TokenType::Pipe {
            return Ok(first);
        }
        let mut components = vec![first];
        while self.peek_type() == TokenType::Pipe {
            self.next_token();
            components.push(self.parse_type_term()?);
        }
        self.validate_union_nullability(&components)?;
        Ok(TypeExpr::Union(components))
    }

    /// Per CIP-86's "Not Null Uses" section: within a closed dynamic
    /// union's component list, either every component must be `NOT NULL`
    /// or none may be -- mixed nullability is a compile-time syntax error.
    fn validate_union_nullability(&self, components: &[TypeExpr]) -> ParseResult<()> {
        let not_null = components[0].direct_not_null();
        if components.iter().all(|c| c.direct_not_null() == not_null) {
            Ok(())
        } else {
            Err(self.error(
                "invalid type: mixed nullability among union components (either all \
                 components of a closed union must be NOT NULL or none may be)",
            ))
        }
    }

    /// Parses a single type term: a `<predefined type>` / `<constructed
    /// type>` / `<dynamic union type>` leaf, followed by zero or more
    /// trailing `LIST`/`ARRAY` postfixes (`T LIST`, `T LIST LIST`, ...).
    /// The trailing-list postfix binds to this single term only, tighter
    /// than the `|` handled by `parse_value_type` -- per CIP-86's "List
    /// Syntax Additions", `BOOLEAN | STRING LIST` means `BOOLEAN |
    /// LIST<STRING>`, not `LIST<BOOLEAN | STRING>`.
    fn parse_type_term(&mut self) -> ParseResult<TypeExpr> {
        let mut term = self.parse_type_leaf()?;
        while self.peek_is_list_or_array_keyword() {
            self.next_token();
            let not_null = self.maybe_consume_not_null();
            term = TypeExpr::List {
                element: Box::new(term),
                not_null,
            };
        }
        Ok(term)
    }

    fn peek_is_list_or_array_keyword(&self) -> bool {
        self.peek_is_ident("LIST") || self.peek_is_ident("ARRAY")
    }

    /// Consumes a trailing `NOT NULL` or its `!` shorthand (CIP-102) if
    /// present (with backtracking, since `NOT` alone does not commit to it);
    /// returns whether it was found. Whitespace (including newlines) before
    /// `!` is permitted since the lexer already discards it between tokens
    /// — matching observed Neo4j behavior rather than the CIP text, which
    /// says whitespace should be disallowed (see CIP-102 NOTES.md Deviations).
    fn maybe_consume_not_null(&mut self) -> bool {
        if self.peek_type() == TokenType::Bang {
            self.next_token();
            return true;
        }
        if self.peek_type() == TokenType::NOT {
            let saved = self.save_pos();
            self.next_token();
            if self.peek_type() == TokenType::NULL {
                self.next_token();
                true
            } else {
                self.restore_pos(saved);
                false
            }
        } else {
            false
        }
    }

    /// Parses a single leaf `<value type>` production (everything except
    /// the trailing-`LIST`/`ARRAY` postfix, which `parse_type_term` layers
    /// on top): predefined scalar/temporal/reference/point/map/nothing/null
    /// types, the `LIST<T>`/`ARRAY<T>` bracket form, `[ANY] PROPERTY
    /// VALUE`, and the `ANY [...]` family (open union, closed union,
    /// `ANY MAP`/`ANY NODE`/etc).
    fn parse_type_leaf(&mut self) -> ParseResult<TypeExpr> {
        let tok = self.peek().clone();
        match tok.ty {
            TokenType::NULL => self.parse_leaf_predefined(PredefinedKind::Null),
            TokenType::NODE => self.parse_leaf_predefined(PredefinedKind::Node),
            TokenType::POINT => self.parse_leaf_predefined(PredefinedKind::Point),
            TokenType::VECTOR => self.parse_vector_type(),
            TokenType::Identifier => {
                let upper = tok.literal.to_ascii_uppercase();
                match upper.as_str() {
                    "ANY" => self.parse_any_type(),
                    "NOTHING" => self.parse_leaf_predefined(PredefinedKind::Nothing),
                    "BOOL" | "BOOLEAN" => self.parse_leaf_predefined(PredefinedKind::Boolean),
                    "STRING" | "VARCHAR" => self.parse_leaf_predefined(PredefinedKind::String),
                    "INT" | "INTEGER" | "INT64" | "INTEGER64" => {
                        self.parse_leaf_predefined(PredefinedKind::Integer)
                    }
                    "SIGNED" => {
                        self.next_token();
                        self.expect_keyword_ident("INTEGER")?;
                        let not_null = self.maybe_consume_not_null();
                        Ok(TypeExpr::Predefined {
                            kind: PredefinedKind::Integer,
                            not_null,
                        })
                    }
                    "FLOAT" | "FLOAT64" => self.parse_leaf_predefined(PredefinedKind::Float),
                    // CIP-216: the four numeric coordinate types with no
                    // 64-bit-aliased plain form are valid only inside
                    // `VECTOR<...>`/`VECTOR(...)`, not as a standalone
                    // property type.
                    "INTEGER8" | "INT8" | "INTEGER16" | "INT16" | "INTEGER32" | "INT32"
                    | "FLOAT32" => Err(self.error_at_token(
                        &tok,
                        format!(
                            "Invalid input '{}', vector-only numeric coordinate types are not \
                             property value types on their own and cannot be used outside \
                             VECTOR<...>",
                            tok.literal
                        ),
                    )),
                    "DATE" => self.parse_leaf_predefined(PredefinedKind::Date),
                    "DURATION" => self.parse_leaf_predefined(PredefinedKind::Duration),
                    "VERTEX" => self.parse_leaf_predefined(PredefinedKind::Node),
                    "EDGE" | "RELATIONSHIP" => {
                        self.parse_leaf_predefined(PredefinedKind::Relationship)
                    }
                    "PATH" => self.parse_leaf_predefined(PredefinedKind::Path),
                    "MAP" => self.parse_leaf_predefined(PredefinedKind::Map),
                    // CIP-246
                    "UUID" => self.parse_leaf_predefined(PredefinedKind::Uuid),
                    "PROPERTY" => {
                        self.next_token();
                        self.parse_property_value_tail()
                    }
                    "LIST" | "ARRAY" => {
                        self.next_token();
                        self.expect(TokenType::LessThan)?;
                        let element = self.parse_value_type()?;
                        self.expect(TokenType::GreaterThan)?;
                        let not_null = self.maybe_consume_not_null();
                        Ok(TypeExpr::List {
                            element: Box::new(element),
                            not_null,
                        })
                    }
                    "TIME" => {
                        self.next_token();
                        self.parse_time_zone_suffix(
                            PredefinedKind::ZonedTime,
                            PredefinedKind::LocalTime,
                        )
                    }
                    "DATETIME" => {
                        self.next_token();
                        self.parse_time_zone_suffix(
                            PredefinedKind::ZonedDateTime,
                            PredefinedKind::LocalDateTime,
                        )
                    }
                    "TIMESTAMP" => {
                        self.next_token();
                        // Unlike bare TIME/DATETIME (whose no-suffix case
                        // means the zoned variant), TIMESTAMP has no
                        // suffix-less form: WITH or WITHOUT is required.
                        if self.peek_type() == TokenType::WITH || self.peek_is_ident("WITHOUT") {
                            self.parse_time_zone_suffix(
                                PredefinedKind::ZonedDateTime,
                                PredefinedKind::LocalDateTime,
                            )
                        } else {
                            let bad = self.peek().clone();
                            Err(self.error_at_token(
                                &bad,
                                format!("expected WITH or WITHOUT, got {:?}", bad.literal),
                            ))
                        }
                    }
                    "LOCAL" => self.parse_time_or_datetime_prefixed(
                        PredefinedKind::LocalTime,
                        PredefinedKind::LocalDateTime,
                    ),
                    "ZONED" => self.parse_time_or_datetime_prefixed(
                        PredefinedKind::ZonedTime,
                        PredefinedKind::ZonedDateTime,
                    ),
                    _ => Err(self.error_at_token(
                        &tok,
                        format!("expected a type name, got {:?}", tok.literal),
                    )),
                }
            }
            _ => {
                Err(self
                    .error_at_token(&tok, format!("expected a type name, got {:?}", tok.literal)))
            }
        }
    }

    /// Parses the two-word `LOCAL`/`ZONED` prefixed temporal forms: the
    /// already-matched prefix word (consumed here) selects the kind pair,
    /// the second word (`TIME`/`DATETIME`) selects the kind within it.
    fn parse_time_or_datetime_prefixed(
        &mut self,
        time: PredefinedKind,
        datetime: PredefinedKind,
    ) -> ParseResult<TypeExpr> {
        self.next_token();
        if self.peek_is_ident("TIME") {
            return self.parse_leaf_predefined(time);
        }
        if self.peek_is_ident("DATETIME") {
            return self.parse_leaf_predefined(datetime);
        }
        let bad = self.peek().clone();
        Err(self.error_at_token(
            &bad,
            format!("expected TIME or DATETIME, got {:?}", bad.literal),
        ))
    }

    /// Parses a `VECTOR` type leaf (CIP-216), already having peeked the
    /// `VECTOR` keyword: Cypher style `VECTOR<TYPE [NOT NULL]>(DIM)`, GQL
    /// style `VECTOR(DIM, TYPE [NOT NULL])`, and both forms' partial
    /// variants (`VECTOR<TYPE>` with no dimension, `VECTOR(DIM)` with no
    /// coordinate type) -- grammatically valid, rejected only later at
    /// constraint-creation time (CIP-216 Negative 1/2, "need to define
    /// both coordinate type and dimension").
    fn parse_vector_type(&mut self) -> ParseResult<TypeExpr> {
        self.next_token(); // consume VECTOR
        let mut coord = None;
        if self.peek_type() == TokenType::LessThan {
            self.next_token();
            coord = Some(self.parse_vector_coord_type()?);
            self.expect(TokenType::GreaterThan)?;
        }
        let mut dimension = None;
        if self.peek_type() == TokenType::LeftParen {
            self.next_token();
            dimension = Some(self.parse_vector_dimension()?);
            // The GQL form's `, TYPE` only applies when the Cypher-style
            // `<...>` above didn't already supply a coordinate type.
            if coord.is_none() && self.peek_type() == TokenType::Comma {
                self.next_token();
                coord = Some(self.parse_vector_coord_type()?);
            }
            self.expect(TokenType::RightParen)?;
        }
        let not_null = self.maybe_consume_not_null();
        Ok(TypeExpr::Vector {
            coord,
            dimension,
            not_null,
        })
    }

    /// Parses a single vector coordinate type keyword (CIP-216's
    /// restricted numeric vocabulary, not the general `<value type>`
    /// grammar) plus its always-implicit-but-optional trailing `NOT NULL`.
    fn parse_vector_coord_type(&mut self) -> ParseResult<VectorCoordKind> {
        let tok = self.peek().clone();
        let kind = if tok.ty == TokenType::Identifier {
            match tok.literal.to_ascii_uppercase().as_str() {
                "INT8" | "INTEGER8" => Some(VectorCoordKind::Int8),
                "INT16" | "INTEGER16" => Some(VectorCoordKind::Int16),
                "INT32" | "INTEGER32" => Some(VectorCoordKind::Int32),
                "INT" | "INTEGER" | "INT64" | "INTEGER64" => Some(VectorCoordKind::Int64),
                "FLOAT32" => Some(VectorCoordKind::Float32),
                "FLOAT" | "FLOAT64" => Some(VectorCoordKind::Float64),
                _ => None,
            }
        } else {
            None
        };
        let kind = kind.ok_or_else(|| {
            self.error_at_token(
                &tok,
                format!("{} is not a valid vector coordinate type", tok.literal),
            )
        })?;
        self.next_token();
        self.maybe_consume_not_null();
        Ok(kind)
    }

    /// Parses a vector's `(DIMENSION)` integer literal, validating the
    /// CIP-216 range (1 to 4096 inclusive) immediately.
    fn parse_vector_dimension(&mut self) -> ParseResult<i64> {
        let neg = if self.peek_type() == TokenType::Dash {
            self.next_token();
            true
        } else {
            false
        };
        let tok = self.peek().clone();
        if tok.ty != TokenType::Integer {
            return Err(self.error_at_token(
                &tok,
                format!(
                    "expected an integer vector dimension, got {:?}",
                    tok.literal
                ),
            ));
        }
        self.next_token();
        let mut val = parse_int_literal(&tok.literal).map_err(|e| self.error_at_token(&tok, e))?;
        if neg {
            val = -val;
        }
        if !(1..=4096).contains(&val) {
            return Err(self.error_at_token(
                &tok,
                format!(
                    "Expected 'DIMENSION' to be INTEGER in the range 1 to 4096 but found {val}."
                ),
            ));
        }
        Ok(val)
    }

    /// Consumes the current (already-peeked) leaf type keyword and an
    /// optional trailing `NOT NULL`.
    fn parse_leaf_predefined(&mut self, kind: PredefinedKind) -> ParseResult<TypeExpr> {
        self.next_token();
        let not_null = self.maybe_consume_not_null();
        Ok(TypeExpr::Predefined { kind, not_null })
    }

    /// Parses the `WITH TIME ZONE` / `WITHOUT TIME ZONE` suffix that may
    /// follow a bare `TIME` or `DATETIME` leaf (`<time type>`/`<datetime
    /// type>`'s second alternative). Without a suffix, the bare leaf is
    /// treated as the zoned variant.
    fn parse_time_zone_suffix(
        &mut self,
        zoned: PredefinedKind,
        local: PredefinedKind,
    ) -> ParseResult<TypeExpr> {
        // The kind is the only per-branch difference; the suffix words
        // and the NOT NULL tail are shared.
        let kind = if self.peek_type() == TokenType::WITH {
            self.next_token();
            self.expect_time_zone_words()?;
            zoned
        } else if self.peek_is_ident("WITHOUT") {
            self.next_token();
            self.expect_time_zone_words()?;
            local
        } else {
            zoned
        };
        let not_null = self.maybe_consume_not_null();
        Ok(TypeExpr::Predefined { kind, not_null })
    }

    /// Consumes the `TIME ZONE` tail of a `WITH[OUT] TIME ZONE` type
    /// suffix, accepting both the two-word spelling and the single-word
    /// `TIMEZONE` alias (both appear in the wild, e.g. CIP-100's own
    /// `TIMESTAMP WITHOUT TIMEZONE`).
    fn expect_time_zone_words(&mut self) -> ParseResult<()> {
        if self.peek_is_ident("TIMEZONE") {
            self.next_token();
            return Ok(());
        }
        self.expect_keyword_ident("TIME")?;
        self.expect_keyword_ident("ZONE")?;
        Ok(())
    }

    /// Consumes `VALUE` plus an optional `NOT NULL` after an already-matched
    /// `PROPERTY` — the `[ANY] PROPERTY VALUE` production's tail, shared by
    /// the bare and the ANY-prefixed spelling.
    fn parse_property_value_tail(&mut self) -> ParseResult<TypeExpr> {
        self.expect_keyword_ident("VALUE")?;
        let not_null = self.maybe_consume_not_null();
        Ok(TypeExpr::PropertyValue { not_null })
    }

    /// Parses everything that can follow the `ANY` keyword: `ANY PROPERTY
    /// VALUE`, `ANY NODE`/`ANY VERTEX`, `ANY EDGE`/`ANY RELATIONSHIP`, `ANY
    /// MAP`, the open dynamic union `ANY [VALUE] [NOT NULL]`, and the
    /// closed dynamic union bracket form `ANY [VALUE] <A | B | ...>` (which
    /// grammatically has no `[NOT NULL]` of its own).
    fn parse_any_type(&mut self) -> ParseResult<TypeExpr> {
        self.next_token(); // consume ANY

        if self.peek_is_ident("PROPERTY") {
            self.next_token();
            return self.parse_property_value_tail();
        }
        if self.peek_type() == TokenType::NODE || self.peek_is_ident("VERTEX") {
            return self.parse_leaf_predefined(PredefinedKind::Node);
        }
        if self.peek_is_ident("EDGE") || self.peek_is_ident("RELATIONSHIP") {
            return self.parse_leaf_predefined(PredefinedKind::Relationship);
        }
        if self.peek_is_ident("MAP") {
            return self.parse_leaf_predefined(PredefinedKind::Map);
        }
        if self.peek_is_ident("VALUE") {
            self.next_token();
        }

        if self.peek_type() == TokenType::LessThan {
            self.next_token();
            let inner = self.parse_value_type()?;
            self.expect(TokenType::GreaterThan)?;
            // No [NOT NULL] production exists for a closed union: a
            // trailing NOT NULL is left unconsumed here and becomes a
            // syntax error further up (CIP-86 "Not Null Uses" > invalid).
            return Ok(match inner {
                TypeExpr::Union(components) => TypeExpr::Union(components),
                single => TypeExpr::Union(vec![single]),
            });
        }

        let not_null = self.maybe_consume_not_null();
        Ok(TypeExpr::Predefined {
            kind: PredefinedKind::Any,
            not_null,
        })
    }
}
