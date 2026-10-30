# Architecture rule documents

An architecture rule document is an ordinary Markdown file in the repository.
It holds the team's prose description of the architecture together with the
rules that enrich and check the graph. Graphicle reads the rule blocks; GitHub
renders the rest of the file as documentation. The design is in
`architecture-concepts.md` (§1.5 and §2).

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
