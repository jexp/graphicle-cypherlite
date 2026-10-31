//! CIP-100 canonical type normalization and name rendering.
//!
//! `CanonType` is the single normalized form of a Cypher type: default
//! names (CIP-100 "Type name normalization"), CIP-100's semantic
//! simplifications (flatten nested unions, drop redundant/subtype members,
//! combine NULL with NOT NULL types), and the canonical type order all
//! already applied. Two semantically equivalent types always normalize to
//! structurally equal `CanonType` trees.
//!
//! This is the shared engine behind two features:
//! - CIP-90's `valueType()`, which renders a *runtime value's* dynamic
//!   type (built bottom-up from the value's own shape by the engine, see
//!   `cypherlite-executor`'s `value_type_name`).
//! - CIP-100's own normalization of a *parsed* `TypeExpr` (property type
//!   constraints, exposed via `SHOW CONSTRAINTS.propertyType`).
//!
//! Note: `TypeExpr` (CIP-86 grammar), the type predicate evaluator, and
//! this module's `type_expr_to_canon` all live in the parser crate since
//! normalization is purely a function of the parsed type grammar; nothing
//! here depends on `CypherValue`.

use std::cmp::Ordering;

use crate::ast::{PredefinedKind, TypeExpr, VectorCoordKind};

/// A fully CIP-100-normalized Cypher type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonType {
    pub kind: CanonKind,
    /// Whether this node is marked NOT NULL. Always `false` for `Nothing`
    /// (normalized away, see `CanonType::predefined`) and for `Union` (the
    /// CIP-86 grammar gives closed unions no `NOT NULL` production of
    /// their own -- nullability is carried per-member instead).
    pub not_null: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonKind {
    Predefined(PredefinedKind),
    List(Box<CanonType>),
    /// `VECTOR<COORD NOT NULL>(DIM)` (CIP-216). Either half may be absent
    /// (`coord: None` renders as bare `VECTOR`, `dimension: None` omits the
    /// `(...)` suffix) -- both are grammatically legal `TypeExpr::Vector`
    /// shapes, kept around only so `validate_property_type` can render
    /// them back into Neo4j's own error text for the "need to define both"
    /// rejection.
    Vector {
        coord: Option<VectorCoordKind>,
        dimension: Option<i64>,
    },
    /// Two or more component types, already flattened, deduped, subtype-
    /// collapsed, and ordered -- the result of `normalize_union`.
    Union(Vec<CanonType>),
}

/// CIP-100 "Default type names" table, vector coordinate half: the
/// canonical coordinate name rendered inside `VECTOR<...>`. `Int64`/
/// `Float64` render as the plain `INTEGER`/`FLOAT` names (CIP-216 S-6: they
/// double as ordinary property type aliases), not `INTEGER64`/`FLOAT64`.
fn vector_coord_name(k: VectorCoordKind) -> &'static str {
    k.canonical_name()
}

/// Reverse of `vector_coord_name`.
fn vector_coord_kind_from_name(name: &str) -> Option<VectorCoordKind> {
    VectorCoordKind::from_canonical_name(name)
}

impl CanonType {
    /// Builds a predefined leaf, applying the "Simplify NULL NOT NULL" /
    /// "Simplify NOTHING NOT NULL" rules (both normalize to plain
    /// `NOTHING`) up front so every other rule can assume `not_null` is
    /// always `false` on `Nothing`.
    pub fn predefined(kind: PredefinedKind, not_null: bool) -> Self {
        if matches!(kind, PredefinedKind::Nothing)
            || (matches!(kind, PredefinedKind::Null) && not_null)
        {
            return CanonType {
                kind: CanonKind::Predefined(PredefinedKind::Nothing),
                not_null: false,
            };
        }
        CanonType {
            kind: CanonKind::Predefined(kind),
            not_null,
        }
    }

    pub fn list(element: CanonType, not_null: bool) -> Self {
        CanonType {
            kind: CanonKind::List(Box::new(element)),
            not_null,
        }
    }
}

/// CIP-100 "Default type names" table.
fn predefined_name(kind: PredefinedKind) -> &'static str {
    match kind {
        PredefinedKind::Nothing => "NOTHING",
        PredefinedKind::Null => "NULL",
        PredefinedKind::Any => "ANY",
        PredefinedKind::Boolean => "BOOLEAN",
        PredefinedKind::String => "STRING",
        PredefinedKind::Integer => "INTEGER",
        PredefinedKind::Float => "FLOAT",
        PredefinedKind::Date => "DATE",
        PredefinedKind::Duration => "DURATION",
        PredefinedKind::Point => "POINT",
        PredefinedKind::LocalTime => "LOCAL TIME",
        PredefinedKind::ZonedTime => "ZONED TIME",
        PredefinedKind::LocalDateTime => "LOCAL DATETIME",
        PredefinedKind::ZonedDateTime => "ZONED DATETIME",
        PredefinedKind::Node => "NODE",
        PredefinedKind::Relationship => "RELATIONSHIP",
        PredefinedKind::Path => "PATH",
        PredefinedKind::Map => "MAP",
        PredefinedKind::Uuid => "UUID",
    }
}

/// CIP-100 "General order of types" rank; lower sorts first. `List`'s rank
/// sits between `MAP` and `PATH` per the doc's constructed-types list;
/// `Vector` (CIP-216, not in the original CIP-100 table) sits directly
/// before `List` -- the only order relationship pinned down by observed
/// live-Neo4j behavior (CIP-216 01#5: `VECTOR<...>(...)` sorts before
/// `LIST<...>` in a union). `Union` is never itself ranked in practice
/// (nested unions are always flattened away, so a bare `Union` never
/// appears as a sibling of anything else needing a rank).
fn rank(kind: &CanonKind) -> u8 {
    match kind {
        CanonKind::Predefined(k) => match k {
            PredefinedKind::Nothing => 0,
            PredefinedKind::Null => 1,
            // CIP-246 amendment to the CIP-100 order: UUID sits between
            // STRING and the numerics.
            PredefinedKind::Boolean => 2,
            PredefinedKind::String => 3,
            PredefinedKind::Uuid => 4,
            PredefinedKind::Integer => 5,
            PredefinedKind::Float => 6,
            PredefinedKind::Date => 7,
            PredefinedKind::LocalTime => 8,
            PredefinedKind::ZonedTime => 9,
            PredefinedKind::LocalDateTime => 10,
            PredefinedKind::ZonedDateTime => 11,
            PredefinedKind::Duration => 12,
            PredefinedKind::Point => 13,
            PredefinedKind::Node => 14,
            PredefinedKind::Relationship => 15,
            PredefinedKind::Map => 16,
            PredefinedKind::Path => 19,
            PredefinedKind::Any => 21,
        },
        CanonKind::Vector { .. } => 17,
        CanonKind::List(_) => 18,
        CanonKind::Union(_) => 20,
    }
}

/// Renders a `CanonType` to its CIP-100 canonical string: uppercase,
/// single-space word separators, no space around `<>`, `|` surrounded by
/// exactly one space on each side.
pub fn render_canon(t: &CanonType) -> String {
    let base = match &t.kind {
        CanonKind::Predefined(k) => predefined_name(*k).to_string(),
        CanonKind::List(elem) => format!("LIST<{}>", render_canon(elem)),
        CanonKind::Vector { coord, dimension } => {
            let mut s = String::from("VECTOR");
            if let Some(c) = coord {
                s.push('<');
                s.push_str(vector_coord_name(*c));
                s.push_str(" NOT NULL>");
            }
            if let Some(d) = dimension {
                s.push('(');
                s.push_str(&d.to_string());
                s.push(')');
            }
            s
        }
        CanonKind::Union(members) => members
            .iter()
            .map(render_canon)
            .collect::<Vec<_>>()
            .join(" | "),
    };
    if t.not_null && !matches!(t.kind, CanonKind::Predefined(PredefinedKind::Nothing)) {
        format!("{base} NOT NULL")
    } else {
        base
    }
}

/// True if every value of `a` is also a value of `b` (CIP-100 "Simplify
/// encompassing types"). Used to drop redundant members from a dynamic
/// union: `NOTHING` is a subtype of everything (including `NULL`), `NULL`
/// is a subtype of every other nullable type, `ANY` is a supertype of
/// everything, `T NOT NULL` is a subtype of the nullable `T`, and
/// `LIST<T1>` is a subtype of `LIST<T2>` when `T1` is a subtype of `T2`.
fn is_subtype(a: &CanonType, b: &CanonType) -> bool {
    if a == b {
        return true;
    }
    if matches!(b.kind, CanonKind::Predefined(PredefinedKind::Any)) {
        return true;
    }
    if matches!(a.kind, CanonKind::Predefined(PredefinedKind::Nothing)) {
        return true;
    }
    if let CanonKind::Union(ma) = &a.kind {
        return ma.iter().all(|m| is_subtype(m, b));
    }
    if let CanonKind::Union(mb) = &b.kind {
        return mb.iter().any(|m| is_subtype(a, m));
    }
    if matches!(a.kind, CanonKind::Predefined(PredefinedKind::Null)) {
        return matches!(b.kind, CanonKind::Predefined(PredefinedKind::Null))
            || (!b.not_null && !matches!(b.kind, CanonKind::Predefined(PredefinedKind::Nothing)));
    }
    match (&a.kind, &b.kind) {
        (CanonKind::Predefined(ka), CanonKind::Predefined(kb)) => {
            ka == kb && (a.not_null || !b.not_null)
        }
        (CanonKind::List(ea), CanonKind::List(eb)) => {
            (a.not_null || !b.not_null) && is_subtype(ea, eb)
        }
        _ => false,
    }
}

/// Whether a member's presence in a union should trigger the "if the
/// dynamic union contains at least one nullable type, then all types are
/// nullable" rule. `NOTHING` never triggers it -- it isn't "nullable" in
/// the sense of containing the null value, it contains no values at all.
fn is_nullable_trigger(t: &CanonType) -> bool {
    !t.not_null && !matches!(t.kind, CanonKind::Predefined(PredefinedKind::Nothing))
}

/// Removes exact duplicates and members that are a strict subtype of a
/// different member still present (CIP-100 "Remove redundant types from
/// dynamic unions" + "Simplify encompassing types").
fn dedup_subtypes(items: Vec<CanonType>) -> Vec<CanonType> {
    let mut result = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let redundant = items.iter().enumerate().any(|(j, other)| {
            if i == j {
                return false;
            }
            if item == other {
                // Exact duplicate: keep only the first occurrence.
                return i > j;
            }
            is_subtype(item, other)
        });
        if !redundant {
            result.push(item.clone());
        }
    }
    result
}

/// Cardinality used by the CIP-100 tie-break algorithm: a closed dynamic
/// union's own component count, or 1 for anything else (a single type is
/// a "union of one").
fn cardinality(t: &CanonType) -> usize {
    match &t.kind {
        CanonKind::Union(members) => members.len(),
        _ => 1,
    }
}

fn as_members(t: &CanonType) -> Vec<&CanonType> {
    match &t.kind {
        CanonKind::Union(members) => members.iter().collect(),
        _ => vec![t],
    }
}

/// CIP-100 total order over normalized types: general type rank first;
/// for two lists, the closed-dynamic-union tie-break algorithm (fewer
/// inner types first, then lexicographic by first differing inner type);
/// NOT NULL ordered directly before the equivalent nullable type.
fn cmp_canon(a: &CanonType, b: &CanonType) -> Ordering {
    let ra = rank(&a.kind);
    let rb = rank(&b.kind);
    if ra != rb {
        return ra.cmp(&rb);
    }
    match (&a.kind, &b.kind) {
        (CanonKind::List(ea), CanonKind::List(eb)) => {
            let ca = cardinality(ea);
            let cb = cardinality(eb);
            if ca != cb {
                return ca.cmp(&cb);
            }
            let (ma, mb) = (as_members(ea), as_members(eb));
            for (x, y) in ma.iter().zip(mb.iter()) {
                let ord = cmp_canon(x, y);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            // NOT NULL before nullable, at the list level itself.
            b.not_null.cmp(&a.not_null)
        }
        (CanonKind::Predefined(_), CanonKind::Predefined(_)) => b.not_null.cmp(&a.not_null),
        _ => Ordering::Equal,
    }
}

/// Normalizes a set of already-normalized component types into their
/// single CIP-100 normalized form: flattens nested unions, removes
/// duplicate/subtype members, applies the "combine NULL and NOT NULL
/// types" encompassing rule (a union with at least one nullable member
/// makes every member nullable), and orders what remains. Returns the
/// single member directly if only one remains once simplified (CIP-100
/// "Simplify dynamic unions with one type"), or `NOTHING` for an empty
/// input.
pub fn normalize_union(components: Vec<CanonType>) -> CanonType {
    let mut flat = Vec::with_capacity(components.len());
    for c in components {
        match c.kind {
            CanonKind::Union(members) => flat.extend(members),
            _ => flat.push(c),
        }
    }

    flat = dedup_subtypes(flat);

    if flat.iter().any(is_nullable_trigger) {
        for m in &mut flat {
            if !matches!(m.kind, CanonKind::Predefined(PredefinedKind::Nothing)) {
                m.not_null = false;
            }
        }
        flat = dedup_subtypes(flat);
    }

    flat.sort_by(cmp_canon);

    match flat.len() {
        0 => CanonType::predefined(PredefinedKind::Nothing, false),
        1 => flat.into_iter().next().unwrap(),
        _ => CanonType {
            kind: CanonKind::Union(flat),
            not_null: false,
        },
    }
}

/// Converts a parsed `TypeExpr` (CIP-86 grammar) to its CIP-100 canonical
/// form.
pub fn type_expr_to_canon(te: &TypeExpr) -> CanonType {
    match te {
        TypeExpr::Predefined { kind, not_null } => CanonType::predefined(*kind, *not_null),
        TypeExpr::List { element, not_null } => {
            CanonType::list(type_expr_to_canon(element), *not_null)
        }
        TypeExpr::PropertyValue { not_null } => property_value_canon(*not_null),
        TypeExpr::Vector {
            coord,
            dimension,
            not_null,
        } => CanonType {
            kind: CanonKind::Vector {
                coord: *coord,
                dimension: *dimension,
            },
            not_null: *not_null,
        },
        TypeExpr::Union(components) => {
            let converted: Vec<CanonType> = components.iter().map(type_expr_to_canon).collect();
            normalize_union(converted)
        }
    }
}

/// CIP-100 "Simplify ANY PROPERTY VALUE to the union of all property
/// types": the 11 property-storable scalar/temporal types plus a
/// `LIST<T NOT NULL>` of each (list elements are always NOT NULL --
/// property-storable lists can never contain a null element).
fn property_value_canon(not_null: bool) -> CanonType {
    const SCALARS: [PredefinedKind; 11] = [
        PredefinedKind::Boolean,
        PredefinedKind::String,
        PredefinedKind::Integer,
        PredefinedKind::Float,
        PredefinedKind::Date,
        PredefinedKind::LocalTime,
        PredefinedKind::ZonedTime,
        PredefinedKind::LocalDateTime,
        PredefinedKind::ZonedDateTime,
        PredefinedKind::Duration,
        PredefinedKind::Point,
    ];
    let mut members: Vec<CanonType> = SCALARS
        .iter()
        .map(|k| CanonType::predefined(*k, not_null))
        .collect();
    for k in SCALARS {
        members.push(CanonType::list(CanonType::predefined(k, true), not_null));
    }
    normalize_union(members)
}

/// Whether `k` is one of the composite/unbounded kinds CIP-85 disallows as
/// a property type (or list inner element type): `NOTHING`/`NULL`/`ANY`/
/// `NODE`/`RELATIONSHIP`/`PATH`/`MAP`. The 11 scalar/temporal property-
/// storable kinds (plus their `LIST<...>` forms) are the only ones left.
fn is_disallowed_property_kind(k: PredefinedKind) -> bool {
    matches!(
        k,
        PredefinedKind::Nothing
            | PredefinedKind::Null
            | PredefinedKind::Any
            | PredefinedKind::Node
            | PredefinedKind::Relationship
            | PredefinedKind::Path
            | PredefinedKind::Map
    )
}

/// Validates a `LIST<...>` inner element type (CIP-85: "lists cannot have
/// a union/list/nullable inner type", NOT NULL is mandatory on the
/// element). Errors are rendered against `list_canon` (the whole
/// `LIST<...>` type), matching Neo4j's error shape. `pub` (not just used by
/// `validate_property_type`) because CIP-178/181's graph-type property
/// types share this exact restriction while allowing top-level `NOT NULL`
/// (which `validate_property_type` itself forbids -- that rule is specific
/// to standalone property type *constraints*, not a node/edge type's own
/// declared properties).
pub fn validate_list_element(list_canon: &CanonType, elem: &CanonType) -> Result<(), String> {
    if matches!(elem.kind, CanonKind::Union(_)) {
        return Err(format!(
            "Invalid property type `{}`. Lists cannot have a union of types as an inner type.",
            render_canon(list_canon)
        ));
    }
    if matches!(elem.kind, CanonKind::List(_)) {
        return Err(format!(
            "Invalid property type `{}`. Lists cannot have a list as an inner type.",
            render_canon(list_canon)
        ));
    }
    // CIP-216 Negative 4: a list of vectors is not a supported property
    // type, regardless of the vector's own NOT NULL/coordinate/dimension.
    if matches!(elem.kind, CanonKind::Vector { .. }) {
        return Err(format!(
            "{} is not supported in property type constraints. A list in this position \
             cannot have VECTOR as an inner type.",
            render_canon(list_canon)
        ));
    }
    if !elem.not_null {
        return Err(format!(
            "Invalid property type `{}`. Lists cannot have a nullable inner type.",
            render_canon(list_canon)
        ));
    }
    if let CanonKind::Predefined(k) = &elem.kind {
        if is_disallowed_property_kind(*k) {
            return Err(format!(
                "{} is not supported in property type constraints.",
                render_canon(list_canon)
            ));
        }
    }
    Ok(())
}

/// Validates a normalized type against CypherLite's property type
/// constraint restrictions (CIP-85): no top-level `NOT NULL` (on a scalar
/// or on the list itself -- Neo4j 2026.05 does not implement `TYPE NOT
/// NULL` for property type constraints), no `NOTHING`/`NULL`/`ANY`/`NODE`/
/// `RELATIONSHIP`/`PATH`/`MAP`, and -- inside a `LIST<...>` -- no union,
/// no nested list, and no nullable inner type (NOT NULL is mandatory
/// there).
pub fn validate_property_type(canon: &CanonType) -> Result<(), String> {
    if canon.not_null {
        return Err(format!(
            "{} is not supported in property type constraints.",
            render_canon(canon)
        ));
    }
    match &canon.kind {
        CanonKind::Predefined(k) => {
            if is_disallowed_property_kind(*k) {
                Err(format!(
                    "{} is not supported in property type constraints.",
                    render_canon(canon)
                ))
            } else {
                Ok(())
            }
        }
        CanonKind::List(elem) => validate_list_element(canon, elem),
        // CIP-216 Negative 1/2: both coordinate type and dimension are
        // mandatory for a vector property type constraint; either omitted
        // makes the whole constraint creation fail (the type itself
        // parses fine -- this is a runtime rejection, not a syntax error).
        CanonKind::Vector { coord, dimension } => {
            if coord.is_none() || dimension.is_none() {
                Err(format!(
                    "{} is not supported in property type constraints. Property type \
                     constraints for vectors need to define both coordinate type and \
                     dimension.",
                    render_canon(canon)
                ))
            } else {
                Ok(())
            }
        }
        CanonKind::Union(members) => {
            for m in members {
                validate_property_type(m)?;
            }
            Ok(())
        }
    }
}

/// Reverse of `render_canon` for a property type constraint's stored
/// canonical string: reconstructs a `TypeExpr` a caller can feed to a
/// value-type matcher. Only understands the shape `render_canon` actually
/// produces for a `validate_property_type`-accepted type (predefined
/// names, `LIST<...>`, a trailing ` NOT NULL`, and `" | "`-separated
/// unions) -- not general Cypher type syntax. Used to re-derive a runtime
/// type matcher for a constraint loaded back from storage (CIP-85
/// enforcement), where only the rendered string persists. Returns `None`
/// if `s` doesn't match that shape (should not happen for a string this
/// module itself produced).
pub fn parse_canon_type_str(s: &str) -> Option<TypeExpr> {
    if s.contains(" | ") {
        let members = s
            .split(" | ")
            .map(parse_single_canon_type_str)
            .collect::<Option<Vec<_>>>()?;
        return Some(TypeExpr::Union(members));
    }
    parse_single_canon_type_str(s)
}

fn parse_single_canon_type_str(s: &str) -> Option<TypeExpr> {
    let (base, not_null) = match s.strip_suffix(" NOT NULL") {
        Some(b) => (b, true),
        None => (s, false),
    };
    if let Some(inner) = base.strip_prefix("LIST<").and_then(|x| x.strip_suffix('>')) {
        return Some(TypeExpr::List {
            element: Box::new(parse_single_canon_type_str(inner)?),
            not_null,
        });
    }
    if let Some(rest) = base.strip_prefix("VECTOR") {
        let (coord, dimension) = parse_vector_canon_tail(rest)?;
        return Some(TypeExpr::Vector {
            coord,
            dimension,
            not_null,
        });
    }
    Some(TypeExpr::Predefined {
        kind: predefined_kind_from_name(base)?,
        not_null,
    })
}

/// Parses the part of a rendered vector type string after the `VECTOR`
/// prefix (and after any outer ` NOT NULL` has already been stripped by
/// the caller): an optional `<COORD NOT NULL>` and an optional `(DIM)`,
/// in that order, and nothing else. Reverse of the `CanonKind::Vector`
/// branch of `render_canon`.
fn parse_vector_canon_tail(rest: &str) -> Option<(Option<VectorCoordKind>, Option<i64>)> {
    let mut s = rest;
    let mut coord = None;
    if let Some(after_lt) = s.strip_prefix('<') {
        let end = after_lt.find('>')?;
        let inner = after_lt[..end].strip_suffix(" NOT NULL")?;
        coord = Some(vector_coord_kind_from_name(inner)?);
        s = &after_lt[end + 1..];
    }
    let dimension = if let Some(after_paren) = s.strip_prefix('(') {
        let end = after_paren.find(')')?;
        let dim = after_paren[..end].parse::<i64>().ok()?;
        s = &after_paren[end + 1..];
        Some(dim)
    } else {
        None
    };
    if !s.is_empty() {
        return None;
    }
    Some((coord, dimension))
}

/// Reverse of `predefined_name`.
fn predefined_kind_from_name(name: &str) -> Option<PredefinedKind> {
    Some(match name {
        "NOTHING" => PredefinedKind::Nothing,
        "NULL" => PredefinedKind::Null,
        "ANY" => PredefinedKind::Any,
        "BOOLEAN" => PredefinedKind::Boolean,
        "STRING" => PredefinedKind::String,
        "INTEGER" => PredefinedKind::Integer,
        "FLOAT" => PredefinedKind::Float,
        "UUID" => PredefinedKind::Uuid,
        "DATE" => PredefinedKind::Date,
        "DURATION" => PredefinedKind::Duration,
        "POINT" => PredefinedKind::Point,
        "LOCAL TIME" => PredefinedKind::LocalTime,
        "ZONED TIME" => PredefinedKind::ZonedTime,
        "LOCAL DATETIME" => PredefinedKind::LocalDateTime,
        "ZONED DATETIME" => PredefinedKind::ZonedDateTime,
        "NODE" => PredefinedKind::Node,
        "RELATIONSHIP" => PredefinedKind::Relationship,
        "PATH" => PredefinedKind::Path,
        "MAP" => PredefinedKind::Map,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kind: PredefinedKind, not_null: bool) -> CanonType {
        CanonType::predefined(kind, not_null)
    }

    #[test]
    fn null_not_null_simplifies_to_nothing() {
        let c = p(PredefinedKind::Null, true);
        assert_eq!(render_canon(&c), "NOTHING");
    }

    #[test]
    fn nothing_not_null_simplifies_to_nothing() {
        let c = p(PredefinedKind::Nothing, true);
        assert_eq!(render_canon(&c), "NOTHING");
    }

    #[test]
    fn union_orders_by_general_type_order() {
        let c = normalize_union(vec![
            p(PredefinedKind::Float, true),
            p(PredefinedKind::Boolean, true),
            p(PredefinedKind::Integer, true),
        ]);
        assert_eq!(
            render_canon(&c),
            "BOOLEAN NOT NULL | INTEGER NOT NULL | FLOAT NOT NULL"
        );
    }

    #[test]
    fn duplicate_and_subtype_members_collapse() {
        let c = normalize_union(vec![
            p(PredefinedKind::Boolean, true),
            p(PredefinedKind::Boolean, true),
        ]);
        assert_eq!(render_canon(&c), "BOOLEAN NOT NULL");
    }

    #[test]
    fn single_type_union_unwraps() {
        let c = normalize_union(vec![p(PredefinedKind::Boolean, true)]);
        assert_eq!(render_canon(&c), "BOOLEAN NOT NULL");
    }

    #[test]
    fn combine_null_and_not_null_makes_all_nullable() {
        let c = normalize_union(vec![
            p(PredefinedKind::Boolean, true),
            p(PredefinedKind::Null, false),
        ]);
        assert_eq!(render_canon(&c), "BOOLEAN");
    }

    #[test]
    fn any_encompasses_everything() {
        let c = normalize_union(vec![
            p(PredefinedKind::Any, false),
            p(PredefinedKind::String, false),
        ]);
        assert_eq!(render_canon(&c), "ANY");
    }

    #[test]
    fn list_of_nothing_is_subtype_of_any_list() {
        let c = normalize_union(vec![
            CanonType::list(p(PredefinedKind::Nothing, false), true),
            CanonType::list(p(PredefinedKind::Integer, true), true),
        ]);
        assert_eq!(render_canon(&c), "LIST<INTEGER NOT NULL> NOT NULL");
    }

    #[test]
    fn list_tie_break_prefers_fewer_inner_types_then_lexicographic() {
        let list_a = CanonType::list(
            normalize_union(vec![
                p(PredefinedKind::Integer, true),
                p(PredefinedKind::Float, true),
            ]),
            true,
        );
        let list_b = CanonType::list(p(PredefinedKind::Boolean, true), true);
        let c = normalize_union(vec![list_a, list_b]);
        assert_eq!(
            render_canon(&c),
            "LIST<BOOLEAN NOT NULL> NOT NULL | LIST<INTEGER NOT NULL | FLOAT NOT NULL> NOT NULL"
        );
    }

    #[test]
    fn validate_rejects_top_level_not_null() {
        let c = p(PredefinedKind::Float, true);
        let err = validate_property_type(&c).unwrap_err();
        assert_eq!(
            err,
            "FLOAT NOT NULL is not supported in property type constraints."
        );
    }

    #[test]
    fn validate_rejects_union_inner_list_type() {
        let c = CanonType::list(
            normalize_union(vec![
                p(PredefinedKind::Boolean, true),
                p(PredefinedKind::Integer, true),
            ]),
            false,
        );
        let err = validate_property_type(&c).unwrap_err();
        assert!(err.starts_with("Invalid property type"));
    }

    #[test]
    fn validate_accepts_plain_scalar_and_list() {
        assert!(validate_property_type(&p(PredefinedKind::Date, false)).is_ok());
        let list = CanonType::list(p(PredefinedKind::Integer, true), false);
        assert!(validate_property_type(&list).is_ok());
    }

    #[test]
    fn validate_rejects_list_of_list() {
        let c = CanonType::list(
            CanonType::list(p(PredefinedKind::Integer, true), true),
            false,
        );
        let err = validate_property_type(&c).unwrap_err();
        assert!(err.starts_with("Invalid property type"));
    }

    #[test]
    fn validate_rejects_list_of_nullable_scalar() {
        let c = CanonType::list(p(PredefinedKind::Date, false), false);
        let err = validate_property_type(&c).unwrap_err();
        assert!(err.starts_with("Invalid property type"));
    }

    #[test]
    fn validate_rejects_list_of_unsupported_kind() {
        let c = CanonType::list(p(PredefinedKind::Map, true), false);
        let err = validate_property_type(&c).unwrap_err();
        assert_eq!(
            err,
            "LIST<MAP NOT NULL> is not supported in property type constraints."
        );
    }

    #[test]
    fn parse_canon_type_str_round_trips_render_canon() {
        for canon in [
            p(PredefinedKind::Integer, false),
            CanonType::list(p(PredefinedKind::String, true), false),
            normalize_union(vec![
                p(PredefinedKind::Boolean, false),
                p(PredefinedKind::String, false),
            ]),
        ] {
            let rendered = render_canon(&canon);
            let parsed = parse_canon_type_str(&rendered).expect("should parse");
            assert_eq!(render_canon(&type_expr_to_canon(&parsed)), rendered);
        }
    }

    #[test]
    fn parse_canon_type_str_rejects_garbage() {
        assert!(parse_canon_type_str("NOT A TYPE").is_none());
    }

    fn vector(coord: Option<VectorCoordKind>, dimension: Option<i64>, not_null: bool) -> CanonType {
        CanonType {
            kind: CanonKind::Vector { coord, dimension },
            not_null,
        }
    }

    #[test]
    fn vector_renders_coordinate_and_dimension() {
        let c = vector(Some(VectorCoordKind::Int32), Some(42), false);
        assert_eq!(render_canon(&c), "VECTOR<INTEGER32 NOT NULL>(42)");
    }

    #[test]
    fn vector_int64_and_float64_coordinates_render_as_plain_aliases() {
        // CIP-216 S-6: INT64/FLOAT64 double as ordinary plain-type aliases,
        // so the vector coordinate name renders as INTEGER/FLOAT, not
        // INTEGER64/FLOAT64.
        assert_eq!(
            render_canon(&vector(Some(VectorCoordKind::Int64), Some(3), false)),
            "VECTOR<INTEGER NOT NULL>(3)"
        );
        assert_eq!(
            render_canon(&vector(Some(VectorCoordKind::Float64), Some(4), false)),
            "VECTOR<FLOAT NOT NULL>(4)"
        );
    }

    #[test]
    fn vector_missing_coordinate_or_dimension_renders_partial_form() {
        assert_eq!(render_canon(&vector(None, Some(3), false)), "VECTOR(3)");
        assert_eq!(
            render_canon(&vector(Some(VectorCoordKind::Int32), None, false)),
            "VECTOR<INTEGER32 NOT NULL>"
        );
    }

    #[test]
    fn validate_rejects_vector_missing_coordinate_or_dimension() {
        assert!(validate_property_type(&vector(None, Some(3), false)).is_err());
        assert!(
            validate_property_type(&vector(Some(VectorCoordKind::Int32), None, false)).is_err()
        );
        assert!(
            validate_property_type(&vector(Some(VectorCoordKind::Int32), Some(3), false)).is_ok()
        );
    }

    #[test]
    fn validate_rejects_outer_not_null_on_vector() {
        let c = vector(Some(VectorCoordKind::Int32), Some(3), true);
        let err = validate_property_type(&c).unwrap_err();
        assert_eq!(
            err,
            "VECTOR<INTEGER32 NOT NULL>(3) NOT NULL is not supported in property type constraints."
        );
    }

    #[test]
    fn validate_rejects_list_of_vector() {
        let c = CanonType::list(vector(Some(VectorCoordKind::Int32), Some(42), true), false);
        let err = validate_property_type(&c).unwrap_err();
        assert!(err.contains("cannot have VECTOR as an inner type"));
    }

    #[test]
    fn vector_sorts_before_list_in_a_union() {
        // CIP-216 01#5 (observed live-Neo4j order): a vector branch sorts
        // before a list branch in a normalized union.
        let c = normalize_union(vec![
            CanonType::list(p(PredefinedKind::Integer, true), false),
            vector(Some(VectorCoordKind::Int64), Some(3), false),
        ]);
        assert_eq!(
            render_canon(&c),
            "VECTOR<INTEGER NOT NULL>(3) | LIST<INTEGER NOT NULL>"
        );
    }

    #[test]
    fn parse_canon_type_str_round_trips_vector() {
        for canon in [
            vector(Some(VectorCoordKind::Int8), Some(1), false),
            vector(Some(VectorCoordKind::Float32), Some(4096), false),
        ] {
            let rendered = render_canon(&canon);
            let parsed = parse_canon_type_str(&rendered).expect("should parse");
            assert_eq!(render_canon(&type_expr_to_canon(&parsed)), rendered);
        }
    }
}
