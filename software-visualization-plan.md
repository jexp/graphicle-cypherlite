# Software-visualization plan — the view-model pack over code, history and supply chain

_2026-11-01. Companions: `research/software-visualization.md` (the D1–D8 survey this builds on), `git-to-graph-plan.md`, `supply-chain-plan.md`, `architecture-concepts.md`. Every "exists" claim below was verified in this checkout (feat/supply-chain tree) against the named file; the two open items are marked._

## 0. The decision this plan implements

The visualization pack is a **query-and-data layer: named, parameterized view-model families computed over the graphs we already have and served as MCP resources plus one parameterized tool** — JSON view-models (and mermaid, for graph-shaped views) with cited `path:line` evidence — **not an embedded renderer**. The research's own conclusions force this split:

- Its §1.10 closing line: "Graphicle owns the substrate; dashboards are just projection functions over it", and §4: "a dashboard = a named, versioned Cypher query (or query pack) + a rendering spec + a parameter set". The projection functions are the durable half; the rendering spec is separable (CodeCharta's `cc.json` is praised for exactly this "analysis from visualization" split). Harrer's fit verdict is the same as method: "the graph→pandas pipeline is the part to steal" — this pack is that pipeline, productized for the MCP audience.
- The research does argue for an embedded web dashboard (its §3, Phases 1–4) — that is the **dashboard project, a follow-on**, not this pack. It already has its spine: the repo serves an embedded HTTP app from axum (`api.rs` `router()`, `/app.js`, `/api/...` JSON routes), so a later `GET /api/.../viz/{family}` route is a thin wrapper around the same render functions this pack lands. Nothing here precommits or blocks it.
- The renderer the research prefers (NVL) sits behind an **unresolved license gate** (§4 below, its §1.9/§2.1); renderer-first would put the pack's centerpiece behind a legal question. JSON/mermaid is renderer-agnostic.
- Essence-series precedent: both siblings shipped query surfaces (history resources, supply-chain tools), zero frontend. A single-binary local-first Rust project has no JS build chain to host one in.

Research §6 named two "decide now" questions. (2) churn definition is **resolved by feat/git-to-graph**: commit-granular churn exists (`:Change`-`:MODIFIES`->`:File`, span-basis `:TOUCHES`->`:Symbol`), which is the *upgrade path* the research wanted over episode-granularity. (1) the NVL license stays open, and because it stays open the pack must not depend on any renderer.

## 1. What exists (verified) vs what is missing

**Exists — the survey's inputs are already in the store, and half its rankings already answered:**

- The history pack (`git_history/queries.rs`): five families behind `graphicle-graph://history/<family>` — `hotspots` (symbol degree × churn, file churn), `ownership` (sole-owner, the bus-factor read), `knowledge-loss` (departed authors × churn), `co-change` (pairs with support/confidence/lift/Jaccard), `temporal-coupling` (co-change joined to live `CALLS`/`IMPORTS`/`IMPORTS_FROM`). Every row cites `relpath:line` (first span-basis `TOUCHES` line). CodeScene's *rankings* are done — as text, top-25 (`TOP`).
- `CO_CHANGED` stored window-keyed (`coupling.rs`; default windows `all` + `90d`), recompute-not-update, changeset-capped — the coupling matrix's raw material and its trend signal (`90d` vs `all`).
- The code graph: symbols with spans, live `CALLS`/`IMPORTS` edges, `query_graph`/`get_neighbors` tools, the impact machinery whose output shapes `supply_chain/tools.rs` already reuses.
- The supply chain: `:Package`/`:Advisory`/`AFFECTED_BY`, the `(:Symbol)-[:RESOLVES_TO]->(:Package)` seam, the pin write path (`supply_chain/on_demand.rs::pin`), `deps_of`/`vuln_impact`/`license_of` tools.
- Arch-rules: `(:ConceptRule)`/`(:ConstraintRule)`/`(:RuleGroup)` stored, enrich applications with provenance and rollback (`arch_rules/executor.rs`, `provenance.rs`), constraint evaluation and report.

**Missing — the new work is form, not fact:** matrix/DSM shape (folder rollup × co-change beside static dependencies); treemap/city layout view-models; the per-file knowledge × churn join (the 2×2 quadrant — `knowledge-loss` is per-person, the map is per-file); time-bucketed drift/timeline over commit dates; conformance/exploration views over arch-rules results (Harrer's declared-vs-actual, D5-lite); the supply-chain risk overlay join; emission formats beyond text (the JSON view-model contract, mermaid).

**D1–D8 coverage, mapped to this pack's families** (`graphicle-graph://viz/<slug>`):

| research | this pack | note |
|---|---|---|
| D1 system map | `hotspot-city` + overlays (`risk-overlay`, `knowledge-map`'s columns) | the *overlay data* lands; interactive map geography deferred (below) |
| D2 hotspot city | `hotspot-city` | CodeScene semantics, CodeCity skin, as a layout model |
| D3 coupling matrix | `coupling-matrix` | co-change × static, side by side, trend-colored |
| D4 call-graph explorer | `impact-neighborhood` | NVL's job becomes JSON + mermaid; the renderer is deferred with it |
| D5 conformance DSM | `arch-conformance` | over stored rule results, not the un-landed `:Module`/`DEPENDS_ON` lift |
| D6 drift timeline | `drift-timeline` | commit dates, not episodes — the honest axis in this store |
| D7 knowledge & memory map | `knowledge-map` | author knowledge (history pack's seam); agent-memory overlays later |
| D8 cross-repo view | `risk-overlay` + `impact-neighborhood`'s external edges | our cross-repo seam is `RESOLVES_TO`, not `CROSS_CALL` |

**Verified gap, decision deferred:** the research's D1 geography assumes stored `(:Community)-[:HAS_MEMBER]` with `CONTINUES` chains — **no `:Community` node or edge is written anywhere in this store** (label search; communities are computed in-projection as `cluster::Communities` on the episode's resident context). v1 districts roll up by folder prefix (`relpath`), which *is* stored; community geography needs either per-episode materialization or the architecture-concepts layer. **[needs decision when the map becomes interactive]**

## 2. The surfaces

- **Resources** `graphicle-graph://viz/<family>` for the default-parameter reports — text/markdown, cited, `TOP`-capped — registered in `graph_tools::resources()` beside `git_history::queries::resources()`. The history precedent, unchanged.
- **One tool** `viz_view(family, format: "json"|"text"|"mermaid", params)` for the parameterized view-models (`focus`, `window`, `cap`, `overlay`, `depth`, `bucket`). A resource URI cannot carry parameters, and seven near-identical tools would duplicate what `queries.rs::render(graph, repo, slug)` already solves with one dispatch — the family discriminator keeps descriptions in one registry. (Supply-chain's per-question tools fit three distinct questions; these families share one parameter contract.)
- **Mermaid** as a format option for graph-shaped families (neighborhood, coupling-around-focus, cross-repo) — zero dependencies, renders in every markdown/MCP client, no license question; it is a serialization, not a renderer commitment. No mermaid or SVG generation exists in the crates today (verified; `pixel.rs` is screenshot→code), so this is new but trivial. **SVG/treemap pixel rendering is deferred** to the dashboard project, which needs the interactive-layout machinery anyway.
- **No new HTTP routes in this pack** — one surface (MCP), like both siblings. The dashboard follow-on adds its route as a wrapper.
- **Evidence discipline carries through**: every row names its subject by `relpath:line` or file path, exactly the history pack's rule; a view-model row without citations is a bug, and mermaid node titles carry them.

## 3. Mechanics

- Plain Cypher statements against the store — anchored on `repository_id` + label key, **against the anchor, never across** `(:Project)` (the supernode rule, git-to-graph §2) — with the arithmetic CypherLite cannot do (percentiles, lift, quadrant math) done in Rust after the query, the `queries.rs` pattern verbatim.
- **Bounded**: `TOP`-capped sections (25); window keys reused from `CO_CHANGED` (`all`/`90d` — no new windows); the changeset cap inherited from the coupling job, never re-derived; focus families take a focus symbol/file plus a depth bound (the impact machinery's bounded transitive precedent — no unbounded variable-length paths).
- **Determinism is the v1 layout pin**: view-models are pure functions of (repository, family, params, window keys, as_of) — canonical sorts (path order, `a < b` pair orientation), `BTreeMap` ordering — so the same inputs yield byte-identical JSON, and a squarified treemap over a canonical path sort needs **no stored coordinates**. The layout *algorithm* is versioned by id (`"city-v1"`); a relayout is a new id, never a mutation (research §5's rule kept, minus the coordinate store — stored pins, incremental placement and tombstones are the dashboard project's machinery).
- **Pinning a view** follows `supply_chain/on_demand.rs::pin` in shape: when a view must outlive the question (a ranking cited in a review, a CI verdict), record the analysis reference — (family, params, window keys, as_of, `input_hash`) — so a re-run reproduces byte-identical output; the derived data it read (`CO_CHANGED` windows) is already stored keyed, and nothing else accumulates. **No new job kinds**: every input exists or is derived by the existing `history`/`coupling`/`enrich`/OSV jobs.

## 4. The NVL license question — OPEN, for the user

NVL is Neo4j's TypeScript visualization library family (`@neo4j-nvl/base`, `interaction-handlers`, `react`, `layout-workers`; repo `github.com/neo4j/neo4j-nvl`; v2.0.0 Sept 2026 with WebGL2 renderer/physics and `clusterBy`; cypherlite pins 1.2.x/canvas as the battle-tested recipe per its ADR-0151). Its GitHub license reads **NOASSERTION / "SEE LICENSE IN LICENSE.txt"**, and ADR-0151 characterizes the Neo4j Visualization Library License as limiting use "to use with Neo4j products". Our use would be shipping NVL inside a Graphicle web dashboard — a product built on CypherLite, a neo4j-labs project but not itself a Neo4j product; whether embedding through CypherLite qualifies is precisely the unresolved question. **This plan is deliberately renderer-agnostic so the question gates only the deferred dashboard project (D4/D8 rendering), not anything here. [user decision + license-text verification before any NVL-derived work]**

## 5. Feed for gbuild (node sketch)

- **`viz-core`** (M): the family registry (slugs/names/descriptions), the parameter contract, render dispatch, resource listing, the `viz_view` tool, evidence reuse, determinism tests. Acceptance: every family listed and readable on a seeded fixture; unknown family → typed error (`family()` precedent); same inputs → byte-identical output.
- **`hotspot-city`** (M): the D2 view-model — squarified treemap over canonical folder rollup, height = size proxy, color = churn (window-parameterized), top-N hotspot list beside it. Acceptance: byte-identical re-render; a known hotspot of this repo is a tall hot cell; every district/member row cites path evidence.
- **`coupling-matrix`** (M): the D3 view-model — folder × folder co-change matrix (window param) beside the static `CALLS`/`IMPORTS` matrix, cell trend from `90d`-vs-`all`, focus-scope option. Acceptance: the coupling job's known coupled pair is a hot cell; cells hot in one matrix and cold in the other are flagged; canonical order, symmetric, `a < b` once.
- **`knowledge-map`** (S): per-file knowledge × churn join — the 2×2 quadrant and per-folder bus factor, reusing the ownership/knowledge statements. Acceptance: a sole-owner high-churn file lands in the loss-risk quadrant; bus-factor numbers match the `ownership` family's.
- **`impact-neighborhood`** (S): D4 as data — bounded `CALLS`/`IMPORTS`/`IMPORTS_FROM` neighborhood of a focus symbol (direction, depth ≤ 3), JSON and mermaid. Acceptance: mermaid output parses; depth cap enforced; reuses the impact machinery, does not duplicate it.
- **`arch-conformance`** (M): D5-lite over arch-rules — constraint results with rule ids and evidence, concept coverage per folder, declared-vs-actual at folder rollup. Acceptance: a seeded violation row cites rule id + `relpath:line`; coverage counts match the enrich report's.
- **`risk-overlay`** (S): the supply-chain join — files/symbols × advisory exposure via `RESOLVES_TO`→`AFFECTED_BY`, as an overlay column and a standalone list. Acceptance: parity with `vuln_impact` on a synthetic advisory; cited paths.
- **`drift-timeline`** (S): D6 as data — time-bucketed (month/quarter) churn, co-change totals and violation counts over commit `author_date`. Acceptance: buckets monotone; counts match `git log --since/--until | wc` sanity on this repo.
- **`viz-pin`** (S): the pin/analysis-reference write path and re-run (§3). Acceptance: a pinned view re-renders byte-identical offline; the pin records window keys and `as_of`; unpinned views write nothing.

**Sequencing / cut test:** `viz-core` alone in wave 1. The seven families are **mutually independent** — each needs only core plus already-stored data — so they form one parallel wave (hotspot-city, coupling-matrix, knowledge-map, impact-neighborhood, arch-conformance, risk-overlay, drift-timeline). `viz-pin` depends on core + one family to demo against (hotspot-city); it can run in the last wave beside whichever families are still open.

## 6. Out of scope (surveyed, deferred — named)

The **delivery spine**: embedded web dashboard, VS Code webview, HTTP `/api` viz routes (research §3) — the dashboard project. **Any renderer**: NVL (gated on §4) and the `d3-force` SVG fallback alike. **Real 3D/Three.js city and VR** (Getaviz) — even isometric *pixel* rendering is deferred; the view-model carries the height/color channels, drawing is the dashboard's. **Stored pinned-layout coordinates, incremental placement, tombstones** (research §5's full machinery) — v1 determinism replaces it; revisit when a map is interactive. **Community-geography districts** (stored `:Community`/`CONTINUES` — the §1 gap). **Author people-analytics beyond the existing families**: offboarding simulation, team overlays. **A Code Health clone** (per-function complexity — research §6's "what NOT to build"). **Lilienthal's MMI aggregate** (needs the architecture-concepts `:Module`/`DEPENDS_ON` lift, not landed). **LLM-generated wiki views** (DeepWiki/CodeWiki territory). **An Aider-style repo-map view** (near-existing god-nodes/surprises resources; trivial later).
