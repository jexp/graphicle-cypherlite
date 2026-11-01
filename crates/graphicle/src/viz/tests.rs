//! The pack's contract tests: the registry, the parameter contract, the
//! dispatch and the `viz_view` tool surface, on stores seeded with a
//! repository and minimal facts. The family nodes' own tests extend the
//! [`graph`] fixture with the facts their views read.

use super::*;
use crate::api::{self, App};
use crate::git_history::queries;
use crate::model;
use crate::store::s;
use crate::store_graph;
use crate::supply_chain::resolve;
use crate::supply_chain::tools_tests::{APP, Fixture, RUSTSEC_ID, import};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::result::Result;
use tempfile::TempDir;

/// The repository the fixtures seed.
const REPO: &str = "viz";
/// The coupling matrix's slug: its view is served by its own module.
const COUPLING: &str = "coupling-matrix";

/// The impact-neighborhood family's slug.
const IMPACT: &str = "impact-neighborhood";

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
                     SET c.subject = $subject, c.author_date = $date, \
                         c.commit_date = $date \
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

/// The city fixture: the base history plus a churn-heavy folder, a quiet
/// one and a root-level file, with dated commits so the 90-day window
/// (trailing the newest commit) holds fewer changes than all of history.
///
/// ```text
/// commit      date         files
/// 0001        2026-01-01   src/lib.rs                       (outside 90d)
/// 0002        2026-03-01   src/hot/engine.rs, README.md     (outside 90d)
/// 0003        2026-06-01   src/hot/engine.rs, src/hot/pump.rs
/// 0004        2026-06-02   src/hot/engine.rs, src/hot/pump.rs, src/hot/valve.rs
/// 0005        2026-08-20   src/hot/engine.rs, src/cool/vent.rs  (the newest)
/// ```
///
/// All-time churn: engine.rs 4, pump.rs 2, valve.rs/vent.rs/lib.rs/README
/// 1 each; inside the 90 days trailing 2026-08-20 the engine holds 3 and
/// the two 2026-01/03 commits age out.
fn city() -> GraphStore {
    let graph = graph();
    let seed = |oid: &str, date: &str, paths: &[&str]| {
        graph
            .transaction(|db| {
                for path in paths {
                    crate::store_graph::write(
                        db,
                        "MERGE (c:Commit {repository_id: $repo, oid: $oid}) \
                             SET c.commit_date = $date \
                         MERGE (ch:Change {repository_id: $repo, commit: $oid, path: $path}) \
                             SET ch.kind = 'M' \
                         MERGE (f:File {repository_id: $repo, path: $path}) \
                         MERGE (c)-[:CONTAINS_CHANGE]->(ch) MERGE (ch)-[:MODIFIES]->(f)",
                        crate::store_graph::params(&json!({"repo": REPO, "oid": oid,
                            "path": path, "date": date})),
                    )?;
                }
                Ok(())
            })
            .unwrap();
    };
    seed(
        "0000000000000000000000000000000000000002",
        "2026-03-01T00:00:00Z",
        &["src/hot/engine.rs", "README.md"],
    );
    seed(
        "0000000000000000000000000000000000000003",
        "2026-06-01T00:00:00Z",
        &["src/hot/engine.rs", "src/hot/pump.rs"],
    );
    seed(
        "0000000000000000000000000000000000000004",
        "2026-06-02T00:00:00Z",
        &["src/hot/engine.rs", "src/hot/pump.rs", "src/hot/valve.rs"],
    );
    seed(
        "0000000000000000000000000000000000000005",
        "2026-08-20T00:00:00Z",
        &["src/hot/engine.rs", "src/cool/vent.rs"],
    );
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
fn every_family_is_listed_and_reads_its_report() {
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
        // A resource answers on a repository without its facts, never an
        // error and never empty text: the empty-fixture report of every
        // family, the focus families' note included.
        let report = report(&graph, REPO, family.slug).unwrap();
        assert!(!report.trim().is_empty(), "{}", family.slug);
        if family.needs_focus {
            // The tool path keeps the typed refusal for a focus family's
            // call without a focus.
            let refused = render(&graph, REPO, family.slug, &Params::default(), Format::Json);
            assert_eq!(refused.unwrap_err().message, NEEDS_FOCUS);
            continue;
        }
        let packet = render(&graph, REPO, family.slug, &Params::default(), Format::Json).unwrap();
        assert_eq!(packet["family"], family.slug);
        assert!(packet["view"].is_object(), "{}", family.slug);
    }
}

#[test]
fn a_focus_family_resource_answers_the_needs_a_focus_note() {
    let graph = graph();
    // The resource has no focus to center on, so it answers the note; the
    // tool path keeps the typed refusal for the same ask.
    let note = report(&graph, REPO, IMPACT).unwrap();
    assert!(note.starts_with("needs a focus:\n"), "{note}");
    assert!(note.contains("needs a focus: a symbol's"), "{note}");
    let refused = render(&graph, REPO, IMPACT, &Params::default(), Format::Json).unwrap_err();
    assert_eq!(refused.message, NEEDS_FOCUS);
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
    // A declared family answers its diagram (empty without a focus, which
    // is a valid answer), a focus family refuses a call without its focus,
    // and the undeclared ones are refused by name.
    for family in FAMILIES.iter().filter(|family| family.mermaid) {
        if family.needs_focus {
            let refused = render(
                &graph,
                REPO,
                family.slug,
                &Params::default(),
                Format::Mermaid,
            );
            assert_eq!(refused.unwrap_err().message, NEEDS_FOCUS);
            continue;
        }
        let answer = render(
            &graph,
            REPO,
            family.slug,
            &Params::default(),
            Format::Mermaid,
        )
        .unwrap();
        assert!(answer["mermaid"].is_string(), "{}", family.slug);
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
    // echoed; the landed family's model answers under its layout id (an
    // empty city here: the project holds no history facts), and the text
    // form serves the model's own JSON.
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
    assert_eq!(
        answer["structuredContent"]["view"]["layout"],
        hotspot_city::LAYOUT
    );
    assert_eq!(
        answer["content"][0]["text"],
        answer["structuredContent"]["view"].to_string()
    );
    assert_eq!(answer["isError"], false);
    // The text format serves the report; mermaid the declared families'
    // diagram; both carry the whole answer as structured content.
    let text = project
        .call(json!({"family":"drift-timeline","format":"text"}))
        .unwrap();
    assert_eq!(text["structuredContent"]["format"], "text");
    assert_eq!(
        text["content"][0]["text"],
        "drift by month (commit author_date, UTC):\n  none"
    );
    // Every mermaid-declared family has landed; the tool surface answers
    // the focus contract for them (their own tests cover the diagram).
    let needs_focus = project
        .call(json!({"family":"impact-neighborhood","format":"mermaid"}))
        .unwrap_err();
    assert_eq!(
        needs_focus,
        "The impact-neighborhood family needs a focus: a symbol's stable_id or qualified name, or a file path"
    );
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

/// Seeds one commit per `(author email, author name, path)` change, with the
/// history writer's labels: the author's `Person` and `AUTHORED` edge, a
/// kind-`M` change, the file it modifies. The ownership statements read these.
fn seed_history(graph: &GraphStore, repository: &str, changes: &[(&str, &str, &str)]) {
    graph
        .transaction(|db| {
            for (index, (email, name, path)) in changes.iter().enumerate() {
                crate::store_graph::write(
                    db,
                    "MERGE (a:Person {repository_id: $repo, email: $email}) SET a.name = $name \
                     MERGE (c:Commit {repository_id: $repo, oid: $oid}) \
                     MERGE (a)-[:AUTHORED]->(c) \
                     MERGE (ch:Change {repository_id: $repo, commit: $oid, path: $path}) \
                     SET ch.kind = 'M' \
                     MERGE (f:File {repository_id: $repo, path: $path}) \
                     MERGE (c)-[:CONTAINS_CHANGE]->(ch) MERGE (ch)-[:MODIFIES]->(f)",
                    crate::store_graph::params(&json!({"repo": repository,
                        "oid": format!("{:040}", index + 1), "email": email,
                        "name": name, "path": path})),
                )?;
            }
            Ok(())
        })
        .unwrap();
}

// -- The drift timeline (`drift_timeline.rs`) ---------------------------------

use crate::git_history::fixture::{Repo, ada, bob};
use crate::git_history::{coupling, writer};

/// A repository of `graph` with its `:Project`, as the writer's tests seed one.
fn project_node(graph: &GraphStore, repository: &str) {
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "CREATE (p:Project {id: $id, workspace_id: 'ws', project_key: $id})",
                store_graph::params(&json!({"id": repository})),
            )?;
            Ok(())
        })
        .unwrap();
}

/// Seed one commit of `repository` at `date` that changed `paths` (kind `M`).
fn seed_commit(graph: &GraphStore, repository: &str, oid: &str, date: &str, paths: &[&str]) {
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "MERGE (c:Commit {repository_id: $repo, oid: $oid}) \
                 SET c.subject = $oid, c.author_date = $date",
                store_graph::params(&json!({"repo": repository, "oid": oid, "date": date})),
            )?;
            for path in paths {
                store_graph::write(
                    db,
                    "MATCH (c:Commit {repository_id: $repo, oid: $oid}) \
                     MERGE (ch:Change {repository_id: $repo, commit: $oid, path: $path}) \
                     SET ch.kind = 'M' \
                     MERGE (c)-[:CONTAINS_CHANGE]->(ch)",
                    store_graph::params(&json!({"repo": repository, "oid": oid, "path": path})),
                )?;
            }
            Ok(())
        })
        .unwrap();
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

/// One symbol of a code-graph fixture: its stable id `id-<name>`, its
/// qualified name, its file.
fn symbol(graph: &GraphStore, repository: &str, name: &str, path: &str) {
    graph
        .transaction(|db| {
            crate::store_graph::write(
                db,
                "MERGE (s:Symbol {repository_id: $repo, stable_id: $id}) \
                 SET s.qualified_name = $name, s.relpath = $path, s.kind = 'function'",
                crate::store_graph::params(&json!({"repo": repository,
                    "id": format!("id-{name}"), "name": name, "path": path})),
            )?;
            Ok(())
        })
        .unwrap();
}

/// The knowledge fixture: Ada owns `src/core/auth.rs` alone (four changes),
/// Ada and Bob share `src/core/util.rs` (one each), Bob alone changes
/// `docs/readme.md` (one). Churn: 4, 2, 1; bus factor: 1, 2, 1.
fn knowledge_graph() -> GraphStore {
    let graph = GraphStore::open_in_memory().unwrap();
    let ada = ("ada@example.com", "Ada", "src/core/auth.rs");
    let mut changes = vec![ada; 4];
    changes.push(("ada@example.com", "Ada", "src/core/util.rs"));
    changes.push(("bob@example.com", "Bob", "src/core/util.rs"));
    changes.push(("bob@example.com", "Bob", "docs/readme.md"));
    seed_history(&graph, REPO, &changes);
    graph
}

/// The knowledge-map answer for `params`, its view-model.
fn knowledge_view(graph: &GraphStore, params: &Params) -> Value {
    render(graph, REPO, "knowledge-map", params, Format::Json).unwrap()["view"].clone()
}

#[test]
fn a_sole_owner_high_churn_file_lands_in_the_loss_risk_quadrant() {
    let view = knowledge_view(&knowledge_graph(), &Params::default());
    // The median split: churn at or above the median churn (2) is high,
    // bus factor at or below the median bus factor (1) is low knowledge.
    assert_eq!(
        view["thresholds"],
        json!({"churn_median": 2.0, "bus_factor_median": 1.0})
    );
    let files = view["files"].as_array().unwrap();
    assert_eq!(files[0]["evidence"], "src/core/auth.rs");
    assert_eq!(files[0]["churn"], 4);
    assert_eq!(files[0]["bus_factor"], 1);
    assert_eq!(files[0]["quadrant"], "loss-risk");
    // Two authors split one file: high churn, high knowledge.
    assert_eq!(files[1]["evidence"], "src/core/util.rs");
    assert_eq!(files[1]["bus_factor"], 2);
    assert_eq!(files[1]["quadrant"], "shared-hot");
    // One author, few changes: low churn, low knowledge.
    assert_eq!(files[2]["evidence"], "docs/readme.md");
    assert_eq!(files[2]["quadrant"], "dormant");
    assert_eq!(
        view["quadrants"],
        json!({"loss-risk": 1, "shared-hot": 1, "dormant": 1})
    );
    // The cap cuts the list, never the quadrant counts.
    let capped = knowledge_view(
        &knowledge_graph(),
        &Params::parse(&json!({"cap": 1})).unwrap(),
    );
    assert_eq!(capped["files"].as_array().unwrap().len(), 1);
    assert_eq!(capped["quadrants"], view["quadrants"]);
}

#[test]
fn the_bus_factor_agrees_with_the_history_ownership_family() {
    let graph = knowledge_graph();
    let view = knowledge_view(&graph, &Params::default());
    let history = queries::sections(&graph, REPO, "ownership").unwrap();
    let theirs = &history[0].1;
    for file in view["files"].as_array().unwrap() {
        let row = theirs
            .iter()
            .find(|row| row["evidence"] == file["evidence"])
            .unwrap_or_else(|| panic!("no ownership row for {}", file["evidence"]));
        assert_eq!(file["owner_email"], row["owner_email"]);
        assert_eq!(file["ownership"], row["ownership"]);
        assert_eq!(file["total"], row["total"]);
        assert_eq!(file["contributors"], row["contributors"]);
        // A bus factor of one is exactly the history family's sole owner.
        assert_eq!(file["bus_factor"] == 1, row["sole_owner"], "{}", row);
    }
    // The folder bus factor: Ada holds 5 of the 6 touches in src/core.
    let folders = view["folders"].as_array().unwrap();
    assert_eq!(folders[0]["evidence"], "src/core");
    assert_eq!(folders[0]["bus_factor"], 1);
    assert_eq!(folders[0]["total"], 6);
    assert_eq!(folders[1]["evidence"], "docs");
    assert_eq!(folders[1]["bus_factor"], 1);
}

#[test]
fn the_knowledge_map_is_byte_identical_and_anchored_on_its_repository() {
    let graph = knowledge_graph();
    let params = Params::parse(&json!({"overlay": "knowledge", "cap": 5})).unwrap();
    let reordered = Params::parse(&json!({"cap": 5, "overlay": "knowledge"})).unwrap();
    let first = serde_json::to_string(
        &render(&graph, REPO, "knowledge-map", &params, Format::Json).unwrap(),
    )
    .unwrap();
    let second = serde_json::to_string(
        &render(&graph, REPO, "knowledge-map", &reordered, Format::Json).unwrap(),
    )
    .unwrap();
    assert_eq!(first, second);
    let text_first = render(&graph, REPO, "knowledge-map", &params, Format::Text).unwrap();
    let text_again = render(&graph, REPO, "knowledge-map", &reordered, Format::Text).unwrap();
    assert_eq!(text_first, text_again);
    // The citation rule: every row is cited by its path, and the report
    // names the row's path first.
    let report = text_first["text"].as_str().unwrap();
    assert!(
        report.contains("  src/core/auth.rs  quadrant=loss-risk"),
        "{report}"
    );
    // The overlay adds the per-author breakdown.
    let view = knowledge_view(&graph, &params);
    assert_eq!(view["files"][1]["authors"].as_array().unwrap().len(), 2);
    // The supernode rule: another repository's history never reaches the
    // join, so its changes leave this repository's numbers alone.
    let before = knowledge_view(&graph, &Params::default());
    seed_history(
        &graph,
        "other",
        &[("bob@example.com", "Bob", "src/core/auth.rs"); 5],
    );
    assert_eq!(knowledge_view(&graph, &Params::default()), before);
    // A risk overlay is the risk-overlay family's column, refused by name.
    let refused = render(
        &graph,
        REPO,
        "knowledge-map",
        &Params::parse(&json!({"overlay": "risk"})).unwrap(),
        Format::Json,
    )
    .unwrap_err();
    assert!(
        refused.message.contains("risk-overlay"),
        "{}",
        refused.message
    );
}

// The `risk-overlay` family, on the supply-chain facts the tools fixture
// seeds: `src/app.rs` imports `time` on line 3 (locked at 0.2.22, which
// GHSA-wcg3-cvx6-7396 and RUSTSEC-2020-0071 affect; the fix is 0.2.23 for
// RUSTSEC) and `serde` on line 1 (clean).

const TIME_KEY: &str = "pkg:cargo/time@0.2.22";

/// The default fixture with a second file importing `time` on line 7.
fn two_files() -> Fixture {
    let fixture = Fixture::new();
    import(
        &fixture.graph(),
        &fixture.repo,
        "src/other.rs",
        7,
        "time::Duration",
    );
    resolve::resolve(&fixture.graph(), &fixture.repo).unwrap();
    fixture
}

/// The entry of `key` in a model list, by its `field`.
fn entry<'a>(list: &'a Value, field: &str, key: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|e| e[field] == key)
        .unwrap_or_else(|| panic!("no {key} in {list}"))
}

#[test]
fn risk_overlay_advisories_match_vuln_impact_for_the_same_files_and_fixed_in() {
    let fixture = Fixture::new();
    let view = risk_overlay::view(&fixture.graph(), &fixture.repo, &Params::default()).unwrap();
    let advisories = &view.model["advisories"];
    assert_eq!(advisories.as_array().unwrap().len(), 2, "{}", view.model);
    let time = fixture
        .call("vuln_impact", json!({"package": "time"}))
        .unwrap();
    for advisory in advisories.as_array().unwrap() {
        let osv = advisory["osv_id"].as_str().unwrap();
        let impact = fixture
            .call("vuln_impact", json!({"advisory": osv}))
            .unwrap();
        // The same files, cited the same way, and the same advisory record.
        assert_eq!(advisory["files"], impact["files"], "{osv}");
        assert_eq!(advisory["exposure"], impact["summary"]["files"], "{osv}");
        assert_eq!(
            advisory["citations"], impact["summary"]["citations"],
            "{osv}"
        );
        assert_eq!(advisory["title"], impact["advisory"]["title"], "{osv}");
        assert_eq!(advisory["url"], impact["advisory"]["url"], "{osv}");
        // Each version's fixed_in is the one the package answer names.
        for version in advisory["versions"].as_array().unwrap() {
            let locked = entry(
                &time["versions"],
                "purl_at_version",
                version["purl_at_version"].as_str().unwrap(),
            );
            let record = entry(&locked["advisories"], "osv_id", osv);
            assert_eq!(version["fixed_in"], record["fixed_in"], "{osv}");
        }
    }
    let rustsec = entry(advisories, "osv_id", RUSTSEC_ID);
    assert_eq!(rustsec["versions"][0]["purl_at_version"], TIME_KEY);
    assert_eq!(rustsec["versions"][0]["fixed_in"], "0.2.23");
    // The file's advisories carry the same fixed_in.
    let file = &view.model["files"][0];
    assert_eq!(file["path"], APP);
    assert_eq!(file["exposure"], 2);
    let in_file = entry(&file["advisories"], "osv_id", RUSTSEC_ID);
    assert_eq!(in_file["fixed_in"], "0.2.23");
}

#[test]
fn risk_overlay_column_is_the_ranked_list_projected_per_file() {
    let fixture = two_files();
    let view = risk_overlay::view(&fixture.graph(), &fixture.repo, &Params::default()).unwrap();
    let files = view.model["files"].as_array().unwrap();
    // Both files reach the same two advisories: ties break on the evidence.
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, [APP, "src/other.rs"]);
    assert_eq!(files[1]["evidence"], "src/other.rs:7");
    // The column is the list's own per-file projection, value for value.
    let column: Vec<Value> = files
        .iter()
        .map(|f| json!({"path": f["path"], "value": f["exposure"]}))
        .collect();
    assert_eq!(view.model["overlay"]["values"], json!(column));
    // The section prints the same rows in the same order.
    let rows = &view.sections[0].1;
    assert_eq!(rows.len(), files.len());
    for (row, file) in rows.iter().zip(files) {
        assert_eq!(row["file"], file["path"]);
        assert_eq!(row["exposure"], file["exposure"]);
    }
    // A cap cuts the list and the column together.
    let capped = Params {
        cap: 1,
        ..Params::default()
    };
    let view = risk_overlay::view(&fixture.graph(), &fixture.repo, &capped).unwrap();
    assert_eq!(view.model["files"].as_array().unwrap().len(), 1);
    assert_eq!(view.model["overlay"]["values"].as_array().unwrap().len(), 1);
    assert_eq!(view.sections[0].1.len(), 1);
    // The summary counts what the cap does not cut.
    assert_eq!(view.model["summary"]["files"], 2);
}

#[test]
fn risk_overlay_focus_draws_a_valid_quoted_exposure_subgraph() {
    let fixture = two_files();
    let graph = fixture.graph();
    for focus in ["time", APP, "time::OffsetDateTime"] {
        let params = Params {
            focus: Some(focus.to_owned()),
            ..Params::default()
        };
        let view = risk_overlay::view(&graph, &fixture.repo, &params).unwrap();
        let mermaid = view
            .mermaid
            .unwrap_or_else(|| panic!("no mermaid for {focus}"));
        assert!(mermaid.starts_with("graph LR\n"), "{mermaid}");
        let mut declared = BTreeSet::new();
        for line in mermaid.lines().skip(1) {
            if let Some((id, label)) = line.trim().split_once("[\"") {
                // A node: quoted, its quotes balanced, its citation or name in it.
                assert!(label.ends_with("\"]"), "{line}");
                assert_eq!(line.matches('"').count(), 2, "{line}");
                declared.insert(id.to_owned());
            } else {
                assert!(line.contains(" --> "), "{line}");
            }
        }
        for line in mermaid.lines().skip(1).filter(|l| l.contains(" --> ")) {
            let (from, to) = line.trim().split_once(" --> ").unwrap();
            assert!(declared.contains(from) && declared.contains(to), "{line}");
        }
        assert!(mermaid.contains("[\"src/app.rs:3\"]"), "{mermaid}");
        assert!(
            mermaid.contains(&format!("[\"{RUSTSEC_ID}\"]")),
            "{mermaid}"
        );
    }
    // No focus: no diagram. A focus no import resolves: no diagram, named.
    let unfocused = risk_overlay::view(&graph, &fixture.repo, &Params::default()).unwrap();
    assert!(unfocused.mermaid.is_none());
    let absent = Params {
        focus: Some("nothing_here".to_owned()),
        ..Params::default()
    };
    let view = risk_overlay::view(&graph, &fixture.repo, &absent).unwrap();
    assert!(view.mermaid.is_none());
    assert!(
        view.model["not_known"][0]
            .as_str()
            .unwrap()
            .contains("nothing_here")
    );
    // The focus scopes the list too: serde (clean) has no exposure at all.
    let serde = Params {
        focus: Some("serde".to_owned()),
        ..Params::default()
    };
    let view = risk_overlay::view(&graph, &fixture.repo, &serde).unwrap();
    assert_eq!(view.model["files"], json!([]));
    assert!(view.mermaid.is_none());
}

#[test]
fn risk_overlay_is_deterministic_cited_and_repository_bound() {
    let fixture = two_files();
    let graph = fixture.graph();
    let params = Params::parse(&json!({"cap": 10, "focus": "time"})).unwrap();
    let reordered = Params::parse(&json!({"focus": "time", "cap": 10})).unwrap();
    for (first_params, second_params) in
        [(Params::default(), Params::default()), (params, reordered)]
    {
        let first = risk_overlay::view(&graph, &fixture.repo, &first_params).unwrap();
        let second = risk_overlay::view(&graph, &fixture.repo, &second_params).unwrap();
        assert_eq!(
            serde_json::to_string(&first.model).unwrap(),
            serde_json::to_string(&second.model).unwrap()
        );
        assert_eq!(first.mermaid, second.mermaid);
        assert_eq!(
            serde_json::to_string(&first.sections).unwrap(),
            serde_json::to_string(&second.sections).unwrap()
        );
    }
    // The text and json forms of the family serve the same bytes twice.
    let text = report(&graph, &fixture.repo, "risk-overlay").unwrap();
    assert_eq!(text, report(&graph, &fixture.repo, "risk-overlay").unwrap());
    assert!(
        text.contains("src/app.rs:3") && text.contains(RUSTSEC_ID),
        "{text}"
    );
    // Citation rule: every ranked row names its file and line.
    let view = risk_overlay::view(&graph, &fixture.repo, &Params::default()).unwrap();
    for file in view.model["files"].as_array().unwrap() {
        let path = file["path"].as_str().unwrap();
        assert!(
            file["evidence"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{path}:")),
            "{file}"
        );
    }
    for (_, rows) in &view.sections {
        for row in rows {
            assert!(
                !row["evidence"].as_str().unwrap_or_default().is_empty(),
                "{row}"
            );
        }
    }
    // Repository-bound: another project on the same store sees none of these
    // facts (its own not-scanned note, no rows), and the view never reads
    // through (:Project).
    let other = fixture
        .store
        .register(
            &fixture.workspace,
            &fixture.key,
            &json!({"project_key": "other", "name": "other"}),
        )
        .unwrap();
    let view = risk_overlay::view(&graph, s(&other, "id"), &Params::default()).unwrap();
    assert_eq!(view.model["files"], json!([]));
    assert_eq!(view.model["summary"]["citations"], 0);
    assert!(
        view.model["not_known"][0]
            .as_str()
            .unwrap()
            .contains("OSV has not scanned")
    );
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

/// One live edge `from -> to` of `relation` (the projection's live-edge keys:
/// written from episode e1, not yet closed), its call site `path:line`.
fn edge(
    graph: &GraphStore,
    repository: &str,
    relation: &str,
    from: &str,
    to: &str,
    path: &str,
    line: usize,
) {
    let cypher = format!(
        "MATCH (a:Symbol {{repository_id: $repo, stable_id: $from}}) \
         MATCH (b:Symbol {{repository_id: $repo, stable_id: $to}}) \
         MERGE (a)-[r:{relation} {{relpath: $path, source_location: $loc}}]->(b) \
         SET r.valid_from_episode = 'e1', r.relation = $relation"
    );
    graph
        .transaction(|db| {
            crate::store_graph::write(
                db,
                &cypher,
                crate::store_graph::params(&json!({"repo": repository,
                    "from": format!("id-{from}"), "to": format!("id-{to}"),
                    "path": path, "loc": format!("L{line}"),
                    "relation": relation.to_lowercase()})),
            )?;
            Ok(())
        })
        .unwrap();
}

/// Seed one episode of `repository`, projected at `created_at`.
fn seed_episode(
    graph: &GraphStore,
    repository: &str,
    episode: &str,
    sequence: i64,
    created_at: &str,
) {
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "CREATE (e:Episode {repository_id: $repo, episode: $episode, sequence: $sequence, created_at: $at})",
                store_graph::params(&json!({"repo": repository, "episode": episode,
                    "sequence": sequence, "at": created_at})),
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
            .all(|row| !(row["a"] == "src/api" && row["b"] == "src/render"
                || row["a"] == "src/graph" && row["b"] == "src/graph"))
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

/// A chain of `length` symbols `m0 -> m1 -> ... -> m{length-1}`, one file
/// each (`src/m{i}.rs`), the call from `m{i}` on its line `i + 10`. From
/// `m0`, hop `k` is `m{k}`.
fn chain(length: usize) -> GraphStore {
    let graph = GraphStore::open_in_memory().unwrap();
    for i in 0..length {
        symbol(&graph, REPO, &format!("m{i}"), &format!("src/m{i}.rs"));
    }
    for i in 0..length - 1 {
        edge(
            &graph,
            REPO,
            "CALLS",
            &format!("m{i}"),
            &format!("m{}", i + 1),
            &format!("src/m{i}.rs"),
            i + 10,
        );
    }
    graph
}

/// The impact-neighborhood parameters of `focus` at `depth`.
fn focus_params(focus: &str, depth: usize) -> Params {
    Params::parse(&json!({"focus": focus, "depth": depth})).unwrap()
}

/// The node ids of a view-model's `nodes`.
fn node_ids(model: &Value) -> BTreeSet<String> {
    model["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The expected ids `id-m0 .. id-m{count-1}`.
fn chain_ids(count: usize) -> BTreeSet<String> {
    (0..count).map(|i| format!("id-m{i}")).collect()
}

/// Asserts `mermaid` is a flowchart this family writes: the header, quoted
/// node lines and quoted edge lines between declared nodes, no raw quote or
/// angle bracket inside a label.
fn assert_flowchart(mermaid: &str) {
    let mut lines = mermaid.lines();
    assert_eq!(lines.next(), Some("flowchart LR"), "{mermaid}");
    let mut declared = BTreeSet::new();
    for line in lines {
        let rest = line
            .strip_prefix("  n")
            .unwrap_or_else(|| panic!("not a flowchart line: {line}"));
        if let Some((index, label)) = rest.split_once("[\"") {
            let inner = label
                .strip_suffix("\"]")
                .unwrap_or_else(|| panic!("unclosed node label: {line}"));
            let inner = inner.replace("<br/>", "");
            assert!(!inner.contains(['"', '<', '>']), "unescaped label: {line}");
            assert!(index.chars().all(|c| c.is_ascii_digit()), "{line}");
            declared.insert(index.to_owned());
        } else {
            let (from, rest) = rest
                .split_once(" -->|\"")
                .unwrap_or_else(|| panic!("not a node or edge line: {line}"));
            let (label, to) = rest
                .split_once("\"| n")
                .unwrap_or_else(|| panic!("unclosed edge label: {line}"));
            assert!(!label.contains(['"', '<', '>']), "unescaped label: {line}");
            assert!(declared.contains(from), "undeclared source: {line}");
            assert!(declared.contains(to), "undeclared target: {line}");
        }
    }
}

/// Two layout numbers close enough to share the model's hundredth.
fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

/// The engine row of a model, in the window named.
fn engine(graph: &GraphStore, window: &str) -> Value {
    render(
        graph,
        REPO,
        "hotspot-city",
        &Params::parse(&json!({"window": window})).unwrap(),
        Format::Json,
    )
    .unwrap()["view"]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["file"] == "src/hot/engine.rs")
        .unwrap()
        .clone()
}

#[test]
fn the_city_re_renders_byte_identically_and_versions_its_layout() {
    let graph = city();
    let params = Params::parse(&json!({"window": "90d", "cap": 10})).unwrap();
    let reordered = Params::parse(&json!({"cap": 10, "window": "90d"})).unwrap();
    for format in [Format::Text, Format::Json] {
        let first =
            serde_json::to_string(&render(&graph, REPO, "hotspot-city", &params, format).unwrap())
                .unwrap();
        let second = serde_json::to_string(
            &render(&graph, REPO, "hotspot-city", &reordered, format).unwrap(),
        )
        .unwrap();
        let again =
            serde_json::to_string(&render(&graph, REPO, "hotspot-city", &params, format).unwrap())
                .unwrap();
        assert_eq!(
            first, second,
            "the parameters, named in another order: {format:?}"
        );
        assert_eq!(first, again, "a re-render of the same inputs: {format:?}");
    }
    // The layout id versions the algorithm in every model.
    let packet = render(&graph, REPO, "hotspot-city", &params, Format::Json).unwrap();
    assert_eq!(packet["view"]["layout"], hotspot_city::LAYOUT);
    // And the equality is not vacuous: the window keys the color channel,
    // so the 90-day city colors fewer of the engine's changes than all of
    // history does, over the same height.
    assert_eq!(engine(&graph, "all")["churn_window"], json!(4));
    assert_eq!(engine(&graph, "90d")["churn_window"], json!(3));
    assert_eq!(engine(&graph, "90d")["height"], json!(4));
    assert_eq!(engine(&graph, "90d")["heat"], json!(1.0));
}

#[test]
fn the_known_hotspot_is_a_tall_hot_cell_matching_the_squarify_result() {
    let graph = city();
    let view = render(
        &graph,
        REPO,
        "hotspot-city",
        &Params::default(),
        Format::Json,
    )
    .unwrap();
    let model = &view["view"];
    assert_eq!(model["layout"], hotspot_city::LAYOUT);

    // The canonical sort the layout runs on: districts by value with ties
    // by path — src/hot's 7, then the three 1s, `.` before `src` before
    // `src/cool` — and the model's district bounds are its squarify result.
    let canvas = hotspot_city::Rect {
        x: 0.0,
        y: 0.0,
        w: 1000.0,
        h: 1000.0,
    };
    let districts: Vec<(String, f64)> = [
        ("src/hot", 7.0),
        (".", 1.0),
        ("src", 1.0),
        ("src/cool", 1.0),
    ]
    .iter()
    .map(|(name, value)| (name.to_string(), *value))
    .collect();
    let cells = hotspot_city::squarify(&districts, canvas);
    let rows = model["districts"].as_array().unwrap();
    assert_eq!(rows.len(), cells.len());
    for (row, (name, cell)) in rows.iter().zip(&cells) {
        assert_eq!(row["district"], name.as_str(), "{row}");
        for (key, value) in [("x", cell.x), ("y", cell.y), ("w", cell.w), ("h", cell.h)] {
            assert!(
                close(row["bounds"][key].as_f64().unwrap(), value),
                "{key} of {row}"
            );
        }
        let aspect = (cell.w / cell.h).max(cell.h / cell.w);
        assert!(close(row["aspect"].as_f64().unwrap(), aspect), "{row}");
    }

    // The members of the churn-heavy district over its bounds, likewise.
    let hot = cells.iter().find(|(name, _)| name == "src/hot").unwrap().1;
    let members: Vec<(String, f64)> = [
        ("src/hot/engine.rs", 4.0),
        ("src/hot/pump.rs", 2.0),
        ("src/hot/valve.rs", 1.0),
    ]
    .iter()
    .map(|(path, value)| (path.to_string(), *value))
    .collect();
    let member_cells = hotspot_city::squarify(&members, hot);
    let rows = model["members"].as_array().unwrap();
    for (row, (path, cell)) in rows.iter().take(member_cells.len()).zip(&member_cells) {
        assert_eq!(row["file"], path.as_str(), "{row}");
        for (key, value) in [("x", cell.x), ("y", cell.y), ("w", cell.w), ("h", cell.h)] {
            assert!(
                close(row["cell"][key].as_f64().unwrap(), value),
                "{key} of {row}"
            );
        }
    }

    // The known hotspot is a tall hot cell: the height channel's maximum,
    // the largest cell of the city, and the color channel's hottest heat.
    let engine = rows
        .iter()
        .find(|row| row["file"] == "src/hot/engine.rs")
        .unwrap();
    let area =
        |row: &Value| row["cell"]["w"].as_f64().unwrap() * row["cell"]["h"].as_f64().unwrap();
    assert_eq!(engine["height"], json!(4));
    assert_eq!(engine["churn_window"], json!(4));
    assert_eq!(engine["heat"], json!(1.0));
    assert!(
        rows.iter().all(|row| area(row) <= area(engine) + 0.01),
        "{engine}"
    );
    // Squarified, not degenerate: roughly square, in the district that
    // holds the city's seven-tenths.
    assert!(engine["aspect"].as_f64().unwrap() < 2.0, "{engine}");
    let district = model["districts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["district"] == "src/hot")
        .unwrap();
    assert!(close(
        district["bounds"]["w"].as_f64().unwrap() * district["bounds"]["h"].as_f64().unwrap(),
        700_000.0
    ));
}

#[test]
fn the_squarify_layout_tiles_the_canvas_over_the_canonical_sort() {
    let canvas = hotspot_city::Rect {
        x: 0.0,
        y: 0.0,
        w: 1000.0,
        h: 1000.0,
    };
    let items: Vec<(String, f64)> = [("d", 7.0), ("a", 1.0), ("b", 1.0), ("c", 1.0)]
        .iter()
        .map(|(key, value)| (key.to_string(), *value))
        .collect();
    let cells = hotspot_city::squarify(&items, canvas);
    // Every item gets its cell, in the order it was given.
    assert_eq!(cells.len(), items.len());
    assert!(
        cells
            .iter()
            .zip(&items)
            .all(|((name, _), (key, _))| name == key)
    );
    // The cells tile the canvas, each with its share of the area, and
    // none leaves the canvas.
    let total: f64 = cells.iter().map(|(_, cell)| cell.w * cell.h).sum();
    assert!(close(total, 1_000_000.0));
    for ((name, cell), (_, value)) in cells.iter().zip(&items) {
        assert!(close(cell.w * cell.h, value / 10.0 * 1_000_000.0), "{name}");
        assert!(
            cell.x >= -0.01
                && cell.y >= -0.01
                && cell.x + cell.w <= 1000.01
                && cell.y + cell.h <= 1000.01,
            "{name} leaves the canvas: {cell:?}"
        );
    }
    // The same inputs lay out the same cells, and a single item takes the
    // whole canvas.
    assert_eq!(hotspot_city::squarify(&items, canvas), cells);
    assert_eq!(
        hotspot_city::squarify(&[("only".to_string(), 5.0)], canvas),
        vec![("only".to_string(), canvas)]
    );
}

#[test]
fn every_city_row_cites_its_path_and_the_sections_are_capped() {
    let graph = city();
    let params = Params::parse(&json!({"cap": 2})).unwrap();
    let view = hotspot_city::view(&graph, REPO, &params).unwrap();
    // Every district and member row carries its path as evidence.
    for key in ["districts", "members"] {
        for row in view.model[key].as_array().unwrap() {
            let cited = row["evidence"].as_str().unwrap_or_default();
            let named = row
                .get("file")
                .or_else(|| row.get("district"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert_eq!(cited, named, "{row}");
            assert!(!cited.is_empty(), "{row}");
        }
    }
    // The sections: the rank-then-evidence order, cut to the cap — five
    // files and four districts in the fixture, two rows each.
    assert_eq!(view.sections.len(), 2);
    for (title, rows) in &view.sections {
        assert_eq!(rows.len(), 2, "the cap cut: {title}");
        for row in rows {
            assert!(!row["evidence"].as_str().unwrap_or_default().is_empty());
        }
    }
    let hot = &view.sections[0].1;
    assert_eq!(hot[0]["evidence"], "src/hot/engine.rs");
    assert_eq!(hot[0]["churn_window"], json!(4));
    assert_eq!(hot[1]["evidence"], "src/hot/pump.rs");
    let rolled = &view.sections[1].1;
    assert_eq!(rolled[0]["evidence"], "src/hot");
    assert_eq!(rolled[0]["churn_window"], json!(7));
    // The tie at one windowed change goes to the smaller path: `.` (the
    // README's district) before `src/cool`.
    assert_eq!(rolled[1]["evidence"], ".");
    assert_eq!(rolled[1]["churn_window"], json!(1));
    // The report prints its rows through the shared line: the evidence
    // first, then the fields by name.
    let report = report(&graph, REPO, "hotspot-city").unwrap();
    assert!(
        report.contains("hotspot files by windowed churn:"),
        "{report}"
    );
    assert!(
        report.contains(
            "src/hot/engine.rs  file=src/hot/engine.rs district=src/hot \
             height=4 churn_window=4 heat=1.00"
        ),
        "{report}"
    );
    // The supernode rule: every labeled node the family matches is
    // anchored on its repository_id key, and no statement crosses
    // `(:Project)`.
    for statement in hotspot_city::STATEMENTS {
        for label in ["Commit", "Change", "File"] {
            let anchored = format!(":{label} {{repository_id: $repo}}");
            assert_eq!(
                statement.matches(&format!(":{label}")).count(),
                statement.matches(&anchored).count(),
                "an unanchored :{label} in: {statement}"
            );
        }
        assert!(!statement.contains(":Project"), "{statement}");
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

#[test]
fn an_overlay_the_family_does_not_interpret_is_answered_honestly() {
    let graph = city();
    let params = Params::parse(&json!({"overlay": "risk"})).unwrap();
    let view = render(&graph, REPO, "hotspot-city", &params, Format::Json).unwrap();
    let overlay = &view["view"]["overlay"];
    assert_eq!(overlay["asked"], "risk");
    assert_eq!(overlay["interpreted"], false);
    assert!(
        overlay["note"]
            .as_str()
            .unwrap()
            .contains("does not interpret"),
        "{overlay}"
    );
    // The knowledge overlay is answered just as honestly; no overlay asked
    // leaves nothing to answer.
    let knowledge = render(
        &graph,
        REPO,
        "hotspot-city",
        &Params::parse(&json!({"overlay": "knowledge"})).unwrap(),
        Format::Json,
    )
    .unwrap();
    assert_eq!(knowledge["view"]["overlay"]["asked"], "knowledge");
    assert_eq!(knowledge["view"]["overlay"]["interpreted"], false);
    let plain = render(
        &graph,
        REPO,
        "hotspot-city",
        &Params::default(),
        Format::Json,
    )
    .unwrap();
    assert_eq!(plain["view"]["overlay"], json!({"asked": "none"}));
}

#[test]
fn the_focus_is_required_and_the_depth_cap_is_refused_past_three() {
    let graph = chain(3);
    let error = render(&graph, REPO, IMPACT, &Params::default(), Format::Json).unwrap_err();
    assert!(error.message.contains("needs a focus"), "{}", error.message);
    // The contract's own bound: a fourth hop is never asked for.
    assert!(Params::parse(&json!({"focus": "id-m0", "depth": 4})).is_err());
}

#[test]
fn the_depth_cap_is_enforced_and_the_fourth_hop_is_absent() {
    // A six-symbol chain, longer than any depth the contract allows.
    let graph = chain(6);
    for (depth, count) in [(1, 2), (2, 3), (3, 4)] {
        let answer = render(
            &graph,
            REPO,
            IMPACT,
            &focus_params("id-m0", depth),
            Format::Json,
        )
        .unwrap();
        assert_eq!(node_ids(&answer["view"]), chain_ids(count), "depth {depth}");
        assert_eq!(answer["view"]["depth"], depth);
    }
    // The focus's fourth-hop neighbor, m4, is absent at depth 3; m3 is the
    // third hop and present.
    let answer = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("id-m0", 3),
        Format::Json,
    )
    .unwrap();
    assert!(!node_ids(&answer["view"]).contains("id-m4"));
    assert!(node_ids(&answer["view"]).contains("id-m3"));
    let depths: Vec<_> = answer["view"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["id"].clone(), node["depth"].clone()))
        .collect();
    assert!(depths.contains(&(json!("id-m3"), json!(3))), "{depths:?}");
}

#[test]
fn edges_are_labelled_by_direction_and_keep_their_orientation() {
    let graph = chain(4);
    // The focus m2 at depth 1: m1 calls m2 (reached in), m2 calls m3 (out).
    let answer = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("id-m2", 1),
        Format::Json,
    )
    .unwrap();
    let edges = answer["view"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2, "{edges:?}");
    let found = |from: &str, to: &str| {
        edges
            .iter()
            .find(|edge| edge["from"] == from && edge["to"] == to)
            .unwrap_or_else(|| panic!("no edge {from} -> {to}: {edges:?}"))
            .clone()
    };
    let caller = found("id-m1", "id-m2");
    assert_eq!(caller["direction"], "in");
    assert_eq!(caller["relation"], "CALLS");
    assert_eq!(caller["evidence"], "src/m1.rs:11");
    let callee = found("id-m2", "id-m3");
    assert_eq!(callee["direction"], "out");
    assert_eq!(callee["evidence"], "src/m2.rs:12");
    assert_eq!(answer["view"]["direction"], "both");
}

#[test]
fn imports_are_walked_beside_calls() {
    let graph = GraphStore::open_in_memory().unwrap();
    symbol(&graph, REPO, "lib", "src/lib.rs");
    symbol(&graph, REPO, "util", "src/util.rs");
    edge(&graph, REPO, "IMPORTS_FROM", "lib", "util", "src/lib.rs", 3);
    let answer = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("src/lib.rs", 1),
        Format::Json,
    )
    .unwrap();
    assert_eq!(
        node_ids(&answer["view"]),
        ["id-lib", "id-util"]
            .map(String::from)
            .into_iter()
            .collect()
    );
    assert_eq!(answer["view"]["edges"][0]["relation"], "IMPORTS_FROM");
}

#[test]
fn an_unknown_focus_answers_an_empty_neighborhood() {
    let graph = chain(3);
    let answer = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("nowhere", 2),
        Format::Json,
    )
    .unwrap();
    assert_eq!(answer["view"]["found"], false);
    assert!(node_ids(&answer["view"]).is_empty());
    assert_eq!(answer["view"]["truncated"], false);
    let mermaid = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("nowhere", 2),
        Format::Mermaid,
    )
    .unwrap();
    assert_eq!(mermaid["mermaid"], "");
}

#[test]
fn the_walk_stays_inside_its_repository() {
    let graph = chain(3);
    // The same stable id in another repository, with a call to a symbol of
    // its own: the focus's neighborhood never reaches it.
    symbol(&graph, "other", "m0", "other/m0.rs");
    symbol(&graph, "other", "x", "other/x.rs");
    edge(&graph, "other", "CALLS", "m0", "x", "other/m0.rs", 1);
    let answer = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("id-m0", 3),
        Format::Json,
    )
    .unwrap();
    assert_eq!(node_ids(&answer["view"]), chain_ids(3));
}

#[test]
fn the_cap_cuts_both_sections_and_says_so() {
    let graph = chain(6);
    let params = Params::parse(&json!({"focus": "id-m0", "depth": 3, "cap": 2})).unwrap();
    let packet = render(&graph, REPO, IMPACT, &params, Format::Json).unwrap();
    assert_eq!(packet["view"]["nodes"].as_array().unwrap().len(), 2);
    // Nearer rows first: the focus, then its first hop; the one edge
    // between them is the only one whose both ends are kept.
    assert_eq!(node_ids(&packet["view"]), chain_ids(2));
    assert_eq!(packet["view"]["edges"].as_array().unwrap().len(), 1);
    assert_eq!(packet["view"]["truncated"], true);
}

#[test]
fn the_mermaid_output_is_a_flowchart_with_quoted_labels() {
    let graph = chain(4);
    let answer = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("id-m1", 2),
        Format::Mermaid,
    )
    .unwrap();
    let mermaid = answer["mermaid"].as_str().unwrap();
    assert_flowchart(mermaid);
    // The focus is the first node; its neighbors follow by depth.
    assert!(
        mermaid.contains("  n0[\"m1<br/>src/m1.rs (depth 0)\"]"),
        "{mermaid}"
    );
    assert!(mermaid.contains("-->|\"CALLS src/m1.rs:11\"|"), "{mermaid}");
}

#[test]
fn special_characters_in_paths_are_escaped_in_mermaid() {
    let graph = GraphStore::open_in_memory().unwrap();
    let path = "src/we#ird \"<name>\".rs";
    symbol(&graph, REPO, "a", path);
    symbol(&graph, REPO, "b", "src/b.rs");
    edge(&graph, REPO, "CALLS", "a", "b", path, 7);
    let answer = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("id-a", 1),
        Format::Mermaid,
    )
    .unwrap();
    let mermaid = answer["mermaid"].as_str().unwrap();
    assert_flowchart(mermaid);
    assert!(
        mermaid.contains("-->|\"CALLS src/we#35;ird #quot;#lt;name#gt;#quot;.rs:7\"| n1"),
        "{mermaid}"
    );
}

#[test]
fn the_same_inputs_render_byte_identical_output_and_every_row_is_cited() {
    let params = focus_params("id-m2", 3);
    let graph = chain(6);
    for format in [Format::Text, Format::Json, Format::Mermaid] {
        let first =
            serde_json::to_string(&render(&graph, REPO, IMPACT, &params, format).unwrap()).unwrap();
        let again =
            serde_json::to_string(&render(&graph, REPO, IMPACT, &params, format).unwrap()).unwrap();
        // A second store seeded the same way: the same bytes.
        let other =
            serde_json::to_string(&render(&chain(6), REPO, IMPACT, &params, format).unwrap())
                .unwrap();
        assert_eq!(first, again, "{format:?}");
        assert_eq!(first, other, "{format:?}");
    }
    let answer = render(&graph, REPO, IMPACT, &params, Format::Json).unwrap();
    for row in answer["view"]["nodes"].as_array().unwrap() {
        assert!(
            row["evidence"].as_str().unwrap().starts_with("src/m"),
            "{row}"
        );
    }
    for row in answer["view"]["edges"].as_array().unwrap() {
        let evidence = row["evidence"].as_str().unwrap();
        assert!(
            evidence.starts_with("src/m") && evidence.contains(':'),
            "{row}"
        );
    }
    let text = render(&graph, REPO, IMPACT, &params, Format::Text).unwrap();
    assert!(text["text"].as_str().unwrap().contains("src/m1.rs:11"));
}

/// Seed one violation of `episode`; an empty `path` names no location.
fn seed_violation(
    graph: &GraphStore,
    repository: &str,
    episode: &str,
    rule: &str,
    path: &str,
    line: i64,
) {
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "CREATE (v:Violation {repository_id: $repo, episode: $episode, rule: $rule, \
                 severity: 'error', rule_sha: 'sha', path: $path, line: $line})",
                store_graph::params(&json!({"repo": repository, "episode": episode,
                    "rule": rule, "path": path, "line": line})),
            )?;
            Ok(())
        })
        .unwrap();
}

/// The drift timeline of `repository` at the default parameters, with the
/// bucket named.
fn timeline(graph: &GraphStore, repository: &str, bucket: &str) -> Value {
    let params = Params::parse(&json!({"bucket": bucket})).unwrap();
    let view = drift_timeline::view(graph, repository, &params).unwrap();
    let rows = view.sections[0].1.clone();
    json!({"model": view.model, "rows": rows})
}

/// The series entries of a timeline answer.
fn entries(answer: &Value) -> Vec<Value> {
    answer["model"]["series"].as_array().unwrap().clone()
}

/// A timeline's entry for `label` (`2026-02`, `2026-Q1`).
fn bucket_of<'a>(series: &'a [Value], label: &str) -> &'a Value {
    series
        .iter()
        .find(|entry| entry["bucket"] == label)
        .unwrap_or_else(|| panic!("no bucket {label} in {series:?}"))
}

/// Three months with a gap: January two commits, March one, April one.
fn seeded_months() -> GraphStore {
    let graph = GraphStore::open_in_memory().unwrap();
    seed_commit(
        &graph,
        REPO,
        "a1",
        "2026-01-05T10:00:00+00:00",
        &["a.rs", "b.rs"],
    );
    seed_commit(&graph, REPO, "a2", "2026-01-20T10:00:00+00:00", &["a.rs"]);
    seed_commit(
        &graph,
        REPO,
        "a3",
        "2026-03-10T10:00:00+00:00",
        &["c.rs", "d.rs", "e.rs"],
    );
    seed_commit(
        &graph,
        REPO,
        "a4",
        "2026-04-02T10:00:00+00:00",
        &["a.rs", "b.rs"],
    );
    graph
}

#[test]
fn drift_timeline_buckets_are_monotone_and_empty_months_are_zeros() {
    let graph = seeded_months();
    let answer = timeline(&graph, REPO, "month");
    let series = entries(&answer);
    let labels: Vec<&str> = series
        .iter()
        .map(|e| e["bucket"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["2026-01", "2026-02", "2026-03", "2026-04"]);
    // Sorted, each bucket's end is the next one's start, start < end.
    for pair in series.windows(2) {
        assert_eq!(pair[0]["end"], pair[1]["start"]);
    }
    for entry in &series {
        assert!(entry["start"].as_str().unwrap() < entry["end"].as_str().unwrap());
    }
    // February has no commit: it is in the series, as a zero with its label.
    let february = bucket_of(&series, "2026-02");
    assert_eq!(february["commits"], 0);
    assert_eq!(february["churn"], 0);
    assert_eq!(february["co_change"], 0);
    assert_eq!(february["violations"], 0);
    assert_eq!(february["evidence"], "2026-02");
    assert!(february["first_commit"].is_null());
    assert!(february["top_commits"].as_array().unwrap().is_empty());
    // The rows of the text report match the series one for one.
    assert_eq!(answer["rows"].as_array().unwrap().len(), series.len());
    // The quarter bucket: January to April is two quarters, both populated.
    let quarters = timeline(&graph, REPO, "quarter");
    let quarter_entries = entries(&quarters);
    let labels: Vec<&str> = quarter_entries
        .iter()
        .map(|e| e["bucket"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["2026-Q1", "2026-Q2"]);
    assert_eq!(bucket_of(&entries(&quarters), "2026-Q1")["commits"], 3);
    assert_eq!(bucket_of(&entries(&quarters), "2026-Q2")["commits"], 1);
}

#[test]
fn drift_timeline_counts_match_git_log_on_a_real_repository() {
    // A real fixture repository: the commits are dated, the gap month is real.
    let dir = tempfile::TempDir::new().unwrap();
    let repo = Repo::init(dir.path());
    repo.write("a.rs", "a one\n");
    repo.write("b.rs", "b one\n");
    repo.commit(ada("2026-01-05T10:00:00+00:00"), "jan one", "");
    repo.write("a.rs", "a two\n");
    repo.commit(bob("2026-01-20T10:00:00+00:00"), "jan two", "");
    for path in ["c.rs", "d.rs", "e.rs"] {
        repo.write(path, "new\n");
    }
    repo.commit(ada("2026-03-10T10:00:00+00:00"), "mar one", "");
    repo.remove("d.rs");
    repo.write("e.rs", "e two\n");
    repo.commit(bob("2026-03-28T10:00:00+00:00"), "mar two", "");
    repo.write("a.rs", "a three\n");
    repo.write("b.rs", "b two\n");
    repo.commit(bob("2026-04-02T10:00:00+00:00"), "apr one", "");

    let graph = GraphStore::open_in_memory().unwrap();
    project_node(&graph, "git");
    writer::ingest(
        &graph,
        "git",
        repo.root(),
        &crate::git_history::Window::default(),
        &|| Ok(()),
    )
    .unwrap();
    let answer = timeline(&graph, "git", "month");
    let series = entries(&answer);
    assert_eq!(series.len(), 4, "{answer:#}");

    // The ground truth: git log over each bucket's own boundaries, the end
    // inclusive by its last second.
    for entry in &series {
        let start = entry["start"].as_str().unwrap();
        let end = chrono::NaiveDate::parse_from_str(entry["end"].as_str().unwrap(), "%Y-%m-%d")
            .unwrap()
            .pred_opt()
            .unwrap();
        let since = format!("{start}T00:00:00Z");
        let until = format!("{end}T23:59:59Z");
        let commits = git_log(repo.root(), &since, &until, &["--format=%H"]);
        let names = git_log(repo.root(), &since, &until, &["--name-only", "--format="]);
        let label = entry["bucket"].as_str().unwrap();
        assert_eq!(entry["commits"], commits.len(), "{label} commits");
        assert_eq!(entry["churn"], names.len(), "{label} churn");
    }
    // The buckets, pinned: January two commits and three changes, February
    // empty, March two commits and five changes (the deletion counts), April
    // one commit and two changes.
    let churn: Vec<&Value> = series.iter().map(|e| &e["churn"]).collect();
    assert_eq!(churn, [&json!(3), &json!(0), &json!(5), &json!(2)]);
    // Co-change: the pairs of each commit's distinct files — one in January
    // (a.rs with b.rs), three in March's first commit and one in its second
    // (d.rs deleted, e.rs modified), one in April.
    let co: Vec<&Value> = series.iter().map(|e| &e["co_change"]).collect();
    assert_eq!(co, [&json!(1), &json!(0), &json!(4), &json!(1)]);
    // The whole-history co-change total is the stored `all` window's.
    coupling::derive(
        &graph,
        "git",
        chrono::Utc::now(),
        &coupling::Config::default(),
        &|| Ok(()),
    )
    .unwrap();
    let stored: i64 = graph
        .read(|db| {
            store_graph::rows(
                db,
                "MATCH (:File {repository_id: $repo})-[r:CO_CHANGED {window: 'all'}]->\
                 (:File {repository_id: $repo}) RETURN r.count AS n",
                store_graph::params(&json!({"repo": "git"})),
            )
        })
        .unwrap()
        .iter()
        .map(|row| row["n"].as_i64().unwrap())
        .sum();
    assert_eq!(stored, co.iter().map(|v| v.as_i64().unwrap()).sum::<i64>());
    // The quarter is the sum of its months.
    let quarters = timeline(&graph, "git", "quarter");
    let q1 = bucket_of(&entries(&quarters), "2026-Q1").clone();
    assert_eq!(q1["churn"], 8);
    assert_eq!(q1["commits"], 4);
    assert_eq!(q1["co_change"], 5);
    assert_eq!(q1["evidence"], series[0]["evidence"]);
}

/// The output lines of `git log` over `[since, until]`, with `args`.
fn git_log(root: &std::path::Path, since: &str, until: &str, args: &[&str]) -> Vec<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "--since", since, "--until", until])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn violations_sit_on_their_episodes_stamp_and_focus_narrows_them() {
    let graph = seeded_months();
    seed_episode(&graph, REPO, "ep-feb", 1, "2026-02-15T00:00:00Z");
    seed_episode(&graph, REPO, "ep-mar", 2, "2026-03-02T00:00:00Z");
    seed_violation(&graph, REPO, "ep-feb", "R1", "a.rs", 3);
    seed_violation(&graph, REPO, "ep-feb", "R2", "", 0);
    seed_violation(&graph, REPO, "ep-mar", "R1", "b.rs", 9);
    // An episode with no stamp: its violations are outside the series.
    seed_violation(&graph, REPO, "ep-gone", "R1", "a.rs", 1);
    let answer = timeline(&graph, REPO, "month");
    let series = entries(&answer);
    assert_eq!(bucket_of(&series, "2026-02")["violations"], 2);
    assert_eq!(bucket_of(&series, "2026-03")["violations"], 1);
    assert_eq!(answer["model"]["totals"]["violations"], 3);
    assert_eq!(answer["model"]["totals"]["unplaced_violations"], 1);
    // The cited violations: by rule, then location; a row with no path is
    // cited by its rule.
    let cited: Vec<&Value> = bucket_of(&series, "2026-02")["top_violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| &v["evidence"])
        .collect();
    assert_eq!(cited, [&json!("a.rs:3"), &json!("R2")]);
    // Focus on a.rs: the February violation on it, and the commits touching it.
    let params = Params::parse(&json!({"bucket": "month", "focus": "a.rs"})).unwrap();
    let view = drift_timeline::view(&graph, REPO, &params).unwrap();
    let focused = view.model["series"].as_array().unwrap().clone();
    assert_eq!(bucket_of(&focused, "2026-02")["violations"], 1);
    assert_eq!(bucket_of(&focused, "2026-03")["violations"], 0);
    assert_eq!(bucket_of(&focused, "2026-03")["commits"], 0);
    assert_eq!(bucket_of(&focused, "2026-01")["commits"], 2);
    // The focus keeps the full span: the gaps stay visible.
    assert_eq!(focused.len(), series.len());
}

#[test]
fn drift_timeline_is_deterministic_capped_and_cites_its_commits() {
    let graph = seeded_months();
    seed_episode(&graph, REPO, "ep-feb", 1, "2026-02-15T00:00:00Z");
    seed_violation(&graph, REPO, "ep-feb", "R1", "a.rs", 3);
    // Another repository's history and its episodes never reach this one.
    seed_commit(
        &graph,
        "other",
        "z1",
        "2026-01-07T10:00:00+00:00",
        &["x.rs", "y.rs"],
    );
    project_node(&graph, REPO);
    let before = timeline(&graph, REPO, "month");
    seed_commit(
        &graph,
        "other",
        "z2",
        "2026-02-07T10:00:00+00:00",
        &["x.rs"],
    );
    assert_eq!(
        serde_json::to_string(&before).unwrap(),
        serde_json::to_string(&timeline(&graph, REPO, "month")).unwrap()
    );
    // The same parameters render byte-identically, in each bucket.
    for bucket in ["month", "quarter"] {
        let first = serde_json::to_string(&timeline(&graph, REPO, bucket)).unwrap();
        let second = serde_json::to_string(&timeline(&graph, REPO, bucket)).unwrap();
        assert_eq!(first, second);
    }
    // The cap bounds the detail lists; the series and its totals stay whole.
    let capped =
        drift_timeline::view(&graph, REPO, &Params::parse(&json!({"cap": 1})).unwrap()).unwrap();
    let full = drift_timeline::view(&graph, REPO, &Params::default()).unwrap();
    assert_eq!(capped.model["totals"], full.model["totals"]);
    assert_eq!(capped.model["series"].as_array().unwrap().len(), 4);
    let january = bucket_of(capped.model["series"].as_array().unwrap(), "2026-01");
    assert_eq!(january["top_commits"].as_array().unwrap().len(), 1);
    assert_eq!(january["commits"], 2);
    let full_january = bucket_of(full.model["series"].as_array().unwrap(), "2026-01");
    assert_eq!(full_january["top_commits"].as_array().unwrap().len(), 2);
    // The citation rule: every row names a commit of the history, or its label
    // when the bucket has none; a boundary commit is one of its own commits.
    let oids = ["a1", "a2", "a3", "a4"];
    for row in &full.sections[0].1 {
        let evidence = row["evidence"].as_str().unwrap();
        if row["commits"] == 0 {
            assert_eq!(evidence, row["bucket"].as_str().unwrap());
        } else {
            assert!(oids.contains(&evidence), "{evidence}");
            assert_eq!(row["first_commit"], evidence);
        }
    }
    // Empty history: no buckets, zero totals, no panic.
    let empty = drift_timeline::view(
        &GraphStore::open_in_memory().unwrap(),
        REPO,
        &Params::default(),
    )
    .unwrap();
    assert!(empty.sections[0].1.is_empty());
    assert_eq!(empty.model["series"], json!([]));
    assert_eq!(empty.model["totals"]["commits"], 0);
}

// -- The conformance view (`arch_conformance.rs`) ------------------------------

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
    conformance_of(CONFORMANCE_RULES)
}

/// [`conformance_graph`] over the rule document `rules`.
fn conformance_of(rules: &str) -> (GraphStore, crate::arch_rules::EnrichReport) {
    use crate::store_graph;
    let graph = GraphStore::open_in_memory().unwrap();
    let document = crate::arch_rules::parse(rules).unwrap();
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
                   id: 'job-1', input_hash: $hash, updated_at: '2026-01-01T00:00:00Z', result: $result})",
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

/// A rule set of one constraint and no concept: the call edges from the api
/// folder into the db folder, the three the code graph holds.
const CONSTRAINT_ONLY_RULES: &str = "````constraint\nid: arch:Viz/CrossLayerCall\nseverity: BLOCKER\n```cypher\nMATCH (a:Symbol {repository_id: $repo})-[:CALLS]->(b:Symbol {repository_id: $repo}) WHERE a.relpath STARTS WITH 'src/api/' AND b.relpath STARTS WITH 'src/db/' RETURN a, b\n```\n````\n";

#[test]
fn a_constraint_only_rule_set_shows_its_recorded_results_and_violations() {
    let (graph, _) = conformance_of(CONSTRAINT_ONLY_RULES);
    let view = super::arch_conformance::view(&graph, REPO, &Params::default()).unwrap();
    assert_eq!(view.model["episode"], EPISODE);
    let row = row_of(&view.model["constraints"], "arch:Viz/CrossLayerCall");
    assert_eq!(row["status"], "violations");
    assert_eq!(row["violations"], 3);
    assert_eq!(view.model["violations"].as_array().unwrap().len(), 3);
    assert_eq!(view.model["totals"]["violations"], 3);
    assert_eq!(view.model["concepts"], json!([]));
}

// -- The viz pin (`pin.rs`) ---------------------------------------------------

/// The stamp the first pin is taken at, and a later one.
const PINNED_AT: &str = "2026-11-01T00:00:00Z";
const LATER: &str = "2026-12-01T00:00:00Z";

/// The parameters the pin tests pin: the 90-day window, a small cap.
fn pinned_params() -> Params {
    Params::parse(&json!({"window": "90d", "cap": 10})).unwrap()
}

/// The `:ViewPin` nodes of a store snapshot.
fn view_pins(state: &Value) -> Vec<Value> {
    state["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["labels"].to_string().contains(pin::LABEL))
        .cloned()
        .collect()
}

#[test]
fn a_pinned_view_replays_byte_identically_after_the_clock_advances() {
    let graph = city();
    let params = pinned_params();
    let created = pin::pin(
        &graph,
        REPO,
        "hotspot-city",
        &params,
        "ci-verdict",
        PINNED_AT,
    )
    .unwrap();
    assert_eq!(created["created"], json!(true));
    assert_eq!(created["as_of"], json!(PINNED_AT));
    assert_eq!(created["window_keys"], json!("[\"90d\"]"));
    let hash = created["input_hash"].as_str().unwrap().to_owned();
    let fresh = render(&graph, REPO, "hotspot-city", &params, Format::Json).unwrap();
    // The re-run: the pinned family at the pinned parameters. It reads no
    // clock, so a replay at a later wall time is the same replay.
    let first = pin::replay(&graph, REPO, &hash).unwrap();
    let again = pin::replay(&graph, REPO, &hash).unwrap();
    assert_eq!(first["verified"], json!(true));
    assert_eq!(first["pin"]["output_sha256"], created["output_sha256"]);
    assert_eq!(first["pin"]["as_of"], json!(PINNED_AT));
    assert_eq!(
        serde_json::to_string(&first["view"]).unwrap(),
        serde_json::to_string(&fresh["view"]).unwrap(),
        "the replayed view is the pinned render, byte for byte"
    );
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&again).unwrap()
    );
    // Advancing the clock to LATER names a new input tuple: a new pin, a new
    // node. The earlier reference still replays.
    let later = pin::pin(&graph, REPO, "hotspot-city", &params, "ci-verdict", LATER).unwrap();
    assert_eq!(later["created"], json!(true));
    assert_ne!(later["input_hash"], created["input_hash"]);
    assert_eq!(later["output_sha256"], created["output_sha256"]);
    assert_eq!(
        pin::replay(&graph, REPO, &hash).unwrap()["verified"],
        json!(true)
    );
    // An unknown pin is a refusal, and so is an unknown family.
    assert!(pin::replay(&graph, REPO, "not-a-pin").is_err());
    assert!(pin::pin(&graph, REPO, "no-such-family", &params, "x", PINNED_AT).is_err());
}

#[test]
fn a_changed_world_fails_the_verification_and_names_the_divergence() {
    let graph = city();
    let params = pinned_params();
    let created = pin::pin(
        &graph,
        REPO,
        "hotspot-city",
        &params,
        "ci-verdict",
        PINNED_AT,
    )
    .unwrap();
    let hash = created["input_hash"].as_str().unwrap().to_owned();
    let recorded = created["output_sha256"].as_str().unwrap().to_owned();
    // More history lands under the pinned view: the facts it reads changed.
    // A commit that modifies the file, as the history writer stores it.
    graph
        .transaction(|db| {
            store_graph::write(
                db,
                "MERGE (c:Commit {repository_id: $repo, oid: $oid}) \
                     SET c.commit_date = $date \
                 MERGE (ch:Change {repository_id: $repo, commit: $oid, path: $path}) \
                     SET ch.kind = 'M' \
                 MERGE (f:File {repository_id: $repo, path: $path}) \
                 MERGE (c)-[:CONTAINS_CHANGE]->(ch) MERGE (ch)-[:MODIFIES]->(f)",
                store_graph::params(&json!({"repo": REPO,
                    "oid": "0000000000000000000000000000000000000099",
                    "path": "src/hot/engine.rs", "date": "2026-10-15T00:00:00Z"})),
            )?;
            Ok(())
        })
        .unwrap();
    let error = pin::replay(&graph, REPO, &hash).unwrap_err();
    assert_eq!(error.status.as_u16(), 409);
    assert!(error.message.contains("diverged"), "{}", error.message);
    assert!(error.message.contains(&hash), "{}", error.message);
    assert!(error.message.contains(&recorded), "{}", error.message);
    // The same tuple is not silently re-pinned over the void reference.
    let repin = pin::pin(
        &graph,
        REPO,
        "hotspot-city",
        &params,
        "ci-verdict",
        PINNED_AT,
    );
    assert!(repin.unwrap_err().message.contains("diverged"));
    // The changed world is cited by pinning it anew at its own stamp.
    let anew = pin::pin(&graph, REPO, "hotspot-city", &params, "ci-verdict", LATER).unwrap();
    assert_eq!(anew["created"], json!(true));
    assert_eq!(
        pin::replay(&graph, REPO, anew["input_hash"].as_str().unwrap()).unwrap()["verified"],
        json!(true)
    );
}

#[test]
fn unpinned_renders_write_nothing_and_a_pin_writes_only_its_node() {
    let graph = city();
    let params = pinned_params();
    let before = store_graph::test_state(&graph);
    let focused =
        Params::parse(&json!({"window": "90d", "cap": 10, "focus": "src/hot/engine.rs"})).unwrap();
    for family in FAMILIES.iter() {
        for format in [Format::Text, Format::Json] {
            render(&graph, REPO, family.slug, &focused, format).unwrap();
        }
        if family.mermaid {
            render(&graph, REPO, family.slug, &focused, Format::Mermaid).unwrap();
        }
        // The default report of a focus family is its refusal, a read too.
        let _ = report(&graph, REPO, family.slug);
    }
    assert_eq!(store_graph::test_state(&graph), before, "renders are reads");
    assert!(view_pins(&before).is_empty());
    pin::pin(
        &graph,
        REPO,
        "hotspot-city",
        &params,
        "ci-verdict",
        PINNED_AT,
    )
    .unwrap();
    let after = store_graph::test_state(&graph);
    assert_eq!(view_pins(&after).len(), 1);
    let without_pins = |state: &Value| {
        let nodes: Vec<Value> = state["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| !node["labels"].to_string().contains(pin::LABEL))
            .cloned()
            .collect();
        json!({"nodes": nodes, "rels": state["rels"]})
    };
    assert_eq!(
        without_pins(&after),
        without_pins(&before),
        "a pin writes its one node and nothing else"
    );
}

/// Every mermaid-declaring family escapes a label the same way: `#` before
/// the quote and angle brackets (so the entities survive), a newline as a
/// space. Each family's own `<br/>` sits outside the escaped text.
#[test]
fn every_mermaid_family_escapes_a_label_with_hash_and_newline() {
    assert_eq!(
        super::mermaid_label("a#b\n\"<c>\""),
        "a#35;b #quot;#lt;c#gt;#quot;"
    );

    // impact-neighborhood: a symbol whose name holds `#` and a newline.
    let graph = GraphStore::open_in_memory().unwrap();
    symbol(&graph, REPO, "a#b\nc", "src/a.rs");
    symbol(&graph, REPO, "d", "src/d.rs");
    edge(&graph, REPO, "CALLS", "a#b\nc", "d", "src/a.rs", 3);
    let impact = render(
        &graph,
        REPO,
        IMPACT,
        &focus_params("id-a#b\nc", 1),
        Format::Mermaid,
    )
    .unwrap();
    let mermaid = impact["mermaid"].as_str().unwrap();
    assert_flowchart(mermaid);
    assert!(mermaid.contains("a#35;b c<br/>"), "{mermaid}");

    // coupling-matrix: a folder whose name holds `#` and a newline, drawn
    // beside the focus folder.
    let graph = coupling_graph();
    co_change(&graph, "lib#a\nb/x.rs", "src/viz/mod.rs", "all", 3);
    let coupling = render(
        &graph,
        REPO,
        COUPLING,
        &Params {
            focus: Some("src/viz/mod.rs".to_owned()),
            ..Params::default()
        },
        Format::Mermaid,
    )
    .unwrap();
    let mermaid = coupling["mermaid"].as_str().unwrap();
    assert!(mermaid.contains("lib#35;a b<br/>"), "{mermaid}");
    assert!(!mermaid.contains("lib#a"), "{mermaid}");

    // risk-overlay: a focus file whose path holds `#` and a newline.
    let fixture = two_files();
    import(
        &fixture.graph(),
        &fixture.repo,
        "src/a#b\nc.rs",
        7,
        "time::Duration",
    );
    resolve::resolve(&fixture.graph(), &fixture.repo).unwrap();
    let risk = render(
        &fixture.graph(),
        &fixture.repo,
        "risk-overlay",
        &Params {
            focus: Some("src/a#b\nc.rs".to_owned()),
            ..Params::default()
        },
        Format::Mermaid,
    )
    .unwrap();
    let mermaid = risk["mermaid"].as_str().unwrap();
    assert!(mermaid.contains("src/a#35;b c.rs"), "{mermaid}");
    assert!(!mermaid.contains("a#b"), "{mermaid}");
}
