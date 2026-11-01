# Architecture rules

Graphicle is three crates, and the boundaries between them are the
architecture this document declares and checks. The rules are applied to
this repository's own projected code graph (`graphicle analyze`; the rule
format is documented in [architecture-rules.md](../architecture-rules.md)),
which makes this file the dogfood pack: the first rule set the engine runs
is over the engine.

- **crates/graphicle** (package `graphicle-runtime`, binary `graphicle`) is
  the **host layer**: the command line, the MCP server, the store, sync and
  the job queue that orchestrate everything below it. It is the only crate
  that knows about projects, sessions and people.
- **crates/graphicle-graph** (package `graphicle`) is the **engine layer**:
  tree-sitter extraction, the code-graph build and the analyses over it. It
  answers questions about code; it does not store anything.
- **crates/graphicle-memory** is the **memory layer**, the library the other
  two sit on: text processing, structure analysis and retrieval.

Throughout, a *dependency edge* is one of the relations the projection
records between symbols: `CALLS` and `INDIRECT_CALL` (a call), `IMPORTS` and
`IMPORTS_FROM` (a use statement), `REFERENCES` (a type reference), `USES`.
Rules read the `:Symbol` node, which carries `kind`, `qualified_name` and
`relpath` directly; a version's finer attributes are one stringified JSON
property on `:SymbolVersion`, which Cypher cannot take apart.

## Modules

A symbol's module is its crate and the `src/` directory that holds it:
`crates/graphicle/src/arch_rules/mod.rs` belongs to the module
`graphicle/arch_rules`, a file directly under `src/` belongs to the crate's
`(root)` module. Tests and documentation are not modules of the
architecture, so only `crates/*/src/` is read.

````concept
id: arch:Module/ByRelPath
description: A symbol's module is its crate and the src directory that holds it
```cypher
MATCH (s:Symbol {repository_id: $repo})-[:HAS_VERSION]->(:Live)
WHERE s.relpath =~ 'crates/[^/]+/src/[^/]+(/.*)?'
WITH s, split(s.relpath, '/') AS parts
WITH s, parts[1] + '/' +
     CASE WHEN size(parts) > 4 THEN parts[3] ELSE '(root)' END AS name
MERGE (m:Module {repository_id: $repo, name: name})
MERGE (s)-[:IN_MODULE {concept: $rule}]->(m)
```
````

## Layers

Each module sits in exactly one layer, by its crate: everything under
`crates/graphicle/src` is the host layer, `crates/graphicle-graph/src` the
engine layer, `crates/graphicle-memory/src` the memory layer. The concept
labels the symbols with their layer and records the declared layering as
`(:Module)-[:IN_LAYER]->(:Layer)` edges.

````concept
id: arch:Layer/OfCrate
requires: [arch:Module/ByRelPath]
description: Every symbol of a crate's src tree carries its layer label
```cypher
MATCH (s:Symbol {repository_id: $repo})-[:IN_MODULE]->(m:Module {repository_id: $repo})
WITH s, m, split(m.name, '/')[0] AS crate
SET s:$(CASE crate WHEN 'graphicle' THEN 'Host'
                   WHEN 'graphicle-graph' THEN 'Engine'
                   WHEN 'graphicle-memory' THEN 'Memory' END + 'Layer')
WITH DISTINCT m, crate
MERGE (l:Layer {repository_id: $repo,
                name: CASE crate WHEN 'graphicle' THEN 'Host'
                                 WHEN 'graphicle-graph' THEN 'Engine'
                                 WHEN 'graphicle-memory' THEN 'Memory' END})
MERGE (m)-[:IN_LAYER {concept: $rule}]->(l)
```
````

## Coupling metrics

The fan-in and fan-out family from the jQA quick guide, over the dependency
edges between *named* code symbols — functions, types and methods, whose
`qualified_name` is a name. The projection also emits one file-level
aggregate symbol per source file (its `qualified_name` is a path); those
hold a file's totals, not a symbol's coupling, so they are left out. Each
step is materialized before the next reads it: fan-out first, then fan-in
over the symbols fan-out covered, then a combined coupling property — the
`requires` chain is the same idea in rule form.

````concept
id: arch:Metric/FanOut
description: Fan-out is the count of a named code symbol's outgoing dependency edges
```cypher
MATCH (s:Symbol {repository_id: $repo, kind: 'code'})-[:HAS_VERSION]->(:Live)
WHERE NOT s.qualified_name CONTAINS '/'
OPTIONAL MATCH (s)-[o]->(t:Symbol {repository_id: $repo})
WHERE type(o) IN ['CALLS','INDIRECT_CALL','IMPORTS','IMPORTS_FROM',
                  'REFERENCES','USES'] AND t <> s
WITH s, count(o) AS fan_out
SET s.fan_out = fan_out
```
````

````concept
id: arch:Metric/FanIn
requires: [arch:Metric/FanOut]
description: Fan-in is the count of a named code symbol's incoming dependency edges
```cypher
MATCH (s:Symbol {repository_id: $repo, kind: 'code'})-[:HAS_VERSION]->(:Live)
WHERE s.fan_out IS NOT NULL
OPTIONAL MATCH (src:Symbol {repository_id: $repo})-[i]->(s)
WHERE type(i) IN ['CALLS','INDIRECT_CALL','IMPORTS','IMPORTS_FROM',
                  'REFERENCES','USES'] AND src <> s
WITH s, count(i) AS fan_in
SET s.fan_in = fan_in
```
````

````concept
id: arch:Metric/Coupling
requires: [arch:Metric/FanIn]
description: Coupling combines a named code symbol's fan-in and fan-out
```cypher
MATCH (s:Symbol {repository_id: $repo, kind: 'code'})-[:HAS_VERSION]->(:Live)
WHERE s.fan_in IS NOT NULL AND s.fan_out IS NOT NULL
SET s.coupling = s.fan_in + s.fan_out
```
````

## The fan-out budget

A code symbol that fans out widely is expensive to change, so the pack
holds a budget for it as a `:RuleRange` node rather than a number in a
config file — the threshold is graph content, versioned with everything
else and queryable next to the symbols it judges. One symbol is exempt
with the reason stated on the exemption itself; an exemption without a
reason is debt, not a decision: `checks` in `terminal.rs` assembles the
doctor's checklist, and every check it names is one more row of that
checklist, not one more thing it is coupled to.

````concept
id: arch:Threshold/FanOut
requires: [arch:Metric/Coupling]
description: The fan-out budget and its stated exception live in the graph
```cypher
MERGE (range:RuleRange {repository_id: $repo, metric: 'fan_out'})
SET range.max = 30
WITH range
MATCH (e:Symbol {repository_id: $repo, qualified_name: 'checks',
                 relpath: 'crates/graphicle/src/terminal.rs'})
MERGE (x:RuleException {repository_id: $repo,
                        symbol: 'checks',
                        relpath: 'crates/graphicle/src/terminal.rs'})
SET x.reason = 'the doctor assembles every health check by design; its fan-out is the checklist it renders, not coupling between parts'
MERGE (x)-[:EXEMPTS {concept: $rule}]->(e)
```
````

````constraint
id: arch:Coupling/FanOutWithinBudget
requires: [arch:Threshold/FanOut]
severity: MINOR
description: A named code symbol fans out further than the budget allows
```cypher
MATCH (s:Symbol {repository_id: $repo, kind: 'code'})-[:HAS_VERSION]->(:Live)
MATCH (range:RuleRange {repository_id: $repo, metric: 'fan_out'})
WHERE s.fan_out > range.max
  AND NOT ((:RuleException {repository_id: $repo})-[:EXEMPTS]->(s))
RETURN s.qualified_name AS symbol, s.fan_out AS fan_out, s.relpath AS path
```
````

## Engine internals

The engine's extraction pipeline — the `extract/`, `extract_members/` and
`extractors/` directories under `crates/graphicle-graph/src`, and the
`extract*.rs` stages that sit directly in the crate root — is
implementation detail: it changes with every language and grammar, and the
host has no business knowing any of that. The engine's facade is its
`api.rs`, `analyze.rs` and `build.rs` modules. The boundary concept marks
the internal symbols with their module and file location; the constraint
then reads like the architecture it checks, in the layer language of the
concepts above it.

````concept
id: arch:Boundary/EngineInternal
requires: [arch:Layer/OfCrate]
description: The engine's extraction pipeline symbols are engine-internal
```cypher
MATCH (s:Symbol {repository_id: $repo})-[:IN_MODULE]->(m:Module {repository_id: $repo})
WHERE m.name STARTS WITH 'graphicle-graph/extract'
   OR s.relpath =~ 'crates/graphicle-graph/src/extract[^/]*[.]rs'
SET s:EngineInternal
WITH DISTINCT m
WHERE m.name STARTS WITH 'graphicle-graph/extract'
SET m:EngineInternal
```
````

````constraint
id: arch:Encapsulation/HostNotIntoEngineInternals
requires: [arch:Boundary/EngineInternal]
severity: MAJOR
description: Host-layer symbols must not reach into the engine's extraction pipeline
```cypher
MATCH (a:Symbol:HostLayer {repository_id: $repo})-[r]->
      (b:Symbol:EngineInternal {repository_id: $repo, kind: 'code'})
WHERE type(r) IN ['CALLS','INDIRECT_CALL','IMPORTS','IMPORTS_FROM',
                  'REFERENCES','USES']
RETURN a.qualified_name AS host_symbol, b.qualified_name AS engine_internal,
       b.relpath AS path, type(r) AS via,
       toInteger(substring(r.source_location, 1)) AS line
```
````

Reading this constraint's report: every row is a dependency edge the
projection recorded from a host symbol into the extraction pipeline. The
rows this pack's first run produced are, on inspection, cross-file
name-resolution artifacts — `base64`'s `Engine`, axum's `Router` and
tokio's `Receiver` resolved to same-named symbols under `extractors/`,
and local closures (`origin`, `unresolved`, `nested`) resolved to engine
functions of the same name — so the boundary itself holds, and the rows
name an extractor problem, not an architecture one. A resolved edge does
not record which `use` or path it was resolved through, so no rule can
tell the two apart; that is the gap this dogfood run surfaced.

## The pack

````group
id: arch:default
include: [arch:*]
description: Every rule of this repository's pack, for graphicle analyze and CI
````

## Reproducing these numbers

The measurements in this document were taken on the repository's own code —
the three crates, the docs and the root markdown, without
`crates/graphicle-graph/vendor` (the tree-sitter grammar C sources). A sync
of the full repository including that tree does not merely risk the batch
cap: it fails outright today — the 64 MiB `put_objects` batch equals
CypherLite's WAL payload cap, so `sync_commit` ends in "WAL payload exceeds
64 MiB limit". Sync the trimmed scope, or raise/split the WAL-bound batch
first. The rules are unaffected either way: no rule matches anything under
`vendor/`.
