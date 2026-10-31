//! CIP-224/225/278 SEARCH MATCH execution: the per-row source
//! specification machinery -- one index search's candidate window, or a
//! COMBINE's weighted reciprocal rank fusion over two or more (possibly
//! nested) sources -- and the `search_match_clause` entry the executor's
//! dispatch calls. The vector-similarity scoring kernels and the bounded
//! top-k window live in [`super::vector`]; this module owns the SEARCH
//! clause semantics layered over them.

use std::collections::{BTreeMap, HashMap};

use cypherlite_core::{Error, PropertyMap, Result, Value};
use cypherlite_ir::{
    Expr, MatchMode, PathPattern, PatternElement, SearchClause, SearchCombineSpec, SearchIndexSpec,
    SearchKind, SearchSpec,
};
use cypherlite_model::EntityKind;

use crate::ctx::ExecCtx;
use crate::eval::{eval, value_type_name, Row};
use crate::overlay::ExecGraph;

use super::match_clause::{bind_pattern_nulls, match_clause};
use super::vector::{
    materialize_candidate, ranked_better_cmp, vector_index_candidates, vector_operand, TopWindow,
    VectorSearchFilter,
};

/// Evaluate a SEARCH window expression (`LIMIT` or `SKIP`/`OFFSET`),
/// requiring a non-negative INTEGER within `i32` range.
fn eval_window_int(
    graph: &ExecGraph<'_>,
    input: &Row,
    expr: &Expr,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    clause: &str,
) -> Result<usize> {
    match eval(graph, input, expr, params, cypher_version)? {
        Value::Int(value) if (0..=i32::MAX as i64).contains(&value) => Ok(value as usize),
        _ => Err(Error::syntax(format!(
            "NumberOutOfRange: SEARCH {clause} must be an INTEGER NOT NULL"
        ))),
    }
}

/// CIP-225 fulltext candidate generation: scan the named FULLTEXT index's
/// entities, tokenize the indexed string properties, and score each entity
/// by the query terms it contains (OR semantics, case-insensitive; the
/// score is the total occurrence count of the query terms). Sorted by score
/// desc, then element id; then the CIP-225 offset window and LIMIT.
///
/// WHERE is rejected for FULLTEXT SEARCH by the front-end validator (Neo4j
/// error 42N14), so there is no inner filter here.
fn fulltext_index_candidates(
    ctx: ExecCtx<'_>,
    index_name: &str,
    query: &str,
    offset: usize,
    limit: usize,
) -> Result<(EntityKind, Vec<(Value, f64)>)> {
    let graph = ctx.graph;
    let theater = graph.show_schema()?;
    let detail = theater
        .data
        .index_details
        .get(index_name)
        .cloned()
        .ok_or_else(|| Error::execution(format!("IndexDoesNotExist: {index_name}")))?;
    drop(theater);
    if detail.kind != crate::redo::IndexKindSpec::Fulltext {
        return Err(Error::execution(format!("WrongIndexType: {index_name}")));
    }
    let terms = tokenize_text(query);
    let scan = |properties: &PropertyMap| -> Option<f64> {
        let mut score = 0.0f64;
        for property in &detail.properties {
            let Some(Value::String(text)) = properties.get(property) else {
                continue;
            };
            let tokens = tokenize_text(text);
            for term in &terms {
                score += tokens
                    .iter()
                    .filter(|t| t.as_str() == term.as_str())
                    .count() as f64;
            }
        }
        (score > 0.0).then_some(score)
    };
    let kind = EntityKind::from_relationship_flag(detail.relationship);
    let mut top = TopWindow::new(offset, limit);
    if kind.is_relationship() {
        for id in graph.all_rel_ids() {
            let Some(rel_type) = graph.rel_type(id) else {
                continue;
            };
            if !detail.labels.iter().any(|label| label == &rel_type) {
                continue;
            }
            let Some(rel) = graph.rel(id) else { continue };
            let Some(score) = scan(&rel.properties) else {
                continue;
            };
            top.push(score, id);
        }
    } else {
        for id in graph.all_node_ids() {
            let Some(node) = graph.node(id) else { continue };
            if !detail
                .labels
                .iter()
                .any(|label| node.labels.iter().any(|actual| actual == label))
            {
                continue;
            }
            let Some(score) = scan(&node.properties) else {
                continue;
            };
            top.push(score, node.id);
        }
    }
    let mut candidates = Vec::new();
    for (id, score) in top.into_sorted_rows() {
        candidates.push((materialize_candidate(graph, kind, id), score));
    }
    Ok((kind, candidates))
}

/// Tokenize text for fulltext scoring: lowercase, split on any run of
/// non-alphanumeric characters (a stand-in for the standard analyzer; the
/// CIP's configurable analyzers are accepted syntactically but not
/// differentiated).
fn tokenize_text(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// One source specification's ranked candidate list: the entity kind of
/// the source's index(es), plus the materialized `(entity, score)` rows,
/// best-first. `None`: the source's query evaluated absent.
type SpecCandidates = Option<(EntityKind, Vec<(Value, f64)>)>;

/// The OPTIONAL rule's null row for one input row: the pattern's bound
/// variables nulled, the score column (if named) nulled, pushed once --
/// the one shape both OPTIONAL sites in `search_match_clause` emit.
fn push_optional_null_row(
    mut row: Row,
    patterns: &[PathPattern],
    score_var: &Option<String>,
    out: &mut Vec<Row>,
) {
    bind_pattern_nulls(&mut row, patterns);
    if let Some(score) = score_var {
        row.insert(score.clone(), Value::Null);
    }
    out.push(row);
}

/// The candidates one CIP-278 `<source specification clause>` yields for
/// one incoming row of the driving table, ranked best-first
/// ([`ranked_better_cmp`]'s order: score descending, entity id ascending).
/// `None`: the source's query evaluated absent (a NULL or non-list VECTOR
/// query), so the source contributes no rows -- the caller applies the
/// OPTIONAL rule, exactly as the pre-CIP-278 top-level index path did.
fn search_spec_candidates(
    ctx: ExecCtx<'_>,
    input: &Row,
    spec: &SearchSpec,
) -> Result<SpecCandidates> {
    match spec {
        SearchSpec::Index(index) => search_index_candidates(ctx, input, index),
        SearchSpec::Combine(combine) => search_combine_candidates(ctx, input, combine),
    }
}

/// One CIP-225 `<index specification clause>`'s candidates for one
/// incoming row -- the pre-CIP-278 top-level index behavior, unchanged,
/// now also serving as a weighted source inside a COMBINE.
fn search_index_candidates(
    ctx: ExecCtx<'_>,
    input: &Row,
    index: &SearchIndexSpec,
) -> Result<SpecCandidates> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let limit = eval_window_int(
        graph,
        input,
        &index.limit_expr,
        params,
        cypher_version,
        "LIMIT",
    )?;
    // CIP-225 `<offset clause>`, shared by both kinds.
    let offset = match &index.offset_expr {
        Some(expr) => eval_window_int(
            graph,
            input,
            expr.as_ref(),
            params,
            cypher_version,
            "SKIP/OFFSET",
        )?,
        None => 0,
    };
    // The FOR expression is the query for both kinds — evaluated once,
    // each arm only interprets the resulting value.
    let query = eval(graph, input, &index.for_expr, params, cypher_version)?;
    match index.kind {
        SearchKind::Vector => {
            let Some(query) = vector_operand(&query)? else {
                return Ok(None);
            };
            Ok(Some(vector_index_candidates(
                ctx,
                &index.index_name,
                &query,
                offset,
                limit,
                VectorSearchFilter {
                    filters: &index.filters,
                    input,
                },
            )?))
        }
        SearchKind::Fulltext => {
            // CIP-225: the fulltext query is a STRING (analyzer accepted,
            // not differentiated).
            let Value::String(query) = query else {
                return Err(Error::ty(
                    "InvalidValueType: fulltext SEARCH requires a STRING query",
                ));
            };
            Ok(Some(fulltext_index_candidates(
                ctx,
                &index.index_name,
                &query,
                offset,
                limit,
            )?))
        }
    }
}

/// One CIP-278 `<combined specification clause>`'s fused candidates for
/// one incoming row: weighted reciprocal rank fusion (WRRF) over the
/// ranked lists of its weighted sources (which may themselves be nested
/// combined specifications, Example P3). Each source's own list is ranked
/// after its own SKIP/OFFSET/LIMIT window; a source's rank-`i` entry
/// (0-based) contributes `weight / (constant + i + 1)` (the CIP's
/// `weight / (rrfConstant + rankIndex + 1)`), and an entity missing from a
/// source contributes nothing from that source. The fused list is ordered
/// by the fused score (the CIP's documented tie-breaker was removed --
/// implementation-dependent, CLG-102; this engine ties on the entity id,
/// ascending, via [`ranked_better_cmp`]) and windowed by the combined
/// specification's own optional LIMIT (Rule 7). `None`: every source's
/// query evaluated absent.
fn search_combine_candidates(
    ctx: ExecCtx<'_>,
    input: &Row,
    combine: &SearchCombineSpec,
) -> Result<SpecCandidates> {
    // Rules 3/4: the dampening constant, default 60.
    let constant = match &combine.constant_expr {
        Some(expr) => eval_wrrf_number(ctx, input, expr, "CONSTANT")?,
        None => 60.0,
    };
    let mut kind: Option<EntityKind> = None;
    // The fused scores keyed by entity id, so a later source adds its
    // contribution to the same entity instead of duplicating it; the
    // final order comes from the total sort below.
    let mut fused: HashMap<i64, (f64, Value)> = HashMap::new();
    for source in &combine.sources {
        // An absent source query contributes no ranks -- the CIP's
        // "missing from a source" rule (zero contribution).
        let Some((source_kind, candidates)) = search_spec_candidates(ctx, input, &source.spec)?
        else {
            continue;
        };
        // "Compatibility of indexes"/Example O1.1: every source must index
        // the same entity kind as the first.
        match kind {
            Some(kind) => {
                if kind != source_kind {
                    return Err(Error::ty(format!(
                        "InvalidValueType: Expected the value to be of type {}, but was of type {}",
                        kind.display_name(),
                        source_kind.display_name(),
                    )));
                }
            }
            None => kind = Some(source_kind),
        }
        // Rules 1/2: the source weight, default 1 (not normalized).
        let weight = match &source.weight_expr {
            Some(expr) => eval_wrrf_number(ctx, input, expr, "WEIGHT")?,
            None => 1.0,
        };
        for (rank, (candidate, _score)) in candidates.into_iter().enumerate() {
            let id = candidate_entity_id(&candidate)?;
            let contribution = weight / (constant + rank as f64 + 1.0);
            fused
                .entry(id)
                .and_modify(|(score, _)| *score += contribution)
                .or_insert((contribution, candidate));
        }
    }
    let Some(kind) = kind else {
        return Ok(None);
    };
    // The accumulate phase ends and the rank/window phase begins —
    // the binding name says so.
    let mut ranked: Vec<(i64, f64, Value)> = fused
        .into_iter()
        .map(|(id, (score, candidate))| (id, score, candidate))
        .collect();
    ranked.sort_by(|a, b| ranked_better_cmp(b.1, b.0, a.1, a.0));
    // Rule 7: the combined specification's own optional LIMIT keeps the n
    // highest-scoring fused results.
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let limit = match &combine.limit_expr {
        Some(expr) => eval_window_int(graph, input, expr, params, cypher_version, "LIMIT")?,
        None => usize::MAX,
    };
    let candidates = ranked
        .into_iter()
        .take(limit)
        .map(|(_id, score, candidate)| (candidate, score))
        .collect();
    Ok(Some((kind, candidates)))
}

/// Evaluates a CIP-278 Rule 1/Rule 3 control expression (a WEIGHT or the
/// WRRF WITH CONSTANT): INTEGER NOT NULL or FLOAT NOT NULL, at least 0 and
/// at most Java's `Integer.MAX_VALUE` (2147483647). Any other value (NULL
/// included) is the CIP's invalid-type error.
fn eval_wrrf_number(ctx: ExecCtx<'_>, input: &Row, expr: &Expr, clause: &str) -> Result<f64> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let value = eval(graph, input, expr, params, cypher_version)?;
    let number = match &value {
        Value::Int(number) => *number as f64,
        Value::Float(number) => *number,
        _ => {
            return Err(Error::ty(format!(
                "InvalidValueType: Expected the value to be of type INTEGER NOT NULL or \
                 FLOAT NOT NULL, but was of type {} (SEARCH {clause})",
                value_type_name(&value),
            )))
        }
    };
    if !(0.0..=f64::from(i32::MAX)).contains(&number) {
        return Err(Error::syntax(format!(
            "NumberOutOfRange: SEARCH {clause} must be between 0 and 2147483647"
        )));
    }
    Ok(number)
}

/// The entity id of one search candidate -- the WRRF fusion key. A
/// non-entity candidate is the same catalog-drift contract the candidate
/// seek already carries, not a silently skipped entry.
fn candidate_entity_id(candidate: &Value) -> Result<i64> {
    match candidate {
        Value::Node(node) => Ok(node.id),
        Value::Relationship(rel) => Ok(rel.id),
        _ => Err(Error::execution(
            "InvalidValueType: SEARCH candidate is not a node or relationship",
        )),
    }
}

pub(crate) fn search_match_clause(
    ctx: ExecCtx<'_>,
    rows: Vec<Row>,
    optional: bool,
    match_mode: MatchMode,
    patterns: &[PathPattern],
    where_clause: Option<&Expr>,
    search: &SearchClause,
) -> Result<Vec<Row>> {
    // The binding's own typed kind (stellar round 2, std-2): compare the
    // enum directly, no bool projection of either side. A pure function of
    // the pattern and the binding name -- loop-invariant, so it is scanned
    // once, not per input row.
    let binding_kind = patterns
        .iter()
        .flat_map(|path| &path.elements)
        .filter_map(|element| match element {
            PatternElement::Node(node)
                if node.variable.as_deref() == Some(search.binding_var.as_str()) =>
            {
                Some(EntityKind::Node)
            }
            PatternElement::Relationship(rel)
                if rel.variable.as_deref() == Some(search.binding_var.as_str()) =>
            {
                Some(EntityKind::Relationship)
            }
            _ => None,
        })
        .next();
    let mut out = Vec::new();
    for input in rows {
        // The CIP-278 source specification's candidate list -- one index
        // search or a COMBINE's fused list, ranked best-first. `None`: the
        // source's query evaluated absent, so it contributes no rows and
        // the single OPTIONAL rule below covers the input row (exactly the
        // pre-CIP-278 null-vector-query behavior).
        let mut produced = 0;
        if let Some((index_kind, candidates)) = search_spec_candidates(ctx, &input, &search.spec)? {
            if binding_kind != Some(index_kind) {
                return Err(Error::ty(
                    "InvalidValueType: SEARCH binding entity does not match index",
                ));
            }
            for (candidate, score) in candidates {
                // When the binding variable is already bound on the input row
                // (e.g. a hybrid query composing two SEARCH clauses through
                // WITH), a candidate must match that binding; a different
                // candidate is dropped rather than overwriting it. Verified on
                // Neo4j 2026.09.0: `MATCH (p) SEARCH p IN (VECTOR ...) WITH p
                // MATCH (p) SEARCH p IN (FULLTEXT ...)` intersects the two
                // result sets.
                if let Some(existing) = input.get(&search.binding_var) {
                    if !existing.is_null() && !existing.eq_non_null(&candidate) {
                        continue;
                    }
                }
                let mut seeded = input.clone();
                seeded.insert(search.binding_var.clone(), candidate);
                if let Some(score_var) = &search.score_var {
                    seeded.insert(score_var.clone(), Value::Float(score));
                }
                let matched = match_clause(
                    ctx,
                    vec![seeded],
                    false,
                    match_mode,
                    patterns,
                    where_clause,
                    None,
                    None,
                )?;
                produced += matched.len();
                out.extend(matched);
            }
        }
        // The OPTIONAL null-row rule, stated once (stellar round 3): no
        // source match and no produced row both mean the input row rides
        // through with the score null.
        if produced == 0 && optional {
            push_optional_null_row(input, patterns, &search.score_var, &mut out);
        }
    }
    Ok(out)
}
