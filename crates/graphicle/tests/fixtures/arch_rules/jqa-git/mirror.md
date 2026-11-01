# Git history rules, Markdown mirror

The same rules as `git.adoc` in this folder, written in the Markdown layout.
The parser test reads both files and compares the rules they declare.

````concept
id: fixture-git:PersonsByEmail
description: Flag author nodes that share an email as having aliases.
```cypher
MATCH (a:Author)
WITH a.email AS email, collect(a.name) AS names
WHERE size(names) > 1
MATCH (b:Author {email: email})
SET b.has_aliases = true
```
````

````concept
id: fixture-git:CommitsWithParents
description: Label commits that have a parent, as a count of HAS_PARENT edges.
```cypher
MATCH (c:Commit)-[:HAS_PARENT]->(:Commit)
WITH c, count(*) AS parents
SET c.parent_count = parents
```
````

````concept
id: fixture-git:ChurnPerFile
description: Count the commits that change each file.
```cypher
MATCH (f:File)<-[:MODIFIES]-(:Change)<-[:CONTAINS_CHANGE]-(c:Commit)
WITH f, count(DISTINCT c) AS changes
SET f.churn = changes
```
````

````concept
id: fixture-git:CommitsPerMonth
severity: MINOR
description: Count commits per month.
```cypher
MATCH (c:Commit)
RETURN date(c.author_date).year AS year, date(c.author_date).month AS month,
       count(c) AS commits
ORDER BY year, month
```
````

````constraint
id: fixture-git:BusFactorRisk
requires: [fixture-git:ChurnPerFile]
severity: MAJOR
description: A file with a churn above 50 and a single author.
```cypher
MATCH (f:File)
WHERE f.churn > 50
MATCH (f)<-[:MODIFIES]-(:Change)<-[:CONTAINS_CHANGE]-(:Commit)<-[:AUTHORED]-(p:Person)
WITH f, count(DISTINCT p) AS authors
WHERE authors = 1
RETURN f.path AS file, authors
```
````

````concept
id: fixture-git:CoupledFiles
requires: [fixture-git:ChurnPerFile, fixture-git:CommitsWithParents]
description: Pairs of files that change in the same commit.
```cypher
MATCH (c:Commit)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f1:File),
      (c)-[:CONTAINS_CHANGE]->(:Change)-[:MODIFIES]->(f2:File)
WHERE f1.path < f2.path
WITH f1, f2, count(DISTINCT c) AS together
WHERE together > 2
MERGE (f1)-[r:CHANGED_TOGETHER_WITH]-(f2)
SET r.count = together
```
````

````constraint
id: fixture-git:Refactorings
requires: [fixture-git:*]
severity: CRITICAL
description: Commits that touch more than fifty files.
```cypher
MATCH (c:Commit)-[:CONTAINS_CHANGE]->(ch:Change)
WITH c, count(ch) AS files
WHERE files > 50
RETURN c.oid AS commit, files
```
````

````group
id: fixture-git
include: [fixture-git:*]
overrides: [fixture-git:ChurnPerFile=MAJOR]
````
