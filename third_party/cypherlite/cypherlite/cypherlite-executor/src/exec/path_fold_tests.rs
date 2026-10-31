use super::agg_fold::take_path_fold_bindings;
use super::test_support::{build, exec_cypher};
use super::*;
use crate::graph::Graph;
use cypherlite_core::NodeId;

fn person_named(g: &Graph, id: i64, first: &str, last: &str) -> NodeId {
    g.create_node(
        vec!["Person".into()],
        BTreeMap::from([
            ("firstName".into(), Value::String(first.into())),
            ("lastName".into(), Value::String(last.into())),
            ("ID".into(), Value::Int(id)),
        ]),
    )
    .unwrap()
}

#[test]
fn path_fold_counts_distinct_comments_in_a_place() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let berlin = g
        .create_node(
            vec!["Place".into()],
            BTreeMap::from([("name".into(), Value::String("Berlin".into()))]),
        )
        .unwrap();
    let paris = g
        .create_node(
            vec!["Place".into()],
            BTreeMap::from([("name".into(), Value::String("Paris".into()))]),
        )
        .unwrap();
    let local = person_named(&g, 10, "Ada", "Local");
    let away = person_named(&g, 11, "Bea", "Away");
    g.create_rel(local, berlin, "personIsLocatedIn".into(), BTreeMap::new())
        .unwrap();
    g.create_rel(away, paris, "personIsLocatedIn".into(), BTreeMap::new())
        .unwrap();
    for (author, id, content) in [
        (local, 1, Some("one")),
        (local, 2, Some("two")),
        (local, 3, None),
        (away, 4, Some("away")),
    ] {
        let mut props = BTreeMap::from([("ID".into(), Value::Int(id))]);
        if let Some(content) = content {
            props.insert("content".into(), Value::String(content.into()));
        }
        let comment = g.create_node(vec!["Comment".into()], props).unwrap();
        g.create_rel(comment, author, "commentHasCreator".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();
    let _ = take_path_fold_bindings();
    let result = exec_cypher(
        &g,
        "MATCH (c:Comment)-[:commentHasCreator]->(p:Person)-[:personIsLocatedIn]->(l:Place) \
         WHERE c.content IS NOT NULL AND l.name = \"Berlin\" \
         RETURN COUNT(DISTINCT c.ID) AS num_comments",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(2)]]);
    assert_eq!(take_path_fold_bindings(), 2);
}

#[test]
fn path_fold_counts_distinct_likers() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let rafael = person_named(&g, 1, "Rafael", "Alonso");
    let other = person_named(&g, 2, "Other", "Author");
    let ada = person_named(&g, 3, "Ada", "Liker");
    let bea = person_named(&g, 4, "Bea", "Liker");
    let cam = person_named(&g, 5, "Cam", "Liker");
    let comment = |id: i64, author: NodeId| {
        let comment = g
            .create_node(
                vec!["Comment".into()],
                BTreeMap::from([("ID".into(), Value::Int(id))]),
            )
            .unwrap();
        g.create_rel(comment, author, "commentHasCreator".into(), BTreeMap::new())
            .unwrap();
        comment
    };
    let first = comment(10, rafael);
    let second = comment(11, rafael);
    let foreign = comment(12, other);
    g.create_rel(ada, first, "likeComment".into(), BTreeMap::new())
        .unwrap();
    g.create_rel(ada, second, "likeComment".into(), BTreeMap::new())
        .unwrap();
    g.create_rel(bea, first, "likeComment".into(), BTreeMap::new())
        .unwrap();
    g.create_rel(cam, foreign, "likeComment".into(), BTreeMap::new())
        .unwrap();
    g.commit_write();
    let _ = take_path_fold_bindings();
    let result = exec_cypher(
        &g,
        "MATCH (p:Person)<-[:commentHasCreator]-(c:Comment)<-[:likeComment]-(p2:Person) \
         WHERE p.firstName = \"Rafael\" AND p.lastName = \"Alonso\" \
         RETURN COUNT(DISTINCT p2.ID) AS num_persons",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(2)]]);
    assert!(take_path_fold_bindings() >= 3);
}

#[test]
fn path_fold_groups_interests_of_students() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let school = g
        .create_node(
            vec!["Organisation".into()],
            BTreeMap::from([("name".into(), Value::String("IISc".into()))]),
        )
        .unwrap();
    let other = g
        .create_node(
            vec!["Organisation".into()],
            BTreeMap::from([("name".into(), Value::String("Other".into()))]),
        )
        .unwrap();
    let rust = g
        .create_node(
            vec!["Tag".into()],
            BTreeMap::from([("name".into(), Value::String("Rust".into()))]),
        )
        .unwrap();
    let go = g
        .create_node(
            vec!["Tag".into()],
            BTreeMap::from([("name".into(), Value::String("Go".into()))]),
        )
        .unwrap();
    let mut next_id = 1;
    let mut student = |org: NodeId, tags: &[NodeId]| {
        let person = person_named(&g, next_id, "Stu", "Dent");
        next_id += 1;
        g.create_rel(person, org, "studyAt".into(), BTreeMap::new())
            .unwrap();
        for tag in tags {
            g.create_rel(person, *tag, "hasInterest".into(), BTreeMap::new())
                .unwrap();
        }
    };
    student(school, &[rust, go]);
    student(school, &[rust]);
    student(other, &[rust]);
    g.commit_write();
    let _ = take_path_fold_bindings();
    let result = exec_cypher(
        &g,
        "MATCH (p:Person)-[:studyAt]->(o:Organisation), (p)-[:hasInterest]->(t:Tag) \
         WHERE o.name = \"IISc\" \
         RETURN t.name AS name, COUNT(*) AS tag_count \
         ORDER BY tag_count DESC LIMIT 1",
    );
    assert_eq!(
        result.rows,
        vec![vec![Value::String("Rust".into()), Value::Int(2)]]
    );
    assert!(take_path_fold_bindings() >= 2);
}

#[test]
fn path_fold_requires_both_comma_patterns() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let school = g
        .create_node(
            vec!["Organisation".into()],
            BTreeMap::from([("name".into(), Value::String("Oxford".into()))]),
        )
        .unwrap();
    let tag = g
        .create_node(
            vec!["Tag".into()],
            BTreeMap::from([("name".into(), Value::String("Shakespeare".into()))]),
        )
        .unwrap();
    let enroll = |id: i64, links: &[(&str, NodeId)]| {
        let person = g
            .create_node(
                vec!["Person".into()],
                BTreeMap::from([("ID".into(), Value::Int(id))]),
            )
            .unwrap();
        for (kind, target) in links {
            g.create_rel(person, *target, (*kind).into(), BTreeMap::new())
                .unwrap();
        }
    };
    enroll(1, &[("studyAt", school), ("hasInterest", tag)]);
    enroll(2, &[("studyAt", school), ("hasInterest", tag)]);
    enroll(3, &[("studyAt", school)]);
    enroll(4, &[("hasInterest", tag)]);
    g.commit_write();
    let _ = take_path_fold_bindings();
    let result = exec_cypher(
        &g,
        "MATCH (p:Person)-[:studyAt]->(o:Organisation), (p)-[:hasInterest]->(t:Tag) \
         WHERE o.name = \"Oxford\" AND t.name = \"Shakespeare\" \
         RETURN COUNT(DISTINCT p.ID) AS num_p",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(2)]]);
    assert_eq!(take_path_fold_bindings(), 2);
}

#[test]
fn path_fold_counts_comments_by_place() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let tag = g
        .create_node(
            vec!["Tag".into()],
            BTreeMap::from([("name".into(), Value::String("Copernicus".into()))]),
        )
        .unwrap();
    let other = g
        .create_node(
            vec!["Tag".into()],
            BTreeMap::from([("name".into(), Value::String("Other".into()))]),
        )
        .unwrap();
    let india = g
        .create_node(
            vec!["Place".into()],
            BTreeMap::from([("name".into(), Value::String("India".into()))]),
        )
        .unwrap();
    let spain = g
        .create_node(
            vec!["Place".into()],
            BTreeMap::from([("name".into(), Value::String("Spain".into()))]),
        )
        .unwrap();
    let comment = |id: i64, tag: NodeId, place: NodeId| {
        let comment = g
            .create_node(
                vec!["Comment".into()],
                BTreeMap::from([("ID".into(), Value::Int(id))]),
            )
            .unwrap();
        g.create_rel(comment, tag, "commentHasTag".into(), BTreeMap::new())
            .unwrap();
        g.create_rel(comment, place, "commentIsLocatedIn".into(), BTreeMap::new())
            .unwrap();
    };
    comment(1, tag, india);
    comment(2, tag, india);
    comment(3, tag, spain);
    comment(4, other, india);
    g.commit_write();
    let _ = take_path_fold_bindings();
    let result = exec_cypher(
        &g,
        "MATCH (c:Comment)-[:commentHasTag]->(t:Tag), (c)-[:commentIsLocatedIn]->(l:Place) \
         WHERE t.name CONTAINS \"Copernicus\" \
         RETURN l.name AS name, COUNT(c.ID) AS comment_count \
         ORDER BY comment_count DESC",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::String("India".into()), Value::Int(2)],
            vec![Value::String("Spain".into()), Value::Int(1)],
        ]
    );
    assert_eq!(take_path_fold_bindings(), 3);
}

#[test]
fn variable_length_and_relationship_sum_stay_materialized() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("id".into(), Value::Int(1))]),
        )
        .unwrap();
    let mid = g.create_node(vec!["M".into()], BTreeMap::new()).unwrap();
    let b = g
        .create_node(
            vec!["B".into()],
            BTreeMap::from([("id".into(), Value::Int(2)), ("n".into(), Value::Int(10))]),
        )
        .unwrap();
    let c = g
        .create_node(
            vec!["C".into()],
            BTreeMap::from([("id".into(), Value::Int(3))]),
        )
        .unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.create_rel(a, mid, "R".into(), BTreeMap::new()).unwrap();
    g.create_rel(mid, b, "R".into(), BTreeMap::new()).unwrap();
    g.create_rel(
        a,
        b,
        "E".into(),
        BTreeMap::from([("w".into(), Value::Int(10))]),
    )
    .unwrap();
    g.create_rel(b, c, "E".into(), BTreeMap::new()).unwrap();
    g.commit_write();

    let _ = take_path_fold_bindings();
    let varied = exec_cypher(&g, "MATCH (a:A)-[:R*1..2]->(b:B) RETURN count(*) AS n");
    assert_eq!(varied.rows, vec![vec![Value::Int(2)]]);
    assert_eq!(take_path_fold_bindings(), 0);

    let summed = exec_cypher(
        &g,
        "MATCH (a:A)-[e:E]->(b:B)-[:E]->(c:C) RETURN sum(e.w) AS s",
    );
    assert_eq!(summed.rows, vec![vec![Value::Int(10)]]);
    assert_eq!(take_path_fold_bindings(), 0);
}

#[test]
fn repeated_relationship_variable_stays_on_the_matcher() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    let c = g.create_node(vec!["C".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, b, "T".into(), BTreeMap::new()).unwrap();
    g.create_rel(b, c, "T".into(), BTreeMap::new()).unwrap();
    g.commit_write();

    let _ = take_path_fold_bindings();
    let implicit = exec_cypher(&g, "MATCH ()-[r]-()-[r]-() RETURN count(*) AS count");
    assert_eq!(implicit.rows, vec![vec![Value::Int(0)]]);
    assert_eq!(take_path_fold_bindings(), 0);

    let repeatable = exec_cypher(
        &g,
        "MATCH REPEATABLE ELEMENTS ()-[r]-()-[r]-() RETURN count(*) AS count",
    );
    assert_eq!(repeatable.rows, vec![vec![Value::Int(4)]]);
    assert_eq!(take_path_fold_bindings(), 0);

    let directed = exec_cypher(
        &g,
        "MATCH (:A)-[e:T]->(:B)-[e]->() RETURN count(*) AS count",
    );
    assert_eq!(directed.rows, vec![vec![Value::Int(0)]]);
    assert_eq!(take_path_fold_bindings(), 0);
}

/// The `__` namespace is reserved against user aliases (OPLANE_REQ-00328162):
/// an aggregate aliased into the `__` namespace makes `fold_return` decline
/// (the uniform reservation every optimized path honors) and the ordinary
/// aggregation answers with the user's column name kept.
#[test]
fn path_fold_declines_a_user_alias_in_the_internal_namespace() {
    let _ = take_path_fold_bindings();
    let built = build(&[&["A"], &["B"]], &[(0, 1, "R")]);
    let r = exec_cypher(
        &built.graph,
        "MATCH (a:A)-[:R]->(b:B) RETURN count(b) AS __agg_base",
    );
    assert_eq!(r.columns, vec!["__agg_base".to_string()]);
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    assert_eq!(take_path_fold_bindings(), 0);
}
