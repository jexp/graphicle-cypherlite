use super::*;

impl Parser {
    /// Reads a single identifier-like token (variable name, property
    /// name, ...) and returns its literal -- for contexts that need a bare
    /// name without going through the full pattern grammar (`USING`-hint
    /// targets, DDL property lists, the `ALTER CURRENT GRAPH TYPE` body).
    /// `what` names the expected name in the error (`expected <what>, got
    /// ...`). Label names read through `parse_label_name` instead
    /// (ADR-0169).
    pub(super) fn parse_bare_name(&mut self, what: &str) -> ParseResult<String> {
        let tok = self.next_token();
        if !tok.ty.is_ident_like() {
            return Err(
                self.error_at_token(&tok, format!("expected {what}, got {:?}", tok.literal))
            );
        }
        Ok(tok.literal)
    }

    /// Reads one symbolic-name token (`TokenType::is_schema_name()`: an
    /// `Identifier` or any keyword, ADR-0191) and returns its literal as
    /// written; anything else is `expected <what>, got ...`. The
    /// consume-or-error counterpart of `parse_bare_name` for the
    /// ADR-0191-widened slots: every symbolic-name reject read (SEARCH /
    /// SCORE AS / SET / REMOVE / FOREACH / CALL scope / REPORT STATUS AS /
    /// LET variables, property-name reads, map-projection items, reduce
    /// head variables, ...) routes through here so the check stays
    /// single-sourced.
    pub(super) fn parse_schema_name(&mut self, what: &str) -> ParseResult<String> {
        let tok = self.next_token();
        if !tok.ty.is_schema_name() {
            return Err(
                self.error_at_token(&tok, format!("expected {what}, got {:?}", tok.literal))
            );
        }
        Ok(tok.literal)
    }

    /// Reads the alias token after `AS` and returns its literal as
    /// written. ADR-0191: any keyword is a symbolic name after `AS` --
    /// nothing else has a meaning directly after it -- so every
    /// `AS <alias>` site (RETURN/WITH items, UNWIND, LOAD CSV, CALL/SHOW
    /// ... YIELD, REPORT STATUS AS) reads through the one symbolic-name
    /// read (`parse_schema_name`) and the check stays single-sourced.
    pub(super) fn parse_alias_after_as(&mut self) -> ParseResult<String> {
        self.parse_schema_name("alias after AS")
    }

    /// Reads one label or relationship-type name token and returns its
    /// literal as written. ADR-0169: any keyword is accepted
    /// (`TokenType::is_schema_name`), so `:Order` and `` :`Order` `` name
    /// the same label. The label counterpart of `parse_bare_name`, which
    /// stays on `is_ident_like()` for variables and other names.
    pub(super) fn parse_label_name(&mut self) -> ParseResult<String> {
        self.parse_schema_name("a label name")
    }

    /// Parses one label/type atom: a label name (a keyword included,
    /// ADR-0169), or a `$(<expr>)`/`$all(<expr>)`/`$any(<expr>)` dynamic
    /// label/type name, optionally quantified (CIP-249/156/221 in label
    /// expressions and colon chains, CIP-162 in SET/REMOVE label items). A
    /// dynamic atom is not one of CIP-14's legacy-vs-GQL "mixing" operator
    /// symbols (`&`, `|`, `!`, `%`) -- it combines freely with a legacy
    /// colon-conjunction elsewhere in the same clause (CIP-156
    /// `09-general-examples.feature` scenarios 5/6) -- so this records no
    /// label-mixing state.
    pub(super) fn parse_label_atom(&mut self) -> ParseResult<LabelExpr> {
        let Some(quantifier) = self.dynamic_label_quantifier() else {
            return Ok(LabelExpr::Label(self.parse_label_name()?));
        };
        self.next_token(); // consume the '$' parameter token (empty/"all"/"any")
        self.next_token(); // consume (
        let expr = self.parse_expression()?;
        self.expect(TokenType::RightParen)?;
        Ok(LabelExpr::Dynamic(DynamicLabelExpr {
            expr: Box::new(expr),
            quantifier,
        }))
    }

    /// First-token check of `parse_label_atom`: true if the current token
    /// starts a label atom -- a label name or a `$(`/`$all(`/`$any(`
    /// dynamic opener.
    pub(super) fn peek_starts_label_atom(&self) -> bool {
        self.peek_type().is_schema_name() || self.dynamic_label_quantifier().is_some()
    }

    /// If the current token is a `$(`/`$all(`/`$any(` dynamic label
    /// expression opener, returns its quantifier. `$all`/`$any` are matched
    /// case-insensitively per CIP-221's `<dynamic label expression
    /// quantifier> ::= ALL | ANY`.
    pub(super) fn dynamic_label_quantifier(&self) -> Option<DynamicLabelQuantifier> {
        self.dynamic_label_quantifier_at(0)
    }

    /// [`Parser::dynamic_label_quantifier`] for the token `offset` ahead:
    /// the dynamic label/type atom opener (`$(`/`$all(`/`$any(`) and the
    /// quantifier it spells, if any. One check shared by the atom reader
    /// (`parse_label_atom`), the label-starter lookahead
    /// (`peek_can_start_label_expr_at`) and the colon-chain detection
    /// (`parse_label_expr_body`).
    pub(super) fn dynamic_label_quantifier_at(
        &self,
        offset: usize,
    ) -> Option<DynamicLabelQuantifier> {
        if self.peek_at(offset).ty != TokenType::Parameter
            || self.peek_at(offset + 1).ty != TokenType::LeftParen
        {
            return None;
        }
        let literal = &self.peek_at(offset).literal;
        if literal.is_empty() {
            Some(DynamicLabelQuantifier::Default)
        } else if literal.eq_ignore_ascii_case("all") {
            Some(DynamicLabelQuantifier::All)
        } else if literal.eq_ignore_ascii_case("any") {
            Some(DynamicLabelQuantifier::Any)
        } else {
            None
        }
    }
}
