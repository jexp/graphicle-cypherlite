//! The software-visualization pack (software-visualization-plan.md): named
//! view-model families computed over the graphs the store already holds —
//! the git history, the code graph and the supply chain — and served as
//! `graphicle-graph://viz/<slug>` resources plus the one parameterized
//! [`viz_view`](OPERATION) tool. Not an embedded renderer: a family is a
//! projection function over the substrate (the plan's §0 decision), and the
//! JSON view-model — or mermaid, for the graph-shaped families — is the
//! deliverable every client renders.
//!
//! | slug                  | view (the plan's §5 families)                                |
//! |-----------------------|--------------------------------------------------------------|
//! | `hotspot-city`        | a city-shaped churn map over folder rollup, hot list beside  |
//! | `coupling-matrix`     | folder × folder co-change beside static calls and imports    |
//! | `knowledge-map`       | per-file knowledge × churn: the 2×2 quadrant, bus factor     |
//! | `impact-neighborhood` | a focus symbol's bounded calls and imports neighborhood      |
//! | `arch-conformance`    | stored rule results, coverage, declared against actual       |
//! | `risk-overlay`        | advisory exposure of files and symbols through their imports |
//! | `drift-timeline`      | time-bucketed churn, co-change totals, violation counts      |
//!
//! # The contract a family implements
//!
//! [`Family::render`] reads `(graph, repository_id, params)` and answers a
//! [`View`]: the `sections` its text report prints (a title and its cited
//! rows), the `model` its json form serves, and — for the families that
//! declare the format — its `mermaid` diagram. Text and json are served for
//! every family, mermaid only by declaration ([`Family::mermaid`]); the
//! dispatcher refuses mermaid for the others before the family runs. A
//! family's queries anchor on `repository_id` and never pass through
//! `(:Project)` (the supernode rule), and every bound comes from
//! [`Params`]: no unbounded walks, no windows but the stored
//! [`CO_CHANGED`](crate::git_history::coupling) keys.
//!
//! # Evidence
//!
//! Every row names what it is about in `evidence` — a symbol's
//! `relpath:line` or a file's path — the history pack's rule
//! ([`crate::git_history::queries`]); a view-model row without a citation is
//! a bug, and mermaid node titles carry their citations.
//!
//! # Determinism (the plan's §3 layout pin)
//!
//! A view is a pure function of (repository, family, parameters, window
//! keys, as_of): the same inputs render byte-identical output. This is law
//! for every family:
//! - maps and sets are `BTreeMap`/`BTreeSet`, never hash-ordered;
//! - a list is sorted canonically before it is cut to `cap`: by its rank,
//!   ties by the smaller `evidence` (the history pack's tie rule);
//! - a pair is kept in one orientation, `a < b`;
//! - no query's row order is relied on: the Rust side sorts;
//! - the parameters in force are echoed in every answer ([`Params::json`]),
//!   whatever order the call named them in, so a report names the inputs
//!   that shaped it.
//!
//! [`Section`] and [`line`] are the history pack's, shared so both packs
//! print a row identically: the evidence first, then its fields by name.

use crate::git_history::queries::{Section, line};
use crate::store::{Store, repo, s};
use crate::store_graph::{self, GraphStore};
use crate::{Error, Result};
use serde_json::{Value, json};

mod arch_conformance;
mod coupling_matrix;
mod drift_timeline;
mod hotspot_city;
mod impact_neighborhood;
mod knowledge_map;
pub mod pin;
mod risk_overlay;
#[cfg(test)]
mod tests;

/// The resource URI prefix of the pack: `graphicle-graph://viz/<family>`.
pub const PREFIX: &str = "graphicle-graph://viz/";
/// The operation of `viz_view`, the code-graph half of the server.
pub const OPERATION: &str = "graph.viz_view";
/// Rows one section reports by default (the history pack's `TOP`).
pub const TOP: usize = 25;
/// The most rows a call may ask for.
pub const MAX_CAP: usize = 100;
/// The default neighborhood bound of the focus families.
pub const DEFAULT_DEPTH: usize = 2;
/// The most hops a focus family walks (the impact machinery's bound).
pub const MAX_DEPTH: usize = 3;
/// The focus argument's length bound: one path or symbol name.
pub const MAX_FOCUS: usize = 512;
/// The stored co-change windows a `window` may name
/// (`git_history::coupling`'s keys, no new ones).
pub const WINDOWS: [&str; 2] = ["all", "90d"];
/// The overlay columns an `overlay` may add to a map family.
pub const OVERLAYS: [&str; 3] = ["none", "risk", "knowledge"];
/// The time buckets a timeline family may be asked for.
pub const BUCKETS: [&str; 2] = ["month", "quarter"];
/// The fields of the parameter contract, in the order the contract states
/// them (a refusal names them all).
const PARAMS: [&str; 6] = ["focus", "window", "cap", "overlay", "depth", "bucket"];
/// The refusal a focus family gives a call without a focus (the tool path),
/// and the note its resource answers with (the report path).
const NEEDS_FOCUS: &str = "The impact-neighborhood family needs a focus: a symbol's stable_id or qualified name, or a file path";

/// One family of the pack: the slug its resource URI and the tool's `family`
/// argument name it, its name and description as `resources/list` publishes
/// them, whether it serves the mermaid format, and its renderer — the
/// function the family's own node lands.
pub struct Family {
    pub slug: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Whether the family serves the mermaid format (the graph-shaped
    /// families: a neighborhood, a matrix around a focus, the cross-repo
    /// edges). Text and json every family serves.
    pub mermaid: bool,
    /// Whether the family's view is centered on a focus it cannot answer
    /// without one. Its resource answers the note, its tool call refuses.
    pub needs_focus: bool,
    /// The family's view of `(repository, params)`.
    pub render: Render,
}

/// A family's renderer, the contract its node implements.
pub type Render = fn(&GraphStore, &str, &Params) -> Result<View>;

/// The families, in the order `resources/list` publishes them.
pub const FAMILIES: [Family; 7] = [
    Family {
        slug: "hotspot-city",
        name: "Hotspot City",
        description: "A city-shaped view of churn: files as buildings over folder rollup, colored by windowed churn, the hot list beside it",
        mermaid: false,
        needs_focus: false,
        render: hotspot_city::view,
    },
    Family {
        slug: "coupling-matrix",
        name: "Coupling Matrix",
        description: "Folder-by-folder co-change beside static calls and imports, cells trend-colored by window",
        mermaid: true,
        needs_focus: false,
        render: coupling_matrix::view,
    },
    Family {
        slug: "knowledge-map",
        name: "Knowledge Map",
        description: "Per-file author knowledge against churn: the 2x2 quadrant and per-folder bus factor",
        mermaid: false,
        needs_focus: false,
        render: knowledge_map::view,
    },
    Family {
        slug: "impact-neighborhood",
        name: "Impact Neighborhood",
        description: "A focus symbol's bounded calls and imports neighborhood, with direction and depth",
        mermaid: true,
        needs_focus: true,
        render: impact_neighborhood::view,
    },
    Family {
        slug: "arch-conformance",
        name: "Architecture Conformance",
        description: "Stored rule results with evidence: violations, concept coverage, declared against actual at folder rollup",
        mermaid: false,
        needs_focus: false,
        render: arch_conformance::view,
    },
    Family {
        slug: "risk-overlay",
        name: "Supply-chain Risk",
        description: "Advisory exposure of files and symbols through the packages their imports resolve to",
        mermaid: true,
        needs_focus: false,
        render: risk_overlay::view,
    },
    Family {
        slug: "drift-timeline",
        name: "Drift Timeline",
        description: "Time-bucketed churn, co-change totals and violation counts over commit dates",
        mermaid: false,
        needs_focus: false,
        render: drift_timeline::view,
    },
];

/// One family's answer: the three forms its formats serve.
pub struct View {
    /// The sections of its text report: a title and its cited rows.
    pub sections: Vec<Section>,
    /// The view-model its json form serves: an object whose shape is the
    /// family's own.
    pub model: Value,
    /// Its mermaid diagram; `None` for a family that declares the format
    /// but has no diagram for the parameters (an empty focus).
    pub mermaid: Option<String>,
}

/// The answer format a call asks for: text and json for every family,
/// mermaid for the families that declare it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    Mermaid,
}

impl Format {
    /// The format a `format` argument names.
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "text" => Self::Text,
            "json" => Self::Json,
            "mermaid" => Self::Mermaid,
            _ => return Err(Error::bad("format must be text, json or mermaid")),
        })
    }

    /// The name the argument and the answer's `format` field carry.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Mermaid => "mermaid",
        }
    }
}

/// The stored co-change window a churn-colored view reads (the coupling
/// job's keys, no new ones).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Window {
    /// The whole-history derivation.
    #[default]
    All,
    /// The ninety-day window.
    NinetyDays,
}

impl Window {
    /// The window a `window` argument names.
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "all" => Self::All,
            "90d" => Self::NinetyDays,
            _ => {
                return Err(Error::bad(format!(
                    "window must be {}",
                    WINDOWS.join(" or ")
                )));
            }
        })
    }

    /// The key the store holds it as.
    pub fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::NinetyDays => "90d",
        }
    }
}

/// The overlay column a map family adds beside its own channels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overlay {
    #[default]
    None,
    /// Advisory exposure, the `risk-overlay` family's column.
    Risk,
    /// Author knowledge, the `knowledge-map` family's columns.
    Knowledge,
}

impl Overlay {
    /// The overlay an `overlay` argument names.
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "none" => Self::None,
            "risk" => Self::Risk,
            "knowledge" => Self::Knowledge,
            _ => {
                return Err(Error::bad(format!(
                    "overlay must be {}",
                    OVERLAYS.join(", ")
                )));
            }
        })
    }

    /// The name the argument and the echoed parameters carry.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Risk => "risk",
            Self::Knowledge => "knowledge",
        }
    }
}

/// The time bucket a timeline family groups its counts by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bucket {
    #[default]
    Month,
    Quarter,
}

impl Bucket {
    /// The bucket a `bucket` argument names.
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "month" => Self::Month,
            "quarter" => Self::Quarter,
            _ => {
                return Err(Error::bad(format!(
                    "bucket must be {}",
                    BUCKETS.join(" or ")
                )));
            }
        })
    }

    /// The name the argument and the echoed parameters carry.
    pub fn name(self) -> &'static str {
        match self {
            Self::Month => "month",
            Self::Quarter => "quarter",
        }
    }
}

/// The shared parameter contract every family reads (the plan's §2): each
/// field optional, the defaults the plan's §3 bounds set.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    /// What the focus families center on: a symbol's `stable_id` or
    /// qualified name, or a file path. The focus families refuse a call
    /// without one; the contract only carries it.
    pub focus: Option<String>,
    /// The stored co-change window the churn-colored views read.
    pub window: Window,
    /// Rows one section reports.
    pub cap: usize,
    /// The overlay column a map family adds.
    pub overlay: Overlay,
    /// The focus families' neighborhood bound.
    pub depth: usize,
    /// The timeline families' time bucket.
    pub bucket: Bucket,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            focus: None,
            window: Window::default(),
            cap: TOP,
            overlay: Overlay::default(),
            depth: DEFAULT_DEPTH,
            bucket: Bucket::default(),
        }
    }
}

impl Params {
    /// The parameters of a call's `params` object: every field optional,
    /// absent and `null` alike, a wrong value refused by name (the command
    /// line's value-parser discipline), an unknown field refused rather
    /// than ignored.
    pub fn parse(args: &Value) -> Result<Self> {
        let mut params = Self::default();
        if args.is_null() {
            return Ok(params);
        }
        let Some(fields) = args.as_object() else {
            return Err(Error::bad("params must be an object"));
        };
        for (key, value) in fields {
            if value.is_null() {
                continue;
            }
            match key.as_str() {
                "focus" => params.focus = Some(crate::string(args, "focus", MAX_FOCUS)?.to_owned()),
                "window" => params.window = Window::parse(crate::string(args, "window", 8)?)?,
                "cap" => params.cap = number(args, "cap", 1, MAX_CAP)?,
                "overlay" => params.overlay = Overlay::parse(crate::string(args, "overlay", 16)?)?,
                "depth" => params.depth = number(args, "depth", 1, MAX_DEPTH)?,
                "bucket" => params.bucket = Bucket::parse(crate::string(args, "bucket", 16)?)?,
                _ => {
                    return Err(Error::bad(format!(
                        "Unknown viz parameter: {key}; use one of {}",
                        PARAMS.join(", ")
                    )));
                }
            }
        }
        Ok(params)
    }

    /// The parameters in force, as every answer echoes them: one value per
    /// field, in the contract's order — the input half of the determinism
    /// contract, so the same parameters echo identically whatever order the
    /// call named them in.
    pub fn json(&self) -> Value {
        json!({"focus": self.focus, "window": self.window.key(), "cap": self.cap,
            "overlay": self.overlay.name(), "depth": self.depth, "bucket": self.bucket.name()})
    }
}

/// An integer parameter of the contract, named in its refusal.
fn number(args: &Value, key: &str, min: usize, max: usize) -> Result<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| (min..=max).contains(n))
        .ok_or_else(|| Error::bad(format!("{key} must be a number from {min} to {max}")))
}

/// The pack's family for a resource URI, its slug: `None` for any other
/// URI, or a slug the registry does not hold (the history pack's
/// `family()`).
pub fn family(uri: &str) -> Option<&'static str> {
    FAMILIES
        .iter()
        .map(|family| family.slug)
        .find(|slug| uri.strip_prefix(PREFIX) == Some(slug))
}

/// The family of `slug`, or the typed refusal that names every known one.
fn family_of(slug: &str) -> Result<&'static Family> {
    FAMILIES
        .iter()
        .find(|family| family.slug == slug)
        .ok_or_else(|| {
            Error::bad(format!(
                "Unknown viz family: {slug}; use one of {}",
                FAMILIES
                    .iter()
                    .map(|family| family.slug)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// The pack's resource specs, in the shape `resources/list` publishes: each
/// family's default-parameter text report under its URI.
pub fn resources() -> Vec<Value> {
    FAMILIES
        .iter()
        .map(|family| {
            json!({"uri": format!("{PREFIX}{}", family.slug), "name": family.name,
                "description": family.description, "mimeType": "text/plain"})
        })
        .collect()
}

/// The family's default-parameter text report, the resource its
/// `graphicle-graph://viz/<slug>` URI serves. A focus family has no focus
/// here, so its resource answers the needs-a-focus note rather than the
/// tool's refusal: a resource answers, it never errors on an empty ask.
pub fn report(graph: &GraphStore, repository_id: &str, slug: &str) -> Result<String> {
    let family = family_of(slug)?;
    let view = if family.needs_focus {
        View {
            sections: vec![("needs a focus", vec![json!({"note": NEEDS_FOCUS})])],
            model: json!({"found": false}),
            mermaid: None,
        }
    } else {
        (family.render)(graph, repository_id, &Params::default())?
    };
    Ok(render_text(&view))
}

/// The family's answer in `format`, as the `viz_view` tool serves it: the
/// family and format named, the parameters in force echoed, and the view —
/// `text` (the cited report), `view` (the json view-model) or `mermaid`.
/// Mermaid for a family that does not serve it is refused before the family
/// runs; an unknown family is the typed refusal.
pub fn render(
    graph: &GraphStore,
    repository_id: &str,
    slug: &str,
    params: &Params,
    format: Format,
) -> Result<Value> {
    let family = family_of(slug)?;
    if format == Format::Mermaid && !family.mermaid {
        return Err(Error::bad(format!(
            "The {} family serves text and json, not mermaid",
            family.slug
        )));
    }
    let view = (family.render)(graph, repository_id, params)?;
    let mut packet =
        json!({"family": family.slug, "format": format.name(), "params": params.json()});
    match format {
        Format::Text => packet["text"] = json!(render_text(&view)),
        Format::Json => packet["view"] = view.model,
        Format::Mermaid => packet["mermaid"] = json!(view.mermaid.unwrap_or_default()),
    }
    Ok(packet)
}

/// The `viz_view` answer ([`OPERATION`]): one family's view in the format
/// asked, on the project the call reads.
pub fn viz_view(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    let slug = crate::string(args, "family", 64)?;
    let format = match args.get("format") {
        None | Some(Value::Null) => Format::Json,
        Some(_) => Format::parse(crate::string(args, "format", 16)?)?,
    };
    let params = Params::parse(args.get("params").unwrap_or(&Value::Null))?;
    let named = crate::string(args, "repository_id", 100)?;
    store.authorized_read(workspace, token, "read", |db, p| {
        let repository = repo(db, p, named)?;
        render(&store.graph(), s(&repository, "id"), slug, &params, format)
    })
}

/// The tool's MCP reply: the report, the view-model's JSON or the diagram
/// as the text the model reads, the whole answer as structured content.
pub fn mcp_reply(packet: Value) -> Value {
    let text = match packet["format"].as_str() {
        Some("text") => packet["text"].as_str().unwrap_or_default().to_owned(),
        Some("mermaid") => packet["mermaid"].as_str().unwrap_or_default().to_owned(),
        _ => packet["view"].to_string(),
    };
    json!({"content":[{"type":"text","text":text}],"structuredContent":packet,"isError":false})
}

/// The text form of a view: one block per section, one line per row, the
/// row's `evidence` first (the history pack's renderer, through its
/// [`line`]).
fn render_text(view: &View) -> String {
    let mut out = Vec::new();
    for (title, rows) in &view.sections {
        out.push(format!("{title}:"));
        if rows.is_empty() {
            out.push("  none".to_string());
        }
        out.extend(rows.iter().map(|row| format!("  {}", line(row))));
    }
    out.join("\n")
}

/// The rows of a view's row accessor: a row's text field, `""` when absent.
fn text(row: &Value, key: &str) -> String {
    store_graph::s(row, key).to_string()
}

/// A row's integer field, `0` when absent (the history pack's `int`).
fn int(row: &Value, key: &str) -> i64 {
    row[key].as_i64().unwrap_or(0)
}

/// A row's evidence: the citation every row names what it is about by, the
/// tie key of the canonical sort.
fn evidence(row: &Value) -> &str {
    store_graph::s(row, "evidence")
}

/// The folder of a file: its directory, `.` for a file at the root.
fn folder_of(path: &str) -> String {
    path.rsplit_once('/')
        .map_or_else(|| ".".to_owned(), |(dir, _)| dir.to_owned())
}

/// The read a family's statement runs on the code graph, its rows back.
fn run(graph: &GraphStore, cypher: &str, args: &Value) -> Result<Vec<Value>> {
    graph.read(|db| store_graph::rows(db, cypher, store_graph::params(args)))
}

/// A mermaid label's text, escaped for a double-quoted label by every
/// mermaid-declaring family: `#` first so its own entity survives, then the
/// quote and the angle brackets, and a newline as a space. A family wraps the
/// escaped text in its quotes and inserts its own `<br/>` outside it.
fn mermaid_label(text: &str) -> String {
    text.replace('#', "#35;")
        .replace('"', "#quot;")
        .replace('<', "#lt;")
        .replace('>', "#gt;")
        .replace(['\n', '\r'], " ")
}
