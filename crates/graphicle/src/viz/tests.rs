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
        if family.slug == "drift-timeline" {
            // Its own view, tested below: an empty history has no buckets.
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
    assert_eq!(
        text["content"][0]["text"],
        "drift by month (commit author_date, UTC):\n  none"
    );
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

// -- The drift timeline (`drift_timeline.rs`) ---------------------------------

use crate::git_history::fixture::{Repo, ada, bob};
use crate::git_history::{coupling, writer};
use crate::store_graph;

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
