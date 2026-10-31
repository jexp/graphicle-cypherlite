//! LABS-183 / ADR-0191: a keyword is a symbolic name after `AS` and as a
//! variable reference. These tests run the three recommendations.adoc
//! walkthrough statements that found the gap, verbatim, against a small
//! hand-built graph and pin their exact result rows: `count(a) AS as` is
//! read back by `(5*gs)+(3*as)+(4*ds)`, and `AS union` is passed to
//! `size(union)`.
//!
//! The three statements are the `[source,cypher]` blocks at lines 218, 268
//! and 307 of `documentation/recommendations.adoc` in
//! neo4j-graph-examples/recommendations (fetched 2026-10-08), copied
//! verbatim including the `//` comment line and the trailing spaces.

mod common;

use cypherlite::Value;

use common::{connection, run, string};

/// A small hand-built recommendations graph: four movies, five genres,
/// actors and directors. `Wizard of Oz, The` and `Interstellar` share the
/// `Adventure` genre, the `Shared Actor` and no director; `Inception`,
/// `The Matrix` and `Interstellar` share `Sci-Fi`, with distinct extra
/// traits so every statement's rows have distinct scores.
const FIXTURE: &str = "\
CREATE (w:Movie {title: 'Wizard of Oz, The'}),
       (i:Movie {title: 'Inception'}),
       (t:Movie {title: 'Interstellar'}),
       (m:Movie {title: 'The Matrix'}),
       (fantasy:Genre {name: 'Fantasy'}),
       (adventure:Genre {name: 'Adventure'}),
       (scifi:Genre {name: 'Sci-Fi'}),
       (action:Genre {name: 'Action'}),
       (drama:Genre {name: 'Drama'}),
       (w)-[:IN_GENRE]->(fantasy),
       (w)-[:IN_GENRE]->(adventure),
       (i)-[:IN_GENRE]->(scifi),
       (i)-[:IN_GENRE]->(action),
       (t)-[:IN_GENRE]->(scifi),
       (t)-[:IN_GENRE]->(adventure),
       (t)-[:IN_GENRE]->(drama),
       (m)-[:IN_GENRE]->(scifi),
       (wActor:Person {name: 'Wizard Actor'})-[:ACTED_IN]->(w),
       (shared:Person {name: 'Shared Actor'})-[:ACTED_IN]->(w),
       (shared)-[:ACTED_IN]->(t),
       (tActor:Person {name: 'Interstellar Actor'})-[:ACTED_IN]->(t),
       (iActor:Person {name: 'Inception Actor'})-[:ACTED_IN]->(i),
       (mActor:Person {name: 'Matrix Actor'})-[:ACTED_IN]->(m),
       (wDirector:Person {name: 'Wizard Director'})-[:DIRECTED]->(w),
       (tDirector:Person {name: 'Interstellar Director'})-[:DIRECTED]->(t),
       (iDirector:Person {name: 'Inception Director'})-[:DIRECTED]->(i),
       (mDirector:Person {name: 'Matrix Director'})-[:DIRECTED]->(m)";

/// `recommendations.adoc` line 218 (weighted content algorithm): aliases
/// `count(a)` as `as` and reads it back in the score expression.
const RECOMMENDATIONS_LINE_218: &str = "\
// Find similar movies by common genres
MATCH (m:Movie) WHERE m.title = 'Wizard of Oz, The'
MATCH (m)-[:IN_GENRE]->(g:Genre)<-[:IN_GENRE]-(rec:Movie)

WITH m, rec, count(*) AS gs

OPTIONAL MATCH (m)<-[:ACTED_IN]-(a)-[:ACTED_IN]->(rec)
WITH m, rec, gs, count(a) AS as

OPTIONAL MATCH (m)<-[:DIRECTED]-(d)-[:DIRECTED]->(rec)
WITH m, rec, gs, as, count(d) AS ds

RETURN rec.title AS recommendation, 
       (5*gs)+(3*as)+(4*ds) AS score 
ORDER BY score DESC LIMIT 25";

/// `recommendations.adoc` line 268 (Jaccard index over genres): aliases the
/// genre union as `union` and passes it to `size(union)`.
const RECOMMENDATIONS_LINE_268: &str = "\
MATCH (m:Movie {title:'Inception'})-[:IN_GENRE]->
      (g:Genre)<-[:IN_GENRE]-(other:Movie)
WITH m, other, count(g) AS intersection, collect(g.name) as common

WITH m,other, intersection, common,
     [(m)-[:IN_GENRE]->(mg) | mg.name] AS set1,
     [(other)-[:IN_GENRE]->(og) | og.name] AS set2

WITH m,other,intersection, common, set1, set2,
     set1+[x IN set2 WHERE NOT x IN set1] AS union

RETURN m.title, other.title, common, set1,set2,
       ((1.0*intersection)/size(union)) AS jaccard 

ORDER BY jaccard DESC LIMIT 25";

/// `recommendations.adoc` line 307 (Jaccard index over genres, actors and
/// directors): the same `AS union` spelling over all three trait types.
const RECOMMENDATIONS_LINE_307: &str = "\
MATCH (m:Movie {title: 'Inception'})-[:IN_GENRE|ACTED_IN|DIRECTED]-
                   (t)<-[:IN_GENRE|ACTED_IN|DIRECTED]-(other:Movie)
WITH m, other, count(t) AS intersection, collect(t.name) AS common,
     [(m)-[:IN_GENRE|ACTED_IN|DIRECTED]-(mt) | mt.name] AS set1,
     [(other)-[:IN_GENRE|ACTED_IN|DIRECTED]-(ot) | ot.name] AS set2

WITH m,other,intersection, common, set1, set2,
     set1 + [x IN set2 WHERE NOT x IN set1] AS union

RETURN m.title, other.title, common, set1,set2, 
       ((1.0*intersection)/size(union)) AS jaccard 
ORDER BY jaccard DESC LIMIT 25";

/// A string list value for row assertions.
fn list(items: &[&str]) -> Value {
    Value::List(
        items
            .iter()
            .map(|item| string(item))
            .collect::<Vec<Value>>()
            .into(),
    )
}

/// The three walkthrough statements run as written and return the rows the
/// fixture graph yields:
///
/// - weighted: `Interstellar` shares one genre and the shared actor with
///   `Wizard of Oz, The` (no shared director), so `gs=1`, `as=1`, `ds=0`
///   and `score=8`;
/// - Jaccard over genres: `The Matrix` shares `Sci-Fi` (union 2) and
///   `Interstellar` shares `Sci-Fi` (union 4);
/// - Jaccard over all traits: `The Matrix` shares `Sci-Fi` (union 6) and
///   `Interstellar` shares `Sci-Fi` (union 9).
///
/// A movie itself never matches: reusing the same `IN_GENRE` relationship on
/// both sides of the pattern violates Cypher's relationship uniqueness.
#[test]
fn recommendations_walkthrough_statements_run_verbatim() {
    let mut c = connection();
    run(&mut c, FIXTURE);

    let weighted = run(&mut c, RECOMMENDATIONS_LINE_218);
    assert_eq!(
        weighted.header.columns,
        vec!["recommendation".to_string(), "score".to_string()]
    );
    assert_eq!(
        weighted.rows,
        vec![vec![string("Interstellar"), Value::Int(8)]]
    );

    let jaccard_genres = run(&mut c, RECOMMENDATIONS_LINE_268);
    assert_eq!(
        jaccard_genres.header.columns,
        vec![
            "m.title".to_string(),
            "other.title".to_string(),
            "common".to_string(),
            "set1".to_string(),
            "set2".to_string(),
            "jaccard".to_string(),
        ]
    );
    assert_eq!(
        jaccard_genres.rows,
        vec![
            vec![
                string("Inception"),
                string("The Matrix"),
                list(&["Sci-Fi"]),
                // ADR-0203: a node's expansion walks each relationship
                // chain newest-first, so `collect` sees Inception's genres
                // Action (created second) before Sci-Fi.
                list(&["Action", "Sci-Fi"]),
                list(&["Sci-Fi"]),
                Value::Float(0.5),
            ],
            vec![
                string("Inception"),
                string("Interstellar"),
                list(&["Sci-Fi"]),
                list(&["Action", "Sci-Fi"]),
                list(&["Drama", "Adventure", "Sci-Fi"]),
                Value::Float(0.25),
            ],
        ]
    );

    let jaccard_all = run(&mut c, RECOMMENDATIONS_LINE_307);
    assert_eq!(
        jaccard_all.header.columns,
        vec![
            "m.title".to_string(),
            "other.title".to_string(),
            "common".to_string(),
            "set1".to_string(),
            "set2".to_string(),
            "jaccard".to_string(),
        ]
    );
    assert_eq!(
        jaccard_all.rows,
        vec![
            vec![
                string("Inception"),
                string("The Matrix"),
                list(&["Sci-Fi"]),
                list(&["Action", "Sci-Fi", "Inception Actor", "Inception Director"]),
                list(&["Sci-Fi", "Matrix Actor", "Matrix Director"]),
                Value::Float(1.0 / 6.0),
            ],
            vec![
                string("Inception"),
                string("Interstellar"),
                list(&["Sci-Fi"]),
                list(&["Action", "Sci-Fi", "Inception Actor", "Inception Director"]),
                // Newest-first per relationship chain (ADR-0203): genres
                // Drama > Adventure > Sci-Fi, then the newer ACTED_IN
                // (Interstellar Actor) before Shared Actor.
                list(&[
                    "Drama",
                    "Adventure",
                    "Sci-Fi",
                    "Interstellar Actor",
                    "Shared Actor",
                    "Interstellar Director",
                ]),
                Value::Float(1.0 / 9.0),
            ],
        ]
    );
}
