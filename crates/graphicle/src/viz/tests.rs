//! The pack's contract tests: the registry, the parameter contract, the
//! dispatch and the `viz_view` tool surface, on stores seeded with a
//! repository and minimal facts. The family nodes' own tests extend the
//! [`graph`] fixture with the facts their views read.

use super::*;
use crate::api::{self, App};
use crate::model;
use crate::store::s;
use crate::store_graph;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::result::Result;
use tempfile::TempDir;

/// The repository the fixtures seed.
const REPO: &str = "viz";
/// The coupling matrix's slug: its view is served by its own module.
const COUPLING: &str = "coupling-matrix";

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
        // The coupling matrix has its own view; its tests are below.
        if family.slug == COUPLING {
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
    for family in FAMILIES
        .iter()
        .filter(|family| family.mermaid && family.slug != COUPLING)
    {
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
        .call(json!({"family":"impact-neighborhood","format":"mermaid"}))
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

/// The coupling matrix's fixture: [`graph`] with stored co-change pairs (the
/// coupling job's `CO_CHANGED` shape, smaller endpoint as the source) and
/// live or closed static relations (`CALLS` edges of the projection).
///
/// The folder pairs it yields, in canonical order, with their readings:
/// - `src/git_history` x `src/viz`: co-change 5 (90d 4), one static edge: a
///   hot, rising cell that is not flagged.
/// - `src/render` x `src/store`: co-change 4 (90d 0), no static edge: cooling,
///   flagged hidden coupling.
/// - `src/viz` x `src/viz`: co-change 3 (90d 3), one static edge inside the
///   folder: a diagonal cell.
/// - `.` x `src/viz`: co-change 2 (90d 2), no static edge: below the hot
///   threshold, so not flagged.
/// - `src/api` x `src/graph`: one static edge, no co-change: stale coupling.
///   The same folder's self-edge and the closed `src/api` to `src/render`
///   edge contribute no cell.
fn coupling_graph() -> GraphStore {
    let graph = graph();
    for (a, b, all, recent) in [
        ("src/git_history/queries.rs", "src/viz/mod.rs", 5, 4),
        ("src/render/b.rs", "src/store/a.rs", 4, 0),
        ("src/viz/mod.rs", "src/viz/tests.rs", 3, 3),
        ("README.md", "src/viz/mod.rs", 2, 2),
    ] {
        co_change(&graph, a, b, "all", all);
        co_change(&graph, a, b, "90d", recent);
    }
    // Live relations: one across the known pair, one inside src/viz, one
    // stale across src/api and src/graph, and a file's own relation.
    relation(
        &graph,
        "src/git_history/queries.rs",
        "f",
        "src/viz/mod.rs",
        "g",
        None,
    );
    relation(&graph, "src/viz/mod.rs", "g", "src/viz/tests.rs", "t", None);
    relation(&graph, "src/graph/x.rs", "h", "src/api/y.rs", "k", None);
    relation(&graph, "src/graph/x.rs", "h", "src/graph/x.rs", "m", None);
    // A closed relation: superseded, so it is no live dependency.
    relation(
        &graph,
        "src/api/y.rs",
        "k",
        "src/render/b.rs",
        "z",
        Some("e2"),
    );
    graph
}

/// One stored co-change pair of a window, as the coupling job writes it.
fn co_change(graph: &GraphStore, a: &str, b: &str, window: &str, count: i64) {
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "MERGE (x:File {repository_id: $repo, path: $a}) \
                 MERGE (y:File {repository_id: $repo, path: $b}) \
                 MERGE (x)-[r:CO_CHANGED {window: $window}]->(y) SET r.count = $count",
                store_graph::params(&json!({"repo": REPO, "a": a, "b": b,
                    "window": window, "count": count})),
            )?;
            Ok(())
        })
        .unwrap();
}

/// One symbol-to-symbol `CALLS` edge, live unless `closed` names the episode
/// that superseded it.
fn relation(
    graph: &GraphStore,
    from_file: &str,
    from: &str,
    to_file: &str,
    to: &str,
    closed: Option<&str>,
) {
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "MERGE (x:Symbol {repository_id: $repo, stable_id: $from}) SET x.relpath = $from_file \
                 MERGE (y:Symbol {repository_id: $repo, stable_id: $to}) SET y.relpath = $to_file \
                 MERGE (x)-[r:CALLS {relpath: $from_file}]->(y) \
                 SET r.valid_from_episode = 'e1', r.valid_to_episode = $closed",
                store_graph::params(&json!({"repo": REPO,
                    "from": format!("{from_file}::{from}"), "to": format!("{to_file}::{to}"),
                    "from_file": from_file, "to_file": to_file, "closed": closed})),
            )?;
            Ok(())
        })
        .unwrap();
}

/// The matrix's rendered view under `params`, in its json view-model.
fn coupling_model(graph: &GraphStore, params: &Params) -> Value {
    render(graph, REPO, COUPLING, params, Format::Json).unwrap()["view"].clone()
}

/// The row of one folder pair, either orientation.
fn cell<'a>(model: &'a Value, a: &str, b: &str) -> &'a Value {
    model["cells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| (row["a"] == a && row["b"] == b) || (row["a"] == b && row["b"] == a))
        .unwrap_or_else(|| panic!("no cell {a} x {b}: {model}"))
}

#[test]
fn the_known_coupled_pair_is_a_hot_cell_and_the_two_mismatches_are_flagged() {
    let graph = coupling_graph();
    let model = coupling_model(&graph, &Params::default());
    // Hot in both matrices: co-change 5 (90d 4) with a static edge, rising
    // from the 90-day window, not flagged, cited by its file pair.
    let known = cell(&model, "src/git_history", "src/viz");
    assert_eq!(known["co_all"], 5, "{known}");
    assert_eq!(known["co_90d"], 4, "{known}");
    assert_eq!(known["static"], 1, "{known}");
    assert_eq!(known["trend"], "rising", "{known}");
    assert_eq!(known["flag"], "none", "{known}");
    assert_eq!(
        known["evidence"],
        "src/git_history/queries.rs + src/viz/mod.rs"
    );
    // Hidden coupling: hot co-change with no structural edge, and the 90-day
    // window has gone quiet on it.
    let hidden = cell(&model, "src/render", "src/store");
    assert_eq!(hidden["flag"], "hidden", "{hidden}");
    assert_eq!(hidden["static"], 0, "{hidden}");
    assert_eq!(hidden["trend"], "cooling", "{hidden}");
    // Stale coupling: a structural edge with no co-change.
    let stale = cell(&model, "src/api", "src/graph");
    assert_eq!(stale["flag"], "stale", "{stale}");
    assert_eq!(stale["co_all"], 0, "{stale}");
    assert_eq!(stale["trend"], "absent", "{stale}");
    // Below the hot threshold, no structural edge: cold, not flagged.
    let cold = cell(&model, ".", "src/viz");
    assert_eq!(cold["co_all"], 2, "{cold}");
    assert_eq!(cold["flag"], "none", "{cold}");
    assert_eq!(cold["trend"], "rising", "{cold}");
    // The flagged sections name exactly the two mismatches.
    let view = super::coupling_matrix::view(&graph, REPO, &Params::default()).unwrap();
    let section = |title: &str| {
        view.sections
            .iter()
            .find(|(name, _)| *name == title)
            .map(|(_, rows)| rows.clone())
            .unwrap()
    };
    assert_eq!(section("hidden coupling").len(), 1);
    assert_eq!(section("hidden coupling")[0]["a"], "src/render");
    assert_eq!(section("stale coupling").len(), 1);
    assert_eq!(section("stale coupling")[0]["b"], "src/graph");
    // The dead relation across src/api and src/render makes no cell, and the
    // file's own relation makes no diagonal cell for src/graph.
    assert!(
        model["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| !(row["a"] == "src/api" && row["b"] == "src/render")
                && !(row["a"] == "src/graph" && row["b"] == "src/graph"))
    );
}

#[test]
fn the_matrix_is_canonical_symmetric_and_lists_each_pair_once() {
    let graph = coupling_graph();
    let model = coupling_model(&graph, &Params::default());
    // Folders: sorted, unique, and every cell's endpoints among them.
    let folders: Vec<&str> = model["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|folder| folder.as_str().unwrap())
        .collect();
    let mut sorted = folders.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(folders, sorted);
    assert_eq!(
        folders,
        [
            ".",
            "src/api",
            "src/git_history",
            "src/graph",
            "src/render",
            "src/store",
            "src/viz"
        ]
    );
    // Each pair once, oriented a <= b, endpoints in the folder list.
    let mut seen = BTreeSet::new();
    for row in model["cells"].as_array().unwrap() {
        let (a, b) = (row["a"].as_str().unwrap(), row["b"].as_str().unwrap());
        assert!(a <= b, "{row}");
        assert!(seen.insert((a.to_owned(), b.to_owned())), "{row}");
        assert!(folders.contains(&a) && folders.contains(&b), "{row}");
    }
    // Symmetric: either orientation of a pair reads the same row.
    for (a, b) in [("src/viz", "src/git_history"), ("src/viz", "src/viz")] {
        assert_eq!(cell(&model, a, b), cell(&model, b, a));
    }
    // The diagonal is one row, not two.
    let diagonal = model["cells"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["a"] == "src/viz" && row["b"] == "src/viz")
        .count();
    assert_eq!(diagonal, 1);
    // The serialization shape: the four fields, the row's readings by name.
    let keys: BTreeSet<&str> = model
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from(["cells", "folders", "focus", "window"])
    );
    let row_keys: BTreeSet<&str> = model["cells"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        row_keys,
        BTreeSet::from([
            "a", "b", "co_90d", "co_all", "evidence", "flag", "static", "trend"
        ])
    );
    // The cap bounds every section.
    let capped = coupling_model(
        &graph,
        &Params {
            cap: 2,
            ..Params::default()
        },
    );
    assert_eq!(capped["cells"].as_array().unwrap().len(), 2);
}

#[test]
fn a_focus_narrows_the_matrix_and_draws_valid_quoted_mermaid() {
    let graph = coupling_graph();
    let focused = |focus: &str| Params {
        focus: Some(focus.to_owned()),
        ..Params::default()
    };
    let all = coupling_model(&graph, &Params::default());
    assert_eq!(all["cells"].as_array().unwrap().len(), 5);
    // A file focus scopes to its folder's row: three cells, all touching
    // src/viz, and the focus folder in the list.
    let narrowed = coupling_model(&graph, &focused("src/viz/mod.rs"));
    let rows = narrowed["cells"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{narrowed}");
    assert!(
        rows.iter()
            .all(|row| row["a"] == "src/viz" || row["b"] == "src/viz")
    );
    assert_eq!(narrowed["focus"]["folder"], "src/viz");
    assert_eq!(narrowed["focus"]["resolved"], true);
    assert_eq!(
        narrowed["folders"],
        json!([".", "src/git_history", "src/viz"])
    );
    // A folder focus names its own row; a focus naming nothing narrows to
    // nothing and draws nothing.
    let hidden = coupling_model(&graph, &focused("src/render"));
    assert_eq!(hidden["cells"].as_array().unwrap().len(), 1);
    let nothing = coupling_model(&graph, &focused("src/nowhere"));
    assert!(nothing["cells"].as_array().unwrap().is_empty());
    assert_eq!(nothing["focus"]["resolved"], false);
    // Mermaid: the focused case only, every label quoted.
    let view = super::coupling_matrix::view(&graph, REPO, &focused("src/viz/mod.rs")).unwrap();
    let mermaid = view.mermaid.unwrap();
    let lines: Vec<&str> = mermaid.lines().collect();
    assert_eq!(lines[0], "graph LR", "{mermaid}");
    assert!(
        mermaid.contains("N0[\"src/viz<br/>src/viz/mod.rs\"]:::focus"),
        "{mermaid}"
    );
    assert_eq!(
        lines.iter().filter(|line| line.contains("[\"")).count(),
        3,
        "{mermaid}"
    );
    assert_eq!(
        lines.iter().filter(|line| line.contains(" ---|\"")).count(),
        3,
        "{mermaid}"
    );
    for line in &lines[1..] {
        if line.trim_start().starts_with("classDef") {
            continue;
        }
        assert_eq!(line.matches('"').count() % 2, 0, "unquoted label: {line}");
    }
    let unfocused = super::coupling_matrix::view(&graph, REPO, &Params::default()).unwrap();
    assert!(unfocused.mermaid.is_none());
    let nothing = super::coupling_matrix::view(&graph, REPO, &focused("src/nowhere")).unwrap();
    assert!(nothing.mermaid.is_none());
}

#[test]
fn the_same_inputs_render_byte_identical_output_for_the_matrix() {
    let graph = coupling_graph();
    let params = Params::parse(&json!({"cap": 10, "window": "90d"})).unwrap();
    let focus = Params::parse(&json!({"focus": "src/viz/mod.rs", "window": "90d"})).unwrap();
    for params in [params, focus] {
        for format in [Format::Text, Format::Json, Format::Mermaid] {
            let first =
                serde_json::to_string(&render(&graph, REPO, COUPLING, &params, format).unwrap())
                    .unwrap();
            let again =
                serde_json::to_string(&render(&graph, REPO, COUPLING, &params, format).unwrap())
                    .unwrap();
            assert_eq!(first, again, "{format:?}");
        }
    }
}

#[test]
fn the_window_reads_the_hot_threshold_and_the_flags_with_it() {
    let graph = coupling_graph();
    // In the 90-day window the hidden pair has no co-change at all, so it is
    // cold there and not flagged; the window names the reading.
    let recent = coupling_model(
        &graph,
        &Params {
            window: Window::NinetyDays,
            ..Params::default()
        },
    );
    assert_eq!(cell(&recent, "src/render", "src/store")["flag"], "none");
    assert_eq!(cell(&recent, "src/render", "src/store")["co_90d"], 0);
    assert_eq!(recent["window"], "90d");
}
