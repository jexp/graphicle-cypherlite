//! CIP-262 (ADR-0388): the `<value expression>` of a property type's
//! `DEFAULT ON COMMIT` clause -- its restricted shape, its canonical rendering, its
//! static type (for the conformance check against the property's declared
//! value type), its persisted (serde) model, and its evaluation.
//!
//! The parser accepts any expression after `DEFAULT ON COMMIT`
//! (grammar-only, like the rest of a graph-type body); this module is
//! where the CIP-262 shape restriction lands: a literal (boolean, string,
//! integer, float, a list of those, or a map -- a map is a literal shape
//! that no storable property type accepts, so the *storable* check, not
//! the shape check, rejects it, CIP-283 NE3) or one of the allowed type
//! constructors (`uuid()`, the temporal constructors, `duration()`,
//! `point()`, `vector()`), each with literal/constructor arguments.
//! Everything else -- variables, parameters, general function calls,
//! arithmetic -- is rejected with the CIP-262 "expected a literal or a
//! type constructor expression" error.
//!
//! One typed enum is every consumer's single source: validation
//! (`from_expr`), the canonical `DEFAULT ON COMMIT <CVE>` rendering
//! (`render`), the conformance check (`conforms_to`), the persisted
//! model (serde, `PropertyTypeModel::default_value`), and evaluation
//! (`eval_default`, which rebuilds the expression and evaluates it
//! through the engine's own evaluator -- the one canonical evaluation
//! path, so `uuid()` and the temporal constructors behave exactly as
//! they do in queries).

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};
use cypherlite_error::EngineError;
use cypherlite_parser::type_normalize::{type_expr_to_canon, CanonKind, CanonType};
use cypherlite_parser::{Expr, FunctionCallExpr, LiteralValue, MapPair, PredefinedKind, TypeExpr};
use serde::{Deserialize, Serialize};

use crate::eval::{eval, Row};

use crate::overlay::ExecGraph;

/// The restricted `<value expression>` of a `DEFAULT ON COMMIT` clause
/// (CIP-262 Ap1): one typed form for validation, rendering, persistence,
/// and evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DefaultValue {
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    /// A list of allowed value expressions (all inner values static, or
    /// all evaluated at apply time -- the CIP's static/dynamic split is an
    /// evaluation-order optimization, not an observable difference, so
    /// this engine evaluates every default at apply time).
    List(Vec<DefaultValue>),
    /// A map of allowed value expressions: a literal *shape*, but never a
    /// storable property value -- only the storable/conformance checks
    /// reject it, never the shape check.
    Map(Vec<(String, DefaultValue)>),
    /// One of the allowed type constructors with its (restricted)
    /// arguments.
    Constructor {
        name: String,
        args: Vec<DefaultValue>,
    },
}

/// The allowed type constructors with their return types (CIP-262 Ap1):
/// the one table both the shape check (`from_expr`) and the static typing
/// (`static_canon`) consult, so a constructor can never be admitted
/// without a type or typed without being admitted. `vector()` is the one
/// constructor outside it: its `VECTOR<COORD>(DIMENSION)` return type
/// carries parameters, so it has its own arm in both. The no-argument
/// `uuid()` is allowed; constructors *with* arguments are not (a static
/// UUID a constraint author already knows must not be insertable as a
/// default).
const CONSTRUCTOR_TYPES: &[(&str, PredefinedKind)] = &[
    ("uuid", PredefinedKind::Uuid),
    ("date", PredefinedKind::Date),
    ("localtime", PredefinedKind::LocalTime),
    ("local_time", PredefinedKind::LocalTime),
    ("time", PredefinedKind::ZonedTime),
    ("zoned_time", PredefinedKind::ZonedTime),
    ("localdatetime", PredefinedKind::LocalDateTime),
    ("local_datetime", PredefinedKind::LocalDateTime),
    ("datetime", PredefinedKind::ZonedDateTime),
    ("zoned_datetime", PredefinedKind::ZonedDateTime),
    ("duration", PredefinedKind::Duration),
    ("point", PredefinedKind::Point),
];

/// The `vector()` constructor's name, the one constructor outside
/// [`CONSTRUCTOR_TYPES`] (its type carries parameters).
const VECTOR_CONSTRUCTOR: &str = "vector";

/// The return type of an admitted constructor, from the one table.
fn constructor_type(name: &str) -> Option<PredefinedKind> {
    if name == VECTOR_CONSTRUCTOR {
        return None; // typed by its own arm; still admitted
    }
    CONSTRUCTOR_TYPES
        .iter()
        .find(|(constructor, _)| *constructor == name)
        .map(|(_, kind)| *kind)
}

/// Whether `name` is an admitted constructor (the table's, or
/// `vector()`).
fn is_admitted_constructor(name: &str) -> bool {
    name == VECTOR_CONSTRUCTOR || constructor_type(name).is_some()
}

impl DefaultValue {
    /// Captures the parser expression's restricted shape (CIP-262): a
    /// literal (a map among them -- a literal *shape* whose storable
    /// check, not its shape, rejects it, CIP-283 NE3), a list/map of
    /// allowed values, or an allowed type constructor with allowed
    /// arguments. `null` and every other expression shape are the two
    /// CIP-262 rejection cases, as typed errors (the two CIP texts,
    /// verbatim in message form).
    pub fn from_expr(e: &Expr) -> std::result::Result<Self, EngineError> {
        let expected = "expected a literal or a type constructor expression";
        match e {
            Expr::Literal(LiteralValue::Null) => Err(default_shape_error(format!(
                "Invalid input null for default property value. {expected}"
            ))),
            Expr::Literal(LiteralValue::Bool(b)) => Ok(Self::Bool(*b)),
            Expr::Literal(LiteralValue::Integer(i)) => Ok(Self::Integer(*i)),
            Expr::Literal(LiteralValue::Float(f)) => Ok(Self::Float(*f)),
            Expr::Literal(LiteralValue::String(s)) => Ok(Self::String(s.clone())),
            Expr::List(items) => Ok(Self::List(
                items
                    .iter()
                    .map(Self::from_expr)
                    .collect::<std::result::Result<_, _>>()?,
            )),
            Expr::Map(pairs) => Ok(Self::Map(
                pairs
                    .iter()
                    .map(|MapPair { key, value }| {
                        let captured: std::result::Result<_, EngineError> =
                            Ok((key.clone(), Self::from_expr(value)?));
                        captured
                    })
                    .collect::<std::result::Result<Vec<_>, EngineError>>()?,
            )),
            Expr::FunctionCall(call) => {
                let name = call.name.to_ascii_lowercase();
                if !is_admitted_constructor(&name) {
                    return Err(default_shape_error(format!(
                        "Invalid input for default property value on function \
                         '{name}'. {expected}"
                    )));
                }
                if name == "uuid" && !call.args.is_empty() {
                    return Err(default_shape_error(
                        "uuid() takes no arguments in a default property value".to_string(),
                    ));
                }
                let args = call
                    .args
                    .iter()
                    .map(Self::from_expr)
                    .collect::<std::result::Result<_, _>>()?;
                Ok(Self::Constructor { name, args })
            }
            _ => Err(default_shape_error(format!(
                "Invalid input for default property value. {expected}"
            ))),
        }
    }

    /// Whether this value can be stored on a property: every literal leaf
    /// but a map (a map is only ever an argument of a constructor that
    /// consumes it), every list of storable values, and every constructor
    /// of the allowed set.
    pub fn is_storable(&self) -> bool {
        match self {
            Self::Bool(_) | Self::Integer(_) | Self::Float(_) | Self::String(_) => true,
            Self::List(items) => items.iter().all(Self::is_storable),
            Self::Map(_) => false,
            Self::Constructor { .. } => true,
        }
    }

    /// The default's static type, the form the CIP-262 conformance check
    /// compares against the property's declared value type: a constructor
    /// types by its name alone (`uuid()` is `UUID NOT NULL`, the temporals
    /// their own kinds, `point()` is `POINT`, `vector()` is a `VECTOR`
    /// whose coordinate/dimension halves stay unspecified -- a
    /// dimension-mismatched `vector()` default fails at apply time through
    /// the derived property-type constraint, not at declaration time).
    /// The one constructor table answers, so an admitted name always
    /// types; a name outside it fails closed instead of typing `ANY`.
    fn static_canon(&self) -> std::result::Result<CanonType, EngineError> {
        let predefined = |kind| CanonType::predefined(kind, true);
        match self {
            Self::Bool(_) => Ok(predefined(PredefinedKind::Boolean)),
            Self::Integer(_) => Ok(predefined(PredefinedKind::Integer)),
            Self::Float(_) => Ok(predefined(PredefinedKind::Float)),
            Self::String(_) => Ok(predefined(PredefinedKind::String)),
            Self::List(items) => Ok(CanonType::list(
                items
                    .iter()
                    .map(Self::static_canon)
                    .collect::<std::result::Result<Vec<_>, _>>()?
                    .into_iter()
                    .reduce(|a, b| super::canon_intersection(&a, &b))
                    .unwrap_or_else(|| CanonType::predefined(PredefinedKind::Any, true)),
                true,
            )),
            Self::Map(_) => Ok(predefined(PredefinedKind::Map)),
            Self::Constructor { name, .. } => match constructor_type(name) {
                Some(kind) => Ok(predefined(kind)),
                None if name == VECTOR_CONSTRUCTOR => Ok(CanonType {
                    kind: CanonKind::Vector {
                        coord: None,
                        dimension: None,
                    },
                    not_null: true,
                }),
                None => Err(super::syn(format!(
                    "the constructor '{name}' is not an admitted default value constructor"
                ))),
            },
        }
    }

    /// The CIP-262 conformance check: the default's static type is allowed
    /// under the property's declared value type (a `NOT NULL` static type
    /// under a nullable declared type is fine -- a non-null value is
    /// always allowed under a nullable type; the reverse is not). The
    /// subsumption relation itself is the type lattice's
    /// ([`super::canon_allows`], shared with CIP-266's
    /// restriction-only re-specification check).
    pub fn conforms_to(&self, declared: &TypeExpr) -> std::result::Result<bool, EngineError> {
        Ok(super::canon_allows(
            &type_expr_to_canon(declared),
            &self.static_canon()?,
        ))
    }

    /// The canonical form of the value expression (CIP-262 canonical
    /// syntax's `CVE`): string literals single-quoted (backslashes and
    /// embedded quotes escaped), lists bracketed, map keys backticked and
    /// sorted, constructors by name with their canonical arguments.
    pub fn render(&self) -> String {
        match self {
            Self::Bool(b) => b.to_string(),
            Self::Integer(i) => i.to_string(),
            // The fractional marker survives whole-valued floats: the
            // canonical text is the durable snapshot format and restores
            // without re-validation (ADR-0388), so `1.0` rendering as `1`
            // silently turned a FLOAT default into an INTEGER one on
            // restore (the round-10 High) and every subsequent defaulted
            // write failed its FLOAT constraint.
            Self::Float(f) => {
                let text = format!("{f}");
                if text.contains('.')
                    || text.contains('e')
                    || text.contains("inf")
                    || text.contains("NaN")
                {
                    text
                } else {
                    format!("{text}.0")
                }
            }
            Self::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
            Self::List(items) => {
                let inner: Vec<String> = items.iter().map(Self::render).collect();
                format!("[{}]", inner.join(", "))
            }
            Self::Map(pairs) => {
                let mut entries: Vec<String> = pairs
                    .iter()
                    .map(|(k, v)| format!("`{}`: {}", k.replace('`', "``"), v.render()))
                    .collect();
                entries.sort();
                format!("{{{}}}", entries.join(", "))
            }
            Self::Constructor { name, args } => {
                let inner: Vec<String> = args.iter().map(Self::render).collect();
                format!("{name}({})", inner.join(", "))
            }
        }
    }

    /// Rebuilds the parser expression for evaluation: the mechanical
    /// inverse of [`DefaultValue::from_expr`], used only to hand the
    /// value to the engine's own evaluator (and, from the persisted
    /// model, to rebuild the body's parser form).
    pub(crate) fn to_expr(&self) -> Expr {
        match self {
            Self::Bool(b) => Expr::Literal(LiteralValue::Bool(*b)),
            Self::Integer(i) => Expr::Literal(LiteralValue::Integer(*i)),
            Self::Float(f) => Expr::Literal(LiteralValue::Float(*f)),
            Self::String(s) => Expr::Literal(LiteralValue::String(s.clone())),
            Self::List(items) => Expr::List(items.iter().map(Self::to_expr).collect()),
            Self::Map(pairs) => Expr::Map(
                pairs
                    .iter()
                    .map(|(k, v)| MapPair {
                        key: k.clone(),
                        value: v.to_expr(),
                    })
                    .collect(),
            ),
            Self::Constructor { name, args } => Expr::FunctionCall(FunctionCallExpr {
                name: name.clone(),
                args: args.iter().map(Self::to_expr).collect(),
                distinct: false,
            }),
        }
    }
}

/// The typed CIP-262 shape error: the message is the CIP's own text,
/// the kind the graph-type layer's (`SyntaxError`, the same class every
/// other graph-type rejection carries).
fn default_shape_error(message: String) -> EngineError {
    super::syn(message)
}

/// The separator the canonical property rendering places between the
/// property's type and its `DEFAULT ON COMMIT` value -- the one spelling
/// both the renderer and the constraint-validation type-prefix split
/// consult (the round-6 Low: the literal was spelled twice).
pub(crate) const DEFAULT_ON_COMMIT_SEPARATOR: &str = " DEFAULT ON COMMIT ";

/// The canonical `DEFAULT ON COMMIT <CVE>` clause of a property type,
/// rendered from the parsed expression: the one rendering every
/// canonical/display site consults (the graph-type body form, the
/// `SHOW ... AS GRAPH` property strings, the derived property-type
/// constraint's stored string). The shape invariant holds at every
/// caller (the body validator and the model capture reject a
/// non-allowed shape before anything renders), so the capture here
/// fails loud on its invariant instead of inventing a rendering.
pub(crate) fn canonical_default_clause(expr: &Expr) -> Result<String> {
    // Total: the DROP path restates a default in a body that never runs
    // the graph-type body validator (apply_drop), so the shape invariant
    // does not hold at every caller -- an unrenderable restatement is a
    // typed error the exact-match check feeds (the "node element type
    // specified incorrectly" family), never a panic.
    let default =
        DefaultValue::from_expr(expr).map_err(|error| Error::syntax(error.to_string()))?;
    Ok(format!(" DEFAULT ON COMMIT {}", default.render()))
}

/// The default's lowered expression, captured once per statement per
/// property -- only the evaluation runs per entity (the round-10
/// Medium: the re-clone and re-lowering ran on every per-entity call).
pub fn lowered_default(default: &DefaultValue) -> Result<cypherlite_ir::Expr> {
    cypherlite_front::lower_expr(&default.to_expr())
}

/// The per-entity half of [`eval_default`]: evaluate the pre-lowered
/// default through the engine's own evaluator.
pub fn eval_lowered_default(graph: &ExecGraph<'_>, lowered: &cypherlite_ir::Expr) -> Result<Value> {
    eval(graph, &Row::new(), lowered, &BTreeMap::new(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_parser::parser::parse_expression_str;

    fn default_of(src: &str) -> DefaultValue {
        let expr = parse_expression_str(src).expect("parses");
        DefaultValue::from_expr(&expr).expect("shape-valid")
    }

    #[test]
    fn literals_capture_and_render() {
        assert_eq!(default_of("'Hello'").render(), "'Hello'");
        assert_eq!(default_of("42").render(), "42");
        assert_eq!(default_of("true").render(), "true");
        assert_eq!(default_of("6.66").render(), "6.66");
        assert_eq!(default_of("['a', 'b']").render(), "['a', 'b']");
    }

    #[test]
    fn constructors_capture_and_render() {
        assert_eq!(default_of("uuid()").render(), "uuid()");
        assert_eq!(
            default_of("date('2027-01-01')").render(),
            "date('2027-01-01')"
        );
        assert_eq!(
            default_of("point({x: 4, y: 2})").render(),
            "point({`x`: 4, `y`: 2})"
        );
    }

    #[test]
    fn null_and_general_expressions_are_shape_errors() {
        let expr = parse_expression_str("null").expect("parses");
        let error = DefaultValue::from_expr(&expr).unwrap_err().to_string();
        assert!(
            error.contains("Invalid input null for default property value"),
            "{error}"
        );
        let expr = parse_expression_str("toString(5 + $param)").expect("parses");
        let error = DefaultValue::from_expr(&expr).unwrap_err().to_string();
        assert!(
            error.contains("expected a literal or a type constructor expression"),
            "{error}"
        );
        let expr = parse_expression_str("uuid('known')").expect("parses");
        assert!(DefaultValue::from_expr(&expr).is_err());
    }

    #[test]
    fn conformance_follows_the_declared_type() {
        assert!(default_of("42")
            .conforms_to(&parse_type("INTEGER"))
            .unwrap());
        assert!(default_of("42")
            .conforms_to(&parse_type("INTEGER | FLOAT"))
            .unwrap());
        assert!(!default_of("42").conforms_to(&parse_type("STRING")).unwrap());
        assert!(default_of("'Hi'").conforms_to(&parse_type("ANY")).unwrap());
        assert!(default_of("uuid()")
            .conforms_to(&parse_type("UUID"))
            .unwrap());
        assert!(!default_of("uuid()")
            .conforms_to(&parse_type("INTEGER"))
            .unwrap());
        // A NOT NULL default under a nullable declared type is fine.
        assert!(default_of("42")
            .conforms_to(&parse_type("INTEGER NOT NULL"))
            .unwrap());
    }

    #[test]
    fn a_map_is_shape_valid_but_not_storable() {
        let map = default_of("{key: 'value'}");
        assert!(!map.is_storable(), "a map is not storable on a property");
        assert!(default_of("['a']").is_storable());
        assert!(default_of("point({x: 1, y: 2})").is_storable());
    }

    fn parse_type(src: &str) -> TypeExpr {
        let full = format!("CREATE GRAPH TYPE t AS {{ (:L => {{ p :: {src} }}) }}");
        let stmt = cypherlite_parser::parse(&full).expect("parses");
        match stmt.clauses.into_iter().next().expect("one clause") {
            cypherlite_parser::Clause::CreateGraphType(c) => {
                c.body.node_types[0].properties[0].value_type.clone()
            }
            other => panic!("unexpected clause {other:?}"),
        }
    }
    #[test]
    fn whole_valued_floats_render_with_their_fractional_marker() {
        // The round-10 High regression: the canonical text is the durable
        // snapshot format and restores without re-validation, so `1.0`
        // rendering as `1` silently turned a FLOAT default into an
        // INTEGER one on restore.
        assert_eq!(default_of("1.0").render(), "1.0");
        assert_eq!(default_of("2.5").render(), "2.5");
        assert_eq!(default_of("1.0e3").render(), "1000.0");
    }
}
