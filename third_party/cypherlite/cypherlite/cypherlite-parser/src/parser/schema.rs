use super::*;

impl Parser {
    /// Parses the `FOR (n:Label)` / `FOR (n:A|B|...)` / `FOR (n)` /
    /// `FOR ()-[r:TYPE]-()` (all four direction spellings) / `FOR ()-[r]-()`
    /// DDL target shared by `CREATE INDEX` and `CREATE CONSTRAINT`
    /// (CIP-11's relationship-pattern generalization). Returns the entity
    /// kind, the flat label/relationship-type list (empty for a label-less
    /// LOOKUP target), and the pattern's own variable name (the property
    /// list that follows, e.g. `ON (n.prop)`, is written in terms of it).
    pub(super) fn parse_schema_for_target(
        &mut self,
    ) -> ParseResult<(SchemaEntityType, Vec<String>, String)> {
        self.expect(TokenType::FOR)?;
        if self.peek_type() != TokenType::LeftParen {
            let tok = self.peek().clone();
            return Err(self.error_at_token(
                &tok,
                format!("expected '(' after FOR, got {:?}", tok.literal),
            ));
        }
        // A relationship pattern always has empty parens on both sides in
        // this DDL grammar (`FOR ()-[r:TYPE]-()`, `FOR ()<-[r:TYPE]-()`, ...)
        // -- distinguished from the node form by a `)` immediately followed
        // by a dash/left-arrow. Unlike `parse_constraint_for_target_ext`
        // (CIP-177's own, more permissive `FOR (m)-[...]->()` grammar),
        // this stays strict: `CREATE INDEX`/`CREATE VECTOR INDEX` reject a
        // labelled or aliased endpoint on either side (CIP-116 create-
        // index-errors[18]/[19]).
        if self.peek_at(1).ty == TokenType::RightParen
            && matches!(self.peek_at(2).ty, TokenType::Dash | TokenType::ArrowLeft)
        {
            self.parse_node_pattern()?; // consume the empty left `()`
            let rel = self.parse_rel_pattern()?;
            self.parse_node_pattern()?; // consume the (also empty) right `()`
            let labels = match &rel.type_expr {
                Some(le) => le.as_ddl_list().ok_or_else(|| {
                    self.error(
                        "expected a plain relationship type or A|B|... list in the FOR pattern",
                    )
                })?,
                None => Vec::new(),
            };
            let var = rel.variable.clone().unwrap_or_default();
            return Ok((SchemaEntityType::Relationship, labels, var));
        }

        let node = self.parse_node_pattern()?;
        let labels = match &node.label_expr {
            Some(le) => le.as_ddl_list().ok_or_else(|| {
                self.error("expected a plain label or A|B|... list in the FOR pattern")
            })?,
            None => Vec::new(),
        };
        let var = node.variable.clone().unwrap_or_default();
        Ok((SchemaEntityType::Node, labels, var))
    }

    /// Like `parse_schema_for_target`, but for `CREATE CONSTRAINT` only:
    /// also returns the relationship pattern's own source/destination
    /// endpoint aliases, and permits a labelled/aliased endpoint on either
    /// side (CIP-177's `FOR (m)-[:TYPE]->()` / `FOR ()-[:TYPE]->(m)`
    /// label-existence constraints need to know which endpoint `m`
    /// names). `None`/`None` for a node target. Deliberately not shared
    /// with `parse_schema_for_target` -- `CREATE INDEX` must keep
    /// rejecting a labelled/aliased relationship-pattern endpoint (CIP-116
    /// create-index-errors[18]/[19]).
    pub(super) fn parse_constraint_for_target_ext(&mut self) -> ParseResult<ConstraintForTarget> {
        self.expect(TokenType::FOR)?;
        if self.peek_type() != TokenType::LeftParen {
            let tok = self.peek().clone();
            return Err(self.error_at_token(
                &tok,
                format!("expected '(' after FOR, got {:?}", tok.literal),
            ));
        }
        // Try the source endpoint as a general node pattern (empty `()`,
        // or one carrying just an alias like `(m)` -- CIP-177's
        // source/destination label-existence constraints) -- distinguished
        // from a plain node FOR-target by the closing `)` being immediately
        // followed by a dash/left-arrow.
        let saved = self.save_pos();
        let source = self.parse_node_pattern()?;
        if matches!(self.peek_type(), TokenType::Dash | TokenType::ArrowLeft) {
            let rel = self.parse_rel_pattern()?;
            let target = self.parse_node_pattern()?;
            let labels = match &rel.type_expr {
                Some(le) => le.as_ddl_list().ok_or_else(|| {
                    self.error(
                        "expected a plain relationship type or A|B|... list in the FOR pattern",
                    )
                })?,
                None => Vec::new(),
            };
            return Ok(ConstraintForTarget {
                entity_type: SchemaEntityType::Relationship,
                labels,
                variable: rel.variable.unwrap_or_default(),
                source_alias: source.variable,
                target_alias: target.variable,
            });
        }
        self.restore_pos(saved);

        let node = self.parse_node_pattern()?;
        let labels = match &node.label_expr {
            Some(le) => le.as_ddl_list().ok_or_else(|| {
                self.error("expected a plain label or A|B|... list in the FOR pattern")
            })?,
            None => Vec::new(),
        };
        Ok(ConstraintForTarget {
            entity_type: SchemaEntityType::Node,
            labels,
            variable: node.variable.unwrap_or_default(),
            source_alias: None,
            target_alias: None,
        })
    }

    /// `ON (n.prop1, n.prop2, ...)` / paren-less single-property `ON
    /// n.prop` (CIP-11 CR[10]).
    pub(super) fn parse_on_property_list(&mut self) -> ParseResult<Vec<String>> {
        let paren = self.peek_type() == TokenType::LeftParen;
        if paren {
            self.next_token();
        }
        let mut properties = Vec::new();
        loop {
            self.next_token(); // variable name (skip)
            self.expect(TokenType::Dot)?;
            properties.push(self.parse_bare_name("property name")?);
            if !paren || self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        if paren {
            self.expect(TokenType::RightParen)?;
        }
        Ok(properties)
    }

    /// `ON EACH [n.prop1, n.prop2, ...]` (CIP-38 FULLTEXT): a bracketed list
    /// of plain property references -- Neo4j's actual grammar is narrower
    /// than the CIP's own `EACH <expression>` text (Deviation: no general
    /// expressions, e.g. `p.title + p.abstract`, are accepted), and the
    /// list may never be empty.
    pub(super) fn parse_on_each_bracket_list(&mut self) -> ParseResult<Vec<String>> {
        self.expect(TokenType::LeftBracket)?;
        if self.peek_type() == TokenType::RightBracket {
            return Err(self.error("InvalidSyntax: ON EACH requires at least one property"));
        }
        let mut properties = Vec::new();
        loop {
            self.next_token(); // variable name (skip)
            self.expect(TokenType::Dot)?;
            properties.push(self.parse_bare_name("property name")?);
            if self.peek_type() != TokenType::Comma {
                break;
            }
            self.next_token();
        }
        self.expect(TokenType::RightBracket)?;
        let mut seen = std::collections::HashSet::new();
        for p in &properties {
            if !seen.insert(p.clone()) {
                return Err(self.error(format!(
                    "IndexWithDuplicatedTokens: duplicate property '{p}' in ON EACH list"
                )));
            }
        }
        Ok(properties)
    }

    /// `ON EACH labels(n)` / `ON EACH type(r)` (CIP-11 CR[19]/OPT[8]): the
    /// LOOKUP token-index target. The argument is the FOR-pattern's own
    /// variable; a LOOKUP index has no fixed property/label list, so
    /// nothing beyond syntax is captured here.
    pub(super) fn parse_lookup_each_target(
        &mut self,
        entity_type: SchemaEntityType,
    ) -> ParseResult<()> {
        let fn_tok = self.next_token();
        let want = match entity_type {
            SchemaEntityType::Node => "labels",
            SchemaEntityType::Relationship => "type",
        };
        if !fn_tok.literal.eq_ignore_ascii_case(want) {
            return Err(self.error_at_token(
                &fn_tok,
                format!("expected {}(...), got {:?}", want, fn_tok.literal),
            ));
        }
        self.expect(TokenType::LeftParen)?;
        self.next_token(); // the pattern variable (skip)
        self.expect(TokenType::RightParen)?;
        Ok(())
    }

    /// The optional `<name>` in `CREATE INDEX`/`CREATE CONSTRAINT`: a plain
    /// identifier (including the schema keywords `INDEX`/`CONSTRAINT` used
    /// bare, which Neo4j allows as a name here) or a `$param` reference
    /// (CIP-134). Deliberately narrower than `TokenType::is_ident_like`:
    /// that set also contains `IF`/`FOR`/`NOT`/`EXISTS`/etc., the very
    /// tokens expected to immediately follow a name position, so accepting
    /// them here would misparse `CREATE INDEX IF NOT EXISTS ...` (no name)
    /// as naming the index `IF`. Absent (`None`) whenever the next token
    /// isn't one of the safe cases, matching the un-named form.
    pub(super) fn parse_optional_schema_name(&mut self) -> Option<SchemaName> {
        match self.peek_type() {
            TokenType::Parameter => {
                let tok = self.next_token();
                Some(SchemaName::Parameter(tok.literal))
            }
            TokenType::Identifier | TokenType::INDEX | TokenType::CONSTRAINT => {
                let tok = self.next_token();
                Some(SchemaName::Literal(tok.literal))
            }
            _ => None,
        }
    }

    /// The mandatory `<name>` in `DROP INDEX`/`DROP CONSTRAINT` (CIP-134
    /// adds the `$param` form; unlike CREATE, DROP always requires a name
    /// token here).
    pub(super) fn parse_required_schema_name(&mut self, context: &str) -> ParseResult<SchemaName> {
        match self.peek_type() {
            TokenType::Parameter => {
                let tok = self.next_token();
                Ok(SchemaName::Parameter(tok.literal))
            }
            TokenType::Identifier | TokenType::INDEX | TokenType::CONSTRAINT => {
                let tok = self.next_token();
                Ok(SchemaName::Literal(tok.literal))
            }
            _ => {
                let tok = self.peek().clone();
                Err(self.error_at_token(
                    &tok,
                    format!("expected a name after {}, got {:?}", context, tok.literal),
                ))
            }
        }
    }

    /// Consumes an `IF EXISTS` if one comes next; returns whether it did.
    pub(super) fn parse_optional_if_exists(&mut self) -> ParseResult<bool> {
        if self.peek_type() != TokenType::IF {
            return Ok(false);
        }
        self.next_token();
        self.expect(TokenType::EXISTS)?;
        Ok(true)
    }

    /// `<language name> ::= CYPHER <digit>...` -- the version digits are
    /// *not* range-checked here (CIP-170 NOTES.md Deviations #2: an
    /// unsupported value inside `DEFAULT LANGUAGE` is a runtime data
    /// exception, not a compile-time syntax error, unlike CIP-150's
    /// statement-preamble `CYPHER <version>` prefix -- see
    /// `validate::validate_default_language_version`). Only the token
    /// shape (`CYPHER` followed by an integer) is a hard grammar
    /// requirement.
    pub(super) fn parse_cypher_language_name(&mut self) -> ParseResult<u32> {
        if !self.peek_is_ident("cypher") {
            let tok = self.peek().clone();
            return Err(self.error_at_token(
                &tok,
                format!("expected CYPHER in <language name>, got {:?}", tok.literal),
            ));
        }
        self.next_token(); // CYPHER
        let tok = self.next_token();
        if tok.ty != TokenType::Integer {
            return Err(self.error_at_token(
                &tok,
                format!(
                    "expected a version number after CYPHER, got {:?}",
                    tok.literal
                ),
            ));
        }
        tok.literal.parse::<u32>().map_err(|_| {
            self.error_at_token(
                &tok,
                format!("invalid cypher version number: {:?}", tok.literal),
            )
        })
    }

    /// `ALTER DATABASE <name> [IF EXISTS] SET DEFAULT LANGUAGE CYPHER
    /// <n>` (CIP-170) -- under ADR-0116 §2's fixed namespaces this is the
    /// *only* supported `ALTER DATABASE` form: a sole `SET DEFAULT
    /// LANGUAGE` item with no `WAIT` clause. Every other form (`SET
    /// ACCESS`, `SET OPTION`, `REMOVE OPTION`, `SET TOPOLOGY`, any
    /// additional `SET` item, or a trailing `WAIT`/`NOWAIT`) is
    /// recognized and rejected with CE's
    /// `UnsupportedAdministrationCommand`.
    pub(super) fn parse_alter_database(&mut self) -> ParseResult<Clause> {
        let start = self.pos;
        self.next_token(); // consume ALTER
        self.next_token(); // consume DATABASE
        let name = self.parse_required_schema_name("ALTER DATABASE")?;
        let if_exists = self.parse_optional_if_exists()?;
        if self.peek_type() == TokenType::SET && self.peek_at_is_ident(1, "default") {
            self.next_token(); // SET
            let default_language = self.parse_alter_database_default_language()?;
            if matches!(self.peek_type(), TokenType::EOF | TokenType::Semicolon) {
                return Ok(Clause::AlterDatabase(AlterDatabaseClause {
                    name,
                    if_exists,
                    default_language,
                }));
            }
            // A second SET item or a WAIT/NOWAIT clause: recognized but
            // unsupported on fixed namespaces (ADR-0116 §2).
            return self.unsupported_admin_clause(start);
        }
        if self.peek_type() == TokenType::SET || self.peek_type() == TokenType::REMOVE {
            return self.unsupported_admin_clause(start);
        }
        let tok = self.peek().clone();
        Err(self.error_at_token(
            &tok,
            format!(
                "expected SET after ALTER DATABASE ... [IF EXISTS], got {:?}",
                tok.literal
            ),
        ))
    }

    /// The one supported `<alter database set item>`: `DEFAULT LANGUAGE
    /// CYPHER <n>` (ADR-0116 §2). The digits are unvalidated at parse time
    /// -- CIP-170 NOTES.md Deviations #2: an out-of-range version is a
    /// *runtime* data exception (`22N04`).
    pub(super) fn parse_alter_database_default_language(&mut self) -> ParseResult<u32> {
        self.next_token(); // DEFAULT
        self.expect_keyword_ident("LANGUAGE")?;
        self.parse_cypher_language_name()
    }

    pub(super) fn parse_create_index(&mut self, kind: IndexKind) -> ParseResult<CreateIndexClause> {
        self.next_token(); // consume INDEX

        let mut name = self.parse_optional_schema_name();

        let if_not_exists = if self.peek_type() == TokenType::IF {
            self.next_token();
            self.expect(TokenType::NOT)?;
            self.expect(TokenType::EXISTS)?;
            true
        } else {
            false
        };
        if name.is_none() {
            name = self.parse_optional_schema_name();
        }

        if self.peek_type() == TokenType::ON {
            let (label, properties) = self.parse_legacy_index_target()?;
            return Ok(CreateIndexClause {
                name,
                entity_type: SchemaEntityType::Node,
                labels: vec![label],
                properties,
                if_not_exists,
                kind,
                on_each: false,
                with_properties: Vec::new(),
                options: Vec::new(),
                options_param: None,
            });
        }

        let (entity_type, labels, _var) = self.parse_schema_for_target()?;

        self.expect(TokenType::ON)?;

        let each_next = self.peek_type() == TokenType::Identifier
            && self.peek().literal.eq_ignore_ascii_case("each");
        if each_next && !matches!(kind, IndexKind::Fulltext | IndexKind::Lookup) {
            let tok = self.peek().clone();
            return Err(self.error_at_token(
                &tok,
                "InvalidSyntax: ON EACH is only supported for FULLTEXT and LOOKUP indexes"
                    .to_string(),
            ));
        }
        if !each_next && matches!(kind, IndexKind::Fulltext | IndexKind::Lookup) {
            return Err(self.error(
                "InvalidSyntax: FULLTEXT/LOOKUP indexes require ON EACH [...] / ON EACH labels(n)/type(r)",
            ));
        }

        let (properties, on_each) = if each_next {
            self.next_token(); // consume EACH
            if kind == IndexKind::Lookup {
                self.parse_lookup_each_target(entity_type)?;
                (Vec::new(), true)
            } else {
                (self.parse_on_each_bracket_list()?, true)
            }
        } else {
            (self.parse_on_property_list()?, false)
        };

        // CIP-235: `WITH [var.prop1, var.prop2, ...]` -- a VECTOR-only
        // clause listing additional filtering properties, positioned
        // between `ON` and `OPTIONS`. Reuses the CIP-38 `ON EACH [...]`
        // bracket-list parser, which already rejects an empty list (CF3)
        // and duplicate properties within the list (CF8) with exactly the
        // shape this clause needs.
        let with_properties = if kind == IndexKind::Vector && self.peek_type() == TokenType::WITH {
            self.next_token(); // consume WITH
            self.parse_on_each_bracket_list()?
        } else {
            Vec::new()
        };

        let (options, options_param) = if self.peek_type() == TokenType::OPTIONS {
            self.next_token();
            if self.peek_type() == TokenType::Parameter {
                // CIP-116 CR[4]: `OPTIONS $param`, resolved against the
                // query's parameter map at execution time.
                let tok = self.next_token();
                (Vec::new(), Some(tok.literal))
            } else {
                let pairs = self.parse_map_literal()?;
                (pairs.into_iter().map(|p| (p.key, p.value)).collect(), None)
            }
        } else {
            (Vec::new(), None)
        };

        Ok(CreateIndexClause {
            name,
            entity_type,
            labels,
            properties,
            if_not_exists,
            kind,
            on_each,
            with_properties,
            options,
            options_param,
        })
    }

    /// Parses the legacy openCypher index target retained by CypherLite,
    /// `ON :Label(property[, ...])`, starting at `ON`, for `CREATE INDEX`
    /// and `DROP INDEX`. Returns the label (a keyword included, ADR-0169)
    /// and at least one property name; `parse_drop` rejects a legacy
    /// `DROP INDEX` target with more than one.
    pub(super) fn parse_legacy_index_target(&mut self) -> ParseResult<(String, Vec<String>)> {
        self.expect(TokenType::ON)?;
        self.expect(TokenType::Colon)?;
        let label = self.parse_label_name()?;
        self.expect(TokenType::LeftParen)?;
        let mut properties = vec![self.parse_bare_name("property name")?];
        while self.peek_type() == TokenType::Comma {
            self.next_token();
            properties.push(self.parse_bare_name("property name")?);
        }
        self.expect(TokenType::RightParen)?;
        Ok((label, properties))
    }

    pub(super) fn parse_create_constraint(&mut self) -> ParseResult<CreateConstraintClause> {
        self.next_token(); // consume CONSTRAINT

        let mut name = self.parse_optional_schema_name();

        let if_not_exists = if self.peek_type() == TokenType::IF {
            self.next_token();
            self.expect(TokenType::NOT)?;
            self.expect(TokenType::EXISTS)?;
            true
        } else {
            false
        };
        if name.is_none() {
            name = self.parse_optional_schema_name();
        }

        let ConstraintForTarget {
            entity_type,
            labels,
            variable,
            source_alias,
            target_alias,
        } = self.parse_constraint_for_target_ext()?;
        if labels.len() > 1 {
            return Err(self.error(
                "constraints support only a single label or relationship type, not an A|B|... list",
            ));
        }
        let label = labels.into_iter().next().unwrap_or_default();

        self.expect(TokenType::REQUIRE)?;

        // CIP-177: `REQUIRE <alias>:<Label>` -- a label existence
        // constraint (NLABELEXIST / ESRCLABELEXIST / EDESTLABELEXIST),
        // distinct from every other REQUIRE form (all of which continue
        // with `.` into a property path). Neo4j has no runtime for this
        // (CIP-177's own DDL is explicitly illustrative, not normative),
        // so this is CypherLite's implementation target derived from the
        // CIP's notation: a node-entity `alias:Label` requires the FOR
        // target's own label to co-occur with `Label`; a relationship
        // entity's `alias` must name one of the pattern's own source/
        // destination endpoint aliases, selecting which endpoint the
        // constraint applies to.
        if self.peek_type().is_ident_like() && self.peek_at(1).ty == TokenType::Colon {
            let alias_tok = self.next_token();
            self.expect(TokenType::Colon)?;
            let target_label = self.parse_label_name()?;
            let constraint_type = match entity_type {
                // Distinct from `"labelExistence"` (only ever created by
                // `ALTER CURRENT GRAPH TYPE`, CIP-187): both render the
                // same `NODE_LABEL_EXISTENCE` SHOW CONSTRAINTS type, but
                // this standalone `CREATE CONSTRAINT` form is an
                // independent constraint, not one the current graph type
                // manages -- see `classification` in `clause::show`.
                SchemaEntityType::Node => "nodeLabelExistence".to_string(),
                SchemaEntityType::Relationship => {
                    if source_alias.as_deref() == Some(alias_tok.literal.as_str()) {
                        "sourceLabelExistence".to_string()
                    } else if target_alias.as_deref() == Some(alias_tok.literal.as_str()) {
                        "targetLabelExistence".to_string()
                    } else {
                        return Err(self.error_at_token(
                            &alias_tok,
                            format!(
                                "expected the source or destination endpoint alias, got {:?}",
                                alias_tok.literal
                            ),
                        ));
                    }
                }
            };
            return Ok(CreateConstraintClause {
                name,
                entity_type,
                label,
                variable,
                properties: Vec::new(),
                constraint_type,
                if_not_exists,
                property_type: None,
                options: Vec::new(),
                enforced_label: Some(target_label),
            });
        }

        let mut properties = Vec::new();
        if self.peek_type() == TokenType::LeftParen {
            self.next_token();
            loop {
                self.next_token(); // variable
                self.expect(TokenType::Dot)?;
                let prop_tok = self.next_token();
                properties.push(prop_tok.literal.clone());
                if self.peek_type() != TokenType::Comma {
                    break;
                }
                self.next_token();
            }
            self.expect(TokenType::RightParen)?;
        } else {
            self.next_token(); // variable
            self.expect(TokenType::Dot)?;
            let prop_tok = self.next_token();
            properties.push(prop_tok.literal.clone());
        }

        // CIP-216 01#2/#4: the bare `x :: <value type>` surface form (CIP-86)
        // is equivalent to `x IS :: <value type>` and skips the `IS` keyword
        // entirely -- checked before the mandatory `expect(IS)` below, which
        // covers every other constraint-verb spelling (all of which require
        // `IS`).
        if self.peek_type() == TokenType::DoubleColon {
            self.next_token();
            let te = self.parse_value_type()?;
            let options = if self.peek_type() == TokenType::OPTIONS {
                self.next_token();
                let pairs = self.parse_map_literal()?;
                pairs.into_iter().map(|p| (p.key, p.value)).collect()
            } else {
                Vec::new()
            };
            return Ok(CreateConstraintClause {
                name,
                entity_type,
                label,
                variable,
                properties,
                constraint_type: "propertyType".to_string(),
                if_not_exists,
                property_type: Some(te),
                options,
                enforced_label: None,
            });
        }

        self.expect(TokenType::IS)?;
        let tok = self.peek().clone();
        let (constraint_type, property_type) = match tok.ty {
            TokenType::UNIQUE => {
                self.next_token();
                ("unique".to_string(), None)
            }
            TokenType::NOT => {
                self.next_token();
                self.expect(TokenType::NULL)?;
                ("exists".to_string(), None)
            }
            // CIP-62: the entity keyword (NODE / REL / RELATIONSHIP) before
            // UNIQUE/KEY is optional and purely documentary -- the real
            // entity comes from the FOR-clause target, already captured in
            // `entity_type` above -- so it's just consumed and discarded
            // here. `IS KEY` with no entity keyword at all is also
            // accepted (bare KEY, matched below).
            TokenType::NODE | TokenType::REL => {
                self.next_token(); // consume NODE/REL
                self.parse_constraint_verb()?
            }
            TokenType::Identifier if tok.literal.eq_ignore_ascii_case("relationship") => {
                self.next_token(); // consume RELATIONSHIP
                self.parse_constraint_verb()?
            }
            TokenType::Identifier if tok.literal.eq_ignore_ascii_case("key") => {
                self.next_token();
                ("key".to_string(), None)
            }
            TokenType::DoubleColon => {
                // REQUIRE (n.prop) IS :: <value type>  (CIP-85 property
                // type constraint, reusing the CIP-86 <value type> parser).
                self.next_token();
                let te = self.parse_value_type()?;
                ("propertyType".to_string(), Some(te))
            }
            _ if self.peek_is_typed_keyword() => {
                // REQUIRE (n.prop) IS TYPED <value type>
                self.next_token();
                let te = self.parse_value_type()?;
                ("propertyType".to_string(), Some(te))
            }
            _ => {
                return Err(self.error_at_token(
                    &tok,
                    format!(
                        "expected UNIQUE, NOT NULL, KEY, NODE/REL[ATIONSHIP] UNIQUE, \
                         NODE/REL[ATIONSHIP] KEY, ::, or TYPED, got {:?}",
                        tok.literal
                    ),
                ));
            }
        };

        let options = if self.peek_type() == TokenType::OPTIONS {
            self.next_token();
            let pairs = self.parse_map_literal()?;
            pairs.into_iter().map(|p| (p.key, p.value)).collect()
        } else {
            Vec::new()
        };

        Ok(CreateConstraintClause {
            name,
            entity_type,
            label,
            variable,
            properties,
            constraint_type,
            if_not_exists,
            property_type,
            options,
            enforced_label: None,
        })
    }

    /// The `UNIQUE`/`KEY` verb following an optional `NODE`/`REL`/
    /// `RELATIONSHIP` entity keyword in `REQUIRE (...) IS [entity] <verb>`
    /// (CIP-62). The entity keyword itself was already consumed by the
    /// caller and discarded -- see the comment at the call site.
    pub(super) fn parse_constraint_verb(&mut self) -> ParseResult<(String, Option<TypeExpr>)> {
        let tok = self.next_token();
        if tok.ty == TokenType::UNIQUE {
            Ok(("unique".to_string(), None))
        } else if tok.literal.eq_ignore_ascii_case("key") {
            Ok(("key".to_string(), None))
        } else {
            Err(self.error_at_token(
                &tok,
                format!("expected UNIQUE or KEY, got {:?}", tok.literal),
            ))
        }
    }

    /// `ALTER CURRENT GRAPH TYPE (SET|ADD|ALTER|DROP) '{' <graph type
    /// specification body> '}'` (CIP-176/182). `SET`/`DROP` are reserved
    /// keywords (`TokenType::SET`/`TokenType::DROP`) everywhere else, so
    /// they're matched directly; `ADD`/`ALTER` are not reserved and are
    /// recognized by spelling like `SHOW`/`USE` elsewhere in this parser.
    pub(super) fn parse_alter_current_graph_type(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume ALTER
        self.expect_keyword_ident("CURRENT")?;
        self.expect_keyword_ident("GRAPH")?;
        self.expect_keyword_ident("TYPE")?;
        let verb = if self.peek_type() == TokenType::SET {
            self.next_token();
            // ADR-0211: `ALTER CURRENT GRAPH TYPE SET GRAPH TYPE <name>`
            // promotes a `CREATE GRAPH TYPE`-declared named type to current,
            // instead of taking a `{ <body> }`.
            if self.peek_is_ident("graph") && self.peek_at_is_ident(1, "type") {
                self.next_token(); // GRAPH
                self.next_token(); // TYPE
                let name = self.parse_bare_name("identifier")?;
                return Ok(Clause::AlterCurrentGraphType(AlterCurrentGraphTypeClause {
                    verb: AlterGraphTypeVerb::Set,
                    target: AlterCurrentGraphTypeTarget::Named(name),
                }));
            }
            AlterGraphTypeVerb::Set
        } else if self.peek_type() == TokenType::DROP {
            self.next_token();
            AlterGraphTypeVerb::Drop
        } else if self.peek_is_ident("add") {
            self.next_token();
            AlterGraphTypeVerb::Add
        } else if self.peek_is_ident("alter") {
            self.next_token();
            AlterGraphTypeVerb::Alter
        } else {
            let tok = self.peek().clone();
            return Err(self.error_at_token(
                &tok,
                format!("expected SET, ADD, ALTER, or DROP, got {:?}", tok.literal),
            ));
        };
        // CIP-182: only a `DROP` body's grammar allows the bare `CONSTRAINT
        // <name>` (no `FOR ...`) drop-by-name element, and a `DROP` body
        // drops a constraint by name only.
        let (open, body) = self.parse_graph_type_spec_body()?;
        if verb == AlterGraphTypeVerb::Drop {
            self.reject_undesignated_constraint_drops(&open, &body)?;
        } else {
            self.reject_bare_constraint_drops(&open, &body)?;
        }
        Ok(Clause::AlterCurrentGraphType(AlterCurrentGraphTypeClause {
            verb,
            target: AlterCurrentGraphTypeTarget::Body(body),
        }))
    }

    /// `CREATE GRAPH TYPE <name> [AS] '{' <graph type specification body>
    /// '}'` (CIP-178 grammar; the optional `AS` is CIP-180's spelling of
    /// the same body-introducing form). Not a reserved keyword sequence --
    /// `parse_create`'s dispatch recognizes `GRAPH TYPE` by spelling before
    /// calling this.
    pub(super) fn parse_create_graph_type(&mut self) -> ParseResult<Clause> {
        self.expect_keyword_ident("GRAPH")?;
        self.expect_keyword_ident("TYPE")?;
        let name = self.parse_bare_name("identifier")?;
        let via_as = self.peek_type() == TokenType::AS;
        if via_as {
            self.next_token();
        }
        let (open, body) = self.parse_graph_type_spec_body()?;
        self.reject_bare_constraint_drops(&open, &body)?;
        Ok(Clause::CreateGraphType(CreateGraphTypeClause {
            name,
            body,
            via_as,
        }))
    }

    /// `'{' [ <graph type spec element> (',' <graph type spec element>)*
    /// ] '}'` (CIP-178 `<graph type specification body>`). Grammar-only:
    /// see `GraphTypeBody`'s doc comment for why the CIP-178 semantic
    /// rules live elsewhere. Returns the consumed opening `{` with the body,
    /// the token `reject_bare_constraint_drops` and
    /// `reject_undesignated_constraint_drops` errors point at.
    pub(super) fn parse_graph_type_spec_body(&mut self) -> ParseResult<(Token, GraphTypeBody)> {
        let open = self.expect(TokenType::LeftBrace)?;
        let mut body = GraphTypeBody::default();
        if self.peek_type() != TokenType::RightBrace {
            loop {
                self.parse_graph_type_element(&mut body)?;
                if self.peek_type() == TokenType::Comma {
                    self.next_token();
                    continue;
                }
                break;
            }
        }
        self.expect(TokenType::RightBrace)?;
        Ok((open, body))
    }

    /// Rejects a parsed `body` holding a CIP-182 bare `CONSTRAINT <name>`
    /// drop-by-name element: only an `ALTER CURRENT GRAPH TYPE DROP` body's
    /// grammar has one, every other body's `CONSTRAINT` needs its `FOR ...`
    /// specification. The error points at `open`, the body's `{`.
    pub(super) fn reject_bare_constraint_drops(
        &self,
        open: &Token,
        body: &GraphTypeBody,
    ) -> ParseResult<()> {
        match body.drop_constraint_names.first() {
            None => Ok(()),
            Some(name) => Err(self.error_at_token(
                open,
                format!(
                    "constraint {name:?} has no FOR specification; only ALTER CURRENT GRAPH TYPE DROP names a constraint without one"
                ),
            )),
        }
    }

    /// Rejects an `ALTER CURRENT GRAPH TYPE DROP` `body` holding an
    /// undesignated (unnamed) `CONSTRAINT FOR ...` specification: such a
    /// constraint can only be dropped by name (CIP-183 drop [12]). A
    /// standalone specification's reference always has an alias (Rule 2,
    /// `parse_graph_type_constraint_spec`), which tells it apart from the
    /// unnamed constraints a node/edge type specification owns
    /// (`parse_graph_type_owned_constraints`). The error points at `open`,
    /// the body's `{`.
    pub(super) fn reject_undesignated_constraint_drops(
        &self,
        open: &Token,
        body: &GraphTypeBody,
    ) -> ParseResult<()> {
        let undesignated = body.constraints.iter().any(|c| {
            c.name.is_none() && !matches!(c.target, GraphTypeElementRef::InSitu { alias: None, .. })
        });
        if undesignated {
            return Err(self.error_at_token(
                open,
                "undesignated constraints cannot be dropped by specification",
            ));
        }
        Ok(())
    }

    /// One comma-separated top-level element of a `<graph type
    /// specification body>`: a `CONSTRAINT` specification, or a node type
    /// specification `(...)` -- the source endpoint reference of an edge
    /// type specification when a `-` follows it. Either type specification
    /// may end in an inline `REQUIRE ...` constraint list (see
    /// `parse_graph_type_owned_constraints`).
    pub(super) fn parse_graph_type_element(&mut self, body: &mut GraphTypeBody) -> ParseResult<()> {
        if self.peek_type() == TokenType::CONSTRAINT {
            return self.parse_graph_type_constraint_element(body);
        }

        self.expect(TokenType::LeftParen)?;
        let (alias, label) = self.parse_graph_type_ref_inner()?;
        let implies = label.is_some() && self.consume_graph_type_implies();
        let mut implied_labels = Vec::new();
        if implies && matches!(self.peek_type(), TokenType::Colon | TokenType::IS) {
            self.next_token();
            implied_labels.push(self.parse_label_name()?);
            while self.peek_type() == TokenType::Ampersand {
                self.next_token();
                // CIP-180 F1[5]: each subsequent `&`-joined label may
                // repeat its own `:`/IS introducer (`:Zebra&:Mango`), or
                // omit it (CIP-178 NTS[4]: `:Happy&Employed`) -- both
                // spellings denote the same implied label set.
                if matches!(self.peek_type(), TokenType::Colon | TokenType::IS) {
                    self.next_token();
                }
                implied_labels.push(self.parse_label_name()?);
            }
        }
        let properties = self.parse_graph_type_property_block(implies, "Node")?;
        self.expect(TokenType::RightParen)?;

        if self.peek_type() == TokenType::Dash {
            // Edge type specification: the just-parsed `(...)` was its
            // source endpoint reference (`implies` is significant here --
            // Rule 1.2.3/REF[6] -- so it's threaded through rather than
            // discarded).
            let source = Self::build_graph_type_ref_implies(alias, label, implies);
            return self.parse_graph_type_edge_tail(body, source);
        }
        let node = GraphTypeNodeTypeSpec {
            alias,
            identifying_label: label.unwrap_or_default(),
            implied_labels,
            properties,
        };
        self.parse_graph_type_node_tail(body, node)
    }

    /// A `CONSTRAINT` element of a `<graph type specification body>`,
    /// starting at `CONSTRAINT`: a CIP-182 bare drop-by-name, else a full
    /// constraint specification.
    pub(super) fn parse_graph_type_constraint_element(
        &mut self,
        body: &mut GraphTypeBody,
    ) -> ParseResult<()> {
        // CIP-182 `<constraint drop specification>` ::= `CONSTRAINT
        // <name>` -- a bare name with no `FOR ...`, so the element ends
        // right after the name. It is read in every body; the callers of
        // every verb but `DROP` reject it (`reject_bare_constraint_drops`).
        // Ending the lookahead at `,`/`}` rather than "anything but `FOR`"
        // keeps an unnamed `CONSTRAINT FOR ...` specification from reading
        // `FOR` as the constraint name (a `DROP` caller rejects that
        // specification instead -- `reject_undesignated_constraint_drops`).
        if self.peek_at(1).ty.is_ident_like()
            && matches!(self.peek_at(2).ty, TokenType::Comma | TokenType::RightBrace)
        {
            self.next_token(); // consume CONSTRAINT
            let name = self.parse_bare_name("identifier")?;
            body.drop_constraint_names.push(name);
            return Ok(());
        }
        let c = self.parse_graph_type_constraint_spec()?;
        body.constraints.push(c);
        Ok(())
    }

    /// The rest of a node type specification once its `(...)` is parsed:
    /// the trailing inline `REQUIRE ...` list. Records `node` and every
    /// constraint it owns in `body`.
    pub(super) fn parse_graph_type_node_tail(
        &mut self,
        body: &mut GraphTypeBody,
        node: GraphTypeNodeTypeSpec,
    ) -> ParseResult<()> {
        let constraints = self.parse_graph_type_owned_constraints(
            &node.properties,
            &node.identifying_label,
            false,
            node.alias.as_deref(),
        )?;
        body.constraints.extend(constraints);
        body.node_types.push(node);
        Ok(())
    }

    /// The rest of an edge type specification once its `source` endpoint
    /// reference is parsed: `'-' '[' <edge type head> ']' '->' <destination
    /// endpoint>` and the trailing inline `REQUIRE ...` list. Records the
    /// edge type and every constraint it owns in `body`.
    pub(super) fn parse_graph_type_edge_tail(
        &mut self,
        body: &mut GraphTypeBody,
        source: GraphTypeElementRef,
    ) -> ParseResult<()> {
        self.next_token(); // consume '-'
        self.expect(TokenType::LeftBracket)?;
        let (alias, label) = self.parse_graph_type_ref_inner()?;
        // The `=>` marker only counts — and is only consumed — behind a
        // label; `[: => {...}]` has no identifying label to denote.
        let implies = label.is_some() && self.consume_graph_type_implies();
        let identifying_label = label.unwrap_or_default();
        let properties = self.parse_graph_type_property_block(implies, "Edge")?;
        self.expect(TokenType::RightBracket)?;
        self.expect(TokenType::ArrowRight)?;
        self.expect(TokenType::LeftParen)?;
        let (t_alias, t_label) = self.parse_graph_type_ref_inner()?;
        let t_implies = t_label.is_some() && self.consume_graph_type_implies();
        self.expect(TokenType::RightParen)?;
        let target = Self::build_graph_type_ref_implies(t_alias, t_label, t_implies);

        let constraints = self.parse_graph_type_owned_constraints(
            &properties,
            &identifying_label,
            true,
            alias.as_deref(),
        )?;
        body.constraints.extend(constraints);
        body.edge_types.push(GraphTypeEdgeTypeSpec {
            alias,
            identifying_label,
            properties,
            source,
            target,
        });
        Ok(())
    }

    /// The constraints a node/edge type specification owns, all targeting
    /// its own identifying label (CIP-178 Rules 10-12): one per property
    /// carrying an inline constraint, then one per trailing inline
    /// `REQUIRE ...`. `owner_alias` is the specification's own alias.
    pub(super) fn parse_graph_type_owned_constraints(
        &mut self,
        properties: &[GraphTypePropertyType],
        identifying_label: &str,
        is_edge: bool,
        owner_alias: Option<&str>,
    ) -> ParseResult<Vec<GraphTypeConstraintSpec>> {
        let mut constraints =
            Self::take_property_inline_constraints(properties, identifying_label, is_edge);
        let own_ref = GraphTypeElementRef::InSitu {
            alias: None,
            label: identifying_label.to_string(),
            implies: true,
        };
        while self.peek_type() == TokenType::REQUIRE {
            constraints.push(self.parse_graph_type_inline_require(
                own_ref.clone(),
                is_edge,
                owner_alias,
            )?);
        }
        Ok(constraints)
    }

    /// `[alias] [(':'|IS) <label>]` -- the alias/label pair shared by every
    /// `<node type reference>`/`<edge type reference>`/`<node type
    /// specification>`/`<edge type specification>` head. `IS` is a
    /// reserved token (never itself a legal alias in this grammar), so
    /// checking for it before the ident-like alias check disambiguates
    /// `(p IS Person => ...)` (alias `p`) from a hypothetical alias-less
    /// `IS`-introduced label. The label may be a keyword (ADR-0169); the
    /// alias may not.
    pub(super) fn parse_graph_type_ref_inner(
        &mut self,
    ) -> ParseResult<(Option<String>, Option<String>)> {
        let mut alias = None;
        if self.peek_type().is_ident_like() && self.peek_type() != TokenType::IS {
            alias = Some(self.parse_bare_name("identifier")?);
        }
        let mut label = None;
        if self.peek_type() == TokenType::Colon || self.peek_type() == TokenType::IS {
            self.next_token();
            label = Some(self.parse_label_name()?);
        }
        Ok((alias, label))
    }

    /// Consumes a `=>` or `IMPLIES` token if present; returns whether one
    /// was found.
    pub(super) fn consume_graph_type_implies(&mut self) -> bool {
        if self.peek_type() == TokenType::FatArrow || self.peek_is_ident("implies") {
            self.next_token();
            true
        } else {
            false
        }
    }

    /// `'{' [ <property type> (',' <property type>)* ] '}'` (CIP-178
    /// `<property types specification>`). `<property type> ::= <name>
    /// [('::'|TYPED)] <value type> [DEFAULT ON COMMIT <value expression>]
    /// [<inline constraint>]` (CIP-262 adds the `DEFAULT ON COMMIT` clause;
    /// grammar-only here, the shape and the preview gate live in the
    /// semantic layer and statement admission).
    pub(super) fn parse_graph_type_property_types(
        &mut self,
    ) -> ParseResult<Vec<GraphTypePropertyType>> {
        self.expect(TokenType::LeftBrace)?;
        let mut props = Vec::new();
        if self.peek_type() != TokenType::RightBrace {
            loop {
                let name = self.parse_bare_name("identifier")?;
                // ADR-0211: `<name> IS [::] <value type>` is the GQL
                // spelling of `<name> :: <value type>`. The optional `IS`
                // introducer is consumed once, guarded by the inline-
                // constraint lookahead (`IS KEY`/`IS UNIQUE` belong to the
                // constraint parsed below); the `::`/TYPED introducer then
                // has a single branch.
                if self.peek_type() == TokenType::IS
                    && !self.peek_starts_graph_type_inline_constraint()
                {
                    self.next_token(); // consume IS
                }
                if self.peek_type() == TokenType::DoubleColon || self.peek_is_typed_keyword() {
                    self.next_token();
                }
                let value_type = self.parse_value_type()?;
                let default_value = self.parse_graph_type_property_default()?;
                let inline_constraint = if self.peek_type() == TokenType::IS {
                    Some(self.parse_graph_type_inline_property_constraint()?)
                } else {
                    None
                };
                props.push(GraphTypePropertyType {
                    name,
                    value_type,
                    default_value,
                    inline_constraint,
                });
                if self.peek_type() != TokenType::Comma {
                    break;
                }
                self.next_token();
            }
        }
        self.expect(TokenType::RightBrace)?;
        Ok(props)
    }

    /// The optional `DEFAULT ON COMMIT <value expression>` clause of a
    /// graph-type `<property type>` (CIP-262): `DEFAULT` and `COMMIT` are
    /// non-reserved words, so both match as bare identifiers; `ON` is the
    /// keyword token. The value is the ordinary expression parser's -- its
    /// existing nesting-depth limit (ADR-0079) covers the clause, so no
    /// new recursion family is introduced. Shape (literal or type
    /// constructor, never `null`) is the semantic layer's to enforce.
    fn parse_graph_type_property_default(&mut self) -> ParseResult<Option<Expr>> {
        if !self.peek_is_ident("default") {
            return Ok(None);
        }
        self.next_token(); // consume DEFAULT
        self.expect(TokenType::ON)?;
        if !self.peek_is_ident("commit") {
            return Err(self.error_at_token(
                self.peek(),
                "expected COMMIT after DEFAULT ON in a property type".to_string(),
            ));
        }
        self.next_token(); // consume COMMIT
        Ok(Some(self.parse_expression()?))
    }

    /// The `{ <property types> }` block of a node/edge type specification.
    /// The `=>`/`IMPLIES` marker (the `implies` flag, consumed by the
    /// caller) is what makes the preceding label the *explicitly denoted
    /// identifying label*, so a bare `{` without it is a compile-time
    /// syntax error (CIP-178/181/183; current Neo4j rejects the bare
    /// `(:Label {…})` form); no block at all is an empty property list.
    /// One policy for both the node and the edge type-spec parsers.
    fn parse_graph_type_property_block(
        &mut self,
        implies: bool,
        entity: &str,
    ) -> ParseResult<Vec<GraphTypePropertyType>> {
        if self.peek_type() != TokenType::LeftBrace {
            return Ok(Vec::new());
        }
        if !implies {
            return Err(self.error_at_token(
                self.peek(),
                format!(
                    "{entity} type does not have an explicitly denoted identifying label \
                     (use `=>` or `IMPLIES` after the label before the property block)"
                ),
            ));
        }
        self.parse_graph_type_property_types()
    }

    /// A KEY/UNIQUE token mapped to its inline-constraint verb (UNIQUE is a
    /// keyword token, KEY a non-reserved identifier) — the single definition
    /// of the verb token set both the inline-constraint lookahead and its
    /// parser use.
    fn inline_verb_from_token(tok: &Token) -> Option<GraphTypeInlineVerb> {
        match tok.ty {
            TokenType::UNIQUE => Some(GraphTypeInlineVerb::Unique),
            TokenType::Identifier if Self::token_is_ident(tok, "key") => {
                Some(GraphTypeInlineVerb::Key)
            }
            _ => None,
        }
    }

    /// An entity qualifier token (NODE / REL / identifier `relationship`)
    /// mapped to its qualifier — the single definition of the qualifier
    /// token set both the lookahead and the parser use.
    fn entity_qualifier_from_token(tok: &Token) -> Option<GraphTypeEntityQualifier> {
        match tok.ty {
            TokenType::NODE => Some(GraphTypeEntityQualifier::Node),
            TokenType::REL => Some(GraphTypeEntityQualifier::Rel),
            TokenType::Identifier if Self::token_is_ident(tok, "relationship") => {
                Some(GraphTypeEntityQualifier::Rel)
            }
            _ => None,
        }
    }

    /// Whether the tokens after a property declaration's `IS` (not yet
    /// consumed) start an inline constraint -- `IS KEY`, `IS UNIQUE`, or
    /// `IS NODE/REL/RELATIONSHIP KEY|UNIQUE` -- rather than an ADR-0211
    /// `IS [::] <value type>` type declaration.
    pub(super) fn peek_starts_graph_type_inline_constraint(&self) -> bool {
        if Self::inline_verb_from_token(self.peek_at(1)).is_some() {
            return true;
        }
        Self::entity_qualifier_from_token(self.peek_at(1)).is_some()
            && Self::inline_verb_from_token(self.peek_at(2)).is_some()
    }

    /// `IS [NODE|REL|RELATIONSHIP] (KEY|UNIQUE)` -- an inline property-type
    /// constraint (CIP-178 PT[8]-[12]).
    pub(super) fn parse_graph_type_inline_property_constraint(
        &mut self,
    ) -> ParseResult<GraphTypeInlineConstraint> {
        self.expect(TokenType::IS)?;
        let qualifier = Self::entity_qualifier_from_token(self.peek());
        if qualifier.is_some() {
            self.next_token();
        }
        let tok = self.next_token();
        match Self::inline_verb_from_token(&tok) {
            Some(verb) => Ok(GraphTypeInlineConstraint { verb, qualifier }),
            None => Err(self.error_at_token(
                &tok,
                format!("expected KEY or UNIQUE, got {:?}", tok.literal),
            )),
        }
    }

    /// Rewrites every property in `properties` carrying an inline
    /// constraint into a standalone `GraphTypeConstraintSpec` targeting the
    /// owning node/edge type's own identifying label (CIP-178 Rules 10,
    /// 12).
    pub(super) fn take_property_inline_constraints(
        properties: &[GraphTypePropertyType],
        identifying_label: &str,
        is_edge: bool,
    ) -> Vec<GraphTypeConstraintSpec> {
        properties
            .iter()
            .filter_map(|p| {
                p.inline_constraint.map(|ic| GraphTypeConstraintSpec {
                    name: None,
                    target: GraphTypeElementRef::InSitu {
                        alias: None,
                        label: identifying_label.to_string(),
                        implies: true,
                    },
                    is_edge,
                    properties: vec![p.name.clone()],
                    kind: match ic.verb {
                        GraphTypeInlineVerb::Key => GraphTypeConstraintVerb::Key,
                        GraphTypeInlineVerb::Unique => GraphTypeConstraintVerb::Unique,
                    },
                    property_type: None,
                    options: Vec::new(),
                })
            })
            .collect()
    }

    /// A node/edge type specification's trailing inline `REQUIRE
    /// <property name spec> <verb> [OPTIONS <map>]` (no `CONSTRAINT`
    /// keyword, no name -- CIP-178 Rules 11/12). `target` is always the
    /// owning element's own identifying label, in-situ-identified form (see
    /// `parse_graph_type_owned_constraints`). `owner_alias` is the owning
    /// node/edge type specification's own declared element type alias, if
    /// any -- every alias used in the REQUIRE list must match it exactly.
    pub(super) fn parse_graph_type_inline_require(
        &mut self,
        target: GraphTypeElementRef,
        is_edge: bool,
        owner_alias: Option<&str>,
    ) -> ParseResult<GraphTypeConstraintSpec> {
        self.next_token(); // consume REQUIRE
        let properties =
            self.parse_graph_type_property_name_spec_for_owner(owner_alias, is_edge)?;
        let (kind, property_type, options) = self.parse_graph_type_constraint_verb()?;
        // CIP-178 grammar: `<node/edge type inline constraint require>` is
        // only ever a KEY or UNIQUE constraint require -- existence
        // (`IS NOT NULL`) and property type (`:: T`) constraints are not
        // grammatically reachable when attached directly to a node/edge
        // type specification's trailing `REQUIRE` (only via the standalone
        // `CONSTRAINT FOR` form or an inline property constraint).
        if matches!(
            kind,
            GraphTypeConstraintVerb::Exists | GraphTypeConstraintVerb::PropertyType
        ) {
            let kind_word = if is_edge { "edge" } else { "node" };
            let verb_word = if matches!(kind, GraphTypeConstraintVerb::Exists) {
                "existence"
            } else {
                "type"
            };
            return Err(self.error(format!(
                "Unsupported syntax — {kind_word} type property {verb_word} constraints cannot be specified inline of a {kind_word} type"
            )));
        }
        Ok(GraphTypeConstraintSpec {
            name: None,
            target,
            is_edge,
            properties,
            kind,
            property_type,
            options,
        })
    }

    /// `CONSTRAINT [<name>] FOR <node/edge type reference> REQUIRE
    /// <property name spec> <verb> [OPTIONS <map>]` (CIP-178 `<constraint
    /// specification>`). Enforces the purely-local rules that don't need
    /// cross-element context: Rule 1 (the any type is forbidden), Rule 2
    /// (a labelled reference requires an element type alias), Rule 13
    /// (the property name spec's own alias must match the reference's),
    /// Rule 16-of-CS (no duplicate property name in the spec), and the
    /// "existence/property-type constraints are not allowed on a
    /// structurally-identified reference" rule (Rules 22/23). Rules needing
    /// the *whole* body (5-9, 14, 15/16, reference resolution) are checked
    /// by the engine's graph-type validator.
    pub(super) fn parse_graph_type_constraint_spec(
        &mut self,
    ) -> ParseResult<GraphTypeConstraintSpec> {
        self.expect(TokenType::CONSTRAINT)?;
        let name = if self.peek_type() == TokenType::FOR {
            None
        } else {
            Some(self.parse_bare_name("identifier")?)
        };
        self.expect(TokenType::FOR)?;
        let (is_edge, target) = self.parse_graph_type_constraint_target()?;

        let expected_alias = match &target {
            GraphTypeElementRef::Any => {
                return Err(self.error("constraint may not reference the any node type"));
            }
            GraphTypeElementRef::AliasRef(a) => Some(a.clone()),
            GraphTypeElementRef::InSitu { alias: Some(a), .. } => Some(a.clone()),
            GraphTypeElementRef::InSitu { alias: None, .. } => {
                return Err(self.error(if is_edge {
                    "edge type constraint reference requires an element type alias"
                } else {
                    "node type constraint reference requires an element type alias"
                }));
            }
        };

        self.expect(TokenType::REQUIRE)?;
        let properties = self.parse_graph_type_property_name_spec(expected_alias.as_deref())?;
        let (kind, property_type, options) = self.parse_graph_type_constraint_verb()?;

        // Rules 22/23: existence/property-type constraints are not allowed
        // on a structurally-identified reference (an alias reference always
        // resolves to a declared type; an in-situ reference with `=>` must
        // resolve to one).
        let locally_identified = matches!(target, GraphTypeElementRef::AliasRef(_))
            || matches!(target, GraphTypeElementRef::InSitu { implies: true, .. });
        if locally_identified
            && matches!(
                kind,
                GraphTypeConstraintVerb::Exists | GraphTypeConstraintVerb::PropertyType
            )
        {
            return Err(self.error(
                "existence/property type constraints are not allowed on a designated node type",
            ));
        }

        Ok(GraphTypeConstraintSpec {
            name,
            target,
            is_edge,
            properties,
            kind,
            property_type,
            options,
        })
    }

    /// Parses a `<constraint specification>`'s `FOR` target: `'(' ...  ')'`
    /// for a node type reference, or `'(' ')' '-' '[' ... ']' '->' '(' ')'`
    /// for an edge type reference (both node-parens empty). Returns
    /// `(is_edge, reference)`.
    pub(super) fn parse_graph_type_constraint_target(
        &mut self,
    ) -> ParseResult<(bool, GraphTypeElementRef)> {
        self.expect(TokenType::LeftParen)?;
        if self.peek_type() == TokenType::RightParen && self.peek_at(1).ty == TokenType::Dash {
            self.next_token(); // consume ')'
            self.expect(TokenType::Dash)?;
            self.expect(TokenType::LeftBracket)?;
            let (alias, label) = self.parse_graph_type_ref_inner()?;
            let implies = if label.is_some() {
                self.consume_graph_type_implies()
            } else {
                false
            };
            self.expect(TokenType::RightBracket)?;
            self.expect(TokenType::ArrowRight)?;
            self.expect(TokenType::LeftParen)?;
            self.expect(TokenType::RightParen)?;
            return Ok((
                true,
                Self::build_graph_type_ref_implies(alias, label, implies),
            ));
        }
        let (alias, label) = self.parse_graph_type_ref_inner()?;
        let implies = if label.is_some() {
            self.consume_graph_type_implies()
        } else {
            false
        };
        self.expect(TokenType::RightParen)?;
        Ok((
            false,
            Self::build_graph_type_ref_implies(alias, label, implies),
        ))
    }

    /// Builds a `GraphTypeElementRef` from a parsed alias/label/implies
    /// triple -- shared by node/edge type spec endpoints and constraint
    /// `FOR`-targets, where `=>` presence is semantically meaningful
    /// (Rules 1.2.3, 22/23). CIP-178 Rules 3/4 (an edge type's own in-situ
    /// endpoint reference may not carry an alias) and the constraint
    /// `FOR`-target rules (Rule 1/2: the any type is forbidden, a labelled
    /// reference requires an alias) are cross-referential/context-dependent
    /// and are checked by the engine's graph-type validator, not here.
    pub(super) fn build_graph_type_ref_implies(
        alias: Option<String>,
        label: Option<String>,
        implies: bool,
    ) -> GraphTypeElementRef {
        match (alias, label) {
            (None, None) => GraphTypeElementRef::Any,
            (Some(a), None) => GraphTypeElementRef::AliasRef(a),
            (alias, Some(l)) => GraphTypeElementRef::InSitu {
                alias,
                label: l,
                implies,
            },
        }
    }

    /// `<alias>.<prop>` / `'(' <alias>.<prop> (',' <alias>.<prop>)* ')'`
    /// (CIP-178 `<property name specification>`), returning the raw
    /// `(alias, property)` pairs in source order without any alias
    /// validation -- shared by `parse_graph_type_property_name_spec` (the
    /// standalone `CONSTRAINT FOR` form, which already knows a single
    /// `expected_alias`) and `parse_graph_type_inline_require`'s owner-alias
    /// check (which needs to report every *distinct* offending alias, not
    /// just detect a mismatch).
    pub(super) fn parse_graph_type_property_name_entries(
        &mut self,
    ) -> ParseResult<Vec<(String, String)>> {
        let mut entries: Vec<(String, String)> = Vec::new();
        if self.peek_type() == TokenType::LeftParen {
            self.next_token();
            loop {
                let alias = self.parse_bare_name("identifier")?;
                self.expect(TokenType::Dot)?;
                let prop = self.parse_bare_name("identifier")?;
                entries.push((alias, prop));
                if self.peek_type() != TokenType::Comma {
                    break;
                }
                self.next_token();
            }
            self.expect(TokenType::RightParen)?;
        } else {
            let alias = self.parse_bare_name("identifier")?;
            self.expect(TokenType::Dot)?;
            let prop = self.parse_bare_name("identifier")?;
            entries.push((alias, prop));
        }
        Ok(entries)
    }

    /// `<alias>.<prop>` / `'(' <alias>.<prop> (',' <alias>.<prop>)* ')'`
    /// (CIP-178 `<property name specification>`). When `expected_alias` is
    /// given, every entry's alias must match it (Rule 13); duplicate
    /// property names within the spec are always rejected (Rule 16 of the
    /// constraint-specification section).
    pub(super) fn parse_graph_type_property_name_spec(
        &mut self,
        expected_alias: Option<&str>,
    ) -> ParseResult<Vec<String>> {
        let entries = self.parse_graph_type_property_name_entries()?;
        if let Some(expected) = expected_alias {
            if entries.iter().any(|(a, _)| a != expected) {
                return Err(self.error("property name alias must match the reference alias"));
            }
        }
        let mut props = Vec::new();
        for (_, p) in entries {
            if props.contains(&p) {
                return Err(self.error("duplicate property name in property names specification"));
            }
            props.push(p);
        }
        Ok(props)
    }

    /// The `<property names specification>` of a node/edge type
    /// specification's trailing inline `REQUIRE` list (CIP-178 `<node/edge
    /// type inline constraint require>`). Unlike the standalone `CONSTRAINT
    /// FOR` form (`parse_graph_type_property_name_spec`, which always has a
    /// declared reference alias to compare against), the owning node/edge
    /// type specification may not have declared an alias at all
    /// (`owner_alias: None`) -- in which case *any* alias used in the
    /// REQUIRE list is undefined, not just a non-matching one.
    pub(super) fn parse_graph_type_property_name_spec_for_owner(
        &mut self,
        owner_alias: Option<&str>,
        is_edge: bool,
    ) -> ParseResult<Vec<String>> {
        let entries = self.parse_graph_type_property_name_entries()?;
        let mut undefined: Vec<String> = Vec::new();
        for (a, _) in &entries {
            if Some(a.as_str()) != owner_alias && !undefined.contains(a) {
                undefined.push(a.clone());
            }
        }
        if !undefined.is_empty() {
            let kind = if is_edge { "Edge" } else { "Node" };
            let (noun, list) = if undefined.len() == 1 {
                ("alias", undefined[0].clone())
            } else {
                ("aliases", undefined.join(" and "))
            };
            return Err(self.error(format!(
                "{kind} type constraint uses undefined type {noun} {list}"
            )));
        }
        let mut props = Vec::new();
        for (_, p) in entries {
            if props.contains(&p) {
                return Err(self.error("duplicate property name in property names specification"));
            }
            props.push(p);
        }
        Ok(props)
    }

    /// The constraint verb tail of a `<constraint specification>` (CIP-178
    /// CS grammar), starting right after the `<property name spec>`:
    /// either a bare `:: <value type>` or `IS (KEY | [NODE|REL|
    /// RELATIONSHIP] KEY | UNIQUE | [NODE|REL|RELATIONSHIP] UNIQUE | NOT
    /// NULL | (::|TYPED) <value type>)`, followed by an optional `OPTIONS
    /// <map>`. Mirrors (but does not share code with) the top-level
    /// `CREATE CONSTRAINT` verb grammar in `parse_create_constraint` --
    /// unlike that grammar, the entity qualifier here is never restricted
    /// by context (CIP-178 doesn't test a mismatch).
    pub(super) fn parse_graph_type_constraint_verb(
        &mut self,
    ) -> ParseResult<GraphTypeConstraintVerbResult> {
        if self.peek_type() == TokenType::DoubleColon {
            self.next_token();
            let te = self.parse_value_type()?;
            let options = self.parse_graph_type_options_map()?;
            return Ok((GraphTypeConstraintVerb::PropertyType, Some(te), options));
        }
        self.expect(TokenType::IS)?;
        let tok = self.peek().clone();
        let (verb, property_type) = match tok.ty {
            TokenType::UNIQUE => {
                self.next_token();
                (GraphTypeConstraintVerb::Unique, None)
            }
            TokenType::NOT => {
                self.next_token();
                self.expect(TokenType::NULL)?;
                (GraphTypeConstraintVerb::Exists, None)
            }
            TokenType::NODE | TokenType::REL => {
                self.next_token();
                self.parse_graph_type_constraint_kw_verb()?
            }
            TokenType::Identifier if tok.literal.eq_ignore_ascii_case("relationship") => {
                self.next_token();
                self.parse_graph_type_constraint_kw_verb()?
            }
            TokenType::Identifier if tok.literal.eq_ignore_ascii_case("key") => {
                self.next_token();
                (GraphTypeConstraintVerb::Key, None)
            }
            TokenType::DoubleColon => {
                self.next_token();
                let te = self.parse_value_type()?;
                (GraphTypeConstraintVerb::PropertyType, Some(te))
            }
            _ if self.peek_is_typed_keyword() => {
                self.next_token();
                let te = self.parse_value_type()?;
                (GraphTypeConstraintVerb::PropertyType, Some(te))
            }
            _ => {
                return Err(self.error_at_token(
                    &tok,
                    format!(
                        "expected UNIQUE, NOT NULL, KEY, ::, or TYPED, got {:?}",
                        tok.literal
                    ),
                ));
            }
        };
        let options = self.parse_graph_type_options_map()?;
        Ok((verb, property_type, options))
    }

    pub(super) fn parse_graph_type_constraint_kw_verb(
        &mut self,
    ) -> ParseResult<(GraphTypeConstraintVerb, Option<TypeExpr>)> {
        let tok = self.next_token();
        if tok.ty == TokenType::UNIQUE {
            Ok((GraphTypeConstraintVerb::Unique, None))
        } else if tok.literal.eq_ignore_ascii_case("key") {
            Ok((GraphTypeConstraintVerb::Key, None))
        } else {
            Err(self.error_at_token(
                &tok,
                format!("expected UNIQUE or KEY, got {:?}", tok.literal),
            ))
        }
    }

    pub(super) fn parse_graph_type_options_map(&mut self) -> ParseResult<Vec<(String, Expr)>> {
        if self.peek_type() == TokenType::OPTIONS {
            self.next_token();
            let pairs = self.parse_map_literal()?;
            Ok(pairs.into_iter().map(|p| (p.key, p.value)).collect())
        } else {
            Ok(Vec::new())
        }
    }

    /// `TERMINATE TRANSACTION[S] <id-list> [YIELD ...]` (CIP-68 grammar
    /// foundation). Not a `SHOW` spelling -- a separate top-level
    /// context-sensitive keyword, same recognition technique.
    pub(super) fn parse_terminate_transactions(&mut self) -> ParseResult<Clause> {
        self.next_token(); // consume TERMINATE

        // TRANSACTION has no reserved token of its own, so the
        // identifier-spelling check is the whole gate (same technique as
        // FUNCTION(S)/DEFINED).
        self.expect_kind_word(
            TokenType::Identifier,
            "transaction",
            Some("transactions"),
            "TRANSACTION(S) after TERMINATE",
        )?;

        let ids = self.parse_comma_separated_exprs()?;

        let yields = self.parse_show_yield_list()?;

        Ok(Clause::TerminateTransactions(TerminateTransactionsClause {
            ids,
            yields,
        }))
    }
}
