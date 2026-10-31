use super::agg_fold::take_path_fold_bindings;
use super::test_support::exec_cypher;
use super::*;
use crate::graph::Graph;
use cypherlite_core::NodeId;
use cypherlite_ir::Expr;

fn q11_graph() -> Graph {
    let g = Graph::default();
    g.begin_write().unwrap();
    let org = |name: &str, kind: &str| {
        g.create_node(
            vec!["Organisation".into()],
            BTreeMap::from([
                ("name".into(), Value::String(name.into())),
                ("type".into(), Value::String(kind.into())),
            ]),
        )
        .unwrap()
    };
    let acme = org("Acme", "company");
    let state = org("State", "university");
    let beta = org("Beta", "company");
    let gamma = org("Gamma", "company");
    let delta = org("Delta", "company");
    let person = |id: Value| {
        g.create_node(vec!["Person".into()], BTreeMap::from([("ID".into(), id)]))
            .unwrap()
    };
    let person_no_id = || {
        g.create_node(vec!["Person".into()], BTreeMap::new())
            .unwrap()
    };
    let work = |person: NodeId, org: NodeId| {
        g.create_rel(person, org, "workAt".into(), BTreeMap::new())
            .unwrap();
    };
    let both = person(Value::Int(10));
    work(both, acme);
    work(both, state);
    let twice = person(Value::Int(20));
    work(twice, acme);
    work(twice, acme);
    work(person_no_id(), acme);
    work(person(Value::Int(1)), beta);
    work(person(Value::Float(1.0)), beta);
    work(person(Value::Int(2)), gamma);
    work(person(Value::Float(2.5)), gamma);
    work(person(Value::Int(5)), delta);
    work(person(Value::String("5".into())), delta);
    g.commit_write();
    g
}

fn q11_rows(result: &QueryResult) -> Vec<(String, i64)> {
    result
        .rows
        .iter()
        .map(|row| {
            (
                row[1].as_string().unwrap().to_string(),
                row[0].as_int().unwrap(),
            )
        })
        .collect()
}

const Q11: &str = "MATCH (p:Person)-[:workAt]->(o:Organisation) \
     WHERE o.type <> \"university\" \
     RETURN COUNT(DISTINCT p.ID) AS num_e, o.name AS name \
     ORDER BY num_e DESC, name";

fn assert_q11(result: &QueryResult) {
    assert_eq!(
        q11_rows(result),
        vec![
            ("Acme".into(), 2),
            ("Delta".into(), 2),
            ("Gamma".into(), 2),
            ("Beta".into(), 1),
        ]
    );
}

/// Organisation is the cheaper seed, so `o.type` is tested once per
/// organisation and a university's employees are never walked.
struct OrgSeed;

impl cypherlite_planner::PatternStats for OrgSeed {
    fn label_rows(&self, label: &str) -> Option<u64> {
        match label {
            "Organisation" => Some(1),
            "Person" => Some(1_000),
            _ => None,
        }
    }

    fn equality_rows(&self, _: &str, _: &str, _: &Expr) -> Option<u64> {
        None
    }

    fn contains_rows(&self, _: &str, _: &str, _: &Expr) -> Option<u64> {
        None
    }

    fn rel_type_rows(&self, _: &str) -> Option<u64> {
        None
    }
}

fn exec_org_seed(g: &Graph, cypher: &str) -> QueryResult {
    let query = cypherlite_front::parse_and_lower(cypher).unwrap();
    let plan = cypherlite_planner::plan_with_stats(query, &mut |_| false, &OrgSeed).unwrap();
    let pin = g.pin_now().unwrap();
    let schema = g.schema_snapshot();
    execute_read_at(&ReadAdmission {
        graph: g,
        snapshot_ts: pin.ts(),
        schema: &schema,
        request: ExecRequest::new(&plan, &BTreeMap::new(), cypher),
        cancel: None,
        ontology_document: None,
    })
    .unwrap()
}

#[test]
fn direct_fold_counts_distinct_employees_from_person() {
    let g = q11_graph();
    let _ = take_path_fold_bindings();
    let result = exec_cypher(&g, Q11);
    assert_q11(&result);
    // Four Acme edges (one repeated, one null id), two each for Beta, Gamma, and Delta.
    assert_eq!(take_path_fold_bindings(), 10);
}

#[test]
fn direct_fold_counts_distinct_employees_from_organisation() {
    let g = q11_graph();
    let _ = take_path_fold_bindings();
    let result = exec_org_seed(&g, Q11);
    assert_q11(&result);
    assert_eq!(take_path_fold_bindings(), 10);

    let _ = take_path_fold_bindings();
    let flipped = exec_org_seed(
        &g,
        "MATCH (p:Person)-[:workAt]->(o:Organisation) \
         WHERE \"university\" <> o.type \
         RETURN COUNT(DISTINCT p.ID) AS num_e, o.name AS name \
         ORDER BY num_e DESC, name",
    );
    assert_q11(&flipped);
    assert_eq!(take_path_fold_bindings(), 10);
}

#[test]
fn both_end_predicate_stays_on_the_scratch_fold() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let matching = g
        .create_node(
            vec!["Organisation".into()],
            BTreeMap::from([
                ("name".into(), Value::String("Match".into())),
                ("dept".into(), Value::Int(1)),
            ]),
        )
        .unwrap();
    let other = g
        .create_node(
            vec!["Organisation".into()],
            BTreeMap::from([
                ("name".into(), Value::String("Other".into())),
                ("dept".into(), Value::Int(9)),
            ]),
        )
        .unwrap();
    for dept in [1, 2] {
        let person = g
            .create_node(
                vec!["Person".into()],
                BTreeMap::from([("dept".into(), Value::Int(dept))]),
            )
            .unwrap();
        g.create_rel(person, matching, "workAt".into(), BTreeMap::new())
            .unwrap();
    }
    let outsider = g
        .create_node(
            vec!["Person".into()],
            BTreeMap::from([("dept".into(), Value::Int(1))]),
        )
        .unwrap();
    g.create_rel(outsider, other, "workAt".into(), BTreeMap::new())
        .unwrap();
    g.commit_write();

    let query = "MATCH (p:Person)-[:workAt]->(o:Organisation) \
         WHERE p.dept = o.dept \
         RETURN o.name AS name, count(*) AS n";
    let _ = take_path_fold_bindings();
    let folded = exec_cypher(&g, query);
    assert_eq!(take_path_fold_bindings(), 0);
    let grouped = exec_cypher(&g, &format!("{query} GROUP BY o.name"));
    let mut folded_rows = folded.rows.clone();
    let mut grouped_rows = grouped.rows.clone();
    folded_rows.sort_by(|a, b| a[0].as_string().unwrap().cmp(b[0].as_string().unwrap()));
    grouped_rows.sort_by(|a, b| a[0].as_string().unwrap().cmp(b[0].as_string().unwrap()));
    assert_eq!(folded_rows, grouped_rows);
    assert_eq!(folded_rows.len(), 1);
    assert_eq!(folded_rows[0][0], Value::String("Match".into()));
    assert_eq!(folded_rows[0][1], Value::Int(1));
}
