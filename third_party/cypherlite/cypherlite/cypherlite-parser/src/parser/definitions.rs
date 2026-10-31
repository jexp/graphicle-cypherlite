//! CIP-231 local callable definitions: the `DEFINE PROCEDURE …`/`DEFINE
//! FUNCTION …` block parsed at the head of a `<statement block>`.
//!
//! Grammar (CIP-231, gated behind the ADR-0371 `local-callables` preview
//! feature at the front end; the parser itself is unconditional, the same
//! shape as the CIP-278 `COMBINE` gate):
//!
//! ```text
//! <definition block>       ::= <definition statement>...
//! <definition statement>   ::= DEFINE <procedure definition>
//!                             | DEFINE <function definition>
//! <procedure definition>   ::= PROCEDURE <qualified name> <callable input
//!                              signature> [ <typed> ( <output fields> ) ]
//!                              { <procedure specification> }
//! <function definition>    ::= FUNCTION <qualified name> <callable input
//!                              signature> [ <typed> <value type> ]
//!                              ( = <value expression> | { <procedure
//!                              specification> } )
//! <input field signature>  ::= <field name> [ <typed> <value type> ]
//!                              [ = <value expression> ]
//! ```
//!
//! `DEFINE`, `PROCEDURE`, `FUNCTION`, and the `TYPED` marker are
//! non-reserved words recognized by spelling on plain `Identifier` tokens
//! (`at_define_keyword` checks `DEFINE` *followed by* `PROCEDURE`/`FUNCTION`
//! before committing, so `define` stays a usable variable name). Bodies
//! recurse through [`Parser::parse_statement_block`], the single ADR-0257
//! `block_depth` choke point every statement-nesting cycle funnels
//! through, so definition nesting is depth-guarded like every other
//! nested-body family.
//!
//! The AST normalization the CIP specifies (pull-up of nested definitions)
//! and the call-site rewrites live in the front end
//! (`cypherlite-front`'s `local_callables`), not here: this module only
//! parses and applies the per-definition syntactic rules the CIP authors
//! as parse-time errors (duplicate parameter names, `NOT NULL` in a
//! signature type, a default that references variables).

use super::*;

/// CIP-231 `<typed>`: the `::` separator or the `TYPED` marker word.
const TYPED_MARKER: &str = "typed";

impl Parser {
    /// True when a `<definition statement>` starts at the current
    /// position: the non-reserved word `DEFINE` immediately followed by
    /// `PROCEDURE` or `FUNCTION` (also by spelling, so `define` alone
    /// stays an ordinary identifier/variable name everywhere else).
    pub(super) fn at_define_keyword(&self) -> bool {
        self.peek_at_is_ident(0, "define")
            && (self.peek_at_is_ident(1, "procedure") || self.peek_at_is_ident(1, "function"))
    }

    /// Parses the whole `<definition block>`: zero or more `DEFINE`
    /// statements at the current position. The single call site is
    /// `parse_statement_block_at_current_depth`, the one choke point every
    /// braced and top-level statement-block parse funnels through.
    pub(super) fn parse_local_definitions(&mut self) -> ParseResult<Vec<LocalDefinition>> {
        let mut definitions = Vec::new();
        while self.at_define_keyword() {
            definitions.push(self.parse_local_definition()?);
        }
        Ok(definitions)
    }

    /// Parses one `DEFINE <procedure definition> | <function definition>`.
    fn parse_local_definition(&mut self) -> ParseResult<LocalDefinition> {
        self.next_token(); // "define" -- guarded by at_define_keyword
        let kind_tok = self.next_token();
        let kind = if Self::token_is_ident(&kind_tok, "procedure") {
            LocalCallableKind::Procedure
        } else if Self::token_is_ident(&kind_tok, "function") {
            LocalCallableKind::Function
        } else {
            return Err(self.error_at_token(
                &kind_tok,
                "InvalidSyntax: expected PROCEDURE or FUNCTION after DEFINE",
            ));
        };

        // The fully-qualified callable name: namespace segments joined on
        // `.`, kept as written (NAMESPACING-OF-CALLABLE-NAMES-ALLOWS).
        let mut name = self.parse_schema_name("a local callable name")?;
        while self.peek_type() == TokenType::Dot {
            self.next_token();
            name.push('.');
            name.push_str(&self.parse_schema_name("a local callable name segment")?);
        }

        let params = self.parse_local_input_signature(&name)?;

        let output = if self.at_typed_marker() {
            Some(self.parse_local_output(kind, &name)?)
        } else {
            None
        };

        // <function definition body> ::= <simple function definition body>
        // | <subquery function definition body>; a procedure always takes
        // the braced <procedure definition body> -- an `= <expr>` procedure
        // body is rejected here, at the one place the grammar decides it,
        // so no downstream layer re-checks the invariant.
        if kind == LocalCallableKind::Procedure && self.peek_type() == TokenType::Equals {
            return Err(self.error(
                "InvalidSyntax: a local procedure definition requires a braced query body, \
                 not `= <expression>`",
            ));
        }
        let body = if kind == LocalCallableKind::Function && self.peek_type() == TokenType::Equals {
            self.next_token();
            LocalDefinitionBody::Expression(self.parse_expression()?)
        } else {
            self.expect(TokenType::LeftBrace)?;
            // ADR-0257: the body is a full nested statement-block parse --
            // the `block_depth` guard engages on this recursion, so deeply
            // nested definition bodies are a typed error, not a stack
            // overflow (pinned in tests/limits.rs).
            let inner = self.parse_statement_block()?;
            self.expect(TokenType::RightBrace)?;
            LocalDefinitionBody::Query(inner)
        };

        Ok(LocalDefinition {
            kind,
            name,
            params,
            output,
            body,
        })
    }

    /// True when the `<typed>` marker (`::` or the word `TYPED`) starts at
    /// the current position.
    fn at_typed_marker(&self) -> bool {
        self.peek_type() == TokenType::DoubleColon || self.peek_is_ident(TYPED_MARKER)
    }

    /// Consumes the `<typed>` marker (`::` or `TYPED`): one token either
    /// way, so a single consume covers both spellings.
    fn consume_typed_marker(&mut self) {
        self.next_token();
    }

    /// Parses `<callable input signature>`: `( [field [:: type] [= expr]]
    /// , ... )`. Applies the CIP's per-signature syntactic rules: duplicate
    /// parameter names (42N67), a default that references variables, a
    /// literal default whose type contradicts the written parameter type
    /// (22NB1), and any `NOT NULL` anywhere in a parameter type (42NAL).
    fn parse_local_input_signature(&mut self, callable: &str) -> ParseResult<Vec<LocalParam>> {
        self.expect(TokenType::LeftParen)?;
        let mut params: Vec<LocalParam> = Vec::new();
        if self.peek_type() != TokenType::RightParen {
            loop {
                let name = self.parse_schema_name("a local callable parameter name")?;
                let type_expr = self.parse_optional_field_type(&[
                    TokenType::Comma,
                    TokenType::RightParen,
                    TokenType::Equals,
                ])?;
                let default = if self.peek_type() == TokenType::Equals {
                    self.next_token();
                    let expr = self.parse_expression()?;
                    // Defaults may not reference variables (CIP-231: the
                    // declared type of the incoming working record of a
                    // default's expression is the unit record type). The
                    // reference check is a whole-tree walk, so it lives in
                    // the front end (`local_callables`), which owns the
                    // shared statement/expression walkers; the literal
                    // type check below is local to the parse.
                    if let (Some(t), Expr::Literal(lit)) = (&type_expr, &expr) {
                        if let Some(rendered) = literal_type_mismatch(lit, t) {
                            return Err(self.error(format!(
                                "Type mismatch: expected to be {rendered} but was {}",
                                literal_kind_name(lit)
                            )));
                        }
                    }
                    Some(expr)
                } else {
                    None
                };
                if params.iter().any(|p| p.name == name) {
                    return Err(self.error(format!(
                        "Duplicate parameter {name} in local callable signature."
                    )));
                }
                // CIP-231 defaults fill from the right: once a parameter
                // carries a default, every later parameter must carry one
                // too -- otherwise the rewrite's arity check would face a
                // required parameter after an optional one (a panic, not
                // an error).
                if params.iter().any(|p| p.default.is_some()) && default.is_none() {
                    return Err(self.error(format!(
                        "InvalidSyntax: the parameter {name} is required, but an \
                         earlier parameter of local callable {callable} has a default; \
                         required parameters must precede defaulted ones."
                    )));
                }
                params.push(LocalParam {
                    name,
                    type_expr,
                    default,
                });
                if self.peek_type() == TokenType::Comma {
                    self.next_token();
                } else {
                    break;
                }
            }
        }
        self.expect(TokenType::RightParen)?;
        for param in &params {
            if let Some(t) = &param.type_expr {
                if type_contains_not_null(t) {
                    return Err(self.error(format!(
                        "{} is not supported as local callable parameter type. Adjust the \
                         type of parameter {} of local callable {}.",
                        render_type(t),
                        param.name,
                        callable
                    )));
                }
            }
        }
        Ok(params)
    }

    /// The CIP-231 field-type grammar `<name> [ <::|TYPED> <value type> ]`:
    /// the marker is optional, so both the marked (`band :: INTEGER`) and
    /// the bare (`band INTEGER`) spellings parse; `terminators` names the
    /// tokens that end the field instead (the input list adds `=`, the
    /// output list does not).
    fn parse_optional_field_type(
        &mut self,
        terminators: &[TokenType],
    ) -> ParseResult<Option<TypeExpr>> {
        if self.at_typed_marker() {
            self.consume_typed_marker();
            Ok(Some(self.parse_value_type()?))
        } else if !terminators.contains(&self.peek_type()) {
            Ok(Some(self.parse_value_type()?))
        } else {
            Ok(None)
        }
    }

    /// Parses the declared output type after the `<typed>` marker:
    /// a procedure's `( field [:: type] , ... )` field list (42NAJ on
    /// `NOT NULL`) or a function's `<value type>` return type (42NAK).
    fn parse_local_output(
        &mut self,
        kind: LocalCallableKind,
        callable: &str,
    ) -> ParseResult<LocalOutput> {
        self.consume_typed_marker();
        match kind {
            LocalCallableKind::Procedure => {
                self.expect(TokenType::LeftParen)?;
                let mut fields = Vec::new();
                if self.peek_type() != TokenType::RightParen {
                    loop {
                        let name = self.parse_schema_name("a local procedure output field")?;
                        let type_expr = self.parse_optional_field_type(&[
                            TokenType::Comma,
                            TokenType::RightParen,
                        ])?;
                        if let Some(t) = &type_expr {
                            if type_contains_not_null(t) {
                                return Err(self.error(format!(
                                    "{} is not supported as local procedure output type. \
                                     Adjust the type of output field {} of local procedure {}.",
                                    render_type(t),
                                    name,
                                    callable
                                )));
                            }
                        }
                        if fields
                            .iter()
                            .any(|field: &LocalOutputField| field.name == name)
                        {
                            return Err(self.error(format!(
                                "Duplicate output field {name} in local callable signature."
                            )));
                        }
                        fields.push(LocalOutputField { name, type_expr });
                        if self.peek_type() == TokenType::Comma {
                            self.next_token();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(TokenType::RightParen)?;
                Ok(LocalOutput::Fields(fields))
            }
            LocalCallableKind::Function => {
                let t = self.parse_value_type()?;
                if type_contains_not_null(&t) {
                    return Err(self.error(format!(
                        "{} is not supported as local function return type. Adjust the \
                         return type of local function {}.",
                        render_type(&t),
                        callable
                    )));
                }
                Ok(LocalOutput::Type(t))
            }
        }
    }
}

/// True when any `NOT NULL` appears anywhere in the type (CIP-231: neither
/// a nullable marker on a parameter/output/return type nor a nullable list
/// element is supported in a local callable signature).
fn type_contains_not_null(t: &TypeExpr) -> bool {
    match t {
        TypeExpr::Predefined { not_null, .. }
        | TypeExpr::List { not_null, .. }
        | TypeExpr::PropertyValue { not_null }
        | TypeExpr::Vector { not_null, .. } => *not_null || type_contains_not_null_list(t),
        TypeExpr::Union(items) => items.iter().any(type_contains_not_null),
    }
}

/// The recursive tail of [`type_contains_not_null`] for the container
/// forms whose own `not_null` was already checked.
fn type_contains_not_null_list(t: &TypeExpr) -> bool {
    match t {
        TypeExpr::List { element, .. } => type_contains_not_null(element),
        TypeExpr::Union(items) => items.iter().any(type_contains_not_null),
        _ => false,
    }
}

/// Renders a signature type for the authored error texts (the canonical
/// spelling of the same tree, `INTEGER NOT NULL` staying `INTEGER NOT
/// NULL`).
fn render_type(t: &TypeExpr) -> String {
    crate::type_normalize::render_canon(&crate::type_normalize::type_expr_to_canon(t))
}

/// The mismatch a literal default has against a written parameter type:
/// `Some(rendered expected)` when the literal's kind contradicts the type,
/// `None` when it matches (or the type is too rich to contradict a literal,
/// in which case runtime checking is the doc'd deferral, ADR-0382).
fn literal_type_mismatch(lit: &LiteralValue, t: &TypeExpr) -> Option<String> {
    let expected = match t {
        TypeExpr::Predefined { kind, .. } => *kind,
        TypeExpr::Union(items) => {
            // Any union member that the literal satisfies is enough.
            return if items
                .iter()
                .any(|item| literal_type_mismatch(lit, item).is_none())
            {
                None
            } else {
                Some(render_type(t))
            };
        }
        _ => return None,
    };
    let assignable = matches!(
        (lit, expected),
        (
            LiteralValue::Null,
            PredefinedKind::Null | PredefinedKind::Nothing | PredefinedKind::Any,
        ) | (
            LiteralValue::Bool(_),
            PredefinedKind::Boolean | PredefinedKind::Any
        ) | (
            LiteralValue::Integer(_),
            PredefinedKind::Integer | PredefinedKind::Any
        ) | (
            LiteralValue::Float(_),
            PredefinedKind::Float | PredefinedKind::Integer | PredefinedKind::Any,
        ) | (
            LiteralValue::String(_),
            PredefinedKind::String | PredefinedKind::Any
        )
    );
    if assignable {
        None
    } else {
        Some(render_type(t))
    }
}

/// The literal's own kind name for the 22NB1-style mismatch text.
fn literal_kind_name(lit: &LiteralValue) -> &'static str {
    match lit {
        LiteralValue::Null => "NULL",
        LiteralValue::Bool(_) => "BOOLEAN",
        LiteralValue::Integer(_) => "INTEGER",
        LiteralValue::Float(_) => "FLOAT",
        LiteralValue::String(_) => "STRING",
    }
}

#[cfg(test)]
mod tests {
    use super::render_type;

    use crate::parse;

    fn definition_err(query: &str) -> String {
        parse(query).unwrap_err().to_string()
    }

    #[test]
    fn parses_a_simple_function_definition() {
        let stmt = parse(
            "DEFINE FUNCTION isEven(num) = CASE WHEN num % 2 = 0 THEN true \
                          ELSE false END UNWIND range(1,4) AS x RETURN x, isEven(x)",
        )
        .unwrap();
        assert_eq!(stmt.definitions.len(), 1);
        let def = &stmt.definitions[0];
        assert_eq!(def.kind, crate::ast::LocalCallableKind::Function);
        assert_eq!(def.name, "isEven");
        assert_eq!(def.params.len(), 1);
        assert_eq!(def.params[0].name, "num");
        assert!(def.params[0].default.is_none());
        assert!(def.output.is_none());
        assert!(matches!(
            def.body,
            crate::ast::LocalDefinitionBody::Expression(_)
        ));
    }

    #[test]
    fn parses_a_namespaced_procedure_with_query_body() {
        let stmt = parse(
            "DEFINE PROCEDURE foo.bar(name :: STRING) :: (person :: NODE) { \
                          CREATE (p:Person {name: name}) RETURN p AS person } \
                          CALL foo.bar('Ada') YIELD person RETURN person.name",
        )
        .unwrap();
        let def = &stmt.definitions[0];
        assert_eq!(def.kind, crate::ast::LocalCallableKind::Procedure);
        assert_eq!(def.name, "foo.bar");
        assert_eq!(
            def.params[0].type_expr.as_ref().map(render_type),
            Some("STRING".to_string())
        );
        match &def.output {
            Some(crate::ast::LocalOutput::Fields(fields)) => {
                assert_eq!(fields[0].name, "person");
            }
            other => panic!("expected field output signature, got {other:?}"),
        }
        assert!(matches!(
            def.body,
            crate::ast::LocalDefinitionBody::Query(_)
        ));
    }

    #[test]
    fn parses_optional_parameters_with_defaults_and_typed_marker() {
        let stmt = parse(
            "DEFINE FUNCTION isEven(num, radix TYPED INTEGER = 2) = \
                          CASE WHEN num % radix = 0 THEN true ELSE false END RETURN 1 AS one",
        )
        .unwrap();
        let params = &stmt.definitions[0].params;
        assert_eq!(params.len(), 2);
        assert!(params[1].default.is_some());
        assert_eq!(
            params[1].type_expr.as_ref().map(render_type),
            Some("INTEGER".to_string())
        );
    }

    #[test]
    fn parses_a_function_query_body() {
        let stmt = parse(
            "DEFINE FUNCTION getBandName(name) { \
                          OPTIONAL MATCH (p:Person {name: name})-[:IN]->(b:Band) \
                          RETURN b.name LIMIT 1 } RETURN 1 AS one",
        )
        .unwrap();
        assert!(matches!(
            stmt.definitions[0].body,
            crate::ast::LocalDefinitionBody::Query(_)
        ));
    }

    #[test]
    fn parses_nested_definitions_inside_a_body() {
        let stmt = parse(
            "DEFINE FUNCTION hasEven(num) { \
                          DEFINE FUNCTION isEven(n) = n % 2 = 0 \
                          RETURN isEven(num) LIMIT 1 } RETURN 1 AS one",
        )
        .unwrap();
        match &stmt.definitions[0].body {
            crate::ast::LocalDefinitionBody::Query(inner) => {
                assert_eq!(inner.definitions.len(), 1);
                assert_eq!(inner.definitions[0].name, "isEven");
            }
            other => panic!("expected a query body, got {other:?}"),
        }
    }

    #[test]
    fn define_stays_an_ordinary_identifier() {
        // `define` without a following PROCEDURE/FUNCTION is just a
        // variable name; `PROCEDURE`/`FUNCTION` likewise stay usable.
        let stmt = parse("MATCH (define) RETURN define AS d").unwrap();
        assert!(stmt.definitions.is_empty());
        parse("RETURN [procedure IN [1,2] | procedure] AS fns").unwrap();
    }

    #[test]
    fn procedure_equals_body_is_rejected_at_parse_time() {
        // A procedure definition requires a braced query body; the `=`
        // form is a function-only body, rejected where the grammar
        // decides it.
        let error = definition_err("DEFINE PROCEDURE foo() = 1 CALL foo() YIELD a RETURN a");
        assert!(
            error.contains("a local procedure definition requires a braced query body"),
            "{error}"
        );
    }

    #[test]
    fn duplicate_parameter_names_are_rejected() {
        let error = definition_err(
            "DEFINE PROCEDURE foo(x, x) { RETURN 1 AS a } CALL foo(1, 2) YIELD a RETURN a",
        );
        assert!(
            error.contains("Duplicate parameter x in local callable signature."),
            "{error}"
        );
    }

    #[test]
    fn not_null_parameter_types_are_rejected() {
        let error = definition_err(
            "DEFINE PROCEDURE foo(a :: INT, b :: INT NOT NULL) { \
                                   RETURN 1 AS a } RETURN 1 AS one",
        );
        assert!(
            error.contains("INT NOT NULL is not supported as local callable parameter type")
                || error
                    .contains("INTEGER NOT NULL is not supported as local callable parameter type"),
            "{error}"
        );
        let error =
            definition_err("DEFINE FUNCTION foo.a(a :: LIST<INT NOT NULL>) = 1 RETURN foo.a([1])");
        assert!(
            error.contains(
                "is not supported as local callable parameter type. \
                            Adjust the type of parameter a of local callable foo.a"
            ) || error.contains(
                "is not supported as local callable parameter type. Adjust the \
                               type of parameter a of local callable foo.a"
            ),
            "{error}"
        );
    }

    #[test]
    fn not_null_output_and_return_types_are_rejected() {
        let error = definition_err(
            "DEFINE PROCEDURE foo() :: (b :: INT NOT NULL) { RETURN 2 AS b } RETURN 1 AS one",
        );
        assert!(
            error.contains("is not supported as local procedure output type"),
            "{error}"
        );
        let error = definition_err("DEFINE FUNCTION foo() :: INT NOT NULL = 2 RETURN foo()");
        assert!(
            error.contains("is not supported as local function return type"),
            "{error}"
        );
    }

    #[test]
    fn mistyped_literal_defaults_are_rejected() {
        let error = definition_err("DEFINE FUNCTION foo.a(x :: INT = \"abc\") = x RETURN foo.a()");
        assert!(
            error.contains("Type mismatch: expected to be INTEGER")
                || error.contains("Type mismatch: expected to be INT"),
            "{error}"
        );
    }
}
