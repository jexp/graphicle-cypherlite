//! Point construction and geospatial predicates.

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};

/// `point()` argument handling (ADR-0167, LABS-169). Nodes and relationships
/// are resolved to their property maps by the caller (`bi_point`), so only
/// maps and null reach this function.
pub(crate) fn point_construct(vals: &[Value]) -> Result<Value> {
    match vals.first() {
        Some(Value::Map(map)) => point_from_map(map),
        Some(Value::Null) => Ok(Value::Null),
        _ => Err(Error::invalid_argument_type(
            "point() requires a map, node, or relationship",
        )),
    }
}

/// Coordinate family of a point: planar cartesian (`x`/`y`) or geographic
/// WGS 84 (`latitude`/`longitude`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    Cartesian,
    Geographic,
}

/// The spatial reference identifier domain: exactly the four codes Neo4j's
/// `point()` accepts (ADR-0167).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Srid {
    Wgs84_2D,
    Wgs84_3D,
    Cartesian2D,
    Cartesian3D,
}

impl Srid {
    /// Validate a raw code against the four-srid domain.
    fn from_i64(i: i64) -> Result<Srid> {
        match i {
            4326 => Ok(Srid::Wgs84_2D),
            4979 => Ok(Srid::Wgs84_3D),
            7203 => Ok(Srid::Cartesian2D),
            9157 => Ok(Srid::Cartesian3D),
            _ => Err(Error::ty(format!(
                "InvalidArgumentValue: point() unknown srid {i}: expected one of 4326, 4979, 7203, 9157"
            ))),
        }
    }

    fn code(self) -> i32 {
        match self {
            Srid::Wgs84_2D => 4326,
            Srid::Wgs84_3D => 4979,
            Srid::Cartesian2D => 7203,
            Srid::Cartesian3D => 9157,
        }
    }

    fn family(self) -> Family {
        match self {
            Srid::Wgs84_2D | Srid::Wgs84_3D => Family::Geographic,
            Srid::Cartesian2D | Srid::Cartesian3D => Family::Cartesian,
        }
    }

    fn is_3d(self) -> bool {
        matches!(self, Srid::Wgs84_3D | Srid::Cartesian3D)
    }

    /// The srid Neo4j picks when the map has no `srid` key.
    fn default(family: Family, has_z: bool) -> Srid {
        match (family, has_z) {
            (Family::Geographic, false) => Srid::Wgs84_2D,
            (Family::Geographic, true) => Srid::Wgs84_3D,
            (Family::Cartesian, false) => Srid::Cartesian2D,
            (Family::Cartesian, true) => Srid::Cartesian3D,
        }
    }

    /// Family of a raw srid code on a stored point; codes outside the
    /// four-srid domain (not constructible via `point()`) fall back to
    /// cartesian, i.e. planar distance.
    fn family_of(code: i32) -> Family {
        match code {
            4326 | 4979 => Family::Geographic,
            _ => Family::Cartesian,
        }
    }
}

/// Neo4j-verified construction rules: cartesian iff both `x` and `y` are
/// present, else geographic iff both `latitude` and `longitude` are present,
/// else an error. Extra properties are ignored.
fn point_from_map(map: &BTreeMap<String, Value>) -> Result<Value> {
    let family = if map.contains_key("x") && map.contains_key("y") {
        Family::Cartesian
    } else if map.contains_key("latitude") && map.contains_key("longitude") {
        Family::Geographic
    } else {
        return Err(Error::ty(
            "InvalidArgumentValue: point() requires either 'x' and 'y', or 'latitude' and 'longitude'",
        ));
    };
    let (mut x, y, z) = match family {
        // `height` is ignored for cartesian points.
        Family::Cartesian => (
            required_coordinate(map, "x")?,
            required_coordinate(map, "y")?,
            coordinate(map, "z")?,
        ),
        // Geographic: longitude is x, latitude is y; `z` wins over `height`.
        // `height` is still type-checked eagerly when a valid `z` wins —
        // deliberate, Neo4j-verified (a non-numeric height errors, 22N27).
        Family::Geographic => (
            required_coordinate(map, "longitude")?,
            required_coordinate(map, "latitude")?,
            coordinate(map, "z")?.or(coordinate(map, "height")?),
        ),
    };
    let srid = resolve_srid(map, family, z)?;
    if family == Family::Geographic {
        if !(-90.0..=90.0).contains(&y) {
            return Err(Error::ty(
                "InvalidArgumentValue: point() latitude must be in the range [-90, 90]",
            ));
        }
        x = wrap_longitude(x);
    }
    Ok(Value::Point(cypherlite_core::PointValue {
        srid: srid.code(),
        x,
        y,
        z,
    }))
}

/// Resolve the map's `srid` key: absent picks the family/dimension default;
/// a present value must be an integer from the four-code domain, matching
/// the coordinate dimension (3D srid iff `z` is present), and geographic
/// coordinates reject cartesian srids.
fn resolve_srid(map: &BTreeMap<String, Value>, family: Family, z: Option<f64>) -> Result<Srid> {
    match map.get("srid") {
        None => Ok(Srid::default(family, z.is_some())),
        Some(Value::Int(i)) => {
            let srid = Srid::from_i64(*i)?;
            if srid.is_3d() != z.is_some() {
                return Err(Error::ty(format!(
                    "InvalidArgumentValue: point() srid {} does not match the coordinate dimension",
                    srid.code()
                )));
            }
            if family == Family::Geographic && srid.family() == Family::Cartesian {
                return Err(Error::ty(
                    "InvalidArgumentValue: point() geographic coordinates require srid 4326 or 4979",
                ));
            }
            Ok(srid)
        }
        Some(_) => Err(Error::invalid_argument_type(
            "point() 'srid' must be an integer",
        )),
    }
}

/// A coordinate key that may be absent; present values must be finite
/// numbers. Neo4j-verified (22N24): NaN and ±Infinity are rejected for
/// every coordinate key.
fn coordinate(map: &BTreeMap<String, Value>, key: &str) -> Result<Option<f64>> {
    let value = match map.get(key) {
        None => return Ok(None),
        Some(Value::Int(i)) => *i as f64,
        Some(Value::Float(f)) => *f,
        Some(_) => return Err(coordinate_type_error(key)),
    };
    if !value.is_finite() {
        return Err(Error::ty(format!(
            "InvalidArgumentValue: point() coordinate '{key}' must be finite"
        )));
    }
    Ok(Some(value))
}

/// The one construction of the pinned wrong-type wording for a `point()`
/// coordinate key — shared by `coordinate`'s wrong-typed case and
/// `required_coordinate`'s absent-key case.
fn coordinate_type_error(key: &str) -> Error {
    Error::invalid_argument_type(format!(
        "point() coordinate '{key}' must be a float or integer"
    ))
}

/// A coordinate key guaranteed present by the family selection; null and
/// non-numeric values are errors.
fn required_coordinate(map: &BTreeMap<String, Value>, key: &str) -> Result<f64> {
    coordinate(map, key)?.ok_or_else(|| coordinate_type_error(key))
}

/// Neo4j longitude wrap: into [-180, 180], keeping both endpoints
/// (180.0 → 180.0, -180.0 → -180.0, 180.5 → -179.5, -200.0 → 160.0).
fn wrap_longitude(longitude: f64) -> f64 {
    if (-180.0..=180.0).contains(&longitude) {
        longitude
    } else {
        180.0 - (180.0 - longitude).rem_euclid(360.0)
    }
}

pub(crate) fn point_distance(vals: &[Value]) -> Result<Value> {
    match (vals.first(), vals.get(1)) {
        (Some(Value::Null), _) | (_, Some(Value::Null)) => Ok(Value::Null),
        (Some(Value::Point(a)), Some(Value::Point(b))) => {
            let planar = if Srid::family_of(a.srid) == Family::Geographic {
                let lat1 = a.y.to_radians();
                let lat2 = b.y.to_radians();
                let dlat = (b.y - a.y).to_radians();
                let dlon = (b.x - a.x).to_radians();
                let h = (dlat / 2.0).sin().powi(2)
                    + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
                // Neo4j's haversine earth radius in meters (LABS-256).
                6_378_140.0 * 2.0 * h.sqrt().asin()
            } else {
                ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt()
            };
            let distance = match (a.z, b.z) {
                (Some(az), Some(bz)) => (planar.powi(2) + (bz - az).powi(2)).sqrt(),
                _ => planar,
            };
            Ok(Value::Float(distance))
        }
        _ => Err(Error::ty("distance() requires two points")),
    }
}

pub(crate) fn point_within_bbox(vals: &[Value]) -> Result<Value> {
    match (vals.first(), vals.get(1), vals.get(2)) {
        (Some(Value::Point(p)), Some(Value::Point(lo)), Some(Value::Point(hi))) => Ok(Value::Bool(
            p.x >= lo.x && p.x <= hi.x && p.y >= lo.y && p.y <= hi.y,
        )),
        _ if vals.iter().any(Value::is_null) => Ok(Value::Null),
        _ => Err(Error::ty("point.withinBBox() requires three points")),
    }
}

pub(crate) fn point_within_distance(vals: &[Value]) -> Result<Value> {
    let distance = point_distance(&vals[..vals.len().min(2)])?;
    match (distance, vals.get(2).and_then(Value::as_number)) {
        (Value::Float(d), Some(max)) => Ok(Value::Bool(d <= max)),
        (Value::Null, _) => Ok(Value::Null),
        _ => Err(Error::ty(
            "point.withinDistance() requires points and distance",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(pairs: &[(&str, Value)]) -> Result<Value> {
        let map = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>();
        point_construct(&[Value::Map(map.into())])
    }

    fn srid_xyz(v: &Value) -> (i32, f64, f64, Option<f64>) {
        let Value::Point(p) = v else {
            panic!("expected point, got {v:?}")
        };
        (p.srid, p.x, p.y, p.z)
    }

    fn geo(x: f64, y: f64) -> Value {
        Value::Point(cypherlite_core::PointValue {
            srid: 4326,
            x,
            y,
            z: None,
        })
    }

    #[test]
    fn geographic_distance_uses_neo4j_haversine_radius() {
        // LABS-256: the openstreetmap walkthrough pair — Delacorte Musical
        // Clock to Zoo School. Neo4j returns 21.493084025393262, which is
        // the haversine with a 6_378_140 m earth radius (not the IUGG mean
        // radius 6_371_008.8 m, which yields 21.469…).
        let clock = geo(-73.9711748, 40.7681147);
        let zoo_school = geo(-73.9709581, 40.768013);
        let Value::Float(d) = point_distance(&[clock, zoo_school]).unwrap() else {
            panic!("expected float distance")
        };
        // The store rounds coordinates to 7 decimals (~11 mm), so compare
        // against the Neo4j reference with slack for that quantization.
        assert!(
            (d - 21.493084025393262).abs() < 1e-3,
            "distance {d} diverges from the Neo4j reference 21.493084025393262"
        );
    }

    #[test]
    fn null_yields_null_and_scalars_are_type_errors() {
        assert_eq!(point_construct(&[Value::Null]).unwrap(), Value::Null);
        for v in [Value::String("foo".into()), Value::Int(42)] {
            let err = point_construct(&[v]).unwrap_err().to_string();
            // The TCK harness derives the error kind from this prefix.
            assert!(
                err.contains("InvalidArgumentType: point() requires a map, node, or relationship"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn cartesian_requires_both_x_and_y() {
        let (srid, x, y, z) =
            srid_xyz(&point(&[("x", Value::Int(1)), ("y", Value::Int(2))]).unwrap());
        assert_eq!((srid, x, y, z), (7203, 1.0, 2.0, None));
        // z upgrades to 3D cartesian.
        let (srid, .., z) = srid_xyz(
            &point(&[
                ("x", Value::Float(1.0)),
                ("y", Value::Float(2.0)),
                ("z", Value::Float(3.0)),
            ])
            .unwrap(),
        );
        assert_eq!((srid, z), (9157, Some(3.0)));
        // height is ignored for cartesian points.
        let (srid, .., z) = srid_xyz(
            &point(&[
                ("x", Value::Int(1)),
                ("y", Value::Int(2)),
                ("height", Value::Int(100)),
            ])
            .unwrap(),
        );
        assert_eq!((srid, z), (7203, None));
        // Partial key sets are errors.
        assert!(point(&[("x", Value::Int(1))]).is_err());
        assert!(point(&[("y", Value::Int(1))]).is_err());
        assert!(point(&[]).is_err());
    }

    #[test]
    fn geographic_requires_both_latitude_and_longitude() {
        let (srid, x, y, z) = srid_xyz(
            &point(&[
                ("latitude", Value::Float(59.3293)),
                ("longitude", Value::Float(18.0686)),
            ])
            .unwrap(),
        );
        // longitude is x, latitude is y.
        assert_eq!((srid, x, y, z), (4326, 18.0686, 59.3293, None));
        // height gives 3D geographic.
        let (srid, .., z) = srid_xyz(
            &point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Int(18)),
                ("height", Value::Int(100)),
            ])
            .unwrap(),
        );
        assert_eq!((srid, z), (4979, Some(100.0)));
        // z wins over height when both are present.
        let (srid, .., z) = srid_xyz(
            &point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Int(18)),
                ("z", Value::Int(3)),
                ("height", Value::Int(100)),
            ])
            .unwrap(),
        );
        assert_eq!((srid, z), (4979, Some(3.0)));
        assert!(point(&[("latitude", Value::Int(59))]).is_err());
    }

    #[test]
    fn cartesian_keys_win_over_geographic_keys() {
        // {x, y, latitude, longitude} → cartesian.
        let (srid, x, y, _) = srid_xyz(
            &point(&[
                ("x", Value::Int(1)),
                ("y", Value::Int(2)),
                ("latitude", Value::Int(59)),
                ("longitude", Value::Int(18)),
            ])
            .unwrap(),
        );
        assert_eq!((srid, x, y), (7203, 1.0, 2.0));
        // {x, latitude, longitude} → geographic (no y, so not cartesian).
        let (srid, x, y, _) = srid_xyz(
            &point(&[
                ("x", Value::Int(1)),
                ("latitude", Value::Int(59)),
                ("longitude", Value::Int(18)),
            ])
            .unwrap(),
        );
        assert_eq!((srid, x, y), (4326, 18.0, 59.0));
    }

    #[test]
    fn extra_properties_are_ignored() {
        let (srid, x, y, _) = srid_xyz(
            &point(&[
                ("latitude", Value::Float(59.3293)),
                ("longitude", Value::Float(18.0686)),
                ("address", Value::String("5 Corrie Road".into())),
                ("postcode", Value::String("AB1 2CD".into())),
            ])
            .unwrap(),
        );
        assert_eq!((srid, x, y), (4326, 18.0686, 59.3293));
    }

    #[test]
    fn non_numeric_coordinates_are_errors() {
        assert!(point(&[("x", Value::String("1".into())), ("y", Value::Int(2))]).is_err());
        assert!(point(&[("x", Value::Null), ("y", Value::Int(2))]).is_err());
        assert!(point(&[
            ("latitude", Value::Int(59)),
            ("longitude", Value::Int(18)),
            ("height", Value::String("100".into())),
        ])
        .is_err());
    }

    #[test]
    fn non_finite_coordinates_are_errors() {
        // Neo4j-verified (22N24): NaN and ±Infinity in any coordinate error.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            // Cartesian family: x, y, z.
            assert!(point(&[("x", Value::Float(bad)), ("y", Value::Int(1))]).is_err());
            assert!(point(&[("x", Value::Int(1)), ("y", Value::Float(bad))]).is_err());
            assert!(point(&[
                ("x", Value::Int(1)),
                ("y", Value::Int(2)),
                ("z", Value::Float(bad)),
            ])
            .is_err());
            // Geographic family: longitude, latitude, z, height.
            assert!(point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Float(bad)),
            ])
            .is_err());
            assert!(point(&[
                ("latitude", Value::Float(bad)),
                ("longitude", Value::Int(18)),
            ])
            .is_err());
            assert!(point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Int(18)),
                ("z", Value::Float(bad)),
            ])
            .is_err());
            assert!(point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Int(18)),
                ("height", Value::Float(bad)),
            ])
            .is_err());
        }
        // The error is an InvalidArgumentValue naming the coordinate key.
        let err = point(&[("x", Value::Float(f64::NAN)), ("y", Value::Int(1))])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("InvalidArgumentValue: point() coordinate 'x' must be finite"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn height_is_type_checked_even_when_z_wins() {
        // Neo4j-verified (22N27): height is validated eagerly, so a
        // non-numeric height errors even though a valid z takes precedence.
        let err = point(&[
            ("latitude", Value::Float(59.0)),
            ("longitude", Value::Float(18.0)),
            ("z", Value::Float(3.0)),
            ("height", Value::String("abc".into())),
        ])
        .unwrap_err()
        .to_string();
        assert!(
            err.contains(
                "InvalidArgumentType: point() coordinate 'height' must be a float or integer"
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn srid_must_be_a_known_integer() {
        // Float and string srids are errors (previously silently ignored).
        assert!(point(&[
            ("x", Value::Int(1)),
            ("y", Value::Int(2)),
            ("srid", Value::Float(4326.0)),
        ])
        .is_err());
        assert!(point(&[
            ("x", Value::Int(1)),
            ("y", Value::Int(2)),
            ("srid", Value::String("7203".into())),
        ])
        .is_err());
        // Unknown code.
        assert!(point(&[
            ("x", Value::Int(1)),
            ("y", Value::Int(2)),
            ("srid", Value::Int(9999)),
        ])
        .is_err());
    }

    #[test]
    fn srid_must_match_dimension_and_family() {
        // 2D srid with z present.
        assert!(point(&[
            ("x", Value::Int(1)),
            ("y", Value::Int(2)),
            ("z", Value::Int(3)),
            ("srid", Value::Int(7203)),
        ])
        .is_err());
        // 3D srid without z.
        assert!(point(&[
            ("x", Value::Int(1)),
            ("y", Value::Int(2)),
            ("srid", Value::Int(9157)),
        ])
        .is_err());
        // Geographic keys with a cartesian srid.
        assert!(point(&[
            ("latitude", Value::Int(59)),
            ("longitude", Value::Int(18)),
            ("srid", Value::Int(7203)),
        ])
        .is_err());
        // 3D geographic srid without z/height.
        assert!(point(&[
            ("latitude", Value::Int(59)),
            ("longitude", Value::Int(18)),
            ("srid", Value::Int(4979)),
        ])
        .is_err());
        // Cartesian keys accept any of the four srids, dimension-checked.
        let (srid, x, y, z) = srid_xyz(
            &point(&[
                ("x", Value::Int(1)),
                ("y", Value::Int(2)),
                ("srid", Value::Int(4326)),
            ])
            .unwrap(),
        );
        assert_eq!((srid, x, y, z), (4326, 1.0, 2.0, None));
        let (srid, ..) = srid_xyz(
            &point(&[
                ("x", Value::Int(1)),
                ("y", Value::Int(2)),
                ("z", Value::Int(3)),
                ("srid", Value::Int(9157)),
            ])
            .unwrap(),
        );
        assert_eq!(srid, 9157);
        // Geographic keys with an explicit geographic srid.
        let (srid, ..) = srid_xyz(
            &point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Int(18)),
                ("srid", Value::Int(4326)),
            ])
            .unwrap(),
        );
        assert_eq!(srid, 4326);
    }

    #[test]
    fn geographic_ranges_are_enforced() {
        // Latitude must be in [-90, 90].
        assert!(point(&[("latitude", Value::Int(100)), ("longitude", Value::Int(18)),]).is_err());
        assert!(point(&[
            ("latitude", Value::Float(-90.0)),
            ("longitude", Value::Int(18)),
        ])
        .is_ok());
        // Longitude wraps into [-180, 180], keeping both endpoints.
        let (_, x, ..) = srid_xyz(
            &point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Float(-200.0)),
            ])
            .unwrap(),
        );
        assert_eq!(x, 160.0);
        let (_, x, ..) = srid_xyz(
            &point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Float(180.5)),
            ])
            .unwrap(),
        );
        assert_eq!(x, -179.5);
        let (_, x, ..) = srid_xyz(
            &point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Float(180.0)),
            ])
            .unwrap(),
        );
        assert_eq!(x, 180.0);
        let (_, x, ..) = srid_xyz(
            &point(&[
                ("latitude", Value::Int(59)),
                ("longitude", Value::Float(-180.0)),
            ])
            .unwrap(),
        );
        assert_eq!(x, -180.0);
    }
}
