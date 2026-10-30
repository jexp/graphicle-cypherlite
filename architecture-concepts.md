# Architecture concepts & constraints — a jQAssistant-inspired enrichment layer for Graphicle

_2026-10-15. Companions: `findings.md` (architecture), `cypherlite-migration.md` (storage plan). This layer is designed to land **after** the SQLite → CypherLite migration; it presumes the §2 graph model there (labels, dynamic properties, typed rels, `*_HIST` history, vector/fulltext indexes, `SEARCH`)._

## 0. Motivation

Graphicle already builds the *factual* graph — symbols, files, modules, CALLS/CONTAINS/IMPORTS/INHERITS, bi-temporally versioned — and derives communities, mental models, observations, hints, and evidence on top. What it lacks is a *declared* architecture: the difference between "here is a community the Leiden run found" and "here is the Catalog bounded context the team believes in". jQAssistant's concepts/constraints model is the proven shape for exactly this gap, and its execution substrate (Cypher over a labeled graph with dynamic properties) is precisely what CypherLite gives us post-migration. The twist that makes this more than "jQA for Rust repos": the enriched graph is served to *coding agents* over MCP, so architecture stops being a CI report and becomes queryable context at edit time.

jQA refresher (for alignment, not introduction): rules are declared in Asciidoc or XML; a **concept** is a Cypher query whose matches get labeled/connected (enrichment); a **constraint** is a query whose result rows are *violations*; **groups** compose rules with severity overrides; `requiresConcept` builds a rule dependency DAG; severities run INFO..BLOCKER; `<verify><rowCount/>` and report definitions shape output. We adopt the shape, change the substrate and the audience.

## 1. Core idea: concepts as queries that enrich the graph

A **concept = an id, a description, a Cypher query, and an optional `requires` list**. Executing a concept runs its query; matches are materialized into the graph as **labels, relationship types, and properties**. Two concept families:

- **Technical concepts**: `Layer`, `Module`, `Component`, `Service`, `Test`, `ApiEndpoint`, `Repository` (the pattern, not the repo), `Configuration`, `Generated`, `Framework:Spring`/`Framework:Axum`, …
- **Business-domain concepts**: `Catalog`, `Warehouse`, `Ordering`, `Logistics`, `Billing` — bounded contexts, aggregates, use cases. These are what let constraints and agents speak the domain language instead of package names.

Materialization rules:

- Concept application is **idempotent** (MERGE-based) and **provenance-tracked**: every label/rel/property a concept writes is derivable from a rule id, so enrichment can be rolled back per rule (`REMOVE n:Catalog` / `MATCH ()-[r:BELONGS_TO {concept:$id}]->() DELETE r`).
- Concepts compose through `requires` (jQA's `requiresConcept`): the engine topologically sorts the rule DAG before execution; a failed concept fails its dependents with `SKIPPED`, not half-applied state. Overriding/extending a shipped concept (jQA's pattern of project concepts *adding* labels on top of `java:GeneratedType`) is supported by allowing multiple rules to contribute to the same id-space label.
- Concepts are **episode-scoped like everything else**: enrichment writes participate in the bi-temporal model, so "when did Catalog start depending on Warehouse" is answerable, and an episode re-projection re-runs the rule set rather than copying stale labels.

Sketch — concept definitions (stored in the graph, mirroring how Graphicle already stores Memory/Hint nodes):

```cypher
(:ConceptRule {id:'arch:Layer/Persistence', description:'…', requires:['arch:Module/*'],
               severity:'MINOR', query:'MATCH … SET …', state:'active'}) -[:OF]->(:Project)
```

Example concept bodies:

```cypher
// arch:Component/FromModule — every module becomes a Component node
MATCH (m:Module)
MERGE (c:Component {name: m.name, repository_id: m.repository_id})
MERGE (m)-[:REALIZED_AS {concept:'arch:Component/FromModule'}]->(c);

// arch:Domain/Catalog — domain labeling by naming convention
MATCH (s:Symbol)-[:HAS_VERSION]->(v:Live)
WHERE v.qualified_name =~ '.*catalog.*' OR s.relpath STARTS WITH 'crates/catalog/'
MERGE (d:DomainConcept {name:'Catalog', repository_id: s.repository_id})
MERGE (s)-[:DOMAIN {concept:'arch:Domain/Catalog', confidence:0.8}]->(d);
```

## 1.5 Rules as documents — the authoring surface

jQA's most underrated design decision is that **rules live in project files, not in a tool database**: Asciidoc/Markdown documents in the repo, concepts and constraints as fenced blocks with metadata, applied as a post-processing step over the scanned graph. We keep that property wholesale:

```
docs/architecture/layers.md      ← the team's file, reviewed in PRs, rendered by GitHub
```concepts
id: arch:Layer/Persistence
requires: [arch:Module/*]
severity: MINOR
description: Persistence components are modules named *.persistence or *.db
```cypher
MATCH (m:Module) WHERE m.name =~ '.*\\.(persistence|db)'
MERGE (l:Layer {name:'Persistence'}) …
```constraint
id: arch:NoCycles
severity: BLOCKER
```cypher
MATCH p = (m1:Module)-[:DEPENDS_ON*2..6]->(m1) RETURN …
```
```

Consequences worth stating explicitly:

- **Maintainable like code**: rules diff, review, and blame in git; the document *is* the rule base. Rendering the doc gives the team a readable architecture description for free — the rules and the prose around them live together, which is exactly what keeps both honest.
- **Post-processing application**: rule documents are ingested as part of sync (they're just `:Source` files with a role), parsed into `(:ConceptRule)`/`(:ConstraintRule)` nodes, and executed after the projection of an episode completes — enrichment is a deterministic function of (code episode, rule set revision), which the bi-temporal store can version like everything else.
- **"Architecture skills" framing** (the AI-native pitch): a rule document is structurally an *agent skill for the graph* — frontmatter-ish metadata, prose intent, executable blocks, applied on demand. Graphicle can publish/discover/share rule packs the way agents share skills: a team's `architecture/*.md` is a skill pack that any agent (or any other repo) can load. That is a much easier story for the AI-pilled audience than "static analysis rules" — **enrichment skills that teach your agent the architecture**. Marketplace/distribution mechanics of skills (install a pack, pin a version, extend a rule id) map one-to-one onto jQA's plugin/group model.

## 2. Constraints: rules against the enriched graph

A **constraint** is a query over the *enriched* graph whose returned rows are violations. Because concepts ran first, constraints read like the architecture they check:

```cypher
// high-level, domain language: no Catalog code may depend on Warehouse internals
MATCH (a:Symbol)-[:DOMAIN]->(:DomainConcept {name:'Catalog'}),
      (a)-[:CALLS|IMPORTS|USES]->(b:Symbol)-[:DOMAIN]->(:DomainConcept {name:'Warehouse'})
WHERE NOT (b)-[:PUBLIC_API]->(:Component)
RETURN a, b;

// low-level: no cyclic dependencies between modules
MATCH p = (m1:Module)-[:DEPENDS_ON*2..6]->(m1)
RETURN m1.name AS cycle, [n IN nodes(p) | n.name] AS path;
```

- **Severity**: INFO / MINOR / MAJOR / CRITICAL / BLOCKER, with per-group overrides — jQA's group mechanism maps directly onto Graphicle's project config (a `(:RuleGroup)` node with include patterns `my-rules:*` and severity overrides).
- **Verification modes** beyond "rows = violations" (borrowed from jQA `<verify>`): `rowCount {max: N}` for ratchet rules ("no more than the current 7 cycles"), aggregation checks ("module fan-out ≤ 20"). Ratchets are the pragmatic way to adopt constraints on a codebase that already violates them.
- **Two enforcement surfaces**, deliberately different:
  - **CI**: `graphicle analyze --group arch:default` exits non-zero on violations ≥ configured severity; output is a table + optional report artifacts. Same loop as `jqassistant:analyze`.
  - **Agent time**: violations surface as *warnings in context*, not gates — see §4. A BLOCKER at CI can be a hint at edit time ("this call would introduce Catalog→Warehouse"); the agent explains, the CI decides.

## 3. Reports & visualization

jQA's `<report>` (PlantUML/GraphML per-rule output, CSV, custom report types) generalizes here because our substrate renders text well:

- **Diagrams**: rules declare a `report` block; a built-in renderer turns query results into **Mermaid** (default — agents and Markdown both render it) and Graphviz DOT. Component diagrams, layer diagrams, dependency matrices of the *declared* architecture, not the raw 50k-symbol graph.
- **Markdown reports**: per-group summary — concepts applied with match counts, constraints with violation tables linking back to `relpath:line` evidence (reusing the existing EvidenceLink/cited-excerpt machinery so every violation row is clickable and citable).
- **Software cartography** (vissoft'18, Nier et al.): the code-as-map metaphor is a natural fit for Graphicle's communities + concepts. Cities/map renderings where *elevation = churn or violation density, districts = domain concepts, borders = declared component boundaries vs. actual dependency flow*. Worth a spike as a static SVG/HTML renderer fed by the enriched graph; the vissoft paper's "consistent, stable map layout" point argues for pinning layout coordinates on `(:Component)` nodes so diffs between episodes are visible, not re-laid-out.
- Charts (coupling over time, violations per episode) can be emitted via the existing jobs/report pipeline; the bi-temporal store makes "architecture drift over time" a first-class query.
- **Cookbook recipes — GraphGists, markdown edition**: interactive, runnable guides mixing prose, a sample graph, and live `cypher` steps, rendered on the local web app (and embeddable in the VS Code webview). CypherLite's cookbook system is the pattern to copy wholesale — markdown instead of Asciidoc, `cypher cell=<id>` steps with recorded result blocks (`COOKBOOK_UPDATE=1` round-trips, so docs never show a wrong answer), collapsible `## Data` sections with source+license, Mermaid model drawings, and the WASM-parity check that runs the same cells in the browser build. Two uses here: **onboarding** (a `:play`-style walkthrough of the architecture layer and each dashboard — the Neo4j Browser guide format) and **rule-pack documentation** — every shipped rule pack ships a cookbook recipe that demonstrates it against a small fixture project, so a team learns `arch:*` rules by running them, not by reading schema. Recipes run against `:memory:` stores built from the recipe's own data, never the user's live store.

## 4. Agent access — the actual differentiator

jQA stops at the report. Graphicle's architecture layer is **served live to coding agents**, closing the loop with the machinery that already exists (mental models, observations, hints, `ask`, cited evidence):

- **New MCP tools / query templates**:
  - `arch_locate(symbol)` → labels, layer, component, domain concepts + the concepts' descriptions. Agent learns "this function is in Catalog, Persistence layer" in one call instead of re-deriving it from paths.
  - `arch_rules(scope)` → the constraints relevant to a file/component, with descriptions — the agent reads the rules *before* editing.
  - `arch_preflight(files or symbols, planned change kind)` → the constraints whose patterns match the affected neighborhood, evaluated as a **trial**: which currently-passing constraints would this edit plausibly violate? ("moving X adds a Catalog→Warehouse edge; constraint `arch:DomainIsolation` is BLOCKER in CI").
  - `arch_impact(symbol)` → blast radius expressed in *architecture* terms: which components/layers/domain concepts become unreachable or newly coupled — not just the raw transitive-closure node list.
  - `arch_explain(constraint)` → description, severity, current violation count, example violations with cited evidence.
- **Integration with existing machinery**:
  - Mental models get anchored to concept nodes: an observation about "Catalog's invariants" links `-[:GROUNDED_IN]->` the `:DomainConcept`, not just a path scope — so it survives refactors that move files.
  - Retrieval hints can be *concept-scoped* (`(:Hint {scope:'domain:Catalog'})`) and fulltext/vector `SEARCH` over concept descriptions gives agents fuzzy entry points ("which concept covers shipping?").
  - `ask` answers cite the architecture layer: "this belongs to Ordering (rule `arch:Domain/Ordering`, confidence 0.8)" — grounded, not vibes.
- The payoff: agents stop reconstructing the architecture from scratch every session. The enrichment is computed once per episode, versioned, and served.

## 5. Why the CypherLite backend makes this natural

- **Labels + dynamic properties + typed rels** are exactly jQA's enrichment primitives — jQA ran on Neo4j for a reason. Concepts doing `SET n:Catalog`, `MERGE (s)-[:BELONGS_TO]->(c)`, `SET r.confidence = 0.8` are one statement each; in the SQLite world this was unthinkable (new join tables per concept).
- **`:Live` label partitions and `*_HIST` rel types** (migration §2.4) mean concept queries automatically run against the current code state, while concept *output* can be versioned the same way (concept rels close as `BELONGS_TO_HIST`).
- **Vector + fulltext `SEARCH`** powers concept *suggestion* (§7): embedding symbol names/docs and searching near a domain vocabulary is a native query, not an ETL job.
- **`:memory:` instances** give trial enrichment runs: apply a candidate rule set to a scratch copy, inspect match counts and sample violations, discard. This is the dry-run mode jQA never had cheaply (it needs a store either way; ours is free).
- **Constraints/indexes**: uniqueness on rule ids, `SEARCH` indexes on rule descriptions. And CypherLite *schema* constraints (NODE KEY etc.) should not be confused with *architecture* constraints — the doc uses "constraint" in the jQA sense throughout.
- Single-writer job queue (findings §"coupling smell" resolved) means rule execution is just another `(:Job {kind:'enrich'})`, resumable and cacheable via `input_hash` of (rule set hash, episode).

## 6. Concrete graph-model sketch

```cypher
(:Symbol)-[:BELONGS_TO {concept, confidence}]->(:Component)-[:PART_OF_LAYER]->(:Layer)
(:Component)-[:DOMAIN]->(:DomainConcept {name:'Catalog'})
(:Symbol)-[:DOMAIN {concept:'arch:Domain/Catalog', confidence:0.8}]->(:DomainConcept)
(:Module)-[:REALIZED_AS]->(:Component)
(:Test)-[:COVERS]->(:File)                      -- already in migration §2.4
(:Layer)-[:ALLOWS_DEPENDENCY]->(:Layer)         -- declared architecture
(:Layer)-[:FORBIDS_DEPENDENCY]->(:Layer)
(:ConceptRule), (:ConstraintRule), (:RuleGroup) -- the rule base, in-graph, episode/versioned
(:Violation {rule, severity, episode, evidence…})-[:OF]->(:Project)
(:Violation)-[:CITES]->(:Symbol)                -- reuse cited-evidence machinery
```

Dependency queries become architecture queries by lifting raw edges:

```cypher
// module-level dependency, derived per episode
MATCH (a:Module)-[:CONTAINS*]->(s1:Symbol)-[:CALLS|IMPORTS]->(s2:Symbol)<-[:CONTAINS*]-(b:Module)
WHERE a <> b
MERGE (a)-[d:DEPENDS_ON {episode:$ep}]->(b) SET d.weight = coalesce(d.weight,0)+1;
```

## 7. Concept sources

In rough order of trust:

1. **Explicit declarations** in rule documents (§1.5 — Markdown/Asciidoc with fenced rule blocks under `docs/architecture/` or `.graphicle/`; docs are the rules).
2. **Build files / module structure**: Cargo workspace members, package.json workspaces, Gradle modules → `:Module`/`:Component`.
3. **Naming & directory conventions**: `*/test/*`, `*.controller.*`, `*Repository` → `:Test`, `:ApiEndpoint`, etc. Low confidence, high coverage.
4. **Annotations/attributes**: `#[tokio::main]`, `#[test]`, derive macros, framework route decorators → extractor-emitted facts the concepts consume.
5. **LLM-proposed concepts, validated by queries** — the bridge to existing machinery: the alias-proposal flow (`(:Entity)-[:ALIAS_OF {proposal_id, score, state}]`) and observations already have a propose→validate→accept lifecycle. Extend it: the LLM reads community reports + names + doc links, proposes `(:DomainConcept {name:'Warehouse'})` *with a candidate membership query*; the query runs against a `:memory:` trial enrichment; match counts/coverage decide auto-apply vs. human approval. Vector SEARCH over symbol/chunk embeddings against a domain glossary seeds the proposals.
6. **Imported jQA rule sets** where licenses allow — jQA core rules are GPL-ish in places (check per-plugin; the Maven/plugin rules vary). Many of ours will be language-agnostic re-implementations anyway; the naming-convention and layering ideas transfer, the Java-specific queries don't.
7. **The author's own jQA quick guide** (https://guides.neo4j.com/jqa, a Neo4j Browser `:play` guide) — general concept/constraint/query material. Beyond its Java-specific examples, three of its patterns map directly onto this design:
   - **Label-then-query** (`SET m:Public`, `:Investigate`, `:Immutable`) — the §1 concept idiom, with labels doubling as *workflow marks* for human follow-up, which bridges into the hint/observation machinery.
   - **Thresholds, ranges and exceptions as nodes, not config** (`CREATE (medium:TestCoverageRange {…})`, violation queries joining against them, exception lists with stated reasons and `SKIP n` tolerated debt) — in our store these become `(:RuleRange)`/`(:Exception {reason})` nodes, versioned and episode-scoped for free.
   - **Derived-metric composition** (fanIn/fanOut → defaults → `typeCoupling` → in-package variants, each step materialized before the next reads it) — the `requires` DAG demonstrated end-to-end, and the seed of the coupling metrics the git-history layer queries; its churn × complexity → coverage-requirement example (git plugin + JaCoCo in one query) is this layer's pitch in miniature.
   Its examples (visibility, immutability, encapsulation/`:Internal`, coverage ranges) are natural first dogfooding rule-pack content for Phase 1.

## 8. Phased roadmap (after the migration lands)

- **Phase 1 — rule engine core**: rule storage nodes, DAG resolution (`requires`, group includes, wildcard patterns), topological executor as a job kind, idempotent MERGE application with `concept:` provenance, `graphicle analyze` CLI with severity thresholds. Dogfood on Graphicle itself (modules/layers of the three crates).
- **Phase 2 — constraints + CI**: violation model, ratchet (`rowCount max`) mode, markdown/table output, exit codes; GitHub Action recipe.
- **Phase 3 — agent surface**: MCP tools from §4, concept-scoped hints, mental-model anchoring, `arch_preflight` trial runs on `:memory:`.
- **Phase 4 — suggestion loop**: LLM concept proposals via embeddings + observation flow, human/agent approval path, drift reporting over episodes.
- **Phase 5 — cartography & reports**: Mermaid/Graphviz renderers, stable-layout SVG map, drift-over-time charts.

## 9. Open questions

- **Re-execution semantics**: full wipe-and-reapply of concept labels per episode, or incremental (only re-run rules whose `requires` inputs changed — needs an `(:Artifact)-[:DEPENDS_ON]->(:Source)` integration with the invalidation machinery)? Full re-apply is simpler and probably fast enough; measure.
- **Rule language**: raw Cypher only, or a thin DSL for the common "label by predicate" shape? jQA shows raw Cypher is fine for power users but steep for casual rule authors; LLM-authored rules may make this moot.
- **Rule document format**: Markdown fenced blocks (renders everywhere, agents read/write it natively) vs. jQA's Asciidoc (richer metadata conventions); and the block-metadata shape — YAML-ish headers in the fence info string vs. a preceding heading convention. Leaning Markdown + structured fence headers, matching how agent skills are authored today.
- **Conflict handling** when two concepts disagree (naming says Catalog, LLM says Ordering): multi-domain membership allowed (a symbol can be in two concepts) vs. single-assignment with precedence? Lean multi-membership + confidence, but constraints must tolerate it.
- **Rule versioning**: rules in the repo config change over time; violations are episode-versioned — do we pin `rule_sha` onto each `:Violation` (yes, probably) and diff rule sets across episodes?
- **Performance ceiling**: whole-graph concept queries on 100k-symbol repos inside the single-writer engine — what's the budget before enrichment needs to run in a `:memory:` side-instance and merge back? (Migration §6 item 12's read-only-open coexistence may matter here.)
- **License audit** for importing any actual jQA rule text (§7.6).
- **CypherLite gaps**: verify dynamic label `SET`/`REMOVE` volume cost and MERGE-heavy enrichment throughput during the migration spike (item 7 there) — enrichment is the workload most sensitive to both.
