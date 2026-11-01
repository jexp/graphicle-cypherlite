//! The pack's contract tests: the registry, the parameter contract, the
//! dispatch and the `viz_view` tool surface, on stores seeded with a
//! repository and minimal facts. The family nodes' own tests extend the
//! [`graph`] fixture with the facts their views read.

use super::*;
use crate::api::{self, App};
use crate::git_history::queries;
use crate::model;
use crate::store::s;
use crate::supply_chain::resolve;
use crate::supply_chain::tools_tests::{APP, Fixture, RUSTSEC_ID, import};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::result::Result;
use tempfile::TempDir;

/// The repository the fixtures seed.
const REPO: &str = "viz";

/// The families whose nodes have landed: their views are no longer stubs.
const LANDED: [&str; 1] = ["risk-overlay"];

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
        // The knowledge map has its own view, tested below.
        if family.slug == "knowledge-map" {
            continue;
        }
        let spec = specs
            .iter()
            .find(|spec| spec["uri"] == uri.as_str())
            .unwrap_or_else(|| panic!("no spec for {uri}: {specs:?}"));
        assert_eq!(spec["name"], family.name, "{uri}");
        assert_eq!(spec["mimeType"], "text/plain", "{uri}");
        // The stub's text report: the note, the section empty — a resource
        // answers, never errors, exactly as the history pack's families do
        // on a repository without their facts.
        if LANDED.contains(&family.slug) {
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
    for family in FAMILIES
        .iter()
        .filter(|family| family.mermaid && !LANDED.contains(&family.slug))
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
