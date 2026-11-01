# GUAC → Graphicle integration assessment

_2026-10-29. Researched against GUAC v1.1.0 (guacsec/guac, March 2026) and Graphicle's `feat/cypherlite-backend` branch (the SQLite → CypherLite migration as implemented, HEAD `99c3f5aa`). Citations are repo-relative `file:line` on that branch unless noted. GUAC sources: [docs.guac.sh](https://docs.guac.sh/), the [guacsec/guac](https://github.com/guacsec/guac) repo, the [v1.0 announcement](https://safeguard.sh/resources/blog/guac-v1-0-release-graph-supply-chain-2025)._

## 1. GUAC primer

**What it is.** GUAC ("Graph for Understanding Artifact Composition") is an OpenSSF incubating project (originating 2022 at Kusari, Google, Purdue, Citi), written in Go, **Apache-2.0**. v1.0 (June 2025) pledged 1.x stability for the GraphQL schema, certifier/ingestor plugin contracts, and parsers; v1.1.0 is the current release. It does *not* produce attestations (Sigstore's job) or score repos (Scorecard's job) — it **aggregates heterogeneous supply-chain documents, normalizes software identity, and serves the resulting knowledge graph via GraphQL**.

**Ontology** (conceptual: [guac-ontology](https://docs.guac.sh/guac/guac-ontology/); authoritative: the GraphQL schema, one file per type in `pkg/assembler/graphql/schema/`). Three trees:

- **Software tree** (facts): **Package** nodes following [purl](https://github.com/package-url/purl-spec) semantics — `type` (npm/pypi/cargo/maven…), `namespace`, `name`, `version`, `qualifiers`, `subpath`; **Source** nodes (VCS type, host namespace, repo name, tag/commit); **Artifact** nodes (algorithm + digest); **Builder** nodes (build-system URI).
- **Evidence tree** (attested claims attached at any level of the software tree). The edge types, from the schema directory: `isOccurrence` (artifact ↔ package/source), `isDependency`, `hasSourceAt`, `hasSBOM`, `hasSLSA`, `certifyVuln`, `certifyLegal`, `certifyScorecard`, `certifyVEXStatement`, `certifyGood`/`certifyBad` (policy verdicts), `hashEqual`, `pkgEqual`, `vulnEqual`, `vulnMetadata`, `hasMetadata`, plus `contact` and `license` node types. Two shapes worth remembering:
  - `isDependency`: `package` → `dependencyPackage` (must be PackageVersion level), `dependencyType` ∈ `DIRECT | INDIRECT | UNKNOWN`, `justification`, `origin` (document it came from), `collector` (GUAC component), `documentRef` (blob-store pointer).
  - `certifyVuln`: `package` → `vulnerability` (or an explicit **NoVuln** node — "scanned, nothing found" is recorded, not absent), with `ScanMetadata` (`timeScanned`, `dbUri`, `dbVersion`, `scannerUri`, `scannerVersion`, `origin`, `collector`, `documentRef`).
- **Actor tree** (identity/trust topologies — still deferred beyond beta).

Deliberate design decision: package→hash mappings are *opinions* kept in the evidence tree, so disagreeing documents can coexist as counterfactuals. GUAC's demo story is **"known knowns / known unknowns / unknown unknowns"**: a query over a purl shows which evidence exists and, just as importantly, which is missing (no SBOM, no SLSA, no scorecard) — [known-and-unknown](https://docs.guac.sh/guac/known-and-unknown/).

**Pipeline.** Components chained over NATS blob/queue streams, each also runnable standalone:

- **Collectors** (`guaccollect`): files, OCI registries, deps.dev, GitHub releases, GCS/S3 blob stores → CycloneDX (1.4–1.7), SPDX (2.2–2.3, 3.0.1), SLSA Provenance v1, in-toto ITE-6, DSSE, OpenVEX, CSAF 2.0, Scorecard documents.
- **Ingestor** (`guacingest`): parses documents into graph inputs; the **assembler** writes them through a backend abstraction — keyvalue/in-memory (dev), **ent+PostgreSQL**, **ArangoDB** (recommended for large graphs), neo4j/openCypher (incomplete/unsupported).
- **Certifiers** (`guaccert`): enrich the graph from live APIs, polling or one-shot with a last-scan window — **OSV** (batched `querybatch`-style API, default 4h scan window), **deps.dev** (gRPC; writes `hasSourceAt`, `certifyScorecard`, and `isDependency` with `--retrieve-dependencies`; supports npm, PyPI, Go, Maven, **Cargo**, NuGet), **ClearlyDefined** (license/provenance), **Scorecard**, **End-of-Life**. A `collectsub` service feeds certifiers newly-seen packages dynamically.
- **Serving**: a GraphQL server (:8080, playground + schema) and a REST ingestion API (:8081); `guacone query` CLI subcommands (`known`, `path`/neighbors, vuln, good/bad). Typical production deploy: Helm chart with cron-driven certifiers.

**Rust specifics.** GUAC has **no Cargo.lock collector** — the intended path is `cargo cyclonedx` (or another SBOM generator) → CycloneDX document → GUAC; there is a [2026 Rust project goal](https://goals.rust-lang.org/2026/stabilize-cargo-sbom.html) to stabilize a native Cargo SBOM. OSV covers crates.io via RustSec, and deps.dev supports the Cargo ecosystem, so enrichment works once package versions are in the graph by any route.

## 2. Overlap and complementarity

| | Graphicle | GUAC |
|---|---|---|
| Domain | **intra-repo, source-level**: symbols, files, modules, CALLS/CONTAINS/IMPORTS/INHERITS, communities, observations | **inter-package, ecosystem-level**: packages, artifacts, sources, builders, attestations |
| Resolution | every `use`/`import` *inside* the scanned repo, resolved to symbols | dependencies *between* released packages, resolved to versions/purls |
| Time model | bi-temporal episodes (`:Live`/`*_HIST`, valid/recorded time) — first-class time travel | edges carry `origin`/`collector`/scan time; no time travel |
| Consumer | coding agents over MCP, with citations and hints | humans/CI via GraphQL, dashboards, policy queries |
| Trust stance | extracted facts + agent-observable gaps (`UnresolvedImport`, `UnresolvedCall`) | attestations with explicit evidence provenance; NoVuln/negative evidence is a node |

**They meet at exactly one seam, and Graphicle currently truncates it.** The extractor turns every Rust `use` into an `imports_from` edge (`crates/graphicle-graph/src/extractors/rust.rs:760-800`), and undeclared targets of `imports`/`imports_from`/`re_exports` get typed **external stub** nodes (`crates/graphicle-graph/src/build.rs:37-39`, stub shape at `build.rs:99-101`) — but the stub carries no version, no ecosystem, no resolution, and a `use` of an external crate connects the file to a name, not to a *package version*. Meanwhile the package-manifest extractor (`crates/graphicle-graph/src/manifest_ingest.rs:14-21` recognizes `cargo.toml`/`pyproject.toml`/`go.mod`/`pom.xml`/`apm.yml`) emits a `pkg:<name>` node with the project's own name/version and `depends_on` edges to **dep names only** — and those edges to undeclared (external) targets are **dropped** by the build's endpoint rules (`crates/graphicle-graph/src/build.rs:146-155`, explorer-verified). Cargo.lock and package.json are never parsed (classified only as vendored retrieval context, `crates/graphicle/src/code_question.rs:114`).

So: **GUAC's entire domain begins where Graphicle's external stubs end.** GUAC's ideas that map onto Graphicle are the *ontology subset* for package identity + dependency + vulnerability/license evidence — not its infrastructure. Conversely, nothing in GUAC helps with the intra-repo graph, communities, memory, or agent serving; those stay Graphicle-only.

One genuine conceptual transplant, beyond schema: GUAC's **negative evidence and known-unknown discipline** (`NoVuln`, "missing SBOM/SLSA is itself a finding"). Graphicle already has the same instinct (`UnresolvedImport` rows carry a `reason`, `crates/graphicle/src/graph_projection.rs:2403`), and the agent-serving framing makes it stronger: "you import `rand` 0.8.5; 0 direct vulns (scanned 2026-10-28, OSV db v…)" or "this import was never resolved to a workspace package" is exactly the kind of grounded context `recall_project` serves.

## 3. What already exists in Graphicle to build on

Verified on `feat/cypherlite-backend`:

1. **A `:Package` label already exists in the CypherLite store** — the cross-repo linker records package identity per repo/episode: `MERGE (p:Package {workspace_id, repository_id, episode, ecosystem, name}) SET p.version` (`crates/graphicle/src/graph_projection.rs:2503-2531`). But it is identity-only (used to match imports across workspace repos), written from `MANIFEST_FILES = ["pyproject.toml", "setup.cfg", "package.json", "go.mod"]` (`graph_projection.rs:2042`) — **Cargo/Rust is not in the cross-repo ecosystem set at all** (`crates/graphicle-memory/src/graph/cross_repo_imports.rs:51-73`, Python/Node/Go only) — and it is not connected to source symbols or to any external package.
2. **Manifest ingestion runs in the runtime's projection path** (`crates/graphicle/src/graph_projection.rs` builds via the engine's `incremental::extract`, which dispatches manifests per `crates/graphicle-graph/src/extract.rs:108-109,193`), so a `cargo.toml` scan already produces a `type:"package"` node — minus the dropped external `depends_on` edges.
3. **Cargo workspace introspection exists but is unreachable from the runtime**: `introspect_cargo` resolves `[workspace.members]` globs and emits `crate_depends_on` edges — workspace-local crates only, external deps skipped (`crates/graphicle-graph/src/cargo_introspect.rs:106-220`, skip at `:195-197`). Only the engine CLI calls it (`cargo_introspect` referenced from `crates/graphicle-graph/src/cli_extract.rs:667-676` and dispatch `"cargo.introspect"` at `crates/graphicle-graph/src/lib.rs:887`); `crates/graphicle/src` never invokes it.
4. **The graph store has the machinery a GUAC-style layer needs**: dynamic relationship types `CREATE (a)-[r:$(row.reltype)]->(b)` (`graph_projection.rs:900`), history type-flips `CALLS` → `CALLS_HIST` with validity windows (`graph_projection.rs:912-928`), `:Live`/`:SymbolVersion` versioning (`graph_projection.rs:877-883`), a promoted `GRAPH TYPE` with keys (`crates/graphicle/src/store_graph.rs:399-436`), and fulltext/vector `SEARCH` indexes (`store_graph.rs:443-499`).
5. **The job pipeline has a clean slot for a certifier-style job**: `KINDS` at `crates/graphicle/src/jobs.rs:255-270` (with per-kind permissions), dispatch in `crates/graphicle/src/workflows.rs`, admission classes in `crates/graphicle/src/ingestion.rs:44,202-211`, and job dedup by `(repository_id, kind, input_hash)` NODE KEY (`store_graph.rs:429`) — which is precisely GUAC's "re-scan when inputs change, else reuse" certifier model. The `link` job already auto-follows every projection (`jobs.rs:469-473`), showing the pattern for a follow-on enrichment job.
6. **No vulnerability/supply-chain integration exists today**: no `osv|cve|vuln|sbom|spdx|cyclonedx|deps.dev` hits anywhere in the crates (verified by grep; `audit` hits are confidence audits and team-access tables).

## 4. Proposed graph schema (native Cypher, GUAC-shaped)

Design rule: adopt GUAC's **node identity discipline and evidence properties**, not its node/edge names verbatim, and not its attestation-document indirection. Graphicle records provenance as properties on the fact (as it already does for code edges: `relpath`, `source_location`, `derivation`, migration plan §2.4), so GUAC's `origin`/`collector`/`ScanMetadata` become edge properties. Keep it episode-scoped like everything else.

```cypher
-- Package identity, GUAC's purl decomposition flattened (recommended) or kept whole:
(:Package {
  ecosystem: 'cargo',            -- GUAC purl `type`
  name: 'rand',
  version: '0.8.5',              -- PackageVersion level; version='' is name-level (GUAC's two-level rule)
  namespace: '',                 -- crates.io has none; npm scope / maven group go here
  purl: 'pkg:cargo/rand@0.8.5',  -- precomposed for exact-match joins against OSV/deps.dev
  repository_id, episode, created_at
})
-- graph-type key: CONSTRAINT FOR (p:Package) REQUIRE (p.repository_id, p.purl) IS KEY

-- Own-package linkage: the manifest node and the code it contains
(:File {path:'Cargo.toml'})-[:DESCRIBES]->(:Package {version:''})          -- name-level package
(:Symbol {kind:'module'})-[:MODULE_OF]->(:Package)                        -- source ↔ its own package

-- Resolved dependencies from the lockfile (Cargo.lock / package-lock.json):
(:Package)-[:DEPENDS_ON {
  dependency_type: 'DIRECT'|'INDIRECT',     -- GUAC isDependency enum; lockfiles give this for free
  version_req: '^0.8',                      -- from Cargo.toml (the *declared* constraint)
  origin: 'Cargo.lock#sha256:…',            -- GUAC origin: the document the fact came from
  collector: 'graphicle-lockfile',          -- GUAC collector: which component wrote it
  valid_from_episode, valid_to_episode      -- the standard episode window (type-flip on close)
}]->(:Package)

-- Where the code graph meets the package graph (the seam that is missing today):
(:Symbol)-[:IMPORTS]->(:Symbol {type:'external'})     -- exists today (external stubs)
(:Symbol {type:'external'})-[:RESOLVES_TO]->(:Package) -- NEW: stub name → resolved lockfile package

-- Vulnerability evidence (GUAC certifyVuln, property-form):
(:Vulnerability {id:'GHSA-…/OSV-2023-…', aliases:[…], summary, severity, ecosystem,
                 fixed_versions:[…], url})
(:Package)-[:AFFECTED_BY {
  scanner: 'osv.dev', db_version: '…', time_scanned: <iso>,   -- GUAC ScanMetadata
  origin: 'https://api.osv.dev/v1/querybatch', collector: 'graphicle-osv'
}]->(:Vulnerability)
-- negative evidence, GUAC's NoVuln, as a scan marker node (so "checked, clean" is queryable):
(:Package)-[:SCANNED_CLEAN {time_scanned, db_version}]->(:VulnScan {id, repository_id})

-- License evidence (GUAC certifyLegal, simplified to declared-license aggregation):
(:License {id: 'Apache-2.0'})                                 -- SPDX identifier nodes
(:Package)-[:LICENSED_AS {source: 'declared'|'discovered', origin: 'Cargo.toml:license'}]->(:License)

-- Optional, only if the user opts into network enrichment (GUAC deps.dev certifier shape):
(:Package)-[:HAS_SOURCE_AT {origin: 'deps.dev', collector: 'graphicle-depsdev'}]->(:RepoUrl {url})
```

Naming collision to settle before implementing: invalidation already uses `(:Artifact)-[:DEPENDS_ON]->(:Source)` (migration plan §2.5). Distinct endpoints make coexistence legal, but two `DEPENDS_ON` meanings in one store is a documentation hazard — either accept it (endpoints disambiguate) or name the package edge `:PKG_DEPENDS_ON`. Recommend keeping `DEPENDS_ON` between packages and documenting, since manifest_ingest's relation string is already `depends_on` (`manifest_ingest.rs:471`).

Two example agent-facing queries this buys, both one traversal:

```cypher
-- "Why is this vulnerable crate in my build, and what of ours touches it?"
MATCH (v:Vulnerability)<-[:AFFECTED_BY]-(dep:Package)<-[:DEPENDS_ON*0..3]-(root:Package)
      <-[:DESCRIBES]-(:File {path:'Cargo.toml'})
MATCH (ext:Symbol {type:'external'})-[:RESOLVES_TO]->(dep)
MATCH (importer:Symbol)-[:IMPORTS]->(ext)
RETURN v.id, dep.name, dep.version, collector(importer) AS our_code;

-- "Upgrade target": everything fixed by bumping one dependency
MATCH (p:Package {ecosystem:'cargo'})-[:AFFECTED_BY]->(v:Vulnerability)
UNWIND v.fixed_versions AS fixed
MATCH (p2:Package {name:p.name, version:fixed}) RETURN p.name, fixed;
```

## 5. Integration mechanics: GUAC-as-library vs native subset

- **GUAC is Go; Graphicle is Rust.** There are no Rust bindings, and GUAC's useful surface (parsers, certifiers) is coupled to its assembler/NATS plumbing, not exposed as a clean library. Calling it means a subprocess plus its GraphQL API.
- **Subprocess/service route**: run `guaccollect` + GraphQL server (keyvalue in-memory backend is enough for one repo), poll the GraphQL API from Graphicle, map results into the schema above. Cost: a Go binary and NATS as runtime dependencies (or the docker-compose stack), a second datastore, an external service lifecycle — against Graphicle's single-binary, local-first, no-account design (`findings.md`: "single Rust binary… no API key, no account"). Only worth offering as an **optional connector for teams already running GUAC**, the way other connectors work — not as the mechanism.
- **Native subset (recommended)**: the parts of GUAC that matter to coding agents are small. GUAC's real complexity is SBOM/attestation format parsing and identity normalization across ecosystems; for the inbound side Graphicle can start from the **lockfile**, which is simpler and *more* precise than an SBOM (exact resolved versions, no purl guessing):
  - `Cargo.lock` parsing: the `cargo-lock` crate (Apache-2.0/MIT, from the Cargo team's ecosystem) — no cargo invocation needed; `guppy` is the heavier alternative.
  - `package.json` + `package-lock.json`, `pyproject.toml` + resolved locks: later, same shape.
  - OSV: a REST `POST /v1/querybatch` (up to 1000 packages per batch, `ecosystem: "crates.io"` covered by RustSec). `reqwest` is already a runtime dependency (`crates/graphicle/Cargo.toml:31`), and the connector/OAuth machinery plus job queue already handle gated network access with accounting.
  - License: `Cargo.toml` `license`/`license-file` fields (already reachable via manifest parsing), SPDX expression via a small parser crate; deps.dev as optional transitive-license enrichment.
  - Estimated size: one lockfile collector + one OSV certifier + schema/MCP surface — hundreds of lines, not a port.
- **License compatibility**: GUAC code is **Apache-2.0** (verified from the repo's README and the schema file headers); Graphicle is Apache-2.0 throughout (`crates/graphicle/Cargo.toml:7`, `crates/graphicle-graph/Cargo.toml:11`, `crates/graphicle-memory/Cargo.toml:8`, root `LICENSE`). Porting schema concepts or field names is unencumbered; any actual code would be a re-implementation in Rust regardless. GUAC docs are CC BY 4.0 — fine to paraphrase, attribute if quoting. `cargo-lock` and typical SPDX-expression crates are Apache-2.0/MIT — compatible.

## 6. Phased recommendation

| Phase | Item | Verdict | Reasoning |
|---|---|---|---|
| **0** | GUAC infrastructure: GraphQL/REST servers, NATS, collector/certifier mesh, collectsub, Arango/Postgres backends, Helm deployment | **Skip** | Org-scale ingestion architecture; contradicts single-binary local-first; Graphicle's job queue + CypherLite already cover scheduling and storage for one workspace. |
| **0** | Actor/trust tree, DSSE signature verification, `hashEqual`/`pkgEqual` identity disputes | **Skip** | Sigstore/PKI territory; no coding-agent value; GUAC itself keeps the actor tree out of beta. |
| **1** | **Lockfile ingestion → `:Package` + `DEPENDS_ON` (Cargo first)** | **Do first** | Fills the exact gap where external `depends_on` edges are dropped today (`build.rs:146-155`); lockfiles give resolved versions + DIRECT/INDIRECT for free; no network needed, so it fits local-first and lands purely in the sync/`project` pipeline. Wire the manifest `pkg:` node and external stubs to `:Package` via `DESCRIBES`/`RESOLVES_TO`. Also add `Cargo.toml` to the cross-repo `MANIFEST_FILES`/ecosystem set (`graph_projection.rs:2042`) — Rust is the repo's own language and is currently absent from workspace linking. |
| **2** | **OSV vulnerability enrichment** as a new job kind (e.g. `vuln`), following the `link`-job pattern; opt-in network like connectors; dedup via `input_hash` on lockfile content + a re-scan TTL (GUAC's `last-scan` window analog); `SCANNED_CLEAN` negative-evidence nodes | **Do** | The highest-agent-value GUAC idea; OSV's batch API is trivial to call with `reqwest`; job-dedup/NODE KEY (`store_graph.rs:429`) already encodes GUAC's certifier reuse model; negative evidence is the known-unknowns discipline Graphicle's hints/observations are built for. |
| **3** | **License aggregation** (`Cargo.toml` declared licenses → `:License`/`LICENSED_AS`; optional deps.dev for transitive) | **Do, small** | Cheap, offline for the declared part, and a frequent compliance question agents get asked. |
| **3** | MCP surface: `deps_of` / `vuln_scan` / dependency-impact queries; enrich `workspace_map` and `recall_project` rankings with package context | **Do with 1–2** | This is the point of the exercise — the graph is only valuable if agents can query it. Blast-radius/dependents queries are the same shape as existing `get_pr_impact`/`shortest_path` tools (`crates/graphicle/src/graph_tools.rs:68-79`). |
| **4** | **SBOM import** (CycloneDX/SPDX produced by CI) for non-Cargo projects | **Later / optional** | This is where GUAC's parser complexity actually lives; a CycloneDX-JSON parse into the same `:Package` model is moderate work. Worth it only when users ask; until then lockfiles cover Rust/Node/Python. |
| **4** | GUAC bridge connector (subprocess + GraphQL polling) for teams already running GUAC | **Optional connector** | Only as an opt-in connector, mapping GUAC GraphQL results into the native schema; never a core dependency. |
| **—** | SLSA/attestation nodes, `certifyScorecard`, `certifyGood/Bad` policy verdicts, VEX statements, EOL certifier | **Skip (revisit)** | Supply-chain governance, not code comprehension. An agent editing code rarely needs build provenance; if a user base emerges (e.g. CRA compliance), it slots into the same `:Package` model later. |

Sequencing note: Phase 1 is purely offline and lands inside the existing `project` job (the manifest extractor already runs there); Phase 2 introduces the first *dependency-aware* network job and should reuse the connector permission model (`KINDS` permission tiers at `jobs.rs:255-270`) rather than inventing a new one.

## 7. Sources

- [GUAC ontology](https://docs.guac.sh/guac/guac-ontology/) · [ontology definition](https://docs.guac.sh/guac/guac-ontology-definition/) · [known-and-unknown tutorial](https://docs.guac.sh/guac/known-and-unknown/)
- [guacsec/guac repo](https://github.com/guacsec/guac) (README: Apache-2.0, architecture, backends) · [GraphQL schema directory](https://github.com/guacsec/guac/tree/main/pkg/assembler/graphql/schema) (full node/edge type list; `isDependency.graphql`, `certifyVuln.graphql` field shapes) · [releases](https://github.com/guacsec/guac/releases) (v1.1.0)
- [OSV certifier](https://docs.guac.sh/guac/osv-certifier/) · [deps.dev certifier](https://docs.guac.sh/guac/certifier-deps-dev/)
- [GUAC v1.0 release announcement](https://safeguard.sh/resources/blog/guac-v1-0-release-graph-supply-chain-2025)
- [cargo-cyclonedx](https://github.com/CycloneDX/cyclonedx-rust-cargo) (the Cargo→SBOM path GUAC expects) · [Rust 2026 goal: stabilize Cargo SBOM](https://goals.rust-lang.org/2026/stabilize-cargo-sbom.html)
- Graphicle: `crates/graphicle/src/{graph_projection,store_graph,jobs,graph_tools}.rs`, `crates/graphicle-graph/src/{manifest_ingest,cargo_introspect,build}.rs`, `crates/graphicle-memory/src/graph/cross_repo_imports.rs`, plus the root planning docs (`findings.md`, `cypherlite-migration.md`, `architecture-concepts.md`).
