# Architecture rule documents

An architecture rule document is an ordinary file in the repository: Markdown
(`.md`) or AsciiDoc (`.adoc`, in jQAssistant's convention). It holds the team's
prose description of the architecture together with the rules that enrich and
check the graph. Graphicle reads the rule blocks; the rest of the file is
documentation. The design is in `architecture-concepts.md` (§1.5 and §2).

Both formats read into the same rule structs (`crates/graphicle/src/arch_rules/model.rs`):
the frontend only finds the blocks and their metadata, and every rule check
lives in the shared model. `arch_rules::parse_file` chooses the frontend by
extension; `parse_markdown` and `parse_adoc` name it directly.

## Layout

Each rule is one fenced block. The block's info string names its kind:

| Info string   | Meaning                                                          |
|---------------|------------------------------------------------------------------|
| `concept`     | A query whose matches are labelled and connected in the graph.   |
| `constraint`  | A query whose returned rows are violations.                      |
| `group`       | A selection of rules by include pattern, with severity overrides. |

The fence is four backticks, so a three-backtick ```cypher fence can sit inside
it. The block holds `key: value` metadata lines first. A concept or constraint
then carries its Cypher body in a nested ```cypher fence. A group has no body.

`````markdown
````concept
id: arch:Layer/Persistence
requires: [arch:Module/*]
severity: MINOR
description: Persistence modules
```cypher
MATCH (m:Module) WHERE m.name =~ '.*\\.(persistence|db)'
MERGE (l:Layer {name: 'Persistence'})
```
````
`````

The choice between one four-backtick outer fence with a nested ```cypher fence
and sibling fences (a ```concept fence followed by a ```cypher fence) rests on
how the nesting closes. A sibling layout has no grouping: nothing ties a body
to the metadata above it, and a stray fence closes the block early. The nested
layout keeps each rule in one unit that a closing four-backtick fence ends, and
it renders on GitHub as a code block whose inner ```cypher line is literal
text. Rules and prose are kept apart: text placed after a rule's nested body,
inside the block, is an error.

## Metadata

Concepts and constraints accept these keys:

| Key           | Required | Meaning                                                              |
|---------------|----------|----------------------------------------------------------------------|
| `id`          | yes      | The rule's identifier, for example `arch:Layer/Persistence`. Plain text without spaces. |
| `description` | no       | One line of prose, shown to agents and in reports.                  |
| `requires`    | no       | A list of rule ids that run first. `arch:Module/*` names every rule whose id starts with `arch:Module/`. |
| `severity`    | constraints: yes; concepts: no (default `INFO`) | One of `INFO`, `MINOR`, `MAJOR`, `CRITICAL`, `BLOCKER`. |

Groups accept `id`, `description`, `include` (a list of rule ids or patterns,
at least one) and `overrides` (a list of `id=SEVERITY`, applied to the rules
the group selects).

Lists are written `[a, b]`. The wildcard `*` is allowed only as the last
character of a pattern. Each id is declared once per document.

## Example document

The complete document below is a valid rule file. It is shown here inside a
five-backtick fence, so GitHub renders the rule blocks as code and the parser
skips the whole example when this file is read as a document.

`````markdown
# Architecture rules

The persistence layer owns every database module. Catalog code must not
reach into Warehouse internals.

````concept
id: arch:Layer/Persistence
requires: [arch:Module/*]
severity: MINOR
description: Persistence modules are those named *.persistence or *.db
```cypher
MATCH (m:Module) WHERE m.name =~ '.*\\.(persistence|db)'
MERGE (l:Layer {name: 'Persistence'})
```
````

````concept
id: arch:Domain/Catalog
severity: INFO
description: Symbols under the catalog crate belong to the Catalog domain
```cypher
MATCH (s:Symbol) WHERE s.relpath STARTS WITH 'crates/catalog/'
MERGE (d:DomainConcept {name: 'Catalog', repository_id: s.repository_id})
MERGE (s)-[:DOMAIN {concept: 'arch:Domain/Catalog', confidence: 0.8}]->(d)
```
````

````constraint
id: arch:NoCycles
severity: BLOCKER
description: Modules must not depend on each other in a cycle
```cypher
MATCH p = (m1:Module)-[:DEPENDS_ON*2..6]->(m1)
RETURN m1.name AS cycle, [n IN nodes(p) | n.name] AS path
```
````

````constraint
id: arch:CatalogIsolation
requires: [arch:Domain/Catalog]
severity: MAJOR
description: Catalog code does not call Warehouse internals
```cypher
MATCH (a:Symbol)-[:DOMAIN]->(:DomainConcept {name: 'Catalog'}),
      (a)-[:CALLS]->(b:Symbol)-[:DOMAIN]->(:DomainConcept {name: 'Warehouse'})
WHERE NOT (b)-[:PUBLIC_API]->(:Component)
RETURN a.qualified_name AS from, b.qualified_name AS to
```
````

````group
id: arch:default
include: [arch:*]
overrides: [arch:CatalogIsolation=CRITICAL]
description: The default architecture pack; CatalogIsolation is raised for CI
````
`````

## Identity and change tracking

Each rule is stored under its project and `id`. A rule's `rule_sha` is a
digest of its kind, id, description, requires or include list, severity,
overrides and Cypher body. It does not depend on where the block sits in the
file, so moving a rule without editing it keeps its `rule_sha`, and
re-parsing an unchanged document writes the same values again.

## Errors

A malformed block stops the parse and names the block and its line, for
example:

```text
constraint block 'arch:NoSeverity' at line 3: missing required key 'severity'
```

Unknown keys, duplicate ids, a missing ```cypher body, an unclosed fence, an
invalid severity and a malformed list are reported the same way. Fenced blocks
whose info string is not `concept`, `constraint` or `group` are not rules and
are skipped with their contents, so examples and code samples in prose are
safe.

## Where rules live

Rule documents are regular repository files. The layout does not fix a
directory; `docs/architecture/` or `.graphicle/` are the expected homes. A
project's rules are stored as `:ConceptRule`, `:ConstraintRule` and
`:RuleGroup` nodes scoped to that project.

## Execution

When a projection records an episode for a project that has rules, the rule
set's concepts are applied to that episode's graph as one `enrich` job,
keyed by (rule set, episode) — re-projecting the same content replays it,
and a changed head set is a new episode with its own run.

- **Parameters**: a concept's Cypher runs as written, with `$repo` (the
  repository id), `$episode` (the episode being enriched) and `$rule` (the
  rule's own id) as parameters.
- **Order**: concepts run in the order their `requires` DAG resolves to;
  every declared group runs, in group-id order, with its severity overrides
  applied (a project that declared no group runs every rule). Constraints
  are resolved with them and never run by the concept pass; they are
  evaluated afterwards (see Constraints below).
- **Failure**: one graph transaction per rule. A rule whose query fails
  leaves nothing behind, is reported under `failed`, and everything that
  requires it (directly or through another rule) is skipped.
- **Idempotence**: write concepts with `MERGE` and `SET`; a re-run against
  the same episode then changes nothing.
- **Provenance and rollback**: what a rule wrote is recorded per episode as
  `(:RuleApplication {repository_id, rule, episode})` — `:LABELED {label}`
  edges for the labels it set, a `concept`/`episode` stamp on the
  relationships it created, and a `provenance` map naming created nodes,
  created relationships and set properties. Rolling a rule's application
  back removes exactly those labels, relationships and properties (nodes it
  created go too, once its removals leave them empty). A rule that *deletes*
  graph content cannot be rolled back: only additive writes are recorded.

### Constraints

A constraint is a query over the **enriched** graph whose result rows are
violations. The enrich job runs it right after the concepts have applied to
the episode, in the same job, with the same parameters (`$repo`, `$episode`,
`$rule`).

- **Each row is a violation**: one `(:Violation)` node per returned row,
  linked to the project with `(:Violation)-[:IN_PROJECT]->(:Project)`. It
  carries `repository_id`, `episode`, `rule` (the constraint id), `rule_sha`
  and `severity`.
- **Severity** is the effective severity the resolution gives the constraint,
  after its group overrides (for example `overrides: [arch:X=BLOCKER]`).
- **Evidence**: each returned column is kept as a property of the violation.
  Scalars are stored as they are; nodes, lists and maps are stored as JSON
  text. A column named `repository_id`, `episode`, `rule`, `rule_sha` or
  `severity` is stored as `evidence_<column>`. `path` and `line` are set when
  the row provides them: from a `path` or `line` column, or from a node
  column's `relpath`/`path` and `line`/`start_line`.
- **Replacement**: evaluating a constraint replaces its rows for the episode.
  Re-running an unchanged rule set on the same episode leaves one set of
  rows; another episode keeps its own.
- **Passing**: a query that returns no rows writes no violations and is
  reported as `passing`.
- **Errors**: a query that fails is rolled back whole, reported as `error`,
  and leaves no rows for that episode. A constraint that requires a concept
  which failed is reported as `skipped`.

The enrich job's result carries the outcome under `constraints`: the counts
`violations`, `passing`, `errors` and `skipped`, and one entry per constraint.
Verification modes beyond "rows are violations" (such as `rowCount` ratchets)
are not part of this version.

## AsciiDoc (jQAssistant convention)

A `.adoc` rule document follows the layout jQAssistant's own rule files use
(jqa-dashboard's `dashboard.adoc`). A rule is a `----` listing whose block
attributes name its kind; the id is the block's anchor; the description is its
`.Title`:

```asciidoc
[[arch:Layer/Persistence]]
[source,cypher,role=concept,severity=MINOR,requiresConcept=arch:Module/*]
.Persistence modules are those named *.persistence or *.db.
----
MATCH (m:Module) WHERE m.name =~ '.*\\.(persistence|db)'
MERGE (l:Layer {name: 'Persistence'})
----

[[arch:NoCycles]]
[source,cypher,role=constraint,severity="BLOCKER"]
.Modules must not depend on each other in a cycle.
----
MATCH p = (m1:Module)-[:DEPENDS_ON*2..6]->(m1) RETURN p
----

[[arch:default]]
[role=group, includesConcepts="arch:*", overrides="arch:NoCycles=MAJOR"]
== Default pack

The group has no listing; its section ends the block.
```

- `include::queries/churn.adoc[]` splices the file in, relative to the
  document's folder; includes nest up to eight levels deep.
- A rule block that is malformed is reported with its file, kind, id and line,
  as in Markdown: `constraint block 'arch:NoSeverity' of queries/x.adoc at line 3: missing required key 'severity'`.
- Severity and requires may be written either way jQAssistant does: a bare
  attribute (`severity=MINOR`), a quoted one (`severity="MINOR"`), a single
  reference (`requiresConcept=`) or a list (`requiresConcepts="a, b"`).

The AsciiDoc example above is a `asciidoc` fence, which the Markdown parser
skips, so it is not read as a rule in this file. Its tests read it directly.

### Construct compatibility

Checked against the jQAssistant rule files that the Git plugin and the
dashboard publish (`research/jqa-git-queries.md`), and against fixtures written
in the same dialect under `crates/graphicle/tests/fixtures/arch_rules/jqa-git/`.
No GPL-licensed text is copied: every fixture is written for this repository.

| Construct | Where seen | Status | How / why |
|---|---|---|---|
| `[[id]]` anchor on a rule | dashboard.adoc, git.adoc | supported | The anchor is the rule id. An `id=` attribute must agree with it. |
| `[source,cypher,role=concept]` listing | dashboard.adoc | supported | The rule body. |
| `[source, cypher]` (space after comma) | git.adoc sample | supported | Items are trimmed. |
| `role="concept"` / `role=concept` | dashboard.adoc | supported | Quotes are removed. |
| `role=constraint` | jQA rule convention | supported | Violation rule. |
| `role=group` on a section, no listing | dashboard.adoc `[role=group]` | supported | The section ends the block. |
| `.Title` line | dashboard.adoc | adapted | Becomes `description`. |
| `description=` attribute | (ours) | supported | Wins over `.Title` when both are given. |
| `severity=MINOR` / `severity="MAJOR"` | jQA convention for `[role=concept,severity=...]` | supported | Both spellings. A concept with no severity is INFO. |
| `requiresConcept=x` (single) | dashboard.adoc `requiresConcepts="..."` | supported | Feeds the same list as `requiresConcepts`. |
| `requiresConcepts="a, b"` | dashboard.adoc | supported | Comma-separated list, trimmed. |
| `includesConcepts="p:*"` on a group | dashboard.adoc | adapted | Becomes `include`; the wildcard keeps its meaning (trailing `*` only). |
| `includesConstraints=` on a group | jQA convention | adapted | Becomes `include`, same as `includesConcepts`. |
| `overrides="id=SEV"` on a group | (ours) | supported | Severity overrides; jQA has no equivalent. |
| `verify="aggregation"` | dashboard.adoc `verify=` | adapted | Read and ignored. jQA's verification mode is not implemented; the body runs as written. |
| `[[id,reftext]]` anchor with reftext | jQA docs | adapted | The reftext is dropped; only the id is kept. |
| `include::file[]` | jQA rule files split by concern | adapted | Resolved relative to the including file, recursively to 8 levels. A missing file is an error. Include attributes (`leveloffset=`, `tags=`) are not applied. |
| Prose, sections, paragraphs | every rule file | supported | Skipped. Text between rules is documentation. |
| `////` comment block and `//` line comment | git.adoc | supported | Skipped, including any rule-looking block inside. |
| `[source,cypher]` with no role | dashboard.adoc examples | supported | Not a rule; skipped. |
| Tables (`[options="header"]`, `|===`) | git.adoc configuration | supported | Skipped as prose. |
| Other `[source,lang]` listings with a role | (none seen) | unsupported | A rule body must be `cypher`; any other language is an error, not a silent skip. |
| Attribute references `{name}` in a rule | jQA docs (document attributes) | unsupported | No attribute context is evaluated. A rule that uses one is refused, naming the line. |
| `ifdef::` / `ifndef::` / `ifeval::` / `endif::` | jQA docs | unsupported | Conditionals need an attribute context. Refused, so a rule behind a conditional is never silently included or dropped. |
| `include::` target with `{attr}` | jQA docs | unsupported | Same reason as attribute references. |
| `[source,cypher]` without `role` but an anchor | git.adoc sample | supported | Skipped: without a `role` it is not a rule. |
| Single-valued attribute given twice (`id`, `description`, `severity`, `overrides`, `source`) | (ours) | unsupported (rejected) | Error: a repeated attribute has no single meaning. `role` and the `requires`/`include` lists are not checked for repeats: a repeated list merges, and `role` takes its first kind. |
| Unknown attribute on a rule block | jQA: any key | unsupported (rejected) | Error, naming the key: a typo such as `severty=` must not become silent INFO. Known keys are listed in `adoc.rs`. |
| Anchor-less rule (no `[[id]]` and no `id=`) | (ours) | unsupported | Error: a rule needs an id. |
| Git sample query with `GitBranch`/`HEAD` | git.adoc sample | unsupported | Pre-migration model names. The query is ordinary Cypher once the labels are updated, which is out of scope for the frontend. |

The jQA Git rules in `research/jqa-git-queries.md` use their own model
(`:Git:Commit`, `:Git:Author`, `:Git:File`). Their Cypher is carried as
written; the frontend does not rewrite labels. A rule that targets the
Graphicle git schema must name those labels itself.
