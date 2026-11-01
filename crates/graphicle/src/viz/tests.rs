//! The pack's contract tests: the registry, the parameter contract, the
//! dispatch and the `viz_view` tool surface, on stores seeded with a
//! repository and minimal facts. The family nodes' own tests extend the
//! [`graph`] fixture with the facts their views read.

use super::*;
use crate::api::{self, App};
use crate::model;
use crate::store::s;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::result::Result;
use tempfile::TempDir;

/// The repository the fixtures seed.
const REPO: &str = "viz";

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
        // A family whose node has landed reads its own view (its tests
        // below); the stub contract holds for the rest.
        if family.slug == IMPACT {
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
        .filter(|family| family.mermaid && family.slug != IMPACT)
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
