use super::*;

pub(super) fn map_exprs(xs: &[cl::Expr]) -> Result<Vec<mdb::Expr>> {
    xs.iter().map(lower_expr).collect()
}

/// One GQL function alias (CIP-245): the snake_case GQL/SQL spelling the
/// parser accepts, the Cypher-canonical function name it lowers to, and
/// whether the spelling is a temporal constructor — the vocabulary the
/// `tostring_full` precision policy matches (see the table below).
struct GqlFunctionAlias {
    gql: &'static str,
    cypher: &'static str,
    temporal_constructor: bool,
}

/// GQL function aliases (CIP-245): the snake_case GQL/SQL spellings the
/// parser accepts, each lowered to its Cypher-canonical function name.
/// The `temporal_constructor` markers are the `toString` rendering
/// vocabulary: `toString(<temporal constructor call>)` renders full
/// precision, so the lowering renames such a call to the internal
/// `tostring_full` function (an `UNADVERTISED_FUNCTIONS` name, never
/// written by users). One table carries both policies — the rename and
/// the rendering — so a new spelling updates both together.
///
/// This is the scalar-function alias table. It is a second table beside
/// [`crate::agg::AGGREGATE_ALIASES`] (which names itself "the single alias
/// table") by necessity, not drift: that table's entries must target
/// members of the canonical `AGGREGATES` set and carry display spellings
/// for column naming — invariants these non-aggregate renames cannot
/// satisfy. One table per function class, both consulted from
/// `lower_function_name`.
const GQL_FUNCTION_ALIASES: &[GqlFunctionAlias] = &[
    GqlFunctionAlias {
        gql: "path_length",
        cypher: "length",
        temporal_constructor: false,
    },
    GqlFunctionAlias {
        gql: "duration_between",
        cypher: "duration.between",
        temporal_constructor: false,
    },
    GqlFunctionAlias {
        gql: "local_time",
        cypher: "localtime",
        temporal_constructor: true,
    },
    GqlFunctionAlias {
        gql: "local_datetime",
        cypher: "localdatetime",
        temporal_constructor: true,
    },
    GqlFunctionAlias {
        gql: "zoned_time",
        cypher: "time",
        temporal_constructor: true,
    },
    GqlFunctionAlias {
        gql: "zoned_datetime",
        cypher: "datetime",
        temporal_constructor: true,
    },
];

/// CIP-214: `vector_distance`/`vector_norm` take a bare distance-metric
/// keyword (e.g. EUCLIDEAN) at the argument position the canonical
/// `crate::ast::collect::distance_metric_arg_index` table records; the keyword
/// parses as a variable reference. Resolve known metric names to the
/// typed `DistanceMetric` literal; anything else stays a `Variable` so
/// the binder rejects it with `UndefinedVariable` when it is not in
/// scope.
fn resolve_vector_metric_arg(lowered_name: &str, args: &mut [mdb::Expr]) {
    let Some(metric_index) = crate::ast::collect::distance_metric_arg_index(lowered_name) else {
        return;
    };
    if let Some(metric_arg) = args.get_mut(metric_index) {
        if let mdb::Expr::Variable(metric) = metric_arg {
            if let Some(metric) = mdb::VectorMetric::from_name(metric) {
                *metric_arg = mdb::Expr::DistanceMetric(metric);
            }
        }
    }
}

/// The IR name for a parsed function call, applying the lowering's
/// three name policies in order: the `tostring_full` precision rename,
/// the aggregate alias display spelling (CIP-128, resolved from the
/// single alias table in `agg`), and the GQL scalar alias table
/// (CIP-245). A name matching no policy keeps its parsed spelling.
fn lower_function_name(f: &cl::FunctionCallExpr, lowered_name: &str) -> String {
    if is_tostring_of_gql_temporal(f, lowered_name) {
        "tostring_full".into()
    } else if let Some(display) = crate::agg::display_agg_alias(lowered_name) {
        display.into()
    } else if let Some(alias) = GQL_FUNCTION_ALIASES
        .iter()
        .find(|alias| alias.gql == lowered_name)
    {
        alias.cypher.into()
    } else {
        f.name.clone()
    }
}

/// True when the call is `toString(<GQL temporal constructor call>)`
/// (case-insensitive on both names): the caller whose argument names one
/// of the temporal-constructor entries of `GQL_FUNCTION_ALIASES`,
/// lowering to the full-precision `tostring_full` function. The match is
/// name-based — the lower layer has no typed view of the argument call;
/// a parser-level typing is out of scope here (noted on the PR).
fn is_tostring_of_gql_temporal(f: &cl::FunctionCallExpr, lowered_name: &str) -> bool {
    lowered_name == "tostring"
        && matches!(
            f.args.first(),
            Some(cl::Expr::FunctionCall(inner))
                if is_gql_temporal_constructor(&inner.name.to_ascii_lowercase())
        )
}

/// True when `lowered_name` is one of the temporal-constructor entries of
/// `GQL_FUNCTION_ALIASES` — the spellings whose `toString(<call>)`
/// renders full precision.
fn is_gql_temporal_constructor(lowered_name: &str) -> bool {
    GQL_FUNCTION_ALIASES
        .iter()
        .any(|alias| alias.temporal_constructor && alias.gql == lowered_name)
}

pub fn lower_expr(e: &cl::Expr) -> Result<mdb::Expr> {
    Ok(match e {
        cl::Expr::Literal(cl::LiteralValue::Null) => mdb::Expr::Null,
        cl::Expr::Literal(cl::LiteralValue::Bool(b)) => mdb::Expr::Bool(*b),
        cl::Expr::Literal(cl::LiteralValue::Integer(i)) => mdb::Expr::Int(*i),
        cl::Expr::Literal(cl::LiteralValue::Float(f)) => mdb::Expr::Float(*f),
        cl::Expr::Literal(cl::LiteralValue::String(s)) => mdb::Expr::String(s.clone()),
        // CIP-261: lower to the internal `string.interp` function. Literal
        // text parts become String args and embeds their own expressions;
        // an embedded string literal renders identically to its text, so
        // the two arg shapes cannot diverge in output.
        cl::Expr::StringInterp(parts) => {
            let mut args = Vec::with_capacity(parts.len());
            for p in parts {
                match p {
                    cl::InterpPart::Text(t) => args.push(mdb::Expr::String(t.clone())),
                    cl::InterpPart::Interp(e) => args.push(lower_expr(e)?),
                }
            }
            mdb::Expr::Function {
                name: "string.interp".into(),
                distinct: false,
                args,
            }
        }
        cl::Expr::List(xs) => mdb::Expr::List(map_exprs(xs)?),
        cl::Expr::Map(pairs) => mdb::Expr::Map(
            pairs
                .iter()
                .map(|p| Ok((p.key.clone(), lower_expr(&p.value)?)))
                .collect::<Result<Vec<_>>>()?,
        ),
        cl::Expr::Variable(v) => mdb::Expr::Variable(v.clone()),
        cl::Expr::Parameter(p) => mdb::Expr::Parameter(p.clone()),
        cl::Expr::Property(p) => {
            if let Some(key) = &p.key_expr {
                mdb::Expr::DynamicProperty {
                    expr: Box::new(lower_expr(&p.expr)?),
                    key: Box::new(lower_expr(key)?),
                }
            } else {
                mdb::Expr::Property {
                    expr: Box::new(lower_expr(&p.expr)?),
                    key: p.property.clone(),
                }
            }
        }
        cl::Expr::FunctionCall(f) => {
            let mut args = map_exprs(&f.args)?;
            let lowered_name = f.name.to_ascii_lowercase();
            resolve_vector_metric_arg(&lowered_name, &mut args);
            mdb::Expr::Function {
                name: lower_function_name(f, &lowered_name),
                distinct: f.distinct,
                args,
            }
        }
        cl::Expr::Binary(b) => mdb::Expr::Binary {
            op: bin_op(&b.op)?,
            left: Box::new(lower_expr(&b.left)?),
            right: Box::new(lower_expr(&b.right)?),
        },
        cl::Expr::Unary(u) => mdb::Expr::Unary {
            op: unary_op(&u.op)?,
            expr: Box::new(lower_expr(&u.expr)?),
        },
        cl::Expr::Comparison(c) => mdb::Expr::Binary {
            op: cmp_op(&c.op)?,
            left: Box::new(lower_expr(&c.left)?),
            right: Box::new(lower_expr(&c.right)?),
        },
        cl::Expr::In(i) => mdb::Expr::Binary {
            op: mdb::BinaryOp::In,
            left: Box::new(lower_expr(&i.expr)?),
            right: Box::new(lower_expr(&i.list)?),
        },
        cl::Expr::IsNull(n) => mdb::Expr::Unary {
            op: if n.negated {
                mdb::UnaryOp::IsNotNull
            } else {
                mdb::UnaryOp::IsNull
            },
            expr: Box::new(lower_expr(&n.expr)?),
        },
        cl::Expr::StringMatch(s) => mdb::Expr::Binary {
            op: match s.op.as_str() {
                "STARTS WITH" => mdb::BinaryOp::StartsWith,
                "ENDS WITH" => mdb::BinaryOp::EndsWith,
                "CONTAINS" => mdb::BinaryOp::Contains,
                _ => return Err(nyi(&format!("string match {}", s.op))),
            },
            left: Box::new(lower_expr(&s.left)?),
            right: Box::new(lower_expr(&s.right)?),
        },
        cl::Expr::CountStar => mdb::Expr::CountStar,
        cl::Expr::Star => mdb::Expr::Star,
        cl::Expr::Case(c) => mdb::Expr::Case {
            operand: lower_boxed(c.test.as_ref(), lower_expr)?,
            when_then: c
                .whens
                .iter()
                .map(|w| {
                    Ok((
                        lower_expr(&w.condition)?,
                        lower_expr(&w.result)?,
                        w.is_predicate,
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
            else_expr: lower_boxed(c.else_expr.as_ref(), lower_expr)?,
        },
        cl::Expr::ExistsSubquery(s) => mdb::Expr::ExistsSubquery {
            query: lower_statement_impl(&s.statement)?,
        },
        cl::Expr::ListComprehension(l) => mdb::Expr::ListComp {
            ident: l.variable.clone(),
            list: Box::new(lower_expr(&l.list_expr)?),
            predicate: lower_boxed(l.where_expr.as_ref(), lower_expr)?,
            projection: Box::new(match &l.map_expr {
                Some(m) => lower_expr(m)?,
                None => mdb::Expr::Variable(l.variable.clone()),
            }),
        },
        cl::Expr::MapComprehension(mc) => mdb::Expr::MapComp {
            key_ident: mc.key_var.clone(),
            value_ident: mc.value_var.clone(),
            source: Box::new(lower_expr(&mc.source_expr)?),
            predicate: lower_boxed(mc.where_expr.as_ref(), lower_expr)?,
            key_expr: Box::new(lower_expr(&mc.key_expr)?),
            value_expr: Box::new(lower_expr(&mc.value_expr)?),
        },
        cl::Expr::PatternComprehension(p) => mdb::Expr::PatternComp {
            path: {
                let mut path = lower_path(&p.pattern)?;
                path.variable = p.path_variable.clone().or(path.variable);
                path
            },
            predicate: lower_boxed(p.where_expr.as_ref(), lower_expr)?,
            projection: Box::new(lower_expr(&p.map_expr)?),
        },
        cl::Expr::ShortestPath(s) => mdb::Expr::ShortestPath {
            all: s.all,
            path: lower_path(&s.pattern)?,
        },
        cl::Expr::Subscript(s) => {
            if s.slice {
                mdb::Expr::Slice {
                    expr: Box::new(lower_expr(&s.expr)?),
                    start: lower_boxed(s.from.as_ref(), lower_expr)?,
                    end: lower_boxed(s.to.as_ref(), lower_expr)?,
                }
            } else {
                mdb::Expr::Subscript {
                    expr: Box::new(lower_expr(&s.expr)?),
                    index: Box::new(lower_expr(
                        s.index.as_ref().ok_or_else(|| nyi("empty subscript"))?,
                    )?),
                }
            }
        }
        cl::Expr::Quantifier(q) => mdb::Expr::Quantifier {
            kind: match q.quantifier.to_ascii_lowercase().as_str() {
                "all" => mdb::QuantifierKind::All,
                "any" => mdb::QuantifierKind::Any,
                "none" => mdb::QuantifierKind::None,
                "single" => mdb::QuantifierKind::Single,
                _ => return Err(nyi(&format!("quantifier {}", q.quantifier))),
            },
            ident: q.variable.clone(),
            list: Box::new(lower_expr(&q.list)?),
            predicate: Box::new(match &q.predicate {
                Some(p) => lower_expr(p)?,
                None => mdb::Expr::Bool(true),
            }),
        },
        cl::Expr::HasLabel(h) => {
            let check = mdb::Expr::LabelPredicate {
                expr: Box::new(lower_expr(&h.expr)?),
                label_expr: lower_label_expr(&h.label_expr)?,
                strict: true,
            };
            if h.negated {
                mdb::Expr::Unary {
                    op: mdb::UnaryOp::Not,
                    expr: Box::new(check),
                }
            } else {
                check
            }
        }
        cl::Expr::Paren(inner) => lower_expr(inner)?,
        cl::Expr::PatternPredicate(p) => {
            let path = lower_path(&p.pattern)?;
            if path.rels.is_empty() {
                return Err(Error::syntax(
                    "InvalidArgumentType: a pattern predicate requires a relationship",
                ));
            }
            mdb::Expr::PatternPred { path }
        }
        cl::Expr::CountSubquery(s) => mdb::Expr::CountSubquery {
            query: lower_statement_impl(&s.statement)?,
        },
        cl::Expr::CollectSubquery(s) => mdb::Expr::CollectSubquery {
            query: lower_statement_impl(&s.statement)?,
        },
        cl::Expr::Reduce(r) => mdb::Expr::Reduce {
            accumulator: r.accumulator.clone(),
            initial: Box::new(lower_expr(&r.init_expr)?),
            ident: r.variable.clone(),
            list: Box::new(lower_expr(&r.list_expr)?),
            expression: Box::new(lower_expr(&r.expression)?),
        },
        cl::Expr::AllReduce(r) => mdb::Expr::AllReduce {
            accumulator: r.accumulator.clone(),
            initial: Box::new(lower_expr(&r.init_expr)?),
            ident: r.variable.clone(),
            list: Box::new(lower_expr(&r.list_expr)?),
            expression: Box::new(lower_expr(&r.expression)?),
            predicate: Box::new(lower_expr(&r.predicate)?),
        },
        cl::Expr::IsType(t) => mdb::Expr::IsType {
            expr: Box::new(lower_expr(&t.expr)?),
            type_expr: lower_type_expr(&t.type_expr),
            negated: t.negated,
        },
        cl::Expr::IsNormalized(n) => mdb::Expr::IsNormalized {
            expr: Box::new(lower_expr(&n.expr)?),
            form: n.form.clone(),
            negated: n.negated,
        },
        cl::Expr::MapProjection(projection) => mdb::Expr::MapProjection {
            expr: Box::new(lower_expr(&projection.expr)?),
            items: projection
                .items
                .iter()
                .map(|item| {
                    Ok(match item {
                        cl::MapProjectionItem::KeyValue(key, value) => {
                            mdb::MapProjectionItem::KeyValue {
                                key: key.clone(),
                                value: lower_expr(value)?,
                            }
                        }
                        cl::MapProjectionItem::Property(key) => {
                            mdb::MapProjectionItem::Property { key: key.clone() }
                        }
                        cl::MapProjectionItem::Variable(name) => {
                            mdb::MapProjectionItem::Variable { name: name.clone() }
                        }
                        cl::MapProjectionItem::AllProperties => {
                            mdb::MapProjectionItem::AllProperties
                        }
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        },
    })
}

pub(super) fn lower_label_expr(e: &cl::LabelExpr) -> Result<mdb::LabelExpr> {
    Ok(match e {
        cl::LabelExpr::Label(name) => mdb::LabelExpr::Label(name.clone()),
        cl::LabelExpr::Wildcard => mdb::LabelExpr::Wildcard,
        cl::LabelExpr::Dynamic(d) => mdb::LabelExpr::Dynamic {
            expr: Box::new(lower_expr(&d.expr)?),
            quantifier: match d.quantifier {
                cl::DynamicLabelQuantifier::Default => mdb::DynamicLabelQuantifier::Default,
                cl::DynamicLabelQuantifier::All => mdb::DynamicLabelQuantifier::All,
                cl::DynamicLabelQuantifier::Any => mdb::DynamicLabelQuantifier::Any,
            },
        },
        cl::LabelExpr::Not(inner) => mdb::LabelExpr::Not(Box::new(lower_label_expr(inner)?)),
        cl::LabelExpr::And(items) => {
            mdb::LabelExpr::And(items.iter().map(lower_label_expr).collect::<Result<_>>()?)
        }
        cl::LabelExpr::Or(items) => {
            mdb::LabelExpr::Or(items.iter().map(lower_label_expr).collect::<Result<_>>()?)
        }
    })
}

/// Lower a parser-AST `TypeExpr` to the planner-IR `TypeExpr` (the
/// executor's type currency — e.g. re-parsed constraint type strings fed to
/// `value_matches_type`).
pub fn lower_type_expr(t: &cl::TypeExpr) -> mdb::TypeExpr {
    match t {
        cl::TypeExpr::Predefined { kind, not_null } => mdb::TypeExpr::Predefined {
            kind: match kind {
                cl::PredefinedKind::Nothing => mdb::PredefinedType::Nothing,
                cl::PredefinedKind::Null => mdb::PredefinedType::Null,
                cl::PredefinedKind::Any => mdb::PredefinedType::Any,
                cl::PredefinedKind::Boolean => mdb::PredefinedType::Boolean,
                cl::PredefinedKind::String => mdb::PredefinedType::String,
                cl::PredefinedKind::Integer => mdb::PredefinedType::Integer,
                cl::PredefinedKind::Float => mdb::PredefinedType::Float,
                cl::PredefinedKind::Date => mdb::PredefinedType::Date,
                cl::PredefinedKind::Duration => mdb::PredefinedType::Duration,
                cl::PredefinedKind::Point => mdb::PredefinedType::Point,
                cl::PredefinedKind::LocalTime => mdb::PredefinedType::LocalTime,
                cl::PredefinedKind::ZonedTime => mdb::PredefinedType::ZonedTime,
                cl::PredefinedKind::LocalDateTime => mdb::PredefinedType::LocalDateTime,
                cl::PredefinedKind::ZonedDateTime => mdb::PredefinedType::ZonedDateTime,
                cl::PredefinedKind::Node => mdb::PredefinedType::Node,
                cl::PredefinedKind::Relationship => mdb::PredefinedType::Relationship,
                cl::PredefinedKind::Path => mdb::PredefinedType::Path,
                cl::PredefinedKind::Map => mdb::PredefinedType::Map,
                cl::PredefinedKind::Uuid => mdb::PredefinedType::Uuid,
            },
            not_null: *not_null,
        },
        cl::TypeExpr::List { element, not_null } => mdb::TypeExpr::List {
            element: Box::new(lower_type_expr(element)),
            not_null: *not_null,
        },
        cl::TypeExpr::PropertyValue { not_null } => mdb::TypeExpr::PropertyValue {
            not_null: *not_null,
        },
        cl::TypeExpr::Union(items) => {
            mdb::TypeExpr::Union(items.iter().map(lower_type_expr).collect())
        }
        cl::TypeExpr::Vector {
            coord,
            dimension,
            not_null,
        } => mdb::TypeExpr::Vector {
            coord: coord.map(|c| c.canonical_name().to_string()),
            dimension: *dimension,
            not_null: *not_null,
        },
    }
}

pub(super) fn bin_op(op: &str) -> Result<mdb::BinaryOp> {
    Ok(match op {
        "OR" => mdb::BinaryOp::Or,
        "XOR" => mdb::BinaryOp::Xor,
        "AND" => mdb::BinaryOp::And,
        "+" => mdb::BinaryOp::Add,
        "-" => mdb::BinaryOp::Sub,
        "*" => mdb::BinaryOp::Mul,
        "/" => mdb::BinaryOp::Div,
        "%" => mdb::BinaryOp::Mod,
        "^" => mdb::BinaryOp::Pow,
        "||" => mdb::BinaryOp::Concat,
        _ => return Err(nyi(&format!("binary op {op}"))),
    })
}

pub(super) fn cmp_op(op: &str) -> Result<mdb::BinaryOp> {
    Ok(match op {
        "=" => mdb::BinaryOp::Eq,
        "<>" => mdb::BinaryOp::Neq,
        "<" => mdb::BinaryOp::Lt,
        ">" => mdb::BinaryOp::Gt,
        "<=" => mdb::BinaryOp::Lte,
        ">=" => mdb::BinaryOp::Gte,
        "=~" => mdb::BinaryOp::Regex,
        _ => return Err(nyi(&format!("comparison {op}"))),
    })
}

pub(super) fn unary_op(op: &str) -> Result<mdb::UnaryOp> {
    Ok(match op {
        "NOT" => mdb::UnaryOp::Not,
        "+" => mdb::UnaryOp::Plus,
        "-" => mdb::UnaryOp::Minus,
        _ => return Err(nyi(&format!("unary op {op}"))),
    })
}
