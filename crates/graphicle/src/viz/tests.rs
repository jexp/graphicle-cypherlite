//! The pack's contract tests: the registry, the parameter contract, the
//! dispatch and the `viz_view` tool surface, on stores seeded with a
//! repository and minimal facts. The family nodes' own tests extend the
//! [`graph`] fixture with the facts their views read.

use super::*;
use crate::api::{self, App};
use crate::model;
use crate::store::s;
use serde_json::{Value, json};
use std::result::Result;
use tempfile::TempDir;

/// The repository the fixtures seed.
const REPO: &str = "viz";

/// A graph store with one repository and the least history facts a family
/// reads: one commit, its one modification, one file (the history writer's
/// labels, `git_history::writer`).
fn graph() -> GraphStore {
    let graph = GraphStore::open_in_memory().unwrap();
    graph
        .transaction(|db| {
            crate::store_graph::write(
                db,
                "MERGE (c:Commit {repository_id: $repo, oid: $oid}) \
                     SET c.subject = $subject, c.author_date = $date \
                 MERGE (ch:Change {repository_id: $repo, commit: $oid, path: $path}) \
                     SET ch.kind = 'M' \
                 MERGE (f:File {repository_id: $repo, path: $path}) \
                 MERGE (c)-[:CONTAINS_CHANGE]->(ch) MERGE (ch)-[:MODIFIES]->(f)",
                crate::store_graph::params(&json!({"repo": REPO,
                    "oid": "0000000000000000000000000000000000000001",
                    "path": "src/lib.rs", "subject": "first",
                    "date": "2026-01-01T00:00:00Z"})),
            )?;
            Ok(())
        })
        .unwrap();
    graph
}

/// A registered project on its own store, as the tool surface reads one
/// (the supply-chain tools' test fixture, without its lockfile facts).
struct Project {
    _dir: TempDir,
    #[allow(dead_code)]
    store: Store,
    workspace: String,
    key: String,
    repo: String,
    app: App,
}

impl Project {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let created = store.create_workspace("Viz").unwrap();
        let (workspace, key) = (
            s(&created, "id").to_string(),
            s(&created, "capability").to_string(),
        );
        let registered = store
            .register(
                &workspace,
                &key,
                &json!({"project_key": "app", "name": "app"}),
            )
            .unwrap();
        let repo = s(&registered, "id").to_string();
        let app = App::new(store.clone(), model::Config::default(), vec![]).unwrap();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
            app,
        }
    }

    /// One MCP request, as the project's connection answers it.
    fn rpc(&self, method: &str, params: Value) -> Value {
        api::mcp(
            &self.app,
            &self.workspace,
            &self.key,
            &self.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .unwrap()
    }

    /// One `viz_view` call: its result, or the message of the error it
    /// carries.
    fn call(&self, arguments: Value) -> Result<Value, String> {
        let reply = self.rpc(
            "tools/call",
            json!({"name":"viz_view","arguments":arguments}),
        );
        if let Some(error) = reply.get("error") {
            return Err(error["message"].as_str().unwrap_or_default().to_owned());
        }
        let result = &reply["result"];
        if result["isError"] == true {
            return Err(result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned());
        }
        Ok(result.clone())
    }
}

#[test]
fn the_uri_family_is_its_slug_and_nothing_else() {
    assert_eq!(
        family("graphicle-graph://viz/hotspot-city"),
        Some("hotspot-city")
    );
    assert_eq!(
        family("graphicle-graph://viz/impact-neighborhood"),
        Some("impact-neighborhood")
    );
    // Another pack's URI, a deeper path, an unknown slug: none of them.
    assert_eq!(family("graphicle-graph://history/hotspots"), None);
    assert_eq!(family("graphicle-graph://viz/hotspot-city/x"), None);
    assert_eq!(family("graphicle-graph://viz/city"), None);
    assert_eq!(family("graphicle-graph://stats"), None);
}

#[test]
fn every_family_is_listed_as_a_resource_and_reads_as_a_stub() {
    let graph = graph();
    let specs = resources();
    assert_eq!(specs.len(), FAMILIES.len());
    for family in &FAMILIES {
        let uri = format!("{PREFIX}{}", family.slug);
        assert_eq!(super::family(&uri), Some(family.slug), "{uri}");
        let spec = specs
            .iter()
            .find(|spec| spec["uri"] == uri.as_str())
            .unwrap_or_else(|| panic!("no spec for {uri}: {specs:?}"));
        assert_eq!(spec["name"], family.name, "{uri}");
        assert_eq!(spec["mimeType"], "text/plain", "{uri}");
        // The families whose node has landed answer their own view (see
        // their own tests); the rest are still stubs.
        if family.slug == "arch-conformance" {
            continue;
        }
        // The stub's text report: the note, the section empty — a resource
        // answers, never errors, exactly as the history pack's families do
        // on a repository without their facts.
        let report = report(&graph, REPO, family.slug).unwrap();
        assert_eq!(report, format!("{NOT_YET}:\n  none"), "{}", family.slug);
        // The stub's json view-model: the empty object, the family named.
        let packet = render(&graph, REPO, family.slug, &Params::default(), Format::Json).unwrap();
        assert_eq!(packet["family"], family.slug);
        assert_eq!(packet["view"], json!({}), "{}", family.slug);
    }
}

#[test]
fn the_parameter_contract_parses_valid_values_and_refuses_bad_ones() {
    // Absent, and a null, are the defaults; so is every null field.
    assert_eq!(Params::parse(&json!({})).unwrap(), Params::default());
    assert_eq!(Params::parse(&Value::Null).unwrap(), Params::default());
    assert_eq!(
        Params::parse(&json!({"focus": null, "cap": null})).unwrap(),
        Params::default()
    );
    // Every field, in whatever order the call names them.
    let full = Params::parse(&json!({"focus":"src/lib.rs","window":"90d","cap":10,
        "overlay":"risk","depth":3,"bucket":"quarter"}))
    .unwrap();
    assert_eq!(
        full,
        Params {
            focus: Some("src/lib.rs".to_owned()),
            window: Window::NinetyDays,
            cap: 10,
            overlay: Overlay::Risk,
            depth: 3,
            bucket: Bucket::Quarter,
        }
    );
    let reordered = Params::parse(&json!({"bucket":"quarter","depth":3,"overlay":"risk",
        "cap":10,"window":"90d","focus":"src/lib.rs"}))
    .unwrap();
    assert_eq!(full, reordered);
    // The echo: one value per field, the contract's order, `focus` null when
    // unset — the same parameters echo identically.
    assert_eq!(
        Params::default().json(),
        json!({"focus":null,"window":"all","cap":TOP,"overlay":"none",
            "depth":DEFAULT_DEPTH,"bucket":"month"})
    );
    assert_eq!(full.json(), reordered.json());
    // Each wrong value is refused by name.
    let refusal = |text: &str| text.to_owned();
    for (args, expected) in [
        (
            json!({"window":"30d"}),
            format!("window must be {}", WINDOWS.join(" or ")),
        ),
        (json!({"window":30}), refusal("Expected window, 1–8 bytes")),
        (
            json!({"cap":0}),
            refusal("cap must be a number from 1 to 100"),
        ),
        (
            json!({"cap":MAX_CAP + 1}),
            refusal("cap must be a number from 1 to 100"),
        ),
        (
            json!({"cap":"25"}),
            refusal("cap must be a number from 1 to 100"),
        ),
        (
            json!({"depth":0}),
            refusal("depth must be a number from 1 to 3"),
        ),
        (
            json!({"depth":MAX_DEPTH + 1}),
            refusal("depth must be a number from 1 to 3"),
        ),
        (
            json!({"overlay":"churn"}),
            format!("overlay must be {}", OVERLAYS.join(", ")),
        ),
        (
            json!({"bucket":"week"}),
            format!("bucket must be {}", BUCKETS.join(" or ")),
        ),
        (json!({"focus":""}), refusal("Expected focus, 1–512 bytes")),
        (
            json!({"focus":"x".repeat(MAX_FOCUS + 1)}),
            refusal("Expected focus, 1–512 bytes"),
        ),
        (
            json!({"limit":5}),
            format!(
                "Unknown viz parameter: limit; use one of {}",
                PARAMS.join(", ")
            ),
        ),
        (json!("all"), refusal("params must be an object")),
        (json!([1]), refusal("params must be an object")),
    ] {
        let error = Params::parse(&args).unwrap_err();
        assert_eq!(error.message, expected, "{args}");
    }
    // The format likewise.
    assert_eq!(Format::parse("text").unwrap(), Format::Text);
    assert_eq!(Format::parse("json").unwrap(), Format::Json);
    assert_eq!(Format::parse("mermaid").unwrap(), Format::Mermaid);
    assert_eq!(
        Format::parse("svg").unwrap_err().message,
        "format must be text, json or mermaid"
    );
}

#[test]
fn an_unknown_family_is_refused_with_the_known_list() {
    let graph = graph();
    let error = render(&graph, REPO, "city", &Params::default(), Format::Text).unwrap_err();
    assert_eq!(
        error.message,
        format!(
            "Unknown viz family: city; use one of {}",
            FAMILIES
                .iter()
                .map(|family| family.slug)
                .collect::<Vec<_>>()
                .join(", ")
        )
    );
    let error = report(&graph, REPO, "city").unwrap_err();
    assert!(
        error.message.starts_with("Unknown viz family: city"),
        "{}",
        error.message
    );
}

#[test]
fn mermaid_is_served_only_by_the_families_that_declare_it() {
    let graph = graph();
    // A declared family: the stub's note as a mermaid comment, which is a
    // valid diagram.
    for family in FAMILIES.iter().filter(|family| family.mermaid) {
        let answer = render(
            &graph,
            REPO,
            family.slug,
            &Params::default(),
            Format::Mermaid,
        )
        .unwrap();
        assert_eq!(
            answer["mermaid"],
            format!("%% {NOT_YET}\n"),
            "{}",
            family.slug
        );
    }
    // An undeclared family: the named refusal, before the family runs.
    for slug in [
        "hotspot-city",
        "knowledge-map",
        "arch-conformance",
        "drift-timeline",
    ] {
        let error = render(&graph, REPO, slug, &Params::default(), Format::Mermaid).unwrap_err();
        assert_eq!(
            error.message,
            format!("The {slug} family serves text and json, not mermaid")
        );
    }
}

#[test]
fn the_same_inputs_render_byte_identical_output() {
    let graph = graph();
    // The same parameters, named in a different order: the same bytes, in
    // every format every family serves.
    let params = Params::parse(&json!({"cap":10,"window":"90d","focus":"src/lib.rs"})).unwrap();
    let reordered = Params::parse(&json!({"focus":"src/lib.rs","window":"90d","cap":10})).unwrap();
    assert_eq!(params, reordered);
    for family in &FAMILIES {
        // Every format the family serves: text and json always, mermaid by
        // declaration.
        let mut formats = vec![Format::Text, Format::Json];
        if family.mermaid {
            formats.push(Format::Mermaid);
        }
        for format in formats {
            let first =
                serde_json::to_string(&render(&graph, REPO, family.slug, &params, format).unwrap())
                    .unwrap();
            let second = serde_json::to_string(
                &render(&graph, REPO, family.slug, &reordered, format).unwrap(),
            )
            .unwrap();
            assert_eq!(first, second, "{} in {format:?}", family.slug);
            // A re-render of the very same inputs: byte-identical again.
            let again =
                serde_json::to_string(&render(&graph, REPO, family.slug, &params, format).unwrap())
                    .unwrap();
            assert_eq!(first, again, "{} in {format:?}", family.slug);
        }
    }
    // The parameters are in the bytes, so the equality is not vacuous.
    let default = serde_json::to_string(
        &render(
            &graph,
            REPO,
            "hotspot-city",
            &Params::default(),
            Format::Json,
        )
        .unwrap(),
    )
    .unwrap();
    let capped = serde_json::to_string(
        &render(&graph, REPO, "hotspot-city", &params, Format::Json).unwrap(),
    )
    .unwrap();
    assert_ne!(default, capped);
}

#[test]
fn viz_view_answers_on_the_tool_surface_read_only() {
    let project = Project::new();
    // Listed, read-only, with the family enum and the strict parameter
    // contract beside the shared `repo` argument.
    let listed = project.rpc("tools/list", json!({}));
    let tool = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "viz_view")
        .unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
    assert_eq!(tool["inputSchema"]["required"], json!(["family"]));
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    let schema = &tool["inputSchema"]["properties"];
    let families: Vec<&str> = schema["family"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|slug| slug.as_str().unwrap())
        .collect();
    assert_eq!(
        families,
        FAMILIES
            .iter()
            .map(|family| family.slug)
            .collect::<Vec<_>>()
    );
    for (field, max) in [("cap", MAX_CAP), ("depth", MAX_DEPTH)] {
        assert_eq!(schema["params"]["properties"][field]["maximum"], json!(max));
    }
    // The default format is the view-model; the parameters in force are
    // echoed; the stub's model is the empty object, served as the text too.
    let answer = project
        .call(json!({"family":"hotspot-city","params":{"cap":10,"window":"90d"}}))
        .unwrap();
    assert_eq!(answer["structuredContent"]["family"], "hotspot-city");
    assert_eq!(answer["structuredContent"]["format"], "json");
    assert_eq!(
        answer["structuredContent"]["params"],
        json!({"focus":null,"window":"90d","cap":10,"overlay":"none",
            "depth":DEFAULT_DEPTH,"bucket":"month"})
    );
    assert_eq!(answer["structuredContent"]["view"], json!({}));
    assert_eq!(answer["content"][0]["text"], json!({}).to_string());
    assert_eq!(answer["isError"], false);
    // The text format serves the report; mermaid the declared families'
    // diagram; both carry the whole answer as structured content.
    let text = project
        .call(json!({"family":"drift-timeline","format":"text"}))
        .unwrap();
    assert_eq!(text["structuredContent"]["format"], "text");
    assert_eq!(text["content"][0]["text"], format!("{NOT_YET}:\n  none"));
    let mermaid = project
        .call(json!({"family":"coupling-matrix","format":"mermaid"}))
        .unwrap();
    assert_eq!(mermaid["content"][0]["text"], format!("%% {NOT_YET}\n"));
    // Mermaid for a family that does not serve it: the named refusal.
    let refused = project
        .call(json!({"family":"hotspot-city","format":"mermaid"}))
        .unwrap_err();
    assert_eq!(
        refused,
        "The hotspot-city family serves text and json, not mermaid"
    );
    // A wrong parameter value and an unknown family are refused by the
    // published contract on the tool surface, and by name beneath it (the
    // command surface, where no tool schema stands in front of the
    // dispatcher).
    for refused in [
        project
            .call(json!({"family":"hotspot-city","params":{"window":"30d"}}))
            .unwrap_err(),
        project.call(json!({"family":"city"})).unwrap_err(),
    ] {
        assert!(
            refused.contains("do not match the published contract"),
            "{refused}"
        );
    }
    let error = api::command(
        &project.app,
        &project.workspace,
        &project.key,
        OPERATION,
        &json!({"family":"hotspot-city","params":{"window":"30d"},
            "repository_id":project.repo}),
    )
    .unwrap_err();
    assert_eq!(error.message, "window must be all or 90d");
    let error = api::command(
        &project.app,
        &project.workspace,
        &project.key,
        OPERATION,
        &json!({"family":"city","repository_id":project.repo}),
    )
    .unwrap_err();
    assert!(
        error.message.starts_with("Unknown viz family: city"),
        "{}",
        error.message
    );
}

/// The conformance fixture's rule document: two concepts that label the
/// symbols (one of them a node the rule creates, so it has no location), a
/// constraint over a call from the api folder into persistence, and one
/// that passes.
const CONFORMANCE_RULES: &str = "````concept\nid: arch:Viz/Persistence\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) WHERE s.stable_id STARTS WITH 'db_' SET s:Persistence\n```\n````\n\
````concept\nid: arch:Viz/Api\n```cypher\nMATCH (s:Symbol {repository_id: $repo}) WHERE s.relpath STARTS WITH 'src/api/' SET s:Api\n```\n````\n\
````concept\nid: arch:Viz/Layer\n```cypher\nMERGE (l:Layer {repository_id: $repo, name: 'Persistence'})\n```\n````\n\
````constraint\nid: arch:Viz/NoApiPersistence\nseverity: MAJOR\nrequires: [arch:Viz/Api, arch:Viz/Persistence]\n```cypher\nMATCH (a:Api {repository_id: $repo})-[:CALLS]->(p:Persistence {repository_id: $repo}) RETURN a, p\n```\n````\n\
````constraint\nid: arch:Viz/NoGhostLayer\nseverity: MINOR\nrequires: [arch:Viz/Layer]\n```cypher\nMATCH (l:Layer {repository_id: $repo, name: 'Ghost'}) RETURN l\n```\n````\n";

/// The episode the conformance fixture's enrich run is recorded for.
const EPISODE: &str = "ep-1";

/// A store with the conformance rules run over a small code graph, as the
/// `enrich` job leaves it: the concept applications and violations in the
/// store, and the job's recorded report. No `:Project` node exists, so the
/// view is answered without the supernode.
fn conformance_graph() -> (GraphStore, crate::arch_rules::EnrichReport) {
    use crate::store_graph;
    let graph = GraphStore::open_in_memory().unwrap();
    let document = crate::arch_rules::parse(CONFORMANCE_RULES).unwrap();
    crate::arch_rules::store(&graph, REPO, "docs/architecture/viz.md", &document).unwrap();
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "CREATE (:Symbol {repository_id: $repo, stable_id: 'db_store', kind: 'function', \
                   qualified_name: 'db_store', relpath: 'src/db/store.rs', line: 12}) \
                 CREATE (:Symbol {repository_id: $repo, stable_id: 'db_pool', kind: 'function', \
                   qualified_name: 'db_pool', relpath: 'src/db/pool.rs', line: 7}) \
                 CREATE (:Symbol {repository_id: $repo, stable_id: 'api_routes', kind: 'function', \
                   qualified_name: 'api_routes', relpath: 'src/api/routes.rs', line: 30}) \
                 CREATE (:Symbol {repository_id: $repo, stable_id: 'api_health', kind: 'function', \
                   qualified_name: 'api_health', relpath: 'src/api/routes.rs', line: 50}) \
                 CREATE (:Symbol {repository_id: $repo, stable_id: 'util_hash', kind: 'function', \
                   qualified_name: 'util_hash', relpath: 'src/util.rs', line: 3})",
                store_graph::params(&json!({"repo": REPO})),
            )?;
            store_graph::write(
                db,
                "MATCH (a:Symbol {stable_id: 'api_routes'}), (b:Symbol {stable_id: 'db_store'}) \
                 CREATE (a)-[:CALLS]->(b)",
                store_graph::params(&json!({})),
            )?;
            store_graph::write(
                db,
                "MATCH (a:Symbol {stable_id: 'api_routes'}), (b:Symbol {stable_id: 'db_pool'}) \
                 CREATE (a)-[:CALLS]->(b)",
                store_graph::params(&json!({})),
            )?;
            store_graph::write(
                db,
                "MATCH (a:Symbol {stable_id: 'api_health'}), (b:Symbol {stable_id: 'db_pool'}) \
                 CREATE (a)-[:CALLS]->(b)",
                store_graph::params(&json!({})),
            )?;
            Ok(())
        })
        .unwrap();
    let mut report = crate::arch_rules::enrich(&graph, REPO, EPISODE, &|| Ok(())).unwrap();
    let constraints =
        crate::arch_rules::constraints::evaluate(&graph, REPO, EPISODE, &report, &|| Ok(()))
            .unwrap();
    report.constraints = Some(constraints);
    // The job the enrich queue records: the same result the analyze report
    // reads back.
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "CREATE (:Job {repository_id: $repo, kind: 'enrich', state: 'complete', \
                   id: 'job-1', input_hash: $hash, result: $result})",
                store_graph::params(&json!({"repo": REPO,
                    "hash": crate::jobs::enrich_input_hash(&report.rule_set, EPISODE),
                    "result": report.to_value().to_string()})),
            )?;
            Ok(())
        })
        .unwrap();
    (graph, report)
}

/// The conformance view's rows of one section, by its rule id.
fn row_of<'a>(rows: &'a Value, rule: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["rule"] == rule)
        .unwrap_or_else(|| panic!("no row for {rule}: {rows}"))
}

#[test]
fn a_violation_row_cites_its_rule_and_its_relpath_line() {
    let (graph, _) = conformance_graph();
    let view = super::arch_conformance::view(&graph, REPO, &Params::default()).unwrap();
    let violations = &view.model["violations"];
    assert_eq!(violations.as_array().unwrap().len(), 3, "{violations}");
    // The caller's node is the one cited: the api symbol at its line.
    let cited = violations
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["rule"] == "arch:Viz/NoApiPersistence")
        .map(|row| row["evidence"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        cited,
        [
            "src/api/routes.rs:30",
            "src/api/routes.rs:30",
            "src/api/routes.rs:50"
        ]
    );
    // The text report prints the same citation beside the rule.
    let text = super::report(&graph, REPO, "arch-conformance").unwrap();
    assert!(text.contains("src/api/routes.rs:30  "), "{text}");
    assert!(text.contains("rule=arch:Viz/NoApiPersistence"), "{text}");
}

#[test]
fn coverage_and_results_match_the_enrich_report_of_the_same_store() {
    let (graph, report) = conformance_graph();
    let view = super::arch_conformance::view(&graph, REPO, &Params::default()).unwrap();
    // Every concept's labels are the labels its application reported.
    for applied in report.plans.iter().flat_map(|plan| &plan.applied) {
        let row = row_of(&view.model["concepts"], &applied.rule);
        assert_eq!(row["labels"], json!(applied.labels), "{}", applied.rule);
    }
    // The Layer concept labeled a node with no location: counted, not placed.
    assert_eq!(
        row_of(&view.model["concepts"], "arch:Viz/Layer")["unplaced"],
        1
    );
    // Every constraint's status and count are the recorded result's.
    let constraints = report.constraints.as_ref().unwrap();
    for result in &constraints.results {
        let row = row_of(&view.model["constraints"], &result.rule);
        assert_eq!(row["status"], serde_json::to_value(result.status).unwrap());
        assert_eq!(
            row["violations"],
            json!(result.violations.unwrap_or(0)),
            "{}",
            result.rule
        );
    }
    assert_eq!(
        view.model["totals"]["violations"],
        json!(constraints.violations)
    );
    assert_eq!(view.model["episode"], EPISODE);
    // The folder rollup places the api folder's violations beside their rule.
    let api = view.model["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["evidence"] == "src/api")
        .expect("the api folder is rolled up");
    assert_eq!(api["violations"], 3);
    assert_eq!(api["declared"], json!(["arch:Viz/NoApiPersistence"]));
}

#[test]
fn the_conformance_view_is_byte_identical_and_capped_and_scoped() {
    let (graph, _) = conformance_graph();
    let params = Params::parse(&json!({"cap": 2, "focus": "src/api"})).unwrap();
    let reordered = Params::parse(&json!({"focus": "src/api", "cap": 2})).unwrap();
    assert_eq!(params, reordered);
    let first = serde_json::to_string(
        &render(&graph, REPO, "arch-conformance", &params, Format::Json).unwrap(),
    )
    .unwrap();
    let second = serde_json::to_string(
        &render(&graph, REPO, "arch-conformance", &reordered, Format::Json).unwrap(),
    )
    .unwrap();
    assert_eq!(first, second);
    let again = serde_json::to_string(
        &render(&graph, REPO, "arch-conformance", &params, Format::Json).unwrap(),
    )
    .unwrap();
    assert_eq!(first, again);
    let text = |params: &Params| {
        serde_json::to_string(
            &render(&graph, REPO, "arch-conformance", params, Format::Text).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(text(&params), text(&reordered));

    // The cap cuts the rows and keeps the uncut total; the rank puts the
    // violation of the highest severity first, then the smaller evidence.
    let view = super::arch_conformance::view(&graph, REPO, &params).unwrap();
    assert_eq!(view.model["violations"].as_array().unwrap().len(), 2);
    assert_eq!(view.model["totals"]["violations"], 3);
    assert_eq!(
        view.model["violations"][0]["evidence"],
        "src/api/routes.rs:30"
    );
    // The focus scopes the violations to the folder, and nothing beneath
    // another folder leaks in.
    let elsewhere = Params::parse(&json!({"focus": "src/db"})).unwrap();
    let view = super::arch_conformance::view(&graph, REPO, &elsewhere).unwrap();
    assert_eq!(view.model["totals"]["violations"], 0);
    assert_eq!(view.model["episode"], EPISODE);
    // Every cited row names what it is about: a relpath:line, a folder or a
    // rule document location.
    let full = super::arch_conformance::view(&graph, REPO, &Params::default()).unwrap();
    for (_, rows) in &full.sections {
        for row in rows {
            assert!(
                !row["evidence"].as_str().unwrap_or_default().is_empty(),
                "{row}"
            );
        }
    }
}

#[test]
fn an_empty_store_reads_as_an_empty_conformance_view() {
    let graph = graph();
    let view = super::arch_conformance::view(&graph, REPO, &Params::default()).unwrap();
    assert_eq!(view.model["episode"], Value::Null);
    assert_eq!(view.model["totals"]["violations"], 0);
    assert!(view.mermaid.is_none());
    assert!(view.sections.iter().all(|(_, rows)| rows.is_empty()));
}
