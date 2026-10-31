//! The canon-type algebra the graph-type semantic layer resolves with:
//! the subsumption relation (`canon_allows` -- CIP-266's restriction-only
//! re-specification check and CIP-262's default-value conformance share
//! it), the pointwise intersection (`canon_intersection` -- CIP-266's
//! diamond rule, CIP-262's mixed-literal list element type), and the
//! union-member normalization the intersection's union results keep. One
//! module, consulted by `inherit` and `default_value` alike: the lattice
//! is not an inheritance concern and not a default-value concern.

use cypherlite_parser::type_normalize::{CanonKind, CanonType};
use cypherlite_parser::PredefinedKind;

/// The subsumption relation over normalized types: whether every value
/// `inner` allows is also allowed by `outer`. Nullability only rejects a
/// nullable `inner` under a `NOT NULL` `outer`; `ANY` is the top; a union
/// `inner` needs every member covered; a union `outer` covers through any
/// member.
pub(crate) fn canon_allows(outer: &CanonType, inner: &CanonType) -> bool {
    if outer.not_null && !inner.not_null {
        return false;
    }
    match &inner.kind {
        CanonKind::Union(members) => members.iter().all(|m| canon_allows(outer, m)),
        _ => match &outer.kind {
            CanonKind::Predefined(PredefinedKind::Any) => true,
            CanonKind::Union(parts) => parts.iter().any(|part| canon_allows(part, inner)),
            CanonKind::Predefined(kind) => {
                matches!(&inner.kind, CanonKind::Predefined(inner_kind) if inner_kind == kind)
            }
            CanonKind::List(element) => match &inner.kind {
                CanonKind::List(inner_element) => canon_allows(element, inner_element),
                _ => false,
            },
            CanonKind::Vector { coord, dimension } => match &inner.kind {
                CanonKind::Vector {
                    coord: inner_coord,
                    dimension: inner_dimension,
                } => {
                    // A specified declared parameter constrains the inner
                    // value; an unspecified declared parameter is the
                    // wildcard (the rust review's Medium: differing vectors
                    // used to pass DDL validation and then fail every
                    // write at runtime).
                    coord.is_none_or(|c| inner_coord.is_some_and(|ic| ic == c))
                        && dimension.is_none_or(|d| inner_dimension.is_some_and(|id| id == d))
                }
                _ => false,
            },
        },
    }
}

/// The pointwise intersection of two normalized types (CIP-266 diamond
/// rule): `ANY` is the identity; a shared predefined kind or list/vector
/// element intersects recursively; everything else falls to `NULL` (the
/// CIP's not-instantiable marker, `NOTHING` once nullability combines).
pub(crate) fn canon_intersection(a: &CanonType, b: &CanonType) -> CanonType {
    let not_null = a.not_null || b.not_null;
    let kind = match (&a.kind, &b.kind) {
        (CanonKind::Predefined(PredefinedKind::Any), x)
        | (x, CanonKind::Predefined(PredefinedKind::Any)) => x.clone(),
        (CanonKind::Predefined(k1), CanonKind::Predefined(k2)) if k1 == k2 => {
            CanonKind::Predefined(*k1)
        }
        (CanonKind::Union(parts), other) | (other, CanonKind::Union(parts)) => {
            // The non-union side's `NOT NULL` carries into every member;
            // the union side's own is always false (a `Union` is nullable),
            // so this discriminates the or-pattern's two orders (the rust
            // review's Medium: the hardcoded b.not_null dropped it when
            // the union was b).
            let other_not_null = if matches!(a.kind, CanonKind::Union(_)) {
                b.not_null
            } else {
                a.not_null
            };
            let other_type = CanonType {
                kind: other.clone(),
                not_null: other_not_null,
            };
            let mut members: Vec<CanonType> = parts
                .iter()
                .map(|part| canon_intersection(part, &other_type))
                .filter(|met| {
                    !matches!(
                        met.kind,
                        CanonKind::Predefined(PredefinedKind::Null | PredefinedKind::Nothing)
                    )
                })
                .collect();
            match members.len() {
                0 => CanonKind::Predefined(PredefinedKind::Null),
                1 => members.pop().expect("len checked").kind,
                _ => CanonKind::Union(normalize_members(members)),
            }
        }
        (CanonKind::List(e1), CanonKind::List(e2)) => {
            CanonKind::List(Box::new(canon_intersection(e1, e2)))
        }
        (
            CanonKind::Vector {
                coord: coord_a,
                dimension: dimension_a,
            },
            CanonKind::Vector {
                coord: coord_b,
                dimension: dimension_b,
            },
        ) => {
            // Conflicting specifications on either parameter intersect to
            // NULL -- no value can satisfy both branches of a diamond --
            // and the tighter specification wins each parameter
            // otherwise (the rust review's Medium).
            let coord_conflict = matches!((coord_a, coord_b), (Some(x), Some(y)) if x != y);
            let dimension_conflict =
                matches!((dimension_a, dimension_b), (Some(x), Some(y)) if x != y);
            if coord_conflict || dimension_conflict {
                CanonKind::Predefined(PredefinedKind::Null)
            } else {
                CanonKind::Vector {
                    coord: coord_a.or(*coord_b),
                    dimension: dimension_a.or(*dimension_b),
                }
            }
        }
        _ => CanonKind::Predefined(PredefinedKind::Null),
    };
    CanonType { kind, not_null }
}

/// The union members deduplicated structurally (a member already present
/// by `CanonType` equality is dropped), preserving first-occurrence order.
pub(crate) fn normalize_members(members: Vec<CanonType>) -> Vec<CanonType> {
    let mut out: Vec<CanonType> = Vec::new();
    for member in members {
        if !out.contains(&member) {
            out.push(member);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_parser::VectorCoordKind;

    fn predefined(kind: PredefinedKind, not_null: bool) -> CanonType {
        CanonType {
            kind: CanonKind::Predefined(kind),
            not_null,
        }
    }

    #[test]
    fn a_union_side_intersection_carries_the_other_sides_not_null() {
        // The rust review's Medium regression: with the union on the b
        // side, the hardcoded `not_null: b.not_null` dropped the other
        // side's NOT NULL; the non-union side's flag carries into every
        // member regardless of argument order.
        let union_b = CanonType {
            kind: CanonKind::Union(vec![predefined(PredefinedKind::Integer, false)]),
            not_null: false,
        };
        let other_a = predefined(PredefinedKind::Integer, true);
        assert!(canon_intersection(&other_a, &union_b).not_null);
        assert!(canon_intersection(&union_b, &other_a).not_null);
    }

    #[test]
    fn vector_parameters_honor_the_tighter_specification_and_conflicts_go_null() {
        // The rust review's Medium regression: differing fully-specified
        // vectors used to intersect to whichever side the match kept,
        // passing DDL validation and then failing every write at runtime.
        let vec = |coord: Option<VectorCoordKind>, dimension: Option<i64>| CanonType {
            kind: CanonKind::Vector { coord, dimension },
            not_null: false,
        };
        // The tighter specification wins each parameter.
        let tighter = canon_intersection(
            &vec(None, None),
            &vec(Some(VectorCoordKind::Float32), Some(1536)),
        );
        assert!(matches!(
            tighter.kind,
            CanonKind::Vector {
                coord: Some(VectorCoordKind::Float32),
                dimension: Some(1536)
            }
        ));
        // Conflicting specifications intersect to NULL -- no value can
        // satisfy both branches of the diamond.
        let conflicting = canon_intersection(
            &vec(Some(VectorCoordKind::Float32), None),
            &vec(Some(VectorCoordKind::Float64), None),
        );
        assert!(matches!(
            conflicting.kind,
            CanonKind::Predefined(PredefinedKind::Null)
        ));
    }
}
