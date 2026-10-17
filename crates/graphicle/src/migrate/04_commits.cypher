-- Domain: git history (gf_commits, gf_commit_parents). Plan §2.4:
-- (:Commit {repository_id, oid, tree_oid, committed_us, recorded_us,
-- object})-[:PARENT {position}]->(:Commit). `object` is base64 (the graph
-- value model has no byte string). Commit history is a fact about the
-- repository, not a re-derivable projection, so it is carried over.

-- @csv commits.csv
-- @verify MATCH (n:Commit) RETURN count(n) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MERGE (c:Commit {repository_id: row.repository_id, oid: row.oid})
SET c.tree_oid = row.tree_oid,
    c.committed_us = toInteger(row.committed_us),
    c.recorded_us = toInteger(row.recorded_us),
    c.object = row.object;

-- @csv commit_parents.csv
-- @verify MATCH ()-[r:PARENT]->() RETURN count(r) AS n
LOAD CSV WITH HEADERS FROM $csv AS row
MATCH (c:Commit {repository_id: row.repository_id, oid: row.commit_oid})
MATCH (p:Commit {repository_id: row.repository_id, oid: row.parent_oid})
MERGE (c)-[r:PARENT {position: toInteger(row.position)}]->(p);
