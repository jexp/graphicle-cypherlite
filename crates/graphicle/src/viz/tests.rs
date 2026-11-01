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
fn every_family_is_listed_and_the_waiting_ones_read_as_stubs() {
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
        // The landed family answers its own view (its own tests below
        // assert it); the families still waiting for their node answer the
        // stub's note — a resource answers, never errors, exactly as the
        // history pack's families do on a repository without their facts.
        if family.slug == "hotspot-city" {
            continue;
        }
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
