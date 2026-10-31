use crate::{Database, Value};

#[test]
fn point_accepts_location_nodes_like_the_pole_walkthrough() {
    // LABS-169: the pole.adoc walkthrough calls point(l) on Location nodes.
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "CREATE (:Location {address: '5 Corrie Road', longitude: -2.5318, latitude: 53.4808}), \
         (:Location {address: '1 Pole Street', longitude: -2.5300, latitude: 53.4790}), \
         (:Location {address: '9 Flag Avenue', longitude: -2.5100, latitude: 53.4700})",
    )
    .unwrap();
    let r = c
        .query(
            "MATCH (l:Location {address: '5 Corrie Road'}) \
             WITH point(l) AS corrie \
             MATCH (x:Location) \
             WITH x, point.distance(point(x), corrie) AS d \
             RETURN x.address AS address, d ORDER BY d",
        )
        .unwrap();
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0][0], Value::String("5 Corrie Road".into()));
    assert_eq!(r.rows[0][1], Value::Float(0.0));
    // Distances increase away from the corrie node.
    let Value::Float(d1) = r.rows[1][1] else {
        panic!("expected float distance")
    };
    let Value::Float(d2) = r.rows[2][1] else {
        panic!("expected float distance")
    };
    assert!(d1 > 0.0 && d2 > d1, "distances should ascend: {d1}, {d2}");
}

#[test]
fn point_reads_entity_property_maps() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query(
        "CREATE (:P {x: 1, y: 2}), (:P {x: 1, y: 2, z: 3}), \
         (:P {longitude: 18.0, latitude: 59.0}), \
         (:P {longitude: 18.0, latitude: 59.0, height: 100}), \
         (:P {x: 1, y: 2, srid: 4326})",
    )
    .unwrap();
    let r = c
        .query("MATCH (n:P) RETURN point(n).srid AS srid ORDER BY srid")
        .unwrap();
    let srids: Vec<Value> = r
        .rows
        .into_iter()
        .map(|row| row.into_iter().next().unwrap())
        .collect();
    assert_eq!(
        srids,
        vec![
            Value::Int(4326), // geographic
            Value::Int(4326), // cartesian keys, explicit geographic srid
            Value::Int(4979), // geographic with height
            Value::Int(7203), // cartesian
            Value::Int(9157), // cartesian 3D
        ]
    );

    // Relationships are map-like too.
    c.query("CREATE (:A)-[:R {x: 3, y: 4}]->(:A)").unwrap();
    let r = c
        .query("MATCH ()-[r:R]->() RETURN point(r).srid AS srid, point(r).x AS x")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(7203));
    assert_eq!(r.rows[0][1], Value::Float(3.0));
}

#[test]
fn point_argument_errors() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();

    // point(null) is null.
    let r = c.query("RETURN point(null) AS p").unwrap();
    assert_eq!(r.rows[0][0], Value::Null);

    // Scalars are type errors naming map/node/relationship.
    for q in ["RETURN point('foo') AS p", "RETURN point(42) AS p"] {
        let err = c.query(q).unwrap_err().to_string();
        assert!(
            err.contains("map, node, or relationship"),
            "expected a map/node/relationship type error, got {err}"
        );
    }

    // A node without coordinate keys errors like an empty map.
    c.query("CREATE (:NoCoords {name: 'x'})").unwrap();
    let err = c
        .query("MATCH (n:NoCoords) RETURN point(n) AS p")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("'x' and 'y', or 'latitude' and 'longitude'"),
        "expected missing-coordinate error, got {err}"
    );

    // Range and srid rules hold through the query surface.
    assert!(c
        .query("RETURN point({latitude: 100, longitude: 18}) AS p")
        .is_err());
    assert!(c
        .query("RETURN point({x: 1, y: 2, srid: 4326.0}) AS p")
        .is_err());
    assert!(c
        .query("RETURN point({x: 1, y: 2, srid: 9999}) AS p")
        .is_err());
    let r = c
        .query("RETURN point({latitude: 59, longitude: -200}).x AS x")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Float(160.0));

    // Accessing a deleted entity is rejected like properties().
    let err = c
        .query("CREATE (n:Doomed {x: 1, y: 2}) WITH * MATCH (n) DETACH DELETE n RETURN point(n)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("DeletedEntityAccess"),
        "expected DeletedEntityAccess, got {err}"
    );
}
